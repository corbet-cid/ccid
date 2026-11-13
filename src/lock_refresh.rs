//! `ccid lock-refresh`: the one mover of a repository's committed `Cargo.lock` files.
//!
//! A lock refresh is started by an event, never by a schedule: the release of one of the fleet's
//! own crates re-locks the repositories that depend on it, a release run refreshes third-party
//! versions in its preflight, and a person can dispatch it by hand. Its result is an ordinary
//! lock-only commit that goes through the repository's normal flow, so the commit's own verify
//! run is the proof. Advisories stay a check: a vulnerable lock still turns a commit red.
//!
//! What moves (no ccid step moves a third-party version on its own):
//! - always: every branch-tracking git package that the release channel's fleet data classifies
//!   as first-party (the float's selection), to the head of its branch, with
//!   `cargo update -p <git+url#name@version> …`; Cargo moves a third-party package only where a
//!   moved first-party package requires it, and every such change is reported;
//! - `--package NAME` (repeatable): a registry package that a release event names, with
//!   `cargo update -p NAME@<locked version>` for each locked registry version of it, to the newest
//!   version its requirements allow; a name no lock holds moves nothing and is reported;
//! - `--third-party`: one plain `cargo update` per lock, every package to the newest version its
//!   requirements allow (release preflight, manual dispatch).
//!
//! The resolution runs in a copy of the checkout, like the float; then exactly the lock files whose
//! bytes changed are written back. Nothing else in the checkout is written. Inside a ccid check
//! the command refuses: a check never moves the locks it is keyed on.
use crate::{cache, compile_cache, failure, fleet, float, value, Environment, Result};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::Command,
};

/// How one lock is moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Update {
    /// `cargo update -p <spec> …`
    Packages(BTreeSet<String>),
    /// `cargo update`
    All,
}

impl Update {
    pub(crate) fn arguments(&self) -> Vec<String> {
        let mut arguments = vec!["update".to_owned()];
        if let Update::Packages(specs) = self {
            for spec in specs {
                arguments.push("-p".into());
                arguments.push(spec.clone());
            }
        }
        arguments
    }
}

/// A crate name as Cargo accepts it.
fn valid_crate_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `name@version` of every registry package of `text` named in `names`.
pub(crate) fn registry_specs(lock: &str, text: &str, names: &[String]) -> Result<BTreeSet<String>> {
    let value: toml::Value = toml::from_str(text)
        .map_err(|error| failure(format!("Lock {lock} is not TOML: {error}")))?;
    let mut specs = BTreeSet::new();
    for package in value
        .get("package")
        .and_then(|packages| packages.as_array())
        .into_iter()
        .flatten()
    {
        let field = |key: &str| package.get(key).and_then(|v| v.as_str());
        let (Some(name), Some(version), Some(source)) =
            (field("name"), field("version"), field("source"))
        else {
            continue;
        };
        if (source.starts_with("registry+") || source.starts_with("sparse+"))
            && names.iter().any(|wanted| wanted == name)
        {
            specs.insert(format!("{name}@{version}"));
        }
    }
    Ok(specs)
}

/// What each lock gets: the first-party git selection plus the named registry packages, or one
/// plain update with `third_party`. Locks with nothing to move are absent.
pub(crate) fn plan(
    committed: &BTreeMap<String, Vec<u8>>,
    first_party: &float::Selection,
    packages: &[String],
    third_party: bool,
) -> Result<BTreeMap<String, Update>> {
    let mut planned = BTreeMap::new();
    for (lock, bytes) in committed {
        if third_party {
            planned.insert(lock.clone(), Update::All);
            continue;
        }
        let text =
            std::str::from_utf8(bytes).map_err(|_| failure(format!("Lock {lock} is not UTF-8")))?;
        let mut specs = first_party.get(lock).cloned().unwrap_or_default();
        specs.extend(registry_specs(lock, text, packages)?);
        if !specs.is_empty() {
            planned.insert(lock.clone(), Update::Packages(specs));
        }
    }
    Ok(planned)
}

/// `ccid lock-refresh --repo REPO [--package NAME]… [--third-party]`: move the committed locks
/// in place and print one JSON line describing what moved.
pub fn run(repo: &Path, packages: &[String], third_party: bool) -> Result<u8> {
    let environment: Environment = std::env::vars_os().collect();
    refresh(repo, packages, third_party, &environment)
}

pub(crate) fn refresh(
    repo: &Path,
    packages: &[String],
    third_party: bool,
    environment: &Environment,
) -> Result<u8> {
    if value(environment, "CCID_TARGET_LOCK_HELD").is_some()
        || value(environment, "CCID_CACHE_CHILD").is_some()
    {
        return Err(failure(
            "lock-refresh moves the committed locks; it never runs inside a ccid check",
        ));
    }
    if let Some(bad) = packages.iter().find(|name| !valid_crate_name(name)) {
        return Err(failure(format!("--package {bad:?} is not a crate name")));
    }
    let root = repo.canonicalize()?;
    let mut committed = BTreeMap::new();
    for path in float::committed_lock_paths(&root)? {
        let bytes = fs::read(root.join(float::relative(&path)?))
            .map_err(|error| failure(format!("Committed lock {path} is unreadable: {error}")))?;
        committed.insert(path, bytes);
    }
    // The report classifies every moved entry, so fleet data is required even when no lock
    // tracks a branch.
    let (fleet, _) = fleet::load(environment)
        .map_err(|error| failure(format!("lock-refresh needs fleet data: {error}")))?;
    let first_party = float::select(&committed, environment)?
        .map(|(selection, _)| selection)
        .unwrap_or_default();
    let planned = plan(&committed, &first_party, packages, third_party)?;
    let mut held: BTreeSet<&String> = BTreeSet::new();
    for (lock, bytes) in &committed {
        let text = std::str::from_utf8(bytes).unwrap_or_default();
        for name in packages {
            if !registry_specs(lock, text, std::slice::from_ref(name))?.is_empty() {
                held.insert(name);
            }
        }
    }
    let absent: Vec<&String> = packages
        .iter()
        .filter(|name| !held.contains(name))
        .collect();
    let mut resolved = committed.clone();
    if !planned.is_empty() {
        let scratch = cache::quiet_scratch(&mut environment.clone())?;
        let stage = scratch.path().join("lock-refresh");
        fs::create_dir(&stage)?;
        cache::copy_source(&root, &stage, true)?;
        let cargo = compile_cache::executable(environment, "cargo")
            .ok_or_else(|| failure("lock-refresh cannot find cargo on PATH"))?;
        for (lock, update) in &planned {
            let file = stage.join(float::relative(lock)?);
            if !fs::symlink_metadata(&file)?.is_file() {
                return Err(failure(format!(
                    "Lock {lock} is not a regular file; refusing to resolve it"
                )));
            }
            let output = Command::new(&cargo)
                .args(update.arguments())
                .current_dir(file.parent().unwrap_or(&stage))
                .env_clear()
                .envs(environment.iter())
                .output()
                .map_err(|error| failure(format!("lock-refresh cannot run cargo: {error}")))?;
            if !output.status.success() {
                return Err(failure(format!(
                    "lock-refresh: cargo {} failed in {lock}: {}",
                    update.arguments().join(" "),
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        for lock in committed.keys() {
            resolved.insert(lock.clone(), fs::read(stage.join(float::relative(lock)?))?);
        }
    }
    let changed: Vec<&String> = committed
        .iter()
        .filter(|(lock, bytes)| resolved.get(*lock) != Some(*bytes))
        .map(|(lock, _)| lock)
        .collect();
    for lock in &changed {
        let destination = root.join(float::relative(lock)?);
        float::refuse_link(&destination)?;
        fs::write(&destination, &resolved[*lock])?;
    }
    let requested: BTreeMap<&String, Vec<String>> = planned
        .iter()
        .map(|(lock, update)| (lock, update.arguments()))
        .collect();
    println!(
        "{}",
        json!({
            "mode": if third_party { "third-party" } else { "first-party" },
            "requested": requested,
            "changed": changed,
            "absent_packages": absent,
            "first_party": float::lock_changes(&committed, &resolved, &fleet, true)?,
            "third_party": float::lock_changes(&committed, &resolved, &fleet, false)?,
        })
    );
    Ok(0)
}

#[cfg(all(test, unix))]
mod tests;

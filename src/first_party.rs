//! Require main declarations and one resolved revision per first-party crate.
//!
//! A port of the copied `.github/check-first-party.py` scripts (63 repositories,
//! 25 variants) into one shared check. Every first-party git dependency must
//! declare `branch = "main"` (or the branch of a matching retained entry) and
//! every first-party lock entry must resolve to exactly one full 40-hex
//! revision. The fleet data (`src/fleet.rs`) decides what is first-party; no
//! owner list lives here.
//!
//! Exit statuses: 0 passes, 1 refusal (`check-first-party: <message> (<path>)`),
//! 2 operational error (`ccid first-party: <error>`).

use crate::{failure, fleet::Fleet, fleet::Origin, Result};
use clap::Args;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn is_hex40(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One deliberately retained original owner on another line.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Retained {
    /// Locked package name.
    pub package: String,
    /// Exact repository spelling as written, with or without `.git`.
    pub repository: String,
    /// The retained line, for example `v01`.
    pub branch: String,
    /// Exact locked version, when the exception is versioned.
    #[serde(default)]
    pub version: Option<String>,
}

/// The `[first_party]` table of `.ci/ccid.toml`. Absent table means defaults.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Accept `rev=<fragment>` as a lock query (un-migrated upstreams).
    #[serde(default)]
    pub rev_selectors: bool,
    /// Refuse sources under a fleet alias prefix.
    #[serde(default)]
    pub canonical_only: bool,
    /// A root `Cargo.toml` needs a root `Cargo.lock`.
    #[serde(default)]
    pub require_root_lock: bool,
    /// Every tracked `Cargo.toml` needs a sibling `Cargo.lock`, tracked or not.
    #[serde(default)]
    pub lock_per_manifest: bool,
    /// Extra directory names skipped by the archive walk.
    #[serde(default)]
    pub skip: Vec<String>,
    /// Retained original owners.
    #[serde(default)]
    pub retained: Vec<Retained>,
}

impl Config {
    fn validate(&self) -> Result<()> {
        for name in &self.skip {
            if name.is_empty()
                || name.contains('/')
                || name.contains('\\')
                || *name == "."
                || *name == ".."
            {
                return Err(failure(format!(
                    "first_party skip {name:?} must be a plain directory name"
                )));
            }
        }
        let mut seen = BTreeSet::new();
        for entry in &self.retained {
            if entry.package.is_empty() {
                return Err(failure("first_party retained package must not be empty"));
            }
            if entry.repository.is_empty() {
                return Err(failure("first_party retained repository must not be empty"));
            }
            if entry.branch.is_empty() {
                return Err(failure("first_party retained branch must not be empty"));
            }
            if entry.version.as_deref().is_some_and(str::is_empty) {
                return Err(failure(format!(
                    "first_party retained version for {} must not be empty",
                    entry.package
                )));
            }
            if !seen.insert((entry.package.clone(), entry.repository.clone())) {
                return Err(failure(format!(
                    "duplicate first_party retained entry for {} {}",
                    entry.package, entry.repository
                )));
            }
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
pub struct Options {
    /// Repository root to scan.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// Manifest carrying `[first_party]`.
    #[arg(long, default_value = ".ci/ccid.toml")]
    pub manifest: PathBuf,
    /// Walk the tree instead of `git ls-files`, skipping build directories.
    #[arg(long)]
    pub archive: bool,
    /// Check manifests only, no locks.
    #[arg(long)]
    pub manifest_only: bool,
    /// Print the self-test pointer and exit 0.
    #[arg(long)]
    pub self_test: bool,
    /// Refused: the resolved-graph mode lives in a later lane.
    #[arg(long)]
    pub metadata: Option<PathBuf>,
}

/// The manifest file contributes only the first-party table; every other table
/// belongs to other subcommands and is ignored here.
#[derive(Debug, Deserialize)]
struct FileManifest {
    #[serde(default)]
    first_party: Option<Config>,
}

/// Split a lock source by plain string operations: strip one `git+`, the
/// fragment is the text after the first `#`, the query the text between the
/// first `?` and the `#`, the repository the text before the `?`.
fn split_source(source: &str) -> (&str, &str, &str) {
    let rest = source.strip_prefix("git+").unwrap_or(source);
    let (before_hash, fragment) = match rest.find('#') {
        Some(index) => (&rest[..index], &rest[index + 1..]),
        None => (rest, ""),
    };
    let (repository, query) = match before_hash.find('?') {
        Some(index) => (&before_hash[..index], &before_hash[index + 1..]),
        None => (before_hash, ""),
    };
    (repository, query, fragment)
}

fn package_name(entry: &toml::Value) -> Option<&str> {
    entry.get("name").and_then(|v| v.as_str())
}

fn package_version(entry: &toml::Value) -> Option<&str> {
    entry.get("version").and_then(|v| v.as_str())
}

fn package_source(entry: &toml::Value) -> &str {
    entry.get("source").and_then(|v| v.as_str()).unwrap_or("")
}

/// Pure lock rule, testable without files. The `Err` message is the text after
/// `check-first-party: `.
pub fn check_lock(
    packages: &[toml::Value],
    fleet: &Fleet,
    config: &Config,
) -> std::result::Result<(), String> {
    let retained_names: BTreeSet<&str> = config
        .retained
        .iter()
        .map(|entry| entry.package.as_str())
        .collect();
    let mut names = BTreeSet::new();
    for package in packages {
        let Some(name) = package_name(package) else {
            continue;
        };
        let source = package_source(package);
        if fleet.classify(source).is_some() || retained_names.contains(name) {
            names.insert(name);
        }
    }
    for name in names {
        let entries: Vec<&toml::Value> = packages
            .iter()
            .filter(|package| package_name(package) == Some(name))
            .collect();
        if entries.len() != 1 {
            return Err(format!("Duplicate first-party crate: {name}"));
        }
        let package = entries[0];
        let raw = package_source(package);
        if config.canonical_only && matches!(fleet.classify(raw), Some(Origin::Alias { .. })) {
            return Err(format!(
                "First-party source must use its canonical owner URL: {raw}"
            ));
        }
        let version = package_version(package);
        let mut retained_match = false;
        for entry in config.retained.iter().filter(|e| e.package == name) {
            if let Some(expected) = entry.version.as_deref() {
                if version != Some(expected) {
                    continue;
                }
            }
            if !raw.starts_with("git+") {
                continue;
            }
            let (repository, query, fragment) = split_source(raw);
            if repository == entry.repository
                && query == format!("branch={}", entry.branch)
                && is_hex40(fragment)
            {
                retained_match = true;
                break;
            }
        }
        if retained_match {
            continue;
        }
        let Some(_) = fleet.classify(raw) else {
            let (_, _, fragment) = split_source(raw);
            if !is_hex40(fragment) {
                return Err(format!("Missing resolved revision for {name}"));
            }
            return Err(format!("Invalid first-party source selector for {name}"));
        };
        let (_, query, fragment) = split_source(raw);
        if !is_hex40(fragment) {
            return Err(format!("Missing resolved revision for {name}"));
        }
        if query == "branch=main" {
            continue;
        }
        if config.rev_selectors && query == format!("rev={fragment}") {
            continue;
        }
        return Err(format!("Invalid first-party source selector for {name}"));
    }
    Ok(())
}

fn check_manifest_inner(
    value: &toml::Value,
    fleet: &Fleet,
    config: &Config,
    alias: Option<&str>,
) -> std::result::Result<(), String> {
    if let Some(table) = value.as_table() {
        if let Some(git) = table.get("git").and_then(|v| v.as_str()) {
            let package = table
                .get("package")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| alias.unwrap_or(""));
            // Versioned retained entries are lock-only exceptions: manifests
            // still declare main. Unversioned retained entries constrain both.
            let retained = config.retained.iter().find(|entry| {
                entry.version.is_none() && entry.repository == git && entry.package == package
            });
            if let Some(entry) = retained {
                let branch = table.get("branch").and_then(|v| v.as_str());
                if branch != Some(entry.branch.as_str())
                    || table.contains_key("rev")
                    || table.contains_key("tag")
                {
                    return Err(format!(
                        "First-party dependency must declare branch = \"{}\"",
                        entry.branch
                    ));
                }
            } else if fleet.classify(git).is_some() {
                if config.canonical_only
                    && matches!(fleet.classify(git), Some(Origin::Alias { .. }))
                {
                    return Err(format!(
                        "First-party source must use its canonical owner URL: {git}"
                    ));
                }
                let branch = table.get("branch").and_then(|v| v.as_str());
                if branch != Some("main") || table.contains_key("rev") || table.contains_key("tag")
                {
                    return Err("First-party dependency must declare branch = \"main\"".to_owned());
                }
            }
        }
        for (key, item) in table {
            check_manifest_inner(item, fleet, config, Some(key))?;
        }
        return Ok(());
    }
    if let Some(items) = value.as_array() {
        for item in items {
            check_manifest_inner(item, fleet, config, None)?;
        }
    }
    Ok(())
}

/// Pure manifest rule, testable without files. The `Err` message is the text
/// after `check-first-party: `.
pub fn check_manifest(
    value: &toml::Value,
    fleet: &Fleet,
    config: &Config,
) -> std::result::Result<(), String> {
    check_manifest_inner(value, fleet, config, None)
}

fn git_files(root: &Path) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
        .arg("ls-files")
        .arg("-z")
        .current_dir(root)
        .output()
        .map_err(|error| failure(format!("cannot run git ls-files: {error}")))?;
    if !output.status.success() {
        return Err(failure(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|error| failure(format!("git ls-files is not UTF-8: {error}")))?;
    let mut files = Vec::new();
    for name in text.split('\0') {
        if name.is_empty() {
            continue;
        }
        let path = PathBuf::from(name);
        if path
            .file_name()
            .is_some_and(|n| n == "Cargo.toml" || n == "Cargo.lock")
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn walk_files(root: &Path, skip: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        let absolute = root.join(&relative);
        let entries = fs::read_dir(&absolute)
            .map_err(|error| failure(format!("cannot read {}: {error}", absolute.display())))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| failure(format!("cannot read {}: {error}", absolute.display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = entry
                .file_type()
                .map_err(|error| failure(format!("cannot stat {}: {error}", absolute.display())))?;
            if file_type.is_dir() {
                if name == ".git"
                    || name == "target"
                    || name == "node_modules"
                    || skip.contains(&name)
                {
                    continue;
                }
                stack.push(relative.join(&name));
            } else if file_type.is_file() && (name == "Cargo.toml" || name == "Cargo.lock") {
                out.push(relative.join(&name));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Run the first-party check. `Ok(code)` is the process exit code; refusals
/// print `check-first-party: <message> (<path>)` and return `Ok(1)`.
pub fn run(options: &Options) -> Result<u8> {
    if options.self_test {
        println!("first-party: the self-test is ccid's own test suite");
        return Ok(0);
    }
    if options.metadata.is_some() {
        return Err(failure(
            "--metadata is not supported yet (first-party graph lane)",
        ));
    }
    let environment: crate::Environment = std::env::vars_os().collect();
    let (fleet, _) = crate::fleet::load(&environment)?;
    crate::fleet::require_keyed(&environment, &fleet)?;
    let root = options.repo.canonicalize().map_err(|error| {
        failure(format!(
            "cannot resolve repository {}: {error}",
            options.repo.display()
        ))
    })?;
    let manifest_path = root.join(&options.manifest);
    let bytes = fs::read(&manifest_path).map_err(|error| {
        failure(format!(
            "cannot read manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| failure(format!("cannot parse manifest: {error}")))?;
    let parsed: FileManifest =
        toml::from_str(text).map_err(|error| failure(format!("cannot parse manifest: {error}")))?;
    let config = parsed.first_party.unwrap_or_default();
    config.validate()?;
    let mut files = if !options.archive && root.join(".git").exists() {
        git_files(&root)?
    } else {
        walk_files(&root, &config.skip)?
    };
    if config.require_root_lock
        && (!files.iter().any(|p| p == Path::new("Cargo.toml"))
            || !files.iter().any(|p| p == Path::new("Cargo.lock")))
    {
        let missing = if !files.iter().any(|p| p == Path::new("Cargo.toml")) {
            "Cargo.toml"
        } else {
            "Cargo.lock"
        };
        eprintln!("check-first-party: Missing committed root manifest or lock ({missing})");
        return Ok(1);
    }
    if config.lock_per_manifest {
        let mut extra = Vec::new();
        for manifest in files
            .iter()
            .filter(|p| p.file_name().is_some_and(|n| n == "Cargo.toml"))
        {
            let lock = manifest
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.join("Cargo.lock"))
                .unwrap_or_else(|| PathBuf::from("Cargo.lock"));
            if !root.join(&lock).is_file() {
                eprintln!(
                    "check-first-party: Missing resolved snapshot: {} ({})",
                    lock.display(),
                    lock.display()
                );
                return Ok(1);
            }
            if !files.contains(&lock) {
                extra.push(lock);
            }
        }
        files.extend(extra);
        files.sort();
    }
    let mut manifests = BTreeMap::new();
    let mut locks = BTreeMap::new();
    for path in &files {
        if path.file_name().is_some_and(|n| n == "Cargo.toml") {
            manifests.insert(
                path.clone(),
                fs::read(root.join(path))
                    .map_err(|error| failure(format!("cannot read {}: {error}", path.display())))?,
            );
        } else if !options.manifest_only {
            locks.insert(
                path.clone(),
                fs::read(root.join(path))
                    .map_err(|error| failure(format!("cannot read {}: {error}", path.display())))?,
            );
        }
    }
    for (path, bytes) in &manifests {
        let text = std::str::from_utf8(bytes)
            .map_err(|error| failure(format!("cannot parse {}: {error}", path.display())))?;
        let value: toml::Value = toml::from_str(text)
            .map_err(|error| failure(format!("cannot parse {}: {error}", path.display())))?;
        if let Err(message) = check_manifest(&value, &fleet, &config) {
            eprintln!("check-first-party: {message} ({})", path.display());
            return Ok(1);
        }
    }
    if !options.manifest_only {
        for (path, bytes) in &locks {
            let text = std::str::from_utf8(bytes)
                .map_err(|error| failure(format!("cannot parse {}: {error}", path.display())))?;
            let value: toml::Value = toml::from_str(text)
                .map_err(|error| failure(format!("cannot parse {}: {error}", path.display())))?;
            let packages = match value.get("package") {
                None => Vec::new(),
                Some(toml::Value::Array(items)) => items.clone(),
                Some(_) => {
                    return Err(failure(format!(
                        "cannot parse {}: package is not an array",
                        path.display()
                    )));
                }
            };
            if let Err(message) = check_lock(&packages, &fleet, &config) {
                eprintln!("check-first-party: {message} ({})", path.display());
                return Ok(1);
            }
        }
    }
    println!("First-party declarations follow main; locked crate revisions are unique");
    Ok(0)
}

#[cfg(test)]
mod tests;

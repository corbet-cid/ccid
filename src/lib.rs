#![forbid(unsafe_code)]

use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs, io,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub const SOURCE_REVISION: &str = env!("CCID_SOURCE_REVISION");
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub type Environment = BTreeMap<OsString, OsString>;

mod admission;
mod budget;
mod cache;
mod cached;
mod checks;
mod dependency;
pub mod jobs;
pub mod push;
pub mod render;
mod runner;
mod source;
pub mod tor;

use budget::positive;
pub use budget::{budget, Budget};
pub use cached::run_cached;
pub use checks::cargo_commands;
use checks::{cargo_prefix, javascript_commands, nix_check, validate_check};
pub use dependency::resolve_cargo;
pub use runner::Runner;
use source::safe_relative;
pub use source::{sha256_file, verify_source};

fn failure(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::other(message.into()).into()
}
fn event(value: serde_json::Value) {
    println!("{value}");
    append_receipt(&value);
}

/// Optional machine-readable copy of every event, enabled by `CCID_RECEIPT=<file>`.
/// `ccid cached` declares this file as a cached task output, so a result restored
/// from the cache still carries the receipt of the run that produced it.
fn append_receipt(value: &serde_json::Value) {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    static RECEIPT: OnceLock<Option<Mutex<fs::File>>> = OnceLock::new();
    let file = RECEIPT.get_or_init(|| {
        let path = std::env::var_os("CCID_RECEIPT").filter(|p| !p.is_empty())?;
        let path = Path::new(&path);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = fs::create_dir_all(parent);
        }
        match fs::OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => Some(Mutex::new(file)),
            Err(error) => {
                eprintln!("ccid: cannot open CCID_RECEIPT {}: {error}", path.display());
                None
            }
        }
    });
    if let Some(file) = file {
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{value}");
        }
    }
}
fn value(environment: &Environment, name: &str) -> Option<String> {
    environment
        .get(&OsString::from(name))
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
}
fn set(environment: &mut Environment, name: &str, v: impl Into<OsString>) {
    environment.insert(name.into(), v.into());
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}

fn validate_command(argv: &[String]) -> Result<()> {
    if argv.first().is_none_or(String::is_empty) || argv.iter().any(|arg| arg.contains('\0')) {
        return Err(failure(
            "Commands require a nonempty executable and arguments without NUL bytes",
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    project: String,
    checks: BTreeMap<String, Check>,
    #[serde(default)]
    jobs: BTreeMap<String, jobs::Job>,
    #[serde(default)]
    render: Option<render::Config>,
    /// Dependency push fan-out declarations, keyed by local name. Each entry
    /// renders a push adapter in THIS repository that runs the consumer's
    /// repository-owned job with this event commit pinned as expected.
    #[serde(default)]
    push_consumer: BTreeMap<String, render::PushConsumer>,
    /// Canonical forge URL of this repository, used to bake self push
    /// adapters. Optional; push adapter rendering requires it.
    #[serde(default)]
    repository: Option<String>,
}
#[derive(Debug, Deserialize, serde::Serialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Check {
    kind: String,
    actions: Option<Vec<String>>,
    toolchain: Option<String>,
    workspace: bool,
    all_features: bool,
    all_targets: bool,
    release: bool,
    features: Vec<String>,
    packages: Vec<String>,
    exclude: Vec<String>,
    test_runner: Option<String>,
    mode: Option<String>,
    expected_checks: Option<Vec<String>>,
    checks: Vec<String>,
    manager: Option<String>,
    scripts: Option<Vec<String>>,
    install: Option<bool>,
    commands: Vec<Vec<String>>,
    cache_outputs: Vec<String>,
    cache_tools: Vec<Vec<String>>,
    cache_env: Vec<String>,
    cache_commit: bool,
    cache_inputs: Option<Vec<String>>,
}
/// Read and validate the manifest header; returns the parsed manifest and its exact bytes.
fn load_manifest(root: &Path, manifest: &Path) -> Result<(Manifest, Vec<u8>)> {
    let bytes = fs::read(root.join(manifest))?;
    let parsed: Manifest = toml::from_str(std::str::from_utf8(&bytes)?)?;
    let slug = &parsed.project;
    if parsed.schema != 1
        || slug.is_empty()
        || !slug.as_bytes()[0].is_ascii_alphanumeric()
        || !slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(failure(
            "Manifest requires schema=1 and a plain project slug",
        ));
    }
    Ok((parsed, bytes))
}

/// Resolve comma-separated selectors to declared check names and validate the
/// complete selection before any check can execute. Planning and execution
/// share the same command builders and static rules.
fn select_checks(manifest: &Manifest, selectors: &[String]) -> Result<Vec<String>> {
    let mut selected = Vec::new();
    for selector in selectors {
        for name in selector.split(',').filter(|s| !s.is_empty()) {
            if !selected.contains(&name.to_owned()) {
                selected.push(name.to_owned());
            }
        }
    }
    if selected.is_empty()
        || selected
            .iter()
            .any(|name| !manifest.checks.contains_key(name))
    {
        return Err(failure("Select one or more declared nonempty check names"));
    }
    for name in &selected {
        validate_check(&manifest.checks[name])
            .map_err(|error| failure(format!("Invalid check {name}: {error}")))?;
    }
    Ok(selected)
}

pub fn run_checks(repo: &Path, manifest: &Path, selectors: &[String], plan: bool) -> Result<()> {
    run_checks_inner(repo, manifest, selectors, plan, CheckContext::default())
}

pub(crate) fn run_checks_with_environment(
    repo: &Path,
    manifest: &Path,
    selectors: &[String],
    plan: bool,
    environment: Environment,
    verified_commit: &str,
    deadline: Instant,
) -> Result<()> {
    run_checks_inner(
        repo,
        manifest,
        selectors,
        plan,
        CheckContext {
            verified_commit: Some(verified_commit),
            base_environment: Some(environment),
            enclosing_deadline: Some(deadline),
            stable_archive: false,
        },
    )
}

pub fn run_archive_checks(
    archive: &Path,
    digest: &str,
    commit: &str,
    manifest: &Path,
    selectors: &[String],
    plan: bool,
) -> Result<()> {
    safe_relative(manifest)?;
    let mut environment: Environment = std::env::vars_os().collect();
    cache::repository_identity(Path::new("."), &environment, true)?;
    let source = cache::scratch(&mut environment)?;
    verify_source(archive, digest, commit, source.path())?;
    run_checks_inner(
        source.path(),
        manifest,
        selectors,
        plan,
        CheckContext {
            verified_commit: Some(commit),
            stable_archive: true,
            ..CheckContext::default()
        },
    )
}

#[derive(Default)]
struct CheckContext<'a> {
    verified_commit: Option<&'a str>,
    base_environment: Option<Environment>,
    enclosing_deadline: Option<Instant>,
    stable_archive: bool,
}

fn run_checks_inner(
    repo: &Path,
    manifest: &Path,
    selectors: &[String],
    plan: bool,
    context: CheckContext<'_>,
) -> Result<()> {
    let CheckContext {
        verified_commit,
        base_environment,
        enclosing_deadline,
        stable_archive,
    } = context;
    let archive = verified_commit.is_some();
    let root = repo.canonicalize()?;
    let (manifest, bytes) = load_manifest(&root, manifest)?;
    let slug = &manifest.project;
    let selected = select_checks(&manifest, selectors)?;
    let mut environment = base_environment.unwrap_or_else(|| std::env::vars_os().collect());
    if let Some(commit) = verified_commit {
        set(&mut environment, "CI_COMMIT_SHA", commit);
    }
    let resources = budget(&environment)?;
    let requested_deadline = Instant::now()
        .checked_add(Duration::from_secs(resources.timeout))
        .ok_or_else(|| failure("Check deadline is out of range"))?;
    let deadline = enclosing_deadline.map_or(requested_deadline, |deadline| {
        deadline.min(requested_deadline)
    });
    let linker = value(&environment, "CI_LINKER").unwrap_or_else(|| "system".into());
    if !["system", "mold"].contains(&linker.as_str()) {
        return Err(failure("CI_LINKER must be system or mold"));
    }
    let identity = cache::repository_identity(&root, &environment, archive)?;
    let target = cache::target_directory(&root, &identity, &environment)?;
    event(
        json!({"event":"plan", "project":slug,"checks":selected,"budget":resources,"linker_request":linker,"repository":identity,"target_directory":target,"source_commit":value(&environment,"CI_COMMIT_SHA"),"manifest_sha256":format!("{:x}",Sha256::digest(&bytes)),"tool_revision":SOURCE_REVISION}),
    );
    if plan {
        return Ok(());
    }
    if cfg!(all(windows, not(feature = "windows-experimental"))) {
        return Err(failure("Windows check execution requires an experimental native build; process-tree cancellation remains unverified"));
    }
    let (target, _lock) = cache::lock_target(&target, deadline)?;
    admission::admit(&mut environment)?;
    let resources = budget(&environment)?;
    set(
        &mut environment,
        "CARGO_BUILD_JOBS",
        resources.jobs.to_string(),
    );
    set(
        &mut environment,
        "RUST_TEST_THREADS",
        resources.test_threads.to_string(),
    );
    set(&mut environment, "CARGO_TARGET_DIR", target.as_os_str());
    let nix_config = format!(
        "{}\nmax-jobs = {}\ncores = {}\n",
        value(&environment, "NIX_CONFIG").unwrap_or_default(),
        resources.nix_jobs,
        resources.nix_cores
    );
    set(&mut environment, "NIX_CONFIG", nix_config);
    event(json!({"event":"allocation", "budget":resources}));
    if let Some(build) = value(&environment, "CARGO_BUILD_BUILD_DIR") {
        if root.join(build).canonicalize().ok().as_ref() != Some(&target) {
            return Err(failure("A distinct CARGO_BUILD_BUILD_DIR is unsupported: intermediates must share the locked target directory"));
        }
    }
    set(&mut environment, "CARGO_TARGET_DIR", target.as_os_str());
    set(
        &mut environment,
        "CCID_TARGET_LOCK_HELD",
        target.as_os_str(),
    );
    // The verified archive has no mutable checkout or untracked inputs. Give it
    // a stable canonical path only while holding the actual Cargo target lock.
    // Resolver candidates retain their original path for post-check auditing.
    let stable_source = if stable_archive {
        Some(cache::StableSource::prepare(&root, &target)?)
    } else {
        None
    };
    let root = stable_source
        .as_ref()
        .map_or(root, |source| source.path().to_owned());
    let _scratch = cache::scratch(&mut environment)?;
    let freshness = if archive {
        Some(cache::Freshness::prepare(&root, &target, &identity)?)
    } else {
        // A local check may compile uncommitted inputs into this same target.
        cache::Freshness::invalidate(&target)?;
        None
    };
    let mut runner = Runner::until(root, environment, deadline)?;
    for name in selected {
        let check = &manifest.checks[&name];
        let started = Instant::now();
        event(json!({"event":"check-start","check":name}));
        match check.kind.as_str() {
            "cargo" => {
                let (cargo, rustc) = cargo_prefix(check, &runner)?;
                let commands = cargo_commands(check, &cargo, &resources.test_threads.to_string())?;
                runner.run(&rustc, false)?;
                for command in commands {
                    runner.run(&command, false)?;
                }
            }
            "nix" => nix_check(check, &mut runner)?,
            "javascript" => {
                for command in javascript_commands(check)? {
                    runner.run(&command, false)?;
                }
            }
            "commands" => {
                runner.nix_inventory = None;
                if check.commands.is_empty() {
                    return Err(failure("Custom command selection is empty"));
                }
                for command in &check.commands {
                    runner.run(command, false)?;
                }
            }
            _ => return Err(failure(format!("Unknown check kind: {}", check.kind))),
        }
        event(
            json!({"event":"check-success","check":name,"seconds":started.elapsed().as_secs_f64()}),
        );
    }
    if INTERRUPTED.load(Ordering::SeqCst) {
        return Err(failure(
            "Check interrupted before publishing freshness metadata",
        ));
    }
    if let Some(freshness) = freshness {
        freshness.complete()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(all(windows, feature = "windows-experimental"))]
mod windows;

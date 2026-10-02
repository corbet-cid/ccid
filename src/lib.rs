#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs, io,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

pub const SOURCE_REVISION: &str = env!("CCID_SOURCE_REVISION");
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub type Environment = BTreeMap<OsString, OsString>;

mod admission;
mod cache;
mod checks;
mod dependency;
pub mod forge;
mod runner;
mod source;

pub use checks::cargo_commands;
use checks::{cargo_prefix, javascript_commands, nix_check, validate_check};
pub use dependency::{resolve_cargo, resolve_inventory};
pub use runner::Runner;
use source::safe_relative;
pub use source::{sha256_file, verify_source};

fn failure(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::other(message.into()).into()
}
fn event(value: serde_json::Value) {
    println!("{value}");
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
fn positive(v: &str, name: &str) -> Result<u64> {
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) || v.starts_with('0') {
        return Err(failure(format!("{name} must be a positive integer")));
    }
    Ok(v.parse()?)
}
fn cpu_budget() -> u64 {
    let available = thread::available_parallelism().map_or(1, |n| n.get() as u64);
    if let Ok(quota) = fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        let fields: Vec<_> = quota.split_whitespace().collect();
        if fields.first() == Some(&"max") {
            return available;
        }
        if let [q, p] = fields.as_slice() {
            if let (Ok(q), Ok(p)) = (q.parse::<u64>(), p.parse::<u64>()) {
                if let Some(jobs) = q.checked_div(p) {
                    return available.min(jobs.max(1));
                }
            }
        }
    }
    available.clamp(1, 4)
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Budget {
    pub jobs: u64,
    pub test_threads: u64,
    pub nix_jobs: u64,
    pub nix_cores: u64,
    pub memory_mb: Option<u64>,
    pub timeout: u64,
}
pub fn budget(environment: &Environment) -> Result<Budget> {
    let jobs = value(environment, "CI_JOBS").or_else(|| value(environment, "CARGO_BUILD_JOBS"));
    let mut jobs = positive(&jobs.unwrap_or_else(|| cpu_budget().to_string()), "CI_JOBS")?;
    let memory = value(environment, "CI_MEMORY_MB")
        .map(|v| positive(&v, "CI_MEMORY_MB"))
        .transpose()?;
    if let Some(per_job) = value(environment, "CI_MEMORY_PER_JOB_MB") {
        let per_job = positive(&per_job, "CI_MEMORY_PER_JOB_MB")?;
        let allocation =
            memory.ok_or_else(|| failure("CI_MEMORY_PER_JOB_MB requires CI_MEMORY_MB"))?;
        if allocation < per_job {
            return Err(failure("Memory allocation cannot fit one job"));
        }
        jobs = jobs.min(allocation / per_job);
    }
    let nix_jobs = positive(
        &value(environment, "CI_NIX_JOBS").unwrap_or_else(|| "1".into()),
        "CI_NIX_JOBS",
    )?;
    if nix_jobs > jobs {
        return Err(failure("CI_NIX_JOBS exceeds the allocated CPU budget"));
    }
    Ok(Budget {
        jobs,
        test_threads: positive(
            &value(environment, "CI_TEST_THREADS").unwrap_or_else(|| jobs.to_string()),
            "CI_TEST_THREADS",
        )?,
        nix_jobs,
        nix_cores: jobs / nix_jobs,
        memory_mb: memory,
        timeout: positive(
            &value(environment, "CI_TIMEOUT").unwrap_or_else(|| "2700".into()),
            "CI_TIMEOUT",
        )?,
    })
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
}
#[derive(Debug, Deserialize, Default)]
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
    let manifest_path = root.join(manifest);
    let bytes = fs::read(&manifest_path)?;
    let manifest: Manifest = toml::from_str(std::str::from_utf8(&bytes)?)?;
    let slug = &manifest.project;
    if manifest.schema != 1
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
    // Validate the complete selection before any earlier check can execute.
    // Planning and execution share the same command builders and static rules.
    for name in &selected {
        validate_check(&manifest.checks[name])
            .map_err(|error| failure(format!("Invalid check {name}: {error}")))?;
    }
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

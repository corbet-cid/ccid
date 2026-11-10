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

/// The shared build profile (profiles/dev.toml) as a cargo config fragment. See the file for why it is config and never CARGO_PROFILE_* environment.
pub const DEV_PROFILE: &str = include_str!("../profiles/dev.toml");
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub mod submission;
pub type Environment = BTreeMap<OsString, OsString>;

mod admission;
mod budget;
mod cache;
mod cached;
mod cached_outputs;
mod cached_verdict;
pub mod check_shell;
mod checks;
pub mod compile_cache;
pub mod content_pins;
pub mod coverage;
mod dependency;
pub mod fleet;
#[cfg(unix)]
pub mod job_run;
pub mod jobs;
pub mod push;
pub mod render;
pub mod resolver;
mod runner;
pub mod rustc_diag;
mod source;
#[cfg(unix)]
pub mod toolchain;
pub mod tor;
pub(crate) mod tor_inputs;
pub mod typescript_cache;
pub mod verdict;

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
    /// Opt in to the one rendered check-shell. Presence of the table enables
    /// it; an older ccid (which does not know the key) still parses the
    /// manifest because this struct ignores unknown fields.
    #[serde(default)]
    check_shell: Option<render::CheckShell>,
    /// Dependency push fan-out declarations, keyed by local name. Each entry
    /// renders a push adapter in THIS repository that runs the consumer's
    /// repository-owned job with this event commit pinned as expected.
    #[serde(default)]
    push_consumer: BTreeMap<String, render::PushConsumer>,
    /// Canonical forge URL of this repository, used to bake self push
    /// adapters. Optional; push adapter rendering requires it.
    #[serde(default)]
    repository: Option<String>,
    /// Which jobs gate landing (see `verdict`). Absent: `verify`, if declared.
    #[serde(default)]
    verdict: Option<Verdict>,
    /// Coverage exclusions for the emitted-source gate. Nothing consumes this
    /// field yet: the coverage command reads the same table through a partial
    /// probe so that manifests without a check section keep working. Adding
    /// the key is still a rollout event: an older ccid refuses it because
    /// this struct denies unknown fields.
    #[serde(default)]
    #[allow(dead_code)]
    coverage: Option<coverage::Config>,
    /// Locked content pins checked by `ccid content-pins`.
    #[serde(default)]
    #[allow(dead_code)]
    content_pins: Vec<content_pins::Pin>,
}
/// The jobs whose success is the commit's aggregated verdict; every other job
/// (release, publish, scheduled gates) reports under its own non-gating context.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Verdict {
    jobs: Vec<String>,
}
#[derive(Debug, Deserialize, serde::Serialize, Default, Clone)]
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
    cache: Option<bool>,
    /// Deterministic checks are pure by default; `false` opts a check that
    /// observes the network or the clock out of result caching. It decides
    /// whether a result is cached, never what the result is, so it is not
    /// serialized into any key.
    #[serde(skip_serializing)]
    cache_pure: Option<bool>,
    /// The check reads the release channel's fleet data: its keys include the fleet digest and it refuses to run without fleet data.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    fleet: bool,
}
/// Read and validate the manifest header; returns the parsed manifest and its exact bytes.
fn load_manifest(root: &Path, manifest: &Path) -> Result<(Manifest, Vec<u8>)> {
    let bytes = fs::read(root.join(manifest))?;
    let mut parsed: Manifest = toml::from_str(std::str::from_utf8(&bytes)?)?;
    // `fleet = true` keys the fleet digest through the existing cache_env path (verdict store and moon).
    fleet::normalise(&mut parsed.checks)?;
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
    let manifest_path = manifest;
    let (manifest, bytes) = load_manifest(&root, manifest)?;
    let slug = &manifest.project;
    let selected = select_checks(&manifest, selectors)?;
    let mut environment = base_environment.unwrap_or_else(|| std::env::vars_os().collect());
    if let Some(commit) = verified_commit {
        set(&mut environment, "CI_COMMIT_SHA", commit);
    }
    if value(&environment, "CCID_CACHE_REPLAY_ONLY").as_deref() == Some("1") {
        return Err(failure(
            "Completed result unavailable from cache; refusing duplicate computation",
        ));
    }
    let cache_child = value(&environment, "CCID_CACHE_CHILD").as_deref() == Some("1");
    if cache_child {
        // Moon adds task/workspace/run metadata after cmnp filters the parent
        // environment. It is orchestration context, never a check input.
        environment.retain(|key, _| !key.to_string_lossy().starts_with("MOON_"));
    }
    // ccid records its receipt from the process environment. Checked
    // programs must not inherit it: nested command libraries could write
    // generated receipt files into the otherwise immutable source tree.
    for key in [
        "CCID_RECEIPT",
        "CCID_TOOL_IDENTITY",
        "CCID_CACHE_CHILD",
        "CCID_CACHE_REPLAY_ONLY",
    ] {
        environment.remove(std::ffi::OsStr::new(key));
    }
    // Before any key is computed or any check runs: a `fleet = true` check fails closed without
    // fleet data, and every key of this selection sees the digest of the data it will read.
    fleet::bind_selection(&manifest.checks, &selected, &mut environment, cache_child)?;
    if !cache_child && value(&environment, "CCID_RESULT_CACHE").is_some() {
        let requested = Instant::now()
            .checked_add(Duration::from_secs(budget(&environment)?.timeout))
            .ok_or_else(|| failure("Check deadline is out of range"))?;
        let deadline = enclosing_deadline.map_or(requested, |limit| limit.min(requested));
        let mut measurements: Vec<serde_json::Value> = Vec::new();
        let mut bypassed = Vec::new();
        // All entrypoints (archive, manual jobs, push and direct checks) converge
        // here, before taking the Cargo target lock. Cache misses re-enter once.
        for name in &selected {
            let remaining = deadline.saturating_duration_since(Instant::now()).as_secs();
            if remaining == 0 {
                return Err(failure("Check deadline expired before cache transaction"));
            }
            set(&mut environment, "CI_TIMEOUT", remaining.to_string());
            let check = &manifest.checks[name];
            if checks::cache_eligible(check) {
                cached::run_cached_with_environment(
                    repo,
                    manifest_path,
                    std::slice::from_ref(name),
                    plan,
                    false,
                    environment.clone(),
                )?;
                if !plan {
                    let metrics: serde_json::Value =
                        serde_json::from_slice(&fs::read(root.join(".ccid/cache-metrics.json"))?)?;
                    measurements.extend(
                        metrics["checks"]
                            .as_array()
                            .ok_or_else(|| failure("Missing check cache measurements"))?
                            .iter()
                            .cloned(),
                    );
                    if let Some(items) = metrics["bypassed"].as_array() {
                        bypassed.extend(
                            items
                                .iter()
                                .filter_map(|item| item["check"].as_str())
                                .map(str::to_owned),
                        );
                    }
                }
            } else {
                bypassed.push(name.clone());
                event(
                    json!({"event":"cache-bypass","check":name,"reason":"check opts out of result caching or its kind is not cacheable"}),
                );
                let mut direct = environment.clone();
                direct.remove(std::ffi::OsStr::new("CCID_RESULT_CACHE"));
                run_checks_inner(
                    repo,
                    manifest_path,
                    std::slice::from_ref(name),
                    plan,
                    CheckContext {
                        verified_commit,
                        base_environment: Some(direct),
                        enclosing_deadline: Some(deadline),
                        stable_archive,
                    },
                )?;
            }
        }
        if !plan {
            let hits = measurements
                .iter()
                .filter(|check| check["computed"] == 0)
                .count();
            let requests = measurements.len();
            let summary = json!({"event":"cache-selection","checks":measurements,"bypassed":bypassed,"result":{"hits":hits,"requests":requests,"hit_rate":if requests == 0 { None } else { Some(hits as f64 / requests as f64) }},"compile":null,"nix_eval":null,"fetch":null});
            fs::create_dir_all(root.join(".ccid"))?;
            fs::write(
                root.join(".ccid/cache-metrics.json"),
                serde_json::to_vec_pretty(&summary)?,
            )?;
            event(summary);
        }
        return Ok(());
    }
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
    // Caller-declared compiler environment remains semantic. Only our own
    // storage routing may move to Cargo's CLI, outside rustc/sccache env keys.
    let caller_target = value(&environment, "CARGO_TARGET_DIR").is_some()
        || value(&environment, "CARGO_BUILD_TARGET_DIR").is_some();
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
    // The runner's canonical Rust homes win over whatever the calling script exported: one rustup/cargo home per machine means one toolchain chain and one registry source
    // path, which a compile-cache key contains. The runner names them CCID_RUSTUP_HOME / CCID_CARGO_HOME; a script's own per-repository values become dead weight.
    cache::apply_canonical_rust_homes(&mut environment);
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
    let output_root = root.clone();
    let stable_cargo = cache_child
        && selected
            .iter()
            .all(|name| manifest.checks[name].kind == "cargo");
    let stable_source = if stable_cargo {
        Some(cache::StableSource::prepare_cached(&root, &target)?)
    } else if stable_archive {
        Some(cache::StableSource::prepare(&root, &target)?)
    } else {
        None
    };
    let root = stable_source
        .as_ref()
        .map_or(root, |source| source.path().to_owned());
    let _stable_scratch = if stable_cargo {
        Some(cache::StableSource::scratch(&target, &mut environment)?)
    } else {
        None
    };
    let _scratch = if stable_cargo {
        None
    } else {
        Some(cache::scratch(&mut environment)?)
    };
    let freshness = if archive || stable_cargo {
        Some(cache::Freshness::prepare(&root, &target, &identity)?)
    } else {
        // A local check may compile uncommitted inputs into this same target.
        cache::Freshness::invalidate(&target)?;
        None
    };
    let scratch_for_kache = _stable_scratch
        .as_ref()
        .map(|scratch| scratch.path().to_owned())
        .or_else(|| _scratch.as_ref().map(|scratch| scratch.path().to_owned()));
    let scratch_for_profile = scratch_for_kache.clone();
    if let Some(scratch) = scratch_for_kache {
        cache::prepare_kache_runtime(&mut environment, &scratch);
    }
    compile_cache::prepare_go(&mut environment);
    let _compiler_cache = compile_cache::CompilerCache::prepare(&root, &mut environment);
    let mut runner = {
        // Resolver pre-step at the common verified-source execution point
        // (covers run_checks, run_archive_checks and Crow/Argo adapters).
        // Runs AFTER Q's central cache routing above and AFTER stable
        // source/scratch setup, so manifest/lock scanning reads checked
        // source and installs config into the environment moved into Runner
        // below, BEFORE any git fetch/check commands. jobs::execute reaches
        // the same helper; the APPLIED marker keeps nested jobs->ccid
        // execution to a single installation. Content identity stays
        // semantic: pure cache keys exclude the selected store/jobdir and
        // per-run metadata (see checks::validate_cached_contract); this only
        // rewrites selected transport for declared inputs.
        let remaining = deadline.saturating_duration_since(Instant::now()).as_secs();
        if remaining > 0 {
            let scratch_path = _stable_scratch
                .as_ref()
                .map(|s| s.path())
                .or(_scratch.as_ref().map(|s| s.path()));
            if let Some(scratch_path) = scratch_path {
                crate::resolver::maybe_prepare_runner_env(
                    &mut environment,
                    &root,
                    scratch_path,
                    &[],
                    remaining,
                )?;
            }
        }
        Runner::until(root, environment, deadline)?
    };
    for name in &selected {
        let check = &manifest.checks[name];
        // Fleet consumers accept the data only inside a check that declared it (and was keyed with it).
        if check.fleet {
            set(&mut runner.environment, fleet::FLEET_KEYED_ENV, "1");
        } else {
            runner
                .environment
                .remove(std::ffi::OsStr::new(fleet::FLEET_KEYED_ENV));
        }
        let started = Instant::now();
        event(json!({"event":"check-start","check":name}));
        match check.kind.as_str() {
            "cargo" => {
                let (cargo, rustc) = cargo_prefix(check, &runner)?;
                let mut commands =
                    cargo_commands(check, &cargo, &resources.test_threads.to_string())?;
                let mut cargo_environment = runner.environment.clone();
                if !caller_target {
                    cargo_environment.remove(std::ffi::OsStr::new("CARGO_TARGET_DIR"));
                    // Every compilation action has --locked, before any rustc
                    // argument delimiter. cargo fmt has no target-dir option.
                    for command in &mut commands {
                        if let Some(index) = command.iter().position(|arg| arg == "--locked") {
                            command.splice(
                                index..index,
                                ["--target-dir".into(), target.to_string_lossy().into_owned()],
                            );
                        }
                    }
                }
                // Incremental compilation for every cargo check (cache::incremental_default); its state is never removed.
                if let Some(flag) = cache::incremental_default(&cargo_environment) {
                    set(&mut cargo_environment, "CARGO_INCREMENTAL", flag);
                }
                // The shared profile as a config file argument, unless the job declares profile settings itself (environment) : one mechanism fleet-wide, see profiles/dev.toml.
                let declares_profile = cargo_environment
                    .keys()
                    .any(|name| name.to_string_lossy().starts_with("CARGO_PROFILE_"));
                if !declares_profile {
                    if let Some(scratch) = scratch_for_profile.as_ref() {
                        let file = scratch.join("ccid-dev-profile.toml");
                        if fs::write(&file, DEV_PROFILE).is_ok() {
                            let file = file.to_string_lossy().into_owned();
                            checks::apply_config_file(&mut commands, cargo.len(), &file);
                        }
                    }
                }
                let cargo_runner =
                    Runner::until(runner.root.clone(), cargo_environment, runner.deadline)?;
                runner.run(&rustc, false)?;
                for command in commands {
                    cargo_runner.run(&command, false)?;
                }
            }
            "nix" => nix_check(check, &mut runner)?,
            "javascript" => {
                let mut typescript = None;
                for (index, command) in javascript_commands(check)?.into_iter().enumerate() {
                    if index == usize::from(check.install.unwrap_or(true)) {
                        typescript = typescript_cache::TypeScriptCache::prepare(&mut runner);
                    }
                    runner.run(&command, false)?;
                }
                drop(typescript);
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
    if stable_cargo {
        for name in &selected {
            for output in &manifest.checks[name].cache_outputs {
                safe_relative(Path::new(output))?;
                let source = runner.root.join(output);
                let destination = output_root.join(output);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                if !source.is_file() {
                    return Err(failure("Stable Cargo cache outputs must be explicit files"));
                }
                fs::copy(source, destination)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(all(windows, feature = "windows-experimental"))]
mod windows;

//! Tor milestone orchestration: disposable private network, Records, Snowflake browser.
//!
//! New orchestration logic in safe Rust. Legacy product drivers run unmodified
//! as supervised subprocesses (transitional, pending port):
//! - ctrn `.ci/tor-tools.sh`, `.ci/native-coverage.sh`, `.ci/private-network.py`,
//!   `.ci/browser-build.sh`, `.ci/wasm-tools.sh`, node `.ci/browser-driver.mjs`.
//! - cmsh `.ci/records.sh` (drives the live cdht Records probe; product untouched).
//!
//! Environment contract (all fail closed when absent or malformed):
//! - `CI_COMMIT_SHA`: exact staged main commit (40 lowercase hex).
//! - `CI_PIPELINE_NUMBER`: scheduler pipeline number (evidence namespacing).
//! - `CI_TIMEOUT`: total seconds for the enclosing deadline (default 2700, as budget).
//! - `CI_JOBS`: build/test parallelism (default 2, forwarded to drivers).
//! - `CARGO_HOME`: absolute persistent cache root (evidence + tool caches).
//! - `HOME`: cache-owned tool location (`$HOME/.cache/ctrn-tools`).
//! - `PATH`: provisioned worker tools, including the pinned `ccid`, `google-chrome`
//!   (chromium shim), node, openssl, and the nightly toolchain for coverage legs.
//! - Snowflake only: `STOPGAP_PRODUCT_SOURCE_ARCHIVE` / `_SHA256` (helper-staged
//!   product tar from `.ci/archives.toml`) and `CFRY_SOURCE_BUNDLE` / `_SHA256`
//!   (helper-staged Ferry git bundle). The job's existing `GIT_CONFIG_*`
//!   insteadOf entries are extended, never replaced.
//!
//! No operator Tor gateways or relays are used. The Snowflake fixture reaches the
//! public Tor network through the Tor Project broker and volunteer proxies only
//! (`{"testOnly": false, "snowflake": true}`); the node driver's stdout/stderr
//! stay suppressed and only the structured contract evidence is validated.
use crate::{
    event, failure, sha256_file, tor_inputs, verify_source, Environment, Result, Runner,
    SOURCE_REVISION,
};
use clap::Subcommand;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Stage timeouts in seconds. Dispatch must set `CI_TIMEOUT` at or above the
/// largest stage of the selected job (7200 snowflake, 5400 private/records,
/// 14400 v01-tor-live: the frozen driver caps its fixture at 3600 seconds and
/// resolve/build/probe headroom follows the 291/12-class full-run shape).
const TOOLCHAIN_PROBE_TIMEOUT: u64 = 120;
const GIT_OPERATION_TIMEOUT: u64 = 600;
const TOR_TOOLS_TIMEOUT: u64 = 1800;
const NATIVE_COVERAGE_TIMEOUT: u64 = 5400;
const BROWSER_BUILD_TIMEOUT: u64 = 3600;
const OPENSSL_TIMEOUT: u64 = 300;
const NODE_DRIVER_TIMEOUT: u64 = 1620;
const RECORDS_TIMEOUT: u64 = 5400;
const CARGO_BUILD_TIMEOUT: u64 = 1800;
const V01_TOR_LIVE_TIMEOUT: u64 = 14400;

/// Expected keys of the browser-build environment file. Any deviation fails
/// closed, mirroring the verify job's exact gate on `$GITHUB_ENV`.
const BROWSER_ENV_KEYS: [&str; 3] = ["TORJS_DIST", "TOR_GATEWAY_BIN", "FERRY_BROWSER"];

/// Tor milestone subcommands, one per owning-repository job.
#[derive(Subcommand)]
pub enum Action {
    /// ctrn private Tor network with native coverage (Chutney fixture).
    PrivateNetwork {
        /// Staged source root the drivers run against.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Internal: watch the enclosing executor instead of supervising.
        #[arg(long, hide = true)]
        parent_watch: bool,
    },
    /// cmsh live Records probe over the private Tor network (owns cdht evidence).
    Records {
        /// Staged source root the drivers run against.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Internal: watch the enclosing executor instead of supervising.
        #[arg(long, hide = true)]
        parent_watch: bool,
    },
    /// ctrn public browser contract entering through Snowflake.
    SnowflakeBrowser {
        /// Staged source root (main-owned manifest; product arrives staged).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Internal: watch the enclosing executor instead of supervising.
        #[arg(long, hide = true)]
        parent_watch: bool,
    },
    /// cmsh original nine-onion Records contract on the private Tor network.
    V01TorLive {
        /// Staged source root (main-owned manifest; frozen inputs arrive staged).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Internal: watch the enclosing executor instead of supervising.
        #[arg(long, hide = true)]
        parent_watch: bool,
    },
}

impl Action {
    /// Supervisor re-exec marker, appended after all subcommand arguments.
    pub fn parent_watch(&self) -> bool {
        match self {
            Action::PrivateNetwork { parent_watch, .. }
            | Action::Records { parent_watch, .. }
            | Action::SnowflakeBrowser { parent_watch, .. }
            | Action::V01TorLive { parent_watch, .. } => *parent_watch,
        }
    }

    fn repo(&self) -> &Path {
        match self {
            Action::PrivateNetwork { repo, .. }
            | Action::Records { repo, .. }
            | Action::SnowflakeBrowser { repo, .. }
            | Action::V01TorLive { repo, .. } => repo,
        }
    }
}

/// Dispatch one Tor milestone job from its verified source checkout.
pub fn run(action: &Action) -> Result<()> {
    let environment: Environment = std::env::vars_os().collect();
    let repo = action.repo();
    match action {
        Action::PrivateNetwork { .. } => private_network(repo, &environment),
        Action::Records { .. } => records(repo, &environment),
        Action::SnowflakeBrowser { .. } => snowflake_browser(repo, &environment),
        Action::V01TorLive { .. } => v01_tor_live(repo, &environment),
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(ToString::to_string).collect()
}

fn required(env: &Environment, key: &str) -> Result<String> {
    env.get(&OsString::from(key))
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| failure(format!("Tor job requires non-empty {key}")))
}

fn hex_identity(value: &str, what: &str) -> Result<String> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value.to_owned())
    } else {
        Err(failure(format!(
            "Tor job requires a full commit SHA for {what}"
        )))
    }
}

fn absolute_dir(value: &str, key: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(failure(format!(
            "Tor job requires an absolute path in {key}"
        )))
    }
}

fn global_deadline(env: &Environment) -> Result<Instant> {
    let seconds: u64 = match env
        .get(&OsString::from("CI_TIMEOUT"))
        .and_then(|value| value.to_str())
    {
        None | Some("") => 2700,
        Some(raw) => raw
            .trim()
            .parse()
            .map_err(|_| failure("CI_TIMEOUT must be a positive number of seconds"))?,
    };
    if seconds == 0 {
        return Err(failure("CI_TIMEOUT must be a positive number of seconds"));
    }
    Instant::now()
        .checked_add(Duration::from_secs(seconds))
        .ok_or_else(|| failure("Tor job deadline is out of range"))
}

fn stage_deadline(global: Instant, timeout_secs: u64) -> Result<Instant> {
    let stage = Instant::now()
        .checked_add(Duration::from_secs(timeout_secs))
        .ok_or_else(|| failure("Tor stage deadline is out of range"))?;
    Ok(stage.min(global))
}

#[derive(Debug)]
struct StageOutcome {
    seconds: f64,
}

/// One supervised stage: bounded deadline, owned process-group cleanup,
/// no ignored failures. Nonzero exit or timeout returns a named error.
struct Stage {
    name: &'static str,
    command: Vec<String>,
    workdir: PathBuf,
    extra_env: Vec<(String, String)>,
    timeout_secs: u64,
    capture_stdout: bool,
    mute_stdout: bool,
    mute_stderr: bool,
}

fn run_stage(base: &Environment, global: Instant, stage: Stage) -> Result<(StageOutcome, String)> {
    let mut environment = base.clone();
    for (key, value) in stage.extra_env {
        environment.insert(OsString::from(key.as_str()), OsString::from(value.as_str()));
    }
    let deadline = stage_deadline(global, stage.timeout_secs)?;
    let started = Instant::now();
    let runner = Runner::until(stage.workdir.to_owned(), environment, deadline)?;
    let runner = if stage.mute_stderr {
        runner.without_child_stderr()
    } else {
        runner
    };
    let runner = if stage.mute_stdout {
        runner.without_child_stdout()
    } else {
        runner
    };
    match runner.run(&stage.command, stage.capture_stdout) {
        Ok(output) => {
            let seconds = started.elapsed().as_secs_f64();
            event(json!({"event": "tor-stage", "stage": stage.name, "seconds": seconds}));
            Ok((StageOutcome { seconds }, output))
        }
        Err(error) => {
            let name = stage.name;
            Err(failure(format!("Tor stage {name} failed: {error}")))
        }
    }
}

/// Capture one trusted version probe line.
fn probe(
    base: &Environment,
    global: Instant,
    workdir: &Path,
    command: &[String],
) -> Result<String> {
    let (_, output) = run_stage(
        base,
        global,
        Stage {
            name: "toolchain-probe",
            command: command.to_owned(),
            workdir: workdir.to_owned(),
            extra_env: Vec::new(),
            timeout_secs: TOOLCHAIN_PROBE_TIMEOUT,
            capture_stdout: true,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    Ok(output)
}

fn write_receipt(dir: &Path, value: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| failure(format!("Tor job cannot render receipt: {error}")))?;
    fs::write(dir.join("receipt.json"), text + "\n")?;
    Ok(())
}

fn optional(env: &Environment, key: &str) -> Option<String> {
    env.get(&OsString::from(key))
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Persistent job evidence directory under the worker cache root. The pipeline
/// identifier must be path-safe. Every attempt owns a unique directory that
/// includes the job identity (`.../<commit>/<pipeline>/<job>/attempt-XXXXXX`);
/// no attempt ever reuses another attempt's evidence and no prior data is
/// ever deleted or renamed.
fn evidence_root(env: &Environment, component: &str, job: &str) -> Result<PathBuf> {
    let cargo = absolute_dir(&required(env, "CARGO_HOME")?, "CARGO_HOME")?;
    let commit = hex_identity(&required(env, "CI_COMMIT_SHA")?, "CI_COMMIT_SHA")?;
    let pipeline = required(env, "CI_PIPELINE_NUMBER")?;
    if pipeline.len() > 64
        || !pipeline
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        || pipeline.contains("..")
    {
        return Err(failure("Tor job requires a path-safe CI_PIPELINE_NUMBER"));
    }
    let parent = cargo.join(format!("{component}/evidence/{commit}/{pipeline}/{job}"));
    fs::create_dir_all(&parent)?;
    let attempt = tempfile::Builder::new()
        .prefix("attempt-")
        .tempdir_in(&parent)
        .map_err(|error| failure(format!("Tor job cannot create evidence directory: {error}")))?;
    Ok(attempt.keep())
}

/// Short job-owned Tor scratch under an explicit parent, bypassing the
/// inherited worker-nested `TMPDIR` (`ccid-job-…/nix-shell-…/ccid-job-…`).
/// The nested shape pushes Chutney node dirs past the 108-byte `sun_path`
/// limit (a node dir alone measured 116 chars, `control` 124,
/// `control.authcookie` 135), so every Tor child (Chutney, Chrome) inherits
/// this short root via both `TMPDIR` and `RUNNER_TEMP`. Lifetime stays
/// `TempDir` RAII; evidence roots, `CARGO_HOME`/`RUSTUP_HOME` and the stable
/// tool `HOME` are untouched.
///
/// The leaf carries explicit owner-only permissions: pinned `tempfile`
/// creates tempdirs with default (umask-derived) modes, so without this the
/// leaf would inherit group/other bits from a permissive worker umask and
/// Arti's ownership/permission checks would refuse the state tree before
/// bootstrap. Combined with the restrictive process umask the Chutney jobs
/// install on entry, the leaf is exactly `0o700` on Unix.
fn tor_scratch_in(parent: &Path) -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("ccid-tor-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder
        .tempdir_in(parent)
        .map_err(|error| failure(format!("Tor job cannot create scratch: {error}")))
}

/// General scratch root. The Snowflake browser job keeps this path unchanged.
fn tor_scratch() -> Result<tempfile::TempDir> {
    tor_scratch_in(Path::new("/tmp"))
}

/// Native-job scratch root. The Crow execution namespace mounts `/tmp`
/// without the sticky bit, so Arti's ancestor permission check refuses any
/// tree beneath it; the standard `/var/tmp` (sticky) passes that check while
/// staying just as short for `sun_path`.
fn native_tor_scratch() -> Result<tempfile::TempDir> {
    tor_scratch_in(Path::new("/var/tmp"))
}

/// Group/other permission bits masked out for the Chutney job processes
/// (`private-network`, `records`): with these masked, default-created
/// directories become `0o700` and files `0o600`, keeping job-owned state
/// (Arti directories, fixture temp paths, TLS keys) private to the job user.
/// Snowflake keeps its retired green behavior and never installs this mask.
#[cfg(unix)]
fn tor_process_umask() -> rustix::fs::Mode {
    use rustix::fs::Mode;
    Mode::RGRP | Mode::WGRP | Mode::XGRP | Mode::ROTH | Mode::WOTH | Mode::XOTH
}

/// Install the restrictive [`tor_process_umask`] for this process. Safe
/// `rustix` API, no `unsafe` (forbidden crate-wide). Idempotent: re-entry
/// keeps the mask restricted. Called only from the Chutney job entries above;
/// no RAII restoration, since the only caller chain is the short-lived native
/// CLI child process (`main.rs` is the sole `tor::run` caller).
#[cfg(unix)]
fn restrict_tor_process_umask() {
    rustix::process::umask(tor_process_umask());
}

fn apply_tor_scratch_env(base: &mut Environment, scratch: &tempfile::TempDir) {
    base.insert(
        OsString::from("TMPDIR"),
        scratch.path().as_os_str().to_owned(),
    );
    base.insert(
        OsString::from("RUNNER_TEMP"),
        scratch.path().as_os_str().to_owned(),
    );
}

fn current_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Full `source` field of a locked first-party package (`git+<url>#<rev>`).
fn lock_source(lock_text: &str, package: &str) -> Result<String> {
    let value: toml::Value = toml::from_str(lock_text)
        .map_err(|error| failure(format!("Tor job cannot parse Cargo.lock: {error}")))?;
    let packages = value
        .get("package")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| failure("Cargo.lock has no packages"))?;
    for entry in packages {
        if entry.get("name").and_then(toml::Value::as_str) != Some(package) {
            continue;
        }
        return entry
            .get("source")
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| failure(format!("Cargo.lock package {package} has no source")));
    }
    Err(failure(format!("Cargo.lock has no package {package}")))
}

/// First-party revision pinned by a committed lockfile (`<url>#<sha>` source).
fn lock_rev(lock_text: &str, package: &str) -> Result<String> {
    let source = lock_source(lock_text, package)?;
    let revision = source.split('#').nth(1).ok_or_else(|| {
        failure(format!(
            "Cargo.lock package {package} has no pinned revision"
        ))
    })?;
    hex_identity(revision, package)
}

/// Clone URLs covered by one locked Git source: the canonical URL the builder
/// clones plus its `.git` spelling, so the staged mirror intercepts both
/// without hardcoding any forge or owner in shared code.
fn lock_clone_urls(source: &str) -> Result<Vec<String>> {
    let without_scheme = source
        .strip_prefix("git+")
        .ok_or_else(|| failure("Locked Git source must start with git+"))?;
    let base = without_scheme
        .split(['?', '#'])
        .next()
        .ok_or_else(|| failure("Locked Git source has no URL"))?;
    if base.is_empty() {
        return Err(failure("Locked Git source has no URL"));
    }
    let mut urls = vec![base.to_owned()];
    if !base.ends_with(".git") {
        urls.push(format!("{base}.git"));
    }
    Ok(urls)
}

/// Exact auxiliary revision declared by the staged `.ci/archives.toml`.
fn aux_revision(archives_text: &str, name: &str) -> Result<String> {
    let value: toml::Value = toml::from_str(archives_text)
        .map_err(|error| failure(format!("Tor job cannot parse .ci/archives.toml: {error}")))?;
    let revision = value
        .get("archives")
        .and_then(|archives| archives.get(name))
        .and_then(|entry| entry.get("revision"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| failure(format!(".ci/archives.toml has no revision for {name}")))?;
    hex_identity(revision, name)
}

/// SHA-256 identity of a helper-staged auxiliary file.
fn require_digest(path: &Path, expected: &str) -> Result<()> {
    if !path.is_file() {
        let location = path.display().to_string();
        return Err(failure(format!(
            "Tor job auxiliary source missing: {location}"
        )));
    }
    let actual = sha256_file(path)?;
    if actual != expected.to_lowercase() {
        let location = path.display().to_string();
        return Err(failure(format!(
            "Tor job auxiliary source SHA-256 mismatch: {location}"
        )));
    }
    Ok(())
}

/// Readiness evidence the disposable network must leave behind (fail-closed).
fn require_private_network_evidence(artifact: &Path) -> Result<()> {
    for file in ["shared-random-readiness.json", "listener-ports.json"] {
        let path = artifact.join(file);
        let location = path.display().to_string();
        let text = fs::read_to_string(&path)
            .map_err(|_| failure(format!("Private Tor evidence missing: {location}")))?;
        serde_json::from_str::<Value>(&text)
            .map_err(|_| failure(format!("Private Tor evidence is not JSON: {location}")))?;
    }
    Ok(())
}

/// Exact fixture outputs worth persisting (names observed in the transitional
/// driver; anything else stays in job scratch). Node log tails are 4 KiB each.
fn artifact_kept(name: &str) -> bool {
    matches!(
        name,
        "shared-random-readiness.json"
            | "listener-ports.json"
            | "contract.txt"
            | "consensus-microdesc-start.txt"
            | "consensus-microdesc-readiness.txt"
    ) || name.starts_with("node-status-")
        || name.ends_with("-tor.stdout")
        || name.ends_with("-tor.stderr")
        || name.ends_with("-notice.log")
        || name.ends_with("-info.log")
}

fn persist_private_network_evidence(
    source: &Path,
    destination: &Path,
    strict: bool,
) -> Result<Vec<String>> {
    fs::create_dir_all(destination)?;
    let mut kept = Vec::new();
    let entries = fs::read_dir(source).map_err(|_| {
        let location = source.display().to_string();
        failure(format!("Private Tor artifact missing: {location}"))
    })?;
    for entry in entries {
        let entry =
            entry.map_err(|error| failure(format!("Tor job cannot list artifact: {error}")))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if artifact_kept(&name) {
            fs::copy(entry.path(), destination.join(&name))?;
            kept.push(name);
        }
    }
    kept.sort();
    if strict {
        require_private_network_evidence(destination)?;
    }
    Ok(kept)
}

/// Coverage reports the instrumented legs must leave behind. On the success
/// path a missing report is a failure, never an okay result; on the salvage
/// path whatever exists is copied without judgment.
fn copy_coverage(repo: &Path, evidence: &Path, strict: bool) -> Result<Vec<String>> {
    let mut kept = Vec::new();
    for file in ["coverage.json", "coverage.lcov", "coverage.txt"] {
        let source = repo.join(file);
        if source.is_file() {
            fs::copy(&source, evidence.join(file))?;
            kept.push(file.to_owned());
        } else if strict {
            return Err(failure(format!("Tor job coverage report missing: {file}")));
        }
    }
    Ok(kept)
}

/// Single receipt writer for both outcomes: success records the detail the
/// job proved, failure records the error with the stages reached so far.
/// The original error always propagates; receipt I/O never masks it.
fn finish(
    evidence: &Path,
    env: &Environment,
    job: &str,
    commit: &str,
    stages: Vec<Value>,
    detail: Result<Value>,
) -> Result<()> {
    match detail {
        Ok(detail) => {
            let mut receipt = receipt_base(env, job, commit)?;
            receipt["outcome"] = Value::String("success".to_owned());
            receipt["stages"] = Value::Array(stages);
            let detail = detail
                .as_object()
                .ok_or_else(|| failure("Tor job detail must be a JSON object"))?;
            for (key, value) in detail {
                receipt[key] = value.clone();
            }
            write_receipt(evidence, &receipt)?;
            event(
                json!({"event": "tor-receipt", "job": job, "receipt": evidence.join("receipt.json").to_string_lossy()}),
            );
            Ok(())
        }
        Err(error) => {
            let mut receipt = json!({
                "job": job,
                "main_commit": commit,
                "tool_revision": SOURCE_REVISION,
                "outcome": "failure",
                "error": error.to_string(),
                "stages": stages,
                "unix_seconds": current_unix_seconds(),
            });
            if let Some(pipeline) = optional(env, "CI_PIPELINE_NUMBER") {
                receipt["pipeline"] = Value::String(pipeline);
            }
            let _ = write_receipt(evidence, &receipt);
            event(
                json!({"event": "tor-receipt", "job": job, "outcome": "failure", "receipt": evidence.join("receipt.json").to_string_lossy()}),
            );
            Err(error)
        }
    }
}

/// Exact pass labels the approved Snowflake contract records on the public
/// path (six observed in the stopgap source). Every label must be present;
/// extras are recorded but do not fail. Any drift fails closed and forces an
/// explicit update of this set.
const SNOWFLAKE_PASSED_LABELS: [&str; 6] = [
    "controlled cancellation, pending capacity, deadlines and scoped-key transfer",
    "Snowflake transport refuses every non-bridge address before any network use",
    "independent clients and onion services",
    "128 full duplex frames including empty frame through Ferry wasm codec",
    "loss closes reads, accept and dial without direct fallback",
    "fresh runtime restores scoped onion identity and exchanges frames",
];

/// Validate the structured browser contract evidence (exit status is not proof).
fn validate_browser_evidence(path: &Path, commit: &str) -> Result<Value> {
    let location = path.display().to_string();
    let text = fs::read_to_string(path)
        .map_err(|_| failure(format!("Browser contract evidence missing: {location}")))?;
    let evidence: Value = serde_json::from_str(&text)
        .map_err(|_| failure("Browser contract evidence is not JSON"))?;
    let failure_path = path.with_extension("json.failure.json");
    if failure_path.is_file() {
        return Err(failure("Browser contract left failure evidence behind"));
    }
    let contract = evidence
        .get("contract")
        .filter(|contract| contract.get("stage").and_then(Value::as_str) == Some("passed"))
        .ok_or_else(|| failure("Browser contract did not reach the passed stage"))?;
    let labels: Vec<&str> = contract
        .get("passed")
        .and_then(Value::as_array)
        .map(|passed| {
            passed
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<&str>>()
        })
        .unwrap_or_default();
    for expected in SNOWFLAKE_PASSED_LABELS {
        if !labels.contains(&expected) {
            return Err(failure(format!(
                "Browser contract is missing pass label: {expected}"
            )));
        }
    }
    if evidence.get("network").and_then(Value::as_str) != Some("public") {
        return Err(failure(
            "Browser contract evidence is not the public network",
        ));
    }
    if evidence.get("https").and_then(Value::as_bool) != Some(true) {
        return Err(failure("Browser contract evidence is not HTTPS"));
    }
    if evidence
        .get("snowflakeBrokerRequests")
        .and_then(Value::as_u64)
        .is_none_or(|count| count == 0)
    {
        return Err(failure("Browser contract made no Snowflake broker request"));
    }
    if evidence
        .get("nonRelayCanaryConnections")
        .and_then(Value::as_u64)
        .is_none_or(|count| count != 0)
    {
        return Err(failure("Browser contract touched the non-relay canary"));
    }
    if evidence
        .get("unexpectedExternalRequests")
        .and_then(Value::as_array)
        .is_none_or(|requests| !requests.is_empty())
    {
        return Err(failure(
            "Browser contract made unexpected external requests",
        ));
    }
    if evidence.get("source").and_then(Value::as_str) != Some(commit) {
        return Err(failure(
            "Browser contract evidence is bound to another commit",
        ));
    }
    Ok(contract.clone())
}

/// Exact browser-build environment file contract (mirrors the verify gate).
/// `TORJS_DIST` and `FERRY_BROWSER` are directories; `TOR_GATEWAY_BIN` is the
/// gateway executable file. Duplicate keys fail closed.
fn read_browser_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let location = path.display().to_string();
    let text = fs::read_to_string(path)
        .map_err(|_| failure(format!("Browser environment file missing: {location}")))?;
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| failure("Browser environment file has a malformed line"))?;
        if values.contains_key(key) {
            return Err(failure("Browser environment file has a duplicate key"));
        }
        values.insert(key.to_owned(), value.to_owned());
    }
    let keys: Vec<String> = values.keys().cloned().collect();
    let mut expected: Vec<String> = BROWSER_ENV_KEYS.iter().map(ToString::to_string).collect();
    expected.sort();
    if keys != expected {
        return Err(failure(
            "Browser environment file must declare exactly TORJS_DIST, TOR_GATEWAY_BIN and FERRY_BROWSER",
        ));
    }
    for key in ["TORJS_DIST", "FERRY_BROWSER"] {
        if !Path::new(&values[key]).is_dir() {
            return Err(failure(format!("Browser environment path missing: {key}")));
        }
    }
    if !Path::new(&values["TOR_GATEWAY_BIN"]).is_file() {
        return Err(failure(
            "Browser environment gateway binary missing: TOR_GATEWAY_BIN",
        ));
    }
    Ok(values)
}

/// Third-party wasm-port pins the transitional builder clones (recorded, not trusted).
fn transitional_upstream_pins(script_text: &str) -> Result<Vec<String>> {
    let mut pins = Vec::new();
    for token in script_text.split_whitespace() {
        if token.len() == 40 && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            pins.push(token.to_owned());
        }
    }
    pins.sort();
    pins.dedup();
    Ok(pins)
}

/// Resolve a provisioned binary from `PATH` (fail-closed, no shell lookup).
fn resolve_on_path(env: &Environment, name: &str) -> Result<String> {
    let path = required(env, "PATH")?;
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    Err(failure(format!("Tor job requires {name} on PATH")))
}

/// Extend (never replace) the job's Git insteadOf entries with a local mirror.
/// `local` is a filesystem path; the `file://` scheme is added here, so
/// callers must not include it.
fn push_instead_of(env: &mut Environment, urls: &[&str], local: &str) {
    let mut count: usize = env
        .get(&OsString::from("GIT_CONFIG_COUNT"))
        .and_then(|value| value.to_str())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    for url in urls {
        env.insert(
            OsString::from(format!("GIT_CONFIG_KEY_{count}")),
            OsString::from(format!("url.file://{local}.insteadOf")),
        );
        env.insert(
            OsString::from(format!("GIT_CONFIG_VALUE_{count}")),
            OsString::from(*url),
        );
        count += 1;
    }
    env.insert(
        OsString::from("GIT_CONFIG_COUNT"),
        OsString::from(count.to_string()),
    );
}

fn receipt_base(env: &Environment, job: &str, commit: &str) -> Result<Value> {
    Ok(json!({
        "job": job,
        "main_commit": commit,
        "tool_revision": SOURCE_REVISION,
        "pipeline": required(env, "CI_PIPELINE_NUMBER")?,
        "unix_seconds": current_unix_seconds(),
    }))
}

/// Nightly toolchain environment mirroring the verify job's Tor legs: rustup
/// proxies first on `PATH` with an explicit nightly toolchain and resolved
/// compiler binaries (bare `cargo +nightly` is never used).
fn nightly_env(
    base: &Environment,
    global: Instant,
    workdir: &Path,
) -> Result<Vec<(String, String)>> {
    let cargo_home = required(base, "CARGO_HOME")?;
    let path_value = required(base, "PATH")?;
    let rustc = probe(
        base,
        global,
        workdir,
        &argv(&["rustup", "which", "--toolchain", "nightly", "rustc"]),
    )?;
    let cargo = probe(
        base,
        global,
        workdir,
        &argv(&["rustup", "which", "--toolchain", "nightly", "cargo"]),
    )?;
    Ok(vec![
        ("RUSTUP_TOOLCHAIN".to_owned(), "nightly".to_owned()),
        ("PATH".to_owned(), format!("{cargo_home}/bin:{path_value}")),
        ("RUSTC".to_owned(), rustc),
        ("CARGO".to_owned(), cargo),
    ])
}

fn private_network(repo: &Path, env: &Environment) -> Result<()> {
    // Owner-private file-creation mask for this Chutney job's dedicated
    // `ccid tor` child process (spawned per job via `ccid check`; see
    // `main.rs`/`supervision.rs`). Precedes all scratch, evidence, and child
    // creation below; every descendant inherits it. Snowflake keeps its
    // retired green behavior and is excluded.
    #[cfg(unix)]
    restrict_tor_process_umask();
    let commit = hex_identity(&required(env, "CI_COMMIT_SHA")?, "CI_COMMIT_SHA")?;
    let evidence = evidence_root(env, "ctrn-tor", "private-network")?;
    let scratch = native_tor_scratch()?;
    let global = global_deadline(env)?;
    let mut base = env.clone();
    base.insert(
        OsString::from("HOME"),
        OsString::from(stable_tool_home(env)?),
    );
    apply_tor_scratch_env(&mut base, &scratch);
    let mut stages = Vec::new();
    let detail = private_network_inner(repo, &base, global, &scratch, &evidence, &mut stages);
    if detail.is_err() {
        // Best-effort salvage: scratch outlives the failed stages here.
        let _ = copy_coverage(repo, &evidence, false);
        let _ = persist_private_network_evidence(
            &scratch.path().join("tor-evidence"),
            &evidence.join("tor-artifact"),
            false,
        );
    }
    finish(&evidence, env, "private-network", &commit, stages, detail)
}

fn private_network_inner(
    repo: &Path,
    base: &Environment,
    global: Instant,
    scratch: &tempfile::TempDir,
    evidence: &Path,
    stages: &mut Vec<Value>,
) -> Result<Value> {
    let nightly = nightly_env(base, global, repo)?;
    let (outcome, _) = run_stage(
        base,
        global,
        Stage {
            name: "tor-tools",
            command: argv(&["bash", ".ci/tor-tools.sh"]),
            workdir: repo.to_owned(),
            extra_env: nightly.clone(),
            timeout_secs: TOR_TOOLS_TIMEOUT,
            capture_stdout: false,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    stages.push(json!({"stage": "tor-tools", "seconds": outcome.seconds}));
    let (outcome, _) = run_stage(
        base,
        global,
        Stage {
            name: "native-coverage",
            command: argv(&["bash", ".ci/native-coverage.sh"]),
            workdir: repo.to_owned(),
            extra_env: nightly.clone(),
            timeout_secs: NATIVE_COVERAGE_TIMEOUT,
            capture_stdout: false,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    stages.push(json!({"stage": "native-coverage", "seconds": outcome.seconds}));
    let home = required(base, "HOME")?;
    let tor_bin = format!("{home}/.cache/ctrn-tools/bin/tor");
    let tor_version = probe(base, global, repo, &argv(&[tor_bin.as_str(), "--version"]))?;
    let rustc_version = probe(base, global, repo, &argv(&["rustc", "--version"]))?;
    let cargo_version = probe(base, global, repo, &argv(&["cargo", "--version"]))?;
    let lock_text = fs::read_to_string(repo.join("Cargo.lock"))
        .map_err(|_| failure("Tor job requires the committed Cargo.lock"))?;
    let cfry = lock_rev(&lock_text, "cfry")?;
    let artifact = evidence.join("tor-artifact");
    let kept =
        persist_private_network_evidence(&scratch.path().join("tor-evidence"), &artifact, true)?;
    let coverage = copy_coverage(repo, evidence, true)?;
    Ok(json!({
        "pins": {"cfry": cfry},
        "toolchain": {"tor": tor_version, "rustc": rustc_version, "cargo": cargo_version},
        "probe": {"example": "private_network", "source": "ctrn-main"},
        "evidence": {"artifact": artifact.to_string_lossy(), "kept": kept, "coverage": coverage},
        "tool_home": home,
        "source_archive_sha256": optional(base, "SOURCE_SHA256"),
        "adapter": {"job": "private-network", "command": "ccid tor private-network"},
    }))
}

/// Tor library environment mirroring the cmsh verify job's records leg: the Crow
/// worker provisions libevent/openssl/zlib store paths and this scopes them
/// (plus gcc) to the fixture build only. The Ubuntu `apt-get` path is never
/// taken on Crow; these variables fail closed when the provisioning that
/// replaces `sudo apt-get` is absent, so no silent no-op install can pass.
fn tor_library_env(base: &Environment) -> Result<Vec<(String, String)>> {
    let out = required(base, "CI_TOR_LIB_OUT")?;
    let dev = required(base, "CI_TOR_LIB_DEV")?;
    let mut libraries = Vec::new();
    for path in out.split_whitespace() {
        libraries.push(format!("{path}/lib"));
    }
    let mut configs = Vec::new();
    for path in dev.split_whitespace() {
        configs.push(format!("{path}/lib/pkgconfig"));
        configs.push(format!("{path}/share/pkgconfig"));
    }
    if libraries.is_empty() || configs.is_empty() {
        return Err(failure(
            "CI_TOR_LIB_OUT/CI_TOR_LIB_DEV carry no store paths",
        ));
    }
    let mut library_path = libraries.join(":");
    if let Some(existing) = base
        .get(&OsString::from("LD_LIBRARY_PATH"))
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
    {
        library_path.push(':');
        library_path.push_str(existing);
    }
    let mut config_path = configs.join(":");
    if let Some(existing) = base
        .get(&OsString::from("PKG_CONFIG_PATH"))
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
    {
        config_path.push(':');
        config_path.push_str(existing);
    }
    Ok(vec![
        ("CC".to_owned(), "gcc".to_owned()),
        ("LD_LIBRARY_PATH".to_owned(), library_path),
        ("PKG_CONFIG_PATH".to_owned(), config_path),
    ])
}

fn records(repo: &Path, env: &Environment) -> Result<()> {
    // Same owner-private mask as `private_network`: the Records Chutney job
    // runs in its own `ccid tor` child process. Snowflake is excluded.
    #[cfg(unix)]
    restrict_tor_process_umask();
    let commit = hex_identity(&required(env, "CI_COMMIT_SHA")?, "CI_COMMIT_SHA")?;
    let evidence = evidence_root(env, "cmsh-tor", "records")?;
    let scratch = native_tor_scratch()?;
    let global = global_deadline(env)?;
    let mut base = env.clone();
    base.insert(
        OsString::from("HOME"),
        OsString::from(stable_tool_home(env)?),
    );
    apply_tor_scratch_env(&mut base, &scratch);
    let mut stages = Vec::new();
    let detail = records_inner(repo, &base, global, &scratch, &evidence, &mut stages);
    if detail.is_err() {
        // Best-effort salvage: scratch outlives the failed stages here.
        let _ = persist_private_network_evidence(
            &scratch.path().join("records-tor-evidence"),
            &evidence.join("tor-artifact"),
            false,
        );
    }
    finish(&evidence, env, "records", &commit, stages, detail)
}

/// Lock-resolved fixture transport: the exact ctrn source cargo materialized
/// for the committed lockfile. The revision must equal the lock pin, and the
/// fixture entry points must exist; nothing is cloned or fetched here.
#[derive(Debug)]
struct TransportSource {
    dir: PathBuf,
    revision: String,
}

fn transport_source(metadata_text: &str, lock_ctrn_rev: &str) -> Result<TransportSource> {
    let metadata: Value =
        serde_json::from_str(metadata_text).map_err(|_| failure("cargo metadata is not JSON"))?;
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| failure("cargo metadata has no packages"))?;
    for package in packages {
        if package.get("name").and_then(Value::as_str) != Some("ctrn") {
            continue;
        }
        let manifest = package
            .get("manifest_path")
            .and_then(Value::as_str)
            .ok_or_else(|| failure("cargo metadata ctrn package has no manifest path"))?;
        let source = package
            .get("source")
            .and_then(Value::as_str)
            .ok_or_else(|| failure("cargo metadata ctrn package has no source"))?;
        let revision = source
            .split('#')
            .nth(1)
            .ok_or_else(|| failure("cargo metadata ctrn source has no pinned revision"))?;
        hex_identity(revision, "ctrn")?;
        if revision != lock_ctrn_rev {
            return Err(failure(
                "cargo metadata ctrn revision does not match the committed Cargo.lock pin",
            ));
        }
        let dir = PathBuf::from(manifest)
            .parent()
            .ok_or_else(|| failure("cargo metadata ctrn manifest has no parent"))?
            .to_owned();
        for file in [".ci/tor-tools.sh", ".ci/private-network.py"] {
            if !dir.join(file).is_file() {
                return Err(failure(format!("Lock-resolved ctrn source has no {file}")));
            }
        }
        return Ok(TransportSource {
            dir,
            revision: revision.to_owned(),
        });
    }
    Err(failure("cargo metadata has no ctrn package"))
}

fn records_inner(
    repo: &Path,
    base: &Environment,
    global: Instant,
    scratch: &tempfile::TempDir,
    evidence: &Path,
    stages: &mut Vec<Value>,
) -> Result<Value> {
    // Port of cmsh `.ci/records.sh` (kept unmodified as legacy reference):
    // resolve the exact fixture transport from the committed lockfile, set up
    // tools from the staged Nix-capable tooling source (never the pinned
    // pre-Crow script), build the probe, and run the ORIGINAL pinned fixture
    // driver and probe binary. Product pins are unchanged; no shims are added.
    let lock_text = fs::read_to_string(repo.join("Cargo.lock"))
        .map_err(|_| failure("Tor job requires the committed Cargo.lock"))?;
    let lock_ctrn = lock_rev(&lock_text, "ctrn")?;
    let cdht = lock_rev(&lock_text, "cdht")?;
    let cfry = lock_rev(&lock_text, "cfry")?;
    let (outcome, metadata_text) = run_stage(
        base,
        global,
        Stage {
            name: "cargo-metadata",
            command: argv(&["cargo", "metadata", "--locked", "--format-version", "1"]),
            workdir: repo.to_owned(),
            extra_env: Vec::new(),
            timeout_secs: GIT_OPERATION_TIMEOUT,
            capture_stdout: true,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    stages.push(json!({"stage": "cargo-metadata", "seconds": outcome.seconds}));
    let transport = transport_source(&metadata_text, &lock_ctrn)?;
    let archives_text = fs::read_to_string(repo.join(".ci/archives.toml"))
        .map_err(|_| failure("Records job requires the committed .ci/archives.toml"))?;
    let tooling_rev = aux_revision(&archives_text, "ctrn-tooling")?;
    let tooling_archive = required(base, "CTRN_TOOLING_SOURCE_ARCHIVE")?;
    let tooling_digest = required(base, "CTRN_TOOLING_SOURCE_SHA256")?;
    require_digest(&PathBuf::from(&tooling_archive), &tooling_digest)?;
    let tooling = scratch.path().join("ctrn-tooling");
    verify_source(
        &PathBuf::from(&tooling_archive),
        &tooling_digest,
        &tooling_rev,
        &tooling,
    )?;
    // The staged tooling must be the Nix-capable setup, not the pre-Crow
    // script with unconditional `sudo apt-get`.
    let tooling_script = tooling.join(".ci/tor-tools.sh");
    let tooling_text = fs::read_to_string(&tooling_script)
        .map_err(|_| failure("Staged ctrn tooling has no .ci/tor-tools.sh"))?;
    if !tooling_text.contains("CI_TOR_LIB_OUT") {
        return Err(failure(
            "Staged ctrn tooling predates the Crow branch; refusing the apt-get path",
        ));
    }
    let tor_env = tor_library_env(base)?;
    let mut tooling_command = argv(&["bash"]);
    tooling_command.push(tooling_script.to_string_lossy().into_owned());
    let (outcome, _) = run_stage(
        base,
        global,
        Stage {
            name: "tor-tools",
            command: tooling_command,
            workdir: repo.to_owned(),
            extra_env: tor_env.clone(),
            timeout_secs: TOR_TOOLS_TIMEOUT,
            capture_stdout: false,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    stages.push(json!({"stage": "tor-tools", "seconds": outcome.seconds}));
    let home = required(base, "HOME")?;
    let tools = format!("{home}/.cache/ctrn-tools");
    let venv_python = format!("{tools}/venv/bin/python");
    if !Path::new(&venv_python).is_file() {
        return Err(failure("Tooling did not provision the fixture venv python"));
    }
    let (outcome, _) = run_stage(
        base,
        global,
        Stage {
            name: "tor-records-build",
            command: argv(&[
                "cargo",
                "build",
                "--locked",
                "--example",
                "tor_records",
                "--features",
                "tor",
            ]),
            workdir: repo.to_owned(),
            extra_env: tor_env.clone(),
            timeout_secs: CARGO_BUILD_TIMEOUT,
            capture_stdout: false,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    stages.push(json!({"stage": "tor-records-build", "seconds": outcome.seconds}));
    let tor_bin = format!("{tools}/bin/tor");
    let tor_version = probe(base, global, repo, &argv(&[tor_bin.as_str(), "--version"]))?;
    let rustc_version = probe(base, global, repo, &argv(&["rustc", "--version"]))?;
    let cargo_version = probe(base, global, repo, &argv(&["cargo", "--version"]))?;
    let metadata: Value =
        serde_json::from_str(&metadata_text).map_err(|_| failure("cargo metadata is not JSON"))?;
    let target = metadata
        .get("target_directory")
        .and_then(Value::as_str)
        .ok_or_else(|| failure("cargo metadata has no target directory"))?;
    let probe_bin = format!("{target}/debug/examples/tor_records");
    if !Path::new(&probe_bin).is_file() {
        return Err(failure("tor_records probe binary missing after build"));
    }
    let tor_artifact = scratch.path().join("records-tor-evidence");
    let fixture = transport.dir.join(".ci/private-network.py");
    let mut probe_command = vec![venv_python];
    probe_command.push(fixture.to_string_lossy().into_owned());
    let (outcome, _) = run_stage(
        base,
        global,
        Stage {
            name: "records",
            command: probe_command,
            workdir: repo.to_owned(),
            extra_env: vec![
                ("CHUTNEY_SOURCE".to_owned(), format!("{tools}/chutney")),
                ("TOR_BIN".to_owned(), tor_bin),
                (
                    "TOR_GENCERT_BIN".to_owned(),
                    format!("{tools}/bin/tor-gencert"),
                ),
                ("TOR_PROBE_BIN".to_owned(), probe_bin),
                (
                    "TOR_ARTIFACT".to_owned(),
                    tor_artifact.to_string_lossy().into_owned(),
                ),
            ],
            timeout_secs: RECORDS_TIMEOUT,
            capture_stdout: false,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    stages.push(json!({"stage": "records", "seconds": outcome.seconds}));
    let artifact = evidence.join("tor-artifact");
    let kept = persist_private_network_evidence(&tor_artifact, &artifact, true)?;
    Ok(json!({
        "pins": {"cdht": cdht, "ctrn": transport.revision, "cfry": cfry},
        "tooling": {"ctrn_tooling": tooling_rev, "sha256": tooling_digest},
        "toolchain": {"tor": tor_version, "rustc": rustc_version, "cargo": cargo_version},
        "probe": {"example": "tor_records", "features": ["tor"], "source": "cmsh-main"},
        "evidence": {"artifact": artifact.to_string_lossy(), "kept": kept},
        "tool_home": home,
        "source_archive_sha256": optional(base, "SOURCE_SHA256"),
        "adapter": {"job": "records", "command": "ccid tor records"},
        "note": "Live cdht Records evidence is owned by this cmsh probe at the pinned cdht revision; cdht sim checks prove nothing about Tor.",
    }))
}

/// Frozen nine-onion driver set executed transitionally: the exact B3
/// qualifier scripts, verified by blob SHA-256 after extraction from the
/// staged frozen root archive. No new Python logic; all orchestration is Rust.
const V01_DRIVER_FILES: [&str; 3] = [
    ".ci/v01-tor-live.py",
    ".ci/v01-tor-records.py",
    ".ci/v01-records.py",
];

/// Compatibility normalization for the frozen B3 driver: its fixture
/// scratch hardcodes `dir="/tmp"`, whose worker-namespace mode (2777,
/// non-sticky) the native ownership checks refuse; `/var/tmp` (1777,
/// sticky) is the trusted standard root. Applied in safe Rust to a
/// separate adjacent runtime copy only after the original archive and
/// driver hashes verify; the original stays byte-identical. Exactly one
/// occurrence is required; anything else fails closed. This does NOT
/// claim byte-identity of the executed file: only this path literal is
/// adapted, every assertion stays unchanged.
const V01_TMPDIR_CALL: &str = "TemporaryDirectory(prefix=\"v01tor-\", dir=\"/tmp\")";
const V01_TMPDIR_NORMALIZED: &str = "TemporaryDirectory(prefix=\"v01tor-\", dir=\"/var/tmp\")";
/// Distinct runtime filename so the driver's root-vs-sibling integrity
/// check keeps matching the byte-exact originals beside it.
const V01_RUNTIME_DRIVER: &str = "v01-tor-live-runtime.py";

fn v01_normalize_tmpdir(original: &str) -> Result<String> {
    if original.matches("dir=\"/tmp\"").count() != 1 {
        return Err(failure(
            "Tor job live driver has an unexpected /tmp literal set",
        ));
    }
    if original.matches(V01_TMPDIR_CALL).count() != 1 {
        return Err(failure(
            "Tor job live driver compatibility call changed shape",
        ));
    }
    Ok(original.replacen(V01_TMPDIR_CALL, V01_TMPDIR_NORMALIZED, 1))
}

/// Staged live inputs for this job, resolved from the shared manifest
/// with the exact expected input set enforced: no missing, renamed, or
/// extra entries pass. Supplied environment paths are overrides only;
/// committed digests stay the trust anchor.
/// Expected Crow variable pair per staged input name. Names and pairs
/// are enforced together so renamed or duplicated variables fail before
/// launch rather than resolving to the wrong file.
const V01_INPUT_VARIABLES: [(&str, &str, &str); 3] = [
    (
        "v01-source-bundle",
        "V01_SOURCE_BUNDLE",
        "V01_SOURCE_BUNDLE_SHA256",
    ),
    (
        "v01-chutney",
        "V01_CHUTNEY_SOURCE_ARCHIVE",
        "V01_CHUTNEY_SOURCE_SHA256",
    ),
    (
        "v01-root-source",
        "V01_ROOT_SOURCE_ARCHIVE",
        "V01_ROOT_SOURCE_SHA256",
    ),
];

/// Expected fixed tool names: receipt, the three retained executables,
/// the six retained Rust toolchain executables, and the two Wasm stdlib
/// files. All twelve are checked; committed paths are verified before use.
const V01_TOOL_NAMES: [&str; 12] = [
    "cargo",
    "cargo-clippy",
    "clippy-driver",
    "python",
    "receipt",
    "rustc",
    "rustdoc",
    "rustfmt",
    "tor",
    "tor-gencert",
    "wasm-libcore",
    "wasm-libstd",
];

/// Retained Rust executables bound into the driver environment: parent
/// directories of the six verified executables, deterministic order,
/// deduplicated, ahead of the existing PATH.
const V01_RUST_EXECUTABLES: [&str; 6] = [
    "cargo",
    "cargo-clippy",
    "clippy-driver",
    "rustc",
    "rustdoc",
    "rustfmt",
];

/// Retained 1.98.1 toolchain selection for the frozen driver: PATH entries
/// from the verified executable parents plus exact compiler paths. The
/// frozen driver's own version, wasm-std and unchanged-tools gates
/// validate the selection; worker defaults, RUSTUP_HOME and installations
/// stay untouched.
fn v01_tool_env(
    tools: &BTreeMap<String, String>,
    base: &Environment,
) -> Result<Vec<(String, String)>> {
    let mut dirs = Vec::new();
    for name in V01_RUST_EXECUTABLES {
        let path = tools
            .get(name)
            .ok_or_else(|| failure(format!("Tor job live tools declare no {name}")))?;
        let parent = Path::new(path)
            .parent()
            .ok_or_else(|| failure(format!("Tor job live tool has no parent directory: {name}")))?;
        dirs.push(parent.to_string_lossy().into_owned());
    }
    dirs.sort();
    dirs.dedup();
    let path = match base
        .get(&OsString::from("PATH"))
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
    {
        Some(existing) => format!("{}:{existing}", dirs.join(":")),
        None => dirs.join(":"),
    };
    let mut env = vec![("PATH".to_owned(), path)];
    for (variable, tool) in [
        ("RUSTC", "rustc"),
        ("CARGO", "cargo"),
        ("RUSTDOC", "rustdoc"),
    ] {
        let path = tools
            .get(tool)
            .ok_or_else(|| failure(format!("Tor job live tools declare no {tool}")))?;
        env.push((variable.to_owned(), path.clone()));
    }
    Ok(env)
}

fn v01_inputs(
    manifest: &tor_inputs::Manifest,
    workflows: &[&str],
) -> Result<Vec<tor_inputs::LiveInput>> {
    let mut inputs = Vec::new();
    for input in manifest.live_inputs.values() {
        if let Some(scoped) = &input.workflows {
            if !scoped
                .iter()
                .any(|workflow| workflows.contains(&workflow.as_str()))
            {
                continue;
            }
        }
        inputs.push(input.clone());
    }
    let mut names: Vec<&str> = inputs.iter().map(|input| input.name.as_str()).collect();
    names.sort();
    if names != ["v01-chutney", "v01-root-source", "v01-source-bundle"] {
        return Err(failure(
            "Tor job live inputs declare an unexpected input set",
        ));
    }
    for (name, archive_variable, digest_variable) in V01_INPUT_VARIABLES {
        let input = inputs
            .iter()
            .find(|input| input.name == name)
            .ok_or_else(|| failure(format!("Tor job live inputs declare no {name}")))?;
        if input.archive_variable != archive_variable || input.digest_variable != digest_variable {
            return Err(failure(format!(
                "Tor job live input {name} declares unexpected variables"
            )));
        }
    }
    let mut variables: Vec<&str> = inputs
        .iter()
        .flat_map(|input| {
            [
                input.archive_variable.as_str(),
                input.digest_variable.as_str(),
            ]
        })
        .collect();
    variables.sort();
    variables.dedup();
    if variables.len() != 2 * inputs.len() {
        return Err(failure("Tor job live inputs declare duplicated variables"));
    }
    Ok(inputs)
}

fn v01_tor_live(repo: &Path, env: &Environment) -> Result<()> {
    // Same owner-private mask as the other Chutney jobs: this runs in its
    // own `ccid tor` child process.
    #[cfg(unix)]
    restrict_tor_process_umask();
    let commit = hex_identity(&required(env, "CI_COMMIT_SHA")?, "CI_COMMIT_SHA")?;
    let evidence = evidence_root(env, "cmsh-tor", "v01-tor-live")?;
    // Use the trusted /var/tmp root like the other native jobs.
    let scratch = native_tor_scratch()?;
    let global = global_deadline(env)?;
    let mut base = env.clone();
    apply_tor_scratch_env(&mut base, &scratch);
    let mut stages = Vec::new();
    let detail = v01_tor_live_inner(
        repo,
        &base,
        global,
        &scratch,
        &evidence,
        &commit,
        &mut stages,
    );
    // No salvage pass: the frozen driver writes all evidence incrementally
    // into the job-owned evidence directory below, so completed artifacts
    // already persist on failure paths.
    finish(&evidence, env, "v01-tor-live", &commit, stages, detail)
}

fn v01_tor_live_inner(
    repo: &Path,
    base: &Environment,
    global: Instant,
    scratch: &tempfile::TempDir,
    evidence: &Path,
    commit: &str,
    stages: &mut Vec<Value>,
) -> Result<Value> {
    let pipeline = required(base, "CI_PIPELINE_NUMBER")?;
    // Resolve every staged input against its committed digest. Supplied
    // environment paths are overrides only; the committed digest is the
    // trust anchor in all cases. Absence of the manifest fails here: the
    // runtime always requires it.
    let manifest = tor_inputs::load_manifest(repo)?.ok_or_else(|| {
        failure("Tor job requires the committed .ci/live-inputs.toml for v01-tor-live")
    })?;
    // Compiler-wrapper diagnostic pin: the exact frozen library this job
    // instruments. Committed here, never taken from caller-supplied env.
    let diag = manifest
        .diag
        .get("discovery-lib")
        .ok_or_else(|| failure("Tor job live manifest declares no diag discovery-lib pin"))?;
    if diag.file != "src/tor_discovery.rs" {
        return Err(failure(
            "Tor job diag discovery-lib must pin src/tor_discovery.rs",
        ));
    }
    let manifest_inputs = v01_inputs(&manifest, &["v01-tor-live"])?;
    let mut resolved: BTreeMap<String, String> = BTreeMap::new();
    let mut root_commit = String::new();
    let mut root_sha = String::new();
    for input in &manifest_inputs {
        let path = base
            .get(&OsString::from(&input.archive_variable))
            .and_then(|value| value.to_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| input.default_path.clone());
        require_digest(Path::new(&path), &input.sha256)?;
        if let Some(expected) = base
            .get(&OsString::from(&input.digest_variable))
            .and_then(|value| value.to_str())
            .filter(|value| !value.trim().is_empty())
        {
            if expected.to_lowercase() != input.sha256 {
                return Err(failure(format!(
                    "Tor job supplied digest differs from the committed pin: {}",
                    input.digest_variable
                )));
            }
        }
        if input.name == "v01-root-source" {
            root_commit = input.commit.clone().ok_or_else(|| {
                failure("Tor job live input v01-root-source requires a commit pin")
            })?;
            root_sha = input.sha256.clone();
        }
        resolved.insert(input.archive_variable.clone(), path);
    }
    if root_commit.is_empty() {
        return Err(failure(
            "Tor job live inputs declare no v01-root-source commit",
        ));
    }
    // Frozen drivers come from the verified frozen root archive: whole-tar
    // digest, embedded commit identity, then per-blob digests. The branch
    // ref stays alive as the object source; this job never clones or
    // fetches.
    let drivers = scratch.path().join("v01-drivers");
    let root_archive = resolved
        .get("V01_ROOT_SOURCE_ARCHIVE")
        .ok_or_else(|| failure("Tor job live inputs declare no V01_ROOT_SOURCE_ARCHIVE"))?;
    verify_source(Path::new(root_archive), &root_sha, &root_commit, &drivers)?;
    {
        let mut listed: Vec<&str> = manifest
            .live_drivers
            .values()
            .map(|entry| entry.file.as_str())
            .collect();
        listed.sort();
        let mut expected: Vec<&str> = V01_DRIVER_FILES.to_vec();
        expected.sort();
        if listed != expected {
            return Err(failure(
                "Tor job live drivers declare an unexpected file set",
            ));
        }
    }
    for relative in V01_DRIVER_FILES {
        let expected = manifest
            .live_drivers
            .values()
            .find(|entry| entry.file == relative)
            .map(|entry| entry.sha256.clone())
            .ok_or_else(|| failure(format!("Tor job live drivers declare no {relative}")))?;
        require_digest(&drivers.join(relative), &expected)?;
    }
    // Fixed ambient tools and receipt, verified in place, never copied: the
    // Nix runtime closure stays valid only at these paths. The exact
    // twelve names are enforced so no tool is silently missing or added.
    let mut tool_names: Vec<&str> = manifest.live_tools.keys().map(String::as_str).collect();
    tool_names.sort();
    if tool_names != V01_TOOL_NAMES {
        return Err(failure("Tor job live tools declare an unexpected tool set"));
    }
    let mut tools = BTreeMap::new();
    for (name, entry) in &manifest.live_tools {
        require_digest(Path::new(&entry.path), &entry.sha256)?;
        tools.insert(name.clone(), entry.path.clone());
    }
    // Compatibility runtime copy beside the byte-exact originals: the
    // executed file adapts one trusted-root literal (digests of both
    // recorded below); the driver's root-vs-sibling integrity check keeps
    // matching the untouched originals, and `__file__`-relative helper
    // loads resolve in the same directory.
    let original_driver = drivers.join(".ci/v01-tor-live.py");
    let original_digest = sha256_file(&original_driver)?;
    let original_text = fs::read_to_string(&original_driver)
        .map_err(|_| failure("Tor job live driver unreadable after verification".to_owned()))?;
    let runtime_text = v01_normalize_tmpdir(&original_text)?;
    let runtime_driver = drivers.join(format!(".ci/{V01_RUNTIME_DRIVER}"));
    fs::write(&runtime_driver, runtime_text.as_bytes())
        .map_err(|_| failure("Tor job live driver runtime copy unwritable".to_owned()))?;
    let runtime_digest = sha256_file(&runtime_driver)?;
    let v01_evidence = evidence.join("v01-evidence");
    let python = tools
        .get("python")
        .ok_or_else(|| failure("Tor job live tools declare no python"))?;
    let target = target_subdir(base, "v01-tor-live")?;
    // Compiler-wrapper diagnostic binary: a copy of this verified tool
    // inside job-private scratch, so the workspace-wrapper path is unique
    // per attempt (isolating filename-hashed artifacts) while dependency
    // caches stay shared. A preexisting workspace wrapper is never
    // overridden or disabled: its presence fails clearly instead.
    if base
        .get(&OsString::from("RUSTC_WORKSPACE_WRAPPER"))
        .and_then(|value| value.to_str())
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Err(failure(
            "Tor job refuses a preexisting RUSTC_WORKSPACE_WRAPPER; unset it to run v01-tor-live",
        ));
    }
    let wrapper = scratch.path().join("ccid-rustc-wrapper");
    let own = std::env::current_exe()
        .map_err(|_| failure("Tor job cannot locate its own executable for wrapper staging"))?;
    fs::copy(&own, &wrapper)
        .map_err(|_| failure("Tor job cannot stage its diagnostic wrapper copy".to_owned()))?;
    if sha256_file(&wrapper)? != sha256_file(&own)? {
        return Err(failure(
            "Tor job diagnostic wrapper copy differs from its own binary",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&wrapper)
            .map_err(|_| failure("Tor job cannot inspect its diagnostic wrapper copy".to_owned()))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            return Err(failure("Tor job diagnostic wrapper copy is not executable"));
        }
    }
    let wrapper_receipt = evidence.join("compiler-diagnostic.jsonl");
    let mut driver_env = vec![
        (
            "V01_EVIDENCE_DIR".to_owned(),
            v01_evidence.to_string_lossy().into_owned(),
        ),
        (
            "V01_CHUTNEY_TREE_RECEIPT".to_owned(),
            v01_evidence
                .join("chutney-runtime-tree.json")
                .to_string_lossy()
                .into_owned(),
        ),
        ("CARGO_TARGET_DIR".to_owned(), target),
        (
            "V01_TOR_LIVE_HARNESS".to_owned(),
            runtime_driver.to_string_lossy().into_owned(),
        ),
        // Frozen driver requirements, unchanged from the branch qualifier:
        // the harness checks the cvld clone URL and matches the frozen root
        // commit against its own CI commit. Both are public frozen values,
        // set here so the unmodified driver sees its exact expected setup.
        ("CI".to_owned(), "true".to_owned()),
        (
            "CI_REPO_CLONE_URL".to_owned(),
            "https://forge.corbet.ch/corbet-libs/cvld.git".to_owned(),
        ),
        ("CI_COMMIT_SHA".to_owned(), root_commit.clone()),
        ("CI_PIPELINE_NUMBER".to_owned(), pipeline),
        ("PYTHONDONTWRITEBYTECODE".to_owned(), "1".to_owned()),
        ("PYTHONNOUSERSITE".to_owned(), "1".to_owned()),
        // Compiler-wrapper diagnostic: workspace members build through the
        // staged copy above (unique per attempt); the wrapper instruments
        // only the pinned frozen library and restores it afterwards.
        // Plain RUSTC_WRAPPER, if set, stays untouched for cargo to nest.
        (
            "RUSTC_WORKSPACE_WRAPPER".to_owned(),
            wrapper.to_string_lossy().into_owned(),
        ),
        ("CCID_RUSTC_DIAG".to_owned(), "1".to_owned()),
        (
            "CCID_DIAG_ROOT".to_owned(),
            scratch.path().to_string_lossy().into_owned(),
        ),
        ("CCID_DIAG_ORIGINAL_SHA256".to_owned(), diag.sha256.clone()),
        (
            "CCID_DIAG_RECEIPT".to_owned(),
            wrapper_receipt.to_string_lossy().into_owned(),
        ),
    ];
    for (variable, path) in &resolved {
        driver_env.push((variable.clone(), path.clone()));
    }
    // The frozen driver requires the committed digests explicitly alongside
    // the paths (its own `h.required` gates); forward them from the
    // committed manifest, never from supplied env alone (already cross-
    // checked above).
    for input in &manifest_inputs {
        driver_env.push((input.digest_variable.clone(), input.sha256.clone()));
    }
    // The frozen driver names its Chutney input V01_CHUTNEY_ARCHIVE; the
    // declared Crow variable keeps the existing auxiliary suffix pattern.
    if let Some(chutney) = resolved.get("V01_CHUTNEY_SOURCE_ARCHIVE") {
        driver_env.push(("V01_CHUTNEY_ARCHIVE".to_owned(), chutney.clone()));
    }
    // The frozen driver requires the root commit explicitly alongside the
    // root archive (its workflow/root identity match); forward the
    // committed pin, never the current job commit.
    driver_env.push(("V01_ROOT_SOURCE_COMMIT".to_owned(), root_commit.clone()));
    // Retained 1.98.1 toolchain selection: verified executable parents
    // first on PATH plus exact compiler paths, so the frozen driver's own
    // version gates pass without touching worker defaults or installations.
    driver_env.extend(v01_tool_env(&tools, base)?);
    let (outcome, _) = run_stage(
        base,
        global,
        Stage {
            name: "v01-tor-live",
            command: {
                // Verified retained python from the tool manifest, never an
                // ambient unverified interpreter.
                let mut command = argv(&[python.as_str()]);
                command.push(runtime_driver.to_string_lossy().into_owned());
                command
            },
            workdir: scratch.path().to_owned(),
            extra_env: driver_env,
            timeout_secs: V01_TOR_LIVE_TIMEOUT,
            capture_stdout: false,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    stages.push(json!({"stage": "v01-tor-live", "seconds": outcome.seconds}));
    // Driver exports audited: receipt.json is always written (finally
    // block) and must record the exact pass status; contract.txt must
    // carry the exact original success marker on exactly one complete
    // line. All three checks live under the job-owned evidence root.
    let receipt = v01_evidence.join("receipt.json");
    let receipt_text = fs::read_to_string(&receipt)
        .map_err(|_| failure("Tor job live driver left no receipt".to_owned()))?;
    let receipt_status = serde_json::from_str::<Value>(&receipt_text)
        .ok()
        .and_then(|receipt| {
            receipt
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    if receipt_status.as_deref() != Some("passed-original-private-nine-onion-contract") {
        return Err(failure(
            "Tor job live driver receipt does not record the exact pass status",
        ));
    }
    let contract = v01_evidence.join("private-tor/contract.txt");
    // Exact original success line (frozen driver SUCCESS value, single line):
    // any truncation would weaken the gate the driver itself enforces.
    let marker = "original ENV0/PeerInfo/FindNode and encrypted Get/Set/Watch over nine independent onion services passed; bootstrap storage absent; dynamic routing loss verified";
    let text = fs::read_to_string(&contract)
        .map_err(|_| failure("Tor job live driver left no contract evidence".to_owned()))?;
    if text.lines().filter(|line| *line == marker).count() != 1 {
        return Err(failure("Tor job live driver contract marker missing"));
    }
    // Compiler-wrapper diagnostic evidence: nonempty JSONL, every entry for
    // the pinned library with a clean compile and verified restore. A
    // missing receipt means the instrumented unit never compiled and must
    // never pass, even when the driver is otherwise green.
    let wrapper_lines: Vec<String> = fs::read_to_string(&wrapper_receipt)
        .map_err(|_| {
            failure(
                "Tor job compiler diagnostic receipt missing; refusing uninstrumented pass"
                    .to_owned(),
            )
        })?
        .lines()
        .map(str::to_owned)
        .collect();
    if wrapper_lines.is_empty() {
        return Err(failure(
            "Tor job compiler diagnostic receipt empty; refusing uninstrumented pass",
        ));
    }
    for line in &wrapper_lines {
        let entry: Value = serde_json::from_str(line)
            .map_err(|_| failure("Tor job compiler diagnostic receipt is not JSON".to_owned()))?;
        let approved = entry.get("crate_name").and_then(Value::as_str) == Some("cmsh")
            && entry.get("compiler_exit").and_then(Value::as_u64) == Some(0)
            && entry.get("restored").and_then(Value::as_bool) == Some(true)
            && entry.get("original_sha256").and_then(Value::as_str) == Some(diag.sha256.as_str())
            && entry.get("diagnostic_sha256").and_then(Value::as_str)
                == Some(diag.diagnostic_sha256.as_str());
        if !approved {
            return Err(failure(
                "Tor job compiler diagnostic receipt does not record a clean instrumented compile",
            ));
        }
    }
    Ok(json!({
        "inputs": manifest_inputs
            .into_iter()
            .map(|input| (input.name, input.sha256))
            .collect::<BTreeMap<String, String>>(),
        "main_commit": commit,
        "root_commit": root_commit,
        "tools": tools,
        "driver": {
            "original": "v01-tor-live.py",
            "original_sha256": original_digest,
            "runtime_copy": V01_RUNTIME_DRIVER,
            "runtime_sha256": runtime_digest,
            "adaptation": "single path literal dir=\"/tmp\" to dir=\"/var/tmp\"; assertions unchanged",
        },
        "diagnostic": {
            "library": diag.file.as_str(),
            "original_sha256": diag.sha256.as_str(),
            "diagnostic_sha256": diag.diagnostic_sha256.as_str(),
            "wrapper_receipt": wrapper_receipt.to_string_lossy(),
            "compiler_runs": wrapper_lines.len(),
            "note": "Executed library bytes were diagnostic, not original; probe sources and assertions stay original.",
        },
        "evidence": {"dir": v01_evidence.to_string_lossy()},
        "source_archive_sha256": optional(base, "SOURCE_SHA256"),
        "adapter": {"job": "v01-tor-live", "command": "ccid tor v01-tor-live"},
        "note": "Original nine-onion Records contract via the frozen B3 driver; transitional until ported to safe Rust orchestration.",
    }))
}

/// Persistent wasm-port target roots under the check's locked Cargo target
/// directory (warm across runs). Freshness contract: extracted auxiliary
/// sources (product tar, cfry mirror, tooling) live in ephemeral job scratch
/// and are rebuilt every attempt, so their outputs never leak across product
/// revisions; only the compiler's own dependency fingerprints persist here.
/// The `check` runner always supplies the root while holding the target lock;
/// a standalone direct invocation without the held lock fails closed so no
/// unlocked path is ever claimed as stable.
fn target_subdir(base: &Environment, name: &str) -> Result<String> {
    let root = required(base, "CARGO_TARGET_DIR")?;
    let held = required(base, "CCID_TARGET_LOCK_HELD")?;
    let root_path = PathBuf::from(&root);
    let held_path = PathBuf::from(&held);
    if !root_path.is_absolute() {
        return Err(failure("Tor job requires an absolute CARGO_TARGET_DIR"));
    }
    if !held_path.is_absolute() {
        return Err(failure(
            "Tor job requires an absolute CCID_TARGET_LOCK_HELD",
        ));
    }
    let locked = match (root_path.canonicalize(), held_path.canonicalize()) {
        (Ok(canonical_root), Ok(canonical_held)) => canonical_root == canonical_held,
        _ => root == held,
    };
    if !locked {
        return Err(failure(
            "Tor job requires the locked Cargo target directory (CCID_TARGET_LOCK_HELD must match CARGO_TARGET_DIR)",
        ));
    }
    let dir = root_path.join(name);
    if !dir.is_absolute() {
        return Err(failure("Tor job requires an absolute CARGO_TARGET_DIR"));
    }
    fs::create_dir_all(&dir)?;
    Ok(dir.to_string_lossy().into_owned())
}

/// Stable tool HOME beneath the locked Cargo target directory. Cached venv
/// pip shebangs embed the interpreter path, so a transient per-job HOME
/// breaks relocation across Crow workers; this stable absolute HOME keeps
/// tooling and fixture on the same path warm across runs. `CARGO_HOME` and
/// `RUSTUP_HOME` stay explicit and untouched.
fn stable_tool_home(base: &Environment) -> Result<String> {
    target_subdir(base, "tor-home")
}

fn snowflake_browser(repo: &Path, env: &Environment) -> Result<()> {
    let commit = hex_identity(&required(env, "CI_COMMIT_SHA")?, "CI_COMMIT_SHA")?;
    let evidence = evidence_root(env, "ctrn-tor", "snowflake-browser")?;
    let scratch = tor_scratch()?;
    let global = global_deadline(env)?;
    let mut base = env.clone();
    apply_tor_scratch_env(&mut base, &scratch);
    let mut stages = Vec::new();
    let detail = snowflake_browser_inner(repo, &mut base, global, &scratch, &evidence, &mut stages);
    if detail.is_err() {
        // Best-effort salvage: scratch outlives the failed stages here. The
        // ephemeral TLS key and certificate never leave scratch.
        for file in ["tor-js-Cargo.lock", "tor-js-lock.diff"] {
            let source = scratch.path().join(file);
            if source.is_file() {
                let _ = fs::copy(&source, evidence.join(file));
            }
        }
    }
    finish(&evidence, env, "snowflake-browser", &commit, stages, detail)
}

fn snowflake_browser_inner(
    repo: &Path,
    base: &mut Environment,
    global: Instant,
    scratch: &tempfile::TempDir,
    evidence: &Path,
    stages: &mut Vec<Value>,
) -> Result<Value> {
    let archives_text = fs::read_to_string(repo.join(".ci/archives.toml"))
        .map_err(|_| failure("Snowflake job requires the committed .ci/archives.toml"))?;
    let stopgap_rev = aux_revision(&archives_text, "stopgap-product")?;
    let cfry_rev = aux_revision(&archives_text, "cfry")?;
    let lock_text = fs::read_to_string(repo.join("Cargo.lock"))
        .map_err(|_| failure("Snowflake job requires the committed Cargo.lock"))?;
    let lock_cfry = lock_rev(&lock_text, "cfry")?;
    if lock_cfry != cfry_rev {
        return Err(failure(
            "Snowflake job auxiliary cfry revision does not match the committed Cargo.lock pin",
        ));
    }
    let product = scratch.path().join("product");
    let stopgap_archive = required(base, "STOPGAP_PRODUCT_SOURCE_ARCHIVE")?;
    let stopgap_digest = required(base, "STOPGAP_PRODUCT_SOURCE_SHA256")?;
    require_digest(&PathBuf::from(&stopgap_archive), &stopgap_digest)?;
    verify_source(
        &PathBuf::from(&stopgap_archive),
        &stopgap_digest,
        &stopgap_rev,
        &product,
    )?;
    // Two-level product binding: the main commit owns the job, while this file
    // and the receipt pin the exact product code the browser actually ran.
    fs::write(
        evidence.join("product-commit.txt"),
        stopgap_rev.clone() + "\n",
    )?;
    let product_lock = fs::read_to_string(product.join("Cargo.lock"))
        .map_err(|_| failure("Snowflake product has no Cargo.lock"))?;
    if lock_rev(&product_lock, "cfry")? != cfry_rev {
        return Err(failure(
            "Snowflake product Cargo.lock cfry pin does not match the staged cfry revision",
        ));
    }
    let cfry_bundle = required(base, "CFRY_SOURCE_BUNDLE")?;
    let cfry_digest = required(base, "CFRY_SOURCE_SHA256")?;
    let bundle = PathBuf::from(&cfry_bundle);
    require_digest(&bundle, &cfry_digest)?;
    let mirror = scratch.path().join("cfry-source.git");
    let mut bundle_verify = argv(&["git", "bundle", "verify"]);
    bundle_verify.push(bundle.to_string_lossy().into_owned());
    run_stage(
        base,
        global,
        Stage {
            name: "cfry-bundle-verify",
            command: bundle_verify,
            workdir: repo.to_owned(),
            extra_env: Vec::new(),
            timeout_secs: GIT_OPERATION_TIMEOUT,
            capture_stdout: false,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    let mut bundle_heads = argv(&["git", "bundle", "list-heads"]);
    bundle_heads.push(bundle.to_string_lossy().into_owned());
    let (_, heads) = run_stage(
        base,
        global,
        Stage {
            name: "cfry-bundle-heads",
            command: bundle_heads,
            workdir: repo.to_owned(),
            extra_env: Vec::new(),
            timeout_secs: GIT_OPERATION_TIMEOUT,
            capture_stdout: true,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    if heads.trim() != format!("{cfry_rev} refs/heads/source") {
        return Err(failure(
            "Staged cfry bundle does not advertise the declared revision",
        ));
    }
    let mut mirror_command = argv(&["git", "clone", "--quiet", "--mirror"]);
    mirror_command.push(bundle.to_string_lossy().into_owned());
    mirror_command.push(mirror.to_string_lossy().into_owned());
    run_stage(
        base,
        global,
        Stage {
            name: "cfry-mirror-clone",
            command: mirror_command,
            workdir: repo.to_owned(),
            extra_env: Vec::new(),
            timeout_secs: GIT_OPERATION_TIMEOUT,
            capture_stdout: false,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    let mut mirror_head = argv(&["git", "--git-dir"]);
    mirror_head.push(mirror.to_string_lossy().into_owned());
    mirror_head.push("symbolic-ref".to_owned());
    mirror_head.push("HEAD".to_owned());
    mirror_head.push("refs/heads/source".to_owned());
    run_stage(
        base,
        global,
        Stage {
            name: "cfry-mirror-head",
            command: mirror_head,
            workdir: repo.to_owned(),
            extra_env: Vec::new(),
            timeout_secs: GIT_OPERATION_TIMEOUT,
            capture_stdout: false,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    // Route the transitional builder's Ferry clone at the staged mirror.
    // The job's own insteadOf entries (Forgejo for first-party) are extended.
    // Clone URLs derive from the committed lock pin, never from shared literals.
    let ferry_source = lock_source(&lock_text, "cfry")?;
    let ferry_urls = lock_clone_urls(&ferry_source)?;
    let ferry_refs: Vec<&str> = ferry_urls.iter().map(String::as_str).collect();
    push_instead_of(base, &ferry_refs, &mirror.to_string_lossy());
    let build_script = fs::read_to_string(product.join(".ci/browser-build.sh"))
        .map_err(|_| failure("Snowflake product has no .ci/browser-build.sh"))?;
    let upstream_pins = transitional_upstream_pins(&build_script)?;
    if upstream_pins.len() != 2 {
        return Err(failure(
            "Snowflake builder third-party pins changed; update the recorded set explicitly",
        ));
    }
    let rustc_version = probe(base, global, &product, &argv(&["rustc", "--version"]))?;
    let cargo_version = probe(base, global, &product, &argv(&["cargo", "--version"]))?;
    let node_version = probe(base, global, &product, &argv(&["node", "--version"]))?;
    let chrome_bin = resolve_on_path(base, "google-chrome")?;
    let chrome_version = probe(
        base,
        global,
        &product,
        &argv(&[chrome_bin.as_str(), "--version"]),
    )?;
    let github_env = scratch.path().join("snowflake.env");
    fs::write(&github_env, "")?;
    let (outcome, _) = run_stage(
        base,
        global,
        Stage {
            name: "browser-build",
            command: argv(&["bash", ".ci/browser-build.sh"]),
            workdir: product.clone(),
            extra_env: vec![
                ("TOR_BROWSER_STAGE".to_owned(), "service".to_owned()),
                (
                    "GITHUB_ENV".to_owned(),
                    github_env.to_string_lossy().into_owned(),
                ),
                (
                    "TORJS_TARGET_DIR".to_owned(),
                    target_subdir(base, "tor-js")?,
                ),
                ("FERRY_TARGET_DIR".to_owned(), target_subdir(base, "ferry")?),
            ],
            timeout_secs: BROWSER_BUILD_TIMEOUT,
            capture_stdout: false,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    stages.push(json!({"stage": "browser-build", "seconds": outcome.seconds}));
    let browser_env = read_browser_env_file(&github_env)?;
    for file in ["tor-js-Cargo.lock", "tor-js-lock.diff"] {
        let source = scratch.path().join(file);
        if source.is_file() {
            fs::copy(&source, evidence.join(file))?;
        }
    }
    let key = scratch.path().join("https.key");
    let cert = scratch.path().join("https.pem");
    let mut openssl_command = argv(&[
        "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout",
    ]);
    openssl_command.push(key.to_string_lossy().into_owned());
    openssl_command.push("-out".to_owned());
    openssl_command.push(cert.to_string_lossy().into_owned());
    for flag in [
        "-days",
        "1",
        "-subj",
        "/CN=localhost",
        "-addext",
        "subjectAltName=IP:127.0.0.1",
    ] {
        openssl_command.push(flag.to_owned());
    }
    run_stage(
        base,
        global,
        Stage {
            name: "https-identity",
            command: openssl_command,
            workdir: product.clone(),
            extra_env: Vec::new(),
            timeout_secs: OPENSSL_TIMEOUT,
            capture_stdout: false,
            mute_stdout: false,
            mute_stderr: false,
        },
    )?;
    let fixture = scratch.path().join("fixture.json");
    fs::write(&fixture, "{\"testOnly\": false, \"snowflake\": true}\n")?;
    let contract_path = evidence.join("browser-contract.json");
    let driver = product.join(".ci/browser-driver.mjs");
    if !driver.is_file() {
        return Err(failure("Snowflake product has no .ci/browser-driver.mjs"));
    }
    let mut node_command = argv(&["node"]);
    node_command.push(driver.to_string_lossy().into_owned());
    let (outcome, _) = run_stage(
        base,
        global,
        Stage {
            name: "snowflake-browser",
            command: node_command,
            workdir: product.clone(),
            extra_env: vec![
                (
                    "TOR_FIXTURE_JSON".to_owned(),
                    fixture.to_string_lossy().into_owned(),
                ),
                (
                    "BROWSER_EVIDENCE".to_owned(),
                    contract_path.to_string_lossy().into_owned(),
                ),
                // Driver evidence source: the staged product revision, never main.
                ("CI_COMMIT_SHA".to_owned(), stopgap_rev.clone()),
                ("BROWSER_BIN".to_owned(), chrome_bin.clone()),
                ("TORJS_DIST".to_owned(), browser_env["TORJS_DIST"].clone()),
                (
                    "FERRY_BROWSER".to_owned(),
                    browser_env["FERRY_BROWSER"].clone(),
                ),
                ("HTTPS_KEY".to_owned(), key.to_string_lossy().into_owned()),
                ("HTTPS_CERT".to_owned(), cert.to_string_lossy().into_owned()),
            ],
            timeout_secs: NODE_DRIVER_TIMEOUT,
            capture_stdout: false,
            mute_stdout: true,
            mute_stderr: true,
        },
    )?;
    stages.push(json!({"stage": "snowflake-browser", "seconds": outcome.seconds}));
    let contract = validate_browser_evidence(&contract_path, &stopgap_rev)?;
    Ok(json!({
        "pins": {
            "product": stopgap_rev,
            "cfry": cfry_rev,
            "upstream_wasm_ports": upstream_pins,
        },
        "auxiliary": {
            "stopgap_product": {"revision": stopgap_rev, "sha256": stopgap_digest},
            "cfry": {"revision": cfry_rev, "sha256": cfry_digest},
        },
        "toolchain": {
            "rustc": rustc_version,
            "cargo": cargo_version,
            "node": node_version,
            "chrome": chrome_version,
        },
        "contract": {
            "stage": contract.get("stage"),
            "passed": contract.get("passed").and_then(Value::as_array).map(Vec::len),
            "evidence": contract_path.to_string_lossy(),
        },
        "browser": {
            "TORJS_DIST": browser_env["TORJS_DIST"],
            "FERRY_BROWSER": browser_env["FERRY_BROWSER"],
        },
        "source_archive_sha256": optional(base, "SOURCE_SHA256"),
        "adapter": {"job": "snowflake-browser", "command": "ccid tor snowflake-browser"},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(items: &[(&str, &str)]) -> Environment {
        items
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect()
    }

    fn tor_runner(millis: u64) -> (tempfile::TempDir, Runner) {
        let directory = tempfile::tempdir().unwrap();
        let runner = Runner::new(
            directory.path().into(),
            Environment::new(),
            Duration::from_millis(millis),
        )
        .unwrap();
        (directory, runner)
    }

    const SAMPLE_LOCK: &str = r#"
[[package]]
name = "cdht"
version = "0.1.0"
source = "git+https://github.com/corbet-foss/cdht?branch=main#9f44ffb779f296bd9162fceeee8aa9f62b1a9a66"

[[package]]
name = "cfry"
version = "0.1.0"
source = "git+https://github.com/corbet-foss/cfry?branch=main#219611e3dd0c50bbc284bff79f95651453524564"

[[package]]
name = "ctrn"
version = "0.1.0"
source = "git+https://github.com/corbet-foss/ctrn?branch=main#9894d84f726132129c81f03a2c42be131d9aa1ac"
"#;

    #[test]
    fn lock_revisions_identify_first_party_source() {
        assert_eq!(
            lock_rev(SAMPLE_LOCK, "cdht").unwrap(),
            "9f44ffb779f296bd9162fceeee8aa9f62b1a9a66"
        );
        assert_eq!(
            lock_rev(SAMPLE_LOCK, "ctrn").unwrap(),
            "9894d84f726132129c81f03a2c42be131d9aa1ac"
        );
        assert_eq!(
            lock_rev(SAMPLE_LOCK, "cfry").unwrap(),
            "219611e3dd0c50bbc284bff79f95651453524564"
        );
    }

    #[test]
    fn lock_without_package_or_revision_fails() {
        assert!(lock_rev(SAMPLE_LOCK, "missing").is_err());
        assert!(lock_rev("not toml [[", "cfry").is_err());
        let no_source = "[[package]]\nname = \"cfry\"\nversion = \"0.1.0\"\n";
        assert!(lock_rev(no_source, "cfry").is_err());
        let short = "[[package]]\nname = \"cfry\"\nversion = \"0.1.0\"\nsource = \"git+https://example.invalid/x#abc\"\n";
        assert!(lock_rev(short, "cfry").is_err());
    }

    #[test]
    fn lock_clone_urls_cover_both_upstream_spellings() {
        assert_eq!(
            lock_clone_urls("git+https://github.com/corbet-foss/cfry?branch=main#219611e3dd0c50bbc284bff79f95651453524564").unwrap(),
            [
                "https://github.com/corbet-foss/cfry",
                "https://github.com/corbet-foss/cfry.git",
            ]
        );
        assert!(lock_clone_urls("https://example.invalid/x#abc").is_err());
        assert!(lock_clone_urls("git+?#abc").is_err());
    }

    const SAMPLE_ARCHIVES: &str = r#"
schema = 1
[archives.stopgap-product]
kind = "archive"
revision = "5e31e9d852b166e7d3d1d197b269fba675f3680d"
archive_variable = "STOPGAP_PRODUCT_SOURCE_ARCHIVE"
digest_variable = "STOPGAP_PRODUCT_SOURCE_SHA256"
workflows = ["tor-jobs"]
[archives.cfry]
kind = "git-bundle"
revision = "219611e3dd0c50bbc284bff79f95651453524564"
archive_variable = "CFRY_SOURCE_BUNDLE"
digest_variable = "CFRY_SOURCE_SHA256"
workflows = ["tor-jobs"]
"#;

    #[test]
    fn auxiliary_revisions_come_from_declared_source_config() {
        assert_eq!(
            aux_revision(SAMPLE_ARCHIVES, "stopgap-product").unwrap(),
            "5e31e9d852b166e7d3d1d197b269fba675f3680d"
        );
        assert_eq!(
            aux_revision(SAMPLE_ARCHIVES, "cfry").unwrap(),
            "219611e3dd0c50bbc284bff79f95651453524564"
        );
    }

    #[test]
    fn malformed_source_config_fails() {
        assert!(aux_revision("schema = 1", "cfry").is_err());
        assert!(aux_revision(SAMPLE_ARCHIVES, "missing").is_err());
        let short = SAMPLE_ARCHIVES.replace("219611e3dd0c50bbc284bff79f95651453524564", "219611e3");
        assert!(aux_revision(&short, "cfry").is_err());
    }

    #[test]
    fn auxiliary_digest_mismatch_and_missing_files_fail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aux.tar");
        fs::write(&path, b"staged bytes").unwrap();
        let digest = sha256_file(&path).unwrap();
        require_digest(&path, &digest).unwrap();
        require_digest(&path, &"0".repeat(64)).unwrap_err();
        require_digest(&directory.path().join("absent.tar"), &digest).unwrap_err();
    }

    #[test]
    fn nonzero_subprocess_exit_propagates_with_stage_name() {
        let (_directory, runner) = tor_runner(5000);
        runner.run(&argv(&["false"]), false).unwrap_err();
        let base: Environment = Environment::new();
        let global = Instant::now() + Duration::from_secs(5);
        let outcome = run_stage(
            &base,
            global,
            Stage {
                name: "probe-stage",
                command: argv(&["false"]),
                workdir: std::env::temp_dir(),
                extra_env: Vec::new(),
                timeout_secs: 5,
                capture_stdout: false,
                mute_stdout: false,
                mute_stderr: false,
            },
        );
        let error = outcome.unwrap_err();
        assert!(
            error.to_string().contains("probe-stage"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn stage_timeout_kills_a_lingering_subprocess() {
        let (_directory, runner) = tor_runner(200);
        let error = runner.run(&argv(&["sleep", "30"]), false).unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn browser_evidence_requires_a_passed_public_contract() {
        let directory = tempfile::tempdir().unwrap();
        let commit = "0e53a6a867b0b213c5bbc59c14c1c55d3a26b7b0";
        let labels = vec![
            "controlled cancellation, pending capacity, deadlines and scoped-key transfer",
            "Snowflake transport refuses every non-bridge address before any network use",
            "independent clients and onion services",
            "128 full duplex frames including empty frame through Ferry wasm codec",
            "loss closes reads, accept and dial without direct fallback",
            "fresh runtime restores scoped onion identity and exchanges frames",
        ];
        let golden = json!({
            "source": commit,
            "network": "public",
            "https": true,
            "contract": {"stage": "passed", "passed": labels.clone()},
            "nonRelayCanaryConnections": 0,
            "unexpectedExternalRequests": [],
            "snowflakeBrokerRequests": 3,
        });
        let path = directory.path().join("browser-contract.json");
        fs::write(&path, serde_json::to_string(&golden).unwrap()).unwrap();
        let contract = validate_browser_evidence(&path, commit).unwrap();
        assert_eq!(
            contract.get("stage").and_then(Value::as_str),
            Some("passed")
        );
        fs::remove_file(&path).unwrap();
        validate_browser_evidence(&path, commit).unwrap_err();
        let mut incomplete = golden.clone();
        let _ = incomplete["contract"]["passed"]
            .as_array_mut()
            .unwrap()
            .pop();
        fs::write(&path, serde_json::to_string(&incomplete).unwrap()).unwrap();
        validate_browser_evidence(&path, commit).unwrap_err();
        for mutation in [
            json!({"network": "private"}),
            json!({"https": false}),
            json!({"contract": {"stage": "initialization", "passed": labels}}),
            json!({"contract": {"stage": "passed", "passed": []}}),
            json!({"snowflakeBrokerRequests": 0}),
            json!({"nonRelayCanaryConnections": 1}),
            json!({"unexpectedExternalRequests": ["https://example.invalid"]}),
            json!({"source": "f".repeat(40)}),
        ] {
            let mut mutated = golden.clone();
            for (key, value) in mutation.as_object().unwrap() {
                mutated[key.as_str()] = value.clone();
            }
            fs::write(&path, serde_json::to_string(&mutated).unwrap()).unwrap();
            validate_browser_evidence(&path, commit).unwrap_err();
        }
        fs::write(&path, serde_json::to_string(&golden).unwrap()).unwrap();
        fs::write(path.with_extension("json.failure.json"), "{}").unwrap();
        validate_browser_evidence(&path, commit).unwrap_err();
    }

    #[test]
    fn browser_environment_file_must_declare_exact_paths() {
        let directory = tempfile::tempdir().unwrap();
        let dist = directory.path().join("dist");
        let gateway = directory.path().join("tor-js-gateway");
        let ferry = directory.path().join("ferry");
        fs::create_dir_all(&dist).unwrap();
        fs::create_dir_all(&ferry).unwrap();
        fs::write(&gateway, b"gateway").unwrap();
        let path = directory.path().join("browser.env");
        let dist_display = dist.display().to_string();
        let gateway_display = gateway.display().to_string();
        let ferry_display = ferry.display().to_string();
        let valid = format!(
            "TORJS_DIST={dist_display}\nTOR_GATEWAY_BIN={gateway_display}\nFERRY_BROWSER={ferry_display}\n",
        );
        fs::write(&path, &valid).unwrap();
        let values = read_browser_env_file(&path).unwrap();
        assert_eq!(values["TORJS_DIST"], dist.to_string_lossy().into_owned());
        fs::write(&path, "TORJS_DIST=/nonexistent\n").unwrap();
        read_browser_env_file(&path).unwrap_err();
        fs::write(&path, valid.clone() + "EXTRA=1\n").unwrap();
        read_browser_env_file(&path).unwrap_err();
        fs::write(&path, valid.clone() + "TORJS_DIST=/other\n").unwrap();
        read_browser_env_file(&path).unwrap_err();
        fs::remove_file(&gateway).unwrap();
        fs::create_dir_all(&gateway).unwrap();
        fs::write(&path, &valid).unwrap();
        read_browser_env_file(&path).unwrap_err();
    }

    #[test]
    fn instead_of_entries_extend_existing_job_config() {
        let mut env = environment(&[
            ("GIT_CONFIG_COUNT", "2"),
            (
                "GIT_CONFIG_KEY_0",
                "url.https://git.corbet.ch/corbet-foss/.insteadOf",
            ),
            ("GIT_CONFIG_VALUE_0", "https://github.com/corbet-foss/"),
        ]);
        push_instead_of(
            &mut env,
            &["https://github.com/corbet-foss/cfry.git"],
            "/tmp/mirror.git",
        );
        assert_eq!(
            env.get(&OsString::from("GIT_CONFIG_COUNT"))
                .and_then(|value| value.to_str()),
            Some("3")
        );
        assert_eq!(
            env.get(&OsString::from("GIT_CONFIG_KEY_2"))
                .and_then(|value| value.to_str()),
            Some("url.file:///tmp/mirror.git.insteadOf")
        );
        assert_eq!(
            env.get(&OsString::from("GIT_CONFIG_KEY_0"))
                .and_then(|value| value.to_str()),
            Some("url.https://git.corbet.ch/corbet-foss/.insteadOf")
        );
    }

    #[test]
    fn missing_private_network_evidence_fails() {
        let directory = tempfile::tempdir().unwrap();
        require_private_network_evidence(directory.path()).unwrap_err();
        fs::write(
            directory.path().join("shared-random-readiness.json"),
            "not json",
        )
        .unwrap();
        fs::write(directory.path().join("listener-ports.json"), "{}").unwrap();
        require_private_network_evidence(directory.path()).unwrap_err();
        fs::write(directory.path().join("shared-random-readiness.json"), "{}").unwrap();
        require_private_network_evidence(directory.path()).unwrap();
    }

    #[test]
    fn upstream_pins_track_exact_builder_revisions() {
        let script = "git -C \"$torjs\" checkout --quiet 65bb21d536a405ba706689ca384fad2ee25e60a7\ngit -C \"$arti\" checkout --quiet 2be5b51e895a687d1e7fc3ce1c01896730bcb9d3\n";
        assert_eq!(transitional_upstream_pins(script).unwrap().len(), 2);
        assert!(transitional_upstream_pins("no pins here")
            .unwrap()
            .is_empty());
    }

    /// The insteadOf interception must redirect a real `git clone` at the
    /// upstream URL into the local mirror with zero network use.
    #[test]
    fn instead_of_entries_redirect_a_real_clone_into_the_mirror() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let origin = root.join("origin");
        fs::create_dir_all(&origin).unwrap();
        let script = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&origin)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .unwrap();
            assert!(status.success());
        };
        script(&["init", "--quiet"]);
        script(&[
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "fixture",
        ]);
        let commit = {
            let output = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&origin)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        let mut environment: Environment = Environment::new();
        // Production stages always inherit the provisioned PATH; the Runner
        // clears the environment, so the test must supply it explicitly.
        if let Ok(path) = std::env::var("PATH") {
            environment.insert(OsString::from("PATH"), OsString::from(path));
        }
        push_instead_of(
            &mut environment,
            &["https://example.invalid/first-party.git"],
            &origin.to_string_lossy(),
        );
        let checkout = root.join("checkout");
        let mut clone_command = argv(&[
            "git",
            "clone",
            "--quiet",
            "https://example.invalid/first-party.git",
        ]);
        clone_command.push(checkout.to_string_lossy().into_owned());
        let runner = Runner::new(root.to_owned(), environment, Duration::from_secs(120)).unwrap();
        runner.run(&clone_command, false).unwrap();
        let cloned = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&checkout)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(cloned.status.success());
        assert_eq!(String::from_utf8(cloned.stdout).unwrap().trim(), commit);
    }

    #[test]
    fn artifact_allowlist_keeps_fixture_outputs_only() {
        for name in [
            "shared-random-readiness.json",
            "listener-ports.json",
            "contract.txt",
            "consensus-microdesc-start.txt",
            "consensus-microdesc-readiness.txt",
            "node-status-started.json",
            "authority-tor.stdout",
            "authority-tor.stderr",
            "authority-notice.log",
            "authority-info.log",
        ] {
            assert!(artifact_kept(name), "{name}");
        }
        for name in [
            "torrc",
            "authority-key",
            "browser-contract.json",
            "receipt.json",
            "x",
        ] {
            assert!(!artifact_kept(name), "{name}");
        }
    }

    #[test]
    fn evidence_pipeline_must_be_path_safe() {
        let directory = tempfile::tempdir().unwrap();
        let cargo = directory.path().join("cargo-home");
        fs::create_dir_all(&cargo).unwrap();
        let base = environment(&[
            ("CARGO_HOME", cargo.to_str().unwrap()),
            ("CI_COMMIT_SHA", "0e53a6a867b0b213c5bbc59c14c1c55d3a26b7b0"),
            ("CI_PIPELINE_NUMBER", "11"),
        ]);
        let first = evidence_root(&base, "ctrn-tor", "private-network").unwrap();
        assert!(first.to_string_lossy().contains(
            "ctrn-tor/evidence/0e53a6a867b0b213c5bbc59c14c1c55d3a26b7b0/11/private-network/attempt-"
        ));
        fs::write(first.join("receipt.json"), "{}").unwrap();
        let second = evidence_root(&base, "ctrn-tor", "private-network").unwrap();
        assert_ne!(first, second);
        assert!(first.join("receipt.json").is_file());
        assert!(!second.join("receipt.json").is_file());
        for bad in ["../escape", "a/b", "..", "x/../../y", "p|pe"] {
            let mut hostile = base.clone();
            hostile.insert(
                OsString::from("CI_PIPELINE_NUMBER"),
                OsString::from(bad.to_string()),
            );
            evidence_root(&hostile, "ctrn-tor", "private-network").unwrap_err();
        }
    }

    #[test]
    fn finish_records_success_detail_and_failure_stages() {
        let directory = tempfile::tempdir().unwrap();
        let evidence = directory.path().join("evidence");
        fs::create_dir_all(&evidence).unwrap();
        let env = environment(&[("CI_PIPELINE_NUMBER", "11")]);
        let stages = vec![json!({"stage": "tor-tools", "seconds": 1.5})];
        finish(
            &evidence,
            &env,
            "private-network",
            "0e53a6a867b0b213c5bbc59c14c1c55d3a26b7b0",
            stages.clone(),
            Ok(json!({"pins": {"cfry": "abc"}})),
        )
        .unwrap();
        let receipt: Value =
            serde_json::from_str(&fs::read_to_string(evidence.join("receipt.json")).unwrap())
                .unwrap();
        assert_eq!(
            receipt.get("outcome").and_then(Value::as_str),
            Some("success")
        );
        assert_eq!(
            receipt
                .get("pins")
                .and_then(|pins| pins.get("cfry"))
                .and_then(Value::as_str),
            Some("abc")
        );
        assert_eq!(
            receipt
                .get("stages")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        finish(
            &evidence,
            &env,
            "private-network",
            "0e53a6a867b0b213c5bbc59c14c1c55d3a26b7b0",
            stages,
            Err(failure("fixture exploded")),
        )
        .unwrap_err();
        let receipt: Value =
            serde_json::from_str(&fs::read_to_string(evidence.join("receipt.json")).unwrap())
                .unwrap();
        assert_eq!(
            receipt.get("outcome").and_then(Value::as_str),
            Some("failure")
        );
        assert_eq!(
            receipt.get("error").and_then(Value::as_str),
            Some("fixture exploded")
        );
    }

    #[test]
    fn transport_source_resolves_the_locked_fixture() {
        let directory = tempfile::tempdir().unwrap();
        let transport = directory.path().join("ctrn-checkout");
        fs::create_dir_all(transport.join(".ci")).unwrap();
        fs::write(transport.join(".ci/tor-tools.sh"), b"tools").unwrap();
        fs::write(transport.join(".ci/private-network.py"), b"fixture").unwrap();
        let manifest = transport.join("Cargo.toml");
        fs::write(&manifest, b"[package]").unwrap();
        let metadata = json!({
            "packages": [
                {
                    "name": "ctrn",
                    "manifest_path": manifest.to_string_lossy(),
                    "source": "git+https://github.com/corbet-foss/ctrn?branch=main#9894d84f726132129c81f03a2c42be131d9aa1ac",
                }
            ],
            "target_directory": directory.path().join("target").to_string_lossy(),
        });
        let source = transport_source(
            &serde_json::to_string(&metadata).unwrap(),
            "9894d84f726132129c81f03a2c42be131d9aa1ac",
        )
        .unwrap();
        assert_eq!(source.dir, transport);
        assert_eq!(source.revision, "9894d84f726132129c81f03a2c42be131d9aa1ac");
        transport_source(
            &serde_json::to_string(&metadata).unwrap(),
            "0000000000000000000000000000000000000000",
        )
        .unwrap_err();
        fs::remove_file(transport.join(".ci/private-network.py")).unwrap();
        transport_source(
            &serde_json::to_string(&metadata).unwrap(),
            "9894d84f726132129c81f03a2c42be131d9aa1ac",
        )
        .unwrap_err();
        transport_source("{}", "9894d84f726132129c81f03a2c42be131d9aa1ac").unwrap_err();
    }

    #[test]
    fn tor_library_env_scopes_worker_store_paths() {
        let env = environment(&[
            ("CI_TOR_LIB_OUT", "/nix/store/aaa /nix/store/bbb"),
            ("CI_TOR_LIB_DEV", "/nix/store/ccc"),
            ("LD_LIBRARY_PATH", "/existing/lib"),
        ]);
        let scoped = tor_library_env(&env).unwrap();
        let get = |key: &str| {
            scoped
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
                .unwrap()
        };
        assert_eq!(get("CC"), "gcc");
        assert_eq!(
            get("LD_LIBRARY_PATH"),
            "/nix/store/aaa/lib:/nix/store/bbb/lib:/existing/lib"
        );
        assert_eq!(
            get("PKG_CONFIG_PATH"),
            "/nix/store/ccc/lib/pkgconfig:/nix/store/ccc/share/pkgconfig"
        );
        tor_library_env(&environment(&[])).unwrap_err();
        tor_library_env(&environment(&[
            ("CI_TOR_LIB_OUT", ""),
            ("CI_TOR_LIB_DEV", ""),
        ]))
        .unwrap_err();
    }

    #[test]
    fn stable_tool_home_is_independent_of_transient_incoming_home() {
        let target = tempfile::tempdir().unwrap();
        let target_path = target.path().to_string_lossy().into_owned();
        let first = environment(&[
            ("CARGO_TARGET_DIR", target_path.as_str()),
            ("CCID_TARGET_LOCK_HELD", target_path.as_str()),
            ("HOME", "/transient/home-a"),
            ("CARGO_HOME", "/cargo"),
            ("RUSTUP_HOME", "/rustup"),
        ]);
        let second = environment(&[
            ("CARGO_TARGET_DIR", target_path.as_str()),
            ("CCID_TARGET_LOCK_HELD", target_path.as_str()),
            ("HOME", "/transient/home-b"),
            ("CARGO_HOME", "/cargo"),
            ("RUSTUP_HOME", "/rustup"),
        ]);
        let first_home = stable_tool_home(&first).unwrap();
        let second_home = stable_tool_home(&second).unwrap();
        assert_eq!(first_home, second_home);
        assert!(Path::new(&first_home).is_absolute());
        assert!(first_home.ends_with("tor-home"));
        assert!(Path::new(&first_home).is_dir());
        // A distinct locked target owns a distinct stable home.
        let other = tempfile::tempdir().unwrap();
        let other_path = other.path().to_string_lossy().into_owned();
        let third = environment(&[
            ("CARGO_TARGET_DIR", other_path.as_str()),
            ("CCID_TARGET_LOCK_HELD", other_path.as_str()),
            ("HOME", "/transient/home-a"),
            ("CARGO_HOME", "/cargo"),
            ("RUSTUP_HOME", "/rustup"),
        ]);
        assert_ne!(stable_tool_home(&third).unwrap(), first_home);
        // Callers overlay HOME while leaving the explicit cargo/rustup roots.
        let mut base = first.clone();
        base.insert(OsString::from("HOME"), OsString::from(first_home.clone()));
        assert_eq!(
            base.get(&OsString::from("CARGO_HOME"))
                .and_then(|value| value.to_str()),
            Some("/cargo")
        );
        assert_eq!(
            base.get(&OsString::from("RUSTUP_HOME"))
                .and_then(|value| value.to_str()),
            Some("/rustup")
        );
        assert_eq!(
            base.get(&OsString::from("HOME"))
                .and_then(|value| value.to_str()),
            Some(first_home.as_str())
        );
    }

    #[test]
    fn stable_tool_home_rejects_unlocked_or_malformed_target() {
        let target = tempfile::tempdir().unwrap();
        let target_path = target.path().to_string_lossy().into_owned();
        // Absent lock fails closed for standalone direct invocations.
        stable_tool_home(&environment(&[
            ("CARGO_TARGET_DIR", target_path.as_str()),
            ("HOME", "/transient/home"),
        ]))
        .unwrap_err();
        // Mismatched lock fails closed.
        let other = tempfile::tempdir().unwrap();
        let other_path = other.path().to_string_lossy().into_owned();
        stable_tool_home(&environment(&[
            ("CARGO_TARGET_DIR", target_path.as_str()),
            ("CCID_TARGET_LOCK_HELD", other_path.as_str()),
            ("HOME", "/transient/home"),
        ]))
        .unwrap_err();
        // Relative or missing target fails closed.
        stable_tool_home(&environment(&[
            ("CARGO_TARGET_DIR", "relative/target"),
            ("CCID_TARGET_LOCK_HELD", "relative/target"),
        ]))
        .unwrap_err();
        stable_tool_home(&environment(&[(
            "CCID_TARGET_LOCK_HELD",
            target_path.as_str(),
        )]))
        .unwrap_err();
    }

    #[test]
    fn tor_short_scratch_bypasses_nested_worker_tmpdir() {
        // Generic worker-nested TMPDIR shapes (long enough to push fixture
        // socket paths past the 108-byte `sun_path` limit, NUL included).
        let incoming_a = format!(
            "/tmp/nested-run-aaaaaaaa/nested-shell-1111111111-2222222222/nested-run-bbbbbbbb/{}",
            "q".repeat(100)
        );
        let incoming_b = format!(
            "/tmp/nested-run-cccccccc/nested-shell-3333333333-4444444444/nested-run-dddddddd/{}",
            "z".repeat(100)
        );
        assert!(incoming_a.len() > 150);
        assert!(incoming_b.len() > 150);
        let mut paths = Vec::new();
        for incoming in [&incoming_a, &incoming_b] {
            let mut base = environment(&[
                ("TMPDIR", incoming.as_str()),
                ("RUNNER_TEMP", incoming.as_str()),
            ]);
            let scratch = tor_scratch().unwrap();
            apply_tor_scratch_env(&mut base, &scratch);
            let path = scratch.path().to_string_lossy().into_owned();
            assert!(
                path.starts_with("/tmp/ccid-tor-"),
                "unexpected scratch: {path}"
            );
            assert!(path.len() < 32, "scratch too long: {path}");
            assert!(
                !path.contains("nested-run"),
                "scratch inherits nesting: {path}"
            );
            for key in ["TMPDIR", "RUNNER_TEMP"] {
                assert_eq!(
                    base.get(&OsString::from(key))
                        .and_then(|value| value.to_str()),
                    Some(path.as_str()),
                    "outgoing {key} must point at the short scratch"
                );
            }
            // Representative worst-case AF_UNIX consumers must fit sun_path
            // (108 bytes including the NUL terminator).
            for tail in [
                "fixture-transport-qqqqqqqq/tor/nodes.1234567890/000a/control",
                "fixture-transport-qqqqqqqq/tor/nodes.1234567890/000a/control.authcookie",
                "browser-profile/SingletonSocket",
            ] {
                let socket = Path::new(&path).join(tail).to_string_lossy().into_owned();
                assert!(
                    socket.len() < 108,
                    "socket path too long ({}): {socket}",
                    socket.len()
                );
            }
            // The helper never touches the huge incoming dir.
            assert!(!Path::new(incoming).exists());
            paths.push(path);
            // RAII cleanup removes the short scratch on drop.
            drop(scratch);
            assert!(!Path::new(paths.last().unwrap()).exists());
        }
        assert_ne!(paths[0], paths[1], "scratches must be unique");
    }

    /// From a permissive starting umask, the Tor process setup yields
    /// owner-private job dirs. umask is process-global, so the mutation runs
    /// in an isolated child: this same test binary re-invoked with only the
    /// ignored probe below selected. The parent never calls umask, hence its
    /// mask (and every parallel test) is unaffected by construction.
    #[cfg(unix)]
    #[test]
    fn tor_process_dirs_are_private_under_permissive_umask() {
        // umask is process-global, so the mutation runs in an isolated child:
        // this same test binary re-invoked with only the ignored probe below
        // selected. The parent never calls umask, hence its mask (and every
        // parallel test) is unaffected by construction. The fully qualified
        // filter plus the one-test assertion below keep a zero-test pass from
        // ever counting as success.
        let filter = "tor::tests::tor_umask_probe_child";
        let exe = std::env::current_exe().unwrap();
        let output = std::process::Command::new(exe)
            .arg(filter)
            .args(["--exact", "--ignored"])
            .env("CCID_TOR_UMASK_PROBE_CHILD", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success()
                && stdout.contains("test result: ok. 1 passed")
                && !stdout.contains("0 passed"),
            "tor umask probe child failed (status: {}):\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            output.status,
            stdout,
            stderr,
        );
    }

    /// Subprocess probe for `tor_process_dirs_are_private_under_permissive_umask`.
    /// Runs its assertions only when spawned by that parent (marker env set);
    /// otherwise it is a no-op pass, so ordinary suite runs — including any
    /// `--ignored`/`--include-ignored` invocation — stay umask-neutral. Uses
    /// the existing test binary, not a new CLI feature.
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn tor_umask_probe_child() {
        use std::os::unix::fs::PermissionsExt;
        if std::env::var_os("CCID_TOR_UMASK_PROBE_CHILD").is_none() {
            return;
        }
        let mode_of = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        // Permissive starting point, as on shared CI workers: group/other bits
        // flow through to default-created directories.
        rustix::process::umask(rustix::fs::Mode::WOTH);
        let canary = tempfile::Builder::new()
            .prefix("ccid-umask-canary-")
            .tempdir_in("/tmp")
            .unwrap();
        let canary_path = canary.path().to_string_lossy().into_owned();
        assert_eq!(
            mode_of(canary.path()),
            0o775,
            "permissive premise not established: {canary_path}"
        );
        // Production setup: restrictive mask first, then the Tor scratch.
        restrict_tor_process_umask();
        // Re-asserting the mask returns the previous one, proving the
        // production call above took effect (no return-value plumbing needed).
        assert_eq!(
            rustix::process::umask(tor_process_umask()),
            tor_process_umask()
        );
        let scratch = tor_scratch().unwrap();
        let scratch_path = scratch.path().to_string_lossy().into_owned();
        assert!(
            scratch_path.starts_with("/tmp/ccid-tor-"),
            "unexpected scratch: {scratch_path}"
        );
        assert!(scratch_path.len() < 32, "scratch too long: {scratch_path}");
        assert_eq!(scratch.path().parent(), Some(Path::new("/tmp")));
        assert_eq!(mode_of(scratch.path()), 0o700);
        // Descendant fixture/probe tempdir plus a secret-like file, mirroring
        // the Chutney node dirs and guard/key files Arti protects.
        let descendant = tempfile::Builder::new()
            .prefix("probe-")
            .tempdir_in(scratch.path())
            .unwrap();
        assert_eq!(mode_of(descendant.path()), 0o700);
        let secret = descendant.path().join("guards.json");
        std::fs::write(&secret, b"{interrupted guard state").unwrap();
        assert_eq!(mode_of(&secret), 0o600);
        // Representative worst-case socket still fits sun_path with NUL room.
        let socket = descendant
            .path()
            .join("nodes.1234567890/000a/control.authcookie")
            .to_string_lossy()
            .into_owned();
        assert!(socket.len() < 108, "socket too long: {socket}");
        // Native scratch shares the leaf mechanics under the trusted
        // `/var/tmp` root; the Snowflake `/tmp` path above is unchanged.
        let native = native_tor_scratch().unwrap();
        let native_path = native.path().to_string_lossy().into_owned();
        assert!(
            native_path.starts_with("/var/tmp/ccid-tor-"),
            "unexpected native scratch: {native_path}"
        );
        assert_eq!(native.path().parent(), Some(Path::new("/var/tmp")));
        assert!(
            native_path.len() < 32,
            "native scratch too long: {native_path}"
        );
        assert_eq!(mode_of(native.path()), 0o700);
        let native_secret = native.path().join("guards.json");
        std::fs::write(&native_secret, b"{interrupted guard state").unwrap();
        assert_eq!(mode_of(&native_secret), 0o600);
        let native_socket = native
            .path()
            .join("nodes.1234567890/000a/control.authcookie")
            .to_string_lossy()
            .into_owned();
        assert!(
            native_socket.len() < 108,
            "native socket too long: {native_socket}"
        );
        // The pre-existing permissive dir is untouched by the setup.
        assert_eq!(mode_of(canary.path()), 0o775);
        // Unique owned paths with RAII cleanup (innermost first).
        let second = tor_scratch().unwrap();
        assert_ne!(scratch.path(), second.path());
        let second_path = second.path().to_string_lossy().into_owned();
        drop(descendant);
        drop(second);
        drop(scratch);
        drop(native);
        assert!(!Path::new(&scratch_path).exists());
        assert!(!Path::new(&second_path).exists());
        assert!(!Path::new(&native_path).exists());
        drop(canary);
    }

    const SAMPLE_LIVE_INPUTS: &str = r#"
schema = 1
[live-inputs.v01-source-bundle]
archive_variable = "V01_SOURCE_BUNDLE"
digest_variable = "V01_SOURCE_BUNDLE_SHA256"
sha256 = "a89129a0f4e827c807f1453ef40c5968d1a063267d4be8741783abdaa31e82b6"
default_path = "/workspaces/ci-sources/v01-tor-records/a89129a0f4e827c807f1453ef40c5968d1a063267d4be8741783abdaa31e82b6.tar"
workflows = ["v01-tor-live"]
[live-inputs.v01-chutney]
archive_variable = "V01_CHUTNEY_SOURCE_ARCHIVE"
digest_variable = "V01_CHUTNEY_SOURCE_SHA256"
sha256 = "3d8748142f5d1fc3243371b33ff5a431d444c69b836d22718898f1787eec00ee"
default_path = "/workspaces/ci-sources/10-source-chutney/3d8748142f5d1fc3243371b33ff5a431d444c69b836d22718898f1787eec00ee.tar"
workflows = ["v01-tor-live"]
[live-inputs.v01-root-source]
archive_variable = "V01_ROOT_SOURCE_ARCHIVE"
digest_variable = "V01_ROOT_SOURCE_SHA256"
sha256 = "a84b41fcf68d71507db9e46072c3c329ec1b2667114c38fe3ff275bd7ab6cb1d"
default_path = "/workspaces/ci-sources/146/a84b41fcf68d71507db9e46072c3c329ec1b2667114c38fe3ff275bd7ab6cb1d.tar"
commit = "bb0f72d89dbfc5323580205740139fc84cc7a284"
workflows = ["v01-tor-live"]
[live-tools.receipt]
path = "/workspaces/component-releases/cmsg/20a55ac8159811bbac7c8370ae7f28fc075f2153/tor-tools/tools.json"
sha256 = "b3a64bbc373b886dc164e859c1d26e7a4055a7d65b8c2556a0bd2cff4ac152e0"
[live-tools.tor]
path = "/workspaces/component-tools/cmsg/tor-0.4.9.12-20a55ac8159811bbac7c8370ae7f28fc075f2153/bin/tor"
sha256 = "3b03c797db84b76bcde96f68997656cc42368b173c0da88b40d5acc2d666cc1b"
[live-tools.tor-gencert]
path = "/workspaces/component-tools/cmsg/tor-0.4.9.12-20a55ac8159811bbac7c8370ae7f28fc075f2153/bin/tor-gencert"
sha256 = "68df7a29ae0bed669df63b4c8d8658240cb951618860623212fdbd8993923114"
[live-tools.python]
path = "/workspaces/component-tools/cmsg/python-20a55ac8159811bbac7c8370ae7f28fc075f2153/bin/python"
sha256 = "f5cce9ecc914b0c2eee78056c1c816aa02ae4c73c0aaf958eb5e0bb9281f34ea"
[live-tools.rustc]
path = "/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin/rustc"
sha256 = "8f40f2f394f7ff470ccfac7da1f03661756216a77fdb3181bfb81bbf01e07cdb"
[live-tools.cargo]
path = "/nix/store/w20n3pmhhd1av9llykxa3gd21c9jsm8l-cargo-1.98.1/bin/cargo"
sha256 = "88a18d3c29700de42bc2a2f5590e919e36557964cf4e1dd2f60f892376f6c102"
[live-tools.rustdoc]
path = "/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin/rustdoc"
sha256 = "999b2098594b2b8d91db71e11bfbb2c0bd8cf24acac792308ac08c267f813bcb"
[live-tools.rustfmt]
path = "/nix/store/jpcqlbhkwxwvq507mq0hkacpxbxcdwjj-rustfmt-1.98.1/bin/rustfmt"
sha256 = "9fc2eff4ce7281f2a77c0164ab8e8b55fcef98f3c8ba96d4e4179ba8f3abc7fd"
[live-tools.cargo-clippy]
path = "/nix/store/n88pdyarvdpyw98fahxcb03733nsclhq-clippy-1.98.1/bin/cargo-clippy"
sha256 = "4396915d16e54967584fb1730560ca5c92bbea823398a8749c1e05109ad8e39c"
[live-tools.clippy-driver]
path = "/nix/store/n88pdyarvdpyw98fahxcb03733nsclhq-clippy-1.98.1/bin/clippy-driver"
sha256 = "0616995d944bbdd0b2bc9aee822d4a0cb07b05f8eabf01367a67fc94de05089f"
[live-tools.wasm-libcore]
path = "/nix/store/xvp6nfxayb07si2jaggqwvx3iykw89g2-rustc-1.98.1/lib/rustlib/wasm32-unknown-unknown/lib/libcore-e6b063672db74229.rlib"
sha256 = "e917c01a724e0622601835f8a6f1c8bff102f5f40a61152ce95a099a4bc7ba61"
[live-tools.wasm-libstd]
path = "/nix/store/xvp6nfxayb07si2jaggqwvx3iykw89g2-rustc-1.98.1/lib/rustlib/wasm32-unknown-unknown/lib/libstd-0d5130a4ee2cc288.rlib"
sha256 = "61ce675fface73dbbf431603767a3aa7f05bf9d6995d0555855fa5e4ead667e6"
[diag.discovery-lib]
file = "src/tor_discovery.rs"
sha256 = "fc6c8db74254e6246bb6da69228e4afe2c17ff42575133bb3ab1b530d2c30041"
diagnostic_sha256 = "f67ec53ed7f19e9bf7293eca631a24687b14fc2558109269edcbd901fade91d7"
[live-drivers.harness]
file = ".ci/v01-tor-live.py"
sha256 = "20314ee07fc2adaae05c4018268abec49865c2a78e0710519fa55de0b1c5cc5b"
"#;

    fn live_repo(text: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".ci")).unwrap();
        fs::write(root.path().join(".ci/live-inputs.toml"), text).unwrap();
        root
    }

    fn shared_manifest(text: &str) -> tor_inputs::Manifest {
        let root = live_repo(text);
        tor_inputs::load_manifest(root.path())
            .unwrap()
            .expect("manifest present")
    }

    #[test]
    fn v01_inputs_enforce_exact_input_set_through_shared_parser() {
        let manifest = shared_manifest(SAMPLE_LIVE_INPUTS);
        let inputs = v01_inputs(&manifest, &["v01-tor-live"]).unwrap();
        assert_eq!(inputs.len(), 3);
        let root = inputs
            .iter()
            .find(|input| input.name == "v01-root-source")
            .unwrap();
        assert_eq!(
            root.commit.as_deref(),
            Some("bb0f72d89dbfc5323580205740139fc84cc7a284")
        );
        assert_eq!(root.digest_variable, "V01_ROOT_SOURCE_SHA256");
        // A manifest missing one entry, or scoped to another workflow,
        // fails the exact-set gate rather than running degraded.
        let lopped =
            SAMPLE_LIVE_INPUTS.replace("[live-inputs.v01-chutney]", "[live-inputs.v01-chutney-x]");
        assert!(v01_inputs(&shared_manifest(&lopped), &["v01-tor-live"]).is_err());
        assert!(v01_inputs(&manifest, &["tor-records"]).is_err());
    }

    #[test]
    fn v01_inputs_reject_renamed_or_duplicated_variables() {
        let manifest = shared_manifest(SAMPLE_LIVE_INPUTS);
        assert_eq!(v01_inputs(&manifest, &["v01-tor-live"]).unwrap().len(), 3);
        // Renamed pair on one entry fails before launch.
        let renamed = SAMPLE_LIVE_INPUTS.replace(
            "archive_variable = \"V01_CHUTNEY_SOURCE_ARCHIVE\"",
            "archive_variable = \"V01_SOURCE_BUNDLE\"",
        );
        assert!(v01_inputs(&shared_manifest(&renamed), &["v01-tor-live"]).is_err());
        // Digest variable swapped onto the wrong entry fails as well.
        let swapped = SAMPLE_LIVE_INPUTS.replace(
            "digest_variable = \"V01_ROOT_SOURCE_SHA256\"",
            "digest_variable = \"V01_SOURCE_BUNDLE_SHA256\"",
        );
        assert!(v01_inputs(&shared_manifest(&swapped), &["v01-tor-live"]).is_err());
    }

    fn tool_map(manifest: &tor_inputs::Manifest) -> BTreeMap<String, String> {
        manifest
            .live_tools
            .iter()
            .map(|(name, entry)| (name.clone(), entry.path.clone()))
            .collect()
    }

    #[test]
    fn v01_tool_env_binds_retained_toolchain_first() {
        let manifest = shared_manifest(SAMPLE_LIVE_INPUTS);
        let tools = tool_map(&manifest);
        assert_eq!(tools.len(), 12);
        let mut base = environment(&[("PATH", "/usr/bin:/bin")]);
        let bound: BTreeMap<String, String> =
            v01_tool_env(&tools, &base).unwrap().into_iter().collect();
        let path = &bound["PATH"];
        // Four unique retained bin dirs (rustc+rustdoc share one), ahead
        // of the existing worker PATH, each exactly once.
        let first: Vec<&str> = path.split(':').take(4).collect();
        assert_eq!(
            first,
            [
                "/nix/store/jpcqlbhkwxwvq507mq0hkacpxbxcdwjj-rustfmt-1.98.1/bin",
                "/nix/store/n88pdyarvdpyw98fahxcb03733nsclhq-clippy-1.98.1/bin",
                "/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin",
                "/nix/store/w20n3pmhhd1av9llykxa3gd21c9jsm8l-cargo-1.98.1/bin",
            ]
        );
        assert!(path.ends_with(":/usr/bin:/bin"));
        assert_eq!(
            bound["RUSTC"],
            "/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin/rustc"
        );
        assert_eq!(
            bound["CARGO"],
            "/nix/store/w20n3pmhhd1av9llykxa3gd21c9jsm8l-cargo-1.98.1/bin/cargo"
        );
        assert_eq!(
            bound["RUSTDOC"],
            "/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin/rustdoc"
        );
        // A missing required tool refuses instead of falling back.
        let mut short = tools.clone();
        short.remove("rustc");
        assert!(v01_tool_env(&short, &base).is_err());
        // Empty worker PATH still yields a usable binding.
        base.remove(&OsString::from("PATH"));
        let bare: BTreeMap<String, String> =
            v01_tool_env(&tools, &base).unwrap().into_iter().collect();
        assert!(!bare["PATH"].contains("::"));
        assert!(!bare["PATH"].ends_with(':'));
    }

    #[test]
    fn v01_digest_tampering_fails_closed_at_resolution() {
        // Shape-valid tampering parses (the shared loader checks shape);
        // resolution against the digest refuses.
        let tampered = SAMPLE_LIVE_INPUTS.replace("a89129a0", "b89129a0");
        let manifest = shared_manifest(&tampered);
        let bundle = &manifest.live_inputs["v01-source-bundle"];
        assert!(bundle.sha256.starts_with("b89129a0"));
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("bundle.tar");
        fs::write(&file, b"tampered").unwrap();
        assert!(require_digest(&file, &bundle.sha256).is_err());
        // Unknown sections, traversal paths, and bad digests refuse at load.
        let unknown = format!("{SAMPLE_LIVE_INPUTS}\n[extra]\n");
        let root = live_repo(&unknown);
        assert!(tor_inputs::load_manifest(root.path()).is_err());
        let traversal = SAMPLE_LIVE_INPUTS.replace(
            "/workspaces/ci-sources/v01-tor-records/",
            "/workspaces/../x/",
        );
        let root = live_repo(&traversal);
        assert!(tor_inputs::load_manifest(root.path()).is_err());
        let absent = tempfile::tempdir().unwrap();
        assert!(tor_inputs::load_manifest(absent.path()).unwrap().is_none());
    }

    #[test]
    fn live_fixed_and_require_digest_fail_closed() {
        let manifest = shared_manifest(SAMPLE_LIVE_INPUTS);
        let receipt = &manifest.live_tools["receipt"];
        // Missing file fails before any hash comparison. The committed
        // ambient path is not used here: unit tests must not depend on
        // worker ambient state, where the retained receipt exists.
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("absent.json");
        assert!(require_digest(&missing, &receipt.sha256).is_err());
        // Tampered content fails the digest gate; exact content passes.
        let file = directory.path().join("tool");
        fs::write(&file, b"tampered").unwrap();
        assert!(require_digest(&file, &receipt.sha256).is_err());
        fs::write(&file, b"").unwrap();
        assert!(require_digest(&file, &receipt.sha256).is_err());
        let empty_sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(require_digest(&file, empty_sha).is_ok());
    }

    #[test]
    fn v01_tmpdir_normalization_touches_exactly_one_literal() {
        let original = "head\n    with tempfile.TemporaryDirectory(prefix=\"v01tor-\", dir=\"/tmp\") as scratch:\ntail\n";
        let normalized = v01_normalize_tmpdir(original).unwrap();
        assert!(normalized.contains("dir=\"/var/tmp\""));
        assert!(!normalized.contains("dir=\"/tmp\""));
        // Every other byte is preserved: only the adapted span differs.
        assert_eq!(
            normalized.len() as i64 - original.len() as i64,
            "var/tmp".len() as i64 - "tmp".len() as i64
        );
        assert!(normalized.starts_with("head\n"));
        assert!(normalized.ends_with("tail\n"));
        assert!(v01_normalize_tmpdir("no scratch here").is_err());
        let duplicated = format!("{original}{original}");
        assert!(v01_normalize_tmpdir(&duplicated).is_err());
    }
}

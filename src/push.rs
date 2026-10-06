//! Shared newest-head push coalescing for one consumer job.
//!
//! All push pipelines for the same canonical consumer URL, branch, and job
//! share one namespace under an existing persistent cache root. Each event
//! records canonical trigger provenance (repository URL, branch, commit) and
//! a monotonic generation under a short notification lock, observes a bounded
//! quiet period, then either attaches to a completed receipt that already
//! covers its trigger (tool, runtime, and config identity matched first,
//! then graph proven) or becomes the single builder under the execution
//! lock. The admitted generation is frozen before graph resolution: events
//! arriving during the build stay pending for exactly one later latest-head
//! run. Failures propagate only to triggers they actually cover; a missing,
//! corrupt, or crashed receipt never counts as success. Receipts are never
//! pruned here. Every wait (debounce, lock, ancestry) is bounded by the
//! caller's overall deadline; contention is reported distinctly from IO or
//! corrupt-state errors. Locks are kernel file locks: a crashed holder
//! releases automatically, and state files are written atomically.
use crate::{
    failure,
    jobs::{is_self_checks_command, Refresh},
    Result,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Bounded quiet observation (seconds) before the admitted build starts.
pub const QUIET_SECS: u64 = 10;
/// Upper bound (seconds) for one burst to settle.
pub const MAX_SECS: u64 = 30;
/// Bounded wait for the short notification lock.
const NOTIFY_WAIT_SECS: u64 = 60;
/// Poll interval (milliseconds) while another builder holds the execution lock.
const ATTACH_POLL_MILLIS: u64 = 100;
/// Bound for one ancestry probe (fetch + merge-base).
const ANCESTRY_SECS: u64 = 120;
/// Bound for persisted pending triggers per namespace; fail closed past it.
const PENDING_CAP: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    Consumer,
    Dependency,
}

/// One push event. Coverage is always proven against the admitted receipt's
/// selected graph, never assumed from package name alone: the canonical
/// trigger repository URL plus branch plus commit identify the event.
#[derive(Debug, Clone)]
pub struct Trigger {
    pub kind: TriggerKind,
    /// Canonical forge repository URL the event came from (no credentials).
    pub repo: String,
    pub branch: String,
    pub sha: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PushSpec {
    pub consumer_url: String,
    pub consumer_branch: String,
    pub job: String,
    pub trigger: Trigger,
    pub cache_root: PathBuf,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// This caller built the admitted generation.
    Built { generation: u64 },
    /// This caller reused a completed receipt covering its trigger.
    Attached { generation: u64 },
}

/// Narrow identity supplied by production (graph half) through one callback
/// type. The coalescer never reimplements staging or resolution to derive
/// it: the builder records what production observed, attach requires exact
/// equality before any ancestry. `runtime_identity` binds the actual
/// compiler/runtime/environment captured around the gates with the effective
/// job environment (rustc, cargo, clippy, whitelisted compile env); probes
/// fail closed, never a reusable sentinel. `config_identity` binds the live
/// consumer full SHA plus the selected job (`{head}:{job}`): the commit pins
/// the immutable manifest, check definitions, and env settings, so a new
/// manifest or source always invalidates prior proof without parsing a
/// remote manifest at every attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildIdentity {
    pub tool_revision: String,
    pub runtime_identity: String,
    pub config_identity: String,
}

/// Canonical config binding: live consumer head plus selected job.
pub fn config_for(live_head: &str, job: &str) -> String {
    format!("{}:{job}", live_head.trim())
}

/// What the builder actually produced: the proved graph plus the runtime
/// captured around the gates and the per-source selected branches. The
/// graph worker fills `runtime_identity` by calling `capture_runtime`
/// before and after the gates with the merged gate environment, requiring
/// equality via `require_runtime_unchanged`, and fills `dep_branches` from
/// the resolved selection (package name to selected source branch).
#[derive(Debug, Clone)]
pub struct BuildOutput {
    pub body: ReceiptBody,
    pub runtime_identity: String,
    pub dep_branches: BTreeMap<String, String>,
}

/// Fresh live proof for one attach decision: current identity (whose config
/// already binds the live head plus job) and the live head itself, so
/// callers compare a single fetch against the receipt without re-fetching.
#[derive(Debug, Clone)]
pub struct LiveProof {
    pub identity: BuildIdentity,
    pub live_head: String,
}

/// Live proof callbacks owned by production; tests inject fakes over local
/// git fixtures. All probes respect the caller's overall deadline and never
/// reset it.
pub trait CoalescePolicy {
    /// Current proof for `spec`: live head plus tool, runtime, and config
    /// identity. Err means "unproven": the caller builds instead of
    /// attaching. Carries the caller deadline for every probe.
    fn current_proof(&self, spec: &PushSpec, deadline: Instant) -> Result<LiveProof>;
    /// Ancestor-or-equal on the trigger's own repository and branch.
    /// False means "unproven": the caller builds instead of attaching.
    fn is_ancestor(&self, url: &str, branch: &str, old: &str, new: &str, deadline: Instant)
        -> bool;
}

/// Production policy: real bounded git probes with exact ref validation and
/// a real toolchain fingerprint over the effective worker environment.
/// Config is always the live head plus job, never empty.
pub struct ProductionPolicy;

impl CoalescePolicy for ProductionPolicy {
    fn current_proof(&self, spec: &PushSpec, deadline: Instant) -> Result<LiveProof> {
        let live_head = fetch_live_head(&spec.consumer_url, &spec.consumer_branch, deadline)?;
        // The build gates run with the manifest-owned per-job environment
        // merged in (see stage_consumer), so attach must fingerprint that
        // same merged environment: read the COMMITTED manifest at the live
        // head from the owned object store, never the dirty filesystem.
        // Anything unavailable fails closed (caller builds instead).
        let effective = production_effective_environment(spec, &live_head, deadline)?;
        let runtime_identity = capture_runtime(&effective, deadline)?;
        Ok(LiveProof {
            identity: BuildIdentity {
                tool_revision: crate::SOURCE_REVISION.into(),
                runtime_identity,
                config_identity: config_for(&live_head, &spec.job),
            },
            live_head,
        })
    }
    fn is_ancestor(
        &self,
        url: &str,
        branch: &str,
        old: &str,
        new: &str,
        deadline: Instant,
    ) -> bool {
        proven_ancestor_bounded(url, branch, old, new, deadline)
    }
}

/// Effective compile environment for one attach decision: the ambient worker
/// environment overlaid with the manifest-owned per-job environment read
/// from the COMMITTED `.ci/ccid.toml` at `live_head` in the owned namespace
/// object store. Read-only: never fetches, resets, or rebuilds; never reads
/// the dirty workdir filesystem manifest. Any unavailable or invalid store
/// state fails closed with `Err`, so the caller builds (which re-stages and
/// re-verifies) instead of attaching under a guessed environment.
///
/// Callers only attach when the live head equals the receipt head, so the
/// committed manifest read here is exactly the receipt's configuration.
fn production_effective_environment(
    spec: &PushSpec,
    live_head: &str,
    deadline: Instant,
) -> Result<crate::Environment> {
    if Instant::now() >= deadline {
        return Err(failure(
            "Push run deadline exceeded before attach environment",
        ));
    }
    if !valid_sha(live_head) {
        return Err(failure("Live consumer head is not a full commit"));
    }
    let dir = namespace_dir(
        &spec.cache_root,
        &spec.consumer_url,
        &spec.consumer_branch,
        &spec.job,
    )?;
    // The namespace must already belong to this consumer+branch+job;
    // otherwise there is no owned store to read from.
    let owner_value = serde_json::json!({
        "consumer_url": crate::cache::canonical_repository(&spec.consumer_url)
            .map_err(|_| failure("Consumer URL must be a plain forge URL"))?,
        "consumer_branch": spec.consumer_branch,
        "job": spec.job,
    });
    let owner_bytes = serde_json::to_string(&owner_value)?;
    match std::fs::read(dir.join("owner.json")) {
        Ok(existing) => {
            if existing != owner_bytes.as_bytes() {
                return Err(failure("Shared workdir belongs to another consumer"));
            }
        }
        Err(_) => {
            return Err(failure(
                "No owned consumer checkout for attach; refusing a guessed environment",
            ));
        }
    }
    let work = dir.join("work");
    let mut git_env: crate::Environment = BTreeMap::new();
    git_env.insert(
        std::ffi::OsString::from("PATH"),
        std::env::var_os("PATH").unwrap_or_default(),
    );
    for (key, value) in [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_LFS_SKIP_SMUDGE", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("LC_ALL", "C"),
    ] {
        git_env.insert(
            std::ffi::OsString::from(key),
            std::ffi::OsString::from(value),
        );
    }
    let runner = crate::Runner::until(work.clone(), git_env, deadline)?;
    let git = |args: &[&str]| {
        let mut argv = vec![
            "git".into(),
            "-c".into(),
            "core.hooksPath=/dev/null".into(),
            "-c".into(),
            "credential.helper=".into(),
            "-c".into(),
            "fetch.fsckObjects=true".into(),
        ];
        argv.extend(args.iter().map(|s| (*s).into()));
        runner.run(&argv, true)
    };
    // Origin binding: the owned checkout must belong to this consumer.
    // Read-only; a missing or foreign checkout fails closed.
    match git(&["remote", "get-url", "origin"]) {
        Ok(existing) => {
            if !canonical_eq(existing.trim(), &spec.consumer_url)? {
                return Err(failure("Shared workdir belongs to another consumer"));
            }
        }
        Err(_) => {
            return Err(failure(
                "No owned consumer checkout for attach; refusing a guessed environment",
            ));
        }
    }
    // Immutable config: the committed manifest blob at the live head, not
    // the workdir filesystem (which may hold dirty or newer bytes). Absent
    // objects (head not yet staged locally) fail closed to a build.
    let committed = git(&[
        "cat-file",
        "-p",
        format!("{live_head}:.ci/ccid.toml").as_str(),
    ])?;
    let manifest: toml::Value = toml::from_str(&committed)?;
    let mut declared: BTreeMap<String, String> = BTreeMap::new();
    if let Some(table) = manifest
        .get("jobs")
        .and_then(|jobs| jobs.get(&spec.job))
        .and_then(|job| job.get("environment"))
    {
        let table = table
            .as_table()
            .ok_or_else(|| failure("Job environment must be a string table"))?;
        for (key, value) in table {
            let value = value
                .as_str()
                .ok_or_else(|| failure("Job environment values must be strings"))?;
            declared.insert(key.clone(), value.to_owned());
        }
    }
    crate::jobs::validate_job_environment(&declared)?;
    let mut effective: crate::Environment = std::env::vars_os().collect();
    crate::jobs::apply_job_environment(&mut effective, &declared);
    Ok(effective)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TriggerSer {
    kind: String,
    #[serde(default)]
    repo: String,
    branch: String,
    sha: String,
    name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingEntry {
    generation: u64,
    trigger: TriggerSer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DepSel {
    url: String,
    fetch: String,
    sha: String,
    /// Selected source branch (ref) for this dependency. Empty on receipts
    /// written before this correction, which therefore never attach for
    /// dependency triggers. The graph worker populates it from the resolved
    /// selection; see COALESCE-FIX-READY.md.
    #[serde(default)]
    branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Receipt {
    generation: u64,
    /// Generation frozen before resolution; events above it stay pending.
    #[serde(default)]
    admitted: u64,
    outcome: String,
    error: String,
    consumer_url: String,
    consumer_branch: String,
    consumer_commit: String,
    deps: BTreeMap<String, DepSel>,
    tool_revision: String,
    /// Actual toolchain/runtime fingerprint at build time; empty on receipts
    /// written before this correction, which therefore never attach.
    #[serde(default)]
    runtime_identity: String,
    /// Consumer configuration fingerprint at build time (manifest, checks).
    /// Empty on old receipts, which therefore never attach.
    #[serde(default)]
    config_identity: String,
    manifest_sha: String,
    job: String,
    checks: Vec<String>,
    rematerialized: Vec<String>,
    trigger: TriggerSer,
    /// Every trigger admitted to this build. Failure receipts propagate only
    /// to identical members of this set, never by graph coverage.
    #[serde(default)]
    admitted_triggers: Vec<TriggerSer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    generation: u64,
    receipt_generation: u64,
    /// Pending notifications not yet consumed by a completed receipt.
    /// Truncated only for generations consumed by a recorded receipt; never
    /// pruned otherwise. Crash-safe: a missing state never attaches.
    #[serde(default)]
    pending: Vec<PendingEntry>,
}

/// What a builder selected and proved. Production fills this from staging,
/// refresh, gates, and the final lock; tests inject it.
#[derive(Debug, Clone)]
pub struct ReceiptBody {
    pub outcome: String,
    pub error: String,
    pub consumer_commit: String,
    pub deps: BTreeMap<String, DepSelBody>,
    pub rematerialized: Vec<String>,
    pub manifest_sha: String,
    pub checks: Vec<String>,
    pub trigger: Trigger,
}

#[derive(Debug, Clone)]
pub struct DepSelBody {
    pub url: String,
    pub fetch: String,
    pub sha: String,
}

pub fn validate_spec(spec: &PushSpec) -> Result<()> {
    let consumer_canonical = crate::cache::canonical_repository(&spec.consumer_url)
        .map_err(|_| failure("Consumer URL must be a plain forge URL"))?;
    for value in [&spec.consumer_branch, &spec.job] {
        if value.is_empty()
            || value.contains("CCID")
            || !value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        {
            return Err(failure("Consumer branch and job must be plain"));
        }
    }
    validate_trigger(&spec.trigger)?;
    let trigger_canonical = crate::cache::canonical_repository(&spec.trigger.repo)
        .map_err(|_| failure("Trigger repository must be a plain forge URL"))?;
    match spec.trigger.kind {
        TriggerKind::Consumer => {
            if trigger_canonical != consumer_canonical {
                return Err(failure(
                    "Consumer triggers must name the consumer repository",
                ));
            }
            if spec.trigger.branch != spec.consumer_branch {
                return Err(failure(
                    "Consumer trigger branch must match the consumer branch",
                ));
            }
        }
        TriggerKind::Dependency => {
            if trigger_canonical == consumer_canonical {
                return Err(failure(
                    "Dependency triggers must name a dependency repository",
                ));
            }
        }
    }
    if !spec.cache_root.is_absolute() {
        return Err(failure("Coalescing requires an absolute cache root"));
    }
    if spec.timeout_secs == 0 {
        return Err(failure("Coalescing requires a positive timeout"));
    }
    Ok(())
}

fn validate_trigger(trigger: &Trigger) -> Result<()> {
    crate::cache::canonical_repository(&trigger.repo)
        .map_err(|_| failure("Trigger repository must be a plain forge URL"))?;
    if trigger.branch.is_empty()
        || !trigger
            .branch
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        return Err(failure("Trigger branch must be plain"));
    }
    if !valid_sha(&trigger.sha) {
        return Err(failure("Trigger commit must be a full Git SHA"));
    }
    match (&trigger.kind, &trigger.name) {
        (TriggerKind::Consumer, None) => Ok(()),
        (TriggerKind::Consumer, Some(_)) => Err(failure("Consumer triggers carry no package name")),
        (TriggerKind::Dependency, Some(name))
            if !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)) =>
        {
            Ok(())
        }
        (TriggerKind::Dependency, _) => {
            Err(failure("Dependency triggers require a plain package name"))
        }
    }
}

fn trigger_ser(trigger: &Trigger) -> TriggerSer {
    TriggerSer {
        kind: trigger_kind_name(&trigger.kind),
        repo: trigger.repo.clone(),
        branch: trigger.branch.clone(),
        sha: trigger.sha.clone(),
        name: trigger.name.clone(),
    }
}

pub(crate) fn valid_sha(value: &str) -> bool {
    [40, 64].contains(&value.len())
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

pub(crate) fn namespace_dir(root: &Path, url: &str, branch: &str, job: &str) -> Result<PathBuf> {
    let canonical = crate::cache::canonical_repository(url)
        .map_err(|_| failure("Consumer URL must be a plain forge URL"))?;
    let readable: String = canonical
        .rsplit('/')
        .next()
        .unwrap_or("local")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let digest = format!(
        "{:x}",
        sha2::Sha256::digest(format!("{canonical}\n{branch}\n{job}").as_bytes())
    );
    Ok(root
        .join("ccid-coalesce")
        .join(format!("{readable}-{digest}")))
}

/// Contention (another live builder holds the lock) is distinct from
/// hard IO failures: callers wait on contention but fail loudly on IO or
/// corrupt state. Kernel file lock, released automatically on crash.
/// Test-only: production paths use deadline-capped `lock_file_until` and
/// `try_lock_file`.
#[cfg(test)]
pub(crate) fn lock_file(path: &Path, wait_secs: u64) -> Result<File> {
    let overall = Instant::now()
        .checked_add(Duration::from_secs(wait_secs))
        .ok_or_else(|| failure("Coalescing lock deadline is out of range"))?;
    match try_lock_file(path, wait_secs, overall)? {
        Some(held) => Ok(held),
        None => Err(failure(
            "Coalescing lock is held past its bound (contention)",
        )),
    }
}

/// Production lock acquisition capped by the caller's overall deadline:
/// waits only while time remains, never inventing a fresh bound.
/// Ok(held) or Err (contention past the deadline, hard IO, or overrun).
/// Tests and non-job paths keep using `lock_file` with explicit bounds.
fn lock_file_until(path: &Path, overall: Instant) -> Result<File> {
    if Instant::now() >= overall {
        return Err(failure("Push run deadline exceeded waiting for lock"));
    }
    // Truncation only shortens the wait; the overall cap still rules.
    let wait_secs = overall.duration_since(Instant::now()).as_secs();
    match try_lock_file(path, wait_secs, overall)? {
        Some(held) => Ok(held),
        None => Err(failure("Push run deadline exceeded waiting for lock")),
    }
}

/// Bounded lock attempt capped by both `wait_secs` and `overall`.
/// Ok(Some) is held, Ok(None) is live contention, Err is hard IO.
fn try_lock_file(path: &Path, wait_secs: u64, overall: Instant) -> Result<Option<File>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let bound = Instant::now()
        .checked_add(Duration::from_secs(wait_secs))
        .ok_or_else(|| failure("Coalescing lock deadline is out of range"))?
        .min(overall);
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(Some(lock)),
            Err(std::fs::TryLockError::WouldBlock)
                if Instant::now() < bound
                    && !crate::INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= overall {
                    return Err(failure("Push run deadline exceeded waiting for lock"));
                }
                return Ok(None);
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

fn load_state(dir: &Path) -> Result<State> {
    let path = dir.join("state.json");
    match std::fs::read(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(State {
            generation: 0,
            receipt_generation: 0,
            pending: Vec::new(),
        }),
        Err(error) => Err(error.into()),
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|_| failure("Coalescing state is corrupt"))
        }
    }
}

fn store_state(dir: &Path, state: &State) -> Result<()> {
    if state.pending.len() > PENDING_CAP {
        return Err(failure(
            "Coalescing pending queue is full; refusing to drop provenance",
        ));
    }
    let bytes = serde_json::to_string(state)?;
    let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
    use std::io::Write;
    temporary.write_all(bytes.as_bytes())?;
    temporary
        .persist(dir.join("state.json"))
        .map_err(|error| failure(format!("Coalescing state write refused: {error}")))?;
    Ok(())
}

fn trigger_kind_name(kind: &TriggerKind) -> String {
    match kind {
        TriggerKind::Consumer => "consumer".into(),
        TriggerKind::Dependency => "dependency".into(),
    }
}

fn receipt_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("receipt-{generation}.json"))
}

fn load_receipt(dir: &Path, generation: u64) -> Option<Receipt> {
    let bytes = std::fs::read(receipt_path(dir, generation)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn store_receipt(dir: &Path, receipt: &Receipt) -> Result<()> {
    let bytes = serde_json::to_string_pretty(receipt)?;
    let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
    use std::io::Write;
    temporary.write_all(bytes.as_bytes())?;
    temporary
        .persist(receipt_path(dir, receipt.generation))
        .map_err(|error| failure(format!("Coalescing receipt write refused: {error}")))?;
    Ok(())
}

// No receipt pruning: receipts are durable evidence until a separately
// authorized retention action. Only `work/` scratch and TempDir cleanups
// remove program-owned temporary files.

/// Pure coverage decision: exact graph inclusion plus exact identity.
/// Identity (tool, runtime, config) is checked FIRST: any mismatch refuses
/// before ancestry. Source identity (canonical repository plus branch) is
/// checked before ancestry as well: package name alone never attaches.
/// Never cmsg-SHA-alone: outcome, identity, source, and graph all participate.
fn covered(receipt: &Receipt, trigger: &Trigger, current: &BuildIdentity) -> bool {
    if !identity_matches(receipt, current) {
        return false;
    }
    if !source_matches(receipt, trigger) {
        return false;
    }
    match trigger.kind {
        TriggerKind::Consumer => receipt.consumer_commit == trigger.sha,
        TriggerKind::Dependency => trigger
            .name
            .as_ref()
            .and_then(|name| receipt.deps.get(name))
            .is_some_and(|selected| selected.sha == trigger.sha),
    }
}

fn identity_matches(receipt: &Receipt, current: &BuildIdentity) -> bool {
    // No placeholders: production always binds tool plus a real runtime
    // fingerprint plus live-head config. Old receipts with empty fields only
    // equal an equally empty current, which production never produces, so
    // they fail closed and rebuild.
    receipt.tool_revision == current.tool_revision
        && receipt.runtime_identity == current.runtime_identity
        && receipt.config_identity == current.config_identity
}

/// Canonical source identity: the trigger's repository and branch must name
/// the recorded graph's source before any commit comparison. Consumer events
/// must name the consumer; dependency events must name the selected
/// dependency fetch source on the actually selected source branch recorded
/// in the receipt (never the consumer branch as a proxy).
fn source_matches(receipt: &Receipt, trigger: &Trigger) -> bool {
    let Ok(trigger_canonical) = crate::cache::canonical_repository(&trigger.repo) else {
        return false;
    };
    match trigger.kind {
        TriggerKind::Consumer => {
            let Ok(consumer_canonical) = crate::cache::canonical_repository(&receipt.consumer_url)
            else {
                return false;
            };
            trigger_canonical == consumer_canonical && trigger.branch == receipt.consumer_branch
        }
        TriggerKind::Dependency => {
            let Some(name) = trigger.name.as_ref() else {
                return false;
            };
            let Some(selected) = receipt.deps.get(name) else {
                return false;
            };
            // Per-source branch recorded at build time: the event branch must
            // equal the actually selected source branch. Empty recorded
            // branches (pre-correction receipts) never attach. Wrong-branch
            // events never attach even when the commit exists elsewhere.
            if selected.branch.is_empty() || trigger.branch != selected.branch {
                return false;
            }
            let Ok(selected_canonical) = crate::cache::canonical_repository(&selected.fetch)
                .or_else(|_| crate::cache::canonical_repository(&selected.url))
            else {
                return false;
            };
            trigger_canonical == selected_canonical
        }
    }
}

fn emit(outcome: &PushOutcome, trigger: &Trigger, generation: u64) {
    let (event, built) = match outcome {
        PushOutcome::Built { .. } => ("push-built", true),
        PushOutcome::Attached { .. } => ("push-attached", false),
    };
    crate::event(
        serde_json::json!({"event": event, "job_built": built, "generation": generation,
        "trigger_repo": trigger.repo, "trigger_branch": trigger.branch,
        "trigger_commit": trigger.sha, "trigger_name": trigger.name}),
    );
}

/// Record this event and observe a bounded quiet period. Persists the full
/// trigger provenance with its generation; returns the admitted (newest
/// seen) generation frozen for this caller. Every wait is capped by both the
/// burst bounds and `overall`: the overall job deadline is never reset.
fn notify_and_settle(
    dir: &Path,
    trigger: &Trigger,
    quiet_secs: u64,
    max_secs: u64,
    overall: Instant,
) -> Result<u64> {
    let admitted = {
        let _lock = lock_file_until(&dir.join("notify.lock"), overall)?;
        if Instant::now() >= overall {
            return Err(failure("Push run deadline exceeded before settle"));
        }
        let mut state = load_state(dir)?;
        state.generation = state.generation.saturating_add(1);
        let admitted = state.generation;
        state.pending.push(PendingEntry {
            generation: admitted,
            trigger: trigger_ser(trigger),
        });
        store_state(dir, &state)?;
        admitted
    };
    let mut admitted = admitted;
    let start = Instant::now();
    let mut quiet = 0u64;
    while quiet < quiet_secs && start.elapsed() < Duration::from_secs(max_secs) {
        if Instant::now() >= overall || crate::INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(failure(
                "Push run deadline exceeded while settling the burst",
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
        // Re-lock briefly so recorders are never blocked behind a quiet wait.
        let remaining = remaining_wait(NOTIFY_WAIT_SECS, overall)?;
        let _lock = match try_lock_file(&dir.join("notify.lock"), remaining, overall)? {
            Some(held) => held,
            None => continue,
        };
        let state = load_state(dir)?;
        if state.generation > admitted {
            admitted = state.generation;
            quiet = 0;
        } else {
            quiet += 1;
        }
    }
    if Instant::now() >= overall {
        return Err(failure(
            "Push run deadline exceeded while settling the burst",
        ));
    }
    Ok(admitted)
}

fn remaining_wait(wait_secs: u64, overall: Instant) -> Result<u64> {
    let now = Instant::now();
    if now >= overall {
        return Err(failure("Push run deadline exceeded"));
    }
    let remaining = overall.duration_since(now).as_secs();
    Ok(remaining.min(wait_secs))
}

/// Per-caller attach examination cache. Receipts are immutable per
/// generation (one builder records exactly one receipt per admitted
/// generation), so an examined generation is never worth re-probing:
/// waiters sleep on cheap local state instead of repeating live
/// ls-remote, toolchain probes, and ancestry fetches every poll.
#[derive(Debug, Default)]
struct AttachCache {
    examined: Option<u64>,
}

/// Attach examination with poll-storm protection. The local receipt
/// generation is cheap; live policy probes and ancestry run at most once
/// per generation. Errors (deadline, IO, failure propagation) stay
/// terminal and are never cached. A receipt that lands mid-probe is still
/// evaluated once via a single bounded re-examination.
fn try_attach_cached(
    dir: &Path,
    spec: &PushSpec,
    trigger: &Trigger,
    policy: &dyn CoalescePolicy,
    deadline: Instant,
    cache: &mut AttachCache,
) -> Result<Option<PushOutcome>> {
    let current = load_state(dir)?.receipt_generation;
    if cache.examined == Some(current) {
        return Ok(None);
    }
    match try_attach(dir, spec, trigger, policy, deadline)? {
        Some(outcome) => Ok(Some(outcome)),
        None => {
            let fresh = load_state(dir)?.receipt_generation;
            cache.examined = Some(fresh);
            if fresh != current {
                try_attach(dir, spec, trigger, policy, deadline)
            } else {
                Ok(None)
            }
        }
    }
}

/// Attach to the newest completed receipt when it covers this trigger,
/// exactly or by proven ancestry on the same branch and source. Identity
/// (tool, runtime, live-head config) and source (canonical repository plus
/// selected branch) are required BEFORE ancestry on every path: a mismatch
/// never falls through to a fetch. Before ANY attach, consumer or
/// dependency, the live consumer head must equal the receipt's consumer
/// commit: a new manifest or source invalidates prior proof, including
/// consumer ancestor events and dependency exact-sha events. Missing,
/// corrupt, or non-covering receipts never attach; failed outcomes
/// propagate only to identical admitted triggers with full source proof
/// (never a success, never a blind rerun). Any probe failure means
/// "unproven", which builds.
fn try_attach(
    dir: &Path,
    spec: &PushSpec,
    trigger: &Trigger,
    policy: &dyn CoalescePolicy,
    deadline: Instant,
) -> Result<Option<PushOutcome>> {
    if Instant::now() >= deadline {
        return Err(failure("Push run deadline exceeded before attach"));
    }
    let state = load_state(dir)?;
    if state.receipt_generation == 0 {
        return Ok(None);
    }
    let Some(receipt) = load_receipt(dir, state.receipt_generation) else {
        return Ok(None);
    };
    if spec.job != receipt.job {
        return Ok(None);
    }
    let proof = match policy.current_proof(spec, deadline) {
        Ok(proof) => proof,
        Err(_) => return Ok(None),
    };
    // Live head gates everything: stale receipts never attach, whatever the
    // trigger kind or ancestry. This is the manifest/source invalidation:
    // the head pins the whole consumer configuration.
    if proof.live_head.trim() != receipt.consumer_commit {
        return Ok(None);
    }
    // Failed receipts propagate only to identical admitted triggers under
    // the same identity and full source proof: same kind, repository,
    // branch, commit, and package name, with the live head above. Anything
    // else needs graph coverage below, never success.
    if receipt.outcome != "pass" {
        let identical = receipt
            .admitted_triggers
            .iter()
            .chain(std::iter::once(&receipt.trigger))
            .any(|recorded| trigger_ser_matches(recorded, trigger));
        if identical {
            if !identity_matches(&receipt, &proof.identity) {
                return Ok(None);
            }
            // Source provenance for the failure: proven-graph failures
            // require the recorded source entry, but a failure recorded
            // before graph resolution carries no deps at all. For those
            // early failures the exact admitted-tuple match above already
            // proves provenance (same canonical repository, branch,
            // commit, and package name), so dependency triggers propagate
            // the recorded failure instead of repeating the failed
            // resolver. This is failure evidence under full identity,
            // never a successful graph claim: coverage below still
            // requires the recorded graph, and settle refuses non-pass.
            let source_ok = match trigger.kind {
                TriggerKind::Consumer => failure_source_matches(&receipt, trigger),
                TriggerKind::Dependency => match trigger.name.as_ref() {
                    None => false,
                    Some(name) => match receipt.deps.get(name) {
                        Some(_) => failure_source_matches(&receipt, trigger),
                        // Failed before graph resolution: no deps recorded.
                        None if receipt.deps.is_empty() => true,
                        None => return Ok(None),
                    },
                },
            };
            if !source_ok {
                return Ok(None);
            }
            return Err(failure(format!(
                "Trigger {} already failed in run {}; not rerun",
                trigger.sha, state.receipt_generation
            )));
        }
        // A non-identical trigger may still share a failed graph (gate
        // failure with proven deps): fall through to coverage, which settles
        // to Err propagation, never success. No blind rerun here.
    }
    if covered(&receipt, trigger, &proof.identity) {
        return settle_receipt(state.receipt_generation, &receipt, trigger);
    }
    // Identity and source already matched (covered checks both); a mismatch
    // above returns None before any fetch. Delayed notification: the trigger
    // may already be superseded inside this receipt's selected graph. Prove
    // it with one bounded ancestry check on the trigger's own branch.
    if !identity_matches(&receipt, &proof.identity) || !source_matches(&receipt, trigger) {
        return Ok(None);
    }
    let (url, branch, cover) = match trigger.kind {
        TriggerKind::Consumer => (
            spec.consumer_url.clone(),
            spec.consumer_branch.clone(),
            receipt.consumer_commit.clone(),
        ),
        TriggerKind::Dependency => {
            let Some(name) = trigger.name.as_ref() else {
                return Ok(None);
            };
            let Some(selected) = receipt.deps.get(name) else {
                return Ok(None);
            };
            (
                selected.fetch.clone(),
                selected.branch.clone(),
                selected.sha.clone(),
            )
        }
    };
    if Instant::now() >= deadline {
        return Err(failure("Push run deadline exceeded before ancestry"));
    }
    if policy.is_ancestor(&url, &branch, &trigger.sha, &cover, deadline) {
        return settle_receipt(state.receipt_generation, &receipt, trigger);
    }
    Ok(None)
}

fn trigger_ser_matches(recorded: &TriggerSer, trigger: &Trigger) -> bool {
    if recorded.kind != trigger_kind_name(&trigger.kind)
        || recorded.branch != trigger.branch
        || recorded.sha != trigger.sha
        || recorded.name != trigger.name
    {
        return false;
    }
    let Ok(recorded_canonical) = crate::cache::canonical_repository(&recorded.repo) else {
        return false;
    };
    let Ok(trigger_canonical) = crate::cache::canonical_repository(&trigger.repo) else {
        return false;
    };
    recorded_canonical == trigger_canonical
}

fn failure_source_matches(receipt: &Receipt, trigger: &Trigger) -> bool {
    source_matches(receipt, trigger)
}

fn settle_receipt(
    generation: u64,
    receipt: &Receipt,
    trigger: &Trigger,
) -> Result<Option<PushOutcome>> {
    if receipt.outcome != "pass" {
        return Err(failure(format!(
            "Coalesced run {generation} failed; trigger {} did not rerun it",
            trigger.sha
        )));
    }
    Ok(Some(PushOutcome::Attached { generation }))
}

/// Environment keys fingerprinted into the runtime identity: toolchain and
/// compile settings only. Scheduler event IDs, repository names, and cache
/// paths are never included.
const RUNTIME_WHITELIST: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTUP_TOOLCHAIN",
    "CC",
    "CXX",
    "AR",
    "RANLIB",
    "LD",
    "LDFLAGS",
    "CFLAGS",
    "CXXFLAGS",
];

/// Actual toolchain fingerprint for the effective job environment, captured
/// through the existing bounded `Runner` before the caller deadline: verbose
/// rustc plus cargo plus clippy versions, trimmed, plus every set
/// whitelisted compile variable. Preserves the caller's real
/// `RUSTUP_HOME`/`CARGO_HOME` (never `env_clear` without them). Any missing
/// tool or probe error fails closed with `Err`: no `unavailable` sentinel
/// that could attach as reusable success.
///
/// The graph worker calls this before and after the gates with the merged
/// gate environment and requires equality via `require_runtime_unchanged`;
/// see `BuildOutput` and COALESCE-FIX-READY.md for the exact integration.
pub fn capture_runtime(effective: &crate::Environment, deadline: Instant) -> Result<String> {
    if Instant::now() >= deadline {
        return Err(failure("Push run deadline exceeded before runtime probe"));
    }
    let mut environment = effective.clone();
    environment.insert(
        std::ffi::OsString::from("PATH"),
        effective
            .get(&std::ffi::OsString::from("PATH"))
            .cloned()
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default(),
    );
    for (key, value) in [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("LC_ALL", "C"),
    ] {
        environment.insert(
            std::ffi::OsString::from(key),
            std::ffi::OsString::from(value),
        );
    }
    let scratch = tempfile::TempDir::new()?;
    let runner = crate::Runner::until(scratch.path().into(), environment.clone(), deadline)?;
    let probe = |argv: &[&str]| {
        runner.run(
            &argv.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            true,
        )
    };
    // Verbose rustc pins version, host, and commit; failures fail closed.
    let rustc = probe(&["rustc", "-vV"])?;
    let cargo = probe(&["cargo", "--version"])?;
    let clippy = probe(&["cargo", "clippy", "--version"])?;
    let mut fingerprint = format!("{rustc}\n{cargo}\n{clippy}");
    let mut whitelisted: BTreeMap<String, String> = BTreeMap::new();
    for key in RUNTIME_WHITELIST {
        let lookup = std::ffi::OsString::from(*key);
        if let Some(value) = environment.get(&lookup) {
            if !value.is_empty() {
                whitelisted.insert((*key).to_owned(), value.to_string_lossy().into_owned());
            }
        }
    }
    for (key, value) in &whitelisted {
        fingerprint.push_str(&format!("\n{key}={value}"));
    }
    if rustc.is_empty() || cargo.is_empty() || clippy.is_empty() {
        return Err(failure("Toolchain probe returned empty output"));
    }
    Ok(fingerprint)
}

/// Require the gate-merged runtime to be unchanged across the gates.
/// The graph worker calls `capture_runtime` before and after `run_gates`
/// with the merged gate environment; any drift fails the receipt instead of
/// recording a mixed-toolchain pass.
pub fn require_runtime_unchanged(before: &str, after: &str) -> Result<()> {
    if before.is_empty() || after.is_empty() {
        return Err(failure(
            "Refusing to record a build with unmeasured toolchain identity",
        ));
    }
    if before != after {
        return Err(failure(
            "Toolchain changed during the gates; refusing a mixed-toolchain receipt",
        ));
    }
    Ok(())
}

/// Parse one `git ls-remote <url> <branch>` response, requiring the exact
/// ref for `branch`. Never the first arbitrary line: zero or multiple
/// matching refs, or a sha that is not a full commit, is `Err` (unproven).
fn parse_ls_remote_exact(output: &str, branch: &str) -> Result<String> {
    let mut matches = Vec::new();
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let (Some(sha), Some(git_ref)) = (parts.next(), parts.next()) else {
            continue;
        };
        if git_ref == format!("refs/heads/{branch}") || git_ref == branch {
            matches.push(sha.trim().to_owned());
        }
    }
    if matches.len() != 1 || !valid_sha(&matches[0]) {
        return Err(failure("Live consumer head response is not exact"));
    }
    Ok(matches.remove(0))
}

/// Newest commit for one branch without a checkout: one bounded ls-remote
/// with exact ref validation, output trimmed. Any failure is Err
/// (unproven), which builds.
fn fetch_live_head(url: &str, branch: &str, deadline: Instant) -> Result<String> {
    if Instant::now() >= deadline {
        return Err(failure("Push run deadline exceeded before live head"));
    }
    let scratch = tempfile::TempDir::new()?;
    let mut environment: crate::Environment = BTreeMap::new();
    environment.insert(
        std::ffi::OsString::from("PATH"),
        std::env::var_os("PATH").unwrap_or_default(),
    );
    for (key, value) in [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("LC_ALL", "C"),
    ] {
        environment.insert(
            std::ffi::OsString::from(key),
            std::ffi::OsString::from(value),
        );
    }
    let runner = crate::Runner::until(scratch.path().into(), environment, deadline)?;
    let output = runner.run(
        &[
            "git".into(),
            "-c".into(),
            "core.hooksPath=/dev/null".into(),
            "-c".into(),
            "credential.helper=".into(),
            "-c".into(),
            "protocol.file.allow=never".into(),
            "ls-remote".into(),
            url.into(),
            branch.into(),
        ],
        true,
    )?;
    parse_ls_remote_exact(&output, branch)
}

/// True when `old` is an ancestor-or-equal of `new` on the given repository,
/// bounded by the caller's overall deadline (never reset to a fresh full
/// window). Fetches the branch into a fresh bare scratch repo, then answers
/// from local objects only. Unknown is false: unproven builds.
fn proven_ancestor_bounded(
    url: &str,
    branch: &str,
    old: &str,
    new: &str,
    overall: Instant,
) -> bool {
    if old == new {
        return true;
    }
    if Instant::now() >= overall {
        return false;
    }
    let remaining = overall
        .duration_since(Instant::now())
        .as_secs()
        .min(ANCESTRY_SECS);
    let deadline = match Instant::now().checked_add(Duration::from_secs(remaining)) {
        Some(deadline) => deadline,
        None => return false,
    };
    let scratch = match tempfile::TempDir::new() {
        Ok(scratch) => scratch,
        Err(_) => return false,
    };
    let mut environment: crate::Environment = BTreeMap::new();
    environment.insert(
        std::ffi::OsString::from("PATH"),
        std::env::var_os("PATH").unwrap_or_default(),
    );
    for (key, value) in [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("LC_ALL", "C"),
    ] {
        environment.insert(
            std::ffi::OsString::from(key),
            std::ffi::OsString::from(value),
        );
    }
    let runner = match crate::Runner::until(scratch.path().into(), environment, deadline) {
        Ok(runner) => runner,
        Err(_) => return false,
    };
    let git = |args: &[&str]| {
        let mut argv = vec![
            "git".into(),
            "-c".into(),
            "core.hooksPath=/dev/null".into(),
            "-c".into(),
            "credential.helper=".into(),
            "-c".into(),
            "protocol.file.allow=never".into(),
        ];
        argv.extend(args.iter().map(|s| (*s).into()));
        runner.run(&argv, true)
    };
    if git(&["init", "--quiet", "--bare", "."]).is_err() {
        return false;
    }
    if git(&["fetch", "--quiet", url, branch]).is_err() {
        crate::event(serde_json::json!({"event": "push-ancestry-unreadable", "branch": branch}));
        return false;
    }
    let scratch_path = scratch.path().to_owned();
    is_ancestor_in(&scratch_path, old, new)
}

/// Ancestor-or-equal from local objects only: empty rev-list count means
/// `old` is reachable from `new`. Missing objects or git errors are false
/// (unproven), which builds instead of attaching.
fn is_ancestor_in(dir: &Path, old: &str, new: &str) -> bool {
    if old == new {
        return true;
    }
    if !valid_sha(old) || !valid_sha(new) {
        return false;
    }
    let output = std::process::Command::new("git")
        .args(["rev-list", "--count", &format!("{new}..{old}")])
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8(output.stdout)
            .map(|text| text.trim() == "0")
            .unwrap_or(false),
        _ => false,
    }
}

/// Drive one push event to shared completion. `build` runs only while this
/// caller holds the execution lock; it returns the proved output: the graph
/// body plus the gate-merged runtime captured around the gates and the
/// per-source selected branches. `policy` supplies the live proof and bounded
/// ancestry. Tests inject fakes; production resolves, builds, and records.
/// The admitted generation is frozen before resolution: `build` receives the
/// frozen generation, `record_receipt` stores exactly it, and later arrivals
/// stay pending for one newer run.
pub fn run_push_with<F, P>(
    spec: &PushSpec,
    build: F,
    policy: &P,
    quiet_secs: u64,
    max_secs: u64,
) -> Result<PushOutcome>
where
    F: Fn(u64) -> Result<BuildOutput>,
    P: CoalescePolicy,
{
    validate_spec(spec)?;
    let overall = Instant::now()
        .checked_add(Duration::from_secs(spec.timeout_secs))
        .ok_or_else(|| failure("Push run deadline is out of range"))?;
    let dir = namespace_dir(
        &spec.cache_root,
        &spec.consumer_url,
        &spec.consumer_branch,
        &spec.job,
    )?;
    std::fs::create_dir_all(&dir)?;
    let admitted = notify_and_settle(&dir, &spec.trigger, quiet_secs, max_secs, overall)?;
    // Per-caller attach examination: receipts are immutable per
    // generation, so waiters poll cheap local state and run expensive
    // live probes plus ancestry at most once per receipt generation.
    // Unknown proof always means "build", never "attach".
    let mut examined = AttachCache::default();
    // Fast path: a completed receipt may already cover this trigger
    // (idempotent re-delivery or a build that finished during settle).
    if let Some(outcome) =
        try_attach_cached(&dir, spec, &spec.trigger, policy, overall, &mut examined)?
    {
        emit(&outcome, &spec.trigger, admitted);
        return Ok(outcome);
    }
    loop {
        if Instant::now() >= overall || crate::INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(failure("Push run timed out waiting for the shared build"));
        }
        match try_lock_file(&dir.join("exec.lock"), 0, overall)? {
            Some(_held) => {
                // Freeze the admitted set under the notification lock before
                // any resolution: events arriving during `build` stay pending.
                // The lock wait is capped by the caller's overall deadline.
                let (frozen, admitted_triggers) = {
                    let _lock = lock_file_until(&dir.join("notify.lock"), overall)?;
                    let state = load_state(&dir)?;
                    let frozen = state.generation.max(admitted);
                    let admitted_triggers: Vec<TriggerSer> = state
                        .pending
                        .iter()
                        .filter(|entry| entry.generation <= frozen)
                        .map(|entry| entry.trigger.clone())
                        .collect();
                    (frozen, admitted_triggers)
                };
                // A receipt completed while waiting covers us without building.
                // An unchanged receipt reuses the earlier examination, so an
                // unproven receipt means a safe build here, never a pass.
                if let Some(outcome) =
                    try_attach_cached(&dir, spec, &spec.trigger, policy, overall, &mut examined)?
                {
                    emit(&outcome, &spec.trigger, frozen);
                    return Ok(outcome);
                }
                let output = build(frozen)?;
                let outcome =
                    record_receipt(&dir, spec, &output, frozen, admitted_triggers, overall)?;
                emit(&outcome, &spec.trigger, frozen);
                if output.body.outcome != "pass" {
                    return Err(failure(format!(
                        "Admitted build {frozen} failed; failure recorded, not retried"
                    )));
                }
                return Ok(outcome);
            }
            None => {
                // While the exec lock is busy, poll only cheap local state:
                // the cached examination skips repeated live git probes,
                // runtime probes, and ancestry fetches until a new receipt
                // generation lands. The poll interval itself is unchanged.
                std::thread::sleep(Duration::from_millis(ATTACH_POLL_MILLIS));
                if let Some(outcome) =
                    try_attach_cached(&dir, spec, &spec.trigger, policy, overall, &mut examined)?
                {
                    emit(&outcome, &spec.trigger, admitted);
                    return Ok(outcome);
                }
            }
        }
    }
}

fn record_receipt(
    dir: &Path,
    spec: &PushSpec,
    output: &BuildOutput,
    frozen: u64,
    admitted_triggers: Vec<TriggerSer>,
    deadline: Instant,
) -> Result<PushOutcome> {
    let body = &output.body;
    if output.runtime_identity.is_empty() {
        return Err(failure(
            "Refusing to record a build with unmeasured toolchain identity",
        ));
    }
    let mut deps = BTreeMap::new();
    for (name, selected) in &body.deps {
        deps.insert(
            name.clone(),
            DepSel {
                url: selected.url.clone(),
                fetch: selected.fetch.clone(),
                sha: selected.sha.clone(),
                branch: output.dep_branches.get(name).cloned().unwrap_or_default(),
            },
        );
    }
    // Config is derived from the ACTUAL built consumer commit plus the
    // selected job, never a pre-build head that might race: the commit pins
    // the whole consumer configuration. A new manifest or source is a new
    // commit, which invalidates this receipt via the live-head gate.
    let config_identity = config_for(&body.consumer_commit, &spec.job);
    let mut admitted = admitted_triggers;
    let own = trigger_ser(&body.trigger);
    if !admitted.iter().any(|recorded| {
        recorded.kind == own.kind
            && recorded.repo == own.repo
            && recorded.branch == own.branch
            && recorded.sha == own.sha
            && recorded.name == own.name
    }) {
        admitted.push(own.clone());
    }
    let receipt = Receipt {
        generation: frozen,
        admitted: frozen,
        outcome: body.outcome.clone(),
        error: body.error.clone(),
        consumer_url: spec.consumer_url.clone(),
        consumer_branch: spec.consumer_branch.clone(),
        consumer_commit: body.consumer_commit.clone(),
        deps,
        tool_revision: crate::SOURCE_REVISION.into(),
        runtime_identity: output.runtime_identity.clone(),
        config_identity,
        manifest_sha: body.manifest_sha.clone(),
        job: spec.job.clone(),
        checks: body.checks.clone(),
        rematerialized: body.rematerialized.clone(),
        trigger: own,
        admitted_triggers: admitted,
    };
    store_receipt(dir, &receipt)?;
    {
        let _lock = lock_file_until(&dir.join("notify.lock"), deadline)?;
        let mut state = load_state(dir)?;
        state.receipt_generation = frozen;
        state.pending.retain(|entry| entry.generation > frozen);
        store_state(dir, &state)?;
    }
    // No receipt pruning: durable evidence retained.
    Ok(PushOutcome::Built { generation: frozen })
}

/// Production entry: shared coalescing around one real admitted build.
/// Resolves timeout from the operator budget like execute-job.
#[allow(clippy::too_many_arguments)]
pub fn push_run(
    consumer_url: String,
    consumer_branch: String,
    job: String,
    trigger_kind: String,
    trigger_sha: String,
    trigger_name: String,
    trigger_branch: String,
    trigger_repo: String,
    cache_root: PathBuf,
    timeout_secs: u64,
) -> Result<PushOutcome> {
    let kind = match trigger_kind.as_str() {
        "self" => TriggerKind::Consumer,
        "dep" => TriggerKind::Dependency,
        _ => return Err(failure("Trigger kind must be self or dep")),
    };
    let name = match kind {
        TriggerKind::Consumer => None,
        TriggerKind::Dependency => {
            if trigger_name.is_empty() {
                return Err(failure("Dependency triggers require a package name"));
            }
            Some(trigger_name)
        }
    };
    let trigger_repo = if trigger_repo.is_empty() {
        match kind {
            TriggerKind::Consumer => consumer_url.clone(),
            TriggerKind::Dependency => {
                return Err(failure("Dependency triggers require a trigger repository"));
            }
        }
    } else {
        trigger_repo
    };
    let spec = PushSpec {
        consumer_url,
        consumer_branch,
        job,
        trigger: Trigger {
            kind,
            repo: trigger_repo,
            branch: trigger_branch,
            sha: trigger_sha,
            name,
        },
        cache_root,
        timeout_secs,
    };
    let overall = Instant::now()
        .checked_add(Duration::from_secs(timeout_secs))
        .ok_or_else(|| failure("Push run deadline is out of range"))?;
    let policy = ProductionPolicy;
    run_push_with(
        &spec,
        // Production glue: the graph build returns the gate-merged runtime
        // plus per-source branches directly in `BuildOutput`.
        |_| production_build(&spec, overall),
        &policy,
        QUIET_SECS,
        MAX_SECS,
    )
}

/// One admitted build: stage newest, verify, refresh once, run gates,
/// record the tested graph. Any failure records a fail receipt first so
/// waiting triggers propagate it instead of rebuilding blindly.
///
/// Returns the coalescer `BuildOutput`: the proved body plus the
/// gate-merged runtime and per-source branches. Early (pre-gate) errors
/// still record a truthful failure receipt with a freshly measured runtime
/// under the planned environment, never a pass and never an empty
/// fingerprint; a failed probe itself fails closed with `Err`.
fn production_build(spec: &PushSpec, overall: Instant) -> Result<BuildOutput> {
    let dir = namespace_dir(
        &spec.cache_root,
        &spec.consumer_url,
        &spec.consumer_branch,
        &spec.job,
    )?;
    // Admission gates resource pressure before anything expensive, exactly
    // like execute-job. Attach-only callers never reach here.
    let mut environment: crate::Environment = std::env::vars_os().collect();
    crate::admission::admit(&mut environment)?;
    let budget = crate::budget(&environment)?;
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(budget.timeout))
        .ok_or_else(|| failure("Push build deadline is out of range"))?
        .min(overall);
    let planned = stage_consumer(&dir, spec, deadline)?;
    // Tool pin: the staged manifest must name this exact binary revision.
    // A re-pin lands with the manifest that needs it, in one commit.
    if planned.tool_revision != crate::SOURCE_REVISION {
        return Err(failure(
            "Consumer manifest tool revision does not match this binary; re-pin required",
        ));
    }
    match build_inner(&planned, &spec.trigger) {
        Ok(output) => Ok(output),
        Err(error) => {
            // No graph was proven: only an identical retrigger may attach
            // to this failure, never graph coverage. The failure receipt
            // still carries a measured runtime so identity-gated failure
            // propagation works; an unmeasurable toolchain fails closed.
            let runtime_identity = capture_runtime(&planned.environment, planned.deadline)?;
            if runtime_identity.is_empty() {
                return Err(failure(
                    "Refusing to record a build with unmeasured toolchain identity",
                ));
            }
            Ok(BuildOutput {
                body: ReceiptBody {
                    outcome: "fail".into(),
                    error: error.to_string(),
                    consumer_commit: planned.head.clone(),
                    deps: BTreeMap::new(),
                    rematerialized: Vec::new(),
                    manifest_sha: planned.manifest_sha.clone(),
                    checks: planned.checks.clone(),
                    trigger: spec.trigger.clone(),
                },
                runtime_identity,
                dep_branches: BTreeMap::new(),
            })
        }
    }
}

/// The admitted build itself: resolve once with the complete active
/// transitive first-party v01 graph (Cargo.lock, not root tables), prefetch
/// for the frozen gates, run declared checks truly --offline --locked with
/// the same full lock bytes for every gate, and record the tested graph.
/// Any lock mutation during gates fails even if the gate command exits 0.
/// Gate failures still record (with the tested graph) so waiters propagate
/// instead of rebuilding blindly.
///
/// Returns the proved body plus the gate-merged runtime fingerprint
/// (captured with the exact gate execution environment before and after the
/// gates, drift refused) and the per-source selected branches, so receipts
/// bind what the gates actually observed.
fn build_inner(planned: &PlannedBuild, trigger: &Trigger) -> Result<BuildOutput> {
    // Never silently ignore an arbitrary job command: push jobs must carry
    // the explicit generic sentinel (validated in jobs::plan, re-checked
    // here). Anything else fails closed.
    if !is_self_checks_command(&planned.command) {
        return Err(failure(
            "Push execution requires explicit ccid:run-declared-checks command",
        ));
    }
    let selected = select_active_graph(&planned.workdir, &planned.refresh)?;
    if selected.is_empty() {
        return Err(failure(
            "Refresh scope selected no dependencies; refusing an unproven graph",
        ));
    }
    let dep_branches = dep_branches_from(&selected);
    // Gate-merged runtime: the exact gate execution environment, captured
    // before the gates run. The target lock is held by `run_gates` itself;
    // capture only needs the same resolved environment value.
    let gate_env = gate_execution_env(planned, &gate_target_dir(planned)?)?;
    let runtime_before = capture_runtime(&gate_env, planned.deadline)?;
    refresh_graph(planned, &selected)?;
    // Resolve-once point: full lock bytes + full selected graph retained as
    // durable evidence before any gate runs (namespace dir survives workdir
    // resets and later cleanups).
    let resolved_bytes = read_full_lock(&planned.workdir)?;
    let resolved_hash = format!("{:x}", sha2::Sha256::digest(&resolved_bytes));
    retain_resolved_evidence(planned, &selected, &resolved_bytes, &resolved_hash)?;
    let pre = lock_graph(&planned.workdir)?;
    let gates = run_gates(planned, &resolved_bytes);
    // Gate-merged runtime, after: any toolchain drift across the gates fails
    // closed with no mixed-toolchain receipt (see below). Evidence is
    // retained first so the drift is diagnosable from the namespace dir.
    let runtime_after = capture_runtime(&gate_env, planned.deadline)?;
    let post = lock_graph(&planned.workdir)?;
    // Frozen gates: full lock bytes must be exactly unchanged. Any mutation
    // fails even if every gate command exited 0. Compare full bytes (not
    // merely selected root names) plus all selected source entries.
    let post_bytes = read_full_lock(&planned.workdir)?;
    let frozen = gates.and_then(|_| {
        if post_bytes != resolved_bytes {
            return Err(failure(
                "Frozen lock mutated during gates; refusing an unproven graph",
            ));
        }
        // All selected source entries must still match (sha + identity).
        for sel in &selected {
            match (pre.get(&sel.package), post.get(&sel.package)) {
                (Some(before), Some(after))
                    if before.sha == after.sha
                        && before.url == after.url
                        && before.fetch == after.fetch => {}
                _ => {
                    return Err(failure(format!(
                        "Frozen graph entry changed during gates: {}",
                        sel.package
                    )));
                }
            }
        }
        Ok(())
    });
    // Retain before/after evidence even on failure (namespace dir).
    retain_gate_evidence(planned, &resolved_bytes, &post_bytes)?;
    // A drifted toolchain means neither fingerprint binds the whole run:
    // refuse a mixed-toolchain receipt entirely (fail closed, no pass, no
    // failure receipt with an ambiguous identity). The caller records an
    // early-error failure with a freshly measured runtime instead.
    require_runtime_unchanged(&runtime_before, &runtime_after)?;
    let mut deps = BTreeMap::new();
    for (name, selected) in &post {
        deps.insert(
            name.clone(),
            DepSelBody {
                url: selected.url.clone(),
                fetch: selected.fetch.clone(),
                sha: selected.sha.clone(),
            },
        );
    }
    // rematerialized is gate drift evidence: with frozen enforcement it is
    // empty on pass; on fail it names the selected entries that moved (for
    // diagnosis, never as a passing graph).
    let mut rematerialized = Vec::new();
    for sel in &selected {
        match (pre.get(&sel.package), post.get(&sel.package)) {
            (Some(before), Some(after)) if before.sha != after.sha => {
                rematerialized.push(sel.package.clone());
            }
            (None, Some(_)) | (Some(_), None) => {
                rematerialized.push(sel.package.clone());
            }
            _ => {}
        }
    }
    let outcome = if frozen.is_ok() { "pass" } else { "fail" };
    let body = ReceiptBody {
        outcome: outcome.into(),
        error: frozen
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default(),
        consumer_commit: planned.head.clone(),
        deps,
        rematerialized,
        manifest_sha: planned.manifest_sha.clone(),
        checks: planned.checks.clone(),
        trigger: trigger.clone(),
    };
    Ok(BuildOutput {
        body,
        runtime_identity: runtime_before,
        dep_branches,
    })
}

/// Per-source selected branches from the resolved graph selection: package
/// name to the actually selected source branch (ref) from the GraphSel
/// parser. Never empty on success: `build_inner` refuses empty selections,
/// so dependency receipts always carry the branch `source_matches`
/// requires for attach.
fn dep_branches_from(selected: &[GraphSel]) -> BTreeMap<String, String> {
    selected
        .iter()
        .map(|entry| (entry.package.clone(), entry.git_ref.clone()))
        .collect()
}

struct PlannedBuild {
    workdir: PathBuf,
    ns_dir: PathBuf,
    head: String,
    manifest_sha: String,
    tool_revision: String,
    checks: Vec<String>,
    command: Vec<String>,
    refresh: Refresh,
    environment: crate::Environment,
    deadline: Instant,
}

/// Fetch newest consumer head into the shared workdir and verify it.
/// Persistent across admitted builds (warm git objects for later ancestry
/// reasoning); every build re-verifies from scratch.
fn stage_consumer(dir: &Path, spec: &PushSpec, deadline: Instant) -> Result<PlannedBuild> {
    let work = dir.join("work");
    std::fs::create_dir_all(&work)?;
    let mut environment: crate::Environment = BTreeMap::new();
    environment.insert(
        std::ffi::OsString::from("PATH"),
        std::env::var_os("PATH").unwrap_or_default(),
    );
    for (key, value) in [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_LFS_SKIP_SMUDGE", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("LC_ALL", "C"),
    ] {
        environment.insert(
            std::ffi::OsString::from(key),
            std::ffi::OsString::from(value),
        );
    }
    let runner = crate::Runner::until(work.clone(), environment.clone(), deadline)?;
    let git = |args: &[&str]| {
        let mut argv = vec![
            "git".into(),
            "-c".into(),
            "core.hooksPath=/dev/null".into(),
            "-c".into(),
            "credential.helper=".into(),
            "-c".into(),
            "fetch.fsckObjects=true".into(),
        ];
        argv.extend(args.iter().map(|s| (*s).into()));
        runner.run(&argv, true)
    };
    // Bind this directory to exactly one consumer; anything else fails
    // closed rather than mixing histories. The namespace directory itself is
    // program-owned (under CI_CACHE_ROOT/CARGO_HOME/ccid-coalesce); an
    // ownership marker pins it to one consumer+branch+job so we never reset
    // unknown data, even if the workdir remote was tampered with.
    match git(&["remote", "get-url", "origin"]) {
        Ok(existing) => {
            let existing = existing.trim().to_owned();
            if canonical_eq(&existing, &spec.consumer_url)? {
                // Reuse.
            } else {
                return Err(failure("Shared workdir belongs to another consumer"));
            }
        }
        Err(_) => {
            git(&["init", "--quiet", "."])?;
            git(&["remote", "add", "origin", spec.consumer_url.as_str()])?;
        }
    }
    let owner_path = dir.join("owner.json");
    let owner_value = serde_json::json!({
        "consumer_url": crate::cache::canonical_repository(&spec.consumer_url)
            .map_err(|_| failure("Consumer URL must be a plain forge URL"))?,
        "consumer_branch": spec.consumer_branch,
        "job": spec.job,
    });
    let owner_bytes = serde_json::to_string(&owner_value)?;
    match std::fs::read(&owner_path) {
        Ok(existing) => {
            if existing != owner_bytes.as_bytes() {
                return Err(failure("Shared workdir belongs to another consumer"));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::write(&owner_path, owner_bytes.as_bytes())?;
        }
        Err(error) => return Err(error.into()),
    }
    git(&[
        "fetch",
        "--quiet",
        "--prune",
        "origin",
        spec.consumer_branch.as_str(),
    ])?;
    let origin_ref = format!("origin/{}", spec.consumer_branch);
    let head = git(&["rev-parse", origin_ref.as_str()])?;
    // Reset existing history, or check out detached on a fresh directory;
    // both converge below and are verified byte-exact afterwards.
    if git(&["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok() {
        git(&["reset", "--quiet", "--hard", head.as_str()])?;
    } else {
        git(&["checkout", "--quiet", "--force", head.as_str()])?;
    }
    git(&["clean", "--quiet", "-fd"])?;
    if git(&["rev-parse", "HEAD"])? != head {
        return Err(failure("Staged consumer checkout does not match its head"));
    }
    // Canonical top-level plus a clean tree: no dirty source masquerading.
    // Only our own prior results directory is tolerated untracked.
    let toplevel = git(&["rev-parse", "--show-toplevel"])?;
    let canonical_work = work
        .canonicalize()
        .map_err(|_| failure("Staged workdir is unreadable"))?;
    let canonical_top = PathBuf::from(toplevel)
        .canonicalize()
        .map_err(|_| failure("Staged top level is unreadable"))?;
    if canonical_top != canonical_work {
        return Err(failure("Staged consumer is not the repository top level"));
    }
    let porcelain = git(&["status", "--porcelain=v1", "-uall"])?;
    for line in porcelain.lines() {
        let entry = line.get(3..).unwrap_or("");
        if line.starts_with("??") && (entry == ".ccid/" || entry.starts_with(".ccid/")) {
            continue;
        }
        return Err(failure(format!("Staged consumer is not clean: {line}")));
    }
    // Manifest: pin match, job opt-in, refresh scope, checks. Jobs::plan
    // validates selection and command shape.
    let manifest_bytes = std::fs::read(work.join(".ci/ccid.toml"))?;
    let manifest_sha = format!("{:x}", sha2::Sha256::digest(&manifest_bytes));
    let manifest: toml::Value = toml::from_str(std::str::from_utf8(&manifest_bytes)?)?;
    let tool_revision = manifest
        .get("render")
        .and_then(|r| r.get("tool_revision"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| failure("Consumer manifest names no tool revision"))?
        .to_owned();
    let planned = crate::jobs::plan(&work, Path::new(".ci/ccid.toml"), &spec.job, None)?;
    if !planned
        .push_branches
        .iter()
        .any(|b| b == &spec.consumer_branch)
    {
        return Err(failure(format!(
            "Job {} is not push-enabled for branch {}",
            spec.job, spec.consumer_branch
        )));
    }
    let refresh = planned.refresh.clone().ok_or_else(|| {
        failure("Push jobs require a manifest refresh scope for the newest graph")
    })?;
    let mut gates: crate::Environment = std::env::vars_os().collect();
    gates.retain(|key, _| !key.to_string_lossy().starts_with("CCID_STATUS_"));
    gates.insert("CI_COMMIT_SHA".into(), head.clone().into());
    gates.insert(
        "CI_COMMIT_BRANCH".into(),
        spec.consumer_branch.clone().into(),
    );
    gates.insert("CI_REPOSITORY_URL".into(), spec.consumer_url.clone().into());
    gates.insert("CCID_BIN".into(), std::env::current_exe()?.into_os_string());
    gates.insert("CHECKS".into(), planned.checks.join(",").into());
    gates.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    gates.insert("GIT_LFS_SKIP_SMUDGE".into(), "1".into());
    // Manifest-owned per-job environment (e.g. warm CARGO_TARGET_DIR,
    // build parallelism, fetch policy) applies to prepare/refresh/check
    // alike. Reserved identity/status keys can never be overridden.
    crate::jobs::apply_job_environment(&mut gates, &planned.environment);
    // Bound/validate the declared values through admission/budget exactly
    // like scheduler-provided ones, tightening the deadline if the declared
    // budget is smaller.
    crate::admission::admit(&mut gates)?;
    let declared_timeout = crate::budget(&gates)?.timeout;
    let tightened = Instant::now()
        .checked_add(Duration::from_secs(declared_timeout))
        .ok_or_else(|| failure("Push build deadline is out of range"))?;
    let deadline = deadline.min(tightened);
    Ok(PlannedBuild {
        workdir: work,
        ns_dir: dir.to_owned(),
        head,
        manifest_sha,
        tool_revision,
        checks: planned.checks.clone(),
        command: planned.command.clone(),
        refresh,
        environment: gates,
        deadline,
    })
}

fn canonical_eq(left: &str, right: &str) -> Result<bool> {
    let left = crate::cache::canonical_repository(left)
        .map_err(|_| failure("Origin URL is not a plain forge URL"))?;
    let right = crate::cache::canonical_repository(right)
        .map_err(|_| failure("Consumer URL is not a plain forge URL"))?;
    Ok(left == right)
}

/// Manifest-owned newest-head refresh, resolved once with explicit Cargo
/// package selections: every git dependency declaring the refresh branch
/// whose canonical source starts with an allowed prefix. No per-dep
/// ls-remote sweep; Cargo's own fetch is the single resolution round.
/// Legacy helper, superseded in production by `select_active_graph`;
/// retained test-only for the preserved root-table selection suite.
#[cfg(test)]
fn select_refresh(manifest: &toml::Value, refresh: &Refresh) -> Result<Vec<String>> {
    let mut selected = Vec::new();
    let mut tables = Vec::new();
    if let Some(deps) = manifest.get("dependencies") {
        tables.push(deps);
    }
    if let Some(targets) = manifest.get("target").and_then(|t| t.as_table()) {
        for target in targets.values() {
            if let Some(deps) = target.get("dependencies") {
                tables.push(deps);
            }
        }
    }
    if let Some(patches) = manifest.get("patch").and_then(|p| p.as_table()) {
        for source in patches.values() {
            tables.push(source);
        }
    }
    for table in tables {
        let Some(deps) = table.as_table() else {
            continue;
        };
        for (name, entry) in deps {
            let Some(detail) = entry.as_table() else {
                continue;
            };
            let (Some(url), Some(branch)) = (
                detail.get("git").and_then(|u| u.as_str()),
                detail.get("branch").and_then(|b| b.as_str()),
            ) else {
                continue;
            };
            if branch != refresh.branch {
                continue;
            }
            let canonical = crate::cache::canonical_repository(url)
                .map_err(|_| failure(format!("Dependency {name} source is not plain")))?;
            if !refresh
                .sources
                .iter()
                .any(|prefix| canonical == *prefix || canonical.starts_with(&format!("{prefix}/")))
            {
                continue;
            }
            if !selected.contains(&name.clone()) {
                selected.push(name.clone());
            }
        }
    }
    selected.sort();
    Ok(selected)
}

/// Read the full git selection out of a resolved lockfile, binding per
/// canonical source/ref and package/version/commit identity. Never
/// overwrites collisions silently: coexisting versions or sources with the
/// same package name fail closed (a richer receipt keyed by
/// source+package+version is tracked as follow-up in GRAPH-FIX-READY).
fn lock_graph(repo: &Path) -> Result<BTreeMap<String, DepSelBody>> {
    let bytes = std::fs::read(repo.join("Cargo.lock"))?;
    let value: toml::Value = toml::from_str(std::str::from_utf8(&bytes)?)?;
    let packages = value
        .get("package")
        .and_then(|p| p.as_array())
        .ok_or_else(|| failure("Resolved lock has no package table"))?;
    let mut graph: BTreeMap<String, DepSelBody> = BTreeMap::new();
    // Full identity seen per package name, to refuse silent collision loss.
    let mut seen: BTreeMap<String, (String, String, String, String)> = BTreeMap::new();
    for package in packages {
        let entry = package
            .as_table()
            .ok_or_else(|| failure("Resolved lock entry is malformed"))?;
        let name = entry
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| failure("Resolved lock entry has no name"))?;
        let version = entry
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| failure("Resolved lock entry has no version"))?;
        let Some(source) = entry.get("source").and_then(|s| s.as_str()) else {
            continue;
        };
        // Only git sources carry a commit fragment; registry and others are
        // not part of the first-party refresh graph.
        if !source.starts_with("git+") {
            continue;
        }
        let parsed = parse_lock_source(source)?;
        // Bind canonical URL + ref + package + version + commit. Manifest
        // source config errors (non-plain URLs, bad shas) fail closed here.
        let identity = (
            parsed.canonical.clone(),
            parsed.git_ref.clone(),
            version.to_owned(),
            parsed.sha.clone(),
        );
        if let Some(first) = seen.get(name) {
            if first != &identity {
                return Err(failure(format!(
                    "Resolved lock has colliding package name with different source/version/commit: {name}"
                )));
            }
            continue;
        }
        seen.insert(name.to_owned(), identity);
        // Same-name same-identity repeats (e.g. workspace duplicates) keep one.
        graph.entry(name.to_owned()).or_insert(DepSelBody {
            url: parsed.canonical,
            fetch: parsed.fetch,
            sha: parsed.sha,
        });
    }
    Ok(graph)
}

/// Complete active transitive first-party selection: every git lock entry
/// whose canonical source matches the refresh scope plus every direct v01
/// manifest edge (root + workspace members + target/dev/build tables, with
/// renamed `package =` honoured). Never reads [patch] tables, so
/// [patch.unused] and all other patches are excluded. Returns real package
/// names (not alias keys) sorted and deduped; ambiguous same-name разными
/// source/version selections fail closed for unambiguous cargo update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphSel {
    pub package: String,
    pub version: String,
    pub canonical_url: String,
    pub git_ref: String,
}

fn select_active_graph(workdir: &Path, refresh: &Refresh) -> Result<Vec<GraphSel>> {
    // 1. Transitive closure from the resolved lock (all matching git entries).
    let lock_bytes = std::fs::read(workdir.join("Cargo.lock"))
        .map_err(|_| failure("Push refresh requires a committed Cargo.lock"))?;
    let lock_value: toml::Value = toml::from_str(std::str::from_utf8(&lock_bytes)?)?;
    let packages = lock_value
        .get("package")
        .and_then(|p| p.as_array())
        .ok_or_else(|| failure("Resolved lock has no package table"))?;
    // package -> (version, canonical, ref), failing on collisions.
    let mut by_name: BTreeMap<String, (String, String, String)> = BTreeMap::new();
    for package in packages {
        let Some(entry) = package.as_table() else {
            return Err(failure("Resolved lock entry is malformed"));
        };
        let (Some(name), Some(version)) = (
            entry.get("name").and_then(|n| n.as_str()),
            entry.get("version").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let Some(source) = entry.get("source").and_then(|s| s.as_str()) else {
            continue;
        };
        if !source.starts_with("git+") {
            continue;
        }
        let Ok(parsed) = parse_lock_source(source) else {
            continue;
        };
        if parsed.git_ref != refresh.branch {
            continue;
        }
        if !refresh.sources.iter().any(|prefix| {
            parsed.canonical == *prefix || parsed.canonical.starts_with(&format!("{prefix}/"))
        }) {
            continue;
        }
        let identity = (
            version.to_owned(),
            parsed.canonical.clone(),
            parsed.git_ref.clone(),
        );
        if let Some(first) = by_name.get(name) {
            if first != &identity {
                return Err(failure(format!(
                    "Ambiguous first-party package from two sources/versions: {name}"
                )));
            }
            continue;
        }
        by_name.insert(
            name.to_owned(),
            (version.to_owned(), parsed.canonical, parsed.git_ref),
        );
    }
    // 2. Union with direct manifest edges (catches stale locks where a
    // direct v01 edge has no matching lock entry yet, e.g. cshm). Reads root
    // plus workspace members, all target/dev/build tables. Renamed deps use
    // their real `package =` name. Never reads [patch] (excludes unused).
    let mut manifests = vec![workdir.join("Cargo.toml")];
    if let Ok(root_bytes) = std::fs::read(workdir.join("Cargo.toml")) {
        if let Ok(root) =
            toml::from_str::<toml::Value>(std::str::from_utf8(&root_bytes).unwrap_or(""))
        {
            if let Some(workspace) = root.get("workspace") {
                if let Some(members) = workspace.get("members").and_then(|m| m.as_array()) {
                    for member in members.iter().filter_map(|m| m.as_str()) {
                        // Only plain relative member paths (no globs for push).
                        if member.is_empty()
                            || member.contains(['*', '?', '[', ']', '{', '}', '!', '\\'])
                            || member.starts_with('/')
                            || member.contains("..")
                        {
                            continue;
                        }
                        manifests.push(workdir.join(member).join("Cargo.toml"));
                    }
                }
            }
        }
    }
    for manifest_path in manifests {
        let Ok(bytes) = std::fs::read(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = toml::from_str::<toml::Value>(std::str::from_utf8(&bytes).unwrap_or(""))
        else {
            return Err(failure("Consumer manifest is not valid TOML"));
        };
        let mut tables: Vec<&toml::Value> = Vec::new();
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(deps) = manifest.get(key) {
                tables.push(deps);
            }
        }
        if let Some(targets) = manifest.get("target").and_then(|t| t.as_table()) {
            for target in targets.values() {
                for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    if let Some(deps) = target.get(key) {
                        tables.push(deps);
                    }
                }
            }
        }
        // Intentionally no [patch] tables: direct no-patch graph plus
        // [patch.unused] exclusion. Patch sweeps are not transitive resolvers.
        for table in tables {
            let Some(deps) = table.as_table() else {
                continue;
            };
            for (alias, entry) in deps {
                let Some(detail) = entry.as_table() else {
                    continue;
                };
                let (Some(url), Some(branch)) = (
                    detail.get("git").and_then(|u| u.as_str()),
                    detail.get("branch").and_then(|b| b.as_str()),
                ) else {
                    continue;
                };
                if branch != refresh.branch {
                    continue;
                }
                let Ok(canonical) = crate::cache::canonical_repository(url) else {
                    return Err(failure(format!("Dependency {alias} source is not plain")));
                };
                if !refresh.sources.iter().any(|prefix| {
                    canonical == *prefix || canonical.starts_with(&format!("{prefix}/"))
                }) {
                    continue;
                }
                // Renamed deps: real package name is `package =`, else alias.
                let real = detail
                    .get("package")
                    .and_then(|p| p.as_str())
                    .unwrap_or(alias);
                if !by_name.contains_key(real) {
                    // Version unknown from manifest alone; record with empty
                    // version and let `cargo update -p <package>` resolve.
                    // Ambiguity with a locked entry of the same name but a
                    // different source is still refused below via lock check.
                    by_name.insert(
                        real.to_owned(),
                        (String::new(), canonical, branch.to_owned()),
                    );
                }
            }
        }
    }
    let mut selected: Vec<GraphSel> = by_name
        .into_iter()
        .map(|(package, (version, canonical_url, git_ref))| GraphSel {
            package,
            version,
            canonical_url,
            git_ref,
        })
        .collect();
    selected.sort_by(|a, b| a.package.cmp(&b.package));
    Ok(selected)
}

struct ParsedSource {
    canonical: String,
    fetch: String,
    git_ref: String,
    sha: String,
}

fn parse_lock_source(source: &str) -> Result<ParsedSource> {
    let Some((location, fragment)) = source.split_once('#') else {
        return Err(failure("Resolved lock source has no commit"));
    };
    if fragment.is_empty() || !valid_sha(fragment) {
        return Err(failure("Resolved lock commit is not a full SHA"));
    }
    let bare = location.strip_prefix("git+").unwrap_or(location);
    // Split query (?branch=v01&...) from the fetch URL before canonicalizing.
    let (fetch_base, query) = match bare.split_once('?') {
        Some((base, q)) => (base, Some(q)),
        None => (bare, None),
    };
    let canonical = crate::cache::canonical_repository(fetch_base)
        .map_err(|_| failure("Resolved lock source is not a plain forge URL"))?;
    // Branch/ref comes from the source query (?branch=, ?rev=, ?tag=).
    // First-party v01 entries carry ?branch=v01; anything else is kept as
    // its raw ref value for identity binding (mismatches fail at selection).
    let mut git_ref = String::new();
    if let Some(q) = query {
        for part in q.split('&') {
            if let Some(v) = part.strip_prefix("branch=") {
                git_ref = v.to_owned();
                break;
            }
            if let Some(v) = part.strip_prefix("rev=") {
                git_ref = v.to_owned();
            } else if let Some(v) = part.strip_prefix("tag=") {
                if git_ref.is_empty() {
                    git_ref = v.to_owned();
                }
            }
        }
    }
    Ok(ParsedSource {
        canonical,
        fetch: fetch_base.to_owned(),
        git_ref,
        sha: fragment.to_owned(),
    })
}

fn read_full_lock(workdir: &Path) -> Result<Vec<u8>> {
    Ok(std::fs::read(workdir.join("Cargo.lock"))?)
}

fn retain_resolved_evidence(
    planned: &PlannedBuild,
    selected: &[GraphSel],
    lock_bytes: &[u8],
    lock_hash: &str,
) -> Result<()> {
    let ns = &planned.ns_dir;
    std::fs::create_dir_all(ns)?;
    // Newly resolved graph (selected source/package/version/commit) plus the
    // full lock retained as evidence before any gate runs.
    let graph_json = serde_json::to_string_pretty(
        &selected
            .iter()
            .map(|s| {
                serde_json::json!({
                    "package": s.package,
                    "version": s.version,
                    "canonical_url": s.canonical_url,
                    "ref": s.git_ref,
                })
            })
            .collect::<Vec<_>>(),
    )?;
    // Filenames include the head so concurrent bursts never clobber.
    let head = &planned.head;
    std::fs::write(ns.join(format!("resolved-{head}.lock")), lock_bytes)?;
    std::fs::write(
        ns.join(format!("resolved-{head}.graph.json")),
        graph_json.as_bytes(),
    )?;
    std::fs::write(
        ns.join(format!("resolved-{head}.hash")),
        format!(
            "{lock_hash}\n{}\n{}\n",
            planned.manifest_sha, planned.tool_revision
        )
        .as_bytes(),
    )?;
    // Actual compiler versions + manifest hash before gates (durable).
    let runner = crate::Runner::until(
        planned.workdir.clone(),
        planned.environment.clone(),
        planned.deadline,
    )?;
    let rustc = runner
        .run(&["rustc".to_owned(), "--version".to_owned()], true)
        .unwrap_or_else(|_| "rustc unknown".into());
    let cargo = runner
        .run(&["cargo".to_owned(), "--version".to_owned()], true)
        .unwrap_or_else(|_| "cargo unknown".into());
    std::fs::write(
        ns.join(format!("toolchain-{head}.txt")),
        format!(
            "{rustc}\n{cargo}\nmanifest_sha={}\ntool_revision={}\n",
            planned.manifest_sha, planned.tool_revision
        )
        .as_bytes(),
    )?;
    Ok(())
}

fn retain_gate_evidence(planned: &PlannedBuild, before: &[u8], after: &[u8]) -> Result<()> {
    let head = &planned.head;
    std::fs::write(
        planned.ns_dir.join(format!("gate-before-{head}.lock")),
        before,
    )?;
    std::fs::write(
        planned.ns_dir.join(format!("gate-after-{head}.lock")),
        after,
    )?;
    Ok(())
}

/// Gates run the declared checks generically under the pinned binary and the
/// existing warm target, truly --offline --locked with the same full lock
/// bytes for every gate. One cache target lock is held for the ENTIRE gates
/// phase (no per-gate lock/unlock, no bypass): the legacy shell refresh/check
/// script re-resolves online, so it must never be the official gate
/// implementation. Only explicit manifest `commands` arrays with --offline
/// --locked are run. Any lock change fails even if the gate command exits 0.
/// Gate execution environment: the planned (manifest-merged) environment
/// plus budget-bound parallelism and the shared locked target. Shared by
/// gate execution and runtime capture so the fingerprinted environment is
/// exactly the gates' environment. The extra keys beyond
/// `planned.environment` (`CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`,
/// `CARGO_TARGET_DIR`, `CCID_TARGET_LOCK_HELD`) are execution wiring, none
/// of them in the runtime fingerprint whitelist, so captures over
/// `planned.environment` alone would fingerprint identically; sharing this
/// helper keeps that parity structural instead of coincidental.
fn gate_execution_env(planned: &PlannedBuild, target: &Path) -> Result<crate::Environment> {
    let mut gate_env = planned.environment.clone();
    let resources = crate::budget(&gate_env)?;
    gate_env.insert("CARGO_BUILD_JOBS".into(), resources.jobs.to_string().into());
    gate_env.insert(
        "RUST_TEST_THREADS".into(),
        resources.test_threads.to_string().into(),
    );
    gate_env.insert("CARGO_TARGET_DIR".into(), target.as_os_str().to_owned());
    gate_env.insert(
        "CCID_TARGET_LOCK_HELD".into(),
        target.as_os_str().to_owned(),
    );
    if let Some(build) = gate_env.get(&std::ffi::OsString::from("CARGO_BUILD_BUILD_DIR")) {
        if Path::new(build) != target {
            return Err(failure("A distinct CARGO_BUILD_BUILD_DIR is unsupported: intermediates must share the locked target directory"));
        }
    }
    Ok(gate_env)
}

/// Resolve the shared warm target directory for the gates without taking
/// its lock. Gate execution takes the lock separately; runtime capture only
/// needs the same resolved path value in the environment.
fn gate_target_dir(planned: &PlannedBuild) -> Result<PathBuf> {
    let root = planned.workdir.canonicalize()?;
    let identity = crate::cache::repository_identity(&root, &planned.environment, false)?;
    crate::cache::target_directory(&root, &identity, &planned.environment)
}

fn run_gates(planned: &PlannedBuild, frozen_lock: &[u8]) -> Result<()> {
    if !is_self_checks_command(&planned.command) {
        return Err(failure(
            "Push execution requires explicit ccid:run-declared-checks command",
        ));
    }
    let gate_argv = gate_commands(&planned.workdir, &planned.checks)?;
    // Resolve the SAME warm target the manual route uses (explicit
    // CARGO_TARGET_DIR from the declared per-job environment when present,
    // otherwise the standard cache derivation), then hold its lock across
    // every gate command.
    let target = gate_target_dir(planned)?;
    let (target, _target_lock) = crate::cache::lock_target(&target, planned.deadline)?;
    // Gate environment mirrors the standard check allocation (budget-bound
    // parallelism, shared locked target) without re-locking per gate.
    let gate_env = gate_execution_env(planned, &target)?;
    let runner = crate::Runner::until(planned.workdir.clone(), gate_env, planned.deadline)?;
    for (index, argv) in gate_argv.iter().enumerate() {
        runner.run(argv, false)?;
        let now = read_full_lock(&planned.workdir)?;
        if now != frozen_lock {
            return Err(failure(format!(
                "Frozen lock mutated during gate {} ({}); refusing an unproven graph",
                index,
                planned.checks.join(",")
            )));
        }
    }
    Ok(())
}

/// Resolve declared gate argv generically from the manifest (check order,
/// command order within each check), after offline/locked validation.
/// Shared by validation and execution so both agree on what runs.
fn gate_commands(workdir: &Path, checks: &[String]) -> Result<Vec<Vec<String>>> {
    validate_gate_manifest(workdir, checks)?;
    let bytes = std::fs::read(workdir.join(".ci/ccid.toml"))
        .map_err(|_| failure("Push gates require .ci/ccid.toml"))?;
    let manifest: toml::Value = toml::from_str(std::str::from_utf8(&bytes)?)?;
    let checks_table = manifest
        .get("checks")
        .and_then(|c| c.as_table())
        .ok_or_else(|| failure("Push manifest names no checks"))?;
    let mut out = Vec::new();
    for name in checks {
        let check = checks_table
            .get(name)
            .ok_or_else(|| failure(format!("Push check is not declared: {name}")))?;
        let commands = check
            .get("commands")
            .and_then(|c| c.as_array())
            .ok_or_else(|| failure(format!("Push check has no commands: {name}")))?;
        for command in commands {
            let argv = command
                .as_array()
                .ok_or_else(|| failure(format!("Push gate command is malformed: {name}")))?;
            out.push(
                argv.iter()
                    .map(|v| {
                        v.as_str()
                            .ok_or_else(|| failure(format!("Push gate arg is malformed: {name}")))
                            .map(str::to_owned)
                    })
                    .collect::<Result<Vec<String>>>()?,
            );
        }
    }
    Ok(out)
}

/// Manifest gate validation: push gates must be explicit cargo commands with
/// --offline --locked (no online re-resolution). Fails closed on unknown
/// kinds, missing flags, or source config errors. Generic: no product
/// features or skips are hardcoded here; exact argv preservation is asserted
/// in graph tests with fixtures.
fn validate_gate_manifest(workdir: &Path, checks: &[String]) -> Result<()> {
    let bytes = std::fs::read(workdir.join(".ci/ccid.toml"))
        .map_err(|_| failure("Push gates require .ci/ccid.toml"))?;
    let manifest: toml::Value = toml::from_str(std::str::from_utf8(&bytes)?)?;
    let checks_table = manifest
        .get("checks")
        .and_then(|c| c.as_table())
        .ok_or_else(|| failure("Push manifest names no checks"))?;
    for name in checks {
        let check = checks_table
            .get(name)
            .ok_or_else(|| failure(format!("Push check is not declared: {name}")))?;
        let kind = check
            .get("kind")
            .and_then(|k| k.as_str())
            .ok_or_else(|| failure(format!("Push check has no kind: {name}")))?;
        // Only explicit commands are allowed for frozen push gates: cargo
        // kinds build their own argv without --offline, nix/javascript need
        // network or installs. Anything else fails closed.
        if kind != "commands" {
            return Err(failure(format!(
                "Push gates require kind=commands for frozen execution: {name}"
            )));
        }
        let commands = check
            .get("commands")
            .and_then(|c| c.as_array())
            .ok_or_else(|| failure(format!("Push check has no commands: {name}")))?;
        if commands.is_empty() {
            return Err(failure(format!("Push check commands are empty: {name}")));
        }
        for command in commands {
            let argv = command
                .as_array()
                .ok_or_else(|| failure(format!("Push gate command is malformed: {name}")))?;
            let parts: Vec<String> = argv
                .iter()
                .map(|v| {
                    v.as_str()
                        .ok_or_else(|| failure(format!("Push gate arg is malformed: {name}")))
                        .map(str::to_owned)
                })
                .collect::<Result<Vec<String>>>()?;
            if parts.is_empty() || parts.iter().any(|a| a.contains('\0')) {
                return Err(failure(format!("Push gate command is invalid: {name}")));
            }
            // Cargo gates must be truly offline + locked; nothing may
            // re-resolve during gates. Non-cargo commands are refused for
            // frozen push gates (no shell/Python additions). The flags must
            // precede the first `--` separator: options after it are
            // test/program arguments, not Cargo flags, so a `--locked` that
            // only appears there must not satisfy the freeze.
            if parts[0] == "cargo" {
                let cargo_args: &[String] = match parts.iter().position(|arg| arg == "--") {
                    Some(index) => &parts[..index],
                    None => &parts[..],
                };
                if !cargo_args.contains(&"--offline".to_owned())
                    || !cargo_args.contains(&"--locked".to_owned())
                {
                    return Err(failure(format!(
                        "Push cargo gates require --offline --locked: {name}"
                    )));
                }
            } else {
                return Err(failure(format!(
                    "Push gates allow only cargo commands: {name}"
                )));
            }
        }
    }
    Ok(())
}

/// Single newest-head resolution round with unambiguous source/package
/// selections, generic manifest-owned preparation, then prefetch so the
/// frozen gates need no network. Runs with the validated planned environment
/// (preserving worker origin/cache/branch identities, declared per-job env
/// and budget), never ambient. No new shell/Python logic: preparation runs
/// only manifest-declared existing driver commands (default none).
fn refresh_graph(planned: &PlannedBuild, selected: &[GraphSel]) -> Result<()> {
    let mut argv = vec!["cargo".to_owned(), "update".to_owned()];
    for sel in selected {
        // Unambiguous by construction: select_active_graph refused duplicate
        // names from different sources/versions, and we pass real package
        // names (not alias keys). Cargo errors loudly on any residual
        // ambiguity instead of resolving the wrong graph.
        argv.push("-p".into());
        argv.push(sel.package.clone());
    }
    let runner = crate::Runner::until(
        planned.workdir.clone(),
        planned.environment.clone(),
        planned.deadline,
    )?;
    runner.run(&argv, false)?;
    // Generic manifest-owned lock preparation (existing checked-in drivers
    // only, e.g. a compatibility selection): runs once AFTER `cargo update`
    // and BEFORE the freeze (`cargo fetch --locked`) and gates, under the
    // same bounded planned Runner. Default empty (no preparation). A failing
    // preparation propagates immediately so the frozen gates never run on an
    // unprepared graph.
    run_prepare_commands(&runner, &planned.refresh.prepare_commands)?;
    runner.run(&["cargo".into(), "fetch".into(), "--locked".into()], false)?;
    Ok(())
}

/// Run generic manifest-owned preparation commands in order under a bounded
/// Runner. Empty means no preparation. The first failure propagates and
/// later commands never run (callers must invoke this strictly before the
/// freeze so gates only ever see the prepared graph).
pub(crate) fn run_prepare_commands(runner: &crate::Runner, commands: &[Vec<String>]) -> Result<()> {
    for argv in commands {
        crate::jobs::validate_prepare_commands(std::slice::from_ref(argv))?;
        runner.run(argv, false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const URL: &str = "https://forge.example.invalid/cpkg/demo.git";
    const DEP_URL: &str = "https://forge.example.invalid/cpkg/deplib.git";

    fn setup(trigger: Trigger) -> (tempfile::TempDir, PushSpec) {
        let guard = tempfile::tempdir().unwrap();
        // Cache root must be stable across callers in one burst: use a
        // sibling directory that outlives the per-spec TempDir guard.
        let root = guard.path().join("cache-root");
        std::fs::create_dir_all(&root).unwrap();
        let spec = PushSpec {
            consumer_url: URL.into(),
            consumer_branch: "v01".into(),
            job: "fast".into(),
            trigger,
            cache_root: root,
            timeout_secs: 20,
        };
        (guard, spec)
    }

    fn consumer_trigger(sha: &str) -> Trigger {
        Trigger {
            kind: TriggerKind::Consumer,
            repo: URL.into(),
            branch: "v01".into(),
            sha: sha.into(),
            name: None,
        }
    }

    fn dep_trigger(name: &str, sha: &str) -> Trigger {
        Trigger {
            kind: TriggerKind::Dependency,
            repo: DEP_URL.into(),
            branch: "v01".into(),
            sha: sha.into(),
            name: Some(name.into()),
        }
    }

    fn body(trigger: Trigger, outcome: &str, deps: &[(&str, String)]) -> ReceiptBody {
        // Coherent fixture: a consumer body proves its own trigger head, so
        // exact-match attach works; dependency fixtures prove the shared
        // consumer head "c"*40 unless overridden.
        match trigger.kind {
            TriggerKind::Consumer => {
                let head = trigger.sha.clone();
                body_with_consumer(trigger, outcome, deps, &head)
            }
            TriggerKind::Dependency => body_with_consumer(trigger, outcome, deps, &"c".repeat(40)),
        }
    }

    fn body_with_consumer(
        trigger: Trigger,
        outcome: &str,
        deps: &[(&str, String)],
        consumer_commit: &str,
    ) -> ReceiptBody {
        ReceiptBody {
            outcome: outcome.into(),
            error: String::new(),
            consumer_commit: consumer_commit.into(),
            deps: deps
                .iter()
                .map(|(name, sha)| {
                    (
                        (*name).into(),
                        DepSelBody {
                            url: "forge.example.invalid/cpkg/deplib".into(),
                            fetch: DEP_URL.into(),
                            sha: sha.clone(),
                        },
                    )
                })
                .collect(),
            rematerialized: Vec::new(),
            manifest_sha: "m".repeat(64),
            checks: vec!["fast".into()],
            trigger,
        }
    }

    fn output_for(body: ReceiptBody) -> BuildOutput {
        BuildOutput {
            runtime_identity: "test-runtime-v1".into(),
            dep_branches: body
                .deps
                .keys()
                .map(|name| (name.clone(), "v01".into()))
                .collect(),
            body,
        }
    }

    /// Shared ancestry-probe shape for the fake policy: kept behind an
    /// alias because the inline form trips `type_complexity`.
    type AncestryFn = Arc<dyn Fn(&str, &str, &str, &str) -> bool + Send + Sync>;

    #[derive(Clone)]
    struct FakePolicy {
        runtime_identity: String,
        live_head: Option<String>,
        ancestry: AncestryFn,
    }

    impl CoalescePolicy for FakePolicy {
        fn current_proof(&self, spec: &PushSpec, _deadline: Instant) -> Result<LiveProof> {
            let live_head = self
                .live_head
                .clone()
                .ok_or_else(|| failure("no live consumer head"))?;
            Ok(LiveProof {
                identity: BuildIdentity {
                    tool_revision: crate::SOURCE_REVISION.into(),
                    runtime_identity: self.runtime_identity.clone(),
                    config_identity: config_for(&live_head, &spec.job),
                },
                live_head,
            })
        }
        fn is_ancestor(
            &self,
            url: &str,
            branch: &str,
            old: &str,
            new: &str,
            _deadline: Instant,
        ) -> bool {
            (self.ancestry)(url, branch, old, new)
        }
    }

    fn fake_policy_live(head: &str) -> FakePolicy {
        FakePolicy {
            runtime_identity: "test-runtime-v1".into(),
            live_head: Some(head.into()),
            ancestry: Arc::new(|_, _, old, new| old == new),
        }
    }

    fn store_test_receipt(spec: &PushSpec, receipt: &Receipt) {
        let dir = namespace_dir(
            &spec.cache_root,
            &spec.consumer_url,
            &spec.consumer_branch,
            &spec.job,
        )
        .unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        store_receipt(&dir, receipt).unwrap();
        let pending = vec![PendingEntry {
            generation: receipt.generation,
            trigger: receipt.trigger.clone(),
        }];
        store_state(
            &dir,
            &State {
                generation: receipt.generation,
                receipt_generation: receipt.generation,
                pending,
            },
        )
        .unwrap();
    }

    fn receipt_for(trigger: Trigger, outcome: &str, deps: &[(&str, String)]) -> Receipt {
        let body = body(trigger.clone(), outcome, deps);
        Receipt {
            generation: 7,
            admitted: 7,
            outcome: body.outcome.clone(),
            error: String::new(),
            consumer_url: URL.into(),
            consumer_branch: "v01".into(),
            consumer_commit: body.consumer_commit.clone(),
            deps: body
                .deps
                .iter()
                .map(|(name, selected)| {
                    (
                        name.clone(),
                        DepSel {
                            url: selected.url.clone(),
                            fetch: selected.fetch.clone(),
                            sha: selected.sha.clone(),
                            branch: "v01".into(),
                        },
                    )
                })
                .collect(),
            tool_revision: crate::SOURCE_REVISION.into(),
            runtime_identity: "test-runtime-v1".into(),
            config_identity: config_for(&body.consumer_commit, "fast"),
            manifest_sha: body.manifest_sha.clone(),
            job: "fast".into(),
            checks: body.checks.clone(),
            rematerialized: Vec::new(),
            trigger: trigger_ser(&trigger),
            admitted_triggers: vec![trigger_ser(&trigger)],
        }
    }

    fn fixture_repo() -> (tempfile::TempDir, String, String) {
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q", "-b", "v01"]);
        git(&["config", "user.name", "Fixture"]);
        git(&["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(repo.path().join("a.txt"), "a\n").unwrap();
        git(&["add", "--all"]);
        git(&["-c", "commit.gpgsign=false", "commit", "-qm", "one"]);
        let old = git(&["rev-parse", "HEAD"]);
        std::fs::write(repo.path().join("b.txt"), "b\n").unwrap();
        git(&["add", "--all"]);
        git(&["-c", "commit.gpgsign=false", "commit", "-qm", "two"]);
        let new = git(&["rev-parse", "HEAD"]);
        assert!(is_ancestor_in(repo.path(), &old, &new));
        (repo, old, new)
    }

    #[test]
    fn spec_validation_fails_closed() {
        let (_guard, good) = setup(consumer_trigger(&"a".repeat(40)));
        validate_spec(&good).unwrap();
        let (_guard, mut bad) = setup(consumer_trigger(&"a".repeat(40)));
        bad.consumer_branch = "bad branch".into();
        assert!(validate_spec(&bad).is_err());
        let (_guard, mut bad) = setup(consumer_trigger(&"a".repeat(40)));
        bad.consumer_url = "https://user:pass@forge.example.invalid/r.git".into();
        assert!(validate_spec(&bad).is_err());
        let (_guard, bad) = setup(consumer_trigger("short"));
        assert!(validate_spec(&bad).is_err());
        let (_guard, bad) = setup(dep_trigger("", &"a".repeat(40)));
        assert!(validate_spec(&bad).is_err());
        let (_guard, good_name) = setup(dep_trigger("ok-name_1", &"a".repeat(40)));
        validate_spec(&good_name).unwrap();
        let (_guard, mut bad) = setup(consumer_trigger(&"a".repeat(40)));
        bad.trigger.name = Some("nope".into());
        assert!(validate_spec(&bad).is_err());
        let (_guard, mut bad) = setup(consumer_trigger(&"a".repeat(40)));
        bad.cache_root = PathBuf::from("relative");
        assert!(validate_spec(&bad).is_err());
        let (_guard, mut bad) = setup(consumer_trigger(&"a".repeat(40)));
        bad.timeout_secs = 0;
        assert!(validate_spec(&bad).is_err());
        // Canonical trigger provenance: consumer names the consumer repo and
        // branch; dependencies name a distinct canonical repository.
        let (_guard, mut bad) = setup(consumer_trigger(&"a".repeat(40)));
        bad.trigger.repo = DEP_URL.into();
        assert!(validate_spec(&bad).is_err());
        let (_guard, mut bad) = setup(consumer_trigger(&"a".repeat(40)));
        bad.trigger.branch = "other".into();
        assert!(validate_spec(&bad).is_err());
        let (_guard, mut bad) = setup(dep_trigger("deplib", &"a".repeat(40)));
        bad.trigger.repo = URL.into();
        assert!(validate_spec(&bad).is_err());
        let (_guard, mut bad) = setup(dep_trigger("deplib", &"a".repeat(40)));
        bad.trigger.repo = "https://user:pass@forge.example.invalid/x.git".into();
        assert!(validate_spec(&bad).is_err());
        let (_guard, mut bad) = setup(dep_trigger("deplib", &"a".repeat(40)));
        bad.trigger.repo = String::new();
        assert!(validate_spec(&bad).is_err());
    }

    #[test]
    fn namespace_is_deterministic_and_job_scoped() {
        let root = PathBuf::from("/cache");
        let left = namespace_dir(root.as_path(), URL, "v01", "fast").unwrap();
        let again = namespace_dir(root.as_path(), URL, "v01", "fast").unwrap();
        assert_eq!(left, again);
        assert_ne!(
            left,
            namespace_dir(root.as_path(), URL, "v01", "slow").unwrap()
        );
        assert_ne!(
            left,
            namespace_dir(root.as_path(), URL, "main", "fast").unwrap()
        );
        assert!(left.to_string_lossy().contains("demo"));
        assert!(namespace_dir(root.as_path(), "not a url at all!!!", "v01", "fast").is_err());
    }

    #[test]
    fn refresh_selection_covers_all_branches_and_prefixes() {
        let manifest: toml::Value = toml::from_str(
            "[dependencies]\n\
             keep = { git = 'https://forge.example.invalid/cpkg/keep.git', branch = 'v01' }\n\
             mainline = { git = 'https://forge.example.invalid/cpkg/mainline.git', branch = 'main' }\n\
             outside = { git = 'https://other.example.invalid/x/outside.git', branch = 'v01' }\n\
             sibling = { git = 'https://forge.example.invalid/other/sibling.git', branch = 'v01' }\n\
             [target.'cfg(unix)'.dependencies]\n\
             tdep = { git = 'https://forge.example.invalid/cpkg/tdep.git', branch = 'v01' }\n\
             [patch.'https://github.com/example/orig']\n\
             pdep = { git = 'https://forge.example.invalid/cpkg/pdep.git', branch = 'v01' }\n",
        )
        .unwrap();
        let refresh = Refresh {
            branch: "v01".into(),
            sources: vec!["forge.example.invalid/cpkg".into()],
            prepare_commands: Vec::new(),
        };
        assert_eq!(
            select_refresh(&manifest, &refresh).unwrap(),
            ["keep".to_owned(), "pdep".to_owned(), "tdep".to_owned()]
        );
        let wide = Refresh {
            branch: "v01".into(),
            sources: vec!["forge.example.invalid".into()],
            prepare_commands: Vec::new(),
        };
        let selected = select_refresh(&manifest, &wide).unwrap();
        assert!(selected.contains(&"sibling".to_owned()));
        assert!(!selected.contains(&"outside".to_owned()));
        assert!(!selected.contains(&"mainline".to_owned()));
    }

    #[test]
    fn refresh_config_validation_fails_closed() {
        let good = Refresh {
            branch: "v01".into(),
            sources: vec!["forge.example.invalid/cpkg".into()],
            prepare_commands: Vec::new(),
        };
        crate::jobs::validate_refresh(&good).unwrap();
        for bad in [
            Refresh {
                branch: "bad branch".into(),
                sources: vec!["forge.example.invalid/cpkg".into()],
                prepare_commands: Vec::new(),
            },
            Refresh {
                branch: "v01".into(),
                sources: vec![],
                prepare_commands: Vec::new(),
            },
            Refresh {
                branch: "v01".into(),
                sources: vec!["https://forge.example.invalid/cpkg".into()],
                prepare_commands: Vec::new(),
            },
            Refresh {
                branch: "v01".into(),
                sources: vec!["forge.example.invalid/cpkg/".into()],
                prepare_commands: Vec::new(),
            },
            Refresh {
                branch: "v01".into(),
                sources: vec!["FORGE.EXAMPLE.INVALID/cpkg".into()],
                prepare_commands: Vec::new(),
            },
        ] {
            assert!(crate::jobs::validate_refresh(&bad).is_err());
        }
    }

    #[test]
    fn cross_repo_simultaneous_notifications_share_one_build() {
        let consumer_sha = "a".repeat(40);
        let dep_sha = "d".repeat(40);
        let (_guard, consumer_spec) = setup(consumer_trigger(&consumer_sha));
        let dep_spec = PushSpec {
            trigger: dep_trigger("deplib", &dep_sha),
            ..consumer_spec.clone()
        };
        let calls = Arc::new(Mutex::new(0u32));
        let consumer_body = body_with_consumer(
            consumer_spec.trigger.clone(),
            "pass",
            &[("deplib", dep_sha.clone())],
            &consumer_sha,
        );
        let build_consumer_body = consumer_body.clone();
        let build = {
            let calls = calls.clone();
            let body = build_consumer_body.clone();
            move |_: u64| {
                *calls.lock().unwrap() += 1;
                std::thread::sleep(Duration::from_millis(300));
                Ok(output_for(body.clone()))
            }
        };
        let policy = fake_policy_live(&consumer_sha);
        let first = consumer_spec.clone();
        let second = dep_spec.clone();
        let first_policy = policy.clone();
        let first_handle =
            std::thread::spawn(move || run_push_with(&first, &build, &first_policy, 0, 2));
        let build2 = {
            let calls = calls.clone();
            let body = consumer_body.clone();
            move |_: u64| {
                *calls.lock().unwrap() += 1;
                std::thread::sleep(Duration::from_millis(300));
                Ok(output_for(body.clone()))
            }
        };
        let second_handle = {
            let policy = policy.clone();
            std::thread::spawn(move || run_push_with(&second, &build2, &policy, 0, 2))
        };
        let first_outcome = first_handle.join().unwrap().unwrap();
        let second_outcome = second_handle.join().unwrap().unwrap();
        assert_eq!(*calls.lock().unwrap(), 1, "one admitted build per burst");
        let built = [&first_outcome, &second_outcome]
            .iter()
            .filter(|outcome| matches!(outcome, PushOutcome::Built { .. }))
            .count();
        assert_eq!(built, 1, "exactly one builder across repositories");
    }

    #[test]
    fn overlapping_callers_share_one_build() {
        let (_guard, spec) = setup(consumer_trigger(&"a".repeat(40)));
        let calls = Arc::new(Mutex::new(0u32));
        let policy = fake_policy_live(&"a".repeat(40));
        let trigger = spec.trigger.clone();
        let build = Arc::new({
            let calls = calls.clone();
            move |_: u64| {
                *calls.lock().unwrap() += 1;
                std::thread::sleep(Duration::from_millis(300));
                Ok(output_for(body(trigger.clone(), "pass", &[])))
            }
        });
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let spec = spec.clone();
                let policy = policy.clone();
                let build = build.clone();
                std::thread::spawn(move || run_push_with(&spec, &*build, &policy, 0, 5))
            })
            .collect();
        let mut built = 0;
        for handle in handles {
            match handle.join().unwrap().unwrap() {
                PushOutcome::Built { .. } => built += 1,
                PushOutcome::Attached { .. } => {}
            }
        }
        assert_eq!(*calls.lock().unwrap(), 1, "one admitted build per burst");
        assert_eq!(built, 1, "exactly one builder, two attachers");
    }

    #[test]
    fn event_during_build_gets_one_later_latest_build() {
        let (_guard, first) = setup(consumer_trigger(&"a".repeat(40)));
        let calls = Arc::new(Mutex::new(0u32));
        let policy = fake_policy_live(&"a".repeat(40));
        let build_first = {
            let calls = calls.clone();
            let trigger = first.trigger.clone();
            move |_: u64| {
                *calls.lock().unwrap() += 1;
                std::thread::sleep(Duration::from_millis(400));
                Ok(output_for(body(trigger.clone(), "pass", &[])))
            }
        };
        let second = PushSpec {
            trigger: consumer_trigger(&"b".repeat(40)),
            ..first.clone()
        };
        let build_second = {
            let calls = calls.clone();
            let trigger = second.trigger.clone();
            move |_: u64| {
                *calls.lock().unwrap() += 1;
                std::thread::sleep(Duration::from_millis(100));
                Ok(output_for(body(trigger.clone(), "pass", &[])))
            }
        };
        let first_policy = policy.clone();
        let first_handle =
            std::thread::spawn(move || run_push_with(&first, &build_first, &first_policy, 0, 10));
        std::thread::sleep(Duration::from_millis(150));
        // A newer consumer event lands mid-build on the same branch: it
        // cannot attach (different sha, ancestry false here), so exactly
        // one later latest-head build runs for it.
        let second_handle = {
            let policy = policy.clone();
            std::thread::spawn(move || run_push_with(&second, &build_second, &policy, 0, 10))
        };
        assert!(matches!(
            first_handle.join().unwrap().unwrap(),
            PushOutcome::Built { .. }
        ));
        assert!(matches!(
            second_handle.join().unwrap().unwrap(),
            PushOutcome::Built { .. }
        ));
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[test]
    fn admitted_generation_frozen_and_mid_build_events_stay_pending() {
        let (_guard, spec) = setup(consumer_trigger(&"a".repeat(40)));
        let policy = fake_policy_live(&"a".repeat(40));
        let dir = namespace_dir(
            &spec.cache_root,
            &spec.consumer_url,
            &spec.consumer_branch,
            &spec.job,
        )
        .unwrap();
        let build = |frozen: u64| {
            // Simulate an event arriving during resolution: it must not join
            // the frozen receipt.
            let _lock = lock_file(&dir.join("notify.lock"), NOTIFY_WAIT_SECS).unwrap();
            let mut state = load_state(&dir).unwrap();
            state.generation = state.generation.saturating_add(1);
            let newer = state.generation;
            assert!(newer > frozen);
            state.pending.push(PendingEntry {
                generation: newer,
                trigger: trigger_ser(&consumer_trigger(&"b".repeat(40))),
            });
            store_state(&dir, &state).unwrap();
            drop(_lock);
            Ok(output_for(body(spec.trigger.clone(), "pass", &[])))
        };
        let outcome = run_push_with(&spec, build, &policy, 0, 5).unwrap();
        let frozen = match outcome {
            PushOutcome::Built { generation } => generation,
            PushOutcome::Attached { .. } => panic!("first run must build"),
        };
        let state = load_state(&dir).unwrap();
        assert_eq!(state.receipt_generation, frozen);
        assert!(state.generation > frozen, "mid-build event stays pending");
        assert!(
            state.pending.iter().any(|entry| entry.generation > frozen),
            "pending retains the newer trigger"
        );
        let receipt: Receipt =
            serde_json::from_slice(&std::fs::read(receipt_path(&dir, frozen)).unwrap()).unwrap();
        assert_eq!(receipt.admitted, frozen);
        assert!(receipt_path(&dir, frozen).exists());
    }

    #[test]
    fn failures_propagate_without_rerun_and_crash_never_counts() {
        let (_guard, spec) = setup(consumer_trigger(&"a".repeat(40)));
        let calls = Arc::new(Mutex::new(0u32));
        let policy = fake_policy_live(&"a".repeat(40));
        let build = |_: u64| {
            *calls.lock().unwrap() += 1;
            Ok(output_for(body(spec.trigger.clone(), "fail", &[])))
        };
        assert!(run_push_with(&spec, build, &policy, 0, 2).is_err());
        assert_eq!(*calls.lock().unwrap(), 1);
        // Identical retrigger propagates the recorded failure, no rebuild.
        assert!(run_push_with(&spec, build, &policy, 0, 2).is_err());
        assert_eq!(*calls.lock().unwrap(), 1, "no blind rerun of known failure");
        // A different trigger does not inherit the failure: it builds anew.
        let other = PushSpec {
            trigger: consumer_trigger(&"b".repeat(40)),
            ..spec.clone()
        };
        let rebuild_other = |_: u64| {
            *calls.lock().unwrap() += 1;
            Ok(output_for(body(other.trigger.clone(), "pass", &[])))
        };
        // Live still names "a" here; the newer trigger cannot attach to the
        // failed head, so it builds its own head.
        run_push_with(&other, rebuild_other, &policy, 0, 2).unwrap();
        assert_eq!(*calls.lock().unwrap(), 2);
        // A crashed (missing) current receipt never counts: remove exactly
        // the recorded receipt, rebuild once.
        let dir = namespace_dir(
            &spec.cache_root,
            &spec.consumer_url,
            &spec.consumer_branch,
            &spec.job,
        )
        .unwrap();
        let current = load_state(&dir).unwrap().receipt_generation;
        std::fs::remove_file(receipt_path(&dir, current)).unwrap();
        let policy_b = fake_policy_live(&"b".repeat(40));
        let rebuild = |_: u64| {
            *calls.lock().unwrap() += 1;
            Ok(output_for(body(spec.trigger.clone(), "pass", &[])))
        };
        run_push_with(&spec, rebuild, &policy_b, 0, 2).unwrap();
        assert_eq!(*calls.lock().unwrap(), 3);
        // A corrupt current receipt never attaches either: it forces a rebuild.
        let current = load_state(&dir).unwrap().receipt_generation;
        std::fs::write(receipt_path(&dir, current), "not json").unwrap();
        run_push_with(&spec, rebuild, &policy_b, 0, 2).unwrap();
        assert_eq!(*calls.lock().unwrap(), 4);
    }

    #[test]
    fn corrupt_state_fails_loudly_not_silently() {
        let (_guard, spec) = setup(consumer_trigger(&"a".repeat(40)));
        let dir = namespace_dir(
            &spec.cache_root,
            &spec.consumer_url,
            &spec.consumer_branch,
            &spec.job,
        )
        .unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.json"), "{broken").unwrap();
        let policy = fake_policy_live(&"a".repeat(40));
        let build = |_: u64| Ok(output_for(body(spec.trigger.clone(), "pass", &[])));
        assert!(run_push_with(&spec, build, &policy, 0, 1).is_err());
    }

    #[test]
    fn lock_contention_waits_but_io_fails_loudly() {
        let (_guard, spec) = setup(consumer_trigger(&"a".repeat(40)));
        let policy = fake_policy_live(&"a".repeat(40));
        let dir = namespace_dir(
            &spec.cache_root,
            &spec.consumer_url,
            &spec.consumer_branch,
            &spec.job,
        )
        .unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let overall = Instant::now() + Duration::from_secs(5);
        let held = try_lock_file(&dir.join("exec.lock"), 0, overall)
            .unwrap()
            .expect("free lock must hold");
        assert!(
            try_lock_file(&dir.join("exec.lock"), 0, overall)
                .unwrap()
                .is_none(),
            "held lock must report contention, not IO"
        );
        drop(held);
        assert!(
            try_lock_file(&dir.join("exec.lock"), 0, overall)
                .unwrap()
                .is_some(),
            "released lock must hold again"
        );
        let build = |_: u64| Ok(output_for(body(spec.trigger.clone(), "pass", &[])));
        run_push_with(&spec, build, &policy, 0, 2).unwrap();
    }

    #[test]
    fn waiting_polls_examine_each_receipt_once() {
        // With the exec lock busy, waiters must poll cheap local state
        // only: exactly one proof plus one ancestry attempt for the stale
        // receipt no matter how many polls elapse, then exactly one
        // reevaluation when the new receipt lands. Regression test for the
        // poll storm (ls-remote plus toolchain probes plus ancestry per
        // poll per waiter). The poll interval itself is unchanged.
        struct CountingPolicy {
            proof_calls: Arc<Mutex<usize>>,
            ancestor_calls: Arc<Mutex<usize>>,
            live_head: Arc<Mutex<String>>,
        }
        impl CoalescePolicy for CountingPolicy {
            fn current_proof(&self, spec: &PushSpec, _deadline: Instant) -> Result<LiveProof> {
                *self.proof_calls.lock().unwrap() += 1;
                let live_head = self.live_head.lock().unwrap().clone();
                Ok(LiveProof {
                    identity: BuildIdentity {
                        tool_revision: crate::SOURCE_REVISION.into(),
                        runtime_identity: "test-runtime-v1".into(),
                        config_identity: config_for(&live_head, &spec.job),
                    },
                    live_head,
                })
            }
            fn is_ancestor(&self, _: &str, _: &str, _: &str, _: &str, _: Instant) -> bool {
                *self.ancestor_calls.lock().unwrap() += 1;
                false
            }
        }
        let head = "c".repeat(40);
        let trigger = consumer_trigger(&"d".repeat(40));
        let (_guard, spec) = setup(trigger.clone());
        let dir = namespace_dir(
            &spec.cache_root,
            &spec.consumer_url,
            &spec.consumer_branch,
            &spec.job,
        )
        .unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        // Stale receipt: the live head matches, but the trigger commit is
        // neither the receipt head nor an ancestor of it.
        store_test_receipt(&spec, &receipt_for(consumer_trigger(&head), "pass", &[]));
        let overall = Instant::now() + Duration::from_secs(30);
        let _held = try_lock_file(&dir.join("exec.lock"), 0, overall)
            .unwrap()
            .expect("test holds the exec lock");
        let proof_calls = Arc::new(Mutex::new(0usize));
        let ancestor_calls = Arc::new(Mutex::new(0usize));
        let live_head = Arc::new(Mutex::new(head.clone()));
        let policy = CountingPolicy {
            proof_calls: proof_calls.clone(),
            ancestor_calls: ancestor_calls.clone(),
            live_head: live_head.clone(),
        };
        let spec2 = spec.clone();
        let waiter = std::thread::spawn(move || {
            let build = |_: u64| -> Result<BuildOutput> {
                panic!("waiter must attach, never build while the exec lock is held")
            };
            run_push_with(&spec, build, &policy, 0, 20)
        });
        // The initial examination probes once; several more polls on the
        // unchanged receipt must not re-probe.
        let start = Instant::now();
        while *proof_calls.lock().unwrap() == 0 {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "waiter never ran its initial examination"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(350));
        assert_eq!(
            *proof_calls.lock().unwrap(),
            1,
            "unchanged stale receipt must not be re-probed per poll"
        );
        assert_eq!(
            *ancestor_calls.lock().unwrap(),
            1,
            "unchanged stale ancestry must not repeat per poll"
        );
        // A new covering receipt is evaluated exactly once, then attached.
        *live_head.lock().unwrap() = trigger.sha.clone();
        let mut covering = receipt_for(consumer_trigger(&trigger.sha), "pass", &[]);
        covering.generation = 8;
        covering.admitted = 8;
        store_test_receipt(&spec2, &covering);
        match waiter.join().expect("waiter thread panicked") {
            Ok(PushOutcome::Attached { generation }) => assert_eq!(generation, 8),
            other => panic!("waiter must attach to the new receipt, got {other:?}"),
        }
        assert_eq!(
            *proof_calls.lock().unwrap(),
            2,
            "new receipt must be evaluated exactly once"
        );
        assert_eq!(
            *ancestor_calls.lock().unwrap(),
            1,
            "exact-match attach needs no ancestry"
        );
    }

    #[test]
    fn notify_lock_contention_respects_caller_deadline() {
        // A caller with 300ms left must not invent a fresh 60s lock wait:
        // contention fails fast with a deadline error, never a long sleep.
        let (_guard, spec) = setup(consumer_trigger(&"e".repeat(40)));
        let dir = namespace_dir(
            &spec.cache_root,
            &spec.consumer_url,
            &spec.consumer_branch,
            &spec.job,
        )
        .unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let overall = Instant::now() + Duration::from_secs(30);
        let _held = try_lock_file(&dir.join("notify.lock"), 0, overall)
            .unwrap()
            .expect("test holds the notify lock");
        let start = Instant::now();
        let short = Instant::now() + Duration::from_millis(300);
        let result = notify_and_settle(&dir, &spec.trigger, 0, 0, short);
        assert!(
            result.is_err(),
            "contended lock must fail fast past a short deadline"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "short deadline must not sleep (took {:?})",
            start.elapsed()
        );
    }

    #[test]
    fn failed_before_graph_propagates_to_admitted_dep_events() {
        // The resolver failed before recording any graph (empty deps):
        // both admitted dependency events must receive the recorded
        // failure with exactly one build total — no second build, and
        // never a success.
        let head = "c".repeat(40);
        let first = dep_trigger("deplib", &"d".repeat(40));
        let second = dep_trigger("otherlib", &"e".repeat(40));
        let (_guard, spec1) = setup(first.clone());
        let dir = namespace_dir(
            &spec1.cache_root,
            &spec1.consumer_url,
            &spec1.consumer_branch,
            &spec1.job,
        )
        .unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        // Admit the second event before the only build runs, so both share
        // one frozen admitted set.
        let generous = Instant::now() + Duration::from_secs(30);
        notify_and_settle(&dir, &second, 0, 0, generous).unwrap();
        let builds = Arc::new(Mutex::new(0usize));
        let builds_clone = builds.clone();
        let policy = fake_policy_live(&head);
        let fail_once = |_: u64| -> Result<BuildOutput> {
            *builds_clone.lock().unwrap() += 1;
            Ok(output_for(body(first.clone(), "fail", &[])))
        };
        assert!(
            run_push_with(&spec1, fail_once, &policy, 0, 10).is_err(),
            "failed build must report failure"
        );
        assert_eq!(*builds.lock().unwrap(), 1, "exactly one failed build");
        // The second admitted event propagates the same recorded failure.
        let mut spec2 = spec1.clone();
        spec2.trigger = second.clone();
        let rebuild = |_: u64| -> Result<BuildOutput> {
            panic!("admitted failure must propagate, never rebuild")
        };
        assert!(
            run_push_with(&spec2, rebuild, &policy, 0, 10).is_err(),
            "admitted failure must propagate as failure"
        );
        assert_eq!(
            *builds.lock().unwrap(),
            1,
            "no second build for an admitted failure"
        );
    }

    #[test]
    fn ancestry_proves_superseded_commits_offline() {
        let (_repo, old, new) = fixture_repo();
        let dir = tempfile::tempdir().unwrap();
        assert!(is_ancestor_in(dir.path(), &new, &new));
        let (_repo2, old2, new2) = fixture_repo();
        assert_ne!(old2, new2);
        let _ = (old, new);
    }

    #[test]
    fn tool_mismatch_never_falls_through_to_ancestry() {
        let (_fixture, old, new) = fixture_repo();
        let (_guard, spec) = setup(Trigger {
            kind: TriggerKind::Consumer,
            repo: URL.into(),
            branch: "v01".into(),
            sha: old.clone(),
            name: None,
        });
        let mut receipt = receipt_for(
            Trigger {
                kind: TriggerKind::Consumer,
                repo: URL.into(),
                branch: "v01".into(),
                sha: new.clone(),
                name: None,
            },
            "pass",
            &[],
        );
        receipt.consumer_commit = new.clone();
        receipt.config_identity = config_for(&new, "fast");
        receipt.tool_revision = "old-tool".into();
        receipt.runtime_identity = "old-runtime".into();
        store_test_receipt(&spec, &receipt);
        let probed = Arc::new(Mutex::new(false));
        let probed_clone = probed.clone();
        let policy = FakePolicy {
            runtime_identity: "new-runtime".into(),
            live_head: Some(new.clone()),
            ancestry: Arc::new(move |_, _, _, _| {
                *probed_clone.lock().unwrap() = true;
                true
            }),
        };
        let calls = Arc::new(Mutex::new(0u32));
        let build = |_: u64| {
            *calls.lock().unwrap() += 1;
            Ok(output_for(body(spec.trigger.clone(), "pass", &[])))
        };
        run_push_with(&spec, build, &policy, 0, 2).unwrap();
        assert_eq!(*calls.lock().unwrap(), 1, "tool mismatch must build");
        assert!(
            !*probed.lock().unwrap(),
            "identity mismatch must not probe ancestry"
        );
    }

    #[test]
    fn runtime_change_invalidates_exact_match() {
        let sha = "a".repeat(40);
        let (_guard, spec) = setup(consumer_trigger(&sha));
        let receipt = receipt_for(spec.trigger.clone(), "pass", &[]);
        store_test_receipt(&spec, &receipt);
        // Same live head, but the worker toolchain changed: rebuild.
        let changed_runtime = FakePolicy {
            runtime_identity: "changed-runtime".into(),
            live_head: Some(sha),
            ancestry: Arc::new(|_, _, old, new| old == new),
        };
        let calls = Arc::new(Mutex::new(0u32));
        let build = |_: u64| {
            *calls.lock().unwrap() += 1;
            Ok(output_for(body(spec.trigger.clone(), "pass", &[])))
        };
        run_push_with(&spec, build, &changed_runtime, 0, 2).unwrap();
        assert_eq!(*calls.lock().unwrap(), 1, "runtime change must rebuild");
    }

    #[test]
    fn stale_live_head_rejects_consumer_exact_and_ancestor() {
        // Receipt proves head C1; live moved to C2 (new manifest/source).
        // Both the exact retrigger of C1 and an ancestor C0 of C1 must
        // rebuild at the new head instead of attaching to stale proof.
        let (_fixture, old, new) = fixture_repo();
        let live = "e".repeat(40);
        let (_guard, spec) = setup(Trigger {
            kind: TriggerKind::Consumer,
            repo: URL.into(),
            branch: "v01".into(),
            sha: new.clone(),
            name: None,
        });
        let mut receipt = receipt_for(
            Trigger {
                kind: TriggerKind::Consumer,
                repo: URL.into(),
                branch: "v01".into(),
                sha: new.clone(),
                name: None,
            },
            "pass",
            &[],
        );
        receipt.consumer_commit = new.clone();
        receipt.config_identity = config_for(&new, "fast");
        store_test_receipt(&spec, &receipt);
        let policy = FakePolicy {
            runtime_identity: "test-runtime-v1".into(),
            live_head: Some(live.clone()),
            ancestry: {
                let (old_head, new_head) = (old.clone(), new.clone());
                Arc::new(move |_, _, o, n| o == n || (o == old_head && n == new_head))
            },
        };
        let calls = Arc::new(Mutex::new(0u32));
        let build = |_: u64| {
            *calls.lock().unwrap() += 1;
            Ok(output_for(body(spec.trigger.clone(), "pass", &[])))
        };
        // Exact stale retrigger rebuilds.
        run_push_with(&spec, build, &policy, 0, 2).unwrap();
        assert_eq!(*calls.lock().unwrap(), 1);
        // Ancestor of the stale head rebuilds too, despite proven ancestry.
        let ancestor = PushSpec {
            trigger: Trigger {
                kind: TriggerKind::Consumer,
                repo: URL.into(),
                branch: "v01".into(),
                sha: old.clone(),
                name: None,
            },
            ..spec.clone()
        };
        let build_ancestor = |_: u64| {
            *calls.lock().unwrap() += 1;
            Ok(output_for(body(ancestor.trigger.clone(), "pass", &[])))
        };
        run_push_with(&ancestor, build_ancestor, &policy, 0, 2).unwrap();
        assert_eq!(*calls.lock().unwrap(), 2, "stale ancestor must rebuild");
    }

    #[test]
    fn wrong_source_or_branch_rejects_before_ancestry() {
        let sha = "d".repeat(40);
        let (_guard, spec) = setup(dep_trigger("deplib", &sha));
        let receipt = receipt_for(spec.trigger.clone(), "pass", &[("deplib", sha)]);
        store_test_receipt(&spec, &receipt);
        let wrong_sha = "d".repeat(40);
        let wrong_repo = PushSpec {
            trigger: Trigger {
                kind: TriggerKind::Dependency,
                repo: "https://forge.example.invalid/other/wrong.git".into(),
                branch: "v01".into(),
                sha: wrong_sha,
                name: Some("deplib".into()),
            },
            ..spec.clone()
        };
        let probed = Arc::new(Mutex::new(false));
        let probed_clone = probed.clone();
        let policy = FakePolicy {
            runtime_identity: "test-runtime-v1".into(),
            live_head: Some("c".repeat(40)),
            ancestry: Arc::new(move |_, _, _, _| {
                *probed_clone.lock().unwrap() = true;
                true
            }),
        };
        let build = |_: u64| Ok(output_for(body(wrong_repo.trigger.clone(), "pass", &[])));
        assert!(validate_spec(&wrong_repo).is_ok());
        run_push_with(&wrong_repo, build, &policy, 0, 2).unwrap();
        assert!(
            !*probed.lock().unwrap(),
            "wrong source must not probe ancestry"
        );
        // Recorded per-source branch is v01; an event on another branch for
        // the same package and commit must rebuild.
        let wrong_branch = PushSpec {
            trigger: Trigger {
                kind: TriggerKind::Dependency,
                repo: DEP_URL.into(),
                branch: "other".into(),
                sha: "d".repeat(40),
                name: Some("deplib".into()),
            },
            ..spec.clone()
        };
        let build = |_: u64| Ok(output_for(body(wrong_branch.trigger.clone(), "pass", &[])));
        let policy = fake_policy_live(&"c".repeat(40));
        run_push_with(&wrong_branch, build, &policy, 0, 2).unwrap();
    }

    #[test]
    fn dependency_requires_live_consumer_head_proof() {
        let dep_sha = "d".repeat(40);
        let (_guard, spec) = setup(dep_trigger("deplib", &dep_sha));
        let receipt = receipt_for(spec.trigger.clone(), "pass", &[("deplib", dep_sha)]);
        store_test_receipt(&spec, &receipt);
        // Fresh live head first: the stored receipt is still newest, so a
        // matching head attaches to it instead of rebuilding.
        let fresh_policy = fake_policy_live(&"c".repeat(40));
        let build = |_: u64| -> Result<BuildOutput> { panic!("must attach") };
        match run_push_with(&spec, build, &fresh_policy, 0, 2).unwrap() {
            PushOutcome::Attached { generation } => assert_eq!(generation, 7),
            PushOutcome::Built { .. } => panic!("must attach, not build"),
        }
        // Stale live head after that: the receipt no longer proves the
        // consumer configuration, so the same trigger rebuilds.
        let stale_policy = FakePolicy {
            runtime_identity: "test-runtime-v1".into(),
            live_head: Some("e".repeat(40)),
            ancestry: Arc::new(|_, _, old, new| old == new),
        };
        let calls = Arc::new(Mutex::new(0u32));
        let build = |_: u64| {
            *calls.lock().unwrap() += 1;
            Ok(output_for(body(spec.trigger.clone(), "pass", &[])))
        };
        run_push_with(&spec, build, &stale_policy, 0, 2).unwrap();
        assert_eq!(*calls.lock().unwrap(), 1, "stale consumer must rebuild");
    }

    #[test]
    fn receipts_are_never_pruned_and_deadline_bounds_settle() {
        let (_guard, spec) = setup(consumer_trigger(&"a".repeat(40)));
        let policy_a = fake_policy_live(&"a".repeat(40));
        let build = |_: u64| Ok(output_for(body(spec.trigger.clone(), "pass", &[])));
        run_push_with(&spec, build, &policy_a, 0, 1).unwrap();
        let other = PushSpec {
            trigger: consumer_trigger(&"b".repeat(40)),
            ..spec.clone()
        };
        let policy_b = fake_policy_live(&"b".repeat(40));
        let build_other = |_: u64| Ok(output_for(body(other.trigger.clone(), "pass", &[])));
        run_push_with(&other, build_other, &policy_b, 0, 1).unwrap();
        let dir = namespace_dir(
            &spec.cache_root,
            &spec.consumer_url,
            &spec.consumer_branch,
            &spec.job,
        )
        .unwrap();
        let receipts: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("receipt-"))
            .collect();
        assert!(
            receipts.len() >= 2,
            "both receipts retained, no pruning: {receipts:?}"
        );
        let tight = PushSpec {
            timeout_secs: 1,
            ..spec.clone()
        };
        let started = Instant::now();
        assert!(run_push_with(&tight, build, &policy_a, 10, 30).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "overall deadline bounds debounce"
        );
    }

    #[test]
    fn ls_remote_parsing_requires_the_exact_ref() {
        let sha = "a".repeat(40);
        let other = "b".repeat(40);
        // Exact single ref accepted.
        assert_eq!(
            parse_ls_remote_exact(&format!("{sha}\trefs/heads/v01\n"), "v01").unwrap(),
            sha
        );
        // First arbitrary line for another ref must not win.
        assert!(
            parse_ls_remote_exact(
                &format!("{other}\trefs/heads/other\n{sha}\trefs/heads/v01\n"),
                "v01"
            )
            .unwrap()
                == sha
        );
        // No matching ref, multiple matches, or a bad sha all fail closed.
        assert!(parse_ls_remote_exact(&format!("{other}\trefs/heads/other\n"), "v01").is_err());
        assert!(parse_ls_remote_exact(
            &format!("{sha}\trefs/heads/v01\n{sha}\trefs/heads/v01\n"),
            "v01"
        )
        .is_err());
        assert!(parse_ls_remote_exact("short\trefs/heads/v01\n", "v01").is_err());
        assert!(parse_ls_remote_exact("", "v01").is_err());
    }

    #[test]
    fn runtime_stable_helper_fails_closed_on_drift() {
        require_runtime_unchanged("r1", "r1").unwrap();
        assert!(require_runtime_unchanged("r1", "r2").is_err());
        assert!(require_runtime_unchanged("", "r1").is_err());
        assert!(require_runtime_unchanged("r1", "").is_err());
    }

    #[test]
    fn dep_branches_come_from_selected_source_branches() {
        // The receipt branch for each dependency is the actually selected
        // source branch from the GraphSel parser, never empty on success.
        let selected = vec![
            GraphSel {
                package: "keep".into(),
                version: "0.1.0".into(),
                canonical_url: "forge.example.invalid/cpkg/keep".into(),
                git_ref: "v01".into(),
            },
            GraphSel {
                package: "hidden".into(),
                version: "0.2.0".into(),
                canonical_url: "forge.example.invalid/cpkg/hidden".into(),
                git_ref: "v01".into(),
            },
        ];
        let branches = dep_branches_from(&selected);
        assert_eq!(branches.len(), 2);
        assert_eq!(branches.get("keep").map(String::as_str), Some("v01"));
        assert_eq!(branches.get("hidden").map(String::as_str), Some("v01"));
        for branch in branches.values() {
            assert!(!branch.is_empty());
        }
        // No selection means no branches; build_inner refuses this before
        // any receipt, so empty-branch receipts can never attach.
        assert!(dep_branches_from(&[]).is_empty());
    }

    #[test]
    fn gate_execution_env_preserves_declared_compile_overrides() {
        // The runtime fingerprint is captured with exactly this environment,
        // so a declared compile override must survive into it (parity by
        // construction, no separate ambient capture).
        let work = tempfile::tempdir().unwrap();
        let ns = tempfile::tempdir().unwrap();
        let mut environment: crate::Environment = BTreeMap::new();
        let mut declared = BTreeMap::new();
        declared.insert("RUSTFLAGS".to_owned(), "--cfg test-override".to_owned());
        crate::jobs::apply_job_environment(&mut environment, &declared);
        let planned = PlannedBuild {
            workdir: work.path().into(),
            ns_dir: ns.path().into(),
            head: "a".repeat(40),
            manifest_sha: "manifest".into(),
            tool_revision: crate::SOURCE_REVISION.into(),
            checks: vec!["fast".into()],
            command: vec![crate::jobs::SELF_CHECKS_SENTINEL.into()],
            refresh: Refresh {
                branch: "v01".into(),
                sources: Vec::new(),
                prepare_commands: Vec::new(),
            },
            environment,
            deadline: Instant::now() + Duration::from_secs(60),
        };
        let gate_env = gate_execution_env(&planned, Path::new("/tmp/ccid-test-target")).unwrap();
        assert_eq!(
            gate_env
                .get(&std::ffi::OsString::from("RUSTFLAGS"))
                .map(|value| value.to_string_lossy().into_owned()),
            Some("--cfg test-override".into())
        );
        // Reserved identity keys can never be smuggled through the manifest.
        let mut reserved = BTreeMap::new();
        reserved.insert("CCID_BIN".to_owned(), "/tmp/evil".to_owned());
        assert!(crate::jobs::validate_job_environment(&reserved).is_err());
    }

    #[test]
    fn attach_environment_reads_committed_manifest_not_dirty_filesystem() {
        // Owned-store fixture: a consumer repo with a committed manifest,
        // cloned into the namespace workdir with the consumer origin bound.
        // All git operations are local paths; no network.
        let remote = tempfile::tempdir().unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(remote.path(), &["init", "-q", "-b", "v01"]);
        git(remote.path(), &["config", "user.name", "Fixture"]);
        git(
            remote.path(),
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::create_dir_all(remote.path().join(".ci")).unwrap();
        std::fs::write(
            remote.path().join(".ci/ccid.toml"),
            "[jobs.fast]\nchecks = []\nworkflow = 'verify'\ncommand = ['ccid:run-declared-checks']\n\
             environment = { RUSTFLAGS = '--cfg committed-flag' }\n",
        )
        .unwrap();
        git(remote.path(), &["add", "--all"]);
        git(
            remote.path(),
            &["-c", "commit.gpgsign=false", "commit", "-qm", "one"],
        );
        let head = git(remote.path(), &["rev-parse", "HEAD"]);
        assert!(valid_sha(&head));

        let cache = tempfile::tempdir().unwrap();
        let consumer_url = "https://forge.example.invalid/cpkg/demo.git";
        let spec = PushSpec {
            consumer_url: consumer_url.into(),
            consumer_branch: "v01".into(),
            job: "fast".into(),
            trigger: consumer_trigger(&head),
            cache_root: cache.path().into(),
            timeout_secs: 120,
        };
        let dir = namespace_dir(
            &spec.cache_root,
            &spec.consumer_url,
            &spec.consumer_branch,
            &spec.job,
        )
        .unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let owner = serde_json::json!({
            "consumer_url": crate::cache::canonical_repository(&spec.consumer_url).unwrap(),
            "consumer_branch": spec.consumer_branch,
            "job": spec.job,
        });
        std::fs::write(
            dir.join("owner.json"),
            serde_json::to_string(&owner).unwrap(),
        )
        .unwrap();
        let work = dir.join("work");
        git(
            cache.path(),
            &[
                "clone",
                "-q",
                remote.path().to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        git(&work, &["remote", "set-url", "origin", consumer_url]);
        let deadline = Instant::now() + Duration::from_secs(60);

        // Committed environment wins.
        let effective = production_effective_environment(&spec, &head, deadline).unwrap();
        assert_eq!(
            effective
                .get(&std::ffi::OsString::from("RUSTFLAGS"))
                .map(|value| value.to_string_lossy().into_owned()),
            Some("--cfg committed-flag".into())
        );

        // A dirty filesystem manifest is ignored: the committed blob wins.
        std::fs::write(
            work.join(".ci/ccid.toml"),
            "[jobs.fast]\nchecks = []\nworkflow = 'verify'\ncommand = ['ccid:run-declared-checks']\n\
             environment = { RUSTFLAGS = '--cfg dirty-flag' }\n",
        )
        .unwrap();
        let effective = production_effective_environment(&spec, &head, deadline).unwrap();
        assert_eq!(
            effective
                .get(&std::ffi::OsString::from("RUSTFLAGS"))
                .map(|value| value.to_string_lossy().into_owned()),
            Some("--cfg committed-flag".into())
        );

        // Reserved identity keys in the committed manifest fail closed.
        std::fs::write(
            remote.path().join(".ci/ccid.toml"),
            "[jobs.fast]\nchecks = []\nworkflow = 'verify'\ncommand = ['ccid:run-declared-checks']\n\
             environment = { CCID_BIN = '/tmp/evil' }\n",
        )
        .unwrap();
        git(remote.path(), &["add", "--all"]);
        git(
            remote.path(),
            &["-c", "commit.gpgsign=false", "commit", "-qm", "two"],
        );
        git(
            &work,
            &["fetch", "-q", remote.path().to_str().unwrap(), "v01"],
        );
        let bad_head = git(&work, &["rev-parse", "FETCH_HEAD"]);
        assert!(production_effective_environment(&spec, &bad_head, deadline).is_err());

        // No owned store at all fails closed (caller builds instead).
        let missing = PushSpec {
            job: "other".into(),
            ..spec.clone()
        };
        assert!(production_effective_environment(&missing, &head, deadline).is_err());
    }

    #[test]
    fn delayed_trigger_attaches_to_covering_receipt() {
        let (_guard, spec) = setup(dep_trigger("deplib", &"d".repeat(40)));
        let trigger = spec.trigger.clone();
        let receipt = receipt_for(trigger.clone(), "pass", &[("deplib", "d".repeat(40))]);
        store_test_receipt(&spec, &receipt);
        let policy = fake_policy_live(&"c".repeat(40));
        let build = |_: u64| -> Result<BuildOutput> { panic!("covered trigger must not build") };
        match run_push_with(&spec, build, &policy, 0, 1).unwrap() {
            PushOutcome::Attached { generation } => assert_eq!(generation, 7),
            PushOutcome::Built { .. } => panic!("must attach, not build"),
        }
        // Same-sha retrigger of a failed receipt propagates without building.
        let failed = receipt_for(trigger.clone(), "fail", &[("deplib", "d".repeat(40))]);
        store_test_receipt(&spec, &failed);
        let build = |_: u64| -> Result<BuildOutput> {
            panic!("failed receipt must propagate, not rebuild")
        };
        assert!(run_push_with(&spec, build, &policy, 0, 1).is_err());
    }
}

// Graph-execution tests live separately to avoid merge conflicts with the
// coalescer worker's inline tests. They cover the transitive v01 selection,
// frozen offline gates, and manifest validation owned by this half.
#[path = "push_graph_tests.rs"]
#[cfg(test)]
mod push_graph_tests;

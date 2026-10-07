//! Hidden compiler-wrapper diagnostic for one frozen library build.
//!
//! Cargo invokes this binary as `RUSTC_WORKSPACE_WRAPPER` (selected by the
//! `CCID_RUSTC_DIAG` environment marker) with the real compiler path as
//! the first argument followed by rustc arguments. Only the exact cmsh
//! library unit is instrumented: crate name `cmsh`, manifest beneath the
//! job-owned frozen-source root, and `src/tor_discovery.rs` matching its
//! committed digest. Every other invocation passes through to the real
//! compiler (or a composed outer wrapper) untouched.
//!
//! The transform inserts static-stage `eprintln!` diagnostics (error enum
//! variant or a transport error's static kind and context only, never peer IDs,
//! paths, keys or payloads) at exact anchor sites, compiles, then restores
//! the original bytes and verifies the restore — including on compiler
//! failure. Original sources stay verified; the receipt records
//! original/diagnostic digests, the transformation list, and the
//! compiler/status outcome. Unix only: the owning jobs run
//! on Linux workers.
use crate::{failure, Result};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

/// Environment marker selecting wrapper mode (set by the owning job only).
/// Public because the binary entry point reads it before CLI parsing.
pub const MODE_ENV: &str = "CCID_RUSTC_DIAG";
/// Job-owned frozen-source root; the compiled manifest must sit beneath it.
pub(crate) const ROOT_ENV: &str = "CCID_DIAG_ROOT";
/// Committed SHA-256 of the exact frozen `src/tor_discovery.rs`.
pub(crate) const ORIGINAL_ENV: &str = "CCID_DIAG_ORIGINAL_SHA256";
/// Receipt JSON path inside job-owned evidence.
pub(crate) const RECEIPT_ENV: &str = "CCID_DIAG_RECEIPT";
/// Crate selected for instrumentation. Nothing else is ever touched.
pub(crate) const TARGET_CRATE: &str = "cmsh";
/// Instrumented file relative to the crate manifest directory.
pub(crate) const TARGET_FILE: &str = "src/tor_discovery.rs";
/// Lock sibling bounding concurrent transforms of one source file.
pub(crate) const LOCK_SUFFIX: &str = ".ccid-diag.lock";

/// One instrumentation site: exact anchor text must occur exactly once;
/// the replacement preserves return/control flow and only adds a static
/// stage diagnostic. Error values printed are the coarse unit-only enum.
struct Site {
    name: &'static str,
    anchor: &'static str,
    replacement: &'static str,
}

fn sites() -> Vec<Site> {
    vec![
        Site {
            name: "discovery RPC success",
            anchor: r#"                    Ok(peers) => {
                        search.finish(&id, true)?;"#,
            replacement: r#"                    Ok(peers) => {
                        eprintln!("ccid-diag discovery-rpc-ok: elapsed_ms={}", self.clock.now().saturating_sub(start));
                        search.finish(&id, true)?;"#,
        },
        Site {
            name: "discovery RPC failure",
            anchor: r#"                    Err(_) => {
                        search.finish(&id, false)?;"#,
            replacement: r#"                    Err(error) => {
                        eprintln!("ccid-diag discovery-rpc-error: {error:?}; elapsed_ms={}", self.clock.now().saturating_sub(start));
                        search.finish(&id, false)?;"#,
        },
        Site {
            name: "discovery outcome",
            anchor: r#"        Ok(TorDiscoveryReport {
            peers: search.storage_peers(),"#,
            replacement: r#"        eprintln!("ccid-diag discovery-result: peers={} expired={} exhausted={} limited={} elapsed_ms={}", search.storage_peers().len(), expired, search.exhausted(), search.limited(), self.clock.now().saturating_sub(start));
        Ok(TorDiscoveryReport {
            peers: search.storage_peers(),"#,
        },
        // Every FindNode, record and watch call shares this transport site.
        // ctrn errors are a coarse kind plus a compile-time context string by
        // construction (no addresses, keys, payloads or upstream text), so
        // printing one separates local resource limits, onion connection
        // failures and stream failures behind the single `Unavailable`.
        Site {
            name: "transport failure",
            anchor: r#"            .app_call(&checked.endpoint, outgoing.as_bytes())
            .await
            .map_err(|_| Error::Unavailable)?;"#,
            replacement: r#"            .app_call(&checked.endpoint, outgoing.as_bytes())
            .await
            .map_err(|error| {
                eprintln!("ccid-diag transport-error: {error}");
                Error::Unavailable
            })?;"#,
        },
    ]
}

/// Apply every site exactly once. Any missing or duplicated anchor fails
/// closed so a changed source is never compiled half-instrumented.
pub(crate) fn transform(original: &str) -> Result<String> {
    let mut text = original.to_owned();
    for site in sites() {
        if text.matches(site.anchor).count() != 1 {
            return Err(failure(format!(
                "Compiler diagnostic patch site changed: {}",
                site.name
            )));
        }
        text = text.replacen(site.anchor, site.replacement, 1);
    }
    Ok(text)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn is_self(path: &Path) -> bool {
    let current = std::env::current_exe().ok();
    let target = path.canonicalize().ok();
    match (current, target) {
        (Some(current), Some(target)) => current == target,
        _ => false,
    }
}

/// Collect existing `.rs` input files from rustc argv, resolving relative
/// paths against the process working directory. Flags, `--extern` rlibs,
/// `-L` directories and `--cfg` values never qualify: only real input files
/// do, so exactly one must remain for a library unit.
fn rustc_inputs(args: &[OsString]) -> Result<Vec<PathBuf>> {
    let cwd = std::env::current_dir()
        .map_err(|_| failure("Compiler diagnostic cannot read working directory"))?;
    let mut inputs = Vec::new();
    for arg in args {
        let text = arg.to_string_lossy();
        if text.starts_with('-') || !text.ends_with(".rs") {
            continue;
        }
        let candidate = PathBuf::from(text.as_ref());
        let absolute = if candidate.is_absolute() {
            candidate
        } else {
            cwd.join(candidate)
        };
        if absolute.is_file() {
            inputs.push(absolute);
        }
    }
    Ok(inputs)
}

/// Decide whether this invocation is the instrumented target and, if so,
/// resolve its canonical paths. Anything but the exact crate passes
/// through before touching paths; a claimed target with bad config or
/// paths fails instead of passing through silently.
/// Decide whether this invocation is the instrumented target and, if so,
/// resolve its canonical source path: crate name matches, the manifest
/// sits beneath the job root, the actual rustc input is exactly the
/// crate-root `src/lib.rs`, and the target file stays beneath the manifest.
/// Anything but the exact crate passes through before touching paths; a
/// claimed target with bad config or paths fails instead of passing
/// through silently.
fn resolve_target(
    crate_name: Option<&str>,
    manifest_dir: Option<&Path>,
    args: &[OsString],
    root: &Path,
) -> Result<Option<PathBuf>> {
    if crate_name != Some(TARGET_CRATE) {
        return Ok(None);
    }
    let Some(manifest) = manifest_dir else {
        return Err(failure(
            "Compiler diagnostic target crate has no manifest directory",
        ));
    };
    let root = root
        .canonicalize()
        .map_err(|_| failure("Compiler diagnostic cannot resolve frozen-source root"))?;
    let manifest = manifest
        .canonicalize()
        .map_err(|_| failure("Compiler diagnostic cannot resolve manifest directory"))?;
    if !manifest.starts_with(&root) {
        return Err(failure(
            "Compiler diagnostic manifest escapes the frozen-source root",
        ));
    }
    let lib = manifest.join("src/lib.rs");
    if !lib.is_file() {
        return Err(failure(
            "Compiler diagnostic manifest has no src/lib.rs input",
        ));
    }
    let lib = lib
        .canonicalize()
        .map_err(|_| failure("Compiler diagnostic cannot resolve crate input"))?;
    if !lib.starts_with(&manifest) {
        return Err(failure(
            "Compiler diagnostic crate input escapes the manifest directory",
        ));
    }
    let mut inputs = Vec::new();
    for input in rustc_inputs(args)? {
        inputs.push(
            input
                .canonicalize()
                .map_err(|_| failure("Compiler diagnostic cannot resolve rustc input"))?,
        );
    }
    if inputs.len() != 1 {
        return Err(failure(
            "Compiler diagnostic rustc input is not a single library file",
        ));
    }
    if inputs[0] != lib {
        return Err(failure(
            "Compiler diagnostic rustc input is not the crate root",
        ));
    }
    let source = manifest.join(TARGET_FILE);
    let source = source
        .canonicalize()
        .map_err(|_| failure("Compiler diagnostic cannot resolve instrumented source"))?;
    if !source.starts_with(&manifest) {
        return Err(failure(
            "Compiler diagnostic source escapes the manifest directory",
        ));
    }
    Ok(Some(source))
}

fn crate_name_of(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--crate-name" {
            return iter.next().cloned();
        }
        if let Some(name) = arg.strip_prefix("--crate-name=") {
            return Some(name.to_owned());
        }
    }
    None
}

/// Scoped exclusive lock for one source file, released (file removed) on
/// drop. The canonical bytes travel with the guard so unwinding paths
/// still attempt the restore; verification and error reporting stay with
/// the explicit path below.
struct SourceLock {
    path: PathBuf,
    source: PathBuf,
    original: Option<Vec<u8>>,
}

fn acquire_lock(source: &Path) -> Result<SourceLock> {
    let mut path = source.as_os_str().to_owned();
    path.push(LOCK_SUFFIX);
    let path = PathBuf::from(path);
    for _ in 0..100 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                return Ok(SourceLock {
                    path,
                    source: source.to_owned(),
                    original: None,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => {
                return Err(failure(format!(
                    "Compiler diagnostic cannot lock source: {error}"
                )));
            }
        }
    }
    Err(failure(
        "Compiler diagnostic lock is held by another compilation",
    ))
}

impl SourceLock {
    /// Arm the guard with the canonical bytes once they are verified, so
    /// even an unwinding path below attempts the restore.
    fn arm(&mut self, original: Vec<u8>) {
        self.original = Some(original);
    }

    fn disarm(&mut self) {
        self.original = None;
    }
}

impl Drop for SourceLock {
    fn drop(&mut self) {
        if let Some(original) = self.original.take() {
            let _ = fs::write(&self.source, original);
        }
        let _ = fs::remove_file(&self.path);
    }
}

fn write_receipt(path: &Path, value: serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|_| {
                failure(format!(
                    "Compiler diagnostic cannot create receipt directory: {}",
                    parent.display()
                ))
            })?;
        }
    }
    // Append-only (JSONL): every unit firing leaves a complete record under
    // the same scoped lock instead of overwriting a previous firing.
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|_| {
            failure(format!(
                "Compiler diagnostic cannot write receipt: {}",
                path.display()
            ))
        })?;
    writeln!(
        file,
        "{}",
        serde_json::to_string(&value).unwrap_or_default()
    )
    .map_err(|_| {
        failure(format!(
            "Compiler diagnostic cannot write receipt: {}",
            path.display()
        ))
    })
}

struct DiagConfig {
    root: PathBuf,
    original_sha256: String,
    receipt: PathBuf,
}

fn config_from_env() -> Result<DiagConfig> {
    config_from_map(&|key| std::env::var_os(key))
}

/// Load mandatory wrapper configuration from an explicit lookup, so unit
/// tests exercise every branch without touching process-global state.
/// In wrapper mode all three values are mandatory: an unconfigured
/// invocation fails instead of silently qualifying uninstrumented code.
fn config_from_map(get: &dyn Fn(&str) -> Option<OsString>) -> Result<DiagConfig> {
    let root = match get(ROOT_ENV) {
        Some(root) if !root.is_empty() => PathBuf::from(root),
        _ => {
            return Err(failure(
                "Compiler diagnostic needs CCID_DIAG_ROOT in wrapper mode",
            ));
        }
    };
    let original_sha256 = match get(ORIGINAL_ENV).and_then(|value| value.into_string().ok()) {
        Some(value) if !value.is_empty() => value,
        _ => {
            return Err(failure(
                "Compiler diagnostic needs CCID_DIAG_ORIGINAL_SHA256 in wrapper mode",
            ));
        }
    };
    let receipt = match get(RECEIPT_ENV) {
        Some(receipt) if !receipt.is_empty() => PathBuf::from(receipt),
        _ => {
            return Err(failure(
                "Compiler diagnostic needs CCID_DIAG_RECEIPT in wrapper mode",
            ));
        }
    };
    Ok(DiagConfig {
        root,
        original_sha256,
        receipt,
    })
}

/// Cargo composes the outer cache wrapper natively. Invoke its supplied
/// compiler directly, refusing recursion into this executable.
fn resolve_compiler(compiler: &str) -> Result<Vec<OsString>> {
    if is_self(Path::new(compiler)) {
        return Err(failure(
            "Compiler diagnostic refers to itself; refusing to recurse",
        ));
    }
    Ok(vec![OsString::from(compiler)])
}

/// Entry point for wrapper mode: argv[0] is this binary, argv[1] is the
/// real compiler, the rest are rustc arguments. Returns the process exit
/// code for main to translate.
pub fn run_wrapped() -> Result<ExitCode> {
    let argv: Vec<OsString> = std::env::args_os().collect();
    if argv.len() < 2 {
        return Err(failure(
            "Compiler diagnostic wrapper needs a compiler argument",
        ));
    }
    let compiler = argv[1].to_string_lossy().into_owned();
    let rest: Vec<OsString> = argv[2..].to_vec();
    let passthrough = || -> Result<ExitCode> {
        // Non-target invocations run the real (or chained) compiler
        // unchanged and leave no receipt: silence, not spam.
        let mut command = resolve_compiler(&compiler)?;
        command.extend(rest.iter().cloned());
        let status = Command::new(&command[0])
            .args(&command[1..])
            .status()
            .map_err(|error| {
                failure(format!("Compiler diagnostic cannot run compiler: {error}"))
            })?;
        Ok(code_of(status.code()))
    };
    let config = config_from_env()?;
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
    let rest_strings: Vec<String> = rest
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let source = match resolve_target(
        crate_name_of(&rest_strings).as_deref(),
        manifest_dir.as_deref(),
        &rest,
        &config.root,
    )? {
        Some(source) => source,
        None => return passthrough(),
    };
    let mut lock = acquire_lock(&source)?;
    let original = fs::read(&source).map_err(|_| {
        failure(format!(
            "Compiler diagnostic cannot read source: {}",
            source.display()
        ))
    })?;
    if sha256_hex(&original) != config.original_sha256 {
        return Err(failure(
            "Compiler diagnostic source digest mismatch; refusing changed bytes",
        ));
    }
    let mut invocation = resolve_compiler(&compiler)?;
    invocation.extend(rest.iter().cloned());
    lock.arm(original.clone());
    let diagnostic = transform(
        std::str::from_utf8(&original)
            .map_err(|_| failure("Compiler diagnostic source is not valid UTF-8"))?,
    )?;
    let diagnostic_sha256 = sha256_hex(diagnostic.as_bytes());
    fs::write(&source, diagnostic.as_bytes()).map_err(|_| {
        failure(format!(
            "Compiler diagnostic cannot write source: {}",
            source.display()
        ))
    })?;
    let exit = execute_guarded(
        || run_compiler(&invocation),
        &source,
        &original,
        &config.original_sha256,
        &diagnostic_sha256,
        &config.receipt,
    )?;
    lock.disarm();
    Ok(code_of(Some(exit)))
}

/// Run one compiler invocation, translating a signal death into a job
/// failure rather than an unrepresentable status.
fn run_compiler(invocation: &[OsString]) -> Result<i32> {
    let status = Command::new(&invocation[0])
        .args(&invocation[1..])
        .status()
        .map_err(|error| failure(format!("Compiler diagnostic cannot run compiler: {error}")))?;
    status
        .code()
        .ok_or_else(|| failure("Compiler diagnostic compiler terminated by signal"))
}

/// Execute the compiler against the instrumented source with unconditional
/// verified restore and receipt: success, nonzero compiler exit, and spawn
/// failure all restore and record; only a failed restore or receipt masks
/// the compiler outcome. The `invoke` closure keeps this unit-testable
/// without spawning real compilers.
fn execute_guarded(
    invoke: impl FnOnce() -> Result<i32>,
    source: &Path,
    original: &[u8],
    expected_sha256: &str,
    diagnostic_sha256: &str,
    receipt_path: &Path,
) -> Result<i32> {
    let outcome = invoke();
    let restored = restore_source(source, original, expected_sha256).unwrap_or(false);
    let receipt = json!({
        "crate_name": TARGET_CRATE,
        "original_sha256": expected_sha256,
        "diagnostic_sha256": diagnostic_sha256,
        "transformations": sites().iter().map(|site| site.name).collect::<Vec<_>>(),
        "compiler_exit": outcome.as_ref().ok().copied(),
        "restored": restored,
    });
    write_receipt(receipt_path, receipt)?;
    if !restored {
        return Err(failure(
            "Compiler diagnostic could not restore original source bytes",
        ));
    }
    outcome
}

fn code_of(code: Option<i32>) -> ExitCode {
    match code {
        Some(code) => ExitCode::from(code as u8),
        None => ExitCode::from(2),
    }
}

/// Restore canonical bytes and verify the restore. Used after the
/// compiler returns, on success and on failure alike.
fn restore_source(path: &Path, original: &[u8], expected_sha256: &str) -> Result<bool> {
    fs::write(path, original).map_err(|_| {
        failure(format!(
            "Compiler diagnostic cannot restore source: {}",
            path.display()
        ))
    })?;
    Ok(fs::read(path)
        .map(|bytes| sha256_hex(&bytes) == expected_sha256)
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANCHORED: &str =
        "head\n        let received = self.discovery.authenticate(bytes.clone()).await?;\ntail\n";

    /// Each bounded discovery diagnostic appears exactly once.
    const EXPECTED_TAGS: [&str; 4] = [
        "ccid-diag discovery-rpc-ok",
        "ccid-diag discovery-rpc-error",
        "ccid-diag discovery-result",
        "ccid-diag transport-error",
    ];

    #[test]
    fn transform_rejects_missing_or_duplicated_anchors() {
        assert!(transform("no anchors here").is_err());
        let doubled = format!("{ANCHORED}{ANCHORED}");
        assert!(transform(&doubled).is_err());
    }

    /// Exact frozen library source used for transform verification.
    /// Candidate `src/tor_discovery.rs` (FSL product tree, frozen bundle input).
    const FROZEN_LIB: &str = r#"//! Original Veilid discovery over actual Tor complete-message calls. No fixed
//! population, unsigned key/onion map, operator record store or direct sockets.
use cdht::{
    Error, NodeId, Verifier,
    discovery::Discovery,
    rpc::{
        Answer, Question, SignedOperation, Statement, UnverifiedPeerInfoBytes,
        envelope::{AuthenticatedOperation, Envelope, TimeWindow},
        peers::{DHTV, FindAnswer, VerifiedExtensionPeer, find_node, find_query, find_response},
    },
};
use futures::{
    StreamExt,
    future::{Either, select},
    stream::FuturesUnordered,
};
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

/// Keys supplies a node-purpose signing owner. Implementations authorize, then
/// use their synchronous borrowed signer callback; no borrowed key crosses I/O.
pub trait TorNodeIdentity: crate::MaybeSend + crate::MaybeSync {
    /// This community-local node's public VLD0 coordinate.
    fn node_id(&self) -> NodeId;
    /// Sign precisely this original question for one authenticated destination.
    fn question<'a>(
        &'a self,
        question: &'a Question,
        destination: &'a NodeId,
    ) -> crate::BoxFuture<'a, Result<SignedOperation, Error>>;
    /// Sign this original FindNode answer for the authenticated requester.
    fn answer<'a>(
        &'a self,
        answer: &'a FindAnswer,
        destination: &'a NodeId,
    ) -> crate::BoxFuture<'a, Result<SignedOperation, Error>>;
    /// Sign an original record answer for the authenticated requester node.
    fn record_answer<'a>(
        &'a self,
        answer: &'a Answer,
        destination: &'a NodeId,
    ) -> crate::BoxFuture<'a, Result<SignedOperation, Error>>;
    /// Sign an original ValueChanged statement for the authenticated watch target.
    fn statement<'a>(
        &'a self,
        statement: &'a Statement,
        destination: &'a NodeId,
    ) -> crate::BoxFuture<'a, Result<SignedOperation, Error>>;
    /// Seal an already signed original operation with this node's original ENV0
    /// signature and DH, independently of the inner watcher/record signer.
    fn seal<'a>(
        &'a self,
        operation: &'a SignedOperation,
        destination: &'a NodeId,
        timestamp_us: u64,
    ) -> crate::BoxFuture<'a, Result<Vec<u8>, Error>>;
    /// Decrypt an authenticated envelope through the retained scoped node owner.
    fn open<'a>(
        &'a self,
        envelope: &'a Envelope,
    ) -> crate::BoxFuture<'a, Result<AuthenticatedOperation, Error>>;
}
/// Original anonymous or schema-member watch signer, independent of node identity.
/// Keys authorizes the exact record question and lends its signer synchronously.
pub trait TorWatcher: crate::MaybeSend + crate::MaybeSync {
    /// Public original watcher identity for this record capability.
    fn public_key(&self, key: &cdht::RecordKey) -> Result<NodeId, Error>;
    /// Sign only this record's original WatchValueQ for one authenticated node.
    fn question<'a>(
        &'a self,
        key: &'a cdht::RecordKey,
        question: &'a Question,
        destination: &'a NodeId,
    ) -> crate::BoxFuture<'a, Result<SignedOperation, Error>>;
}
/// Actual runtime wall clock for original ENV0 timestamp authentication. The Tor
/// monotonic Clock remains the separate source for operation deadlines.
pub trait TorEnvelopeTime: crate::MaybeSend + crate::MaybeSync {
    /// Current Unix time in microseconds. Failure refuses envelope acceptance.
    fn now_us(&self) -> Result<u64, Error>;
}
/// Resource limits for discovery only; no record expiry or replication policy.
#[derive(Clone, Copy)]
pub struct TorDiscoveryLimits {
    /// Maximum retained authenticated routing identities.
    pub peers: NonZeroUsize,
    /// Concurrent discovery RPCs. Native core's configured fanout is the reference.
    pub lanes: NonZeroUsize,
    /// Overall lookup deadline, in monotonic milliseconds.
    pub timeout_ms: u64,
}
/// Explicit construction inputs for one bounded routing owner.
pub struct TorDiscoveryConfig<I, V> {
    /// Complete-message port on the actual running Tor node.
    pub messages: Arc<ctrn::messages::Messages>,
    /// Scoped node signing owner.
    pub identity: I,
    /// Maintained strict VLD0 verifier.
    pub verifier: V,
    /// Original signed advertisement for the actual listener.
    pub own: UnverifiedPeerInfoBytes,
    /// Operator bootstrap hints, independently signed by each advertised node.
    pub bootstrap: Vec<UnverifiedPeerInfoBytes>,
    /// The same node's monotonic runtime clock.
    pub clock: Arc<dyn ctrn::Clock>,
    /// Caller-selected work bounds.
    pub limits: TorDiscoveryLimits,
    /// Actual runtime wall clock; request payloads never set acceptance time.
    pub envelope_time: Arc<dyn TorEnvelopeTime>,
    /// Explicit original envelope freshness limits.
    pub envelope_window: TimeWindow,
}
#[derive(Clone)]
struct Peer {
    info: Arc<VerifiedExtensionPeer>,
    endpoint: ctrn::Address,
    responsive: bool,
}
fn peer(info: VerifiedExtensionPeer) -> Result<Peer, Error> {
    let ad = info.advertisement();
    let mut endpoint = None;
    for dial in &ad.dial_info {
        let checked = ctrn::OnionEndpoint::from_node_info(
            dial.protocol,
            &ad.outbound_protocols,
            &ad.address_types,
            &dial.detail,
        )
        .map_err(|_| Error::Encoding)?;
        if endpoint.is_none() {
            endpoint = Some(checked.address());
        }
    }
    Ok(Peer {
        endpoint: endpoint.ok_or(Error::Encoding)?,
        info: Arc::new(info),
        responsive: false,
    })
}
/// Results expose partial knowledge; no discovery result establishes DHT absence.
pub struct TorDiscoveryReport {
    /// Responsive, independently authenticated storage peers, closest first.
    pub peers: Vec<Arc<VerifiedExtensionPeer>>,
    /// Every discovered query completed before the deadline and resource bound.
    pub exhausted: bool,
    /// The lookup hit its explicit resource limit.
    pub limited: bool,
}
/// Bounded signed routing state for one community's actual Tor runtime. A
/// bootstrap can use this alone: the type has no DHT record storage capability.
pub struct TorDiscovery<I, V> {
    messages: Arc<ctrn::messages::Messages>,
    identity: I,
    verifier: V,
    own: Peer,
    clock: Arc<dyn ctrn::Clock>,
    limits: TorDiscoveryLimits,
    routes: Mutex<BTreeMap<NodeId, Peer>>,
    envelope_time: Arc<dyn TorEnvelopeTime>,
    envelope_window: TimeWindow,
}
impl<I: TorNodeIdentity, V: Verifier> TorDiscovery<I, V> {
    /// Bind signed self-advertisement to an ACTUAL live listener and matching
    /// scoped node identity. Bootstrap entries are original independently signed
    /// PeerInfo, not caller-supplied coordinates or a complete device roster.
    pub fn new(
        config: TorDiscoveryConfig<I, V>,
        listener: &ctrn::messages::MessageListener,
    ) -> Result<Self, Error> {
        let TorDiscoveryConfig {
            messages,
            identity,
            verifier,
            own,
            bootstrap,
            clock,
            limits,
            envelope_time,
            envelope_window,
        } = config;
        if limits.timeout_ms == 0
            || limits.lanes.get() > limits.peers.get()
            || bootstrap.len() > limits.peers.get()
        {
            return Err(Error::Encoding);
        }
        let own = peer(own.verify_extension(&verifier)?)?;
        if own.info.node_id() != &identity.node_id()
            || own.endpoint.bytes() != listener.address().bytes()
        {
            return Err(Error::WrongRecord);
        }
        let this = Self {
            messages,
            identity,
            verifier,
            own,
            clock,
            limits,
            routes: Mutex::new(BTreeMap::new()),
            envelope_time,
            envelope_window,
        };
        for info in bootstrap {
            this.observe(peer(info.verify_extension(&this.verifier)?)?)?;
        }
        Ok(this)
    }
    fn observe(&self, peer: Peer) -> Result<(), Error> {
        let id = *peer.info.node_id();
        if id == self.identity.node_id() {
            return Ok(());
        }
        let mut routes = self.routes.lock().map_err(|_| Error::Unavailable)?;
        if let Some(previous) = routes.get(&id) {
            let old = previous.info.advertisement().timestamp_us;
            let new = peer.info.advertisement().timestamp_us;
            if new < old {
                return Ok(());
            }
            if new == old {
                return if previous.info.advertisement() == peer.info.advertisement() {
                    Ok(())
                } else {
                    Err(Error::Conflict)
                };
            }
        } else if routes.len() == self.limits.peers.get() {
            return Err(Error::Unavailable);
        }
        routes.insert(id, peer);
        Ok(())
    }
    async fn call_find(
        &self,
        to: Arc<VerifiedExtensionPeer>,
        target: NodeId,
    ) -> Result<Vec<Peer>, Error> {
        let question = find_node(&target, &[DHTV])?.with_sender(self.own.info.original())?;
        let signed = self.identity.question(&question, to.node_id()).await?;
        if signed.operation() != question.as_bytes() || signed.signer() != &self.identity.node_id()
        {
            return Err(Error::Encoding);
        }
        let received = self.exchange(&to, &question, &signed).await?;
        find_response(&signed, received.operation(), &self.verifier)?
            .into_iter()
            .map(peer)
            .collect()
    }
    async fn exchange(
        &self,
        to: &VerifiedExtensionPeer,
        question: &Question,
        signed: &SignedOperation,
    ) -> Result<AuthenticatedOperation, Error> {
        let checked = peer(to.original().verify_extension(&self.verifier)?)?;
        if signed.operation() != question.as_bytes() {
            return Err(Error::Encoding);
        }
        // Verify the signed result, including exact destination, before transmission.
        let signed = SignedOperation::verify(
            signed.as_bytes().to_vec(),
            to.node_id(),
            signed.signer(),
            &self.verifier,
        )?;
        let now = self.envelope_time.now_us()?;
        let outgoing = self.identity.seal(&signed, to.node_id(), now).await?;
        let outgoing = Envelope::verify(
            outgoing,
            to.node_id(),
            now,
            self.envelope_window,
            &self.verifier,
        )?;
        if outgoing.sender() != &self.identity.node_id() {
            return Err(Error::BadSignature);
        }
        let answer = self
            .messages
            .app_call(&checked.endpoint, outgoing.as_bytes())
            .await
            .map_err(|_| Error::Unavailable)?;
        let answer = Envelope::verify(
            answer,
            &self.identity.node_id(),
            self.envelope_time.now_us()?,
            self.envelope_window,
            &self.verifier,
        )?;
        if answer.sender() != to.node_id() {
            return Err(Error::BadSignature);
        }
        let received = self.identity.open(&answer).await?;
        if received.sender() != to.node_id() || received.operation().signer() != to.node_id() {
            return Err(Error::BadSignature);
        }
        Ok(received)
    }
    pub(crate) fn now_ms(&self) -> u64 {
        self.clock.now()
    }
    pub(crate) fn now_us(&self) -> Result<u64, Error> {
        self.envelope_time.now_us()
    }
    pub(crate) fn verifier(&self) -> &V {
        &self.verifier
    }
    pub(crate) fn node_id(&self) -> NodeId {
        self.identity.node_id()
    }
    pub(crate) fn sleep_until(&self, deadline_ms: u64) -> crate::BoxFuture<'_, ()> {
        self.clock.sleep_until(deadline_ms)
    }
    pub(crate) fn stores_records(&self) -> bool {
        self.own.info.advertisement().capabilities.contains(&DHTV)
    }
    pub(crate) fn mark_responsive(&self, node: &NodeId, responsive: bool) -> Result<(), Error> {
        if let Some(peer) = self
            .routes
            .lock()
            .map_err(|_| Error::Unavailable)?
            .get_mut(node)
        {
            peer.responsive = responsive;
        }
        Ok(())
    }
    pub(crate) fn route(&self, node: &NodeId) -> Result<Arc<VerifiedExtensionPeer>, Error> {
        self.routes
            .lock()
            .map_err(|_| Error::Unavailable)?
            .get(node)
            .map(|p| p.info.clone())
            .ok_or(Error::Unavailable)
    }
    pub(crate) fn observe_hints(&self, hints: &[UnverifiedPeerInfoBytes]) -> Result<(), Error> {
        for hint in hints {
            self.observe(peer(hint.verify_extension(&self.verifier)?)?)?;
        }
        Ok(())
    }
    pub(crate) fn storage_hints(&self, target: &NodeId) -> Result<Vec<NodeId>, Error> {
        let mut nodes: Vec<_> = self
            .routes
            .lock()
            .map_err(|_| Error::Unavailable)?
            .values()
            .filter(|peer| peer.info.advertisement().capabilities.contains(&DHTV))
            .map(|peer| *peer.info.node_id())
            .collect();
        nodes.sort_unstable_by_key(|node| std::array::from_fn::<_, 32, _>(|i| node[i] ^ target[i]));
        nodes.truncate(cdht::MAX_FANOUT_NODES);
        Ok(nodes)
    }
    pub(crate) fn closer(&self, target: &NodeId) -> Result<Vec<UnverifiedPeerInfoBytes>, Error> {
        let own_distance = std::array::from_fn::<_, 32, _>(|i| self.node_id()[i] ^ target[i]);
        let mut peers: Vec<_> = self
            .routes
            .lock()
            .map_err(|_| Error::Unavailable)?
            .values()
            .filter(|p| p.responsive && p.info.advertisement().capabilities.contains(&DHTV))
            .map(|p| p.info.clone())
            .collect();
        peers.sort_by_key(|p| std::array::from_fn::<_, 32, _>(|i| p.node_id()[i] ^ target[i]));
        peers
            .into_iter()
            .filter(|p| {
                std::array::from_fn::<_, 32, _>(|i| p.node_id()[i] ^ target[i]) < own_distance
            })
            .take(20)
            .map(|p| UnverifiedPeerInfoBytes::from_bytes(p.original().as_bytes().to_vec()))
            .collect()
    }
    pub(crate) async fn call_record(
        &self,
        to: &NodeId,
        question: &Question,
    ) -> Result<AuthenticatedOperation, Error> {
        let route = self.route(to)?;
        let signed = self.identity.question(question, to).await?;
        if signed.signer() != &self.node_id() {
            return Err(Error::BadSignature);
        }
        let result = self.exchange(&route, question, &signed).await;
        self.mark_responsive(to, result.is_ok())?;
        result
    }
    pub(crate) fn with_sender(&self, question: Question) -> Result<Question, Error> {
        question.with_sender(self.own.info.original())
    }
    pub(crate) async fn authenticate(
        &self,
        bytes: Vec<u8>,
    ) -> Result<AuthenticatedOperation, Error> {
        let envelope = Envelope::verify(
            bytes,
            &self.node_id(),
            self.now_us()?,
            self.envelope_window,
            &self.verifier,
        )?;
        let received = self.identity.open(&envelope).await?;
        if received.sender() != envelope.sender() {
            return Err(Error::BadSignature);
        }
        Ok(received)
    }
    pub(crate) fn observe_sender(&self, received: &AuthenticatedOperation) -> Result<(), Error> {
        let sender = peer(received.sender_peer(&self.verifier)?)?;
        self.observe(sender)?;
        self.mark_responsive(received.sender(), true)
    }
    pub(crate) async fn call_watch<W: TorWatcher>(
        &self,
        to: &NodeId,
        key: &cdht::RecordKey,
        question: &Question,
        watcher: &W,
    ) -> Result<AuthenticatedOperation, Error> {
        let route = self.route(to)?;
        let signed = watcher.question(key, question, to).await?;
        if signed.signer() != &watcher.public_key(key)? {
            return Err(Error::BadSignature);
        }
        let result = self.exchange(&route, question, &signed).await;
        self.mark_responsive(to, result.is_ok())?;
        result
    }
    pub(crate) async fn reply_record(
        &self,
        answer: &Answer,
        target: &NodeId,
    ) -> Result<Vec<u8>, Error> {
        let signed = self.identity.record_answer(answer, target).await?;
        if signed.operation() != answer.as_bytes() {
            return Err(Error::Encoding);
        }
        let checked = SignedOperation::verify(
            signed.as_bytes().to_vec(),
            target,
            &self.node_id(),
            &self.verifier,
        )?;
        self.identity.seal(&checked, target, self.now_us()?).await
    }
    pub(crate) async fn send_statement(
        &self,
        statement: &Statement,
        target: &NodeId,
    ) -> Result<(), Error> {
        let route = self.route(target)?;
        let route = peer(route.original().verify_extension(&self.verifier)?)?;
        let signed = self.identity.statement(statement, target).await?;
        if signed.operation() != statement.as_bytes() {
            return Err(Error::Encoding);
        }
        let checked = SignedOperation::verify(
            signed.as_bytes().to_vec(),
            target,
            &self.node_id(),
            &self.verifier,
        )?;
        let bytes = self.identity.seal(&checked, target, self.now_us()?).await?;
        self.messages
            .app_message(&route.endpoint, &bytes)
            .await
            .map_err(|_| Error::Unavailable)
    }
    /// Query dynamic original peer hints over Tor, unique and XOR-ordered, with
    /// bounded concurrent lanes and an actual runtime deadline. A timed-out lookup
    /// returns only its authenticated partial results, never a false empty vault.
    pub async fn discover(&self, target: NodeId) -> Result<TorDiscoveryReport, Error> {
        let start = self.clock.now();
        let deadline = start
            .checked_add(self.limits.timeout_ms)
            .ok_or(Error::Unavailable)?;
        let mut search = Discovery::new(target, self.identity.node_id(), self.limits.peers);
        for entry in self.routes.lock().map_err(|_| Error::Unavailable)?.values() {
            search.insert(entry.info.clone())?;
        }
        let work = async {
            let mut pending = FuturesUnordered::new();
            loop {
                while pending.len() < self.limits.lanes.get() {
                    let Some(peer) = search.next_peer() else {
                        break;
                    };
                    pending.push(async move {
                        let id = *peer.node_id();
                        (id, self.call_find(peer, target).await)
                    });
                }
                let Some((id, result)) = pending.next().await else {
                    break;
                };
                match result {
                    Ok(peers) => {
                        search.finish(&id, true)?;
                        self.mark_responsive(&id, true)?;
                        for next in peers {
                            let info = next.info.clone();
                            self.observe(next)?;
                            if search.insert(info).is_err() {
                                return Ok::<(), Error>(());
                            }
                        }
                    }
                    Err(_) => {
                        search.finish(&id, false)?;
                        self.mark_responsive(&id, false)?;
                    }
                }
                if self.clock.now() < start {
                    return Err(Error::Unavailable);
                }
            }
            Ok(())
        };
        let expired = match select(Box::pin(work), self.clock.sleep_until(deadline)).await {
            Either::Left((result, _)) => {
                result?;
                false
            }
            Either::Right((_, work)) => {
                drop(work);
                true
            }
        };
        Ok(TorDiscoveryReport {
            peers: search.storage_peers(),
            exhausted: !expired && search.exhausted(),
            limited: search.limited(),
        })
    }
    /// Handle a bootstrap/routing FindNode request. Only the actual request signer
    /// may introduce its self-signed endpoint. Returned peers remain original bytes.
    /// The caller sends this result through the received one-use Tor reply port.
    pub async fn answer_find(&self, request: Vec<u8>) -> Result<Vec<u8>, Error> {
        let envelope = Envelope::verify(
            request,
            &self.identity.node_id(),
            self.envelope_time.now_us()?,
            self.envelope_window,
            &self.verifier,
        )?;
        let received = self.identity.open(&envelope).await?;
        if received.sender() != envelope.sender()
            || received.operation().signer() != received.sender()
        {
            return Err(Error::BadSignature);
        }
        let request = received.operation();
        let sender = peer(received.sender_peer(&self.verifier)?)?;
        let (_, target, caps) = find_query(request)?;
        self.observe(sender)?;
        self.mark_responsive(received.sender(), true)?;
        let mut peers: Vec<_> = self
            .routes
            .lock()
            .map_err(|_| Error::Unavailable)?
            .values()
            .map(|p| p.info.clone())
            .chain(std::iter::once(self.own.info.clone()))
            .filter(|p| {
                caps.iter()
                    .all(|cap| p.advertisement().capabilities.contains(cap))
            })
            .collect();
        peers.sort_unstable_by_key(|p| {
            std::array::from_fn::<_, 32, _>(|i| p.node_id()[i] ^ target[i])
        });
        peers.truncate(20);
        let original = peers
            .iter()
            .map(|p| UnverifiedPeerInfoBytes::from_bytes(p.original().as_bytes().to_vec()))
            .collect::<Result<Vec<_>, _>>()?;
        let answer = FindAnswer::new(request, &original)?;
        let signed = self.identity.answer(&answer, request.signer()).await?;
        if signed.operation() != answer.as_bytes() {
            return Err(Error::Encoding);
        }
        let checked = SignedOperation::verify(
            signed.as_bytes().to_vec(),
            request.signer(),
            &self.identity.node_id(),
            &self.verifier,
        )?;
        self.identity
            .seal(&checked, envelope.sender(), self.envelope_time.now_us()?)
            .await
    }
}
"#;

    const FROZEN_ORIGINAL_SHA256: &str =
        "fc6c8db74254e6246bb6da69228e4afe2c17ff42575133bb3ab1b530d2c30041";
    /// Deterministic diagnostic digest of `transform(FROZEN_LIB)`, derived
    /// mechanically and pinned here and in the consumer manifest. Any table
    /// change must update both under review.
    const EXPECTED_DIAGNOSTIC_SHA256: &str =
        "42dc2e7df8f7e134a0a385587176bb0d2713954a12ca1e217e98e150d8f79785";

    #[test]
    fn transform_applies_to_real_frozen_file() {
        // The table anchors must match the real frozen bytes they were
        // generated from; the runtime hash gate enforces this in production,
        // and this test enforces table-fixture consistency here.
        assert_eq!(sha256_hex(FROZEN_LIB.as_bytes()), FROZEN_ORIGINAL_SHA256);
        let transformed = transform(FROZEN_LIB).unwrap();
        // Deterministic diagnostic digest, pinned here and in the consumer
        // manifest: any table change must update both under review.
        assert_eq!(
            sha256_hex(transformed.as_bytes()),
            EXPECTED_DIAGNOSTIC_SHA256
        );
        for tag in EXPECTED_TAGS {
            let hits = transformed
                .match_indices(tag)
                .filter(|(index, _)| {
                    matches!(
                        transformed[index + tag.len()..].chars().next(),
                        Some(':') | Some('"')
                    )
                })
                .count();
            assert_eq!(hits, 1, "{tag}");
        }
        let mut restored = transformed.clone();
        for site in sites().into_iter().rev() {
            restored = restored.replacen(site.replacement, site.anchor, 1);
        }
        assert_eq!(restored, FROZEN_LIB);
    }

    #[test]
    fn transform_rejects_doubled_frozen_file() {
        let doubled = format!("{FROZEN_LIB}{FROZEN_LIB}");
        assert!(transform(&doubled).is_err());
    }

    fn unit_layout(root: &tempfile::TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let manifest = root.path().join("work").join("cmsh");
        fs::create_dir_all(manifest.join("src")).unwrap();
        let lib = manifest.join("src/lib.rs");
        let target = manifest.join("src/tor_discovery.rs");
        fs::write(&lib, b"crate cmsh;").unwrap();
        fs::write(&target, b"original bytes").unwrap();
        (manifest, lib, target)
    }

    fn unit_argv(lib: &Path) -> Vec<OsString> {
        vec![
            OsString::from("--crate-name"),
            OsString::from("cmsh"),
            OsString::from(lib),
        ]
    }

    #[test]
    fn gating_passes_only_the_exact_target() {
        let root = tempfile::tempdir().unwrap();
        let (manifest, lib, target) = unit_layout(&root);
        let resolved = resolve_target(Some("cmsh"), Some(&manifest), &unit_argv(&lib), root.path())
            .unwrap()
            .expect("exact target resolves");
        assert_eq!(resolved, target.canonicalize().unwrap());
        // Anything but the exact crate passes through before touching paths.
        assert!(
            resolve_target(Some("cdht"), Some(&manifest), &unit_argv(&lib), root.path())
                .unwrap()
                .is_none()
        );
        assert!(
            resolve_target(None, Some(&manifest), &unit_argv(&lib), root.path())
                .unwrap()
                .is_none()
        );
        // A claimed target with bad config or paths fails, never passes through.
        assert!(resolve_target(Some("cmsh"), None, &unit_argv(&lib), root.path()).is_err());
        assert!(resolve_target(
            Some("cmsh"),
            Some(&manifest),
            &unit_argv(&lib),
            &root.path().join("elsewhere"),
        )
        .is_err());
        assert!(resolve_target(Some("cmsh"), Some(&manifest), &[], root.path()).is_err());
        let other = root.path().join("other.rs");
        fs::write(&other, b"other").unwrap();
        let mut two = unit_argv(&lib);
        two.push(other.into_os_string());
        assert!(resolve_target(Some("cmsh"), Some(&manifest), &two, root.path()).is_err());
        // Flags, rlibs and non-files never qualify as inputs on their own.
        let flags = vec![
            OsString::from("--edition=2021"),
            OsString::from("--extern"),
            OsString::from("foo=bar.rlib"),
        ];
        assert!(resolve_target(Some("cmsh"), Some(&manifest), &flags, root.path()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn gating_rejects_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let (manifest, lib, _) = unit_layout(&root);
        let outside = root.path().join("outside.rs");
        fs::write(&outside, b"escape").unwrap();
        let link = manifest.join("src/tor_discovery.rs");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(
            resolve_target(Some("cmsh"), Some(&manifest), &unit_argv(&lib), root.path()).is_err()
        );
    }

    #[test]
    fn resolve_compiler_composes_chain_without_recursion() {
        let resolved = resolve_compiler("/bin/true").unwrap();
        assert_eq!(resolved, vec![OsString::from("/bin/true")]);
        assert!(!is_self(Path::new("/bin/true")));
    }

    #[test]
    fn restore_source_rewrites_and_verifies_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("tor_discovery.rs");
        fs::write(&source, b"diagnostic bytes").unwrap();
        let expected = sha256_hex(b"original bytes");
        assert!(restore_source(&source, b"original bytes", &expected).unwrap());
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        assert!(!restore_source(&source, b"original bytes", &sha256_hex(b"other")).unwrap());
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
    }

    #[test]
    fn receipt_writer_creates_evidence_parents() {
        let directory = tempfile::tempdir().unwrap();
        let receipt = directory.path().join("nested").join("receipt.json");
        write_receipt(
            &receipt,
            serde_json::json!({"restored": true, "compiler_exit": 0}),
        )
        .unwrap();
        let text = fs::read_to_string(&receipt).unwrap();
        let receipt: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(receipt["restored"].as_bool(), Some(true));
    }

    fn guarded_fixture() -> (tempfile::TempDir, PathBuf, Vec<u8>, String, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("tor_discovery.rs");
        let original = b"canonical bytes".to_vec();
        fs::write(&source, &original).unwrap();
        let expected = sha256_hex(&original);
        let receipt = directory.path().join("evidence").join("receipt.json");
        (directory, source, original, expected, receipt)
    }

    #[test]
    fn execute_guarded_records_success_and_restores() {
        let (_directory, source, original, expected, receipt) = guarded_fixture();
        fs::write(&source, b"diagnostic bytes").unwrap();
        let exit = execute_guarded(
            || Ok(0),
            &source,
            &original,
            &expected,
            &sha256_hex(b"diagnostic bytes"),
            &receipt,
        )
        .unwrap();
        assert_eq!(exit, 0);
        assert_eq!(fs::read(&source).unwrap(), original);
        let lines: Vec<String> = fs::read_to_string(&receipt)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"compiler_exit\":0"));
        assert!(lines[0].contains("\"restored\":true"));
    }

    #[test]
    fn execute_guarded_propagates_compiler_failure_still_restored() {
        let (_directory, source, original, expected, receipt) = guarded_fixture();
        fs::write(&source, b"diagnostic bytes").unwrap();
        let exit = execute_guarded(
            || Ok(1),
            &source,
            &original,
            &expected,
            &sha256_hex(b"diagnostic bytes"),
            &receipt,
        )
        .unwrap();
        assert_eq!(exit, 1);
        assert_eq!(fs::read(&source).unwrap(), original);
    }

    #[test]
    fn execute_guarded_records_spawn_failure_still_restored() {
        let (_directory, source, original, expected, receipt) = guarded_fixture();
        fs::write(&source, b"diagnostic bytes").unwrap();
        let error = execute_guarded(
            || Err(failure("spawn failed")),
            &source,
            &original,
            &expected,
            &sha256_hex(b"diagnostic bytes"),
            &receipt,
        )
        .unwrap_err();
        assert!(format!("{error}").contains("spawn failed"));
        assert_eq!(fs::read(&source).unwrap(), original);
        let text = fs::read_to_string(&receipt).unwrap();
        assert!(text.contains("\"compiler_exit\":null"));
        assert!(text.contains("\"restored\":true"));
    }

    #[test]
    fn execute_guarded_receipt_failure_masks_nothing_silently() {
        let (_directory, source, original, expected, _receipt) = guarded_fixture();
        fs::write(&source, b"diagnostic bytes").unwrap();
        // A receipt path through a regular file cannot be created.
        let blocker = _directory.path().join("blocker");
        fs::write(&blocker, b"not a directory").unwrap();
        let bad_receipt = blocker.join("receipt.json");
        let error = execute_guarded(
            || Ok(0),
            &source,
            &original,
            &expected,
            &sha256_hex(b"diagnostic bytes"),
            &bad_receipt,
        )
        .unwrap_err();
        assert!(format!("{error}").contains("receipt"));
        assert_eq!(fs::read(&source).unwrap(), original);
    }

    #[test]
    #[cfg(unix)]
    fn missing_lock_holder_refuses_second_transform() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("tor_discovery.rs");
        fs::write(&source, b"marker").unwrap();
        let _first = acquire_lock(&source).unwrap();
        assert!(acquire_lock(&source).is_err());
    }
}

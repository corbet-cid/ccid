//! Hidden compiler-wrapper diagnostic for one frozen library build.
//!
//! Cargo invokes this binary as `RUSTC_WRAPPER` (selected by the
//! `CCID_RUSTC_DIAG` environment marker) with the real compiler path as
//! the first argument followed by rustc arguments. Only the exact cmsh
//! library unit is instrumented: crate name `cmsh`, manifest beneath the
//! job-owned frozen-source root, and `src/tor_records.rs` matching its
//! committed digest. Every other invocation passes through to the real
//! compiler (or a composed outer wrapper) untouched.
//!
//! The transform inserts static-stage `eprintln!` diagnostics (error enum
//! variant only, never peer IDs, paths, keys or payloads) at exact anchor
//! sites, compiles, then restores the original bytes and verifies the
//! restore — including on compiler failure. Original sources stay verified;
//! the receipt records original/diagnostic digests, the transformation
//! list, and the compiler/status outcome. Unix only: the owning jobs run
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
/// Committed SHA-256 of the exact frozen `src/tor_records.rs`.
pub(crate) const ORIGINAL_ENV: &str = "CCID_DIAG_ORIGINAL_SHA256";
/// Receipt JSON path inside job-owned evidence.
pub(crate) const RECEIPT_ENV: &str = "CCID_DIAG_RECEIPT";
/// Crate selected for instrumentation. Nothing else is ever touched.
pub(crate) const TARGET_CRATE: &str = "cmsh";
/// Instrumented file relative to the crate manifest directory.
pub(crate) const TARGET_FILE: &str = "src/tor_records.rs";
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
    // Whole-function entries generated mechanically from the frozen
    // source: each anchor is one complete original function, each
    // replacement wraps it in an identifying async block with the
    // stage diagnostics baked in. Exact-once matching fails closed
    // on any upstream change.
    vec![
        Site {
            name: "handle_call function",
            anchor: r#"    pub async fn handle_call(&self, bytes: Vec<u8>) -> Result<Vec<u8>, Error> {
        let received = self.discovery.authenticate(bytes.clone()).await?;
        self.discovery.observe_sender(&received)?;
        if cdht::rpc::peers::find_query(received.operation()).is_ok() {
            return self.discovery.answer_find(bytes).await;
        }
        if let Ok((_, request)) = inspection(received.operation()) {
            if received.operation().signer() != received.sender() {
                return Err(Error::BadSignature);
            }
            let peers = self.discovery.closer(&request.key.0)?;
            let accepted = self.store.is_some() && peers.len() < self.limits.consensus_width.get();
            let found = self
                .store
                .as_ref()
                .map(|store| store.inspect(&request.key, &request.subkeys))
                .transpose()?
                .flatten();
            let (descriptor, seqs) = match found {
                Some((descriptor, seqs)) => (request.want_descriptor.then_some(descriptor), seqs),
                None => (None, Vec::new()),
            };
            let response = InspectionResponse {
                accepted,
                seqs,
                peers,
                descriptor,
            };
            return self
                .discovery
                .reply_record(
                    &Answer::inspection(received.operation(), &response)?,
                    received.sender(),
                )
                .await;
        }
        let (_, request) = query(received.operation())?;
        if !matches!(request, Query::Watch { .. })
            && received.operation().signer() != received.sender()
        {
            return Err(Error::BadSignature);
        }
        let key = match &request {
            Query::Get { key, .. } | Query::Set { key, .. } | Query::Watch { key, .. } => key,
        };
        let peers = self.discovery.closer(&key.0)?;
        let accepted = self.store.is_some() && peers.len() < self.limits.consensus_width.get();
        let response = match request {
            Query::Get {
                key,
                subkey,
                want_descriptor,
            } => {
                let (descriptor, value) = if let Some(store) = &self.store {
                    let descriptor = store.descriptor(&key).await?;
                    let value = if descriptor.is_some() {
                        store.get(&key, subkey).await?
                    } else {
                        None
                    };
                    (descriptor.filter(|_| want_descriptor), value.map(Box::new))
                } else {
                    (None, None)
                };
                Response::Get {
                    accepted,
                    descriptor,
                    value,
                    peers,
                }
            }
            Query::Set { .. } => {
                let mut need_descriptor = false;
                let mut value = None;
                if accepted {
                    match self
                        .store
                        .as_ref()
                        .ok_or(Error::Unavailable)?
                        .accept_value(&received)
                    {
                        Ok(SetOutcome::Accepted) => {}
                        Ok(SetOutcome::Newer(current)) => value = Some(Box::new(current)),
                        Err(Error::UnknownRecord) => need_descriptor = true,
                        Err(error) => return Err(error),
                    }
                }
                Response::Set {
                    accepted,
                    need_descriptor,
                    value,
                    peers,
                }
            }
            Query::Watch { watch_id, .. } => {
                let mut id = watch_id;
                let mut duration_us = 0;
                if accepted {
                    match self
                        .store
                        .as_ref()
                        .ok_or(Error::Unavailable)?
                        .accept_watch(&received, self.discovery.now_us()?)
                    {
                        Ok(lease) => {
                            id = lease.id;
                            duration_us = lease.duration_us;
                        }
                        Err(Error::UnknownRecord) => {}
                        Err(error) => return Err(error),
                    }
                }
                Response::Watch {
                    accepted,
                    duration_us,
                    watch_id: id,
                    peers,
                }
            }
        };
        self.discovery
            .reply_record(
                &Answer::new(received.operation(), &response)?,
                received.sender(),
            )
            .await
    }"#,
            replacement: r#"    pub async fn handle_call(&self, bytes: Vec<u8>) -> Result<Vec<u8>, Error> {
        async move {
            let received = self.discovery.authenticate(bytes.clone()).await
            .inspect_err(|error| {
                eprintln!("ccid-diag tor-watch-server authenticate: {error:?}");
            })?;
            self.discovery.observe_sender(&received)
            .inspect_err(|error| {
                eprintln!("ccid-diag tor-watch-server observe-sender: {error:?}");
            })?;
            if cdht::rpc::peers::find_query(received.operation()).is_ok() {
                return self.discovery.answer_find(bytes).await;
            }
            if let Ok((_, request)) = inspection(received.operation()) {
                if received.operation().signer() != received.sender() {
                    return Err(Error::BadSignature);
                }
                let peers = self.discovery.closer(&request.key.0)?;
                let accepted = self.store.is_some() && peers.len() < self.limits.consensus_width.get();
                let found = self
                    .store
                    .as_ref()
                    .map(|store| store.inspect(&request.key, &request.subkeys))
                    .transpose()?
                    .flatten();
                let (descriptor, seqs) = match found {
                    Some((descriptor, seqs)) => (request.want_descriptor.then_some(descriptor), seqs),
                    None => (None, Vec::new()),
                };
                let response = InspectionResponse {
                    accepted,
                    seqs,
                    peers,
                    descriptor,
                };
                return self
                    .discovery
                    .reply_record(
                        &Answer::inspection(received.operation(), &response)?,
                        received.sender(),
                    )
                    .await;
            }
            let (_, request) = query(received.operation())
            .inspect_err(|error| {
                eprintln!("ccid-diag tor-watch-server decode: {error:?}");
            })?;
            if !matches!(request, Query::Watch { .. })
                && received.operation().signer() != received.sender()
            {
                return Err(Error::BadSignature);
            }
            let key = match &request {
                Query::Get { key, .. } | Query::Set { key, .. } | Query::Watch { key, .. } => key,
            };
            let peers = self.discovery.closer(&key.0)?;
            let accepted = self.store.is_some() && peers.len() < self.limits.consensus_width.get();
            let response = match request {
                Query::Get {
                    key,
                    subkey,
                    want_descriptor,
                } => {
                    let (descriptor, value) = if let Some(store) = &self.store {
                        let descriptor = store.descriptor(&key).await?;
                        let value = if descriptor.is_some() {
                            store.get(&key, subkey).await?
                        } else {
                            None
                        };
                        (descriptor.filter(|_| want_descriptor), value.map(Box::new))
                    } else {
                        (None, None)
                    };
                    Response::Get {
                        accepted,
                        descriptor,
                        value,
                        peers,
                    }
                }
                Query::Set { .. } => {
                    let mut need_descriptor = false;
                    let mut value = None;
                    if accepted {
                        match self
                            .store
                            .as_ref()
                            .ok_or(Error::Unavailable)?
                            .accept_value(&received)
                        {
                            Ok(SetOutcome::Accepted) => {}
                            Ok(SetOutcome::Newer(current)) => value = Some(Box::new(current)),
                            Err(Error::UnknownRecord) => need_descriptor = true,
                            Err(error) => return Err(error),
                        }
                    }
                    Response::Set {
                        accepted,
                        need_descriptor,
                        value,
                        peers,
                    }
                }
                Query::Watch { watch_id, .. } => {
                    let mut id = watch_id;
                    let mut duration_us = 0;
                    if accepted {
                        match self
                            .store
                            .as_ref()
                            .ok_or(Error::Unavailable)?
                            .accept_watch(&received, self.discovery.now_us()?)
                        {
                            Ok(lease) => {
                                id = lease.id;
                                duration_us = lease.duration_us;
                            }
                            Err(Error::UnknownRecord) => {}
                            Err(error) => {
                                eprintln!("ccid-diag tor-watch-server accept: {error:?}");
                                return Err(error);
                            }
                        }
                    }
                    Response::Watch {
                        accepted,
                        duration_us,
                        watch_id: id,
                        peers,
                    }
                }
            };
            self.discovery
                .reply_record(
                    &Answer::new(received.operation(), &response)?,
                    received.sender(),
                )
                .await
        }
        .await
        .inspect_err(|error| {
            eprintln!("ccid-diag tor-watch-server failed: {error:?}");
        })
    }"#,
        },
        Site {
            name: "watch_request function",
            anchor: r#"    async fn watch_request(
        &self,
        node: &NodeId,
        watch: &ClientWatch,
        id: u64,
        count: u32,
    ) -> Result<(u64, u64), Error> {
        let key = watch.descriptor.key();
        let question = self.discovery.with_sender(Question::new(&Query::Watch {
            key,
            subkeys: watch.subkeys.clone(),
            duration_us: 0,
            count,
            watch_id: id,
        })?)?;
        let start = self.discovery.now_ms();
        let answer = self
            .discovery
            .call_watch(node, &key, &question, &self.watcher)
            .await?;
        let response = question.answer(
            answer.operation(),
            Some(&watch.descriptor),
            self.discovery.verifier(),
        )?;
        self.observe_response(&response)?;
        let (duration_us, watch_id) = match response {
            Response::Watch {
                accepted: true,
                duration_us,
                watch_id,
                ..
            } => (duration_us, watch_id),
            Response::Watch {
                accepted: false, ..
            } => return Err(Error::WatchRefused),
            _ => return Err(Error::Encoding),
        };
        let now = self.discovery.now_ms();
        if now < start {
            return Err(Error::Unavailable);
        }
        if count == 0 {
            return if duration_us == 0 {
                Ok((watch_id, now))
            } else {
                Err(Error::Encoding)
            };
        }
        let expires = start
            .checked_add((now - start) / 2)
            .and_then(|mid| mid.checked_add(duration_us / 1000))
            .ok_or(Error::Encoding)?;
        if watch_id == 0 || expires <= now {
            return Err(Error::UnknownWatch);
        }
        Ok((watch_id, expires))
    }"#,
            replacement: r#"    async fn watch_request(
        &self,
        node: &NodeId,
        watch: &ClientWatch,
        id: u64,
        count: u32,
    ) -> Result<(u64, u64), Error> {
        async move {
            let key = watch.descriptor.key();
            let question = self.discovery.with_sender(Question::new(&Query::Watch {
                key,
                subkeys: watch.subkeys.clone(),
                duration_us: 0,
                count,
                watch_id: id,
            })?)?;
            let start = self.discovery.now_ms();
            let answer = self
                .discovery
                .call_watch(node, &key, &question, &self.watcher)
                .await
                .inspect_err(|error| {
                    eprintln!("ccid-diag tor-watch call-watch: {error:?}");
                })?;
            let response = question.answer(
                answer.operation(),
                Some(&watch.descriptor),
                self.discovery.verifier(),
            )
            .inspect_err(|error| {
                eprintln!("ccid-diag tor-watch answer: {error:?}");
            })?;
            self.observe_response(&response)
            .inspect_err(|error| {
                eprintln!("ccid-diag tor-watch observe: {error:?}");
            })?;
            let (duration_us, watch_id) = match response {
                Response::Watch {
                    accepted: true,
                    duration_us,
                    watch_id,
                    ..
                } => (duration_us, watch_id),
                Response::Watch {
                    accepted: false, ..
                } => return Err(Error::WatchRefused),
                _ => return Err(Error::Encoding),
            };
            let now = self.discovery.now_ms();
            if now < start {
                return Err(Error::Unavailable);
            }
            if count == 0 {
                return if duration_us == 0 {
                    Ok((watch_id, now))
                } else {
                    Err(Error::Encoding)
                };
            }
            let expires = start
                .checked_add((now - start) / 2)
                .and_then(|mid| mid.checked_add(duration_us / 1000))
                .ok_or(Error::Encoding)?;
            if watch_id == 0 || expires <= now {
                eprintln!("ccid-diag tor-watch expiry");
                return Err(Error::UnknownWatch);
            }
            Ok((watch_id, expires))
        }
        .await
        .inspect_err(|error| {
            eprintln!("ccid-diag tor-watch-request failed: {error:?}");
        })
    }"#,
        },
        Site {
            name: "watch function",
            anchor: r#"    async fn watch(&self, node: &NodeId, key: &RecordKey) -> Result<WatchId, Error> {
        {
            let watches = self.watches.lock().map_err(|_| Error::Unavailable)?;
            if watches.len() >= self.limits.watches.get() {
                return Err(Error::Unavailable);
            }
            if watches
                .iter()
                .any(|((n, _), w)| n == node && w.descriptor.key() == *key)
            {
                return Err(Error::Conflict);
            }
        }
        let descriptor = Network::descriptor(self, node, key)
            .await?
            .ok_or(Error::UnknownRecord)?;
        let schema = descriptor.validate_for(key, self.discovery.verifier())?;
        let watch = ClientWatch {
            descriptor,
            subkeys: SubkeyRanges::from_iter(0..schema.subkey_count() as u32),
            changed: SubkeyRanges::new(),
            expires_ms: 0,
            count: u32::MAX,
            lost: false,
        };
        let (id, expires_ms) = self.watch_request(node, &watch, 0, watch.count).await?;
        let mut accepted = watch.clone();
        accepted.expires_ms = expires_ms;
        let retained = {
            let mut watches = self.watches.lock().map_err(|_| Error::Unavailable)?;
            if watches.len() >= self.limits.watches.get() || watches.contains_key(&(*node, id)) {
                false
            } else {
                watches.insert((*node, id), accepted);
                true
            }
        };
        if !retained {
            let _ = self.watch_request(node, &watch, id, 0).await;
            return Err(Error::Unavailable);
        }
        Ok(WatchId(id))
    }"#,
            replacement: r#"    async fn watch(&self, node: &NodeId, key: &RecordKey) -> Result<WatchId, Error> {
        async move {
            {
                let watches = self.watches.lock().map_err(|_| Error::Unavailable)?;
                if watches.len() >= self.limits.watches.get() {
                    eprintln!("ccid-diag tor-watch local-limit");
                    return Err(Error::Unavailable);
                }
                if watches
                    .iter()
                    .any(|((n, _), w)| n == node && w.descriptor.key() == *key)
                {
                    eprintln!("ccid-diag tor-watch local-conflict");
                    return Err(Error::Conflict);
                }
            }
            let descriptor = Network::descriptor(self, node, key)
                .await
                .inspect_err(|error| {
                    eprintln!("ccid-diag tor-watch descriptor: {error:?}");
                })?
                .ok_or_else(|| {
                    eprintln!("ccid-diag tor-watch descriptor-missing");
                    Error::UnknownRecord
                })?;
            let schema = descriptor.validate_for(key, self.discovery.verifier())?;
            let watch = ClientWatch {
                descriptor,
                subkeys: SubkeyRanges::from_iter(0..schema.subkey_count() as u32),
                changed: SubkeyRanges::new(),
                expires_ms: 0,
                count: u32::MAX,
                lost: false,
            };
            let (id, expires_ms) = self.watch_request(node, &watch, 0, watch.count).await?;
            let mut accepted = watch.clone();
            accepted.expires_ms = expires_ms;
            let retained = {
                let mut watches = self.watches.lock().map_err(|_| Error::Unavailable)?;
                if watches.len() >= self.limits.watches.get() || watches.contains_key(&(*node, id)) {
                    false
                } else {
                    watches.insert((*node, id), accepted);
                    true
                }
            };
            if !retained {
                let _ = self.watch_request(node, &watch, id, 0).await;
                return Err(Error::Unavailable);
            }
            Ok(WatchId(id))
        }
        .await
        .inspect_err(|error| {
            eprintln!("ccid-diag tor-watch failed: {error:?}");
        })
    }"#,
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

    /// Every diagnostic stage the review requires is present exactly once
    /// across the table: three whole-function tags plus the narrowing
    /// stage tags baked into the wrapped bodies.
    const EXPECTED_TAGS: [&str; 15] = [
        "ccid-diag tor-watch-server failed",
        "ccid-diag tor-watch-request failed",
        "ccid-diag tor-watch failed",
        "ccid-diag tor-watch descriptor",
        "ccid-diag tor-watch descriptor-missing",
        "ccid-diag tor-watch local-limit",
        "ccid-diag tor-watch local-conflict",
        "ccid-diag tor-watch call-watch",
        "ccid-diag tor-watch answer",
        "ccid-diag tor-watch observe",
        "ccid-diag tor-watch expiry",
        "ccid-diag tor-watch-server authenticate",
        "ccid-diag tor-watch-server observe-sender",
        "ccid-diag tor-watch-server decode",
        "ccid-diag tor-watch-server accept",
    ];

    #[test]
    fn transform_rejects_missing_or_duplicated_anchors() {
        assert!(transform("no anchors here").is_err());
        let doubled = format!("{ANCHORED}{ANCHORED}");
        assert!(transform(&doubled).is_err());
    }

    /// Exact frozen library source used for transform verification.
    /// Candidate `src/tor_records.rs` (FSL product tree, frozen bundle input).
    const FROZEN_LIB: &str = r#"//! Original record RPCs over Tor's authenticated dynamic routing owner. Bootstrap
//! construction has no store; member storage keeps original signed values intact.
use crate::tor_discovery::{TorDiscovery, TorNodeIdentity, TorWatcher};
use cdht::{
    Backend, Capacity, Change, Descriptor, Error, LocalBackend, Network, NodeId, RecordKey,
    SetOutcome, SignedValue, Verifier, WatchId,
    rpc::inspect::{Inspection, InspectionResponse, inspection},
    rpc::{Answer, Query, Question, Response, Statement, SubkeyRanges, query, value_changed},
    watches::WatchLimits,
};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

/// Explicit original responsibility width and bounded client-watch resources.
#[derive(Clone, Copy)]
pub struct TorRecordLimits {
    /// Original network.dht.consensus_width (maintained native default is ten).
    pub consensus_width: NonZeroUsize,
    /// Maximum retained per-node client leases, including lost leases to reconcile.
    pub watches: NonZeroUsize,
}
#[derive(Clone)]
struct ClientWatch {
    descriptor: Descriptor,
    subkeys: SubkeyRanges,
    changed: SubkeyRanges,
    expires_ms: u64,
    count: u32,
    lost: bool,
}
struct ListenerLease<'a>(&'a Mutex<BTreeMap<(NodeId, u64), ClientWatch>>);
impl Drop for ListenerLease<'_> {
    fn drop(&mut self) {
        if let Ok(mut watches) = self.0.lock() {
            for watch in watches.values_mut() {
                watch.lost = true;
            }
        }
    }
}
enum Finished {
    Request(Result<(), Error>),
    Maintenance(Result<(), Error>),
}
enum Signal {
    Incoming(Result<ctrn::Received, ctrn::Error>),
    Tick,
    Finished(Finished),
}
/// One actual node's record transport, member storage and original watch leases.
/// The runtime must drive `handle_call`, `handle_statement`, and `maintain` from
/// its retained actual listener/clock, and call `transport_lost` on listener loss.
pub struct TorRecords<I, V: Verifier, W> {
    discovery: Arc<TorDiscovery<I, V>>,
    store: Option<LocalBackend<V>>,
    watcher: W,
    limits: TorRecordLimits,
    watches: Mutex<BTreeMap<(NodeId, u64), ClientWatch>>,
}
impl<I: TorNodeIdentity, V: Verifier, W: TorWatcher> TorRecords<I, V, W> {
    /// Construct without operator storage when `storage` is None. An advertised
    /// DHTV capability must agree with an actual member store, never a ready flag.
    pub fn new(
        discovery: Arc<TorDiscovery<I, V>>,
        watcher: W,
        storage: Option<(Capacity, WatchLimits)>,
        limits: TorRecordLimits,
    ) -> Result<Self, Error>
    where
        V: Clone,
    {
        if limits.consensus_width.get() > 20 || discovery.stores_records() != storage.is_some() {
            return Err(Error::Encoding);
        }
        let store = storage
            .map(|(capacity, watches)| {
                LocalBackend::with_authenticated_watches(
                    discovery.verifier().clone(),
                    capacity,
                    watches,
                )
            })
            .transpose()?;
        Ok(Self {
            discovery,
            store,
            watcher,
            limits,
            watches: Mutex::new(BTreeMap::new()),
        })
    }
    /// Original routing owner; discovery does not imply record availability.
    pub fn discovery(&self) -> &Arc<TorDiscovery<I, V>> {
        &self.discovery
    }
    /// Actual storage inventory. A bootstrap has no member record store at all.
    pub fn storage_usage(&self) -> Option<(usize, usize, usize)> {
        self.store.as_ref().map(LocalBackend::usage)
    }

    /// Drive a dedicated actual record-service listener and its one-second timer.
    /// Requests have bounded concurrency; one maintenance operation can progress
    /// beside them, so renewal I/O never blocks inbound replies. Invalid requests
    /// are refused individually and reported only as coarse errors. The callback
    /// must return promptly and receives no keys, peer identities or payloads.
    ///
    /// The caller retains this future for the service lifetime. Listener failure,
    /// return or cancellation marks every client watch lost. This listener is for
    /// original record RPCs; a generic application listener is a distinct port.
    pub async fn run(
        &self,
        listener: ctrn::messages::MessageListener,
        concurrency: NonZeroUsize,
        mut on_error: impl FnMut(Error),
    ) -> Result<(), Error>
    where
        V: crate::MaybeSend + crate::MaybeSync,
    {
        let _lease = ListenerLease(&self.watches);
        // MessageListener::next may already own an accepted partial connection.
        // Unfold retains that in-flight future across timer/completion selection;
        // dropping a temporary StreamExt::next cannot discard that connection.
        let arrivals = futures::stream::unfold(listener, |mut listener| async move {
            let result = listener.next().await;
            Some((result, listener))
        });
        futures::pin_mut!(arrivals);
        let mut operations: FuturesUnordered<crate::BoxFuture<'_, Finished>> =
            FuturesUnordered::new();
        let mut requests = 0;
        let mut maintaining = false;
        let mut next_tick = self.discovery.now_ms();
        loop {
            let accept = requests < concurrency.get();
            let signal = {
                let incoming = async {
                    if accept {
                        arrivals
                            .next()
                            .await
                            .expect("listener unfold always yields")
                    } else {
                        futures::future::pending().await
                    }
                }
                .fuse();
                let completed = async {
                    match operations.next().await {
                        Some(result) => result,
                        None => futures::future::pending().await,
                    }
                }
                .fuse();
                let tick = self.discovery.sleep_until(next_tick).fuse();
                futures::pin_mut!(incoming, completed, tick);
                futures::select_biased! {
                    () = tick => Signal::Tick,
                    result = completed => Signal::Finished(result),
                    result = incoming => Signal::Incoming(result),
                }
            };
            match signal {
                Signal::Tick => {
                    next_tick = self
                        .discovery
                        .now_ms()
                        .checked_add(1_000)
                        .ok_or(Error::Unavailable)?;
                    if !maintaining {
                        maintaining = true;
                        operations.push(Box::pin(async {
                            Finished::Maintenance(self.maintain().await)
                        }));
                    }
                }
                Signal::Incoming(Err(_)) => return Err(Error::Unavailable),
                Signal::Incoming(Ok(message)) => {
                    requests += 1;
                    operations.push(Box::pin(async move {
                        Finished::Request(match message {
                            ctrn::Received::Call { payload, reply } => {
                                match self.handle_call(payload).await {
                                    Ok(answer) => {
                                        reply.send(&answer).await.map_err(|_| Error::Unavailable)
                                    }
                                    Err(error) => Err(error),
                                }
                            }
                            ctrn::Received::Message(payload) => {
                                self.handle_statement(payload).await
                            }
                        })
                    }));
                }
                Signal::Finished(result) => {
                    let result = match result {
                        Finished::Request(result) => {
                            requests -= 1;
                            result
                        }
                        Finished::Maintenance(result) => {
                            maintaining = false;
                            result
                        }
                    };
                    if let Err(error) = result {
                        on_error(error);
                    }
                }
            }
        }
    }

    async fn request(
        &self,
        node: &NodeId,
        request: Query,
        descriptor: Option<&Descriptor>,
    ) -> Result<Response, Error> {
        let question = self.discovery.with_sender(Question::new(&request)?)?;
        let answer = self.discovery.call_record(node, &question).await?;
        let response =
            question.answer(answer.operation(), descriptor, self.discovery.verifier())?;
        self.observe_response(&response)?;
        Ok(response)
    }
    fn observe_response(&self, response: &Response) -> Result<(), Error> {
        let peers = match response {
            Response::Get { peers, .. }
            | Response::Set { peers, .. }
            | Response::Watch { peers, .. } => peers,
        };
        self.discovery.observe_hints(peers)
    }
    /// Inspect original remote sequence hints without downloading value contents.
    /// A sequence hint is never proof of a record value, membership or freshness.
    pub async fn inspect(
        &self,
        node: &NodeId,
        request: &Inspection,
        descriptor: Option<&Descriptor>,
    ) -> Result<InspectionResponse, Error> {
        let question = self.discovery.with_sender(request.question()?)?;
        let answer = self.discovery.call_record(node, &question).await?;
        let response = question.inspection_answer(
            answer.operation(),
            descriptor,
            self.discovery.verifier(),
        )?;
        self.discovery.observe_hints(&response.peers)?;
        Ok(response)
    }
    /// Handle an original call and return bytes for the actual one-use reply port.
    /// Caller errors must close/refuse that call; they never become successful ACKs.
    pub async fn handle_call(&self, bytes: Vec<u8>) -> Result<Vec<u8>, Error> {
        let received = self.discovery.authenticate(bytes.clone()).await?;
        self.discovery.observe_sender(&received)?;
        if cdht::rpc::peers::find_query(received.operation()).is_ok() {
            return self.discovery.answer_find(bytes).await;
        }
        if let Ok((_, request)) = inspection(received.operation()) {
            if received.operation().signer() != received.sender() {
                return Err(Error::BadSignature);
            }
            let peers = self.discovery.closer(&request.key.0)?;
            let accepted = self.store.is_some() && peers.len() < self.limits.consensus_width.get();
            let found = self
                .store
                .as_ref()
                .map(|store| store.inspect(&request.key, &request.subkeys))
                .transpose()?
                .flatten();
            let (descriptor, seqs) = match found {
                Some((descriptor, seqs)) => (request.want_descriptor.then_some(descriptor), seqs),
                None => (None, Vec::new()),
            };
            let response = InspectionResponse {
                accepted,
                seqs,
                peers,
                descriptor,
            };
            return self
                .discovery
                .reply_record(
                    &Answer::inspection(received.operation(), &response)?,
                    received.sender(),
                )
                .await;
        }
        let (_, request) = query(received.operation())?;
        if !matches!(request, Query::Watch { .. })
            && received.operation().signer() != received.sender()
        {
            return Err(Error::BadSignature);
        }
        let key = match &request {
            Query::Get { key, .. } | Query::Set { key, .. } | Query::Watch { key, .. } => key,
        };
        let peers = self.discovery.closer(&key.0)?;
        let accepted = self.store.is_some() && peers.len() < self.limits.consensus_width.get();
        let response = match request {
            Query::Get {
                key,
                subkey,
                want_descriptor,
            } => {
                let (descriptor, value) = if let Some(store) = &self.store {
                    let descriptor = store.descriptor(&key).await?;
                    let value = if descriptor.is_some() {
                        store.get(&key, subkey).await?
                    } else {
                        None
                    };
                    (descriptor.filter(|_| want_descriptor), value.map(Box::new))
                } else {
                    (None, None)
                };
                Response::Get {
                    accepted,
                    descriptor,
                    value,
                    peers,
                }
            }
            Query::Set { .. } => {
                let mut need_descriptor = false;
                let mut value = None;
                if accepted {
                    match self
                        .store
                        .as_ref()
                        .ok_or(Error::Unavailable)?
                        .accept_value(&received)
                    {
                        Ok(SetOutcome::Accepted) => {}
                        Ok(SetOutcome::Newer(current)) => value = Some(Box::new(current)),
                        Err(Error::UnknownRecord) => need_descriptor = true,
                        Err(error) => return Err(error),
                    }
                }
                Response::Set {
                    accepted,
                    need_descriptor,
                    value,
                    peers,
                }
            }
            Query::Watch { watch_id, .. } => {
                let mut id = watch_id;
                let mut duration_us = 0;
                if accepted {
                    match self
                        .store
                        .as_ref()
                        .ok_or(Error::Unavailable)?
                        .accept_watch(&received, self.discovery.now_us()?)
                    {
                        Ok(lease) => {
                            id = lease.id;
                            duration_us = lease.duration_us;
                        }
                        Err(Error::UnknownRecord) => {}
                        Err(error) => return Err(error),
                    }
                }
                Response::Watch {
                    accepted,
                    duration_us,
                    watch_id: id,
                    peers,
                }
            }
        };
        self.discovery
            .reply_record(
                &Answer::new(received.operation(), &response)?,
                received.sender(),
            )
            .await
    }
    /// Authenticate a real node notification against its exact active lease.
    /// Included values are verified, but readers still inspect the affected subkeys.
    pub async fn handle_statement(&self, bytes: Vec<u8>) -> Result<(), Error> {
        let received = self.discovery.authenticate(bytes).await?;
        if received.operation().signer() != received.sender() {
            return Err(Error::BadSignature);
        }
        let (_, hint) = value_changed(received.operation())?;
        let mut watches = self.watches.lock().map_err(|_| Error::Unavailable)?;
        let watch = watches
            .get_mut(&(*received.sender(), hint.watch_id))
            .ok_or(Error::UnknownWatch)?;
        if watch.lost
            || self.discovery.now_ms() >= watch.expires_ms
            || hint.key != watch.descriptor.key()
        {
            watch.lost = true;
            return Err(Error::UnknownWatch);
        }
        let schema = watch
            .descriptor
            .validate_for(&hint.key, self.discovery.verifier())?;
        if hint
            .subkeys
            .last()
            .is_some_and(|subkey| subkey as usize >= schema.subkey_count())
            || hint.count > watch.count
        {
            watch.lost = true;
            return Err(Error::Encoding);
        }
        if let Some(value) = &hint.value {
            let subkey = hint.subkeys.first().ok_or(Error::Encoding)?;
            value.validate(
                &hint.key,
                &watch.descriptor.owner,
                &schema,
                subkey,
                self.discovery.verifier(),
            )?;
        }
        if hint.count == watch.count {
            return Ok(());
        } // Original duplicate-count suppression.
        watch.count = hint.count;
        watch.changed |= &hint.subkeys;
        watch.lost |= hint.count == 0 || hint.subkeys.is_empty();
        Ok(())
    }
    /// Actual listener/lifecycle loss invalidates every retained client lease.
    pub fn transport_lost(&self) {
        if let Ok(mut watches) = self.watches.lock() {
            for watch in watches.values_mut() {
                watch.lost = true;
            }
        }
    }
    async fn watch_request(
        &self,
        node: &NodeId,
        watch: &ClientWatch,
        id: u64,
        count: u32,
    ) -> Result<(u64, u64), Error> {
        let key = watch.descriptor.key();
        let question = self.discovery.with_sender(Question::new(&Query::Watch {
            key,
            subkeys: watch.subkeys.clone(),
            duration_us: 0,
            count,
            watch_id: id,
        })?)?;
        let start = self.discovery.now_ms();
        let answer = self
            .discovery
            .call_watch(node, &key, &question, &self.watcher)
            .await?;
        let response = question.answer(
            answer.operation(),
            Some(&watch.descriptor),
            self.discovery.verifier(),
        )?;
        self.observe_response(&response)?;
        let (duration_us, watch_id) = match response {
            Response::Watch {
                accepted: true,
                duration_us,
                watch_id,
                ..
            } => (duration_us, watch_id),
            Response::Watch {
                accepted: false, ..
            } => return Err(Error::WatchRefused),
            _ => return Err(Error::Encoding),
        };
        let now = self.discovery.now_ms();
        if now < start {
            return Err(Error::Unavailable);
        }
        if count == 0 {
            return if duration_us == 0 {
                Ok((watch_id, now))
            } else {
                Err(Error::Encoding)
            };
        }
        let expires = start
            .checked_add((now - start) / 2)
            .and_then(|mid| mid.checked_add(duration_us / 1000))
            .ok_or(Error::Encoding)?;
        if watch_id == 0 || expires <= now {
            return Err(Error::UnknownWatch);
        }
        Ok((watch_id, expires))
    }
    /// Drive original pending notifications and the upstream 30-second renewal
    /// window from the actual runtime timer. Failures remain observable lease loss.
    pub async fn maintain(&self) -> Result<(), Error> {
        let mut failure = None;
        if let Some(store) = &self.store {
            let notifications = store.notifications(self.discovery.now_us()?)?;
            for notification in notifications {
                let result = match Statement::value_changed(&notification.hint) {
                    Ok(statement) => {
                        self.discovery
                            .send_statement(&statement, &notification.target)
                            .await
                    }
                    Err(error) => Err(error),
                };
                // Taking a batch consumes its count budget. A failed destination
                // must not silently discard notifications for other destinations
                // or prevent this node's own expiring watches from being renewed.
                if let Err(error) = result {
                    failure.get_or_insert(error);
                }
            }
        }
        let now = self.discovery.now_ms();
        let renew: Vec<_> = self
            .watches
            .lock()
            .map_err(|_| Error::Unavailable)?
            .iter()
            .filter(|(_, w)| !w.lost && now.saturating_add(30_000) >= w.expires_ms)
            .map(|(key, w)| (*key, w.clone()))
            .collect();
        for ((node, id), previous) in renew {
            let result = if now >= previous.expires_ms {
                Err(Error::UnknownWatch)
            } else {
                self.watch_request(&node, &previous, id, previous.count)
                    .await
            };
            if let Some(watch) = self
                .watches
                .lock()
                .map_err(|_| Error::Unavailable)?
                .get_mut(&(node, id))
            {
                match result {
                    Ok((same, expires)) if same == id => watch.expires_ms = expires,
                    _ => {
                        watch.lost = true;
                        failure.get_or_insert(Error::UnknownWatch);
                    }
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}
impl<I: TorNodeIdentity, V: Verifier, W: TorWatcher> Network for TorRecords<I, V, W> {
    fn now_ms(&self) -> u64 {
        self.discovery.now_ms()
    }
    fn hints(&self, key: &RecordKey) -> Result<Vec<NodeId>, Error> {
        self.discovery.storage_hints(&key.0)
    }
    async fn closest(&self, key: &RecordKey) -> Result<Vec<NodeId>, Error> {
        let report = self.discovery.discover(key.0).await?;
        if report.peers.is_empty() {
            return Err(Error::Unavailable);
        }
        Ok(report.peers.iter().map(|p| *p.node_id()).collect())
    }
    async fn descriptor(
        &self,
        node: &NodeId,
        key: &RecordKey,
    ) -> Result<Option<Descriptor>, Error> {
        match self
            .request(
                node,
                Query::Get {
                    key: *key,
                    subkey: 0,
                    want_descriptor: true,
                },
                None,
            )
            .await?
        {
            Response::Get { descriptor, .. } => Ok(descriptor),
            _ => Err(Error::Encoding),
        }
    }
    async fn get(
        &self,
        node: &NodeId,
        key: &RecordKey,
        subkey: u32,
    ) -> Result<Option<SignedValue>, Error> {
        match self
            .request(
                node,
                Query::Get {
                    key: *key,
                    subkey,
                    want_descriptor: true,
                },
                None,
            )
            .await?
        {
            Response::Get { value, .. } => Ok(value.map(|v| *v)),
            _ => Err(Error::Encoding),
        }
    }
    async fn set(
        &self,
        node: &NodeId,
        descriptor: &Descriptor,
        subkey: u32,
        value: &SignedValue,
    ) -> Result<SetOutcome, Error> {
        match self
            .request(
                node,
                Query::Set {
                    key: descriptor.key(),
                    subkey,
                    value: Box::new(value.clone()),
                    descriptor: Some(descriptor.clone()),
                },
                Some(descriptor),
            )
            .await?
        {
            Response::Set {
                accepted: true,
                need_descriptor: false,
                value,
                ..
            } => Ok(value.map_or(SetOutcome::Accepted, |v| SetOutcome::Newer(*v))),
            Response::Set { .. } => Err(Error::Unavailable),
            _ => Err(Error::Encoding),
        }
    }
    async fn watch(&self, node: &NodeId, key: &RecordKey) -> Result<WatchId, Error> {
        {
            let watches = self.watches.lock().map_err(|_| Error::Unavailable)?;
            if watches.len() >= self.limits.watches.get() {
                return Err(Error::Unavailable);
            }
            if watches
                .iter()
                .any(|((n, _), w)| n == node && w.descriptor.key() == *key)
            {
                return Err(Error::Conflict);
            }
        }
        let descriptor = Network::descriptor(self, node, key)
            .await?
            .ok_or(Error::UnknownRecord)?;
        let schema = descriptor.validate_for(key, self.discovery.verifier())?;
        let watch = ClientWatch {
            descriptor,
            subkeys: SubkeyRanges::from_iter(0..schema.subkey_count() as u32),
            changed: SubkeyRanges::new(),
            expires_ms: 0,
            count: u32::MAX,
            lost: false,
        };
        let (id, expires_ms) = self.watch_request(node, &watch, 0, watch.count).await?;
        let mut accepted = watch.clone();
        accepted.expires_ms = expires_ms;
        let retained = {
            let mut watches = self.watches.lock().map_err(|_| Error::Unavailable)?;
            if watches.len() >= self.limits.watches.get() || watches.contains_key(&(*node, id)) {
                false
            } else {
                watches.insert((*node, id), accepted);
                true
            }
        };
        if !retained {
            let _ = self.watch_request(node, &watch, id, 0).await;
            return Err(Error::Unavailable);
        }
        Ok(WatchId(id))
    }
    async fn cancel_watch(&self, node: &NodeId, id: WatchId) -> Result<(), Error> {
        let previous = self
            .watches
            .lock()
            .map_err(|_| Error::Unavailable)?
            .remove(&(*node, id.0));
        if let Some(previous) = previous {
            self.watch_request(node, &previous, id.0, 0).await?;
        }
        Ok(())
    }
    async fn changes(&self, node: &NodeId, id: WatchId) -> Result<Vec<Change>, Error> {
        let previous = self
            .watches
            .lock()
            .map_err(|_| Error::Unavailable)?
            .get(&(*node, id.0))
            .cloned()
            .ok_or(Error::UnknownWatch)?;
        if previous.lost || self.discovery.now_ms() >= previous.expires_ms {
            return Err(Error::UnknownWatch);
        }
        let key = previous.descriptor.key();
        let mut changes = Vec::new();
        for subkey in &previous.changed {
            let value = Network::get(self, node, &key, subkey)
                .await?
                .ok_or(Error::Unavailable)?;
            changes.push(Change {
                key,
                subkey,
                seq: value.seq,
            });
        }
        if let Some(watch) = self
            .watches
            .lock()
            .map_err(|_| Error::Unavailable)?
            .get_mut(&(*node, id.0))
            && watch.count == previous.count
        {
            watch.changed = &watch.changed - &previous.changed;
        }
        Ok(changes)
    }
}

/// Shared network port for Records' original replication engine. Cloning shares
/// this node's transport owner, never another node's storage or watcher identity.
pub struct TorRecordNetwork<I, V: Verifier, W>(Arc<TorRecords<I, V, W>>);
impl<I, V: Verifier, W> Clone for TorRecordNetwork<I, V, W> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<I: TorNodeIdentity, V: Verifier, W: TorWatcher> TorRecords<I, V, W> {
    /// Borrow this actual runtime through the original transport-independent port.
    pub fn network(self: &Arc<Self>) -> TorRecordNetwork<I, V, W> {
        TorRecordNetwork(self.clone())
    }
}
impl<I: TorNodeIdentity, V: Verifier, W: TorWatcher> Network for TorRecordNetwork<I, V, W> {
    fn now_ms(&self) -> u64 {
        Network::now_ms(&*self.0)
    }
    fn hints(&self, key: &RecordKey) -> Result<Vec<NodeId>, Error> {
        self.0.hints(key)
    }
    async fn closest(&self, key: &RecordKey) -> Result<Vec<NodeId>, Error> {
        self.0.closest(key).await
    }
    async fn descriptor(
        &self,
        node: &NodeId,
        key: &RecordKey,
    ) -> Result<Option<Descriptor>, Error> {
        Network::descriptor(&*self.0, node, key).await
    }
    async fn get(
        &self,
        node: &NodeId,
        key: &RecordKey,
        subkey: u32,
    ) -> Result<Option<SignedValue>, Error> {
        self.0.get(node, key, subkey).await
    }
    async fn set(
        &self,
        node: &NodeId,
        descriptor: &Descriptor,
        subkey: u32,
        value: &SignedValue,
    ) -> Result<SetOutcome, Error> {
        self.0.set(node, descriptor, subkey, value).await
    }
    async fn watch(&self, node: &NodeId, key: &RecordKey) -> Result<WatchId, Error> {
        self.0.watch(node, key).await
    }
    async fn cancel_watch(&self, node: &NodeId, id: WatchId) -> Result<(), Error> {
        self.0.cancel_watch(node, id).await
    }
    async fn changes(&self, node: &NodeId, id: WatchId) -> Result<Vec<Change>, Error> {
        self.0.changes(node, id).await
    }
}
"#;
    const FROZEN_ORIGINAL_SHA256: &str =
        "437f7324c7882ae1b14dc4735d685992afd65a82bb180a15e1deee9bff085895";
    /// Deterministic diagnostic digest of `transform(FROZEN_LIB)`, derived
    /// mechanically and pinned here and in the consumer manifest. Any table
    /// change must update both under review.
    const EXPECTED_DIAGNOSTIC_SHA256: &str =
        "56d6de3757ac3aa976d96be74156148dd4e0ddc27adda1a332bb7633fdc4e4a2";

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
        let target = manifest.join("src/tor_records.rs");
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
        let link = manifest.join("src/tor_records.rs");
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
        let source = directory.path().join("tor_records.rs");
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
        assert!(text.contains("\"restored\": true"));
    }

    fn guarded_fixture() -> (tempfile::TempDir, PathBuf, Vec<u8>, String, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("tor_records.rs");
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
        let source = directory.path().join("tor_records.rs");
        fs::write(&source, b"marker").unwrap();
        let _first = acquire_lock(&source).unwrap();
        assert!(acquire_lock(&source).is_err());
    }
}

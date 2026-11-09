//! Content-addressed storage for the files a check declares as `cache_outputs`.
//!
//! A verdict hit skips the ordinary cached transaction, so it also skips moon's
//! own output hydration. Without this store a repository archives or uploads
//! `coverage.json` after a hit and finds nothing. Every declared output is
//! therefore stored under `CCID_RESULT_CACHE/outputs/`, content-addressed by
//! SHA-256 (identical reports share one blob) and indexed by the SAME verdict
//! key that names the pass. A producer writes the outputs and then the verdict;
//! a later hit restores the outputs into their declared paths before it reports
//! success. A verdict whose outputs cannot be restored byte-for-byte is a miss,
//! never a silent success with missing artefacts.
//!
//! Layout:
//!
//! ```text
//! outputs/
//!   blobs/<sha256>            file bytes, or a deterministic tar of a directory
//!   verdicts/<verdict key>.json   the output index (path, kind, digest, size, mode)
//!   prune.stamp               last retention pass
//!   prune.lock                serializes retention passes
//! ```
//!
//! Retention keeps every output set referenced by a verdict that is the newest
//! for its repository+branch, plus a bounded window of older sets. Only this
//! namespace is ever pruned; the verdict records, the moon cache and every
//! other cache are untouched.

use crate::{event, failure, value, Environment, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    thread,
    time::{Duration, SystemTime},
};

/// Namespace below `CCID_RESULT_CACHE`, next to the `verdicts/` store.
pub const DIR: &str = "outputs";
const BLOB_DIR: &str = "blobs";
const INDEX_DIR: &str = "verdicts";
const STAMP: &str = "prune.stamp";
const PRUNE_LOCK: &str = "prune.lock";
/// Schema of the output index. Unknown schemas are refused, never guessed.
pub const SCHEMA: u32 = 1;
/// Older output sets kept per repository+branch in addition to the newest one.
/// One coverage set is a few megabytes, so nine sets are tens of megabytes per
/// branch, not gigabytes (see `docs/verdicts.md`).
pub const WINDOW: usize = 8;
/// A declared output set is bounded: coverage reports are small, and a check
/// that produces more than this is not mirrored here (its verdict is refused).
const MAX_OUTPUT_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OUTPUT_SET_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const MAX_OUTPUTS: usize = 64;
/// An index is a tiny JSON document.
const MAX_INDEX_BYTES: u64 = 256 * 1024;
/// A blob must not be deleted while a concurrent producer is between writing
/// it and publishing its index. Fresh blobs are therefore exempt from GC.
const PRUNE_GRACE: Duration = Duration::from_secs(60 * 60);
/// Retention runs at most this often per store, not on every recorded check.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const LOCK_WAIT: Duration = Duration::from_secs(30);
/// Generated or operational directories a declared output may never name.
const RESERVED: &[&str] = &[".git", ".ccid", ".moon", "target", "node_modules"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Directory,
}

/// One stored output: the declaration's relative path and the content digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub sha256: String,
    pub size: u64,
    /// Unix permission bits of a regular file (0644 or 0755).
    pub mode: u32,
}

/// The index of one verdict's outputs, named by the verdict key itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    pub schema: u32,
    pub key: String,
    pub check: String,
    /// Provenance, never a key input.
    pub repository: String,
    pub branch: String,
    pub commit: String,
    pub finished_at: u64,
    pub producer: String,
    pub entries: Vec<Entry>,
}

/// Everything one store/restore/readiness operation needs about the producer.
pub struct Subject<'a> {
    /// `CCID_RESULT_CACHE`.
    pub cache: &'a Path,
    /// The repository checkout or archive export.
    pub root: &'a Path,
    pub key: &'a str,
    pub check: &'a str,
    pub repository: &'a str,
    pub branch: &'a str,
    pub commit: &'a str,
    /// The check's declared `cache_outputs`, exactly as declared.
    pub outputs: &'a [String],
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A declared output is a strict repository-relative path: no absolute or
/// escaping component, no empty path, no generated/operational directory.
pub fn output_relative(path: &str) -> Result<PathBuf> {
    if path.is_empty() || path.contains('\0') {
        return Err(failure(
            "Declared cache output must be a nonempty relative path",
        ));
    }
    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(name) => {
                if RESERVED.iter().any(|reserved| name == *reserved) {
                    return Err(failure(format!(
                        "Declared cache output {path:?} names a generated or operational path"
                    )));
                }
                normalized.push(name);
            }
            _ => {
                return Err(failure(format!(
                    "Declared cache output {path:?} must be a plain relative path"
                )))
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(failure("Declared cache output must not be empty"));
    }
    Ok(normalized)
}

fn ensure_dir(directory: &Path) -> Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(failure("Output store path is not a directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o2775);
                match builder.create(directory) {
                    Ok(()) => {
                        let _ = fs::set_permissions(directory, fs::Permissions::from_mode(0o2775));
                        Ok(())
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                    Err(error) => Err(error.into()),
                }
            }
            #[cfg(not(unix))]
            {
                fs::create_dir(directory)?;
                Ok(())
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn group_writable(directory: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(directory).is_ok_and(|meta| meta.mode() & 0o020 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        false
    }
}

fn lock_file(path: &Path) -> Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if group_writable(path.parent().unwrap_or(Path::new("."))) {
            let _ = lock.set_permissions(fs::Permissions::from_mode(0o660));
        }
    }
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(failure("Output store lock timed out"))
            }
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

fn blob_path(blob_dir: &Path, digest: &str) -> PathBuf {
    blob_dir.join(digest)
}

fn index_path(index_dir: &Path, key: &str) -> PathBuf {
    index_dir.join(format!("{key}.json"))
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Write one immutable blob. The name is its digest; a damaged existing blob is
/// replaced with the correct bytes (content addressing makes that recovery, not
/// an overwrite of user data).
fn write_blob(blob_dir: &Path, digest: &str, bytes: &[u8]) -> Result<()> {
    ensure_dir(blob_dir)?;
    let path = blob_path(blob_dir, digest);
    if let Ok(meta) = fs::symlink_metadata(&path) {
        if meta.is_file() && meta.len() == bytes.len() as u64 {
            let mut existing = Vec::new();
            if File::open(&path)
                .and_then(|file| file.take(bytes.len() as u64 + 1).read_to_end(&mut existing))
                .is_ok()
                && existing == bytes
            {
                return Ok(());
            }
        }
    }
    let mut file = tempfile::NamedTempFile::new_in(blob_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if group_writable(blob_dir) {
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o660))?;
        }
    }
    file.write_all(bytes)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(&path)
        .map_err(|error| failure(format!("Cannot publish output blob: {error}")))?;
    Ok(())
}

fn read_blob(blob_dir: &Path, entry: &Entry) -> Result<Vec<u8>> {
    let path = blob_path(blob_dir, &entry.sha256);
    let meta = fs::symlink_metadata(&path)
        .map_err(|_| failure(format!("Stored output {} is missing", entry.path)))?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(failure(format!(
            "Stored output {} is not a regular file",
            entry.path
        )));
    }
    if meta.len() != entry.size {
        return Err(failure(format!(
            "Stored output {} has changed size",
            entry.path
        )));
    }
    let mut bytes = Vec::new();
    File::open(&path)?
        .take(entry.size.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != entry.size || digest_bytes(&bytes) != entry.sha256 {
        return Err(failure(format!(
            "Stored output {} does not match its content digest",
            entry.path
        )));
    }
    Ok(bytes)
}

fn relative_name(base: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(base)
        .map_err(|_| failure("Output path escaped its declared root"))?;
    let text = relative
        .to_str()
        .ok_or_else(|| failure("Output paths must be valid UTF-8"))?;
    Ok(text.replace('\\', "/"))
}

fn collect_directory(
    base: &Path,
    directory: &Path,
    out: &mut Vec<(String, PathBuf, bool)>,
) -> Result<()> {
    let mut children: Vec<_> = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|child| child.file_name());
    for child in children {
        let path = child.path();
        let meta = fs::symlink_metadata(&path)?;
        let name = relative_name(base, &path)?;
        if meta.file_type().is_symlink() {
            return Err(failure(format!(
                "Declared output directory contains a symlink: {name}"
            )));
        } else if meta.is_dir() {
            out.push((name, path.clone(), true));
            collect_directory(base, &path, out)?;
        } else if meta.is_file() {
            out.push((name, path, false));
        } else {
            return Err(failure(format!(
                "Declared output directory contains a special file: {name}"
            )));
        }
    }
    Ok(())
}

/// Deterministic tar bytes for a directory: sorted entries, zero mtime/uid/gid,
/// explicit modes, no extension records. Store and verification build the same
/// bytes from the same tree.
fn directory_bytes(directory: &Path) -> Result<Vec<u8>> {
    let mut entries = Vec::new();
    collect_directory(directory, directory, &mut entries)?;
    let mut builder = tar::Builder::new(Vec::new());
    for (name, path, is_dir) in entries {
        let meta = fs::symlink_metadata(&path)?;
        let mut header = tar::Header::new_gnu();
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        if is_dir {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header.set_cksum();
            builder.append_data(&mut header, format!("{name}/"), std::io::empty())?;
        } else {
            let size = meta.len();
            if size > MAX_OUTPUT_FILE_BYTES {
                return Err(failure(format!(
                    "Declared output file {name} exceeds the output store bound"
                )));
            }
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o777
            };
            #[cfg(not(unix))]
            let mode = 0o644;
            let mut bytes = Vec::new();
            File::open(&path)?.read_to_end(&mut bytes)?;
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 });
            header.set_size(size);
            header.set_cksum();
            builder.append_data(&mut header, name, bytes.as_slice())?;
        }
    }
    Ok(builder.into_inner()?)
}

/// The bytes and entry metadata of one declared output.
fn snapshot(root: &Path, output: &str) -> Result<(Entry, Vec<u8>)> {
    let relative = output_relative(output)?;
    let path = root.join(&relative);
    let meta = fs::symlink_metadata(&path).map_err(|_| {
        failure(format!(
            "The check declared cache output {output:?} but produced no such file"
        ))
    })?;
    if meta.file_type().is_symlink() {
        return Err(failure(format!(
            "Declared cache output {output:?} must not be a symlink"
        )));
    }
    let name = relative
        .to_str()
        .ok_or_else(|| failure("Declared cache output must be valid UTF-8"))?
        .to_owned();
    let (kind, mode, bytes) = if meta.is_file() {
        if meta.len() > MAX_OUTPUT_FILE_BYTES {
            return Err(failure(format!(
                "Declared cache output {output:?} exceeds the output store bound"
            )));
        }
        let mut bytes = Vec::new();
        File::open(&path)?.read_to_end(&mut bytes)?;
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o111 != 0 {
                0o755
            } else {
                0o644
            }
        };
        #[cfg(not(unix))]
        let mode = 0o644;
        (Kind::File, mode, bytes)
    } else if meta.is_dir() {
        (Kind::Directory, 0o755, directory_bytes(&path)?)
    } else {
        return Err(failure(format!(
            "Declared cache output {output:?} is not a regular file or directory"
        )));
    };
    let entry = Entry {
        path: name,
        kind,
        sha256: digest_bytes(&bytes),
        size: bytes.len() as u64,
        mode,
    };
    Ok((entry, bytes))
}

fn validate_entry(entry: &Entry) -> Result<()> {
    output_relative(&entry.path)?;
    if !is_hex(&entry.sha256, 64) {
        return Err(failure("Stored output digest must be a sha256 digest"));
    }
    if entry.size > MAX_OUTPUT_SET_BYTES {
        return Err(failure("Stored output exceeds the output store bound"));
    }
    if entry.mode != 0o644 && entry.mode != 0o755 {
        return Err(failure("Stored output mode must be 0644 or 0755"));
    }
    Ok(())
}

fn write_index(index_dir: &Path, index: &Index) -> Result<()> {
    ensure_dir(index_dir)?;
    let path = index_path(index_dir, &index.key);
    let mut file = tempfile::NamedTempFile::new_in(index_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if group_writable(index_dir) {
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o660))?;
        }
    }
    serde_json::to_writer(&mut file, index)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(&path)
        .map_err(|error| failure(format!("Cannot publish output index: {error}")))?;
    #[cfg(unix)]
    {
        let _ = File::open(index_dir).and_then(|dir| dir.sync_all());
    }
    Ok(())
}

fn read_index(index_dir: &Path, key: &str) -> Result<Option<Index>> {
    let path = index_path(index_dir, key);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(failure("Output index must be a regular file"));
    }
    if meta.len() > MAX_INDEX_BYTES {
        return Err(failure("Output index exceeds its size bound"));
    }
    let mut bytes = Vec::new();
    File::open(&path)?
        .take(MAX_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let index: Index =
        serde_json::from_slice(&bytes).map_err(|_| failure("Output index is not a valid index"))?;
    if index.schema != SCHEMA {
        return Err(failure("Unknown output-index schema"));
    }
    if index.key != key || !is_hex(&index.key, 64) {
        return Err(failure("Output index names a different verdict key"));
    }
    if index.check.is_empty() {
        return Err(failure("Output index has no check name"));
    }
    if !is_hex(&index.commit, 40) {
        return Err(failure("Output index commit must be a full Git commit"));
    }
    if index.entries.is_empty() || index.entries.len() > MAX_OUTPUTS {
        return Err(failure("Output index has an invalid entry count"));
    }
    for entry in &index.entries {
        validate_entry(entry)?;
    }
    Ok(Some(index))
}

/// Store every declared output of one successful check, then its index. The
/// caller writes the verdict record only after this returns.
pub fn store(subject: &Subject, producer: &str, finished_at: u64) -> Result<()> {
    if subject.outputs.is_empty() {
        return Ok(());
    }
    if subject.outputs.len() > MAX_OUTPUTS {
        return Err(failure("Too many declared cache outputs"));
    }
    let root = subject.cache.join(DIR);
    ensure_dir(&root)?;
    let blob_dir = root.join(BLOB_DIR);
    let index_dir = root.join(INDEX_DIR);
    ensure_dir(&blob_dir)?;
    ensure_dir(&index_dir)?;
    let mut entries = Vec::new();
    let mut total = 0u64;
    for output in subject.outputs {
        let (entry, bytes) = snapshot(subject.root, output)?;
        total = total.saturating_add(entry.size);
        if total > MAX_OUTPUT_SET_BYTES {
            return Err(failure(
                "Declared cache outputs exceed the output store bound",
            ));
        }
        write_blob(&blob_dir, &entry.sha256, &bytes)?;
        entries.push(entry);
    }
    let index = Index {
        schema: SCHEMA,
        key: subject.key.to_owned(),
        check: subject.check.to_owned(),
        repository: subject.repository.to_owned(),
        branch: subject.branch.to_owned(),
        commit: subject.commit.to_owned(),
        finished_at,
        producer: producer.to_owned(),
        entries,
    };
    write_index(&index_dir, &index)?;
    event(json!({
        "event": "verdict-outputs-stored",
        "check": subject.check,
        "key": subject.key,
        "outputs": index.entries.len(),
        "bytes": total,
    }));
    Ok(())
}

/// Whether the output set exists and every blob is present with the recorded
/// size. Used by `--plan`; it never reads whole coverage reports.
pub fn ready(subject: &Subject) -> Result<()> {
    if subject.outputs.is_empty() {
        return Ok(());
    }
    let root = subject.cache.join(DIR);
    let index = read_index(&root.join(INDEX_DIR), subject.key)?
        .ok_or_else(|| failure("the verdict has no stored outputs"))?;
    if index.check != subject.check {
        return Err(failure("the stored outputs belong to another check"));
    }
    if index.entries.len() != subject.outputs.len() {
        return Err(failure(
            "the stored output set does not match the declaration",
        ));
    }
    for output in subject.outputs {
        let relative = output_relative(output)?;
        let name = relative.to_str().unwrap_or_default();
        let entry = index
            .entries
            .iter()
            .find(|entry| entry.path == name)
            .ok_or_else(|| failure(format!("the stored output set lacks {name:?}")))?;
        let meta = fs::symlink_metadata(blob_path(&root.join(BLOB_DIR), &entry.sha256))
            .map_err(|_| failure(format!("the stored output {name:?} is missing")))?;
        if !meta.file_type().is_file() || meta.len() != entry.size {
            return Err(failure(format!("the stored output {name:?} is incomplete")));
        }
    }
    Ok(())
}

fn clear_existing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => {
            fs::remove_dir_all(path).map_err(|error| {
                failure(format!(
                    "Cannot replace stored output {}: {error}",
                    path.display()
                ))
            })?;
        }
        Ok(_) => {
            fs::remove_file(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn install_file(root: &Path, entry: &Entry, bytes: &[u8]) -> Result<()> {
    let path = root.join(output_relative(&entry.path)?);
    let parent = path
        .parent()
        .ok_or_else(|| failure("Stored output has no parent directory"))?;
    fs::create_dir_all(parent)?;
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_dir()) {
        clear_existing(&path)?;
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(entry.mode))?;
    }
    file.persist(&path)
        .map_err(|error| failure(format!("Cannot restore stored output: {error}")))?;
    Ok(())
}

fn install_directory(root: &Path, entry: &Entry, bytes: &[u8]) -> Result<()> {
    let path = root.join(output_relative(&entry.path)?);
    let parent = path
        .parent()
        .ok_or_else(|| failure("Stored output has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".ccid-output-")
        .tempdir_in(parent)?;
    let mut archive = tar::Archive::new(bytes);
    archive.unpack(staging.path())?;
    // The restored tree must reproduce exactly the bytes that were stored.
    if digest_bytes(&directory_bytes(staging.path())?) != entry.sha256 {
        return Err(failure(format!(
            "Restored output {} does not match its content digest",
            entry.path
        )));
    }
    if fs::symlink_metadata(&path).is_ok() {
        clear_existing(&path)?;
    }
    let staged = staging.keep();
    fs::rename(&staged, &path)
        .map_err(|error| failure(format!("Cannot restore stored output: {error}")))?;
    // The staging root is private tempdir mode; the restored tree is a declared
    // artefact and must stay readable like the directory the check produced.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Restore one verdict's outputs into the checkout. An absent index, a missing
/// or corrupt blob is an error: the caller must treat the lookup as a miss and
/// run the check, never report a hit with missing artefacts.
pub fn restore(subject: &Subject) -> Result<()> {
    if subject.outputs.is_empty() {
        return Ok(());
    }
    let root = subject.cache.join(DIR);
    let index = read_index(&root.join(INDEX_DIR), subject.key)?
        .ok_or_else(|| failure("the verdict has no stored outputs"))?;
    if index.check != subject.check {
        return Err(failure("the stored outputs belong to another check"));
    }
    let blob_dir = root.join(BLOB_DIR);
    let mut bytes = 0u64;
    for output in subject.outputs {
        let relative = output_relative(output)?;
        let name = relative.to_str().unwrap_or_default();
        let entry = index
            .entries
            .iter()
            .find(|entry| entry.path == name)
            .ok_or_else(|| failure(format!("the stored output set lacks {name:?}")))?;
        let content = read_blob(&blob_dir, entry)?;
        bytes = bytes.saturating_add(entry.size);
        match entry.kind {
            Kind::File => install_file(subject.root, entry, &content)?,
            Kind::Directory => install_directory(subject.root, entry, &content)?,
        }
    }
    event(json!({
        "event": "verdict-outputs-restored",
        "check": subject.check,
        "key": subject.key,
        "outputs": index.entries.len(),
        "bytes": bytes,
    }));
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub kept: usize,
    pub removed_sets: usize,
    pub removed_blobs: usize,
    pub bytes: u64,
}

fn mtime_older_than(path: &Path, age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|elapsed| elapsed >= age)
}

/// Retention: keep the newest output set per repository+branch and a bounded
/// window of older ones. Rewrites only this store's index and blob namespace.
pub fn prune(cache: &Path) -> Result<PruneReport> {
    let root = cache.join(DIR);
    let index_dir = root.join(INDEX_DIR);
    let blob_dir = root.join(BLOB_DIR);
    let mut report = PruneReport::default();
    let mut indexes: Vec<Index> = Vec::new();
    let mut complete = true;
    let entries = match fs::read_dir(&index_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error.into()),
    };
    for item in entries {
        let item = item?;
        let path = item.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(key) = name.strip_suffix(".json") else {
            continue;
        };
        if !is_hex(key, 64) {
            continue;
        }
        match read_index(&index_dir, key) {
            Ok(Some(index)) => indexes.push(index),
            // Never delete what cannot be parsed as an index: an operator may
            // need it, and a partial write is not ours to guess at.
            Ok(None) => {}
            // An index that cannot be parsed might still reference blobs. Retention never
            // guesses: it keeps the set and skips blob collection for this pass.
            Err(_) => complete = false,
        }
    }
    let mut groups: BTreeMap<(String, String), Vec<&Index>> = BTreeMap::new();
    for index in &indexes {
        groups
            .entry((index.repository.clone(), index.branch.clone()))
            .or_default()
            .push(index);
    }
    let mut keep: BTreeSet<String> = BTreeSet::new();
    for (_, mut group) in groups {
        group.sort_by(|a, b| {
            b.finished_at
                .cmp(&a.finished_at)
                .then_with(|| a.key.cmp(&b.key))
        });
        for index in group.iter().take(WINDOW + 1) {
            keep.insert(index.key.clone());
        }
    }
    for index in &indexes {
        if keep.contains(&index.key) {
            continue;
        }
        let path = index_path(&index_dir, &index.key);
        if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_file())
            && fs::remove_file(&path).is_ok()
        {
            report.removed_sets += 1;
        }
    }
    report.kept = keep.len();
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for index in &indexes {
        if keep.contains(&index.key) {
            for entry in &index.entries {
                referenced.insert(entry.sha256.clone());
            }
        }
    }
    if complete {
        if let Ok(entries) = fs::read_dir(&blob_dir) {
            for item in entries.flatten() {
                let path = item.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if !is_hex(name, 64) || referenced.contains(name) {
                    continue;
                }
                if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_file()) {
                    continue;
                }
                // A producer may be between writing a blob and publishing its index.
                if !mtime_older_than(&path, PRUNE_GRACE) {
                    continue;
                }
                let size = fs::metadata(&path).map_or(0, |meta| meta.len());
                if fs::remove_file(&path).is_ok() {
                    report.removed_blobs += 1;
                    report.bytes = report.bytes.saturating_add(size);
                }
            }
        }
    }
    if report.removed_sets > 0 || report.removed_blobs > 0 {
        event(json!({
            "event": "verdict-outputs-prune",
            "kept": report.kept,
            "removed_sets": report.removed_sets,
            "removed_blobs": report.removed_blobs,
            "bytes": report.bytes,
        }));
    }
    Ok(report)
}

/// Run retention at most once per store and hour. Best effort: a store that
/// cannot be pruned still serves every stored output correctly.
pub fn maybe_prune(cache: &Path) {
    let root = cache.join(DIR);
    let stamp = root.join(STAMP);
    if fs::metadata(&stamp)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|elapsed| elapsed < PRUNE_INTERVAL)
    {
        return;
    }
    if ensure_dir(&root).is_err() {
        return;
    }
    let Ok(_lock) = lock_file(&root.join(PRUNE_LOCK)) else {
        return;
    };
    if fs::metadata(&stamp)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|elapsed| elapsed < PRUNE_INTERVAL)
    {
        return;
    }
    if prune(cache).is_ok() {
        let _ = File::create(&stamp).and_then(|file| file.sync_all());
    }
}

/// The branch a producer names for provenance and retention grouping. Ambient
/// metadata never enters the key.
pub fn branch(root: &Path, environment: &Environment) -> String {
    value(environment, "CCID_BRANCH")
        .or_else(|| value(environment, "CI_COMMIT_BRANCH"))
        .or_else(|| value(environment, "CI_COMMIT_REF_NAME"))
        .or_else(|| value(environment, "GITHUB_REF_NAME"))
        .or_else(|| {
            let output = std::process::Command::new("git")
                .args(["symbolic-ref", "--short", "HEAD"])
                .current_dir(root)
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            let branch = String::from_utf8(output.stdout).ok()?.trim().to_owned();
            (!branch.is_empty()).then_some(branch)
        })
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests;

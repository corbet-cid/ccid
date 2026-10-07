//! Cache ownership and freshness for verified, disposable archive sources.
use super::*;
use serde::Serialize;
use std::{
    fs::{File, OpenOptions, TryLockError},
    io::{Read, Write},
    path::PathBuf,
    process::Command,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) fn repository_identity(root: &Path, env: &Environment, archive: bool) -> Result<String> {
    if archive
        && ![
            "CARGO_HOME",
            "CI_CACHE_ROOT",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_TARGET_DIR",
        ]
        .iter()
        .any(|name| value(env, name).is_some())
    {
        return Err(failure("Archive checks require an explicit persistent CARGO_HOME or target-cache root; per-job HOME is not a cache"));
    }
    if let Some(url) = value(env, "CI_REPOSITORY_URL") {
        return canonical_repository(&url);
    }
    if archive {
        return Err(failure(
            "Archive checks require canonical CI_REPOSITORY_URL",
        ));
    }
    let origin = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(root)
        .output();
    if let Ok(output) = origin {
        if output.status.success() {
            return canonical_repository(std::str::from_utf8(&output.stdout)?.trim());
        }
    }
    // A local directory without an origin never shares a manifest-slug cache.
    Ok(format!("local:{}", root.display()))
}

pub(crate) fn canonical_repository(url: &str) -> Result<String> {
    let location = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        rest.to_owned()
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        rest.rsplit_once('@')
            .map_or(rest, |(_, path)| path)
            .to_owned()
    } else if let Some((host, path)) = url.strip_prefix("git@").and_then(|v| v.split_once(':')) {
        format!("{host}/{path}")
    } else {
        return Err(failure(
            "Repository identity requires an HTTP(S) or SSH forge URL",
        ));
    };
    let location = location.trim_end_matches('/').trim_end_matches(".git");
    let parts: Vec<_> = location.split('/').collect();
    if parts.len() < 3
        || parts.iter().any(|part| {
            part.is_empty()
                || *part == "."
                || *part == ".."
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-:".contains(&b))
        })
    {
        return Err(failure("Repository identity must contain a plain forge host and repository path, without credentials"));
    }
    Ok(format!(
        "{}/{}",
        parts[0].to_ascii_lowercase(),
        parts[1..].join("/")
    ))
}

pub(crate) fn target_directory(root: &Path, identity: &str, env: &Environment) -> Result<PathBuf> {
    let explicit = value(env, "CARGO_TARGET_DIR").or_else(|| value(env, "CARGO_BUILD_TARGET_DIR"));
    let path = if let Some(path) = explicit {
        PathBuf::from(path)
    } else {
        let base =
            if let Some(path) = value(env, "CI_CACHE_ROOT").or_else(|| value(env, "CARGO_HOME")) {
                PathBuf::from(path)
            } else {
                PathBuf::from(
                    value(env, "HOME")
                        .or_else(|| value(env, "USERPROFILE"))
                        .ok_or_else(|| failure("Set CARGO_HOME or an explicit CARGO_TARGET_DIR"))?,
                )
                .join(".cargo")
            };
        let readable: String = identity
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
        let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
        base.join("targets").join(format!("{readable}-{digest}"))
    };
    Ok(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

pub(crate) fn lock_target(target: &Path, overall_deadline: Instant) -> Result<(PathBuf, File)> {
    fs::create_dir_all(target)?;
    let target = target.canonicalize()?;
    let metadata = target.join(".ccid");
    fs::create_dir_all(&metadata)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(metadata.join("lock"))?;
    let lock_deadline = Instant::now()
        .checked_add(Duration::from_secs(60))
        .ok_or_else(|| failure("Target lock deadline is out of range"))?
        .min(overall_deadline);
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok((target, lock)),
            Err(TryLockError::WouldBlock)
                if Instant::now() < lock_deadline && !INTERRUPTED.load(Ordering::SeqCst) =>
            {
                thread::sleep(Duration::from_millis(100))
            }
            Err(TryLockError::WouldBlock) => {
                return Err(failure(
                    "Target cache is in use or check interrupted; no duplicate work started",
                ))
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

/// A Unix socket path (`sockaddr_un.sun_path`) holds 108 bytes including the
/// terminating NUL, so 107 bytes of path.
#[cfg(unix)]
const SUN_PATH_MAX: usize = 107;
/// Room a checked program keeps below its scratch root for its own socket
/// paths (a `/` plus nested directories and the socket file name).
#[cfg(unix)]
const SOCKET_SUFFIX_BUDGET: usize = 64;
/// Length of the per-job leaf `ccid-job-` plus six random characters.
#[cfg(unix)]
const JOB_LEAF_LEN: usize = "ccid-job-".len() + 6;

/// Per-job scratch directory; exports it as `TMPDIR` to the checked programs.
///
/// The scratch path never enters a cache key (`TMPDIR` and `RUNNER_TEMP` are
/// refused in `cache_env`), so where it lives is free to change. Checked
/// programs create Unix sockets below `TMPDIR`, and a nested worker or
/// `nix-shell` `TMPDIR` pushes such paths past `sun_path`. The scratch therefore
/// stays beneath the inherited `TMPDIR` only while that leaves
/// [`SOCKET_SUFFIX_BUDGET`] bytes for the program; otherwise it becomes a
/// single-level directory in a short per-user base (`/tmp/c<uid>`). If that
/// base is unusable the job degrades to the inherited parent, as before.
pub(crate) fn scratch(env: &mut Environment) -> Result<tempfile::TempDir> {
    scratch_with_short_base(env, Path::new("/tmp"))
}

fn scratch_with_short_base(
    env: &mut Environment,
    short_parent: &Path,
) -> Result<tempfile::TempDir> {
    let parent = scratch_parent(env)?;
    #[cfg(unix)]
    if parent.as_os_str().len() + 1 + JOB_LEAF_LEN + SOCKET_SUFFIX_BUDGET > SUN_PATH_MAX {
        if let Some(scratch) = short_base(short_parent).and_then(|base| job_scratch_in(&base).ok())
        {
            set(env, "TMPDIR", scratch.path().as_os_str());
            return Ok(scratch);
        }
    }
    #[cfg(not(unix))]
    let _ = short_parent;
    scratch_in(env, parent)
}

fn scratch_parent(env: &Environment) -> Result<PathBuf> {
    let parent = value(env, "TMPDIR").map(PathBuf::from).unwrap_or_else(|| {
        #[cfg(unix)]
        {
            PathBuf::from("/tmp")
        }
        #[cfg(not(unix))]
        {
            std::env::temp_dir()
        }
    });
    if parent.as_os_str().is_empty() {
        return Err(failure("Temporary-directory parent must not be empty"));
    }
    Ok(parent)
}

fn job_scratch_in(parent: &Path) -> io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("ccid-job-")
        .tempdir_in(parent)
}

fn scratch_in(env: &mut Environment, parent: PathBuf) -> Result<tempfile::TempDir> {
    let scratch = job_scratch_in(&parent)?;
    set(env, "TMPDIR", scratch.path().as_os_str());
    #[cfg(windows)]
    for name in ["TEMP", "TMP"] {
        set(env, name, scratch.path().as_os_str());
    }
    Ok(scratch)
}

/// The short per-user base `<parent>/c<uid>`. It is created owner-only; an
/// existing path is accepted only as a real directory (never a link) owned by
/// this user without group or other access. Anything else, or an unwritable
/// parent, returns `None` and the caller degrades to the inherited `TMPDIR`.
#[cfg(unix)]
fn short_base(parent: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let uid = rustix::process::geteuid().as_raw();
    let base = parent.join(format!("c{uid}"));
    match fs::DirBuilder::new().mode(0o700).create(&base) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
        Err(_) => return None,
    }
    let meta = fs::symlink_metadata(&base).ok()?;
    (meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0).then_some(base)
}

/// Disposable contents at a stable path. Never retain generated files between
/// runs. The enclosing target lock protects both this path and Cargo's outputs.
pub(crate) struct StableSource(PathBuf);
impl StableSource {
    /// Cached Cargo runs have generated executor state in their checkout.
    /// Strip only the copies inside this guard's owned disposable source.
    pub(crate) fn prepare_cached(source: &Path, target: &Path) -> Result<Self> {
        Self::prepare_inner(source, target, true)
    }

    pub(crate) fn prepare(source: &Path, target: &Path) -> Result<Self> {
        Self::prepare_inner(source, target, false)
    }

    fn prepare_inner(source: &Path, target: &Path, cached: bool) -> Result<Self> {
        if target.starts_with(source) {
            return Err(failure(
                "Stable archive source cannot contain its target cache",
            ));
        }
        let root = target.join(".ccid/source-v1");
        let marker = target.join(".ccid/source-v1.owner");
        const OWNER: &[u8] = b"ccid-verified-source-v1\n";
        match fs::symlink_metadata(&marker) {
            Ok(m) if m.is_file() && fs::read(&marker)? == OWNER => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if root.symlink_metadata().is_ok() {
                    return Err(failure(
                        "Refusing to replace an unowned stable source directory",
                    ));
                }
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&marker)?;
                file.write_all(OWNER)?;
            }
            _ => return Err(failure("Invalid stable source ownership marker")),
        }
        match fs::symlink_metadata(&root) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&root)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            _ => {
                return Err(failure(
                    "Stable source path must be an owned directory, never a link",
                ))
            }
        }
        fs::create_dir(&root)?;
        let owned = Self(root);
        copy_source(source, owned.path(), cached)?;
        event(json!({"event":"stable-source","path":owned.path()}));
        Ok(owned)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// The target lock also owns this fixed scratch location. A changing
    /// temporary path in compiler flags would invalidate Cargo fingerprints.
    /// Checked programs create Unix sockets below TMPDIR and `sun_path` holds
    /// fewer than 108 bytes, so the root stays short on Unix instead of living
    /// inside the long, digest-named target cache.
    pub(crate) fn scratch(target: &Path, environment: &mut Environment) -> Result<Self> {
        #[cfg(unix)]
        if let Some(owned) = Self::short_scratch(target) {
            for name in ["TMPDIR", "RUNNER_TEMP", "TEMP", "TMP"] {
                set(environment, name, owned.path().as_os_str());
            }
            return Ok(owned);
        }
        let root = target.join(".ccid/scratch-v1");
        let marker = target.join(".ccid/scratch-v1.owner");
        const OWNER: &[u8] = b"ccid-scratch-v1\n";
        match fs::symlink_metadata(&marker) {
            Ok(meta) if meta.is_file() && fs::read(&marker)? == OWNER => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if root.symlink_metadata().is_ok() {
                    return Err(failure("Refusing to replace unowned stable scratch"));
                }
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&marker)?
                    .write_all(OWNER)?;
            }
            _ => return Err(failure("Invalid stable scratch ownership marker")),
        }
        match fs::symlink_metadata(&root) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&root)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            _ => return Err(failure("Stable scratch must be an owned directory")),
        }
        fs::create_dir(&root)?;
        for name in ["TMPDIR", "RUNNER_TEMP", "TEMP", "TMP"] {
            set(environment, name, root.as_os_str());
        }
        Ok(Self(root))
    }

    /// Deterministic short scratch directory for one target cache. Only a
    /// missing path or a real directory owned by this user is replaced; any
    /// other state, or an unwritable /tmp, selects the target-local location.
    #[cfg(unix)]
    fn short_scratch(target: &Path) -> Option<Self> {
        use std::os::unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt},
        };
        let digest = format!("{:x}", Sha256::digest(target.as_os_str().as_bytes()));
        let root = Path::new("/tmp").join(format!("ccid-{}", &digest[..16]));
        match fs::symlink_metadata(&root) {
            Ok(meta) if meta.is_dir() && meta.uid() == rustix::process::geteuid().as_raw() => {
                fs::remove_dir_all(&root).ok()?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            _ => return None,
        }
        fs::DirBuilder::new().mode(0o700).create(&root).ok()?;
        Some(Self(root))
    }
}

impl Drop for StableSource {
    fn drop(&mut self) {
        // remove_dir_all does not follow symlinks. Only this versioned,
        // ccid-owned directory is disposable; no cache artifact is removed.
        if let Err(error) = fs::remove_dir_all(&self.0) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!(
                    "ccid: cannot remove owned source {}: {error}",
                    self.0.display()
                );
            }
        }
    }
}

fn copy_source(source: &Path, destination: &Path, cached: bool) -> Result<()> {
    for child in fs::read_dir(source)? {
        let child = child?;
        if cached
            && [
                ".git",
                ".moon",
                ".ccid",
                "target",
                "node_modules",
                "moon.yml",
            ]
            .iter()
            .any(|name| child.file_name() == *name)
        {
            continue;
        }
        let from = child.path();
        let to = destination.join(child.file_name());
        let kind = child.file_type()?;
        if kind.is_dir() {
            fs::create_dir(&to)?;
            copy_source(&from, &to, cached)?;
        } else if kind.is_file() {
            fs::copy(&from, &to)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(&from)?, &to)?;
            #[cfg(windows)]
            if from.is_dir() {
                std::os::windows::fs::symlink_dir(fs::read_link(&from)?, &to)?;
            } else {
                std::os::windows::fs::symlink_file(fs::read_link(&from)?, &to)?;
            }
        } else {
            return Err(failure("Unexpected special file in verified source"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct Stamp {
    seconds: u64,
    nanos: u32,
}
impl Stamp {
    fn from_time(time: SystemTime) -> Result<Self> {
        let d = time.duration_since(UNIX_EPOCH)?;
        Ok(Self {
            seconds: d.as_secs(),
            nanos: d.subsec_nanos(),
        })
    }
    fn time(self) -> Result<SystemTime> {
        if self.nanos >= 1_000_000_000 {
            return Err(failure("Invalid source-state timestamp"));
        }
        UNIX_EPOCH
            .checked_add(Duration::new(self.seconds, self.nanos))
            .ok_or_else(|| failure("Source-state timestamp is out of range"))
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    digest: String,
    kind: String,
    mtime: Stamp,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema: u32,
    identity: String,
    #[serde(default)]
    source_root: Option<PathBuf>,
    completed: Stamp,
    entries: BTreeMap<PathBuf, Entry>,
}

fn scan(root: &Path, relative: &Path, entries: &mut BTreeMap<PathBuf, Entry>) -> Result<String> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)?;
    let mut hash = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        hash.update(metadata.permissions().mode().to_le_bytes());
    }
    let kind = if metadata.is_symlink() {
        hash.update(b"link");
        hash.update(fs::read_link(&path)?.as_os_str().as_encoded_bytes());
        "link"
    } else if metadata.is_file() {
        hash.update(b"file");
        let mut file = File::open(path)?;
        let mut buffer = [0; 65536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        "file"
    } else if metadata.is_dir() {
        hash.update(b"directory");
        let mut children = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let name = child.file_name();
            hash.update((name.as_encoded_bytes().len() as u64).to_le_bytes());
            hash.update(name.as_encoded_bytes());
            hash.update(scan(root, &relative.join(name), entries)?.as_bytes());
        }
        "directory"
    } else {
        return Err(failure(
            "Source freshness only supports regular files, directories and verified symlinks",
        ));
    };
    let digest = format!("{:x}", hash.finalize());
    entries.insert(
        relative.into(),
        Entry {
            digest: digest.clone(),
            kind: kind.into(),
            mtime: Stamp::from_time(metadata.modified()?)?,
        },
    );
    Ok(digest)
}

pub(crate) fn source_tree_digest(root: &Path) -> Result<String> {
    scan(root, Path::new(""), &mut BTreeMap::new())
}

/// Only used while the actual target lock is held, on ccid-owned verified source.
/// State is removed BEFORE reuse; a failed/killed run can never publish freshness.
pub(crate) struct Freshness {
    path: PathBuf,
    root: PathBuf,
    state: State,
}
impl Freshness {
    pub(crate) fn invalidate(target: &Path) -> Result<()> {
        match fs::remove_file(target.join(".ccid/source-state.json")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub(crate) fn prepare(root: &Path, target: &Path, identity: &str) -> Result<Self> {
        if target.starts_with(root) {
            return Err(failure(
                "Archive target cache must be outside its disposable source",
            ));
        }
        let path = target.join(".ccid/source-state.json");
        let previous = match fs::read(&path) {
            Ok(bytes) => {
                fs::remove_file(&path)?; // Only ccid-owned metadata; preserve all compiled artifacts.
                let state: State = serde_json::from_slice(&bytes)?;
                if state.schema != 1 {
                    return Err(failure("Unknown source-state schema"));
                }
                Some(state)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let now = SystemTime::now();
        let fresh = if let Some(old) = &previous {
            now.max(
                old.completed
                    .time()?
                    .checked_add(Duration::from_nanos(1))
                    .ok_or_else(|| failure("Source-state time overflow"))?,
            )
        } else {
            now
        };
        let fresh = Stamp::from_time(fresh)?;
        let mut entries = BTreeMap::new();
        scan(root, Path::new(""), &mut entries)?;
        let mut reused = 0;
        for (relative, entry) in &mut entries {
            // Keep symlink metadata untouched: File::set_times follows links.
            // Their fresh mtime can cause extra work, but cannot mask a changed target.
            if entry.kind == "link" {
                continue;
            }
            let old = previous
                .as_ref()
                .filter(|s| s.identity == identity && s.source_root.as_deref() == Some(root))
                .and_then(|s| s.entries.get(relative))
                .filter(|old| old.digest == entry.digest && old.kind == entry.kind);
            entry.mtime = if let Some(old) = old {
                reused += 1;
                old.mtime
            } else {
                fresh
            };
            File::open(root.join(relative))?
                .set_times(fs::FileTimes::new().set_modified(entry.mtime.time()?))?;
        }
        event(
            json!({"event":"source-freshness","reused_inputs":reused,"total_inputs":entries.len()}),
        );
        Ok(Self {
            path,
            root: root.into(),
            state: State {
                schema: 1,
                identity: identity.into(),
                source_root: Some(root.into()),
                completed: fresh,
                entries,
            },
        })
    }

    pub(crate) fn complete(mut self) -> Result<()> {
        // Cheaply detect added/generated children before hashing potentially huge outputs.
        for (relative, entry) in &self.state.entries {
            let path = self.root.join(relative);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => return Ok(()),
            };
            if Stamp::from_time(metadata.modified()?)? != entry.mtime {
                return Ok(());
            }
            if entry.kind == "directory" {
                for child in fs::read_dir(path)? {
                    if !self
                        .state
                        .entries
                        .contains_key(&relative.join(child?.file_name()))
                    {
                        return Ok(());
                    }
                }
            }
        }
        let mut observed = BTreeMap::new();
        scan(&self.root, Path::new(""), &mut observed)?;
        if observed.len() != self.state.entries.len()
            || observed.iter().any(|(path, entry)| {
                self.state
                    .entries
                    .get(path)
                    .is_none_or(|old| old.digest != entry.digest || old.kind != entry.kind)
            })
        {
            return Ok(());
        }
        self.state.completed = self
            .state
            .completed
            .max(Stamp::from_time(SystemTime::now())?);
        let mut file = tempfile::NamedTempFile::new_in(
            self.path
                .parent()
                .ok_or_else(|| failure("Missing metadata parent"))?,
        )?;
        serde_json::to_writer(&mut file, &self.state)?;
        file.flush()?;
        file.persist(&self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn stable_scratch_is_short_deterministic_and_disposable() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("targets").join("x".repeat(96));
        std::fs::create_dir_all(target.join(".ccid")).unwrap();
        let mut first = super::Environment::new();
        first.insert("RUNNER_TEMP".into(), "/jobs/one".into());
        let mut second = first.clone();
        second.insert("RUNNER_TEMP".into(), "/jobs/two".into());
        let path = {
            let scratch = super::StableSource::scratch(&target, &mut first).unwrap();
            assert!(scratch.path().is_dir());
            assert!(scratch.path().as_os_str().len() < 32);
            for name in ["TMPDIR", "RUNNER_TEMP", "TEMP", "TMP"] {
                assert_eq!(
                    super::value(&first, name).as_deref(),
                    scratch.path().to_str()
                );
            }
            scratch.path().to_owned()
        };
        assert!(!path.exists());
        let _scratch = super::StableSource::scratch(&target, &mut second).unwrap();
        assert_eq!(super::value(&second, "TMPDIR").as_deref(), path.to_str());
        assert_eq!(first, second);
    }
    use super::*;

    /// A worker-nested `TMPDIR` shaped like the one that broke Unix sockets:
    /// outer job scratch, `nix-shell` scratch, inner job scratch.
    #[cfg(unix)]
    fn nested_tmpdir(root: &Path) -> PathBuf {
        let nested = root.join("ccid-job-qtHmRu/nix-shell-258690-3654363251/ccid-job-p1d4KH");
        fs::create_dir_all(&nested).unwrap();
        nested
    }

    #[cfg(unix)]
    fn tmpdir_environment(path: &Path) -> Environment {
        let mut environment = Environment::new();
        environment.insert("TMPDIR".into(), path.as_os_str().into());
        environment
    }

    #[cfg(unix)]
    #[test]
    fn job_scratch_under_a_nested_tmpdir_is_short_single_level_and_socket_safe() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let outer = tempfile::Builder::new()
            .prefix("t")
            .tempdir_in("/tmp")
            .unwrap();
        let nested = nested_tmpdir(outer.path());
        let mut environment = tmpdir_environment(&nested);
        let before = environment.clone();
        let path = {
            let scratch = scratch(&mut environment).unwrap();
            let path = scratch.path().to_owned();
            assert!(path.is_dir());
            assert!(!path.starts_with(&nested), "scratch must not nest");
            assert_eq!(
                path.parent().unwrap(),
                Path::new("/tmp").join(format!("c{}", rustix::process::geteuid().as_raw())),
                "one level below the short per-user base"
            );
            assert!(path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("ccid-job-")));
            assert_eq!(value(&environment, "TMPDIR").as_deref(), path.to_str());
            // The scratch root plus a 64-byte suffix stays under sun_path.
            assert!(path.as_os_str().len() + SOCKET_SUFFIX_BUDGET <= SUN_PATH_MAX);
            // A real socket at the full suffix budget binds under the root.
            let socket = path.join("s".repeat(SOCKET_SUFFIX_BUDGET - 1));
            assert_eq!(socket.as_os_str().len(), path.as_os_str().len() + 64);
            drop(UnixListener::bind(&socket).unwrap());
            let base = fs::metadata(path.parent().unwrap()).unwrap();
            assert_eq!(base.permissions().mode() & 0o077, 0, "base is owner only");
            path
        };
        assert!(!path.exists(), "owned scratch is removed with its guard");
        // Only TMPDIR changes: nothing that could reach a cache key is added.
        let mut expected = before;
        expected.insert("TMPDIR".into(), path.as_os_str().into());
        assert_eq!(environment, expected);
    }

    #[cfg(unix)]
    #[test]
    fn job_scratch_stays_beneath_a_short_tmpdir_and_the_longest_short_root_fits() {
        let parent = tempfile::Builder::new()
            .prefix("s")
            .tempdir_in("/tmp")
            .unwrap();
        let mut environment = tmpdir_environment(parent.path());
        let scratch = scratch(&mut environment).unwrap();
        assert_eq!(scratch.path().parent(), Some(parent.path()));
        // Worst case of the short root: a 10-digit uid.
        let widest_base = format!("/tmp/c{}/", u32::MAX);
        assert!(widest_base.len() + JOB_LEAF_LEN + SOCKET_SUFFIX_BUDGET <= SUN_PATH_MAX);
    }

    #[cfg(unix)]
    #[test]
    fn job_scratch_degrades_to_the_inherited_tmpdir_when_the_short_base_is_unusable() {
        use std::os::unix::fs::PermissionsExt;
        let outer = tempfile::Builder::new()
            .prefix("t")
            .tempdir_in("/tmp")
            .unwrap();
        let nested = nested_tmpdir(outer.path());
        let uid = rustix::process::geteuid().as_raw();
        // A link planted at the base is never followed.
        let linked = outer.path().join("linked");
        let elsewhere = outer.path().join("elsewhere");
        fs::create_dir_all(&linked).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, linked.join(format!("c{uid}"))).unwrap();
        // A group- or other-accessible base is refused.
        let open = outer.path().join("open");
        fs::create_dir_all(open.join(format!("c{uid}"))).unwrap();
        fs::set_permissions(
            open.join(format!("c{uid}")),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        // An unwritable (absent) parent is refused too.
        let absent = outer.path().join("absent");
        for short_parent in [&linked, &open, &absent] {
            let mut environment = tmpdir_environment(&nested);
            let scratch = scratch_with_short_base(&mut environment, short_parent).unwrap();
            assert_eq!(scratch.path().parent(), Some(nested.as_path()));
            assert_eq!(
                value(&environment, "TMPDIR").as_deref(),
                scratch.path().to_str()
            );
        }
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn stable_sources_remove_only_owned_contents_and_refuse_symlink_roots() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("input");
        let target = temp.path().join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(target.join(".ccid/source-v1")).unwrap();
        fs::write(source.join("value"), "source").unwrap();
        assert!(StableSource::prepare(&source, &target).is_err());
        fs::remove_dir(target.join(".ccid/source-v1")).unwrap();
        std::os::unix::fs::symlink("value", source.join("link")).unwrap();
        {
            let owned = StableSource::prepare(&source, &target).unwrap();
            assert_eq!(
                fs::read_link(owned.path().join("link")).unwrap(),
                PathBuf::from("value")
            );
            fs::write(owned.path().join("generated"), "discard").unwrap();
        }
        assert!(!target.join(".ccid/source-v1").exists());
        {
            let owned = StableSource::prepare(&source, &target).unwrap();
            assert!(!owned.path().join("generated").exists());
        }
        std::os::unix::fs::symlink(&source, target.join(".ccid/source-v1")).unwrap();
        assert!(StableSource::prepare(&source, &target).is_err());
        assert_eq!(fs::read_to_string(source.join("value")).unwrap(), "source");
    }
    #[test]
    fn identity_preserves_forge_and_owner_not_just_project_slug() {
        assert_eq!(
            canonical_repository("git@github.com:owner/repo.git").unwrap(),
            canonical_repository("https://github.com/owner/repo").unwrap()
        );
        assert_ne!(
            canonical_repository("https://forge.example/owner/repo").unwrap(),
            canonical_repository("https://github.com/owner/repo").unwrap()
        );
        assert!(canonical_repository("https://token@github.com/owner/repo").is_err());
        assert!(canonical_repository("https://github.com/../repo").is_err());
    }
    #[test]
    fn canonical_namespace_is_stable_and_separates_owners() {
        let env = Environment::from([("CARGO_HOME".into(), "/cargo".into())]);
        let first = target_directory(Path::new("/source"), "forge/one/repo", &env).unwrap();
        assert!(first.starts_with("/cargo/targets"));
        assert_ne!(
            first,
            target_directory(Path::new("/source"), "forge/two/repo", &env).unwrap()
        );
    }
    #[cfg(unix)]
    #[test]
    fn target_aliases_share_the_same_lock() {
        let temp = tempfile::TempDir::new().unwrap();
        let original = temp.path().join("original");
        let (target, _held) =
            lock_target(&original, Instant::now() + Duration::from_secs(1)).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(alias.join(".ccid/lock"))
            .unwrap();
        assert!(matches!(second.try_lock(), Err(TryLockError::WouldBlock)));
    }

    #[test]
    fn target_lock_wait_cannot_outlive_the_overall_deadline() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("target");
        let (_target, _held) =
            lock_target(&target, Instant::now() + Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        assert!(lock_target(&target, started + Duration::from_millis(80)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn freshness_reuses_only_the_same_canonical_source_root() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("target");
        fs::create_dir_all(target.join(".ccid")).unwrap();
        let first = temp.path().join("first");
        let moved = temp.path().join("moved");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&moved).unwrap();
        fs::write(first.join("input"), "same").unwrap();
        fs::write(moved.join("input"), "same").unwrap();

        Freshness::prepare(&first, &target, "forge/owner/repo")
            .unwrap()
            .complete()
            .unwrap();
        let retained = fs::metadata(first.join("input"))
            .unwrap()
            .modified()
            .unwrap();
        Freshness::prepare(&first, &target, "forge/owner/repo")
            .unwrap()
            .complete()
            .unwrap();
        assert_eq!(
            fs::metadata(first.join("input"))
                .unwrap()
                .modified()
                .unwrap(),
            retained
        );

        let state_path = target.join(".ccid/source-state.json");
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("source_root");
        fs::write(&state_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        Freshness::prepare(&first, &target, "forge/owner/repo")
            .unwrap()
            .complete()
            .unwrap();
        let legacy_rebuilt = fs::metadata(first.join("input"))
            .unwrap()
            .modified()
            .unwrap();
        assert!(legacy_rebuilt > retained);

        Freshness::prepare(&moved, &target, "forge/owner/repo")
            .unwrap()
            .complete()
            .unwrap();
        assert!(
            fs::metadata(moved.join("input"))
                .unwrap()
                .modified()
                .unwrap()
                > legacy_rebuilt
        );
    }
}

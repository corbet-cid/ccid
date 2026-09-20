//! Cache ownership and freshness for verified, disposable archive sources.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

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

fn canonical_repository(url: &str) -> Result<String> {
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

pub(crate) fn scratch(env: &mut Environment) -> Result<tempfile::TempDir> {
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
    let scratch = tempfile::Builder::new()
        .prefix("ccid-job-")
        .tempdir_in(parent)?;
    set(env, "TMPDIR", scratch.path().as_os_str());
    #[cfg(windows)]
    for name in ["TEMP", "TMP"] {
        set(env, name, scratch.path().as_os_str());
    }
    Ok(scratch)
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
    use super::*;
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

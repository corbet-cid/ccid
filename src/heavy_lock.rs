//! The machine's heavy-eval lock around ccid's own whole-flake Nix steps.
//!
//! srv and its archlxc container share one RAM pool; one whole-flake evaluation peaks at 7-17 GB, so
//! the hosts declare lock files (infra `lib/heavy-lock.nix`) and every heavy caller blocks on one of
//! them. The shell side is `tools/heavy-lock`; this is the same contract for ccid:
//!
//! - the file is `HEAVY_LOCK`, else `CCID_HEAVY_LOCK` (a CI runner slot's own pool file, named with
//!   the `CCID_` prefix so the cache keys of checks do not vary with the slot), else the first line of
//!   `/etc/corbet/heavy-lock-path`; with none of them no lock is taken (other machines);
//! - `HEAVY_LOCK_HELD` set, or an ancestor process holding a `heavy-eval*.lock` file of the same
//!   directory (read from `/proc/locks`), means the caller is already serialised: no second lock, so
//!   nested calls cannot deadlock;
//! - the checkout's holder marker (`<git-dir>/heavy-lock-holder`, written by `tools/heavy-lock` of infra
//!   while it holds the lock) names a live process and the lock file is locked right now: the caller runs
//!   below `check-changed`, whose `env -i` removed every environment hint. `/proc/locks` cannot be asked
//!   here, it names the short-lived `flock(1)` child that took the lock;
//! - otherwise an exclusive `flock` is taken (blocking, no polling) and held until the guard drops.
//!   The descriptor is close-on-exec, so only this process holds it; children run beneath the holder.
#[cfg(unix)]
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// Held for as long as the guard lives; `None` inside means "no lock was needed".
pub struct HeavyLock {
    #[cfg(unix)]
    _file: Option<fs::File>,
}

/// Where the lock file is, from the three sources in order (see the module doc).
#[cfg(unix)]
fn lock_path(get: &dyn Fn(&str) -> Option<String>, etc: &Path) -> Option<PathBuf> {
    for name in ["HEAVY_LOCK", "CCID_HEAVY_LOCK"] {
        if let Some(value) = get(name).filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(value));
        }
    }
    let text = fs::read_to_string(etc).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| PathBuf::from(line))
}

/// Process ids holding an exclusive `FLOCK` on the file with this inode, from `/proc/locks` text.
#[cfg(unix)]
fn holders(proc_locks: &str, inode: u64) -> Vec<u32> {
    let suffix = format!(":{inode}");
    proc_locks
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            (f.len() >= 6 && f[1] == "FLOCK" && f[3] == "WRITE" && f[5].ends_with(&suffix))
                .then(|| f[4].parse().ok())
                .flatten()
        })
        .collect()
}

/// This process and its ancestors, from `/proc/<pid>/stat` (field 4 is the parent).
#[cfg(unix)]
fn ancestors(mut pid: u32) -> Vec<u32> {
    let mut chain = Vec::new();
    while pid > 1 && chain.len() < 256 {
        chain.push(pid);
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            break;
        };
        // the command name is parenthesised and may contain spaces: the fields start after the last `)`
        let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            break;
        };
        pid = rest
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
    }
    chain
}

/// Whether an ancestor of this process holds any lock file of the pool (`heavy-eval*.lock`).
#[cfg(unix)]
fn ancestor_holds(lock: &Path, proc_locks: &str, chain: &[u32]) -> bool {
    let dir = lock.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let mut files: Vec<PathBuf> = vec![lock.to_path_buf()];
    files.extend(entries.flatten().map(|e| e.path()).filter(|p| {
        p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            n.starts_with("heavy-eval") && n.ends_with(".lock")
        })
    }));
    files.iter().any(|file| {
        fs::metadata(file)
            .map(|m| holders(proc_locks, m.ino()).iter().any(|h| chain.contains(h)))
            .unwrap_or(false)
    })
}

/// The marker rule of tools/heavy-lock `heavy_lock_held_by_marker`: a live process named in the checkout's marker, and the lock file locked by someone.
#[cfg(unix)]
fn held_by_marker(lock: &Path) -> bool {
    let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || dir.is_empty() {
        return false;
    }
    let Ok(text) = fs::read_to_string(Path::new(&dir).join("heavy-lock-holder")) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return false;
    };
    if !Path::new(&format!("/proc/{pid}")).exists() {
        return false;
    }
    let Ok(file) = OpenOptions::new().append(true).open(lock) else {
        return false;
    };
    // a probe that cannot lock means somebody holds it; one that can is released at once (the descriptor drops)
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err()
}

impl HeavyLock {
    /// Take the lock for the calling process (see the module doc); a declared lock that cannot be
    /// opened is an error, an undeclared one is not.
    #[cfg(unix)]
    pub fn acquire(what: &str) -> crate::Result<HeavyLock> {
        let get = |name: &str| std::env::var(name).ok();
        let none = HeavyLock { _file: None };
        let Some(path) = lock_path(&get, Path::new("/etc/corbet/heavy-lock-path")) else {
            return Ok(none);
        };
        if get("HEAVY_LOCK_HELD").is_some_and(|v| !v.is_empty()) {
            return Ok(none);
        }
        if held_by_marker(&path) {
            return Ok(none);
        }
        let proc_locks = fs::read_to_string("/proc/locks").unwrap_or_default();
        if ancestor_holds(&path, &proc_locks, &ancestors(std::process::id())) {
            return Ok(none);
        }
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| {
                crate::failure(format!("[heavy-lock] cannot open {}: {e}", path.display()))
            })?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => {
                let note = fs::read_to_string(format!("{}.holder", path.display()))
                    .unwrap_or_default();
                eprintln!(
                    "ccid: waiting for the heavy-eval lock {} held by {}",
                    path.display(),
                    note.trim().split('\n').next().filter(|n| !n.is_empty()).unwrap_or("unknown")
                );
                loop {
                    match rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive) {
                        Ok(()) => break,
                        Err(rustix::io::Errno::INTR) => {
                            if crate::INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
                                return Err(crate::failure("[heavy-lock] interrupted while waiting"));
                            }
                        }
                        Err(e) => {
                            return Err(crate::failure(format!("[heavy-lock] flock failed: {e}")))
                        }
                    }
                }
            }
            Err(e) => return Err(crate::failure(format!("[heavy-lock] flock failed: {e}"))),
        }
        // best effort: the note names the holder for the next waiter; the file is truncated by the next holder
        let _ = file.set_len(0);
        let note = format!(
            "{}:{} ccid {what}\n",
            std::env::var("HOSTNAME").unwrap_or_default(),
            std::process::id()
        );
        let _ = fs::write(format!("{}.holder", path.display()), note);
        let _ = file.flush();
        Ok(HeavyLock { _file: Some(file) })
    }

    #[cfg(not(unix))]
    pub fn acquire(_what: &str) -> crate::Result<HeavyLock> {
        Ok(HeavyLock {})
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn path_prefers_heavy_lock_then_ccid_pool_then_etc_file() {
        let tmp = std::env::temp_dir().join(format!("ccid-hl-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        let etc = tmp.join("path");
        fs::write(&etc, "/etc/declared.lock\nignored\n").unwrap();
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let both = env(&[("HEAVY_LOCK", "/a"), ("CCID_HEAVY_LOCK", "/b")]);
        assert_eq!(lock_path(&both, &etc), Some(PathBuf::from("/a")));
        let pool = env(&[("CCID_HEAVY_LOCK", "/b")]);
        assert_eq!(lock_path(&pool, &etc), Some(PathBuf::from("/b")));
        assert_eq!(
            lock_path(&env(&[]), &etc),
            Some(PathBuf::from("/etc/declared.lock"))
        );
        assert_eq!(lock_path(&env(&[]), &tmp.join("absent")), None);
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn proc_locks_rows_name_exclusive_flock_holders_of_one_inode() {
        let text = "1: FLOCK  ADVISORY  WRITE 4242 00:2a:777 0 EOF\n\
                    2: FLOCK  ADVISORY  READ 4343 00:2a:777 0 EOF\n\
                    3: FLOCK  ADVISORY  WRITE 4444 00:2a:7770 0 EOF\n\
                    4: POSIX  ADVISORY  WRITE 4545 00:2a:777 0 EOF\n";
        assert_eq!(holders(text, 777), vec![4242]);
    }

    #[test]
    fn two_callers_on_one_file_serialise_and_a_second_file_does_not_wait() {
        let tmp = std::env::temp_dir().join(format!("ccid-hl2-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        let (a, b) = (tmp.join("heavy-eval-ci-0.lock"), tmp.join("heavy-eval-ci-1.lock"));
        let open = |p: &Path| OpenOptions::new().append(true).create(true).open(p).unwrap();
        let (fa, fb) = (open(&a), open(&b));
        rustix::fs::flock(&fa, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        // the same file refuses a second holder (flock locks belong to the open file description) ...
        let second = open(&a);
        assert!(rustix::fs::flock(&second, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err());
        // ... another pool file is free
        rustix::fs::flock(&fb, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        // an ancestor (here: this very process) holding a pool file makes a nested call pass through
        let proc_locks = fs::read_to_string("/proc/locks").unwrap();
        assert!(ancestor_holds(&b, &proc_locks, &ancestors(std::process::id())));
        drop((fa, fb, second));
        let proc_locks = fs::read_to_string("/proc/locks").unwrap();
        assert!(!ancestor_holds(&a, &proc_locks, &ancestors(std::process::id())));
        fs::remove_dir_all(&tmp).unwrap();
    }
}

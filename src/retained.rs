//! `ccid retained`: the content-addressed retained store, `$CI_RETAINED_ROOT/<kind>/<digest>/`.
//!
//! Some CI captures cannot be rebuilt (a recorded proof, a measured fixture). They are kept write-once under the digest
//! of their own content, independent of any repository, namespace or forge URL: a writer puts a file or directory in and
//! learns its digest, a reader asks for a kind and a digest and nothing else. Moving an organisation or renaming a
//! repository changes no path here.
//!
//! * the digest of a file is its sha256; the digest of a directory is the sha256 of the sorted lines
//!   `<relative path>\0<file sha256>\n` of its regular files (a symlink or special file is refused);
//! * `put` copies into a temporary sibling and renames it into place, so a reader never sees half an entry; an entry that
//!   exists must hold the same digest (it is verified, never replaced); entries are made read-only;
//! * `get` verifies the digest before it answers, so a damaged entry is an error, never a silent wrong answer;
//! * the root is `--root`, else `$CI_RETAINED_ROOT`, else `$CI_CACHE_ROOT/retained` (`ccid job-run` exports the variable).

use crate::{failure, Result};
use clap::{Args, Subcommand};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Args, Debug)]
pub struct Options {
    /// The store root (default: $CI_RETAINED_ROOT, else $CI_CACHE_ROOT/retained).
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Subcommand, Debug)]
pub enum Action {
    /// Keep a file or directory under its digest; prints the digest.
    Put {
        #[arg(long)]
        kind: String,
        /// Also answer to this earlier digest (an old scheme's name for the same capture): `get --digest <alias>` finds it.
        #[arg(long)]
        alias: Vec<String>,
        path: PathBuf,
    },
    /// Print the path of the entry after verifying it, or copy it with --into.
    Get {
        #[arg(long)]
        kind: String,
        #[arg(long)]
        digest: String,
        /// Copy the entry's content here instead of printing its path.
        #[arg(long)]
        into: Option<PathBuf>,
    },
    /// Verify every entry of a kind (or of the whole store).
    Verify {
        #[arg(long)]
        kind: Option<String>,
    },
    /// Print the store root.
    Root,
}

fn plain(name: &str, what: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(failure(format!(
            "{what} {name:?}: lowercase letters, digits, '-' and '_' only"
        )))
    }
}

fn digest_ok(digest: &str) -> Result<()> {
    if digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(failure("a digest is 64 lowercase hex digits"))
    }
}

fn file_digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1 << 16];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// The regular files below `root` (relative path to digest); anything else is refused.
fn tree(root: &Path) -> Result<BTreeMap<String, String>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = fs::symlink_metadata(&path)?.file_type();
            if kind.is_dir() {
                walk(base, &path, out)?;
            } else if kind.is_file() {
                let relative = path
                    .strip_prefix(base)
                    .map_err(|_| failure("path outside the tree"))?
                    .to_str()
                    .ok_or_else(|| failure("a file name is not valid UTF-8"))?
                    .to_owned();
                out.insert(relative, file_digest(&path)?);
            } else {
                return Err(failure(format!(
                    "{} is a symlink or a special file: a retained tree holds regular files only",
                    path.display()
                )));
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

/// The digest of a file (its sha256) or of a directory (see the module doc).
pub fn digest_of(path: &Path) -> Result<String> {
    let kind = fs::symlink_metadata(path)?.file_type();
    if kind.is_file() {
        return file_digest(path);
    }
    if !kind.is_dir() {
        return Err(failure(format!(
            "{} is neither a file nor a directory",
            path.display()
        )));
    }
    let mut hasher = Sha256::new();
    for (name, digest) in tree(path)? {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(digest.as_bytes());
        hasher.update(b"\n");
    }
    Ok(hex::encode(hasher.finalize()))
}

fn root_of(options: &Options) -> Result<PathBuf> {
    let root = options
        .root
        .clone()
        .or_else(|| {
            std::env::var_os("CI_RETAINED_ROOT")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| {
            std::env::var_os("CI_CACHE_ROOT")
                .filter(|v| !v.is_empty())
                .map(|v| PathBuf::from(v).join("retained"))
        })
        .ok_or_else(|| {
            failure("no store: pass --root or set CI_RETAINED_ROOT (or CI_CACHE_ROOT)")
        })?;
    if root.is_absolute() {
        Ok(root)
    } else {
        Err(failure("the retained store root must be absolute"))
    }
}

/// The content of an entry: a single file `content` or a tree below `content/`.
fn content_path(entry: &Path) -> PathBuf {
    entry.join("content")
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if fs::symlink_metadata(entry.path())?.file_type().is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(path)?;
    let mode = if meta.is_dir() { 0o555 } else { 0o444 };
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            read_only(&entry?.path())?;
        }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn read_only(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.is_dir() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    writable(&entry.path());
                }
            }
        } else {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
        }
    }
}

#[cfg(not(unix))]
fn writable(_path: &Path) {}

/// Keep `path` under `<root>/<kind>/<digest>/content`; returns the digest.
pub fn put(root: &Path, kind: &str, path: &Path) -> Result<String> {
    plain(kind, "kind")?;
    let digest = digest_of(path)?;
    let entry = root.join(kind).join(&digest);
    if entry.exists() {
        verify_entry(&entry, &digest)?;
        return Ok(digest);
    }
    fs::create_dir_all(root.join(kind))?;
    let staging = root
        .join(kind)
        .join(format!(".incoming-{}-{}", std::process::id(), digest));
    if staging.exists() {
        writable(&staging);
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    let content = content_path(&staging);
    if fs::symlink_metadata(path)?.file_type().is_dir() {
        copy_tree(path, &content)?;
    } else {
        fs::copy(path, &content)?;
    }
    if digest_of(&content)? != digest {
        writable(&staging);
        let _ = fs::remove_dir_all(&staging);
        return Err(failure("the source changed while it was being copied"));
    }
    read_only(&staging)?;
    match fs::rename(&staging, &entry) {
        Ok(()) => Ok(digest),
        Err(_) if entry.exists() => {
            // A concurrent writer won the rename with the same digest.
            writable(&staging);
            let _ = fs::remove_dir_all(&staging);
            verify_entry(&entry, &digest)?;
            Ok(digest)
        }
        Err(error) => Err(error.into()),
    }
}

fn verify_entry(entry: &Path, digest: &str) -> Result<()> {
    let content = content_path(entry);
    if !content.exists() {
        return Err(failure(format!("{} has no content", entry.display())));
    }
    let actual = digest_of(&content)?;
    if actual == digest {
        Ok(())
    } else {
        Err(failure(format!(
            "{} is damaged: its content digests to {actual}, not {digest}",
            entry.display()
        )))
    }
}

/// Record that `alias` (a digest in an earlier scheme) names the entry `digest`.
pub fn alias(root: &Path, kind: &str, alias: &str, digest: &str) -> Result<()> {
    plain(kind, "kind")?;
    digest_ok(alias)?;
    digest_ok(digest)?;
    let dir = root.join(kind).join(".alias");
    fs::create_dir_all(&dir)?;
    let file = dir.join(alias);
    match fs::read_to_string(&file) {
        Ok(existing) if existing.trim() == digest => return Ok(()),
        Ok(existing) => {
            return Err(failure(format!(
                "alias {alias} already names {}, not {digest}",
                existing.trim()
            )))
        }
        Err(_) => {}
    }
    fs::write(&file, format!("{digest}\n"))?;
    Ok(())
}

/// The verified content path of an entry (`digest` may be an alias).
pub fn get(root: &Path, kind: &str, digest: &str) -> Result<PathBuf> {
    plain(kind, "kind")?;
    digest_ok(digest)?;
    let named = root.join(kind).join(".alias").join(digest);
    let resolved = match fs::read_to_string(&named) {
        Ok(target) => target.trim().to_owned(),
        Err(_) => digest.to_owned(),
    };
    digest_ok(&resolved)?;
    let digest = resolved.as_str();
    let entry = root.join(kind).join(digest);
    if !entry.is_dir() {
        return Err(failure(format!(
            "no retained {kind} entry {digest} under {}",
            root.display()
        )));
    }
    verify_entry(&entry, digest)?;
    Ok(content_path(&entry))
}

fn verify_all(root: &Path, kind: Option<&str>) -> Result<usize> {
    let mut count = 0;
    let kinds: Vec<String> = match kind {
        Some(kind) => vec![kind.to_owned()],
        None => fs::read_dir(root)?
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .collect(),
    };
    for kind in kinds {
        plain(&kind, "kind")?;
        let dir = root.join(&kind);
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            digest_ok(&name)?;
            verify_entry(&entry.path(), &name)?;
            count += 1;
        }
    }
    Ok(count)
}

pub fn run(options: &Options) -> Result<u8> {
    let root = root_of(options)?;
    match &options.action {
        Action::Root => println!("{}", root.display()),
        Action::Put {
            kind,
            alias: aliases,
            path,
        } => {
            let digest = put(&root, kind, path)?;
            for name in aliases {
                alias(&root, kind, name, &digest)?;
            }
            println!("{digest}");
        }
        Action::Get { kind, digest, into } => {
            let content = get(&root, kind, digest)?;
            match into {
                None => println!("{}", content.display()),
                Some(destination) => {
                    if fs::symlink_metadata(&content)?.file_type().is_dir() {
                        copy_tree(&content, destination)?;
                    } else {
                        if let Some(parent) = destination.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::copy(&content, destination)?;
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fn open_up(path: &Path) {
                            if let Ok(meta) = fs::symlink_metadata(path) {
                                if meta.is_dir() {
                                    let _ = fs::set_permissions(
                                        path,
                                        fs::Permissions::from_mode(0o755),
                                    );
                                    if let Ok(entries) = fs::read_dir(path) {
                                        for entry in entries.flatten() {
                                            open_up(&entry.path());
                                        }
                                    }
                                } else {
                                    let _ = fs::set_permissions(
                                        path,
                                        fs::Permissions::from_mode(0o644),
                                    );
                                }
                            }
                        }
                        open_up(destination);
                    }
                }
            }
        }
        Action::Verify { kind } => {
            let n = verify_all(&root, kind.as_deref())?;
            println!("retained: {n} entries verified");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        dir
    }

    #[test]
    fn a_file_is_kept_under_its_sha256_and_read_back_by_digest_alone() {
        let store = tempfile::tempdir().unwrap();
        let src = tree_with(&[("a.json", "{\"x\":1}")]);
        let digest = put(store.path(), "record-fixture", &src.path().join("a.json")).unwrap();
        assert_eq!(digest.len(), 64);
        let content = get(store.path(), "record-fixture", &digest).unwrap();
        assert_eq!(fs::read_to_string(content).unwrap(), "{\"x\":1}");
        // the same bytes again are the same entry
        assert_eq!(
            put(store.path(), "record-fixture", &src.path().join("a.json")).unwrap(),
            digest
        );
    }

    #[test]
    fn a_directory_digest_follows_content_not_order_or_location() {
        let a = tree_with(&[("x/one", "1"), ("two", "2")]);
        let b = tree_with(&[("two", "2"), ("x/one", "1")]);
        assert_eq!(digest_of(a.path()).unwrap(), digest_of(b.path()).unwrap());
        let c = tree_with(&[("x/one", "1"), ("two", "3")]);
        assert_ne!(digest_of(a.path()).unwrap(), digest_of(c.path()).unwrap());
        let store = tempfile::tempdir().unwrap();
        let digest = put(store.path(), "proof", a.path()).unwrap();
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("copy");
        let content = get(store.path(), "proof", &digest).unwrap();
        copy_tree(&content, &dest).unwrap();
        assert_eq!(digest_of(&dest).unwrap(), digest);
    }

    #[cfg(unix)]
    #[test]
    fn entries_are_read_only_and_a_damaged_entry_is_an_error_not_an_answer() {
        use std::os::unix::fs::PermissionsExt;
        let store = tempfile::tempdir().unwrap();
        let src = tree_with(&[("f", "payload")]);
        let digest = put(store.path(), "k", &src.path().join("f")).unwrap();
        let content = get(store.path(), "k", &digest).unwrap();
        assert_eq!(
            fs::metadata(&content).unwrap().permissions().mode() & 0o222,
            0
        );
        // damage it behind the store's back
        fs::set_permissions(&content, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&content, "tampered").unwrap();
        let error = get(store.path(), "k", &digest).unwrap_err().to_string();
        assert!(error.contains("damaged"), "{error}");
        assert!(put(store.path(), "k", &src.path().join("f")).is_err());
        assert!(verify_all(store.path(), None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_tree_with_a_symlink_is_refused() {
        let store = tempfile::tempdir().unwrap();
        let src = tree_with(&[("f", "1")]);
        std::os::unix::fs::symlink("f", src.path().join("link")).unwrap();
        assert!(put(store.path(), "k", src.path())
            .unwrap_err()
            .to_string()
            .contains("symlink"));
    }

    #[test]
    fn an_old_digest_is_an_alias_of_the_entry_and_never_of_two() {
        let store = tempfile::tempdir().unwrap();
        let src = tree_with(&[("f", "1")]);
        let digest = put(store.path(), "k", src.path()).unwrap();
        let old = "ab".repeat(32);
        alias(store.path(), "k", &old, &digest).unwrap();
        alias(store.path(), "k", &old, &digest).unwrap();
        assert_eq!(
            get(store.path(), "k", &old).unwrap(),
            get(store.path(), "k", &digest).unwrap()
        );
        assert!(alias(store.path(), "k", &old, &"cd".repeat(32)).is_err());
    }

    #[test]
    fn kinds_and_digests_are_plain() {
        let store = tempfile::tempdir().unwrap();
        let src = tree_with(&[("f", "1")]);
        for bad in ["", "../x", "A", "a/b"] {
            assert!(
                put(store.path(), bad, &src.path().join("f")).is_err(),
                "{bad:?}"
            );
        }
        assert!(get(store.path(), "k", "short").is_err());
        assert!(get(store.path(), "k", &"0".repeat(64))
            .unwrap_err()
            .to_string()
            .contains("no retained"));
    }
}

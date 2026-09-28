//! Verify an immutable source snapshot before archive extraction.
use crate::{event, failure, Result};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, Write},
    path::{Component, Path, PathBuf},
    process::Command,
};

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn hex_identity(value: &str, sizes: &[usize]) -> bool {
    sizes.contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn safe_relative(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => normalized.push(name),
            Component::CurDir => {}
            Component::ParentDir if normalized.pop() => {}
            _ => return Err(failure("Source archive contains an escaping path or link")),
        }
    }
    Ok(normalized)
}
// Resolve every link using the complete archive map, including links encountered
// before a `..` component. Pure lexical normalization misses chained escapes.
pub(crate) fn resolve_archive_path(
    path: &Path,
    links: &BTreeMap<PathBuf, PathBuf>,
    active: &mut BTreeSet<PathBuf>,
) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if resolved.pop() => {}
            Component::Normal(name) => {
                resolved.push(name);
                if let Some(target) = links.get(&resolved) {
                    let link = resolved.clone();
                    if active.len() >= 64 {
                        return Err(failure("Source symbolic-link chain exceeds 64 entries"));
                    }
                    if !active.insert(link.clone()) {
                        return Err(failure("Source archive contains a cyclic symbolic link"));
                    }
                    resolved = resolve_archive_path(
                        &link.parent().unwrap_or_else(|| Path::new("")).join(target),
                        links,
                        active,
                    )?;
                    active.remove(&link);
                }
            }
            _ => return Err(failure("Source archive contains an escaping path or link")),
        }
    }
    Ok(resolved)
}

pub fn verify_source(archive: &Path, digest: &str, commit: &str, destination: &Path) -> Result<()> {
    if !hex_identity(digest, &[64]) || !hex_identity(commit, &[40, 64]) {
        return Err(failure(
            "Source digest and commit must be complete lowercase identities",
        ));
    }
    // Hash the bytes while copying into a private, automatically removed snapshot.
    // Every later pass uses this handle, so pathname replacement or in-place writes
    // to the supplied archive cannot change the verified extraction input.
    let mut input = File::open(archive)?;
    let mut snapshot = tempfile::tempfile()?;
    let mut actual = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let size = input.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        snapshot.write_all(&buffer[..size])?;
        actual.update(&buffer[..size]);
    }
    if format!("{:x}", actual.finalize()) != digest {
        return Err(failure("Source archive SHA-256 mismatch"));
    }
    snapshot.rewind()?;
    let result = Command::new("git")
        .arg("get-tar-commit-id")
        .stdin(snapshot.try_clone()?)
        .output()?;
    if !result.status.success() || String::from_utf8(result.stdout)?.trim() != commit {
        return Err(failure("Source archive embedded commit mismatch"));
    }
    if destination.exists() && fs::read_dir(destination)?.next().is_some() {
        return Err(failure(
            "Source destination must be empty; existing work is never overwritten",
        ));
    }
    snapshot.rewind()?;
    let mut paths = Vec::new();
    let mut links = BTreeMap::new();
    for entry in tar::Archive::new(&mut snapshot).entries()? {
        let entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        let path = entry.path()?;
        if path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(failure("Source archive contains an unsafe path"));
        }
        let path = safe_relative(&path)?;
        if kind.is_symlink() {
            let target = entry
                .link_name()?
                .ok_or_else(|| failure("Source link has no target"))?
                .into_owned();
            links.insert(path.clone(), target);
        } else if !kind.is_file() && !kind.is_dir() {
            return Err(failure(
                "Source archive contains a special file or hard link",
            ));
        }
        paths.push(path);
    }
    for path in &paths {
        resolve_archive_path(path, &links, &mut BTreeSet::new())?;
        // Git archives never contain children beneath a symbolic-link member.
        if path
            .ancestors()
            .skip(1)
            .any(|parent| links.contains_key(parent))
        {
            return Err(failure("Source archive writes beneath a symbolic link"));
        }
    }
    snapshot.rewind()?;
    fs::create_dir_all(destination)?;
    let mut archive = tar::Archive::new(snapshot);
    archive.set_preserve_mtime(false);
    archive.set_preserve_permissions(false);
    archive.unpack(destination)?;
    event(json!({"event":"source", "commit":commit, "sha256":digest, "files":paths.len()}));
    Ok(())
}

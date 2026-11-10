//! Retention of a freshly resolved npm lock as a commit artifact.
//!
//! This is the `resolve-npm` maintenance job the repositories used to run as
//! `.ci/resolve-npm.sh`: an explicit resolution (`npm install
//! --package-lock-only`), never a verification verdict. It resolves OUTSIDE
//! the declared source (HR1): the commit is exported with `git archive` into
//! a scratch directory and `npm` runs only there, so the checkout a verdict
//! is keyed on is never written. The retained lock lands under
//! `$ARTIFACT_ROOT/$CI_COMMIT_SHA/resolve-npm/`.
//!
//! The Rust `resolve-dependencies` jobs are NOT ported: per the cmtymeet
//! answer Q9, `xtask lock-refresh` is the one mover of Rust pins, so a bare
//! `cargo update` (the 10 base variants) would only generate lock drift
//! against the two-way third-party sync (HR2). Invoking this command without
//! `--npm` is refused.

use crate::{failure, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Per-repository data for `ccid resolve-lock` (`[resolve]` in `.ci/ccid.toml`).
///
/// The npm job takes no per-repository selection: the command is identical in
/// every repository. The only knob is whether the retained lock is echoed to
/// stdout (the cblc script did, the cwlt script did not; unified: yes).
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Print the retained lock between `---LOCK-BEGIN---` and `---LOCK-END---`.
    #[serde(default = "default_true")]
    pub print_lock: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { print_lock: true }
    }
}

fn default_true() -> bool {
    true
}

/// The exact npm resolution the retired `.ci/resolve-npm.sh` scripts ran.
/// No more, no less: npm decides the pins, ccid only retains the result.
const NPM_ARGV: &[&str] = &[
    "install",
    "--package-lock-only",
    "--ignore-scripts",
    "--no-audit",
    "--no-fund",
];

const LOCK_NAME: &str = "package-lock.json";
const LAYOUT_DIR: &str = "resolve-npm";

/// Run the npm lock resolution; `Ok(code)` is the process exit code.
pub fn run(repo: &Path, manifest: &Path, npm: bool) -> Result<u8> {
    if !npm {
        return Err(failure(
            "Rust lock resolution is retired; xtask lock-refresh is the one mover of Rust pins (use --npm for the npm lock)",
        ));
    }
    let config = crate::resolve_config(repo, manifest)?;
    let artifact_root = artifact_root(repo)?;
    let revision = revision()?;
    let work = export_commit(repo, &revision)?;
    let Some(resolved) = resolve_in_copy(&work)? else {
        eprintln!("ccid resolve-lock: npm install failed");
        return Ok(1);
    };
    write_layout(&artifact_root, &revision, &resolved)?;
    report(&resolved, config.print_lock)?;
    println!("Resolved npm lock only; retain it in a commit and run the full verify job.");
    Ok(0)
}

/// `ARTIFACT_ROOT` must name an absolute directory outside `--repo`.
fn artifact_root(repo: &Path) -> Result<PathBuf> {
    let root = std::env::var_os("ARTIFACT_ROOT")
        .map(PathBuf::from)
        .ok_or_else(|| failure("ARTIFACT_ROOT is not set"))?;
    check_artifact_root(repo, &root)?;
    Ok(root)
}

fn check_artifact_root(repo: &Path, root: &Path) -> Result<()> {
    if !root.is_absolute() {
        return Err(failure("ARTIFACT_ROOT must be an absolute path"));
    }
    let anchor = repo
        .canonicalize()
        .map_err(|_| failure("cannot resolve --repo"))?;
    if root.starts_with(&anchor) {
        return Err(failure("ARTIFACT_ROOT must be outside --repo"));
    }
    Ok(())
}

/// `CI_COMMIT_SHA` names the exported commit: 40 lowercase hex characters.
fn revision() -> Result<String> {
    let revision = std::env::var_os("CI_COMMIT_SHA")
        .map(|value| value.to_string_lossy().into_owned())
        .ok_or_else(|| failure("CI_COMMIT_SHA is not set"))?;
    if valid_revision(&revision) {
        Ok(revision)
    } else {
        Err(failure("CI_COMMIT_SHA must be 40 lowercase hex characters"))
    }
}

fn valid_revision(revision: &str) -> bool {
    revision.len() == 40
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Export the commit outside the repository; the checkout is only read.
fn export_commit(repo: &Path, revision: &str) -> Result<tempfile::TempDir> {
    let work = tempfile::tempdir()?;
    let archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("archive")
        .arg(revision)
        .output()
        .map_err(|_| failure("cannot run git archive"))?;
    if !archive.status.success() {
        return Err(failure("git archive of CI_COMMIT_SHA failed"));
    }
    tar::Archive::new(archive.stdout.as_slice())
        .unpack(work.path())
        .map_err(|_| failure("cannot unpack the archived commit"))?;
    Ok(work)
}

/// Resolve the lock in the exported copy; npm output passes through.
/// `Ok(None)` is a failing npm invocation (exit 1, never a pass).
fn resolve_in_copy(work: &tempfile::TempDir) -> Result<Option<Vec<u8>>> {
    let mut command = Command::new("npm");
    command.args(NPM_ARGV).current_dir(work.path());
    let status = command
        .status()
        .map_err(|_| failure("cannot run npm install"))?;
    if !status.success() {
        return Ok(None);
    }
    fs::read(work.path().join(LOCK_NAME))
        .map_err(|_| failure("npm install left no package-lock.json"))
        .map(Some)
}

/// Retain the resolved lock via a temp file plus rename, so a failure leaves
/// no partial lock behind.
fn write_layout(root: &Path, revision: &str, resolved: &[u8]) -> Result<()> {
    let directory = layout_dir(root, revision);
    fs::create_dir_all(&directory)?;
    let staged = tempfile::NamedTempFile::new_in(&directory)?;
    fs::write(staged.path(), resolved)?;
    staged
        .persist(directory.join(LOCK_NAME))
        .map_err(|_| failure("cannot retain package-lock.json"))?;
    Ok(())
}

fn layout_dir(root: &Path, revision: &str) -> PathBuf {
    root.join(revision).join(LAYOUT_DIR)
}

fn report(resolved: &[u8], print_lock: bool) -> Result<()> {
    println!("{:x}  {}", Sha256::digest(resolved), LOCK_NAME);
    if print_lock {
        println!("---LOCK-BEGIN---");
        print!(
            "{}",
            std::str::from_utf8(resolved)
                .map_err(|_| failure("retained package-lock.json is not UTF-8"))?
        );
        if !resolved.ends_with(b"\n") {
            println!();
        }
        println!("---LOCK-END---");
    }
    Ok(())
}

#[cfg(test)]
mod tests;

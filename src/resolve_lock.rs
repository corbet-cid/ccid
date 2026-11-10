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
//! Rust lock resolution is deliberately not provided: a bare `cargo update`
//! moves third-party pins on its own (lock drift, HR2), and Rust pins have
//! their own mover outside ccid. Invoking this command without `--npm` is
//! refused.

use crate::{failure, load_manifest, Result, INTERRUPTED};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::Ordering,
};

/// Per-repository data for `ccid resolve-lock` (`[resolve]` in `.ci/ccid.toml`).
///
/// The npm job takes no per-repository selection: the command is identical in
/// every repository. The only knob is whether the retained lock is echoed to
/// stdout (the retired scripts differed; unified default: yes).
#[derive(Debug, Deserialize)]
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
            "Rust lock resolution is retired: a bare cargo update drifts third-party pins (use --npm for the npm lock)",
        ));
    }
    let (parsed, _) = load_manifest(repo, manifest)?;
    let config = parsed.resolve.unwrap_or_default();
    let artifact_root = artifact_root(repo)?;
    let revision = revision()?;
    let work = export_commit(repo, &revision)?;
    interrupted()?;
    let Some(resolved) = resolve_in_copy(&work)? else {
        eprintln!("ccid resolve-lock: npm install failed");
        return Ok(1);
    };
    // A cancelled run retains nothing (the scratch copy is removed on return).
    interrupted()?;
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
    if root
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(failure("ARTIFACT_ROOT must not contain '..'"));
    }
    let anchor = repo
        .canonicalize()
        .map_err(|error| failure(format!("cannot resolve --repo: {error}")))?;
    if resolved_path(root)?.starts_with(&anchor) {
        return Err(failure("ARTIFACT_ROOT must be outside --repo"));
    }
    Ok(())
}

/// `path` with its longest existing prefix canonicalized, so a symbolic link
/// into the repository is seen for what it is; the missing rest is appended
/// as written (it has no `..`, see the caller).
fn resolved_path(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(resolved) => {
                return Ok(rest.iter().rev().fold(resolved, |acc, part| acc.join(part)))
            }
            Err(error) => match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_owned());
                    existing = parent;
                }
                _ => return Err(failure(format!("cannot resolve ARTIFACT_ROOT: {error}"))),
            },
        }
    }
}

fn interrupted() -> Result<()> {
    if INTERRUPTED.load(Ordering::SeqCst) {
        return Err(failure("interrupted; nothing retained"));
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

/// Export the commit outside the repository; the checkout is only read. The
/// archive streams from git into the scratch directory (never held in
/// memory); git's own diagnostics pass through on stderr.
fn export_commit(repo: &Path, revision: &str) -> Result<tempfile::TempDir> {
    let work = tempfile::tempdir()?;
    let mut archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("archive")
        .arg(revision)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| failure(format!("cannot run git archive: {error}")))?;
    let stdout = archive
        .stdout
        .take()
        .ok_or_else(|| failure("git archive has no output pipe"))?;
    let mut reader = tar::Archive::new(stdout);
    let unpacked = reader.unpack(work.path());
    if unpacked.is_ok() {
        // Read git's trailing record padding to the end: closing the pipe
        // early would make a successful archive die of SIGPIPE.
        std::io::copy(&mut reader.into_inner(), &mut std::io::sink())?;
    } else {
        drop(reader);
    }
    let status = archive.wait()?;
    if !status.success() {
        return Err(failure(format!(
            "git archive of CI_COMMIT_SHA failed ({status})"
        )));
    }
    unpacked.map_err(|error| failure(format!("cannot unpack the archived commit: {error}")))?;
    Ok(work)
}

/// Resolve the lock in the exported copy; npm output passes through.
/// `Ok(None)` is a failing npm invocation (exit 1, never a pass).
fn resolve_in_copy(work: &tempfile::TempDir) -> Result<Option<Vec<u8>>> {
    let mut command = Command::new("npm");
    command.args(NPM_ARGV).current_dir(work.path());
    let status = command
        .status()
        .map_err(|error| failure(format!("cannot run npm install: {error}")))?;
    if !status.success() {
        return Ok(None);
    }
    fs::read(work.path().join(LOCK_NAME))
        .map_err(|error| failure(format!("npm install left no package-lock.json: {error}")))
        .map(Some)
}

/// Retain the resolved lock via a temp file plus rename, so a failure leaves
/// no partial lock behind. The file is world-readable (0644), as the retired
/// scripts' `cp` left it; a temp file alone would be 0600.
fn write_layout(root: &Path, revision: &str, resolved: &[u8]) -> Result<()> {
    let directory = layout_dir(root, revision);
    fs::create_dir_all(&directory)?;
    let mut staged = tempfile::NamedTempFile::new_in(&directory)?;
    staged.write_all(resolved)?;
    staged.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        staged
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    staged
        .persist(directory.join(LOCK_NAME))
        .map_err(|error| failure(format!("cannot retain package-lock.json: {error}")))?;
    Ok(())
}

fn layout_dir(root: &Path, revision: &str) -> PathBuf {
    root.join(revision).join(LAYOUT_DIR)
}

fn report(resolved: &[u8], print_lock: bool) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{:x}  {}", Sha256::digest(resolved), LOCK_NAME)?;
    if print_lock {
        writeln!(out, "---LOCK-BEGIN---")?;
        out.write_all(resolved)?;
        if !resolved.ends_with(b"\n") {
            writeln!(out)?;
        }
        writeln!(out, "---LOCK-END---")?;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests;

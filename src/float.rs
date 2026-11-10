//! Float the first-party git dependencies to the newest commit of their branch, OUTSIDE the
//! declared source (`docs/verdicts.md`, "Resolution before keying").
//!
//! Selection: every package of every committed lock whose source is a branch-tracking git URL
//! (`?branch=`) that the release channel's fleet data classifies as first-party (a canonical
//! owner prefix or a legacy alias, any spelling of the forge host). Resolution: exactly
//! `cargo update -p <git+url#name@version>` of those packages, one invocation per lock
//! directory, in a copy of the checkout under ccid's scratch. Nothing ccid does here writes the
//! checkout: the content a verdict or a moon task is keyed on stays the committed content.
//!
//! The [`Resolution`] names the resolved locks, the commit of every git package they pin and a
//! digest of both. The digest is declared environment (`CCID_GIT_RESOLUTION`, appended to the
//! `cache_env` of every selected check) so the verdict store and cmnp's moon tasks key the exact
//! locks the checks build. The checks run in a staged copy of the checkout with the resolved
//! locks overlaid. A child `ccid check` (a moon task, a nested run) receives the locks in
//! `CCID_RESOLVED_LOCKS`, recomputes the digest and refuses a mismatch; it never resolves again.

use crate::{cache, cached_verdict, compile_cache, event, failure, fleet, set, value};
use crate::{Environment, Result};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

/// The digest of the resolution a run keys and builds (lowercase sha256 hex). Declared
/// environment of every selected check of a floated run.
pub const RESOLUTION_ENV: &str = "CCID_GIT_RESOLUTION";
/// A directory holding the resolved locks at their repository-relative paths. Plumbing for a
/// child run: never a key input (the digest names the content).
pub const LOCKS_ENV: &str = "CCID_RESOLVED_LOCKS";

/// The resolved locks of one float.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// Every lock of the committed tree, by repository-relative path, as resolved.
    pub locks: BTreeMap<String, Vec<u8>>,
    /// The commit of every git package the resolved locks pin, keyed
    /// `<lock path>:<package>:<url>` (the verdict key's `git_dependencies`).
    pub commits: BTreeMap<String, String>,
    /// sha256 of the canonical JSON `{"commits": …, "locks": {<path>: <sha256 of the bytes>}}`.
    pub digest: String,
}

/// The canonical document the digest names. Field order and sorted maps make it unique.
#[derive(Serialize)]
struct Canonical<'a> {
    commits: &'a BTreeMap<String, String>,
    locks: BTreeMap<&'a str, String>,
}

impl Resolution {
    /// The resolution these lock bytes describe.
    pub fn from_locks(locks: BTreeMap<String, Vec<u8>>) -> Result<Self> {
        let mut texts = Vec::new();
        for (path, bytes) in &locks {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| failure(format!("Resolved lock {path} is not UTF-8")))?;
            texts.push((path.as_str(), text));
        }
        let commits = cached_verdict::lock_dependencies(texts)?;
        let digest = digest_of(&commits, &locks)?;
        Ok(Self {
            locks,
            commits,
            digest,
        })
    }

    /// Write every resolved lock under `directory` at its repository-relative path.
    pub fn write_into(&self, directory: &Path) -> Result<()> {
        for (path, bytes) in &self.locks {
            let destination = directory.join(relative(path)?);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            refuse_link(&destination)?;
            fs::write(&destination, bytes)?;
        }
        Ok(())
    }
}

fn digest_of(
    commits: &BTreeMap<String, String>,
    locks: &BTreeMap<String, Vec<u8>>,
) -> Result<String> {
    let canonical = Canonical {
        commits,
        locks: locks
            .iter()
            .map(|(path, bytes)| (path.as_str(), format!("{:x}", Sha256::digest(bytes))))
            .collect(),
    };
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical)?)
    ))
}

/// A repository-relative lock path as a plain relative path (no `..`, no root).
fn relative(path: &str) -> Result<PathBuf> {
    let relative = PathBuf::from(path);
    if path.is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(failure(format!(
            "Lock path {path:?} is not repository-relative"
        )));
    }
    Ok(relative)
}

/// A path that exists as a symbolic link would redirect a write; refuse it.
fn refuse_link(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(failure(format!(
            "{} is a symbolic link; refusing to write through it",
            path.display()
        ))),
        Ok(meta) if !meta.is_file() => {
            Err(failure(format!("{} is not a regular file", path.display())))
        }
        _ => Ok(()),
    }
}

/// The commit the checkout has checked out, if it is a repository with one. The checkout's own
/// HEAD, never an inherited `CI_COMMIT_SHA` (a nested run of another repository inherits one).
fn head(root: &Path) -> Option<String> {
    if !cached_verdict::is_repository(root) {
        return None;
    }
    git(root, &["rev-parse", "--verify", "-q", "HEAD^{commit}"])
        .ok()
        .filter(|commit| commit.len() == 40)
}

/// The lock paths of the committed tree: from HEAD's tree in a repository, from the files
/// otherwise (an export, or a repository without a commit, has no tree to ask).
fn committed_lock_paths(root: &Path) -> Result<Vec<String>> {
    match head(root) {
        Some(commit) => cached_verdict::lock_paths(root, &commit),
        None => cached_verdict::walked_lock_paths(root),
    }
}

/// One branch-tracking git package of one lock.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tracking {
    lock: String,
    /// The lock's source URL without `git+` and without the `#<commit>` fragment.
    url: String,
    spec: String,
}

fn branch_tracking(lock: &str, text: &str) -> Result<Vec<Tracking>> {
    let value: toml::Value = toml::from_str(text)
        .map_err(|error| failure(format!("Lock {lock} is not TOML: {error}")))?;
    let mut found = Vec::new();
    for package in value
        .get("package")
        .and_then(|packages| packages.as_array())
        .into_iter()
        .flatten()
    {
        let (Some(name), Some(version), Some(source)) = (
            package.get("name").and_then(|name| name.as_str()),
            package.get("version").and_then(|version| version.as_str()),
            package.get("source").and_then(|source| source.as_str()),
        ) else {
            continue;
        };
        let Some(rest) = source.strip_prefix("git+") else {
            continue;
        };
        let Some((url, _)) = rest.split_once('#') else {
            continue;
        };
        // Only branch-tracking packages move; a pinned `rev` or `tag` is committed content.
        if !url.contains("branch=") {
            continue;
        }
        found.push(Tracking {
            lock: lock.to_owned(),
            url: url.to_owned(),
            // The lock's own URL spelling: Cargo must find the package under the source it recorded.
            spec: cached_verdict::git_spec(url, name, version),
        });
    }
    Ok(found)
}

/// The first-party specs to float, per lock path.
type Selection = BTreeMap<String, BTreeSet<String>>;

/// The packages to float, per lock path: branch-tracking and first-party by the fleet data. The
/// host of `CI_REPOSITORY_URL` is never consulted. Fleet data is loaded only when a lock tracks a
/// branch, and its absence then is an error (never a silent "nothing to float").
fn select(
    committed: &BTreeMap<String, Vec<u8>>,
    environment: &Environment,
) -> Result<Option<(Selection, fleet::Fleet)>> {
    let mut tracking = Vec::new();
    for (lock, bytes) in committed {
        let text =
            std::str::from_utf8(bytes).map_err(|_| failure(format!("Lock {lock} is not UTF-8")))?;
        tracking.extend(branch_tracking(lock, text)?);
    }
    if tracking.is_empty() {
        return Ok(None);
    }
    let (fleet, _) = fleet::load(environment).map_err(|error| {
        failure(format!(
            "{} branch-tracking git dependencies need fleet data to select the first-party ones: {error}",
            tracking.len()
        ))
    })?;
    let mut selected = Selection::new();
    for package in tracking {
        if fleet.classify(&package.url).is_some() {
            selected
                .entry(package.lock)
                .or_default()
                .insert(package.spec);
        }
    }
    Ok((!selected.is_empty()).then_some((selected, fleet)))
}

/// What a float did besides the locks: how many packages it asked Cargo to move, and every
/// third-party lock entry that differs from the committed lock (each one forced by a moved
/// first-party package's own requirements; HR2: reported, never silent).
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub packages: usize,
    pub third_party: Vec<serde_json::Value>,
}

/// One float in a fresh resolution stage under `scratch`: copy the checkout's content there, run
/// `cargo update -p <spec>…` per lock directory, read the resolved locks back. `None` when
/// nothing is selected. The checkout is only read.
pub(crate) fn resolve_in(
    root: &Path,
    environment: &Environment,
    scratch: impl FnOnce() -> Result<tempfile::TempDir>,
) -> Result<Option<(Resolution, Report, tempfile::TempDir)>> {
    let paths = committed_lock_paths(root)?;
    let mut committed = BTreeMap::new();
    for path in &paths {
        let bytes = fs::read(root.join(relative(path)?))
            .map_err(|error| failure(format!("Committed lock {path} is unreadable: {error}")))?;
        committed.insert(path.clone(), bytes);
    }
    let Some((selected, fleet)) = select(&committed, environment)? else {
        return Ok(None);
    };
    // Only a run that floats something creates a resolution stage.
    let scratch = scratch()?;
    let stage = scratch.path().join("resolution");
    fs::create_dir(&stage)?;
    cache::copy_source(root, &stage, true)?;
    let cargo = compile_cache::executable(environment, "cargo")
        .ok_or_else(|| failure("Dependency float cannot find cargo on PATH"))?;
    let mut packages = 0;
    for (lock, specs) in &selected {
        let file = stage.join(relative(lock)?);
        let meta = fs::symlink_metadata(&file)?;
        if !meta.is_file() {
            return Err(failure(format!(
                "Lock {lock} is not a regular file; refusing to resolve it"
            )));
        }
        let directory = file.parent().unwrap_or(&stage);
        let mut arguments = vec!["update".to_owned()];
        for spec in specs {
            arguments.push("-p".into());
            arguments.push(spec.clone());
        }
        let output = Command::new(&cargo)
            .args(&arguments)
            .current_dir(directory)
            .env_clear()
            .envs(environment.iter())
            .output()
            .map_err(|error| failure(format!("Dependency float cannot run cargo: {error}")))?;
        if !output.status.success() {
            return Err(failure(format!(
                "Dependency float cannot resolve {} in {lock}: {}",
                specs.iter().cloned().collect::<Vec<_>>().join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        packages += specs.len();
    }
    let mut locks = BTreeMap::new();
    for path in &paths {
        locks.insert(path.clone(), fs::read(stage.join(relative(path)?))?);
    }
    fs::remove_dir_all(&stage)?;
    let resolution = Resolution::from_locks(locks)?;
    let report = Report {
        packages,
        third_party: third_party_changes(&committed, &resolution.locks, &fleet)?,
    };
    Ok(Some((resolution, report, scratch)))
}

/// Third-party entries (a source the fleet does not classify as first-party) of every lock that
/// differ between `before` and `after`: added, removed, or changed (dependencies, checksum).
fn third_party_changes(
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
    fleet: &fleet::Fleet,
) -> Result<Vec<serde_json::Value>> {
    type Entries = BTreeMap<(String, String, String), toml::Value>;
    let entries = |bytes: &[u8]| -> Result<Entries> {
        let value: toml::Value = toml::from_str(std::str::from_utf8(bytes)?)?;
        let mut out = Entries::new();
        for package in value
            .get("package")
            .and_then(|packages| packages.as_array())
            .into_iter()
            .flatten()
        {
            let field = |name: &str| {
                package
                    .get(name)
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            };
            let (Some(name), Some(version), Some(source)) =
                (field("name"), field("version"), field("source"))
            else {
                continue;
            };
            if fleet.classify(&source).is_some() {
                continue;
            }
            out.insert((name, version, source), package.clone());
        }
        Ok(out)
    };
    let mut changes = Vec::new();
    for (path, old) in before {
        let Some(new) = after.get(path) else {
            continue;
        };
        if old == new {
            continue;
        }
        let (old, new) = (entries(old)?, entries(new)?);
        let keys: BTreeSet<&(String, String, String)> = old.keys().chain(new.keys()).collect();
        for key in keys {
            let change = match (old.get(key), new.get(key)) {
                (Some(a), Some(b)) if a == b => continue,
                (Some(_), Some(_)) => "changed",
                (Some(_), None) => "removed",
                (None, Some(_)) => "added",
                (None, None) => continue,
            };
            changes.push(json!({
                "lock": path,
                "name": key.0,
                "version": key.1,
                "source": key.2,
                "change": change,
            }));
        }
    }
    Ok(changes)
}

/// A float that a run keys and builds, and the scratch that holds its locks for child runs.
pub(crate) struct Handover {
    pub(crate) resolution: Resolution,
    _owned: Option<tempfile::TempDir>,
}

/// Before anything of a selection is keyed or run. An inherited `CCID_GIT_RESOLUTION` is the
/// resolution an enclosing run keyed: its locks (`CCID_RESOLVED_LOCKS`) are read and verified,
/// never resolved again. A moon task (`cache_child`) without one was keyed without a float and
/// floats nothing. Otherwise this resolves; a non-empty resolution is bound into `environment`
/// (digest and locks directory) for every child.
pub(crate) fn establish(
    root: &Path,
    environment: &mut Environment,
    cache_child: bool,
) -> Result<Option<Handover>> {
    if let Some(digest) = value(environment, RESOLUTION_ENV) {
        let resolution = received(root, environment, &digest)?;
        return Ok(Some(Handover {
            resolution,
            _owned: None,
        }));
    }
    if value(environment, LOCKS_ENV).is_some() {
        return Err(failure(format!(
            "{LOCKS_ENV} is set without {RESOLUTION_ENV}: resolved locks without the digest they were keyed with"
        )));
    }
    if cache_child {
        return Ok(None);
    }
    let Some((resolution, report, scratch)) = resolve_in(root, environment, || {
        cache::scratch(&mut environment.clone())
    })?
    else {
        return Ok(None);
    };
    let directory = scratch.path().join("locks");
    resolution.write_into(&directory)?;
    event(json!({
        "event": "float-resolve",
        "packages": report.packages,
        "digest": resolution.digest,
        "third_party": report.third_party,
    }));
    set(environment, RESOLUTION_ENV, resolution.digest.clone());
    set(environment, LOCKS_ENV, directory.as_os_str());
    Ok(Some(Handover {
        resolution,
        _owned: Some(scratch),
    }))
}

/// The resolution an enclosing run handed over, verified against the digest it was keyed with.
fn received(root: &Path, environment: &Environment, digest: &str) -> Result<Resolution> {
    let directory = value(environment, LOCKS_ENV).map(PathBuf::from).ok_or_else(|| {
        failure(format!(
            "{RESOLUTION_ENV} is set but {LOCKS_ENV} is not: the enclosing run's resolved locks did not reach this run (a moon task needs cmnp to forward {LOCKS_ENV} as plumbing)"
        ))
    })?;
    let mut locks = BTreeMap::new();
    for path in committed_lock_paths(root)? {
        let file = directory.join(relative(&path)?);
        let bytes = fleet::read_regular(&file).map_err(|error| {
            failure(format!(
                "{LOCKS_ENV} {}: resolved lock {path}: {error}",
                directory.display()
            ))
        })?;
        locks.insert(path, bytes);
    }
    let resolution = Resolution::from_locks(locks)?;
    if resolution.digest != digest {
        return Err(failure(format!(
            "The resolved locks in {LOCKS_ENV} {} name resolution {}, but this run was keyed with {RESOLUTION_ENV}={digest}",
            directory.display(),
            resolution.digest
        )));
    }
    Ok(resolution)
}

/// Declare the resolution digest in the `cache_env` of every selected check (after the fleet
/// digest), so the verdict identity and the moon task identity both name it. Called only for a
/// non-empty resolution: a repository without a float keys exactly as before.
pub(crate) fn bind(checks: &mut BTreeMap<String, crate::Check>, selected: &[String]) {
    for name in selected {
        if let Some(check) = checks.get_mut(name) {
            if !check.cache_env.iter().any(|entry| entry == RESOLUTION_ENV) {
                check.cache_env.push(RESOLUTION_ENV.to_owned());
            }
        }
    }
}

/// Checked programs never inherit the hand-over: a nested ccid run of another repository (a
/// test suite, a land tool) must resolve its own locks, never verify against these.
pub(crate) fn withhold(environment: &mut Environment) {
    environment.remove(OsStr::new(RESOLUTION_ENV));
    environment.remove(OsStr::new(LOCKS_ENV));
}

/// Make a staged copy of the checkout build the resolution: overlay the resolved locks and give
/// the stage git metadata of its own when the checkout is a repository, so `git ls-files` lists
/// the committed tree and `git rev-parse HEAD` prints the real commit. The checkout is only read.
pub(crate) fn stage(checkout: &Path, stage: &Path, resolution: &Resolution) -> Result<()> {
    resolution.write_into(stage)?;
    if let Some(commit) = head(checkout) {
        attach_git(checkout, stage, &commit)?;
    }
    event(json!({
        "event": "float-stage",
        "digest": resolution.digest,
        "path": stage,
    }));
    Ok(())
}

/// Git commands of the staging: never redirected to another repository by the environment.
fn git(directory: &Path, arguments: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.args(arguments).current_dir(directory);
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
    ] {
        command.env_remove(name);
    }
    let output = command
        .output()
        .map_err(|error| failure(format!("Dependency float needs git: {error}")))?;
    if !output.status.success() {
        return Err(failure(format!(
            "Dependency float git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// A private repository in the stage that borrows the checkout's objects (read-only, through
/// `objects/info/alternates`): HEAD detached at `commit`, the index read from its tree. No
/// worktree is registered in the checkout's `.git`, so nothing is left to unregister after a crash.
fn attach_git(checkout: &Path, stage: &Path, commit: &str) -> Result<()> {
    let objects = git(
        checkout,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ],
    )?;
    let shallow = git(
        checkout,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "shallow",
        ],
    )?;
    let metadata = stage.join(".git");
    if fs::symlink_metadata(&metadata).is_ok() {
        return Err(failure(
            "The staged source already holds .git; refusing to replace it",
        ));
    }
    git(
        stage,
        &[
            "-c",
            "init.defaultBranch=main",
            "init",
            "--quiet",
            "--template=",
        ],
    )?;
    let info = metadata.join("objects/info");
    fs::create_dir_all(&info)?;
    fs::write(info.join("alternates"), format!("{objects}\n"))?;
    if Path::new(&shallow).is_file() {
        fs::copy(&shallow, metadata.join("shallow"))?;
    }
    git(stage, &["update-ref", "--no-deref", "HEAD", commit])?;
    git(stage, &["read-tree", commit])?;
    Ok(())
}

/// After a staged run: the locks the run built must still be the resolution's. A check that
/// rewrote them (an in-check float) built inputs no key names; the run reports it, and the
/// verdict store refuses to record it (`receipt_reports_moved_locks`).
pub(crate) fn report_moved_locks(stage: &Path, resolution: &Resolution) -> Result<()> {
    let mut moved = Vec::new();
    for (path, bytes) in &resolution.locks {
        if fs::read(stage.join(relative(path)?)).ok().as_ref() != Some(bytes) {
            moved.push(path.clone());
        }
    }
    if !moved.is_empty() {
        event(json!({
            "event": MOVED_EVENT,
            "digest": resolution.digest,
            "locks": moved,
        }));
    }
    Ok(())
}

const MOVED_EVENT: &str = "float-locks-moved";

/// Whether the receipt of `check` (`.ccid/results/<check>.jsonl`) reports staged locks the check
/// moved while it ran.
pub(crate) fn receipt_reports_moved_locks(root: &Path, check: &str) -> bool {
    let Ok(text) = fs::read_to_string(root.join(".ccid/results").join(format!("{check}.jsonl")))
    else {
        return false;
    };
    text.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .is_ok_and(|event| event["event"] == MOVED_EVENT)
    })
}

/// The absolute form of `path` with every existing ancestor resolved (links included), so a
/// destination can be compared with a canonical repository root before it exists.
fn resolved_destination(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut resolved = canonical;
            for component in rest.iter().rev() {
                resolved.push(component);
            }
            return Ok(resolved);
        }
        let Some(name) = existing.file_name() else {
            return Err(failure(format!("{} cannot be resolved", path.display())));
        };
        rest.push(name.to_owned());
        existing = existing
            .parent()
            .ok_or_else(|| failure(format!("{} cannot be resolved", path.display())))?;
    }
}

/// `ccid float --repo REPO --into DIR`: the resolution only. Writes the resolved locks under DIR
/// at their repository-relative paths and prints one JSON line. Never writes the repository and
/// refuses a DIR inside it.
pub fn run(repo: &Path, into: &Path) -> Result<u8> {
    let environment: Environment = std::env::vars_os().collect();
    let root = repo.canonicalize()?;
    let destination = resolved_destination(into)?;
    if destination.starts_with(&root) {
        return Err(failure(format!(
            "--into {} is inside the repository {}; the float never writes the repository",
            into.display(),
            root.display()
        )));
    }
    let outcome = resolve_in(&root, &environment, || {
        cache::quiet_scratch(&mut environment.clone())
    })?;
    let line = match outcome {
        None => json!({"packages": 0, "digest": null, "commits": {}, "third_party": []}),
        Some((resolution, report, _scratch)) => {
            for path in resolution.locks.keys() {
                if destination.join(relative(path)?).starts_with(&root) {
                    return Err(failure(format!(
                        "--into {}: the resolved lock {path} would land inside the repository",
                        into.display()
                    )));
                }
            }
            resolution.write_into(&destination)?;
            json!({
                "packages": report.packages,
                "digest": resolution.digest,
                "commits": resolution.commits,
                "third_party": report.third_party,
            })
        }
    };
    println!("{line}");
    Ok(0)
}

#[cfg(test)]
mod tests;

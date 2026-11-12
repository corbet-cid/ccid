//! The shared cached-check verdict: one store that a land gate writes and CI reads.
//!
//! A verdict answers exactly one question for exactly one check: "did this check pass on this
//! content?" Its key is content only (see [`KeyComponents`]): the content of the committed tree,
//! every git dependency commit the builds resolve to, the semantic identity of the tools the
//! check uses, the shared build profile, the check's declaration and declared environment, and
//! the verdict epoch. No path, URL, commit, run id, timestamp or host name is part of the key.
//!
//! The record names the ccid binary that produced it (bytes and source revision), the repository
//! and commit it came from, and whether a lane or CI produced it. CI accepts a record only when
//! every key component equals the ones it computes and the producing binary is named by the
//! release channel (or is the very channel binary CI is running); anything else is a miss, never
//! a partial match.
//!
//! Coverage is a check like any other. A repository that declares its coverage work in a check
//! (or in the `verify` check that contains it) gets one verdict per tree; a later nightly
//! coverage run that selects the same check finds the record and builds nothing.

use crate::{checks, event, failure, value, Environment, Result, DEV_PROFILE, SOURCE_REVISION};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Schema of the key and the record. Unknown schemas are refused, never guessed.
pub const SCHEMA: u32 = 2;
/// Namespace below `CCID_RESULT_CACHE` that holds the verdict records.
pub const STORE_DIR: &str = "verdicts";
/// An attempt-state or verdict record is tiny; anything larger is refused without parsing.
const MAX_RECORD_BYTES: u64 = 256 * 1024;
/// How long a writer waits for the per-key lock before giving up.
const LOCK_WAIT: Duration = Duration::from_secs(60);
/// Generated directories that are not part of the committed tree. Git's own path list already
/// excludes them; an archive export (no `.git`) is walked with the same exclusion.
const GENERATED: &[&str] = &[".git", ".moon", ".ccid", "target", "node_modules"];

/// Every content input of one check's verdict, in canonical order.
///
/// `tree` is the SHA-256 of the tree's content (path, kind and bytes of every committed file):
/// a CI checkout of a commit and a land export of the same commit produce the same value.
/// `git_dependencies` maps every git-sourced package of every `Cargo.lock` (root and nested) to
/// the commit the build resolves it to, keyed `<lock path>:<package>:<url>`; reading it after the
/// producer's dependency resolution makes it the RESOLVED commit, not the committed pin.
/// `toolchain` maps a stable role to the SHA-256 of the tool's semantic identity (release and
/// target; never the installed bytes). `profile` is the
/// SHA-256 of the shared build profile (`profiles/dev.toml`) compiled into the producing binary.
/// `check_identity` is the SHA-256 of the check declaration plus the values of the declared
/// environment (`SEMANTIC_ENV` plus the check's `cache_env`), never of ambient run state.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyComponents {
    pub schema: u32,
    pub tree: String,
    pub git_dependencies: BTreeMap<String, String>,
    pub toolchain: BTreeMap<String, String>,
    pub profile: String,
    pub epoch: u32,
    pub check: String,
    pub check_identity: String,
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_plain(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

impl KeyComponents {
    /// Fail-closed validation of every component before it can name a record.
    pub fn validate(&self) -> Result<()> {
        if self.schema != SCHEMA {
            return Err(failure("Unknown verdict-key schema"));
        }
        if !is_hex(&self.tree, 64) {
            return Err(failure("Verdict key tree must be a sha256 tree digest"));
        }
        for (package, commit) in &self.git_dependencies {
            if package.is_empty() || package.contains('\0') || !is_hex(commit, 40) {
                return Err(failure(
                    "Verdict key git dependencies must name a package and a full commit id",
                ));
            }
        }
        if self.toolchain.is_empty() {
            return Err(failure("Verdict key requires at least one tool identity"));
        }
        for (role, digest) in &self.toolchain {
            if !is_plain(role) || !is_hex(digest, 64) {
                return Err(failure(
                    "Verdict key tools must be a plain role and a sha256 digest",
                ));
            }
        }
        if !is_hex(&self.profile, 64) {
            return Err(failure("Verdict key profile must be a sha256 digest"));
        }
        if self.epoch != crate::cached::VERDICT_EPOCH {
            return Err(failure("Verdict key names a different verdict epoch"));
        }
        if !is_plain(&self.check) {
            return Err(failure("Verdict key check must be a plain name"));
        }
        if !is_hex(&self.check_identity, 64) {
            return Err(failure(
                "Verdict key check identity must be a sha256 digest",
            ));
        }
        Ok(())
    }

    /// Canonical bytes: fixed field order, sorted maps. The key is the SHA-256 of these bytes.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }

    pub fn key_hex(&self) -> Result<String> {
        Ok(format!("{:x}", Sha256::digest(self.canonical_bytes()?)))
    }
}

/// Whether a lane or CI produced a record. It is provenance, never a key input: CI accepts a
/// lane's verdict and a later lane run accepts CI's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProducerKind {
    Lane,
    Ci,
}

impl ProducerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProducerKind::Lane => "lane",
            ProducerKind::Ci => "ci",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Producer {
    pub kind: ProducerKind,
    /// Free-form provenance (a host, a lane name, a pipeline URL); never a key input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

/// The ccid binary that produced a record: the digest of its bytes and the source revision it
/// was built from. CI accepts only a pair named by the release channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub sha256: String,
    pub source_revision: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultWord {
    Success,
    Failure,
}

/// One check's verdict for one key. Only `success` is ever accepted; a `failure` record is kept
/// for operators and is never a pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub schema: u32,
    pub key: String,
    pub components: KeyComponents,
    /// The exact commit the tree came from (provenance; the tree is the identity).
    pub commit: String,
    /// Canonical repository identity (provenance; never a key input).
    #[serde(default)]
    pub repository: String,
    pub result: ResultWord,
    pub producer: Producer,
    pub tool: Tool,
    /// Unix seconds, UTC.
    pub started_at: u64,
    pub finished_at: u64,
    pub duration_seconds: u64,
}

impl Record {
    fn validate(&self, key: &str) -> Result<()> {
        if self.schema != SCHEMA {
            return Err(failure("Unknown verdict-record schema"));
        }
        if self.key != key {
            return Err(failure("Verdict record names a different key"));
        }
        if !is_hex(&self.key, 64) {
            return Err(failure("Verdict record key must be a sha256 digest"));
        }
        self.components.validate()?;
        if self.components.key_hex()? != self.key {
            return Err(failure(
                "Verdict record components do not hash to the record key",
            ));
        }
        if !is_hex(&self.commit, 40) {
            return Err(failure("Verdict record commit must be a full Git commit"));
        }
        if !is_hex(&self.tool.sha256, 64) {
            return Err(failure(
                "Verdict record tool digest must be a sha256 digest",
            ));
        }
        if self.finished_at < self.started_at {
            return Err(failure("Verdict record ends before it starts"));
        }
        Ok(())
    }
}

/// The outcome of a lookup. A hit is the only outcome that skips the check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Hit,
    Miss,
    Invalid,
    Disabled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Hit => "hit",
            Status::Miss => "miss",
            Status::Invalid => "invalid",
            Status::Disabled => "disabled",
        }
    }
}

/// Everything one selected check needs for a lookup or a record.
pub struct Prepared {
    pub check: String,
    pub key: String,
    pub components: KeyComponents,
    pub commit: String,
    repository: String,
    /// The producer's branch, provenance for output retention only. It never enters the key.
    branch: String,
    root: PathBuf,
    /// The exact tree list the key was built from, so the post-run check re-reads the same
    /// paths and ignores outputs the check wrote beside them.
    tree_entries: Vec<TreeEntry>,
    /// The git dependency commits of the checkout's own locks when the key was built. A floated
    /// key names the resolution's commits instead; the checkout's locks must still read the same
    /// after the run, because ccid never writes them.
    checkout_dependencies: BTreeMap<String, String>,
    /// `CCID_RESULT_CACHE`; the verdict store is its `verdicts/` child.
    cache: Option<PathBuf>,
    store: Option<PathBuf>,
    /// The check's declared `cache_outputs`, normalized and validated.
    outputs: Vec<String>,
}

/// One lookup result, with the reason a record was not accepted.
pub struct Lookup {
    pub status: Status,
    pub reason: Option<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| failure(format!("Verdict key needs git: {error}")))?;
    if !output.status.success() {
        return Err(failure(format!(
            "Verdict key git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn git_raw(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| failure(format!("Verdict key needs git: {error}")))?;
    if !output.status.success() {
        return Err(failure(format!(
            "Verdict key git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8(output.stdout)?)
}

pub(crate) fn is_repository(root: &Path) -> bool {
    root.join(".git").exists()
}

/// The commit whose tree is judged. Explicit `--commit` wins, then `CI_COMMIT_SHA`, then HEAD.
pub fn commit(root: &Path, explicit: Option<&str>, environment: &Environment) -> Result<String> {
    let commit = match explicit {
        Some(commit) => commit.to_owned(),
        None => match value(environment, "CI_COMMIT_SHA") {
            Some(commit) => commit,
            None => git(root, &["rev-parse", "HEAD"])?,
        },
    };
    if !is_hex(&commit, 40) {
        return Err(failure("Verdict key requires a full 40-hex commit id"));
    }
    // In a checkout the commit must exist; an archive export has no repository to ask.
    if is_repository(root) {
        git(
            root,
            &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
        )?;
    }
    Ok(commit)
}

fn file_sha256(path: &Path) -> Result<String> {
    crate::sha256_file(path)
}

fn mode_for_meta(meta: &fs::Metadata) -> &'static str {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return "100755";
        }
    }
    "100644"
}

/// One entry of the tree: a path, its Git kind and, for a submodule, the commit it names.
struct TreeEntry {
    path: String,
    mode: &'static str,
    submodule: Option<String>,
}

/// Walk one exported tree (no `.git`) exactly as `git archive` laid it out.
fn archive_entries(root: &Path, path: &Path, out: &mut Vec<TreeEntry>) -> Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(path)?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for entry in entries {
        let name = entry
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| failure("Verdict key paths require UTF-8"))?;
        if GENERATED.contains(&name) && entry.is_dir() {
            continue;
        }
        let relative = entry
            .strip_prefix(root)?
            .to_str()
            .ok_or_else(|| failure("Verdict key paths require UTF-8"))?
            .replace('\\', "/");
        if relative == "moon.yml" {
            continue;
        }
        let meta = fs::symlink_metadata(&entry)?;
        if meta.is_dir() {
            archive_entries(root, &entry, out)?;
        } else if meta.is_symlink() {
            out.push(TreeEntry {
                path: relative,
                mode: "120000",
                submodule: None,
            });
        } else if meta.is_file() {
            out.push(TreeEntry {
                path: relative,
                mode: mode_for_meta(&meta),
                submodule: None,
            });
        } else {
            return Err(failure("Verdict key refuses special files"));
        }
    }
    Ok(())
}

fn tree_entries(root: &Path, commit: &str) -> Result<Vec<TreeEntry>> {
    let mut entries: Vec<TreeEntry> = Vec::new();
    if is_repository(root) {
        let output = Command::new("git")
            .args(["ls-tree", "-r", "-z", "--full-tree", commit])
            .current_dir(root)
            .output()
            .map_err(|error| failure(format!("Verdict key needs git: {error}")))?;
        if !output.status.success() {
            return Err(failure(format!(
                "Verdict key cannot list the commit tree: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        for record in output.stdout.split(|byte| *byte == 0) {
            if record.is_empty() {
                continue;
            }
            let record = std::str::from_utf8(record)
                .map_err(|_| failure("Verdict key tree paths require UTF-8"))?;
            let (head, path) = record
                .split_once('\t')
                .ok_or_else(|| failure("Verdict key cannot parse the commit tree"))?;
            let mut parts = head.split_whitespace();
            let mode = parts.next().unwrap_or_default();
            let kind = parts.next().unwrap_or_default();
            let object = parts.next().unwrap_or_default();
            match kind {
                "blob" => entries.push(TreeEntry {
                    path: path.to_owned(),
                    mode: if mode == "100755" { "100755" } else { "100644" },
                    submodule: None,
                }),
                // A submodule's content is another commit, which the tree names exactly.
                "commit" => entries.push(TreeEntry {
                    path: path.to_owned(),
                    mode: "160000",
                    submodule: Some(object.to_owned()),
                }),
                other => {
                    return Err(failure(format!(
                        "Verdict key does not know tree entry kind {other:?}"
                    )))
                }
            }
        }
    } else {
        archive_entries(root, root, &mut entries)?;
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn entries_digest(root: &Path, entries: &[TreeEntry]) -> Result<String> {
    let mut hash = Sha256::new();
    for entry in entries {
        let payload = if let Some(object) = &entry.submodule {
            format!("commit:{object}")
        } else {
            let full = root.join(&entry.path);
            let meta = fs::symlink_metadata(&full).map_err(|error| {
                failure(format!("Verdict key cannot read {}: {error}", entry.path))
            })?;
            if entry.mode == "120000" {
                let target = fs::read_link(&full)?;
                format!(
                    "link:{}",
                    target
                        .to_str()
                        .ok_or_else(|| failure("Verdict key symlink target requires UTF-8"))?
                )
            } else if meta.is_file() {
                format!("sha256:{}", file_sha256(&full)?)
            } else {
                return Err(failure(format!(
                    "Verdict key tree entry {} is not a file",
                    entry.path
                )));
            }
        };
        hash.update(entry.path.as_bytes());
        hash.update([0]);
        hash.update(entry.mode.as_bytes());
        hash.update([0]);
        hash.update(payload.as_bytes());
        hash.update(b"\n");
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
fn tree_digest(root: &Path, commit: &str) -> Result<String> {
    entries_digest(root, &tree_entries(root, commit)?)
}

/// Directories whose committed locks never decide a build (fuzz harnesses, vendored trees,
/// installed packages). The same exclusion the land's nested-lock scan uses.
const LOCK_SKIP: &[&str] = &["fuzz", "target", "node_modules", "vendor"];

pub(crate) fn lock_paths(root: &Path, commit: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    if is_repository(root) {
        let listing = git(root, &["ls-tree", "-r", "--name-only", commit])?;
        for path in listing.lines() {
            if path.rsplit('/').next() == Some("Cargo.lock")
                && !path
                    .split('/')
                    .any(|component| LOCK_SKIP.contains(&component))
            {
                paths.push(path.to_owned());
            }
        }
    } else {
        paths = walked_lock_paths(root)?;
    }
    Ok(paths)
}

/// The locks of an exported tree (no repository to ask), with the same exclusions.
pub(crate) fn walked_lock_paths(root: &Path) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    {
        fn walk(root: &Path, path: &Path, out: &mut Vec<String>) -> Result<()> {
            let mut entries: Vec<PathBuf> = fs::read_dir(path)?
                .collect::<std::io::Result<Vec<_>>>()?
                .into_iter()
                .map(|entry| entry.path())
                .collect();
            entries.sort();
            for entry in entries {
                let name = entry.file_name().and_then(OsStr::to_str).unwrap_or("");
                if (GENERATED.contains(&name) || LOCK_SKIP.contains(&name)) && entry.is_dir() {
                    continue;
                }
                if entry.is_dir() {
                    walk(root, &entry, out)?;
                } else if name == "Cargo.lock" {
                    let relative = entry
                        .strip_prefix(root)?
                        .to_str()
                        .ok_or_else(|| failure("Verdict key paths require UTF-8"))?
                        .replace('\\', "/");
                    out.push(relative);
                }
            }
            Ok(())
        }
        walk(root, root, &mut paths)?;
    }
    Ok(paths)
}

/// Every git dependency commit of every lock **as it stands in the working tree**, keyed
/// `<lock path>:<package>:<url>`. ccid never rewrites these locks: a floated run keys the
/// resolution's commits instead (`float::Resolution::commits`, the same map over the resolved
/// locks).
fn git_dependencies(root: &Path, commit: &str) -> Result<BTreeMap<String, String>> {
    let mut locks = Vec::new();
    for path in lock_paths(root, commit)? {
        let text = fs::read_to_string(root.join(&path))
            .map_err(|error| failure(format!("Committed lock {path} is unreadable: {error}")))?;
        locks.push((path, text));
    }
    lock_dependencies(
        locks
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str())),
    )
}

/// The git dependency commits of the given locks (`(lock path, lock text)`), keyed
/// `<lock path>:<package>:<url>`.
pub(crate) fn lock_dependencies<'a>(
    locks: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<BTreeMap<String, String>> {
    let mut resolved = BTreeMap::new();
    for (path, text) in locks {
        let value: toml::Value = toml::from_str(text)
            .map_err(|error| failure(format!("Committed lock {path} is not TOML: {error}")))?;
        for package in value
            .get("package")
            .and_then(|packages| packages.as_array())
            .into_iter()
            .flatten()
        {
            let (Some(name), Some(source)) = (
                package.get("name").and_then(|name| name.as_str()),
                package.get("source").and_then(|source| source.as_str()),
            ) else {
                continue;
            };
            let Some(rest) = source.strip_prefix("git+") else {
                continue;
            };
            let Some((url, rev)) = rest.split_once('#') else {
                return Err(failure(format!(
                    "Committed lock {path} pins {name} from git without a commit"
                )));
            };
            let url = url.split('?').next().unwrap_or(url);
            if !is_hex(rev, 40) {
                return Err(failure(format!(
                    "Committed lock {path} pins {name} at a non-commit revision"
                )));
            }
            resolved.insert(format!("{path}:{name}:{url}"), rev.to_owned());
        }
    }
    Ok(resolved)
}

/// Cargo's package-id specification for one git package: the lock's source without its
/// `#<commit>` fragment, keeping the `git+` scheme and the `?branch=` query.
pub(crate) fn git_spec(url: &str, name: &str, version: &str) -> String {
    format!("git+{url}#{name}@{version}")
}

/// The executable of a declared compiler/linker value: `CC` may be a whole command line
/// (`"sccache-corbet cc"`, `"cc -m32"`). The compiler is the program it runs, whose bytes are
/// part of the toolchain; a compile-cache wrapper and flags are not the compiler (flags belong
/// in `CFLAGS`, which stays a declared input).
fn program_of(value: &str) -> &str {
    value
        .split_whitespace()
        .rev()
        .find(|token| !token.starts_with('-'))
        .unwrap_or(value)
}

fn find_program(name: &str, environment: &Environment) -> Result<PathBuf> {
    let path = if name.contains('/') {
        PathBuf::from(name)
    } else {
        crate::compile_cache::executable(environment, name)
            .ok_or_else(|| failure(format!("Verdict key cannot resolve tool {name:?}")))?
    };
    // Do NOT canonicalize: a rustup proxy is an executable whose argv[0] selects the toolchain.
    // Resolving the symlink to `rustup` would run rustup itself instead of the compiler. The
    // check below follows symlinks and refuses a missing or non-file target.
    if !path.is_file() {
        return Err(failure(format!("Verdict key tool {name:?} is not a file")));
    }
    Ok(path)
}

fn run_tool(program: &Path, args: &[&str], environment: &Environment) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .env_clear()
        .envs(environment.iter())
        .output()
        .map_err(|error| {
            failure(format!(
                "Verdict key cannot run {}: {error}",
                program.display()
            ))
        })?;
    if !output.status.success() {
        return Err(failure(format!(
            "Verdict key cannot resolve {} {}: {}",
            program.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Some tools (notably older Python) print their version on stderr. Either stream is the
    // tool's answer; both empty is not an answer.
    let text = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    Ok(text.trim().replace("\r\n", "\n"))
}

/// The program name a role resolves to. The role names the tool, but a role like `nightly-rustc`
/// or `cargo-llvm-cov` still runs `rustc` or `cargo-llvm-cov`; the probe follows the program.
fn program_name(path: &Path) -> String {
    path.file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("tool")
        .to_owned()
}

/// The release-identifying lines of `cargo -vV`. Cargo also prints the host's libgit2,
/// libcurl, OpenSSL and OS detection; those are properties of the host build, not of the
/// release that orchestrates a build, and a distribution and an upstream install of the same
/// cargo disagree on them.
fn rust_verbose_identity(program: &str, text: &str) -> String {
    if program != "cargo" {
        return text.to_owned();
    }
    let keep = [
        "cargo ",
        "release:",
        "commit-hash:",
        "commit-date:",
        "host:",
    ];
    text.lines()
        .filter(|line| keep.iter().any(|prefix| line.starts_with(prefix)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tools whose semantic release is printed by `-vV` (Rust) rather than `--version`.
fn uses_rust_verbose_version(program: &str) -> bool {
    program == "rustc" || program == "cargo" || program == "rustdoc"
}

/// C-family compilers: `--version` plus `-dumpmachine`, because the target triple is part of
/// what a compiler produces and is not in the version line of every compiler.
fn is_c_family(program: &str) -> bool {
    matches!(
        program,
        "cc" | "c++" | "cxx" | "gcc" | "g++" | "clang" | "clang++" | "clang-cpp"
    )
}

/// The SEMANTIC identity of one tool: what release it is and what target it produces, never the
/// host-specific bytes of the installed file or an installation path. The same tool release
/// installed by upstream rustup, by a Nix package (with patchelf'd binaries) or by a distribution
/// therefore gets the same identity on every host.
///
/// `rustup` is deliberately not identified: it only dispatches to a toolchain. The selected
/// compiler is probed below, and a rustup update that does not change that compiler changes
/// nothing a build sees.
///
/// `declared` is the declared `cache_tools` argv behind the program: how the manifest asks this
/// tool for its release. That query runs first, verbatim. A bare program name is asked with
/// `--version`, falling back to `-v`, so tools that only answer `-v` (poppler's `pdfinfo` and
/// `pdftotext`) still get a semantic identity instead of no verdict.
fn tool_identity(
    program: &str,
    path: &Path,
    environment: &Environment,
    declared: &[String],
) -> Result<String> {
    if program == "rustup" {
        return Err(failure(
            "Verdict key does not identify rustup: it compiles nothing",
        ));
    }
    let mut attempts: Vec<Vec<String>> = Vec::new();
    if !declared.is_empty() {
        attempts.push(declared.to_vec());
    }
    if uses_rust_verbose_version(program) {
        attempts.push(vec!["-vV".to_owned()]);
    }
    attempts.push(vec!["--version".to_owned()]);
    // A `cargo-*` external subcommand is normally invoked by cargo as
    // `cargo-<name> <name> ...`; some (cargo-llvm-cov) accept `--version` only in
    // that form.
    if let Some(subcommand) = program.strip_prefix("cargo-") {
        if !subcommand.is_empty() {
            attempts.push(vec![subcommand.to_owned(), "--version".to_owned()]);
        }
    }
    attempts.push(vec!["-v".to_owned()]);
    let mut first_err: Option<Box<dyn std::error::Error + Send + Sync>> = None;
    let mut text: Option<String> = None;
    for attempt in &attempts {
        let args: Vec<&str> = attempt.iter().map(String::as_str).collect();
        match run_tool(path, &args, environment) {
            Ok(answer) => {
                text = Some(answer);
                break;
            }
            Err(error) => {
                if first_err.is_none() {
                    first_err = Some(error);
                }
            }
        }
    }
    let mut text = match text {
        Some(text) => text,
        None => {
            return Err(
                first_err.unwrap_or_else(|| failure("Verdict key found no tool to identify"))
            );
        }
    };
    if uses_rust_verbose_version(program) {
        text = rust_verbose_identity(program, &text);
    }
    if is_c_family(program) {
        // `clang --version` and some GCC builds append the installation directory or the
        // configure prefix. Those are host build facts, not the compiler's identity.
        text = text
            .lines()
            .filter(|line| {
                !line.starts_with("InstalledDir:")
                    && !line.starts_with("Configured with:")
                    && !line.starts_with("InstalledDir ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let target = run_tool(path, &["-dumpmachine"], environment)?;
        text = format!("{text}\n{target}");
    }
    Ok(text)
}

fn add_tool(
    tools: &mut BTreeMap<String, String>,
    role: &str,
    path: &Path,
    environment: &Environment,
    declared: &[String],
) -> Result<String> {
    if !is_plain(role) {
        return Err(failure(format!(
            "Verdict key tool role {role:?} is not plain"
        )));
    }
    let program = program_name(path);
    let identity = tool_identity(&program, path, environment, declared).map_err(|error| {
        failure(format!(
            "Verdict key cannot identify tool {role:?}: {error}"
        ))
    })?;
    let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
    match tools.insert(role.to_owned(), digest.clone()) {
        None => Ok(identity),
        Some(existing) if existing == digest => Ok(identity),
        Some(_) => Err(failure(format!(
            "Verdict key has two different tools for role {role:?}"
        ))),
    }
}

fn rustup_tool(toolchain: &str, program: &str, environment: &Environment) -> Result<PathBuf> {
    let rustup = find_program("rustup", environment)?;
    let reported = run_tool(
        &rustup,
        &["which", "--toolchain", toolchain, program],
        environment,
    )?;
    let path = PathBuf::from(reported).canonicalize().map_err(|error| {
        failure(format!(
            "Verdict key cannot resolve {toolchain} {program}: {error}"
        ))
    })?;
    if !path.is_file() {
        return Err(failure(format!(
            "Verdict key resolved {toolchain} {program} to a non-file"
        )));
    }
    Ok(path)
}

fn add_command_tool(
    tools: &mut BTreeMap<String, String>,
    argv: &[String],
    // The manifest's version query for this tool (`cache_tools` only): the argv behind the
    // program, probed verbatim first. A check's own `commands` pass `None`: their trailing
    // arguments run the check, they never ask for a release.
    version_query: Option<&[String]>,
    environment: &Environment,
) -> Result<()> {
    let Some(program) = argv.first() else {
        return Err(failure("Verdict key cannot identify an empty command"));
    };
    let base = Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(program);
    if base == "rustup" && argv.get(1).map(String::as_str) == Some("run") {
        let Some(toolchain) = argv.get(2) else {
            return Err(failure(
                "Verdict key cannot identify `rustup run` without a toolchain",
            ));
        };
        let Some(selected) = argv.get(3) else {
            return Err(failure(
                "Verdict key cannot identify `rustup run` without a program",
            ));
        };
        let path = rustup_tool(toolchain, selected, environment)?;
        // `rustup` itself is not part of the key: it dispatches to the selected toolchain and
        // never compiles anything. The selected compiler is identified semantically below, so a
        // rustup update that changes no compiler release changes nothing a build sees.
        // The toolchain qualifies the role: stable rustc and nightly rustc are different
        // compilers and both can appear in one check's tools.
        add_tool(
            tools,
            &format!("{toolchain}-{selected}"),
            &path,
            environment,
            &[],
        )?;
        // `cargo llvm-cov` and friends are external subcommands; their version matters too.
        if selected == "cargo" {
            if let Some(subcommand) = argv.get(4).filter(|arg| !arg.starts_with('-')) {
                if let Some(path) =
                    crate::compile_cache::executable(environment, &format!("cargo-{subcommand}"))
                {
                    add_tool(
                        tools,
                        &format!("{toolchain}-cargo-{subcommand}"),
                        &path,
                        environment,
                        &[],
                    )?;
                }
            }
        }
        return Ok(());
    }
    if base == "rustup" {
        // A declared probe of rustup is not a build input; see `tool_identity`.
        return Ok(());
    }
    let path = find_program(program, environment)?;
    add_tool(
        tools,
        base,
        &path,
        environment,
        version_query.unwrap_or(&[]),
    )?;
    if base == "cargo" {
        if let Some(subcommand) = argv.get(1).filter(|arg| !arg.starts_with('-')) {
            if let Some(path) =
                crate::compile_cache::executable(environment, &format!("cargo-{subcommand}"))
            {
                add_tool(
                    tools,
                    &format!("cargo-{subcommand}"),
                    &path,
                    environment,
                    &[],
                )?;
            }
        }
    }
    Ok(())
}

/// The C/C++ compiler a build gets from `CC`/`CXX`, or the platform default when unset. A
/// compile-cache wrapper in front of it (`sccache-corbet cc`) changes no result, so only the
/// compiler's bytes are keyed. Commands that do not find one simply do not bind that role.
fn add_compiler_tools(
    tools: &mut BTreeMap<String, String>,
    environment: &Environment,
) -> Result<()> {
    for (role, variable, default) in [("cc", "CC", "cc"), ("cxx", "CXX", "c++")] {
        let program = value(environment, variable);
        let program = program.as_deref().map(program_of).unwrap_or(default);
        if let Ok(path) = find_program(program, environment) {
            add_tool(tools, role, &path, environment, &[])?;
        }
    }
    Ok(())
}

/// Tools whose bytes can change one check's result, by role. A check that cannot identify its
/// tools gets no verdict (the ordinary cached run still happens).
fn toolchain(check: &crate::Check, environment: &Environment) -> Result<BTreeMap<String, String>> {
    let mut tools = BTreeMap::new();
    match check.kind.as_str() {
        "cargo" => {
            let (rustc, cargo) = match check.toolchain.as_deref().unwrap_or("system") {
                "system" => (
                    find_program("rustc", environment)?,
                    find_program("cargo", environment)?,
                ),
                toolchain => (
                    rustup_tool(toolchain, "rustc", environment)?,
                    rustup_tool(toolchain, "cargo", environment)?,
                ),
            };
            add_tool(&mut tools, "rustc", &rustc, environment, &[])?;
            add_tool(&mut tools, "cargo", &cargo, environment, &[])?;
            if check.test_runner.as_deref() == Some("nextest") {
                add_tool(
                    &mut tools,
                    "cargo-nextest",
                    &find_program("cargo-nextest", environment)?,
                    environment,
                    &[],
                )?;
            }
            for (role, variable) in [("cc", "CC"), ("cxx", "CXX")] {
                if let Some(program) = value(environment, variable) {
                    add_tool(
                        &mut tools,
                        role,
                        &find_program(&program, environment)?,
                        environment,
                        &[],
                    )?;
                }
            }
            if let Some(linker) = value(environment, "CI_LINKER") {
                add_tool(
                    &mut tools,
                    "linker",
                    &find_program(&linker, environment)?,
                    environment,
                    &[],
                )?;
            }
            // A compiler cache wrapper changes no result, only how it was produced: it is
            // cache plumbing, deliberately outside the verdict key.
        }
        "commands" => {
            for argv in &check.commands {
                add_command_tool(&mut tools, argv, None, environment)?;
            }
            for argv in &check.cache_tools {
                // The manifest declares how to ask this tool for its release; that query is
                // probed verbatim. An empty argv is refused below, never sliced here.
                let query: &[String] = if argv.is_empty() { &[] } else { &argv[1..] };
                add_command_tool(&mut tools, argv, Some(query), environment)?;
            }
            // A check script resolves dependencies with Git; its release decides the resolution.
            if let Some(path) = crate::compile_cache::executable(environment, "git") {
                add_tool(&mut tools, "git", &path, environment, &[])?;
            }
            if check.cache_tools.is_empty() {
                // A custom command that declares no tools is keyed by the common toolchains it
                // can find, exactly as the ordinary cached key does.
                for name in ["rustc", "cargo", "node", "python3", "nix"] {
                    if let Some(path) = crate::compile_cache::executable(environment, name) {
                        add_tool(&mut tools, name, &path, environment, &[])?;
                    }
                }
            }
            add_compiler_tools(&mut tools, environment)?;
            if let Some(linker) = value(environment, "CI_LINKER") {
                add_tool(
                    &mut tools,
                    "linker",
                    &find_program(program_of(&linker), environment)?,
                    environment,
                    &[],
                )?;
            }
        }
        "javascript" => {
            let manager = check.manager.as_deref().unwrap_or("npm");
            if manager != "deno" {
                add_tool(
                    &mut tools,
                    "node",
                    &find_program("node", environment)?,
                    environment,
                    &[],
                )?;
            }
            add_tool(
                &mut tools,
                manager,
                &find_program(manager, environment)?,
                environment,
                &[],
            )?;
        }
        "nix" => {
            add_tool(
                &mut tools,
                "nix",
                &find_program("nix", environment)?,
                environment,
                &[],
            )?;
        }
        other => {
            return Err(failure(format!(
                "Verdict key does not know the toolchain of check kind {other:?}"
            )))
        }
    }
    if tools.is_empty() {
        return Err(failure("Verdict key found no tool to identify"));
    }
    Ok(tools)
}

/// Path-list variables whose entries may be locators rather than semantic inputs.
const PATH_LISTS: &[&str] = &[
    "LD_LIBRARY_PATH",
    "LIBRARY_PATH",
    "PKG_CONFIG_PATH",
    "PYTHONPATH",
    "NODE_PATH",
    "CPATH",
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "CMAKE_PREFIX_PATH",
];

/// A Cargo build-output directory: a `target` component followed by a profile (or `deps`).
fn is_build_output(path: &Path) -> bool {
    let components: Vec<&str> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    components.windows(2).any(|pair| {
        pair[0] == "target"
            && matches!(
                pair[1],
                "debug" | "release" | "test" | "bench" | "deps" | "doc"
            )
    })
}

/// Normalise a declared path list to the entries that describe content, not a producer's
/// process state:
///
/// * entries inside the checkout are build state (the tree digest already names the source),
/// * entries inside the producer's Cargo target directory, its manifest's `target` directory or
///   any `target/<profile>` are build outputs,
/// * entries under the selected Rust toolchain's `lib` trees are locators for libraries whose
///   release the semantic toolchain identity already names.
///
/// Duplicate and empty entries are removed. The result is order-preserving, so a genuinely
/// different library search path still changes the key.
fn normalise_path_list(list: &str, root: &Path, environment: &Environment) -> String {
    let rustup_libs =
        value_os(environment, "RUSTUP_HOME").map(|home| Path::new(&home).join("toolchains"));
    let cargo_target = value(environment, "CARGO_TARGET_DIR").map(PathBuf::from);
    let manifest_target = value(environment, "CARGO_MANIFEST_DIR")
        .map(|manifest| Path::new(&manifest).join("target"));
    let mut seen = BTreeSet::new();
    list.split(':')
        .filter(|entry| !entry.is_empty())
        .filter(|entry| {
            let path = Path::new(entry);
            !path.starts_with(root)
                && !rustup_libs
                    .as_ref()
                    .is_some_and(|libraries| path.starts_with(libraries))
                && !cargo_target
                    .as_ref()
                    .is_some_and(|target| path.starts_with(target))
                && !manifest_target
                    .as_ref()
                    .is_some_and(|target| path.starts_with(target))
                && !is_build_output(path)
                && seen.insert((*entry).to_owned())
        })
        .collect::<Vec<_>>()
        .join(":")
}

fn value_os(environment: &Environment, name: &str) -> Option<std::ffi::OsString> {
    environment.get(OsStr::new(name)).cloned()
}

/// The check declaration plus the exact values of the environment variables it declares
/// (`SEMANTIC_ENV` plus `cache_env`). An empty declared variable and an absent one are different
/// inputs, as they are for the ordinary result cache. Ambient environment never enters the key.
///
/// The payload is built once so `CCID_VERDICT_DEBUG=1` can print exactly what the identity hashes;
/// without that, two producers that disagree have no way to name the disagreeing input.
fn check_identity_payload(
    check: &crate::Check,
    environment: &Environment,
    root: &Path,
) -> Result<serde_json::Value> {
    // Process plumbing that the fleet's workstation and its runner disagree on without any
    // effect on what a check computes: the compiler cache wrapper and the presentation locale.
    // A check that really depends on one declares it in `cache_env`, which is never filtered.
    // `CC`/`CXX` select a compiler; the compiler's identity is keyed through the toolchain. A
    // plain selector (`cc`, `clang`, `sccache-corbet cc`) is therefore not a separate input,
    // which lets a runner with a cache wrapper and a land without it agree. A selector that
    // carries flags is different: those flags are keyed verbatim.
    const SELECTOR: &[&str] = &["CC", "CXX"];
    const PRESENTATION: &[&str] = &["RUSTC_WRAPPER", "LANG", "LC_ALL", "LANGUAGE", "TZ"];
    // Systemd per-unit directories (`StateDirectory=`/`RuntimeDirectory=`): the unit layout
    // names a producer's process state, never a check input. Excluded even when declared,
    // like INVOCATION_ID is absent: the unit layout can never split the ledger.
    const NEVER_SEMANTIC: &[&str] = &["STATE_DIRECTORY", "RUNTIME_DIRECTORY"];
    let mut declared: BTreeMap<String, Option<String>> = BTreeMap::new();
    for name in cmnp::inputs::SEMANTIC_ENV
        .iter()
        .copied()
        .filter(|name| !PRESENTATION.contains(name))
        .chain(check.cache_env.iter().map(String::as_str))
        .filter(|name| !NEVER_SEMANTIC.contains(name))
    {
        let value = environment
            .get(OsStr::new(name))
            .map(|v| v.to_string_lossy().into_owned());
        if SELECTOR.contains(&name)
            && value
                .as_deref()
                .is_none_or(|v| !v.split_whitespace().any(|token| token.starts_with('-')))
        {
            continue;
        }
        let value = match value {
            Some(value) if PATH_LISTS.contains(&name) => {
                Some(normalise_path_list(&value, root, environment))
            }
            other => other,
        };
        declared.insert(name.to_owned(), value);
    }
    Ok(json!({"declaration": check, "env": declared}))
}

fn store_root(environment: &Environment) -> Option<PathBuf> {
    value(environment, "CCID_RESULT_CACHE").map(PathBuf::from)
}

/// Canonical repository provenance: the repository path (`owner/name`), never the transport.
///
/// The same repository is reachable as `git.example`, `forge.example`,
/// `forge.example.internal` or an in-cluster service name, and the content tree digest already
/// identifies the content. A scheme, user, host, port or `.git` suffix is therefore a transport
/// alias, not part of the identity: strip them all. This is provenance only and never a key input.
fn canonical_repository(raw: &str) -> String {
    let path = if let Ok(url) = url::Url::parse(raw) {
        url.path().trim_start_matches('/').to_owned()
    } else if let Some((_, path)) = raw.rsplit_once(':') {
        // scp-style `git@host:owner/name.git`; a plain `owner/name` has no colon.
        path.trim_start_matches('/').to_owned()
    } else {
        raw.trim_start_matches('/').to_owned()
    };
    let path = path.trim_end_matches('/');
    path.strip_suffix(".git").unwrap_or(path).to_owned()
}

/// The repository identity as provenance: the canonical path of `CI_REPOSITORY_URL`, else the
/// checkout's origin, else `unknown`.
fn repository_identity(root: &Path, environment: &Environment) -> String {
    value(environment, "CI_REPOSITORY_URL")
        .or_else(|| {
            if is_repository(root) {
                git(root, &["config", "--get", "remote.origin.url"]).ok()
            } else {
                None
            }
        })
        .map(|raw| canonical_repository(&raw))
        .filter(|canonical| !canonical.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// The verdict layer only speaks for content a producer actually committed or exported. A
/// checkout with unrelated local modifications has no exact tree to name, so it gets no verdict
/// (the ordinary cache still works). A locally rewritten `Cargo.lock` is the one tracked
/// modification that is keyed (by its content, through the tree digest): ccid itself never
/// writes it, it resolves in a copy (`float`), but a producer may have.
fn applicable(root: &Path, outputs: &[String]) -> Result<()> {
    if !is_repository(root) {
        return Ok(());
    }
    let modified = git_raw(root, &["status", "--porcelain", "--untracked-files=no"])?;
    for line in modified.lines() {
        let path = line.get(3..).unwrap_or("").trim();
        if path.rsplit('/').next() != Some("Cargo.lock") {
            return Err(failure(format!(
                "Verdict key refuses a working tree modified outside Cargo.lock: {path}"
            )));
        }
    }
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard"])?;
    for path in untracked.lines() {
        // A declared output left by an earlier run in this checkout is evidence the check
        // produced, not input it reads: a second selection or a plan must still key the tree.
        if outputs
            .iter()
            .any(|output| path == output || path.starts_with(&format!("{output}/")))
        {
            continue;
        }
        if path == "moon.yml"
            || GENERATED
                .iter()
                .any(|generated| path == *generated || path.starts_with(&format!("{generated}/")))
        {
            continue;
        }
        return Err(failure(format!(
            "Verdict key refuses an untracked file the check could read: {path}"
        )));
    }
    Ok(())
}

/// The check's declared outputs, validated before they can name a restore
/// destination: a plain relative path that is not tracked source and not a
/// generated/operational directory. A declared output that would overwrite
/// source input never gets a verdict (the ordinary cached run still works).
fn check_outputs(root: &Path, check: &crate::Check) -> Result<Vec<String>> {
    if check.cache_outputs.len() > crate::cached_outputs::MAX_OUTPUTS {
        return Err(failure("The check declares too many cache outputs"));
    }
    let mut outputs = Vec::new();
    for output in &check.cache_outputs {
        let relative = crate::cached_outputs::output_relative(output)?;
        if is_repository(root) {
            let tracked = git(root, &["ls-files", "--cached", "-z", "--", output.as_str()])?;
            if !tracked.is_empty() {
                return Err(failure(format!(
                    "A declared cache output must not overwrite tracked source: {output:?}"
                )));
            }
        }
        outputs.push(
            relative
                .to_str()
                .ok_or_else(|| failure("Declared cache output must be valid UTF-8"))?
                .to_owned(),
        );
    }
    Ok(outputs)
}

#[cfg(test)]
pub(crate) fn prepare(
    root: &Path,
    check_name: &str,
    check: &crate::Check,
    environment: &Environment,
    commit_id: &str,
    ignore_untracked: &[String],
) -> Result<Prepared> {
    prepare_resolved(
        root,
        check_name,
        check,
        environment,
        commit_id,
        ignore_untracked,
        None,
    )
}

/// [`prepare`] for a floated run: `resolved` is the resolution's commit map
/// (`float::Resolution::commits`), keyed as `git_dependencies` in place of the checkout's own.
/// The check must declare the resolution digest (`float::bind`) and the environment must carry
/// it: the digest names the resolved locks, which the tree digest of the checkout does not.
pub(crate) fn prepare_resolved(
    root: &Path,
    check_name: &str,
    check: &crate::Check,
    environment: &Environment,
    commit_id: &str,
    ignore_untracked: &[String],
    resolved: Option<&BTreeMap<String, String>>,
) -> Result<Prepared> {
    if resolved.is_some()
        && (value(environment, crate::float::RESOLUTION_ENV).is_none()
            || !check
                .cache_env
                .iter()
                .any(|name| name == crate::float::RESOLUTION_ENV))
    {
        return Err(failure(format!(
            "A floated check is never keyed without {} in its declared environment",
            crate::float::RESOLUTION_ENV
        )));
    }
    // The digest is what makes a `fleet = true` verdict specific to its fleet data: never key
    // such a check without it (an absent value would name every fleet at once).
    if check.fleet && value(environment, crate::fleet::FLEET_DIGEST_ENV).is_none() {
        return Err(failure(format!(
            "A fleet = true check is never keyed without {}",
            crate::fleet::FLEET_DIGEST_ENV
        )));
    }
    let outputs = check_outputs(root, check)?;
    // The check's own declared outputs are always evidence, whether or not a wider selection
    // named them too.
    let mut ignore = ignore_untracked.to_vec();
    ignore.extend(outputs.iter().cloned());
    applicable(root, &ignore)?;
    let entries = tree_entries(root, commit_id)?;
    let identity = check_identity_payload(check, environment, root)?;
    let check_identity = format!("{:x}", Sha256::digest(serde_json::to_vec(&identity)?));
    let checkout_dependencies = git_dependencies(root, commit_id)?;
    let components = KeyComponents {
        schema: SCHEMA,
        tree: entries_digest(root, &entries)?,
        git_dependencies: resolved
            .cloned()
            .unwrap_or_else(|| checkout_dependencies.clone()),
        toolchain: toolchain(check, environment)?,
        profile: format!("{:x}", Sha256::digest(DEV_PROFILE.as_bytes())),
        epoch: crate::cached::VERDICT_EPOCH,
        check: check_name.to_owned(),
        check_identity,
    };
    let key = components.key_hex()?;
    if value(environment, "CCID_VERDICT_DEBUG").as_deref() == Some("1") {
        // Opt-in: print the exact identity input whose hash is in the key. Values are the
        // check's declared environment; they never leave the producer unless this asks.
        event(json!({
            "event": "verdict-debug",
            "check": check_name,
            "key": key,
            "identity": identity,
            "check_identity": components.check_identity,
            "toolchain": components.toolchain,
        }));
    }
    let cache = store_root(environment);
    let store = cache.as_ref().map(|root| root.join(STORE_DIR));
    Ok(Prepared {
        check: check_name.to_owned(),
        key,
        components,
        commit: commit_id.to_owned(),
        repository: repository_identity(root, environment),
        branch: crate::cached_outputs::branch(root, environment),
        root: root.to_path_buf(),
        tree_entries: entries,
        checkout_dependencies,
        cache,
        store,
        outputs,
    })
}

/// The output-store subject of one prepared check.
fn output_subject<'a>(
    prepared: &'a Prepared,
    cache: &'a Path,
) -> crate::cached_outputs::Subject<'a> {
    crate::cached_outputs::Subject {
        cache,
        root: &prepared.root,
        key: &prepared.key,
        check: &prepared.check,
        repository: &prepared.repository,
        branch: &prepared.branch,
        commit: &prepared.commit,
        outputs: &prepared.outputs,
    }
}

/// Restore a hit's declared outputs. `Ok(())` means every declared output is
/// byte-identical to the recorded content; any error makes the caller treat
/// the lookup as a miss.
pub fn restore_outputs(prepared: &Prepared, _environment: &Environment) -> Result<()> {
    let Some(cache) = &prepared.cache else {
        return Err(failure("CCID_RESULT_CACHE is not provisioned"));
    };
    crate::cached_outputs::restore(&output_subject(prepared, cache))
}

/// Whether a hit's declared outputs are present without reading them whole.
pub fn outputs_ready(prepared: &Prepared, _environment: &Environment) -> Result<()> {
    let Some(cache) = &prepared.cache else {
        return Err(failure("CCID_RESULT_CACHE is not provisioned"));
    };
    crate::cached_outputs::ready(&output_subject(prepared, cache))
}

/// Prepare every selected check. A check whose key cannot be computed is simply absent: the
/// ordinary cached run still executes it, and no verdict is written for it. `resolved` is the
/// commit map of this run's float, if it floated anything.
pub fn prepare_selection(
    root: &Path,
    checks: &BTreeMap<String, crate::Check>,
    selected: &[String],
    environment: &Environment,
    commit_id: &str,
    resolved: Option<&BTreeMap<String, String>>,
) -> Vec<Prepared> {
    // A selection may contain more than one output-producing check; every declared output of
    // the selection is evidence a previous run may have left in this checkout, never input.
    let ignored: Vec<String> = selected
        .iter()
        .flat_map(|name| checks[name].cache_outputs.iter())
        .filter_map(|output| crate::cached_outputs::output_relative(output).ok())
        .filter_map(|output| output.to_str().map(str::to_owned))
        .collect();
    let mut prepared = Vec::new();
    for name in selected {
        match prepare_resolved(
            root,
            name,
            &checks[name],
            environment,
            commit_id,
            &ignored,
            resolved,
        ) {
            Ok(entry) => prepared.push(entry),
            Err(error) => event(json!({
                "event": "verdict-unavailable",
                "check": name,
                "reason": error.to_string(),
            })),
        }
    }
    prepared
}

fn ensure_store(directory: &Path) -> Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(failure("Verdict store path is not a directory")),
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

fn record_path(directory: &Path, key: &str) -> PathBuf {
    directory.join(format!("{key}.json"))
}

fn read_record(directory: &Path, key: &str) -> Result<Option<Record>> {
    let path = record_path(directory, key);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(failure("Verdict record must not be a symlink"))
        }
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(failure("Verdict record must be a regular file"))
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut bytes = Vec::new();
    File::open(&path)?
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(failure("Verdict record exceeds its size bound"));
    }
    let record: Record = serde_json::from_slice(&bytes)
        .map_err(|_| failure("Verdict record is not a valid record"))?;
    record.validate(key)?;
    Ok(Some(record))
}

fn lock_key(directory: &Path, key: &str) -> Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(format!("{key}.lock")))?;
    // Both producers must be able to take the same key's lock, whoever created it first.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if fs::metadata(directory).is_ok_and(|meta| meta.mode() & 0o020 != 0) {
            let _ = lock.set_permissions(fs::Permissions::from_mode(0o660));
        }
    }
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(TryLockError::WouldBlock) => return Err(failure("Verdict record lock timed out")),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

/// Publish the record while the caller holds the key's lock.
fn write_record_locked(directory: &Path, record: &Record) -> Result<()> {
    let key = &record.key;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if fs::metadata(directory).is_ok_and(|meta| meta.mode() & 0o020 != 0) {
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o660))?;
        }
    }
    serde_json::to_writer(&mut file, record)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(record_path(directory, key))
        .map_err(|error| failure(format!("Cannot publish verdict record: {error}")))?;
    #[cfg(unix)]
    {
        let _ = File::open(directory).and_then(|dir| dir.sync_all());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_record(directory: &Path, record: &Record) -> Result<()> {
    ensure_store(directory)?;
    let key = &record.key;
    let _lock = lock_key(directory, key)?;
    // First success wins: two producers that agree on the key agree on the result, and a later
    // failure must never erase a pass.
    if let Ok(Some(existing)) = read_record(directory, key) {
        if existing.result == ResultWord::Success {
            return Ok(());
        }
    }
    write_record_locked(directory, record)
}

/// Publish a pass and its declared outputs as one transaction, under the key's lock: every
/// output blob and the output index are durable first, and only then the verdict record. A
/// failure before that point leaves no verdict, so no later hit can report success without its
/// artefacts. An existing pass at the same key always wins on the result, and a later failure
/// never erases it. Its provenance may be refreshed by a success from a different producing
/// binary: CI accepts only its channel binary's records, so after a channel bump the first run
/// that recomputes must be able to leave a record the new binary accepts (otherwise every later
/// run would miss forever). Finally, the output set is (re)written, which repairs a set left
/// incomplete by an older release or a damaged store.
fn publish_record(
    directory: &Path,
    record: &Record,
    prepared: &Prepared,
    producer: &Producer,
) -> Result<()> {
    ensure_store(directory)?;
    let key = &record.key;
    let _lock = lock_key(directory, key)?;
    let replace = match read_record(directory, key) {
        // The same producing binary already published this exact pass.
        Ok(Some(existing))
            if existing.result == ResultWord::Success && existing.tool == record.tool =>
        {
            false
        }
        // No pass, a failure record, or a pass from another producing binary.
        _ => true,
    };
    if !prepared.outputs.is_empty() {
        let cache = prepared
            .cache
            .as_deref()
            .ok_or_else(|| failure("CCID_RESULT_CACHE is not provisioned"))?;
        crate::cached_outputs::store(
            &output_subject(prepared, cache),
            producer.kind.as_str(),
            record.finished_at,
        )?;
    }
    if replace {
        write_record_locked(directory, record)?;
    }
    if let Some(cache) = prepared.cache.as_deref() {
        crate::cached_outputs::maybe_prune(cache);
    }
    Ok(())
}

/// The producing binary's own identity: SHA-256 of its bytes and the revision compiled into it.
pub(crate) fn own_tool() -> Result<Tool> {
    static TOOL: OnceLock<std::result::Result<Tool, String>> = OnceLock::new();
    TOOL.get_or_init(|| {
        let exe = std::env::current_exe()
            .map_err(|error| format!("cannot locate the running ccid: {error}"))?;
        let exe = exe
            .canonicalize()
            .map_err(|error| format!("cannot resolve the running ccid: {error}"))?;
        let sha256 = crate::sha256_file(&exe)
            .map_err(|error| format!("cannot hash the running ccid: {error}"))?;
        Ok(Tool {
            sha256,
            source_revision: SOURCE_REVISION.to_owned(),
        })
    })
    .clone()
    .map_err(failure)
}

/// One release channel entry: a full commit and the digest of the binary built from it.
#[derive(Clone, Debug, Deserialize)]
struct ChannelEntry {
    revision: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct Channel {
    default: ChannelEntry,
    #[serde(default)]
    classes: BTreeMap<String, ChannelEntry>,
}

/// The channel file is a runner fact (`CCID_CHANNEL_FILE`), never a repository fact. Without it
/// CI accepts only the binary it is running, which the workflow verified against the channel
/// before it started this process.
fn channel_accepts(tool: &Tool, environment: &Environment) -> std::result::Result<(), String> {
    if let Some(path) = value(environment, "CCID_CHANNEL_FILE") {
        let path = Path::new(&path);
        let meta =
            fs::symlink_metadata(path).map_err(|_| "channel file is unreadable".to_owned())?;
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            return Err("channel file must be a regular file".to_owned());
        }
        let bytes = fs::read(path).map_err(|_| "channel file is unreadable".to_owned())?;
        let channel: Channel = serde_json::from_slice(&bytes)
            .map_err(|_| "channel file is not valid JSON".to_owned())?;
        let mut entries = vec![&channel.default];
        entries.extend(channel.classes.values());
        for entry in entries {
            if !is_hex(&entry.sha256, 64) || !is_hex(&entry.revision, 40) {
                continue;
            }
            if entry.sha256 != tool.sha256 {
                continue;
            }
            // A binary built outside CI carries "unversioned"; its digest is still the identity.
            if is_hex(&tool.source_revision, 40) && entry.revision != tool.source_revision {
                continue;
            }
            return Ok(());
        }
        return Err("the producing ccid binary is not on the release channel".to_owned());
    }
    if value(environment, "CI").is_some() {
        // The workflow verified this very binary against the channel before exec.
        let own = own_tool().map_err(|error| error.to_string())?;
        if own.sha256 != tool.sha256 || own.source_revision != tool.source_revision {
            return Err("the producing ccid binary is not this channel binary".to_owned());
        }
        return Ok(());
    }
    // A lane producer uses the channel release it fetched; full key equality has been checked.
    Ok(())
}

/// Read the record and decide, fail closed, whether it answers for `prepared`.
pub fn lookup(prepared: &Prepared, environment: &Environment) -> Lookup {
    let Some(directory) = &prepared.store else {
        return Lookup {
            status: Status::Disabled,
            reason: Some("CCID_RESULT_CACHE is not provisioned".into()),
        };
    };
    let record = match read_record(directory, &prepared.key) {
        Ok(Some(record)) => record,
        Ok(None) => {
            return Lookup {
                status: Status::Miss,
                reason: Some("no verdict for this key".into()),
            }
        }
        Err(error) => {
            return Lookup {
                status: Status::Invalid,
                reason: Some(error.to_string()),
            }
        }
    };
    if record.key != prepared.key || record.components != prepared.components {
        return Lookup {
            status: Status::Invalid,
            reason: Some("verdict record does not match every key component".into()),
        };
    }
    if record.result != ResultWord::Success {
        return Lookup {
            status: Status::Miss,
            reason: Some("the recorded result is not a pass".into()),
        };
    }
    if let Err(reason) = channel_accepts(&record.tool, environment) {
        return Lookup {
            status: Status::Invalid,
            reason: Some(reason),
        };
    }
    Lookup {
        status: Status::Hit,
        reason: None,
    }
}

/// The lookup a plan reports: a pass whose declared outputs are not present is a miss, because
/// a hit would leave the archive/upload steps without the report the check promised.
pub fn lookup_ready(prepared: &Prepared, environment: &Environment) -> Lookup {
    let lookup = lookup(prepared, environment);
    if lookup.status != Status::Hit {
        return lookup;
    }
    match outputs_ready(prepared, environment) {
        Ok(()) => lookup,
        Err(error) => Lookup {
            status: Status::Miss,
            reason: Some(format!(
                "the verdict's stored outputs are unavailable: {error}"
            )),
        },
    }
}

/// Record a successful run of every prepared check that actually went through the result cache.
/// `bypassed` names checks that ran uncached for a semantic reason; their result is not reused.
/// The record is written only when the checkout still holds the inputs the key was built from:
/// a check that wrote the checkout (its tree or its locks) never leaves a pass under the old key.
/// A floated check ran in a staged copy; its run reports locks it moved there
/// (`float-locks-moved` in its receipt), which refuses the record too.
pub fn record_success(
    prepared: &[Prepared],
    bypassed: &BTreeSet<String>,
    producer: &Producer,
    started_at: u64,
    environment: &Environment,
) -> Result<()> {
    let tool = match own_tool() {
        Ok(tool) => tool,
        Err(error) => {
            event(json!({"event":"verdict-store-error","reason":error.to_string()}));
            return Ok(());
        }
    };
    let finished_at = now();
    for entry in prepared {
        if bypassed.contains(&entry.check) {
            event(json!({
                "event": "verdict-skipped",
                "check": entry.check,
                "reason": "the check ran uncached",
            }));
            continue;
        }
        let Some(directory) = &entry.store else {
            continue;
        };
        let current = (|| -> Result<(String, BTreeMap<String, String>)> {
            Ok((
                entries_digest(&entry.root, &entry.tree_entries)?,
                git_dependencies(&entry.root, &entry.commit)?,
            ))
        })();
        match current {
            Ok((tree, dependencies))
                if tree == entry.components.tree && dependencies == entry.checkout_dependencies => {
            }
            Ok(_) => {
                event(json!({
                    "event": "verdict-skipped",
                    "check": entry.check,
                    "reason": "the check's inputs moved while it ran",
                }));
                continue;
            }
            Err(error) => {
                event(
                    json!({"event":"verdict-store-error","check":entry.check,"reason":error.to_string()}),
                );
                continue;
            }
        }
        if crate::float::receipt_reports_moved_locks(&entry.root, &entry.check) {
            event(json!({
                "event": "verdict-skipped",
                "check": entry.check,
                "reason": "the check moved its staged locks while it ran",
            }));
            continue;
        }
        let record = Record {
            schema: SCHEMA,
            key: entry.key.clone(),
            components: entry.components.clone(),
            commit: entry.commit.clone(),
            repository: entry.repository.clone(),
            result: ResultWord::Success,
            producer: producer.clone(),
            tool: tool.clone(),
            started_at,
            finished_at,
            duration_seconds: finished_at.saturating_sub(started_at),
        };
        match publish_record(directory, &record, entry, producer) {
            Ok(()) => event(json!({
                "event": "verdict-record",
                "check": entry.check,
                "key": entry.key,
                "producer": producer.kind.as_str(),
                "tool": tool.sha256,
                "result": "success",
                "outputs": entry.outputs.len(),
            })),
            // The check itself passed; a store failure must not turn it red. CI will recompute,
            // and a declared output set that cannot be stored deliberately leaves no verdict.
            Err(error) => event(json!({
                "event": "verdict-store-error",
                "check": entry.check,
                "reason": error.to_string(),
            })),
        }
    }
    let _ = environment;
    Ok(())
}

/// The producer of a run: explicit `--producer`, else CI when the scheduler sets `CI`, else lane.
pub fn producer(explicit: Option<&str>, environment: &Environment) -> Result<Producer> {
    let kind = match explicit {
        Some("lane") => ProducerKind::Lane,
        Some("ci") => ProducerKind::Ci,
        Some(other) => {
            return Err(failure(format!(
                "Unknown verdict producer {other:?}; use lane or ci"
            )))
        }
        None => {
            if value(environment, "CI").is_some() {
                ProducerKind::Ci
            } else {
                ProducerKind::Lane
            }
        }
    };
    let identity = value(environment, "CCID_PRODUCER_ID")
        .or_else(|| value(environment, "CI_PIPELINE_URL"))
        .or_else(|| value(environment, "CI_JOB_ID"))
        .or_else(|| {
            #[cfg(unix)]
            {
                std::env::var("HOSTNAME").ok()
            }
            #[cfg(not(unix))]
            {
                None
            }
        });
    Ok(Producer { kind, identity })
}

/// The checks that ran outside the result cache in the last cached run, from its metrics file.
pub fn bypassed(root: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Ok(bytes) = fs::read(root.join(".ccid/cache-metrics.json")) else {
        return names;
    };
    let Ok(metrics) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return names;
    };
    for item in metrics["bypassed"].as_array().into_iter().flatten() {
        if let Some(name) = item["check"].as_str() {
            names.insert(name.to_owned());
        }
    }
    names
}

/// A one-line plan event for `ccid cached --plan`.
pub fn plan_event(prepared: &Prepared, environment: &Environment) {
    let lookup = lookup_ready(prepared, environment);
    event(json!({
        "event": "verdict-plan",
        "check": prepared.check,
        "key": prepared.key,
        "status": lookup.status.as_str(),
        "reason": lookup.reason,
    }));
}

/// The bypass reasons a plan reports without running anything.
pub fn plan_bypass(
    check: &crate::Check,
    cmnp_check: &cmnp::executor::Check,
    root: &Path,
) -> Option<String> {
    if !checks::cache_eligible(check) {
        return Some(if check.cache == Some(false) {
            "declares cache = false".into()
        } else if check.cache_pure == Some(false) {
            "declares cache_pure = false".into()
        } else if check.cache_commit {
            "declares cache_commit = true".into()
        } else {
            format!("check kind {:?} is not cacheable", check.kind)
        });
    }
    cmnp::volatile_reason(cmnp_check, root).map(|reason| format!("observes live state: {reason}"))
}

#[cfg(test)]
mod tests;

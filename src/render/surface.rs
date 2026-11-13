//! The CI surface (`render_schema = 3`): ccid owns every CI file of a repository.
//!
//! Every file under the surface roots (`.ci/`, `.circleci/`, `.crow/`, `.forgejo/`, `.github/`, `.gitlab/`,
//! `.gitlab-ci.yml`, `bitbucket-pipelines.yml`) has exactly one class:
//!
//! | class | meaning | `render` | `render --check` |
//! |---|---|---|---|
//! | rendered | written by ccid (the render marker, or `jobs.json`'s generator) | writes; removes it when no longer rendered | drift is red |
//! | data | `.ci/ccid.toml` and the files ccid itself reads | reads | must parse |
//! | script | a repository script the manifest names (a check or job argv, `cache_inputs`), one such a script uses, or one `[surface] keep` lists with a reason | leaves | - |
//! | community | provider metadata that is not CI (issue templates, CODEOWNERS, ...) | leaves | - |
//! | upstream | a fork's own CI files, listed in `[surface] upstream` | never touches | - |
//! | legacy | a known retired family | deletes it once its replacement is live and nothing uses it; refuses it before | red |
//! | unknown | anything else | refuses (nothing is written) | red |
//!
//! The table below is generic: it names path families, never repositories. What only a repository knows is
//! data in its `[surface]` table. Render deletes only legacy files that git can restore (tracked and unmodified),
//! never a symlink, never a file another file still uses, and never an unknown file.
//!
//! Schemas 1 and 2 never reach this module: they render byte for byte as before.

use super::{metadata, owned};
use crate::{failure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command as Process,
};

/// Directories whose every file belongs to the CI surface.
pub const ROOTS: [&str; 6] = [
    ".ci",
    ".circleci",
    ".crow",
    ".forgejo",
    ".github",
    ".gitlab",
];
/// Files at the repository root that belong to it.
pub const ROOT_FILES: [&str; 2] = [".gitlab-ci.yml", "bitbucket-pipelines.yml"];
/// The runner labels a job may ask for at `render_schema = 3`.
pub const LABELS: [&str; 3] = ["linux", "windows", "macos"];
/// The label of a job that names none.
pub const DEFAULT_LABEL: &str = "linux";
/// Text files larger than this are not searched for references (a CI script is far smaller).
const READ_LIMIT: u64 = 1 << 20;

/// `[surface]`: what the renderer cannot know about a repository.
#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Repository scripts under the surface roots that no declared check names, with the reason: path = "reason".
    pub keep: BTreeMap<String, String>,
    /// A fork's own CI files (paths or globs under the surface roots). ccid never touches them.
    pub upstream: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Rendered,
    Data,
    Script,
    Community,
    Upstream,
    Legacy,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Render writes the file (new, changed or unchanged).
    Write,
    /// Render leaves the file as it is.
    Keep,
    /// Render deletes the file.
    Delete,
    /// Render refuses the repository while the file is there (nothing is written).
    Refuse,
}

/// One file of the surface and what a `render_schema = 3` render does with it.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub path: PathBuf,
    pub class: Class,
    pub action: Action,
    /// The table family the file belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<&'static str>,
    /// Legacy only: what replaces the family.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement: Option<&'static str>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// One row of the surface table: a path family, its class and, for legacy families, the replacement.
struct Family {
    id: &'static str,
    class: Class,
    patterns: &'static [&'static str],
    /// The file belongs to the family only when it contains this text.
    contains: Option<&'static str>,
    /// Legacy only: what replaces the family.
    replacement: &'static str,
    /// Legacy only: what must be live before render deletes the family. `None`: the replacement is live.
    waits_for: Option<&'static str>,
}

const fn data(id: &'static str, patterns: &'static [&'static str]) -> Family {
    Family {
        id,
        class: Class::Data,
        patterns,
        contains: None,
        replacement: "",
        waits_for: None,
    }
}

const fn community(id: &'static str, patterns: &'static [&'static str]) -> Family {
    Family {
        id,
        class: Class::Community,
        patterns,
        contains: None,
        replacement: "",
        waits_for: None,
    }
}

const fn legacy(
    id: &'static str,
    patterns: &'static [&'static str],
    replacement: &'static str,
    waits_for: Option<&'static str>,
) -> Family {
    Family {
        id,
        class: Class::Legacy,
        patterns,
        contains: None,
        replacement,
        waits_for,
    }
}

const RELEASE_TEMPLATE: Option<&str> = Some("the rendered release flow");
const NO_REMOTE: &str = "a Forgejo job on the `windows` or `macos` label, or an evaluation on `linux` for aarch64 (render_schema = 3 has no remote scheduler)";

/// The surface table, first match wins. Rendered files are recognised by their marker before the table is read.
const TABLE: &[Family] = &[
    data(
        "manifest",
        &[".ci/ccid.toml", ".ci/archives.toml", ".ci/live-inputs.toml"],
    ),
    community(
        "provider-community",
        &[
            ".github/ISSUE_TEMPLATE/**",
            ".github/ISSUE_TEMPLATE.md",
            ".github/issue_template.md",
            ".github/DISCUSSION_TEMPLATE/**",
            ".github/PULL_REQUEST_TEMPLATE/**",
            ".github/PULL_REQUEST_TEMPLATE.md",
            ".github/pull_request_template.md",
            ".github/CODEOWNERS",
            ".github/FUNDING.yml",
            ".github/SECURITY.md",
            ".github/CONTRIBUTING.md",
            ".github/CODE_OF_CONDUCT.md",
            ".github/SUPPORT.md",
            ".github/RELEASE_TEMPLATE.md",
            ".github/copilot-instructions.md",
            ".gitlab/issue_templates/**",
            ".gitlab/merge_request_templates/**",
            ".gitlab/CODEOWNERS",
            ".forgejo/ISSUE_TEMPLATE/**",
            ".forgejo/issue_template/**",
            ".forgejo/PULL_REQUEST_TEMPLATE.md",
            ".forgejo/pull_request_template.md",
            ".ci/README.md",
        ],
    ),
    legacy(
        "dependabot",
        &[
            ".github/dependabot.yml",
            ".github/dependabot.yaml",
            ".github/renovate.json",
            ".github/renovate.json5",
            ".github/workflows/dependabot-auto-merge.yml",
            ".github/workflows/dependabot-automerge.yml",
            ".github/workflows/dependabot-merge.yml",
            ".github/test-auto-merge.mjs",
        ],
        "nothing: dependencies move by event (`ccid lock-refresh`), and advisories stay a verify check",
        None,
    ),
    legacy(
        "crow",
        &[".crow/**", ".github/workflows/ccid.yml"],
        "Forgejo Actions (Crow is retired)",
        None,
    ),
    legacy(
        "remote-scheduler",
        &[".github/workflows/ccid-remote.yml", ".ci/providers.toml"],
        NO_REMOTE,
        None,
    ),
    Family {
        id: "remote-scheduler",
        class: Class::Legacy,
        patterns: &[".gitlab-ci.yml"],
        contains: Some("CCID_REMOTE_JOB"),
        replacement: NO_REMOTE,
        waits_for: None,
    },
    legacy(
        "circleci",
        &[".circleci/**"],
        "Forgejo Actions (the only CI engine)",
        None,
    ),
    legacy(
        "bitbucket",
        &["bitbucket-pipelines.yml"],
        "nothing: a Bitbucket copy is git only",
        None,
    ),
    // The CI files of the mirror forges that earlier releases rendered (R6, R7). Only a file that carries the render
    // marker belongs here: a hand-written file at one of these paths is the repository's own and stays unknown.
    Family {
        id: "mirror-ci",
        class: Class::Legacy,
        patterns: &super::retired::FILES,
        contains: Some(super::CROW_MARKER),
        replacement: super::retired::REPLACEMENT,
        waits_for: None,
    },
    legacy(
        "github-ci",
        &[".github/workflows/ci.yml"],
        "per-commit verify on Forgejo (a mirror forge runs no CI: R6)",
        None,
    ),
    legacy(
        "github-release",
        &[".github/workflows/release.yml"],
        "the rendered Forgejo release flow (a registry that a forge publishes only through its CI is published from our side or not at all: R46)",
        RELEASE_TEMPLATE,
    ),
    legacy(
        "forgejo-release",
        &[
            ".forgejo/workflows/release.yml",
            ".forgejo/workflows/release-macos.yml",
        ],
        "the rendered Forgejo release flow",
        RELEASE_TEMPLATE,
    ),
    legacy(
        "release-check",
        &[".ci/release-check.py"],
        "nothing: `render --check` of the rendered release files",
        RELEASE_TEMPLATE,
    ),
    legacy(
        "lock-refresh",
        &[
            ".github/workflows/refresh-lock.yml",
            ".github/workflows/resolve.yml",
        ],
        "`ccid lock-refresh`, started by events",
        Some("the workflow that runs `ccid lock-refresh` on events (the release preflight, the release-published dispatch)"),
    ),
    legacy(
        "job-script",
        &[".ci/run-cached.sh"],
        "`ccid job-run` with the `ccid:run-declared-checks` command",
        Some("the job runtime (`[runtime]`)"),
    ),
    legacy(
        "first-party-graph",
        &[
            ".github/check-pins.mjs",
            ".github/check-pins.test.mjs",
            ".github/check-lock.py",
            ".github/test-lock.py",
            ".ci/check-lock.py",
            ".ci/test-lock.py",
        ],
        "`ccid first-party --metadata`",
        Some("`ccid first-party --metadata-issuers`"),
    ),
    legacy(
        "browser-coverage",
        &[
            ".ci/browser-coverage-runner.py",
            ".github/browser-coverage-runner.py",
        ],
        "a shared browser coverage runner",
        Some("a shared browser coverage runner"),
    ),
    legacy(
        "first-party",
        &[".github/check-first-party.py", ".ci/check-first-party.py"],
        "`ccid first-party`",
        None,
    ),
    legacy(
        "coverage",
        &[
            ".github/check-coverage.py",
            ".github/check-coverage-json.py",
            ".github/check-source-coverage.py",
            ".github/test-source-coverage.py",
            ".github/test-check-source-coverage.py",
            ".github/test-coverage.py",
            ".github/coverage-runtime/**",
            ".github/requirements.txt",
            ".ci/check-coverage.py",
            ".ci/check-coverage-json.py",
            ".ci/check-source-coverage.py",
            ".ci/test-source-coverage.py",
            ".ci/test-coverage.py",
            ".ci/coverage-runtime/**",
            ".ci/requirements.txt",
        ],
        "`ccid coverage`",
        None,
    ),
    legacy(
        "coverage-exclusions",
        &[
            ".github/coverage-exclusions.json",
            ".ci/coverage-exclusions.json",
        ],
        "`[[coverage.exclusions]]` in .ci/ccid.toml (`ccid coverage convert-exclusions`)",
        None,
    ),
    legacy(
        "advisories",
        &[".github/check-advisory-exposure.mjs", ".ci/advisories.sh"],
        "`ccid advisories`",
        None,
    ),
    legacy(
        "content-pins",
        &[
            ".ci/check-fixtures.py",
            ".ci/check-native-sdk.py",
            ".ci/test-native-sdk.py",
        ],
        "`ccid content-pins`",
        None,
    ),
    legacy(
        "toolchain",
        &[".ci/toolchain-lock.sh", ".ci/wasm-runner.sh"],
        "`ccid toolchain`",
        None,
    ),
    legacy(
        "float",
        &[".ci/update-our-crates.sh", ".ci/our-crates.txt"],
        "`ccid float`",
        None,
    ),
    legacy(
        "resolve",
        &[
            ".ci/resolve-dependencies.sh",
            ".ci/test-resolve-dependencies.py",
            ".ci/resolve-npm.sh",
        ],
        "`ccid resolve-lock --npm` (Rust lock resolution is retired)",
        None,
    ),
    legacy(
        "verify-script",
        &[
            ".ci/verify.sh",
            ".ci/native.sh",
            ".ci/check.sh",
            ".ci/native-check.sh",
        ],
        "`[checks.verify] kind = \"commands\"`",
        None,
    ),
    legacy(
        "actionlint",
        &[".github/actionlint.yaml", ".github/actionlint.yml"],
        "nothing: the rendered workflows are actionlint-clean in ccid's own tests",
        None,
    ),
];

/// The table family of a file that is not ccid's own, by path and (for content-scoped families) content.
fn family(relative: &str, content: Option<&[u8]>) -> Option<&'static Family> {
    TABLE.iter().find(|family| {
        family
            .patterns
            .iter()
            .any(|pattern| glob_match(pattern, relative))
            && family.contains.is_none_or(|needle| {
                content.is_some_and(|bytes| contains(bytes, needle.as_bytes()))
            })
    })
}

/// The release families once the manifest declares `[release]`: their replacement is live for that repository,
/// so render deletes them (or replaces them where it renders its own file). Without the table they wait.
const LIVE_RELEASE: &[Family] = &[
    legacy(
        "github-release",
        &[".github/workflows/release.yml"],
        "the rendered Forgejo release flow (a registry that a forge publishes only through its CI is published from our side or not at all: R46)",
        None,
    ),
    legacy(
        "forgejo-release",
        &[
            ".forgejo/workflows/release.yml",
            ".forgejo/workflows/release-macos.yml",
        ],
        "the rendered Forgejo release flow",
        None,
    ),
    legacy(
        "release-check",
        &[".ci/release-check.py"],
        "nothing: `render --check` of the rendered release files",
        None,
    ),
];

/// [`family`], with the release families live when the manifest declares `[release]`.
fn family_for(relative: &str, content: Option<&[u8]>, release: bool) -> Option<&'static Family> {
    let table = family(relative, content)?;
    if release {
        if let Some(live) = LIVE_RELEASE.iter().find(|live| live.id == table.id) {
            return Some(live);
        }
    }
    Some(table)
}

/// `*` and `?` inside one path segment, `**` as a whole segment for any number of segments.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    segments_match(&pattern, &path)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
        Some((first, rest)) => {
            !path.is_empty()
                && segment_match(first.as_bytes(), path[0].as_bytes())
                && segments_match(rest, &path[1..])
        }
    }
}

fn segment_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&byte| byte == b'*')
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// `needle` occurs in `haystack` as a whole path or name: not inside a longer file name.
fn mentions(haystack: &[u8], needle: &[u8]) -> bool {
    let name = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-';
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    (0..=haystack.len() - needle.len()).any(|start| {
        let end = start + needle.len();
        &haystack[start..end] == needle
            && (start == 0 || !(name(haystack[start - 1]) || haystack[start - 1] == b'.'))
            && (end == haystack.len() || !name(haystack[end]))
    })
}

/// A repository-relative path written with `/`, refused when it could leave the repository or is not plain.
fn plain_relative(path: &str) -> Option<String> {
    let path = path.strip_prefix("./").unwrap_or(path);
    let ok = !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    ok.then(|| path.to_owned())
}

fn under_roots(path: &str) -> bool {
    ROOT_FILES.contains(&path)
        || ROOTS.iter().any(|root| {
            path.strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
        })
}

/// Validate `[surface]`: every entry names a plain path under the surface roots, keep entries exist and say why.
pub(super) fn validate(repo: &Path, config: &Config) -> Result<()> {
    for (path, reason) in &config.keep {
        let plain = plain_relative(path)
            .filter(|plain| plain == path && under_roots(plain))
            .ok_or_else(|| {
                failure(format!(
                    "[surface] keep must name a plain path under {} or {}: {path}",
                    ROOTS.join(", "),
                    ROOT_FILES.join(", ")
                ))
            })?;
        if reason.trim().is_empty() {
            return Err(failure(format!("[surface] keep needs a reason: {plain}")));
        }
        match metadata(&repo.join(&plain))? {
            Some(found) if found.is_file() && !found.file_type().is_symlink() => {}
            _ => {
                return Err(failure(format!(
                    "[surface] keep names a file that is not in the repository: {plain}"
                )))
            }
        }
    }
    for pattern in &config.upstream {
        if plain_relative(pattern).as_deref() != Some(pattern.as_str()) || !under_roots(pattern) {
            return Err(failure(format!(
                "[surface] upstream must name paths or globs under the surface roots: {pattern}"
            )));
        }
    }
    Ok(())
}

/// One file of the surface on disk.
struct Found {
    relative: String,
    symlink: bool,
}

/// Every file under the surface roots, without following a symlink.
fn surface_files(repo: &Path) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    for root in ROOTS {
        let Some(info) = metadata(&repo.join(root))? else {
            continue;
        };
        if !info.is_dir() || info.file_type().is_symlink() {
            return Err(failure(format!("Unsafe adapter directory: {root}")));
        }
        walk(repo, Path::new(root), &mut found)?;
    }
    // In a git work tree the surface is what git sees: tracked files and new files git does not ignore
    // (a `__pycache__` beside a script is nobody's CI file).
    if let Some(seen) = visible(repo) {
        found.retain(|file| seen.contains(&file.relative));
    }
    for file in ROOT_FILES {
        if let Some(info) = metadata(&repo.join(file))? {
            found.push(Found {
                relative: file.to_owned(),
                symlink: info.file_type().is_symlink(),
            });
        }
    }
    found.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(found)
}

fn walk(repo: &Path, directory: &Path, found: &mut Vec<Found>) -> Result<()> {
    let mut entries: Vec<_> =
        fs::read_dir(repo.join(directory))?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let relative = directory.join(entry.file_name());
        let kind = entry.file_type()?;
        let text = relative
            .to_str()
            .ok_or_else(|| {
                failure(format!(
                    "A CI file name is not UTF-8: {}",
                    relative.display()
                ))
            })?
            .replace('\\', "/");
        if kind.is_dir() {
            walk(repo, &relative, found)?;
        } else {
            found.push(Found {
                relative: text,
                symlink: kind.is_symlink(),
            });
        }
    }
    Ok(())
}

/// The surface files git sees (tracked, or new and not ignored), or `None` outside a git work tree.
fn visible(repo: &Path) -> Option<BTreeSet<String>> {
    let output = Process::new("git")
        .args([
            "--literal-pathspecs",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
        ])
        .args(ROOTS)
        .args(ROOT_FILES)
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
            .filter_map(|path| std::str::from_utf8(path).ok().map(str::to_owned))
            .collect(),
    )
}

/// The tracked files of the repository (`git ls-files`), or `None` outside a git work tree.
fn tracked(repo: &Path) -> Option<Vec<String>> {
    let output = Process::new("git")
        .args(["--literal-pathspecs", "ls-files", "-z"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
            .filter_map(|path| std::str::from_utf8(path).ok().map(str::to_owned))
            .collect(),
    )
}

/// Every file of a directory that is not a git work tree, skipping build output and symlinks.
fn walked(repo: &Path) -> Result<Vec<String>> {
    fn visit(repo: &Path, directory: &Path, out: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(repo.join(directory))? {
            let entry = entry?;
            let name = entry.file_name();
            let kind = entry.file_type()?;
            let relative = directory.join(&name);
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if matches!(
                    name.to_str(),
                    Some(".git" | "target" | "node_modules" | ".direnv")
                ) {
                    continue;
                }
                visit(repo, &relative, out)?;
            } else if let Some(text) = relative.to_str() {
                out.push(text.replace('\\', "/"));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(repo, Path::new(""), &mut out)?;
    Ok(out)
}

/// Prose mentions files; it never runs one.
fn prose(relative: &str) -> bool {
    let name = Path::new(relative)
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        Path::new(&name).extension().and_then(|ext| ext.to_str()),
        Some("md" | "markdown" | "mdx" | "txt" | "rst" | "adoc" | "org")
    ) || name.starts_with("license")
        || name.starts_with("changelog")
        || name.starts_with("copying")
        || name == "notice"
}

/// A file's text for the reference search: regular, at most [`READ_LIMIT`] bytes, no NUL in its head.
fn text(repo: &Path, relative: &str) -> Option<Vec<u8>> {
    let path = repo.join(relative);
    let info = fs::symlink_metadata(&path).ok()?;
    if !info.is_file() || info.len() > READ_LIMIT {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    (!bytes[..bytes.len().min(8192)].contains(&0)).then_some(bytes)
}

/// The strings by which another file refers to `relative`: its path, its directories below a surface root
/// (other than the workflow directories) and, for a file in the same directory, its name.
fn needles(relative: &str) -> (Vec<String>, Option<(String, String)>) {
    let mut full = vec![relative.to_owned()];
    let mut parent = Path::new(relative).parent();
    while let Some(directory) = parent {
        let text = directory.to_string_lossy().replace('\\', "/");
        if text.matches('/').count() >= 1
            && text != ".github/workflows"
            && text != ".forgejo/workflows"
        {
            full.push(text);
        }
        parent = directory.parent();
    }
    let name = Path::new(relative)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let directory = Path::new(relative)
        .parent()
        .map(|directory| directory.to_string_lossy().replace('\\', "/"));
    (full, name.zip(directory))
}

fn refers(referrer: &str, text: &[u8], target: &str) -> bool {
    if referrer == target {
        return false;
    }
    let (full, local) = needles(target);
    if full.iter().any(|needle| mentions(text, needle.as_bytes())) {
        return true;
    }
    local.is_some_and(|(name, directory)| {
        Path::new(referrer)
            .parent()
            .is_some_and(|parent| parent.to_string_lossy().replace('\\', "/") == directory)
            && mentions(text, name.as_bytes())
    })
}

/// What git can restore: the paths among `paths` that are tracked and unmodified in the work tree.
fn restorable(repo: &Path, paths: &[String]) -> BTreeSet<String> {
    if paths.is_empty() {
        return BTreeSet::new();
    }
    let run = |args: &[&str]| -> Option<Vec<String>> {
        let output = Process::new("git")
            .arg("--literal-pathspecs")
            .args(args)
            .arg("--")
            .args(paths)
            .current_dir(repo)
            .output()
            .ok()?;
        output.status.success().then(|| {
            output
                .stdout
                .split(|&byte| byte == 0)
                .filter(|item| !item.is_empty())
                .filter_map(|item| std::str::from_utf8(item).ok().map(str::to_owned))
                .collect()
        })
    };
    let Some(tracked) = run(&["ls-files", "-z"]) else {
        return BTreeSet::new();
    };
    let Some(status) = run(&[
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--no-renames",
    ]) else {
        return BTreeSet::new();
    };
    // Without renames a porcelain -z entry is "XY path".
    let dirty: BTreeSet<String> = status
        .iter()
        .map(|entry| entry.get(3..).unwrap_or(entry).to_owned())
        .collect();
    tracked
        .into_iter()
        .filter(|path| !dirty.contains(path))
        .collect()
}

/// Classify every surface file of `repo` for a `render_schema = 3` render whose files are `outputs`.
///
pub(super) fn assess(
    repo: &Path,
    parsed: &crate::Manifest,
    outputs: &BTreeMap<PathBuf, String>,
) -> Result<Vec<Entry>> {
    let default = Config::default();
    let config = parsed.surface.as_ref().unwrap_or(&default);
    validate(repo, config)?;
    let named = parsed.named_strings();
    let globs = parsed.input_globs();
    let rendered: BTreeMap<String, &String> = outputs
        .iter()
        .map(|(path, body)| (path.to_string_lossy().replace('\\', "/"), body))
        .collect();
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    let entry = |path: &str, class, action, family: Option<&'static Family>, note: String| Entry {
        path: PathBuf::from(path),
        class,
        action,
        family: family.map(|family| family.id),
        replacement: family
            .filter(|family| family.class == Class::Legacy)
            .map(|family| family.replacement),
        note,
    };
    // Files classified Legacy or Unknown that only the reference analysis below can settle.
    let mut open: Vec<(String, Option<&'static Family>)> = Vec::new();
    let found = surface_files(repo)?;
    let present: BTreeSet<&str> = found.iter().map(|file| file.relative.as_str()).collect();
    for path in rendered
        .keys()
        .filter(|path| !present.contains(path.as_str()))
    {
        entries.insert(
            path.clone(),
            entry(path, Class::Rendered, Action::Write, None, "new".to_owned()),
        );
    }
    for file in &found {
        let relative = file.relative.as_str();
        let content = if file.symlink {
            None
        } else {
            Some(fs::read(repo.join(relative))?)
        };
        let ours = content
            .as_deref()
            .is_some_and(|bytes| owned(Path::new(relative), &String::from_utf8_lossy(bytes)));
        let table = family_for(relative, content.as_deref(), parsed.release.is_some());
        if rendered.contains_key(relative) {
            let item = if ours {
                entry(
                    relative,
                    Class::Rendered,
                    Action::Write,
                    None,
                    String::new(),
                )
            } else if table
                .is_some_and(|family| family.class == Class::Legacy && family.waits_for.is_none())
                && !file.symlink
            {
                entry(
                    relative,
                    Class::Legacy,
                    Action::Write,
                    table,
                    "a hand-written file of a retired family where ccid renders its own".to_owned(),
                )
            } else {
                entry(
                    relative,
                    table.map_or(Class::Unknown, |family| family.class),
                    Action::Refuse,
                    table,
                    "a hand-written file at a path ccid renders".to_owned(),
                )
            };
            entries.insert(relative.to_owned(), item);
            continue;
        }
        if ours {
            let item = if super::retired::FILES.contains(&relative) {
                // The CI of a mirror forge that an earlier release rendered: retired, not merely "no longer rendered".
                entry(
                    relative,
                    Class::Legacy,
                    Action::Delete,
                    table,
                    "a mirror forge runs no CI (R6, R7)".to_owned(),
                )
            } else {
                entry(
                    relative,
                    Class::Rendered,
                    Action::Delete,
                    None,
                    "no longer rendered".to_owned(),
                )
            };
            entries.insert(relative.to_owned(), item);
            continue;
        }
        if config
            .upstream
            .iter()
            .any(|pattern| glob_match(pattern, relative))
        {
            entries.insert(
                relative.to_owned(),
                entry(
                    relative,
                    Class::Upstream,
                    Action::Keep,
                    table,
                    String::new(),
                ),
            );
            continue;
        }
        if let Some(reason) = config.keep.get(relative) {
            entries.insert(
                relative.to_owned(),
                entry(relative, Class::Script, Action::Keep, table, reason.clone()),
            );
            continue;
        }
        let by_manifest = named
            .iter()
            .any(|value| mentions(value.as_bytes(), relative.as_bytes()))
            || globs
                .iter()
                .any(|glob| plain_relative(glob).is_some_and(|glob| glob_match(&glob, relative)));
        match table.map(|family| family.class) {
            Some(class @ (Class::Data | Class::Community)) => {
                entries.insert(
                    relative.to_owned(),
                    entry(relative, class, Action::Keep, table, String::new()),
                );
            }
            Some(Class::Legacy) if by_manifest => {
                entries.insert(
                    relative.to_owned(),
                    entry(
                        relative,
                        Class::Legacy,
                        Action::Keep,
                        table,
                        "still named by .ci/ccid.toml".to_owned(),
                    ),
                );
            }
            None if by_manifest => {
                entries.insert(
                    relative.to_owned(),
                    entry(
                        relative,
                        Class::Script,
                        Action::Keep,
                        None,
                        "named by .ci/ccid.toml".to_owned(),
                    ),
                );
            }
            _ if file.symlink => {
                entries.insert(
                    relative.to_owned(),
                    entry(
                        relative,
                        table.map_or(Class::Unknown, |family| family.class),
                        Action::Refuse,
                        table,
                        "a symlink: ccid never follows or deletes one".to_owned(),
                    ),
                );
            }
            _ => open.push((relative.to_owned(), table)),
        }
    }
    settle(repo, &mut entries, open, &rendered, &entry)?;
    // Render deletes or replaces a hand-written file only when git can restore it.
    let touched: Vec<String> = entries
        .values()
        .filter(|item| {
            item.class == Class::Legacy && matches!(item.action, Action::Delete | Action::Write)
        })
        .map(|item| item.path.to_string_lossy().into_owned())
        .collect();
    let safe = restorable(repo, &touched);
    for path in touched {
        if !safe.contains(&path) {
            let item = entries.get_mut(&path).expect("touched entries exist");
            item.action = Action::Refuse;
            item.note =
                "not committed as it is: render deletes or replaces only files git can restore"
                    .to_owned();
        }
    }
    Ok(entries.into_values().collect())
}

type EntryFn<'a> = dyn Fn(&str, Class, Action, Option<&'static Family>, String) -> Entry + 'a;

/// Settle the open (legacy and unknown) files by reference: a file that a kept script uses is kept as well;
/// a live legacy file goes only when nothing that stays uses it; anything else unknown is refused.
fn settle(
    repo: &Path,
    entries: &mut BTreeMap<String, Entry>,
    open: Vec<(String, Option<&'static Family>)>,
    rendered: &BTreeMap<String, &String>,
    entry: &EntryFn<'_>,
) -> Result<()> {
    if open.is_empty() {
        return Ok(());
    }
    // Every text that can refer to an open file: the repository's files (the manifest is read as data above),
    // with the rendered body in place of a rendered file.
    let mut paths: BTreeSet<String> = match tracked(repo) {
        Some(paths) => paths.into_iter().collect(),
        None => walked(repo)?.into_iter().collect(),
    };
    paths.extend(entries.keys().cloned());
    paths.extend(open.iter().map(|(path, _)| path.clone()));
    paths.remove(".ci/ccid.toml");
    let texts: BTreeMap<String, Vec<u8>> = paths
        .into_iter()
        .filter_map(|path| {
            let body = match rendered.get(&path) {
                Some(body) => Some(body.as_bytes().to_vec()),
                None => text(repo, &path),
            };
            body.map(|body| (path, body))
        })
        .collect();
    // Who uses each open file. Prose (README, notes) only mentions a file; it uses none.
    let referrers: BTreeMap<&str, Vec<&str>> = open
        .iter()
        .map(|(target, _)| {
            let by = texts
                .iter()
                .filter(|(referrer, body)| !prose(referrer) && refers(referrer, body, target))
                .map(|(referrer, _)| referrer.as_str())
                .collect();
            (target.as_str(), by)
        })
        .collect();
    let mentioned = |target: &str| {
        texts
            .iter()
            .filter(|(referrer, body)| prose(referrer) && refers(referrer, body, target))
            .map(|(referrer, _)| referrer.as_str())
            .collect::<Vec<_>>()
    };
    let deleted_rendered: BTreeSet<String> = entries
        .values()
        .filter(|item| item.action == Action::Delete)
        .map(|item| item.path.to_string_lossy().into_owned())
        .collect();
    let mut going: BTreeSet<&str> = open
        .iter()
        .filter(|(_, family)| family.is_some_and(|family| family.waits_for.is_none()))
        .map(|(path, _)| path.as_str())
        .collect();
    // A script the manifest runs makes the files it uses scripts too (transitively). The roots are what the
    // manifest names (also legacy scripts), the `[surface] keep` entries and the data files. A file that stays only
    // because something else uses it is no root: a refused workflow and the file it calls must not keep each other.
    let mut kept: BTreeSet<String> = entries
        .values()
        .filter(|item| {
            item.action == Action::Keep
                && matches!(item.class, Class::Script | Class::Data | Class::Legacy)
        })
        .map(|item| item.path.to_string_lossy().into_owned())
        .collect();
    let mut used_by: BTreeMap<&str, &str> = BTreeMap::new();
    loop {
        let mut changed = false;
        for (target, _) in &open {
            if kept.contains(target) {
                continue;
            }
            if let Some(by) = referrers[target.as_str()]
                .iter()
                .find(|referrer| kept.contains(**referrer))
            {
                kept.insert(target.clone());
                used_by.insert(target.as_str(), by);
                going.remove(target.as_str());
                changed = true;
            }
        }
        // A live legacy file stays while anything that stays refers to it.
        for (target, _) in &open {
            if !going.contains(target.as_str()) {
                continue;
            }
            if let Some(by) = referrers[target.as_str()].iter().find(|referrer| {
                !going.contains(**referrer) && !deleted_rendered.contains(**referrer)
            }) {
                going.remove(target.as_str());
                // Kept, but not a script the manifest runs: what it uses is not kept through it.
                used_by.insert(target.as_str(), by);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for (target, family) in &open {
        let target = target.as_str();
        let item = match (family, kept.contains(target)) {
            (Some(family), true) => entry(
                target,
                Class::Legacy,
                Action::Keep,
                Some(family),
                format!("still used by {}", used_by[target]),
            ),
            (None, true) => entry(
                target,
                Class::Script,
                Action::Keep,
                None,
                format!("used by {}", used_by[target]),
            ),
            (Some(family), false) if going.contains(target) => {
                let prose = mentioned(target);
                let note = if prose.is_empty() {
                    String::new()
                } else {
                    format!("mentioned in {}", prose.join(", "))
                };
                entry(target, Class::Legacy, Action::Delete, Some(family), note)
            }
            (Some(family), false) => match (family.waits_for, used_by.get(target)) {
                // A family whose replacement is not live yet goes only through the manifest or a kept script.
                (Some(waits), _) => entry(
                    target,
                    Class::Legacy,
                    Action::Refuse,
                    Some(family),
                    format!("render deletes it once {waits} is live; until then the repository stays on render_schema = 2"),
                ),
                (None, by) => entry(
                    target,
                    Class::Legacy,
                    Action::Keep,
                    Some(family),
                    format!("still used by {}", by.copied().unwrap_or("a file that stays")),
                ),
            },
            (None, false) => {
                let note = match referrers[target]
                    .iter()
                    .find(|referrer| !deleted_rendered.contains(**referrer))
                {
                    Some(by) => format!(
                        "used by {by}: list it in [surface] keep with the reason, or name it in a declared check"
                    ),
                    None => "list it in [surface] keep or upstream, or remove it".to_owned(),
                };
                entry(target, Class::Unknown, Action::Refuse, None, note)
            }
        };
        entries.insert(target.to_owned(), item);
    }
    Ok(())
}

/// One line per refused file, for the error of a refused render.
pub(super) fn refusal(entries: &[Entry]) -> Option<String> {
    let refused: Vec<String> = entries
        .iter()
        .filter(|item| item.action == Action::Refuse)
        .map(|item| {
            let what = match (item.class, item.family) {
                (Class::Legacy, Some(family)) => format!("legacy {family}"),
                (class, _) => format!("{class:?}").to_lowercase(),
            };
            format!("{} ({what}: {})", item.path.display(), item.note)
        })
        .collect();
    (!refused.is_empty()).then(|| {
        format!(
            "render_schema = 3 owns the whole CI surface and refuses: {}",
            refused.join("; ")
        )
    })
}

/// `ccid surface report`: every CI file of one repository.
#[derive(Debug, Serialize)]
pub struct Report {
    /// The manifest's `render_schema` (absent: the repository pins its release).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_schema: Option<u32>,
    pub files: Vec<Entry>,
}

/// One data edit of `.ci/ccid.toml` on the way to `render_schema = 3`.
#[derive(Debug, Clone, Serialize)]
pub struct Edit {
    /// The kind of edit, the same for every repository (for fleet totals).
    pub kind: &'static str,
    /// The manifest key the edit changes.
    pub key: String,
    /// The value before the edit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub was: Option<String>,
    /// The value after the edit; absent when the key goes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now: Option<String>,
}

/// `ccid surface plan` of one repository: the mechanical data edits, the decisions a person makes, and what a
/// `render_schema = 3` render of the edited manifest writes, deletes, keeps and refuses.
#[derive(Debug, Default, Serialize)]
pub struct Plan {
    pub repo: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_schema: Option<i64>,
    /// Nothing refused, no decision open: the edits and one render align the repository.
    pub ready: bool,
    pub edits: Vec<Edit>,
    /// What a person decides; the plan applied the conservative default (named in the text).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    /// Behaviour the edits change.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The edited manifest does not render at `render_schema = 3`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Files the render writes that differ from the repository's (new or changed).
    pub writes: Vec<PathBuf>,
    pub deletes: Vec<Entry>,
    /// Legacy files kept because something that stays still uses them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<Entry>,
    pub refused: Vec<Entry>,
}

impl Plan {
    pub(super) fn settle(&mut self, entries: Vec<Entry>) {
        for item in entries {
            match (item.class, item.action) {
                (_, Action::Delete) => self.deletes.push(item),
                (_, Action::Refuse) => self.refused.push(item),
                (Class::Legacy, Action::Keep) => self.kept.push(item),
                (Class::Legacy, Action::Write) => self.writes.push(item.path),
                _ => {}
            }
        }
        self.writes.sort();
        self.writes.dedup();
    }

    pub(super) fn finish(&mut self) {
        self.ready = self.error.is_none() && self.refused.is_empty() && self.decisions.is_empty();
    }
}

fn shown(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn strings(value: Option<&toml::Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

/// Apply the mechanical `render_schema = 3` edits to a manifest's TOML value. Returns the edits, the decisions a
/// person makes (the plan applies the conservative default: a remote or Crow job goes or moves to Forgejo) and notes.
pub(super) fn align(value: &mut toml::Value) -> (Vec<Edit>, Vec<String>, Vec<String>) {
    let mut edits = Vec::new();
    let mut decisions = Vec::new();
    let mut notes = Vec::new();
    let Some(table) = value.as_table_mut() else {
        decisions.push("the manifest is not a TOML table".to_owned());
        return (edits, decisions, notes);
    };
    let render = table
        .entry("render")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let Some(render) = render.as_table_mut() {
        let was = render.insert("render_schema".to_owned(), toml::Value::Integer(3));
        if was.as_ref().and_then(toml::Value::as_integer) != Some(3) {
            edits.push(Edit {
                kind: "render_schema",
                key: "render.render_schema".to_owned(),
                was: was.as_ref().map(shown),
                now: Some("3".to_owned()),
            });
        }
        for key in ["tool_revision", "tool_sha256", "allow_crow"] {
            if let Some(was) = render.remove(key) {
                edits.push(Edit {
                    kind: if key == "allow_crow" {
                        "allow_crow"
                    } else {
                        "own_pin"
                    },
                    key: format!("render.{key}"),
                    was: Some(shown(&was)),
                    now: None,
                });
            }
        }
    }
    if table.remove("push_consumer").is_some() {
        decisions
            .push("push_consumer: Crow fan-out has no Forgejo form; the plan drops it".to_owned());
    }
    let mut verdict = strings(table.get("verdict").and_then(|verdict| verdict.get("jobs")));
    // The evaluation-only aarch64 pattern: a nix repository covers aarch64 by evaluating every system without
    // building (`kind = "nix"`, `mode = "eval"`), on `linux`; a native aarch64 job is never a runner (B6).
    let nix_checks: Vec<(String, Option<String>)> = table
        .get("checks")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
        .filter(|(_, check)| check.get("kind").and_then(toml::Value::as_str) == Some("nix"))
        .map(|(name, check)| {
            (
                name.clone(),
                check
                    .get("mode")
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned),
            )
        })
        .collect();
    let mut aarch64_remote = Vec::new();
    let Some(jobs) = table.get_mut("jobs").and_then(toml::Value::as_table_mut) else {
        decisions.push("no [jobs]: render_schema = 3 needs a verdict job".to_owned());
        return (edits, decisions, notes);
    };
    // Schedulers: remote jobs go, Crow and Argo jobs move to Forgejo.
    let names: Vec<String> = jobs.keys().cloned().collect();
    for name in &names {
        let job = &jobs[name];
        let scheduler = job
            .get("scheduler")
            .and_then(toml::Value::as_str)
            .unwrap_or("crow");
        match scheduler {
            "forgejo" => {}
            "remote" => {
                let provider = job.get("provider").map(shown).unwrap_or_default();
                let platform = job.get("platform").map(shown).unwrap_or_default();
                let aarch64 = platform.contains("arm64") || platform.contains("aarch64");
                if aarch64 && !nix_checks.is_empty() {
                    aarch64_remote.push(name.clone());
                    notes.push(format!(
                        "remote job {name} ({provider}, {platform}): aarch64 is covered by evaluation only (`nix flake check --all-systems --no-build`, on linux); the plan drops the remote job"
                    ));
                } else {
                    decisions.push(format!(
                        "remote job {name} ({provider}, {platform}): a Forgejo job on the windows or macos label, an evaluation on linux for aarch64, or nothing; the plan drops it"
                    ));
                }
                jobs.remove(name);
                if let Some(list) = verdict.as_mut() {
                    list.retain(|job| job != name);
                }
                edits.push(Edit {
                    kind: "remote_job",
                    key: format!("jobs.{name}"),
                    was: Some(format!("scheduler = remote ({provider}, {platform})")),
                    now: None,
                });
            }
            other => {
                decisions.push(format!(
                    "job {name} runs on {other}: render_schema = 3 runs Forgejo jobs only; the plan sets scheduler = \"forgejo\""
                ));
                if let Some(job) = jobs.get_mut(name).and_then(toml::Value::as_table_mut) {
                    job.insert(
                        "scheduler".to_owned(),
                        toml::Value::String("forgejo".to_owned()),
                    );
                }
            }
        }
    }
    let gating: Vec<String> = verdict.unwrap_or_else(|| {
        if jobs.contains_key("verify") {
            vec!["verify".to_owned()]
        } else {
            Vec::new()
        }
    });
    if gating.is_empty() {
        decisions.push(
            "no verdict job: render_schema = 3 verifies every commit ([jobs.verify] or [verdict] jobs)".to_owned(),
        );
    }
    // Labels: every label outside linux, windows and macos becomes linux.
    for (name, job) in jobs.iter_mut() {
        let Some(job) = job.as_table_mut() else {
            continue;
        };
        if let Some(label) = job.get("runs_on").and_then(toml::Value::as_str) {
            if !LABELS.contains(&label) {
                let was = label.to_owned();
                job.insert(
                    "runs_on".to_owned(),
                    toml::Value::String(DEFAULT_LABEL.to_owned()),
                );
                edits.push(Edit {
                    kind: "runs_on",
                    key: format!("jobs.{name}.runs_on"),
                    was: Some(was),
                    now: Some(DEFAULT_LABEL.to_owned()),
                });
            }
        }
    }
    // A separate pull-request job with the verdict's checks and command folds into the verdict job.
    let shape = |job: &toml::Value| {
        let mut checks = strings(job.get("checks")).unwrap_or_default();
        checks.sort();
        (
            checks,
            strings(job.get("command")).unwrap_or_default(),
            job.get("runs_on")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
        )
    };
    let verdicts: Vec<(Vec<String>, Vec<String>, Option<String>)> = gating
        .iter()
        .filter_map(|name| jobs.get(name))
        .map(shape)
        .collect();
    let folded: Vec<String> = jobs
        .iter()
        .filter(|(name, job)| {
            !gating.contains(name)
                && strings(job.get("events")).as_deref() == Some(&["pull_request".to_owned()][..])
                && {
                    let (checks, command, label) = shape(job);
                    verdicts.iter().any(|(vchecks, vcommand, vlabel)| {
                        checks.iter().all(|check| vchecks.contains(check))
                            && command == *vcommand
                            && label == *vlabel
                    })
                }
        })
        .map(|(name, _)| name.clone())
        .collect();
    for name in folded {
        jobs.remove(&name);
        edits.push(Edit {
            kind: "pull_request_job",
            key: format!("jobs.{name}"),
            was: Some("events = [\"pull_request\"], the verdict's checks".to_owned()),
            now: None,
        });
    }
    // Events: verdict jobs take none (every commit), the manual default is implicit for the others.
    for (name, job) in jobs.iter_mut() {
        let Some(job) = job.as_table_mut() else {
            continue;
        };
        let events = strings(job.get("events"));
        let verdict = gating.contains(name);
        let drop = match events.as_deref() {
            None => false,
            Some(_) if verdict => true,
            Some(events) => events == ["manual".to_owned()],
        };
        if drop {
            job.remove("events");
            edits.push(Edit {
                kind: if verdict {
                    "verdict_events"
                } else {
                    "manual_events"
                },
                key: format!("jobs.{name}.events"),
                was: events.map(|events| format!("{events:?}")),
                now: None,
            });
        }
    }
    // A workflow the verdict shares with other jobs: the other jobs get their own.
    let verdict_workflows: BTreeSet<String> = gating
        .iter()
        .filter_map(|name| jobs.get(name)?.get("workflow")?.as_str().map(str::to_owned))
        .collect();
    let taken: BTreeSet<String> = jobs
        .values()
        .filter_map(|job| job.get("workflow")?.as_str().map(str::to_owned))
        .collect();
    let mut moved = Vec::new();
    for (name, job) in jobs.iter_mut() {
        if gating.contains(name) {
            continue;
        }
        let Some(job) = job.as_table_mut() else {
            continue;
        };
        let Some(workflow) = job
            .get("workflow")
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        if verdict_workflows.contains(&workflow) && !taken.contains(name) {
            job.insert("workflow".to_owned(), toml::Value::String(name.clone()));
            edits.push(Edit {
                kind: "own_workflow",
                key: format!("jobs.{name}.workflow"),
                was: Some(workflow),
                now: Some(name.clone()),
            });
            if job.get("events").is_none() {
                moved.push(name.clone());
            }
        }
    }
    if !moved.is_empty() {
        notes.push(format!(
            "jobs {} ran with the verdict on every push; at render_schema = 3 they run by hand unless [verdict] jobs lists them",
            moved.join(", ")
        ));
    }
    if let Some(list) = table.get_mut("verdict").and_then(toml::Value::as_table_mut) {
        list.insert(
            "jobs".to_owned(),
            toml::Value::Array(gating.iter().cloned().map(toml::Value::String).collect()),
        );
    }
    if !aarch64_remote.is_empty() {
        aarch64_evaluation(table, &nix_checks, &gating, &mut edits);
    }
    (edits, decisions, notes)
}

/// The evaluation-only aarch64 check a dropped native aarch64 job leaves behind: the nix check with
/// `mode = "eval"` (declared, or added as `[checks.eval]`) in the first verdict job's checks.
fn aarch64_evaluation(
    table: &mut toml::Table,
    nix_checks: &[(String, Option<String>)],
    gating: &[String],
    edits: &mut Vec<Edit>,
) {
    let Some(verdict) = gating.first() else {
        return;
    };
    let declared = nix_checks
        .iter()
        .find(|(_, mode)| mode.as_deref() == Some("eval"))
        .map(|(name, _)| name.clone());
    let name = match declared {
        Some(name) => name,
        None => {
            let name = ["eval", "aarch64-eval"]
                .into_iter()
                .find(|name| {
                    !table
                        .get("checks")
                        .is_some_and(|checks| checks.get(name).is_some())
                })
                .unwrap_or("aarch64-eval")
                .to_owned();
            let mut check = toml::Table::new();
            check.insert("kind".to_owned(), toml::Value::String("nix".to_owned()));
            check.insert("mode".to_owned(), toml::Value::String("eval".to_owned()));
            let checks = table
                .entry("checks")
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if let Some(checks) = checks.as_table_mut() {
                checks.insert(name.clone(), toml::Value::Table(check));
            }
            edits.push(Edit {
                kind: "aarch64_eval",
                key: format!("checks.{name}"),
                was: None,
                now: Some("kind = \"nix\", mode = \"eval\"".to_owned()),
            });
            name
        }
    };
    let Some(job) = table
        .get_mut("jobs")
        .and_then(|jobs| jobs.get_mut(verdict))
        .and_then(toml::Value::as_table_mut)
    else {
        return;
    };
    let mut listed = strings(job.get("checks")).unwrap_or_default();
    if !listed.contains(&name) {
        listed.push(name.clone());
        job.insert(
            "checks".to_owned(),
            toml::Value::Array(listed.into_iter().map(toml::Value::String).collect()),
        );
        edits.push(Edit {
            kind: "aarch64_eval",
            key: format!("jobs.{verdict}.checks"),
            was: None,
            now: Some(format!("+ {name}")),
        });
    }
}

/// `ccid surface` subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Print every CI file of the repository with its class and what a `render_schema = 3` render does with it.
    Report {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = ".ci/ccid.toml")]
        manifest: PathBuf,
    },
    /// Plan the move to `render_schema = 3` of one or more repositories: the data edits of each manifest, the
    /// decisions a person makes, and what render then writes, deletes, keeps and refuses. Reads only.
    Plan {
        /// A repository work tree; repeatable.
        #[arg(long = "repo", default_value = ".")]
        repos: Vec<PathBuf>,
        #[arg(long, default_value = ".ci/ccid.toml")]
        manifest: PathBuf,
        /// Print only the fleet totals.
        #[arg(long)]
        summary: bool,
    },
}

/// Counts over many plans: repositories and files.
#[derive(Debug, Default, Serialize)]
pub struct Count {
    pub repos: usize,
    pub files: usize,
}

/// `ccid surface plan` over many repositories.
#[derive(Debug, Default, Serialize)]
pub struct Totals {
    pub repos: usize,
    pub ready: usize,
    pub refused: usize,
    pub decisions: usize,
    pub errors: usize,
    /// Edit kind -> repositories.
    pub edits: BTreeMap<&'static str, usize>,
    /// Family (or `rendered`) -> deleted files.
    pub deletes: BTreeMap<String, Count>,
    /// Family (or `unknown`) -> refused files.
    pub refusals: BTreeMap<String, Count>,
    /// Family -> legacy files kept because something still uses them.
    pub kept: BTreeMap<String, Count>,
}

fn family_key(item: &Entry) -> String {
    match (item.family, item.class) {
        (Some(family), Class::Legacy) => family.to_owned(),
        (_, class) => format!("{class:?}").to_lowercase(),
    }
}

fn tally(into: &mut BTreeMap<String, Count>, items: &[Entry]) {
    let mut seen = BTreeSet::new();
    for item in items {
        let key = family_key(item);
        let count = into.entry(key.clone()).or_default();
        count.files += 1;
        if seen.insert(key) {
            count.repos += 1;
        }
    }
}

pub fn totals(plans: &[Plan]) -> Totals {
    let mut totals = Totals {
        repos: plans.len(),
        ..Totals::default()
    };
    for plan in plans {
        totals.ready += usize::from(plan.ready);
        totals.refused += usize::from(!plan.refused.is_empty());
        totals.decisions += usize::from(!plan.decisions.is_empty());
        totals.errors += usize::from(plan.error.is_some());
        let kinds: BTreeSet<&'static str> = plan.edits.iter().map(|edit| edit.kind).collect();
        for kind in kinds {
            *totals.edits.entry(kind).or_default() += 1;
        }
        tally(&mut totals.deletes, &plan.deletes);
        tally(&mut totals.refusals, &plan.refused);
        tally(&mut totals.kept, &plan.kept);
    }
    totals
}

pub fn run(command: &Command) -> Result<u8> {
    match command {
        Command::Report { repo, manifest } => {
            let report = super::surface_report(repo, manifest)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Plan {
            repos,
            manifest,
            summary,
        } => {
            let plans: Vec<Plan> = repos
                .iter()
                .map(|repo| super::surface_plan(repo, manifest))
                .collect();
            let totals = totals(&plans);
            let out = if *summary {
                serde_json::json!({ "totals": totals })
            } else {
                serde_json::json!({ "totals": totals, "plans": plans })
            };
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_segments_and_any_depth() {
        assert!(glob_match(".crow/**", ".crow/a.yaml"));
        assert!(glob_match(".crow/**", ".crow/x/y.yaml"));
        assert!(!glob_match(".crow/**", ".crowd/a.yaml"));
        assert!(glob_match(
            ".github/ISSUE_TEMPLATE/**",
            ".github/ISSUE_TEMPLATE/bug.md"
        ));
        assert!(glob_match(".ci/*.py", ".ci/check.py"));
        assert!(!glob_match(".ci/*.py", ".ci/sub/check.py"));
        assert!(glob_match("src/**/*.rs", "src/main.rs"));
        assert!(glob_match("src/**/*.rs", "src/a/b/main.rs"));
        assert!(glob_match(".ci/check-?.sh", ".ci/check-a.sh"));
        assert!(!glob_match(
            ".github/workflows/ci.yml",
            ".github/workflows/ci.yaml"
        ));
    }

    #[test]
    fn the_table_names_families_never_repositories_and_every_legacy_family_says_what_replaces_it() {
        let mut ids = BTreeSet::new();
        for family in TABLE {
            ids.insert(family.id);
            assert!(!family.patterns.is_empty(), "{}", family.id);
            for pattern in family.patterns {
                assert!(
                    under_roots(pattern.trim_end_matches("/**")) || under_roots(pattern),
                    "{pattern}"
                );
                assert!(!pattern.contains("corbet"), "{pattern}");
            }
            match family.class {
                Class::Legacy => assert!(!family.replacement.is_empty(), "{}", family.id),
                _ => assert!(family.replacement.is_empty() && family.waits_for.is_none()),
            }
        }
        assert!(ids.len() > 10);
    }

    #[test]
    fn content_scoped_families_need_their_text() {
        assert_eq!(
            family(".gitlab-ci.yml", Some(b"x: $CCID_REMOTE_JOB")).map(|f| f.id),
            Some("remote-scheduler")
        );
        assert!(family(".gitlab-ci.yml", Some(b"include: upstream")).is_none());
        assert!(family(".gitlab-ci.yml", None).is_none());
    }

    #[test]
    fn prose_mentions_and_never_uses() {
        for path in [
            "README.md",
            ".ci/README.md",
            "docs/ci.rst",
            "CHANGELOG",
            "LICENSE.md",
            "notes.txt",
        ] {
            assert!(prose(path), "{path}");
        }
        for path in [
            "Makefile",
            ".ci/verify.sh",
            "package.json",
            "flake.nix",
            ".github/workflows/ci.yml",
        ] {
            assert!(!prose(path), "{path}");
        }
    }

    #[test]
    fn references_by_path_directory_or_sibling_name() {
        assert!(!refers(
            ".ci/native-check.sh",
            b"exec native-check.sh",
            ".ci/check.sh"
        ));
        assert!(refers(
            ".ci/verify.sh",
            b"bash \"$(dirname \"$0\")/check.sh\" --all",
            ".ci/check.sh"
        ));
        assert!(refers("Makefile", b"run .ci/verify.sh", ".ci/verify.sh"));
        assert!(refers(
            ".ci/run-cached.sh",
            b"\"$here/toolchain-lock.sh\"",
            ".ci/toolchain-lock.sh"
        ));
        assert!(!refers(
            "Makefile",
            b"toolchain-lock.sh",
            ".ci/toolchain-lock.sh"
        ));
        assert!(refers(
            ".github/check-coverage.py",
            b"RUNTIME = '.github/coverage-runtime'",
            ".github/coverage-runtime/requirements.txt"
        ));
        assert!(!refers(
            "README.md",
            b"see .github/workflows",
            ".github/workflows/ci.yml"
        ));
        assert!(!refers(".ci/verify.sh", b".ci/verify.sh", ".ci/verify.sh"));
    }

    #[test]
    fn surface_config_entries_are_plain_paths_under_the_roots() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".ci")).unwrap();
        fs::write(root.path().join(".ci/publish.py"), "").unwrap();
        let config = |keep: &[(&str, &str)], upstream: &[&str]| Config {
            keep: keep
                .iter()
                .map(|(path, reason)| ((*path).to_owned(), (*reason).to_owned()))
                .collect(),
            upstream: upstream.iter().map(|path| (*path).to_owned()).collect(),
        };
        assert!(validate(
            root.path(),
            &config(
                &[(".ci/publish.py", "the packaging helper")],
                &[".gitlab-ci.yml", ".github/workflows/*"]
            )
        )
        .is_ok());
        for bad in [
            config(&[(".ci/publish.py", " ")], &[]),
            config(&[(".ci/missing.py", "why")], &[]),
            config(&[("src/main.rs", "why")], &[]),
            config(&[("./.ci/publish.py", "why")], &[]),
            config(&[(".ci/../src/x", "why")], &[]),
            config(&[], &["README.md"]),
            config(&[], &["/etc/passwd"]),
        ] {
            assert!(validate(root.path(), &bad).is_err());
        }
    }
}

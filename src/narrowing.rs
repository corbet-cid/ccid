//! Narrowed verdict keys: a check whose read-set was MEASURED is keyed on that read-set.
//!
//! A verdict normally keys the whole committed tree, so a commit that only touches a README
//! reruns the test battery. `ccid measure-inputs` derives the files a check actually touches
//! (see `docs/measure-inputs.md`); a check that carries that measurement's provenance
//! (`cache_inputs_measured`) is keyed on the files its `cache_inputs` select plus the locks and
//! ecosystem manifests, and a commit outside that set reuses its verdict.
//!
//! A false skip is worse than a slow run, so the narrowing is fail-closed. The whole-tree key is
//! used, and the reason is reported, unless every one of these holds:
//!
//! * the check declares `cache_inputs` and provenance from `ccid measure-inputs`;
//! * the provenance is well formed and names a released ccid revision;
//! * the check's definition (commands, kind, options) still hashes to the digest the inputs were
//!   measured against, so an edited check is keyed on the whole tree until it is measured again,
//!   and the `cache_inputs` list is the one that was measured, not one edited by hand since;
//! * the tree's top-level entries are exactly those the measurement classified: an entry the
//!   measurement never saw is an input outside the measured set, seen when the check runs.
//!
//! A key that is narrowed by those conditions alone would trust a measurement made earlier, and a
//! source change can make a check start reading a file the measurement never saw. So the key is
//! only a *claim*, and the run that stores a verdict under it proves the claim on that run: the
//! check executes in a copy of the tree that holds the read-set and nothing else (every file
//! outside it is present but replaced by [`PLACEHOLDER`] bytes, so the tree has the same shape and
//! a read of one is a read of garbage, never a read of a missing file). A pass proves the read-set
//! suffices for the tree being judged; a failure means the measurement is stale, and the check
//! runs again on the whole tree, whose verdict is stored under the whole-tree key with a
//! `remeasure` flag. The key names its proof ([`PROOF`]), so only a verdict recorded by that proof
//! can ever answer a narrowed lookup. A stale read-set measures itself again, once, through the
//! verdict store (`cached_verdict::read_sets`): the measurement a traced run leaves there is what
//! the next proof confirms, and what lookups then take before the manifest's.
//!
//! The key then names the selected files only. It always adds the locks and ecosystem manifests
//! (the same set the moon result cache adds to a narrowed declaration) and everything under
//! `.ci/`, where the check's own scripts live, so a change to the commands' scripts, the
//! dependency resolution or the compiler configuration is never outside the set.
use crate::{failure, Check, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::Write,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

/// Directories that are tool state or build output, never a check input. The same list the
/// whole-tree digest and the moon cache apply to an archive export.
const GENERATED: &[&str] = &[".git", ".moon", ".ccid", "target", "node_modules"];

/// Provenance of a measured `cache_inputs`: what `ccid measure-inputs` prints for the check.
///
/// It is manifest data and never part of any key by itself: whether it is still valid decides
/// whether the key narrows (see the module documentation), and the digests it carries are
/// compared against the repository as it is now.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Measured {
    /// The full source revision of the ccid that measured (a released build, never a local one).
    pub tool: String,
    /// SHA-256 of the check's definition as measured (see [`commands_digest`]).
    pub commands: String,
    /// SHA-256 of the `cache_inputs` list the measurement produced (see [`inputs_digest`]), so a
    /// list edited by hand after the measurement is not taken for the measured one.
    pub inputs: String,
    /// SHA-256 of the tree's top-level entry names as measured (see [`layout_digest`]).
    pub layout: String,
    /// The day of the measurement, `YYYY-MM-DD` (UTC). Provenance for a reader; it never expires.
    pub date: String,
}

/// How the run that stores a narrowed verdict proves its read-set suffices (see the module
/// documentation). It is part of every narrowed key, so a verdict recorded by any other method,
/// or by none, can never answer a narrowed lookup.
pub(crate) const PROOF: &str = "stripped-copy-1";

/// The content of every file outside the read-set in the proving copy: not valid UTF-8, not valid
/// in any file format, and never empty, so a check that reads one fails instead of passing on
/// content it did not have.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) const PLACEHOLDER: &[u8] = b"\0\xffccid: outside the measured inputs\xff\0\n";

/// Files git itself reads for the copy to be a repository; they are not the check's inputs and
/// keep their real content in the proving copy.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn is_plumbing(path: &str) -> bool {
    matches!(
        path.rsplit('/').next(),
        Some(".gitignore" | ".gitattributes" | ".gitmodules")
    )
}

/// The measured read-set as a component of a verdict key. Present only on a narrowed key, so a
/// whole-tree key is exactly what it was before narrowing existed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReadSetKey {
    /// SHA-256 of the sorted, de-duplicated `cache_inputs` globs.
    pub inputs: String,
    /// The measured top-level layout (equal to the current one, or the key would be whole-tree).
    pub layout: String,
    /// The measured check definition (equal to the current one, likewise).
    pub commands: String,
    /// How a run proves, before it stores a verdict under this key, that the read-set suffices
    /// (always [`PROOF`]; a key naming another method is refused).
    pub proof: String,
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `YYYY-MM-DD` with a plausible month and day.
fn is_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let number = |range: std::ops::Range<usize>| -> Option<u32> {
        let text = &value[range];
        text.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| text.parse().ok())
            .flatten()
    };
    matches!(
        (number(0..4), number(5..7), number(8..10)),
        (Some(year), Some(month), Some(day))
            if year >= 2000 && (1..=12).contains(&month) && (1..=31).contains(&day)
    )
}

impl Measured {
    /// Whether the provenance is well formed. A released revision is required: a local build
    /// reports `unversioned`, which no one can reproduce.
    pub(crate) fn validate(&self) -> Result<()> {
        if !is_hex(&self.tool, 40) {
            return Err(failure(
                "cache_inputs_measured.tool must be the full source revision of a released ccid",
            ));
        }
        if !is_hex(&self.commands, 64) || !is_hex(&self.inputs, 64) || !is_hex(&self.layout, 64) {
            return Err(failure(
                "cache_inputs_measured.commands, .inputs and .layout must be sha256 digests",
            ));
        }
        if !is_date(&self.date) {
            return Err(failure("cache_inputs_measured.date must be YYYY-MM-DD"));
        }
        Ok(())
    }
}

/// Structural rules of the two manifest fields, enforced when a check is selected. A stale
/// measurement is not an error here (it is the ordinary state after editing a check); a
/// measurement without inputs, or a malformed one, is a manifest mistake.
pub(crate) fn validate_declaration(check: &Check) -> Result<()> {
    match (&check.cache_inputs, &check.cache_inputs_measured) {
        (_, None) => Ok(()),
        (None, Some(_)) => Err(failure(
            "cache_inputs_measured needs the cache_inputs it was measured for",
        )),
        (Some(_), Some(measured)) => measured.validate(),
    }
}

/// The digest of a check's definition: everything the check declares except the narrowing
/// itself (`cache_inputs`, `cache_inputs_measured`) and the float's resolution variable, which
/// a floated run adds to `cache_env` and which names the resolved locks, not the check.
///
/// `ccid measure-inputs` prints it with the measurement; a key narrows only while it still
/// matches, so an edit to the commands, the kind, the options or the declared environment is
/// keyed on the whole tree until the inputs are measured again.
pub(crate) fn commands_digest(check: &Check) -> Result<String> {
    let mut value = serde_json::to_value(check)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| failure("A check declaration is not an object"))?;
    object.remove("cache_inputs");
    object.remove("cache_inputs_measured");
    if let Some(environment) = object
        .get_mut("cache_env")
        .and_then(serde_json::Value::as_array_mut)
    {
        environment.retain(|name| name.as_str() != Some(crate::float::RESOLUTION_ENV));
    }
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?)))
}

/// The digest of a `cache_inputs` list: its sorted, de-duplicated globs.
pub(crate) fn inputs_digest(globs: &[String]) -> Result<String> {
    let sorted: BTreeSet<&String> = globs.iter().collect();
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&sorted)?)
    ))
}

/// The first path component of every tree entry, minus tool state: the top-level names the
/// repository holds.
fn top_level<'a>(paths: impl IntoIterator<Item = &'a str>) -> BTreeSet<&'a str> {
    paths
        .into_iter()
        .map(|path| path.split('/').next().unwrap_or(path))
        .filter(|name| !GENERATED.contains(name) && *name != "moon.yml")
        .collect()
}

/// The digest of the tree's top-level entry names. A measurement classified every top-level
/// entry that existed as read or not read; a name that appears or disappears afterwards was not
/// classified.
pub(crate) fn layout_digest<'a>(paths: impl IntoIterator<Item = &'a str>) -> String {
    let mut hash = Sha256::new();
    for name in top_level(paths) {
        hash.update(name.as_bytes());
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())
}

/// The revision a measurement made by this build names in its provenance.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn measuring_revision() -> String {
    #[cfg(test)]
    if let Some(revision) = test_revision::get() {
        return revision;
    }
    crate::SOURCE_REVISION.to_owned()
}

/// Whether this build can make a measurement a key accepts: a local build reports
/// `unversioned`, which no key narrows on.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn can_measure() -> bool {
    is_hex(&measuring_revision(), 40)
}

/// Tests build ccid locally (`unversioned`); they name a revision for the measurements they make.
#[cfg(test)]
pub(crate) mod test_revision {
    use std::cell::RefCell;

    thread_local! {
        static REVISION: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub(crate) fn set(revision: Option<&str>) {
        REVISION.with(|cell| *cell.borrow_mut() = revision.map(str::to_owned));
    }

    pub(crate) fn get() -> Option<String> {
        REVISION.with(|cell| cell.borrow().clone())
    }
}

/// The day, `YYYY-MM-DD` in UTC, for the provenance a measurement prints.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn today() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    civil_date(i64::try_from(seconds / 86_400).unwrap_or(0))
}

/// Days since 1970-01-01 as a proleptic Gregorian date (Hinnant's civil-from-days).
#[cfg_attr(not(unix), allow(dead_code))]
fn civil_date(days: i64) -> String {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Locks and tool configuration a narrow declaration cannot leave out, per check kind. This is
/// the moon result cache's list, so both caches narrow around the same dependency resolution and
/// compiler configuration.
fn required(kind: &str) -> &'static [&'static str] {
    match kind {
        "cargo" => &[
            "**/Cargo.toml",
            "**/Cargo.lock",
            "**/.cargo/**",
            "**/rust-toolchain*",
        ],
        "javascript" => &[
            "**/package.json",
            "**/package-lock.json",
            "**/pnpm-lock.yaml",
            "**/bun.lock*",
            "**/deno.lock",
            "**/deno.json*",
            "**/bunfig.toml",
            "**/.npmrc",
            "**/pnpm-workspace.yaml",
        ],
        "commands" => &[
            "**/Cargo.toml",
            "**/Cargo.lock",
            "**/.cargo/**",
            "**/rust-toolchain*",
            "**/go.mod",
            "**/go.sum",
            "**/go.work",
            "**/go.work.sum",
            "**/package.json",
            "**/package-lock.json",
            "**/npm-shrinkwrap.json",
            "**/pnpm-lock.yaml",
            "**/pnpm-workspace.yaml",
            "**/bun.lock*",
            "**/bunfig.toml",
            "**/yarn.lock",
            "**/.yarnrc*",
            "**/.npmrc",
            "**/deno.json*",
            "**/deno.lock",
            "**/tsconfig*.json",
            "**/pyproject.toml",
            "**/uv.lock",
            "**/requirements*.txt",
            "**/poetry.lock",
            "**/Pipfile*",
            "**/.python-version",
            "**/flake.nix",
            "**/flake.lock",
            "**/typst.toml",
            "**/CMakeLists.txt",
            "**/CMakePresets.json",
            "**/conan.lock",
            "**/vcpkg.json",
            "**/vcpkg-configuration.json",
            "**/Dockerfile*",
            "**/.dockerignore",
            "**/docker-bake.*",
            "**/.prototools",
            "**/.tool-versions",
            "**/buf.lock",
            "**/.terraform.lock.hcl",
        ],
        _ => &[],
    }
}

/// The files of a tree that `patterns` select, by git's own pathspec matcher (the one
/// `ccid measure-inputs` verifies with and the moon cache narrows with). The tree is presented
/// to git as a throwaway index, so a checkout, an archive export and a list of paths all
/// answer alike and no object is read.
fn match_paths(paths: &[&str], patterns: &[String]) -> Result<BTreeSet<String>> {
    if patterns.is_empty() || paths.is_empty() {
        return Ok(BTreeSet::new());
    }
    let scratch = tempfile::tempdir()?;
    let git_dir = scratch.path().join("g");
    let index = scratch.path().join("index");
    let git = |arguments: &[&str]| {
        let mut command = Command::new("git");
        command
            .args(["-c", "core.hooksPath=/dev/null"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_DIR", &git_dir)
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(arguments);
        command
    };
    let output = Command::new("git")
        .args(["init", "--quiet", "--bare"])
        .arg(&git_dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .map_err(|error| failure(format!("Narrowing needs git: {error}")))?;
    if !output.status.success() {
        return Err(failure("Narrowing cannot create its scratch index"));
    }
    // Every path is a regular file with a placeholder object id: only the names are matched.
    let mut feed = Vec::new();
    for path in paths {
        feed.extend_from_slice(
            format!(
                "100644 {} 0\t{path}\0",
                "0123456789012345678901234567890123456789"
            )
            .as_bytes(),
        );
    }
    let mut child = git(&["update-index", "--add", "-z", "--index-info"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| failure(format!("Narrowing needs git: {error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| failure("Narrowing cannot feed git"))?
        .write_all(&feed)?;
    let fed = child.wait_with_output()?;
    if !fed.status.success() {
        return Err(failure(format!(
            "Narrowing cannot index the tree: {}",
            String::from_utf8_lossy(&fed.stderr).trim()
        )));
    }
    let listed = git(&["ls-files", "--cached", "-z", "--"])
        .args(patterns)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| failure(format!("Narrowing needs git: {error}")))?;
    if !listed.status.success() {
        return Err(failure(format!(
            "Narrowing cannot match the declared inputs: {}",
            String::from_utf8_lossy(&listed.stderr).trim()
        )));
    }
    Ok(listed
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect())
}

/// How a verdict key covers the tree.
pub(crate) enum Scope {
    /// The whole committed tree. `reason` says why a check that declares `cache_inputs` is not
    /// narrowed; it is `None` for a check that declares none.
    Tree { reason: Option<String> },
    /// Only the measured read-set: `selected` are indices into the (sorted) tree entries.
    ReadSet {
        key: ReadSetKey,
        selected: Vec<usize>,
    },
}

fn tree(reason: impl Into<String>) -> Scope {
    Scope::Tree {
        reason: Some(reason.into()),
    }
}

/// Decide how `check`'s verdict covers a tree whose entries are `paths` (sorted, as the whole-tree
/// digest orders them). Fails closed: anything that is not exactly the measured situation is the
/// whole tree.
pub(crate) fn decide(check: &Check, paths: &[&str]) -> Scope {
    let Some(globs) = check
        .cache_inputs
        .as_ref()
        .filter(|globs| !globs.is_empty())
    else {
        return Scope::Tree { reason: None };
    };
    if check.kind == "nix" {
        return tree("a nix check keys every file");
    }
    let Some(measured) = &check.cache_inputs_measured else {
        return tree("cache_inputs carries no measure-inputs provenance");
    };
    if let Err(error) = measured.validate() {
        return tree(error.to_string());
    }
    let commands = match commands_digest(check) {
        Ok(digest) => digest,
        Err(error) => return tree(error.to_string()),
    };
    if measured.commands != commands {
        return tree("the check changed since its inputs were measured");
    }
    let inputs = match inputs_digest(globs) {
        Ok(digest) => digest,
        Err(error) => return tree(error.to_string()),
    };
    if measured.inputs != inputs {
        return tree("cache_inputs differ from the list that was measured");
    }
    let layout = layout_digest(paths.iter().copied());
    if measured.layout != layout {
        return tree(
            "the top-level entries differ from those the inputs were measured on (an input outside the measured set)",
        );
    }
    let mut patterns: Vec<String> = globs
        .iter()
        .map(|glob| match glob.strip_prefix('!') {
            Some(excluded) => format!(":(glob,exclude){excluded}"),
            None => format!(":(glob){glob}"),
        })
        .collect();
    for generated in [
        ".git/**",
        ".moon/**",
        ".ccid/**",
        "moon.yml",
        "target/**",
        "node_modules/**",
    ] {
        patterns.push(format!(":(glob,exclude){generated}"));
    }
    for output in &check.cache_outputs {
        patterns.push(format!(":(glob,exclude){output}"));
        patterns.push(format!(":(glob,exclude){output}/**"));
    }
    let mut always: Vec<String> = required(&check.kind)
        .iter()
        .map(|glob| format!(":(glob){glob}"))
        .collect();
    // The commands' own scripts and the manifest live here; the measurement always includes it.
    always.push(":(glob).ci/**".to_owned());
    let (declared, forced) = match (match_paths(paths, &patterns), match_paths(paths, &always)) {
        (Ok(declared), Ok(forced)) => (declared, forced),
        (Err(error), _) | (_, Err(error)) => return tree(error.to_string()),
    };
    let chosen: BTreeSet<String> = declared.union(&forced).cloned().collect();
    let selected: Vec<usize> = paths
        .iter()
        .enumerate()
        .filter(|(_, path)| {
            chosen.contains(**path)
                && !path.split('/').any(|part| GENERATED.contains(&part))
                && **path != "moon.yml"
        })
        .map(|(index, _)| index)
        .collect();
    if selected.is_empty() {
        return tree("the declared inputs select no file");
    }
    Scope::ReadSet {
        key: ReadSetKey {
            inputs,
            layout,
            commands,
            proof: PROOF.to_owned(),
        },
        selected,
    }
}

#[cfg(test)]
mod tests;

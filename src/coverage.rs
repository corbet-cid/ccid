//! The coverage gates: one emitted-source gate and one totals gate.
//!
//! The source gate cross-checks three reports from the same LLVM execution
//! (LCOV counters, the raw JSON export, the annotated text report) and
//! requires every emitted production line and branch to be covered, except
//! for manifest-declared exclusions. The totals gate checks exact LLVM line
//! and branch counts per unit, optionally requiring every `src/**/*.rs` file
//! to appear in the report. `ccid coverage run` produces the three reports
//! from one `cargo llvm-cov` execution with the nightly toolchain and a
//! private target directory, then runs the declared gates on them; `gate`
//! and `totals` evaluate reports a repository produced itself.

use crate::{Environment, Result};
use clap::Subcommand;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    str::FromStr,
    sync::atomic::Ordering,
};

fn failure(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::other(message.into()).into()
}

/// Coverage target an exclusion entry applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Native,
    Wasm,
    Browser,
}

impl Target {
    /// The manifest spelling of the target.
    pub fn as_str(self) -> &'static str {
        match self {
            Target::Native => "native",
            Target::Wasm => "wasm",
            Target::Browser => "browser",
        }
    }
}

impl FromStr for Target {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Target, String> {
        match value {
            "native" => Ok(Target::Native),
            "wasm" => Ok(Target::Wasm),
            "browser" => Ok(Target::Browser),
            _ => Err("Unknown coverage target".to_owned()),
        }
    }
}

fn default_target_list() -> Vec<String> {
    vec![Target::Native.as_str().to_owned()]
}

/// One `[[coverage.exclusions]]` entry in `.ci/ccid.toml`.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Exclusion {
    /// Repository-relative production Rust source path.
    pub file: String,
    /// Excluded 1-based source line.
    pub line: i64,
    /// The stripped source line; exactly one of `source` or `context`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The three unstripped lines around `line`; exactly one of `source` or `context`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<String>>,
    /// Why the exclusion exists; never empty.
    pub reason: String,
    /// Optional pointer at the reviewed justification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// Targets this entry applies to; every entry is validated on every run.
    #[serde(default = "default_target_list")]
    pub targets: Vec<String>,
    /// Optional exact branch arm `{ block, id }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<toml::Value>,
    /// Optional map of repository-relative path to sha256 hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_sources: Option<toml::Value>,
    /// Optional map of package name to `{ version, source }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_packages: Option<toml::Value>,
    /// Optional map of THIRD-PARTY crate name to `{ version, source }` (plus the
    /// registry `checksum` when the lock carries one), matched against the
    /// `Cargo.lock` entry. Crates the fleet owns follow newest main and are never
    /// pinned here; the replacement for `evidence_packages` and a whole-lock hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_locked: Option<toml::Value>,
}

/// The `[coverage]` table of `.ci/ccid.toml`.
///
/// Every key but `exclusions` configures `ccid coverage run`; absent keys take
/// the defaults below, which are exactly the arguments of the shared verify
/// scripts.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub exclusions: Vec<Exclusion>,
    /// Arguments of the one `cargo llvm-cov` execution (default [`DEFAULT_LLVM_COV`]).
    #[serde(default)]
    pub llvm_cov: Option<Vec<String>>,
    /// Arguments of both `cargo llvm-cov report` invocations (default [`DEFAULT_REPORT`]).
    #[serde(default)]
    pub report: Option<Vec<String>>,
    /// Extra arguments of the annotated text report only.
    #[serde(default)]
    pub text: Vec<String>,
    /// The gates, in order, that read the run's reports (default `["source"]`).
    #[serde(default)]
    pub gates: Option<Vec<String>>,
    /// The totals gate's `--require-sources`, relative to the repository.
    #[serde(default)]
    pub require_sources: Option<String>,
    /// Copy the reports to `$ARTIFACT_ROOT` after the run, also on failure.
    #[serde(default)]
    pub artifacts: bool,
    /// The wasm execution of the same run: the repository's command that runs the wasm tests
    /// instrumented and writes the three reports; ccid gates them with target `wasm`.
    #[serde(default)]
    pub wasm: Option<Wasm>,
}

/// `[coverage.wasm]`: the second execution of `ccid coverage run`.
///
/// `command` runs in the repository root after the native gates passed, with the private
/// directory `CCID_COVERAGE_OUT` it must fill with `coverage.json`, `coverage.lcov` and
/// `coverage.txt` of the wasm execution (the same three reports as the native one). The same
/// `gates` read them with target `wasm`, so an exclusion with `targets = ["wasm"]` applies here and
/// one with `["native"]` does not. A line that only the wasm tests reach is judged by this
/// execution instead of counting as uncovered natively.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Wasm {
    pub command: Vec<String>,
}

/// Default `[coverage] llvm_cov`.
pub const DEFAULT_LLVM_COV: &[&str] = &[
    "--workspace",
    "--all-features",
    "--branch",
    "--no-cfg-coverage",
    "--no-cfg-coverage-nightly",
    "--ignore-filename-regex",
    "/tests/",
];

/// Default `[coverage] report`.
pub const DEFAULT_REPORT: &[&str] = &["--branch", "--ignore-filename-regex", "/tests/"];

/// The three reports of one run, written into the repository (the check's `cache_outputs`).
pub const REPORTS: [&str; 3] = ["coverage.json", "coverage.lcov", "coverage.txt"];

/// Arguments `ccid coverage run` sets itself: a configured copy would move a
/// report, the build or the lock policy away from the one checked execution.
const OWNED_ARGUMENTS: &[&str] = &[
    "--json",
    "--lcov",
    "--text",
    "--html",
    "--cobertura",
    "--codecov",
    "--output-path",
    "--output-dir",
    "--no-report",
    "--locked",
    "--target-dir",
];

/// One gate of `ccid coverage run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Totals,
    Source,
}

impl Config {
    /// `llvm_cov`, or its default.
    pub fn llvm_cov_arguments(&self) -> Vec<String> {
        self.llvm_cov.clone().unwrap_or_else(|| {
            DEFAULT_LLVM_COV
                .iter()
                .map(|arg| (*arg).to_owned())
                .collect()
        })
    }

    /// `report`, or its default.
    pub fn report_arguments(&self) -> Vec<String> {
        self.report
            .clone()
            .unwrap_or_else(|| DEFAULT_REPORT.iter().map(|arg| (*arg).to_owned()).collect())
    }

    /// The declared gates in order; refuses an empty list, an unknown name and a repetition.
    pub fn gate_order(&self) -> std::result::Result<Vec<Gate>, String> {
        let Some(names) = &self.gates else {
            return Ok(vec![Gate::Source]);
        };
        if names.is_empty() {
            return Err("[coverage] gates must name at least one gate".to_owned());
        }
        let mut order = Vec::new();
        for name in names {
            let gate = match name.as_str() {
                "totals" => Gate::Totals,
                "source" => Gate::Source,
                _ => {
                    return Err(format!(
                        "[coverage] gates: unknown gate {name:?} (known: totals, source)"
                    ))
                }
            };
            if order.contains(&gate) {
                return Err(format!("[coverage] gates: {name:?} is listed twice"));
            }
            order.push(gate);
        }
        Ok(order)
    }

    /// Everything `ccid coverage run` needs from the table, checked before anything runs.
    pub fn validate_run(&self) -> std::result::Result<Vec<Gate>, String> {
        for (key, arguments) in [
            ("llvm_cov", self.llvm_cov_arguments()),
            ("report", self.report_arguments()),
            ("text", self.text.clone()),
        ] {
            for argument in &arguments {
                let name = argument
                    .split_once('=')
                    .map_or(argument.as_str(), |(name, _)| name);
                if OWNED_ARGUMENTS.contains(&name) {
                    return Err(format!(
                        "[coverage] {key}: {argument} is set by ccid coverage run itself"
                    ));
                }
                if argument.contains('\0') {
                    return Err(format!("[coverage] {key}: arguments cannot contain NUL"));
                }
            }
        }
        if let Some(directory) = &self.require_sources {
            let path = Path::new(directory);
            if directory.is_empty()
                || path.is_absolute()
                || path
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
            {
                return Err(
                    "[coverage] require_sources must be a relative path inside the repository"
                        .to_owned(),
                );
            }
        }
        if let Some(wasm) = &self.wasm {
            if wasm.command.first().is_none_or(String::is_empty)
                || wasm.command.iter().any(|arg| arg.contains('\0'))
            {
                return Err("[coverage.wasm] command needs a nonempty executable".to_owned());
            }
        }
        let gates = self.gate_order()?;
        if self.require_sources.is_some() && !gates.contains(&Gate::Totals) {
            return Err("[coverage] require_sources needs the totals gate in gates".to_owned());
        }
        Ok(gates)
    }
}

/// Stdout lines of a passing gate, in order.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub output: Vec<String>,
}

/// Join `path` onto `base` when relative and normalize `.` and `..` lexically.
fn absolutize(path: &Path, base: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

fn resolve_root(root: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    absolutize(root, &cwd)
}

/// Map an absolute path to a repository-relative production Rust source path.
fn production_path(absolute: &Path, root: &Path) -> std::result::Result<String, String> {
    let relative = absolute.strip_prefix(root).map_err(|_| {
        format!(
            "'{}' is not in the subpath of '{}'",
            absolute.display(),
            root.display()
        )
    })?;
    if relative.extension().and_then(|suffix| suffix.to_str()) != Some("rs")
        || relative.components().any(|component| {
            matches!(component, Component::Normal(name) if name == "tests" || name == "target")
        })
    {
        return Err(format!(
            "Unexpected production source: {}",
            relative.display()
        ));
    }
    Ok(relative.to_string_lossy().into_owned())
}

fn source_path(value: &str, root: &Path) -> std::result::Result<String, String> {
    let absolute = absolutize(Path::new(value), root);
    production_path(&absolute, root)
}

fn json_count(value: Option<&serde_json::Value>) -> Option<i64> {
    match value {
        Some(serde_json::Value::Number(count)) => count.as_i64(),
        _ => None,
    }
}

/// One applied exclusion: the entry, its repository-relative file, line and
/// optional exact branch arm.
type Applied<'a> = (&'a Exclusion, String, u64, Option<(u64, u64)>);

/// Validate one exclusion entry against the repository, whatever its targets.
fn validate_exclusion(entry: &Exclusion, root: &Path) -> std::result::Result<(), String> {
    let absolute = absolutize(Path::new(&entry.file), root);
    production_path(&absolute, root)
        .map_err(|_| "Exclusion must identify production Rust source".to_owned())?;
    if entry.line < 1 {
        return Err("Invalid excluded source line".to_owned());
    }
    if entry.source.is_none() == entry.context.is_none() {
        return Err("Exclusion requires exactly one of source or context".to_owned());
    }
    if entry.reason.is_empty() {
        return Err("Exclusion requires a reason".to_owned());
    }
    if entry.targets.is_empty()
        || entry
            .targets
            .iter()
            .any(|target| Target::from_str(target).is_err())
    {
        return Err("Invalid exclusion target".to_owned());
    }
    let text = fs::read_to_string(&absolute).map_err(|error| error.to_string())?;
    let file_lines: Vec<&str> = text.split('\n').collect();
    if let Some(source) = &entry.source {
        let actual = file_lines
            .get(entry.line as usize - 1)
            .map(|row| row.trim().to_owned());
        if actual.as_ref() != Some(source) {
            return Err("Excluded source moved or changed; review it".to_owned());
        }
    }
    if let Some(context) = &entry.context {
        if context.len() != 3 {
            return Err("Invalid exclusion context".to_owned());
        }
        if entry.line < 2 {
            return Err("Invalid excluded source line".to_owned());
        }
        let start = entry.line as usize - 2;
        let actual: Vec<&str> = file_lines
            .iter()
            .skip(start)
            .take(3)
            .map(|row| row.trim_end_matches('\r'))
            .collect();
        // Compare unstripped: split('\n') already removed the newline; a file
        // shorter than the window cannot match.
        if actual.len() != 3
            || actual
                .iter()
                .zip(context.iter())
                .any(|(have, want)| have != want)
        {
            return Err("Excluded source moved or changed; review it".to_owned());
        }
    }
    if let Some(sources) = &entry.evidence_sources {
        let table = sources
            .as_table()
            .ok_or("Invalid evidence source inventory")?;
        for (name, digest) in table {
            let digest = digest.as_str().ok_or("Invalid evidence source inventory")?;
            let dependency = absolutize(Path::new(name), root);
            if dependency.strip_prefix(root).is_err() {
                return Err("Exclusion evidence changed; review it".to_owned());
            }
            let bytes = fs::read(&dependency).map_err(|error| error.to_string())?;
            if format!("{:x}", sha2::Sha256::digest(&bytes)) != digest {
                return Err("Exclusion evidence changed; review it".to_owned());
            }
        }
    }
    if let Some(packages) = &entry.evidence_packages {
        let table = packages
            .as_table()
            .ok_or("Invalid proof dependency inventory")?;
        if !table.is_empty() {
            let locked =
                fs::read_to_string(root.join("Cargo.lock")).map_err(|error| error.to_string())?;
            let parsed: toml::Value = toml::from_str(&locked).map_err(|error| error.to_string())?;
            let locked_packages = parsed
                .get("package")
                .and_then(|packages| packages.as_array())
                .ok_or("Invalid Cargo.lock package inventory")?;
            let exact =
                Regex::new(r"^git\+.+#[0-9a-f]{40}$").expect("fixed proof identity pattern");
            for (name, identity) in table {
                let identity = identity
                    .as_table()
                    .ok_or("Proof dependency requires an exact locked Git identity")?;
                if name.is_empty()
                    || identity.len() != 2
                    || identity
                        .get("version")
                        .and_then(|v| v.as_str())
                        .is_none_or(|v| v.is_empty())
                    || identity
                        .get("source")
                        .and_then(|v| v.as_str())
                        .is_none_or(|v| v.is_empty())
                    || !exact.is_match(identity["source"].as_str().unwrap_or(""))
                {
                    return Err("Proof dependency requires an exact locked Git identity".to_owned());
                }
                let version = identity["version"].as_str().unwrap_or("");
                let source = identity["source"].as_str().unwrap_or("");
                let matches = locked_packages
                    .iter()
                    .filter(|package| {
                        package.get("name").and_then(|n| n.as_str()) == Some(name.as_str())
                    })
                    .collect::<Vec<_>>();
                if matches.len() != 1
                    || matches[0].get("version").and_then(|v| v.as_str()) != Some(version)
                    || matches[0].get("source").and_then(|v| v.as_str()) != Some(source)
                {
                    return Err("Proof dependency changed or is not unique; review it".to_owned());
                }
            }
        }
    }
    if let Some(locked) = &entry.evidence_locked {
        validate_evidence_locked(locked, root)?;
    }
    if let Some(branch) = &entry.branch {
        let table = branch
            .as_table()
            .filter(|table| {
                table.len() == 2 && table.contains_key("block") && table.contains_key("id")
            })
            .ok_or("Exclusion requires one exact branch arm")?;
        for key in ["block", "id"] {
            match table.get(key) {
                Some(toml::Value::Integer(value)) if *value >= 0 => {}
                _ => return Err("Invalid excluded branch coordinate".to_owned()),
            }
        }
        let pinned = entry
            .evidence_sources
            .as_ref()
            .and_then(|sources| sources.as_table())
            .is_some_and(|table| table.contains_key(&entry.file));
        if !pinned {
            return Err("Branch exception requires complete source hash".to_owned());
        }
    }
    Ok(())
}

/// Third-party lock evidence: each named crate must have a `Cargo.lock` entry with
/// exactly the pinned version, source and (when pinned) checksum. A crate the fleet
/// owns (a Git source: owned crates follow newest main) is refused, so a lock
/// refresh that only moves owned crates never invalidates the exclusion.
fn validate_evidence_locked(locked: &toml::Value, root: &Path) -> std::result::Result<(), String> {
    let table = locked
        .as_table()
        .ok_or("Invalid locked dependency inventory")?;
    if table.is_empty() {
        return Ok(());
    }
    let text = fs::read_to_string(root.join("Cargo.lock")).map_err(|error| error.to_string())?;
    let parsed: toml::Value = toml::from_str(&text).map_err(|error| error.to_string())?;
    let packages = parsed
        .get("package")
        .and_then(|packages| packages.as_array())
        .ok_or("Invalid Cargo.lock package inventory")?;
    for (name, pin) in table {
        let pin = pin
            .as_table()
            .filter(|pin| {
                !name.is_empty()
                    && pin
                        .keys()
                        .all(|key| matches!(key.as_str(), "version" | "source" | "checksum"))
                    && ["version", "source"].iter().all(|key| {
                        pin.get(*key)
                            .and_then(|v| v.as_str())
                            .is_some_and(|v| !v.is_empty())
                    })
            })
            .ok_or("Locked dependency requires a version and a source")?;
        let source = pin["source"].as_str().unwrap_or("");
        if source.starts_with("git+") {
            return Err(format!(
                "Locked dependency {name} is a Git (owned) crate; pin your own source files instead"
            ));
        }
        let version = pin["version"].as_str().unwrap_or("");
        let pinned_checksum = pin.get("checksum").and_then(|v| v.as_str());
        let found = packages.iter().any(|package| {
            package.get("name").and_then(|v| v.as_str()) == Some(name.as_str())
                && package.get("version").and_then(|v| v.as_str()) == Some(version)
                && package.get("source").and_then(|v| v.as_str()) == Some(source)
                && pinned_checksum
                    .is_none_or(|sum| package.get("checksum").and_then(|v| v.as_str()) == Some(sum))
        });
        if !found {
            return Err("Locked dependency changed or is missing; review it".to_owned());
        }
    }
    Ok(())
}

struct RawFile {
    /// The branch locations LCOV must carry two arms for (see [`emitted_branches`]).
    branches: Vec<[i64; 4]>,
    /// File-level locations with a nonzero or unreadable execution count.
    counted: BTreeSet<[i64; 4]>,
    /// File-level locations the function records omit (folded regions); each
    /// must start on a line LCOV reports unexecuted.
    folded: BTreeSet<[i64; 4]>,
    lines_count: Option<i64>,
    lines_covered: Option<i64>,
    branches_count: Option<i64>,
    branches_covered: Option<i64>,
}

/// `[line, col, end_line, end_col]` of one raw JSON branch record.
fn branch_location(branch: &serde_json::Value) -> std::result::Result<[i64; 4], String> {
    let slots = branch
        .as_array()
        .ok_or("Missing emitted branch locations")?;
    if slots.len() < 4 {
        return Err("Missing emitted branch locations".to_owned());
    }
    let mut location = [0i64; 4];
    for (index, slot) in slots.iter().take(4).enumerate() {
        location[index] = slot.as_i64().ok_or("Missing emitted branch locations")?;
    }
    Ok(location)
}

fn raw_file(item: &serde_json::Value) -> std::result::Result<RawFile, String> {
    let branches = item
        .get("branches")
        .and_then(|branches| branches.as_array())
        .ok_or("Missing emitted branch locations")?;
    let mut locations = Vec::new();
    let mut counted = BTreeSet::new();
    for branch in branches {
        let location = branch_location(branch)?;
        let unexecuted = [4, 5].iter().all(|index| {
            branch
                .as_array()
                .and_then(|slots| slots.get(*index))
                .and_then(|count| count.as_i64())
                == Some(0)
        });
        if !unexecuted {
            counted.insert(location);
        }
        locations.push(location);
    }
    let summary = item.get("summary").ok_or("Invalid raw JSON summary")?;
    let pick =
        |metric: &str, field: &str| json_count(summary.get(metric).and_then(|m| m.get(field)));
    Ok(RawFile {
        branches: locations,
        counted,
        folded: BTreeSet::new(),
        lines_count: pick("lines", "count"),
        lines_covered: pick("lines", "covered"),
        branches_count: pick("branches", "count"),
        branches_covered: pick("branches", "covered"),
    })
}

/// The branch inventory LCOV is held to, from a unit's function records.
///
/// LLVM's JSON exporter lists a file's branch regions twice: once per file
/// (`files[].branches`, every region of every function record) and once per
/// function (`functions[].branches`). The per-function lists and the LCOV
/// exporter drop the same regions, those whose two counters are both folded to
/// the constant zero; the per-file list keeps them. Folded regions are no real
/// branch; rustc emits them, for example, for every branch of a function it
/// instrumented but never generated code for (an uninstantiated generic).
/// When a unit carries function records, each production file's inventory is
/// therefore the per-function one, on three conditions that keep a dropped
/// branch from passing: every per-function location is also a per-file
/// location; every per-file location the function records omit was never
/// executed (both counts zero, as a folded region's are); and, checked in
/// [`gate`] once LCOV is read, every omitted location starts on a line LCOV
/// reports with count zero. The last condition keeps the line gate in charge
/// of the omitted code: an uninstantiated function's lines are uncovered and
/// refused unless a reviewed exclusion names them, while a folded region on an
/// executed line (a never-called closure beside executed code) still refuses
/// as a missing branch. A unit without function records keeps the per-file
/// inventory.
fn emitted_branches(
    functions: &serde_json::Value,
    files: &BTreeMap<PathBuf, String>,
    root: &Path,
    expected: &mut BTreeMap<String, RawFile>,
) -> std::result::Result<(), String> {
    let functions = functions
        .as_array()
        .ok_or("Missing emitted branch locations")?;
    let mut emitted: BTreeMap<String, BTreeSet<[i64; 4]>> = BTreeMap::new();
    for function in functions {
        let filenames = function
            .get("filenames")
            .and_then(|names| names.as_array())
            .ok_or("Missing emitted branch locations")?;
        let branches = function
            .get("branches")
            .and_then(|branches| branches.as_array())
            .ok_or("Missing emitted branch locations")?;
        for branch in branches {
            let location = branch_location(branch)?;
            let name = branch
                .as_array()
                .and_then(|slots| slots.get(6))
                .and_then(|file| file.as_u64())
                .and_then(|file| filenames.get(usize::try_from(file).ok()?))
                .and_then(|name| name.as_str())
                .ok_or("Missing emitted branch locations")?;
            // Function records also name files the export filtered out (tests, dependencies).
            if let Some(file) = files.get(&absolutize(Path::new(name), root)) {
                emitted.entry(file.clone()).or_default().insert(location);
            }
        }
    }
    for file in files.values() {
        let raw = expected.get_mut(file).expect("unit file inventory");
        let inventory = emitted.remove(file).unwrap_or_default();
        let listed: BTreeSet<[i64; 4]> = raw.branches.iter().copied().collect();
        if !inventory.is_subset(&listed)
            || listed
                .difference(&inventory)
                .any(|location| raw.counted.contains(location))
        {
            return Err("Missing emitted branch locations".to_owned());
        }
        raw.folded = listed.difference(&inventory).copied().collect();
        raw.branches = inventory.into_iter().collect();
    }
    Ok(())
}

/// Gate emitted production lines and branches from one LLVM execution.
///
/// Pure: reads repository files for exclusion evidence but no process
/// environment. The `Err` message is the text after `Coverage gate: `.
pub fn gate(
    lcov: &str,
    raw_json: &serde_json::Value,
    annotated: &str,
    root: &Path,
    exclusions: &[Exclusion],
    target: Target,
) -> std::result::Result<Report, String> {
    let root = resolve_root(root);
    if raw_json.get("type").and_then(|kind| kind.as_str()) != Some("llvm.coverage.json.export")
        || raw_json
            .get("data")
            .and_then(|data| data.as_array())
            .is_none_or(|data| data.is_empty())
    {
        return Err("Missing companion raw LLVM JSON".to_owned());
    }
    let mut expected: BTreeMap<String, RawFile> = BTreeMap::new();
    for unit in raw_json["data"].as_array().expect("checked data array") {
        let items = unit.get("files").and_then(|files| files.as_array());
        let Some(items) = items else {
            return Err("Missing companion raw LLVM JSON".to_owned());
        };
        let mut unit_files: BTreeMap<PathBuf, String> = BTreeMap::new();
        for item in items {
            let filename = item.get("filename").and_then(|name| name.as_str());
            let Some(filename) = filename else {
                return Err("Missing companion raw LLVM JSON".to_owned());
            };
            let path = source_path(filename, &root)?;
            if expected.contains_key(&path) {
                return Err("Duplicate raw source".to_owned());
            }
            unit_files.insert(absolutize(Path::new(filename), &root), path.clone());
            expected.insert(path, raw_file(item)?);
        }
        if let Some(functions) = unit.get("functions") {
            emitted_branches(functions, &unit_files, &root, &mut expected)?;
        }
    }
    if expected.is_empty() {
        return Err("Empty production file inventory".to_owned());
    }

    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut lines: BTreeMap<(String, u64), u64> = BTreeMap::new();
    let mut branches: BTreeMap<(String, u64, u64, u64), u64> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut summaries: BTreeMap<String, i64> = BTreeMap::new();
    let finish = |current: &mut Option<String>,
                  summaries: &BTreeMap<String, i64>,
                  branches: &BTreeMap<(String, u64, u64, u64), u64>,
                  expected: &BTreeMap<String, RawFile>|
     -> std::result::Result<(), String> {
        let Some(file) = current.clone() else {
            return Err("Unexpected end of record".to_owned());
        };
        let raw = expected
            .get(&file)
            .ok_or("Incomplete or empty source coverage inventory")?;
        let local: Vec<u64> = branches
            .iter()
            .filter(|((path, _, _, _), _)| path == &file)
            .map(|(_, count)| *count)
            .collect();
        let mut distinct: BTreeMap<i64, usize> = BTreeMap::new();
        let mut seen: BTreeSet<[i64; 4]> = BTreeSet::new();
        for location in &raw.branches {
            if seen.insert(*location) {
                *distinct.entry(location[0]).or_default() += 1;
            }
        }
        let mut actual: BTreeMap<i64, usize> = BTreeMap::new();
        for ((_, line, _, _), _) in branches.iter().filter(|((path, _, _, _), _)| path == &file) {
            *actual.entry(*line as i64).or_default() += 1;
        }
        let doubled: BTreeMap<i64, usize> = distinct
            .iter()
            .map(|(line, count)| (*line, 2 * count))
            .collect();
        if actual != doubled {
            return Err("Missing emitted branch locations".to_owned());
        }
        for (key, field) in [
            ("LF", raw.lines_count),
            ("LH", raw.lines_covered),
            ("BRF", raw.branches_count),
            ("BRH", raw.branches_covered),
        ] {
            let Some(count) = field else {
                return Err("Invalid raw JSON summary".to_owned());
            };
            if count < 0 {
                return Err("Invalid raw JSON summary".to_owned());
            }
            if key.starts_with("BR")
                && local.is_empty()
                && count == 0
                && !summaries.contains_key(key)
            {
                continue;
            }
            if summaries.get(key) != Some(&count) {
                return Err(format!("Inconsistent {key} for {file}"));
            }
        }
        *current = None;
        Ok(())
    };
    for record in lcov.split('\n') {
        if record.is_empty() {
            continue;
        }
        if let Some(path) = record.strip_prefix("SF:") {
            if current.is_some() {
                return Err("Unterminated source record".to_owned());
            }
            let file = source_path(path, &root)?;
            if !files.insert(file.clone()) {
                return Err("Duplicate source record".to_owned());
            }
            current = Some(file);
            summaries = BTreeMap::new();
        } else if record == "end_of_record" {
            finish(&mut current, &summaries, &branches, &expected)?;
        } else if let Some(rest) = record.strip_prefix("DA:") {
            let mut parts = rest.split(',');
            let number = parts.next().ok_or("Invalid or duplicate line record")?;
            let count = parts.next().ok_or("Invalid or duplicate line record")?;
            let number: i64 = number
                .parse()
                .map_err(|_| "Invalid or duplicate line record")?;
            let count: i64 = count
                .parse()
                .map_err(|_| "Invalid or duplicate line record")?;
            let Some(file) = current.clone() else {
                return Err("Invalid or duplicate line record".to_owned());
            };
            if number <= 0 || count < 0 || lines.contains_key(&(file.clone(), number as u64)) {
                return Err("Invalid or duplicate line record".to_owned());
            }
            lines.insert((file, number as u64), count as u64);
        } else if let Some(rest) = record.strip_prefix("BRDA:") {
            let parts: Vec<&str> = rest.split(',').collect();
            if parts.len() != 4 {
                return Err("Invalid or duplicate branch record".to_owned());
            }
            let number: i64 = parts[0]
                .parse()
                .map_err(|_| "Invalid or duplicate branch record")?;
            let block: i64 = parts[1]
                .parse()
                .map_err(|_| "Invalid or duplicate branch record")?;
            let id: i64 = parts[2]
                .parse()
                .map_err(|_| "Invalid or duplicate branch record")?;
            let Some(file) = current.clone() else {
                return Err("Invalid or duplicate branch record".to_owned());
            };
            if number <= 0 || block < 0 || id < 0 {
                return Err("Invalid or duplicate branch record".to_owned());
            }
            let key = (file, number as u64, block as u64, id as u64);
            if branches.contains_key(&key) {
                return Err("Invalid or duplicate branch record".to_owned());
            }
            let value = if parts[3] == "-" {
                0u64
            } else {
                parts[3]
                    .parse::<i64>()
                    .map_err(|_| "Invalid or duplicate branch record")
                    .and_then(|count| {
                        if count < 0 {
                            Err("Negative branch count")
                        } else {
                            Ok(count as u64)
                        }
                    })
                    .map_err(str::to_owned)?
            };
            branches.insert(key, value);
        } else if ["LF", "LH", "BRF", "BRH"].contains(&record.split(':').next().unwrap_or("")) {
            let (key, value) = record.split_once(':').expect("checked summary shape");
            let value: i64 = value.parse().map_err(|_| "Invalid or duplicate summary")?;
            if current.is_none() || summaries.contains_key(key) || value < 0 {
                return Err("Invalid or duplicate summary".to_owned());
            }
            summaries.insert(key.to_owned(), value);
        } else if !record.starts_with("TN:")
            && !record.starts_with("FN:")
            && !record.starts_with("FNDA:")
            && !record.starts_with("FNF:")
            && !record.starts_with("FNH:")
        {
            return Err(format!("Unexpected LCOV record: {record}"));
        }
    }
    if current.is_some()
        || files != expected.keys().cloned().collect::<BTreeSet<_>>()
        || lines.is_empty()
    {
        return Err("Incomplete or empty source coverage inventory".to_owned());
    }
    // A folded region leaves the branch inventory only where the line gate
    // already sees its line unexecuted (see `emitted_branches`).
    for (file, raw) in &expected {
        if raw.folded.iter().any(|location| {
            u64::try_from(location[0])
                .ok()
                .and_then(|line| lines.get(&(file.clone(), line)))
                != Some(&0)
        }) {
            return Err("Missing emitted branch locations".to_owned());
        }
    }

    let mut annotated_lines: BTreeMap<(String, u64), bool> = BTreeMap::new();
    let mut annotated_files: BTreeSet<String> = BTreeSet::new();
    current = None;
    let has_heading = annotated
        .split('\n')
        .any(|row| row.starts_with('/') && row.ends_with(".rs:"));
    if files.len() == 1 && !has_heading {
        let sole = files.iter().next().expect("exactly one file").clone();
        current = Some(sole.clone());
        annotated_files.insert(sole);
    }
    let pattern = Regex::new(r"^\s*(\d+)\|\s*([0-9]+(?:\.[0-9]+)?[kMGT]?)\|")
        .expect("fixed annotated pattern");
    for record in annotated.split('\n') {
        if record.starts_with('/') && record.ends_with(".rs:") {
            let file = source_path(&record[..record.len() - 1], &root)?;
            if !annotated_files.insert(file.clone()) {
                return Err("Duplicate annotated source".to_owned());
            }
            current = Some(file);
        }
        if let Some(hit) = pattern.captures(record) {
            let number: u64 = hit[1]
                .parse()
                .map_err(|_| "Invalid annotated line inventory")?;
            let Some(file) = current.clone() else {
                return Err("Invalid annotated line inventory".to_owned());
            };
            if annotated_lines.contains_key(&(file.clone(), number)) {
                return Err("Invalid annotated line inventory".to_owned());
            }
            let magnitude: f64 = hit[2]
                .trim_end_matches(['k', 'M', 'G', 'T'])
                .parse()
                .map_err(|_| "Invalid annotated line inventory")?;
            annotated_lines.insert((file, number), magnitude > 0.0);
        }
    }
    if annotated_files != files
        || annotated_lines.keys().cloned().collect::<BTreeSet<_>>()
            != lines.keys().cloned().collect::<BTreeSet<_>>()
    {
        return Err("Incomplete emitted line inventory".to_owned());
    }
    if annotated_lines
        .iter()
        .any(|(key, hit)| (lines[key] > 0) != *hit)
    {
        return Err("Inconsistent emitted line coverage".to_owned());
    }

    for entry in exclusions {
        validate_exclusion(entry, &root)?;
    }
    let mut applied: Vec<Applied<'_>> = Vec::new();
    for entry in exclusions {
        if entry.targets.iter().any(|name| name == target.as_str()) {
            let branch = entry.branch.as_ref().map(|value| {
                let table = value.as_table().expect("validated branch arm");
                (
                    table["block"].as_integer().unwrap_or(0) as u64,
                    table["id"].as_integer().unwrap_or(0) as u64,
                )
            });
            let absolute = absolutize(Path::new(&entry.file), &root);
            let file = production_path(&absolute, &root)
                .map_err(|_| "Exclusion must identify production Rust source".to_owned())?;
            applied.push((entry, file, entry.line as u64, branch));
        }
    }
    let mut allowed: BTreeMap<(String, u64), &Exclusion> = BTreeMap::new();
    let mut allowed_branches: BTreeMap<(String, u64, u64, u64), &Exclusion> = BTreeMap::new();
    for (entry, file, line, branch) in &applied {
        if let Some((block, id)) = branch {
            if allowed_branches
                .insert((file.clone(), *line, *block, *id), *entry)
                .is_some()
            {
                return Err("Duplicate branch exclusion".to_owned());
            }
        } else if allowed.insert((file.clone(), *line), *entry).is_some() {
            return Err("Duplicate line exclusion".to_owned());
        }
    }

    if !allowed.keys().all(|key| lines.contains_key(key)) {
        return Err("Exclusion absent from actual LLVM evidence".to_owned());
    }
    if allowed.keys().any(|key| lines[key] != 0) {
        return Err("Excluded line exercised; remove its exception".to_owned());
    }
    for key in branches.keys() {
        if allowed.contains_key(&(key.0.clone(), key.1)) && !allowed_branches.contains_key(key) {
            return Err("Line exception requires every contained branch arm".to_owned());
        }
    }
    if !allowed_branches
        .keys()
        .all(|key| branches.contains_key(key))
    {
        return Err("Branch exclusion absent from actual LLVM evidence".to_owned());
    }
    if allowed_branches.keys().any(|key| branches[key] != 0) {
        return Err("Excluded branch exercised; remove its exception".to_owned());
    }
    let mut report = Report::default();
    for ((file, line), entry) in &allowed {
        report
            .output
            .push(format!("Excluded line: {file}:{line} {}", entry.reason));
        lines.remove(&(file.clone(), *line));
    }
    for ((file, line, block, id), entry) in &allowed_branches {
        report.output.push(format!(
            "Excluded branch: {file}:{line}:{block}:{id} {}",
            entry.reason
        ));
        branches.remove(&(file.clone(), *line, *block, *id));
    }
    let missing_lines: Vec<String> = lines
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|((file, line), _)| format!("('{file}', {line})"))
        .collect();
    let missing_branches: Vec<String> = branches
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|((file, line, block, id), _)| format!("('{file}', {line}, {block}, {id})"))
        .collect();
    let covered_lines = lines.len() - missing_lines.len();
    let covered_branches = branches.len() - missing_branches.len();
    if exclusions.is_empty() {
        report
            .output
            .push(format!("lines: {covered_lines}/{}", lines.len()));
    } else {
        report.output.push(format!(
            "lines: {covered_lines}/{}; exclusions: {}",
            lines.len(),
            applied.len()
        ));
    }
    report
        .output
        .push(format!("branches: {covered_branches}/{}", branches.len()));
    if !missing_lines.is_empty() || !missing_branches.is_empty() {
        return Err(format!(
            "Uncovered source lines: [{}]; branches: [{}]",
            missing_lines.join(", "),
            missing_branches.join(", ")
        ));
    }
    Ok(report)
}

/// Gate exact LLVM line and branch counts, without rounded percentages.
///
/// With `require_sources`, every `**/*.rs` below that directory (resolved
/// against the current directory) must appear among the report's file names.
/// Each returned string is one stdout line.
pub fn totals(
    raw_json: &serde_json::Value,
    require_sources: Option<&Path>,
) -> std::result::Result<Vec<String>, String> {
    if raw_json.get("type").and_then(|kind| kind.as_str()) != Some("llvm.coverage.json.export")
        || raw_json
            .get("data")
            .and_then(|data| data.as_array())
            .is_none_or(|data| data.is_empty())
    {
        return Err("Expected a nonempty LLVM coverage export".to_owned());
    }
    if let Some(directory) = require_sources {
        let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
        let root = absolutize(directory, &cwd);
        let mut wanted: BTreeSet<String> = BTreeSet::new();
        if root.is_dir() {
            collect_sources(&root, &root, &mut wanted)?;
        }
        let mut observed: BTreeSet<String> = BTreeSet::new();
        for unit in raw_json["data"].as_array().expect("checked data array") {
            let files = unit.get("files").and_then(|files| files.as_array());
            let Some(files) = files else {
                return Err("Expected a nonempty LLVM coverage export".to_owned());
            };
            for file in files {
                let Some(name) = file.get("filename").and_then(|name| name.as_str()) else {
                    return Err("Expected a nonempty LLVM coverage export".to_owned());
                };
                observed.insert(
                    absolutize(Path::new(name), &cwd)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        let missing: Vec<String> = wanted.difference(&observed).cloned().collect();
        if !missing.is_empty() {
            return Err(format!(
                "Missing production sources: {}",
                missing.join(", ")
            ));
        }
    }
    let mut output = Vec::new();
    let mut failures = Vec::new();
    let mut lines = 0i64;
    for unit in raw_json["data"].as_array().expect("checked data array") {
        let unit_totals = unit.get("totals");
        for metric in ["lines", "branches"] {
            let value = unit_totals.and_then(|totals| totals.get(metric));
            let count = value
                .and_then(|value| value.get("count"))
                .and_then(|count| json_count(Some(count)));
            let covered = value
                .and_then(|value| value.get("covered"))
                .and_then(|covered| json_count(Some(covered)));
            let (Some(count), Some(covered)) = (count, covered) else {
                return Err(format!("Invalid {metric} counts"));
            };
            if !(0 <= covered && covered <= count) {
                return Err(format!("Invalid {metric} counts"));
            }
            if metric == "lines" {
                lines += count;
            }
            output.push(format!("{metric}: {covered}/{count}"));
            if covered != count {
                failures.push(format!("{metric}: {} uncovered", count - covered));
            }
        }
    }
    if lines == 0 {
        return Err("Empty line coverage cannot pass".to_owned());
    }
    if !failures.is_empty() {
        return Err(failures.join("; "));
    }
    Ok(output)
}

fn collect_sources(
    root: &Path,
    directory: &Path,
    out: &mut BTreeSet<String>,
) -> std::result::Result<(), String> {
    let entries = fs::read_dir(directory).map_err(|error| error.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let is_link = entry
            .file_type()
            .map(|kind| kind.is_symlink())
            .unwrap_or(false);
        if path.is_dir() && !is_link {
            collect_sources(root, &path, out)?;
        } else if path.extension().and_then(|suffix| suffix.to_str()) == Some("rs") {
            out.insert(absolutize(&path, root).to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// Move a JSON value into TOML. Legacy exclusion files only carry strings,
/// integers, arrays and tables; anything else is a shape error.
fn json_to_toml(value: &serde_json::Value) -> Option<toml::Value> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(value) => Some(toml::Value::Boolean(*value)),
        serde_json::Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Some(toml::Value::Integer(value))
            } else if let Some(value) = value.as_u64() {
                value.try_into().ok().map(toml::Value::Integer)
            } else {
                value.as_f64().map(toml::Value::Float)
            }
        }
        serde_json::Value::String(value) => Some(toml::Value::String(value.clone())),
        serde_json::Value::Array(values) => values
            .iter()
            .map(json_to_toml)
            .collect::<Option<Vec<_>>>()
            .map(toml::Value::Array),
        serde_json::Value::Object(entries) => entries
            .iter()
            .map(|(key, value)| json_to_toml(value).map(|value| (key.clone(), value)))
            .collect::<Option<toml::map::Map<_, _>>>()
            .map(toml::Value::Table),
    }
}

/// Convert a legacy coverage-exclusions JSON list to `[[coverage.exclusions]]`
/// TOML.
///
/// `default_targets` applies to entries without `target`/`targets`. Entries
/// keep their input order; fields follow the manifest schema order. Anything
/// that matches no legacy shape is an error naming the entry index.
pub fn convert_exclusions(
    legacy_json: &str,
    default_targets: &[Target],
) -> std::result::Result<String, String> {
    let parsed: serde_json::Value = serde_json::from_str(legacy_json)
        .map_err(|error| format!("Invalid legacy exclusions: {error}"))?;
    let entries = parsed
        .as_array()
        .ok_or("Invalid legacy exclusions: expected an array")?;
    let mut converted: Vec<Exclusion> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let invalid = |detail: &str| format!("Invalid legacy exclusion at index {index}: {detail}");
        let table = entry.as_object().ok_or(invalid("expected an object"))?;
        let line = table
            .get("line")
            .and_then(|line| line.as_i64())
            .ok_or(invalid("entry requires an integer line"))?;
        let reason = table
            .get("reason")
            .and_then(|reason| reason.as_str())
            .map(str::to_owned);
        let evidence = table
            .get("evidence")
            .and_then(|evidence| evidence.as_str())
            .map(str::to_owned);
        if table.get("evidence").is_some() && evidence.is_none() {
            return Err(invalid("evidence must be a string"));
        }
        let mut targets: Option<Vec<String>> = None;
        if let Some(value) = table.get("targets") {
            let names = value
                .as_array()
                .ok_or(invalid("targets must be a string array"))?;
            let mut kept = Vec::new();
            for name in names {
                let name = name
                    .as_str()
                    .ok_or(invalid("targets must be a string array"))?;
                Target::from_str(name).map_err(|_| invalid("unknown coverage target"))?;
                kept.push(name.to_owned());
            }
            targets = Some(kept);
        } else if let Some(value) = table.get("target") {
            let name = value.as_str().ok_or(invalid("target must be a string"))?;
            Target::from_str(name).map_err(|_| invalid("unknown coverage target"))?;
            targets = Some(vec![name.to_owned()]);
        }
        let branch = table
            .get("branch")
            .map(|branch| {
                let table = branch
                    .as_object()
                    .ok_or(invalid("branch must be { block, id }"))?;
                if table.len() != 2 || !table.contains_key("block") || !table.contains_key("id") {
                    return Err(invalid("branch must be { block, id }"));
                }
                for key in ["block", "id"] {
                    if table[key].as_i64().is_none() {
                        return Err(invalid("branch coordinates must be integers"));
                    }
                }
                json_to_toml(branch).ok_or(invalid("branch must convert to TOML"))
            })
            .transpose()?;
        let carried = |key: &str| -> std::result::Result<Option<toml::Value>, String> {
            table
                .get(key)
                .map(|value| {
                    if !value.is_object() {
                        return Err(invalid(&format!("{key} must be an object")));
                    }
                    json_to_toml(value).ok_or(invalid(&format!("{key} must convert to TOML")))
                })
                .transpose()
        };
        let evidence_sources = carried("evidence_sources")?;
        let evidence_packages = carried("evidence_packages")?;
        let evidence_locked = carried("evidence_locked")?;
        let (file, source, context) =
            if let Some(name) = table.get("file").and_then(|file| file.as_str()) {
                let source = table
                    .get("source")
                    .and_then(|source| source.as_str())
                    .map(str::to_owned);
                if table.get("source").is_some() && source.is_none() {
                    return Err(invalid("source must be a string"));
                }
                let source = source.ok_or(invalid("file entries require source"))?;
                (name.to_owned(), Some(source), None)
            } else if table.contains_key("path") && table.contains_key("context") {
                let name = table["path"]
                    .as_str()
                    .ok_or(invalid("path must be a string"))?;
                let rows = table["context"]
                    .as_array()
                    .ok_or(invalid("context must be a string array"))?;
                let mut kept = Vec::new();
                for row in rows {
                    kept.push(
                        row.as_str()
                            .ok_or(invalid("context must be a string array"))?
                            .to_owned(),
                    );
                }
                (name.to_owned(), None, Some(kept))
            } else if table.contains_key("path") && table.contains_key("source") {
                let name = table["path"]
                    .as_str()
                    .ok_or(invalid("path must be a string"))?;
                let source = table["source"]
                    .as_str()
                    .ok_or(invalid("source must be a string"))?;
                (name.to_owned(), Some(source.to_owned()), None)
            } else {
                return Err(invalid("entry has no file or path shape"));
            };
        converted.push(Exclusion {
            file,
            line,
            source,
            context,
            reason: reason.ok_or(invalid("entry requires a reason"))?,
            evidence,
            targets: targets.unwrap_or_else(|| {
                default_targets
                    .iter()
                    .map(|target| target.as_str().to_owned())
                    .collect()
            }),
            branch,
            evidence_sources,
            evidence_packages,
            evidence_locked,
        });
    }
    #[derive(Serialize)]
    struct Document {
        coverage: DocumentCoverage,
    }
    #[derive(Serialize)]
    struct DocumentCoverage {
        exclusions: Vec<Exclusion>,
    }
    toml::to_string(&Document {
        coverage: DocumentCoverage {
            exclusions: converted,
        },
    })
    .map_err(|error| error.to_string())
}

#[derive(Subcommand)]
pub enum Action {
    /// Gate emitted production lines and branches from one LLVM execution.
    Gate {
        /// LCOV report from `cargo llvm-cov report --lcov`.
        lcov: PathBuf,
        /// Companion raw JSON export from `cargo llvm-cov --json`.
        json: PathBuf,
        /// Same-execution annotated text report from `cargo llvm-cov report --text`.
        annotated: PathBuf,
        /// Coverage target the exclusions apply to.
        #[arg(default_value = "native")]
        target: String,
        /// Repository root the reports and exclusions resolve against.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Manifest carrying `[[coverage.exclusions]]`.
        #[arg(long, default_value = ".ci/ccid.toml")]
        manifest: PathBuf,
    },
    /// Gate exact LLVM line and branch counts, without rounded percentages.
    Totals {
        /// Raw JSON export from `cargo llvm-cov --json`.
        json: PathBuf,
        /// Every `**/*.rs` below this directory must appear in the report.
        #[arg(long)]
        require_sources: Option<PathBuf>,
    },
    /// Convert a legacy coverage-exclusions JSON file to manifest TOML.
    ConvertExclusions {
        /// Legacy `.github/coverage-exclusions.json` or `docs/coverage-exclusions.json`.
        #[arg(long)]
        from: PathBuf,
        /// Targets for entries without `target`/`targets`, comma-separated.
        #[arg(long, default_value = "native")]
        default_targets: String,
    },
    /// Run `cargo llvm-cov` once (nightly, private target directory outside the
    /// repository), write coverage.json, coverage.lcov and coverage.txt into the
    /// repository, then run the `[coverage] gates` on those three reports.
    Run {
        /// Coverage target the exclusions apply to.
        #[arg(long, default_value = "native")]
        target: String,
        /// Repository the reports are written into; the run's working directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Manifest carrying `[coverage]`.
        #[arg(long, default_value = ".ci/ccid.toml")]
        manifest: PathBuf,
    },
}

/// The manifest subset the coverage commands read; every other table is ignored.
#[derive(Debug, Deserialize)]
struct ManifestFile {
    #[serde(default)]
    coverage: Option<Config>,
}

/// Run one coverage subcommand. `Ok(code)` is the process exit code;
/// gate refusals print `Coverage gate: <message>` and return `Ok(1)`.
pub fn run(action: Action) -> Result<u8> {
    match action {
        Action::Gate {
            lcov,
            json,
            annotated,
            target,
            repo,
            manifest,
        } => {
            let target = Target::from_str(&target);
            let Ok(target) = target else {
                eprintln!("Coverage gate: Unknown coverage target");
                return Ok(1);
            };
            let root = resolve_root(&repo);
            let read = |path: &PathBuf| {
                let absolute = absolutize(path, &root);
                fs::read_to_string(&absolute)
                    .map_err(|error| format!("{}: {error}", absolute.display()))
            };
            let lcov_text = match read(&lcov) {
                Ok(text) => text,
                Err(error) => {
                    eprintln!("Coverage gate: {error}");
                    return Ok(1);
                }
            };
            let json_text = match read(&json) {
                Ok(text) => text,
                Err(error) => {
                    eprintln!("Coverage gate: {error}");
                    return Ok(1);
                }
            };
            let annotated_text = match read(&annotated) {
                Ok(text) => text,
                Err(error) => {
                    eprintln!("Coverage gate: {error}");
                    return Ok(1);
                }
            };
            let raw: serde_json::Value = match serde_json::from_str(&json_text) {
                Ok(raw) => raw,
                Err(error) => {
                    eprintln!("Coverage gate: {error}");
                    return Ok(1);
                }
            };
            let exclusions = load_config(&absolutize(&manifest, &root))?.exclusions;
            match gate(
                &lcov_text,
                &raw,
                &annotated_text,
                &root,
                &exclusions,
                target,
            ) {
                Ok(report) => {
                    for line in report.output {
                        println!("{line}");
                    }
                    Ok(0)
                }
                Err(message) => {
                    eprintln!("Coverage gate: {message}");
                    Ok(1)
                }
            }
        }
        Action::Totals {
            json,
            require_sources,
        } => {
            let text = match fs::read_to_string(&json) {
                Ok(text) => text,
                Err(error) => {
                    eprintln!("Coverage gate: {}: {error}", json.display());
                    return Ok(1);
                }
            };
            let raw: serde_json::Value = match serde_json::from_str(&text) {
                Ok(raw) => raw,
                Err(error) => {
                    eprintln!("Coverage gate: {error}");
                    return Ok(1);
                }
            };
            match totals(&raw, require_sources.as_deref()) {
                Ok(output) => {
                    for line in output {
                        println!("{line}");
                    }
                    Ok(0)
                }
                Err(message) => {
                    eprintln!("Coverage gate: {message}");
                    Ok(1)
                }
            }
        }
        Action::ConvertExclusions {
            from,
            default_targets,
        } => {
            let mut selected = Vec::new();
            for name in default_targets
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                match Target::from_str(name) {
                    Ok(target) => selected.push(target),
                    Err(message) => return Err(failure(message)),
                }
            }
            let text = fs::read_to_string(&from)
                .map_err(|error| failure(format!("{}: {error}", from.display())))?;
            let document = convert_exclusions(&text, &selected).map_err(failure)?;
            print!("{document}");
            Ok(0)
        }
        Action::Run {
            target,
            repo,
            manifest,
        } => {
            let environment: Environment = std::env::vars_os().collect();
            run_coverage(&target, &repo, &manifest, &environment)
        }
    }
}

/// The `[coverage]` table of a manifest; a missing manifest is all defaults.
fn load_config(path: &Path) -> Result<Config> {
    match fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(failure(format!("{}: {error}", path.display()))),
        Ok(text) => {
            let parsed: ManifestFile = toml::from_str(&text)
                .map_err(|error| failure(format!("{}: {error}", path.display())))?;
            Ok(parsed.coverage.unwrap_or_default())
        }
    }
}

fn variable(environment: &Environment, name: &str) -> Option<OsString> {
    environment
        .get(OsStr::new(name))
        .filter(|value| !value.is_empty())
        .cloned()
}

/// The directory the private target is created in: `TMPDIR`, else the platform
/// default. Refused when it lies inside the repository (the staged copy when
/// the run floated): the build would write into the checked tree.
pub fn private_target_parent(temporary: &Path, repo: &Path) -> Result<PathBuf> {
    let parent = temporary.canonicalize().map_err(|error| {
        failure(format!(
            "coverage target parent {}: {error}",
            temporary.display()
        ))
    })?;
    if parent.starts_with(repo) {
        return Err(failure(format!(
            "unsafe coverage target: {} is inside {}",
            parent.display(),
            repo.display()
        )));
    }
    Ok(parent)
}

/// `$ARTIFACT_ROOT` for `artifacts = true`: set, absolute and outside the repository.
fn artifact_root(environment: &Environment, repo: &Path) -> Result<PathBuf> {
    let directory = variable(environment, "ARTIFACT_ROOT")
        .map(PathBuf::from)
        .ok_or_else(|| failure("[coverage] artifacts = true requires ARTIFACT_ROOT"))?;
    if !directory.is_absolute() {
        return Err(failure("ARTIFACT_ROOT must be absolute"));
    }
    let resolved = directory
        .canonicalize()
        .unwrap_or_else(|_| absolutize(&directory, Path::new("/")));
    if resolved.starts_with(repo) {
        return Err(failure(format!(
            "ARTIFACT_ROOT {} is inside {}",
            directory.display(),
            repo.display()
        )));
    }
    Ok(directory)
}

/// The three `cargo llvm-cov` invocations of one run, in order: the
/// instrumented execution with its JSON export, then the LCOV and annotated
/// text reports of that same execution.
pub fn report_commands(cargo: &Path, config: &Config) -> [Vec<OsString>; 3] {
    let command = |head: &[&str], configured: &[&[String]], tail: [&str; 3]| {
        let mut argv = vec![cargo.as_os_str().to_owned()];
        argv.extend(head.iter().map(OsString::from));
        for arguments in configured {
            argv.extend(arguments.iter().map(OsString::from));
        }
        argv.extend(tail.iter().map(OsString::from));
        argv
    };
    let report = config.report_arguments();
    [
        command(
            &["llvm-cov", "--locked"],
            &[&config.llvm_cov_arguments()],
            ["--json", "--output-path", REPORTS[0]],
        ),
        command(
            &["llvm-cov", "report", "--locked"],
            &[&report],
            ["--lcov", "--output-path", REPORTS[1]],
        ),
        command(
            &["llvm-cov", "report", "--locked"],
            &[&report, &config.text],
            ["--text", "--output-path", REPORTS[2]],
        ),
    ]
}

fn display(command: &[OsString]) -> String {
    command
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `rustup which --toolchain nightly <tool>`: an existing file, else an error.
fn nightly_tool(environment: &Environment, tool: &str) -> Result<PathBuf> {
    let output = Command::new("rustup")
        .args(["which", "--toolchain", "nightly", tool])
        .env_clear()
        .envs(environment)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| {
            failure(format!(
                "the nightly toolchain is unavailable: cannot run rustup: {error}"
            ))
        })?;
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !output.status.success() || !path.is_file() {
        return Err(failure(format!(
            "the nightly toolchain is unavailable: rustup which --toolchain nightly {tool}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(path)
}

/// Run one tool command in `root`; `Ok(false)` after printing its failure.
fn execute(command: &[OsString], environment: &Environment, root: &Path) -> Result<bool> {
    if crate::INTERRUPTED.load(Ordering::SeqCst) {
        return Err(failure("ccid coverage run: interrupted"));
    }
    let status = Command::new(&command[0])
        .args(&command[1..])
        .current_dir(root)
        .env_clear()
        .envs(environment)
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|error| failure(format!("cannot start {}: {error}", display(command))))?;
    if status.success() {
        return Ok(true);
    }
    let code = status
        .code()
        .map_or_else(|| "signal".to_owned(), |code| code.to_string());
    eprintln!(
        "ccid coverage run: {} failed (exit {code})",
        display(command)
    );
    Ok(false)
}

/// The gates of one run over its three reports; prints like `gate` and `totals`.
fn run_gates(root: &Path, reports: &Path, config: &Config, gates: &[Gate], target: Target) -> u8 {
    let mut texts = Vec::new();
    for name in REPORTS {
        let path = reports.join(name);
        match fs::read_to_string(&path) {
            Ok(text) => texts.push(text),
            Err(error) => {
                eprintln!("Coverage gate: {}: {error}", path.display());
                return 1;
            }
        }
    }
    let raw: serde_json::Value = match serde_json::from_str(&texts[0]) {
        Ok(raw) => raw,
        Err(error) => {
            eprintln!("Coverage gate: {error}");
            return 1;
        }
    };
    for selected in gates {
        let outcome = match selected {
            Gate::Totals => {
                let required = config
                    .require_sources
                    .as_ref()
                    .map(|directory| root.join(directory));
                totals(&raw, required.as_deref())
            }
            Gate::Source => gate(&texts[1], &raw, &texts[2], root, &config.exclusions, target)
                .map(|report| report.output),
        };
        match outcome {
            Ok(lines) => {
                for line in lines {
                    println!("{line}");
                }
            }
            Err(message) => {
                eprintln!("Coverage gate: {message}");
                return 1;
            }
        }
    }
    0
}

/// Produce the reports with a private target directory, removed in every case, and gate them.
fn produce(
    root: &Path,
    config: &Config,
    gates: &[Gate],
    target: Target,
    environment: &Environment,
) -> Result<u8> {
    let temporary = variable(environment, "TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let parent = private_target_parent(&temporary, root)?;
    for name in REPORTS {
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_dir() => fs::remove_file(&path)?,
            Ok(_) => {
                return Err(failure(format!(
                    "{} is a directory, not a coverage report",
                    path.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let rustc = nightly_tool(environment, "rustc")?;
    let cargo = nightly_tool(environment, "cargo")?;
    let bin = rustc
        .parent()
        .ok_or_else(|| failure("the nightly rustc has no directory"))?;
    let mut child = environment.clone();
    let path = std::env::join_paths(
        std::iter::once(bin.to_path_buf()).chain(
            variable(environment, "PATH")
                .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
                .unwrap_or_default(),
        ),
    )
    .map_err(|error| failure(format!("cannot extend PATH: {error}")))?;
    child.insert("PATH".into(), path);
    child.insert("RUSTC".into(), rustc.clone().into_os_string());
    child.insert("CARGO".into(), cargo.clone().into_os_string());
    child.insert("RUSTUP_TOOLCHAIN".into(), "nightly".into());
    for name in ["CARGO_BUILD_TARGET_DIR", "CARGO_LLVM_COV_TARGET_DIR"] {
        child.remove(OsStr::new(name));
    }
    let probe = [
        cargo.clone().into_os_string(),
        "llvm-cov".into(),
        "--version".into(),
    ];
    let available = Command::new(&probe[0])
        .args(&probe[1..])
        .current_dir(root)
        .env_clear()
        .envs(&child)
        .stdin(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !available {
        return Err(failure(format!(
            "cargo-llvm-cov is unavailable for the nightly toolchain: {} failed",
            display(&probe)
        )));
    }
    let directory = tempfile::Builder::new()
        .prefix("ccid-coverage-")
        .tempdir_in(&parent)?;
    child.insert(
        "CARGO_TARGET_DIR".into(),
        directory.path().as_os_str().to_owned(),
    );
    eprintln!(
        "ccid coverage run: nightly {} with target directory {}",
        rustc.display(),
        directory.path().display()
    );
    let mut outcome = Ok(0);
    for command in report_commands(&cargo, config) {
        match execute(&command, &child, root) {
            Ok(true) => {}
            Ok(false) => {
                outcome = Ok(1);
                break;
            }
            Err(error) => {
                outcome = Err(error);
                break;
            }
        }
    }
    if matches!(outcome, Ok(0)) {
        outcome = Ok(run_gates(root, root, config, gates, target));
    }
    if matches!(outcome, Ok(0)) {
        if let Some(wasm) = &config.wasm {
            if target != Target::Native {
                return Err(failure(
                    "[coverage.wasm] runs with --target native: the wasm execution is gated as wasm itself",
                ));
            }
            outcome = wasm_stage(root, wasm, config, gates, &child, &parent);
        }
    }
    let kept = directory.path().to_path_buf();
    if let Err(error) = directory.close() {
        eprintln!(
            "ccid coverage run: cannot remove the coverage target {}: {error}",
            kept.display()
        );
        if matches!(outcome, Ok(0)) {
            outcome = Err(failure("the coverage target directory was not removed"));
        }
    }
    outcome
}

/// The wasm execution: run the repository's command into a private report directory and gate
/// its reports with target `wasm`. The directory is removed in every case.
fn wasm_stage(
    root: &Path,
    wasm: &Wasm,
    config: &Config,
    gates: &[Gate],
    child: &Environment,
    parent: &Path,
) -> Result<u8> {
    let out = tempfile::Builder::new()
        .prefix("ccid-coverage-wasm-")
        .tempdir_in(parent)?;
    let mut environment = child.clone();
    environment.insert(
        "CCID_COVERAGE_OUT".into(),
        out.path().as_os_str().to_owned(),
    );
    // The wasm tests build their own target tree: never the native one.
    environment.remove(OsStr::new("CARGO_TARGET_DIR"));
    eprintln!(
        "ccid coverage run: wasm execution {} into {}",
        wasm.command.join(" "),
        out.path().display()
    );
    let argv: Vec<OsString> = wasm.command.iter().map(OsString::from).collect();
    let outcome = match execute(&argv, &environment, root) {
        Ok(true) => Ok(run_gates(root, out.path(), config, gates, Target::Wasm)),
        Ok(false) => Ok(1),
        Err(error) => Err(error),
    };
    out.close()?;
    outcome
}

/// `ccid coverage run`. `Ok(code)`: 0 pass, 1 tool failure or gate refusal;
/// `Err` (exit 2): invalid configuration, missing nightly or cargo-llvm-cov, an
/// unsafe target directory or artifact root.
pub fn run_coverage(
    target: &str,
    repo: &Path,
    manifest: &Path,
    environment: &Environment,
) -> Result<u8> {
    let target = Target::from_str(target)
        .map_err(|_| failure(format!("Unknown coverage target {target:?}")))?;
    let root = resolve_root(repo)
        .canonicalize()
        .map_err(|error| failure(format!("{}: {error}", repo.display())))?;
    let config = load_config(&absolutize(manifest, &root))?;
    let gates = config.validate_run().map_err(failure)?;
    let artifacts = if config.artifacts {
        Some(artifact_root(environment, &root)?)
    } else {
        None
    };
    let mut outcome = produce(&root, &config, &gates, target, environment);
    if let Some(directory) = artifacts {
        // Like the scripts' exit trap: whatever exists, also after a failure.
        let copied = fs::create_dir_all(&directory).and_then(|()| {
            for name in REPORTS {
                let report = root.join(name);
                if report.is_file() {
                    fs::copy(&report, directory.join(name))?;
                }
            }
            Ok(())
        });
        if let Err(error) = copied {
            eprintln!(
                "ccid coverage run: cannot copy the reports to {}: {error}",
                directory.display()
            );
            if matches!(outcome, Ok(0)) {
                outcome = Ok(1);
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests;

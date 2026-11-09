//! The coverage gates: one emitted-source gate and one totals gate.
//!
//! The source gate cross-checks three reports from the same LLVM execution
//! (LCOV counters, the raw JSON export, the annotated text report) and
//! requires every emitted production line and branch to be covered, except
//! for manifest-declared exclusions. The totals gate checks exact LLVM line
//! and branch counts per unit, optionally requiring every `src/**/*.rs` file
//! to appear in the report. Running `cargo llvm-cov` stays in the calling
//! repository; this module only evaluates the reports it produced.

use crate::Result;
use clap::Subcommand;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    str::FromStr,
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
}

/// The `[coverage]` table of `.ci/ccid.toml`.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub exclusions: Vec<Exclusion>,
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

struct RawFile {
    branches: Vec<[i64; 4]>,
    lines_count: Option<i64>,
    lines_covered: Option<i64>,
    branches_count: Option<i64>,
    branches_covered: Option<i64>,
}

fn raw_file(item: &serde_json::Value) -> std::result::Result<RawFile, String> {
    let branches = item
        .get("branches")
        .and_then(|branches| branches.as_array())
        .ok_or("Missing emitted branch locations")?;
    let mut locations = Vec::new();
    for branch in branches {
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
        locations.push(location);
    }
    let summary = item.get("summary").ok_or("Invalid raw JSON summary")?;
    let pick =
        |metric: &str, field: &str| json_count(summary.get(metric).and_then(|m| m.get(field)));
    Ok(RawFile {
        branches: locations,
        lines_count: pick("lines", "count"),
        lines_covered: pick("lines", "covered"),
        branches_count: pick("branches", "count"),
        branches_covered: pick("branches", "covered"),
    })
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
        for item in items {
            let filename = item.get("filename").and_then(|name| name.as_str());
            let Some(filename) = filename else {
                return Err("Missing companion raw LLVM JSON".to_owned());
            };
            let path = source_path(filename, &root)?;
            if expected.contains_key(&path) {
                return Err("Duplicate raw source".to_owned());
            }
            expected.insert(path, raw_file(item)?);
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
            let manifest_path = absolutize(&manifest, &root);
            let exclusions = match fs::read_to_string(&manifest_path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(failure(format!("{}: {error}", manifest_path.display()))),
                Ok(text) => {
                    let parsed: ManifestFile = toml::from_str(&text).map_err(|error| {
                        failure(format!("{}: {error}", manifest_path.display()))
                    })?;
                    parsed.coverage.unwrap_or_default().exclusions
                }
            };
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
    }
}

#[cfg(test)]
mod tests;

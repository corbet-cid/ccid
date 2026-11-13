//! Security and dependency work made visible (operating model R47).
//!
//! Two data files, both committed in the repository, are the whole input; nothing here reads a
//! network, a clock or a forge, so `ccid render --check` stays byte-stable:
//!
//! * `.ci/security.toml`, the **ledger** of advisories in the repository's *dependencies*. The daily
//!   fleet check (R49) writes it, and only when an advisory appears or changes state, so a repository
//!   gets at most one update change per day and no "last scanned" date is ever committed.
//! * `.ci/own-advisories.toml`, advisories for vulnerabilities in the project's *own* code, written by
//!   a person. Only these are ever published through a forge's advisory feature.
//!
//! From them ccid renders a status block in `README.md`, a policy block in `SECURITY.md` (both between
//! markers, so hand-written text around them stays), and a section in the release notes. The offline
//! `ccid security plan` shows the calls that would publish the own advisories; it sends nothing.
//!
//! Free text from an advisory database is never rendered: every value that reaches a Markdown file
//! passes a strict character set first, and a value outside it is refused (exit 2), not escaped.

mod blocks;
mod files;
mod publish;
mod text;

#[cfg(test)]
mod tests;

pub use files::{plan_files, FilePlan};
pub use publish::plan_json;
pub use text::release_section;

use crate::{failure, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

/// The ledger of advisories in the repository's dependencies.
pub const LEDGER: &str = ".ci/security.toml";
/// The advisories for the project's own code, the only ones that are published.
pub const OWN: &str = ".ci/own-advisories.toml";
/// Both files are small data; a larger one is a mistake, not a ledger.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// The ledger and own-advisory file format. A change that older readers cannot parse bumps it.
const SCHEMA: u32 = 1;

/// How serious an advisory is. `unknown` is the honest value when the database gives none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Unknown,
    Low,
    Moderate,
    High,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Unknown => "unknown",
            Severity::Low => "low",
            Severity::Moderate => "moderate",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }
}

/// Where an advisory in a dependency stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Affects the locked set and is not fixed yet.
    Open,
    /// The locked set no longer contains the affected version.
    Fixed,
    /// Reviewed and kept on purpose; `reason` says why.
    Accepted,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Open => "open",
            State::Fixed => "fixed",
            State::Accepted => "accepted",
        }
    }
}

/// One advisory that touched the locked dependencies of the repository.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Advisory {
    /// The OSV id, e.g. `GHSA-xxxx-xxxx-xxxx` or `RUSTSEC-2024-0388`.
    pub id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub package: String,
    /// The OSV ecosystem name, e.g. `crates.io` or `npm`.
    pub ecosystem: String,
    /// The locked version the advisory was found against.
    pub affected: String,
    /// The first version that fixes it, when the database names one.
    #[serde(default)]
    pub fixed: Option<String>,
    pub severity: Severity,
    /// `YYYY-MM-DD`: the day the daily check first saw it.
    pub found: String,
    /// `YYYY-MM-DD`: the day the state became `fixed`. Required for `fixed`, refused otherwise.
    #[serde(default)]
    pub resolved: Option<String>,
    pub state: State,
    /// Why an `accepted` advisory stays. Required for `accepted`, refused otherwise.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `.ci/security.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ledger {
    schema: u32,
    #[serde(default, rename = "advisory")]
    pub advisories: Vec<Advisory>,
}

/// One vulnerability in the project's own code, fixed in a release, written for publication.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Own {
    /// A local slug that names the entry in this file; the forge assigns its own id on publication.
    pub key: String,
    /// One plain line, at most 200 characters.
    pub summary: String,
    /// The advisory text as the forge shows it. Reviewed by a person before it is ever sent.
    pub description: String,
    pub severity: Severity,
    /// The forge's ecosystem name for the project's package (GitHub: `rust`, `npm`, `pip`, `go`, ...).
    pub ecosystem: String,
    pub package: String,
    /// The affected versions of the project's own package, e.g. `>= 0.2.0, < 0.3.5`.
    pub affected: String,
    /// The first release that fixes it.
    pub patched: String,
    /// `YYYY-MM-DD`: when the problem was found.
    pub found: String,
    /// `YYYY-MM-DD`: when the fixing release shipped.
    pub resolved: String,
    /// The CVE id when one was assigned.
    #[serde(default)]
    pub cve: Option<String>,
}

/// `.ci/own-advisories.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnFile {
    schema: u32,
    #[serde(default, rename = "advisory")]
    pub advisories: Vec<Own>,
}

/// Read a data file under the repository root, or `None` when it is absent. A symlink, a directory
/// and an oversized file are refused: a status that can be pointed at other files is not a status.
fn read_optional(repo: &Path, relative: &str) -> Result<Option<String>> {
    let path = repo.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(failure(format!("{relative} must be a regular file")));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(failure(format!("{relative} is larger than 4 MiB")));
    }
    let bytes = fs::read(&path)?;
    let text =
        String::from_utf8(bytes).map_err(|_| failure(format!("{relative} is not valid UTF-8")))?;
    Ok(Some(text))
}

/// The ledger of `repo`; empty when the file is absent.
pub fn load_ledger(repo: &Path) -> Result<Ledger> {
    let Some(text) = read_optional(repo, LEDGER)? else {
        return Ok(Ledger {
            schema: SCHEMA,
            advisories: Vec::new(),
        });
    };
    let ledger: Ledger =
        toml::from_str(&text).map_err(|error| failure(format!("Invalid {LEDGER}: {error}")))?;
    ledger.validate()?;
    Ok(ledger)
}

/// The own advisories of `repo`; empty when the file is absent.
pub fn load_own(repo: &Path) -> Result<OwnFile> {
    let Some(text) = read_optional(repo, OWN)? else {
        return Ok(OwnFile {
            schema: SCHEMA,
            advisories: Vec::new(),
        });
    };
    let own: OwnFile =
        toml::from_str(&text).map_err(|error| failure(format!("Invalid {OWN}: {error}")))?;
    own.validate()?;
    Ok(own)
}

// ---- validation -------------------------------------------------------------------------------

fn all_in(value: &str, allowed: impl Fn(char) -> bool) -> bool {
    value.chars().all(allowed)
}

fn bad(file: &str, field: &str, what: &str) -> Box<dyn std::error::Error + Send + Sync> {
    failure(format!("Invalid {file}: {field} {what}"))
}

/// An advisory id: OSV, GHSA, CVE and RUSTSEC ids all fit.
fn valid_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && all_in(value, |c| c.is_ascii_alphanumeric() || "._:-".contains(c))
}

/// A package name in any ecosystem: crates, npm scopes, Go paths, Maven coordinates.
fn valid_package(value: &str) -> bool {
    (1..=214).contains(&value.len())
        && all_in(value, |c| {
            c.is_ascii_alphanumeric() || "._@/:+~-".contains(c)
        })
}

fn valid_ecosystem(value: &str) -> bool {
    (1..=40).contains(&value.len())
        && all_in(value, |c| c.is_ascii_alphanumeric() || "._:-".contains(c))
}

fn valid_version(value: &str) -> bool {
    (1..=100).contains(&value.len())
        && all_in(value, |c| c.is_ascii_alphanumeric() || "._+~:-".contains(c))
}

/// A version range as GitHub writes it: `< 1.2.3`, `>= 1.0, < 1.4`.
fn valid_range(value: &str) -> bool {
    (1..=100).contains(&value.len())
        && value == value.trim()
        && all_in(value, |c| {
            c.is_ascii_alphanumeric() || " .,<>=~^+-_".contains(c)
        })
}

/// One plain line: letters, digits, spaces and a little punctuation, no Markdown or HTML syntax.
fn valid_plain(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.chars().count() <= max
        && value == value.trim()
        && all_in(value, |c| {
            c.is_ascii_alphanumeric() || " .,;:'()/+_-".contains(c)
        })
}

fn valid_key(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && all_in(value, |c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
        })
}

/// `YYYY-MM-DD`, a real calendar day.
fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<u32> {
        let part = &value[range];
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let (Some(year), Some(month), Some(day)) = (digits(0..4), digits(5..7), digits(8..10)) else {
        return false;
    };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    year >= 2000 && (1..=days).contains(&day)
}

/// The ecosystems GitHub's repository advisories accept.
const GITHUB_ECOSYSTEMS: [&str; 13] = [
    "actions", "composer", "erlang", "go", "maven", "npm", "nuget", "other", "pip", "pub",
    "rubygems", "rust", "swift",
];

impl Ledger {
    fn validate(&self) -> Result<()> {
        if self.schema != SCHEMA {
            return Err(failure(format!(
                "Invalid {LEDGER}: schema {} is not supported (this ccid reads schema {SCHEMA})",
                self.schema
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for advisory in &self.advisories {
            advisory.validate()?;
            let key = (&advisory.id, &advisory.package, &advisory.affected);
            if !seen.insert(key) {
                return Err(failure(format!(
                    "Invalid {LEDGER}: {} {} {} is listed twice",
                    advisory.id, advisory.package, advisory.affected
                )));
            }
        }
        Ok(())
    }

    /// Advisories in `state`, newest first (found day, then id): the order every renderer uses.
    pub fn in_state(&self, state: State) -> Vec<&Advisory> {
        let mut list: Vec<&Advisory> = self
            .advisories
            .iter()
            .filter(|advisory| advisory.state == state)
            .collect();
        list.sort_by(|a, b| {
            b.resolved
                .cmp(&a.resolved)
                .then_with(|| b.found.cmp(&a.found))
                .then_with(|| a.id.cmp(&b.id))
                .then_with(|| a.package.cmp(&b.package))
                .then_with(|| a.affected.cmp(&b.affected))
        });
        list
    }
}

impl Advisory {
    fn validate(&self) -> Result<()> {
        let field = |name: &str| format!("advisory {} {name}", self.id);
        if !valid_id(&self.id) {
            return Err(bad(LEDGER, "an advisory id", "is not a plain id"));
        }
        if self.aliases.len() > 32 || !self.aliases.iter().all(|alias| valid_id(alias)) {
            return Err(bad(
                LEDGER,
                &field("aliases"),
                "are not plain ids (at most 32)",
            ));
        }
        if !valid_package(&self.package) {
            return Err(bad(
                LEDGER,
                &field("package"),
                "is not a plain package name",
            ));
        }
        if !valid_ecosystem(&self.ecosystem) {
            return Err(bad(
                LEDGER,
                &field("ecosystem"),
                "is not a plain ecosystem name",
            ));
        }
        if !valid_version(&self.affected) {
            return Err(bad(LEDGER, &field("affected"), "is not a plain version"));
        }
        if self.fixed.as_deref().is_some_and(|v| !valid_version(v)) {
            return Err(bad(LEDGER, &field("fixed"), "is not a plain version"));
        }
        if !valid_date(&self.found) {
            return Err(bad(LEDGER, &field("found"), "is not a YYYY-MM-DD date"));
        }
        if let Some(resolved) = &self.resolved {
            if !valid_date(resolved) {
                return Err(bad(LEDGER, &field("resolved"), "is not a YYYY-MM-DD date"));
            }
            if *resolved < self.found {
                return Err(bad(LEDGER, &field("resolved"), "is before it was found"));
            }
        }
        match self.state {
            State::Fixed => {
                if self.fixed.is_none() || self.resolved.is_none() {
                    return Err(bad(
                        LEDGER,
                        &field("state fixed"),
                        "needs fixed and resolved",
                    ));
                }
            }
            State::Open | State::Accepted => {
                if self.resolved.is_some() {
                    return Err(bad(LEDGER, &field("resolved"), "is only for state fixed"));
                }
            }
        }
        match (self.state, &self.reason) {
            (State::Accepted, Some(reason)) if valid_plain(reason, 200) => {}
            (State::Accepted, _) => {
                return Err(bad(
                    LEDGER,
                    &field("reason"),
                    "is required for state accepted: one plain line of at most 200 characters",
                ));
            }
            (_, Some(_)) => {
                return Err(bad(LEDGER, &field("reason"), "is only for state accepted"));
            }
            (_, None) => {}
        }
        Ok(())
    }
}

impl OwnFile {
    fn validate(&self) -> Result<()> {
        if self.schema != SCHEMA {
            return Err(failure(format!(
                "Invalid {OWN}: schema {} is not supported (this ccid reads schema {SCHEMA})",
                self.schema
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for entry in &self.advisories {
            entry.validate()?;
            if !seen.insert(&entry.key) {
                return Err(failure(format!(
                    "Invalid {OWN}: key {} is used twice",
                    entry.key
                )));
            }
        }
        Ok(())
    }
}

impl Own {
    fn validate(&self) -> Result<()> {
        let field = |name: &str| format!("advisory {} {name}", self.key);
        if !valid_key(&self.key) {
            return Err(bad(
                OWN,
                "a key",
                "is not a lowercase slug of at most 64 characters",
            ));
        }
        if !valid_plain(&self.summary, 200) {
            return Err(bad(
                OWN,
                &field("summary"),
                "is not one plain line of at most 200 characters",
            ));
        }
        if self.description.trim().is_empty()
            || self.description.len() > 16 * 1024
            || self.description.chars().any(|c| c == '\0')
        {
            return Err(bad(
                OWN,
                &field("description"),
                "must be 1 to 16384 bytes of text",
            ));
        }
        if self.severity == Severity::Unknown {
            return Err(bad(
                OWN,
                &field("severity"),
                "must be low, moderate, high or critical",
            ));
        }
        if !GITHUB_ECOSYSTEMS.contains(&self.ecosystem.as_str()) {
            return Err(bad(
                OWN,
                &field("ecosystem"),
                "is not an ecosystem the forges accept",
            ));
        }
        if !valid_package(&self.package) {
            return Err(bad(OWN, &field("package"), "is not a plain package name"));
        }
        if !valid_range(&self.affected) {
            return Err(bad(OWN, &field("affected"), "is not a plain version range"));
        }
        if !valid_version(&self.patched) {
            return Err(bad(OWN, &field("patched"), "is not a plain version"));
        }
        for (name, date) in [("found", &self.found), ("resolved", &self.resolved)] {
            if !valid_date(date) {
                return Err(bad(OWN, &field(name), "is not a YYYY-MM-DD date"));
            }
        }
        if self.resolved < self.found {
            return Err(bad(OWN, &field("resolved"), "is before it was found"));
        }
        if self.cve.as_deref().is_some_and(|cve| !valid_cve(cve)) {
            return Err(bad(OWN, &field("cve"), "is not a CVE id"));
        }
        Ok(())
    }
}

/// `CVE-YYYY-NNNN...`
fn valid_cve(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("CVE-") else {
        return false;
    };
    let mut parts = rest.splitn(2, '-');
    let (Some(year), Some(number)) = (parts.next(), parts.next()) else {
        return false;
    };
    year.len() == 4
        && year.bytes().all(|b| b.is_ascii_digit())
        && (4..=7).contains(&number.len())
        && number.bytes().all(|b| b.is_ascii_digit())
}

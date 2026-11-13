//! The three renderings of the ledger: the README status, the SECURITY.md policy block and the
//! release-notes section. Pure functions of the two data files; the same input always gives the same
//! bytes. Every inserted value was validated against a strict character set when the file was read.

use super::blocks::{BEGIN_PREFIX, END};
use super::{load_ledger, load_own, Advisory, Ledger, Own, OwnFile, State, LEDGER};
use crate::Result;
use std::{fmt::Write as _, path::Path};

const OSV: &str = "https://osv.dev/vulnerability/";
/// Rows of open and accepted advisories a table shows before it points at the ledger.
const OPEN_ROWS: usize = 50;
/// Rows of fixed advisories a table shows (the most recent).
const FIXED_ROWS: usize = 25;
/// Advisory ids a sentence names before it says "and N more".
const NAMED: usize = 5;

/// The first line of a block.
fn begin_line() -> String {
    format!("{BEGIN_PREFIX} (rendered by ccid render from {LEDGER}; do not edit by hand) -->")
}

fn osv_link(id: &str) -> String {
    format!("[{id}]({OSV}{id})")
}

/// `A, B, C and 2 more` for up to [`NAMED`] ids.
fn named(list: &[&Advisory]) -> String {
    let mut ids: Vec<String> = Vec::new();
    for advisory in list {
        let link = osv_link(&advisory.id);
        if !ids.contains(&link) {
            ids.push(link);
        }
    }
    let total = ids.len();
    ids.truncate(NAMED);
    let mut out = ids.join(", ");
    if total > NAMED {
        let _ = write!(out, " and {} more", total - NAMED);
    }
    out
}

/// The one-sentence counts: `2 open (A, B), 1 accepted after review, 14 fixed`.
fn counts(ledger: &Ledger) -> Option<String> {
    let open = ledger.in_state(State::Open);
    let accepted = ledger.in_state(State::Accepted);
    let fixed = ledger.in_state(State::Fixed);
    let mut parts = Vec::new();
    if !open.is_empty() {
        parts.push(format!("{} open ({})", open.len(), named(&open)));
    }
    if !accepted.is_empty() {
        parts.push(format!("{} accepted after review", accepted.len()));
    }
    if !fixed.is_empty() {
        parts.push(format!("{} fixed", fixed.len()));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// The status block of the README: two short paragraphs, no headings.
pub(super) fn readme_block(ledger: &Ledger, own: &OwnFile, policy: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", begin_line());
    let _ = writeln!(
        out,
        "**Security and dependencies.** Dependencies are checked every day against the [OSV](https://osv.dev) advisory database. Fixes and updates are merged automatically once the tests pass; see the [security policy]({policy}).\n"
    );
    match counts(ledger) {
        None => out.push_str("Advisories on record for dependencies: none."),
        Some(counts) => {
            let _ = write!(out, "Advisories in dependencies: {counts}.");
        }
    }
    if !own.advisories.is_empty() {
        let _ = write!(
            out,
            " Published advisories for this project: {}.",
            own.advisories.len()
        );
    }
    let _ = write!(out, "\n{END}");
    out
}

fn table_row(advisory: &Advisory) -> String {
    let state = match &advisory.reason {
        Some(reason) => format!("accepted ({reason})"),
        None => advisory.state.as_str().to_owned(),
    };
    format!(
        "| {} | `{}` ({}) | `{}` | {} | {} | {} | {} | {} |",
        osv_link(&advisory.id),
        advisory.package,
        advisory.ecosystem,
        advisory.affected,
        advisory
            .fixed
            .as_deref()
            .map_or_else(|| "-".to_owned(), |v| format!("`{v}`")),
        advisory.severity.as_str(),
        state,
        advisory.found,
        advisory.resolved.as_deref().unwrap_or("-"),
    )
}

const TABLE_HEAD: &str = "| Advisory | Package | Affected | Fixed in | Severity | State | Found | Resolved |\n|---|---|---|---|---|---|---|---|";

fn own_row(entry: &Own) -> String {
    format!(
        "| {} | `{}` ({}) | `{}` | `{}` | {} | {} | {} |",
        entry.summary,
        entry.package,
        entry.ecosystem,
        entry.affected,
        entry.patched,
        entry.severity.as_str(),
        entry.resolved,
        entry.cve.as_deref().unwrap_or("-"),
    )
}

/// The policy block of SECURITY.md.
pub(super) fn policy_block(ledger: &Ledger, own: &OwnFile) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", begin_line());
    out.push_str("## Dependency and security updates\n\n");
    out.push_str("Dependencies of this project are checked every day against the [OSV](https://osv.dev) advisory database. Security fixes and dependency updates are made automatically and merged once the tests pass. An update whose tests fail is not merged: it is handed to the maintainers' agents to fix, security first, and it stays visible as an issue on every forge this project is on.\n\n");
    out.push_str("### Advisories in dependencies\n\n");
    let open = ledger.in_state(State::Open);
    let accepted = ledger.in_state(State::Accepted);
    let fixed = ledger.in_state(State::Fixed);
    if open.is_empty() && accepted.is_empty() && fixed.is_empty() {
        out.push_str("None on record.\n\n");
    } else {
        let _ = writeln!(out, "{TABLE_HEAD}");
        for advisory in open.iter().chain(&accepted).take(OPEN_ROWS) {
            let _ = writeln!(out, "{}", table_row(advisory));
        }
        for advisory in fixed.iter().take(FIXED_ROWS) {
            let _ = writeln!(out, "{}", table_row(advisory));
        }
        let hidden = open.len() + accepted.len() + fixed.len()
            - (open.len() + accepted.len()).min(OPEN_ROWS)
            - fixed.len().min(FIXED_ROWS);
        if hidden > 0 {
            let _ = writeln!(out, "\n{hidden} more are recorded in `{LEDGER}`.");
        }
        out.push('\n');
    }
    if !own.advisories.is_empty() {
        out.push_str("### Advisories for this project\n\n");
        out.push_str("| Summary | Package | Affected | Patched | Severity | Fixed on | CVE |\n|---|---|---|---|---|---|---|\n");
        let mut entries: Vec<&Own> = own.advisories.iter().collect();
        entries.sort_by(|a, b| b.resolved.cmp(&a.resolved).then_with(|| a.key.cmp(&b.key)));
        for entry in entries {
            let _ = writeln!(out, "{}", own_row(entry));
        }
        out.push('\n');
    }
    out.push_str("### Reporting a vulnerability\n\n");
    out.push_str("Report a vulnerability privately; do not describe it in a public issue. This project is mirrored on several forges, so use the private channel of one of them:\n\n");
    out.push_str("- GitHub: the **Security** tab, then **Report a vulnerability**.\n");
    out.push_str("- GitLab: open a **confidential** issue.\n");
    out.push_str("- The Forgejo and Bitbucket copies have no private channel: use the GitHub or GitLab copy.\n");
    let _ = write!(out, "{END}");
    out
}

/// The section of the release notes, from the data at the released commit alone (the draft and the
/// promotion both build it, possibly in different jobs, and must agree).
pub fn release_section(repo: &Path) -> Result<String> {
    let ledger = load_ledger(repo)?;
    let own = load_own(repo)?;
    Ok(release_section_of(&ledger, &own))
}

pub(super) fn release_section_of(ledger: &Ledger, own: &OwnFile) -> String {
    let mut out = String::from("## Security and dependencies\n\n");
    out.push_str("Dependencies are checked every day against the [OSV](https://osv.dev) advisory database. Fixes and updates are merged automatically once the tests pass.\n\n");
    let open = ledger.in_state(State::Open);
    let accepted = ledger.in_state(State::Accepted);
    let fixed = ledger.in_state(State::Fixed);
    if open.is_empty() {
        out.push_str("- Open advisories in dependencies: none\n");
    } else {
        let _ = writeln!(
            out,
            "- Open advisories in dependencies: {} ({})",
            open.len(),
            named(&open)
        );
    }
    if !accepted.is_empty() {
        let _ = writeln!(
            out,
            "- Accepted after review: {} ({})",
            accepted.len(),
            named(&accepted)
        );
    }
    if !fixed.is_empty() {
        let latest: Vec<String> = fixed
            .iter()
            .take(3)
            .map(|advisory| {
                format!(
                    "{} (`{}` {} to {}, {})",
                    osv_link(&advisory.id),
                    advisory.package,
                    advisory.affected,
                    advisory.fixed.as_deref().unwrap_or("-"),
                    advisory.resolved.as_deref().unwrap_or("-"),
                )
            })
            .collect();
        let _ = writeln!(
            out,
            "- Fixed in dependencies: {}; the latest: {}",
            fixed.len(),
            latest.join("; ")
        );
    }
    if !own.advisories.is_empty() {
        let mut entries: Vec<&Own> = own.advisories.iter().collect();
        entries.sort_by(|a, b| b.resolved.cmp(&a.resolved).then_with(|| a.key.cmp(&b.key)));
        let list: Vec<String> = entries
            .iter()
            .take(NAMED)
            .map(|entry| format!("{} (patched in {})", entry.summary, entry.patched))
            .collect();
        let _ = writeln!(
            out,
            "- Published advisories for this project: {}; the latest: {}",
            entries.len(),
            list.join("; ")
        );
    }
    out
}

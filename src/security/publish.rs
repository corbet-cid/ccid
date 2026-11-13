//! `ccid security plan`: the calls that would publish the project's own advisories, as data.
//!
//! Nothing is sent. The plan names, per forge, whether a free advisory feature exists and, for GitHub,
//! the exact requests with their bodies. The applier (a person-approved step on the datacenter side,
//! with the relay identity's key) fills the `{owner}`, `{repo}` and `{ghsa_id}` placeholders, checks
//! that the copy is public, and sends one repository at a time. Only advisories from
//! `.ci/own-advisories.toml` appear here; the ledger of dependency advisories is never published.

use super::{load_own, Own, Severity, OWN};
use crate::Result;
use serde_json::{json, Value};
use std::path::Path;

/// GitHub's severity names differ from ours: it has `medium` where the ledger says `moderate`.
fn github_severity(severity: Severity) -> &'static str {
    match severity {
        Severity::Low => "low",
        Severity::Moderate => "medium",
        Severity::High => "high",
        Severity::Critical => "critical",
        // Refused when the file is read: an own advisory needs a real severity.
        Severity::Unknown => "low",
    }
}

fn github_create(entry: &Own) -> Value {
    let mut body = json!({
        "summary": entry.summary,
        "description": entry.description,
        "severity": github_severity(entry.severity),
        "vulnerabilities": [{
            "package": { "ecosystem": entry.ecosystem, "name": entry.package },
            "vulnerable_version_range": entry.affected,
            "patched_versions": entry.patched,
        }],
        "start_private_fork": false,
    });
    if let Some(cve) = &entry.cve {
        body["cve_id"] = json!(cve);
    }
    json!({
        "key": entry.key,
        "steps": [
            {
                "step": "look for an existing advisory with this summary and package; stop for this entry if one is there",
                "method": "GET",
                "path": "/repos/{owner}/{repo}/security-advisories?per_page=100",
            },
            {
                "step": "create the advisory as a draft",
                "method": "POST",
                "path": "/repos/{owner}/{repo}/security-advisories",
                "body": body,
            },
            {
                "step": "publish it",
                "method": "PATCH",
                "path": "/repos/{owner}/{repo}/security-advisories/{ghsa_id}",
                "body": { "state": "published" },
            },
        ],
    })
}

/// The plan for `repo` as pretty JSON.
pub fn plan_json(repo: &Path) -> Result<String> {
    let own = load_own(repo)?;
    let entries: Vec<Value> = own.advisories.iter().map(github_create).collect();
    let plan = json!({
        "schema": 1,
        "source": OWN,
        "advisories": own.advisories.len(),
        "sends": false,
        "forges": {
            "github": {
                "feature": "repository security advisories",
                "free": true,
                "publish": !entries.is_empty(),
                "requires": [
                    "the copy is a public repository",
                    "the relay identity's credential with the repository security advisories write permission (fine-grained token) or the repository_advisories:write scope (classic token)",
                    "the credential's account is an administrator or security manager of the copy: GitHub refuses a draft advisory from anyone else",
                    "one repository at a time, paced as the GitHub automation policy says",
                ],
                "settings": [{
                    "step": "switch on private vulnerability reporting (belongs to the copy settings plan, not to this one)",
                    "method": "PUT",
                    "path": "/repos/{owner}/{repo}/private-vulnerability-reporting",
                }],
                "entries": entries,
            },
            "gitlab": {
                "feature": "no published project advisory on a free plan; a maintainer of a public project can request a CVE id with a confidential issue in gitlab-org/cves (by hand, once per vulnerability, not part of this plan); reports go to a confidential issue, which is never relayed",
                "free": false,
                "publish": false,
            },
            "bitbucket": {
                "feature": "none: Bitbucket Cloud has no security advisories and no issues",
                "free": false,
                "publish": false,
            },
            "forgejo": {
                "feature": "none: the server has no advisory API",
                "free": false,
                "publish": false,
            },
        },
    });
    Ok(serde_json::to_string_pretty(&plan)? + "\n")
}

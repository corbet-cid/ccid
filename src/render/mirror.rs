//! The release mirror: the thin provider files that copy a Forgejo release to a downstream forge
//! when a release tag reaches it (pull model, `docs/release-mirror.md`).
//!
//! Rendered into every repository whose source forge can be told: the GitHub workflow
//! `.github/workflows/release-mirror.yml` and the GitLab pipeline `.gitlab-ci.yml`. The repository's
//! own forge location is read from its `origin` remote (or the manifest's `repository`); no
//! repository declares anything by hand.

use super::{metadata, Templates, CROW_MARKER};
use crate::{jobs, Result};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

pub(super) const GITHUB: &str = ".github/workflows/release-mirror.yml";
pub(super) const GITLAB: &str = ".gitlab-ci.yml";
pub(super) const BITBUCKET: &str = "bitbucket-pipelines.yml";
/// The GitHub pull-request check (`render_schema = 3`): the repository's verdict jobs on GitHub's own compute.
pub(super) const GITHUB_CHECK: &str = ".github/workflows/pr-check.yml";

/// The job the mirror platforms' checks and `job-run --mirror` have in common is named here, once.
/// Where the ccid release a mirror check runs is published: the tool's own repository (its package
/// metadata), on whichever forge serves it. The check reads the release channel and the binary
/// anonymously from there.
const TOOL_SOURCE: &str = env!("CARGO_PKG_REPOSITORY");

/// The verdict jobs of a repository that run on the mirror platforms' own compute, and those that do not.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Checks {
    /// Mirrored: the verdict jobs a pull request or merge request runs, in name order.
    pub eligible: Vec<String>,
    /// Not mirrored, with the reason: `(job, reason)`.
    pub forge_only: Vec<(String, String)>,
}

/// Why a verdict job is not mirrored (`SPEC` 3.5), or none when it is. A job is mirrored when it is a
/// verdict job (the caller's choice), runs on `linux`, uses the sentinel command `ccid:run-declared-checks`,
/// has no secrets, reads no sibling repository (whether a sibling is public is not known when rendering)
/// and does not opt out with `mirror = false`.
pub(crate) fn forge_only_reason(plan: &jobs::Plan) -> Option<String> {
    if let Some(reason) = &plan.mirror_optout {
        return Some(format!("mirror = false: {reason}"));
    }
    let label = plan.runs_on.as_deref().unwrap_or("linux");
    if label != "linux" {
        return Some(format!("runner label {label}"));
    }
    if !jobs::is_self_checks_command(&plan.command) {
        return Some(format!("command {}", plain_words(&plan.command)));
    }
    if !plan.secrets.is_empty() {
        return Some("it names secrets".to_owned());
    }
    if !plan.siblings.is_empty() {
        return Some("it reads sibling repositories".to_owned());
    }
    None
}

/// A command as words a step summary and a shell-quoted `echo` can carry: every other character is `?`.
fn plain_words(words: &[String]) -> String {
    let text: String = words
        .join(" ")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || " ._/:=+@-".contains(c) {
                c
            } else {
                '?'
            }
        })
        .collect();
    text.chars().take(120).collect()
}

impl Checks {
    /// Sort the verdict jobs `gating` of `plans` into mirrored and forge-only. A fork (it lists upstream
    /// files in `[surface] upstream`) mirrors nothing: its upstream CI stays what it is.
    pub(super) fn of(
        plans: &BTreeMap<String, jobs::Plan>,
        gating: &[String],
        fork: bool,
    ) -> Checks {
        let mut checks = Checks::default();
        if fork {
            return checks;
        }
        let mut names: Vec<&String> = gating.iter().collect();
        names.sort();
        for name in names {
            let Some(plan) = plans.get(name) else {
                continue;
            };
            match forge_only_reason(plan) {
                None => checks.eligible.push(name.clone()),
                Some(reason) => checks.forge_only.push((name.clone(), reason)),
            }
        }
        checks
    }

    /// The `render` and `render --check` notice for each forge-only verdict job.
    pub(super) fn notices(&self) -> Vec<String> {
        self.forge_only
            .iter()
            .map(|(job, reason)| format!("{job} runs on the forge only: {reason}"))
            .collect()
    }
}

/// `template` with its `CCID_CHECK_CORE` line replaced by the check script, indented to match, and the
/// check's own values filled in.
fn check_script(templates: &Templates, checks: &Checks) -> String {
    templates
        .mirror_check_core
        .replace("CCID_TOOL_SOURCE", TOOL_SOURCE)
        .replace("CCID_CHECK_JOBS", &checks.eligible.join(" "))
}

/// The step that tells a reader which verdict jobs this platform does not run, or nothing.
fn forge_only_step(checks: &Checks) -> String {
    if checks.forge_only.is_empty() {
        return String::new();
    }
    let mut lines = String::new();
    for (job, reason) in &checks.forge_only {
        lines.push_str(&format!(
            "            echo '- `{job}` runs on the forge only: {reason}'\n"
        ));
    }
    format!(
        "      - name: Say which checks run on the forge only\n        run: |\n          # shellcheck disable=SC2016\n          {{\n            echo '### Checks that run on the forge only'\n{lines}          }} >> \"$GITHUB_STEP_SUMMARY\"\n"
    )
}

/// The GitHub pull-request check workflow, or none when no verdict job is mirrored.
pub(super) fn github_check(templates: &Templates, checks: &Checks) -> Result<Option<String>> {
    if checks.eligible.is_empty() {
        return Ok(None);
    }
    let body = splice_marker(
        templates.pr_check_github,
        "CCID_CHECK_CORE",
        &check_script(templates, checks),
    )?;
    let mut out = String::new();
    let mut summary = false;
    for line in body.split_inclusive('\n') {
        if line.trim() == "CCID_CHECK_SUMMARY" {
            out.push_str(&forge_only_step(checks));
            summary = true;
        } else {
            out.push_str(line);
        }
    }
    if !summary {
        return Err(crate::failure(
            "The pull-request check template has no CCID_CHECK_SUMMARY line",
        ));
    }
    Ok(Some(out))
}

/// The GitLab pipeline rules that let a pipeline exist at all (merge requests, release tags, the
/// release mirror's own runs) and the merge-request check job when a verdict job is mirrored.
fn gitlab_check(templates: &Templates, checks: &Checks) -> Result<String> {
    let mut out = templates.mirror_check_gitlab_workflow.to_owned();
    if checks.eligible.is_empty() {
        return Ok(out);
    }
    out.push('\n');
    out.push_str(&splice_marker(
        templates.mirror_check_gitlab_job,
        "CCID_CHECK_CORE",
        &check_script(templates, checks),
    )?);
    Ok(out)
}

/// `body` of the GitLab release mirror with the check parts in front of the release job: the file keeps
/// one copy of the release job's text.
fn with_gitlab_checks(release: &str, check: &str) -> Result<String> {
    const JOB: &str = "\nrelease-mirror:\n";
    let at = release
        .find(JOB)
        .ok_or_else(|| crate::failure("The GitLab release mirror template lost its job"))?;
    let (head, job) = release.split_at(at + 1);
    Ok(format!("{head}{check}\n{job}"))
}

/// Bitbucket's Downloads are a paid-plan feature (a Free workspace answers HTTP 402), so the
/// Bitbucket pipeline is written and tested but not rendered until the plan has them.
const RENDER_BITBUCKET: bool = false;

/// Files this release introduced. `render --check` accepts their absence and reports them as
/// transitional, so a channel bump re-stales no repository; `render` writes them. Remove an
/// entry once the fleet has re-rendered (a later change of its template is a normal transition).
const INTRODUCED: &[&str] = &[GITHUB, GITLAB, BITBUCKET];

pub(super) fn introduced(relative: &Path) -> bool {
    INTRODUCED.iter().any(|path| Path::new(path) == relative)
}

/// `host[:port]` and `owner/repo` of a forge location written as an https, ssh or scp-like URL.
/// Credentials and the ssh port are dropped; anything else is refused.
fn parse_location(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let (authority, path, ssh) = if let Some(rest) = url.strip_prefix("https://") {
        let (authority, path) = rest.split_once('/')?;
        (authority, path, false)
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let (authority, path) = rest.split_once('/')?;
        (authority, path, true)
    } else if !url.contains("://") {
        let (authority, path) = url.split_once(':')?;
        (authority, path, true)
    } else {
        return None;
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if ssh { host.split(':').next()? } else { host };
    let host = host.to_ascii_lowercase();
    let host_ok = !host.is_empty()
        && host.split(':').count() <= 2
        && host.split(':').enumerate().all(|(index, part)| {
            !part.is_empty()
                && part.bytes().all(|b| {
                    if index == 0 {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-'
                    } else {
                        b.is_ascii_digit()
                    }
                })
        })
        && !host.starts_with('.')
        && !host.starts_with('-');
    if !host_ok {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let segment_ok = |segment: &str| {
        !segment.is_empty()
            && !segment.starts_with('.')
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    };
    if !segment_ok(owner) || !segment_ok(name) {
        return None;
    }
    Some((host, format!("{owner}/{name}")))
}

fn origin(repo: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    let url = url.trim();
    (!url.is_empty()).then(|| url.to_owned())
}

/// The host the committed GitHub workflow already names for `path`, so one repository keeps one
/// spelling of its forge whoever renders it (a developer's `origin` and a runner's differ).
fn recorded_host(repo: &Path, path: &str) -> Option<String> {
    let text = std::fs::read_to_string(repo.join(GITHUB)).ok()?;
    if !text.starts_with(CROW_MARKER) {
        return None;
    }
    let line = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("MIRROR_SOURCE='"))?;
    let (host, recorded) = parse_location(line.strip_suffix('\'')?)?;
    (recorded == path).then_some(host)
}

/// The identity URL of this repository on its forge, `https://<host>/<owner>/<repo>`, or none
/// when it cannot be told (a directory without a recognizable `origin`).
pub(super) fn source(repo: &Path, declared: Option<&str>) -> Option<String> {
    let (host, path) = declared
        .and_then(parse_location)
        .or_else(|| origin(repo).and_then(|url| parse_location(&url)))?;
    let host = recorded_host(repo, &path).unwrap_or(host);
    Some(format!("https://{host}/{path}"))
}

fn foreign(repo: &Path, relative: &str) -> Result<bool> {
    let path = repo.join(relative);
    if metadata(&path)?.is_none() {
        return Ok(false);
    }
    Ok(!std::fs::read_to_string(&path).is_ok_and(|text| text.starts_with(CROW_MARKER)))
}

/// `template` with its `CCID_MIRROR_CORE` line replaced by the core script, indented to match.
fn splice(template: &str, core: &str) -> Result<String> {
    splice_marker(template, "CCID_MIRROR_CORE", core)
}

/// `template` with its `marker` line replaced by `core`, indented to match.
fn splice_marker(template: &str, marker: &str, core: &str) -> Result<String> {
    let mut out = String::new();
    let mut found = false;
    for line in template.split_inclusive('\n') {
        if line.trim() == marker {
            let indent = &line[..line.len() - line.trim_start().len()];
            for script in core.lines() {
                if !script.is_empty() {
                    out.push_str(indent);
                    out.push_str(script);
                }
                out.push('\n');
            }
            found = true;
        } else {
            out.push_str(line);
        }
    }
    if !found {
        return Err(crate::failure(format!(
            "A mirror template has no {marker} line"
        )));
    }
    Ok(out)
}

/// The provider files for `source`. A provider file this release does not own (a hand-written
/// `.gitlab-ci.yml`) is left alone and its mirror is not rendered.
///
/// `replace_legacy` (`render_schema = 3`): a hand-written file of a retired family (the remote scheduler's
/// pipeline) is not foreign; the render replaces it with the mirror file.
///
/// `checks` (`render_schema = 3`): the GitLab file also carries the pipeline rules and the merge-request
/// check. Schemas 1 and 2 pass none and render the release mirror alone, as before.
pub(super) fn outputs(
    repo: &Path,
    source: &str,
    templates: &Templates,
    replace_legacy: bool,
    checks: Option<&Checks>,
) -> Result<BTreeMap<PathBuf, String>> {
    let core = templates.mirror_core.replace("CCID_MIRROR_SOURCE", source);
    let mut files = BTreeMap::new();
    files.insert(
        PathBuf::from(GITHUB),
        splice(templates.mirror_github, &core)?,
    );
    let gitlab_body = match checks {
        None => templates.mirror_gitlab.to_owned(),
        Some(checks) => {
            with_gitlab_checks(templates.mirror_gitlab, &gitlab_check(templates, checks)?)?
        }
    };
    let mut gitlab = vec![(GITLAB, gitlab_body.as_str())];
    if RENDER_BITBUCKET {
        gitlab.push((BITBUCKET, templates.mirror_bitbucket));
    }
    for (relative, template) in gitlab {
        if foreign(repo, relative)?
            && !(replace_legacy && super::surface::replaceable(repo, relative)?)
        {
            eprintln!(
                "ccid render: {relative} is not generated here, so its release mirror is not rendered"
            );
            continue;
        }
        files.insert(PathBuf::from(relative), splice(template, &core)?);
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_are_read_from_https_ssh_and_scp_urls() {
        let want = Some(("forge.example".to_owned(), "org/repo".to_owned()));
        for url in [
            "https://forge.example/org/repo",
            "https://forge.example/org/repo.git",
            "https://user:secret@forge.example/org/repo.git",
            "https://FORGE.example/org/repo/",
            "ssh://git@forge.example:2222/org/repo.git",
            "git@forge.example:org/repo.git",
            "forge.example:org/repo",
        ] {
            assert_eq!(parse_location(url), want, "{url}");
        }
        assert_eq!(
            parse_location("https://forge.example:8443/org/repo"),
            Some(("forge.example:8443".to_owned(), "org/repo".to_owned()))
        );
    }

    #[test]
    fn locations_that_could_reach_a_shell_or_another_path_are_refused() {
        for url in [
            "",
            "file:///srv/org/repo.git",
            "/srv/org/repo.git",
            "https://forge.example/org",
            "https://forge.example/org/repo/extra",
            "https://forge.example/org/re'po",
            "https://forge.example/org/$(id)",
            "https://forge.example/../repo",
            "https://-host/org/repo",
            "https://ho st/org/repo",
            "https://forge.example:port/org/repo",
        ] {
            assert_eq!(parse_location(url), None, "{url}");
        }
    }
}

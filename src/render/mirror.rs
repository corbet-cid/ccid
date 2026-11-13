//! The release mirror: the thin provider files that copy a Forgejo release to a downstream forge
//! when a release tag reaches it (pull model, `docs/release-mirror.md`).
//!
//! Rendered into every repository whose source forge can be told: the GitHub workflow
//! `.github/workflows/release-mirror.yml` and the GitLab pipeline `.gitlab-ci.yml`. The repository's
//! own forge location is read from its `origin` remote (or the manifest's `repository`); no
//! repository declares anything by hand.

use super::{metadata, Templates, CROW_MARKER};
use crate::Result;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

pub(super) const GITHUB: &str = ".github/workflows/release-mirror.yml";
pub(super) const GITLAB: &str = ".gitlab-ci.yml";
pub(super) const BITBUCKET: &str = "bitbucket-pipelines.yml";

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
    let mut out = String::new();
    let mut found = false;
    for line in template.split_inclusive('\n') {
        if line.trim() == "CCID_MIRROR_CORE" {
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
        return Err(crate::failure("A release mirror template has no core line"));
    }
    Ok(out)
}

/// The provider files for `source`. A provider file this release does not own (a hand-written
/// `.gitlab-ci.yml`) is left alone and its mirror is not rendered.
pub(super) fn outputs(
    repo: &Path,
    source: &str,
    templates: &Templates,
) -> Result<BTreeMap<PathBuf, String>> {
    let core = templates.mirror_core.replace("CCID_MIRROR_SOURCE", source);
    let mut files = BTreeMap::new();
    files.insert(
        PathBuf::from(GITHUB),
        splice(templates.mirror_github, &core)?,
    );
    let mut gitlab = vec![(GITLAB, templates.mirror_gitlab)];
    if RENDER_BITBUCKET {
        gitlab.push((BITBUCKET, templates.mirror_bitbucket));
    }
    for (relative, template) in gitlab {
        if foreign(repo, relative)? {
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

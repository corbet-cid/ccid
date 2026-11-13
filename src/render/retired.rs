//! The CI files of the mirror forges, retired (operating model R6, R7).
//!
//! Until this release ccid rendered thin provider files into a repository so that its copy on a mirror forge ran CI
//! by itself: the release mirror (`.github/workflows/release-mirror.yml` and a job in `.gitlab-ci.yml`) and, at
//! `render_schema = 3`, the pull-request and merge-request check (`.github/workflows/pr-check.yml`, a job and the
//! pipeline rules in `.gitlab-ci.yml`), and, for a JSR package, the GitHub publisher
//! (`.github/workflows/jsr-publish.yml`). The operating model rules all of them out: no CI runs on any mirror forge,
//! whoever triggers it (R6), and none of our actions may start a mirror run (R7). A release reaches a copy by the
//! datacenter-side upload (R45); a registry that a forge offers only through its CI (JSR, through GitHub's OIDC) is
//! published from our side with a key that needs no renewal, or not at all (R46); an outsider checks a change
//! locally, and our CI runs it only after we took it in (R36).
//!
//! So ccid renders none of these files, for any class and any schema. A file it rendered earlier is retired:
//! `render` removes it. At `render_schema = 3` `render --check` names it as drift ("retired, render removes it"); at
//! schemas 1 and 2 it accepts the file's presence as a declared transition (a notice and the `transitional` list), so
//! a channel bump re-stales no repository, until a later release drops the tolerance. A hand-written file at one of
//! these paths is not ours: `render_schema = 3` accounts for it as an unknown file, older schemas leave it.

use super::{metadata, CROW_MARKER};
use crate::Result;
use std::path::{Path, PathBuf};

/// The paths ccid rendered for the CI of the mirror forges.
pub(super) const FILES: [&str; 5] = [
    ".github/workflows/release-mirror.yml",
    ".github/workflows/pr-check.yml",
    ".github/workflows/jsr-publish.yml",
    ".gitlab-ci.yml",
    "bitbucket-pipelines.yml",
];

/// Why a check at schemas 1 and 2 still accepts the presence of a retired file: the files stay until the next render
/// removes them, so the channel bump that retires them re-stales no repository. The tolerance is dropped in a later
/// batch, once the fleet has been swept.
pub(super) const TRANSITION: &str = "a mirror forge runs no CI, R6 and R7";

/// What replaces the files, as a retired file's report names it.
pub(super) const REPLACEMENT: &str = "nothing: a mirror forge runs no CI (R6, R7); a release reaches a copy by the datacenter-side upload (R45), a registry by our own key (R46)";

/// The retired files of `repo` that ccid rendered (they carry the render marker), sorted. A symlink is never
/// followed, and a hand-written file at one of the paths is not listed.
pub(super) fn rendered(repo: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for relative in FILES {
        let path = repo.join(relative);
        let Some(info) = metadata(&path)? else {
            continue;
        };
        if info.is_file()
            && !info.file_type().is_symlink()
            && std::fs::read(&path)?.starts_with(CROW_MARKER.as_bytes())
        {
            found.push(PathBuf::from(relative));
        }
    }
    found.sort();
    Ok(found)
}

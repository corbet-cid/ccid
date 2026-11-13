//! Security and dependency visibility (operating model R47), rendered rather than hand-written.
//!
//! One fixed text, no data files and no network: `render` keeps a marked block in the README (a status
//! line) and in the policy file GitHub reads (`.github/SECURITY.md`, `SECURITY.md`, `docs/SECURITY.md`;
//! a root `SECURITY.md` is created when there is none), and `ccid release` appends a section to the
//! release notes. A person owns the rest of each file; ccid owns only the text between the markers.
//! The text names no contact address and no forge, so it renders identically on every forge.

pub mod blocks;

use crate::Result;
use blocks::{Place, BEGIN_PREFIX, END};
use std::{
    fs,
    path::{Path, PathBuf},
};

const README: &str = "README.md";
const POLICY_CANDIDATES: [&str; 3] = [".github/SECURITY.md", "SECURITY.md", "docs/SECURITY.md"];
const POLICY_TITLE: &str = "# Security policy";

/// What `render` does with one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePlan {
    /// Relative to the repository root.
    pub path: PathBuf,
    /// The file as it is, or none when it does not exist.
    pub current: Option<String>,
    /// The file as `render` leaves it.
    pub expected: String,
    /// The file carries no block yet: a `--check` accepts that as a declared transition.
    pub introduced: bool,
}

impl FilePlan {
    pub fn drifted(&self) -> bool {
        self.current.as_deref() != Some(self.expected.as_str())
    }
}

fn begin_line() -> String {
    format!("{BEGIN_PREFIX} (rendered by ccid render; do not edit by hand) -->")
}

/// The one-line status for the README.
pub fn readme_block(policy: &str) -> String {
    format!(
        "{}\n**Security and dependencies:** kept up to date automatically (Renovate, with OSV vulnerability alerts); \
         updates are merged when their tests pass. See the [security policy]({}).\n{END}",
        begin_line(),
        policy
    )
}

/// The policy text, the same on every forge.
pub fn policy_block() -> String {
    format!(
        "{begin}\n\
## How dependencies and security fixes are handled\n\n\
- Dependencies are checked by [Renovate](https://docs.renovatebot.com/), with vulnerability alerts from \
[OSV](https://osv.dev/).\n\
- Updates are grouped, and a group is merged into the primary branch when its tests are green; a red \
update is not merged and stays visible as an issue until it is fixed.\n\
- Renovate's dependency dashboard issue lists every pending, blocked and merged update.\n\n\
## Copies run no CI\n\n\
This repository may be mirrored to other forges. The copies run no CI: everything is built and tested \
once, on the primary forge, so a copy never produces a different result and no compute is spent twice. \
Releases are built there and published to the copies.\n\n\
## Reporting a problem\n\n\
Open an issue in this repository's issue tracker on the forge you are using. On a public GitHub copy \
where private vulnerability reporting is enabled, use the Security tab and choose \"Report a \
vulnerability\" instead. Describe the impact; leave out working exploit details from a public issue.\n\
{END}",
        begin = begin_line()
    )
}

/// The release-notes section, appended by `ccid release`.
pub fn release_section() -> String {
    "## Security and dependencies\n\n\
Dependencies are kept current by Renovate with OSV vulnerability alerts; updates are merged when their \
tests are green. See the security policy for how to report a problem."
        .to_owned()
}

fn read(repo: &Path, relative: &str) -> Result<Option<String>> {
    let path = repo.join(relative);
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(crate::failure(format!(
            "{relative} is a symbolic link: ccid keeps its security block only in a regular file"
        )));
    }
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn plan_one(
    repo: &Path,
    relative: &str,
    block: &str,
    place: Place,
    create: Option<String>,
) -> Result<Option<FilePlan>> {
    Ok(match read(repo, relative)? {
        Some(current) => {
            let applied = blocks::apply(&current, block, place, relative)?;
            Some(FilePlan {
                path: PathBuf::from(relative),
                introduced: !applied.had_block,
                expected: applied.text,
                current: Some(current),
            })
        }
        None => create.map(|expected| FilePlan {
            path: PathBuf::from(relative),
            current: None,
            expected,
            introduced: true,
        }),
    })
}

/// The README and policy plans for a repository. A README that does not exist gets no block.
pub fn plan_files(repo: &Path) -> Result<Vec<FilePlan>> {
    let mut plans = Vec::new();
    let policy = POLICY_CANDIDATES
        .iter()
        .find(|candidate| repo.join(candidate).is_file())
        .copied()
        .unwrap_or(POLICY_CANDIDATES[1]);
    plans.extend(plan_one(
        repo,
        README,
        &readme_block(policy),
        Place::AfterTitle,
        None,
    )?);
    let block = policy_block();
    let create = format!("{POLICY_TITLE}\n\n{block}\n");
    plans.extend(plan_one(repo, policy, &block, Place::End, Some(create))?);
    Ok(plans)
}

#[cfg(test)]
mod tests;

//! The files `render` keeps current: the README status block and the SECURITY.md policy block.
//!
//! A person owns both files; ccid owns the text between the markers (`blocks`). A README that does
//! not exist gets no block (it is not created). A SECURITY.md that does not exist is created, with a
//! title and the block. A SECURITY.md that a person wrote keeps all its text and gains the block at
//! the end. GitHub reads `.github/SECURITY.md`, then `SECURITY.md`, then `docs/SECURITY.md`; the
//! block goes into the one it reads, and a new file is the root one.

use super::blocks::{self, Place};
use super::text::{policy_block, readme_block};
use super::{load_ledger, load_own};
use crate::{failure, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

const README: &str = "README.md";
/// Where GitHub looks for the policy, in its order of precedence.
const POLICY_CANDIDATES: [&str; 3] = [".github/SECURITY.md", "SECURITY.md", "docs/SECURITY.md"];
/// The file a repository without any policy gets.
const POLICY_DEFAULT: &str = "SECURITY.md";
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
    /// The file carries no block yet: a `--check` accepts that as a declared transition (the next
    /// render adds it) instead of calling it drift, like any file a ccid release introduces.
    pub introduced: bool,
}

impl FilePlan {
    pub fn drifted(&self) -> bool {
        self.current.as_deref() != Some(self.expected.as_str())
    }
}

/// Read a text file of the repository, or none when it is absent. A symlink or a non-file is refused.
fn read(repo: &Path, relative: &str) -> Result<Option<String>> {
    let path = repo.join(relative);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let bytes = fs::read(&path)?;
            String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| failure(format!("{relative} is not valid UTF-8")))
        }
        Ok(_) => Err(failure(format!(
            "Unsafe file: {relative} is not a regular file"
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The policy file the repository already has, else the default.
fn policy_path(repo: &Path) -> Result<&'static str> {
    for candidate in POLICY_CANDIDATES {
        if read(repo, candidate)?.is_some() {
            return Ok(candidate);
        }
    }
    Ok(POLICY_DEFAULT)
}

/// The link from the README to the policy file.
fn link(policy: &str) -> &str {
    policy
}

/// What `render` does with the README and the policy file of `repo`.
pub fn plan_files(repo: &Path) -> Result<Vec<FilePlan>> {
    let ledger = load_ledger(repo)?;
    let own = load_own(repo)?;
    let policy = policy_path(repo)?;
    let mut plans = Vec::new();

    if let Some(current) = read(repo, README)? {
        let block = readme_block(&ledger, &own, link(policy));
        let applied = blocks::apply(&current, &block, Place::AfterTitle, README)?;
        plans.push(FilePlan {
            path: PathBuf::from(README),
            introduced: !applied.had_block,
            expected: applied.text,
            current: Some(current),
        });
    }

    let block = policy_block(&ledger, &own);
    match read(repo, policy)? {
        Some(current) => {
            let applied = blocks::apply(&current, &block, Place::End, policy)?;
            plans.push(FilePlan {
                path: PathBuf::from(policy),
                introduced: !applied.had_block,
                expected: applied.text,
                current: Some(current),
            });
        }
        None => plans.push(FilePlan {
            path: PathBuf::from(policy),
            current: None,
            expected: format!("{POLICY_TITLE}\n\n{block}\n"),
            introduced: true,
        }),
    }
    Ok(plans)
}

//! A push that ended in an error is checked against the forge before it is called a failure.
//!
//! When a request on a reused HTTP keep-alive connection dies before any response byte, libcurl sends it again on a fresh connection, the
//! `git-receive-pack` POST included. If the first push was already applied, the second one fails with `cannot lock ref ... is at <pushed sha> but
//! expected <old>` although the push landed. A stall of the line also resets a connection before the push reaches the forge. Neither is a forge bug,
//! so after any push error the forge is asked what it holds for the branch:
//!
//! * the pushed commit: the push landed, success;
//! * the value from before the push: it did not arrive, one retry after a short backoff;
//! * anything else, or a forge that cannot be asked: a real error, reported as such.
use super::*;
use std::time::Duration;

/// How long to wait before the one retry and before asking the forge a second time.
const BACKOFF: Duration = Duration::from_secs(3);

/// What a verified push needs from the forge; the tests stand in for it.
pub(super) trait Remote {
    /// The commit the forge holds for the branch (`None`: no such branch); an error when the forge cannot be asked.
    fn tip(&self) -> Result<Option<String>>;
    fn push(&self) -> Result<()>;
    /// The short backoff before a retry.
    fn pause(&self);
}

/// Ask for the branch; one repeat after the backoff, because the line that dropped the push may still be stalled.
fn ask(remote: &dyn Remote) -> Result<Option<String>> {
    remote.tip().or_else(|_| {
        remote.pause();
        remote.tip()
    })
}

/// Push `sha` and verify any error against the forge: `Ok` when the forge holds `sha` afterwards.
pub(super) fn push_verified(remote: &dyn Remote, sha: &str) -> Result<()> {
    // `Err`: the state before the push is unknown, so an unchanged branch can never be told from a foreign write.
    let old = ask(remote).ok();
    let mut retried = false;
    loop {
        let error = match remote.push() {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let tip = ask(remote).map_err(|asked| {
            format!(
                "the push failed ({error}) and the forge cannot be asked what it holds ({asked}): the outcome is unknown"
            )
        })?;
        if tip.as_deref() == Some(sha) {
            eprintln!("push: git reported an error ({error}) but the forge holds {sha}: the push landed (verified with git ls-remote)");
            return Ok(());
        }
        if old.as_ref() != Some(&tip) {
            return Err(format!(
                "the push failed ({error}) and the forge holds {} for the branch, neither the pushed {sha} nor the value from before the push ({}): a real error",
                tip.as_deref().unwrap_or("no branch"),
                old.as_ref().map_or("unknown", |old| old.as_deref().unwrap_or("no branch")),
            )
            .into());
        }
        if retried {
            return Err(format!("the push failed twice ({error}) and the branch on the forge is unchanged: a real error").into());
        }
        retried = true;
        eprintln!("push: the push did not arrive ({error}; the forge still holds the old value): retrying once");
        remote.pause();
    }
}

/// A branch of a repository on the forge, pushed from a local checkout with git.
pub(super) struct GitRemote<'a> {
    pub dir: &'a Path,
    pub url: &'a str,
    pub branch: &'a str,
    /// The commit pushed to the branch.
    pub sha: &'a str,
    pub backoff: Duration,
}

impl Remote for GitRemote<'_> {
    fn tip(&self) -> Result<Option<String>> {
        let reference = format!("refs/heads/{}", self.branch);
        let listing = output(
            &strings(&["git", "ls-remote", self.url, &reference]),
            Some(self.dir),
            None,
        )?;
        Ok(parse_tip(&String::from_utf8(listing)?, &reference))
    }
    fn push(&self) -> Result<()> {
        // The commit itself, not the branch name: the commit is what is verified afterwards.
        output(
            &strings(&[
                "git",
                "push",
                "--quiet",
                "--force",
                self.url,
                &format!("{}:refs/heads/{}", self.sha, self.branch),
            ]),
            Some(self.dir),
            None,
        )
        .map(|_| ())
    }
    fn pause(&self) {
        std::thread::sleep(self.backoff);
    }
}

/// The commit `git ls-remote` lists for exactly `reference`.
pub(super) fn parse_tip(listing: &str, reference: &str) -> Option<String> {
    listing
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(sha, name)| *name == reference && exact_sha(sha))
        .map(|(sha, _)| sha.to_owned())
}

/// Force-push the local `branch` of `dir` to `url` and verify the outcome against the forge.
pub(super) fn push_branch(dir: &Path, url: &str, branch: &str) -> Result<()> {
    let sha = git(
        dir,
        &["rev-parse", "--verify", &format!("{branch}^{{commit}}")],
    )?;
    push_verified(
        &GitRemote {
            dir,
            url,
            branch,
            sha: &sha,
            backoff: BACKOFF,
        },
        &sha,
    )
}

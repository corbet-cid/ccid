//! A push error is verified against the forge: the pushed commit means it landed, the old value means it did not arrive (one retry), anything else
//! is a real error.
use super::*;
use crate::submission::verified_push::{parse_tip, push_verified, GitRemote, Remote};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    time::Duration,
};

const OLD_TIP: &str = "1111111111111111111111111111111111111111";
const FOREIGN_TIP: &str = "2222222222222222222222222222222222222222";

/// What happens to one push.
#[derive(Clone, Copy)]
enum Step {
    /// Applied and answered.
    Ok,
    /// Applied, then the connection dies before any response byte: git reports an error.
    AppliedAnswerLost,
    /// Never arrives (the connection is reset first): git reports an error, the branch is unchanged.
    Lost,
    /// Another writer moves the branch first: git reports an error.
    Foreign,
}

/// A forge with scripted pushes and a counter for everything the code does to it.
struct Forge {
    tip: Cell<Option<&'static str>>,
    steps: RefCell<VecDeque<Step>>,
    pushes: Cell<usize>,
    pauses: Cell<usize>,
    /// How many of the next questions fail (a stalled line), whenever they are asked.
    stalled: Cell<usize>,
    /// Every question after the first push fails.
    down_after_push: Cell<bool>,
}
impl Forge {
    fn new(tip: Option<&'static str>, steps: &[Step]) -> Self {
        Self {
            tip: Cell::new(tip),
            steps: RefCell::new(steps.iter().copied().collect()),
            pushes: Cell::new(0),
            pauses: Cell::new(0),
            stalled: Cell::new(0),
            down_after_push: Cell::new(false),
        }
    }
}
impl Remote for Forge {
    fn tip(&self) -> Result<Option<String>> {
        if self.stalled.get() > 0 || (self.down_after_push.get() && self.pushes.get() > 0) {
            self.stalled.set(self.stalled.get().saturating_sub(1));
            return Err("Recv failure: Connection reset by peer".into());
        }
        Ok(self.tip.get().map(str::to_owned))
    }
    fn push(&self) -> Result<()> {
        self.pushes.set(self.pushes.get() + 1);
        match self.steps.borrow_mut().pop_front().unwrap_or(Step::Ok) {
            Step::Ok => {
                self.tip.set(Some(SHA));
                Ok(())
            }
            Step::AppliedAnswerLost => {
                self.tip.set(Some(SHA));
                Err("git failed (exit Some(128))".into())
            }
            Step::Lost => Err("git failed (exit Some(128))".into()),
            Step::Foreign => {
                self.tip.set(Some(FOREIGN_TIP));
                Err("git failed (exit Some(1))".into())
            }
        }
    }
    fn pause(&self) {
        self.pauses.set(self.pauses.get() + 1);
    }
}

fn message(result: Result<()>) -> String {
    result.expect_err("the push must fail").to_string()
}

#[test]
fn a_clean_push_is_one_push() {
    let forge = Forge::new(Some(OLD_TIP), &[Step::Ok]);
    push_verified(&forge, SHA).unwrap();
    assert_eq!((forge.pushes.get(), forge.pauses.get()), (1, 0));
}

#[test]
fn an_applied_push_whose_answer_was_lost_counts_as_landed_and_is_not_sent_again() {
    for old in [Some(OLD_TIP), None] {
        let forge = Forge::new(old, &[Step::AppliedAnswerLost, Step::Ok]);
        push_verified(&forge, SHA).unwrap();
        assert_eq!(
            forge.pushes.get(),
            1,
            "an applied push must not be sent again"
        );
        assert_eq!(forge.tip.get(), Some(SHA));
    }
}

#[test]
fn a_push_that_never_arrived_is_retried_once_after_a_backoff() {
    for old in [Some(OLD_TIP), None] {
        let forge = Forge::new(old, &[Step::Lost, Step::Ok]);
        push_verified(&forge, SHA).unwrap();
        assert_eq!((forge.pushes.get(), forge.pauses.get()), (2, 1));
        assert_eq!(forge.tip.get(), Some(SHA));
    }
}

#[test]
fn two_pushes_that_never_arrived_are_a_real_error_and_there_is_no_third() {
    let forge = Forge::new(Some(OLD_TIP), &[Step::Lost, Step::Lost, Step::Ok]);
    let text = message(push_verified(&forge, SHA));
    assert!(text.contains("failed twice"), "{text}");
    assert_eq!(forge.pushes.get(), 2);
    assert_eq!(forge.tip.get(), Some(OLD_TIP));
}

#[test]
fn a_value_that_is_neither_the_pushed_commit_nor_the_old_one_is_a_real_error_without_retry() {
    let forge = Forge::new(Some(OLD_TIP), &[Step::Foreign, Step::Ok]);
    let text = message(push_verified(&forge, SHA));
    assert!(
        text.contains("a real error") && text.contains(FOREIGN_TIP),
        "{text}"
    );
    assert_eq!((forge.pushes.get(), forge.pauses.get()), (1, 0));
}

#[test]
fn a_forge_that_cannot_be_asked_after_the_error_leaves_the_outcome_unknown() {
    // the push was applied, but neither question after the error gets through: success cannot be claimed
    let forge = Forge::new(Some(OLD_TIP), &[Step::AppliedAnswerLost]);
    forge.down_after_push.set(true);
    let text = message(push_verified(&forge, SHA));
    assert!(text.contains("the outcome is unknown"), "{text}");
    assert_eq!(forge.pushes.get(), 1);
}

#[test]
fn a_question_that_stalls_once_is_asked_again_after_the_backoff() {
    // the line stalls on the question before the push: the repeat gets through and the old value is known
    let forge = Forge::new(Some(OLD_TIP), &[Step::Lost, Step::Ok]);
    forge.stalled.set(1);
    push_verified(&forge, SHA).unwrap();
    assert_eq!(forge.pushes.get(), 2);
    assert_eq!(
        forge.pauses.get(),
        2,
        "the repeated question and the retry each wait"
    );
}

#[test]
fn the_unknown_state_before_the_push_never_passes_for_unchanged() {
    // both questions before the push fail; the push is lost and the forge then holds the old value, which cannot be told from a foreign write
    let forge = Forge::new(Some(OLD_TIP), &[Step::Lost, Step::Ok]);
    forge.stalled.set(2);
    let text = message(push_verified(&forge, SHA));
    assert!(
        text.contains("a real error") && text.contains("unknown"),
        "{text}"
    );
    assert_eq!(forge.pushes.get(), 1);
}

#[test]
fn ls_remote_output_is_read_for_exactly_the_branch() {
    let listing =
        format!("{OLD_TIP}\trefs/heads/ci/verdict-rollout\n{FOREIGN_TIP}\trefs/heads/other\n");
    assert_eq!(
        parse_tip(&listing, "refs/heads/ci/verdict-rollout").as_deref(),
        Some(OLD_TIP)
    );
    assert_eq!(parse_tip(&listing, "refs/heads/ci/verdict"), None);
    assert_eq!(parse_tip("", "refs/heads/x"), None);
    assert_eq!(parse_tip("short\trefs/heads/x\n", "refs/heads/x"), None);
}

/// The same cases against real git: a bare repository with hooks stands for the forge. Per push attempt its plan says what happens: `ok`; `hangup` (the
/// push is applied, then receive-pack is killed after its ref transaction committed, so the client never gets an answer); `reject` (refused before
/// any ref changes).
#[cfg(unix)]
mod real_git {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const BRANCH: &str = "ci/push-test";

    struct Setup {
        root: tempfile::TempDir,
        work: PathBuf,
        bare: PathBuf,
        sha: String,
    }
    impl Setup {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let work = root.path().join("work");
            let bare = root.path().join("forge.git");
            fs::create_dir(&work).unwrap();
            git(&work, &["init", "-q", "--initial-branch", BRANCH]).unwrap();
            git(&work, &["config", "user.name", "fixture"]).unwrap();
            git(&work, &["config", "user.email", "fixture@example.invalid"]).unwrap();
            git(&work, &["config", "commit.gpgsign", "false"]).unwrap();
            git(&work, &["commit", "-q", "--allow-empty", "-m", "pushed"]).unwrap();
            let sha = git(&work, &["rev-parse", "HEAD"]).unwrap();
            git(
                root.path(),
                &["init", "-q", "--bare", &bare.to_string_lossy()],
            )
            .unwrap();
            Self {
                root,
                work,
                bare,
                sha,
            }
        }
        fn plan(&self, steps: &[&str]) {
            let dir = self.root.path();
            let counter = dir.join("count");
            let armed = dir.join("armed");
            let plan = dir.join("plan");
            fs::write(
                &plan,
                steps.iter().map(|s| format!("{s}\n")).collect::<String>(),
            )
            .unwrap();
            let pre_receive = format!(
                "#!/bin/sh\nn=$(( $(cat '{c}' 2>/dev/null || echo 0) + 1 )); echo \"$n\" >'{c}'\ncase \"$(sed -n \"${{n}}p\" '{p}')\" in\n  hangup) touch '{a}' ;;\n  reject) echo 'refused (plan)' >&2; exit 1 ;;\nesac\nexit 0\n",
                c = counter.display(),
                p = plan.display(),
                a = armed.display(),
            );
            let reference_transaction = format!(
                "#!/bin/sh\nif [ \"$1\" = committed ] && [ -e '{a}' ]; then rm -f '{a}'; kill -9 $PPID; fi\nexit 0\n",
                a = armed.display(),
            );
            for (name, body) in [
                ("pre-receive", pre_receive),
                ("reference-transaction", reference_transaction),
            ] {
                let hook = self.bare.join("hooks").join(name);
                fs::write(&hook, body).unwrap();
                fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        fn pushes(&self) -> usize {
            fs::read_to_string(self.root.path().join("count"))
                .map_or(0, |n| n.trim().parse().unwrap())
        }
        fn held(&self) -> Option<String> {
            git(
                &self.bare,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{BRANCH}"),
                ],
            )
            .ok()
        }
        fn push(&self, url: &str) -> Result<()> {
            let remote = GitRemote {
                dir: &self.work,
                url,
                branch: BRANCH,
                sha: &self.sha,
                backoff: Duration::ZERO,
            };
            push_verified(&remote, &self.sha)
        }
    }

    #[test]
    fn the_branch_wrapper_pushes_the_commit_of_the_local_branch() {
        let forge = Setup::new();
        forge.plan(&["ok"]);
        crate::submission::verified_push::push_branch(
            &forge.work,
            &forge.bare.to_string_lossy(),
            BRANCH,
        )
        .unwrap();
        assert_eq!(forge.held().as_deref(), Some(forge.sha.as_str()));
        assert_eq!(forge.pushes(), 1);
    }

    #[test]
    fn a_push_applied_before_the_connection_died_is_landed_and_not_sent_again() {
        let forge = Setup::new();
        forge.plan(&["hangup", "ok"]);
        forge.push(&forge.bare.to_string_lossy()).unwrap();
        assert_eq!(forge.held().as_deref(), Some(forge.sha.as_str()));
        assert_eq!(forge.pushes(), 1);
    }

    #[test]
    fn a_push_that_never_arrived_is_retried_once() {
        let forge = Setup::new();
        forge.plan(&["reject", "ok"]);
        forge.push(&forge.bare.to_string_lossy()).unwrap();
        assert_eq!(forge.held().as_deref(), Some(forge.sha.as_str()));
        assert_eq!(forge.pushes(), 2);
    }

    #[test]
    fn two_refused_pushes_are_a_real_error() {
        let forge = Setup::new();
        forge.plan(&["reject", "reject", "ok"]);
        let text = message(forge.push(&forge.bare.to_string_lossy()));
        assert!(text.contains("failed twice"), "{text}");
        assert_eq!(forge.held(), None);
        assert_eq!(forge.pushes(), 2);
    }

    #[test]
    fn a_forge_that_cannot_be_reached_is_an_error_that_says_the_outcome_is_unknown() {
        let forge = Setup::new();
        let missing = forge.root.path().join("no-such-forge.git");
        let text = message(forge.push(&missing.to_string_lossy()));
        assert!(text.contains("the outcome is unknown"), "{text}");
    }
}

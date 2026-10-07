use super::*;
use std::os::unix::fs::PermissionsExt;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

struct Reporter {
    dir: tempfile::TempDir,
    sha: String,
}
/// One shared, never-rewritten reporter script: writing an executable while
/// parallel tests fork would race with `exec` (ETXTBSY). Each test gets its
/// own directory holding a symlink to it, which is where the call log lives.
fn shared_script() -> &'static Path {
    static SCRIPT: std::sync::OnceLock<(tempfile::TempDir, PathBuf)> = std::sync::OnceLock::new();
    let (_, path) = SCRIPT.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("status");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/calls.log\"\nexit $(cat \"$(dirname \"$0\")/exit\" 2>/dev/null || echo 0)\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        (dir, script)
    });
    path
}
fn reporter(exit: i32) -> Reporter {
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(shared_script(), dir.path().join("status")).unwrap();
    fs::write(dir.path().join("exit"), exit.to_string()).unwrap();
    let sha = format!("{:x}", Sha256::digest(fs::read(shared_script()).unwrap()));
    Reporter { dir, sha }
}
impl Reporter {
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
    fn options(&self, state_dir: Option<&Path>, job: &str, gating: &str, state: State) -> Options {
        Options {
            commit: COMMIT.into(),
            job: job.into(),
            gating: gating.into(),
            state,
            url: "https://ci.example.invalid/run/7".into(),
            started: 5,
            binary: self.dir.path().join("status"),
            binary_sha256: self.sha.clone(),
            state_dir: state_dir.map(Path::to_path_buf),
            repo: Some("owner/project".into()),
        }
    }
    /// The verdict state posted by the most recent invocation, if any.
    fn last_verdict(&self) -> Option<String> {
        self.calls()
            .iter()
            .rev()
            .find(|c| c.contains("--name ccid/verdict"))
            .and_then(|c| c.rsplit("--state ").next().map(str::to_owned))
    }
}

fn gating(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn the_verdict_needs_every_gating_job_and_fails_on_any_failure() {
    let jobs = |entries: &[(&str, State)]| -> BTreeMap<String, State> {
        entries.iter().map(|(n, s)| ((*n).to_owned(), *s)).collect()
    };
    let both = gating(&["verify", "lint"]);
    assert_eq!(aggregate(&both, &jobs(&[])), State::Pending);
    assert_eq!(
        aggregate(&both, &jobs(&[("verify", State::Success)])),
        State::Pending
    );
    assert_eq!(
        aggregate(
            &both,
            &jobs(&[("verify", State::Success), ("lint", State::Success)])
        ),
        State::Success
    );
    assert_eq!(
        aggregate(
            &both,
            &jobs(&[("verify", State::Pending), ("lint", State::Success)])
        ),
        State::Pending
    );
    assert_eq!(
        aggregate(&both, &jobs(&[("verify", State::Failure)])),
        State::Failure,
        "one failure decides without waiting for the others"
    );
    // Side jobs never matter, and nothing declared means nothing to aggregate.
    assert_eq!(
        aggregate(
            &gating(&["verify"]),
            &jobs(&[("verify", State::Success), ("release", State::Failure)])
        ),
        State::Success
    );
    assert_eq!(
        aggregate(&[], &jobs(&[("verify", State::Success)])),
        State::Pending
    );
}

#[test]
fn a_lone_gating_job_posts_its_own_context_and_the_verdict() {
    let status = reporter(0);
    let state = tempfile::tempdir().unwrap();
    run(&status.options(Some(state.path()), "verify", "verify", State::Success)).unwrap();
    assert_eq!(
        status.calls(),
        vec![
            format!("--commit {COMMIT} --name ccid/verify --url https://ci.example.invalid/run/7 --started 5 --state success"),
            format!("--commit {COMMIT} --name ccid/verdict --url https://ci.example.invalid/run/7 --started 5 --state success"),
        ]
    );
}

#[test]
fn a_failed_side_job_never_touches_the_verdict() {
    let status = reporter(0);
    let state = tempfile::tempdir().unwrap();
    run(&status.options(Some(state.path()), "verify", "verify", State::Success)).unwrap();
    run(&status.options(Some(state.path()), "release", "verify", State::Failure)).unwrap();
    let calls = status.calls();
    assert!(calls
        .iter()
        .any(|c| c.contains("--name ccid/release") && c.ends_with("--state failure")));
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.contains("--name ccid/verdict"))
            .count(),
        1
    );
    assert_eq!(status.last_verdict().as_deref(), Some("success"));
    // Without any gating declaration the commit has no verdict at all.
    let bare = reporter(0);
    run(&bare.options(Some(state.path()), "verify", "", State::Success)).unwrap();
    assert_eq!(bare.calls().len(), 1);
}

#[test]
fn jobs_in_separate_pipelines_share_one_verdict() {
    let status = reporter(0);
    let state = tempfile::tempdir().unwrap();
    let step = |job: &str, state_value: State| {
        run(&status.options(Some(state.path()), job, "verify,lint", state_value)).unwrap();
        status.last_verdict().unwrap()
    };
    assert_eq!(step("verify", State::Pending), "pending");
    assert_eq!(
        step("verify", State::Success),
        "pending",
        "lint has not reported"
    );
    assert_eq!(step("lint", State::Success), "success");
    assert_eq!(
        step("lint", State::Pending),
        "pending",
        "a rerun reopens the verdict"
    );
    assert_eq!(step("lint", State::Failure), "failure");
    assert_eq!(step("lint", State::Success), "success");
}

#[test]
fn records_are_separate_per_commit_and_repository() {
    let status = reporter(0);
    let state = tempfile::tempdir().unwrap();
    run(&status.options(Some(state.path()), "verify", "verify,lint", State::Success)).unwrap();
    let mut other_commit =
        status.options(Some(state.path()), "lint", "verify,lint", State::Success);
    other_commit.commit = "f".repeat(40);
    run(&other_commit).unwrap();
    assert_eq!(status.last_verdict().as_deref(), Some("pending"));
    let mut other_repo = status.options(Some(state.path()), "lint", "verify,lint", State::Success);
    other_repo.repo = Some("owner/another".into());
    run(&other_repo).unwrap();
    assert_eq!(status.last_verdict().as_deref(), Some("pending"));
}

#[test]
fn without_a_shared_record_only_a_lone_gating_job_can_decide() {
    let status = reporter(0);
    run(&status.options(None, "verify", "verify", State::Failure)).unwrap();
    assert_eq!(status.last_verdict().as_deref(), Some("failure"));
    run(&status.options(None, "verify", "verify,lint", State::Success)).unwrap();
    assert_eq!(status.last_verdict().as_deref(), Some("pending"));
    let mut anonymous = status.options(
        Some(tempfile::tempdir().unwrap().path()),
        "verify",
        "verify",
        State::Success,
    );
    anonymous.repo = None;
    run(&anonymous).unwrap();
    assert_eq!(status.last_verdict().as_deref(), Some("success"));
}

#[test]
fn a_tampered_reporter_posts_nothing() {
    let status = reporter(0);
    let mut options = status.options(None, "verify", "verify", State::Success);
    options.binary_sha256 = "0".repeat(64);
    assert!(run(&options).is_err());
    assert!(status.calls().is_empty());
}

#[test]
fn reporter_failures_surface_and_stop_the_verdict() {
    let status = reporter(3);
    let state = tempfile::tempdir().unwrap();
    assert!(run(&status.options(Some(state.path()), "verify", "verify", State::Success)).is_err());
    assert_eq!(
        status.calls().len(),
        1,
        "the verdict is not posted after the job context failed"
    );
}

#[test]
fn malformed_input_is_rejected_before_any_post() {
    let status = reporter(0);
    let good = |commit: &str, job: &str, gating: &str, url: &str| {
        let mut options = status.options(None, job, gating, State::Success);
        options.commit = commit.into();
        options.url = url.into();
        run(&options).is_err()
    };
    assert!(good("abc", "verify", "verify", "https://x.invalid"));
    assert!(good(
        &"A".repeat(40),
        "verify",
        "verify",
        "https://x.invalid"
    ));
    assert!(good(COMMIT, "../verify", "verify", "https://x.invalid"));
    assert!(good(COMMIT, "verify", "verify,../x", "https://x.invalid"));
    assert!(good(
        COMMIT,
        "verify",
        "verify",
        "https://x.invalid\nHeader: 1"
    ));
    assert!(status.calls().is_empty());
    assert!(repository_path("owner/name") && repository_path("group/sub/name"));
    assert!(!repository_path("name") && !repository_path("../x/y") && !repository_path("a//b"));
}

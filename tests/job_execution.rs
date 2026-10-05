#![cfg(unix)]
use serde_json::json;
use std::{fs, path::Path, process::Command};

fn git(repo: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().into()
}

#[test]
fn archived_job_preserves_exact_source_and_rejects_tampered_history() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join(".ci")).unwrap();
    git(&repo, &["init", "-q", "-b", "source"]);
    fs::write(repo.join(".ci/ccid.toml"), "schema=1\nproject='fixture'\n[checks.test]\nkind='commands'\ncommands=[['true']]\n[jobs.verify]\nchecks=['test']\nworkflow='verify'\ncommand=['sh','.ci/run.sh']\n").unwrap();
    fs::write(repo.join(".ci/run.sh"), "set -eu\ntest -x \"$CCID_BIN\"\ntest \"$(git rev-parse HEAD)\" = \"$CI_COMMIT_SHA\"\ntest \"$CHECKS\" = test\nprintf '%s' \"$CI_COMMIT_SHA\" > \"$RESULT\"\n").unwrap();
    git(&repo, &["add", ".ci"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let archive = temp.path().join("source.tar");
    let bundle = temp.path().join("source.bundle");
    git(
        &repo,
        &["archive", "--output", archive.to_str().unwrap(), "HEAD"],
    );
    git(
        &repo,
        &[
            "bundle",
            "create",
            bundle.to_str().unwrap(),
            "refs/heads/source",
        ],
    );
    // The mutable worktree is not the submitted source.
    fs::write(repo.join(".ci/run.sh"), "exit 77\n").unwrap();
    let result = temp.path().join("result");
    let mut request = json!({
        "archive":archive,"sha256":ccid::sha256_file(&archive).unwrap(),
        "commit":commit,"bundle":bundle,"bundle_sha256":ccid::sha256_file(&bundle).unwrap(),
        "tool_revision":ccid::SOURCE_REVISION,"job":"verify",
        "environment":{"RESULT":result, "CI_COMMIT_SHA":"untrusted-override", "CCID_BIN":"/nonexistent/untrusted-override", "CI_MIN_AVAILABLE_MB":"1"}
    });
    let input = temp.path().join("request.json");
    let run = |request: &serde_json::Value, expected: &str| {
        fs::write(&input, serde_json::to_vec(request).unwrap()).unwrap();
        Command::new(env!("CARGO_BIN_EXE_ccid"))
            .args(["execute-job", "--request"])
            .arg(&input)
            .args(["--expect-commit", expected])
            .output()
            .unwrap()
    };
    let rejected = run(&request, &"0".repeat(40));
    assert!(!rejected.status.success());
    assert!(
        !result.exists(),
        "Scheduler commit mismatch must not execute the job"
    );
    let output = run(&request, &commit);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&result).unwrap(), commit);
    fs::remove_file(&result).unwrap();
    request["bundle_sha256"] = json!("0".repeat(64));
    assert!(!run(&request, &commit).status.success());
    assert!(!result.exists(), "Rejected inputs must not execute the job");
}

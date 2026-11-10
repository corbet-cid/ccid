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
            .env_remove("CCID_RESULT_CACHE")
            .env_remove("CCID_REMOTE_CACHE")
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

/// Crow and Argo run jobs through `execute-job`: a `fleet = true` check there gets the same
/// fleet data as under `job-run` (the runner's channel copy, pinned as an immutable snapshot),
/// and still fails closed when the runner has none.
#[test]
fn execute_job_hands_fleet_checks_the_runners_channel_copy() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join(".ci")).unwrap();
    git(&repo, &["init", "-q", "-b", "source"]);
    fs::write(repo.join(".ci/ccid.toml"), "schema=1\nproject='fixture'\n[checks.reads]\nkind='commands'\nfleet=true\ncommands=[['sh','-c','test \"$CCID_FLEET_KEYED\" = 1 && test -n \"$CCID_FLEET_DIGEST\" && test -f \"$CCID_FLEET_FILE\"']]\n[jobs.verify]\nchecks=['reads']\nworkflow='verify'\ncommand=['ccid:run-declared-checks']\n").unwrap();
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
    let request = json!({
        "archive":archive,"sha256":ccid::sha256_file(&archive).unwrap(),
        "commit":commit,"bundle":bundle,"bundle_sha256":ccid::sha256_file(&bundle).unwrap(),
        "tool_revision":ccid::SOURCE_REVISION,"job":"verify",
        "environment":{"CI_MIN_AVAILABLE_MB":"1"}
    });
    let input = temp.path().join("request.json");
    fs::write(&input, serde_json::to_vec(&request).unwrap()).unwrap();
    let home = temp.path().join("home");
    let state = home.join(".cache/ccid-channel");
    fs::create_dir_all(&state).unwrap();
    let run = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccid"));
        for name in [
            "CCID_RESULT_CACHE",
            "CCID_REMOTE_CACHE",
            "CCID_CACHE_CHILD",
            "CCID_CACHE_REPLAY_ONLY",
            "CCID_TARGET_LOCK_HELD",
            "CCID_CHANNEL_FILE",
            "CCID_FLEET_FILE",
            "CCID_FLEET_DIGEST",
            "CCID_FLEET_KEYED",
            "CCID_FORGE_HOSTS",
            "XDG_CACHE_HOME",
            "CI_MEMORY_MB",
            "CI_MEMORY_PER_JOB_MB",
        ] {
            command.env_remove(name);
        }
        command
            .env("HOME", &home)
            .env(
                "CI_REPOSITORY_URL",
                "https://example.invalid/fixtures/fleet",
            )
            .env("TMPDIR", temp.path())
            .env("CARGO_TARGET_DIR", temp.path().join("target"))
            .env("CI_JOBS", "1")
            .env("CI_NIX_JOBS", "1")
            .args(["execute-job", "--request"])
            .arg(&input)
            .args(["--expect-commit", &commit])
            .output()
            .unwrap()
    };
    // No channel copy on this runner: the fleet check fails closed.
    let output = run();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Check reads declares fleet = true but no fleet data is available"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // With the shim's copy the job pins it and the check runs keyed.
    fs::write(
        state.join("last-good.json"),
        r#"{"default":{"revision":"r","sha256":"s"},"fleet":{"schema":1,"first_party":["https://forge.example/org/"]}}"#,
    )
    .unwrap();
    let output = run();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("ccid execute-job: fleet data "), "{stderr}");
    assert!(
        fs::read_dir(&state).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("fleet-")),
        "the job reads an immutable snapshot"
    );
}

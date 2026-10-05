#![cfg(unix)]
#[path = "support/forge_sync.rs"]
mod support;
use std::{fs, os::unix::fs::PermissionsExt};
use support::{git, Fixture};

#[test]
fn plans_then_copies_all_heads_and_exact_tag_objects_without_touching_other_locations() {
    let f = Fixture::new();
    let first = f.commit("first");
    git(&f.source, &["branch", "feature"]);
    git(&f.source, &["tag", "-am", "release", "v1"]);
    let tag = git(&f.source, &["rev-parse", "refs/tags/v1"]);
    let (_, planned) = f.report(&["--all-refs", "--destination", "replica"]);
    assert_eq!(planned["replicas"][0]["state"], "planned");
    assert!(git(&f.replica, &["for-each-ref", "--format=%(refname)"]).is_empty());
    let (output, report) = f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report["complete"], true);
    assert_eq!(report["repository_complete"], false); // The declared offline location was not selected.
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/feature"]), first);
    assert_eq!(git(&f.replica, &["rev-parse", "refs/tags/v1"]), tag);
    let latest = f.commit("second");
    let (_, report) = f.report(&[
        "--ref",
        "refs/heads/main",
        "--destination",
        "replica",
        "--apply",
    ]);
    assert_eq!(report["complete"], true);
    assert_eq!(
        report["replicas"][0]["refs"][0]["reason"],
        "verified-after-push"
    );
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/main"]), latest);
    assert_eq!(git(&f.replica, &["rev-parse", "refs/tags/v1"]), tag);
}

#[test]
fn divergence_tag_replacement_and_replica_only_refs_block_all_writes_to_that_replica() {
    let f = Fixture::new();
    let first = f.commit("first");
    git(&f.source, &["tag", "-am", "release", "v1"]);
    let (_, report) = f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    assert_eq!(report["complete"], true);
    let second = f.commit("source change");
    git(&f.source, &["checkout", "-q", "--detach", &first]);
    let diverged = f.commit("independent change");
    git(
        &f.replica,
        &["fetch", "-q", f.source.to_str().unwrap(), &diverged],
    );
    git(&f.replica, &["update-ref", "refs/heads/main", &diverged]);
    git(&f.replica, &["update-ref", "refs/tags/v1", &first]); // Same peeled commit, different tag object.
    git(&f.replica, &["update-ref", "refs/heads/keep", &first]);
    git(&f.source, &["checkout", "-q", "main"]);
    git(&f.source, &["branch", "new-branch", &second]);
    let (output, report) = f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    assert!(!output.status.success());
    assert_eq!(report["replicas"][0]["state"], "blocked");
    let reasons = report["replicas"][0]["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["reason"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(reasons.contains(&"diverged-branch"));
    assert!(reasons.contains(&"tag-conflict"));
    assert_eq!(report["replicas"][0]["retained_refs"][0], "refs/heads/keep");
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/main"]), diverged);
    assert!(!std::process::Command::new("git")
        .current_dir(&f.replica)
        .args(["show-ref", "--verify", "refs/heads/new-branch"])
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn offline_replica_is_pending_while_other_replicas_can_catch_up_later() {
    let f = Fixture::new();
    let first = f.commit("first");
    let (output, report) = f.report(&[
        "--all-refs",
        "--destination",
        "replica",
        "--destination",
        "offline",
        "--apply",
    ]);
    assert!(!output.status.success());
    assert_eq!(report["complete"], false);
    assert_eq!(report["replicas"][0]["state"], "updated");
    assert_eq!(report["replicas"][1]["state"], "pending");
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/main"]), first);
    let restored = f.root.path().join("missing");
    fs::create_dir(&restored).unwrap();
    git(&restored, &["init", "--bare", "-q"]);
    let (output, report) = f.report(&[
        "--all-refs",
        "--destination",
        "replica",
        "--destination",
        "offline",
        "--apply",
    ]);
    assert!(output.status.success());
    assert_eq!(report["repository_complete"], true);
    assert_eq!(git(&restored, &["rev-parse", "refs/heads/main"]), first);
}

#[test]
fn ahead_replica_and_failed_atomic_push_are_never_reported_complete() {
    let f = Fixture::new();
    let first = f.commit("first");
    let second = f.commit("second");
    f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    git(&f.source, &["reset", "--hard", &first]);
    let (_, report) = f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    assert_eq!(report["replicas"][0]["refs"][0]["reason"], "replica-ahead");
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/main"]), second);
    git(&f.source, &["reset", "--hard", &second]);
    f.commit("third");
    git(&f.source, &["branch", "new-branch"]);
    let hook = f.replica.join("hooks/pre-receive");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let (output, report) = f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    assert!(!output.status.success());
    assert_eq!(report["replicas"][0]["state"], "pending");
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/main"]), second);
    assert!(!std::process::Command::new("git")
        .current_dir(&f.replica)
        .args(["show-ref", "--verify", "refs/heads/new-branch"])
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn historical_lfs_pointers_and_gitlinks_block_promotion_even_after_removal() {
    let f = Fixture::new();
    let first = f.commit("first");
    fs::write(
        f.source.join("large"),
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 12345\n",
            "a".repeat(64)
        ),
    )
    .unwrap();
    git(&f.source, &["add", "large"]);
    git(
        &f.source,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{first},dependency"),
        ],
    );
    git(&f.source, &["commit", "-qm", "external payloads"]);
    git(&f.source, &["rm", "--cached", "dependency", "large"]);
    git(&f.source, &["commit", "-qm", "remove external payloads"]);
    let (output, report) = f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    assert!(!output.status.success());
    assert_eq!(report["source_reason"], "external-content-unverified");
    assert_eq!(report["content"]["lfs_required"], true);
    assert_eq!(report["content"]["submodules_required"], true);
    assert_eq!(report["complete"], false);
    assert!(git(&f.replica, &["for-each-ref", "--format=%(refname)"]).is_empty());
}

#[test]
fn arbitrary_refs_duplicate_destinations_and_missing_source_refs_are_refused() {
    let f = Fixture::new();
    f.commit("first");
    for args in [
        vec![
            "--ref",
            "refs/heads/*",
            "--destination",
            "replica",
            "--apply",
        ],
        vec!["--ref", "HEAD", "--destination", "replica", "--apply"],
        vec!["--all-refs", "--destination", "source", "--apply"],
        vec![
            "--all-refs",
            "--destination",
            "replica",
            "--destination",
            "replica",
            "--apply",
        ],
    ] {
        assert!(!f.run(&args).status.success());
    }
    let (_, report) = f.report(&[
        "--ref",
        "refs/heads/missing",
        "--destination",
        "replica",
        "--apply",
    ]);
    assert_eq!(report["source_reason"], "source-ref-missing");
    assert!(git(&f.replica, &["for-each-ref", "--format=%(refname)"]).is_empty());
}

#[test]
fn bounded_runner_preserves_binary_batch_input_and_null_stdin_default() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("objects");
    let bytes = b"\0\xff\n spaced \0\n";
    fs::write(&input, bytes).unwrap();
    let runner = ccid::Runner::new(
        directory.path().into(),
        std::env::vars_os().collect(),
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(
        runner
            .run_bytes_with_input_file(&["cat".into()], &input)
            .unwrap(),
        bytes
    );
    assert_eq!(fs::read(&input).unwrap(), bytes);
    assert_eq!(runner.run(&["cat".into()], true).unwrap(), "");
}

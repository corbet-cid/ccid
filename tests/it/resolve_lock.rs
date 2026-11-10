//! `ccid resolve-lock --npm`: the retired `.ci/resolve-npm.sh` scripts as one
//! shared command. The fake `npm` on PATH records its argv and stands in for
//! the registry, exactly like the cargo fixture in the scripts' own
//! `test-resolve-dependencies.py`.

#![cfg(unix)]

use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

const LOCK_FIXTURE: &str =
    r#"{"name":"fixture","version":"1.0.0","lockfileVersion":3,"packages":{}}"#;
const MANIFEST: &str = "schema = 1\nproject = \"resolve-fixture\"\n[checks]\n";

struct Fixture {
    root: tempfile::TempDir,
    repo: PathBuf,
    sha: String,
    bin: PathBuf,
    arguments: PathBuf,
    artifacts: PathBuf,
}

fn git(repo: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@invalid",
            "-C",
        ])
        .arg(repo)
        .args(args)
        .status()
        .unwrap()
        .success());
}

fn fixture(manifest: &str) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let bin = root.path().join("bin");
    fs::create_dir_all(bin.clone()).unwrap();
    fs::create_dir_all(repo.join(".ci")).unwrap();
    fs::write(repo.join("package.json"), "{\"name\":\"fixture\"}\n").unwrap();
    fs::write(repo.join("package-lock.json"), "original fixture\n").unwrap();
    fs::write(repo.join(".ci/ccid.toml"), manifest).unwrap();
    let arguments = root.path().join("arguments");
    let lock = root.path().join("resolved-lock.json");
    fs::write(&lock, LOCK_FIXTURE).unwrap();
    fs::write(
        bin.join("npm"),
        format!(
            "#!/usr/bin/env bash\nset -eu\nprintf \"%s\\n\" \"$@\" >> \"{}\"\nif [ \"$FAKE_NPM_STATUS\" != 0 ]; then exit \"$FAKE_NPM_STATUS\"; fi\ncp \"{}\" ./package-lock.json\n",
            arguments.display(),
            lock.display()
        ),
    )
    .unwrap();
    fs::set_permissions(bin.join("npm"), fs::Permissions::from_mode(0o755)).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "fixture"]);
    let sha = String::from_utf8(
        Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let fixture = Fixture {
        root,
        repo,
        sha: sha.trim().to_owned(),
        bin,
        arguments,
        artifacts: PathBuf::new(),
    };
    fixture
}

struct Run {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run(fixture: &Fixture, extra: &[&str], env: &[(&'static str, Option<String>)]) -> Run {
    let path = format!(
        "{}:{}",
        fixture.bin.display(),
        std::env::var("PATH").unwrap()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_ccid"));
    command
        .arg("resolve-lock")
        .arg("--repo")
        .arg(&fixture.repo)
        .arg("--manifest")
        .arg(".ci/ccid.toml")
        .args(extra)
        .env("PATH", path);
    for (name, value) in env {
        match value {
            Some(value) => {
                command.env(name, value);
            }
            None => {
                command.env_remove(name);
            }
        }
    }
    let output = command.output().unwrap();
    Run {
        status: output.status.code().unwrap(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn standard_env(fixture: &Fixture, npm_status: &str) -> Vec<(&'static str, Option<String>)> {
    vec![
        (
            "ARTIFACT_ROOT",
            Some(fixture.artifacts.to_string_lossy().into_owned()),
        ),
        ("CI_COMMIT_SHA", Some(fixture.sha.clone())),
        ("FAKE_NPM_STATUS", Some(npm_status.to_owned())),
    ]
}

fn source_digest(repo: &Path) -> String {
    let output = Command::new("git")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "ls-files",
            "--cached",
            "--others",
            "-z",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let mut names: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
    names.sort_unstable();
    let mut hasher = Sha256::new();
    for name in names {
        if name.is_empty() {
            continue;
        }
        hasher.update(name);
        hasher.update(fs::read(repo.join(String::from_utf8_lossy(name).as_ref())).unwrap());
    }
    format!("{:x}", hasher.finalize())
}

fn setup(manifest: &str, npm_status: &str) -> (Fixture, Vec<(&'static str, Option<String>)>) {
    let mut fixture = fixture(manifest);
    fixture.artifacts = fixture.root.path().join("artifacts");
    fs::create_dir_all(&fixture.artifacts).unwrap();
    let env = standard_env(&fixture, npm_status);
    (fixture, env)
}

#[test]
fn success_retains_lock_layout_digest_and_block() {
    let (fixture, env) = setup(MANIFEST, "0");
    let run = run(&fixture, &["--npm"], &env);
    assert_eq!(run.status, 0, "stderr: {}", run.stderr);
    let retained = fixture
        .artifacts
        .join(&fixture.sha)
        .join("resolve-npm")
        .join("package-lock.json");
    assert_eq!(fs::read(&retained).unwrap(), LOCK_FIXTURE.as_bytes());
    assert_eq!(
        fs::metadata(&retained).unwrap().permissions().mode() & 0o777,
        0o644
    );
    let digest = format!("{:x}", Sha256::digest(LOCK_FIXTURE.as_bytes()));
    assert!(run.stdout.contains(&format!("{digest}  package-lock.json")));
    assert!(run.stdout.contains("---LOCK-BEGIN---"));
    assert!(run.stdout.contains(LOCK_FIXTURE));
    assert!(run.stdout.contains("---LOCK-END---"));
    assert!(run
        .stdout
        .contains("Resolved npm lock only; retain it in a commit and run the full verify job."));
    assert_eq!(
        fs::read_to_string(&fixture.arguments).unwrap(),
        "install\n--package-lock-only\n--ignore-scripts\n--no-audit\n--no-fund\n",
    );
}

#[test]
fn missing_artifact_root_refuses_before_resolution() {
    let (fixture, _) = setup(MANIFEST, "0");
    let env = vec![
        ("ARTIFACT_ROOT", None),
        ("CI_COMMIT_SHA", Some(fixture.sha.clone())),
    ];
    let run = run(&fixture, &["--npm"], &env);
    assert_eq!(run.status, 2, "stdout: {}", run.stdout);
    assert!(run.stderr.contains("ARTIFACT_ROOT"));
    assert!(!fixture.arguments.exists(), "npm must not run");
}

#[test]
fn artifact_root_inside_repo_refuses() {
    let (mut fixture, _) = setup(MANIFEST, "0");
    fixture.artifacts = fixture.repo.join("artifacts");
    let env = standard_env(&fixture, "0");
    let run = run(&fixture, &["--npm"], &env);
    assert_eq!(run.status, 2, "stderr: {}", run.stderr);
    assert!(run.stderr.contains("outside --repo"));
    assert!(!fixture.arguments.exists(), "npm must not run");
}

#[test]
fn bad_revision_refuses_before_resolution() {
    for revision in ["short", "/bad", &"F".repeat(40), ""] {
        let (fixture, mut env) = setup(MANIFEST, "0");
        for entry in &mut env {
            if entry.0 == "CI_COMMIT_SHA" {
                entry.1 = Some(revision.to_owned());
            }
        }
        let run = run(&fixture, &["--npm"], &env);
        assert_eq!(run.status, 2, "revision {revision}: {}", run.stderr);
        assert!(!fixture.arguments.exists(), "npm must not run");
    }
    let (fixture, _) = setup(MANIFEST, "0");
    let env = vec![
        (
            "ARTIFACT_ROOT",
            Some(fixture.artifacts.to_string_lossy().into_owned()),
        ),
        ("CI_COMMIT_SHA", None),
    ];
    let run = run(&fixture, &["--npm"], &env);
    assert_eq!(run.status, 2, "{}", run.stderr);
    assert!(!fixture.arguments.exists(), "npm must not run");
}

#[test]
fn failing_npm_leaves_no_layout() {
    let (fixture, env) = setup(MANIFEST, "3");
    let run = run(&fixture, &["--npm"], &env);
    assert_eq!(run.status, 1, "stderr: {}", run.stderr);
    assert!(run.stderr.contains("npm install failed"));
    assert!(!run.stdout.contains("retain it in a commit"));
    assert!(
        !fixture.artifacts.join(&fixture.sha).exists(),
        "no partial layout dir"
    );
}

#[test]
fn checkout_unchanged_hr1() {
    let (fixture, env) = setup(MANIFEST, "0");
    let before = source_digest(&fixture.repo);
    let lock_before = fs::read(fixture.repo.join("package-lock.json")).unwrap();
    let run = run(&fixture, &["--npm"], &env);
    assert_eq!(run.status, 0, "stderr: {}", run.stderr);
    assert_eq!(source_digest(&fixture.repo), before);
    assert_eq!(
        fs::read(fixture.repo.join("package-lock.json")).unwrap(),
        lock_before
    );
}

#[test]
fn cargo_mode_is_retired() {
    let (fixture, env) = setup(MANIFEST, "0");
    let run = run(&fixture, &[], &env);
    assert_eq!(run.status, 2, "stdout: {}", run.stdout);
    assert!(run.stderr.contains("retired"));
    assert!(!fixture.arguments.exists(), "npm must not run");
}

#[test]
fn unknown_resolve_key_refuses() {
    let (fixture, env) = setup(
        "schema = 1\nproject = \"resolve-fixture\"\n[checks]\n[resolve]\npackages = []\n",
        "0",
    );
    let run = run(&fixture, &["--npm"], &env);
    assert_eq!(run.status, 2, "{}", run.stderr);
    assert!(!fixture.arguments.exists(), "npm must not run");
}

#[test]
fn print_lock_false_suppresses_block() {
    let (fixture, env) = setup(
        "schema = 1\nproject = \"resolve-fixture\"\n[checks]\n[resolve]\nprint_lock = false\n",
        "0",
    );
    let run = run(&fixture, &["--npm"], &env);
    assert_eq!(run.status, 0, "stderr: {}", run.stderr);
    assert!(!run.stdout.contains("---LOCK-BEGIN---"));
    assert!(!run.stdout.contains("---LOCK-END---"));
    assert!(run.stdout.contains("package-lock.json"));
    assert!(run.stdout.contains("retain it in a commit"));
}

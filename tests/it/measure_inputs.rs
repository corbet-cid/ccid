//! `ccid measure-inputs` end to end with a real strace. Ignored by default: it needs strace on
//! PATH, which build workers do not carry. Run it with
//! `cargo test --test it -- --ignored measure_inputs` and strace on PATH.
use std::{fs, path::Path, process::Command};

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn fixture(check: &str) -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    for (path, content) in [
        ("src/a.txt", "a\n"),
        ("inputs/interviews/README.md", "kept\n"),
        ("README.md", "readme\n"),
        ("docs/guide.md", "guide\n"),
    ] {
        let file = repo.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, content).unwrap();
    }
    fs::create_dir_all(repo.join(".ci")).unwrap();
    fs::write(
        repo.join(".ci/ccid.toml"),
        format!(
            "schema = 1\nproject = \"fixture\"\n\n[checks.probe]\nkind = \"commands\"\ncache = false\ncommands = [[\"sh\", \"-c\", {check:?}]]\n"
        ),
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "fixture"]);
    temp
}

fn measure(temp: &tempfile::TempDir) -> (Option<i32>, serde_json::Value, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args(["measure-inputs", "--check", "probe", "--repo"])
        .arg(temp.path().join("repo"))
        .env("TMPDIR", temp.path())
        .env("CARGO_TARGET_DIR", temp.path().join("target"))
        .env("CI_JOBS", "1")
        .env("CI_MIN_AVAILABLE_MB", "1")
        .env_remove("CCID_TARGET_LOCK_HELD")
        .env_remove("CCID_RESULT_CACHE")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|line| line.contains("\"measured\""))
        .unwrap_or("null");
    (
        output.status.code(),
        serde_json::from_str(line).unwrap_or(serde_json::Value::Null),
        format!("{stdout}\n{}", String::from_utf8_lossy(&output.stderr)),
    )
}

#[test]
#[ignore]
fn measure_inputs_keeps_what_the_check_touched_and_verifies_it() {
    // Reads a file below src/, lists a directory a test asserts, probes an optional file; never
    // looks at README.md or docs/.
    let temp = fixture(
        "cat src/a.txt >/dev/null && ls inputs >/dev/null && { test -e optional.txt || true; }",
    );
    let (code, line, text) = measure(&temp);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(
        line["cache_inputs"],
        serde_json::json!([
            ".ci/**",
            "inputs/**",
            "optional.txt",
            "optional.txt/**",
            "src/**"
        ]),
        "{text}"
    );
    assert_eq!(line["verified"], true, "{text}");
    // The provenance that lets the verdict key narrow to these inputs.
    let measured = &line["cache_inputs_measured"];
    assert_eq!(
        measured["commands"].as_str().map(str::len),
        Some(64),
        "{text}"
    );
    assert_eq!(
        measured["layout"].as_str().map(str::len),
        Some(64),
        "{text}"
    );
    assert_eq!(measured["date"].as_str().map(str::len), Some(10), "{text}");
}

#[test]
#[ignore]
fn measure_inputs_keeps_the_whole_tree_for_a_check_that_lists_the_root() {
    let temp = fixture("ls . >/dev/null");
    let (code, line, text) = measure(&temp);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(line["narrowed"], false, "{text}");
    assert!(line["cache_inputs"].is_null(), "{text}");
    assert!(line["cache_inputs_measured"].is_null(), "{text}");
}

#[test]
#[ignore]
fn measure_inputs_refuses_a_failing_check() {
    let temp = fixture("cat missing.txt");
    let (code, line, text) = measure(&temp);
    assert_eq!(code, Some(1), "{text}");
    assert_eq!(line["measured"], false, "{text}");
}

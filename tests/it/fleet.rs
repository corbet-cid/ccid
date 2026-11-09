//! `fleet = true` end to end: the binary under test runs checks that read fleet data from a
//! release channel copy (`CCID_FLEET_FILE`), and fails closed without one.
#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

const BINARY: &str = env!("CARGO_BIN_EXE_ccid");

const CHANNEL: &str = r#"{"default":{"revision":"r","sha256":"s"},"fleet":{"schema":1,"first_party":["https://forge.example/org/"],"aliases":{"https://mirror.example/org/":"https://forge.example/org/"}}}"#;
const OTHER_CHANNEL: &str = r#"{"default":{"revision":"r","sha256":"s"},"fleet":{"schema":1,"first_party":["https://forge.example/moved/"]}}"#;
const FLEETLESS: &str = r#"{"default":{"revision":"r","sha256":"s"}}"#;

/// A repository whose `.ci/ccid.toml` declares the given checks (TOML tables).
fn repository(checks: &str) -> TempDir {
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join(".ci")).unwrap();
    fs::write(
        temp.path().join(".ci/ccid.toml"),
        format!("schema = 1\nproject = 'fleet-fixture'\n{checks}"),
    )
    .unwrap();
    temp
}

fn standard_checks() -> String {
    format!(
        "[checks.reads]\nkind = 'commands'\nfleet = true\ncommands = [['sh', '-c', 'test \"$CCID_FLEET_KEYED\" = 1 && test -n \"$CCID_FLEET_DIGEST\" && test -f \"$CCID_FLEET_FILE\"']]\n\
         [checks.plain]\nkind = 'commands'\ncommands = [['sh', '-c', 'test -z \"$CCID_FLEET_KEYED\"']]\n\
         [checks.consumer]\nkind = 'commands'\nfleet = true\ncommands = [['{BINARY}', 'fleet', 'show']]\n\
         [checks.undeclared]\nkind = 'commands'\ncommands = [['{BINARY}', 'fleet', 'show']]\n\
         [checks.swaps]\nkind = 'commands'\nfleet = true\ncommands = [['sh', '-c', 'cp \"$1\" \"$CCID_FLEET_FILE\" && exec \"$2\" fleet show', 'fixture', 'other.json', '{BINARY}']]\n"
    )
}

/// The binary under test with no inherited fleet, cache or enclosing-check state.
fn ccid(root: &Path, fleet_file: Option<&Path>) -> Command {
    let mut command = Command::new(BINARY);
    command.current_dir(root);
    for name in [
        "CCID_RESULT_CACHE",
        "CCID_REMOTE_CACHE",
        "CCID_CACHE_CHILD",
        "CCID_CACHE_REPLAY_ONLY",
        "CCID_TARGET_LOCK_HELD",
        "CCID_RECEIPT",
        "CCID_CHANNEL_FILE",
        "CCID_FLEET_FILE",
        "CCID_FLEET_DIGEST",
        "CCID_FLEET_KEYED",
        "CI_MIN_AVAILABLE_MB",
        "CI_MEMORY_MB",
        "CI_MEMORY_PER_JOB_MB",
    ] {
        command.env_remove(name);
    }
    command
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("TMPDIR", root)
        .env("CI_JOBS", "1")
        .env("CI_NIX_JOBS", "1")
        .env("CI_TIMEOUT", "120");
    if let Some(file) = fleet_file {
        command.env("CCID_FLEET_FILE", file);
    }
    command
}

fn check(root: &Path, fleet_file: Option<&Path>, checks: &[&str]) -> Output {
    let mut command = ccid(root, fleet_file);
    command.args(["check", "--repo", "."]);
    for name in checks {
        command.args(["--check", name]);
    }
    command.output().unwrap()
}

fn channel(root: &Path, name: &str, content: &str) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, content).unwrap();
    path
}

fn text(output: &Output) -> String {
    format!(
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn fleet_checks_run_keyed_with_a_channel_copy_and_plain_checks_stay_unmarked() {
    let repo = repository(&standard_checks());
    let root = repo.path();
    let file = channel(root, "channel.json", CHANNEL);
    for order in [["reads", "plain"], ["plain", "reads"]] {
        let output = check(root, Some(&file), &order);
        assert!(output.status.success(), "{order:?}: {}", text(&output));
    }
    // A selection without a fleet check needs no fleet data at all.
    let output = check(root, None, &["plain"]);
    assert!(output.status.success(), "{}", text(&output));
}

#[test]
fn fleet_checks_fail_closed_without_fleet_data() {
    let repo = repository(&standard_checks());
    let root = repo.path();
    let fleetless = channel(root, "fleetless.json", FLEETLESS);
    for (file, reason) in [
        (None, "fleet data unavailable: set CCID_FLEET_FILE to a copy of the release channel (channel.json)"),
        (Some(fleetless.as_path()), "the release channel carries no fleet data"),
    ] {
        let output = check(root, file, &["plain", "reads"]);
        assert_eq!(output.status.code(), Some(2), "{}", text(&output));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!(
                "Check reads declares fleet = true but no fleet data is available: {reason}"
            )),
            "{}",
            text(&output)
        );
        // Nothing of the selection ran: the refusal comes before any check.
        assert!(!String::from_utf8_lossy(&output.stdout).contains("check-start"));
    }
}

#[test]
fn consumers_accept_fleet_data_only_inside_a_declared_and_keyed_check() {
    let repo = repository(&standard_checks());
    let root = repo.path();
    let file = channel(root, "channel.json", CHANNEL);
    channel(root, "other.json", OTHER_CHANNEL);
    let output = check(root, Some(&file), &["consumer"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains(r#""fleet":{"schema":1"#));

    let output = check(root, Some(&file), &["undeclared"]);
    assert!(!output.status.success(), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "ccid fleet: this check reads fleet data; declare fleet = true on it in .ci/ccid.toml"
        ),
        "{}",
        text(&output)
    );

    // The data a consumer reads must be the data the check was keyed with.
    let output = check(root, Some(&file), &["swaps"]);
    assert!(!output.status.success(), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("ccid fleet: fleet data changed after the check was keyed"),
        "{}",
        text(&output)
    );
}

#[test]
fn the_cached_entry_point_binds_the_same_fleet_digest() {
    let repo = repository(&standard_checks());
    let root = repo.path();
    let file = channel(root, "channel.json", CHANNEL);
    // Without a result store cmnp runs the check uncached: the full contract still holds.
    let mut cached = ccid(root, Some(&file));
    cached.args([
        "cached", "--repo", ".", "--check", "reads", "--check", "consumer",
    ]);
    let output = cached.output().unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let mut missing = ccid(root, None);
    missing.args(["cached", "--repo", ".", "--check", "reads"]);
    let output = missing.output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Check reads declares fleet = true but no fleet data is available"));
}

#[test]
fn fleet_show_and_validate_report_the_digest() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repository_channel = Path::new(env!("CARGO_MANIFEST_DIR")).join("channel.json");
    let output = ccid(root, None)
        .args(["fleet", "validate", "--channel"])
        .arg(&repository_channel)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let line = String::from_utf8(output.stdout).unwrap();
    let digest = line
        .trim()
        .strip_prefix("fleet data valid: ")
        .unwrap_or_else(|| panic!("{line}"))
        .to_owned();
    assert_eq!(digest.len(), 64);

    let output = ccid(root, Some(&repository_channel))
        .args(["fleet", "show"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let shown: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(shown["digest"], serde_json::json!(digest));
    assert_eq!(
        shown["source"],
        serde_json::json!(repository_channel.to_str().unwrap())
    );
    assert_eq!(shown["fleet"]["schema"], serde_json::json!(1));
    // The printed `fleet` member is the canonical byte string the digest names.
    let line = String::from_utf8(output.stdout).unwrap();
    let canonical = line
        .trim()
        .strip_prefix(r#"{"fleet":"#)
        .and_then(|rest| rest.split_once(r#","digest":"#))
        .map(|(fleet, _)| fleet)
        .unwrap_or_else(|| panic!("{line}"));
    use sha2::Digest;
    assert_eq!(format!("{:x}", sha2::Sha256::digest(canonical)), digest);

    let fleetless = channel(root, "fleetless.json", FLEETLESS);
    let output = ccid(root, None)
        .args(["fleet", "validate", "--channel"])
        .arg(&fleetless)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("ccid fleet: the release channel carries no fleet data"));
    let output = ccid(root, None).args(["fleet", "show"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("ccid fleet: fleet data unavailable"));
}

/// The moon pass-through: a cached run of a `fleet = true` check, as CI runs it. Needs a
/// preinstalled moon (like `cache_acceptance`). With cmnp 6dc3929 the moon task loses
/// `CCID_FLEET_FILE` (cmnp forwards only its plumbing list, SEMANTIC_ENV and cache_env), so
/// this fails closed until cmnp forwards the variable.
#[test]
#[ignore = "Run explicitly on a worker with CCID_TEST_MOON; fails closed until cmnp forwards CCID_FLEET_FILE"]
fn a_cached_fleet_check_runs_in_its_moon_task_and_replays_its_verdict() {
    let moon = PathBuf::from(std::env::var_os("CCID_TEST_MOON").expect("preinstalled moon binary"));
    let repo = repository(
        "[checks.reads]\nkind = 'commands'\nfleet = true\ncache_tools = [['sh', '--version']]\ncommands = [['sh', '-c', 'test \"$CCID_FLEET_KEYED\" = 1 && test -n \"$CCID_FLEET_DIGEST\" && test -f \"$CCID_FLEET_FILE\"']]\n",
    );
    let root = repo.path();
    fs::write(
        root.join(".gitignore"),
        ".ccid/\ntarget/\n.moon/\nmoon.yml\n",
    )
    .unwrap();
    for args in [
        &["init", "--quiet"][..],
        &["add", "."],
        &[
            "-c",
            "user.name=CI",
            "-c",
            "user.email=ci@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    ] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success());
    }
    let store = TempDir::new().unwrap();
    let channel_dir = TempDir::new().unwrap();
    let file = channel(channel_dir.path(), "channel.json", CHANNEL);
    let mut paths = vec![
        Path::new(BINARY).parent().unwrap().to_path_buf(),
        moon.parent().unwrap().to_path_buf(),
    ];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let run = || {
        let mut command = ccid(root, Some(&file));
        command
            .args(["check", "--repo", ".", "--check", "reads"])
            .env("PATH", std::env::join_paths(&paths).unwrap())
            .env("CI_CACHE_ROOT", store.path().join("compile"))
            .env(
                "CI_REPOSITORY_URL",
                "https://example.invalid/test/fleet-fixture",
            )
            .env("CCID_RESULT_CACHE", store.path().join("results"));
        command.output().unwrap()
    };
    let cold = run();
    println!("{}", text(&cold));
    assert!(cold.status.success(), "{}", text(&cold));
    let warm = run();
    println!("{}", text(&warm));
    assert!(warm.status.success(), "{}", text(&warm));
}

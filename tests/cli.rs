#![forbid(unsafe_code)]

use std::{
    fs,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

#[test]
fn source_revision_is_embedded_without_a_repository() {
    let output = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .arg("source-revision")
        .current_dir(TempDir::new().unwrap().path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        ccid::SOURCE_REVISION
    );
}

#[test]
fn invalid_later_checks_fail_before_planning_or_executing_the_selection() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let target = root.join("unused-target");
    for invalid in [
        "kind='unknown'",
        "kind='cargo'\nactions=[]",
        "kind='cargo'\nactions=['misspelled']",
        "kind='cargo'\ntest_runner='misspelled'",
        "kind='cargo'\ntoolchain=''",
        "kind='javascript'\nmanager='misspelled'",
        "kind='javascript'\nscripts=['']",
        "kind='nix'\nmode='misspelled'",
        "kind='nix'\nmode='named'\nchecks=[]",
        "kind='commands'\ncommands=[]",
        "kind='commands'\ncommands=[['']]",
    ] {
        fs::write(
            root.join("ccid.toml"),
            format!(
                "schema=1\nproject='preflight'\n[checks.first]\nkind='commands'\ncommands=[['sh','-c','printf reached > marker']]\n[checks.later]\n{invalid}\n"
            ),
        )
        .unwrap();
        for plan in [false, true] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_ccid"));
            command
                .args([
                    "check",
                    "--manifest",
                    "ccid.toml",
                    "--check",
                    "first,later",
                    "--repo",
                ])
                .arg(root)
                .env("CARGO_TARGET_DIR", &target);
            if plan {
                command.arg("--plan");
            }
            let result = command.output().unwrap();
            assert!(!result.status.success(), "accepted {invalid}, plan={plan}");
            assert!(
                String::from_utf8_lossy(&result.stderr).contains("Invalid check later:"),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(
                !target.exists(),
                "preflight must not create or lock the target"
            );
            assert!(
                !root.join("marker").exists(),
                "earlier commands must not execute"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn command_arguments_preserve_intentional_empty_strings() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::write(
        root.join("ccid.toml"),
        "schema=1\nproject='empty-argument'\n[checks.args]\nkind='commands'\ncommands=[['sh','-c','test \"$#\" = 1 && test -z \"$1\"','fixture','']]\n",
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args([
            "check",
            "--manifest",
            "ccid.toml",
            "--check",
            "args",
            "--repo",
        ])
        .arg(root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("TMPDIR", root)
        .env("CI_JOBS", "1")
        .env("CI_NIX_JOBS", "1")
        .env_remove("CI_MIN_AVAILABLE_MB")
        .env_remove("CI_MEMORY_MB")
        .env_remove("CI_MEMORY_PER_JOB_MB")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[cfg(unix)]
#[test]
fn check_preserves_explicit_package_and_target_roots_and_cleans_its_scratch() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let target = root.join("warm-target");
    let report = root.join("scratch-path");
    let script = "test \"$UV_CACHE_DIR\" = /shared/uv && test \"$BUN_INSTALL_CACHE_DIR\" = /shared/bun && test \"$npm_config_cache\" = /shared/npm && test \"$CARGO_TARGET_DIR\" = \"$1\" && printf '%s' \"$TMPDIR\" > \"$2\"";
    let args = vec![
        "sh",
        "-c",
        script,
        "fixture",
        target.to_str().unwrap(),
        report.to_str().unwrap(),
    ];
    fs::write(
        root.join("ccid.toml"),
        format!(
            "schema=1\nproject='fixture'\n[checks.env]\nkind='commands'\ncommands=[{}]\n",
            serde_json::to_string(&args).unwrap()
        ),
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args([
            "check",
            "--manifest",
            "ccid.toml",
            "--check",
            "env",
            "--repo",
        ])
        .arg(root)
        .env("CARGO_TARGET_DIR", &target)
        .env("CI_CACHE_ROOT", root.join("unused"))
        .env("UV_CACHE_DIR", "/shared/uv")
        .env("BUN_INSTALL_CACHE_DIR", "/shared/bun")
        .env("npm_config_cache", "/shared/npm")
        .env("TMPDIR", root)
        .env("CI_JOBS", "1")
        .env("CI_NIX_JOBS", "1")
        .env_remove("CI_MIN_AVAILABLE_MB")
        .env_remove("CI_MEMORY_MB")
        .env_remove("CI_MEMORY_PER_JOB_MB")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(target.join(".ccid/lock").is_file());
    assert!(!root.join("unused").exists());
    let scratch = fs::read_to_string(report).unwrap();
    assert!(!std::path::Path::new(&scratch).exists());
    assert!(root.exists(), "the caller's scratch parent is retained");
}

#[cfg(target_os = "linux")]
#[test]
fn sigterm_cancels_the_owned_command_and_releases_its_cache_lock() {
    cancellation_releases_owned_resources("-TERM");
}

#[cfg(target_os = "linux")]
#[test]
fn sigkill_of_enclosing_process_still_cleans_commands_scratch_and_cache_lock() {
    cancellation_releases_owned_resources("-KILL");
}

#[cfg(target_os = "linux")]
fn cancellation_releases_owned_resources(signal: &str) {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let pid_file = root.join("command.pid");
    let scratch_file = root.join("scratch.path");
    let cache = root.join("cache");
    let command = vec![
        "sh".to_owned(),
        "-c".into(),
        "trap '' TERM; echo $$ > \"$1\"; printf '%s' \"$TMPDIR\" > \"$2\"; while :; do sleep 10; done".into(),
        "fixture".into(),
        pid_file.to_string_lossy().into_owned(),
        scratch_file.to_string_lossy().into_owned(),
    ];
    fs::write(
        root.join("ccid.toml"),
        format!(
            "schema=1\nproject='signal-fixture'\n[checks.test]\nkind='commands'\ncommands=[{}]\n",
            serde_json::to_string(&command).unwrap()
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args([
            "check",
            "--manifest",
            "ccid.toml",
            "--check",
            "test",
            "--repo",
        ])
        .arg(root)
        .env("CARGO_TARGET_DIR", &cache)
        .env("CI_TIMEOUT", "30")
        .env("CI_JOBS", "1")
        .env("CI_NIX_JOBS", "1")
        .env_remove("CI_MEMORY_MB")
        .env_remove("CI_MEMORY_PER_JOB_MB")
        .env_remove("CI_MIN_AVAILABLE_MB")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut owned = FixtureCleanup {
        child,
        pid_file: pid_file.clone(),
    };
    let fixture_ready = || {
        fs::read_to_string(&pid_file)
            .ok()
            .and_then(|pid| pid.trim().parse::<u32>().ok())
            .is_some()
            && fs::read_to_string(&scratch_file)
                .ok()
                .is_some_and(|path| !path.is_empty() && std::path::Path::new(&path).exists())
    };
    let ready = Instant::now() + Duration::from_secs(5);
    while !fixture_ready() && Instant::now() < ready {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(fixture_ready(), "fixture command and scratch are ready");
    let command_pid = fs::read_to_string(&pid_file).unwrap();
    assert!(Command::new("kill")
        .args([signal, &owned.child.id().to_string()])
        .status()
        .unwrap()
        .success());
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = owned.child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            panic!("ccid did not finish after {signal}");
        }
        thread::sleep(Duration::from_millis(20));
    };
    if signal == "-TERM" {
        assert_eq!(status.code(), Some(2));
    } else {
        assert!(!status.success());
    }
    let scratch = fs::read_to_string(scratch_file).unwrap();
    // After an uncatchable outer kill, the supervisor finishes asynchronously.
    while std::path::Path::new(&scratch).exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let path = format!("/proc/{}/stat", command_pid.trim());
    let stat = fs::read_to_string(path);
    assert!(
        stat.as_ref().is_err() || stat.unwrap().split_whitespace().nth(2) == Some("Z"),
        "owned command survived cancellation"
    );
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(cache.join(".ccid/lock"))
        .unwrap();
    lock.try_lock()
        .expect("cache lock released after cancellation");
    assert!(
        !std::path::Path::new(&scratch).exists(),
        "owned scratch removed after cancellation"
    );
}

#[cfg(target_os = "linux")]
struct FixtureCleanup {
    child: std::process::Child,
    pid_file: std::path::PathBuf,
}
#[cfg(target_os = "linux")]
impl Drop for FixtureCleanup {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(&self.pid_file) {
            if let Ok(pid) = pid.trim().parse::<u32>() {
                // The fixture shell is the leader of the process group ccid owns.
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{pid}")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

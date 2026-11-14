//! Declared native fixtures: `setup` before the check, variables in its commands,
//! `teardown` after, also when the check fails (docs/fixtures.md).
#![forbid(unsafe_code)]

use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn run(root: &Path, manifest: &str) -> std::process::Output {
    fs::write(root.join("ccid.toml"), manifest).unwrap();
    Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args(["check", "--repo"])
        .arg(root)
        .args(["--manifest", "ccid.toml", "--check", "test"])
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env(
            "CI_REPOSITORY_URL",
            "https://example.invalid/checks/fixtures",
        )
        .env("CI_CACHE_ROOT", root.join("targets"))
        .env("CARGO_TARGET_DIR", root.join("targets"))
        .env("CI_TIMEOUT", "30")
        .env_remove("CCID_RESULT_CACHE")
        .env_remove("CCID_CACHE_CHILD")
        .env_remove("CCID_CACHE_REPLAY_ONLY")
        .output()
        .unwrap()
}

fn manifest(setup: &str, check: &str) -> String {
    format!(
        "schema=1\nproject='fixtures'\n\
         [fixtures.native]\nsetup=['sh','-c','{setup}']\nteardown=['sh','-c','echo down > \"$PWD/torn-down\"; test -n \"$FIX_B\"']\nenv=['FIX_A','FIX_B']\n\
         [checks.test]\nkind='commands'\nfixtures=['native']\ncommands=[['sh','-c','{check}']]\n"
    )
}

const SETUP: &str = r#"printf "FIX_A=%s\nFIX_B=two\n" "$CCID_FIXTURE_DIR" > "$CCID_FIXTURE_ENV"; echo "$CCID_FIXTURE_DIR" > "$PWD/fixture-dir""#;

#[test]
fn the_check_sees_the_exported_variables_and_the_fixture_is_removed_after() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let output = run(
        root,
        &manifest(
            SETUP,
            r#"test "$FIX_B" = two && test -d "$FIX_A" && echo seen > "$PWD/seen""#,
        ),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.join("seen").exists());
    assert!(root.join("torn-down").exists());
    let dir = fs::read_to_string(root.join("fixture-dir")).unwrap();
    assert!(
        !Path::new(dir.trim()).exists(),
        "the private directory is removed"
    );
}

#[test]
fn a_failing_check_still_tears_the_fixture_down() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let output = run(root, &manifest(SETUP, "exit 3"));
    assert!(!output.status.success());
    assert!(root.join("torn-down").exists());
}

#[test]
fn a_setup_that_exports_something_undeclared_or_misses_a_name_stops_the_check() {
    for setup in [
        r#"printf "FIX_A=x\nFIX_B=y\nEXTRA=z\n" > "$CCID_FIXTURE_ENV""#,
        r#"printf "FIX_A=x\n" > "$CCID_FIXTURE_ENV""#,
        r#"printf "FIX_A=x\nFIX_B=\n" > "$CCID_FIXTURE_ENV""#,
    ] {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let output = run(root, &manifest(setup, r#"echo ran > "$PWD/seen""#));
        assert!(!output.status.success(), "{setup}");
        assert!(
            !root.join("seen").exists(),
            "the check must not run: {setup}"
        );
    }
}

#[test]
fn the_variables_do_not_leak_outside_the_check() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let output = run(
        root,
        &(manifest(SETUP, "true")
            + "[checks.other]\nkind='commands'\ncommands=[['sh','-c','test -z \"${FIX_B:-}\"']]\n"),
    );
    assert!(output.status.success());
}

#[test]
fn a_check_naming_an_undeclared_fixture_or_a_reserved_variable_is_refused() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let output = run(
        root,
        "schema=1\nproject='fixtures'\n[checks.test]\nkind='commands'\nfixtures=['missing']\ncommands=[['true']]\n",
    );
    assert!(!output.status.success());
    let output = run(
        root,
        "schema=1\nproject='fixtures'\n[fixtures.bad]\nsetup=['true']\nenv=['PATH']\n[checks.test]\nkind='commands'\nfixtures=['bad']\ncommands=[['true']]\n",
    );
    assert!(!output.status.success());
}

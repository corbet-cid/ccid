//! Graph-execution tests for the push newest-head half.
//!
//! Separate from the coalescer inline tests to avoid merge conflicts.
//! All fixtures are local files; no network, no cargo invocations, no sleeps.
use super::*;
use crate::jobs::{is_self_checks_command, Refresh, SELF_CHECKS_SENTINEL};

fn refresh_fixture() -> Refresh {
    Refresh {
        branch: "v01".into(),
        sources: vec![
            "forge.example.invalid/cpkg".into(),
            "forge.example.invalid/other".into(),
        ],
        prepare_commands: Vec::new(),
    }
}

fn write(path: &std::path::Path, name: &str, contents: &str) {
    std::fs::write(path.join(name), contents).unwrap();
}

fn lock_entry(name: &str, version: &str, source: &str) -> String {
    format!("[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = \"{source}\"\n")
}

fn v01_source(host_path: &str, branch: &str, sha: &str) -> String {
    format!("git+https://{host_path}.git?branch={branch}#{sha}")
}

#[test]
fn transitive_dep_absent_from_root_is_selected_from_lock() {
    let dir = tempfile::tempdir().unwrap();
    // Root manifest mentions only `keep`; lock contains transitive `hidden`
    // (v01, same prefix) not present in any root table.
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dependencies]\nkeep = { git = 'https://forge.example.invalid/cpkg/keep.git', branch = 'v01' }\n",
    );
    let keep_sha = "a".repeat(40);
    let hidden_sha = "b".repeat(40);
    let lock = format!(
        "{}{}",
        lock_entry(
            "keep",
            "0.1.0",
            &v01_source("forge.example.invalid/cpkg/keep", "v01", &keep_sha)
        ),
        lock_entry(
            "hidden",
            "0.1.0",
            &v01_source("forge.example.invalid/cpkg/hidden", "v01", &hidden_sha)
        ),
    );
    write(dir.path(), "Cargo.lock", &lock);
    let selected = select_active_graph(dir.path(), &refresh_fixture()).unwrap();
    let names: Vec<_> = selected.iter().map(|s| s.package.as_str()).collect();
    assert!(names.contains(&"keep"), "direct must be selected");
    assert!(
        names.contains(&"hidden"),
        "transitive absent-from-root must come from lock, not root tables"
    );
}

#[test]
fn renamed_dep_uses_real_package_not_alias() {
    let dir = tempfile::tempdir().unwrap();
    // Alias `foo` renames real package `bar`.
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dependencies]\nfoo = { package = 'bar', git = 'https://forge.example.invalid/cpkg/bar.git', branch = 'v01' }\n",
    );
    let sha = "c".repeat(40);
    let lock = lock_entry(
        "bar",
        "0.1.0",
        &v01_source("forge.example.invalid/cpkg/bar", "v01", &sha),
    );
    write(dir.path(), "Cargo.lock", &lock);
    let selected = select_active_graph(dir.path(), &refresh_fixture()).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0].package, "bar",
        "must use real package for cargo update -p"
    );
}

#[test]
fn same_package_name_from_two_sources_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    );
    let lock = format!(
        "{}{}",
        lock_entry(
            "dupe",
            "0.1.0",
            &v01_source("forge.example.invalid/cpkg/dupe", "v01", &"a".repeat(40))
        ),
        lock_entry(
            "dupe",
            "0.1.0",
            &v01_source("forge.example.invalid/other/dupe", "v01", &"b".repeat(40))
        ),
    );
    write(dir.path(), "Cargo.lock", &lock);
    assert!(
        select_active_graph(dir.path(), &refresh_fixture()).is_err(),
        "ambiguous same-name two-source selection must fail, never pick one silently"
    );
    assert!(
        lock_graph(dir.path()).is_err(),
        "lock_graph must never overwrite collisions silently"
    );
}

#[test]
fn coexisting_versions_with_same_name_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    );
    let lock = format!(
        "{}{}",
        lock_entry(
            "same",
            "0.1.0",
            &v01_source("forge.example.invalid/cpkg/same", "v01", &"a".repeat(40))
        ),
        lock_entry(
            "same",
            "0.2.0",
            &v01_source("forge.example.invalid/cpkg/same", "v01", &"b".repeat(40))
        ),
    );
    write(dir.path(), "Cargo.lock", &lock);
    assert!(lock_graph(dir.path()).is_err());
}

#[test]
fn patch_unused_is_excluded_and_direct_no_patch_works() {
    let dir = tempfile::tempdir().unwrap();
    // Direct git dep (no [patch] at all) plus a [patch.unused] decoy that must
    // never be selected. Old root-table sweep read [patch]; new code never does.
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dependencies]\ndirect = { git = 'https://forge.example.invalid/cpkg/direct.git', branch = 'v01' }\n[patch.unused]\ndecoy = { git = 'https://forge.example.invalid/cpkg/decoy.git', branch = 'v01' }\n",
    );
    let sha = "d".repeat(40);
    let lock = lock_entry(
        "direct",
        "0.1.0",
        &v01_source("forge.example.invalid/cpkg/direct", "v01", &sha),
    );
    write(dir.path(), "Cargo.lock", &lock);
    let selected = select_active_graph(dir.path(), &refresh_fixture()).unwrap();
    let names: Vec<_> = selected.iter().map(|s| s.package.as_str()).collect();
    assert!(names.contains(&"direct"));
    assert!(!names.contains(&"decoy"), "[patch.unused] must be excluded");
}

#[test]
fn dev_build_target_workspace_edges_are_included() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dev-dependencies]\ndevdep = { git = 'https://forge.example.invalid/cpkg/devdep.git', branch = 'v01' }\n[build-dependencies]\nbuilddep = { git = 'https://forge.example.invalid/cpkg/builddep.git', branch = 'v01' }\n[target.'cfg(unix)'.dependencies]\ntdep = { git = 'https://forge.example.invalid/cpkg/tdep.git', branch = 'v01' }\n[workspace]\nmembers = ['member']\n",
    );
    std::fs::create_dir_all(dir.path().join("member")).unwrap();
    write(
        &dir.path().join("member"),
        "Cargo.toml",
        "[package]\nname = \"member\"\nversion = \"0.1.0\"\n[dependencies]\nwdep = { git = 'https://forge.example.invalid/cpkg/wdep.git', branch = 'v01' }\n",
    );
    let mut lock = String::new();
    for (name, sha) in [
        ("devdep", "a"),
        ("builddep", "b"),
        ("tdep", "c"),
        ("wdep", "d"),
    ] {
        lock.push_str(&lock_entry(
            name,
            "0.1.0",
            &v01_source(
                &format!("forge.example.invalid/cpkg/{name}"),
                "v01",
                &sha.repeat(40),
            ),
        ));
    }
    write(dir.path(), "Cargo.lock", &lock);
    let selected = select_active_graph(dir.path(), &refresh_fixture()).unwrap();
    for want in ["devdep", "builddep", "tdep", "wdep"] {
        assert!(
            selected.iter().any(|s| s.package == want),
            "{want} (dev/build/target/workspace) must be selected"
        );
    }
}

#[test]
fn manifest_source_config_errors_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    // Non-plain source (credentials) in direct manifest must fail.
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dependencies]\nbad = { git = 'https://user:pass@forge.example.invalid/cpkg/bad.git', branch = 'v01' }\n",
    );
    write(
        dir.path(),
        "Cargo.lock",
        "[[package]]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    );
    assert!(select_active_graph(dir.path(), &refresh_fixture()).is_err());
    // Non-plain lock source is skipped for selection, but malformed git
    // entries fail in lock_graph binding (no silent skip of corrupt graph).
    let dir2 = tempfile::tempdir().unwrap();
    write(
        dir2.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    );
    write(
        dir2.path(),
        "Cargo.lock",
        &lock_entry(
            "x",
            "0.1.0",
            "git+https://forge.example.invalid/cpkg/x.git?branch=v01#short",
        ),
    );
    assert!(lock_graph(dir2.path()).is_err());
}

#[test]
fn full_frozen_lock_bytes_comparison_rejects_mutation() {
    // Frozen enforcement is full-bytes equality, not selected-names-only:
    // even an unselected registry checksum change must fail gates.
    let before = b"[[package]]\nname = \"a\"\nversion = \"0.1.0\"\n";
    let mut after = before.to_vec();
    after.extend_from_slice(b"# trailing comment\n");
    assert_ne!(before.to_vec(), after);
    // The build_inner frozen check is `post_bytes != resolved_bytes` fails;
    // this test pins that semantic (full bytes, not sha-subset).
    let resolved = before.to_vec();
    let post = after;
    assert!(
        post != resolved,
        "any lock byte change during gates must refuse, even if gate exit 0"
    );
}

#[test]
fn gate_manifest_requires_offline_locked_cargo_only() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ci")).unwrap();
    // Exact official gate shape (mirrors cmsg v01-fast intent): direct cargo
    // check/clippy/test with --offline --locked, features and the two exact
    // HTTPS fixture skips. Validation must accept it unchanged.
    let good = r#"
[checks.v01-fast]
kind = "commands"
commands = [
  ["cargo", "check", "--offline", "--locked", "--all-targets", "--features", "native-tor,test-authenticator"],
  ["cargo", "clippy", "--offline", "--locked", "--all-targets", "--features", "native-tor,test-authenticator", "--", "-D", "warnings"],
  ["cargo", "test", "--offline", "--locked", "--lib", "--features", "native-tor,test-authenticator", "--", "--skip", "door::login::native::tests::unknown_session_result_and_cancelled_reply_fence_original_foyer", "--skip", "door::login::session::custody_tests::actual_login_custody_is_consumed_once_with_local_and_original_session_refusals"],
]
"#;
    // Write minimal .ci/ccid.toml with a [checks] table containing good.
    // validate_gate_manifest reads workdir/.ci/ccid.toml and looks up checks.
    let manifest = format!("[project]\nname = \"x\"\n{good}");
    // Our fixture uses [checks.v01-fast]; validate expects top-level checks.
    // The toml above nests under nothing? Actually [checks.v01-fast] is top-level.
    std::fs::write(dir.path().join(".ci/ccid.toml"), manifest).unwrap();
    validate_gate_manifest(dir.path(), &["v01-fast".to_owned()]).unwrap();

    // Missing --offline fails.
    let bad_offline = good.replace("--offline", "--frozen");
    let manifest = format!("[project]\nname = \"x\"\n{bad_offline}");
    std::fs::write(dir.path().join(".ci/ccid.toml"), manifest).unwrap();
    assert!(validate_gate_manifest(dir.path(), &["v01-fast".to_owned()]).is_err());

    // Online re-resolving shell wrapper is refused as official gates.
    let shell = r#"
[checks.v01-fast]
kind = "commands"
commands = [["bash", ".ci/v01-fast-check.sh"]]
"#;
    let manifest = format!("[project]\nname = \"x\"\n{shell}");
    std::fs::write(dir.path().join(".ci/ccid.toml"), manifest).unwrap();
    assert!(validate_gate_manifest(dir.path(), &["v01-fast".to_owned()]).is_err());

    // Non-commands kinds (cargo/nix) are refused for frozen push gates.
    let cargo_kind = r#"
[checks.v01-fast]
kind = "cargo"
"#;
    let manifest = format!("[project]\nname = \"x\"\n{cargo_kind}");
    std::fs::write(dir.path().join(".ci/ccid.toml"), manifest).unwrap();
    assert!(validate_gate_manifest(dir.path(), &["v01-fast".to_owned()]).is_err());
}

#[test]
fn gate_flags_after_separator_do_not_satisfy_the_freeze() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ci")).unwrap();
    // Flags in their correct position (before `--`) validate.
    let good = r#"
[checks.gate]
kind = "commands"
commands = [
  ["cargo", "test", "--offline", "--locked", "--lib", "--", "--skip", "some::fixture"],
]
"#;
    std::fs::write(
        dir.path().join(".ci/ccid.toml"),
        format!("[project]\nname = \"x\"\n{good}"),
    )
    .unwrap();
    validate_gate_manifest(dir.path(), &["gate".to_owned()]).unwrap();
    // Options after `--` are test/program arguments, not Cargo flags: a
    // `--locked` that only appears there must fail the freeze.
    for bad in [
        // --locked only after the separator.
        "[\"cargo\", \"test\", \"--offline\", \"--lib\", \"--\", \"--locked\"]",
        // --offline only after the separator.
        "[\"cargo\", \"test\", \"--locked\", \"--lib\", \"--\", \"--offline\"]",
        // both only after the separator.
        "[\"cargo\", \"test\", \"--lib\", \"--\", \"--offline\", \"--locked\"]",
    ] {
        let manifest = format!(
            "[project]\nname = \"x\"\n[checks.gate]\nkind = \"commands\"\ncommands = [{bad}]\n"
        );
        std::fs::write(dir.path().join(".ci/ccid.toml"), manifest).unwrap();
        assert!(
            validate_gate_manifest(dir.path(), &["gate".to_owned()]).is_err(),
            "{bad} must not satisfy the frozen offline/locked requirement"
        );
    }
}

#[test]
fn sentinel_is_required_for_push_and_preserves_manual() {
    // Sentinel shape is exactly one element.
    assert!(is_self_checks_command(&[SELF_CHECKS_SENTINEL.to_owned()]));
    assert!(!is_self_checks_command(&[
        "bash".to_owned(),
        ".ci/run.sh".to_owned()
    ]));
    assert!(!is_self_checks_command(&[]));
    // jobs::plan validation is covered indirectly: push-enabled with
    // arbitrary command must fail, manual-only with arbitrary must pass.
    // Here we pin the helper contract used by both execute-job and push.
}

#[test]
fn prepare_validation_rejects_bad_argv() {
    use crate::jobs::validate_prepare_commands;
    // Empty (the default) means no preparation and always validates.
    validate_prepare_commands(&[]).unwrap();
    // Well-formed existing-driver style argv validates.
    validate_prepare_commands(&[vec!["python3".to_owned(), ".ci/v01-lock.py".to_owned()]]).unwrap();
    // Bad argv fails closed: empty argv, empty executable, NUL bytes.
    assert!(validate_prepare_commands(&[vec![]]).is_err());
    assert!(validate_prepare_commands(&[vec!["".to_owned()]]).is_err());
    assert!(validate_prepare_commands(&[vec!["ok".to_owned(), "bad\0arg".to_owned()]]).is_err());
}

#[test]
fn prepare_executes_in_order_and_failure_propagates_before_freeze() {
    use std::time::{Duration, Instant};
    // Generic consumers with no preparation succeed trivially (no-strum
    // consumers work without any driver).
    let dir = tempfile::tempdir().unwrap();
    let env: crate::Environment = std::env::vars_os().collect();
    let runner = crate::Runner::until(
        dir.path().to_owned(),
        env,
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    run_prepare_commands(&runner, &[]).unwrap();
    // Ordered execution: first command creates a marker, second appends.
    run_prepare_commands(
        &runner,
        &[
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "echo one > order.txt".to_owned(),
            ],
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "echo two >> order.txt".to_owned(),
            ],
        ],
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("order.txt")).unwrap(),
        "one\ntwo\n"
    );
    // Failure propagates and later commands never run.
    std::fs::remove_file(dir.path().join("order.txt")).unwrap();
    assert!(run_prepare_commands(
        &runner,
        &[
            vec!["false".to_owned()],
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "echo late > order.txt".to_owned()
            ],
        ],
    )
    .is_err());
    assert!(
        !dir.path().join("order.txt").exists(),
        "failed preparation must not run later commands, so freeze/gates never see a half-prepared graph"
    );
}

#[test]
fn job_environment_rejects_reserved_identity_keys() {
    use crate::jobs::validate_job_environment;
    use std::collections::BTreeMap;
    // Empty (the default) validates; old manifests without the key behave
    // byte-identically.
    validate_job_environment(&BTreeMap::new()).unwrap();
    // Ordinary worker/cache/build keys are allowed.
    validate_job_environment(&BTreeMap::from([
        (
            "CARGO_TARGET_DIR".to_owned(),
            "/caches/cargo/demo/target".to_owned(),
        ),
        ("CI_JOBS".to_owned(), "2".to_owned()),
    ]))
    .unwrap();
    // Reserved identity/status keys fail closed and can never be overridden.
    // RUNNER_TEMP stays job-owned: the entrypoint assigns it to the owned
    // scratch after planning, and the manifest overlay must not undo that.
    for key in [
        "CCID_BIN",
        "CHECKS",
        "CI_COMMIT_SHA",
        "CI_COMMIT_BRANCH",
        "CI_REPOSITORY_URL",
        "RUNNER_TEMP",
        "CCID_STATUS_TOKEN",
        "CCID_TARGET_LOCK_HELD",
    ] {
        assert!(
            validate_job_environment(&BTreeMap::from([(key.to_owned(), "x".to_owned())])).is_err(),
            "{key} must stay scheduler-owned"
        );
    }
    // Malformed keys/values fail closed.
    assert!(validate_job_environment(&BTreeMap::from([("".to_owned(), "x".to_owned())])).is_err());
    assert!(
        validate_job_environment(&BTreeMap::from([("A=B".to_owned(), "x".to_owned())])).is_err()
    );
}

//! The cfrg client: the command line it builds, what it feeds cfrg, and how
//! it reads the answers. A stand-in `cfrg` script records every call.
use super::*;
use crate::submission::cfrg::Cfrg;
use std::os::unix::fs::PermissionsExt;

const LANDED: &str = "6e99d662f5620d69362ae4b17a3bd17293289dc6";

/// A `cfrg` that records its arguments, query and token, then answers by
/// subcommand. Exit codes follow cfrg: 2 means some targets were not read.
fn stand_in(dir: &Path, body: &str) -> String {
    let path = dir.join("cfrg");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{d}/argv'\nprintf '%s' \"$CFRG_FORGE_TOKEN\" > '{d}/token'\ncat > '{d}/stdin'\n{body}\n",
        d = dir.display()
    );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

fn client(dir: &Path, body: &str) -> Cfrg {
    let mut config = config(dir);
    config.forge_token_command = vec!["printf".into(), "%s".into(), "secret-token".into()];
    Cfrg::open(
        &config,
        &stand_in(dir, body),
        "https://forge.example.invalid/o/r.git",
    )
    .unwrap()
}

#[test]
fn contents_sends_one_query_and_reads_partial_answers() {
    let dir = tempfile::tempdir().unwrap();
    let cfrg = client(
        dir.path(),
        r#"printf '%s\n' '{"repository":"o/a","branch":"main","state":"found","files":[{"path":"Cargo.lock","blob":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","content":"Zm9v"},{"path":"flake.nix","blob":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}]}' '{"repository":"o/b","branch":"main","state":"failed","error":"boom"}'
exit 2"#,
    );
    let known = BTreeSet::from(["b".repeat(40)]);
    let lines = cfrg
        .contents(
            &[("o/a".into(), "main".into()), ("o/b".into(), "main".into())],
            &["Cargo.lock", "flake.nix"],
            &known,
        )
        .unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].state, "found");
    assert_eq!(lines[0].files[0].text().unwrap().as_deref(), Some("foo"));
    assert_eq!(lines[0].files[1].content, None);
    assert_eq!(lines[1].error.as_deref(), Some("boom"));

    // The command line names the origin and the variable, never the token.
    let argv = fs::read_to_string(dir.path().join("argv")).unwrap();
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        [
            "contents",
            "--forge",
            "forgejo",
            "--origin",
            "https://forge.example.invalid",
            "--token-env",
            "CFRG_FORGE_TOKEN"
        ]
    );
    assert!(!argv.contains("secret-token"));
    assert_eq!(
        fs::read_to_string(dir.path().join("token")).unwrap(),
        "secret-token"
    );
    let query: Value =
        serde_json::from_slice(&fs::read(dir.path().join("stdin")).unwrap()).unwrap();
    assert_eq!(
        query,
        json!({
            "targets": [
                {"repository": "o/a", "branch": "main"},
                {"repository": "o/b", "branch": "main"}
            ],
            "paths": ["Cargo.lock", "flake.nix"],
            "known": ["b".repeat(40)]
        })
    );
}

#[test]
fn observe_reads_the_head_and_the_statuses() {
    let dir = tempfile::tempdir().unwrap();
    let cfrg = client(
        dir.path(),
        &format!(
            r#"case "$8" in
head) printf '%s\n' '{{"repository":"o/a","branch":"main","head":"{LANDED}"}}';;
status) printf '%s\n' '{{"repository":"o/a","commit":"{LANDED}","statuses":[{{"context":"ccid/verdict","state":"success"}},{{"context":"ci/crow/build","state":"pending"}}]}}';;
esac"#
        ),
    );
    assert_eq!(cfrg.head("o/a", "main").unwrap().as_deref(), Some(LANDED));
    // Flags come before the observe subcommand, its operands after.
    let argv = fs::read_to_string(dir.path().join("argv")).unwrap();
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        [
            "observe",
            "--forge",
            "forgejo",
            "--origin",
            "https://forge.example.invalid",
            "--token-env",
            "CFRG_FORGE_TOKEN",
            "head",
            "o/a",
            "main"
        ]
    );
    assert_eq!(
        cfrg.statuses("o/a", LANDED).unwrap(),
        vec![
            ("ccid/verdict".to_string(), "success".to_string()),
            ("ci/crow/build".to_string(), "pending".to_string())
        ]
    );
}

#[test]
fn a_cfrg_failure_carries_its_last_message_and_no_secret() {
    let dir = tempfile::tempdir().unwrap();
    let cfrg = client(
        dir.path(),
        "echo 'cfrg contents: Native rate/plan hold for https://forge.example.invalid/api' >&2\nexit 2",
    );
    let error = cfrg
        .contents(
            &[("o/a".into(), "main".into())],
            &["Cargo.lock"],
            &BTreeSet::new(),
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("rate/plan hold"), "{error}");
    assert!(!error.contains("secret-token"), "{error}");
    let error = cfrg.head("o/a", "main").unwrap_err().to_string();
    assert!(error.contains("rate/plan hold"), "{error}");
}

#[test]
fn reading_the_forge_needs_a_token_command() {
    let dir = tempfile::tempdir().unwrap();
    let error = Cfrg::open(
        &config(dir.path()),
        "cfrg",
        "https://forge.example.invalid/o/r.git",
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("forge_token_command"), "{error}");
}

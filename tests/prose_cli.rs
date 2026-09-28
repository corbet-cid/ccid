#![cfg(unix)]
use serde_json::{json, Value};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

fn backend(root: &std::path::Path, output: &Value) {
    let path = root.join("vale");
    fs::write(&path, format!(
        "#!/bin/sh\nif [ \"$1\" = '--version' ]; then printf '%s\\n' 'vale version 3.23.0'; else printf '%s\\n' '{}'; fi\n",
        output
    )).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn result() -> Value {
    json!({"files":{}, "counts":{"Cqlt.Repetition":0,"Cqlt.VagueClaims":0,"Cqlt.Wordiness":0}})
}

#[test]
fn prose_cli_delegates_to_cqlt_and_distinguishes_quality_from_execution_failure() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("README.md");
    fs::write(&input, "A tool for checking descriptions.\n").unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_ccid"))
            .args(["quality", "prose", "--json", "--file"])
            .arg(&input)
            .env("PATH", root.path())
            .output()
            .unwrap()
    };
    assert_eq!(run().status.code(), Some(2), "missing Vale must fail");
    backend(root.path(), &result());
    let good = run();
    assert_eq!(
        good.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&good.stderr)
    );
    assert_eq!(good.stdout, run().stdout);
    assert!(String::from_utf8_lossy(&good.stderr).contains("\"event\":\"command\""));
    let report: Value = serde_json::from_slice(&good.stdout).unwrap();
    assert_eq!(report["ruleset"], cqlt::prose::RULESET);
    fs::write(&input, "\n").unwrap();
    assert_eq!(
        run().status.code(),
        Some(1),
        "empty input is a quality failure"
    );
    backend(root.path(), &json!({}));
    assert_eq!(
        run().status.code(),
        Some(2),
        "invalid backend output must fail"
    );
}

#[test]
fn prose_cli_refuses_implicit_scope_and_incomplete_collection() {
    let root = tempfile::tempdir().unwrap();
    let base = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccid"));
        command.args(["quality", "prose"]).env("PATH", root.path());
        command
    };
    assert_eq!(base().output().unwrap().status.code(), Some(2));
    let snapshot = root.path().join("snapshot.json");
    fs::write(
        &snapshot,
        json!({
            "schema":1,"source":"forgejo:https://forge.example/api/v1","collected_at":"2026-01-01",
            "scope":["example"],"complete":false,"errors":["incomplete"],"organizations":[]
        })
        .to_string(),
    )
    .unwrap();
    let output = base().arg("--snapshot").arg(snapshot).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("complete collection"));
}

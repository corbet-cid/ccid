use serde_json::json;
use std::{fs, process::Command};

fn snapshot() -> serde_json::Value {
    json!({
        "schema":1,"source":"github:https://api.github.com","collected_at":"2026-01-01",
        "scope":["example"],"complete":true,"errors":[],
        "organizations":[{"login":"example","name":"Example","description":"Tools for authors","website":null,
            "profile":{"state":"present","path":"profile/README.md","bytes":100},"repository_count":0,"repositories":[]}]
    })
}

#[test]
fn quality_cli_is_offline_stable_and_preserves_distinct_exit_codes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("snapshot.json");
    let mut value = snapshot();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_ccid"))
            .args(["quality", "check", "--json", "--snapshot"])
            .arg(&path)
            .env("PATH", root.path())
            .output()
            .unwrap()
    };
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let first = run();
    let second = run();
    assert_eq!(
        first.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(first.stdout, second.stdout);
    let report: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(report["ruleset"], cqlt::RULESET);
    value["organizations"][0]["description"] = serde_json::Value::Null;
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(run().status.code(), Some(1));
    value["complete"] = json!(false);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(run().status.code(), Some(2));
    fs::write(&path, b"not JSON").unwrap();
    assert_eq!(run().status.code(), Some(2));
}

#[test]
fn quality_cli_rejects_policy_typos() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("snapshot.json");
    let policy = root.path().join("policy.json");
    fs::write(&path, serde_json::to_vec(&snapshot()).unwrap()).unwrap();
    fs::write(&policy, br#"{"schema":1,"exeptions":[]}"#).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args(["quality", "check", "--snapshot"])
        .arg(path)
        .arg("--policy")
        .arg(policy)
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
}

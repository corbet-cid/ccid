#![forbid(unsafe_code)]

//! Offline CLI coverage for `ccid forge access plan|apply`. No network, no
//! provider credentials: collectors are fixture JSON files.
use serde_json::Value;
use std::{fs, process::Command};
use tempfile::TempDir;

const IDENTITIES: &str = r#"
schema = 1
[people.alice-example]
github = "alice-gh"
forgejo = "alice-fj"
gitlab = "alice-gl"
[people.bruno-example]
github = "bruno-gh"
forgejo = "bruno-fj"
bitbucket = "bruno-bb"
"#;

fn snapshot(grants: &str) -> String {
    format!(r#"{{"forge": "FORGE", "observed_at": 1759820000, "grants": [{grants}]}}"#)
}

fn access(args: &[&str]) -> (bool, Value, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args(["forge", "access"])
        .args(args)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let plan: Value = serde_json::from_str(&stdout).unwrap();
    (
        output.status.success(),
        plan,
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn fixture() -> TempDir {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::write(root.join("ids.toml"), IDENTITIES).unwrap();
    let fj = snapshot(
        r#"{"handle": "alice-fj", "org": "acme", "team": "dev", "role": "write", "changed_at": 1759810000}"#,
    )
    .replace("FORGE", "forgejo");
    let gl = snapshot("").replace("FORGE", "gitlab");
    let gh = snapshot("").replace("FORGE", "github");
    fs::write(root.join("fj.json"), fj).unwrap();
    fs::write(root.join("gl.json"), gl).unwrap();
    fs::write(root.join("gh.json"), gh).unwrap();
    temp
}

#[test]
fn plan_reports_mirror_calls_without_writing_state() {
    let temp = fixture();
    let root = temp.path().to_str().unwrap().to_string();
    let state = format!("{root}/state.json");
    let (ok, plan, _) = access(&[
        "plan",
        "--identities",
        &format!("{root}/ids.toml"),
        "--baseline",
        &state,
        "--observed",
        &format!("forgejo={root}/fj.json"),
        "--observed",
        &format!("gitlab={root}/gl.json"),
    ]);
    // A missing baseline plans nothing; the operator must initialize first.
    assert!(!ok);
    assert_eq!(plan["baseline_present"], false);
    assert!(plan["actions"].as_array().unwrap().is_empty());
    assert!(!std::path::Path::new(&state).exists());
}

#[test]
fn initialize_then_converge_then_mirror() {
    let temp = fixture();
    let root = temp.path().to_str().unwrap().to_string();
    let ids = format!("{root}/ids.toml");
    let state = format!("{root}/state.json");
    let fj = format!("forgejo={root}/fj.json");
    let gl = format!("gitlab={root}/gl.json");
    let output = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args([
            "forge",
            "access",
            "apply",
            "--identities",
            &ids,
            "--baseline",
            &state,
            "--observed",
            &fj,
            "--observed",
            &gl,
            "--initialize",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(std::path::Path::new(&state).exists());
    // Baseline now matches observations: the plan is converged.
    let (ok, plan, _) = access(&[
        "plan",
        "--identities",
        &ids,
        "--baseline",
        &state,
        "--observed",
        &fj,
        "--observed",
        &gl,
    ]);
    assert!(ok, "{plan}");
    assert_eq!(plan["complete"], true);
    // A fresh Forgejo grant mirrors to GitLab, never to frozen GitHub.
    let changed = snapshot(
        r#"{"handle": "alice-fj", "org": "acme", "team": "dev", "role": "admin", "changed_at": 1759830000}"#,
    )
    .replace("FORGE", "forgejo");
    fs::write(format!("{root}/fj.json"), changed).unwrap();
    let gh = format!("github={root}/gh.json");
    let (ok, plan, _) = access(&[
        "plan",
        "--identities",
        &ids,
        "--baseline",
        &state,
        "--observed",
        &fj,
        "--observed",
        &gl,
        "--observed",
        &gh,
    ]);
    assert!(!ok);
    assert_eq!(plan["actions"].len(), 1);
    assert_eq!(plan["actions"][0]["call"]["forge"], "gitlab");
    assert_eq!(plan["actions"][0]["call"]["operation"], "set-level");
    assert_eq!(plan["actions"][0]["call"]["method"], "POST");
    assert!(plan["skipped_frozen"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["call"]["forge"] == "github"));
    // Apply refuses live writes in this draft and leaves state untouched.
    let before = fs::read_to_string(&state).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args([
            "forge",
            "access",
            "apply",
            "--identities",
            &ids,
            "--baseline",
            &state,
            "--observed",
            &fj,
            "--observed",
            &gl,
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&state).unwrap(), before);
}

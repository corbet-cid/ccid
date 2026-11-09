//! Resolver response transport + real-git behavior tests (fixtures only).
use ccid::resolver::{
    apply_response, build_inventory, parse_plan_output, Alias, RunnerConfig, Store,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::process::Command;

fn config() -> RunnerConfig {
    RunnerConfig {
        schema: 1,
        canonical_base: "https://git.example".into(),
        aliases: vec![Alias {
            url_prefix: "https://github.com/acme".into(),
            canonical_owner: "acme".into(),
        }],
        stores: vec![Store {
            kind: "http-forge".into(),
            location: "http://forgejo.example:3001".into(),
            identity: "forgejo".into(),
            scope: vec!["acme".into()],
            provider: Some("forgejo".into()),
            credential_env: Some("CFRG_RESOLVER_FORGEJO_TOKEN".into()),
            username: Some("oauth2".into()),
            trusted_single_user: false,
        }],
        placement: None,
        tool: None,
        primary_source: None,
        emergency_fallback: false,
        timeout_secs: 30,
    }
}

/// Fabricated cfrg-style response carrying ACTUAL clmr semantics
/// (`instead_of_pairs`: every declared `source_urls` form maps to the
/// decision destination — `via` when routed, the bare canonical pointer when
/// not; canonical bare self-maps are longest-match guards).
/// widget: routed pinned (primary forgejo). neighbor: pointer on moving ref
/// with a non-Forgejo primary (hub).
fn response_text() -> String {
    serde_json::json!({
        "schema": 1,
        "canonical_base": "https://git.example",
        "decisions": [
            {"id": "acme/widget", "path": "acme/widget",
             "ref": "pinned:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
             "primary": "forgejo", "outcome": "routed", "store": 0,
             "via": "http://forgejo.example:3001/acme/widget.git",
             "primary_url": null, "note": "pinned commit verified on store 0",
             "instead_of": [
                 ["https://git.example/acme/widget",
                  "http://forgejo.example:3001/acme/widget.git"],
                 ["https://git.example/acme/widget.git",
                  "http://forgejo.example:3001/acme/widget.git"],
                 ["https://github.com/acme/widget",
                  "http://forgejo.example:3001/acme/widget.git"],
                 ["https://github.com/acme/widget.git",
                  "http://forgejo.example:3001/acme/widget.git"]]},
            {"id": "acme/neighbor", "path": "acme/neighbor",
             "ref": "moving:refs/heads/main", "primary": "hub",
             "outcome": "canonical-pointer", "store": null, "via": null,
             "primary_url": "https://github.com/acme/neighbor",
             "note": "non-Forgejo primary; pointer preserved",
             "instead_of": [
                 ["https://git.example/acme/neighbor",
                  "https://git.example/acme/neighbor"],
                 ["https://git.example/acme/neighbor.git",
                  "https://git.example/acme/neighbor"],
                 ["https://github.com/acme/neighbor",
                  "https://git.example/acme/neighbor"],
                 ["https://github.com/acme/neighbor.git",
                  "https://git.example/acme/neighbor"]]}
        ],
        "credentials": [{"match": "http://forgejo.example:3001",
                         "env": "CFRG_RESOLVER_FORGEJO_TOKEN",
                         "username": "oauth2"}],
        "git_config": ""
    })
    .to_string()
}

fn requested() -> BTreeSet<String> {
    BTreeSet::from(["acme/widget".to_owned(), "acme/neighbor".to_owned()])
}

/// Real `git ls-remote --get-url` (local rewrite only, no network) under the
/// emitted config. Hermetic: system/global config disabled.
fn get_url(env: &BTreeMap<OsString, OsString>, url: &str) -> String {
    let out = Command::new("git")
        .args(["ls-remote", "--get-url", url])
        .envs(env)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git binary");
    assert!(out.status.success(), "get-url failed for {url}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn response_transport_and_real_git_prefix_behavior() {
    let config = config();
    let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
    let routed = apply_response(&mut env, &response_text(), &requested(), "cfrg", &config).unwrap();
    assert_eq!(routed, 8);
    // Routed repo: every declared form maps to the selected endpoint,
    // including owned GitHub aliases and the bare canonical form.
    assert_eq!(
        get_url(&env, "https://git.example/acme/widget"),
        "http://forgejo.example:3001/acme/widget.git"
    );
    assert_eq!(
        get_url(&env, "https://git.example/acme/widget.git"),
        "http://forgejo.example:3001/acme/widget.git"
    );
    assert_eq!(
        get_url(&env, "https://github.com/acme/widget"),
        "http://forgejo.example:3001/acme/widget.git"
    );
    assert_eq!(
        get_url(&env, "https://github.com/acme/widget.git"),
        "http://forgejo.example:3001/acme/widget.git"
    );
    // Raw-prefix artifacts of the same pairs (documented, not exact-safe).
    assert_eq!(
        get_url(&env, "https://git.example/acme/widget/"),
        "http://forgejo.example:3001/acme/widget.git/"
    );
    assert_eq!(
        get_url(&env, "https://git.example/acme/widget.git-evil"),
        "http://forgejo.example:3001/acme/widget.git-evil"
    );
    // Non-Forgejo-primary neighbor: every declared form resolves to the
    // canonical pointer (bare self-map is the longest-match guard).
    assert_eq!(
        get_url(&env, "https://git.example/acme/neighbor"),
        "https://git.example/acme/neighbor"
    );
    assert_eq!(
        get_url(&env, "https://git.example/acme/neighbor.git"),
        "https://git.example/acme/neighbor"
    );
    assert_eq!(
        get_url(&env, "https://git.example/acme/neighbor.git-evil"),
        "https://git.example/acme/neighbor-evil"
    );
    assert_eq!(
        get_url(&env, "https://github.com/acme/neighbor"),
        "https://git.example/acme/neighbor"
    );
    assert_eq!(
        get_url(&env, "https://github.com/acme/neighbor.git"),
        "https://git.example/acme/neighbor"
    );
    // Non-owned URLs are untouched.
    assert_eq!(
        get_url(&env, "https://github.com/other/repo.git"),
        "https://github.com/other/repo.git"
    );
    // Credential entries clear inherited helpers first, then set the scoped
    // helper by exact origin. No token material anywhere.
    let count: usize = env[&OsString::from("GIT_CONFIG_COUNT")]
        .to_string_lossy()
        .parse()
        .unwrap();
    let values: Vec<String> = (0..count)
        .map(|i| {
            env[&OsString::from(format!("GIT_CONFIG_VALUE_{i}"))]
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(values.contains(&String::new()));
    assert!(values.iter().any(|v| v.contains("cfrg credential-helper --env CFRG_RESOLVER_FORGEJO_TOKEN --expect-origin http://forgejo.example:3001")));
    assert!(!values
        .iter()
        .any(|v| v.contains("oauth2:") || v.contains("password=")));
}

#[test]
fn conflicting_same_source_fails_closed() {
    let config = config();
    let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
    let mut value: serde_json::Value = serde_json::from_str(&response_text()).unwrap();
    value["decisions"][0]["instead_of"] = serde_json::json!([
        [
            "https://git.example/acme/widget.git",
            "http://forgejo.example:3001/acme/widget.git"
        ],
        [
            "https://git.example/acme/widget.git",
            "http://other.example/acme/widget.git"
        ],
    ]);
    assert!(apply_response(&mut env, &value.to_string(), &requested(), "cfrg", &config).is_err());
    assert!(!env.contains_key(&OsString::from("GIT_CONFIG_COUNT")));
}

#[test]
fn secret_values_in_response_are_refused() {
    let config = config();
    let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
    env.insert(
        OsString::from("CFRG_RESOLVER_FORGEJO_TOKEN"),
        OsString::from("leaked-secret-value"),
    );
    // Well-formed response carrying the secret value in a note must fail.
    let mut value: serde_json::Value = serde_json::from_str(&response_text()).unwrap();
    value["decisions"][0]["note"] = serde_json::json!("oops leaked-secret-value");
    assert!(apply_response(&mut env, &value.to_string(), &requested(), "cfrg", &config).is_err());
}

#[test]
fn incomplete_response_is_refused() {
    let config = config();
    let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
    let mut value: serde_json::Value = serde_json::from_str(&response_text()).unwrap();
    value["decisions"].as_array_mut().unwrap().pop();
    assert!(apply_response(&mut env, &value.to_string(), &requested(), "cfrg", &config).is_err());
}

#[test]
fn inventory_covers_manifest_lock_and_manual() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[dependencies]\nwidget = { git = \"https://github.com/acme/widget\", branch = \"main\" }\n",
    )
    .unwrap();
    let sha = "d".repeat(40);
    std::fs::write(
        dir.path().join("Cargo.lock"),
        format!("[[package]]\nname = \"widget\"\nversion = \"0.1.0\"\nsource = \"git+https://github.com/acme/widget?branch=main#{sha}\"\n[[package]]\nname = \"tool\"\nversion = \"0.2.0\"\nsource = \"git+https://git.example/acme/tool?rev={sha}#{sha}\"\n"),
    )
    .unwrap();
    let config = config();
    let manual = vec![format!("https://git.example/acme/extra#{sha}")];
    let inv = build_inventory(dir.path(), &manual, &config).unwrap();
    let paths: Vec<&str> = inv.iter().map(|d| d.path.as_str()).collect();
    assert_eq!(paths, vec!["acme/extra", "acme/tool", "acme/widget"]);
    assert_eq!(inv[2].moving, Some("refs/heads/main".into()));
    assert_eq!(inv[1].pinned, Some(sha.clone()));
    // Declared forms: canonical bare/.git + owned alias bare/.git.
    assert!(inv[2].sources.contains("https://git.example/acme/widget"));
    assert!(inv[2]
        .sources
        .contains("https://github.com/acme/widget.git"));
    // Non-owned manual URLs are ignored, never requested.
    let inv2 = build_inventory(
        dir.path(),
        &["https://github.com/other/repo#".to_owned() + &sha],
        &config,
    )
    .unwrap();
    assert_eq!(inv2.len(), 2);
}

#[test]
fn same_owner_repos_keep_distinct_plan_primaries() {
    // Two plan outputs, one owner, different primary_forge values: the
    // runtime must not collapse them into an owner-level primary.
    let plans = [
        (
            r#"{"repository":"a-w","primary_forge":"forgejo","clone_urls":["https://git.example/acme/w"]}"#,
            "forgejo",
        ),
        (
            r#"{"repository":"a-n","primary_forge":"hub","clone_urls":[]}"#,
            "hub",
        ),
    ];
    for (text, want) in plans {
        let (primary, _) = parse_plan_output(text).unwrap();
        assert_eq!(primary, want);
    }
}

/// The resolver's `emergency-fallback` outcome (a moving ref served by a
/// declared store because the canonical pointer or the primary could not
/// answer) installs its rewrite and credential scope like a routed store. The
/// warning goes to the job log; the response itself is otherwise unchanged.
#[test]
fn emergency_fallback_outcome_is_accepted_and_routes_through_its_store() {
    let config = config();
    let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
    let mut value: serde_json::Value = serde_json::from_str(&response_text()).unwrap();
    value["decisions"][1]["outcome"] = serde_json::json!("emergency-fallback");
    value["decisions"][1]["store"] = serde_json::json!(0);
    value["decisions"][1]["via"] =
        serde_json::json!("http://forgejo.example:3001/acme/neighbor.git");
    value["decisions"][1]["instead_of"] = serde_json::json!([
        [
            "https://git.example/acme/neighbor",
            "http://forgejo.example:3001/acme/neighbor.git"
        ],
        [
            "https://git.example/acme/neighbor.git",
            "http://forgejo.example:3001/acme/neighbor.git"
        ]
    ]);
    let routed =
        apply_response(&mut env, &value.to_string(), &requested(), "cfrg", &config).unwrap();
    assert!(routed > 0);
    let keys: Vec<String> = env
        .iter()
        .filter(|(name, _)| name.to_string_lossy().starts_with("GIT_CONFIG_KEY_"))
        .map(|(_, value)| value.to_string_lossy().into_owned())
        .collect();
    assert!(keys
        .iter()
        .any(|key| key == "url.http://forgejo.example:3001/acme/neighbor.git.insteadOf"));
}

#[test]
fn unknown_outcomes_are_still_refused() {
    let config = config();
    let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
    let mut value: serde_json::Value = serde_json::from_str(&response_text()).unwrap();
    value["decisions"][1]["outcome"] = serde_json::json!("emergency");
    assert!(apply_response(&mut env, &value.to_string(), &requested(), "cfrg", &config).is_err());
}

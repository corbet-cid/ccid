use super::*;
use serde_json::json;
use std::collections::BTreeMap;

struct Fixture {
    forge: Forge,
    responses: BTreeMap<String, Value>,
}
impl Api for Fixture {
    fn forge(&self) -> Forge {
        self.forge
    }
    fn source(&self) -> String {
        "https://forge.example/api".into()
    }
    fn get(&self, path: &str) -> Result<Option<Value>> {
        Ok(self.responses.get(path).cloned())
    }
}

fn fixture(forge: Forge) -> Fixture {
    let mut responses = BTreeMap::new();
    let github = forge == Forge::Github;
    let size = if github { "per_page" } else { "limit" };
    responses.insert(
        format!("user/orgs?{size}=100&page=1"),
        json!([{ "id":1,"login":"example","name":"example" }]),
    );
    responses.insert(format!("user/orgs?{size}=100&page=2"), json!([]));
    responses.insert("orgs/example".into(),json!({"name":"Example","full_name":"Example","description":"Tools for authors","blog":"https://example.org","website":"https://example.org"}));
    let prefix = if github {
        "orgs/example/repos?type=all&"
    } else {
        "orgs/example/repos?"
    };
    responses.insert(format!("{prefix}{size}=100&page=1"),json!([{"id":2,"name":"editor","full_name":"example/editor","description":"An editor for authors","homepage":"https://example.org/editor","website":"https://example.org/editor","private":false,"fork":false,"archived":false,"topics":["editing"],"default_branch":"main"}]));
    responses.insert(format!("{prefix}{size}=100&page=2"), json!([]));
    let revision = "a".repeat(40);
    responses.insert(
        "repos/example/editor/branches/main".into(),
        json!({"commit":{"sha":revision,"id":revision}}),
    );
    responses.insert(
        "repos/example/editor/topics".into(),
        json!({"topics":["editing"]}),
    );
    responses.insert(
        format!("repos/example/editor/contents?ref={revision}"),
        json!([
            {"name":"README.md","type":"file","size":80},
            {"name":"LICENSE-MIT","type":"file","size":1000}
        ]),
    );
    responses.insert(
        format!("repos/example/editor/readme?ref={revision}"),
        json!({"path":"README.md","size":80}),
    );
    Fixture { forge, responses }
}

#[test]
fn both_forges_normalize_to_the_same_policy_evidence() {
    let github = collect(&fixture(Forge::Github), vec![]).unwrap();
    let forgejo = collect(&fixture(Forge::Forgejo), vec![]).unwrap();
    assert!(github.complete && forgejo.complete);
    assert_eq!(github.organizations.len(), 1);
    assert_eq!(github.organizations[0].repositories.len(), 1);
    let a = cqlt::evaluate(&github, &Policy::default()).unwrap();
    let b = cqlt::evaluate(&forgejo, &Policy::default()).unwrap();
    assert_eq!(
        serde_json::to_value(&a.checks).unwrap(),
        serde_json::to_value(&b.checks).unwrap()
    );
}

#[test]
fn capped_short_pages_do_not_truncate_inventory() {
    let mut api = fixture(Forge::Github);
    api.responses.insert(
        "user/orgs?per_page=100&page=2".into(),
        json!([{"id":3,"login":"second"}]),
    );
    api.responses
        .insert("user/orgs?per_page=100&page=3".into(), json!([]));
    assert_eq!(pages(&api, "user/orgs").unwrap().len(), 2);
    let snapshot = collect(&api, vec![]).unwrap();
    assert!(!snapshot.complete);
    assert_eq!(
        cqlt::evaluate(&snapshot, &Policy::default())
            .unwrap()
            .exit_code(Severity::Error),
        2
    );
}

#[test]
fn repeated_pages_and_missing_pages_are_errors() {
    let mut api = fixture(Forge::Forgejo);
    let page = api.responses["user/orgs?limit=100&page=1"].clone();
    api.responses
        .insert("user/orgs?limit=100&page=2".into(), page);
    assert!(pages(&api, "user/orgs").is_err());
    api.responses.remove("user/orgs?limit=100&page=2");
    assert!(pages(&api, "user/orgs").is_err());
}

#[test]
fn unreadable_contents_stays_unknown_instead_of_missing() {
    let mut api = fixture(Forge::Github);
    api.responses.remove(&format!(
        "repos/example/editor/contents?ref={}",
        "a".repeat(40)
    ));
    let snapshot = collect(&api, vec![]).unwrap();
    assert!(!snapshot.complete);
    assert!(matches!(
        snapshot.organizations[0].repositories[0].readme,
        Document::Unknown { .. }
    ));
    assert_eq!(
        cqlt::evaluate(&snapshot, &Policy::default())
            .unwrap()
            .exit_code(Severity::Error),
        2
    );
}

#[test]
fn api_urls_and_path_components_do_not_leak_credentials_or_change_routes() {
    for url in [
        "http://forge.example",
        "https://user:secret@forge.example",
        "https://forge.example?token=secret",
        "https:///bad",
        "https://forge.example\n",
    ] {
        assert!(validate_base(url).is_err());
    }
    assert!(validate_base("https://forge.example/api/v1").is_ok());
    assert_eq!(component("feature/a?b#c"), "feature%2Fa%3Fb%23c");
}

#[test]
fn license_variants_count_but_symlinks_and_empty_documents_do_not() {
    let rows = vec![
        json!({"name":"LICENSE-MIT","type":"file","size":50}),
        json!({"name":"LICENSE","type":"symlink","size":15}),
    ];
    assert!(matches!(
        listed_document(&rows, "", &["LICENSE"]).unwrap(),
        Document::Present { bytes: 50, .. }
    ));
    assert!(matches!(
        listed_document(
            &[json!({"name":"README.md","type":"file","size":0})],
            "",
            &["README"]
        )
        .unwrap(),
        Document::Missing
    ));
}

#[test]
fn snapshot_writer_keeps_private_permissions_and_roundtrips() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("snapshot.json");
    let snapshot = collect(&fixture(Forge::Github), vec![]).unwrap();
    write_snapshot(&path, &snapshot).unwrap();
    let loaded: Snapshot = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(loaded.scope, vec!["example"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    }
}

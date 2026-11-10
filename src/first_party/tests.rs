use super::*;
use serde_json::json;

fn fleet() -> Fleet {
    serde_json::from_value(json!({
        "schema": 1,
        "forge_hosts": [],
        "first_party": ["https://forge.example/org-a/"],
        "aliases": {"https://mirror.example/org-a/": "https://forge.example/org-a/"}
    }))
    .unwrap()
}

fn fleet_no_alias() -> Fleet {
    serde_json::from_value(json!({
        "schema": 1,
        "forge_hosts": [],
        "first_party": ["https://forge.example/org-a/"],
        "aliases": {}
    }))
    .unwrap()
}

fn config() -> Config {
    Config::default()
}

fn lock_pkg(name: &str, source: Option<&str>, version: Option<&str>) -> toml::Value {
    let mut map = toml::map::Map::new();
    map.insert("name".into(), toml::Value::String(name.into()));
    if let Some(source) = source {
        map.insert("source".into(), toml::Value::String(source.into()));
    }
    if let Some(version) = version {
        map.insert("version".into(), toml::Value::String(version.into()));
    }
    toml::Value::Table(map)
}

fn manifest(value: &str) -> toml::Value {
    toml::from_str(value).expect("test manifest parses")
}

fn manifest_decl(git: &str, extra: &str) -> toml::Value {
    manifest(&format!(
        "[target.\"cfg(wasm)\".dependencies.alias]\ngit = {git:?}\n{extra}"
    ))
}

fn rejects_lock(packages: Vec<toml::Value>, fleet: &Fleet, config: &Config) {
    let result = check_lock(&packages, fleet, config);
    assert!(
        result.is_err(),
        "invalid lock accepted: {packages:?} with {config:?}"
    );
}

fn rejects_manifest(value: toml::Value, fleet: &Fleet, config: &Config) {
    let result = check_manifest(&value, fleet, config);
    assert!(
        result.is_err(),
        "invalid manifest accepted: {value:?} with {config:?}"
    );
}

// clbs:.github/check-first-party.py self_test, with a test fleet prefix.

#[test]
fn clbs_base_lock_and_manifest() {
    let fleet = fleet_no_alias();
    let config = config();
    let repo = "https://forge.example/org-a/example";
    let source = format!("git+{repo}?branch=main#{}", "a".repeat(40));
    let package = lock_pkg("example", Some(&source), None);
    check_lock(std::slice::from_ref(&package), &fleet, &config).expect("base lock passes");
    check_lock(&[lock_pkg("local", None, None)], &fleet, &config).expect("local package passes");
    rejects_lock(
        vec![lock_pkg(
            "example",
            Some(&format!(
                "git+{repo}?rev={}#{}",
                "a".repeat(40),
                "a".repeat(40)
            )),
            None,
        )],
        &fleet,
        &config,
    );
    for packages in [
        vec![package.clone(), package.clone()],
        vec![package.clone(), lock_pkg("example", None, None)],
        vec![
            package.clone(),
            lock_pkg("example", Some(&format!("{source}b")), None),
        ],
        vec![lock_pkg(
            "example",
            Some(&source.replace("main", "develop")),
            None,
        )],
        vec![lock_pkg(
            "example",
            Some(&source.replace("branch=main", "tag=v1")),
            None,
        )],
        vec![lock_pkg(
            "example",
            Some(&source.replace("branch=main", &format!("rev={}", "b".repeat(40)))),
            None,
        )],
        vec![lock_pkg(
            "example",
            Some(&source.replace(&format!("#{}", "a".repeat(40)), "#short")),
            None,
        )],
        vec![lock_pkg(
            "example",
            Some(&source.replace("branch=main", "branch=main&branch=main")),
            None,
        )],
    ] {
        rejects_lock(packages, &fleet, &config);
    }
    for extra in [
        "",
        &format!("rev = {:?}", "a".repeat(40)),
        "branch = \"other\"",
        &format!("branch = \"main\"\nrev = {:?}", "a".repeat(40)),
        "branch = \"main\"\ntag = \"v1\"",
    ] {
        rejects_manifest(manifest_decl(repo, extra), &fleet, &config);
    }
    check_manifest(
        &manifest(&format!(
            "[workspace.dependencies.alias]\ngit = {repo:?}\npackage = \"example\"\nbranch = \"main\"\n"
        )),
        &fleet,
        &config,
    )
    .expect("workspace alias with package passes");
}

// cgts check_host_fixtures for the canonical prefix and the alias prefix.

#[test]
fn cgts_host_fixtures_canonical_and_alias() {
    let fleet = fleet();
    let config = config();
    for repo in [
        "https://forge.example/org-a/example",
        "https://mirror.example/org-a/example",
    ] {
        let source = format!("git+{repo}?branch=main#{}", "a".repeat(40));
        let package = lock_pkg("example", Some(&source), None);
        check_lock(std::slice::from_ref(&package), &fleet, &config).expect("host fixture passes");
        check_lock(&[lock_pkg("local", None, None)], &fleet, &config).expect("local passes");
        rejects_lock(
            vec![lock_pkg(
                "example",
                Some(&format!(
                    "git+{repo}?rev={}#{}",
                    "a".repeat(40),
                    "a".repeat(40)
                )),
                None,
            )],
            &fleet,
            &config,
        );
        for packages in [
            vec![package.clone(), package.clone()],
            vec![package.clone(), lock_pkg("example", None, None)],
            vec![
                package.clone(),
                lock_pkg("example", Some(&format!("{source}b")), None),
            ],
            vec![lock_pkg(
                "example",
                Some(&source.replace("main", "develop")),
                None,
            )],
            vec![lock_pkg(
                "example",
                Some(&source.replace("branch=main", "tag=v1")),
                None,
            )],
            vec![lock_pkg(
                "example",
                Some(&source.replace("branch=main", &format!("rev={}", "b".repeat(40)))),
                None,
            )],
            vec![lock_pkg(
                "example",
                Some(&source.replace(&format!("#{}", "a".repeat(40)), "#short")),
                None,
            )],
            vec![lock_pkg(
                "example",
                Some(&source.replace("branch=main", "branch=main&branch=main")),
                None,
            )],
        ] {
            rejects_lock(packages, &fleet, &config);
        }
        for extra in [
            "",
            &format!("rev = {:?}", "a".repeat(40)),
            "branch = \"other\"",
            &format!("branch = \"main\"\nrev = {:?}", "a".repeat(40)),
            "branch = \"main\"\ntag = \"v1\"",
        ] {
            rejects_manifest(manifest_decl(repo, extra), &fleet, &config);
        }
        check_manifest(
            &manifest(&format!(
                "[workspace.dependencies.alias]\ngit = {repo:?}\npackage = \"example\"\nbranch = \"main\"\n"
            )),
            &fleet,
            &config,
        )
        .expect("workspace alias passes");
    }
}

// cgts check_original_issuer_fixtures: versioned retained entries are lock-only.

fn cgts_retained() -> Config {
    Config {
        retained: vec![
            Retained {
                package: "cmbl".into(),
                repository: "https://forge.example/org-a/cmbl.git".into(),
                branch: "v01".into(),
                version: Some("0.1.0".into()),
            },
            Retained {
                package: "cmmr".into(),
                repository: "https://forge.example/org-a/cmmr.git".into(),
                branch: "v01".into(),
                version: Some("0.1.0".into()),
            },
        ],
        ..Config::default()
    }
}

#[test]
fn cgts_original_issuer_fixtures() {
    let fleet = fleet_no_alias();
    let config = cgts_retained();
    let sha = "a".repeat(40);
    for (name, version, repo) in [
        ("cmbl", "0.1.0", "https://forge.example/org-a/cmbl.git"),
        ("cmmr", "0.1.0", "https://forge.example/org-a/cmmr.git"),
    ] {
        let source = format!("git+{repo}?branch=v01#{sha}");
        let package = lock_pkg(name, Some(&source), Some(version));
        check_lock(std::slice::from_ref(&package), &fleet, &config).expect("retained v01 passes");
        check_lock(
            &[lock_pkg(
                name,
                Some(&source.replace("branch=v01", "branch=main")),
                Some(version),
            )],
            &fleet,
            &config,
        )
        .expect("retained also accepted on main");
        for bad in [
            lock_pkg(name, Some(&source), Some("0.2.0")),
            lock_pkg(name, None, None),
            lock_pkg("other", Some(&source), Some(version)),
            lock_pkg(
                if name == "cmbl" { "cmmr" } else { "cmbl" },
                Some(&source),
                Some(version),
            ),
            lock_pkg(name, None, Some(version)),
            lock_pkg(
                name,
                Some(&format!("registry+https://example.test/index#{sha}")),
                Some(version),
            ),
            lock_pkg(
                name,
                Some(&source.replace("forge.example", "mirror.example")),
                Some(version),
            ),
            lock_pkg(name, Some(&source.replace("org-a", "org-b")), Some(version)),
            lock_pkg(
                name,
                Some(&source.replace("forge.example", "forge.example.evil.test")),
                Some(version),
            ),
            lock_pkg(
                name,
                Some(&source.replace("forge.example", "user@forge.example")),
                Some(version),
            ),
            lock_pkg(
                name,
                Some(&source.replace("forge.example", "forge.example:443")),
                Some(version),
            ),
            lock_pkg(name, Some(&source.replace(".git?", "?")), Some(version)),
            lock_pkg(name, Some(&source.replacen("git+", "", 1)), Some(version)),
            lock_pkg(name, Some(&source.replace("v01", "v02")), Some(version)),
            lock_pkg(name, Some(&source.replace("v01", "v%30%31")), Some(version)),
            lock_pkg(
                name,
                Some(&source.replace("branch=v01", "tag=v01")),
                Some(version),
            ),
            lock_pkg(
                name,
                Some(&source.replace("branch=v01", &format!("rev={sha}"))),
                Some(version),
            ),
            lock_pkg(
                name,
                Some(&source.replace("branch=v01", "branch=v01&branch=v01")),
                Some(version),
            ),
            lock_pkg(
                name,
                Some(&source.replace("branch=v01", "branch=v01&extra=")),
                Some(version),
            ),
            lock_pkg(
                name,
                Some(&source.replace(&format!("#{sha}"), "#short")),
                Some(version),
            ),
            lock_pkg(
                name,
                Some(&source.replace(&format!("#{sha}"), &format!("#{}", sha.to_uppercase()))),
                Some(version),
            ),
        ] {
            rejects_lock(vec![bad], &fleet, &config);
        }
        rejects_lock(vec![package.clone(), package.clone()], &fleet, &config);
        rejects_lock(
            vec![package.clone(), lock_pkg(name, None, None)],
            &fleet,
            &config,
        );
        // Versioned retained entries do not move manifests: v01 stays refused.
        rejects_manifest(
            manifest(&format!(
                "[dependencies.{name}]\ngit = {repo:?}\nbranch = \"v01\"\n"
            )),
            &fleet,
            &config,
        );
    }
    let both = vec![
        lock_pkg(
            "cmbl",
            Some(&format!(
                "git+https://forge.example/org-a/cmbl.git?branch=v01#{sha}"
            )),
            Some("0.1.0"),
        ),
        lock_pkg(
            "cmmr",
            Some(&format!(
                "git+https://forge.example/org-a/cmmr.git?branch=v01#{sha}"
            )),
            Some("0.1.0"),
        ),
    ];
    check_lock(&both, &fleet, &config).expect("both retained pass together");
}

// cgbl ORIGINAL loop: unversioned retained entries constrain manifests too.

fn cgbl_retained() -> Config {
    Config {
        retained: vec![
            Retained {
                package: "cchr".into(),
                repository: "https://forge.example/org-a/cchr".into(),
                branch: "v01".into(),
                version: None,
            },
            Retained {
                package: "cmbl".into(),
                repository: "https://forge.example/org-a/cmbl.git".into(),
                branch: "v01".into(),
                version: None,
            },
            Retained {
                package: "cmmr".into(),
                repository: "https://forge.example/org-a/cmmr.git".into(),
                branch: "v01".into(),
                version: None,
            },
            Retained {
                package: "cvlk".into(),
                repository: "https://forge.example/org-a/cvlk.git".into(),
                branch: "v01".into(),
                version: None,
            },
        ],
        ..Config::default()
    }
}

#[test]
fn cgbl_original_loop() {
    let fleet = fleet_no_alias();
    let config = cgbl_retained();
    let repo = "https://forge.example/org-a/example";
    let source = format!("git+{repo}?branch=main#{}", "a".repeat(40));
    let package = lock_pkg("example", Some(&source), None);
    check_lock(std::slice::from_ref(&package), &fleet, &config).expect("base passes");
    check_lock(&[lock_pkg("local", None, None)], &fleet, &config).expect("local passes");
    for packages in [
        vec![package.clone(), package.clone()],
        vec![package.clone(), lock_pkg("example", None, None)],
        vec![
            package.clone(),
            lock_pkg("example", Some(&format!("{source}b")), None),
        ],
        vec![lock_pkg(
            "example",
            Some(&source.replace("main", "develop")),
            None,
        )],
        vec![lock_pkg(
            "example",
            Some(&source.replace("branch=main", "tag=v1")),
            None,
        )],
        vec![lock_pkg(
            "example",
            Some(&source.replace("branch=main", &format!("rev={}", "b".repeat(40)))),
            None,
        )],
        vec![lock_pkg(
            "example",
            Some(&source.replace(&format!("#{}", "a".repeat(40)), "#short")),
            None,
        )],
        vec![lock_pkg(
            "example",
            Some(&source.replace("branch=main", "branch=main&branch=main")),
            None,
        )],
    ] {
        rejects_lock(packages, &fleet, &config);
    }
    for extra in [
        "",
        &format!("rev = {:?}", "a".repeat(40)),
        "branch = \"other\"",
        &format!("branch = \"main\"\nrev = {:?}", "a".repeat(40)),
        "branch = \"main\"\ntag = \"v1\"",
    ] {
        rejects_manifest(manifest_decl(repo, extra), &fleet, &config);
    }
    for (name, original) in [
        ("cchr", "https://forge.example/org-a/cchr"),
        ("cmbl", "https://forge.example/org-a/cmbl.git"),
        ("cmmr", "https://forge.example/org-a/cmmr.git"),
        ("cvlk", "https://forge.example/org-a/cvlk.git"),
    ] {
        let retained = lock_pkg(
            name,
            Some(&format!("git+{original}?branch=v01#{}", "a".repeat(40))),
            None,
        );
        check_lock(std::slice::from_ref(&retained), &fleet, &config).expect("retained v01 passes");
        check_manifest(
            &manifest(&format!(
                "[dependencies.{name}]\ngit = {original:?}\nbranch = \"v01\"\n"
            )),
            &fleet,
            &config,
        )
        .expect("retained manifest v01 passes");
        check_manifest(
            &manifest(&format!(
                "[dependencies.alias]\ngit = {original:?}\npackage = {name:?}\nbranch = \"v01\"\n"
            )),
            &fleet,
            &config,
        )
        .expect("retained alias with package passes");
        rejects_manifest(
            manifest(&format!(
                "[dependencies.alias]\ngit = {original:?}\nbranch = \"v01\"\n"
            )),
            &fleet,
            &config,
        );
        rejects_manifest(
            manifest(&format!(
                "[dependencies.{name}]\ngit = {original:?}\nbranch = \"main\"\n"
            )),
            &fleet,
            &config,
        );
        rejects_lock(
            vec![lock_pkg(
                "foreign",
                retained.get("source").and_then(|v| v.as_str()),
                None,
            )],
            &fleet,
            &config,
        );
        rejects_lock(
            vec![lock_pkg(
                name,
                Some(
                    &retained
                        .get("source")
                        .and_then(|v| v.as_str())
                        .unwrap()
                        .replace("branch=v01", "branch=other"),
                ),
                None,
            )],
            &fleet,
            &config,
        );
        rejects_lock(
            vec![lock_pkg(
                name,
                Some(
                    &retained
                        .get("source")
                        .and_then(|v| v.as_str())
                        .unwrap()
                        .replace(original, &format!("{original}.git")),
                ),
                None,
            )],
            &fleet,
            &config,
        );
        let second = retained.clone();
        if let Some(source) = second.get("source").and_then(|v| v.as_str()).map(|s| {
            let mut s = s.to_owned();
            s.pop();
            s.push('b');
            s
        }) {
            rejects_lock(
                vec![retained.clone(), lock_pkg(name, Some(&source), None)],
                &fleet,
                &config,
            );
        }
        rejects_lock(
            vec![lock_pkg(
                name,
                Some(
                    &retained
                        .get("source")
                        .and_then(|v| v.as_str())
                        .unwrap()
                        .replace("branch=v01", "branch=v01&branch=v01"),
                ),
                None,
            )],
            &fleet,
            &config,
        );
    }
}

#[test]
fn alias_accepted_by_default_and_refused_with_canonical_only() {
    let fleet = fleet();
    let source = format!(
        "git+https://mirror.example/org-a/example?branch=main#{}",
        "a".repeat(40)
    );
    let package = lock_pkg("example", Some(&source), None);
    check_lock(std::slice::from_ref(&package), &fleet, &config()).expect("alias passes by default");
    check_manifest(
        &manifest(
            "[dependencies.example]\ngit = \"https://mirror.example/org-a/example\"\nbranch = \"main\"\n",
        ),
        &fleet,
        &config(),
    )
    .expect("alias manifest passes by default");
    let strict = Config {
        canonical_only: true,
        ..Config::default()
    };
    let lock_err = check_lock(std::slice::from_ref(&package), &fleet, &strict).unwrap_err();
    assert_eq!(
        lock_err,
        format!("First-party source must use its canonical owner URL: {source}")
    );
    let manifest_err = check_manifest(
        &manifest(
            "[dependencies.example]\ngit = \"https://mirror.example/org-a/example\"\nbranch = \"main\"\n",
        ),
        &fleet,
        &strict,
    )
    .unwrap_err();
    assert_eq!(
        manifest_err,
        "First-party source must use its canonical owner URL: https://mirror.example/org-a/example"
    );
}

#[test]
fn rev_selectors_accept_rev_queries() {
    let fleet = fleet_no_alias();
    let repo = "https://forge.example/org-a/example";
    let sha = "a".repeat(40);
    let source = format!("git+{repo}?rev={sha}#{sha}");
    let package = lock_pkg("example", Some(&source), None);
    rejects_lock(vec![package.clone()], &fleet, &config());
    let rev = Config {
        rev_selectors: true,
        ..Config::default()
    };
    check_lock(std::slice::from_ref(&package), &fleet, &rev).expect("rev selector passes");
    rejects_lock(
        vec![lock_pkg(
            "example",
            Some(&format!("git+{repo}?rev={sha}#short")),
            None,
        )],
        &fleet,
        &rev,
    );
    rejects_lock(
        vec![lock_pkg(
            "example",
            Some(&format!("git+{repo}?rev={}#{}", "b".repeat(40), sha)),
            None,
        )],
        &fleet,
        &rev,
    );
}

#[test]
fn path_and_git_packages_with_the_same_name_are_duplicates() {
    let fleet = fleet_no_alias();
    let config = config();
    let repo = "https://forge.example/org-a/example";
    let source = format!("git+{repo}?branch=main#{}", "a".repeat(40));
    rejects_lock(
        vec![
            lock_pkg("example", Some(&source), Some("0.1.0")),
            lock_pkg("example", None, Some("0.1.0")),
        ],
        &fleet,
        &config,
    );
}

#[test]
fn missing_fragment_and_bad_query_have_distinct_messages() {
    let fleet = fleet_no_alias();
    let config = config();
    let repo = "https://forge.example/org-a/example";
    let err = check_lock(
        &[lock_pkg(
            "example",
            Some(&format!("git+{repo}?branch=main")),
            None,
        )],
        &fleet,
        &config,
    )
    .unwrap_err();
    assert_eq!(err, "Missing resolved revision for example");
    let err = check_lock(
        &[lock_pkg(
            "example",
            Some(&format!("git+{repo}?branch=develop#{}", "a".repeat(40))),
            None,
        )],
        &fleet,
        &config,
    )
    .unwrap_err();
    assert_eq!(err, "Invalid first-party source selector for example");
}

// ---------------------------------------------------------------------------
// The resolved graph (`--metadata`): `check-pins.test.mjs` of the full variant,
// case by case. Hosts: `forge.example/org-a` is the canonical owner,
// `mirror.example/org-a` its alias, `forge.example/org-b` an unrelated owner.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};

const REVISION: &str = "6b94dacd7fa04aa8847c62c6471a1fc5c0f6c9dc";
const OLDER: &str = "9c076b1e4050529406df27533e12c8e0dd1fc0db";
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

fn graph_source(name: &str, revision: &str) -> String {
    format!("git+https://mirror.example/org-a/{name}?branch=main#{revision}")
}

fn main_source(name: &str) -> String {
    graph_source(name, REVISION)
}

fn without_fragment(source: &str) -> String {
    source.split('#').next().unwrap().to_owned()
}

/// A declaration's package name when the test gives only its source: the last path
/// segment without `.git`.
fn declared_name(source: &str) -> String {
    let url = source.trim_start_matches("git+");
    let path = url.split(['?', '#']).next().unwrap();
    path.rsplit('/')
        .next()
        .unwrap()
        .trim_end_matches(".git")
        .to_owned()
}

/// A cargo-metadata package; each declaration is an override on top of
/// `{name, kind: null, target: null}`.
fn gpkg(name: &str, source: Option<&str>, declarations: &[Value]) -> Value {
    let id = format!("{name}:{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let dependencies: Vec<Value> = declarations
        .iter()
        .map(|declaration| {
            let mut base = serde_json::Map::new();
            let derived = declaration
                .get("source")
                .and_then(Value::as_str)
                .map(declared_name);
            if let Some(derived) = derived {
                base.insert("name".into(), json!(derived));
            }
            base.insert("kind".into(), Value::Null);
            base.insert("target".into(), Value::Null);
            for (key, value) in declaration.as_object().unwrap() {
                base.insert(key.clone(), value.clone());
            }
            Value::Object(base)
        })
        .collect();
    json!({
        "id": id,
        "name": name,
        "source": source,
        "targets": [{"kind": ["lib"], "name": name.replace('-', "_")}],
        "dependencies": dependencies,
    })
}

/// The `cargo metadata` document of `packages`; the first one is the workspace
/// member and root, and every declaration is a selected edge to the first package of
/// that name (`missing` when there is none).
fn gmetadata(packages: Vec<Value>) -> Value {
    let nodes: Vec<Value> = packages
        .iter()
        .map(|owner| {
            let deps: Vec<Value> = owner["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .map(|dependency| {
                    let target = packages
                        .iter()
                        .find(|candidate| candidate["name"] == dependency["name"]);
                    let code = dependency
                        .get("rename")
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| dependency["name"].as_str().unwrap())
                        .replace('-', "_");
                    json!({
                        "name": code,
                        "pkg": target.map_or("missing", |target| target["id"].as_str().unwrap()),
                        "dep_kinds": [{"kind": dependency["kind"], "target": dependency["target"]}],
                    })
                })
                .collect();
            let mut ids: Vec<Value> = Vec::new();
            for dep in &deps {
                if !ids.contains(&dep["pkg"]) {
                    ids.push(dep["pkg"].clone());
                }
            }
            json!({"id": owner["id"], "deps": deps, "dependencies": ids, "features": []})
        })
        .collect();
    json!({
        "version": 1,
        "packages": packages,
        "workspace_members": packages.first().map_or(json!([]), |first| json!([first["id"]])),
        "resolve": {
            "root": packages.first().map_or(Value::Null, |first| first["id"].clone()),
            "nodes": nodes,
        },
    })
}

/// The graph every case starts from: the workspace member, the required package
/// and the case's packages.
fn ggraph(packages: Vec<Value>) -> Value {
    let mut all = vec![
        gpkg("crbk", None, &[]),
        gpkg("crlt", Some(&main_source("crlt")), &[]),
    ];
    all.extend(packages);
    gmetadata(all)
}

fn graph_config() -> Config {
    Config {
        graph: Graph {
            required: vec!["crlt".into()],
        },
        ..cgts_retained()
    }
}

fn graph_check(value: &Value) -> std::result::Result<(), String> {
    check_metadata(value, &fleet(), &graph_config())
}

fn graph_refusal(value: &Value) -> String {
    graph_check(value).expect_err("the graph must be refused")
}

fn issuer_package(name: &str, source: &str) -> Value {
    let mut package = gpkg(name, Some(source), &[]);
    package["version"] = json!("0.1.0");
    package
}

fn issuer_source(name: &str) -> String {
    format!("git+https://forge.example/org-a/{name}.git?branch=v01#{REVISION}")
}

#[test]
fn graph_accepts_a_single_revision_per_leaf_and_unrelated_registry_duplicates() {
    graph_check(&ggraph(vec![
        gpkg("serde", None, &[]),
        gpkg("thiserror", None, &[]),
        gpkg("thiserror", None, &[]),
    ]))
    .unwrap();
}

#[test]
fn graph_rejects_duplicate_direct_and_transitive_first_party_packages() {
    for value in [
        ggraph(vec![gpkg("crlt", Some(&graph_source("crlt", OLDER)), &[])]),
        ggraph(vec![
            gpkg("csgn", Some(&main_source("csgn")), &[]),
            gpkg("csgn", Some(&graph_source("csgn", OLDER)), &[]),
        ]),
        ggraph(vec![gpkg("crlt", None, &[])]),
    ] {
        let message = graph_refusal(&value);
        assert!(message.contains("duplicated"), "{message}");
    }
}

#[test]
fn graph_rejects_other_branches_tags_unqualified_and_abbreviated_revisions() {
    let queries = [
        "branch=develop".to_owned(),
        "tag=v1".to_owned(),
        String::new(),
        format!("rev={}", &REVISION[..7]),
    ];
    for query in queries {
        let floating = format!("git+https://mirror.example/org-a/csgn?{query}#{REVISION}");
        let message = graph_refusal(&ggraph(vec![gpkg("csgn", Some(&floating), &[])]));
        assert_eq!(
            message,
            format!("Expected main and a complete resolved revision: {floating}")
        );
    }
}

#[test]
fn graph_rejects_every_matching_transitive_revision_pin() {
    let pinned = format!("git+https://mirror.example/org-a/csgn?rev={REVISION}#{REVISION}");
    let message = graph_refusal(&ggraph(vec![gpkg("csgn", Some(&pinned), &[])]));
    assert!(message.starts_with("Expected main"), "{message}");
    let owner = gpkg(
        "csgn",
        Some(&main_source("csgn")),
        &[json!({"source": without_fragment(&pinned)})],
    );
    let message = graph_refusal(&ggraph(vec![owner]));
    assert!(
        message.starts_with("Expected main") || message.starts_with("Mismatched"),
        "{message}"
    );
}

#[test]
fn graph_accepts_main_declarations_with_a_unique_complete_resolution() {
    let dependency = json!({"source": "git+https://mirror.example/org-a/cpns?branch=main"});
    graph_check(&ggraph(vec![
        gpkg("cgrd", Some(&main_source("cgrd")), &[dependency]),
        gpkg(
            "cpns",
            Some(&format!(
                "git+https://mirror.example/org-a/cpns?branch=main#{REVISION}"
            )),
            &[],
        ),
    ]))
    .unwrap();
}

#[test]
fn graph_requires_the_required_package() {
    let message = graph_refusal(&gmetadata(vec![gpkg("crbk", None, &[])]));
    assert_eq!(
        message,
        "Missing, invalid or duplicated first-party dependency: crlt"
    );
    let message = graph_refusal(&gmetadata(vec![
        gpkg("crbk", None, &[]),
        gpkg("crlt", None, &[]),
    ]));
    assert!(message.contains("invalid"), "{message}");
}

#[test]
fn graph_canonical_sources_reject_pins_and_mixed_origin_duplicates() {
    let canonical = main_source("crlt").replace("mirror.example", "forge.example");
    graph_check(&gmetadata(vec![gpkg("crlt", Some(&canonical), &[])])).unwrap();
    let message = graph_refusal(&ggraph(vec![gpkg("crlt", Some(&canonical), &[])]));
    assert!(message.contains("duplicated"), "{message}");
    let pinned = canonical.replace("branch=main", &format!("rev={REVISION}"));
    let message = graph_refusal(&gmetadata(vec![gpkg("crlt", Some(&pinned), &[])]));
    assert!(message.starts_with("Expected main"), "{message}");
}

#[test]
fn graph_accepts_exact_original_issuer_resolutions_and_transitive_declarations() {
    let issuers: Vec<Value> = ["cmbl", "cmmr"]
        .iter()
        .map(|name| issuer_package(name, &issuer_source(name)))
        .collect();
    let dependencies: Vec<Value> = issuers
        .iter()
        .map(|owner| {
            json!({
                "name": owner["name"],
                "source": without_fragment(owner["source"].as_str().unwrap()),
            })
        })
        .collect();
    let mut packages = vec![gpkg("cblc", Some(&main_source("cblc")), &dependencies)];
    packages.extend(issuers);
    graph_check(&ggraph(packages)).unwrap();
}

#[test]
fn graph_rejects_original_owner_substitutions_aliases_versions_and_duplicate_identities() {
    for name in ["cmbl", "cmmr"] {
        let original = issuer_source(name);
        let owner = issuer_package(name, &original);
        let other = if name == "cmbl" { "cmmr" } else { "cmbl" };
        let mut bad: Vec<Value> = Vec::new();
        let mut renamed = owner.clone();
        renamed["name"] = json!("other");
        bad.push(renamed);
        let mut swapped = owner.clone();
        swapped["name"] = json!(other);
        bad.push(swapped);
        let mut newer = owner.clone();
        newer["version"] = json!("0.2.0");
        bad.push(newer);
        let mut unversioned = owner.clone();
        unversioned.as_object_mut().unwrap().remove("version");
        bad.push(unversioned);
        let mut sourceless = owner.clone();
        sourceless["source"] = Value::Null;
        bad.push(sourceless);
        for source in [
            "registry+https://example.test/index".to_owned(),
            original.replace("forge.example", "mirror.example"),
            original.replace("org-a", "org-b"),
            original.replace("forge.example", "forge.example.evil.test"),
            original.replace("forge.example", "user@forge.example"),
            original.replace("forge.example", "forge.example:443"),
            original.replace(".git?", "?"),
            original.replace("git+", ""),
            original.replace("v01", "v02"),
            original.replace("v01", "v%30%31"),
            original.replace("branch=v01", "tag=v01"),
            original.replace("branch=v01", &format!("rev={REVISION}")),
            original.replace("branch=v01", "branch=v01&branch=v01"),
            original.replace("branch=v01", "branch=v01&extra="),
            original.replace(&format!("#{REVISION}"), "#short"),
            original.replace(
                &format!("#{REVISION}"),
                &format!("#{}", REVISION.to_uppercase()),
            ),
        ] {
            let mut substituted = owner.clone();
            substituted["source"] = json!(source);
            bad.push(substituted);
        }
        for package in bad {
            let value = ggraph(vec![package.clone()]);
            assert!(graph_check(&value).is_err(), "accepted: {package}");
        }
        let message = graph_refusal(&ggraph(vec![owner.clone(), owner.clone()]));
        assert!(message.contains("duplicated"), "{message}");
        let message = graph_refusal(&ggraph(vec![owner.clone(), gpkg(name, None, &[])]));
        assert!(message.contains("duplicated"), "{message}");
        // The retained branch is an exception, not a requirement: main is accepted too.
        let mut on_main = owner.clone();
        on_main["source"] = json!(original.replace("branch=v01", "branch=main"));
        graph_check(&ggraph(vec![on_main])).unwrap();
    }
}

#[test]
fn graph_rejects_altered_and_unresolved_original_transitive_selectors() {
    let name = "cmbl";
    let declaration = "git+https://forge.example/org-a/cmbl.git?branch=v01".to_owned();
    let owner = issuer_package(name, &format!("{declaration}#{REVISION}"));
    let altered = [
        declaration.replace("v01", "v02"),
        declaration.replace("v01", "v%30%31"),
        format!("{declaration}&branch=v01"),
        format!("{declaration}#{REVISION}"),
        declaration.replace(".git?", "?"),
        declaration.replace("forge.example", "mirror.example"),
        "registry+https://example.test/index".to_owned(),
    ];
    let caller = |source: Value| {
        gpkg(
            "cblc",
            Some(&main_source("cblc")),
            &[json!({"name": name, "source": source})],
        )
    };
    for raw in altered.iter().map(|raw| json!(raw)).chain([Value::Null]) {
        let value = ggraph(vec![owner.clone(), caller(raw.clone())]);
        assert!(graph_check(&value).is_err(), "accepted: {raw}");
    }
    // The exact declaration passes while the retained package is there; without it the
    // edge has nothing to resolve to, and an edge to another name is not this edge.
    graph_check(&ggraph(vec![owner.clone(), caller(json!(declaration))])).unwrap();
    assert!(graph_check(&ggraph(vec![caller(json!(declaration))])).is_err());
    let stray = gpkg(
        "cblc",
        Some(&main_source("cblc")),
        &[json!({"name": "other", "source": declaration})],
    );
    assert!(graph_check(&ggraph(vec![owner, stray])).is_err());
}

#[test]
fn graph_ignores_inactive_upstream_declarations_but_not_selected_edges() {
    let to_v01 =
        |name: &str| without_fragment(&main_source(name)).replace("branch=main", "branch=v01");
    let inactive = gpkg(
        "cgbl",
        Some(&main_source("cgbl")),
        &[
            json!({"name": "cchr", "source": to_v01("cchr"), "kind": "dev"}),
            json!({"name": "cvlk", "source": to_v01("cvlk"), "optional": true}),
        ],
    );
    let id = inactive["id"].clone();
    let mut unused = ggraph(vec![inactive.clone()]);
    for node in unused["resolve"]["nodes"].as_array_mut().unwrap() {
        if node["id"] == id {
            node["deps"] = json!([]);
            node["dependencies"] = json!([]);
        }
    }
    graph_check(&unused).unwrap();
    let activated = gpkg(
        "cchr",
        Some(&main_source("cchr").replace("branch=main", "branch=v01")),
        &[],
    );
    let mut active_owner = inactive;
    let first = active_owner["dependencies"][0].clone();
    active_owner["dependencies"] = json!([first]);
    let message = graph_refusal(&ggraph(vec![active_owner, activated]));
    assert!(message.starts_with("Expected main"), "{message}");
}

#[test]
fn graph_retains_active_workspace_dev_and_build_selectors_including_renamed_targets() {
    for kind in [json!("dev"), json!("build"), Value::Null] {
        let dependency = json!({
            "name": "cpns", "rename": "renamed-owner", "kind": kind, "target": "cfg(unix)",
            "source": without_fragment(&main_source("cpns")),
        });
        let owner = gpkg("workspace", None, std::slice::from_ref(&dependency));
        graph_check(&gmetadata(vec![
            owner,
            gpkg("crlt", Some(&main_source("crlt")), &[]),
            gpkg("cpns", Some(&main_source("cpns")), &[]),
        ]))
        .unwrap();
        let mut bad = dependency.clone();
        bad["source"] =
            json!(without_fragment(&main_source("cpns")).replace("branch=main", "branch=v01"));
        let message = graph_refusal(&gmetadata(vec![
            gpkg("workspace", None, &[bad]),
            gpkg("crlt", Some(&main_source("crlt")), &[]),
            gpkg(
                "cpns",
                Some(&main_source("cpns").replace("branch=main", "branch=v01")),
                &[],
            ),
        ]));
        assert!(message.starts_with("Expected main"), "{message}");
    }
}

#[test]
fn graph_matches_a_library_name_distinct_from_its_package_name() {
    let mut library = gpkg("crlt", Some(&main_source("crlt")), &[]);
    library["targets"][0]["name"] = json!("transaction_owner");
    let mut value = gmetadata(vec![
        gpkg(
            "workspace",
            None,
            &[json!({"name": "crlt", "source": without_fragment(&main_source("crlt"))})],
        ),
        library,
    ]);
    value["resolve"]["nodes"][0]["deps"][0]["name"] = json!("transaction_owner");
    graph_check(&value).unwrap();
}

#[test]
fn graph_refuses_absent_incomplete_contradictory_and_unknown_resolved_data() {
    type Mutation = fn(&mut Value);
    let structural: [(&str, Mutation); 9] = [
        ("Missing resolved dependency graph", |value| {
            value.as_object_mut().unwrap().remove("resolve");
        }),
        ("Incomplete resolved package graph", |value| {
            value["resolve"]["nodes"].as_array_mut().unwrap().pop();
        }),
        ("Unknown or duplicated resolved package ID", |value| {
            value["resolve"]["nodes"][0]["id"] = json!("unknown");
        }),
        ("Unknown resolved root package", |value| {
            value["resolve"]["root"] = json!("unknown");
        }),
        ("Unknown or missing resolved workspace members", |value| {
            value["workspace_members"]
                .as_array_mut()
                .unwrap()
                .push(json!("unknown"));
        }),
        ("Missing or duplicated package ID", |value| {
            let first = value["packages"][0]["id"].clone();
            value["packages"][1]["id"] = first;
        }),
        ("Missing resolved dependency edges", |value| {
            value["resolve"]["nodes"][0]
                .as_object_mut()
                .unwrap()
                .remove("deps");
        }),
        ("Inconsistent resolved dependency edges", |value| {
            value["resolve"]["nodes"][0]["dependencies"]
                .as_array_mut()
                .unwrap()
                .push(json!("unknown"));
        }),
        ("Unknown or incomplete resolved dependency edge", |value| {
            value["resolve"]["nodes"][0]["dependencies"] = json!(["unknown"]);
            value["resolve"]["nodes"][0]["deps"] = json!([
                {"pkg": "unknown", "name": "x", "dep_kinds": [{"kind": null, "target": null}]}
            ]);
        }),
    ];
    for (expected, mutate) in structural {
        let mut value = ggraph(vec![]);
        mutate(&mut value);
        assert_eq!(graph_refusal(&value), expected);
    }
    let edge_mutations: [(&str, Mutation); 7] = [
        // An edge the node's `dependencies` list does not name.
        ("Inconsistent resolved dependency edges", |value| {
            value["resolve"]["nodes"][2]["dependencies"] = json!([]);
        }),
        ("Unknown or incomplete resolved dependency edge", |value| {
            value["resolve"]["nodes"][2]["deps"][0]["dep_kinds"] = json!([]);
        }),
        ("Invalid resolved dependency kind", |value| {
            value["resolve"]["nodes"][2]["deps"][0]["dep_kinds"][0]["kind"] = json!("other");
        }),
        ("Invalid resolved dependency kind", |value| {
            value["resolve"]["nodes"][2]["deps"][0]["dep_kinds"][0]["target"] = json!(7);
        }),
        ("Missing selected dependency declaration", |value| {
            value["resolve"]["nodes"][2]["deps"][0]["name"] = json!("other");
        }),
        ("Missing selected dependency declaration", |value| {
            value["resolve"]["nodes"][2]["deps"][0]["dep_kinds"][0]["kind"] = json!("dev");
        }),
        ("Mismatched selected dependency source", |value| {
            value["packages"][2]["dependencies"][0]["source"] =
                json!(without_fragment(&main_source("cpns")));
        }),
    ];
    for (expected, mutate) in edge_mutations {
        let mut value = ggraph(vec![gpkg(
            "caller",
            None,
            &[json!({"name": "crlt", "source": without_fragment(&main_source("crlt"))})],
        )]);
        mutate(&mut value);
        assert_eq!(graph_refusal(&value), expected);
    }
}

#[test]
fn graph_retained_issuer_outside_the_fleet_keeps_its_exact_declaration() {
    // The core lock rule allows a retained repository outside the fleet; the selected-source
    // mismatch rule only polices first-party sources, so the retained declaration is the
    // guard for such an entry.
    let repository = "https://legacy.example/org-x/legacy.git";
    let config = Config {
        retained: vec![Retained {
            package: "legacy".into(),
            repository: repository.into(),
            branch: "v01".into(),
            version: None,
        }],
        ..Config::default()
    };
    let declaration = format!("git+{repository}?branch=v01");
    let resolved = format!("{declaration}#{REVISION}");
    let caller = |source: &str| {
        gpkg(
            "caller",
            None,
            &[json!({"name": "legacy", "source": source})],
        )
    };
    let accepted = gmetadata(vec![
        caller(&declaration),
        gpkg("legacy", Some(&resolved), &[]),
    ]);
    check_metadata(&accepted, &fleet(), &config).unwrap();
    let other = format!("git+{repository}?branch=other");
    let refused = gmetadata(vec![caller(&other), gpkg("legacy", Some(&resolved), &[])]);
    assert_eq!(
        check_metadata(&refused, &fleet(), &config).unwrap_err(),
        "Invalid first-party source selector for legacy"
    );
    // The package itself must be the exact retained source.
    let moved = gmetadata(vec![
        gpkg("caller", None, &[]),
        gpkg(
            "legacy",
            Some(&format!("git+{repository}?branch=other#{REVISION}")),
            &[],
        ),
    ]);
    assert_eq!(
        check_metadata(&moved, &fleet(), &config).unwrap_err(),
        "Invalid first-party source selector for legacy"
    );
}

// cvln check_metadata: the graph must be present before anything is judged.

#[test]
fn graph_presence_checks_refuse_an_empty_or_foreign_document() {
    let message = "Missing original Cargo metadata graph";
    assert_eq!(graph_refusal(&json!({})), message);
    let mut wrong_version = ggraph(vec![]);
    wrong_version["version"] = json!(2);
    assert_eq!(graph_refusal(&wrong_version), message);
    let mut no_version = ggraph(vec![]);
    no_version.as_object_mut().unwrap().remove("version");
    assert_eq!(graph_refusal(&no_version), message);
    let mut empty = ggraph(vec![]);
    empty["packages"] = json!([]);
    assert_eq!(graph_refusal(&empty), message);
    let mut no_members = ggraph(vec![]);
    no_members["workspace_members"] = json!([]);
    assert_eq!(
        graph_refusal(&no_members),
        "Missing actual workspace package"
    );
    // An absent root is not a virtual workspace: `null` is the explicit form.
    let mut no_root = ggraph(vec![]);
    no_root["resolve"].as_object_mut().unwrap().remove("root");
    assert_eq!(graph_refusal(&no_root), "Unknown resolved root package");
    let mut virtual_workspace = ggraph(vec![]);
    virtual_workspace["resolve"]["root"] = Value::Null;
    graph_check(&virtual_workspace).unwrap();
}

#[test]
fn graph_keeps_the_required_set_empty_by_default() {
    let value = gmetadata(vec![gpkg("crbk", None, &[])]);
    check_metadata(&value, &fleet(), &config()).expect("no required package, nothing first-party");
}

// cpsd/cgbl compact variants: a revision pin is an accepted selector only on request.

#[test]
fn graph_rev_selectors_accept_a_matching_revision_pin() {
    let config = Config {
        rev_selectors: true,
        ..graph_config()
    };
    let pinned_declaration = format!("git+https://mirror.example/org-a/csgn?rev={REVISION}");
    let pinned_package = format!("{pinned_declaration}#{REVISION}");
    let owner = gpkg(
        "cblc",
        Some(&main_source("cblc")),
        &[json!({"source": pinned_declaration})],
    );
    let value = ggraph(vec![owner, gpkg("csgn", Some(&pinned_package), &[])]);
    check_metadata(&value, &fleet(), &config).unwrap();
    assert!(graph_check(&value).is_err(), "off by default");
    for (source, text) in [
        (
            format!("git+https://mirror.example/org-a/csgn?rev={REVISION}#{OLDER}"),
            "resolved revision different from its pin",
        ),
        (
            format!(
                "git+https://mirror.example/org-a/csgn?rev={}#{REVISION}",
                &REVISION[..7]
            ),
            "abbreviated pin",
        ),
        (
            format!("git+https://mirror.example/org-a/csgn?rev={REVISION}"),
            "pin without a resolved revision",
        ),
    ] {
        let value = ggraph(vec![gpkg("csgn", Some(&source), &[])]);
        let message = check_metadata(&value, &fleet(), &config).expect_err(text);
        assert_eq!(
            message,
            format!("Expected main or a matching transitive revision pin: {source}")
        );
    }
}

#[test]
fn graph_canonical_only_refuses_alias_sources_and_declarations() {
    let config = Config {
        canonical_only: true,
        ..graph_config()
    };
    let canonical =
        |name: &str| format!("git+https://forge.example/org-a/{name}?branch=main#{REVISION}");
    let ok = gmetadata(vec![
        gpkg("crbk", None, &[]),
        gpkg("crlt", Some(&canonical("crlt")), &[]),
    ]);
    check_metadata(&ok, &fleet(), &config).unwrap();
    let alias_package = ggraph(vec![gpkg("csgn", Some(&main_source("csgn")), &[])]);
    // `ggraph`'s own required package is on the alias, which is refused first.
    let message = check_metadata(&alias_package, &fleet(), &config).unwrap_err();
    assert_eq!(
        message,
        format!(
            "First-party source must use its canonical owner URL: {}",
            main_source("crlt")
        )
    );
    // An alias declaration that resolves to an alias package is consistent, so the
    // canonical rule is the one that refuses it (the owner is judged before the package).
    let alias_declaration =
        json!({"name": "csgn", "source": without_fragment(&main_source("csgn"))});
    let value = gmetadata(vec![
        gpkg("crbk", None, &[alias_declaration]),
        gpkg("crlt", Some(&canonical("crlt")), &[]),
        gpkg("csgn", Some(&main_source("csgn")), &[]),
    ]);
    let message = check_metadata(&value, &fleet(), &config).unwrap_err();
    assert_eq!(
        message,
        format!(
            "First-party source must use its canonical owner URL: {}",
            without_fragment(&main_source("csgn"))
        )
    );
}

#[test]
fn graph_only_git_sources_are_first_party() {
    // The resolved graph spells git sources `git+<url>`; a bare URL or a registry
    // source on the owner's host is not first-party and is not policed.
    let bare = "https://forge.example/org-a/csgn?branch=develop#abc";
    graph_check(&ggraph(vec![gpkg("csgn", Some(bare), &[])])).unwrap();
}

#[test]
fn graph_required_names_are_validated() {
    let empty = Config {
        graph: Graph {
            required: vec![String::new()],
        },
        ..Config::default()
    };
    assert!(empty.validate().is_err());
    let duplicate = Config {
        graph: Graph {
            required: vec!["crlt".into(), "crlt".into()],
        },
        ..Config::default()
    };
    assert!(duplicate.validate().is_err());
    graph_config().validate().unwrap();
}

#[test]
fn graph_first_refusal_is_in_node_order_then_required_names() {
    // The package check precedes the uniqueness check, as the script's two loops do.
    let value = ggraph(vec![
        gpkg("csgn", Some(&main_source("csgn")), &[]),
        gpkg("csgn", Some(&graph_source("csgn", OLDER)), &[]),
        gpkg(
            "cpns",
            Some(&main_source("cpns").replace("branch=main", "branch=develop")),
            &[],
        ),
    ]);
    let message = graph_refusal(&value);
    assert!(message.starts_with("Expected main"), "{message}");
}

// cnvl `test-lock.py`: the four cases, against the core rules that replace it.
// The script matched owners by substring; the fleet matches by prefix, and the
// alias host stands in for the substring match.

#[test]
fn cnvl_test_lock_cases_hold_for_the_core_rules() {
    let fleet = fleet();
    let config = config();
    let url = "https://mirror.example/org-a/ckmg";
    let sha = "a".repeat(40);
    let lock = |source: &str, copies: usize| -> Vec<toml::Value> {
        (0..copies)
            .map(|_| lock_pkg("ckmg", Some(source), Some("0.0.0")))
            .collect()
    };
    let manifest_for = |selector: &str| {
        manifest(&format!(
            "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n[dependencies]\nckmg = {{ git = \"{url}\", {selector} }}\n"
        ))
    };
    // test_one_main_revision
    check_manifest(&manifest_for("branch = \"main\""), &fleet, &config).unwrap();
    check_lock(
        &lock(&format!("git+{url}?branch=main#{sha}"), 1),
        &fleet,
        &config,
    )
    .unwrap();
    // test_direct_pin_or_other_branch
    for selector in [
        format!("rev = \"{sha}\""),
        "tag = \"v1\"".to_owned(),
        "branch = \"preview\"".to_owned(),
        format!("branch = \"main\", rev = \"{sha}\""),
    ] {
        rejects_manifest(manifest_for(&selector), &fleet, &config);
    }
    // test_transitive_revision_tag_and_other_branch
    for selector in [
        format!("rev={sha}"),
        "tag=v1".to_owned(),
        "branch=preview".to_owned(),
        format!("branch=main&rev={sha}"),
    ] {
        rejects_lock(
            lock(&format!("git+{url}?{selector}#{sha}"), 1),
            &fleet,
            &config,
        );
    }
    // test_duplicate_first_party_package
    rejects_lock(
        lock(&format!("git+{url}?branch=main#{sha}"), 2),
        &fleet,
        &config,
    );
}

// cvlt `test-lock.py`: the cases the core rules cover. Not ported, because no current
// lock needs them (see docs/first-party.md): an original owner refusing `main`, the
// package-name-equals-repository-name rule and its sibling table, the `.git` suffix
// rule, the canonical-host rule and the mixed-revision rule of one repository.

#[test]
fn cvlt_test_lock_cases_the_core_rules_cover() {
    let fleet = fleet_no_alias();
    let repo = "https://forge.example/org-a/ckmg.git";
    let sha = "a".repeat(40);
    let config = Config {
        retained: vec![Retained {
            package: "ckmg".into(),
            repository: repo.into(),
            branch: "v01".into(),
            version: None,
        }],
        ..Config::default()
    };
    let lock = |source: &str, copies: usize| -> Vec<toml::Value> {
        (0..copies)
            .map(|_| lock_pkg("ckmg", Some(source), Some("0.0.0")))
            .collect()
    };
    let declaration = |selector: &str| {
        manifest(&format!(
            "[dependencies]\nckmg = {{ git = \"{repo}\", {selector} }}\n"
        ))
    };
    // test_exact_original_revision
    check_manifest(&declaration("branch = \"v01\""), &fleet, &config).unwrap();
    check_lock(
        &lock(&format!("git+{repo}?branch=v01#{sha}"), 1),
        &fleet,
        &config,
    )
    .unwrap();
    // test_exact_shared_revision
    let shared = "https://forge.example/org-a/csgn";
    check_lock(
        &[lock_pkg(
            "csgn",
            Some(&format!("git+{shared}?branch=main#{sha}")),
            Some("0.0.0"),
        )],
        &fleet,
        &config,
    )
    .unwrap();
    // test_direct_pin_or_other_branch
    for selector in [
        format!("rev = \"{sha}\""),
        "tag = \"v1\"".to_owned(),
        "branch = \"preview\"".to_owned(),
        "branch = \"main\"".to_owned(),
        format!("branch = \"v01\", rev = \"{sha}\""),
        "branch = \"v01\", tag = \"v1\"".to_owned(),
    ] {
        rejects_manifest(declaration(&selector), &fleet, &config);
    }
    // test_transitive_revision_tag_and_other_branch
    for selector in [
        format!("rev={sha}"),
        "tag=v1".to_owned(),
        "branch=preview".to_owned(),
        format!("branch=v01&rev={sha}"),
        "branch=v01&tag=v1".to_owned(),
        "branch=v01&branch=v01".to_owned(),
        "branch=v01&other=value".to_owned(),
    ] {
        rejects_lock(
            lock(&format!("git+{repo}?{selector}#{sha}"), 1),
            &fleet,
            &config,
        );
    }
    // test_duplicate_first_party_package, including a registry or source-less twin
    let source = format!("git+{repo}?branch=v01#{sha}");
    rejects_lock(lock(&source, 2), &fleet, &config);
    for twin in [
        lock_pkg("ckmg", None, Some("1.0.0")),
        lock_pkg(
            "ckmg",
            Some("registry+https://github.com/rust-lang/crates.io-index"),
            Some("1.0.0"),
        ),
    ] {
        let mut packages = lock(&source, 1);
        packages.push(twin);
        rejects_lock(packages, &fleet, &config);
    }
    // test_wrong_owner_url_or_package: a foreign host, a plain-http URL and the same
    // repository without its `.git` suffix are not the retained exact source.
    for url in [
        repo.trim_end_matches(".git").to_owned(),
        repo.replace("https:", "http:"),
        repo.replace("forge.example", "foreign.example"),
        "https://foreign.example/other/ckmg".to_owned(),
    ] {
        rejects_lock(
            lock(&format!("git+{url}?branch=v01#{sha}"), 1),
            &fleet,
            &config,
        );
    }
    // test_short_resolved_revision and test_shared_cannot_select_original_branch_or_suffix
    rejects_lock(
        lock(&format!("git+{repo}?branch=v01#short"), 1),
        &fleet,
        &config,
    );
    for url in [shared.to_owned(), format!("{shared}.git")] {
        rejects_lock(
            vec![lock_pkg(
                "csgn",
                Some(&format!("git+{url}?branch=v01#{sha}")),
                Some("0.0.0"),
            )],
            &fleet,
            &config,
        );
    }
}

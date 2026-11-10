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

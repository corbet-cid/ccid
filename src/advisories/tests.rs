use super::*;

fn cblc_reviewed() -> Vec<Reviewed> {
    // The exact reviewed map of cblc's check-advisory-exposure.mjs.
    vec![
        Reviewed {
            id: "RUSTSEC-2024-0388".to_owned(),
            package: "derivative".to_owned(),
            version: "2.2.0".to_owned(),
        },
        Reviewed {
            id: "RUSTSEC-2024-0436".to_owned(),
            package: "paste".to_owned(),
            version: "1.0.15".to_owned(),
        },
        Reviewed {
            id: "RUSTSEC-2026-0173".to_owned(),
            package: "proc-macro-error2".to_owned(),
            version: "2.0.1".to_owned(),
        },
        Reviewed {
            id: "RUSTSEC-2025-0055".to_owned(),
            package: "tracing-subscriber".to_owned(),
            version: "0.2.25".to_owned(),
        },
        Reviewed {
            id: "RUSTSEC-2023-0071".to_owned(),
            package: "rsa".to_owned(),
            version: "0.10.0-rc.19".to_owned(),
        },
    ]
}

fn cpsd_reviewed() -> Vec<Reviewed> {
    // The exact reviewed map of cpsd's check-advisory-exposure.mjs.
    cblc_reviewed().into_iter().take(4).collect()
}

fn config(reviewed: Vec<Reviewed>) -> Config {
    Config {
        metadata: default_metadata(),
        forbid: ["native-tls".to_owned(), "rustls-pemfile".to_owned()].into(),
        floors: [
            ("hyper".to_owned(), "1.0.0".to_owned()),
            ("h2".to_owned(), "0.4.16".to_owned()),
            ("rustls".to_owned(), "0.23.0".to_owned()),
            ("rustls-webpki".to_owned(), "0.103.13".to_owned()),
        ]
        .into(),
        reviewed,
    }
}

/// Small synthetic metadata: the reviewed packages at their reviewed versions
/// plus repaired transport versions; remaining names are generic.
fn packages(pairs: &[(&str, &str)]) -> Vec<Package> {
    let json = serde_json::json!({
        "packages": pairs.iter().map(|(name, version)| {
            serde_json::json!({"name": name, "version": version})
        }).collect::<Vec<_>>(),
    });
    super::packages(&json).expect("synthetic metadata parses")
}

fn clean_packages() -> Vec<Package> {
    packages(&[
        ("derivative", "2.2.0"),
        ("paste", "1.0.15"),
        ("proc-macro-error2", "2.0.1"),
        ("tracing-subscriber", "0.2.25"),
        ("rsa", "0.10.0-rc.19"),
        ("hyper", "1.3.1"),
        ("h2", "0.4.16"),
        ("rustls", "0.23.4"),
        ("rustls-webpki", "0.103.13"),
        ("generic-crate", "0.1.0"),
    ])
}

const CBLC_POLICY: &str = r#"[advisories]
ignore = [
    { id = "RUSTSEC-2023-0071" },
]
"#;

const CPSD_POLICY: &str = r#"[advisories]
ignore = [
    { id = "RUSTSEC-2024-0388" },
    { id = "RUSTSEC-2024-0436" },
    { id = "RUSTSEC-2026-0173" },
    { id = "RUSTSEC-2025-0055" },
]
"#;

#[test]
fn cblc_reviewed_tree_passes() {
    assert_eq!(
        check(CBLC_POLICY, &clean_packages(), &config(cblc_reviewed())),
        Ok(())
    );
}

#[test]
fn cpsd_reviewed_tree_passes() {
    assert_eq!(
        check(CPSD_POLICY, &clean_packages(), &config(cpsd_reviewed())),
        Ok(())
    );
}

#[test]
fn unreviewed_exception_is_refused() {
    let policy = format!("{CBLC_POLICY}    {{ id = \"RUSTSEC-2099-0001\" }},\n]\n");
    assert_eq!(
        check(&policy, &clean_packages(), &config(cblc_reviewed())),
        Err("Unreviewed advisory exception: RUSTSEC-2099-0001".to_owned())
    );
}

#[test]
fn obsolete_exception_is_refused() {
    let stale = packages(&[("derivative", "2.3.0")]);
    assert_eq!(
        check(
            "{ id = \"RUSTSEC-2024-0388\" }",
            &stale,
            &config(cpsd_reviewed())
        ),
        Err("Remove or reassess obsolete exception: RUSTSEC-2024-0388".to_owned())
    );
}

#[test]
fn missing_reviewed_package_is_obsolete() {
    assert_eq!(
        check(
            "{ id = \"RUSTSEC-2024-0388\" }",
            &packages(&[("generic-crate", "0.1.0")]),
            &config(cpsd_reviewed())
        ),
        Err("Remove or reassess obsolete exception: RUSTSEC-2024-0388".to_owned())
    );
}

#[test]
fn legacy_tls_dependency_is_refused() {
    for name in ["native-tls", "rustls-pemfile"] {
        let present = packages(&[(name, "0.2.0"), ("hyper", "1.3.1")]);
        assert_eq!(
            check("", &present, &config(cpsd_reviewed())),
            Err(format!("Legacy TLS dependency returned: {name}"))
        );
    }
}

#[test]
fn version_floor_violation_is_refused() {
    let old = packages(&[("h2", "0.4.15")]);
    assert_eq!(
        check("", &old, &config(cpsd_reviewed())),
        Err("Version floor violated: h2 0.4.15 < 0.4.16".to_owned())
    );
}

#[test]
fn version_floor_boundaries_pass() {
    for (name, version) in [
        ("hyper", "1.0.0"),
        ("hyper", "2.0.0"),
        ("h2", "0.4.16"),
        ("h2", "0.5.0"),
        ("h2", "1.0.0"),
        ("rustls", "0.23.0"),
        ("rustls-webpki", "0.103.13"),
        ("rustls-webpki", "0.104.0"),
    ] {
        assert_eq!(
            check("", &packages(&[(name, version)]), &config(cpsd_reviewed())),
            Ok(()),
            "{name} {version}"
        );
    }
}

#[test]
fn prerelease_suffix_is_ignored() {
    assert_eq!(parts("1.0.0-rc.1"), [1, 0, 0]);
    assert_eq!(parts("0.10.0-rc.19"), [0, 10, 0]);
    assert_eq!(
        check(
            "",
            &packages(&[("hyper", "1.0.0-rc.1")]),
            &config(cpsd_reviewed())
        ),
        Ok(())
    );
}

#[test]
fn version_parts_parse_release_versions() {
    assert_eq!(parts("1.2.3"), [1, 2, 3]);
    assert_eq!(parts("0.4.16"), [0, 4, 16]);
    assert_eq!(parts("1"), [1, 0, 0]);
    assert_eq!(parts("0.4"), [0, 4, 0]);
    assert_eq!(parts(""), [0, 0, 0]);
    assert_eq!(parts("abc"), [0, 0, 0]);
    assert_eq!(parts("1.02.003"), [1, 2, 3]);
    assert_eq!(parts("1.2.3.4"), [1, 2, 3]);
}

#[test]
fn reviewed_exceptions_win_over_package_refusals() {
    let bad = packages(&[("h2", "0.4.15")]);
    assert_eq!(
        check(
            "{ id = \"RUSTSEC-2099-0001\" }",
            &bad,
            &config(cpsd_reviewed())
        ),
        Err("Unreviewed advisory exception: RUSTSEC-2099-0001".to_owned())
    );
}

#[test]
fn default_config_reads_the_default_metadata_file() {
    assert_eq!(Config::default().metadata, "dependency-metadata.json");
    assert!(Config::default().reviewed.is_empty());
}

#[test]
fn floors_must_be_plain_release_versions() {
    for floor in ["1", "0.4", "0.4.16", "0.103.13"] {
        assert!(valid_floor(floor), "{floor}");
    }
    for floor in [
        "",
        "abc",
        "1.",
        ".1",
        "1..0",
        "0.23.0-rc.1",
        "1.0.0.0",
        "v1.0.0",
        "1.0.0+build",
    ] {
        assert!(!valid_floor(floor), "{floor}");
    }
    let mut bad = config(cpsd_reviewed());
    bad.floors.insert("hyper".to_owned(), "one".to_owned());
    assert!(validate(&bad).is_err());
    assert!(validate(&config(cblc_reviewed())).is_ok());
}

#[test]
fn an_advisory_reviewed_twice_is_refused() {
    let mut reviewed = cpsd_reviewed();
    reviewed.push(Reviewed {
        id: "RUSTSEC-2024-0388".to_owned(),
        package: "derivative".to_owned(),
        version: "2.3.0".to_owned(),
    });
    assert!(validate(&config(reviewed)).is_err());
}

use super::*;
use std::ffi::OsString;

fn fleet() -> Fleet {
    Fleet {
        schema: 1,
        first_party: vec![
            "https://forge.example/org/".into(),
            "https://forge.example/other-org/".into(),
        ],
        aliases: BTreeMap::from([(
            "https://mirror.example/org/".to_owned(),
            "https://forge.example/org/".to_owned(),
        )]),
    }
}

fn with_prefix(prefix: &str) -> Fleet {
    Fleet {
        schema: 1,
        first_party: vec![prefix.to_owned()],
        aliases: BTreeMap::new(),
    }
}

fn environment(items: &[(&str, &str)]) -> Environment {
    items
        .iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect()
}

#[test]
fn a_prefix_is_one_canonical_https_owner_path() {
    for accepted in [
        "https://forge.example/org/",
        "https://forge.example/Org.name_x-1/",
        "https://sub.forge.example/org/",
    ] {
        assert!(
            with_prefix(accepted).validate().is_ok(),
            "{accepted} is a prefix"
        );
    }
    // One case per rule; every error names the offending entry.
    for (refused, reason) in [
        ("forge.example/org/", "is not a URL"),
        ("https://", "is not a URL"),
        ("http://forge.example/org/", "must use https"),
        ("ssh://forge.example/org/", "must use https"),
        (
            "https://user@forge.example/org/",
            "must not carry credentials",
        ),
        (
            "https://:secret@forge.example/org/",
            "must not carry credentials",
        ),
        ("https://forge.example:8443/org/", "must not name a port"),
        (
            "https://forge.example/org/?branch=main",
            "must not carry a query",
        ),
        (
            "https://forge.example/org/#main",
            "must not carry a fragment",
        ),
        ("https://Forge.example/org/", "the host must be lowercase"),
        ("https://forge.example/org", "must end with /"),
        (
            "https://forge.example/",
            "the path must be exactly /<owner>/",
        ),
        (
            "https://forge.example/org/repo/",
            "the path must be exactly /<owner>/",
        ),
        (
            "https://forge.example/o~g/",
            "the path must be exactly /<owner>/",
        ),
        (
            "https://forge.example/o%20g/",
            "the path must be exactly /<owner>/",
        ),
        (
            "https://forge.example/../",
            "the path must be exactly /<owner>/",
        ),
        (
            "https://forge.example/%2e/",
            "the path must be exactly /<owner>/",
        ),
        (
            "https://forge.example:443/org/",
            "is not in canonical URL form",
        ),
        (
            " https://forge.example/org/",
            "is not in canonical URL form",
        ),
        ("HTTPS://forge.example/org/", "is not in canonical URL form"),
        (
            "https://forge.example//org/",
            "the path must be exactly /<owner>/",
        ),
    ] {
        let error = with_prefix(refused).validate().unwrap_err().to_string();
        assert!(error.contains(reason), "{refused}: {error}");
        assert!(
            error.contains(&format!("{refused:?}")),
            "{refused}: {error}"
        );
    }
}

#[test]
fn the_fleet_shape_is_strict() {
    assert!(fleet().validate().is_ok());
    let mut schema = fleet();
    schema.schema = 2;
    assert!(schema
        .validate()
        .unwrap_err()
        .to_string()
        .contains("fleet schema 2 is not supported"));
    let mut empty = fleet();
    empty.first_party.clear();
    empty.aliases.clear();
    assert!(empty
        .validate()
        .unwrap_err()
        .to_string()
        .contains("at least one owner prefix"));
    let mut unsorted = fleet();
    unsorted.first_party.reverse();
    let error = unsorted.validate().unwrap_err().to_string();
    assert!(error.contains("sorted ascending"), "{error}");
    let mut duplicate = fleet();
    duplicate
        .first_party
        .push("https://forge.example/other-org/".into());
    let error = duplicate.validate().unwrap_err().to_string();
    assert!(error.contains("without duplicates"), "{error}");
    let mut dangling = fleet();
    dangling.aliases.insert(
        "https://mirror.example/third/".into(),
        "https://forge.example/third/".into(),
    );
    let error = dangling.validate().unwrap_err().to_string();
    assert!(
        error.contains("the target is not a first_party prefix"),
        "{error}"
    );
    assert!(error.contains("https://mirror.example/third/"), "{error}");
    let mut shadowing = fleet();
    shadowing.aliases.insert(
        "https://forge.example/other-org/".into(),
        "https://forge.example/org/".into(),
    );
    let error = shadowing.validate().unwrap_err().to_string();
    assert!(error.contains("is itself a first_party prefix"), "{error}");
    let mut bad_alias = fleet();
    bad_alias.aliases.insert(
        "http://mirror.example/other/".into(),
        "https://forge.example/org/".into(),
    );
    let error = bad_alias.validate().unwrap_err().to_string();
    assert!(
        error.contains("fleet alias \"http://mirror.example/other/\""),
        "{error}"
    );
}

#[test]
fn classification_is_a_byte_prefix_after_one_git_plus() {
    let fleet = fleet();
    assert_eq!(
        fleet.classify(&format!(
            "git+https://forge.example/org/x?branch=main#{}",
            "a".repeat(40)
        )),
        Some(Origin::Canonical {
            prefix: "https://forge.example/org/"
        })
    );
    assert_eq!(
        fleet.classify("https://forge.example/other-org/y.git"),
        Some(Origin::Canonical {
            prefix: "https://forge.example/other-org/"
        })
    );
    assert_eq!(
        fleet.classify("https://mirror.example/org/y"),
        Some(Origin::Alias {
            legacy: "https://mirror.example/org/",
            canonical: "https://forge.example/org/"
        })
    );
    for outside in [
        "https://forge.example/org-evil/x",
        "https://user@forge.example/org/x",
        "http://forge.example/org/x",
        "git+git+https://forge.example/org/x",
        "https://forge.example/org",
        "https://mirror.example/other-org/x",
        "",
    ] {
        assert_eq!(fleet.classify(outside), None, "{outside}");
    }
}

#[test]
fn insteadof_pairs_route_each_legacy_prefix_to_its_canonical_one() {
    let mut fleet = fleet();
    fleet.aliases.insert(
        "https://archive.example/other-org/".into(),
        "https://forge.example/other-org/".into(),
    );
    assert_eq!(
        fleet.insteadof_pairs(),
        vec![
            (
                "url.https://forge.example/other-org/.insteadOf".to_owned(),
                "https://archive.example/other-org/".to_owned()
            ),
            (
                "url.https://forge.example/org/.insteadOf".to_owned(),
                "https://mirror.example/org/".to_owned()
            ),
        ]
    );
}

#[test]
fn the_digest_names_the_data_not_its_json_spelling() {
    let compact = br#"{"default":{},"fleet":{"schema":1,"first_party":["https://forge.example/org/","https://forge.example/other-org/"],"aliases":{"https://mirror.example/org/":"https://forge.example/org/"}}}"#;
    let spelled = br#"{
      "fleet": {
        "aliases": { "https://mirror.example/org/" : "https://forge.example/org/" },
        "first_party": [ "https://forge.example/org/", "https://forge.example/other-org/" ],
        "schema": 1
      },
      "default": {"revision": "x"}
    }"#;
    let first = from_channel(compact).unwrap();
    let second = from_channel(spelled).unwrap();
    assert_eq!(first, fleet());
    assert_eq!(first.digest().unwrap(), second.digest().unwrap());
    let digest = first.digest().unwrap();
    assert_eq!(digest.len(), 64);
    assert!(digest
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    // Any change of the data changes the digest.
    let mut moved = fleet();
    moved.aliases.clear();
    assert_ne!(moved.digest().unwrap(), digest);
    // An invalid fleet has no digest at all.
    let mut invalid = fleet();
    invalid.first_party.reverse();
    assert!(invalid.digest().is_err());
    assert!(invalid.canonical_bytes().is_err());
}

#[test]
fn channel_documents_without_valid_fleet_data_are_refused() {
    assert!(from_channel(br#"{"default":{}}"#)
        .unwrap_err()
        .to_string()
        .contains("the release channel carries no fleet data"));
    assert!(from_channel(b"[]")
        .unwrap_err()
        .to_string()
        .contains("not a JSON object"));
    assert!(from_channel(b"not json").is_err());
    // Unknown fields are refused: a newer schema is never half-read.
    let error = from_channel(
        br#"{"fleet":{"schema":1,"first_party":["https://forge.example/org/"],"owners":[]}}"#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("malformed"), "{error}");
    let error =
        from_channel(br#"{"fleet":{"schema":1,"first_party":["https://forge.example/org"]}}"#)
            .unwrap_err()
            .to_string();
    assert!(error.contains("must end with /"), "{error}");
    // Aliases are optional.
    assert!(from_channel(
        br#"{"fleet":{"schema":1,"first_party":["https://forge.example/org/"]}}"#
    )
    .is_ok());
}

#[test]
fn load_reads_the_named_regular_file_only() {
    let error = load(&Environment::new()).unwrap_err().to_string();
    assert_eq!(
        error,
        "fleet data unavailable: set CCID_FLEET_FILE to a copy of the release channel (channel.json)"
    );
    assert!(load(&environment(&[(FLEET_FILE_ENV, "")])).is_err());
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("channel.json");
    fs::write(
        &file,
        br#"{"fleet":{"schema":1,"first_party":["https://forge.example/org/"]}}"#,
    )
    .unwrap();
    let (loaded, path) = load(&environment(&[(FLEET_FILE_ENV, file.to_str().unwrap())])).unwrap();
    assert_eq!(loaded, with_prefix("https://forge.example/org/"));
    assert_eq!(path, file);
    #[cfg(unix)]
    {
        let link = directory.path().join("link.json");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let error = load(&environment(&[(FLEET_FILE_ENV, link.to_str().unwrap())]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("must be a regular file"), "{error}");
    }
    let missing = directory.path().join("missing.json");
    let error = load(&environment(&[(FLEET_FILE_ENV, missing.to_str().unwrap())]))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("fleet data unavailable"), "{error}");
    fs::write(&file, br#"{"default":{}}"#).unwrap();
    let error = load(&environment(&[(FLEET_FILE_ENV, file.to_str().unwrap())]))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("the release channel carries no fleet data"),
        "{error}"
    );
}

#[test]
fn consumers_inside_a_check_require_the_declared_and_keyed_fleet() {
    let fleet = fleet();
    let digest = fleet.digest().unwrap();
    // Outside a ccid check nothing is keyed, so nothing is required.
    assert!(require_keyed(&Environment::new(), &fleet).is_ok());
    // Inside a check: declared and keyed with exactly this data.
    let inside = |items: &[(&str, &str)]| {
        let mut items = items.to_vec();
        items.push((INSIDE_CHECK_ENV, "/target"));
        environment(&items)
    };
    assert!(require_keyed(
        &inside(&[(FLEET_KEYED_ENV, "1"), (FLEET_DIGEST_ENV, &digest)]),
        &fleet
    )
    .is_ok());
    assert_eq!(
        require_keyed(&inside(&[(FLEET_DIGEST_ENV, &digest)]), &fleet)
            .unwrap_err()
            .to_string(),
        "this check reads fleet data; declare fleet = true on it in .ci/ccid.toml"
    );
    assert!(require_keyed(
        &inside(&[(FLEET_KEYED_ENV, "0"), (FLEET_DIGEST_ENV, &digest)]),
        &fleet
    )
    .is_err());
    assert_eq!(
        require_keyed(
            &inside(&[(FLEET_KEYED_ENV, "1"), (FLEET_DIGEST_ENV, &"0".repeat(64))]),
            &fleet
        )
        .unwrap_err()
        .to_string(),
        "fleet data changed after the check was keyed"
    );
    assert!(require_keyed(&inside(&[(FLEET_KEYED_ENV, "1")]), &fleet).is_err());
    // An empty marker still means "inside a check": the guard fails closed.
    let mut empty_marker = environment(&[(FLEET_KEYED_ENV, "0")]);
    empty_marker.insert(INSIDE_CHECK_ENV.into(), "".into());
    assert!(require_keyed(&empty_marker, &fleet).is_err());
}

fn checks(manifest: &str) -> BTreeMap<String, crate::Check> {
    #[derive(Deserialize)]
    struct Checks {
        checks: BTreeMap<String, crate::Check>,
    }
    toml::from_str::<Checks>(manifest).unwrap().checks
}

#[test]
fn normalisation_declares_the_digest_once_and_reserves_it_for_fleet_checks() {
    let mut parsed = checks(
        "[checks.reads]\nkind='commands'\nfleet=true\ncommands=[['true']]\n\
         [checks.listed]\nkind='commands'\nfleet=true\ncache_env=['A','CCID_FLEET_DIGEST']\ncommands=[['true']]\n\
         [checks.plain]\nkind='commands'\ncache_env=['A']\ncommands=[['true']]\n",
    );
    normalise(&mut parsed).unwrap();
    assert_eq!(parsed["reads"].cache_env, vec![FLEET_DIGEST_ENV]);
    assert_eq!(parsed["listed"].cache_env, vec!["A", FLEET_DIGEST_ENV]);
    assert_eq!(parsed["plain"].cache_env, vec!["A"]);
    // Idempotent: a second load never adds it twice.
    normalise(&mut parsed).unwrap();
    assert_eq!(parsed["reads"].cache_env, vec![FLEET_DIGEST_ENV]);
    let mut misdeclared = checks(
        "[checks.plain]\nkind='commands'\ncache_env=['CCID_FLEET_DIGEST']\ncommands=[['true']]\n",
    );
    let error = normalise(&mut misdeclared).unwrap_err().to_string();
    assert!(
        error.contains("Check plain lists CCID_FLEET_DIGEST in cache_env without fleet = true"),
        "{error}"
    );
}

fn channel_file(directory: &Path, fleet: &Fleet) -> PathBuf {
    let path = directory.join("channel.json");
    fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({"default": {}, "fleet": fleet})).unwrap(),
    )
    .unwrap();
    path
}

#[test]
fn binding_a_selection_sets_verifies_or_refuses_the_digest() {
    let mut parsed = checks(
        "[checks.reads]\nkind='commands'\nfleet=true\ncommands=[['true']]\n\
         [checks.plain]\nkind='commands'\ncommands=[['true']]\n",
    );
    normalise(&mut parsed).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let file = channel_file(directory.path(), &fleet());
    let digest = fleet().digest().unwrap();
    let file_env = || environment(&[(FLEET_FILE_ENV, file.to_str().unwrap())]);

    // No fleet check selected: the environment is untouched, fleet data or not.
    let mut untouched = Environment::new();
    bind_selection(&parsed, &["plain".into()], &mut untouched, false).unwrap();
    assert!(untouched.is_empty());

    // A fleet check without fleet data fails closed, naming the check.
    let error = bind_selection(
        &parsed,
        &["plain".into(), "reads".into()],
        &mut Environment::new(),
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("Check reads declares fleet = true but no fleet data is available: fleet data unavailable"), "{error}");

    // With fleet data the digest is bound for every key of the selection.
    let mut bound = file_env();
    bind_selection(&parsed, &["reads".into()], &mut bound, false).unwrap();
    assert_eq!(
        value(&bound, FLEET_DIGEST_ENV).as_deref(),
        Some(digest.as_str())
    );

    // An inherited digest of the same data is kept; of other data it is refused, never replaced.
    let mut same = bound.clone();
    bind_selection(&parsed, &["reads".into()], &mut same, true).unwrap();
    assert_eq!(same, bound);
    let mut other = file_env();
    set(&mut other, FLEET_DIGEST_ENV, "0".repeat(64));
    let error = bind_selection(&parsed, &["reads".into()], &mut other, false)
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("fleet data changed after the check was keyed"),
        "{error}"
    );

    // A cached task (moon child) must have been keyed with the digest by its parent.
    let error = bind_selection(&parsed, &["reads".into()], &mut file_env(), true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("keyed without CCID_FLEET_DIGEST"), "{error}");
}

/// The moon cache: cmnp keys and forwards a check's `cache_env`, so the normalised digest is
/// part of every moon task key of a `fleet = true` check (and of no other check's).
#[test]
fn the_moon_task_key_and_environment_carry_the_fleet_digest() {
    let mut parsed = checks(
        "[checks.reads]\nkind='commands'\nfleet=true\ncommands=[['true']]\n\
         [checks.plain]\nkind='commands'\ncommands=[['true']]\n",
    );
    normalise(&mut parsed).unwrap();
    let reads = crate::cached::executor_check(&parsed["reads"]);
    let plain = crate::cached::executor_check(&parsed["plain"]);
    let keyed = |digest: &str| {
        environment(&[
            (FLEET_DIGEST_ENV, digest),
            (FLEET_FILE_ENV, "/channel.json"),
        ])
    };
    let first = cmnp::inputs::semantic_environment(&reads, &keyed(&"a".repeat(64)));
    let second = cmnp::inputs::semantic_environment(&reads, &keyed(&"b".repeat(64)));
    assert_eq!(
        first[FLEET_DIGEST_ENV].as_deref(),
        Some("a".repeat(64).as_str())
    );
    assert_ne!(first, second, "another fleet is another moon key");
    assert!(
        !first.contains_key(FLEET_FILE_ENV),
        "the file's location is never keyed"
    );
    assert_eq!(
        cmnp::inputs::semantic_environment(&plain, &keyed(&"a".repeat(64))),
        cmnp::inputs::semantic_environment(&plain, &keyed(&"b".repeat(64))),
        "a check without fleet = true never keys the digest"
    );
    let forwarded = cmnp::inputs::execution_environment(&reads, &keyed(&"a".repeat(64)));
    assert_eq!(
        forwarded.get(OsStr::new(FLEET_DIGEST_ENV)),
        Some(&OsString::from("a".repeat(64)))
    );
    assert!(
        !cmnp::inputs::execution_environment(&plain, &keyed(&"a".repeat(64)))
            .contains_key(OsStr::new(FLEET_DIGEST_ENV))
    );
}

/// The cached task must also find the fleet data itself. cmnp 6dc3929 forwards only its
/// plumbing list, SEMANTIC_ENV, cache_env and non-`CCID_` ambient names, so CCID_FLEET_FILE is
/// dropped and a cached `fleet = true` check fails closed in its moon task. The fix belongs in
/// cmnp (forward CCID_FLEET_FILE as unkeyed plumbing).
#[test]
#[ignore = "cmnp 6dc3929 drops CCID_FLEET_FILE from moon tasks; enable with the cmnp fix"]
fn the_moon_task_environment_forwards_the_fleet_file() {
    let mut parsed = checks("[checks.reads]\nkind='commands'\nfleet=true\ncommands=[['true']]\n");
    normalise(&mut parsed).unwrap();
    let reads = crate::cached::executor_check(&parsed["reads"]);
    let forwarded = cmnp::inputs::execution_environment(
        &reads,
        &environment(&[(FLEET_DIGEST_ENV, "a"), (FLEET_FILE_ENV, "/channel.json")]),
    );
    assert_eq!(
        forwarded.get(OsStr::new(FLEET_FILE_ENV)),
        Some(&OsString::from("/channel.json"))
    );
}

/// The repository's own release channel carries valid fleet data.
#[test]
fn the_release_channel_of_this_repository_carries_valid_fleet_data() {
    let fleet = from_channel(include_bytes!("../../channel.json")).unwrap();
    assert_eq!(fleet.schema, SCHEMA);
    assert!(!fleet.first_party.is_empty());
}

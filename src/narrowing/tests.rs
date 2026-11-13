use super::*;

fn check() -> Check {
    Check {
        kind: "commands".into(),
        commands: vec![vec!["sh".into(), ".ci/verify.sh".into()]],
        cache_inputs: Some(vec!["src/**".into(), ".ci/**".into()]),
        ..Default::default()
    }
}

const PATHS: &[&str] = &[
    ".ci/ccid.toml",
    ".ci/verify.sh",
    "Cargo.lock",
    "Cargo.toml",
    "README.md",
    "docs/guide.md",
    "src/lib.rs",
    "src/nested/mod.rs",
    "vendor/dep/Cargo.lock",
];

fn measured_for(check: &Check, paths: &[&str]) -> Measured {
    Measured {
        tool: "a".repeat(40),
        commands: commands_digest(check).unwrap(),
        inputs: inputs_digest(check.cache_inputs.as_deref().unwrap_or_default()).unwrap(),
        layout: layout_digest(paths.iter().copied()),
        date: "2026-11-13".into(),
    }
}

fn measured_check() -> Check {
    let mut check = check();
    check.cache_inputs_measured = Some(measured_for(&check, PATHS));
    check
}

fn selected(scope: &Scope) -> Vec<&'static str> {
    match scope {
        Scope::ReadSet { selected, .. } => selected.iter().map(|index| PATHS[*index]).collect(),
        Scope::Tree { reason } => panic!("expected a read-set, got the whole tree: {reason:?}"),
    }
}

fn reason(scope: &Scope) -> String {
    match scope {
        Scope::Tree { reason } => reason.clone().unwrap_or_default(),
        Scope::ReadSet { .. } => panic!("expected the whole tree"),
    }
}

#[test]
fn civil_dates_are_the_proleptic_gregorian_calendar() {
    for (days, date) in [
        (0, "1970-01-01"),
        (59, "1970-03-01"),
        (365, "1971-01-01"),
        (10_957, "2000-01-01"),
        (11_016, "2000-02-29"),
        (19_000, "2022-01-08"),
        (19_782, "2024-02-29"),
        (20_770, "2026-11-13"),
    ] {
        assert_eq!(civil_date(days), date);
    }
    assert!(is_date("2026-11-13"));
    assert!(is_date(&today()));
}

#[test]
fn only_a_plausible_calendar_date_is_provenance() {
    for bad in [
        "",
        "2026-1-13",
        "2026-13-01",
        "2026-00-10",
        "2026-11-32",
        "26-11-13",
        "2026/11/13",
        "1999-12-31",
        "2026-11-1x",
    ] {
        assert!(!is_date(bad), "{bad:?}");
    }
}

#[test]
fn provenance_names_a_released_ccid_and_sha256_digests() {
    let good = measured_for(&check(), PATHS);
    good.validate().unwrap();
    for broken in [
        Measured {
            tool: "unversioned".into(),
            ..good.clone()
        },
        Measured {
            tool: "A".repeat(40),
            ..good.clone()
        },
        Measured {
            tool: "a".repeat(39),
            ..good.clone()
        },
        Measured {
            commands: "b".repeat(63),
            ..good.clone()
        },
        Measured {
            inputs: "b".repeat(63),
            ..good.clone()
        },
        Measured {
            layout: "z".repeat(64),
            ..good.clone()
        },
        Measured {
            date: "yesterday".into(),
            ..good.clone()
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
    }
}

#[test]
fn provenance_without_inputs_is_a_manifest_mistake_and_a_stale_one_is_not() {
    let mut orphan = check();
    orphan.cache_inputs_measured = Some(measured_for(&orphan, PATHS));
    orphan.cache_inputs = None;
    assert!(validate_declaration(&orphan).is_err());

    let mut malformed = measured_check();
    malformed.cache_inputs_measured.as_mut().unwrap().tool = "unversioned".into();
    assert!(validate_declaration(&malformed).is_err());

    // Stale (the check was edited after the measurement) is a normal state, not an error.
    let mut stale = measured_check();
    stale.commands = vec![vec!["sh".into(), "-c".into(), "true".into()]];
    validate_declaration(&stale).unwrap();
    validate_declaration(&check()).unwrap();
}

#[test]
fn the_manifest_field_parses_and_is_never_serialized() {
    let text = format!(
        "kind = 'commands'\ncommands = [['sh', '.ci/verify.sh']]\ncache_inputs = ['src/**']\n\
         cache_inputs_measured = {{ tool = '{}', commands = '{}', inputs = '{}', layout = '{}', date = '2026-11-13' }}\n",
        "a".repeat(40),
        "b".repeat(64),
        "d".repeat(64),
        "c".repeat(64),
    );
    let parsed: Check = toml::from_str(&text).unwrap();
    assert_eq!(
        parsed.cache_inputs_measured.as_ref().unwrap().date,
        "2026-11-13"
    );
    let value = serde_json::to_value(&parsed).unwrap();
    assert!(value.get("cache_inputs_measured").is_none());
    assert!(toml::from_str::<Check>(&text.replace("date =", "day =")).is_err());
}

#[test]
fn the_definition_digest_ignores_the_narrowing_and_the_float_but_nothing_else() {
    let base = commands_digest(&check()).unwrap();
    // The narrowing itself is not part of the definition it was measured against.
    let mut narrowed = check();
    narrowed.cache_inputs = Some(vec!["lib/**".into()]);
    assert_eq!(commands_digest(&narrowed).unwrap(), base);
    assert_eq!(commands_digest(&measured_check()).unwrap(), base);
    // A floated run adds its resolution variable to cache_env: plumbing, not the check.
    let mut floated = check();
    floated.cache_env.push(crate::float::RESOLUTION_ENV.into());
    assert_eq!(commands_digest(&floated).unwrap(), base);
    // Anything else the check declares is part of what was measured.
    let mut commands = check();
    commands.commands = vec![vec!["sh".into(), ".ci/other.sh".into()]];
    assert_ne!(commands_digest(&commands).unwrap(), base);
    let mut environment = check();
    environment.cache_env.push("SOME_FLAG".into());
    assert_ne!(commands_digest(&environment).unwrap(), base);
    let mut toolchain = check();
    toolchain.toolchain = Some("nightly".into());
    assert_ne!(commands_digest(&toolchain).unwrap(), base);
}

#[test]
fn the_layout_is_the_set_of_top_level_names() {
    let base = layout_digest(PATHS.iter().copied());
    let mut reversed = PATHS.to_vec();
    reversed.reverse();
    assert_eq!(layout_digest(reversed), base, "order is irrelevant");
    // Files below an existing top-level entry do not change it.
    let mut deeper = PATHS.to_vec();
    deeper.extend(["docs/new/page.md", "src/new.rs"]);
    assert_eq!(layout_digest(deeper), base);
    // Tool state and build output are not part of the repository's layout.
    let mut generated = PATHS.to_vec();
    generated.extend(["target/debug/x", "node_modules/y", ".moon/z", "moon.yml"]);
    assert_eq!(layout_digest(generated), base);
    // A top-level name that appears or disappears is.
    let mut added = PATHS.to_vec();
    added.push("fixtures/data.json");
    assert_ne!(layout_digest(added), base);
    let removed: Vec<&str> = PATHS
        .iter()
        .copied()
        .filter(|p| *p != "README.md")
        .collect();
    assert_ne!(layout_digest(removed), base);
}

#[test]
fn a_measured_check_is_keyed_on_its_inputs_the_locks_and_the_ci_directory() {
    let scope = decide(&measured_check(), PATHS);
    let files = selected(&scope);
    // The declared globs.
    assert!(files.contains(&"src/lib.rs") && files.contains(&"src/nested/mod.rs"));
    // `.ci/**`, where the commands' own scripts live.
    assert!(files.contains(&".ci/verify.sh") && files.contains(&".ci/ccid.toml"));
    // Locks and ecosystem manifests anywhere, which no narrow declaration can leave out.
    assert!(files.contains(&"Cargo.lock") && files.contains(&"Cargo.toml"));
    assert!(files.contains(&"vendor/dep/Cargo.lock"));
    // Everything the measurement found unread stays outside.
    assert!(!files.contains(&"README.md") && !files.contains(&"docs/guide.md"));
    let Scope::ReadSet { key, .. } = scope else {
        unreachable!()
    };
    assert_eq!(key.layout, layout_digest(PATHS.iter().copied()));
    assert_eq!(key.commands, commands_digest(&check()).unwrap());
    assert_eq!(
        key.inputs,
        inputs_digest(check().cache_inputs.as_deref().unwrap()).unwrap()
    );
}

#[test]
fn the_ci_directory_is_keyed_even_when_the_declared_inputs_forgot_it() {
    let mut check = measured_check();
    check.cache_inputs = Some(vec!["src/**".into()]);
    check.cache_inputs_measured = Some(measured_for(&check, PATHS));
    let files = selected(&decide(&check, PATHS));
    assert!(files.contains(&".ci/verify.sh"));
}

#[test]
fn a_negative_glob_narrows_but_never_removes_a_lock() {
    let mut check = measured_check();
    check.cache_inputs = Some(vec![
        "src/**".into(),
        "!src/nested/**".into(),
        "!Cargo.lock".into(),
    ]);
    check.cache_inputs_measured = Some(measured_for(&check, PATHS));
    let files = selected(&decide(&check, PATHS));
    assert!(files.contains(&"src/lib.rs"));
    assert!(!files.contains(&"src/nested/mod.rs"));
    assert!(files.contains(&"Cargo.lock"));
}

#[test]
fn declared_outputs_are_never_inputs() {
    let mut check = measured_check();
    check.cache_inputs = Some(vec!["src/**".into(), "docs/**".into()]);
    check.cache_outputs = vec!["docs/guide.md".into()];
    check.cache_inputs_measured = Some(measured_for(&check, PATHS));
    let files = selected(&decide(&check, PATHS));
    assert!(!files.contains(&"docs/guide.md"));
}

#[test]
fn every_unmeasured_situation_keeps_the_whole_tree() {
    // No inputs declared: nothing to narrow, and nothing to report.
    let mut plain = check();
    plain.cache_inputs = None;
    assert!(matches!(
        decide(&plain, PATHS),
        Scope::Tree { reason: None }
    ));
    let mut empty = check();
    empty.cache_inputs = Some(Vec::new());
    assert!(matches!(
        decide(&empty, PATHS),
        Scope::Tree { reason: None }
    ));

    // Inputs without provenance: a guessed list is a false skip waiting to happen.
    assert!(reason(&decide(&check(), PATHS)).contains("no measure-inputs provenance"));

    // Provenance from a local build, or malformed.
    let mut local = measured_check();
    local.cache_inputs_measured.as_mut().unwrap().tool = "unversioned".into();
    assert!(reason(&decide(&local, PATHS)).contains("released ccid"));

    // The list was edited by hand after the measurement: provenance is for the measured list.
    let mut hand_edited = measured_check();
    hand_edited.cache_inputs = Some(vec!["src/**".into()]);
    assert!(reason(&decide(&hand_edited, PATHS)).contains("differ from the list that was measured"));
    let mut reordered = measured_check();
    reordered.cache_inputs = Some(vec![".ci/**".into(), "src/**".into(), "src/**".into()]);
    assert!(
        matches!(decide(&reordered, PATHS), Scope::ReadSet { .. }),
        "order and repeats do not matter"
    );

    // The check was edited after the measurement.
    let mut edited = measured_check();
    edited.commands = vec![vec!["sh".into(), ".ci/other.sh".into()]];
    assert!(reason(&decide(&edited, PATHS)).contains("changed since"));

    // A nix check keys every file.
    let mut nix = measured_check();
    nix.kind = "nix".into();
    nix.cache_inputs_measured = Some(measured_for(&nix, PATHS));
    assert!(reason(&decide(&nix, PATHS)).contains("nix"));

    // Inputs that select nothing.
    let mut nothing = measured_check();
    nothing.cache_inputs = Some(vec!["nothing/**".into()]);
    nothing.cache_inputs_measured = Some(measured_for(&nothing, &["README.md"]));
    assert!(reason(&decide(&nothing, &["README.md"])).contains("select no file"));
}

#[test]
fn a_top_level_entry_the_measurement_never_saw_is_an_input_outside_the_measured_set() {
    let check = measured_check();
    let mut added = PATHS.to_vec();
    added.push("fixtures/data.json");
    added.sort_unstable();
    assert!(reason(&decide(&check, &added)).contains("outside the measured set"));
    let removed: Vec<&str> = PATHS
        .iter()
        .copied()
        .filter(|p| *p != "README.md")
        .collect();
    assert!(reason(&decide(&check, &removed)).contains("outside the measured set"));
    // A new file below a classified directory is not.
    let mut deeper = PATHS.to_vec();
    deeper.push("docs/new.md");
    deeper.sort_unstable();
    assert!(matches!(decide(&check, &deeper), Scope::ReadSet { .. }));
}

#[test]
fn glob_characters_in_a_path_do_not_widen_the_selection() {
    let mut check = measured_check();
    check.cache_inputs = Some(vec!["src/**".into()]);
    let paths = [".ci/ccid.toml", "src/a[1].rs", "src/b.rs", "x.txt"];
    check.cache_inputs_measured = Some(measured_for(&check, &paths));
    let files: Vec<&str> = match decide(&check, &paths) {
        Scope::ReadSet { selected, .. } => selected.iter().map(|index| paths[*index]).collect(),
        Scope::Tree { reason } => panic!("{reason:?}"),
    };
    assert_eq!(files, [".ci/ccid.toml", "src/a[1].rs", "src/b.rs"]);
}

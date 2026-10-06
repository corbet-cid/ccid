use super::*;

#[test]
fn only_the_immutable_environment_pin_changes() {
    let old = "a".repeat(40);
    let new = "b".repeat(40);
    let source = format!("# retain {old}\n      CCID_REVISION: \"{old}\"\nother: unchanged\n");
    assert_eq!(
        adapter::propose_pin(&source, &new).unwrap(),
        format!("# retain {old}\n      CCID_REVISION: \"{new}\"\nother: unchanged\n")
    );
}
#[test]
fn ambiguous_missing_and_nonimmutable_pins_are_refused() {
    let line = format!("      CCID_REVISION: '{SHA}'\n");
    for source in [
        String::new(),
        line.repeat(2),
        "      CCID_REVISION: 'main'\n".into(),
    ] {
        assert!(adapter::propose_pin(&source, SHA).is_err());
    }
    assert!(adapter::propose_pin(&line, "main").is_err());
}
#[test]
fn alias_and_profile_preserve_the_original_command_contract() {
    let row = json!({"repository":"owner/repo","reviewed_selector":"CHECK_TARGET","reviewed_step_environment":{"CARGO_INCREMENTAL":"0"}});
    let (selector, variables, steps, environment) = adapter::reviewed(
        &row,
        &json!({"CHECKS":"test"}),
        &strings(&[
            "    environment:",
            "      CCID_REVISION: pinned",
            "    commands:",
            "      - check \"$CHECKS\"",
        ]),
    )
    .unwrap();
    assert_eq!(selector, "CHECK_TARGET");
    assert_eq!(variables, json!({"CHECK_TARGET":"test"}));
    assert_eq!(
        steps,
        strings(&[
            "    environment:",
            "      CARGO_INCREMENTAL: \"0\"",
            "      CCID_REVISION: pinned",
            "    commands:",
            "      - check \"$CHECK_TARGET\""
        ])
    );
    assert_eq!(environment, json!({"CARGO_INCREMENTAL":"0"}));
}
#[test]
fn runtime_overrides_and_unknown_aliases_are_refused() {
    for row in [
        json!({"reviewed_selector":"OTHER"}),
        json!({"reviewed_step_environment":{"CCID_REVISION":SHA}}),
        json!({"reviewed_step_environment":{"NIX_CONFIG":"anything"}}),
        json!({"reviewed_step_environment":{"CARGO_INCREMENTAL":"yes"}}),
    ] {
        assert!(adapter::reviewed(
            &row,
            &json!({"CHECKS":"test"}),
            &strings(&["    environment:"])
        )
        .is_err());
    }
}
#[test]
fn serialized_corpus_key_order_does_not_change_the_yaml_contract() {
    let (_,_,steps,_)=adapter::reviewed(&json!({"reviewed_step_environment":{"CARGO_INCREMENTAL":"0","CARGO_PROFILE_DEV_DEBUG":"0"}}),&json!({"CHECKS":"test"}),&strings(&["    environment:"])).unwrap();
    assert_eq!(
        steps,
        strings(&[
            "    environment:",
            "      CARGO_PROFILE_DEV_DEBUG: \"0\"",
            "      CARGO_INCREMENTAL: \"0\""
        ])
    );
}
fn corpus() -> Value {
    let before = format!("      CCID_REVISION: '{SHA}'\ncommands: keep\n");
    let after = adapter::propose_pin(&before, &"b".repeat(40)).unwrap();
    json!({"tool_commit":"b".repeat(40),"sources":{"before":before,"after":after,"manifest":"schema=1\nproject='fixture'\n[checks.release]\nkind='commands'\ncommands=[['never-publish']]\n"},"pin_updates":[{"repository":"owner/repo","source_commit":SHA,"remote_verified":true,"manifest_sha256":"manifest","expected_checks":["release"],"workflows":[{"path":".crow/release.yaml","before_sha256":"before","after_sha256":"after"}]}]})
}
#[test]
fn release_commands_are_only_planned() {
    let mut calls = 0;
    assert_eq!(
        adapter::custom(&corpus(), |_, _, checks| {
            calls += 1;
            assert_eq!(checks, ["release"]);
            Ok(())
        })
        .unwrap(),
        1
    );
    assert_eq!(calls, 1);
}
#[test]
fn changes_outside_pin_are_rejected_before_any_plan() {
    let mut corpus = corpus();
    corpus["sources"]["after"] = json!(format!(
        "{}trigger: changed\n",
        text(&corpus["sources"], "after")
    ));
    let mut called = false;
    assert!(adapter::custom(&corpus, |_, _, _| {
        called = true;
        Ok(())
    })
    .is_err());
    assert!(!called);
}

fn canonical(raw: &str) -> Result<String> {
    core::canonical_remote(raw, &config(Path::new("/fixture")).origin_aliases)
}
#[test]
fn transports_share_identity_but_forges_do_not() {
    for url in [
        "https://forge.example.invalid/owner/repo.git",
        "git@forge.example.invalid:owner/repo.git",
        "ssh://git@forge.example.invalid:22/owner/repo.git",
        "http://forge.example.invalid:80/owner/repo.git",
    ] {
        assert_eq!(
            canonical(url).unwrap(),
            "https://forge.example.invalid/owner/repo"
        );
    }
    assert_ne!(
        canonical("https://github.com/owner/repo").unwrap(),
        canonical("https://forge.example.invalid/owner/repo").unwrap()
    );
}
#[test]
fn credentials_are_not_part_of_identity() {
    assert_eq!(
        canonical("https://secret:private@forge.example.invalid/owner/repo.git").unwrap(),
        "https://forge.example.invalid/owner/repo"
    );
}
#[test]
fn clone_alias_preserves_port_and_hostname_boundaries() {
    assert_eq!(
        canonical("https://alias.example.invalid/owner/repo").unwrap(),
        "https://forge.example.invalid/owner/repo"
    );
    assert_eq!(
        canonical("https://alias.example.invalid:444/owner/repo").unwrap(),
        "https://alias.example.invalid:444/owner/repo"
    );
    assert_eq!(
        canonical("https://alias.example.invalid.evil/owner/repo").unwrap(),
        "https://alias.example.invalid.evil/owner/repo"
    );
}
#[test]
fn ambiguous_registration_and_ssh_port_are_preserved() {
    assert_eq!(
        canonical("ssh://git@alias.example.invalid:2222/owner/repo").unwrap(),
        "https://alias.example.invalid:2222/owner/repo"
    );
    for url in [
        "file:///repo",
        "/repo",
        "https://forge.example.invalid/repo",
        "git@forge.example.invalid:owner/../repo",
    ] {
        assert!(canonical(url).is_err());
    }
}
#[test]
fn canonical_clone_alias_selects_only_its_forgejo_registration() {
    let f = Fixture::new();
    let mut api = Client::new(&f.commit);
    api.repos = vec![
        json!({"id":1,"active":true,"clone_url":"https://github.com/owner/repo"}),
        json!({"id":2,"active":true,"clone_url":"https://alias.example.invalid/owner/repo"}),
    ];
    assert_eq!(
        core::resolve_repo(&f.config(), &api, &f.repo).unwrap()["id"],
        2
    );
}
#[test]
fn clone_alias_duplicate_active_registrations_fail_closed() {
    let f = Fixture::new();
    let mut api = Client::new(&f.commit);
    api.repos
        .push(json!({"id":2,"active":true,"clone_url":"https://alias.example.invalid/owner/repo"}));
    assert!(core::resolve_repo(&f.config(), &api, &f.repo).is_err());
}
#[test]
fn equal_namespace_on_distinct_forges_never_selects_wrong_repository() {
    let f = Fixture::new();
    let mut api = Client::new(&f.commit);
    api.repos = vec![json!({"id":1,"active":true,"clone_url":"https://github.com/owner/repo"})];
    assert!(core::resolve_repo(&f.config(), &api, &f.repo).is_err());
}

#[test]
fn empty_or_absent_defaults_keep_existing_fallbacks() {
    for source in [
        "steps:\n",
        "variables:\n  CI_MEMORY_MB: {default: \"\"}\nsteps:\n",
    ] {
        assert_eq!(
            pinned::memory_defaults(source).unwrap()["CI_MEMORY_MB"],
            "16384"
        );
    }
}
#[test]
fn only_literal_variable_defaults_are_used() {
    let values=pinned::memory_defaults("variables:\n  CI_MEMORY_MB: {default: \"2048\"}\n  CI_MEMORY_PER_JOB_MB: {default: '1024'}\n  CI_MIN_AVAILABLE_MB: {default: 4096}\nsteps:\n  CI_MEMORY_MB: {default: \"99999\"}\n").unwrap();
    assert_eq!(values["CI_MEMORY_MB"], "2048");
    assert_eq!(values["CI_MEMORY_PER_JOB_MB"], "1024");
    assert_eq!(values["CI_MIN_AVAILABLE_MB"], "4096");
}
#[test]
fn ambiguous_invalid_or_executable_defaults_are_rejected() {
    for value in ["\"$MEMORY\"", "\"$(command)\"", "\"0\"", "-1", "1.5"] {
        assert!(pinned::memory_defaults(&format!(
            "variables:\n  CI_MEMORY_MB: {{default: {value}}}\nsteps:\n"
        ))
        .is_err());
    }
    assert!(pinned::memory_defaults(
        "variables:\n  CI_MEMORY_MB: {default: 2048}\n  CI_MEMORY_MB: {default: 4096}\n"
    )
    .is_err());
}
fn adapter(pin: &str, memory: &str, identity: bool) -> String {
    format!("variables:\n  CI_TOOL_ARCHIVE: {{default: \"\"}}\n  CI_MEMORY_MB: {{default: {memory}}}\n{}steps:\n  CCID_REVISION: '{pin}'\n",if identity {"  CI_REPOSITORY_URL: {default: \"\"}\n  CI_MIN_AVAILABLE_MB: {default: 8192}\n"} else {""})
}
fn tool_fixture(memory: &str, identity: bool) -> Fixture {
    let mut f = Fixture::new();
    let workflow = adapter(&f.commit, memory, identity);
    f.commit(&[(".crow/verify.yaml", workflow.as_bytes())]);
    f.publish();
    f
}
#[test]
fn exact_selected_workflow_profiles_reach_tool_identity() {
    let mut f = Fixture::new();
    let small = adapter(&f.commit, "2048", false);
    let large = adapter(&f.commit, "16384", false);
    f.commit(&[
        (".crow/small.yaml", small.as_bytes()),
        (".crow/large.yaml", large.as_bytes()),
    ]);
    let tool = pinned::identify(
        &f.config(),
        &f.repo,
        &f.commit,
        &["small".into(), "large".into()],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        tool.defaults["CI_MEMORY_MB"],
        BTreeSet::from(["16384".into(), "2048".into()])
    );
}
#[test]
fn committed_memory_default_controls_actual_submission() {
    let f = tool_fixture("2048", false);
    let p = submit::Prepared::new(&f.config(), &f.args(), &Client::new(&f.commit)).unwrap();
    assert_eq!(p.variables["CI_MEMORY_MB"], "2048");
}
#[test]
fn conflicting_workflow_memory_defaults_require_explicit_value() {
    let mut f = Fixture::new();
    let small = adapter(&f.commit, "2048", false);
    let large = adapter(&f.commit, "16384", false);
    f.commit(&[
        (".crow/small.yaml", small.as_bytes()),
        (".crow/large.yaml", large.as_bytes()),
    ]);
    f.publish();
    let mut args = f.args();
    args.workflows = vec!["small".into(), "large".into()];
    let api = Client::new(&f.commit);
    assert!(submit::Prepared::new(&f.config(), &args, &api).is_err());
    args.variables.push("CI_MEMORY_MB=4096".into());
    assert_eq!(
        submit::Prepared::new(&f.config(), &args, &api)
            .unwrap()
            .variables["CI_MEMORY_MB"],
        "4096"
    );
}
#[test]
fn pinned_tool_is_staged_and_its_content_is_part_of_submission_identity() {
    let f = tool_fixture("2048", false);
    let p = submit::Prepared::new(&f.config(), &f.args(), &Client::new(&f.commit)).unwrap();
    assert!(exact_digest(&text(&p.variables, "CI_TOOL_SHA256")));
    assert!(text(&p.variables, "CI_TOOL_ARCHIVE").contains("/ccid/"));
    assert!(p.variables.get("CI_CACHE_ROOT").is_none());
    let mut changed = p.variables.clone();
    changed["CI_TOOL_SHA256"] = json!("changed");
    assert_ne!(
        core::variables_identity(&p.variables),
        core::variables_identity(&changed)
    );
}
#[test]
fn new_archive_contract_receives_reserved_canonical_identity() {
    let f = tool_fixture("2048", true);
    let p = submit::Prepared::new(&f.config(), &f.args(), &Client::new(&f.commit)).unwrap();
    assert_eq!(
        p.variables["CI_REPOSITORY_URL"],
        "https://forge.example.invalid/owner/repo"
    );
    assert_eq!(p.variables["CI_MIN_AVAILABLE_MB"], "8192");
}
#[test]
fn adapter_without_literal_tool_pin_is_rejected() {
    let mut f = Fixture::new();
    f.commit(&[(".crow/verify.yaml", b"variables:\n  CI_TOOL_ARCHIVE: {}\n")]);
    assert!(pinned::identify(&f.config(), &f.repo, &f.commit, &["verify".into()]).is_err());
}

fn declaration(revision: &str, extra: &str) -> String {
    format!("schema = 1\n[archives.upstream]\nrevision = '{revision}'\narchive_variable = 'UPSTREAM_SOURCE_ARCHIVE'\ndigest_variable = 'UPSTREAM_SOURCE_SHA256'\n{extra}")
}
#[test]
fn exact_commit_and_distinct_integrity_variables() {
    let d = pinned::declarations(&declaration(SHA, "")).unwrap();
    assert_eq!(d[0].revision, SHA);
    assert_eq!(d[0].kind, "archive");
    assert_eq!(d[0].digest_variable, "UPSTREAM_SOURCE_SHA256");
}
#[test]
fn moving_refs_and_reserved_identity_overrides_are_rejected() {
    for revision in ["main", "HEAD", "v1"] {
        assert!(pinned::declarations(&declaration(revision, "")).is_err());
    }
    for key in [
        "CI_SOURCE_ARCHIVE",
        "CROW_SOURCE_ARCHIVE",
        "lower_SOURCE_ARCHIVE",
    ] {
        assert!(pinned::declarations(
            &declaration(SHA, "").replace("UPSTREAM_SOURCE_ARCHIVE", key)
        )
        .is_err());
    }
}
#[test]
fn bundle_source_resolves_only_for_the_declared_workflow() {
    let mut f = Fixture::new();
    let raw = declaration("source", "kind = 'git-bundle'\nworkflows = ['selected']\n")
        .replace("UPSTREAM_SOURCE_ARCHIVE", "UPSTREAM_SOURCE_BUNDLE");
    f.commit(&[(".ci/archives.toml", raw.as_bytes())]);
    let mut vars = json!({});
    assert!(pinned::prepare(
        &f.config(),
        &f.repo,
        &f.commit,
        f.root.path(),
        17,
        &mut vars,
        &["other".into()]
    )
    .unwrap()
    .is_empty());
    let sources = pinned::prepare(
        &f.config(),
        &f.repo,
        &f.commit,
        f.root.path(),
        17,
        &mut vars,
        &["selected".into()],
    )
    .unwrap();
    assert_eq!(sources[0].info["revision"], f.commit);
    assert!(text(&vars, "UPSTREAM_SOURCE_BUNDLE").ends_with(".bundle"));
}
#[test]
fn source_selector_is_not_a_moving_tar_revision() {
    assert!(pinned::declarations(&declaration("source", "")).is_err());
}
#[test]
fn bundle_rejects_wrong_transport_variable_and_unknown_kind() {
    assert!(pinned::declarations(&declaration(SHA, "kind='git-bundle'")).is_err());
    assert!(pinned::declarations(&declaration(SHA, "kind='zip'")).is_err());
}

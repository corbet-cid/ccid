use std::{fs, path::Path};

const PIN: &str = "1234567890abcdef1234567890abcdef12345678";

fn manifest(workflow: &str, scheduler: &str) -> String {
    format!(
        "schema=1\nproject='fixture'\n[render]\ntool_revision='{PIN}'\n\
         [checks.test]\nkind='commands'\ncommands=[['true']]\n\
         [jobs.verify]\nchecks=['test']\nworkflow='{workflow}'\ncommand=['sh','.ci/run.sh']\n\
         [jobs.alternate]\nscheduler='{scheduler}'\nchecks=['test']\nworkflow='{workflow}'\ncommand=['sh','.ci/run.sh']\n"
    )
}

fn repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".ci")).unwrap();
    fs::write(
        root.path().join(".ci/ccid.toml"),
        manifest("verify", "argo"),
    )
    .unwrap();
    root
}

fn render(root: &Path, check: bool) -> ccid::Result<ccid::render::Report> {
    ccid::render::render(root, Path::new(".ci/ccid.toml"), check)
}

#[test]
fn deterministic_inventory_retains_scheduler_choice_and_exact_check_contract() {
    let root = repository();
    assert!(render(root.path(), true).is_err());
    assert!(!root.path().join(".crow").exists());
    assert!(!root.path().join(".ci/jobs.json").exists());
    let result = render(root.path(), false).unwrap();
    assert_eq!(
        result.files.len(),
        2,
        "Shared workflows must be emitted once"
    );
    assert!(result.removed.is_empty());
    let inventory_path = root.path().join(".ci/jobs.json");
    let bytes = fs::read(&inventory_path).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["tool_revision"], PIN);
    assert_eq!(
        value["manifest_sha256"],
        ccid::sha256_file(&root.path().join(".ci/ccid.toml")).unwrap()
    );
    assert_eq!(value["jobs"]["verify"]["scheduler"], "crow");
    assert_eq!(value["jobs"]["alternate"]["scheduler"], "argo");
    assert_eq!(
        value["jobs"]["verify"]["checks"],
        serde_json::json!(["test"])
    );
    assert_eq!(
        value["jobs"]["verify"]["command"],
        value["jobs"]["alternate"]["command"]
    );
    render(root.path(), false).unwrap();
    assert_eq!(fs::read(inventory_path).unwrap(), bytes);
    assert!(render(root.path(), true).unwrap().checked);

    let adapter = root.path().join(".crow/verify.yaml");
    let expected = fs::read(&adapter).unwrap();
    fs::write(&adapter, [expected.as_slice(), b"# drift\n"].concat()).unwrap();
    assert!(render(root.path(), true).is_err());
    assert_ne!(
        fs::read(&adapter).unwrap(),
        expected,
        "Check must not repair files"
    );
    render(root.path(), false).unwrap();
    assert_eq!(fs::read(adapter).unwrap(), expected);
}

#[test]
fn invalid_jobs_or_foreign_files_fail_before_any_rendered_file_changes() {
    let root = repository();
    let config = root.path().join(".ci/ccid.toml");
    for invalid in [
        manifest("../escape", "argo"),
        manifest("verify", "unknown"),
        manifest("verify", "crow").replace(PIN, "main"),
        manifest("verify", "crow").replace("checks=['test']", "checks=['missing']"),
    ] {
        fs::write(&config, invalid).unwrap();
        assert!(render(root.path(), false).is_err());
        assert!(!root.path().join(".ci/jobs.json").exists());
        assert!(!root.path().join(".crow").exists());
    }
    fs::write(config, manifest("verify", "crow")).unwrap();
    fs::create_dir(root.path().join(".crow")).unwrap();
    let foreign = root.path().join(".crow/verify.yaml");
    fs::write(&foreign, "# Maintained by another workflow\n").unwrap();
    assert!(render(root.path(), false).is_err());
    assert_eq!(
        fs::read_to_string(foreign).unwrap(),
        "# Maintained by another workflow\n"
    );
    assert!(!root.path().join(".ci/jobs.json").exists());
}

#[test]
fn workflow_renames_remove_only_owned_obsolete_adapters() {
    let root = repository();
    render(root.path(), false).unwrap();
    let other = root.path().join(".crow/native.yaml");
    fs::write(&other, "# Native platform gate\n").unwrap();
    fs::write(
        root.path().join(".ci/ccid.toml"),
        manifest("renamed", "argo"),
    )
    .unwrap();
    assert!(render(root.path(), true).is_err());
    assert!(root.path().join(".crow/verify.yaml").exists());
    let result = render(root.path(), false).unwrap();
    assert_eq!(
        result.removed,
        vec![Path::new(".crow/verify.yaml").to_path_buf()]
    );
    assert!(!root.path().join(".crow/verify.yaml").exists());
    assert!(root.path().join(".crow/renamed.yaml").exists());
    assert_eq!(
        fs::read_to_string(other).unwrap(),
        "# Native platform gate\n"
    );
    render(root.path(), true).unwrap();
}

#[cfg(unix)]
#[test]
fn rendering_does_not_follow_output_directory_or_file_symlinks() {
    use std::os::unix::fs::symlink;
    let root = repository();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), root.path().join(".crow")).unwrap();
    assert!(render(root.path(), false).is_err());
    assert!(!root.path().join(".ci/jobs.json").exists());
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    fs::remove_file(root.path().join(".crow")).unwrap();
    fs::create_dir(root.path().join(".crow")).unwrap();
    let destination = outside.path().join("external.yaml");
    fs::write(&destination, "Keep this file\n").unwrap();
    symlink(&destination, root.path().join(".crow/verify.yaml")).unwrap();
    assert!(render(root.path(), false).is_err());
    assert_eq!(fs::read_to_string(destination).unwrap(), "Keep this file\n");
}

#[cfg(unix)]
#[test]
fn generated_shell_preserves_request_data_and_binds_the_crow_source() {
    use std::{os::unix::fs::PermissionsExt, process::Command};
    let root = repository();
    render(root.path(), false).unwrap();
    let tool = root.path().join("tool");
    fs::write(&tool, format!(
        "#!/bin/sh\nset -eu\nif [ \"$1\" = source-revision ]; then printf '%s\\n' '{PIN}'; exit; fi\n\
         test \"$1\" = execute-job\ntest \"$2\" = --request\ncp \"$3\" \"$RECEIVED\"\n\
         test \"$4\" = --expect-commit\ntest \"$5\" = \"$CI_COMMIT_SHA\"\n"
    )).unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let yaml = fs::read_to_string(root.path().join(".crow/verify.yaml")).unwrap();
    let script = yaml
        .split("  - name: repository-job\n")
        .nth(1)
        .unwrap()
        .split("  - name: native-status-complete\n")
        .next()
        .unwrap()
        .split("      - |\n")
        .nth(1)
        .unwrap()
        .lines()
        .map(|line| line.strip_prefix("        ").unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let request = r#"{"job":"verify","data":"$(touch injected); 'quoted'"}"#;
    let received = root.path().join("received.json");
    let output = Command::new("sh")
        .args(["-c", &script])
        .current_dir(root.path())
        .env("CI_TOOL_BINARY", &tool)
        .env("CI_TOOL_BINARY_SHA256", ccid::sha256_file(&tool).unwrap())
        .env("CCID_REVISION", PIN)
        .env("CI_COMMIT_SHA", "f".repeat(40))
        .env("CCID_JOB_REQUEST", request)
        .env("RECEIVED", &received)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(received).unwrap(),
        format!("{request}\n")
    );
    assert!(!root.path().join("injected").exists());
}

fn push_manifest() -> String {
    format!(
        "schema=1\nproject='fixture'\nrepository='https://forge.example.invalid/cpkg/demo.git'\n\
         [render]\ntool_revision='{PIN}'\n\
         [checks.test]\nkind='commands'\ncommands=[['true']]\n\
         [jobs.fast]\nchecks=['test']\nworkflow='manual'\ncommand=['ccid:run-declared-checks']\n\
         push_branches=['v01']\n\
         [push_consumer.deplib]\nconsumer='https://forge.example.invalid/cpkg/deplib.git'\n\
         branch='v01'\nconsumer_branch='v01'\njob='v01-fast'\nself_name='deplib'\n"
    )
}

#[test]
fn push_adapters_render_per_job_branch_with_baked_identities() {
    let root = repository();
    fs::write(root.path().join(".ci/ccid.toml"), push_manifest()).unwrap();
    assert!(render(root.path(), true).is_err());
    let result = render(root.path(), false).unwrap();
    assert!(result.removed.is_empty());
    let push = root.path().join(".crow/push-fast-v01.yaml");
    let consumer = root.path().join(".crow/push-consumer-deplib.yaml");
    assert!(push.is_file());
    assert!(consumer.is_file());
    for path in [&push, &consumer] {
        let content = fs::read_to_string(path).unwrap();
        assert!(content.starts_with("# Generated by ccid render v1;"));
        assert!(content.contains("- event: push"));
        // Branch nests inside the when item (proven live shape), never
        // top-level; the worker clones nothing here (Rust stages instead).
        assert!(content.contains("\n    branch: v01\n"));
        assert!(content.contains("skip_clone: true"));
        assert!(content.contains(PIN));
        assert!(content.contains("{from_secret: CI_TOOL_BINARY_STATIC}"));
        assert!(content.contains("{from_secret: CI_TOOL_BINARY_SHA256_STATIC}"));
        assert!(!content.contains("${"));
        for placeholder in [
            "CCID_TOOL_REVISION",
            "CCID_SECRET_",
            "CCID_PUSH_JOB",
            "CCID_PUSH_BRANCH",
            "CCID_CONSUMER",
            "CCID_DEP_",
        ] {
            assert!(!content.contains(placeholder));
        }
    }
    let content = fs::read_to_string(push).unwrap();
    assert!(content.contains("--consumer-url \"https://forge.example.invalid/cpkg/demo.git\""));
    assert!(content.contains("--job \"fast\""));
    assert!(content.contains("--trigger-kind self"));
    assert!(content.contains("--trigger-repo \"https://forge.example.invalid/cpkg/demo.git\""));
    let content = fs::read_to_string(consumer).unwrap();
    assert!(content.contains("https://forge.example.invalid/cpkg/deplib.git"));
    assert!(content.contains("push-run --consumer-url"));
    assert!(content.contains("--trigger-kind dep"));
    assert!(content.contains("--trigger-repo \"https://forge.example.invalid/cpkg/demo.git\""));
    let inventory: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join(".ci/jobs.json")).unwrap()).unwrap();
    assert_eq!(
        inventory["jobs"]["fast"]["push_branches"],
        serde_json::json!(["v01"])
    );
    assert_eq!(
        inventory["push_consumers"]["deplib"]["job"],
        serde_json::json!("v01-fast")
    );
    assert!(render(root.path(), true).unwrap().checked);
}

#[test]
fn invalid_push_declarations_fail_before_any_rendered_file_changes() {
    let root = repository();
    let config = root.path().join(".ci/ccid.toml");
    let base = push_manifest();
    for invalid in [
        base.replace("push_branches=['v01']", "push_branches=['bad branch']"),
        base.replace("push_branches=['v01']", "push_branches=['CCID_V01']"),
        // Push-executed jobs must opt into declared-checks execution:
        // an arbitrary command with push branches fails closed.
        base.replace(
            "command=['ccid:run-declared-checks']",
            "command=['sh','.ci/run.sh']",
        ),
        base.replace(
            "consumer='https://forge.example.invalid/cpkg/deplib.git'",
            "consumer='https://user:pass@forge.example.invalid/cpkg/deplib.git'",
        ),
        base.replace(
            "consumer='https://forge.example.invalid/cpkg/deplib.git'",
            "consumer='https://forge.example.invalid/deplib.git\"; touch injected; echo \"'",
        ),
        base.replace("self_name='deplib'", "self_name='deplib;evil'"),
        base.replace("[push_consumer.deplib]", "[push_consumer.'../escape']"),
        base.replace(
            "repository='https://forge.example.invalid/cpkg/demo.git'\n",
            "",
        ),
        base.replace(
            &format!("tool_revision='{PIN}'"),
            &format!("tool_revision='{PIN}'\ntool_secret_binary='lowercase-secret'"),
        ),
    ] {
        fs::write(&config, invalid).unwrap();
        assert!(render(root.path(), false).is_err());
        assert!(!root.path().join(".ci/jobs.json").exists());
        assert!(!root.path().join(".crow").exists());
    }
}

#[test]
fn manual_only_manifests_render_byte_identical_inventories() {
    // No opt-ins anywhere: no push files, and the inventory carries neither
    // push key, so every existing L adapter stays byte-stable.
    let root = repository();
    render(root.path(), false).unwrap();
    assert_eq!(
        fs::read_dir(root.path().join(".crow")).unwrap().count(),
        1,
        "Only the shared manual adapter may render"
    );
    let inventory: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join(".ci/jobs.json")).unwrap()).unwrap();
    assert!(inventory.get("push_branches").is_none());
    assert!(inventory.get("push_consumers").is_none());
    assert!(inventory["jobs"]["verify"].get("push_branches").is_none());
    assert!(inventory["jobs"]["alternate"]
        .get("push_branches")
        .is_none());
    assert!(render(root.path(), true).unwrap().checked);
}

#[test]
fn resolver_token_secret_opt_in_projects_only_the_named_reference() {
    let root = repository();
    let without = manifest("verify", "argo");
    fs::write(root.path().join(".ci/ccid.toml"), without).unwrap();
    render(root.path(), false).unwrap();
    let plain = fs::read_to_string(root.path().join(".crow/verify.yaml")).unwrap();
    assert!(!plain.contains("CFRG_RESOLVER_FORGEJO_TOKEN"));
    assert!(!plain.contains("from_secret"));

    let with = manifest("verify", "argo").replace(
        "[render]",
        "[render]\nresolver_token_secret = 'forgejo_token'",
    );
    fs::write(root.path().join(".ci/ccid.toml"), with).unwrap();
    render(root.path(), false).unwrap();
    let keyed = fs::read_to_string(root.path().join(".crow/verify.yaml")).unwrap();
    assert_eq!(keyed.matches("CFRG_RESOLVER_FORGEJO_TOKEN").count(), 1);
    assert_eq!(keyed.matches("from_secret").count(), 1);
    assert!(keyed.contains("from_secret: \"forgejo_token\""));
    // Native-status steps never carry the reference.
    let status_idx = keyed.find("native-status-pending").unwrap();
    let job_idx = keyed.find("repository-job").unwrap();
    assert!(!keyed[status_idx..job_idx].contains("CFRG_RESOLVER_FORGEJO_TOKEN"));

    let bad = manifest("verify", "argo")
        .replace("[render]", "[render]\nresolver_token_secret = 'bad name!'");
    fs::write(root.path().join(".ci/ccid.toml"), bad).unwrap();
    assert!(render(root.path(), false).is_err());
}

#[test]
fn resolver_token_secret_preserves_auxiliary_source_variables() {
    let root = repository();
    fs::write(
        root.path().join(".ci/archives.toml"),
        "schema = 1\n\
         [archives.shared]\n\
         revision = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n\
         archive_variable = 'SHARED_SOURCE_ARCHIVE'\n\
         digest_variable = 'SHARED_SOURCE_SHA256'\n",
    )
    .unwrap();

    fs::write(
        root.path().join(".ci/ccid.toml"),
        manifest("verify", "argo"),
    )
    .unwrap();
    render(root.path(), false).unwrap();
    let plain = fs::read_to_string(root.path().join(".crow/verify.yaml")).unwrap();
    assert!(plain.contains("SHARED_SOURCE_ARCHIVE: {default: \"\"}"));
    assert!(plain.contains("SHARED_SOURCE_SHA256: {default: \"\"}"));
    assert!(!plain.contains("CFRG_RESOLVER_FORGEJO_TOKEN"));

    let with = manifest("verify", "argo").replace(
        "[render]",
        "[render]\nresolver_token_secret = 'forgejo_token'",
    );
    fs::write(root.path().join(".ci/ccid.toml"), with).unwrap();
    render(root.path(), false).unwrap();
    let keyed = fs::read_to_string(root.path().join(".crow/verify.yaml")).unwrap();
    assert!(keyed.contains("SHARED_SOURCE_ARCHIVE: {default: \"\"}"));
    assert!(keyed.contains("SHARED_SOURCE_SHA256: {default: \"\"}"));
    assert_eq!(keyed.matches("CFRG_RESOLVER_FORGEJO_TOKEN").count(), 1);
    assert!(keyed.contains("from_secret: \"forgejo_token\""));
    let status_idx = keyed.find("native-status-pending").unwrap();
    let job_idx = keyed.find("repository-job").unwrap();
    assert!(!keyed[status_idx..job_idx].contains("CFRG_RESOLVER_FORGEJO_TOKEN"));
}

fn rendered_workflow(root: &tempfile::TempDir) -> String {
    fs::read_to_string(root.path().join(".crow/verify.yaml")).unwrap()
}

#[test]
fn status_steps_report_the_verdict_with_verify_gating_by_default() {
    let root = repository();
    render(root.path(), false).unwrap();
    let yaml = rendered_workflow(&root);
    assert_eq!(yaml.matches("CCID_VERDICT_JOBS: 'verify'").count(), 2);
    assert_eq!(
        yaml.matches("\"$CI_TOOL_BINARY\" verdict --commit").count(),
        2
    );
    assert!(yaml.contains("--gating \"$CCID_VERDICT_JOBS\""));
    assert!(!yaml.contains("CCID_GATING_JOBS"));
    // The reporter is verified through the same verified tool, never skipped.
    assert_eq!(
        yaml.matches("sha256sum --check --strict").count(),
        3,
        "tool check in both status steps and the job step"
    );
}

#[test]
fn a_manifest_declares_which_jobs_gate_and_side_jobs_stay_out() {
    let root = repository();
    let config = root.path().join(".ci/ccid.toml");
    let declared = format!(
        "{}[verdict]\njobs=['verify','alternate']\n",
        manifest("verify", "crow")
    );
    fs::write(&config, declared).unwrap();
    render(root.path(), false).unwrap();
    assert_eq!(
        rendered_workflow(&root)
            .matches("CCID_VERDICT_JOBS: 'verify,alternate'")
            .count(),
        2
    );
    let side_only = format!("{}[verdict]\njobs=['verify']\n", manifest("verify", "crow"));
    fs::write(&config, side_only).unwrap();
    render(root.path(), false).unwrap();
    assert_eq!(
        rendered_workflow(&root)
            .matches("CCID_VERDICT_JOBS: 'verify'")
            .count(),
        2
    );
    assert!(!rendered_workflow(&root).contains("alternate"));
}

#[test]
fn verdict_declarations_are_validated_and_absent_gating_posts_no_verdict() {
    let root = repository();
    let config = root.path().join(".ci/ccid.toml");
    for bad in [
        "[verdict]\njobs=['missing']\n",
        "[verdict]\njobs=['verify','verify']\n",
        "[verdict]\njobs=['verify']\nextra=1\n",
    ] {
        fs::write(&config, format!("{}{bad}", manifest("verify", "crow"))).unwrap();
        assert!(render(root.path(), false).is_err(), "{bad}");
    }
    let no_verify = manifest("verify", "crow").replace("[jobs.verify]", "[jobs.release]");
    fs::write(&config, no_verify).unwrap();
    render(root.path(), false).unwrap();
    assert_eq!(
        rendered_workflow(&root)
            .matches("CCID_VERDICT_JOBS: ''")
            .count(),
        2
    );
}

use std::{
    fs,
    path::{Path, PathBuf},
};

const PIN: &str = "1234567890abcdef1234567890abcdef12345678";

fn manifest(workflow: &str, scheduler: &str) -> String {
    format!(
        "schema=1\nproject='fixture'\n[render]\ntool_revision='{PIN}'\nallow_crow=true\n\
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

#[test]
fn check_shell_table_renders_one_shared_shell_and_removing_it_cleans_up() {
    let root = repository();
    let base = manifest("verify", "argo");
    fs::write(
        root.path().join(".ci/ccid.toml"),
        format!("{base}\n[check_shell]\n"),
    )
    .unwrap();
    let result = render(root.path(), false).unwrap();
    let shell = root.path().join(".ci/check-shell.nix");
    assert!(result.files.contains(&PathBuf::from(".ci/check-shell.nix")));
    let bytes = fs::read(&shell).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(text.starts_with("# Generated by ccid render v1;"));
    assert!(text.contains("b7c2ada94fe99c15b0dbcf4d11fd7850b957a436"));
    assert!(text.contains("export CC=gcc CXX=g++"));
    assert!(text.contains("CC_wasm32_unknown_unknown=clang"));
    assert!(text.contains("llvmPackages.libclang.lib"));
    assert!(text.contains("wild"));
    assert!(text.contains("mold"));
    // The RUNPATH contract: every runtime library directory as an rpath, and
    // no self rpath into the shell's $out, which never exists.
    assert!(text.contains(
        "NIX_LDFLAGS = lib.concatMapStringsSep \" \" (dir: \"-rpath ${dir}\") rpathDirs;"
    ));
    assert!(text.contains("NIX_NO_SELF_RPATH = true;"));
    // The same directories reach Cargo's fingerprint, so a changed shell
    // relinks a persistent target (Cargo never reads NIX_LDFLAGS).
    assert!(text.contains(
        "\"CARGO_TARGET_${pkgs.stdenv.hostPlatform.rust.cargoEnvVarTarget}_RUSTFLAGS\" =\n      lib.concatMapStringsSep \" \" (dir: \"-C link-arg=-Wl,-rpath-link,${dir}\") rpathDirs;"
    ));
    assert!(render(root.path(), true)
        .unwrap()
        .files
        .contains(&PathBuf::from(".ci/check-shell.nix")));

    // Removing the opt-in removes the renderer-owned file, one stack only.
    fs::write(root.path().join(".ci/ccid.toml"), base).unwrap();
    let removed = render(root.path(), false).unwrap();
    assert_eq!(removed.removed, vec![PathBuf::from(".ci/check-shell.nix")]);
    assert!(!shell.exists());
    assert!(render(root.path(), true).is_ok());
}

#[test]
fn a_hand_written_check_shell_is_never_taken_over() {
    let root = repository();
    let shell = root.path().join(".ci/check-shell.nix");
    fs::write(&shell, "# The repository owns this shell\n").unwrap();
    let base = manifest("verify", "argo");
    fs::write(
        root.path().join(".ci/ccid.toml"),
        format!("{base}\n[check_shell]\n"),
    )
    .unwrap();
    assert!(render(root.path(), false).is_err());
    assert_eq!(
        fs::read_to_string(&shell).unwrap(),
        "# The repository owns this shell\n"
    );
    // Without the opt-in a foreign file is not obsolete either.
    fs::write(root.path().join(".ci/ccid.toml"), base).unwrap();
    render(root.path(), false).unwrap();
    assert!(shell.exists());
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
        .split("  - name: native-status-failure\n")
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
         [render]\ntool_revision='{PIN}'\nallow_crow=true\n\
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
            &format!(
                "tool_revision='{PIN}'\nallow_crow=true\ntool_secret_binary='lowercase-secret'"
            ),
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

fn secret_manifest(declaration: &str, release_secrets: &str) -> String {
    format!(
        "schema=1\nproject='fixture'\n[render]\ntool_revision='{PIN}'\nallow_crow=true\n{declaration}\n\
         [checks.test]\nkind='commands'\ncommands=[['true']]\n\
         [jobs.verify]\nchecks=['test']\nworkflow='verify'\ncommand=['sh','.ci/run.sh']\n\
         [jobs.release]\nchecks=['test']\nworkflow='verify'\ncommand=['sh','.ci/run.sh']\n{release_secrets}\n"
    )
}

#[test]
fn secret_environment_projects_named_references_and_scopes_them_to_jobs() {
    let root = repository();
    let write = |text: String| fs::write(root.path().join(".ci/ccid.toml"), text).unwrap();
    write(secret_manifest("", ""));
    render(root.path(), false).unwrap();
    let plain = rendered_workflow(&root);
    assert!(!plain.contains("from_secret"));

    write(secret_manifest(
        "secret_environment = { CFRG_RELEASE_TOKEN = 'cfrg_release_token', ANOTHER_KEY = 'another_key' }",
        "secrets = ['CFRG_RELEASE_TOKEN']",
    ));
    render(root.path(), false).unwrap();
    let keyed = rendered_workflow(&root);
    assert_eq!(keyed.matches("from_secret").count(), 2);
    // Name order, one reference each, between the revision pin and the commands
    // of the repository-job step.
    let job = keyed.find("name: repository-job").unwrap();
    let step = &keyed[job..];
    let pin = step.find("CCID_REVISION: '").unwrap();
    let first = step
        .find("ANOTHER_KEY:\n        from_secret: \"another_key\"")
        .unwrap();
    let second = step
        .find("CFRG_RELEASE_TOKEN:\n        from_secret: \"cfrg_release_token\"")
        .unwrap();
    assert!(pin < first && first < second);
    assert!(second < step.find("commands:").unwrap());
    // Native-status steps never carry a reference.
    let status = keyed.find("native-status-pending").unwrap();
    assert!(!keyed[status..job].contains("from_secret"));
    // The inventory records which job may see a secret, and no other.
    let inventory: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join(".ci/jobs.json")).unwrap()).unwrap();
    assert_eq!(
        inventory["jobs"]["release"]["secrets"],
        serde_json::json!(["CFRG_RELEASE_TOKEN"])
    );
    assert!(inventory["jobs"]["verify"].get("secrets").is_none());
    assert!(inventory["jobs"]["release"].get("withheld").is_none());
    assert!(render(root.path(), true).unwrap().checked);
}

#[test]
fn secret_environment_refuses_names_it_could_shadow_and_undeclared_secrets() {
    let root = repository();
    for declaration in [
        "secret_environment = { lowercase = 'ok' }",
        "secret_environment = { CI_COMMIT_SHA = 'ok' }",
        "secret_environment = { CCID_ANYTHING = 'ok' }",
        "secret_environment = { CFRG_STATUS_TOKEN = 'ok' }",
        "secret_environment = { CFRG_RESOLVER_FORGEJO_TOKEN = 'ok' }",
        "secret_environment = { RUNNER_TEMP = 'ok' }",
        "secret_environment = { GOOD_NAME = 'bad name!' }",
    ] {
        fs::write(
            root.path().join(".ci/ccid.toml"),
            secret_manifest(declaration, ""),
        )
        .unwrap();
        assert!(render(root.path(), false).is_err(), "{declaration}");
        assert!(!root.path().join(".crow").exists());
    }
    fs::write(
        root.path().join(".ci/ccid.toml"),
        secret_manifest("", "secrets = ['NOT_DECLARED']"),
    )
    .unwrap();
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
    // The pending step, the job step (which reports its own result) and the
    // failure step that covers a job that never reached its own report.
    assert_eq!(yaml.matches("CCID_VERDICT_JOBS: 'verify'").count(), 3);
    assert_eq!(
        yaml.matches("\"$CI_TOOL_BINARY\" verdict --commit").count(),
        2
    );
    // Crow 6.4 provides no pipeline status variable to steps.
    assert!(!yaml.contains("CI_PIPELINE_STATUS"));
    assert!(!yaml.contains("native-status-complete"));
    let job = yaml.find("name: repository-job").unwrap();
    let failure = yaml.find("name: native-status-failure").unwrap();
    assert!(yaml[job..failure].contains("CCID_STATUS_CONFIG: '${CCID_STATUS_CONFIG:-}'"));
    assert!(yaml[failure..].contains("- status: [failure]"));
    assert!(yaml[failure..].contains("--state failure"));
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
        3
    );
    let side_only = format!("{}[verdict]\njobs=['verify']\n", manifest("verify", "crow"));
    fs::write(&config, side_only).unwrap();
    render(root.path(), false).unwrap();
    assert_eq!(
        rendered_workflow(&root)
            .matches("CCID_VERDICT_JOBS: 'verify'")
            .count(),
        3
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
        3
    );
}

fn forgejo_manifest(job_extra: &str) -> String {
    format!(
        "schema=1\nproject='fixture'\n[render]\ntool_revision='{PIN}'\nallow_crow=true\n\
         [checks.test]\nkind='commands'\ncommands=[['true']]\n\
         [checks.lint]\nkind='commands'\ncommands=[['true']]\n\
         [jobs.verify]\nscheduler='forgejo'\nchecks=['test','lint']\nworkflow='ci'\ncommand=['ccid:run-declared-checks']\n{job_extra}\
         [jobs.docs]\nscheduler='forgejo'\nchecks=['lint']\nworkflow='ci'\ncommand=['ccid:run-declared-checks']\n"
    )
}

fn forgejo_repository(manifest: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".ci")).unwrap();
    fs::write(root.path().join(".ci/ccid.toml"), manifest).unwrap();
    root
}

#[test]
fn forgejo_jobs_render_one_native_workflow_per_workflow_name_and_no_crow_adapter() {
    let root = forgejo_repository(&forgejo_manifest("runs_on='heavy'\npaths=['src/**']\n"));
    render(root.path(), false).unwrap();
    assert!(!root.path().join(".crow").exists());
    let text = fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap();
    assert!(text.starts_with("# Generated by ccid render v1; edit .ci/ccid.toml instead.\n"));
    assert!(text.contains("name: ci\n"));
    assert!(text.contains("branches: [main]"));
    assert!(text.contains("  pull_request:\n  workflow_dispatch:\n"));
    assert!(text.contains("cancel-in-progress: true"));
    assert!(text.contains("CARGO_INCREMENTAL: \"1\""));
    // The shared build profile is cargo config (profiles/dev.toml) or the CARGO_HOME include, never a CARGO_* variable: those
    // are hashed into every compile-cache key and split lanes from CI.
    assert!(!text.contains("CARGO_PROFILE_") && !text.contains("CARGO_TERM_COLOR"));
    assert!(text.contains("  verify:\n    runs-on: heavy\n"));
    assert!(text.contains("  docs:\n    runs-on: light\n    steps:\n"));
    assert!(text.contains("  verify:\n    runs-on: heavy\n    needs: [docs]\n    if: ${{ !cancelled() }}\n    steps:\n"));
    assert!(text.contains("--check 'test' --check 'lint'"));
    // Git LFS content is pulled over HTTP Basic (the "token" scheme is refused by the LFS endpoint).
    assert!(
        text.contains("git lfs install --local --skip-smudge")
            && text.contains("credential.helper=!f()")
    );
    assert!(text.contains(&format!("CCID_REVISION: {PIN}")));
    // One job lacks paths, so the workflow takes no path filter.
    assert!(!text.contains("paths:"));
    assert!(!text.contains("CCID_TOOL_REVISION") && !text.contains("CCID_CHECK_ARGUMENTS"));
    // Rendering is stable and checkable.
    let before = text.clone();
    render(root.path(), true).unwrap();
    render(root.path(), false).unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap(),
        before
    );
    let inventory: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join(".ci/jobs.json")).unwrap()).unwrap();
    assert_eq!(inventory["jobs"]["verify"]["scheduler"], "forgejo");
    assert_eq!(inventory["jobs"]["verify"]["runs_on"], "heavy");
}

#[test]
fn forgejo_workflows_filter_paths_only_when_every_job_declares_them() {
    let manifest = forgejo_manifest("paths=['src/**','Cargo.toml']\n")
        .replace("[jobs.docs]\n", "[jobs.docs]\npaths=['docs/**']\n");
    let root = forgejo_repository(&manifest);
    render(root.path(), false).unwrap();
    let text = fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap();
    assert!(text
        .contains("    paths:\n      - \"Cargo.toml\"\n      - \"docs/**\"\n      - \"src/**\"\n"));
}

#[test]
fn forgejo_jobs_refuse_crow_only_features_and_unsafe_values() {
    for extra in [
        "runs_on='heavy label'\n",
        "runs_on='CCID_x'\n",
        "paths=['a b']\n",
    ] {
        let root = forgejo_repository(&forgejo_manifest(extra));
        assert!(render(root.path(), false).is_err(), "{extra}");
        assert!(!root.path().join(".forgejo").exists(), "{extra}");
    }
    // Non-Forgejo jobs cannot carry Forgejo keys.
    let root = forgejo_repository(&manifest("verify", "crow").replace(
        "command=['sh','.ci/run.sh']\n[jobs.alternate]",
        "command=['sh','.ci/run.sh']\nruns_on='heavy'\n[jobs.alternate]",
    ));
    assert!(render(root.path(), false).is_err());
}

#[test]
fn switching_a_job_to_forgejo_removes_only_the_owned_crow_adapter() {
    let root = forgejo_repository(&manifest("verify", "crow"));
    render(root.path(), false).unwrap();
    assert!(root.path().join(".crow/verify.yaml").exists());
    fs::write(root.path().join(".crow/handwritten.yaml"), "steps: []\n").unwrap();
    fs::write(root.path().join(".ci/ccid.toml"), forgejo_manifest("")).unwrap();
    let report = render(root.path(), false).unwrap();
    assert_eq!(
        report.removed,
        vec![Path::new(".crow/verify.yaml").to_path_buf()]
    );
    assert!(!root.path().join(".crow/verify.yaml").exists());
    assert!(root.path().join(".crow/handwritten.yaml").exists());
    assert!(root.path().join(".forgejo/workflows/ci.yml").exists());
}

#[test]
fn forgejo_events_select_the_triggers_and_must_agree_within_a_workflow() {
    let manual = forgejo_manifest("events=['manual']\n")
        .replace("[jobs.docs]\n", "[jobs.docs]\nevents=['manual']\n");
    let root = forgejo_repository(&manual);
    render(root.path(), false).unwrap();
    let text = fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap();
    assert!(text.contains("on:\n  workflow_dispatch:\nconcurrency:"));
    assert!(!text.contains("push:") && !text.contains("pull_request:"));
    let split = forgejo_manifest("events=['manual']\n");
    let root = forgejo_repository(&split);
    assert!(render(root.path(), false).is_err());
    let unknown = forgejo_manifest("events=['schedule']\n");
    let root = forgejo_repository(&unknown);
    assert!(render(root.path(), false).is_err());
}

#[test]
fn forgejo_jobs_may_run_a_repository_script_with_the_adapter_contract() {
    let manifest = forgejo_manifest("").replace(
        "[jobs.docs]\nscheduler='forgejo'\nchecks=['lint']\nworkflow='ci'\ncommand=['ccid:run-declared-checks']",
        "[jobs.docs]\nscheduler='forgejo'\nchecks=['lint','test']\nworkflow='ci'\ncommand=['bash',\"it's.sh\"]",
    );
    let root = forgejo_repository(&manifest);
    render(root.path(), false).unwrap();
    let text = fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap();
    assert!(text.contains(
        "export CCID_BIN=\"$tool\" CI_COMMIT_SHA=\"${{ github.sha }}\" CI_REPOSITORY_URL=\"${{ github.server_url }}/${{ github.repository }}\" CHECKS='lint,test'\n          exec 'bash' 'it'\\''s.sh'\n"
    ));
    // The declared-checks job in the same workflow still runs ccid check itself.
    assert!(text.contains("exec \"$tool\" check --repo \"$PWD\" --check 'test' --check 'lint'\n"));
    assert!(!text.contains("CCID_RUN"));
}

#[test]
fn forgejo_tool_digest_is_validated_and_rendered_for_the_hash_checked_fetch() {
    let digest = "ab".repeat(32);
    let manifest = forgejo_manifest("").replace(
        &format!("tool_revision='{PIN}'\nallow_crow=true\n"),
        &format!("tool_revision='{PIN}'\nallow_crow=true\ntool_sha256='{digest}'\n"),
    );
    let root = forgejo_repository(&manifest);
    render(root.path(), false).unwrap();
    let text = fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap();
    assert!(text.contains(&format!("          CCID_TOOL_SHA256: {digest}\n")));
    assert!(text.contains("sha256sum --check --strict"));
    assert!(!text.contains("CCID_TOOL_SHA_LINE"));
    let bad = forgejo_manifest("").replace(
        &format!("tool_revision='{PIN}'\nallow_crow=true\n"),
        &format!("tool_revision='{PIN}'\nallow_crow=true\ntool_sha256='XYZ'\n"),
    );
    let root = forgejo_repository(&bad);
    assert!(render(root.path(), false).is_err());
}

#[test]
fn forgejo_jobs_archive_their_flake_inputs_by_default_and_a_job_can_opt_out() {
    // Every Forgejo job gets the archive post-step unless it declares archive_inputs=false.
    let root = forgejo_repository(&forgejo_manifest(""));
    render(root.path(), false).unwrap();
    let text = fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap();
    assert_eq!(text.matches("Archive the locked flake inputs").count(), 2);
    assert!(text.contains("nix flake archive --json ."));
    assert!(text.contains("CCID_FLAKE_ARCHIVE_PUSH"));
    let inventory: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join(".ci/jobs.json")).unwrap()).unwrap();
    assert_eq!(inventory["jobs"]["verify"]["archive_inputs"], true);
    assert_eq!(inventory["jobs"]["docs"]["archive_inputs"], true);

    let opted_out = forgejo_repository(&forgejo_manifest("archive_inputs=false\n"));
    render(opted_out.path(), false).unwrap();
    let text = fs::read_to_string(opted_out.path().join(".forgejo/workflows/ci.yml")).unwrap();
    assert_eq!(text.matches("Archive the locked flake inputs").count(), 1);
    let inventory: serde_json::Value =
        serde_json::from_slice(&fs::read(opted_out.path().join(".ci/jobs.json")).unwrap()).unwrap();
    assert!(inventory["jobs"]["verify"].get("archive_inputs").is_none());
    assert_eq!(inventory["jobs"]["docs"]["archive_inputs"], true);

    // A non-Forgejo job still refuses to declare it.
    let crow = forgejo_repository(&manifest("verify", "crow").replace(
        "command=['sh','.ci/run.sh']\n[jobs.alternate]",
        "command=['sh','.ci/run.sh']\narchive_inputs=true\n[jobs.alternate]",
    ));
    assert!(render(crow.path(), false).is_err());
}

#[test]
fn forgejo_jobs_export_their_declared_environment_before_the_checks() {
    let manifest = forgejo_manifest("environment={ CI_JOBS='2', QUOTED=\"it's\" }\n");
    let root = forgejo_repository(&manifest);
    render(root.path(), false).unwrap();
    let text = fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap();
    assert!(text.contains("export CI_JOBS='2'\n          export QUOTED='it'\\''s'\n          "));
}

#[test]
fn crow_is_refused_unless_the_manifest_keeps_it_alive_and_forgejo_and_argo_are_not() {
    let crow = manifest("verify", "crow").replace("allow_crow=true\n", "");
    let root = forgejo_repository(&crow);
    let error = render(root.path(), false).unwrap_err().to_string();
    assert!(error.contains("Crow is retired"), "{error}");
    assert!(!root.path().join(".crow").exists());
    assert!(!root.path().join(".ci/jobs.json").exists());
    // the escape hatch keeps old repositories rendering
    render(
        forgejo_repository(&manifest("verify", "crow")).path(),
        false,
    )
    .unwrap();
    // a Forgejo-only manifest needs no flag
    let forgejo = forgejo_manifest("").replace("allow_crow=true\n", "");
    render(forgejo_repository(&forgejo).path(), false).unwrap();
}

#[cfg(unix)]
#[test]
fn forgejo_fetch_step_retries_the_copy_then_asks_the_forge() {
    use std::os::unix::fs::PermissionsExt;
    let root = forgejo_repository(&forgejo_manifest("runs_on='heavy'\n"));
    render(root.path(), false).unwrap();
    let text = fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap();
    let start = text
        .find("          fetched=")
        .expect("retry block rendered");
    let end = text[start..].find("cannot fetch").unwrap() + start;
    let end = start + text[start..end].len() + text[end..].find('\n').unwrap();
    let block: String = text[start..end]
        .replace("${{ github.sha }}", "abc")
        .lines()
        .map(|line| line.trim_start())
        .collect::<Vec<_>>()
        .join("\n");
    // `fetch` is the step's own function; here it fails `fails` times (the copy lags), then works. The forge fallback is told apart by GIT_CONFIG_COUNT=0 AND GIT_CONFIG_SYSTEM=/dev/null (the runner keeps its copy rewrite rules in either place).
    for (fails, forge_only, ok) in [
        (0, false, true),
        (2, false, true),
        (4, true, true),
        (9, false, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("sleep");
        fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let counter = dir.path().join("n");
        let script = format!(
            "set -eu\nPATH={}:$PATH\nfetch() {{ n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; if [ {forge} = 1 ] && {{ [ \"${{GIT_CONFIG_COUNT:-}}\" != 0 ] || [ \"${{GIT_CONFIG_SYSTEM:-}}\" != /dev/null ]; }} ; then return 1; fi; [ $n -gt {fails} ]; }}\n{block}\n",
            dir.path().display(),
            c = counter.display(),
            forge = u8::from(forge_only),
        );
        let status = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .status()
            .unwrap();
        assert_eq!(
            status.success(),
            ok,
            "fails={fails} forge_only={forge_only}"
        );
    }
}

fn remote_manifest() -> String {
    format!(
        "schema=1\nproject='fixture'\n[render]\ntool_revision='{PIN}'\n\
         [checks.test]\nkind='commands'\ncommands=[['true']]\n\
         [jobs.verify]\nscheduler='forgejo'\nchecks=['test']\nworkflow='ci'\ncommand=['ccid:run-declared-checks']\n\
         [jobs.windows]\nscheduler='remote'\nprovider='gitlab'\nplatform='windows-x64'\n\
         workflow='ccid-remote'\nchecks=['test']\npush_branches=['main']\n\
         paths=['src/**','Cargo.toml']\n"
    )
}

#[test]
fn a_retired_remote_job_gets_no_workflow_and_no_inventory_entry() {
    // `scheduler = "remote"` is retired (tests/it/retired_mirror.rs): no provider workflow file is required and the job
    // is not recorded; the other jobs render as before.
    let root = forgejo_repository(&remote_manifest());
    render(root.path(), false).unwrap();
    assert!(!root.path().join(".crow").exists());
    assert!(!root.path().join(".gitlab-ci.yml").exists());
    let workflow = fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap();
    assert!(!workflow.contains("windows"));
    let inventory: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join(".ci/jobs.json")).unwrap()).unwrap();
    assert!(inventory["jobs"]["windows"].is_null());
    assert_eq!(inventory["jobs"]["verify"]["scheduler"], "forgejo");
    render(root.path(), true).unwrap();
}

#[test]
fn forgejo_siblings_are_declared_validated_and_staged_at_their_revisions() {
    let pinned = "0123456789abcdef0123456789abcdef01234567";
    let root = forgejo_repository(&forgejo_manifest(&format!(
        "siblings={{cdate='main','other/cink'='{pinned}'}}\n"
    )));
    render(root.path(), false).unwrap();
    let text = fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap();
    assert_eq!(text.matches("stage_sibling() {").count(), 1);
    assert!(text.contains("stage_sibling 'cdate' \"${{ github.repository_owner }}/cdate\" 'main'"));
    assert!(text.contains(&format!("stage_sibling 'cink' \"other/cink\" '{pinned}'")));
    let inventory: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join(".ci/jobs.json")).unwrap()).unwrap();
    assert_eq!(inventory["jobs"]["verify"]["siblings"]["cdate"], "main");
    assert!(inventory["jobs"]["docs"].get("siblings").is_none());

    // A job without siblings renders no staging at all.
    let plain = forgejo_repository(&forgejo_manifest(""));
    render(plain.path(), false).unwrap();
    let text = fs::read_to_string(plain.path().join(".forgejo/workflows/ci.yml")).unwrap();
    assert!(!text.contains("stage_sibling"));

    for bad in [
        "siblings={cdate='v1'}\n",
        "siblings={cdate='0123456789ABCDEF0123456789abcdef01234567'}\n",
        "siblings={'a/b/c'='main'}\n",
        "siblings={'../x'='main'}\n",
    ] {
        let repository = forgejo_repository(&forgejo_manifest(bad));
        assert!(render(repository.path(), false).is_err(), "{bad}");
    }
    let crow = forgejo_repository(&manifest("verify", "crow").replace(
        "command=['sh','.ci/run.sh']\n[jobs.alternate]",
        "command=['sh','.ci/run.sh']\nsiblings={cdate='main'}\n[jobs.alternate]",
    ));
    assert!(render(crow.path(), false).is_err());
}

#[cfg(unix)]
#[test]
fn the_rendered_sibling_step_stages_exact_checkouts_and_refuses_a_wrong_pin() {
    use std::process::Command;
    let git = |dir: &Path, args: &[&str]| {
        let output = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let forge = tempfile::tempdir().unwrap();
    let remote = forge.path().join("owner/cdate.git");
    fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "--quiet", "--bare", "-b", "main"]);
    let work = forge.path().join("work");
    fs::create_dir(&work).unwrap();
    git(&work, &["init", "--quiet", "-b", "main"]);
    fs::write(work.join("typst.toml"), "name = \"cdate\"\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "--quiet", "-m", "one"]);
    let first = git(&work, &["rev-parse", "HEAD"]);
    fs::write(work.join("more"), "x").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "--quiet", "-m", "two"]);
    let second = git(&work, &["rev-parse", "HEAD"]);
    git(
        &work,
        &["push", "--quiet", remote.to_str().unwrap(), "main"],
    );

    let stage = |revision: &str| {
        let root = forgejo_repository(&forgejo_manifest(&format!(
            "siblings={{cdate='{revision}'}}\n"
        )));
        render(root.path(), false).unwrap();
        let text = fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap();
        let start = text.find("          # Sibling repositories").unwrap();
        let end = text.find("export CCID_SIBLINGS_DIR").unwrap();
        let end = end + text[end..].find('\n').unwrap();
        let script: String = text[start..end]
            .lines()
            .map(|line| line.trim_start_matches("          "))
            .collect::<Vec<_>>()
            .join("\n")
            .replace(
                "${{ github.server_url }}",
                &format!("file://{}", forge.path().display()),
            )
            .replace("${{ github.repository_owner }}", "owner");
        let checkout = tempfile::tempdir().unwrap();
        let tail = "\necho \"DIR=$CCID_SIBLINGS_DIR\"\necho \"REV=$CCID_SIBLINGS_REVISIONS\"\n";
        let output = Command::new("sh")
            .args(["-eu", "-c", &format!("{script}{tail}")])
            .current_dir(checkout.path())
            .env("CCID_FORGE_TOKEN", "unused")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        (output, checkout)
    };
    let (output, checkout) = stage("main");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains(&format!("REV=cdate={second}")), "{stdout}");
    let staged = PathBuf::from(format!("{}.siblings/cdate", checkout.path().display()));
    assert!(staged.join("typst.toml").is_file() && staged.join("more").is_file());
    let (output, checkout) = stage(&first);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let staged = PathBuf::from(format!("{}.siblings/cdate", checkout.path().display()));
    assert!(staged.join("typst.toml").is_file() && !staged.join("more").exists());
}

#[test]
fn render_schema_2_renders_the_stable_shim_without_a_pin_and_refuses_pins() {
    let central = forgejo_manifest("runs_on='heavy'\n")
        .replace(&format!("tool_revision='{PIN}'"), "render_schema=2");
    let root = forgejo_repository(&central);
    render(root.path(), false).unwrap();
    let text = fs::read_to_string(root.path().join(".forgejo/workflows/ci.yml")).unwrap();
    // The shim carries no revision, no digest, no step logic: it finds the central release and runs `ccid job-run`.
    assert!(!text.contains(PIN));
    assert!(!text.contains("CCID_REVISION:") && !text.contains("CCID_TOOL_SHA256: "));
    assert!(!text.contains("CARGO_INCREMENTAL"));
    assert!(text.contains("CCID_CHANNEL_URL"));
    assert!(text.contains("exec \"$tool\" job-run --job 'verify'"));
    assert!(text.contains("exec \"$tool\" job-run --job 'docs'"));
    assert!(text.contains("class='heavy'") && text.contains("class='light'"));
    assert!(!text.contains("git fetch") && !text.contains("lfs"));
    // Stable and checkable.
    render(root.path(), true).unwrap();
    // A pin next to the schema is refused, an unknown schema is refused.
    let pinned = forgejo_manifest("").replace(
        &format!("tool_revision='{PIN}'"),
        &format!("render_schema=2\ntool_revision='{PIN}'"),
    );
    assert!(render(forgejo_repository(&pinned).path(), false).is_err());
    let unknown =
        forgejo_manifest("").replace(&format!("tool_revision='{PIN}'"), "render_schema=3");
    assert!(render(forgejo_repository(&unknown).path(), false).is_err());
}

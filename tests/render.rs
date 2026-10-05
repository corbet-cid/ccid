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

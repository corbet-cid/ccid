//! Pod-local mode: no root ssh, a mounted token and source store.
use super::*;
use std::os::unix::fs::PermissionsExt;

fn json_config(extra: &str) -> String {
    format!(
        r#"{{"api":"https://ci.example.invalid/api/v1","state_root":"/state","host_sources":"/mnt/sources",
        "worker_sources":"/worker/sources","host_tools":"/mnt/tools","worker_tools":"/worker/tools",
        "remote_binary":"/mnt/tools/ccid","tool_repo":"/state/tool","tool_origins":[],
        "argo_namespace":"ci","argo_template":"ccid-job"{extra}}}"#
    )
}
fn load(extra: &str) -> Result<Config> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("submission.json");
    fs::write(&path, json_config(extra))?;
    Config::load(Some(&path))
}

#[test]
fn a_mounted_token_file_replaces_the_command_and_ssh_is_optional() {
    let pod = load(r#","token_file":"/var/run/secrets/crow/token""#).unwrap();
    assert!(pod.ssh.is_empty() && pod.token_command.is_empty());
    assert_eq!(
        pod.token_file.as_deref(),
        Some(Path::new("/var/run/secrets/crow/token"))
    );
    assert!(load(r#","token_command":["cat","/x"]"#).is_ok());
    assert!(load("").is_err(), "a credential source is required");
    assert!(load(r#","token_file":"/a","token_command":["cat","/x"]"#).is_err());
    assert!(load(r#","token_file":"relative/token""#).is_err());
}

#[test]
fn the_token_is_read_from_the_file_and_redacted_from_output() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    fs::write(&token, "sekret-token-value\n").unwrap();
    let mut config = config(dir.path());
    config.token_command = vec![];
    config.token_file = Some(token.clone());
    let crow = core::Crow::new(&config).unwrap();
    assert_eq!(
        crow.redact("Bearer sekret-token-value ok").unwrap(),
        "Bearer [redacted] ok"
    );
    fs::write(&token, vec![b'x'; 5000]).unwrap();
    assert!(
        core::Crow::new(&config).is_err(),
        "oversized token files are refused"
    );
    config.token_file = Some(dir.path().to_path_buf());
    assert!(
        core::Crow::new(&config).is_err(),
        "a directory is not a token file"
    );
}

#[test]
fn without_ssh_commands_run_locally() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.ssh = vec![];
    assert_eq!(
        config.ssh(&strings(&["printf", "hello"]), None).unwrap(),
        b"hello"
    );
    assert_eq!(
        config.ssh(&strings(&["cat"]), Some(b"piped")).unwrap(),
        b"piped"
    );
    assert!(config.ssh(&strings(&["false"]), None).is_err());
}

/// One shared fake receiver, never rewritten (parallel tests would race with exec).
fn receiver() -> &'static Path {
    static SCRIPT: std::sync::OnceLock<(tempfile::TempDir, PathBuf)> = std::sync::OnceLock::new();
    let (_, path) = SCRIPT.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("receiver");
        fs::write(
            &script,
            "#!/bin/sh\ntest \"$1 $2 $3\" = 'crow-ci receive --target'\nmkdir -p \"$(dirname \"$4\")\"\ncat > \"$4\"\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        (dir, script)
    });
    path
}

#[test]
fn sources_are_staged_through_the_mounted_store_without_ssh() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.ssh = vec![];
    config.remote_binary = receiver().to_string_lossy().into_owned();
    config.host_sources = dir.path().join("mounted").to_string_lossy().into_owned();
    let archive = dir.path().join("source.tar");
    fs::write(&archive, b"archive bytes").unwrap();
    let digest = "a".repeat(64);
    let staged = transport::stage(&config, &archive, "namespace", &digest, "tar").unwrap();
    assert_eq!(staged, format!("/worker/sources/namespace/{digest}.tar"));
    assert_eq!(
        fs::read(dir.path().join(format!("mounted/namespace/{digest}.tar"))).unwrap(),
        b"archive bytes"
    );
    config.remote_binary = "false".into();
    assert!(transport::stage(&config, &archive, "namespace", &digest, "tar").is_err());
}

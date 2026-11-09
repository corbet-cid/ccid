use super::*;
use std::io::Write;

fn repo() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn write(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut file = fs::File::create(path).unwrap();
    file.write_all(contents).unwrap();
}

fn lock(packages: &[(&str, &str)]) -> String {
    let mut text = String::from("version = 4\n");
    for (name, version) in packages {
        text.push_str(&format!(
            "[[package]]\nname = \"{name}\"\nversion = \"{version}\"\n"
        ));
    }
    text
}

fn artifact(version_json: &str, digest_json: &str) -> String {
    format!("{{\"version\": {version_json}, \"sha256\": {digest_json}}}")
}

fn lock_repo(root: &Path, packages: &[(&str, &str)], version_json: &str, digest_json: &str) {
    write(&root.join("Cargo.lock"), lock(packages).as_bytes());
    write(
        &root.join(".ci/native-sdk.json"),
        artifact(version_json, digest_json).as_bytes(),
    );
}

fn outcome(root: &Path, name: &str, file: &str, package: &str) -> Outcome {
    verify_lock_version(root, name, file, package).unwrap()
}

fn is_pass(value: &Outcome) -> bool {
    matches!(value, Outcome::Pass(_))
}

fn refusal(value: &Outcome) -> String {
    match value {
        Outcome::Refuse(line) => line.clone(),
        Outcome::Pass(line) => panic!("expected refusal, got pass: {line}"),
    }
}

// The six cases of cblc:.ci/test-native-sdk.py: one accepted and five refused.

#[test]
fn matching_sdk_and_lock_pass() {
    let repo = repo();
    lock_repo(
        repo.path(),
        &[("barretenberg-rs", "5.2.1")],
        "\"5.2.1\"",
        &format!("\"{}\"", "ab".repeat(32)),
    );
    let checked = outcome(
        repo.path(),
        "native-sdk",
        ".ci/native-sdk.json",
        "barretenberg-rs",
    );
    assert!(is_pass(&checked));
}

#[test]
fn missing_duplicate_or_different_sdk_refuses() {
    for versions in [
        Vec::new(),
        vec!["5.2.0"],
        vec!["5.2.1", "5.2.1"],
        vec!["5.2.1", "5.3.0"],
    ] {
        let repo = repo();
        let packages: Vec<(&str, &str)> =
            versions.iter().map(|v| ("barretenberg-rs", *v)).collect();
        lock_repo(
            repo.path(),
            &packages,
            "\"5.2.1\"",
            &format!("\"{}\"", "ab".repeat(32)),
        );
        let line = refusal(&outcome(
            repo.path(),
            "native-sdk",
            ".ci/native-sdk.json",
            "barretenberg-rs",
        ));
        assert_eq!(
            line,
            "content-pins: native-sdk: Locked barretenberg-rs must exactly match the authenticated native artifact",
            "{versions:?}"
        );
    }
}

#[test]
fn invalid_artifact_identity_refuses() {
    for (version_json, digest_json, expected) in [
        (
            "null".to_string(),
            format!("\"{}\"", "ab".repeat(32)),
            "content-pins: native-sdk: Native artifact needs a stable upstream version",
        ),
        (
            "\"5\"".to_string(),
            format!("\"{}\"", "ab".repeat(32)),
            "content-pins: native-sdk: Native artifact needs a stable upstream version",
        ),
        (
            "\"5.2.1\"".to_string(),
            "\"\"".to_string(),
            "content-pins: native-sdk: Native artifact needs a SHA-256 digest",
        ),
        (
            "\"5.2.1\"".to_string(),
            format!("\"{}\"", "AB".repeat(32)),
            "content-pins: native-sdk: Native artifact needs a SHA-256 digest",
        ),
    ] {
        let repo = repo();
        lock_repo(
            repo.path(),
            &[("barretenberg-rs", "5.2.1")],
            &version_json,
            &digest_json,
        );
        let line = refusal(&outcome(
            repo.path(),
            "native-sdk",
            ".ci/native-sdk.json",
            "barretenberg-rs",
        ));
        assert_eq!(line, expected, "{version_json} {digest_json}");
    }
}

#[test]
fn fixture_manifest_match_passes() {
    let repo = repo();
    let root = repo.path();
    write(&root.join("tests/fixtures/a.json"), b"{}");
    let digest = sha256_file(&root.join("tests/fixtures/a.json")).unwrap();
    write(
        &root.join("tests/fixtures/provenance.json"),
        format!("{{\"sha256\": {{\"a.json\": \"{digest}\"}}}}").as_bytes(),
    );
    let checked =
        verify_sha256_manifest(root, "fixtures", "tests/fixtures/provenance.json", "sha256")
            .unwrap();
    match checked {
        Outcome::Pass(line) => assert_eq!(
            line,
            "content-pins: fixtures: 1 files match tests/fixtures/provenance.json"
        ),
        Outcome::Refuse(line) => panic!("expected pass, got refusal: {line}"),
    }
}

#[test]
fn fixture_manifest_mismatch_refuses() {
    let repo = repo();
    let root = repo.path();
    write(&root.join("tests/fixtures/a.json"), b"{}");
    write(
        &root.join("tests/fixtures/provenance.json"),
        b"{\"sha256\": {\"a.json\": \"00\"}}",
    );
    let line = refusal(
        &verify_sha256_manifest(root, "fixtures", "tests/fixtures/provenance.json", "sha256")
            .unwrap(),
    );
    assert_eq!(
        line,
        "content-pins: fixtures: a.json does not match tests/fixtures/provenance.json"
    );
}

#[test]
fn missing_fixture_file_refuses() {
    let repo = repo();
    let root = repo.path();
    write(
        &root.join("tests/fixtures/provenance.json"),
        b"{\"sha256\": {\"gone.json\": \"00\"}}",
    );
    let line = refusal(
        &verify_sha256_manifest(root, "fixtures", "tests/fixtures/provenance.json", "sha256")
            .unwrap(),
    );
    assert_eq!(
        line,
        "content-pins: fixtures: gone.json does not match tests/fixtures/provenance.json"
    );
}

#[test]
fn fixture_path_escape_refuses() {
    for escaped in ["../x", "/etc/passwd"] {
        let repo = repo();
        let root = repo.path();
        write(
            &root.join("tests/fixtures/provenance.json"),
            format!("{{\"sha256\": {{\"{escaped}\": \"00\"}}}}").as_bytes(),
        );
        let line = refusal(
            &verify_sha256_manifest(root, "fixtures", "tests/fixtures/provenance.json", "sha256")
                .unwrap(),
        );
        assert_eq!(
            line,
            format!(
                "content-pins: fixtures: {escaped} does not match tests/fixtures/provenance.json"
            ),
            "{escaped}"
        );
    }
}

#[test]
fn duplicate_pin_names_are_an_operational_error() {
    let repo = repo();
    let root = repo.path();
    write(
        &root.join(".ci/ccid.toml"),
        b"schema = 1\nproject = 'pins'\n[[content_pins]]\nname = 'a'\nkind = 'lock-version'\nfile = '.ci/native-sdk.json'\npackage = 'barretenberg-rs'\n[[content_pins]]\nname = 'a'\nkind = 'lock-version'\nfile = '.ci/native-sdk.json'\npackage = 'barretenberg-rs'\n",
    );
    let result = run(&Options {
        repo: root.to_path_buf(),
        manifest: PathBuf::from(".ci/ccid.toml"),
        pin: Vec::new(),
    });
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("duplicate content pin"));
}

#[test]
fn unknown_pin_kind_is_an_operational_error() {
    let repo = repo();
    let root = repo.path();
    write(
        &root.join(".ci/ccid.toml"),
        b"schema = 1\nproject = 'pins'\n[[content_pins]]\nname = 'a'\nkind = 'bogus'\n",
    );
    let result = run(&Options {
        repo: root.to_path_buf(),
        manifest: PathBuf::from(".ci/ccid.toml"),
        pin: Vec::new(),
    });
    assert!(result.is_err());
}

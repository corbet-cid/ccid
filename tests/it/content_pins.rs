#![forbid(unsafe_code)]

use std::{fs, process::Command};

fn binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_ccid"))
}

fn repo_with(pins: &str) -> tempfile::TempDir {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join(".ci")).unwrap();
    fs::write(
        root.join(".ci/ccid.toml"),
        format!("schema = 1\nproject = 'pins'\n{pins}"),
    )
    .unwrap();
    temp
}

fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(binary())
        .arg("content-pins")
        .arg("--repo")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
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

#[test]
fn content_pins_passes_refuses_and_reports_operational_errors() {
    let pins = "[[content_pins]]\nname = 'native-sdk'\nkind = 'lock-version'\nfile = '.ci/native-sdk.json'\npackage = 'barretenberg-rs'\n";
    let temp = repo_with(pins);
    let root = temp.path();
    let digest = "ab".repeat(32);
    fs::write(
        root.join(".ci/native-sdk.json"),
        format!("{{\"version\": \"5.2.1\", \"sha256\": \"{digest}\"}}"),
    )
    .unwrap();
    fs::write(
        root.join("Cargo.lock"),
        lock(&[("barretenberg-rs", "5.2.1")]),
    )
    .unwrap();

    let pass = run(root, &[]);
    assert_eq!(
        pass.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&pass.stderr)
    );
    assert!(String::from_utf8_lossy(&pass.stdout)
        .contains("content-pins: native-sdk: barretenberg-rs 5.2.1 matches .ci/native-sdk.json"));

    fs::write(
        root.join(".ci/native-sdk.json"),
        format!("{{\"version\": \"5.3.0\", \"sha256\": \"{digest}\"}}"),
    )
    .unwrap();
    let refused = run(root, &[]);
    assert_eq!(
        refused.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(String::from_utf8_lossy(&refused.stderr)
        .contains("Locked barretenberg-rs must exactly match"));

    fs::write(root.join(".ci/native-sdk.json"), b"not json").unwrap();
    let broken = run(root, &[]);
    assert_eq!(
        broken.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&broken.stderr)
    );
}

#[test]
fn content_pins_selects_pins_and_rejects_unknown_names_and_empty_tables() {
    let pins = "[[content_pins]]\nname = 'native-sdk'\nkind = 'lock-version'\nfile = '.ci/native-sdk.json'\npackage = 'barretenberg-rs'\n[[content_pins]]\nname = 'fixtures'\nkind = 'sha256-manifest'\nmanifest = 'tests/fixtures/provenance.json'\nkey = 'sha256'\n";
    let temp = repo_with(pins);
    let root = temp.path();
    let digest = "ab".repeat(32);
    fs::write(
        root.join(".ci/native-sdk.json"),
        format!("{{\"version\": \"5.2.1\", \"sha256\": \"{digest}\"}}"),
    )
    .unwrap();
    fs::write(
        root.join("Cargo.lock"),
        lock(&[("barretenberg-rs", "5.2.1")]),
    )
    .unwrap();
    fs::create_dir_all(root.join("tests/fixtures")).unwrap();
    fs::write(root.join("tests/fixtures/a.json"), b"{}").unwrap();
    let file_digest = ccid::sha256_file(&root.join("tests/fixtures/a.json")).unwrap();
    fs::write(
        root.join("tests/fixtures/provenance.json"),
        format!("{{\"sha256\": {{\"a.json\": \"{file_digest}\"}}}}"),
    )
    .unwrap();

    let both = run(root, &[]);
    assert_eq!(
        both.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&both.stderr)
    );

    let one = run(root, &["--pin", "fixtures"]);
    assert_eq!(
        one.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&one.stderr)
    );
    assert!(String::from_utf8_lossy(&one.stdout).contains("content-pins: fixtures: 1 files match"));

    let unknown = run(root, &["--pin", "nope"]);
    assert_eq!(
        unknown.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&unknown.stderr)
    );

    let empty = repo_with("");
    let none = run(empty.path(), &[]);
    assert_eq!(
        none.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&none.stderr)
    );
    assert!(String::from_utf8_lossy(&none.stderr).contains("no content pins declared"));
}

#[test]
fn content_pins_refuses_escaping_fixture_paths() {
    let pins = "[[content_pins]]\nname = 'fixtures'\nkind = 'sha256-manifest'\nmanifest = 'tests/fixtures/provenance.json'\nkey = 'sha256'\n";
    let temp = repo_with(pins);
    let root = temp.path();
    fs::create_dir_all(root.join("tests/fixtures")).unwrap();
    fs::write(
        root.join("tests/fixtures/provenance.json"),
        b"{\"sha256\": {\"../x\": \"00\"}}",
    )
    .unwrap();
    let refused = run(root, &[]);
    assert_eq!(
        refused.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

use std::{fs, process::Command};

const MANIFEST: &str = r#"schema = 1
project = "advisories-proof"

[advisories]
forbid = ["native-tls", "rustls-pemfile"]

[advisories.floors]
hyper = "1.0.0"
h2 = "0.4.16"
rustls = "0.23.0"
rustls-webpki = "0.103.13"

[[advisories.reviewed]]
id = "RUSTSEC-2024-0388"
package = "derivative"
version = "2.2.0"
"#;

const DENY: &str = r#"[advisories]
ignore = [
    { id = "RUSTSEC-2024-0388", reason = "reviewed" },
]
"#;

fn metadata(packages: &[(&str, &str)]) -> String {
    let list: Vec<_> = packages
        .iter()
        .map(|(name, version)| format!(r#"{{"name":"{name}","version":"{version}"}}"#))
        .collect();
    format!("{{\"packages\":[{}]}}", list.join(","))
}

fn clean_packages() -> String {
    metadata(&[
        ("derivative", "2.2.0"),
        ("hyper", "1.3.1"),
        ("h2", "0.4.16"),
        ("rustls", "0.23.4"),
        ("rustls-webpki", "0.103.13"),
    ])
}

fn repo(deny: Option<&str>, metadata: Option<&str>, manifest: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join(".ci")).unwrap();
    if let Some(deny) = deny {
        fs::write(root.path().join("deny.toml"), deny).unwrap();
    }
    if let Some(metadata) = metadata {
        fs::write(root.path().join("dependency-metadata.json"), metadata).unwrap();
    }
    // Manifests always declare a check; the command under test never runs it.
    let manifest =
        format!("{manifest}\n[checks.placeholder]\nkind = \"commands\"\ncommands = [[\"true\"]]\n");
    fs::write(root.path().join(".ci/ccid.toml"), manifest).unwrap();
    root
}

fn run(root: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args(["advisories", "--repo"])
        .arg(root)
        .args(["--manifest", ".ci/ccid.toml"])
        .output()
        .unwrap()
}

#[test]
fn advisories_cli_passes_a_clean_tree() {
    let root = repo(Some(DENY), Some(&clean_packages()), MANIFEST);
    let output = run(root.path());
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Repaired transport and remaining reviewed advisory versions checked"
    );
}

#[test]
fn advisories_cli_refuses_policy_violations() {
    let cases: Vec<(&str, Option<String>, &str)> = vec![
        (
            "unreviewed",
            Some(metadata(&[("derivative", "2.2.0")])),
            "check-advisory-exposure: Unreviewed advisory exception: RUSTSEC-2099-0001",
        ),
        (
            "obsolete",
            Some(metadata(&[("derivative", "2.3.0")])),
            "check-advisory-exposure: Remove or reassess obsolete exception: RUSTSEC-2024-0388",
        ),
        (
            "forbidden",
            Some(metadata(&[
                ("derivative", "2.2.0"),
                ("native-tls", "0.2.0"),
            ])),
            "check-advisory-exposure: Legacy TLS dependency returned: native-tls",
        ),
        (
            "floor",
            Some(metadata(&[("derivative", "2.2.0"), ("h2", "0.4.15")])),
            "check-advisory-exposure: Version floor violated: h2 0.4.15 < 0.4.16",
        ),
    ];
    for (kind, packages, message) in cases {
        let deny = if kind == "unreviewed" {
            format!("{DENY}    {{ id = \"RUSTSEC-2099-0001\" }},\n]\n")
        } else {
            DENY.to_owned()
        };
        let root = repo(Some(&deny), packages.as_deref(), MANIFEST);
        let output = run(root.path());
        assert_eq!(
            output.status.code(),
            Some(1),
            "{kind}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{kind}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn advisories_cli_fails_closed_on_missing_or_malformed_inputs() {
    // Missing deny.toml.
    let root = repo(None, Some(&clean_packages()), MANIFEST);
    assert_eq!(run(root.path()).status.code(), Some(2));
    // Missing metadata file.
    let root = repo(Some(DENY), None, MANIFEST);
    assert_eq!(run(root.path()).status.code(), Some(2));
    // Unparseable metadata file.
    let root = repo(Some(DENY), Some("not JSON"), MANIFEST);
    assert_eq!(run(root.path()).status.code(), Some(2));
    // Malformed table: unknown keys are refused, never ignored.
    let root = repo(
        Some(DENY),
        Some(&clean_packages()),
        "schema = 1\nproject = \"advisories-proof\"\n[advisories]\nforbid = [\"native-tls\"]\nfloors = {}\nunknown_key = true\n",
    );
    assert_eq!(run(root.path()).status.code(), Some(2));
}

#[test]
fn advisories_cli_runs_without_a_table() {
    // A missing table means no exceptions, never a skipped check.
    let manifest = "schema = 1\nproject = \"advisories-proof\"\n";
    let root = repo(Some("[advisories]\n"), Some("{\"packages\":[]}"), manifest);
    let output = run(root.path());
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let root = repo(
        Some("[advisories]\nignore = [\n    { id = \"RUSTSEC-2024-0388\" },\n]\n"),
        Some("{\"packages\":[]}"),
        manifest,
    );
    assert_eq!(run(root.path()).status.code(), Some(1));
}

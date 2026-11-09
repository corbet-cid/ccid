//! The central release channel (channel.json) must name real releases: a malformed entry would stop every repository that follows the channel.

use serde_json::Value;

fn is_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[test]
fn channel_json_names_full_commit_ids_and_sha256_digests() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("channel.json");
    let channel: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut entries = vec![&channel["default"]];
    if let Some(classes) = channel["classes"].as_object() {
        entries.extend(classes.values());
    }
    assert!(!entries.is_empty());
    for entry in entries {
        let revision = entry["revision"].as_str().unwrap_or_default();
        let digest = entry["sha256"].as_str().unwrap_or_default();
        assert!(
            is_hex(revision, 40),
            "channel revision {revision:?} is not a full commit id"
        );
        assert!(
            is_hex(digest, 64),
            "channel digest {digest:?} is not a sha256"
        );
    }
}

/// Where the runner serves releases (`CCID_TOOL_URL`, the package base): every channel entry must exist there with exactly the digest the channel names. Runs when the
/// runner provides the base (CI on the heavy runner); a workstation without it only checks the format above.
#[test]
fn channel_json_entries_are_published_with_the_named_digest() {
    use sha2::{Digest, Sha256};
    let Some(base) = std::env::var("CCID_TOOL_URL")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        eprintln!("CCID_TOOL_URL is not set: published-digest check skipped");
        let _ = std::fs::write(marker(), "skipped: CCID_TOOL_URL is not set\n");
        return;
    };
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("channel.json");
    let channel: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut entries = vec![&channel["default"]];
    if let Some(classes) = channel["classes"].as_object() {
        entries.extend(classes.values());
    }
    let mut checked = std::collections::BTreeSet::new();
    for entry in entries {
        let revision = entry["revision"].as_str().unwrap();
        let digest = entry["sha256"].as_str().unwrap();
        if !checked.insert((revision.to_owned(), digest.to_owned())) {
            continue;
        }
        let output = std::process::Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--max-time",
                "120",
            ])
            .arg(format!("{base}/{revision}/ccid-x86_64-unknown-linux-gnu"))
            .output()
            .expect("curl runs");
        assert!(
            output.status.success(),
            "release {revision} cannot be fetched from the package registry: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let actual = format!("{:x}", Sha256::digest(&output.stdout));
        assert_eq!(
            actual, digest,
            "release {revision}: the registry's digest differs from the channel's"
        );
    }
    eprintln!(
        "channel.json: {} release(s) fetched and digest-checked",
        checked.len()
    );
}

/// Evidence for operators: a passing test prints nothing in the CI log, so the outcome (checked or skipped) is also written below the target directory.
fn marker() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("channel-check.txt")
}

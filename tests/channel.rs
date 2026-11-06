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

#![forbid(unsafe_code)]

fn main() {
    println!("cargo:rerun-if-env-changed=CCID_SOURCE_REVISION");
    let revision = std::env::var("CCID_SOURCE_REVISION").unwrap_or_else(|_| "unversioned".into());
    assert!(
        revision == "unversioned"
            || revision.len() == 40
                && revision
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "CCID_SOURCE_REVISION must be a full lowercase Git commit"
    );
    println!("cargo:rustc-env=CCID_SOURCE_REVISION={revision}");
}

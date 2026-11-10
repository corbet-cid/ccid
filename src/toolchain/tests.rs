use super::*;

#[test]
fn llvm_tools_accepts_current_and_legacy_shapes() {
    for line in [
        "llvm-tools-x86_64-unknown-linux-gnu",
        "llvm-tools-x86_64-unknown-linux-gnu (installed)",
        "llvm-tools-preview",
        "llvm-tools-preview-x86_64-unknown-linux-gnu",
        "rustfmt-x86_64-unknown-linux-gnu\nllvm-tools-aarch64-apple-darwin",
    ] {
        assert!(has_llvm_tools(line), "{line:?} must be accepted");
    }
}

#[test]
fn llvm_tools_refuses_loose_shapes() {
    for line in [
        "",
        "llvm-tools",
        "llvm-tools-x86_64",
        "llvm-toolsx86_64-unknown-linux-gnu",
        "llvm-tools-preview-x86_64",
        "llvm-tools-previewx",
        "my-llvm-tools-x86_64-unknown-linux-gnu",
        "llvm-tools-X86_64-unknown-linux-gnu",
        "clippy-x86_64-unknown-linux-gnu",
    ] {
        assert!(!has_llvm_tools(line), "{line:?} must be refused");
    }
}

#[test]
fn target_pairs_split_at_the_first_colon() {
    assert_eq!(
        parse_target("stable:wasm32-unknown-unknown").unwrap(),
        ("stable".into(), "wasm32-unknown-unknown".into())
    );
    assert_eq!(
        parse_target("nightly:a:b").unwrap(),
        ("nightly".into(), "a:b".into())
    );
    for bad in ["", "stable", ":x", "stable:"] {
        assert!(parse_target(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn lock_wait_times_out_and_is_released_by_drop() {
    let dir = std::env::temp_dir().join(format!("ccid-tc-unit-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("lock");
    let first = open_lock(&path).unwrap();
    acquire(&first, Mode::Exclusive, Duration::from_secs(1), "t").unwrap();
    let second = open_lock(&path).unwrap();
    let started = Instant::now();
    assert!(acquire(&second, Mode::Shared, Duration::from_millis(300), "t").is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(first);
    acquire(&second, Mode::Shared, Duration::from_secs(1), "t").unwrap();
    drop(second);
    fs::remove_dir_all(&dir).unwrap();
}

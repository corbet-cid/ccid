use super::*;
use std::collections::BTreeSet;
use std::process::Command;
use std::time::Duration;

const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn key(fill: char) -> String {
    std::iter::repeat_n(fill, 64).collect()
}

/// A tiny committed repository whose manifest declares one output-producing check.
fn fixture(outputs: &[&str]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    fs::create_dir_all(path.join("src")).unwrap();
    fs::create_dir_all(path.join(".ci")).unwrap();
    fs::write(
        path.join("Cargo.toml"),
        "[package]\nname='outputs-fixture'\nversion='0.0.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(path.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n").unwrap();
    fs::write(path.join(".gitignore"), ".ccid/\ntarget/\n.moon/\n").unwrap();
    let declared = outputs
        .iter()
        .map(|output| format!("{output:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        path.join(".ci/ccid.toml"),
        format!(
            "schema = 1\nproject = 'outputs'\n[checks.verify]\nkind = 'commands'\ncommands = [['sh', '-c', 'true']]\ncache_tools = [['sh', '--version']]\ncache_outputs = [{declared}]\n"
        ),
    )
    .unwrap();
    git(path, &["init", "--quiet"]);
    git(path, &["add", "."]);
    git(path, &["commit", "--quiet", "-m", "fixture"]);
    root
}

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=CI",
            "-c",
            "user.email=ci@example.invalid",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
        .status
        .success());
}

fn environment(cache: &Path) -> Environment {
    let mut env: Environment = std::env::vars_os().collect();
    for name in [
        "CI",
        "CI_REPOSITORY_URL",
        "CCID_CHANNEL_FILE",
        "CI_COMMIT_BRANCH",
    ] {
        env.remove(std::ffi::OsStr::new(name));
    }
    env.insert("CCID_RESULT_CACHE".into(), cache.as_os_str().to_owned());
    env
}

fn subject<'a>(
    cache: &'a Path,
    root: &'a Path,
    key: &'a str,
    outputs: &'a [String],
) -> Subject<'a> {
    Subject {
        cache,
        root,
        key,
        check: "verify",
        repository: "https://example.invalid/checks/outputs",
        branch: "main",
        commit: COMMIT,
        outputs,
    }
}

#[test]
fn stored_outputs_restore_byte_for_byte_and_deduplicate() {
    let cache = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    fs::create_dir_all(root.join("reports")).unwrap();
    fs::write(root.join("coverage.json"), b"{\"data\":[1,2,3]}\n").unwrap();
    fs::write(root.join("coverage.lcov"), b"TN:\nSF:src/lib.rs\n").unwrap();
    fs::write(root.join("reports/coverage.txt"), b" 90% lines\n").unwrap();
    let outputs = vec![
        "coverage.json".to_owned(),
        "coverage.lcov".to_owned(),
        "reports/coverage.txt".to_owned(),
    ];
    let key_a = key('a');
    let first = subject(cache.path(), root, &key_a, &outputs);
    store(&first, "lane", 10).unwrap();
    let blobs_after_first = fs::read_dir(cache.path().join(DIR).join(BLOB_DIR))
        .unwrap()
        .count();
    assert_eq!(blobs_after_first, 3);
    let index: Index = serde_json::from_slice(
        &fs::read(
            cache
                .path()
                .join(DIR)
                .join(INDEX_DIR)
                .join(format!("{key_a}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(index.key, key_a);
    assert_eq!(index.entries.len(), 3);
    assert_eq!(index.branch, "main");

    // The same bytes under another key share every blob.
    let key_b = key('b');
    let second = subject(cache.path(), root, &key_b, &outputs);
    store(&second, "ci", 11).unwrap();
    assert_eq!(
        fs::read_dir(cache.path().join(DIR).join(BLOB_DIR))
            .unwrap()
            .count(),
        blobs_after_first
    );

    fs::remove_file(root.join("coverage.json")).unwrap();
    fs::remove_file(root.join("coverage.lcov")).unwrap();
    fs::remove_dir_all(root.join("reports")).unwrap();
    restore(&first).unwrap();
    assert_eq!(
        fs::read(root.join("coverage.json")).unwrap(),
        b"{\"data\":[1,2,3]}\n"
    );
    assert_eq!(
        fs::read(root.join("coverage.lcov")).unwrap(),
        b"TN:\nSF:src/lib.rs\n"
    );
    assert_eq!(
        fs::read(root.join("reports/coverage.txt")).unwrap(),
        b" 90% lines\n"
    );

    // A stale file at the declared path is replaced, never merged.
    fs::write(root.join("coverage.lcov"), b"stale\n").unwrap();
    restore(&first).unwrap();
    assert_eq!(
        fs::read(root.join("coverage.lcov")).unwrap(),
        b"TN:\nSF:src/lib.rs\n"
    );
}

#[test]
fn a_directory_output_round_trips_with_modes() {
    let cache = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    fs::create_dir_all(root.join("app/lib")).unwrap();
    fs::write(root.join("app/lib/data.txt"), b"payload\n").unwrap();
    let bin = root.join("app/run");
    fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let outputs = vec!["app".to_owned()];
    let key = key('c');
    let first = subject(cache.path(), root, &key, &outputs);
    let before = directory_bytes(&root.join("app")).unwrap();
    store(&first, "ci", 12).unwrap();
    fs::remove_dir_all(root.join("app")).unwrap();
    restore(&first).unwrap();
    assert_eq!(directory_bytes(&root.join("app")).unwrap(), before);
    assert_eq!(
        fs::read(root.join("app/lib/data.txt")).unwrap(),
        b"payload\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            fs::metadata(root.join("app/run"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
}

#[test]
fn a_corrupt_blob_is_never_restored() {
    let cache = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    fs::write(root.join("coverage.txt"), b"good report\n").unwrap();
    let outputs = vec!["coverage.txt".to_owned()];
    let key_d = key('d');
    let first = subject(cache.path(), root, &key_d, &outputs);
    store(&first, "lane", 13).unwrap();
    let index: Index = serde_json::from_slice(
        &fs::read(
            cache
                .path()
                .join(DIR)
                .join(INDEX_DIR)
                .join(format!("{key_d}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    let blob = cache
        .path()
        .join(DIR)
        .join(BLOB_DIR)
        .join(&index.entries[0].sha256);
    let size = fs::metadata(&blob).unwrap().len();
    fs::write(&blob, vec![b'x'; size as usize]).unwrap();
    fs::remove_file(root.join("coverage.txt")).unwrap();
    assert!(restore(&first).is_err());
}

#[test]
fn retention_keeps_the_newest_and_a_bounded_window_per_repository_and_branch() {
    let cache = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    let outputs = vec!["coverage.txt".to_owned()];
    let mut keys = Vec::new();
    for index in 0..12u64 {
        let key = format!("{index:064x}");
        fs::write(root.join("coverage.txt"), format!("report {index}\n")).unwrap();
        let item = subject(cache.path(), root, &key, &outputs);
        store(&item, "ci", 100 + index).unwrap();
        keys.push(key);
    }
    // A different branch keeps its own newest set.
    let release_key = "f".repeat(64);
    let branch_subject = Subject {
        branch: "release",
        ..subject(cache.path(), root, &release_key, &outputs)
    };
    fs::write(root.join("coverage.txt"), b"release report\n").unwrap();
    store(&branch_subject, "ci", 1).unwrap();

    // Make the blobs look old enough for garbage collection; a fresh blob would be a producer
    // that has not published its index yet and must survive.
    let old = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
    for item in fs::read_dir(cache.path().join(DIR).join(BLOB_DIR)).unwrap() {
        let path = item.unwrap().path();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    let report = prune(cache.path()).unwrap();
    assert_eq!(
        report.kept,
        WINDOW + 1 + 1,
        "newest+window on main, newest on release"
    );
    // The three oldest main sets are gone; the release set stays.
    for key in &keys[..3] {
        assert!(!cache
            .path()
            .join(DIR)
            .join(INDEX_DIR)
            .join(format!("{key}.json"))
            .exists());
    }
    for key in &keys[3..] {
        assert!(cache
            .path()
            .join(DIR)
            .join(INDEX_DIR)
            .join(format!("{key}.json"))
            .exists());
    }
    assert!(report.removed_sets >= 3);
    // Blobs of removed sets are collected, blobs of kept sets survive.
    for key in &keys[3..] {
        assert!(index_path(&cache.path().join(DIR).join(INDEX_DIR), key).is_file());
    }
    assert_eq!(
        fs::read_dir(cache.path().join(DIR).join(BLOB_DIR))
            .unwrap()
            .count(),
        WINDOW + 1 + 1
    );
    // Nothing outside outputs/ was considered.
    fs::create_dir_all(cache.path().join("verdicts")).unwrap();
    fs::write(cache.path().join("verdicts/keep.json"), b"{}").unwrap();
    prune(cache.path()).unwrap();
    assert!(cache.path().join("verdicts/keep.json").is_file());
}

#[test]
fn declared_outputs_must_be_plain_relative_paths_outside_tracked_source() {
    for bad in [
        "",
        "/etc/passwd",
        "../escape",
        ".git/config",
        ".ccid/results/verify.jsonl",
        "target/debug/app",
    ] {
        assert!(output_relative(bad).is_err(), "{bad:?}");
    }
    assert!(output_relative("reports/coverage.json").is_ok());

    // A declared output that is tracked source never names a restore destination.
    let fixture = fixture(&["Cargo.toml"]);
    let env = environment(fixture.path());
    let commit_id = crate::cached_verdict::commit(fixture.path(), None, &env).unwrap();
    let check = crate::Check {
        kind: "commands".into(),
        commands: vec![vec!["sh".into(), "-c".into(), "true".into()]],
        cache_tools: vec![vec!["sh".into(), "--version".into()]],
        cache_outputs: vec!["Cargo.toml".into()],
        ..Default::default()
    };
    assert!(
        crate::cached_verdict::prepare(fixture.path(), "verify", &check, &env, &commit_id).is_err()
    );
}

#[test]
fn a_verdict_is_published_only_after_every_declared_output_is_stored() {
    let repo_fixture = fixture(&["coverage.txt"]);
    let root = repo_fixture.path();
    let cache = tempfile::tempdir().unwrap();
    let env = environment(cache.path());
    let check = crate::Check {
        kind: "commands".into(),
        commands: vec![vec!["sh".into(), "-c".into(), "true".into()]],
        cache_tools: vec![vec!["sh".into(), "--version".into()]],
        cache_outputs: vec!["coverage.txt".into()],
        ..Default::default()
    };
    let commit_id = crate::cached_verdict::commit(root, None, &env).unwrap();
    let prepared =
        crate::cached_verdict::prepare(root, "verify", &check, &env, &commit_id).unwrap();
    let record_path = cache
        .path()
        .join(crate::cached_verdict::STORE_DIR)
        .join(format!("{}.json", prepared.key));
    let producer = crate::cached_verdict::Producer {
        kind: crate::cached_verdict::ProducerKind::Lane,
        identity: Some("test".into()),
    };
    // The check declares a report it did not produce: no verdict may exist, because a future
    // hit could not restore the report.
    crate::cached_verdict::record_success(
        std::slice::from_ref(&prepared),
        &BTreeSet::new(),
        &producer,
        20,
        &env,
    )
    .unwrap();
    assert!(!record_path.exists());
    assert!(!cache
        .path()
        .join(DIR)
        .join(INDEX_DIR)
        .join(format!("{}.json", prepared.key))
        .exists());

    // A complete run publishes both, and a lookup is a hit only when the outputs are there.
    fs::write(root.join("coverage.txt"), b"lines: 90%\n").unwrap();
    crate::cached_verdict::record_success(
        std::slice::from_ref(&prepared),
        &BTreeSet::new(),
        &producer,
        30,
        &env,
    )
    .unwrap();
    assert!(record_path.is_file());
    assert_eq!(
        crate::cached_verdict::lookup_ready(&prepared, &env).status,
        crate::cached_verdict::Status::Hit
    );
    fs::remove_file(root.join("coverage.txt")).unwrap();
    crate::cached_verdict::restore_outputs(&prepared, &env).unwrap();
    assert_eq!(
        fs::read(root.join("coverage.txt")).unwrap(),
        b"lines: 90%\n"
    );

    // A record without an output index (produced by an older release) is not a usable hit.
    let other = fixture(&["coverage.txt"]);
    let other_root = other.path();
    let other_commit = crate::cached_verdict::commit(other_root, None, &env).unwrap();
    let empty_cache = tempfile::tempdir().unwrap();
    let mut other_env = env.clone();
    other_env.insert(
        "CCID_RESULT_CACHE".into(),
        empty_cache.path().as_os_str().to_owned(),
    );
    let other_prepared =
        crate::cached_verdict::prepare(other_root, "verify", &check, &other_env, &other_commit)
            .unwrap();
    crate::cached_verdict::write_record(
        &empty_cache.path().join(crate::cached_verdict::STORE_DIR),
        &crate::cached_verdict::Record {
            schema: crate::cached_verdict::SCHEMA,
            key: other_prepared.key.clone(),
            components: other_prepared.components.clone(),
            commit: other_prepared.commit.clone(),
            repository: "https://example.invalid/checks/outputs".into(),
            result: crate::cached_verdict::ResultWord::Success,
            producer: producer.clone(),
            tool: crate::cached_verdict::own_tool().unwrap(),
            started_at: 1,
            finished_at: 2,
            duration_seconds: 1,
        },
    )
    .unwrap();
    assert_eq!(
        crate::cached_verdict::lookup(&other_prepared, &other_env).status,
        crate::cached_verdict::Status::Hit
    );
    assert_eq!(
        crate::cached_verdict::lookup_ready(&other_prepared, &other_env).status,
        crate::cached_verdict::Status::Miss
    );
    assert!(crate::cached_verdict::restore_outputs(&other_prepared, &other_env).is_err());
}

#[test]
fn a_coverage_toolchain_is_keyed_by_the_nightly_binaries() {
    let fixture = fixture(&[]);
    let root = fixture.path();
    let tools = tempfile::tempdir().unwrap();
    let tool_dir = tools.path();
    let nightly = tool_dir.join("nightly");
    fs::create_dir_all(&nightly).unwrap();
    let rustc = nightly.join("rustc");
    let cargo = nightly.join("cargo");
    let llvm_cov = tool_dir.join("cargo-llvm-cov");
    fs::write(&rustc, b"nightly rustc one\n").unwrap();
    fs::write(&cargo, b"nightly cargo\n").unwrap();
    fs::write(&llvm_cov, b"llvm-cov\n").unwrap();
    let rustup = tool_dir.join("rustup");
    fs::write(
        &rustup,
        format!(
            "#!/bin/sh\n# which --toolchain nightly <program>\ncase \"$4\" in\nrustc) echo {rustc:?} ;;\ncargo) echo {cargo:?} ;;\n*) exit 2 ;;\nesac\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&rustup, &rustc, &cargo, &llvm_cov] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let cache = tempfile::tempdir().unwrap();
    let mut env = environment(cache.path());
    env.insert(
        "PATH".into(),
        std::env::join_paths(
            std::iter::once(tool_dir.to_path_buf())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap(),
    );
    let check = crate::Check {
        kind: "commands".into(),
        commands: vec![vec!["bash".into(), ".ci/verify.sh".into()]],
        cache_tools: vec![
            vec![
                "rustup".into(),
                "run".into(),
                "nightly".into(),
                "rustc".into(),
                "-vV".into(),
            ],
            vec![
                "rustup".into(),
                "run".into(),
                "nightly".into(),
                "cargo".into(),
                "llvm-cov".into(),
                "--version".into(),
            ],
        ],
        ..Default::default()
    };
    let commit_id = crate::cached_verdict::commit(root, None, &env).unwrap();
    let first = crate::cached_verdict::prepare(root, "verify", &check, &env, &commit_id)
        .unwrap()
        .key;
    fs::write(&rustc, b"nightly rustc two\n").unwrap();
    let second = crate::cached_verdict::prepare(root, "verify", &check, &env, &commit_id)
        .unwrap()
        .key;
    assert_ne!(first, second, "the nightly compiler's bytes are keyed");
}

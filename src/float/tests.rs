use super::*;

const CHANNEL: &str = r#"{"fleet":{"schema":1,"forge_hosts":["forge.example","forge.internal.example"],"first_party":["https://forge.example/org/"],"aliases":{"https://mirror.example/org/":"https://forge.example/org/"}}}"#;

fn environment(fleet: Option<&Path>) -> Environment {
    let mut env: Environment = std::env::vars_os().collect();
    for name in [
        "CI_COMMIT_SHA",
        "CI_REPOSITORY_URL",
        fleet::FLEET_FILE_ENV,
        fleet::FORGE_HOSTS_ENV,
        RESOLUTION_ENV,
        LOCKS_ENV,
    ] {
        env.remove(OsStr::new(name));
    }
    if let Some(path) = fleet {
        env.insert(fleet::FLEET_FILE_ENV.into(), path.as_os_str().to_owned());
    }
    env
}

fn channel(directory: &Path) -> PathBuf {
    let path = directory.join("channel.json");
    fs::write(&path, CHANNEL).unwrap();
    path
}

fn package(name: &str, source: &str) -> String {
    format!("[[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\nsource = \"{source}\"\n")
}

fn commit(n: char) -> String {
    n.to_string().repeat(40)
}

fn lock(packages: &[String]) -> Vec<u8> {
    format!("version = 4\n\n{}", packages.join("\n")).into_bytes()
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=float",
            "-c",
            "user.email=float@example.invalid",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A committed repository with the given lock and nothing else that floats.
fn repository(lock_bytes: &[u8]) -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = 'consumer'\nversion = '0.1.0'\nedition = '2021'\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "").unwrap();
    fs::write(root.join("Cargo.lock"), lock_bytes).unwrap();
    git(root, &["init", "-q"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "fixture"]);
    temp
}

#[test]
fn the_digest_names_every_lock_byte_and_every_commit() {
    let first = Resolution::from_locks(BTreeMap::from([(
        "Cargo.lock".to_owned(),
        lock(&[
            package(
                "dep",
                &format!(
                    "git+https://forge.example/org/dep?branch=main#{}",
                    commit('1')
                ),
            ),
            package(
                "serde",
                "registry+https://github.com/rust-lang/crates.io-index",
            ),
        ]),
    )]))
    .unwrap();
    assert_eq!(first.digest.len(), 64);
    assert_eq!(
        first.commits,
        BTreeMap::from([(
            "Cargo.lock:dep:https://forge.example/org/dep".to_owned(),
            commit('1')
        )])
    );
    assert_eq!(
        Resolution::from_locks(first.locks.clone()).unwrap(),
        first,
        "the same locks are the same resolution"
    );
    // A third-party byte the commit map cannot see still moves the digest: the key names the
    // exact locks the checks build, not only the first-party commits.
    let mut third_party = first.locks.clone();
    third_party
        .get_mut("Cargo.lock")
        .unwrap()
        .extend(b"\n[[package]]\nname = \"forced\"\nversion = \"1.0.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n");
    let moved = Resolution::from_locks(third_party).unwrap();
    assert_eq!(moved.commits, first.commits);
    assert_ne!(moved.digest, first.digest);
    // A moved commit moves the digest and the commit map.
    let next = Resolution::from_locks(BTreeMap::from([(
        "Cargo.lock".to_owned(),
        lock(&[package(
            "dep",
            &format!(
                "git+https://forge.example/org/dep?branch=main#{}",
                commit('2')
            ),
        )]),
    )]))
    .unwrap();
    assert_ne!(next.digest, first.digest);
    assert_ne!(next.commits, first.commits);
}

#[test]
fn selection_is_branch_tracking_first_party_and_never_the_repository_host() {
    let temp = tempfile::tempdir().unwrap();
    let file = channel(temp.path());
    let committed = BTreeMap::from([
        (
            "Cargo.lock".to_owned(),
            lock(&[
                package(
                    "canonical",
                    &format!(
                        "git+https://forge.example/org/canonical?branch=main#{}",
                        commit('1')
                    ),
                ),
                package(
                    "spelled",
                    &format!(
                        "git+https://forge.internal.example/org/spelled?branch=main#{}",
                        commit('2')
                    ),
                ),
                package(
                    "legacy",
                    &format!(
                        "git+https://mirror.example/org/legacy.git?branch=main#{}",
                        commit('3')
                    ),
                ),
                package(
                    "foreign",
                    &format!(
                        "git+https://forge.example/other/foreign?branch=main#{}",
                        commit('4')
                    ),
                ),
                package(
                    "pinned",
                    &format!(
                        "git+https://forge.example/org/pinned?rev={0}#{0}",
                        commit('5')
                    ),
                ),
                package(
                    "serde",
                    "registry+https://github.com/rust-lang/crates.io-index",
                ),
            ]),
        ),
        (
            "tools/xtask/Cargo.lock".to_owned(),
            lock(&[package(
                "canonical",
                &format!(
                    "git+https://forge.example/org/canonical?branch=main#{}",
                    commit('1')
                ),
            )]),
        ),
    ]);
    let expected = BTreeMap::from([
        (
            "Cargo.lock".to_owned(),
            BTreeSet::from([
                "git+https://forge.example/org/canonical?branch=main#canonical@0.1.0".to_owned(),
                "git+https://forge.internal.example/org/spelled?branch=main#spelled@0.1.0"
                    .to_owned(),
                "git+https://mirror.example/org/legacy.git?branch=main#legacy@0.1.0".to_owned(),
            ]),
        ),
        (
            "tools/xtask/Cargo.lock".to_owned(),
            BTreeSet::from([
                "git+https://forge.example/org/canonical?branch=main#canonical@0.1.0".to_owned(),
            ]),
        ),
    ]);
    let mut env = environment(Some(&file));
    let (selected, _) = select(&committed, &env).unwrap().unwrap();
    assert_eq!(selected, expected);
    // The forge spelling of the CI checkout never decides anything.
    for url in [
        "https://forge.internal.example/org/consumer",
        "https://forge.example/org/consumer",
        "https://elsewhere.example/someone/consumer",
    ] {
        env.insert("CI_REPOSITORY_URL".into(), url.into());
        assert_eq!(
            select(&committed, &env).unwrap().unwrap().0,
            expected,
            "{url}"
        );
    }
}

#[test]
fn a_branch_tracking_lock_without_fleet_data_is_an_error_naming_the_fleet_file() {
    let committed = BTreeMap::from([(
        "Cargo.lock".to_owned(),
        lock(&[package(
            "foreign",
            &format!(
                "git+https://forge.example/other/foreign?branch=main#{}",
                commit('4')
            ),
        )]),
    )]);
    let error = select(&committed, &environment(None))
        .unwrap_err()
        .to_string();
    assert!(error.contains(fleet::FLEET_FILE_ENV), "{error}");
}

#[test]
fn locks_without_a_branch_tracking_package_need_no_fleet_data_and_float_nothing() {
    let committed = BTreeMap::from([(
        "Cargo.lock".to_owned(),
        lock(&[
            package(
                "pinned",
                &format!(
                    "git+https://forge.example/org/pinned?rev={0}#{0}",
                    commit('5')
                ),
            ),
            package(
                "serde",
                "registry+https://github.com/rust-lang/crates.io-index",
            ),
        ]),
    )]);
    assert!(select(&committed, &environment(None)).unwrap().is_none());
    // Branch-tracking packages that are all third-party float nothing either.
    let temp = tempfile::tempdir().unwrap();
    let file = channel(temp.path());
    let foreign = BTreeMap::from([(
        "Cargo.lock".to_owned(),
        lock(&[package(
            "foreign",
            &format!(
                "git+https://forge.example/other/foreign?branch=main#{}",
                commit('4')
            ),
        )]),
    )]);
    assert!(select(&foreign, &environment(Some(&file)))
        .unwrap()
        .is_none());
}

#[test]
fn a_repository_without_a_float_keys_and_runs_exactly_as_before() {
    let repo = repository(&lock(&[package(
        "pinned",
        &format!(
            "git+https://forge.example/org/pinned?rev={0}#{0}",
            commit('5')
        ),
    )]));
    let before = environment(None);
    let mut env = before.clone();
    assert!(establish(repo.path(), &mut env, false).unwrap().is_none());
    assert_eq!(env, before, "no digest and no locks are handed on");
    // Nothing is bound, so every check declaration (and with it every key) is unchanged.
    let mut checks = BTreeMap::from([("verify".to_owned(), crate::Check::default())]);
    let declared = checks["verify"].cache_env.clone();
    if establish(repo.path(), &mut env, false).unwrap().is_some() {
        bind(&mut checks, &["verify".to_owned()]);
    }
    assert_eq!(checks["verify"].cache_env, declared);
    assert_eq!(
        git(
            repo.path(),
            &["status", "--porcelain", "--untracked-files=all"]
        ),
        ""
    );
    // A nested run of this repository inside another repository's CI job inherits that job's
    // CI_COMMIT_SHA: the float reads this checkout's own HEAD and never asks for that commit.
    let mut nested = before.clone();
    nested.insert("CI_COMMIT_SHA".into(), commit('f').into());
    assert!(establish(repo.path(), &mut nested, false)
        .unwrap()
        .is_none());
    // An export (no repository, no commit) is read from its files.
    let export = tempfile::tempdir().unwrap();
    fs::write(export.path().join("Cargo.lock"), lock(&[])).unwrap();
    assert!(establish(export.path(), &mut before.clone(), false)
        .unwrap()
        .is_none());
}

#[test]
fn bind_appends_the_digest_once_after_every_declared_variable() {
    let mut checks = BTreeMap::from([
        (
            "verify".to_owned(),
            crate::Check {
                cache_env: vec!["RUSTFLAGS".to_owned(), fleet::FLEET_DIGEST_ENV.to_owned()],
                ..Default::default()
            },
        ),
        ("other".to_owned(), crate::Check::default()),
    ]);
    bind(&mut checks, &["verify".to_owned()]);
    bind(&mut checks, &["verify".to_owned()]);
    assert_eq!(
        checks["verify"].cache_env,
        ["RUSTFLAGS", fleet::FLEET_DIGEST_ENV, RESOLUTION_ENV]
    );
    assert!(checks["other"].cache_env.is_empty(), "only the selection");
    // The executor's declaration (the moon key) carries it exactly like the verdict's.
    assert_eq!(
        crate::cached::executor_check(&checks["verify"]).cache_env,
        checks["verify"].cache_env
    );
}

#[test]
fn a_child_verifies_the_hand_over_and_refuses_a_tampered_or_missing_one() {
    let resolved = lock(&[package(
        "dep",
        &format!(
            "git+https://forge.example/org/dep?branch=main#{}",
            commit('2')
        ),
    )]);
    let repo = repository(&lock(&[package(
        "dep",
        &format!(
            "git+https://forge.example/org/dep?branch=main#{}",
            commit('1')
        ),
    )]));
    let resolution =
        Resolution::from_locks(BTreeMap::from([("Cargo.lock".to_owned(), resolved)])).unwrap();
    let hand_over = tempfile::tempdir().unwrap();
    resolution.write_into(hand_over.path()).unwrap();
    let mut env = environment(None);
    env.insert(RESOLUTION_ENV.into(), resolution.digest.clone().into());
    env.insert(LOCKS_ENV.into(), hand_over.path().as_os_str().to_owned());
    // A moon task and a nested run take the hand-over as is: no fleet data, no Cargo.
    for child in [true, false] {
        let mut copy = env.clone();
        let handover = establish(repo.path(), &mut copy, child).unwrap().unwrap();
        assert_eq!(handover.resolution, resolution);
        assert_eq!(copy, env);
    }
    // One changed byte is another resolution than the one keyed.
    let file = hand_over.path().join("Cargo.lock");
    let mut bytes = fs::read(&file).unwrap();
    bytes.extend(b"\n");
    fs::write(&file, bytes).unwrap();
    let error = establish(repo.path(), &mut env.clone(), true)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains(&resolution.digest), "{error}");
    // A digest without its locks: the run cannot build what it was keyed with.
    let mut missing = env.clone();
    missing.remove(OsStr::new(LOCKS_ENV));
    let error = establish(repo.path(), &mut missing, true)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains(LOCKS_ENV), "{error}");
    // Locks without a digest are refused too.
    let mut orphan = env.clone();
    orphan.remove(OsStr::new(RESOLUTION_ENV));
    assert!(establish(repo.path(), &mut orphan, false).is_err());
    // A link in place of a lock is never followed.
    fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(repo.path().join("Cargo.lock"), &file).unwrap();
    assert!(establish(repo.path(), &mut env.clone(), true).is_err());
}

#[test]
fn the_stage_lists_the_committed_tree_and_the_real_commit_without_writing_the_checkout() {
    let repo = repository(&lock(&[package(
        "dep",
        &format!(
            "git+https://forge.example/org/dep?branch=main#{}",
            commit('1')
        ),
    )]));
    let root = repo.path();
    let head = git(root, &["rev-parse", "HEAD"]);
    let listed = git(root, &["ls-files"]);
    let staged = tempfile::tempdir().unwrap();
    cache::copy_source(root, staged.path(), true).unwrap();
    let resolution = Resolution::from_locks(BTreeMap::from([(
        "Cargo.lock".to_owned(),
        lock(&[package(
            "dep",
            &format!(
                "git+https://forge.example/org/dep?branch=main#{}",
                commit('2')
            ),
        )]),
    )]))
    .unwrap();
    stage(root, staged.path(), &resolution).unwrap();
    assert_eq!(git(staged.path(), &["rev-parse", "HEAD"]), head);
    assert_eq!(git(staged.path(), &["ls-files"]), listed);
    assert_eq!(
        fs::read(staged.path().join("Cargo.lock")).unwrap(),
        resolution.locks["Cargo.lock"]
    );
    // The overlaid lock is the stage's one difference from the commit.
    assert_eq!(
        git(staged.path(), &["status", "--porcelain"]),
        "M Cargo.lock"
    );
    // The checkout is untouched, its repository included: no worktree is registered there.
    assert_eq!(
        git(root, &["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
    assert!(!root.join(".git/worktrees").exists());
    assert_ne!(
        fs::read(root.join("Cargo.lock")).unwrap(),
        resolution.locks["Cargo.lock"]
    );
    // A stage already holding repository metadata is never overwritten.
    assert!(attach_git(root, staged.path(), &head).is_err());
}

#[test]
fn third_party_entries_that_differ_are_reported_and_first_party_moves_are_not() {
    let temp = tempfile::tempdir().unwrap();
    let file = channel(temp.path());
    let (fleet, _) = fleet::load(&environment(Some(&file))).unwrap();
    let registry = "registry+https://github.com/rust-lang/crates.io-index";
    let before = BTreeMap::from([(
        "Cargo.lock".to_owned(),
        lock(&[
            package(
                "dep",
                &format!(
                    "git+https://forge.example/org/dep?branch=main#{}",
                    commit('1')
                ),
            ),
            package("kept", registry),
            package("dropped", registry),
        ]),
    )]);
    let after = BTreeMap::from([(
        "Cargo.lock".to_owned(),
        lock(&[
            package(
                "dep",
                &format!(
                    "git+https://forge.example/org/dep?branch=main#{}",
                    commit('2')
                ),
            ),
            package("kept", registry),
            package("forced", registry),
        ]),
    )]);
    let changes = third_party_changes(&before, &after, &fleet).unwrap();
    let names: Vec<(&str, &str)> = changes
        .iter()
        .map(|change| {
            (
                change["name"].as_str().unwrap(),
                change["change"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(names, [("dropped", "removed"), ("forced", "added")]);
    assert!(third_party_changes(&before, &before, &fleet)
        .unwrap()
        .is_empty());
}

#[test]
fn a_receipt_names_moved_staged_locks() {
    let temp = tempfile::tempdir().unwrap();
    let results = temp.path().join(".ccid/results");
    fs::create_dir_all(&results).unwrap();
    assert!(!receipt_reports_moved_locks(temp.path(), "verify"));
    fs::write(
        results.join("verify.jsonl"),
        format!(
            "{}\n{}\n",
            json!({"event":"check-start","check":"verify"}),
            json!({"event": MOVED_EVENT, "locks": ["Cargo.lock"]})
        ),
    )
    .unwrap();
    assert!(receipt_reports_moved_locks(temp.path(), "verify"));
    assert!(!receipt_reports_moved_locks(temp.path(), "other"));
}

#[test]
fn lock_paths_must_stay_inside_the_directory_they_are_written_to() {
    for path in ["../Cargo.lock", "/Cargo.lock", "", "a/../../Cargo.lock"] {
        assert!(relative(path).is_err(), "{path:?}");
    }
    assert_eq!(
        relative("tools/xtask/Cargo.lock").unwrap(),
        PathBuf::from("tools/xtask/Cargo.lock")
    );
    let temp = tempfile::tempdir().unwrap();
    let inside = temp.path().join("repo/out");
    fs::create_dir_all(temp.path().join("repo")).unwrap();
    let root = temp.path().join("repo").canonicalize().unwrap();
    assert!(resolved_destination(&inside).unwrap().starts_with(&root));
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&root, &link).unwrap();
    assert!(
        resolved_destination(&link.join("new/dir"))
            .unwrap()
            .starts_with(&root),
        "a link into the repository is resolved before the comparison"
    );
}

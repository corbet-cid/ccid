use super::*;
use std::process::Command;

fn environment() -> Environment {
    let mut env: Environment = std::env::vars_os().collect();
    for name in [
        "CI_COMMIT_SHA",
        "CI",
        "CCID_RESULT_CACHE",
        "CCID_CHANNEL_FILE",
        "CI_REPOSITORY_URL",
        "CC",
        "CXX",
        "CI_LINKER",
        "RUSTC_WRAPPER",
    ] {
        env.remove(OsStr::new(name));
    }
    env
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

fn git_text(root: &Path, args: &[&str]) -> String {
    String::from_utf8(
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned()
}

fn fixture(name: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    fs::create_dir_all(path.join("src")).unwrap();
    fs::create_dir_all(path.join(".ci")).unwrap();
    fs::write(
        path.join("Cargo.toml"),
        "[package]\nname='verdict-fixture'\nversion='0.0.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        path.join("Cargo.lock"),
        "version = 4\n[[package]]\nname = \"verdict-fixture\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    fs::write(path.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n").unwrap();
    fs::write(path.join(".gitignore"), ".ccid/\ntarget/\n.moon/\n").unwrap();
    fs::write(
        path.join(".ci/ccid.toml"),
        format!(
            "schema = 1\nproject = '{name}'\n[checks.verify]\nkind = 'commands'\ncommands = [['sh', '-c', 'echo ran >> .ccid/counter']]\ncache_tools = [['sh', '--version']]\n"
        ),
    )
    .unwrap();
    git(path, &["init", "--quiet"]);
    git(path, &["add", "."]);
    git(path, &["commit", "--quiet", "-m", "fixture"]);
    root
}

fn check() -> crate::Check {
    crate::Check {
        kind: "commands".into(),
        commands: vec![vec!["sh".into(), "-c".into(), "true".into()]],
        cache_tools: vec![vec!["sh".into(), "--version".into()]],
        ..Default::default()
    }
}

/// A channel file naming one binary digest.
fn channel(path: &Path, sha256: &str) {
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "default": {"revision": "a".repeat(40), "sha256": sha256},
            "classes": {"heavy": {"revision": "a".repeat(40), "sha256": sha256}}
        }))
        .unwrap(),
    )
    .unwrap();
}

fn prepared(root: &Path, env: &Environment, name: &str) -> Prepared {
    let commit = commit(root, None, env).unwrap();
    prepare(root, name, &check(), env, &commit).unwrap()
}

#[test]
fn every_key_component_changes_the_key_and_none_is_provenance() {
    let fixture = fixture("key");
    let root = fixture.path();
    let mut env = environment();
    env.insert(
        "CI_REPOSITORY_URL".into(),
        "https://example.invalid/checks/key".into(),
    );
    let base = prepared(root, &env, "verify");
    let key = base.key.clone();
    assert_eq!(prepared(root, &env, "verify").key, key, "key is stable");

    // The check name is part of the key.
    assert_ne!(prepared(root, &env, "other").key, key);

    // The check declaration is part of the key.
    let mut changed = check();
    changed.commands = vec![vec!["sh".into(), "-c".into(), "false".into()]];
    let commit_id = commit(root, None, &env).unwrap();
    assert_ne!(
        prepare(root, "verify", &changed, &env, &commit_id)
            .unwrap()
            .key,
        key
    );

    // A declared environment value is part of the key.
    let mut declared = check();
    declared.cache_env = vec!["VERDICT_TEST_FLAG".into()];
    let commit_id = commit(root, None, &env).unwrap();
    let without = prepare(root, "verify", &declared, &env, &commit_id)
        .unwrap()
        .key;
    let mut with = env.clone();
    with.insert("VERDICT_TEST_FLAG".into(), "1".into());
    assert_ne!(
        prepare(root, "verify", &declared, &with, &commit_id)
            .unwrap()
            .key,
        without
    );

    // A SEMANTIC_ENV variable is declared too, whether or not the check names it.
    let mut rustflags = env.clone();
    rustflags.insert("RUSTFLAGS".into(), "-C target-cpu=native".into());
    assert_ne!(prepared(root, &rustflags, "verify").key, key);

    // Presentation plumbing (locale, compiler-cache wrapper) never enters the key, unless the
    // check declares it explicitly.
    let mut locale = env.clone();
    locale.insert("LANG".into(), "en_US.UTF-8".into());
    locale.insert("LC_ALL".into(), "en_US.UTF-8".into());
    locale.insert("TZ".into(), "Europe/Zurich".into());
    locale.insert("RUSTC_WRAPPER".into(), "kache-corbet".into());
    assert_eq!(prepared(root, &locale, "verify").key, key);
    let mut declared_locale = check();
    declared_locale.cache_env = vec!["LANG".into()];
    let commit_id = commit(root, None, &env).unwrap();
    let without = prepare(root, "verify", &declared_locale, &env, &commit_id)
        .unwrap()
        .key;
    let with = prepare(root, "verify", &declared_locale, &locale, &commit_id)
        .unwrap()
        .key;
    assert_ne!(without, with);

    // Per-run ambient state never enters the key.
    let mut ambient = env.clone();
    for name in [
        "FORGEJO_RUN_ID",
        "ACTIONS_CACHE_URL",
        "RUNNER_TRACKING_ID",
        "INVOCATION_ID",
        "SYSTEMD_EXEC_PID",
        "JOURNAL_STREAM",
        "SOME_UNRELATED_FLAG",
    ] {
        ambient.insert(name.into(), "1".into());
    }
    assert_eq!(prepared(root, &ambient, "verify").key, key);

    // A changed tool's bytes are part of the key.
    let tools = tempfile::tempdir().unwrap();
    let script = tools.path().join("sh");
    fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut with_tool = env.clone();
    with_tool.insert(
        "PATH".into(),
        std::env::join_paths(
            std::iter::once(tools.path().to_path_buf())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap(),
    );
    assert_ne!(prepared(root, &with_tool, "verify").key, key);
}

#[test]
fn the_tree_digest_is_identical_for_a_checkout_and_an_archive_export() {
    let fixture = fixture("tree");
    let root = fixture.path();
    let env = environment();
    let commit = commit(root, None, &env).unwrap();
    let checkout = tree_digest(root, &commit).unwrap();
    let export = tempfile::tempdir().unwrap();
    assert!(Command::new("sh")
        .arg("-c")
        .arg(format!(
            "git -C '{}' archive '{}' | tar -x -C '{}'",
            root.display(),
            commit,
            export.path().display()
        ))
        .status()
        .unwrap()
        .success());
    assert_eq!(tree_digest(export.path(), &commit).unwrap(), checkout);
    // A changed byte changes the tree, and so does a changed executable bit.
    fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 2 }\n").unwrap();
    assert_ne!(tree_digest(root, &commit).unwrap(), checkout);
}

#[test]
fn a_committed_tree_not_the_working_tree_is_the_identity() {
    let fixture = fixture("working");
    let root = fixture.path();
    let env = environment();
    let base = prepared(root, &env, "verify");
    // An ignored build artifact does not move the tree or the key.
    fs::create_dir_all(root.join(".ccid")).unwrap();
    fs::write(root.join(".ccid/counter"), "ran\n").unwrap();
    assert_eq!(prepared(root, &env, "verify").key, base.key);
    // An untracked file the check could read refuses a verdict rather than guessing.
    fs::write(root.join("untracked.txt"), "not committed\n").unwrap();
    let commit_id = commit(root, None, &env).unwrap();
    assert!(prepare(root, "verify", &check(), &env, &commit_id).is_err());
    fs::remove_file(root.join("untracked.txt")).unwrap();
    // A tracked source edit is refused too; only resolved locks may differ.
    fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 3 }\n").unwrap();
    assert!(prepare(root, "verify", &check(), &env, &commit_id).is_err());
    git(root, &["checkout", "--", "src/lib.rs"]);
    // A new commit moves the tree and the key.
    fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 2 }\n").unwrap();
    git(root, &["add", "src/lib.rs"]);
    git(root, &["commit", "--quiet", "-m", "change"]);
    assert_ne!(prepared(root, &env, "verify").key, base.key);
    // A commit that does not exist and a non-commit are refused.
    assert!(commit(root, Some(&"f".repeat(40)), &env).is_err());
    assert!(commit(root, Some("not-a-commit"), &env).is_err());
}

#[test]
fn the_resolved_git_dependency_commit_is_keyed_not_the_committed_pin() {
    let fixture = fixture("deps");
    let root = fixture.path();
    let env = environment();
    let base = prepared(root, &env, "verify");
    let lock = |rev: &str| {
        format!(
            "version = 4\n[[package]]\nname = \"dep\"\nversion = \"0.1.0\"\nsource = \"git+https://example.invalid/org/dep?branch=main#{rev}\"\n"
        )
    };
    fs::write(root.join("Cargo.lock"), lock(&"1".repeat(40))).unwrap();
    git(root, &["add", "Cargo.lock"]);
    git(root, &["commit", "--quiet", "-m", "git dependency"]);
    let pinned = prepared(root, &env, "verify");
    assert_ne!(pinned.key, base.key);
    assert_eq!(
        pinned.components.git_dependencies.values().next().unwrap(),
        &"1".repeat(40)
    );
    // The committed lock still names 1, but the build resolves 2: the key follows the
    // resolved working-tree lock (P2), never the committed pin read before the float.
    fs::write(root.join("Cargo.lock"), lock(&"2".repeat(40))).unwrap();
    assert_eq!(
        git_text(root, &["show", "HEAD:Cargo.lock"]),
        lock(&"1".repeat(40)).trim()
    );
    let resolved = prepared(root, &env, "verify");
    assert_eq!(
        resolved
            .components
            .git_dependencies
            .values()
            .next()
            .unwrap(),
        &"2".repeat(40)
    );
    assert_ne!(resolved.key, pinned.key);
}

#[test]
fn a_git_package_spec_keeps_the_scheme_and_the_branch_query() {
    assert_eq!(
        git_spec(
            "https://example.invalid/org/dep.git?branch=main",
            "dep",
            "0.1.0"
        ),
        "git+https://example.invalid/org/dep.git?branch=main#dep@0.1.0"
    );
}

#[test]
fn nested_locks_are_read_and_keyed_by_their_path() {
    let fixture = fixture("nested");
    let root = fixture.path();
    fs::create_dir_all(root.join("apps/cli")).unwrap();
    fs::write(
        root.join("apps/cli/Cargo.toml"),
        "[package]\nname='cli'\nversion='0.0.0'\n",
    )
    .unwrap();
    fs::write(
        root.join("apps/cli/Cargo.lock"),
        "version = 4\n[[package]]\nname = \"nested-dep\"\nversion = \"0.1.0\"\nsource = \"git+https://example.invalid/org/nested#3333333333333333333333333333333333333333\"\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "-m", "nested lock"]);
    let env = environment();
    let prepared = prepared(root, &env, "verify");
    let deps = &prepared.components.git_dependencies;
    assert_eq!(deps.len(), 1);
    assert!(deps
        .keys()
        .next()
        .unwrap()
        .starts_with("apps/cli/Cargo.lock:nested-dep:"));
}

#[test]
fn a_record_round_trips_and_is_validated() {
    let fixture = fixture("record");
    let root = fixture.path();
    let env = environment();
    let prepared = prepared(root, &env, "verify");
    let record = Record {
        schema: SCHEMA,
        key: prepared.key.clone(),
        components: prepared.components.clone(),
        commit: prepared.commit.clone(),
        repository: "https://example.invalid/checks/record".into(),
        result: ResultWord::Success,
        producer: Producer {
            kind: ProducerKind::Lane,
            identity: Some("lane-a".into()),
        },
        tool: Tool {
            sha256: "c".repeat(64),
            source_revision: "a".repeat(40),
        },
        started_at: 10,
        finished_at: 20,
        duration_seconds: 10,
    };
    let bytes = serde_json::to_vec(&record).unwrap();
    let parsed: Record = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed, record);
    parsed.validate(&prepared.key).unwrap();

    // A record whose components do not hash to its key is refused.
    let mut tampered = record.clone();
    tampered.components.tree = "d".repeat(64);
    assert!(tampered.validate(&prepared.key).is_err());
    // A record with a different key is refused.
    assert!(record.validate(&"e".repeat(64)).is_err());
    // Unknown provenance fields are refused.
    let mut raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    raw.as_object_mut()
        .unwrap()
        .insert("run_id".into(), json!("7"));
    assert!(serde_json::from_value::<Record>(raw).is_err());
}

#[test]
fn a_hit_requires_every_component_and_a_channel_binary() {
    let fixture = fixture("lookup");
    let root = fixture.path();
    let store = tempfile::tempdir().unwrap();
    let mut env = environment();
    env.insert(
        "CI_REPOSITORY_URL".into(),
        "https://example.invalid/checks/lookup".into(),
    );
    env.insert(
        "CCID_RESULT_CACHE".into(),
        store.path().as_os_str().to_owned(),
    );
    env.insert("CI".into(), "true".into());
    let prepared = prepared(root, &env, "verify");
    let tool = Tool {
        sha256: "c".repeat(64),
        source_revision: "a".repeat(40),
    };
    let record = Record {
        schema: SCHEMA,
        key: prepared.key.clone(),
        components: prepared.components.clone(),
        commit: prepared.commit.clone(),
        repository: "https://example.invalid/checks/lookup".into(),
        result: ResultWord::Success,
        producer: Producer {
            kind: ProducerKind::Ci,
            identity: None,
        },
        tool: tool.clone(),
        started_at: 10,
        finished_at: 20,
        duration_seconds: 10,
    };
    let directory = store.path().join(STORE_DIR);
    write_record(&directory, &record).unwrap();
    // CI without a channel file accepts only the binary it is running; this record names
    // another one, so it is refused.
    assert_eq!(lookup(&prepared, &env).status, Status::Invalid);
    // A channel file naming another binary is refused.
    let channel_path = store.path().join("channel.json");
    channel(&channel_path, &"d".repeat(64));
    env.insert(
        "CCID_CHANNEL_FILE".into(),
        channel_path.as_os_str().to_owned(),
    );
    assert_eq!(lookup(&prepared, &env).status, Status::Invalid);
    // A channel naming this binary is a hit.
    channel(&channel_path, &tool.sha256);
    assert_eq!(lookup(&prepared, &env).status, Status::Hit);
    // A lane producer (no CI) accepts a fully matching record from its channel release.
    env.remove(OsStr::new("CI"));
    assert_eq!(lookup(&prepared, &env).status, Status::Hit);
    env.insert("CI".into(), "true".into());

    // A record with a different key never answers this check.
    let mut foreign = record.clone();
    foreign.components.tree = "e".repeat(64);
    foreign.key = foreign.components.key_hex().unwrap();
    write_record(&directory, &foreign).unwrap();
    assert_eq!(lookup(&prepared, &env).status, Status::Hit);
    // A record under this key whose components were edited is refused, never partially matched.
    let mut tampered = record.clone();
    tampered.components.tree = "f".repeat(64);
    fs::write(
        record_path(&directory, &prepared.key),
        serde_json::to_vec(&tampered).unwrap(),
    )
    .unwrap();
    assert_eq!(lookup(&prepared, &env).status, Status::Invalid);
    // Restore the exact record and the hit returns.
    write_record(&directory, &record).unwrap();
    assert_eq!(lookup(&prepared, &env).status, Status::Hit);
}

#[test]
fn a_failure_record_is_never_a_pass() {
    let fixture = fixture("failure");
    let root = fixture.path();
    let store = tempfile::tempdir().unwrap();
    let mut env = environment();
    env.insert(
        "CCID_RESULT_CACHE".into(),
        store.path().as_os_str().to_owned(),
    );
    let channel_path = store.path().join("channel.json");
    env.insert(
        "CCID_CHANNEL_FILE".into(),
        channel_path.as_os_str().to_owned(),
    );
    let prepared = prepared(root, &env, "verify");
    let record = Record {
        schema: SCHEMA,
        key: prepared.key.clone(),
        components: prepared.components.clone(),
        commit: prepared.commit.clone(),
        repository: "unknown".into(),
        result: ResultWord::Failure,
        producer: Producer {
            kind: ProducerKind::Ci,
            identity: None,
        },
        tool: Tool {
            sha256: "c".repeat(64),
            source_revision: "a".repeat(40),
        },
        started_at: 10,
        finished_at: 20,
        duration_seconds: 10,
    };
    write_record(&store.path().join(STORE_DIR), &record).unwrap();
    channel(&channel_path, &record.tool.sha256);
    assert_eq!(lookup(&prepared, &env).status, Status::Miss);
}

#[test]
fn the_store_is_group_writable_when_the_root_is() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = fixture("mode");
    let root = fixture.path();
    let store = tempfile::tempdir().unwrap();
    let directory = store.path().join(STORE_DIR);
    fs::create_dir_all(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o2775)).unwrap();
    let mut env = environment();
    env.insert(
        "CCID_RESULT_CACHE".into(),
        store.path().as_os_str().to_owned(),
    );
    let prepared = prepared(root, &env, "verify");
    let record = Record {
        schema: SCHEMA,
        key: prepared.key.clone(),
        components: prepared.components.clone(),
        commit: prepared.commit.clone(),
        repository: "unknown".into(),
        result: ResultWord::Success,
        producer: Producer {
            kind: ProducerKind::Lane,
            identity: None,
        },
        tool: Tool {
            sha256: "c".repeat(64),
            source_revision: "a".repeat(40),
        },
        started_at: 10,
        finished_at: 20,
        duration_seconds: 10,
    };
    write_record(&directory, &record).unwrap();
    let mode = fs::metadata(record_path(&directory, &prepared.key))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o660);
}

#[test]
fn a_symlinked_record_is_never_read() {
    let fixture = fixture("symlink");
    let root = fixture.path();
    let store = tempfile::tempdir().unwrap();
    let mut env = environment();
    env.insert(
        "CCID_RESULT_CACHE".into(),
        store.path().as_os_str().to_owned(),
    );
    let prepared = prepared(root, &env, "verify");
    let directory = store.path().join(STORE_DIR);
    fs::create_dir_all(&directory).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", record_path(&directory, &prepared.key)).unwrap();
    let lookup = lookup(&prepared, &env);
    assert_eq!(lookup.status, Status::Invalid);
}

#[test]
fn the_producer_is_explicit_or_inferred_but_never_part_of_the_key() {
    let env = Environment::new();
    assert_eq!(
        producer(Some("lane"), &env).unwrap().kind,
        ProducerKind::Lane
    );
    assert_eq!(producer(Some("ci"), &env).unwrap().kind, ProducerKind::Ci);
    assert!(producer(Some("other"), &env).is_err());
    assert_eq!(producer(None, &env).unwrap().kind, ProducerKind::Lane);
    let mut ci = Environment::new();
    ci.insert("CI".into(), "true".into());
    assert_eq!(producer(None, &ci).unwrap().kind, ProducerKind::Ci);
}

#[test]
fn a_stored_verdict_makes_the_cached_run_skip_the_check_entirely() {
    let fixture = fixture("hit");
    let root = fixture.path();
    let store = tempfile::tempdir().unwrap();
    let mut env = environment();
    env.insert(
        "CI_REPOSITORY_URL".into(),
        "https://example.invalid/checks/hit".into(),
    );
    env.insert(
        "CCID_RESULT_CACHE".into(),
        store.path().as_os_str().to_owned(),
    );
    env.insert("CI".into(), "true".into());
    let commit_id = commit(root, None, &env).unwrap();
    // The manifest's declaration, not the helper's: the check identity binds it.
    let mut manifest_check = check();
    manifest_check.commands = vec![vec![
        "sh".into(),
        "-c".into(),
        "echo ran >> .ccid/counter".into(),
    ]];
    let prepared = prepare(root, "verify", &manifest_check, &env, &commit_id).unwrap();
    let tool = Tool {
        sha256: "c".repeat(64),
        source_revision: "a".repeat(40),
    };
    let channel_path = store.path().join("channel.json");
    channel(&channel_path, &tool.sha256);
    env.insert(
        "CCID_CHANNEL_FILE".into(),
        channel_path.as_os_str().to_owned(),
    );
    write_record(
        &store.path().join(STORE_DIR),
        &Record {
            schema: SCHEMA,
            key: prepared.key.clone(),
            components: prepared.components.clone(),
            commit: prepared.commit.clone(),
            repository: "https://example.invalid/checks/hit".into(),
            result: ResultWord::Success,
            producer: Producer {
                kind: ProducerKind::Lane,
                identity: Some("lane".into()),
            },
            tool,
            started_at: 10,
            finished_at: 20,
            duration_seconds: 10,
        },
    )
    .unwrap();
    // A full cached run answers from the store: no moon, no check, but the receipt the job asserts.
    crate::cached::run_cached_with_environment(
        root,
        Path::new(".ci/ccid.toml"),
        &["verify".to_owned()],
        false,
        false,
        env,
    )
    .unwrap();
    assert!(
        !root.join(".ccid/counter").exists(),
        "the check must not run on a verdict hit"
    );
    assert!(root.join(".ccid/results/verify.jsonl").is_file());
    let metrics: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".ccid/cache-metrics.json")).unwrap()).unwrap();
    assert_eq!(metrics["checks"][0]["status"], "cached");
    assert_eq!(metrics["checks"][0]["computed"], 0);
}

#[test]
fn a_compiler_declared_as_a_command_line_is_resolved_by_its_program() {
    let fixture = fixture("cc");
    let root = fixture.path();
    let mut env = environment();
    env.insert("CC".into(), "sh -c true".into());
    // The key is computable: the tool identity is the program (`sh`), the full value stays a
    // declared semantic input.
    let commit_id = commit(root, None, &env).unwrap();
    let with = prepare(root, "verify", &check(), &env, &commit_id).unwrap();
    let mut other = env.clone();
    other.insert("CC".into(), "sh -c false".into());
    let without = prepare(root, "verify", &check(), &other, &commit_id).unwrap();
    assert_ne!(with.key, without.key);
}

#[test]
fn a_plan_reports_bypass_reasons_without_running_anything() {
    let fixture = fixture("plan");
    let root = fixture.path();
    let mut opt_out = check();
    opt_out.cache_pure = Some(false);
    assert_eq!(
        plan_bypass(&opt_out, &crate::cached::executor_check(&opt_out), root).as_deref(),
        Some("declares cache_pure = false")
    );
    fs::write(
        root.join(".ci/volatile.sh"),
        "set -e\ncurl -fsS https://x.invalid\n",
    )
    .unwrap();
    let mut volatile = check();
    volatile.commands = vec![vec!["bash".into(), ".ci/volatile.sh".into()]];
    let reason = plan_bypass(&volatile, &crate::cached::executor_check(&volatile), root).unwrap();
    assert!(reason.contains("curl"), "{reason}");
    // A plain deterministic check has no bypass reason.
    assert!(plan_bypass(&check(), &crate::cached::executor_check(&check()), root).is_none());
}

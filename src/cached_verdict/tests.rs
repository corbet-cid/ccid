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
    prepare(root, name, &check(), env, &commit, &[]).unwrap()
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
        prepare(root, "verify", &changed, &env, &commit_id, &[])
            .unwrap()
            .key,
        key
    );

    // A declared environment value is part of the key.
    let mut declared = check();
    declared.cache_env = vec!["VERDICT_TEST_FLAG".into()];
    let commit_id = commit(root, None, &env).unwrap();
    let without = prepare(root, "verify", &declared, &env, &commit_id, &[])
        .unwrap()
        .key;
    let mut with = env.clone();
    with.insert("VERDICT_TEST_FLAG".into(), "1".into());
    assert_ne!(
        prepare(root, "verify", &declared, &with, &commit_id, &[])
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
    let without = prepare(root, "verify", &declared_locale, &env, &commit_id, &[])
        .unwrap()
        .key;
    // The declared value must differ from whatever the host already exports.
    let base_lang = env
        .get(OsStr::new("LANG"))
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_default();
    let changed_lang = if base_lang == "en_US.UTF-8" {
        "fr_FR.UTF-8"
    } else {
        "en_US.UTF-8"
    };
    let mut declared_changed = env.clone();
    declared_changed.insert("LANG".into(), changed_lang.into());
    let with = prepare(
        root,
        "verify",
        &declared_locale,
        &declared_changed,
        &commit_id,
        &[],
    )
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
        "STATE_DIRECTORY",
        "RUNTIME_DIRECTORY",
        "SYSTEMD_EXEC_PID",
        "JOURNAL_STREAM",
        "SOME_UNRELATED_FLAG",
    ] {
        ambient.insert(name.into(), "1".into());
    }
    assert_eq!(prepared(root, &ambient, "verify").key, key);

    // A tool's SEMANTIC release is part of the key; its installed bytes are not.
    let tools = tempfile::tempdir().unwrap();
    let script = tools.path().join("sh");
    fs::write(&script, "#!/bin/sh\necho 'sh 9.9 (fixture one)'\n").unwrap();
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
    let semantic_one = prepared(root, &with_tool, "verify").key;
    assert_ne!(semantic_one, key);
    // Same release installed differently (different bytes, same version output) keys the same.
    fs::write(
        &script,
        "#!/bin/sh\n# a different install of the same release\necho 'sh 9.9 (fixture one)'\n",
    )
    .unwrap();
    assert_eq!(prepared(root, &with_tool, "verify").key, semantic_one);
    // A different release changes the key even when the file is otherwise identical.
    fs::write(&script, "#!/bin/sh\necho 'sh 10.0 (fixture two)'\n").unwrap();
    assert_ne!(prepared(root, &with_tool, "verify").key, semantic_one);
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
    assert!(prepare(root, "verify", &check(), &env, &commit_id, &[]).is_err());
    fs::remove_file(root.join("untracked.txt")).unwrap();
    // A tracked source edit is refused too; only resolved locks may differ.
    fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 3 }\n").unwrap();
    assert!(prepare(root, "verify", &check(), &env, &commit_id, &[]).is_err());
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
    let prepared = prepare(root, "verify", &manifest_check, &env, &commit_id, &[]).unwrap();
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
fn a_compiler_selector_is_its_program_and_flags_stay_declared() {
    let fixture = fixture("cc");
    let root = fixture.path();
    let env = environment();
    let commit_id = commit(root, None, &env).unwrap();
    let plain = prepare(root, "verify", &check(), &env, &commit_id, &[]).unwrap();
    // A cache wrapper in front of the same compiler changes no result.
    let mut wrapper = env.clone();
    wrapper.insert("CC".into(), "sccache-corbet cc".into());
    assert_eq!(
        prepare(root, "verify", &check(), &wrapper, &commit_id, &[])
            .unwrap()
            .key,
        plain.key
    );
    // Flags in the selector are semantic and are keyed verbatim.
    let mut flags = env.clone();
    flags.insert("CC".into(), "cc -m32".into());
    assert_ne!(
        prepare(root, "verify", &check(), &flags, &commit_id, &[])
            .unwrap()
            .key,
        plain.key
    );
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

#[test]
fn a_pass_from_another_producing_binary_is_refreshed_not_kept_forever() {
    let fixture = fixture("refresh");
    let root = fixture.path();
    let store = tempfile::tempdir().unwrap();
    let mut env = environment();
    env.insert(
        "CI_REPOSITORY_URL".into(),
        "https://example.invalid/checks/refresh".into(),
    );
    env.insert(
        "CCID_RESULT_CACHE".into(),
        store.path().as_os_str().to_owned(),
    );
    let commit_id = commit(root, None, &env).unwrap();
    let prepared = prepare(root, "verify", &check(), &env, &commit_id, &[]).unwrap();
    let producer = Producer {
        kind: ProducerKind::Lane,
        identity: Some("lane".into()),
    };
    let previous = Tool {
        sha256: "a".repeat(64),
        source_revision: "b".repeat(40),
    };
    write_record(
        &store.path().join(STORE_DIR),
        &Record {
            schema: SCHEMA,
            key: prepared.key.clone(),
            components: prepared.components.clone(),
            commit: prepared.commit.clone(),
            repository: "https://example.invalid/checks/refresh".into(),
            result: ResultWord::Success,
            producer: producer.clone(),
            tool: previous.clone(),
            started_at: 10,
            finished_at: 20,
            duration_seconds: 10,
        },
    )
    .unwrap();
    // A different producing binary computes the same pass (as after a channel bump): its own
    // success must replace the provenance so its channel accepts the record.
    record_success(
        std::slice::from_ref(&prepared),
        &BTreeSet::new(),
        &producer,
        30,
        &env,
    )
    .unwrap();
    let record = read_record(&store.path().join(STORE_DIR), &prepared.key)
        .unwrap()
        .unwrap();
    assert_eq!(record.result, ResultWord::Success);
    assert_ne!(record.tool, previous);
    assert_eq!(record.tool, own_tool().unwrap());
    // A second success from the same binary keeps the record (idempotent).
    record_success(
        std::slice::from_ref(&prepared),
        &BTreeSet::new(),
        &producer,
        40,
        &env,
    )
    .unwrap();
    assert_eq!(
        read_record(&store.path().join(STORE_DIR), &prepared.key)
            .unwrap()
            .unwrap()
            .tool,
        own_tool().unwrap()
    );
}

/// Two installations of the same tool release with different bytes: the upstream rustup /
/// nixpkgs split the verdict key must not see. Same semantic output, different files.
fn host_profile(directory: &Path, marker: &str, rustc_release: &str) {
    fs::create_dir_all(directory).unwrap();
    let write = |name: &str, body: String| {
        let path = directory.join(name);
        fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    };
    write(
        "sh",
        format!("#!/bin/sh\n# {marker}\necho 'sh 5.2.37 (fixture release)'\n"),
    );
    write(
        "rustc",
        format!(
            "#!/bin/sh\n# {marker}\ncat <<'EOF'\nrustc 1.99.1 (58cf8f9c3 2026-10-13)\nbinary: rustc\ncommit-hash: 58cf8f9c35b355199727627d07ae34a4e2846d67\ncommit-date: 2026-10-13\nhost: x86_64-unknown-linux-gnu\nrelease: 1.99.1\nchannel: {rustc_release}\nLLVM version: 23.1.1\nEOF\n"
        ),
    );
    write(
        "cargo",
        format!(
            "#!/bin/sh\n# {marker}\ncat <<'EOF'\ncargo 1.99.1 (5f94df478 2026-08-27)\nrelease: 1.99.1\ncommit-hash: 5f94df478b2b4a5c4c9a9f9c8f9e1d2c3b4a5f60\ncommit-date: 2026-08-27\nhost: x86_64-unknown-linux-gnu\nlibgit2: 1.9.2 (sys:0.20.2 vendored)\nlibcurl: 8.14.1 (sys:0.4.83+curl-8.14.1)\nssl: OpenSSL 3.5.1 1 Jul 2025\nos: {marker}\nEOF\n"
        ),
    );
}

#[test]
fn two_host_profiles_of_the_same_release_share_a_verdict_key() {
    let fixture = fixture("profiles");
    let root = fixture.path();
    let tools = tempfile::tempdir().unwrap();
    let upstream = tools.path().join("upstream");
    let nixpkgs = tools.path().join("nixpkgs");
    host_profile(&upstream, "upstream rustup install", "stable");
    host_profile(&nixpkgs, "nixpkgs rustup install (patchelf'd)", "stable");
    let mut check = check();
    check.commands = vec![vec!["sh".into(), "-c".into(), "true".into()]];
    check.cache_tools = vec![
        vec!["sh".into(), "--version".into()],
        vec!["rustc".into(), "-vV".into()],
        vec!["cargo".into(), "-vV".into()],
    ];
    let commit_id = commit(root, None, &environment()).unwrap();
    let with_path = |directory: &Path| {
        let mut env = environment();
        env.insert(
            "PATH".into(),
            std::env::join_paths(
                std::iter::once(directory.to_path_buf())
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap(),
        );
        env
    };
    let key = |directory: &Path| {
        prepare(
            root,
            "verify",
            &check,
            &with_path(directory),
            &commit_id,
            &[],
        )
        .unwrap()
        .key
    };
    assert_eq!(
        key(&upstream),
        key(&nixpkgs),
        "the same tool release on two hosts keys the same"
    );
    // A different compiler release on one host still changes the key.
    host_profile(&nixpkgs, "nixpkgs rustup install (patchelf'd)", "different");
    assert_ne!(key(&upstream), key(&nixpkgs));
}

#[test]
fn rustup_never_enters_the_key() {
    let fixture = fixture("rustup-drop");
    let root = fixture.path();
    let tools = tempfile::tempdir().unwrap();
    let nightly = tools.path().join("nightly-rustc");
    let write_nightly = |release: &str| {
        fs::write(
            &nightly,
            format!(
                "#!/bin/sh\ncat <<'EOF'\nrustc 1.99.0-nightly ({release} 2026-01-01)\nbinary: rustc\ncommit-hash: {}\ncommit-date: 2026-01-01\nhost: x86_64-unknown-linux-gnu\nrelease: 1.99.0-nightly\nLLVM version: 21.1.8\nEOF\n",
                "b".repeat(40)
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&nightly, fs::Permissions::from_mode(0o755)).unwrap();
        }
    };
    write_nightly("one");
    let with_rustup = |directory: &Path, marker: &str| {
        let rustup = directory.join("rustup");
        fs::write(
            &rustup,
            format!("#!/bin/sh\n# {marker}\necho '{}'\n", nightly.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&rustup, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut env = environment();
        env.insert(
            "PATH".into(),
            std::env::join_paths(
                std::iter::once(directory.to_path_buf())
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap(),
        );
        env
    };
    let profile_a = tempfile::tempdir().unwrap();
    let profile_b = tempfile::tempdir().unwrap();
    let env_a = with_rustup(profile_a.path(), "rustup release one");
    let env_b = with_rustup(profile_b.path(), "a patchelf'd rustup release one");
    let mut check = check();
    check.cache_tools = vec![vec![
        "rustup".into(),
        "run".into(),
        "nightly".into(),
        "rustc".into(),
        "-vV".into(),
    ]];
    let commit_id = commit(root, None, &environment()).unwrap();
    let key_a = prepare(root, "verify", &check, &env_a, &commit_id, &[])
        .unwrap()
        .key;
    let key_b = prepare(root, "verify", &check, &env_b, &commit_id, &[])
        .unwrap()
        .key;
    assert_eq!(
        key_a, key_b,
        "rustup's own bytes and version are not a build input"
    );
    write_nightly("two");
    let key_c = prepare(root, "verify", &check, &env_a, &commit_id, &[])
        .unwrap()
        .key;
    assert_ne!(
        key_a, key_c,
        "the nightly compiler release is a build input"
    );
}

#[test]
fn producer_build_paths_do_not_enter_check_identity() {
    let fixture = fixture("identity-paths");
    let root = fixture.path();
    let env = environment();
    let check = crate::Check {
        kind: "commands".into(),
        cache_env: vec!["LD_LIBRARY_PATH".into()],
        ..Default::default()
    };
    let mut ci = env.clone();
    ci.insert("LD_LIBRARY_PATH".into(), "/nix/store/clang/lib:".into());
    let mut lane = env.clone();
    lane.insert(
        "LD_LIBRARY_PATH".into(),
        format!(
            "/nix/store/clang/lib:{}:/home/lane/.rustup/toolchains/stable/lib:",
            root.join("target/release").display()
        )
        .into(),
    );
    lane.insert("CARGO_TARGET_DIR".into(), root.join("target").into());
    lane.insert("RUSTUP_HOME".into(), "/home/lane/.rustup".into());
    let first = check_identity_payload(&check, &ci, root).unwrap();
    let second = check_identity_payload(&check, &lane, root).unwrap();
    assert_eq!(first, second, "producer build locators are not inputs");
    // A genuinely different library path is still an input.
    let mut other = ci.clone();
    other.insert("LD_LIBRARY_PATH".into(), "/nix/store/other/lib:".into());
    assert_ne!(first, check_identity_payload(&check, &other, root).unwrap());
}

#[test]
fn repository_provenance_is_the_path_not_the_transport_alias() {
    for raw in [
        "https://git.example/org/repo",
        "https://git.example/org/repo.git",
        "https://forge.example/org/repo",
        "https://forge.example.internal/org/repo.git",
        "https://user@git.example:8443/org/repo.git",
        "git@git.example:org/repo.git",
        "org/repo",
    ] {
        assert_eq!(canonical_repository(raw), "org/repo", "{raw}");
    }
    assert_eq!(canonical_repository("unknown"), "unknown");
}

/// A `fleet = true` verdict is specific to its fleet data: the digest enters the key through the
/// normalised `cache_env`, a check without the field keys exactly as before, and a fleet check
/// is never keyed without a digest.
#[test]
fn the_fleet_digest_keys_fleet_checks_and_only_them() {
    let fixture = fixture("fleet-key");
    let root = fixture.path();
    let mut env = environment();
    for name in [
        crate::fleet::FLEET_DIGEST_ENV,
        crate::fleet::FLEET_FILE_ENV,
        crate::fleet::FLEET_KEYED_ENV,
    ] {
        env.remove(OsStr::new(name));
    }
    env.insert(
        "CI_REPOSITORY_URL".into(),
        "https://example.invalid/checks/fleet-key".into(),
    );
    let commit_id = commit(root, None, &env).unwrap();
    let key = |check: &crate::Check, env: &Environment| {
        prepare(root, "verify", check, env, &commit_id, &[]).map(|entry| entry.key)
    };
    let plain = check();
    let mut reads = check();
    reads.fleet = true;
    let mut checks = BTreeMap::from([("verify".to_owned(), reads)]);
    crate::fleet::normalise(&mut checks).unwrap();
    let reads = checks.remove("verify").unwrap();
    let with = |digest: &str| {
        let mut env = env.clone();
        env.insert(crate::fleet::FLEET_DIGEST_ENV.into(), digest.into());
        env
    };
    let error = key(&reads, &env).unwrap_err().to_string();
    assert!(
        error.contains("never keyed without CCID_FLEET_DIGEST"),
        "{error}"
    );
    let first = key(&reads, &with(&"a".repeat(64))).unwrap();
    assert_eq!(first, key(&reads, &with(&"a".repeat(64))).unwrap());
    assert_ne!(first, key(&reads, &with(&"b".repeat(64))).unwrap());
    // A check without fleet = true never sees the digest in its key.
    let baseline = key(&plain, &env).unwrap();
    assert_eq!(baseline, key(&plain, &with(&"a".repeat(64))).unwrap());
    assert_ne!(baseline, first);
    let identity = check_identity_payload(&plain, &with(&"a".repeat(64)), root).unwrap();
    assert!(identity["declaration"].get("fleet").is_none(), "{identity}");
    let identity = check_identity_payload(&reads, &with(&"a".repeat(64)), root).unwrap();
    assert_eq!(identity["declaration"]["fleet"], json!(true));
    assert_eq!(
        identity["env"][crate::fleet::FLEET_DIGEST_ENV],
        json!("a".repeat(64))
    );
}

/// The release channel with fleet data still parses for record acceptance (`channel_accepts`).
#[test]
fn the_release_channel_with_fleet_data_still_names_its_releases() {
    let channel: Channel = serde_json::from_slice(include_bytes!("../../channel.json")).unwrap();
    assert!(is_hex(&channel.default.revision, 40));
    assert!(is_hex(&channel.default.sha256, 64));
    assert!(channel
        .classes
        .values()
        .all(|entry| is_hex(&entry.sha256, 64)));
}

/// A poppler-style tool: it answers `-v` and fails `--version` (and with no arguments).
fn poppler_tool(directory: &Path, release: &str) {
    fs::write(
        directory.join("poppler-tool"),
        format!(
            "#!/bin/sh\nif [ \"$1\" = '-v' ]; then echo 'poppler-tool version {release} (fixture)'; exit 0; fi\necho 'usage: poppler-tool [-v]' >&2\nexit 1\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            directory.join("poppler-tool"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
}

/// A tool that answers both `--version` and `-v`, with different text each.
fn both_tool(directory: &Path) {
    fs::write(
        directory.join("both-tool"),
        "#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo 'both-tool version 1.0 (long)'; exit 0; fi\nif [ \"$1\" = '-v' ]; then echo 'both-tool v1.0 (short)'; exit 0; fi\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            directory.join("both-tool"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
}

fn tool_path_env(env: &Environment, directory: &Path) -> Environment {
    let mut with_tool = env.clone();
    with_tool.insert(
        "PATH".into(),
        std::env::join_paths(
            std::iter::once(directory.to_path_buf())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap(),
    );
    with_tool
}

#[test]
fn the_declared_cache_tools_argv_is_the_version_query_with_a_dash_v_fallback() {
    let fixture = fixture("tool-probe");
    let root = fixture.path();
    let tools = tempfile::tempdir().unwrap();
    poppler_tool(tools.path(), "24.02.0");
    let env = tool_path_env(&environment(), tools.path());
    let commit_id = commit(root, None, &env).unwrap();

    // The declared query runs verbatim: a tool that only answers `-v` gets a verdict.
    // (On the old code this prepare fails: `--version` is the only probe.)
    let mut declared = check();
    declared.commands = vec![vec!["sh".into(), "-c".into(), "true".into()]];
    declared.cache_tools = vec![vec!["poppler-tool".into(), "-v".into()]];
    let verbatim = prepare(root, "verify", &declared, &env, &commit_id, &[]).unwrap();
    // A different release of the same tool changes the key.
    poppler_tool(tools.path(), "25.04.0");
    assert_ne!(
        prepare(root, "verify", &declared, &env, &commit_id, &[])
            .unwrap()
            .key,
        verbatim.key
    );
    poppler_tool(tools.path(), "24.02.0");

    // A bare program name falls back from `--version` to `-v`: the same tool release
    // keys the same digest whether the manifest declares the query or not.
    // (On the old code this prepare fails too.)
    let mut bare = check();
    bare.commands = vec![vec!["sh".into(), "-c".into(), "true".into()]];
    bare.cache_tools = vec![vec!["poppler-tool".into()]];
    let fallback = prepare(root, "verify", &bare, &env, &commit_id, &[]).unwrap();
    assert_eq!(
        fallback.components.toolchain["poppler-tool"],
        verbatim.components.toolchain["poppler-tool"]
    );

    // `--version` still wins when the tool answers it: the fallback never shadows the
    // primary query.
    both_tool(tools.path());
    let long = format!("{:x}", Sha256::digest("both-tool version 1.0 (long)"));
    let short = format!("{:x}", Sha256::digest("both-tool v1.0 (short)"));
    assert_ne!(long, short);
    let mut bare_both = check();
    bare_both.commands = vec![vec!["sh".into(), "-c".into(), "true".into()]];
    bare_both.cache_tools = vec![vec!["both-tool".into()]];
    assert_eq!(
        prepare(root, "verify", &bare_both, &env, &commit_id, &[])
            .unwrap()
            .components
            .toolchain["both-tool"],
        long
    );
    let mut declared_both = check();
    declared_both.commands = vec![vec!["sh".into(), "-c".into(), "true".into()]];
    declared_both.cache_tools = vec![vec!["both-tool".into(), "-v".into()]];
    assert_eq!(
        prepare(root, "verify", &declared_both, &env, &commit_id, &[])
            .unwrap()
            .components
            .toolchain["both-tool"],
        short
    );
}

/// A tool whose `-v` exits 0 without printing anything (and which fails `--version`).
fn silent_tool(directory: &Path) {
    fs::write(
        directory.join("silent-tool"),
        "#!/bin/sh\nif [ \"$1\" = '-v' ]; then exit 0; fi\necho 'usage: silent-tool [-v]' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            directory.join("silent-tool"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
}

#[test]
fn a_silent_dash_v_is_not_a_tool_identity() {
    let fixture = fixture("tool-probe");
    let root = fixture.path();
    let tools = tempfile::tempdir().unwrap();
    silent_tool(tools.path());
    let env = tool_path_env(&environment(), tools.path());
    let commit_id = commit(root, None, &env).unwrap();

    // The `-v` fallback exits 0 but prints nothing: there is no identity, so the
    // bare program gets no verdict instead of keying every release to "".
    // (On the old code this prepare succeeds with the empty-string identity.)
    let mut bare = check();
    bare.commands = vec![vec!["sh".into(), "-c".into(), "true".into()]];
    bare.cache_tools = vec![vec!["silent-tool".into()]];
    assert!(
        prepare(root, "verify", &bare, &env, &commit_id, &[]).is_err(),
        "a tool silent on -v must get no verdict"
    );

    // The same holds when `-v` is the declared query: verbatim silence is not an answer.
    let mut declared = check();
    declared.commands = vec![vec!["sh".into(), "-c".into(), "true".into()]];
    declared.cache_tools = vec![vec!["silent-tool".into(), "-v".into()]];
    assert!(
        prepare(root, "verify", &declared, &env, &commit_id, &[]).is_err(),
        "a declared query printing nothing must get no verdict"
    );
}

#[test]
fn systemd_unit_directories_never_enter_the_semantic_environment() {
    let fixture = fixture("unit-layout");
    let root = fixture.path();
    let env = environment();
    let commit_id = commit(root, None, &env).unwrap();
    // Even declared, the per-unit directories are not inputs: a `StateDirectory=` or
    // `RuntimeDirectory=` layout must never split the ledger.
    // (On the old code the three keys below differ: declared values are keyed.)
    let mut declared = check();
    declared.cache_env = vec!["STATE_DIRECTORY".into(), "RUNTIME_DIRECTORY".into()];
    let base = prepare(root, "verify", &declared, &env, &commit_id, &[]).unwrap();
    let mut unit = env.clone();
    unit.insert("STATE_DIRECTORY".into(), "/var/lib/ccid-runner".into());
    unit.insert("RUNTIME_DIRECTORY".into(), "/run/ccid-runner".into());
    assert_eq!(
        prepare(root, "verify", &declared, &unit, &commit_id, &[])
            .unwrap()
            .key,
        base.key
    );
    // A different unit layout keys the same too.
    let mut other = env.clone();
    other.insert("STATE_DIRECTORY".into(), "/var/lib/other-unit".into());
    other.insert("RUNTIME_DIRECTORY".into(), "/run/other-unit".into());
    assert_eq!(
        prepare(root, "verify", &declared, &other, &commit_id, &[])
            .unwrap()
            .key,
        base.key
    );
    // Neither name reaches the hashed payload.
    let payload = check_identity_payload(&declared, &unit, root).unwrap();
    let names: Vec<&str> = payload["env"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(!names.contains(&"STATE_DIRECTORY"));
    assert!(!names.contains(&"RUNTIME_DIRECTORY"));
}

// ---- narrowed keys: a check keyed on its MEASURED read-set (see `narrowing`) ----

/// The ordinary fixture plus two top-level entries a measurement classifies as unread.
fn narrow_fixture(name: &str) -> tempfile::TempDir {
    let fixture = fixture(name);
    let root = fixture.path();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("docs/guide.md"), "# guide\n").unwrap();
    fs::write(root.join("README.md"), "# readme\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "-m", "docs"]);
    fixture
}

fn commit_all(root: &Path, message: &str) {
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "-m", message]);
}

/// The check as `ccid measure-inputs` would leave it for the tree at HEAD: inputs and provenance
/// (a local test build reports `unversioned`, which a key refuses, so the tool is a revision).
fn measured(root: &Path, _env: &Environment) -> crate::Check {
    let mut check = check();
    check.cache_inputs = Some(vec!["src/**".into(), "Cargo.toml".into(), ".ci/**".into()]);
    let globs = check.cache_inputs.clone().unwrap();
    let mut provenance = measured_provenance(root, &check, &globs).unwrap();
    provenance.tool = "a".repeat(40);
    check.cache_inputs_measured = Some(provenance);
    check
}

#[test]
fn measure_inputs_provenance_is_what_the_key_expects() {
    let fixture = narrow_fixture("narrow-provenance");
    let root = fixture.path();
    let env = environment();
    let mut declared = check();
    declared.cache_inputs = Some(vec!["src/**".into()]);
    let provenance = measured_provenance(root, &declared, &["src/**".to_owned()]).unwrap();
    assert_eq!(provenance.tool, SOURCE_REVISION);
    assert_eq!(
        provenance.commands,
        crate::narrowing::commands_digest(&declared).unwrap()
    );
    let head = commit(root, None, &env).unwrap();
    assert_eq!(provenance.layout, layout_of(root, &head).unwrap());
    // The printed line, put on the check, narrows the key (once the tool is a released ccid).
    let mut released = provenance.clone();
    released.tool = "a".repeat(40);
    released.validate().unwrap();
    declared.cache_inputs_measured = Some(released);
    assert_eq!(prepared_for(root, &env, &declared).scope(), "read-set");
    // A local build is refused when the key is computed: whole tree, with the reason reported.
    declared.cache_inputs_measured = Some(provenance);
    if SOURCE_REVISION == "unversioned" {
        assert_eq!(prepared_for(root, &env, &declared).scope(), "tree");
    }
}

fn prepared_for(root: &Path, env: &Environment, check: &crate::Check) -> Prepared {
    let commit_id = commit(root, None, env).unwrap();
    prepare(root, "verify", check, env, &commit_id, &[]).unwrap()
}

fn store_environment(store: &Path) -> Environment {
    let mut env = environment();
    env.insert(
        "CI_REPOSITORY_URL".into(),
        "https://example.invalid/checks/narrowed".into(),
    );
    env.insert("CCID_RESULT_CACHE".into(), store.as_os_str().to_owned());
    env
}

fn record_for(prepared: &Prepared) -> Record {
    Record {
        schema: SCHEMA,
        key: prepared.key.clone(),
        components: prepared.components.clone(),
        commit: prepared.commit.clone(),
        repository: "example.invalid/checks/narrowed".into(),
        result: ResultWord::Success,
        producer: Producer {
            kind: ProducerKind::Lane,
            identity: None,
        },
        tool: own_tool().unwrap(),
        started_at: 10,
        finished_at: 20,
        duration_seconds: 10,
    }
}

#[test]
fn a_commit_outside_the_read_set_reuses_the_verdict_and_one_inside_reruns() {
    let fixture = narrow_fixture("narrow-reuse");
    let root = fixture.path();
    let store = tempfile::tempdir().unwrap();
    let env = store_environment(store.path());
    let directory = store.path().join(STORE_DIR);
    let narrowed = measured(root, &env);

    let first = prepared_for(root, &env, &narrowed);
    assert_eq!(first.scope(), "read-set");
    assert!(first.components.read_set.is_some());
    assert_eq!(lookup(&first, &env).status, Status::Miss);
    write_record(&directory, &record_for(&first)).unwrap();
    assert_eq!(lookup(&first, &env).status, Status::Hit);

    // The same check without provenance keys the whole tree: the baseline this improves on.
    let mut guessed = narrowed.clone();
    guessed.cache_inputs_measured = None;
    let whole_first = prepared_for(root, &env, &guessed);
    assert_eq!(whole_first.scope(), "tree");
    write_record(&directory, &record_for(&whole_first)).unwrap();

    // A commit that touches only files the check does not read (a new page, a readme).
    fs::write(root.join("docs/guide.md"), "# guide, revised\n").unwrap();
    fs::write(root.join("docs/new.md"), "# new\n").unwrap();
    fs::write(root.join("README.md"), "# readme, revised\n").unwrap();
    commit_all(root, "docs only");
    let second = prepared_for(root, &env, &narrowed);
    assert_ne!(second.commit, first.commit);
    assert_eq!(second.scope(), "read-set");
    assert_eq!(second.key, first.key, "outside the read-set: the same key");
    assert_eq!(lookup(&second, &env).status, Status::Hit);
    let whole_second = prepared_for(root, &env, &guessed);
    assert_ne!(whole_second.key, whole_first.key);
    assert_eq!(lookup(&whole_second, &env).status, Status::Miss);

    // A commit that touches a file the check reads.
    fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 2 }\n").unwrap();
    commit_all(root, "source");
    let third = prepared_for(root, &env, &narrowed);
    assert_eq!(third.scope(), "read-set");
    assert_ne!(third.key, first.key, "inside the read-set: a new key");
    assert_eq!(lookup(&third, &env).status, Status::Miss);

    // So does one that touches a script under .ci/ or the declared manifest.
    let before = third.key.clone();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='verdict-fixture'\nversion='0.0.1'\nedition='2021'\n",
    )
    .unwrap();
    commit_all(root, "manifest");
    assert_ne!(prepared_for(root, &env, &narrowed).key, before);
}

#[test]
fn a_changed_check_toolchain_or_lock_is_never_reused() {
    let fixture = narrow_fixture("narrow-rekey");
    let root = fixture.path();
    let env = environment();
    let narrowed = measured(root, &env);
    let base = prepared_for(root, &env, &narrowed);
    assert_eq!(base.scope(), "read-set");

    // The commands changed after the inputs were measured: the measurement no longer describes
    // the check, so it is keyed on the whole tree (and differs from the narrowed key).
    let mut edited = narrowed.clone();
    edited.commands = vec![vec!["sh".into(), "-c".into(), "false".into()]];
    let rekeyed = prepared_for(root, &env, &edited);
    assert_eq!(rekeyed.scope(), "tree");
    assert_ne!(rekeyed.key, base.key);
    assert!(rekeyed.components.read_set.is_none());
    // A list edited by hand after the measurement is not the measured list.
    let mut hand_edited = narrowed.clone();
    hand_edited.cache_inputs = Some(vec!["src/**".into(), ".ci/**".into()]);
    assert_eq!(prepared_for(root, &env, &hand_edited).scope(), "tree");
    // ... and a docs-only commit then reruns it, until it is measured again.
    let docs_before = rekeyed.key.clone();
    fs::write(root.join("README.md"), "# readme, again\n").unwrap();
    commit_all(root, "docs");
    assert_ne!(prepared_for(root, &env, &edited).key, docs_before);
    let base = prepared_for(root, &env, &narrowed);
    assert_eq!(base.scope(), "read-set");

    // A different release of a tool the check uses is a different key, narrowed or not.
    let tools = tempfile::tempdir().unwrap();
    let script = tools.path().join("sh");
    fs::write(&script, "#!/bin/sh\necho 'sh 9.9 (fixture one)'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut with_tool = env.clone();
    with_tool.insert(
        "PATH".into(),
        tool_path_env(&env, tools.path())
            .remove(OsStr::new("PATH"))
            .unwrap(),
    );
    let one = prepared_for(root, &with_tool, &narrowed);
    assert_eq!(one.scope(), "read-set");
    assert_ne!(one.key, base.key);
    fs::write(&script, "#!/bin/sh\necho 'sh 10.0 (fixture two)'\n").unwrap();
    assert_ne!(prepared_for(root, &with_tool, &narrowed).key, one.key);

    // A lock outside the declared globs is still keyed: no narrow declaration can leave it out.
    fs::write(
        root.join("Cargo.lock"),
        "version = 4\n[[package]]\nname = \"verdict-fixture\"\nversion = \"0.0.1\"\n",
    )
    .unwrap();
    commit_all(root, "lock");
    let moved = prepared_for(root, &env, &narrowed);
    assert_eq!(moved.scope(), "read-set");
    assert_ne!(moved.key, base.key);
}

#[test]
fn an_input_outside_the_measured_set_seen_when_the_check_runs_fails_closed() {
    let fixture = narrow_fixture("narrow-closed");
    let root = fixture.path();
    let store = tempfile::tempdir().unwrap();
    let env = store_environment(store.path());
    let directory = store.path().join(STORE_DIR);
    let narrowed = measured(root, &env);
    let base = prepared_for(root, &env, &narrowed);
    assert_eq!(base.scope(), "read-set");

    // A working tree that differs from the commit outside the read-set is still refused: the
    // check could read it, and the key cannot name it.
    fs::write(root.join("docs/guide.md"), "# edited, not committed\n").unwrap();
    let commit_id = commit(root, None, &env).unwrap();
    assert!(prepare(root, "verify", &narrowed, &env, &commit_id, &[]).is_err());
    git(root, &["checkout", "--quiet", "--", "docs/guide.md"]);
    fs::write(root.join("scratch.txt"), "untracked\n").unwrap();
    assert!(prepare(root, "verify", &narrowed, &env, &commit_id, &[]).is_err());
    fs::remove_file(root.join("scratch.txt")).unwrap();

    // A check that wrote into the tree outside its read-set while it ran leaves no pass, even
    // though the narrowed digest did not move.
    let run = prepared_for(root, &env, &narrowed);
    fs::write(root.join("docs/guide.md"), "# written by the check\n").unwrap();
    let producer = Producer {
        kind: ProducerKind::Lane,
        identity: None,
    };
    record_success(&[run], &BTreeSet::new(), &producer, 0, &env).unwrap();
    assert!(read_record(&directory, &base.key).unwrap().is_none());
    git(root, &["checkout", "--quiet", "--", "docs/guide.md"]);
    // The same run without the write is recorded.
    let clean = prepared_for(root, &env, &narrowed);
    record_success(&[clean], &BTreeSet::new(), &producer, 0, &env).unwrap();
    assert!(read_record(&directory, &base.key).unwrap().is_some());

    // A top-level entry the measurement never saw is an input outside the measured set: the
    // narrowing is invalidated and the whole tree is keyed, so even a "docs" commit reruns.
    fs::create_dir_all(root.join("fixtures")).unwrap();
    fs::write(root.join("fixtures/data.json"), "{}\n").unwrap();
    commit_all(root, "a new top-level directory");
    let unknown = prepared_for(root, &env, &narrowed);
    assert_eq!(unknown.scope(), "tree");
    assert!(unknown.components.read_set.is_none());
    assert_ne!(unknown.key, base.key);
    assert_eq!(lookup(&unknown, &env).status, Status::Miss);
    fs::write(root.join("fixtures/data.json"), "{\"a\":1}\n").unwrap();
    commit_all(root, "edit the new directory");
    let edited = prepared_for(root, &env, &narrowed);
    assert_ne!(
        edited.key, unknown.key,
        "the new entry is keyed, not ignored"
    );
}

#[test]
fn unmeasured_checks_keep_the_whole_tree_key() {
    let fixture = narrow_fixture("narrow-unmeasured");
    let root = fixture.path();
    let env = environment();
    let commit_id = commit(root, None, &env).unwrap();
    let whole = tree_digest(root, &commit_id).unwrap();

    // No inputs declared.
    let plain = prepared_for(root, &env, &check());
    // Inputs declared without measure-inputs provenance (a guess, or a list from before).
    let mut guessed = check();
    guessed.cache_inputs = Some(vec!["src/**".into()]);
    let guess = prepared_for(root, &env, &guessed);
    // Inputs measured for a nix check, which keys every file anyway.
    let mut nix = measured(root, &env);
    nix.kind = "nix".into();
    for entry in [&plain, &guess] {
        assert_eq!(entry.scope(), "tree");
        assert_eq!(
            entry.components.tree, whole,
            "the tree digest is the whole tree"
        );
        assert!(entry.components.read_set.is_none());
        let bytes = String::from_utf8(entry.components.canonical_bytes().unwrap()).unwrap();
        assert!(bytes.starts_with(&format!("{{\"schema\":{SCHEMA},\"tree\":\"{whole}\"")));
        assert!(!bytes.contains("read_set"), "{bytes}");
    }

    // An edit outside the declared inputs changes an unmeasured check's key: it keys everything.
    fs::write(root.join("README.md"), "# readme, revised\n").unwrap();
    commit_all(root, "docs only");
    assert_ne!(prepared_for(root, &env, &check()).key, plain.key);
    assert_ne!(prepared_for(root, &env, &guessed).key, guess.key);
}

#[test]
fn the_key_schema_and_a_golden_key_are_pinned() {
    assert_eq!(SCHEMA, 3);
    let whole = KeyComponents {
        schema: SCHEMA,
        tree: "1".repeat(64),
        git_dependencies: BTreeMap::new(),
        toolchain: BTreeMap::from([("sh".to_owned(), "2".repeat(64))]),
        profile: "3".repeat(64),
        epoch: crate::cached::VERDICT_EPOCH,
        check: "verify".into(),
        check_identity: "4".repeat(64),
        read_set: None,
    };
    let bytes = String::from_utf8(whole.canonical_bytes().unwrap()).unwrap();
    assert_eq!(
        bytes,
        format!(
            "{{\"schema\":3,\"tree\":\"{}\",\"git_dependencies\":{{}},\"toolchain\":{{\"sh\":\"{}\"}},\"profile\":\"{}\",\"epoch\":{},\"check\":\"verify\",\"check_identity\":\"{}\"}}",
            "1".repeat(64),
            "2".repeat(64),
            "3".repeat(64),
            crate::cached::VERDICT_EPOCH,
            "4".repeat(64),
        )
    );
    let mut narrowed = whole.clone();
    narrowed.read_set = Some(crate::narrowing::ReadSetKey {
        inputs: "5".repeat(64),
        layout: "6".repeat(64),
        commands: "7".repeat(64),
    });
    assert_ne!(narrowed.key_hex().unwrap(), whole.key_hex().unwrap());
    let narrowed_bytes = String::from_utf8(narrowed.canonical_bytes().unwrap()).unwrap();
    assert!(narrowed_bytes.starts_with(&bytes[..bytes.len() - 1]));
    assert!(narrowed_bytes.ends_with(&format!(
        ",\"read_set\":{{\"inputs\":\"{}\",\"layout\":\"{}\",\"commands\":\"{}\"}}}}",
        "5".repeat(64),
        "6".repeat(64),
        "7".repeat(64),
    )));
}

#[test]
fn a_narrowed_record_round_trips_and_a_malformed_read_set_is_refused() {
    let fixture = narrow_fixture("narrow-record");
    let root = fixture.path();
    let store = tempfile::tempdir().unwrap();
    let env = store_environment(store.path());
    let directory = store.path().join(STORE_DIR);
    let first = prepared_for(root, &env, &measured(root, &env));
    assert_eq!(first.scope(), "read-set");
    let record = record_for(&first);
    write_record(&directory, &record).unwrap();
    assert_eq!(
        read_record(&directory, &first.key).unwrap().unwrap(),
        record
    );

    let mut broken = first.components.clone();
    broken.read_set.as_mut().unwrap().layout = "not-a-digest".into();
    assert!(broken.validate().is_err());
    // A record of the previous key schema is refused, never read as a pass.
    let mut old = record.clone();
    old.schema = 2;
    old.components.schema = 2;
    assert!(old.validate(&old.key.clone()).is_err());
}

#[test]
fn a_docs_only_commit_skips_a_measured_check_in_a_cached_run_from_its_manifest() {
    let fixture = narrow_fixture("narrow-run");
    let root = fixture.path();
    let store = tempfile::tempdir().unwrap();
    let mut env = store_environment(store.path());
    env.insert("CI".into(), "true".into());
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

    // The manifest as `ccid measure-inputs` leaves it: inputs and provenance on the check.
    let mut declared = check();
    declared.commands = vec![vec![
        "sh".into(),
        "-c".into(),
        "echo ran >> .ccid/counter".into(),
    ]];
    declared.cache_inputs = Some(vec!["src/**".into(), "Cargo.toml".into(), ".ci/**".into()]);
    let globs = declared.cache_inputs.clone().unwrap();
    let mut provenance = measured_provenance(root, &declared, &globs).unwrap();
    provenance.tool = "a".repeat(40);
    fs::write(
        root.join(".ci/ccid.toml"),
        format!(
            "schema = 1\nproject = 'narrow-run'\n[checks.verify]\nkind = 'commands'\n\
             commands = [['sh', '-c', 'echo ran >> .ccid/counter']]\n\
             cache_tools = [['sh', '--version']]\n\
             cache_inputs = ['src/**', 'Cargo.toml', '.ci/**']\n\
             cache_inputs_measured = {{ tool = '{}', commands = '{}', inputs = '{}', layout = '{}', date = '{}' }}\n",
            provenance.tool,
            provenance.commands,
            provenance.inputs,
            provenance.layout,
            provenance.date,
        ),
    )
    .unwrap();
    commit_all(root, "measured manifest");
    let (parsed, _) = crate::load_manifest(root, Path::new(".ci/ccid.toml")).unwrap();
    declared.cache_inputs_measured = Some(provenance);
    assert_eq!(
        parsed.checks["verify"].cache_inputs_measured,
        declared.cache_inputs_measured
    );

    // The producer recorded a pass on this commit.
    let first = prepared_for(root, &env, &parsed.checks["verify"]);
    assert_eq!(first.scope(), "read-set");
    write_record(
        &store.path().join(STORE_DIR),
        &Record {
            tool,
            ..record_for(&first)
        },
    )
    .unwrap();

    // The next commit changes only documentation: the cached run answers from the store.
    fs::write(root.join("docs/guide.md"), "# guide, revised\n").unwrap();
    commit_all(root, "docs only");
    crate::cached::run_cached_with_environment(
        root,
        Path::new(".ci/ccid.toml"),
        &["verify".to_owned()],
        false,
        false,
        env.clone(),
    )
    .unwrap();
    assert!(
        !root.join(".ccid/counter").exists(),
        "the check must not run for a commit outside its read-set"
    );
    let metrics: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".ccid/cache-metrics.json")).unwrap()).unwrap();
    assert_eq!(metrics["checks"][0]["status"], "cached");
    assert_eq!(metrics["checks"][0]["computed"], 0);
}

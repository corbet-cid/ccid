use super::*;
use crate::{
    checks::{javascript_executable, mold_driver},
    source::resolve_archive_path,
};
use std::{
    collections::BTreeSet,
    fs::{OpenOptions, TryLockError},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
};
use tempfile::TempDir;

fn environment(items: &[(&str, &str)]) -> Environment {
    items
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}

#[test]
fn deno_checks_install_locked_dependencies_then_run_tasks() {
    let check = Check {
        kind: "javascript".into(),
        manager: Some("deno".into()),
        scripts: Some(strings(&["check", "test"])),
        ..Check::default()
    };
    assert_eq!(
        checks::javascript_commands(&check).unwrap(),
        vec![
            strings(&["deno", "install", "--frozen-lockfile"]),
            strings(&["deno", "task", "check"]),
            strings(&["deno", "task", "test"]),
        ]
    );
}

#[test]
fn memory_budget_limits_concurrent_jobs() {
    let b = budget(&environment(&[
        ("CI_JOBS", "16"),
        ("CI_MEMORY_MB", "8192"),
        ("CI_MEMORY_PER_JOB_MB", "2048"),
        ("CI_NIX_JOBS", "2"),
    ]))
    .unwrap();
    assert_eq!(
        (b.jobs, b.test_threads, b.nix_jobs, b.nix_cores),
        (4, 4, 2, 2)
    );
}
#[test]
fn empty_pipeline_budget_preserves_worker_allocation() {
    assert_eq!(
        budget(&environment(&[("CI_JOBS", ""), ("CARGO_BUILD_JOBS", "8")]))
            .unwrap()
            .jobs,
        8
    );
}
#[test]
fn invalid_or_unfunded_budgets_fail() {
    for env in [
        environment(&[("CI_JOBS", "0")]),
        environment(&[("CI_MEMORY_PER_JOB_MB", "512")]),
        environment(&[("CI_JOBS", "2"), ("CI_NIX_JOBS", "3")]),
    ] {
        assert!(budget(&env).is_err());
    }
}
#[test]
fn nextest_preserves_doctests_and_arguments() {
    let check: Check = toml::from_str(
        "kind='cargo'\nactions=['test']\ntest_runner='nextest'\nworkspace=true\nall_features=true",
    )
    .unwrap();
    let commands =
        cargo_commands(&check, &strings(&["rustup", "run", "1.94.0", "cargo"]), "8").unwrap();
    assert_eq!(
        commands[0],
        strings(&[
            "rustup",
            "run",
            "1.94.0",
            "cargo",
            "nextest",
            "run",
            "--test-threads",
            "8",
            "--locked",
            "--workspace",
            "--all-features"
        ])
    );
    assert_eq!(
        commands[1],
        strings(&[
            "rustup",
            "run",
            "1.94.0",
            "cargo",
            "test",
            "--doc",
            "--locked",
            "--workspace",
            "--all-features"
        ])
    );
}
#[test]
fn unknown_or_empty_cargo_actions_fail() {
    for text in [
        "kind='cargo'\nactions=[]",
        "kind='cargo'\nactions=['misspelled']",
    ] {
        let check: Check = toml::from_str(text).unwrap();
        assert!(cargo_commands(&check, &strings(&["cargo"]), "1").is_err());
    }
}
#[test]
fn independent_file_handles_contend_for_cache_lock() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("project.lock");
    let open = || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap()
    };
    let first = open();
    let second = open();
    first.try_lock().unwrap();
    assert!(matches!(second.try_lock(), Err(TryLockError::WouldBlock)));
    drop(first);
    // Other parallel tests spawn processes. A fork can briefly retain this
    // open-file-description lock until exec closes its inherited descriptor.
    // Production lock acquisition already waits; require bounded release here.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match second.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("cache lock not released after last owner closed: {error}"),
        }
    }
}
struct SourceFixture {
    _temp: TempDir,
    repo: PathBuf,
    archive: PathBuf,
    commit: String,
    digest: String,
    destination: PathBuf,
}
impl SourceFixture {
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let mut fixture = Self {
            archive: temp.path().join("source.tar"),
            destination: temp.path().join("output"),
            _temp: temp,
            repo,
            commit: String::new(),
            digest: String::new(),
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.name", "Fixture"]);
        fixture.git(&["config", "user.email", "fixture@example.invalid"]);
        fs::write(fixture.repo.join("source.txt"), "exact source\n").unwrap();
        fixture.refresh();
        fixture
    }
    fn refresh(&mut self) {
        self.git(&["add", "--all"]);
        self.git(&["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"]);
        self.commit = self.git(&["rev-parse", "HEAD"]);
        self.git(&[
            "archive",
            "--format=tar",
            "--output",
            self.archive.to_str().unwrap(),
            &self.commit,
        ]);
        self.digest = sha256_file(&self.archive).unwrap();
    }
}
#[test]
fn exact_source_extracts_and_existing_work_is_preserved() {
    let f = SourceFixture::new();
    verify_source(&f.archive, &f.digest, &f.commit, &f.destination).unwrap();
    assert_eq!(
        fs::read_to_string(f.destination.join("source.txt")).unwrap(),
        "exact source\n"
    );
    assert!(verify_source(&f.archive, &f.digest, &f.commit, &f.destination).is_err());
}
#[test]
fn changed_digest_or_commit_fails_before_extraction() {
    let f = SourceFixture::new();
    for (digest, commit) in [
        ("0".repeat(64), f.commit.clone()),
        (f.digest.clone(), "0".repeat(40)),
    ] {
        assert!(verify_source(&f.archive, &digest, &commit, &f.destination).is_err());
        assert!(!f.destination.exists());
    }
}
#[cfg(unix)]
#[test]
fn escaping_symbolic_link_is_rejected_before_extraction() {
    let mut f = SourceFixture::new();
    std::os::unix::fs::symlink("../../outside", f.repo.join("escape")).unwrap();
    f.refresh();
    assert!(verify_source(&f.archive, &f.digest, &f.commit, &f.destination).is_err());
    assert!(!f.destination.exists());
}
#[test]
fn source_paths_cannot_escape_the_archive_root() {
    assert!(safe_relative(Path::new("../outside")).is_err());
    assert!(safe_relative(Path::new("/absolute")).is_err());
    assert_eq!(
        safe_relative(Path::new("inside/../safe")).unwrap(),
        Path::new("safe")
    );
}
#[test]
fn missing_mold_fails_instead_of_changing_the_linker() {
    let tmp = TempDir::new().unwrap();
    assert!(mold_driver(&environment(&[("PATH", tmp.path().to_str().unwrap())])).is_err());
}
#[cfg(unix)]
#[test]
fn mold_uses_nix_origin_before_run_flag() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("wrapped/bin")).unwrap();
    fs::create_dir_all(root.join("wrapped/nix-support")).unwrap();
    fs::write(root.join("wrapped/bin/mold"), "fixture").unwrap();
    fs::write(
        root.join("wrapped/nix-support/orig-bintools"),
        root.join("original").to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(
        mold_driver(&environment(&[(
            "PATH",
            root.join("wrapped/bin").to_str().unwrap()
        )]))
        .unwrap(),
        root.join("original/bin/mold").to_str().unwrap()
    );
}
#[test]
fn deadline_prevents_launching_late_commands() {
    let tmp = TempDir::new().unwrap();
    let runner = Runner::new(
        tmp.path().into(),
        std::env::vars_os().collect(),
        Duration::ZERO,
    )
    .unwrap();
    assert!(runner.run(&strings(&["must-not-execute"]), false).is_err());
}
#[cfg(target_os = "linux")]
#[test]
fn timeout_kills_descendant_that_ignores_term_after_parent_exits() {
    let tmp = TempDir::new().unwrap();
    let pid_file = tmp.path().join("leaf.pid");
    let group_file = tmp.path().join("group.pid");
    let _cleanup = FixtureGroupCleanup(group_file.clone());
    let mut env: Environment = std::env::vars_os().collect();
    set(&mut env, "CCID_FIXTURE_ROLE", "parent");
    set(&mut env, "CCID_FIXTURE_PID", pid_file.as_os_str());
    set(&mut env, "CCID_FIXTURE_GROUP", group_file.as_os_str());
    let runner = Runner::new(tmp.path().into(), env, Duration::from_millis(800)).unwrap();
    let args = vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        "--exact".into(),
        "tests::tree_fixture".into(),
        "--ignored".into(),
        "--nocapture".into(),
    ];
    assert!(runner.run(&args, false).is_err());
    let pid = fs::read_to_string(pid_file).expect("leaf started");
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    for _ in 0..100 {
        let status = fs::read_to_string(&path);
        if status.as_ref().is_err() || status.unwrap().split_whitespace().nth(2) == Some("Z") {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("descendant survived group cleanup");
}
#[test]
#[ignore = "subprocess fixture for owned process-tree cleanup"]
fn tree_fixture() {
    let role = std::env::var("CCID_FIXTURE_ROLE").expect("subprocess only");
    if role == "leaf" {
        ctrlc::set_handler(|| {}).unwrap();
        fs::write(
            std::env::var_os("CCID_FIXTURE_PID").unwrap(),
            std::process::id().to_string(),
        )
        .unwrap();
    } else {
        fs::write(
            std::env::var_os("CCID_FIXTURE_GROUP").unwrap(),
            std::process::id().to_string(),
        )
        .unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::tree_fixture", "--ignored", "--nocapture"])
            .env("CCID_FIXTURE_ROLE", "leaf")
            .spawn()
            .unwrap();
        let _ = child.wait();
        return;
    }
    loop {
        thread::sleep(Duration::from_secs(10));
    }
}

#[cfg(unix)]
#[test]
fn chained_symbolic_link_escape_is_rejected_before_extraction() {
    let mut f = SourceFixture::new();
    std::os::unix::fs::symlink(".", f.repo.join("a")).unwrap();
    std::os::unix::fs::symlink("a/..", f.repo.join("b")).unwrap();
    f.refresh();
    assert!(verify_source(&f.archive, &f.digest, &f.commit, &f.destination).is_err());
    assert!(!f.destination.exists());
}
#[cfg(unix)]
#[test]
fn internal_symbolic_link_chain_is_preserved() {
    let mut f = SourceFixture::new();
    std::os::unix::fs::symlink("source.txt", f.repo.join("a")).unwrap();
    std::os::unix::fs::symlink("a", f.repo.join("b")).unwrap();
    f.refresh();
    verify_source(&f.archive, &f.digest, &f.commit, &f.destination).unwrap();
    assert_eq!(
        fs::read_to_string(f.destination.join("b")).unwrap(),
        "exact source\n"
    );
}
#[test]
fn cyclic_links_are_rejected() {
    let links = BTreeMap::from([
        (PathBuf::from("a"), PathBuf::from("b")),
        (PathBuf::from("b"), PathBuf::from("a")),
    ]);
    assert!(resolve_archive_path(Path::new("a"), &links, &mut BTreeSet::new()).is_err());
}
#[test]
fn misspelled_manifest_fields_fail_closed() {
    assert!(toml::from_str::<Check>("kind='cargo'\nall_feature=true").is_err());
    assert!(toml::from_str::<Manifest>("schema=1\nproject='fixture'\ncheck=[]\n[checks]").is_err());
}
#[test]
fn windows_javascript_launchers_are_explicit() {
    assert_eq!(javascript_executable("npm", true), "npm.cmd");
    assert_eq!(javascript_executable("pnpm", true), "pnpm.cmd");
    assert_eq!(javascript_executable("bun", true), "bun");
    assert_eq!(javascript_executable("npm", false), "npm");
}
#[cfg(not(windows))]
#[test]
fn absent_or_unknown_nix_checks_never_turn_green() {
    let temp = TempDir::new().unwrap();
    let mut runner =
        Runner::new(temp.path().into(), environment(&[]), Duration::from_secs(1)).unwrap();
    runner.nix_inventory = Some(("x86_64-linux".into(), Vec::new()));
    let list: Check = toml::from_str("kind='nix'\nmode='list'").unwrap();
    assert!(nix_check(&list, &mut runner).is_err());
    runner.nix_inventory = Some(("x86_64-linux".into(), strings(&["real"])));
    let named: Check = toml::from_str("kind='nix'\nmode='named'\nchecks=['absent']").unwrap();
    assert!(nix_check(&named, &mut runner).is_err());
    let expected: Check =
        toml::from_str("kind='nix'\nmode='list'\nexpected_checks=['absent']").unwrap();
    assert!(nix_check(&expected, &mut runner).is_err());
    nix_check(&list, &mut runner).unwrap();
}

#[cfg(target_os = "linux")]
struct FixtureGroupCleanup(PathBuf);
#[cfg(target_os = "linux")]
impl Drop for FixtureGroupCleanup {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(&self.0) {
            if let Ok(pid) = pid.trim().parse::<u32>() {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{pid}")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

/// `fleet` is skipped while false: every existing check keeps its exact serialized identity.
#[test]
fn the_fleet_field_serializes_only_when_declared() {
    let plain: Check = toml::from_str("kind='commands'\ncommands=[['true']]").unwrap();
    let value = serde_json::to_value(&plain).unwrap();
    assert!(value.get("fleet").is_none(), "{value}");
    assert_eq!(
        serde_json::to_string(&plain).unwrap(),
        r#"{"kind":"commands","actions":null,"toolchain":null,"workspace":false,"all_features":false,"all_targets":false,"release":false,"features":[],"packages":[],"exclude":[],"test_runner":null,"mode":null,"expected_checks":null,"checks":[],"manager":null,"scripts":null,"install":null,"commands":[["true"]],"cache_outputs":[],"cache_tools":[],"cache_env":[],"cache_commit":false,"cache_inputs":null,"cache":null}"#
    );
    let reads: Check = toml::from_str("kind='commands'\nfleet=true\ncommands=[['true']]").unwrap();
    assert_eq!(serde_json::to_value(&reads).unwrap()["fleet"], json!(true));
    let explicit: Check =
        toml::from_str("kind='commands'\nfleet=false\ncommands=[['true']]").unwrap();
    assert_eq!(
        serde_json::to_vec(&explicit).unwrap(),
        serde_json::to_vec(&plain).unwrap()
    );
}

#[test]
fn a_fleet_check_declares_the_digest_in_cache_env_exactly_once() {
    let temp = TempDir::new().unwrap();
    fs::write(
        temp.path().join("ccid.toml"),
        "schema=1\nproject='fleet'\n\
         [checks.reads]\nkind='commands'\nfleet=true\ncommands=[['true']]\n\
         [checks.listed]\nkind='commands'\nfleet=true\ncache_env=['CCID_FLEET_DIGEST','X']\ncommands=[['true']]\n\
         [checks.plain]\nkind='commands'\ncommands=[['true']]\n",
    )
    .unwrap();
    let (manifest, _) = load_manifest(temp.path(), Path::new("ccid.toml")).unwrap();
    let count = |name: &str| {
        manifest.checks[name]
            .cache_env
            .iter()
            .filter(|entry| entry.as_str() == fleet::FLEET_DIGEST_ENV)
            .count()
    };
    assert_eq!(count("reads"), 1);
    assert_eq!(count("listed"), 1);
    assert_eq!(
        manifest.checks["listed"].cache_env,
        strings(&["CCID_FLEET_DIGEST", "X"])
    );
    assert_eq!(count("plain"), 0);
    assert!(manifest.checks["plain"].cache_env.is_empty());
}

/// ccid's own manifest: no check declares fleet data, so no declaration changes.
#[test]
fn this_repositorys_checks_keep_their_declarations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (manifest, _) = load_manifest(root, Path::new(".ci/ccid.toml")).unwrap();
    for (name, check) in &manifest.checks {
        assert!(!check.fleet, "{name}");
        let value = serde_json::to_value(check).unwrap();
        assert!(value.get("fleet").is_none(), "{name}: {value}");
        assert!(
            !check
                .cache_env
                .iter()
                .any(|entry| entry == fleet::FLEET_DIGEST_ENV),
            "{name}"
        );
    }
}

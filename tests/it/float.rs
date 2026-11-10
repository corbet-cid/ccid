//! The float of first-party git dependencies happens OUTSIDE the declared source (HR1) and moves
//! only the selected first-party packages (HR2): `ccid float --into`, `ccid check` and
//! `ccid cached` against a real local git dependency served through `url.*.insteadOf` (no
//! network), with a real Cargo behind an argv-recording shim.
#![cfg(unix)]

use serde_json::Value;
use std::{
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

const BINARY: &str = env!("CARGO_BIN_EXE_ccid");
const DEP: &str = "https://forge.example/org/dep";
const FOREIGN: &str = "https://forge.example/other/x";
const DEP_SPEC: &str = "update -p git+https://forge.example/org/dep?branch=main#dep@0.1.0";

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=float-test",
            "-c",
            "user.email=float-test@example.invalid",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit_all(repo: &Path, message: &str) -> String {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", message]);
    git(repo, &["rev-parse", "HEAD"])
}

fn text(output: &Output) -> String {
    format!(
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The real Cargo: the one running this test, else the first on PATH.
fn real_cargo() -> PathBuf {
    std::env::var_os("CARGO")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|directory| directory.join("cargo"))
                .find(|candidate| candidate.is_file())
        })
        .expect("cargo")
}

/// Two crates served from bare repositories (`dep` first-party, `x` third-party), a consumer
/// whose lock pins both at their first commit, fleet data naming `https://forge.example/org/`.
struct World {
    temp: TempDir,
    channel: PathBuf,
    consumer: PathBuf,
    /// The first commit of `dep`, which the consumer's lock pins.
    pinned: String,
}

impl World {
    fn new(forge_hosts: &[&str]) -> Self {
        let temp = TempDir::new().unwrap();
        let world = Self {
            channel: temp.path().join("channel.json"),
            consumer: temp.path().join("consumer"),
            pinned: String::new(),
            temp,
        };
        fs::write(
            &world.channel,
            serde_json::json!({"fleet": {
                "schema": 1,
                "forge_hosts": forge_hosts,
                "first_party": ["https://forge.example/org/"],
            }})
            .to_string(),
        )
        .unwrap();
        let shim = world.temp.path().join("shim");
        fs::create_dir(&shim).unwrap();
        let script = shim.join("cargo");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FLOAT_TEST_CARGO_LOG\"\nexec \"$FLOAT_TEST_REAL_CARGO\" \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        for directory in ["home", "cargo-home", "tmp", "target"] {
            fs::create_dir(world.temp.path().join(directory)).unwrap();
            fs::set_permissions(
                world.temp.path().join(directory),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let mut world = world;
        world.pinned = world.publish("dep", "1");
        world.publish("x", "1");
        let consumer = &world.consumer;
        fs::create_dir_all(consumer.join("src")).unwrap();
        fs::create_dir_all(consumer.join(".ci")).unwrap();
        fs::write(
            consumer.join("Cargo.toml"),
            format!(
                "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndep = {{ git = \"{DEP}\", branch = \"main\" }}\nx = {{ git = \"{FOREIGN}\", branch = \"main\" }}\n"
            ),
        )
        .unwrap();
        fs::write(consumer.join("src/lib.rs"), "").unwrap();
        fs::write(consumer.join(".gitignore"), ".ccid/\n.moon/\nmoon.yml\n").unwrap();
        // What the check builds: the dep commit of the lock in its working directory, and what
        // git says about the tree it runs in.
        fs::write(
            consumer.join("show.sh"),
            "set -eu\nawk '/^name = \"dep\"$/ {f=1} f && /^source = / {print \"dep-source=\" $3; exit}' Cargo.lock\necho \"head=$(git rev-parse HEAD)\"\necho \"files=$(git ls-files | tr '\\n' ' ')\"\n",
        )
        .unwrap();
        fs::write(
            consumer.join(".ci/ccid.toml"),
            "schema = 1\nproject = 'float-fixture'\n\n[checks.show]\nkind = 'commands'\ncommands = [['sh', 'show.sh']]\n",
        )
        .unwrap();
        let generate = Command::new(real_cargo())
            .args(["generate-lockfile", "--quiet"])
            .current_dir(consumer)
            .envs(world.environment())
            .output()
            .unwrap();
        assert!(generate.status.success(), "{}", text(&generate));
        git(consumer, &["init", "-q"]);
        commit_all(consumer, "consumer");
        world
    }

    /// Commit `crate_name` with marker `version` and push it to its bare repository.
    fn publish(&self, crate_name: &str, marker: &str) -> String {
        let work = self.temp.path().join("work").join(crate_name);
        let bare = self
            .temp
            .path()
            .join("git")
            .join(format!("{crate_name}.git"));
        if !work.exists() {
            fs::create_dir_all(work.join("src")).unwrap();
            fs::write(
                work.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{crate_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
                ),
            )
            .unwrap();
            git(&work, &["init", "-q"]);
            fs::create_dir_all(bare.parent().unwrap()).unwrap();
            git(
                bare.parent().unwrap(),
                &[
                    "init",
                    "-q",
                    "--bare",
                    bare.file_name().unwrap().to_str().unwrap(),
                ],
            );
        }
        fs::write(
            work.join("src/lib.rs"),
            format!("pub fn marker() -> &'static str {{ \"{marker}\" }}\n"),
        )
        .unwrap();
        let commit = commit_all(&work, marker);
        git(&work, &["push", "-q", bare.to_str().unwrap(), "HEAD:main"]);
        commit
    }

    fn cargo_log(&self) -> PathBuf {
        self.temp.path().join("cargo.log")
    }

    fn cargo_calls(&self) -> Vec<String> {
        fs::read_to_string(self.cargo_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Everything a run needs, isolated from the caller's CI, cache and fleet state: Cargo
    /// reaches the bare repositories through `url.*.insteadOf` via the git CLI.
    fn environment(&self) -> Vec<(OsString, OsString)> {
        let root = self.temp.path();
        let path = std::env::join_paths(
            std::iter::once(root.join("shim"))
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let mut environment: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), path),
            ("CARGO_HOME".into(), root.join("cargo-home").into()),
            ("CARGO_TARGET_DIR".into(), root.join("target").into()),
            ("CARGO_NET_GIT_FETCH_WITH_CLI".into(), "true".into()),
            ("TMPDIR".into(), root.join("tmp").into()),
            ("GIT_CONFIG_COUNT".into(), "2".into()),
            (
                "GIT_CONFIG_KEY_0".into(),
                format!("url.file://{}/git/dep.git.insteadOf", root.display()).into(),
            ),
            ("GIT_CONFIG_VALUE_0".into(), DEP.into()),
            (
                "GIT_CONFIG_KEY_1".into(),
                format!("url.file://{}/git/x.git.insteadOf", root.display()).into(),
            ),
            ("GIT_CONFIG_VALUE_1".into(), FOREIGN.into()),
            ("FLOAT_TEST_CARGO_LOG".into(), self.cargo_log().into()),
            ("FLOAT_TEST_REAL_CARGO".into(), real_cargo().into()),
            ("CI_JOBS".into(), "1".into()),
            ("CI_NIX_JOBS".into(), "1".into()),
            ("CI_TIMEOUT".into(), "300".into()),
        ];
        environment.push(("CCID_FLEET_FILE".into(), self.channel.clone().into()));
        environment
    }

    fn ccid(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BINARY);
        command.args(args).current_dir(&self.consumer);
        for name in [
            "CCID_RESULT_CACHE",
            "CCID_REMOTE_CACHE",
            "CCID_CACHE_CHILD",
            "CCID_CACHE_REPLAY_ONLY",
            "CCID_TARGET_LOCK_HELD",
            "CCID_RECEIPT",
            "CCID_CHANNEL_FILE",
            "CCID_FLEET_DIGEST",
            "CCID_FLEET_KEYED",
            "CCID_FORGE_HOSTS",
            "CCID_GIT_RESOLUTION",
            "CCID_RESOLVED_LOCKS",
            "CI",
            "CI_COMMIT_SHA",
            "CI_REPOSITORY_URL",
            "CI_MIN_AVAILABLE_MB",
            "CI_MEMORY_MB",
            "CI_MEMORY_PER_JOB_MB",
            "XDG_RUNTIME_DIR",
        ] {
            command.env_remove(name);
        }
        command.envs(self.environment());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.ccid(args).output().unwrap()
    }

    fn lock(&self) -> Vec<u8> {
        fs::read(self.consumer.join("Cargo.lock")).unwrap()
    }

    /// cmnp's declared-source digest of the consumer (every `git ls-files --cached --others`
    /// file, `Cargo.lock` included): what a moon task is keyed on and checked against.
    fn source_digest(&self) -> String {
        let check = cmnp::executor::Check {
            kind: "commands".into(),
            commands: vec![vec!["sh".into(), "show.sh".into()]],
            ..Default::default()
        };
        cmnp::inputs::source_digest(&self.consumer, &check, &std::env::vars_os().collect()).unwrap()
    }

    /// The declared source as one value: cmnp's digest and the raw lock bytes.
    fn declared(&self) -> (String, Vec<u8>) {
        (self.source_digest(), self.lock())
    }

    fn float(&self, into: &Path) -> Value {
        let output = self.run(&["float", "--repo", ".", "--into", into.to_str().unwrap()]);
        assert!(output.status.success(), "{}", text(&output));
        let stdout = String::from_utf8(output.stdout).unwrap();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 1, "one JSON line: {stdout}");
        serde_json::from_str(lines[0]).unwrap()
    }
}

fn source_of(lock: &[u8], name: &str) -> String {
    let lock: toml::Value = toml::from_str(std::str::from_utf8(lock).unwrap()).unwrap();
    lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"].as_str() == Some(name))
        .and_then(|package| package["source"].as_str())
        .unwrap()
        .to_owned()
}

fn events(output: &Output, name: &str) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["event"] == name)
        .collect()
}

#[test]
fn float_into_resolves_outside_and_moves_only_the_first_party_package() {
    let world = World::new(&["forge.example"]);
    let declared = world.declared();
    let newest = world.publish("dep", "2");
    let foreign_pin = source_of(&world.lock(), "x");
    world.publish("x", "2");
    let out = world.temp.path().join("resolved");
    let line = world.float(&out);
    assert_eq!(line["packages"], 1, "{line}");
    let digest = line["digest"].as_str().unwrap();
    assert_eq!(digest.len(), 64);
    assert_eq!(
        line["commits"][format!("Cargo.lock:dep:{DEP}")],
        newest.as_str(),
        "{line}"
    );
    let resolved = fs::read(out.join("Cargo.lock")).unwrap();
    assert_eq!(
        source_of(&resolved, "dep"),
        format!("git+{DEP}?branch=main#{newest}")
    );
    // HR2: the third-party git package keeps its pin although its branch moved too, and Cargo
    // was asked for exactly one `update -p <first-party spec>`.
    assert_eq!(source_of(&resolved, "x"), foreign_pin);
    assert_eq!(world.cargo_calls(), [DEP_SPEC]);
    assert_eq!(line["third_party"], serde_json::json!([]));
    // HR1: the declared source is byte-identical.
    assert_eq!(world.declared(), declared);
    assert!(source_of(&world.lock(), "dep").ends_with(&world.pinned));
}

#[test]
fn check_builds_the_float_in_a_stage_and_never_writes_the_checkout() {
    let world = World::new(&["forge.example"]);
    let declared = world.declared();
    let head = git(&world.consumer, &["rev-parse", "HEAD"]);
    let listed = git(&world.consumer, &["ls-files"]).replace('\n', " ");
    let newest = world.publish("dep", "2");
    world.publish("x", "2");
    let output = world.run(&["check", "--repo", ".", "--check", "show"]);
    assert!(output.status.success(), "{}", text(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The check built the newest first-party commit ...
    assert!(
        stdout.contains(&format!("dep-source=\"git+{DEP}?branch=main#{newest}\"")),
        "{}",
        text(&output)
    );
    // ... in a stage whose git names the real commit and lists the committed tree.
    assert!(
        stdout.contains(&format!("head={head}")),
        "{}",
        text(&output)
    );
    assert!(
        stdout.contains(&format!("files={listed} ")),
        "{}",
        text(&output)
    );
    assert_eq!(events(&output, "float-resolve").len(), 1);
    assert_eq!(events(&output, "float-stage").len(), 1);
    assert!(events(&output, "float-locks-moved").is_empty());
    assert_eq!(world.cargo_calls(), [DEP_SPEC]);
    // HR1: the checkout (and its repository) stays exactly as committed.
    assert_eq!(world.declared(), declared);
    assert_eq!(
        git(
            &world.consumer,
            &["status", "--porcelain", "--untracked-files=all"]
        ),
        ""
    );
    assert!(!world.consumer.join(".git/worktrees").exists());
}

#[test]
fn cached_builds_the_float_through_the_executor_and_never_writes_the_checkout() {
    let world = World::new(&["forge.example"]);
    let declared = world.declared();
    let newest = world.publish("dep", "2");
    // `--force` takes cmnp's uncached route whether or not moon is installed: the child
    // `ccid check` must build the parent's float from the hand-over, never resolve again.
    let output = world.run(&["cached", "--repo", ".", "--check", "show", "--force"]);
    assert!(output.status.success(), "{}", text(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("dep-source=\"git+{DEP}?branch=main#{newest}\"")),
        "{}",
        text(&output)
    );
    assert_eq!(events(&output, "float-resolve").len(), 1, "one float");
    assert_eq!(world.cargo_calls(), [DEP_SPEC], "the child never resolves");
    assert_eq!(world.declared(), declared);
}

#[test]
fn the_resolution_digest_follows_the_dependency_and_never_the_checkout() {
    let world = World::new(&["forge.example"]);
    let declared = world.declared();
    world.publish("dep", "2");
    let first = world.float(&world.temp.path().join("a"));
    let again = world.float(&world.temp.path().join("b"));
    assert_eq!(
        first["digest"], again["digest"],
        "no dependency change, same digest"
    );
    assert_eq!(
        fs::read(world.temp.path().join("a/Cargo.lock")).unwrap(),
        fs::read(world.temp.path().join("b/Cargo.lock")).unwrap()
    );
    let third = world.publish("dep", "3");
    let moved = world.float(&world.temp.path().join("c"));
    assert_ne!(moved["digest"], first["digest"]);
    assert_eq!(
        moved["commits"][format!("Cargo.lock:dep:{DEP}")],
        third.as_str()
    );
    assert_eq!(world.declared(), declared);
    assert_eq!(world.cargo_calls(), [DEP_SPEC, DEP_SPEC, DEP_SPEC]);
}

#[test]
fn a_child_builds_the_handed_over_locks_and_refuses_tampered_ones() {
    let world = World::new(&["forge.example"]);
    let newest = world.publish("dep", "2");
    let hand_over = world.temp.path().join("hand-over");
    let line = world.float(&hand_over);
    let digest = line["digest"].as_str().unwrap().to_owned();
    let calls = world.cargo_calls().len();
    let child = |locks: Option<&Path>| {
        let mut command = world.ccid(&["check", "--repo", ".", "--check", "show"]);
        command
            .env("CCID_CACHE_CHILD", "1")
            .env("CCID_GIT_RESOLUTION", &digest)
            .env_remove("CCID_FLEET_FILE");
        if let Some(locks) = locks {
            command.env("CCID_RESOLVED_LOCKS", locks);
        }
        command.output().unwrap()
    };
    // The moon task builds exactly what it was keyed with, without fleet data or Cargo.
    let output = child(Some(&hand_over));
    assert!(output.status.success(), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains(&format!("dep-source=\"git+{DEP}?branch=main#{newest}\"")));
    assert_eq!(world.cargo_calls().len(), calls, "never resolves again");
    // A tampered lock is another resolution: exit 2.
    let lock = hand_over.join("Cargo.lock");
    let original = fs::read(&lock).unwrap();
    fs::write(
        &lock,
        String::from_utf8(original.clone())
            .unwrap()
            .replace(&newest, &world.pinned),
    )
    .unwrap();
    let output = child(Some(&hand_over));
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&digest),
        "{}",
        text(&output)
    );
    // A digest without its locks: exit 2, naming the missing hand-over.
    fs::write(&lock, original).unwrap();
    let output = child(None);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("CCID_RESOLVED_LOCKS"),
        "{}",
        text(&output)
    );
}

#[test]
fn a_branch_tracking_lock_without_fleet_data_is_refused() {
    let world = World::new(&["forge.example"]);
    let declared = world.declared();
    for args in [
        &["check", "--repo", ".", "--check", "show"][..],
        &["float", "--repo", ".", "--into", "../out"][..],
    ] {
        let output = world
            .ccid(args)
            .env_remove("CCID_FLEET_FILE")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", text(&output));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("CCID_FLEET_FILE"),
            "{args:?}: {}",
            text(&output)
        );
    }
    assert!(world.cargo_calls().is_empty());
    assert_eq!(world.declared(), declared);
}

#[test]
fn a_repository_without_branch_tracking_dependencies_runs_as_before() {
    let world = World::new(&["forge.example"]);
    // One dependency pinned to a revision, no other git dependency: nothing tracks a branch.
    fs::write(
        world.consumer.join("Cargo.toml"),
        format!(
            "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndep = {{ git = \"{DEP}\", rev = \"{}\" }}\n",
            world.pinned
        ),
    )
    .unwrap();
    let generate = Command::new(real_cargo())
        .args(["generate-lockfile", "--quiet"])
        .current_dir(&world.consumer)
        .envs(world.environment())
        .output()
        .unwrap();
    assert!(generate.status.success(), "{}", text(&generate));
    commit_all(&world.consumer, "pinned");
    let declared = world.declared();
    let output = world
        .ccid(&["check", "--repo", ".", "--check", "show"])
        .env_remove("CCID_FLEET_FILE")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    assert!(events(&output, "float-resolve").is_empty());
    assert!(events(&output, "float-stage").is_empty());
    assert!(world.cargo_calls().is_empty(), "no Cargo call at all");
    assert_eq!(world.declared(), declared);
}

#[test]
fn into_inside_the_repository_is_refused() {
    let world = World::new(&["forge.example"]);
    world.publish("dep", "2");
    let declared = world.declared();
    for into in ["resolved", ".", "src/locks"] {
        let output = world.run(&["float", "--repo", ".", "--into", into]);
        assert_eq!(output.status.code(), Some(2), "{into}: {}", text(&output));
    }
    assert_eq!(world.declared(), declared);
    assert!(!world.consumer.join("resolved").exists());
}

/// Locks name the canonical forge host while CI checks out through
/// another spelling of it. The repository URL never decides the selection, so lane and CI
/// key the same resolution.
#[test]
fn a_ci_spelling_of_the_forge_host_floats_and_keys_like_the_land() {
    let world = World::new(&["forge.example", "forge.internal.example"]);
    let newest = world.publish("dep", "2");
    let mut digests = Vec::new();
    for (index, url) in [
        "https://forge.internal.example/org/consumer",
        "https://forge.example/org/consumer",
    ]
    .into_iter()
    .enumerate()
    {
        let into = world.temp.path().join(format!("resolved-{index}"));
        let output = world
            .ccid(&["float", "--repo", ".", "--into", into.to_str().unwrap()])
            .env("CI_REPOSITORY_URL", url)
            .output()
            .unwrap();
        assert!(output.status.success(), "{url}: {}", text(&output));
        let line: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(line["packages"], 1, "{url}: {line}");
        assert_eq!(
            line["commits"][format!("Cargo.lock:dep:{DEP}")],
            newest.as_str()
        );
        digests.push((
            line["digest"].clone(),
            fs::read(into.join("Cargo.lock")).unwrap(),
        ));
    }
    assert_eq!(digests[0], digests[1]);
}

/// `ccid cached` with moon and the verdict store: a hit without a dependency change, a miss
/// with another resolution after one, the checkout unchanged and nothing skipped. Needs a
/// preinstalled moon AND a cmnp that forwards `CCID_RESOLVED_LOCKS` to moon tasks as plumbing
/// (cmnp e4bfd9c does not: the moon task then refuses with exit 2).
#[test]
#[ignore = "Run explicitly with CCID_TEST_MOON (a preinstalled moon) and a cmnp that forwards CCID_RESOLVED_LOCKS"]
fn cached_twice_hits_and_a_new_dependency_commit_misses() {
    let moon = PathBuf::from(std::env::var_os("CCID_TEST_MOON").expect("preinstalled moon"));
    let world = World::new(&["forge.example"]);
    let store = world.temp.path().join("results");
    fs::create_dir(&store).unwrap();
    let declared = world.declared();
    world.publish("dep", "2");
    let run = || {
        let path = std::env::join_paths(
            [
                world.temp.path().join("shim"),
                Path::new(BINARY).parent().unwrap().to_path_buf(),
                moon.parent().unwrap().to_path_buf(),
            ]
            .into_iter()
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        world
            .ccid(&["cached", "--repo", ".", "--check", "show"])
            .env("PATH", path)
            .env("CCID_RESULT_CACHE", &store)
            .output()
            .unwrap()
    };
    let digest_of = |output: &Output| events(output, "float-resolve")[0]["digest"].clone();
    let first = run();
    assert!(first.status.success(), "{}", text(&first));
    let second = run();
    assert!(second.status.success(), "{}", text(&second));
    assert_eq!(digest_of(&first), digest_of(&second));
    assert!(
        events(&second, "cache-check")
            .iter()
            .any(|event| event["verdict"] == "hit"),
        "{}",
        text(&second)
    );
    world.publish("dep", "3");
    let third = run();
    assert!(third.status.success(), "{}", text(&third));
    assert_ne!(digest_of(&third), digest_of(&first));
    assert!(!events(&third, "cache-check")
        .iter()
        .any(|event| event["verdict"] == "hit"));
    for output in [&first, &second, &third] {
        assert!(
            events(output, "cache-publication-skipped").is_empty(),
            "{}",
            text(output)
        );
        assert!(
            events(output, "verdict-skipped").is_empty(),
            "{}",
            text(output)
        );
    }
    assert_eq!(world.declared(), declared);
}

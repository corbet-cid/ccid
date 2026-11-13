//! Contributed projects (operating model R10, R11), end to end with the binary under test.
//!
//! The class is declared in a placement copy outside every repository (`CCID_PLACEMENT_FILE`). These tests prove, on
//! a clone of a "foreign" project: render writes nothing into it, no entry point of ours runs its CI, our own checks
//! on it write nothing into it and assume nothing of our infrastructure, and the compile caches serve our own work.
#![cfg(unix)]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

const BINARY: &str = env!("CARGO_BIN_EXE_ccid");
const PIN: &str = "1234567890abcdef1234567890abcdef12345678";
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// Their project on GitHub, and our fork of it on our forge.
const PLACEMENT: &str = r#"{"version":1,"default":"https://forge.example","primaries":{},"native":{"schema":1,
"source":{"origin":"https://forge.example","token_env":"T"},"repositories":[],
"contributed":[{"name":"their-project","upstream":"https://github.com/someone/their-project",
"forks":["https://forge.example/me/their-project"]}]}}"#;

/// A manifest that renders a Forgejo workflow: what would be written into a repository that carries it.
fn render_manifest() -> String {
    format!(
        "schema=1\nproject='fixture'\n[render]\ntool_revision='{PIN}'\n\
         [checks.test]\nkind='commands'\ncommands=[['true']]\n\
         [jobs.verify]\nscheduler='forgejo'\nchecks=['test']\nworkflow='ci'\ncommand=['ccid:run-declared-checks']\n"
    )
}

struct World {
    root: TempDir,
}

impl World {
    fn new() -> Self {
        let world = Self {
            root: TempDir::new().unwrap(),
        };
        fs::write(world.placement(), PLACEMENT).unwrap();
        fs::create_dir_all(world.scratch().join("home")).unwrap();
        // Their clone: it even carries a copy of our manifest, which must not make render write anything.
        world.clone_of("foreign", "https://github.com/someone/their-project.git");
        // An owned repository with the very same content.
        world.clone_of("owned", "https://forge.example/own/thing.git");
        world
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn placement(&self) -> PathBuf {
        self.path().join("placement.json")
    }

    fn scratch(&self) -> PathBuf {
        self.path().join("scratch")
    }

    fn foreign(&self) -> PathBuf {
        self.path().join("foreign")
    }

    fn owned(&self) -> PathBuf {
        self.path().join("owned")
    }

    fn clone_of(&self, name: &str, remote: &str) {
        let repo = self.path().join(name);
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join(".ci")).unwrap();
        fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        fs::write(repo.join(".ci/ccid.toml"), render_manifest()).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["remote", "add", "origin", remote]);
        git(&repo, &["add", "-A"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
    }

    /// An overlay manifest, outside both clones.
    fn overlay(&self, checks: &str) -> PathBuf {
        let directory = self.path().join("overlay");
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("their-project.toml");
        fs::write(
            &file,
            format!("schema = 1\nproject = 'their-project'\n{checks}"),
        )
        .unwrap();
        file
    }

    /// The binary under test with a clean, explicit environment: nothing inherited from a runner or a developer.
    fn ccid(&self) -> Command {
        let mut command = Command::new(BINARY);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", self.scratch().join("home"))
            .env("TMPDIR", self.scratch())
            .env("CARGO_TARGET_DIR", self.scratch().join("target"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("CI_JOBS", "1")
            .env("CI_NIX_JOBS", "1")
            .env("CI_TIMEOUT", "120")
            .env("CCID_PLACEMENT_FILE", self.placement());
        command
    }
}

fn git(directory: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(directory)
        // git's detached background maintenance would change the clone after the snapshot of it.
        .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
        .args(arguments)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {arguments:?}");
}

/// Every path under `root` (the `.git` directory included) with its bytes, or its link target.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(base: &Path, directory: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(base).unwrap().to_owned();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                out.insert(relative.clone(), b"<dir>".to_vec());
                walk(base, &path, out);
            } else if meta.file_type().is_symlink() {
                out.insert(
                    relative,
                    fs::read_link(&path)
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                );
            } else {
                out.insert(relative, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Fail naming the paths that differ, never dumping their bytes.
#[track_caller]
fn assert_unchanged(before: &BTreeMap<PathBuf, Vec<u8>>, root: &Path, what: &str) {
    let after = snapshot(root);
    let mut changed: Vec<String> = Vec::new();
    for (path, bytes) in before {
        match after.get(path) {
            None => changed.push(format!("removed {}", path.display())),
            Some(now) if now != bytes => changed.push(format!("modified {}", path.display())),
            _ => {}
        }
    }
    for path in after.keys().filter(|p| !before.contains_key(*p)) {
        changed.push(format!("added {}", path.display()));
    }
    assert!(changed.is_empty(), "{what}: {changed:?}");
}

/// What git sees as changed or untracked, ignored files included.
fn dirty(repo: &Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=all",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn refused(output: &Output) -> bool {
    output.status.code() == Some(2) && text(&output.stderr).contains("contributed project")
}

// ---- R11: nothing of ours enters their repository ----------------------------------------------------------

#[test]
fn render_writes_nothing_into_a_contributed_project() {
    let world = World::new();
    let before = snapshot(&world.foreign());
    for arguments in [vec!["render"], vec!["render", "--check"]] {
        let output = world
            .ccid()
            .current_dir(world.foreign())
            .args(&arguments)
            .output()
            .unwrap();
        assert!(refused(&output), "{arguments:?}: {}", text(&output.stderr));
        assert!(
            text(&output.stderr).contains("render refused"),
            "{}",
            text(&output.stderr)
        );
    }
    // Pointed at it from outside as well.
    let output = world
        .ccid()
        .args(["render", "--repo"])
        .arg(world.foreign())
        .output()
        .unwrap();
    assert!(refused(&output));
    assert_unchanged(&before, &world.foreign(), "the clone");
    assert_eq!(dirty(&world.foreign()), "");
    assert!(!world.foreign().join(".forgejo").exists());
    assert!(!world.foreign().join(".ci/jobs.json").exists());
}

#[test]
fn render_is_refused_through_our_fork_of_it_too() {
    // A clone of OUR fork on our forge: its remote is the fork, the placement lists the fork.
    let world = World::new();
    world.clone_of("fork", "git@forge.example:me/their-project.git");
    let before = snapshot(&world.path().join("fork"));
    let output = world
        .ccid()
        .args(["render", "--repo"])
        .arg(world.path().join("fork"))
        .output()
        .unwrap();
    assert!(refused(&output), "{}", text(&output.stderr));
    assert_unchanged(&before, &world.path().join("fork"), "the fork clone");
}

#[test]
fn render_still_works_on_an_owned_project_of_the_same_placement() {
    let world = World::new();
    let output = world
        .ccid()
        .args(["render", "--repo"])
        .arg(world.owned())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(world.owned().join(".forgejo/workflows/ci.yml").is_file());
}

#[test]
fn without_a_placement_nothing_is_classified() {
    // A host that has no placement copy keeps working: the class is declared, never guessed.
    let world = World::new();
    let output = world
        .ccid()
        .env_remove("CCID_PLACEMENT_FILE")
        .args(["render", "--repo"])
        .arg(world.foreign())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(world.foreign().join(".forgejo/workflows/ci.yml").is_file());
}

#[test]
fn a_placement_that_is_set_but_unusable_stops_the_work_instead_of_switching_the_guard_off() {
    let world = World::new();
    let bad = world.path().join("bad.json");
    for (content, wanted) in [
        (Some("{"), "not valid JSON"),
        (
            Some(r#"{"native":{"repositories":[],"contributed":[{"name":"x"}]}}"#),
            "needs an upstream",
        ),
        (None, "bad.json"),
    ] {
        match content {
            Some(content) => fs::write(&bad, content).unwrap(),
            None => {
                let _ = fs::remove_file(&bad);
            }
        }
        let before = snapshot(&world.owned());
        let output = world
            .ccid()
            .env("CCID_PLACEMENT_FILE", &bad)
            .args(["render", "--repo"])
            .arg(world.owned())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
        assert!(
            text(&output.stderr).contains(wanted),
            "{}",
            text(&output.stderr)
        );
        assert_unchanged(&before, &world.owned(), "the owned clone");
    }
}

// ---- R10, R11: their CI and their releases never run on our compute -------------------------------------

#[test]
fn no_entry_point_of_ours_runs_or_publishes_for_a_contributed_project() {
    let world = World::new();
    let foreign = world.foreign();
    let foreign_text = foreign.to_str().unwrap();
    let into = world.path().join("fetched");
    let into_text = into.to_str().unwrap();
    let before = snapshot(&foreign);
    let request = world.path().join("request.json");
    fs::write(
        &request,
        format!(
            r#"{{"archive":"a","sha256":"{SHA}","commit":"{SHA}","bundle":"b","bundle_sha256":"{SHA}","tool_revision":"x","job":"verify","environment":{{"CI_REPOSITORY_URL":"https://forge.example/me/their-project.git"}}}}"#
        ),
    )
    .unwrap();
    let cases: Vec<Vec<&str>> = vec![
        vec!["check", "--repo", foreign_text, "--check", "test"],
        vec!["cached", "--repo", foreign_text, "--check", "test"],
        vec!["lock-refresh", "--repo", foreign_text],
        vec!["release", "build", "--repo", foreign_text],
        vec!["release", "preflight", "--repo", foreign_text],
        vec!["release", "leg", "windows", "--repo", foreign_text],
        vec!["release", "promote", "--repo", foreign_text],
        vec!["release", "discard", "--repo", foreign_text],
        vec!["release", "put", "somefile", "--repo", foreign_text],
        vec![
            "release",
            "fetch",
            "--into",
            into_text,
            "--repo",
            foreign_text,
        ],
        vec!["crow-ci", "plan", "--repo", foreign_text, "--workflow", "w"],
        vec!["crow-ci", "run", "--repo", foreign_text, "--workflow", "w"],
        vec!["ci-job", "plan", "--repo", foreign_text, "--job", "verify"],
        vec!["ci-job", "run", "--repo", foreign_text, "--job", "verify"],
        // A forge job of ours, by the forge's own identity of the repository: our fork of it.
        vec![
            "job-run",
            "--job",
            "verify",
            "--repository",
            "me/their-project",
            "--sha",
            SHA,
            "--server-url",
            "https://forge.example/",
            "--owner",
            "me",
        ],
        // An export of its source handed to the land gate's entry point.
        vec![
            "job-run",
            "--job",
            "verify",
            "--repository",
            "own/thing",
            "--sha",
            SHA,
            "--server-url",
            "https://forge.example/",
            "--owner",
            "own",
            "--local",
            "--source",
            foreign_text,
        ],
        vec![
            "verdict",
            "--commit",
            SHA,
            "--job",
            "verify",
            "--state",
            "success",
            "--url",
            "u",
            "--started",
            "1",
            "--binary",
            "/bin/true",
            "--binary-sha256",
            SHA,
            "--repo",
            "me/their-project",
        ],
        vec![
            "push-run",
            "--consumer-url",
            "https://github.com/someone/their-project",
            "--consumer-branch",
            "main",
            "--job",
            "verify",
            "--trigger-sha",
            SHA,
        ],
        vec!["execute-job", "--request", request.to_str().unwrap()],
    ];
    for arguments in cases {
        let output = world
            .ccid()
            .env("CCID_FORGE_TOKEN", "unused")
            // `push-run` wants a persistent cache root before it reaches the guard.
            .env("CARGO_HOME", world.scratch().join("cargo"))
            .args(&arguments)
            .output()
            .unwrap();
        assert!(
            refused(&output),
            "{arguments:?} was not refused: {:?} {}",
            output.status,
            text(&output.stderr)
        );
    }
    assert_unchanged(&before, &foreign, "an entry point touched the clone");
}

#[test]
fn the_same_commands_are_not_refused_for_what_is_not_contributed() {
    // The refusal names the class, nothing else: other repositories and other forges get past it.
    let world = World::new();
    let output = world
        .ccid()
        .args([
            "job-run",
            "--job",
            "verify",
            "--repository",
            "me/their-project",
            "--sha",
            SHA,
            "--server-url",
            "https://elsewhere.example/",
            "--owner",
            "me",
        ])
        .output()
        .unwrap();
    assert!(
        !text(&output.stderr).contains("contributed project"),
        "{}",
        text(&output.stderr)
    );
    assert!(
        text(&output.stderr).contains("CCID_FORGE_TOKEN"),
        "{}",
        text(&output.stderr)
    );
    let output = world
        .ccid()
        .args(["lock-refresh", "--repo"])
        .arg(world.owned())
        .output()
        .unwrap();
    assert!(
        !text(&output.stderr).contains("contributed project"),
        "{}",
        text(&output.stderr)
    );
}

// ---- R11: our own work on their project ---------------------------------------------------------------

fn observed(world: &World) -> PathBuf {
    world.scratch().join("observed.env")
}

/// An overlay whose one check records the environment it runs in.
fn recording_overlay(world: &World) -> PathBuf {
    world.overlay(&format!(
        "[checks.env]\nkind = 'commands'\ncommands = [['sh', '-c', 'env > {}']]\n",
        observed(world).display()
    ))
}

#[test]
fn our_own_check_on_their_project_writes_nothing_into_it_and_passes_nothing_of_ours_on() {
    let world = World::new();
    let overlay = recording_overlay(&world);
    let cache = world.path().join("result-cache");
    fs::create_dir(&cache).unwrap();
    // A machine with fleet data: a good copy is read for the spellings of our forge, and goes no further.
    let channel = world.path().join("channel.json");
    fs::write(
        &channel,
        r#"{"default":{"revision":"r","sha256":"s"},"fleet":{"schema":1,"forge_hosts":["forge.example"],"first_party":["https://forge.example/org/"]}}"#,
    )
    .unwrap();
    let before = snapshot(&world.foreign());
    let output = world
        .ccid()
        // What a developer's machine of ours has, and what a runner has.
        .env("CCID_RESULT_CACHE", &cache)
        .env("CCID_REMOTE_CACHE", "grpc://cache.example")
        .env("CCID_FORGE_TOKEN", "secret-token")
        .env("CCID_FLEET_FILE", &channel)
        .env("CCID_TOOL_ROOT", "/nonexistent/tools")
        .env("CFRG_STATE_DIR", "/nonexistent/state")
        .env("CFRG_FORGEJO_TOKEN", "secret-token")
        .env("GITHUB_TOKEN", "secret-token")
        .env("GH_TOKEN", "secret-token")
        .env("FORGEJO_TOKEN", "secret-token")
        .env("NPM_TOKEN", "secret-token")
        .env("CARGO_REGISTRIES_HOME_TOKEN", "secret-token")
        .env(
            "CI_REPOSITORY_URL",
            "https://forge.example/me/their-project",
        )
        // The project's own environment.
        .env("THEIR_OWN_SETTING", "kept")
        .args(["contributed", "check", "--repo"])
        .arg(world.foreign())
        .arg("--manifest")
        .arg(&overlay)
        .args(["--check", "env"])
        .output()
        .unwrap();
    let stdout = text(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        text(&output.stderr)
    );
    assert!(stdout.contains(r#""event":"contributed""#), "{stdout}");
    assert!(stdout.contains(r#""project":"their-project""#), "{stdout}");
    assert!(stdout.contains("check-success"), "{stdout}");
    // The check ran with the project's environment and none of ours.
    let seen = fs::read_to_string(observed(&world)).unwrap();
    assert!(
        seen.lines().any(|l| l == "THEIR_OWN_SETTING=kept"),
        "{seen}"
    );
    for name in [
        "CCID_RESULT_CACHE",
        "CCID_REMOTE_CACHE",
        "CCID_FORGE_TOKEN",
        "CCID_FLEET_FILE",
        "CCID_TOOL_ROOT",
        "CCID_PLACEMENT_FILE",
        "CFRG_STATE_DIR",
        "CFRG_FORGEJO_TOKEN",
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "FORGEJO_TOKEN",
        "NPM_TOKEN",
        "CARGO_REGISTRIES_HOME_TOKEN",
        "CI_REPOSITORY_URL",
    ] {
        assert!(
            !seen.lines().any(|l| l.starts_with(&format!("{name}="))),
            "{name} reached the project's command:\n{seen}"
        );
    }
    assert!(
        !seen.contains("secret-token"),
        "a credential reached the project's command"
    );
    // No verdict was stored, and the clone is exactly as it was.
    assert_eq!(
        fs::read_dir(&cache).unwrap().count(),
        0,
        "the result cache was used"
    );
    assert_unchanged(&before, &world.foreign(), "the clone");
    assert_eq!(dirty(&world.foreign()), "");
}

#[test]
fn our_own_check_needs_no_cache_no_fleet_and_no_runner() {
    // A bare machine: no result cache, no remote cache, no fleet data, no compile cache, no tool root.
    let world = World::new();
    let overlay = recording_overlay(&world);
    let output = world
        .ccid()
        .args(["contributed", "check", "--repo"])
        .arg(world.foreign())
        .arg("--manifest")
        .arg(&overlay)
        .args(["--check", "env"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
    assert!(observed(&world).is_file());
    assert_eq!(dirty(&world.foreign()), "");
}

/// A directory with a compiler that identifies as GCC and a `ccache` that runs what it is given.
fn fake_toolchain(world: &World) -> PathBuf {
    let bin = world.path().join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    for (name, body) in [
        (
            "gcc",
            "#!/bin/sh\necho 'gcc (GNU) Free Software Foundation'\n",
        ),
        ("ccache", "#!/bin/sh\nexec \"$@\"\n"),
    ] {
        let file = bin.join(name);
        fs::write(&file, body).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

fn compile_cache_backend(stdout: &str) -> Option<serde_json::Value> {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["event"] == "compile-cache")
        .map(|event| event["backend"].clone())
}

#[test]
fn the_compile_cache_serves_our_own_work_when_it_is_provisioned() {
    let world = World::new();
    let overlay = recording_overlay(&world);
    let bin = fake_toolchain(&world);
    let ccache_dir = world.path().join("ccache");
    fs::create_dir(&ccache_dir).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let run = |ccache: bool| {
        let mut command = world.ccid();
        command.env("PATH", &path);
        if ccache {
            command.env("CCACHE_DIR", &ccache_dir);
        }
        command
            .args(["contributed", "check", "--repo"])
            .arg(world.foreign())
            .arg("--manifest")
            .arg(&overlay)
            .args(["--check", "env"])
            .output()
            .unwrap()
    };
    let provisioned = run(true);
    assert!(
        provisioned.status.success(),
        "{}",
        text(&provisioned.stderr)
    );
    assert_eq!(
        compile_cache_backend(&text(&provisioned.stdout)),
        Some(serde_json::json!("ccache")),
        "{}",
        text(&provisioned.stdout)
    );
    // Not provisioned: the same work runs, with no cache behind the compiler.
    let bare = run(false);
    assert!(bare.status.success(), "{}", text(&bare.stderr));
    assert_eq!(
        compile_cache_backend(&text(&bare.stdout)),
        Some(serde_json::Value::Null)
    );
    assert_eq!(dirty(&world.foreign()), "");
}

#[test]
fn a_failing_check_stays_failed() {
    let world = World::new();
    let overlay = world.overlay("[checks.bad]\nkind = 'commands'\ncommands = [['false']]\n");
    let output = world
        .ccid()
        .args(["contributed", "check", "--repo"])
        .arg(world.foreign())
        .arg("--manifest")
        .arg(&overlay)
        .args(["--check", "bad"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!text(&output.stdout).contains("check-success"));
}

#[test]
fn the_plan_of_an_overlay_check_runs_nothing() {
    let world = World::new();
    let overlay = recording_overlay(&world);
    let output = world
        .ccid()
        .args(["contributed", "check", "--plan", "--repo"])
        .arg(world.foreign())
        .arg("--manifest")
        .arg(&overlay)
        .args(["--check", "env"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(text(&output.stdout).contains(r#""event":"plan""#));
    assert!(!observed(&world).exists());
}

#[test]
fn a_forge_job_never_runs_our_checks_on_their_project() {
    // Their CI never runs on our compute: not even when a job is what calls the command.
    let world = World::new();
    let overlay = recording_overlay(&world);
    for variable in [
        "GITHUB_ACTIONS",
        "GITEA_ACTIONS",
        "FORGEJO_ACTIONS",
        "GITLAB_CI",
    ] {
        let output = world
            .ccid()
            .env(variable, "true")
            .args(["contributed", "check", "--repo"])
            .arg(world.foreign())
            .arg("--manifest")
            .arg(&overlay)
            .args(["--check", "env"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{variable}");
        let stderr = text(&output.stderr);
        assert!(
            stderr.contains(variable) && stderr.contains("never runs on our compute"),
            "{stderr}"
        );
    }
    assert!(!observed(&world).exists());
}

#[test]
fn the_overlay_must_live_outside_their_repository() {
    let world = World::new();
    let inside = world.foreign().join(".ci/overlay.toml");
    fs::write(
        &inside,
        "schema = 1\nproject = 'x'\n[checks.t]\nkind = 'commands'\ncommands = [['true']]\n",
    )
    .unwrap();
    let before = snapshot(&world.foreign());
    let output = world
        .ccid()
        .args(["contributed", "check", "--repo"])
        .arg(world.foreign())
        .arg("--manifest")
        .arg(&inside)
        .args(["--check", "t"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output.stderr).contains("must live outside"),
        "{}",
        text(&output.stderr)
    );
    assert_unchanged(&before, &world.foreign(), "the clone");
}

#[test]
fn only_a_declared_contributed_project_takes_the_overlay_route() {
    let world = World::new();
    let overlay = recording_overlay(&world);
    let output = world
        .ccid()
        .args(["contributed", "check", "--repo"])
        .arg(world.owned())
        .arg("--manifest")
        .arg(&overlay)
        .args(["--check", "env"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output.stderr).contains("not a declared contributed project"),
        "{}",
        text(&output.stderr)
    );
    assert!(!observed(&world).exists());
    // Without a placement there is no class to run under.
    let output = world
        .ccid()
        .env_remove("CCID_PLACEMENT_FILE")
        .args(["contributed", "check", "--repo"])
        .arg(world.foreign())
        .arg("--manifest")
        .arg(&overlay)
        .args(["--check", "env"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output.stderr).contains("CCID_PLACEMENT_FILE"),
        "{}",
        text(&output.stderr)
    );
    assert!(!observed(&world).exists());
}

// ---- the declaration -------------------------------------------------------------------------------------------

#[test]
fn which_says_whether_a_working_copy_is_contributed() {
    let world = World::new();
    let foreign = world
        .ccid()
        .args(["contributed", "which", "--repo"])
        .arg(world.foreign())
        .output()
        .unwrap();
    assert_eq!(foreign.status.code(), Some(0));
    let value: serde_json::Value = serde_json::from_slice(&foreign.stdout).unwrap();
    assert_eq!(value["contributed"], true);
    assert_eq!(value["project"], "their-project");
    assert_eq!(
        value["upstream"],
        "https://github.com/someone/their-project"
    );
    let owned = world
        .ccid()
        .args(["contributed", "which", "--repo"])
        .arg(world.owned())
        .output()
        .unwrap();
    assert_eq!(owned.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&owned.stdout).unwrap(),
        serde_json::json!({"contributed": false})
    );
    // Cannot tell: no placement.
    let unknown = world
        .ccid()
        .env_remove("CCID_PLACEMENT_FILE")
        .args(["contributed", "which", "--repo"])
        .arg(world.owned())
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(2));
}

#[test]
fn show_prints_the_declaration_and_its_digest() {
    let world = World::new();
    let output = world.ccid().args(["contributed", "show"]).output().unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["digest"].as_str().unwrap().len(), 64);
    assert_eq!(value["forge_hosts"], serde_json::json!(["forge.example"]));
    assert_eq!(value["projects"][0]["name"], "their-project");
    assert_eq!(
        value["projects"][0]["forks"],
        serde_json::json!(["https://forge.example/me/their-project"])
    );
    let missing = world
        .ccid()
        .env_remove("CCID_PLACEMENT_FILE")
        .args(["contributed", "show"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2));
}

#[test]
fn the_plain_routes_never_write_the_result_cache_into_their_clone() {
    // `ccid check` and `ccid cached` with a result cache write `.ccid` beside the sources; on a contributed project
    // they are refused and point at the overlay route.
    let world = World::new();
    let cache = world.path().join("result-cache");
    fs::create_dir(&cache).unwrap();
    let before = snapshot(&world.foreign());
    for route in ["check", "cached"] {
        let output = world
            .ccid()
            .env("CCID_RESULT_CACHE", &cache)
            .args([route, "--repo"])
            .arg(world.foreign())
            .args(["--check", "test"])
            .output()
            .unwrap();
        assert!(refused(&output), "{route}: {}", text(&output.stderr));
        assert!(
            text(&output.stderr).contains("ccid contributed check"),
            "{}",
            text(&output.stderr)
        );
    }
    assert_unchanged(&before, &world.foreign(), "the clone");
    assert_eq!(fs::read_dir(&cache).unwrap().count(), 0);
    // An owned project of the same placement keeps both routes.
    let output = world
        .ccid()
        .args(["check", "--plan", "--repo"])
        .arg(world.owned())
        .args(["--check", "test"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
}

#[test]
fn the_overlay_may_not_hide_in_a_subdirectory_of_their_repository() {
    // The project's top level is its root, however the working copy is named on the command line.
    let world = World::new();
    fs::create_dir_all(world.foreign().join("docs")).unwrap();
    let inside = world.foreign().join(".ci/overlay.toml");
    fs::write(
        &inside,
        "schema = 1\nproject = 'x'\n[checks.t]\nkind = 'commands'\ncommands = [['true']]\n",
    )
    .unwrap();
    let output = world
        .ccid()
        .args(["contributed", "check", "--plan", "--repo"])
        .arg(world.foreign().join("docs"))
        .arg("--manifest")
        .arg(&inside)
        .args(["--check", "t"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stdout));
    assert!(
        text(&output.stderr).contains("must live outside"),
        "{}",
        text(&output.stderr)
    );
}

#[test]
fn an_overlay_check_that_writes_into_the_working_copy_by_its_kind_is_refused() {
    let world = World::new();
    let overlay = world.overlay(
        "[checks.js]\nkind = 'javascript'\nmanager = 'npm'\nscripts = ['test']\n\
         [checks.fine]\nkind = 'commands'\ncommands = [['true']]\n",
    );
    let before = snapshot(&world.foreign());
    let output = world
        .ccid()
        .args(["contributed", "check", "--repo"])
        .arg(world.foreign())
        .arg("--manifest")
        .arg(&overlay)
        .args(["--check", "js"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("kind \"javascript\"") && stderr.contains("commands"),
        "{stderr}"
    );
    assert_unchanged(&before, &world.foreign(), "the clone");
    // A selection with one refused kind runs none of it.
    let mixed = world
        .ccid()
        .args(["contributed", "check", "--repo"])
        .arg(world.foreign())
        .arg("--manifest")
        .arg(&overlay)
        .args(["--check", "fine,js"])
        .output()
        .unwrap();
    assert_eq!(mixed.status.code(), Some(2));
    assert!(!text(&mixed.stdout).contains("check-start"));
}

#[test]
fn a_runners_own_name_for_the_forge_is_ours_too() {
    let world = World::new();
    let arguments = [
        "job-run",
        "--job",
        "verify",
        "--repository",
        "me/their-project",
        "--sha",
        SHA,
        "--server-url",
        "https://forge.internal/",
        "--owner",
        "me",
    ];
    // Unknown, the host names another forge; the job gets past the refusal (and stops at the missing token).
    let output = world.ccid().args(arguments).output().unwrap();
    assert!(
        !text(&output.stderr).contains("contributed project"),
        "{}",
        text(&output.stderr)
    );
    // The runner names it, as the runner slots do: refused.
    let output = world
        .ccid()
        .env("CCID_FORGE_HOSTS", "forge.internal")
        .args(arguments)
        .output()
        .unwrap();
    assert!(refused(&output), "{}", text(&output.stderr));
}

#[test]
fn a_remote_written_as_a_shorthand_is_read_as_git_expands_it() {
    let world = World::new();
    world.clone_of("shorthand", "gh:someone/their-project");
    git(
        &world.path().join("shorthand"),
        &["config", "url.https://github.com/.insteadOf", "gh:"],
    );
    let before = snapshot(&world.path().join("shorthand"));
    let output = world
        .ccid()
        .args(["render", "--repo"])
        .arg(world.path().join("shorthand"))
        .output()
        .unwrap();
    assert!(refused(&output), "{}", text(&output.stderr));
    assert_unchanged(&before, &world.path().join("shorthand"), "the clone");
}

#[test]
fn a_placement_that_is_not_one_stops_the_work() {
    // The git-pointer view has no repositories: it is not the placement, and must not read as "nothing contributed".
    let world = World::new();
    let wrong = world.path().join("pointer-view.json");
    fs::write(
        &wrong,
        r#"{"default":"https://forge.example","primaries":{},"version":1}"#,
    )
    .unwrap();
    let output = world
        .ccid()
        .env("CCID_PLACEMENT_FILE", &wrong)
        .args(["render", "--repo"])
        .arg(world.owned())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output.stderr).contains("not a placement"),
        "{}",
        text(&output.stderr)
    );
    // An empty variable is not an unset one.
    let output = world
        .ccid()
        .env("CCID_PLACEMENT_FILE", "")
        .args(["render", "--repo"])
        .arg(world.owned())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output.stderr).contains("set but empty"),
        "{}",
        text(&output.stderr)
    );
}

#[test]
fn a_channel_without_fleet_data_does_not_stop_the_guard() {
    // An ordinary state of a runner (the release channel carries no fleet data): the guard still classifies.
    let world = World::new();
    let channel = world.path().join("fleetless.json");
    fs::write(&channel, r#"{"default":{"revision":"r","sha256":"s"}}"#).unwrap();
    let owned = world
        .ccid()
        .env("CCID_FLEET_FILE", &channel)
        .args(["render", "--repo"])
        .arg(world.owned())
        .output()
        .unwrap();
    assert!(owned.status.success(), "{}", text(&owned.stderr));
    let foreign = world
        .ccid()
        .env("CCID_FLEET_FILE", &channel)
        .args(["render", "--repo"])
        .arg(world.foreign())
        .output()
        .unwrap();
    assert!(refused(&foreign), "{}", text(&foreign.stderr));
    // A channel that is not JSON is an error, not a smaller list.
    fs::write(&channel, "not json").unwrap();
    let broken = world
        .ccid()
        .env("CCID_FLEET_FILE", &channel)
        .args(["render", "--repo"])
        .arg(world.owned())
        .output()
        .unwrap();
    assert_eq!(broken.status.code(), Some(2));
    assert!(
        text(&broken.stderr).contains("CCID_FLEET_FILE"),
        "{}",
        text(&broken.stderr)
    );
}

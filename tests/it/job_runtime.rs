//! `ccid job-run` with `[runtime]`: the persistent job environment that replaces a repository's
//! `.ci/run-cached.sh`, against stub `nix`, `rustup` and `cargo` programs (no network, no
//! installs, no real toolchain).
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const REVISION: &str = "b7c2ada94fe99c15b0dbcf4d11fd7850b957a436";
const NAR_HASH: &str = "sha256-IItrdb7Puk05RqOBWZYFC5X6Wl1sJmCfh5MWVHw5iMM=";
const REPOSITORY_URL: &str = "https://forge.example/fixtures/repo";

fn git(repo: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().into()
}

/// Write an executable stub through a child process (no ETXTBSY from this process's own write
/// handle while a concurrent test forks).
fn stub(path: &Path, body: &str) {
    use std::io::Write;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut child = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    sha: String,
}

impl Fixture {
    /// A committed repository whose one job runs `check`, with `tables` appended to its manifest.
    fn new(check: &str, tables: &str) -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for dir in ["home", "cache", "cargo", "rustup", "tmp", "out", "log"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        for tool in ["moon", "proto", "python3"] {
            fs::create_dir_all(root.join("store").join(tool).join("bin")).unwrap();
        }
        let log = root.join("log");
        stub(
            &root.join("bin/nix"),
            &format!(
                "echo \"$*\" >> {log}/nix\nfor arg in \"$@\"; do case \"$arg\" in *'#'*) echo \"$FAKE_STORE/${{arg##*#}}\";; esac; done",
                log = log.display()
            ),
        );
        let cargo = root.join("rust/bin/cargo");
        stub(&cargo, "exit 0");
        stub(
            &root.join("bin/rustup"),
            &format!(
                r#"echo "$RUSTUP_HOME $*" >> {log}/rustup
case "$1 $2" in
  "run stable") exit 0 ;;
  "component list") printf 'rustfmt-x86_64-unknown-linux-gnu\nclippy-x86_64-unknown-linux-gnu\n' ;;
  "target list") echo wasm32-unknown-unknown ;;
  "which --toolchain") echo {cargo} ;;
  *) exit 3 ;;
esac"#,
                log = log.display(),
                cargo = cargo.display()
            ),
        );
        fs::write(
            root.join("channel.json"),
            r#"{"default":{"revision":"r","sha256":"s"},"fleet":{"schema":1,"first_party":["https://forge.example/new/"],"aliases":{"https://mirror.example/old/":"https://forge.example/new/"}}}"#,
        )
        .unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".ci")).unwrap();
        fs::write(
            repo.join(".ci/ccid.toml"),
            format!(
                "schema = 1\nproject = \"fixture\"\n\n[render]\nrender_schema = 2\n\n[checks.probe]\nkind = \"commands\"\ncommands = [[\"sh\", \"-c\", {check:?}]]\n\n[jobs.verify]\nscheduler = \"forgejo\"\nruns_on = \"linux\"\nchecks = [\"probe\"]\nworkflow = \"ci\"\ncommand = [\"ccid:run-declared-checks\"]\n{tables}"
            ),
        )
        .unwrap();
        let render = Command::new(env!("CARGO_BIN_EXE_ccid"))
            .args(["render", "--repo"])
            .arg(&repo)
            .output()
            .unwrap();
        assert!(
            render.status.success(),
            "{}",
            String::from_utf8_lossy(&render.stderr)
        );
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["add", "-A"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        let sha = git(&repo, &["rev-parse", "HEAD"]);
        Fixture {
            _temp: temp,
            root,
            repo,
            sha,
        }
    }

    fn state(&self) -> PathBuf {
        let namespace = format!("{:x}", Sha256::digest(REPOSITORY_URL.as_bytes()));
        self.root.join("cache/ccid-cached").join(&namespace[..24])
    }

    fn toolchains(&self) -> PathBuf {
        self.root.join("cache/ccid-cached/toolchains")
    }

    /// The clean environment of a job: stub programs first on PATH, `extra` on top (an empty value removes a variable).
    fn environment(&self, extra: &[(&str, &str)]) -> BTreeMap<String, String> {
        // The stubs first, then the test's own PATH (git, sh and coreutils live elsewhere on NixOS).
        let path = format!("{}:{}", self.root.join("bin").display(), inherited_path());
        let root = |name: &str| self.root.join(name).display().to_string();
        let mut environment: BTreeMap<String, String> = [
            ("PATH", path),
            ("HOME", root("home")),
            ("TMPDIR", root("tmp")),
            ("CI_CACHE_ROOT", root("cache")),
            ("CARGO_HOME", root("cargo")),
            ("RUSTUP_HOME", root("rustup")),
            ("CARGO_TARGET_DIR", root("target")),
            ("FAKE_STORE", root("store")),
            ("CCID_FLEET_FILE", root("channel.json")),
            ("CI_JOBS", "1".into()),
            ("CI_NIX_JOBS", "1".into()),
            ("CI_MIN_AVAILABLE_MB", "1".into()),
            ("GIT_CONFIG_COUNT", "1".into()),
            ("GIT_CONFIG_KEY_0", "fixture.inherited".into()),
            ("GIT_CONFIG_VALUE_0", "kept".into()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
        for (key, value) in extra {
            if value.is_empty() {
                environment.remove(*key);
            } else {
                environment.insert((*key).to_owned(), (*value).to_owned());
            }
        }
        environment
    }

    /// `ccid job-run --local` in a clean environment: stub programs first on PATH.
    fn command(&self, extra: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccid"));
        command.env_clear();
        command
            .envs(self.environment(extra))
            .current_dir(&self.root)
            .args(["job-run", "--local", "--source"])
            .arg(&self.repo)
            .args([
                "--sha",
                &self.sha,
                "--job",
                "verify",
                "--repository",
                "fixtures/repo",
                "--server-url",
                "https://forge.example",
                "--owner",
                "fixtures",
            ]);
        command
    }

    fn out(&self, name: &str) -> PathBuf {
        self.root.join("out").join(name)
    }

    fn check_environment(&self) -> BTreeMap<String, String> {
        fs::read_to_string(self.out("env"))
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    }
}

/// The per-run compiler wrapper directory `ccid check` puts in front of a check's PATH.
fn compiler_wrapper(entry: &str) -> bool {
    entry
        .rsplit('/')
        .next()
        .is_some_and(|last| last.starts_with("ccid-compiler-"))
}

fn inherited_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
}

fn wait_for(path: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !path.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            use std::io::Read;
            let (mut stdout, mut stderr) = (String::new(), String::new());
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut stdout)
                .unwrap();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("job-run ended ({status}) before the check started:\n{stdout}\n{stderr}");
        }
        assert!(Instant::now() < deadline, "the check never started");
        thread::sleep(Duration::from_millis(20));
    }
}

fn text(output: &Output) -> String {
    format!(
        "status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

const RUNTIME: &str = "\n[runtime]\ntools = [\"python3\"]\ninstall_root = true\nstate_env = { YARN_CACHE_FOLDER = \"yarn-cache\" }\n\n[toolchain]\ntargets = [\"stable:wasm32-unknown-unknown\"]\n";

#[test]
fn the_runtime_sets_up_the_scripts_environment_and_holds_its_locks_across_exec() {
    // The check records what it sees, then blocks until the test releases it.
    let probe = "env > \"$OUT/env\"; echo probe-on-stderr >&2; touch \"$OUT/started\"; while [ ! -e \"$OUT/release\" ]; do sleep 0.05; done";
    let fixture = Fixture::new(probe, RUNTIME);
    // OUT reaches the check through the job's process environment.
    let out = fixture.root.join("out").display().to_string();
    let mut first = fixture
        .command(&[("OUT", &out)])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&fixture.out("started"), &mut first);
    let state = fixture.state();
    let toolchains = fixture.toolchains();

    // The job command (ccid check, exec'd by job-run) holds the state lock exclusive and the
    // toolchain lock shared.
    let state_lock = OpenOptions::new()
        .write(true)
        .open(state.join("lock"))
        .unwrap();
    assert!(matches!(
        state_lock.try_lock(),
        Err(TryLockError::WouldBlock)
    ));
    let toolchain_lock = OpenOptions::new()
        .write(true)
        .open(toolchains.join("install.lock"))
        .unwrap();
    assert!(matches!(
        toolchain_lock.try_lock(),
        Err(TryLockError::WouldBlock)
    ));
    toolchain_lock.try_lock_shared().unwrap();
    toolchain_lock.unlock().unwrap();

    // A second job of the same repository waits for the state lock and gives up with exit 1.
    let second = fixture
        .command(&[("OUT", &out), ("CI_TIMEOUT", "1")])
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(1), "{}", text(&second));
    assert!(
        String::from_utf8_lossy(&second.stderr)
            .contains("timed out after 1 s waiting for the job state lock"),
        "{}",
        text(&second)
    );

    fs::write(fixture.out("release"), "").unwrap();
    let output = first.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", text(&output));
    // The job's stderr joins its stdout (the scripts' `exec 2>&1`).
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("probe-on-stderr"),
        "{}",
        text(&output)
    );
    // The command is gone, so are its locks.
    state_lock.try_lock().unwrap();
    state_lock.unlock().unwrap();

    let seen = fixture.check_environment();
    let path = |p: PathBuf| p.display().to_string();
    let proto = toolchains.join("proto");
    let expected = [
        ("PROTO_HOME", path(proto.clone())),
        ("MOON_HOME", path(state.join("moon-home"))),
        ("PROTO_AUTO_CLEAN", "false".into()),
        ("PROTO_TELEMETRY", "false".into()),
        ("PROTO_YES", "true".into()),
        ("RUSTUP_INIT_SKIP_PATH_CHECK", "yes".into()),
        ("RUSTUP_TOOLCHAIN", "stable".into()),
        ("CARGO_NET_GIT_FETCH_WITH_CLI", "true".into()),
        ("CI_JOBS", "1".into()),
        ("CARGO_INSTALL_ROOT", path(state.join("install-bin"))),
        ("ARTIFACT_ROOT", path(state.join("release-artifacts"))),
        ("YARN_CACHE_FOLDER", path(state.join("yarn-cache"))),
        ("CI_REPOSITORY_URL", REPOSITORY_URL.into()),
        ("GIT_CONFIG_COUNT", "2".into()),
        ("GIT_CONFIG_KEY_0", "fixture.inherited".into()),
        ("GIT_CONFIG_VALUE_0", "kept".into()),
        (
            "GIT_CONFIG_KEY_1",
            "url.https://forge.example/new/.insteadOf".into(),
        ),
        ("GIT_CONFIG_VALUE_1", "https://mirror.example/old/".into()),
    ];
    for (name, value) in expected {
        assert_eq!(seen.get(name), Some(&value), "{name}");
    }
    for directory in [
        "moon-home",
        "install-bin/bin",
        "release-artifacts",
        "yarn-cache",
    ] {
        assert!(state.join(directory).is_dir(), "{directory}");
    }
    // The script's SOURCE_SHA256: the digest of `git archive --format=tar HEAD`.
    let archive = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["archive", "--format=tar", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        seen.get("SOURCE_SHA256"),
        Some(&format!("{:x}", Sha256::digest(&archive.stdout)))
    );
    // PATH: stable cargo, the install root, this ccid, proto, then the tools (the last built
    // first) in front of the job's own PATH.
    let ccid = Path::new(env!("CARGO_BIN_EXE_ccid"))
        .parent()
        .unwrap()
        .to_owned();
    let store = fixture.root.join("store");
    let prefix = [
        fixture.root.join("rust/bin"),
        state.join("install-bin/bin"),
        ccid,
        proto.join("shims"),
        proto.join("bin"),
        store.join("python3/bin"),
        store.join("proto/bin"),
        store.join("moon/bin"),
        fixture.root.join("bin"),
    ]
    .map(path)
    .join(":");
    // (`ccid check` itself puts its per-run compiler wrapper directory in front.)
    let checked: Vec<&str> = seen["PATH"]
        .split(':')
        .skip_while(|entry| compiler_wrapper(entry))
        .collect();
    assert!(
        checked.join(":").starts_with(&format!("{prefix}:")),
        "{} does not start with {prefix}",
        seen["PATH"]
    );
    // The tools come from the check-shell's nixpkgs pin, moon and proto always.
    let nix = fs::read_to_string(fixture.root.join("log/nix")).unwrap();
    let source = format!("github:NixOS/nixpkgs/{REVISION}?narHash={NAR_HASH}");
    assert_eq!(
        nix.trim_end(),
        format!("build --no-link --print-out-paths {source}#moon {source}#proto {source}#python3")
    );
    // The toolchain probes ran against the job's Rust homes; nothing was installed.
    let rustup = fs::read_to_string(fixture.root.join("log/rustup")).unwrap();
    assert!(
        rustup
            .lines()
            .all(|line| line.starts_with(&path(fixture.root.join("rustup")))),
        "{rustup}"
    );
    assert!(
        !rustup.contains(" install") && !rustup.contains(" add"),
        "{rustup}"
    );
    // Nothing of the runtime lands in the checkout (no moon cache links, no state).
    assert!(!fixture.repo.join(".moon").exists());
}

#[test]
fn without_the_runtime_a_job_runs_as_before() {
    let probe = "env > \"$OUT/env\"; echo probe-on-stderr >&2";
    let fixture = Fixture::new(probe, "");
    let out = fixture.root.join("out").display().to_string();
    let output = fixture.command(&[("OUT", &out)]).output().unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let seen = fixture.check_environment();
    for name in [
        "PROTO_HOME",
        "MOON_HOME",
        "SOURCE_SHA256",
        "ARTIFACT_ROOT",
        "CARGO_INSTALL_ROOT",
        "RUSTUP_TOOLCHAIN",
    ] {
        assert!(!seen.contains_key(name), "{name}");
    }
    assert_eq!(seen["GIT_CONFIG_COUNT"], "1");
    let checked: Vec<&str> = seen["PATH"]
        .split(':')
        .skip_while(|entry| compiler_wrapper(entry))
        .collect();
    assert_eq!(
        checked.join(":"),
        format!(
            "{}:{}",
            fixture.root.join("bin").display(),
            inherited_path()
        )
    );
    // stderr stays stderr.
    assert!(String::from_utf8_lossy(&output.stderr).contains("probe-on-stderr"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("probe-on-stderr"));
    assert!(!fixture.root.join("log/nix").exists(), "no tools built");
    assert!(!fixture.root.join("cache/ccid-cached").exists(), "no state");
}

#[test]
fn the_runtime_refuses_to_run_without_fleet_data_or_a_state_root() {
    let fixture = Fixture::new("true", RUNTIME);
    let output = fixture
        .command(&[("CCID_FLEET_FILE", "")])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("[runtime] needs the release channel's fleet data"),
        "{}",
        text(&output)
    );
    let output = fixture
        .command(&[("CI_CACHE_ROOT", ""), ("CARGO_HOME", "relative")])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("needs an absolute persistent CI_CACHE_ROOT or CARGO_HOME"),
        "{}",
        text(&output)
    );
    assert!(!fixture.root.join("log/nix").exists(), "nothing was built");
}

#[test]
fn the_runtime_serves_only_sentinel_jobs() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    fs::create_dir_all(repo.join(".ci")).unwrap();
    fs::write(
        repo.join(".ci/ccid.toml"),
        "schema = 1\nproject = \"fixture\"\n[render]\nrender_schema = 2\n[checks.verify]\nkind = \"commands\"\ncommands = [[\"true\"]]\n[jobs.verify]\nscheduler = \"forgejo\"\nchecks = [\"verify\"]\nworkflow = \"ci\"\ncommand = [\"bash\", \".ci/run-cached.sh\"]\n[runtime]\n",
    )
    .unwrap();
    let render = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args(["render", "--repo"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(!render.status.success());
    assert!(
        String::from_utf8_lossy(&render.stderr).contains(
            "[runtime] serves ccid:run-declared-checks jobs only; job verify runs its own command"
        ),
        "{}",
        String::from_utf8_lossy(&render.stderr)
    );
    // [toolchain] alone is data nothing reads: refused.
    fs::write(
        repo.join(".ci/ccid.toml"),
        "schema = 1\nproject = \"fixture\"\n[checks.verify]\nkind = \"commands\"\ncommands = [[\"true\"]]\n[toolchain]\nnightly = true\n",
    )
    .unwrap();
    let render = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args(["render", "--repo"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(!render.status.success());
    assert!(String::from_utf8_lossy(&render.stderr)
        .contains("[toolchain] is read by the job runtime only"));
}

/// The job entry point still requires what it always did: the forge arguments (a mirror forge runs no CI, so there is
/// no tokenless mode of it).
#[test]
fn job_run_without_the_forge_arguments_is_a_usage_error() {
    let bare = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args(["job-run", "--job", "verify"])
        .output()
        .unwrap();
    assert_eq!(bare.status.code(), Some(2), "{}", text(&bare));
    let mirror = Command::new(env!("CARGO_BIN_EXE_ccid"))
        .args(["job-run", "--mirror", "--job", "verify"])
        .output()
        .unwrap();
    assert_eq!(mirror.status.code(), Some(2), "{}", text(&mirror));
}

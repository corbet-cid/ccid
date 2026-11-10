//! `ccid toolchain` against stub `rustup`, `proto`, `getent` and `cargo`
//! scripts: no real installs, no network.

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const LLVM: &str = "llvm-tools-x86_64-unknown-linux-gnu";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ccid-tc-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["bin", "state", "cargo", "rustup"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let state = root.join("state");
        let script = |name: &str, body: &str| {
            let path = root.join("bin").join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            let mut perms = fs::metadata(&path).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            fs::set_permissions(&path, perms).unwrap();
        };
        let s = state.display();
        script(
            "rustup",
            &format!(
                r#"echo "HOME=$HOME rustup $*" >> {s}/log
case "$1 $2" in
  "run stable") [ -e {s}/stable ] ;;
  "run nightly") [ -e {s}/nightly ] ;;
  "component list") cat {s}/comps."$4" 2>/dev/null; true ;;
  "target list") cat {s}/targets."$4" 2>/dev/null; true ;;
  "component add")
    [ -e {s}/fail_install ] && exit 1
    [ -e {s}/no_effect ] && exit 0
    if [ "$3" = llvm-tools-preview ]; then echo {LLVM} >> {s}/comps."$5"; else echo "$3-x86_64" >> {s}/comps."$5"; fi ;;
  "toolchain install")
    [ -e {s}/fail_install ] && exit 1
    [ -e {s}/no_effect ] && exit 0
    touch {s}/"$3"; echo {LLVM} >> {s}/comps."$3" ;;
  "target add")
    [ -e {s}/fail_install ] && exit 1
    [ -e {s}/no_effect ] && exit 0
    echo "$3" >> {s}/targets."$5" ;;
  *) exit 3 ;;
esac"#
            ),
        );
        script(
            "proto",
            &format!(
                r#"echo "HOME=$HOME proto $*" >> {s}/log
[ -e {s}/fail_install ] && exit 1
[ -e {s}/no_effect ] && exit 0
touch {s}/stable"#
            ),
        );
        script("getent", r#"echo "x:x:$2:$2:x:/stub/home:/bin/sh""#);
        script("cargo", &format!(r#"echo "cargo $*" >> {s}/log"#));
        Self { root }
    }

    fn state(&self, name: &str) -> PathBuf {
        self.root.join("state").join(name)
    }

    fn touch(&self, name: &str) {
        File::create(self.state(name)).unwrap();
    }

    fn write(&self, name: &str, text: &str) {
        fs::write(self.state(name), text).unwrap();
    }

    fn warm(&self) {
        self.touch("stable");
        self.touch("nightly");
        self.write("comps.stable", "rustfmt-x86_64\nclippy-x86_64\n");
        self.write("comps.nightly", &format!("{LLVM}\n"));
        self.write("targets.stable", "wasm32-unknown-unknown\n");
    }

    fn lock(&self) -> PathBuf {
        self.root.join("install.lock")
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.state("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn mutations(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|line| {
                line.contains(" add ") || line.contains(" install ") || line.contains("proto")
            })
            .collect()
    }

    fn ccid(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccid"));
        command
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("HOME", "/nonexistent")
            .env("CARGO_HOME", self.root.join("cargo"))
            .env("RUSTUP_HOME", self.root.join("rustup"))
            .stdin(Stdio::null());
        command
    }

    fn ensure(&self, timeout: u64, extra: &[&str], run: &[&str]) -> Command {
        let mut command = self.ccid();
        command
            .args(["toolchain", "ensure", "--lock-file"])
            .arg(self.lock())
            .args(["--timeout", &timeout.to_string()])
            .args(extra)
            .arg("--")
            .args(run);
        command
    }

    /// Both lock files can be taken exclusively. Polled: a concurrent test
    /// thread may fork while a descriptor is open, and the forked child keeps
    /// the open file description (and so the lock) until its own exec.
    fn assert_free(&self) {
        for path in [self.lock(), self.root.join("install.lock.gate")] {
            let file = open(&path);
            let deadline = Instant::now() + Duration::from_secs(5);
            while file.try_lock().is_err() {
                assert!(
                    Instant::now() < deadline,
                    "{} is still locked",
                    path.display()
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn open(path: &Path) -> File {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .unwrap()
}

fn is_locked_exclusively_refused(path: &Path) -> bool {
    let file = open(path);
    file.try_lock().is_err()
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "condition not reached in 10 s");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn warm_runs_no_install_and_takes_no_exclusive_lock() {
    let f = Fixture::new("warm");
    f.warm();
    // A shared holder would block an exclusive request for the whole run.
    let holder = open(&f.lock());
    holder.lock_shared().unwrap();
    let status = f
        .ensure(5, &["--target", "stable:wasm32-unknown-unknown"], &["true"])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(f.mutations().is_empty(), "{:?}", f.mutations());
    drop(holder);
    f.assert_free();
}

#[test]
fn missing_clippy_adds_exactly_clippy_with_the_euid_home() {
    let f = Fixture::new("clippy");
    f.warm();
    f.write("comps.stable", "rustfmt-x86_64\n");
    let status = f.ensure(5, &[], &["true"]).status().unwrap();
    assert!(status.success());
    assert_eq!(
        f.mutations(),
        ["HOME=/stub/home rustup component add clippy --toolchain stable"]
    );
}

#[test]
fn missing_nightly_is_installed_and_no_nightly_skips_it() {
    let f = Fixture::new("nightly");
    f.warm();
    fs::remove_file(f.state("nightly")).unwrap();
    assert!(f
        .ensure(5, &["--no-nightly"], &["true"])
        .status()
        .unwrap()
        .success());
    assert!(f.mutations().is_empty());
    assert!(f.ensure(5, &[], &["true"]).status().unwrap().success());
    assert_eq!(
        f.mutations(),
        ["HOME=/stub/home rustup toolchain install nightly --component llvm-tools-preview"]
    );
}

#[test]
fn missing_target_is_added() {
    let f = Fixture::new("target");
    f.warm();
    let status = f
        .ensure(5, &["--target", "stable:wasm32-wasip1"], &["true"])
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        f.mutations(),
        ["HOME=/stub/home rustup target add wasm32-wasip1 --toolchain stable"]
    );
}

#[test]
fn cold_host_installs_stable_through_proto() {
    let f = Fixture::new("cold");
    f.touch("nightly");
    f.write("comps.nightly", &format!("{LLVM}\n"));
    let status = f.ensure(5, &[], &["true"]).status().unwrap();
    assert!(status.success());
    let mutations = f.mutations();
    assert_eq!(mutations[0], "HOME=/stub/home proto install rust stable");
    assert_eq!(mutations.len(), 3, "{mutations:?}"); // proto, rustfmt, clippy
}

#[test]
fn install_failure_exits_one_and_frees_both_locks() {
    let f = Fixture::new("install-fails");
    f.warm();
    f.write("comps.stable", "");
    f.touch("fail_install");
    let output = f.ensure(5, &[], &["true"]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("toolchain"));
    f.assert_free();
}

#[test]
fn recheck_failure_exits_one_and_frees_both_locks() {
    let f = Fixture::new("recheck");
    f.warm();
    f.write("comps.stable", "");
    f.touch("no_effect");
    let output = f.ensure(5, &[], &["true"]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("still not ready"));
    f.assert_free();
}

#[test]
fn relative_cargo_home_is_refused_before_any_install() {
    let f = Fixture::new("relative");
    f.warm();
    f.write("comps.stable", "");
    let output = f
        .ensure(5, &[], &["true"])
        .env("CARGO_HOME", "cargo")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("CARGO_HOME must be absolute"));
    assert!(f.mutations().is_empty());
}

#[test]
fn lock_timeout_under_an_exclusive_holder_exits_one_in_time() {
    let f = Fixture::new("timeout");
    f.warm();
    let holder = open(&f.lock());
    holder.lock().unwrap();
    let started = Instant::now();
    let output = f.ensure(1, &[], &["true"]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
    drop(holder);
    f.assert_free();
}

#[test]
fn the_gate_stops_probes_until_it_is_released() {
    let f = Fixture::new("gate");
    f.warm();
    let gate = open(&f.root.join("install.lock.gate"));
    gate.lock().unwrap();
    let mut child = f.ensure(10, &[], &["true"]).spawn().unwrap();
    thread::sleep(Duration::from_millis(600));
    assert!(
        f.log().is_empty(),
        "probes ran behind a held gate: {:?}",
        f.log()
    );
    assert!(child.try_wait().unwrap().is_none());
    drop(gate);
    assert!(child.wait().unwrap().success());
    assert!(!f.log().is_empty());
}

/// A job that holds the toolchain lock shared until killed. The marker file
/// proves the lock is held: it is created by the exec'd shell, after `ensure`.
fn holder(f: &Fixture, name: &str) -> Child {
    let marker = f.root.join(name);
    let child = f
        .ensure(
            5,
            &[],
            &[
                "sh",
                "-c",
                r#"touch "$1"; exec sleep 60"#,
                "sh",
                &marker.to_string_lossy(),
            ],
        )
        .spawn()
        .unwrap();
    wait_for(|| marker.exists());
    child
}

#[test]
fn the_lock_survives_exec_and_ends_with_the_command() {
    let f = Fixture::new("exec");
    f.warm();
    let output = f
        .ensure(
            5,
            &[],
            &[
                "sh",
                "-c",
                r#"flock -n -x "$1" true && echo granted || echo refused"#,
                "sh",
                &f.lock().to_string_lossy(),
            ],
        )
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "refused");
    f.assert_free();
}

#[test]
fn concurrent_holders_share_and_a_kill_releases_the_lock() {
    let f = Fixture::new("holders");
    f.warm();
    let mut first = holder(&f, "first");
    let mut second = holder(&f, "second");
    // Both run at once: shared locks coexist (the second one got past the
    // gate and the main lock while the first still held it).
    assert!(first.try_wait().unwrap().is_none());
    assert!(second.try_wait().unwrap().is_none());
    first.kill().unwrap();
    first.wait().unwrap();
    assert!(
        is_locked_exclusively_refused(&f.lock()),
        "second still holds it"
    );
    second.kill().unwrap();
    second.wait().unwrap();
    f.assert_free();
}

#[test]
fn a_writer_waits_for_holders_and_provisions_after_they_leave() {
    let f = Fixture::new("writer");
    f.warm();
    let mut reader = holder(&f, "reader");
    f.write("comps.stable", "rustfmt-x86_64\n"); // clippy vanishes: a writer is needed
    let mut writer = f.ensure(20, &[], &["true"]).spawn().unwrap();
    thread::sleep(Duration::from_millis(700));
    assert!(
        writer.try_wait().unwrap().is_none(),
        "writer ran under a shared holder"
    );
    assert!(f.mutations().is_empty());
    reader.kill().unwrap();
    reader.wait().unwrap();
    assert!(writer.wait().unwrap().success());
    assert_eq!(f.mutations().len(), 1);
}

#[test]
fn ready_reports_through_the_exit_code() {
    let f = Fixture::new("ready");
    f.warm();
    let ready = |args: &[&str]| {
        f.ccid()
            .args(["toolchain", "ready"])
            .args(args)
            .status()
            .unwrap()
            .code()
    };
    assert_eq!(ready(&[]), Some(0));
    assert_eq!(
        ready(&["--target", "stable:wasm32-unknown-unknown"]),
        Some(0)
    );
    assert_eq!(ready(&["--target", "stable:wasm32-wasip1"]), Some(1));
    assert_eq!(ready(&["--target", "bad"]), Some(2));
    fs::remove_file(f.state("nightly")).unwrap();
    assert_eq!(ready(&[]), Some(1));
    assert_eq!(ready(&["--no-nightly"]), Some(0));
    assert!(f.mutations().is_empty());
}

#[test]
fn ready_without_rustup_is_not_ready() {
    let f = Fixture::new("norustup");
    let status = f
        .ccid()
        .env("PATH", "/usr/bin:/bin")
        .args(["toolchain", "ready"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));
}

#[test]
fn install_exact_skips_a_matching_binary_and_installs_otherwise() {
    let f = Fixture::new("exact");
    let root = f.root.join("exact-root");
    fs::create_dir_all(root.join("bin")).unwrap();
    let binary = root.join("bin/tool");
    fs::write(&binary, "#!/bin/sh\necho 'tool 1.2.3'\n").unwrap();
    let mut perms = fs::metadata(&binary).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&binary, perms).unwrap();
    let run = |version: &str| {
        f.ccid()
            .env("CARGO_INSTALL_ROOT", &root)
            .args(["toolchain", "install-exact", "tool", "tool-cli", version])
            .status()
            .unwrap()
            .code()
    };
    assert_eq!(run("1.2.3"), Some(0));
    assert!(f.log().is_empty());
    // Exact equality: 1.2.30 must not be satisfied by 1.2.3, and the stub
    // cargo installs nothing, so the recheck fails.
    assert_eq!(run("1.2.30"), Some(1));
    assert_eq!(f.log().len(), 1);
    assert!(f.log()[0].starts_with("cargo install --locked --version 1.2.30 --root"));
    // Missing install root is a failure, not a pass.
    let status = f
        .ccid()
        .args(["toolchain", "install-exact", "tool", "tool-cli", "1.2.3"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));
}

mod wasm {
    use super::*;
    use sha2::{Digest, Sha256};

    const VERSION: &str = "0.2.105";

    struct Served {
        f: Fixture,
        serve: PathBuf,
        repo: PathBuf,
        into: PathBuf,
    }

    fn serve(name: &str, good_digest: bool) -> Served {
        let f = Fixture::new(name);
        let repo = f.root.join("repo");
        let serve = f.root.join("serve").join(VERSION);
        let package = f.root.join("pkg/wasm-bindgen-x");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&serve).unwrap();
        fs::create_dir_all(&package).unwrap();
        fs::write(
            repo.join("Cargo.lock"),
            format!(
                "[[package]]\nname = \"wasm-bindgen\"\nversion = \"{VERSION}\"\n\n[[package]]\nname = \"other\"\nversion = \"1.0.0\"\n"
            ),
        )
        .unwrap();
        let runner = package.join("wasm-bindgen-test-runner");
        fs::write(
            &runner,
            format!("#!/bin/sh\necho 'wasm-bindgen-test-runner {VERSION}'\n"),
        )
        .unwrap();
        let mut perms = fs::metadata(&runner).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&runner, perms).unwrap();
        let archive = serve.join(format!(
            "wasm-bindgen-{VERSION}-x86_64-unknown-linux-musl.tar.gz"
        ));
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(f.root.join("pkg"))
            .arg("wasm-bindgen-x")
            .status()
            .unwrap();
        assert!(status.success());
        let digest = if good_digest {
            format!("{:x}", Sha256::digest(fs::read(&archive).unwrap()))
        } else {
            "0".repeat(64)
        };
        let name = archive.file_name().unwrap().to_string_lossy().into_owned();
        fs::write(
            format!("{}.sha256sum", archive.display()),
            format!("{digest}  {name}\n"),
        )
        .unwrap();
        let into = f.root.join("into");
        Served {
            f,
            serve,
            repo,
            into,
        }
    }

    fn run(s: &Served) -> std::process::Output {
        s.f.ccid()
            .env(
                "WASM_BINDGEN_DOWNLOAD_BASE",
                format!("file://{}", s.serve.parent().unwrap().display()),
            )
            .args(["toolchain", "wasm-bindgen", "--repo"])
            .arg(&s.repo)
            .arg("--into")
            .arg(&s.into)
            .output()
            .unwrap()
    }

    #[test]
    fn good_digest_returns_the_directory_and_is_reused_offline() {
        if std::env::consts::ARCH != "x86_64" {
            eprintln!("skipped: wasm-bindgen runner is x86_64 only");
            return;
        }
        let s = serve("wasm-good", true);
        let out = run(&s);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let expected = s.into.join(VERSION);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            expected.display().to_string()
        );
        assert!(expected.join("wasm-bindgen-test-runner").is_file());
        // No scratch directory is left in `into`.
        let entries: Vec<_> = fs::read_dir(&s.into).unwrap().collect();
        assert_eq!(entries.len(), 1);
        // Second call: the served files are gone, so only a reuse can pass.
        fs::remove_dir_all(&s.serve).unwrap();
        let again = run(&s);
        assert!(
            again.status.success(),
            "{}",
            String::from_utf8_lossy(&again.stderr)
        );
        assert_eq!(again.stdout, out.stdout);
    }

    #[test]
    fn bad_digest_fails_and_installs_nothing() {
        if std::env::consts::ARCH != "x86_64" {
            return;
        }
        let s = serve("wasm-bad", false);
        let out = run(&s);
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("sha256 mismatch"));
        assert!(!s.into.join(VERSION).exists());
        assert_eq!(fs::read_dir(&s.into).unwrap().count(), 0);
    }

    #[test]
    fn zero_or_several_versions_are_errors() {
        if std::env::consts::ARCH != "x86_64" {
            return;
        }
        let s = serve("wasm-lock", true);
        fs::write(
            s.repo.join("Cargo.lock"),
            "[[package]]\nname = \"other\"\nversion = \"1\"\n",
        )
        .unwrap();
        assert_eq!(run(&s).status.code(), Some(1));
        fs::write(
            s.repo.join("Cargo.lock"),
            "[[package]]\nname = \"wasm-bindgen\"\nversion = \"0.2.1\"\n[[package]]\nname = \"wasm-bindgen\"\nversion = \"0.2.2\"\n",
        )
        .unwrap();
        let out = run(&s);
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("several"));
    }
}

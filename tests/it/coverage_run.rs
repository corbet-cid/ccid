//! `ccid coverage run` against stub `rustup` and nightly `cargo` scripts: the
//! stub cargo logs its argv and environment and writes fixture reports, so no
//! toolchain, build or network is involved.

use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

/// Write an executable stub through a child process: a stub this process wrote
/// itself can be held open for writing by a concurrently forked child, and its
/// exec then fails with ETXTBSY.
fn write_stub(path: &Path, script: &str) {
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
        .write_all(script.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success(), "stub {}", path.display());
}

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new(manifest: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for name in ["bin", "nightly/bin", "state", "tmp", "repo/src", "repo/.ci"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        let fixture = Self {
            _directory: directory,
            root,
        };
        let state = fixture.state_dir();
        let nightly = fixture.root.join("nightly/bin");
        let s = state.display();
        let n = nightly.display();
        write_stub(
            &fixture.root.join("bin/rustup"),
            &format!(
                r#"#!/bin/sh
echo "rustup $*" >> {s}/log
[ -e {s}/no_nightly ] && {{ echo "error: toolchain 'nightly' is not installed" >&2; exit 1; }}
[ "$1 $2 $3" = "which --toolchain nightly" ] || exit 3
case "$4" in
  rustc) echo {n}/rustc ;;
  cargo) echo {n}/cargo ;;
  *) exit 4 ;;
esac
"#
            ),
        );
        write_stub(&nightly.join("rustc"), "#!/bin/sh\necho rustc-stub\n");
        write_stub(
            &nightly.join("cargo"),
            &format!(
                r#"#!/bin/sh
echo "argv $*" >> {s}/log
echo "env RUSTUP_TOOLCHAIN=$RUSTUP_TOOLCHAIN CARGO_TARGET_DIR=$CARGO_TARGET_DIR RUSTC=$RUSTC CARGO=$CARGO LLVM_COV_TARGET=${{CARGO_LLVM_COV_TARGET_DIR-unset}} PATH=$PATH PWD=$(pwd)" >> {s}/log
[ "$1" = llvm-cov ] || exit 64
if [ "$2" = --version ]; then
  [ -e {s}/no_llvm_cov ] && exit 101
  echo "cargo-llvm-cov 0.0.0-stub"
  exit 0
fi
for out; do :; done
case " $* " in
  *" --json "*) kind=json ;;
  *" --lcov "*) kind=lcov ;;
  *" --text "*) kind=txt ;;
  *) exit 65 ;;
esac
[ -d "$CARGO_TARGET_DIR" ] || exit 66
echo "$CARGO_TARGET_DIR" > {s}/target
touch "$CARGO_TARGET_DIR/built"
[ -e {s}/fail_$kind ] && exit 7
cp {s}/coverage.$kind "$out"
"#
            ),
        );
        fs::write(
            fixture.repo().join("src/lib.rs"),
            "first source line\nsecond source line\n",
        )
        .unwrap();
        fs::write(fixture.repo().join(".ci/ccid.toml"), manifest).unwrap();
        fixture.reports(1);
        fixture
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    fn touch(&self, name: &str) {
        fs::write(self.state_dir().join(name), "").unwrap();
    }

    /// The reports the stub cargo writes; `second` is line 2's hit count.
    fn reports(&self, second: u64) {
        let source = format!("{}/src/lib.rs", self.repo().display());
        let covered = if second > 0 { 2 } else { 1 };
        fs::write(
            self.state_dir().join("coverage.json"),
            serde_json::json!({
                "type": "llvm.coverage.json.export",
                "data": [{
                    "files": [{
                        "filename": source,
                        "branches": [[1, 1, 1, 2, 2, 1, 0, 0, 4]],
                        "summary": {"lines": {"count": 2, "covered": covered}, "branches": {"count": 2, "covered": 2}},
                    }],
                    "functions": [{
                        "name": "lib",
                        "count": 3,
                        "filenames": [source],
                        "regions": [[1, 1, 2, 2, 3, 0, 0, 0]],
                        "branches": [[1, 1, 1, 2, 2, 1, 0, 0, 4]],
                        "mcdc_records": [],
                    }],
                    "totals": {"lines": {"count": 2, "covered": covered}, "branches": {"count": 2, "covered": 2}},
                }],
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            self.state_dir().join("coverage.lcov"),
            format!(
                "SF:{source}\nDA:1,3\nDA:2,{second}\nLF:2\nLH:{covered}\n\
                 BRDA:1,0,0,2\nBRDA:1,0,1,1\nBRF:2\nBRH:2\nend_of_record\n"
            ),
        )
        .unwrap();
        fs::write(
            self.state_dir().join("coverage.txt"),
            format!("{source}:\n    1| 3|first source line\n    2| {second}|second source line\n"),
        )
        .unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccid"));
        command
            .args(args)
            .current_dir(self.repo())
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("HOME", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("CARGO_TARGET_DIR", self.root.join("outer-target"))
            .env(
                "CARGO_LLVM_COV_TARGET_DIR",
                self.root.join("llvm-cov-target"),
            )
            .stdin(Stdio::null());
        command
    }

    fn run(&self) -> Output {
        self.command(&["coverage", "run"]).output().unwrap()
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.state_dir().join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn argv(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter_map(|line| line.strip_prefix("argv ").map(str::to_owned))
            .collect()
    }

    /// The private target the stub saw, which must be gone afterwards.
    fn target(&self) -> PathBuf {
        PathBuf::from(
            fs::read_to_string(self.state_dir().join("target"))
                .unwrap()
                .trim(),
        )
    }
}

fn text(output: &Output) -> String {
    format!(
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

const CLBS: &str = "--branch --ignore-filename-regex /tests/";

#[test]
fn run_executes_one_llvm_cov_and_two_reports_then_the_source_gate() {
    let fixture = Fixture::new("schema = 1\nproject = \"p\"\n");
    let output = fixture.run();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert_eq!(
        fixture.argv(),
        [
            "llvm-cov --version".to_owned(),
            "llvm-cov --locked --workspace --all-features --branch --no-cfg-coverage \
             --no-cfg-coverage-nightly --ignore-filename-regex /tests/ --json --output-path coverage.json"
                .to_owned(),
            format!("llvm-cov report --locked {CLBS} --lcov --output-path coverage.lcov"),
            format!("llvm-cov report --locked {CLBS} --text --output-path coverage.txt"),
        ]
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("lines: 2/2\nbranches: 2/2"),
        "{}",
        text(&output)
    );
    for name in ["coverage.json", "coverage.lcov", "coverage.txt"] {
        assert!(fixture.repo().join(name).is_file(), "{name}");
    }
}

#[test]
fn run_environment_is_nightly_with_a_private_target_outside_the_repository() {
    let fixture = Fixture::new("schema = 1\nproject = \"p\"\n");
    let output = fixture.run();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let nightly = fixture.root.join("nightly/bin");
    let target = fixture.target();
    assert!(target.starts_with(fixture.root.join("tmp")), "{target:?}");
    assert!(!target.starts_with(fixture.repo()));
    assert!(!target.exists(), "the private target must be removed");
    let environments: Vec<String> = fixture
        .log()
        .into_iter()
        .filter(|line| line.starts_with("env "))
        .collect();
    assert_eq!(environments.len(), 4);
    for (index, line) in environments.iter().enumerate() {
        assert!(line.contains("RUSTUP_TOOLCHAIN=nightly "), "{line}");
        assert!(
            line.contains(&format!("RUSTC={}/rustc ", nightly.display())),
            "{line}"
        );
        assert!(
            line.contains(&format!("CARGO={}/cargo ", nightly.display())),
            "{line}"
        );
        assert!(
            line.contains(&format!("PATH={}:", nightly.display())),
            "{line}"
        );
        assert!(line.contains("LLVM_COV_TARGET=unset "), "{line}");
        assert!(
            line.ends_with(&format!("PWD={}", fixture.repo().display())),
            "{line}"
        );
        if index > 0 {
            assert!(
                line.contains(&format!("CARGO_TARGET_DIR={} ", target.display())),
                "{line}"
            );
        }
    }
}

#[test]
fn run_text_arguments_reach_only_the_text_report() {
    let fixture = Fixture::new(
        "schema = 1\nproject = \"p\"\n[coverage]\nllvm_cov = [\"--branch\"]\n\
         report = [\"--branch\", \"--ignore-filename-regex\", \"(^|/)(tests|scripts)/\"]\n\
         text = [\"--show-instantiations\"]\n",
    );
    let output = fixture.run();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let report = "--branch --ignore-filename-regex (^|/)(tests|scripts)/";
    assert_eq!(
        fixture.argv()[1..],
        [
            "llvm-cov --locked --branch --json --output-path coverage.json".to_owned(),
            format!("llvm-cov report --locked {report} --lcov --output-path coverage.lcov"),
            format!(
                "llvm-cov report --locked {report} --show-instantiations --text --output-path coverage.txt"
            ),
        ]
    );
}

#[test]
fn run_gate_always_reads_three_reports_without_caller_arguments() {
    // The 3-argument gate called with 2 arguments cannot happen: the run hands
    // the source gate its own three reports.
    let fixture = Fixture::new("schema = 1\nproject = \"p\"\n[coverage]\ngates = [\"source\"]\n");
    let output = fixture.run();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    // The annotated report is one of the gate's inputs: a disagreeing one refuses.
    let annotated = fixture.state_dir().join("coverage.txt");
    let disagreeing = fs::read_to_string(&annotated)
        .unwrap()
        .replace("2| 1|", "2| 0|");
    fs::write(&annotated, disagreeing).unwrap();
    let output = fixture.run();
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Coverage gate: Inconsistent emitted line coverage"),
        "{}",
        text(&output)
    );
}

#[test]
fn run_both_gates_read_the_same_single_execution() {
    let fixture = Fixture::new(
        "schema = 1\nproject = \"p\"\n[coverage]\ngates = [\"totals\", \"source\"]\nrequire_sources = \"src\"\n",
    );
    let output = fixture.run();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let executions = fixture
        .argv()
        .iter()
        .filter(|line| line.starts_with("llvm-cov --locked"))
        .count();
    assert_eq!(executions, 1);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "cargo-llvm-cov 0.0.0-stub\nlines: 2/2\nbranches: 2/2\nlines: 2/2\nbranches: 2/2\n"
    );
}

#[test]
fn run_tool_failure_is_exit_one_and_removes_the_target() {
    let fixture = Fixture::new("schema = 1\nproject = \"p\"\n");
    fixture.touch("fail_lcov");
    let output = fixture.run();
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "ccid coverage run: {}/cargo llvm-cov report --locked {CLBS} --lcov --output-path coverage.lcov failed (exit 7)",
            fixture.root.join("nightly/bin").display()
        )),
        "{stderr}"
    );
    assert_eq!(fixture.argv().len(), 3, "no command after the failure");
    assert!(!fixture.target().exists());
}

#[test]
fn run_gate_refusal_is_exit_one() {
    let fixture = Fixture::new("schema = 1\nproject = \"p\"\n");
    fixture.reports(0);
    let output = fixture.run();
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Coverage gate: Uncovered source lines: [('src/lib.rs', 2)]; branches: []"),
        "{}",
        text(&output)
    );
    assert!(!fixture.target().exists());
}

#[test]
fn run_copies_existing_reports_as_artifacts_also_on_failure() {
    let fixture = Fixture::new("schema = 1\nproject = \"p\"\n[coverage]\nartifacts = true\n");
    fixture.touch("fail_txt");
    let artifacts = fixture.root.join("artifacts");
    let output = fixture
        .command(&["coverage", "run"])
        .env("ARTIFACT_ROOT", &artifacts)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(artifacts.join("coverage.json").is_file());
    assert!(artifacts.join("coverage.lcov").is_file());
    assert!(!artifacts.join("coverage.txt").exists());
}

#[test]
fn run_artifact_root_must_be_set_and_outside_the_repository() {
    let fixture = Fixture::new("schema = 1\nproject = \"p\"\n[coverage]\nartifacts = true\n");
    let output = fixture.run();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    let inside = fixture
        .command(&["coverage", "run"])
        .env("ARTIFACT_ROOT", fixture.repo().join("out"))
        .output()
        .unwrap();
    assert_eq!(inside.status.code(), Some(2), "{}", text(&inside));
    assert!(fixture.argv().is_empty(), "nothing runs before the refusal");
}

#[test]
fn run_missing_nightly_or_llvm_cov_is_exit_two() {
    let fixture = Fixture::new("schema = 1\nproject = \"p\"\n");
    fixture.touch("no_nightly");
    let output = fixture.run();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("nightly toolchain is unavailable"));
    assert!(fixture.argv().is_empty());
    fs::remove_file(fixture.state_dir().join("no_nightly")).unwrap();
    fixture.touch("no_llvm_cov");
    let output = fixture.run();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cargo-llvm-cov is unavailable"));
    assert_eq!(fixture.argv(), ["llvm-cov --version"]);
}

#[test]
fn run_refuses_a_target_inside_the_repository() {
    let fixture = Fixture::new("schema = 1\nproject = \"p\"\n");
    let inside = fixture.repo().join("scratch");
    fs::create_dir_all(&inside).unwrap();
    let output = fixture
        .command(&["coverage", "run"])
        .env("TMPDIR", &inside)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsafe coverage target"));
    assert!(fixture.argv().is_empty(), "nothing runs before the refusal");
    assert_eq!(fs::read_dir(&inside).unwrap().count(), 0);
}

#[test]
fn run_refuses_invalid_gates_before_running() {
    for gates in ["[]", "[\"source\", \"source\"]", "[\"lines\"]"] {
        let fixture = Fixture::new(&format!(
            "schema = 1\nproject = \"p\"\n[coverage]\ngates = {gates}\n"
        ));
        let output = fixture.run();
        assert_eq!(output.status.code(), Some(2), "{gates}: {}", text(&output));
        assert!(fixture.argv().is_empty(), "{gates}");
    }
}

/// sha256 over the sorted `git ls-files --cached --others -z` paths and their
/// contents (the set cmnp hashes), the check's declared outputs left out.
fn source_digest(repo: &Path, outputs: &[&str]) -> String {
    let listing = Command::new("git")
        .args(["ls-files", "--cached", "--others", "-z"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(listing.status.success());
    let mut paths: Vec<String> = String::from_utf8(listing.stdout)
        .unwrap()
        .split('\0')
        .filter(|path| !path.is_empty() && !outputs.contains(path))
        .map(str::to_owned)
        .collect();
    paths.sort();
    paths.dedup();
    let mut digest = Sha256::new();
    for path in paths {
        digest.update(path.as_bytes());
        digest.update([0]);
        digest.update(fs::read(repo.join(&path)).unwrap());
        digest.update([0]);
    }
    format!("{:x}", digest.finalize())
}

#[test]
fn converted_verify_check_leaves_the_declared_source_unchanged() {
    let manifest = "schema = 1\nproject = \"p\"\n\n[checks.verify]\nkind = \"commands\"\n\
        commands = [[\"ccid\", \"coverage\", \"run\"]]\n\
        cache_outputs = [\"coverage.json\", \"coverage.lcov\", \"coverage.txt\"]\n\n\
        [coverage]\ngates = [\"source\"]\n";
    let fixture = Fixture::new(manifest);
    let repo = fixture.repo();
    for arguments in [
        &["init", "-q"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=proof",
            "-c",
            "user.email=proof@invalid",
            "commit",
            "-qm",
            "snapshot",
        ],
    ] {
        assert!(Command::new("git")
            .args(arguments)
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
    }
    let outputs = ["coverage.json", "coverage.lcov", "coverage.txt"];
    let before = source_digest(&repo, &outputs);
    let binaries = Path::new(env!("CARGO_BIN_EXE_ccid")).parent().unwrap();
    let output = fixture
        .command(&["check", "--repo", ".", "--check", "verify"])
        .env(
            "PATH",
            format!(
                "{}:{}:{}",
                fixture.root.join("bin").display(),
                binaries.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("CI_JOBS", "1")
        .env("CI_NIX_JOBS", "1")
        .env("CI_TIMEOUT", "120")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(repo.join("coverage.txt").is_file());
    assert_eq!(source_digest(&repo, &outputs), before);
    assert!(!fixture.target().starts_with(&repo));
    assert!(!fixture.target().exists());
}

fn wasm_manifest(command: &str) -> String {
    format!(
        "schema = 1\nproject = \"p\"\n[coverage.wasm]\ncommand = [\"sh\", \"-c\", {command:?}]\n"
    )
}

#[test]
fn run_gates_the_wasm_execution_after_the_native_one() {
    let fixture = Fixture::new(&wasm_manifest(
        "cp \"$STATE\"/coverage.* \"$CCID_COVERAGE_OUT\"/ && echo wasm-ran >> \"$STATE\"/log && echo \"$CCID_COVERAGE_OUT\" > \"$STATE\"/wasm-out && test -z \"${CARGO_TARGET_DIR:-}\"",
    ));
    fixture.reports(1);
    let output = fixture
        .command(&["coverage", "run"])
        .env("STATE", fixture.state_dir())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(fixture.log().iter().any(|line| line == "wasm-ran"));
    let out = fs::read_to_string(fixture.state_dir().join("wasm-out")).unwrap();
    assert!(
        !Path::new(out.trim()).exists(),
        "the wasm report directory is removed"
    );
}

#[test]
fn run_hands_the_wasm_command_ccids_browser_runner() {
    let fixture = Fixture::new(&wasm_manifest(
        "cp \"$STATE\"/coverage.* \"$CCID_COVERAGE_OUT\"/ && echo \"$CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER\" > \"$STATE\"/runner",
    ));
    fixture.reports(1);
    let output = fixture
        .command(&["coverage", "run"])
        .env("STATE", fixture.state_dir())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let runner = fs::read_to_string(fixture.state_dir().join("runner")).unwrap();
    assert!(
        runner.trim_end().ends_with(" coverage wasm-runner"),
        "{runner}"
    );
}

#[test]
fn run_refuses_when_the_wasm_reports_leave_a_line_uncovered() {
    let fixture = Fixture::new(&wasm_manifest(
        "cp \"$STATE\"/wasm-bad/coverage.* \"$CCID_COVERAGE_OUT\"/",
    ));
    fixture.reports(0);
    fs::create_dir_all(fixture.state_dir().join("wasm-bad")).unwrap();
    for name in ["coverage.json", "coverage.lcov", "coverage.txt"] {
        fs::copy(
            fixture.state_dir().join(name),
            fixture.state_dir().join("wasm-bad").join(name),
        )
        .unwrap();
    }
    // The native execution is complete; only the wasm reports are short.
    fixture.reports(1);
    let output = fixture
        .command(&["coverage", "run"])
        .env("STATE", fixture.state_dir())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Uncovered source lines"),
        "{}",
        text(&output)
    );
}

#[test]
fn run_stops_when_the_wasm_command_fails() {
    let fixture = Fixture::new(&wasm_manifest("exit 7"));
    let output = fixture.run();
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
}

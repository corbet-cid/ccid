//! Live worker acceptance. Requires preinstalled moon and GNU time; never installs tools.
#![forbid(unsafe_code)]
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::Instant,
};

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
            "user.email=ci@example.invalid"
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
        .status
        .success());
}

fn fixture(parent: &Path, name: &str, docs: &str, value: u64) -> PathBuf {
    let root = parent.join(name);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir(root.join(".ci")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='cache-acceptance-fixture'\nversion='0.0.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        root.join("Cargo.lock"),
        "version = 4\n[[package]]\nname = \"cache-acceptance-fixture\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), format!("pub fn value() -> u64 {{ {value} }}\n#[test] fn compute() {{ assert!(std::env::var_os(\"CCID_RECEIPT\").is_none()); assert!(std::env::var_os(\"CCID_CACHE_CHILD\").is_none()); let mut n = value(); for i in 0..30000000u64 {{ n = std::hint::black_box(n.wrapping_mul(6364136223846793005).wrapping_add(i)); }} assert_ne!(n, 0); }}\n")).unwrap();
    fs::write(root.join("README.md"), docs).unwrap();
    fs::write(root.join(".ci/ccid.toml"), "schema = 1\nproject = 'cache-acceptance-fixture'\n[checks.test]\nkind = 'cargo'\nactions = ['test']\n").unwrap();
    git(&root, &["init", "--quiet"]);
    git(
        &root,
        &["add", "Cargo.toml", "Cargo.lock", "src", "README.md", ".ci"],
    );
    git(&root, &["commit", "--quiet", "-m", "Fixture source"]);
    root
}

struct Trial {
    output: Output,
    cpu: f64,
    wall: f64,
    metrics: Option<Value>,
}

struct Harness {
    binary: PathBuf,
    time: PathBuf,
    path: std::ffi::OsString,
    cache: PathBuf,
    compile: PathBuf,
    remote: Option<String>,
}
impl Harness {
    fn command(&self, root: &Path, cached: bool, usage: &Path) -> Command {
        let mut command = Command::new(&self.time);
        command
            .args(["-f", "{\"user\":%U,\"system\":%S}", "-o"])
            .arg(usage)
            .arg("--")
            .arg(&self.binary)
            .args(["check", "--repo"])
            .arg(root)
            .args(["--check", "test"])
            .current_dir(root)
            .env("PATH", &self.path)
            .env("CI_CACHE_ROOT", &self.compile)
            .env(
                "CI_REPOSITORY_URL",
                "https://example.invalid/test/cache-fixture",
            )
            .env("CI_JOBS", "2")
            .env("CI_TEST_THREADS", "1")
            .env("CI_TIMEOUT", "180")
            .env("CMNP_TIME", &self.time)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .env_remove("CARGO_BUILD_BUILD_DIR")
            .env_remove("CCID_TARGET_LOCK_HELD")
            .env_remove("CCID_RECEIPT")
            .env_remove("CCID_CACHE_REPLAY_ONLY")
            .env_remove("CCID_RESULT_CACHE")
            .env_remove("CCID_CACHE_CHILD");
        if cached {
            command.env("CCID_RESULT_CACHE", &self.cache);
        } else {
            command.env("CCID_CACHE_CHILD", "1");
        }
        if let Some(remote) = &self.remote {
            command.env("CCID_REMOTE_CACHE", remote);
        }
        command
    }
    fn run(&self, root: &Path, cached: bool) -> Trial {
        // The timer output is outside the source tree so it cannot change a key.
        let timer = tempfile::NamedTempFile::new_in(&self.compile).unwrap();
        let started = Instant::now();
        let output = self.command(root, cached, timer.path()).output().unwrap();
        let wall = started.elapsed().as_secs_f64();
        assert!(
            output.status.success(),
            "stdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let cpu: Value = serde_json::from_slice(&fs::read(timer.path()).unwrap()).unwrap();
        let metrics = fs::read(root.join(".ccid/cache-metrics.json"))
            .ok()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap());
        Trial {
            output,
            cpu: cpu["user"].as_f64().unwrap() + cpu["system"].as_f64().unwrap(),
            wall,
            metrics,
        }
    }
}

fn computed(trial: &Trial) -> u64 {
    trial.metrics.as_ref().unwrap()["checks"][0]["computed"]
        .as_u64()
        .unwrap()
}
fn record(trial: &Trial) -> Value {
    json!({"cpu_seconds":trial.cpu,"wall_seconds":trial.wall,"cache":trial.metrics})
}

#[test]
#[ignore = "Run explicitly on a worker with CCID_TEST_MOON and CCID_TEST_TIME"]
fn unchanged_docs_and_concurrent_jobs() {
    let moon = PathBuf::from(std::env::var_os("CCID_TEST_MOON").expect("preinstalled moon binary"));
    let time =
        PathBuf::from(std::env::var_os("CCID_TEST_TIME").expect("preinstalled GNU time binary"));
    let parent = std::env::var_os("CCID_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::tempdir_in(parent).unwrap();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_ccid"));
    let mut paths = vec![
        binary.parent().unwrap().to_path_buf(),
        moon.parent().unwrap().to_path_buf(),
    ];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let harness = Harness {
        binary,
        time,
        path: std::env::join_paths(paths).unwrap(),
        cache: owned.path().join("results"),
        compile: owned.path().join("compile"),
        remote: std::env::var("CCID_TEST_REMOTE").ok(),
    };
    fs::create_dir(&harness.cache).unwrap();
    fs::create_dir(&harness.compile).unwrap();
    let baseline = fixture(owned.path(), "baseline", "Original docs\n", 1);
    let prime = harness.run(&baseline, false);
    let before = harness.run(&baseline, false);
    let cold = harness.run(&fixture(owned.path(), "cold", "Original docs\n", 1), true);
    let warm = harness.run(&fixture(owned.path(), "warm", "Original docs\n", 1), true);
    assert_eq!(computed(&warm), 0, "unchanged result must be restored");
    let docs = harness.run(
        &fixture(owned.path(), "docs", "Updated unrelated docs\n", 1),
        true,
    );
    assert!(
        !String::from_utf8_lossy(&docs.output.stderr)
            .contains("Compiling cache-acceptance-fixture"),
        "docs edit recompiled the fixture: {}",
        String::from_utf8_lossy(&docs.output.stderr)
    );
    let a = fixture(owned.path(), "concurrent-a", "Original docs\n", 2);
    let b = fixture(owned.path(), "concurrent-b", "Original docs\n", 2);
    let (a, b) = std::thread::scope(|scope| {
        let one = scope.spawn(|| harness.run(&a, true));
        let two = scope.spawn(|| harness.run(&b, true));
        (one.join().unwrap(), two.join().unwrap())
    });
    assert_eq!(
        computed(&a) + computed(&b),
        1,
        "identical concurrent processes must compute once"
    );
    for trial in [&cold, &warm, &docs, &a, &b] {
        assert_eq!(
            trial.metrics.as_ref().unwrap()["checks"][0]["duplicate_computations"],
            0
        );
    }
    let result = json!({"event":"cache-acceptance","scope":"separate processes and workspaces on one worker; native remote configured externally","prime":record(&prime),"before":record(&before),"cold":record(&cold),"after":record(&warm),"docs":record(&docs),"concurrent":[record(&a),record(&b)],"before_cpu_minutes":before.cpu/60.0,"after_cpu_minutes":warm.cpu/60.0,"saved_cpu_minutes":(before.cpu-warm.cpu)/60.0});
    println!("{result}");
    if let Some(path) = std::env::var_os("CCID_TEST_REPORT") {
        fs::write(path, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    }
}

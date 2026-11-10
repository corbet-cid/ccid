#![forbid(unsafe_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ccid"))
}

const CHANNEL: &str = r#"{"default":{"revision":"r","sha256":"s"},"fleet":{"schema":1,"first_party":["https://forge.example/org-a/"],"aliases":{"https://mirror.example/org-a/":"https://forge.example/org-a/"}}}"#;

fn channel(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, CHANNEL).unwrap();
    path
}

fn ccid_direct(repo: &Path, fleet_file: Option<&Path>, args: &[&str]) -> Output {
    let mut command = Command::new(binary());
    command.arg("first-party").arg("--repo").arg(repo);
    for arg in args {
        command.arg(arg);
    }
    for name in [
        "CCID_RESULT_CACHE",
        "CCID_REMOTE_CACHE",
        "CCID_CACHE_CHILD",
        "CCID_CACHE_REPLAY_ONLY",
        "CCID_TARGET_LOCK_HELD",
        "CCID_RECEIPT",
        "CCID_CHANNEL_FILE",
        "CCID_FLEET_FILE",
        "CCID_FLEET_DIGEST",
        "CCID_FLEET_KEYED",
        "CCID_FORGE_HOSTS",
    ] {
        command.env_remove(name);
    }
    if let Some(file) = fleet_file {
        command.env("CCID_FLEET_FILE", file);
    }
    command.output().unwrap()
}

fn ccid_check(repo: &Path, fleet_file: Option<&Path>, checks: &[&str]) -> Output {
    let mut command = Command::new(binary());
    command.arg("check").arg("--repo").arg(".");
    for name in checks {
        command.args(["--check", name]);
    }
    command.current_dir(repo);
    for name in [
        "CCID_RESULT_CACHE",
        "CCID_REMOTE_CACHE",
        "CCID_CACHE_CHILD",
        "CCID_CACHE_REPLAY_ONLY",
        "CCID_TARGET_LOCK_HELD",
        "CCID_RECEIPT",
        "CCID_CHANNEL_FILE",
        "CCID_FLEET_FILE",
        "CCID_FLEET_DIGEST",
        "CCID_FLEET_KEYED",
        "CCID_FORGE_HOSTS",
        "CI_MIN_AVAILABLE_MB",
        "CI_MEMORY_MB",
        "CI_MEMORY_PER_JOB_MB",
    ] {
        command.env_remove(name);
    }
    command
        .env("CARGO_TARGET_DIR", repo.join("target"))
        .env("TMPDIR", repo)
        .env("CI_JOBS", "1")
        .env("CI_NIX_JOBS", "1")
        .env("CI_TIMEOUT", "120");
    if let Some(file) = fleet_file {
        command.env("CCID_FLEET_FILE", file);
    }
    command.output().unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn git_init(root: &Path) {
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success());
    let status = Command::new("git")
        .args(["add", "-A"])
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success());
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=proof",
            "-c",
            "user.email=proof@invalid",
            "commit",
            "-qm",
            "snapshot",
        ])
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success());
}

fn repo_with_manifest(root: &Path, manifest: &str, lock: Option<&str>) {
    fs::create_dir_all(root.join(".ci")).unwrap();
    fs::write(
        root.join(".ci/ccid.toml"),
        format!("schema = 1\nproject = 'fixture'\n{manifest}"),
    )
    .unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    if let Some(lock) = lock {
        fs::write(root.join("Cargo.lock"), lock).unwrap();
    }
}

fn valid_lock() -> String {
    format!(
        "[[package]]\nname = \"example\"\nversion = \"0.1.0\"\nsource = \"git+https://forge.example/org-a/example?branch=main#{}\"\n",
        "a".repeat(40)
    )
}

#[test]
fn first_party_passes_refuses_and_reports_operational_errors() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    repo_with_manifest(root, "", Some(&valid_lock()));
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[dependencies.example]\ngit = \"https://forge.example/org-a/example\"\nbranch = \"main\"\n",
    )
    .unwrap();
    git_init(root);
    let fleet = channel(root, "channel.json");

    let pass = ccid_direct(root, Some(&fleet), &[]);
    assert_eq!(pass.status.code(), Some(0), "{}", text(&pass));
    assert!(String::from_utf8_lossy(&pass.stdout)
        .contains("First-party declarations follow main; locked crate revisions are unique"));

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[dependencies.example]\ngit = \"https://forge.example/org-a/example\"\nbranch = \"develop\"\n",
    )
    .unwrap();
    let refused = ccid_direct(root, Some(&fleet), &[]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused));
    assert!(String::from_utf8_lossy(&refused.stderr).contains(
        "check-first-party: First-party dependency must declare branch = \"main\" (Cargo.toml)"
    ));

    fs::write(root.join("Cargo.toml"), "not toml [").unwrap();
    let broken = ccid_direct(root, Some(&fleet), &[]);
    assert_eq!(broken.status.code(), Some(2), "{}", text(&broken));
    assert!(String::from_utf8_lossy(&broken.stderr).contains("ccid first-party:"));

    let no_fleet = ccid_direct(root, None, &[]);
    assert_eq!(no_fleet.status.code(), Some(2), "{}", text(&no_fleet));
}

/// Manifests and locks are checked in one sorted path order, as the scripts
/// walked `git ls-files`: the first refusal names the first bad path, and a
/// later unreadable file does not hide it.
#[test]
fn first_party_reports_the_first_refusal_in_path_order() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    repo_with_manifest(root, "", None);
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    fs::write(
        root.join("a/Cargo.lock"),
        valid_lock().replace("branch=main", "branch=develop"),
    )
    .unwrap();
    fs::write(
        root.join("b/Cargo.toml"),
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\n[dependencies.example]\ngit = \"https://forge.example/org-a/example\"\nbranch = \"develop\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("c")).unwrap();
    fs::write(root.join("c/Cargo.toml"), "not toml [").unwrap();
    git_init(root);
    let fleet = channel(root, "channel.json");

    let refused = ccid_direct(root, Some(&fleet), &[]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(
            "check-first-party: Invalid first-party source selector for example (a/Cargo.lock)"
        ),
        "{}",
        text(&refused)
    );

    let only_manifests = ccid_direct(root, Some(&fleet), &["--manifest-only"]);
    assert_eq!(
        only_manifests.status.code(),
        Some(1),
        "{}",
        text(&only_manifests)
    );
    assert!(
        String::from_utf8_lossy(&only_manifests.stderr).contains(
            "check-first-party: First-party dependency must declare branch = \"main\" (b/Cargo.toml)"
        ),
        "{}",
        text(&only_manifests)
    );
}

#[test]
fn first_party_self_test_and_metadata() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join(".ci")).unwrap();
    fs::write(
        root.join(".ci/ccid.toml"),
        "schema = 1\nproject = 'fixture'\n",
    )
    .unwrap();
    let fleet = channel(root, "channel.json");

    let self_test = ccid_direct(root, Some(&fleet), &["--self-test"]);
    assert_eq!(self_test.status.code(), Some(0), "{}", text(&self_test));
    assert!(String::from_utf8_lossy(&self_test.stdout)
        .contains("first-party: the self-test is ccid's own test suite"));
}

/// The `cargo metadata` document of a workspace member `fixture` that depends on the
/// first-party package `example` locked at `example_source`.
fn graph(example_source: &str) -> String {
    let declaration = "git+https://forge.example/org-a/example?branch=main";
    serde_json::json!({
        "version": 1,
        "packages": [
            {"id": "fixture", "name": "fixture", "version": "0.1.0", "source": null,
             "targets": [{"kind": ["lib"], "name": "fixture"}],
             "dependencies": [{"name": "example", "source": declaration, "kind": null,
                               "target": null, "rename": null}]},
            {"id": "example", "name": "example", "version": "0.1.0", "source": example_source,
             "targets": [{"kind": ["lib"], "name": "example"}], "dependencies": []},
        ],
        "workspace_members": ["fixture"],
        "resolve": {"root": "fixture", "nodes": [
            {"id": "fixture", "dependencies": ["example"], "features": [],
             "deps": [{"name": "example", "pkg": "example",
                       "dep_kinds": [{"kind": null, "target": null}]}]},
            {"id": "example", "dependencies": [], "deps": [], "features": []},
        ]},
    })
    .to_string()
}

fn locked_example() -> String {
    format!(
        "git+https://forge.example/org-a/example?branch=main#{}",
        "a".repeat(40)
    )
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn first_party_metadata_passes_refuses_and_reports_operational_errors() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    repo_with_manifest(root, "", Some(&valid_lock()));
    git_init(root);
    let fleet = channel(root, "channel.json");
    let outside = tempfile::TempDir::new().unwrap();
    let write = |name: &str, content: &str| -> String {
        let path = outside.path().join(name);
        fs::write(&path, content).unwrap();
        path.display().to_string()
    };

    let good = write("good.json", &graph(&locked_example()));
    let pass = ccid_direct(root, Some(&fleet), &["--metadata", &good]);
    assert_eq!(pass.status.code(), Some(0), "{}", text(&pass));
    let stdout = String::from_utf8_lossy(&pass.stdout);
    assert!(
        stdout.contains("First-party declarations follow main; locked crate revisions are unique")
    );
    assert!(
        stdout.contains("First-party dependencies resolve once each to a complete source revision")
    );

    // A graph refusal is exit 1 and names the graph file.
    let floating = locked_example().replace("branch=main", "branch=develop");
    let bad = write("bad.json", &graph(&floating));
    let refused = ccid_direct(root, Some(&fleet), &["--metadata", &bad]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused));
    assert!(
        stderr(&refused).contains(&format!(
            "check-first-party: Mismatched selected dependency source ({bad})"
        )),
        "{}",
        text(&refused)
    );

    // Unreadable or unparseable graph input is exit 2.
    let missing = outside.path().join("absent.json").display().to_string();
    let absent = ccid_direct(root, Some(&fleet), &["--metadata", &missing]);
    assert_eq!(absent.status.code(), Some(2), "{}", text(&absent));
    assert!(
        stderr(&absent).contains("ccid first-party: cannot read"),
        "{}",
        text(&absent)
    );
    let garbage = write("garbage.json", "{not json");
    let broken = ccid_direct(root, Some(&fleet), &["--metadata", &garbage]);
    assert_eq!(broken.status.code(), Some(2), "{}", text(&broken));
    assert!(
        stderr(&broken).contains("ccid first-party: cannot parse"),
        "{}",
        text(&broken)
    );
    let array = write("array.json", "[]");
    let wrong = ccid_direct(root, Some(&fleet), &["--metadata", &array]);
    assert_eq!(wrong.status.code(), Some(2), "{}", text(&wrong));
    assert!(
        stderr(&wrong).contains("not a JSON object"),
        "{}",
        text(&wrong)
    );

    // A JSON object that is not a cargo metadata graph is a refusal, never a pass.
    let empty = write("empty.json", "{}");
    let foreign = ccid_direct(root, Some(&fleet), &["--metadata", &empty]);
    assert_eq!(foreign.status.code(), Some(1), "{}", text(&foreign));
    assert!(
        stderr(&foreign).contains("check-first-party: Missing original Cargo metadata graph"),
        "{}",
        text(&foreign)
    );

    // The graph and the manifest scan are exclusive in one flag only.
    let exclusive = ccid_direct(
        root,
        Some(&fleet),
        &["--metadata", &good, "--manifest-only"],
    );
    assert_eq!(exclusive.status.code(), Some(2), "{}", text(&exclusive));
    assert!(stderr(&exclusive).contains("--metadata and --manifest-only are exclusive"));

    // Fleet data is required here too.
    let no_fleet = ccid_direct(root, None, &["--metadata", &good]);
    assert_eq!(no_fleet.status.code(), Some(2), "{}", text(&no_fleet));
}

/// The scan runs first, as in the scripts that took `--metadata` after their lock walk: its
/// refusal is the one reported, while an unreadable graph file is read before the scan.
#[test]
fn first_party_metadata_follows_the_scan_and_reads_its_file_first() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    repo_with_manifest(
        root,
        "",
        Some(&valid_lock().replace("branch=main", "branch=develop")),
    );
    git_init(root);
    let fleet = channel(root, "channel.json");
    let outside = tempfile::TempDir::new().unwrap();
    let bad = outside.path().join("bad.json");
    fs::write(
        &bad,
        graph(&locked_example().replace("branch=main", "branch=develop")),
    )
    .unwrap();
    let bad = bad.display().to_string();

    let refused = ccid_direct(root, Some(&fleet), &["--metadata", &bad]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused));
    assert!(
        stderr(&refused).contains(
            "check-first-party: Invalid first-party source selector for example (Cargo.lock)"
        ),
        "{}",
        text(&refused)
    );
    let missing = outside.path().join("absent.json").display().to_string();
    let absent = ccid_direct(root, Some(&fleet), &["--metadata", &missing]);
    assert_eq!(absent.status.code(), Some(2), "{}", text(&absent));
}

/// `[first_party.graph] required` comes from the manifest: a graph without the package is
/// refused, and an unknown graph field is a bad manifest.
#[test]
fn first_party_metadata_requires_the_manifest_declared_packages() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    repo_with_manifest(
        root,
        "[first_party.graph]\nrequired = [\"core\"]\n",
        Some(&valid_lock()),
    );
    git_init(root);
    let fleet = channel(root, "channel.json");
    let outside = tempfile::TempDir::new().unwrap();
    let good = outside.path().join("good.json");
    fs::write(&good, graph(&locked_example())).unwrap();
    let good = good.display().to_string();

    let refused = ccid_direct(root, Some(&fleet), &["--metadata", &good]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused));
    assert!(
        stderr(&refused).contains(&format!(
            "check-first-party: Missing, invalid or duplicated first-party dependency: core ({good})"
        )),
        "{}",
        text(&refused)
    );
    // Without --metadata the table is inert: the scan alone passes.
    let scan = ccid_direct(root, Some(&fleet), &[]);
    assert_eq!(scan.status.code(), Some(0), "{}", text(&scan));

    fs::write(
        root.join(".ci/ccid.toml"),
        "schema = 1\nproject = 'fixture'\n[first_party.graph]\nunknown = true\n",
    )
    .unwrap();
    let bad_table = ccid_direct(root, Some(&fleet), &["--metadata", &good]);
    assert_eq!(bad_table.status.code(), Some(2), "{}", text(&bad_table));
    fs::write(
        root.join(".ci/ccid.toml"),
        "schema = 1\nproject = 'fixture'\n[first_party.graph]\nrequired = ['']\n",
    )
    .unwrap();
    let empty_name = ccid_direct(root, Some(&fleet), &["--metadata", &good]);
    assert_eq!(empty_name.status.code(), Some(2), "{}", text(&empty_name));
}

#[test]
fn first_party_archive_nested_require_root_and_lock_per_manifest() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    repo_with_manifest(root, "", Some(&valid_lock()));
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(
        root.join("nested/Cargo.toml"),
        "[package]\nname = \"nested\"\nversion = \"0.1.0\"\n[dependencies.example]\ngit = \"https://forge.example/org-a/example\"\nbranch = \"main\"\n",
    )
    .unwrap();
    fs::write(root.join("nested/Cargo.lock"), valid_lock()).unwrap();
    git_init(root);
    let fleet = channel(root, "channel.json");

    let pass = ccid_direct(root, Some(&fleet), &[]);
    assert_eq!(pass.status.code(), Some(0), "{}", text(&pass));

    let export = tempfile::TempDir::new().unwrap();
    let copy = export.path().join("copy");
    fs::create_dir_all(&copy).unwrap();
    for entry in ["Cargo.toml", "Cargo.lock", ".ci", "nested"] {
        let from = root.join(entry);
        let to = copy.join(entry);
        if from.is_dir() {
            fs_extra_copy_dir(&from, &to);
        } else {
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::copy(&from, &to).unwrap();
        }
    }
    assert!(!copy.join(".git").exists());
    let archived = ccid_direct(&copy, Some(&fleet), &["--archive"]);
    assert_eq!(archived.status.code(), Some(0), "{}", text(&archived));

    let strict = tempfile::TempDir::new().unwrap();
    let root2 = strict.path();
    repo_with_manifest(root2, "[first_party]\nrequire_root_lock = true\n", None);
    git_init(root2);
    let fleet2 = channel(root2, "channel.json");
    let missing = ccid_direct(root2, Some(&fleet2), &[]);
    assert_eq!(missing.status.code(), Some(1), "{}", text(&missing));
    assert!(String::from_utf8_lossy(&missing.stderr)
        .contains("Missing committed root manifest or lock"));

    let tracked = tempfile::TempDir::new().unwrap();
    let root3 = tracked.path();
    repo_with_manifest(root3, "[first_party]\nlock_per_manifest = true\n", None);
    fs::write(
        root3.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    git_init(root3);
    let fleet3 = channel(root3, "channel.json");
    let absent = ccid_direct(root3, Some(&fleet3), &[]);
    assert_eq!(absent.status.code(), Some(1), "{}", text(&absent));
    assert!(String::from_utf8_lossy(&absent.stderr).contains("Missing resolved snapshot"));

    fs::write(
        root3.join("Cargo.lock"),
        "[[package]]\nname = \"local\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let now_present = ccid_direct(root3, Some(&fleet3), &[]);
    assert_eq!(now_present.status.code(), Some(0), "{}", text(&now_present));
}

fn fs_extra_copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let dest = to.join(&name);
        if entry.file_type().unwrap().is_dir() {
            fs_extra_copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

#[test]
fn first_party_runs_inside_a_fleet_keyed_check_only() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path();
    let binary = binary();
    let binary = binary.to_string_lossy().replace('\\', "\\\\");
    fs::create_dir_all(root.join(".ci")).unwrap();
    fs::write(
        root.join(".ci/ccid.toml"),
        format!(
            "schema = 1\nproject = 'fixture'\n[checks.consumer]\nkind = 'commands'\nfleet = true\ncommands = [[\"{binary}\", \"first-party\", \"--repo\", \".\"]]\n[checks.undeclared]\nkind = 'commands'\ncommands = [[\"{binary}\", \"first-party\", \"--repo\", \".\"]]\n"
        ),
    )
    .unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let fleet = channel(root, "channel.json");

    let keyed = ccid_check(root, Some(&fleet), &["consumer"]);
    assert!(keyed.status.success(), "{}", text(&keyed));

    let undeclared = ccid_check(root, Some(&fleet), &["undeclared"]);
    assert!(!undeclared.status.success(), "{}", text(&undeclared));
    assert!(
        String::from_utf8_lossy(&undeclared.stderr).contains("declare fleet = true"),
        "{}",
        text(&undeclared)
    );
}

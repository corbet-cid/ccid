use super::*;
use std::{ffi::OsStr, path::PathBuf};

const CHANNEL: &str = r#"{"fleet":{"schema":1,"forge_hosts":["forge.example"],"first_party":["https://forge.example/org/"]}}"#;
const CRATES: &str = "registry+https://github.com/rust-lang/crates.io-index";

fn package(name: &str, version: &str, source: &str) -> String {
    format!("[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = \"{source}\"\n")
}

fn first_party(commit: char) -> String {
    format!(
        "git+https://forge.example/org/dep?branch=main#{}",
        commit.to_string().repeat(40)
    )
}

fn lock() -> String {
    format!(
        "version = 4\n\n{}\n{}\n{}\n{}",
        package("dep", "0.1.0", &first_party('1')),
        package(
            "foreign",
            "0.1.0",
            &format!(
                "git+https://forge.example/other/foreign?branch=main#{}",
                "4".repeat(40)
            )
        ),
        package("cgrade", "0.3.0", CRATES),
        package("serde", "1.0.0", CRATES),
    )
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=refresh",
            "-c",
            "user.email=refresh@example.invalid",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A committed repository with a root lock and a nested one, a stub cargo that logs its
/// arguments and moves the entries it is asked to move, and fleet data.
struct Fixture {
    temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        for directory in ["src", "tools/x"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        for (directory, name) in [("", "consumer"), ("tools/x/", "x")] {
            fs::write(
                root.join(format!("{directory}Cargo.toml")),
                format!("[package]\nname = '{name}'\nversion = '0.1.0'\nedition = '2021'\n"),
            )
            .unwrap();
            fs::write(root.join(format!("{directory}Cargo.lock")), lock()).unwrap();
        }
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(root.join("README.md"), "fixture\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "fixture"]);
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let log = temp.path().join("cargo.log");
        // `update` alone moves serde; `-p` moves the named entries.
        let script = format!(
            r#"#!/bin/sh
echo "$(basename "$PWD")|$*" >> {log}
[ "$1" = update ] || exit 9
shift
if [ $# -eq 0 ]; then sed -i 's/^version = "1.0.0"$/version = "1.0.1"/' Cargo.lock; exit 0; fi
while [ $# -gt 0 ]; do
  case "$2" in
    git+*dep@0.1.0) sed -i 's/{one}/{two}/' Cargo.lock ;;
    cgrade@0.3.0) sed -i 's/^version = "0.3.0"$/version = "0.3.1"/' Cargo.lock ;;
    *) exit 8 ;;
  esac
  shift 2
done
"#,
            log = log.display(),
            one = "1".repeat(40),
            two = "2".repeat(40)
        );
        let path = bin.join("cargo");
        fs::write(&path, script).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&path, permissions).unwrap();
        fs::write(temp.path().join("channel.json"), CHANNEL).unwrap();
        Fixture { temp }
    }

    fn root(&self) -> PathBuf {
        self.temp.path().join("repo")
    }

    fn environment(&self) -> Environment {
        let mut environment: Environment = std::env::vars_os().collect();
        for name in [
            "CCID_TARGET_LOCK_HELD",
            "CCID_CACHE_CHILD",
            fleet::FORGE_HOSTS_ENV,
        ] {
            environment.remove(OsStr::new(name));
        }
        let path = format!(
            "{}:{}",
            self.temp.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        environment.insert("PATH".into(), path.into());
        environment.insert(
            fleet::FLEET_FILE_ENV.into(),
            self.temp.path().join("channel.json").into(),
        );
        environment.insert("TMPDIR".into(), self.temp.path().into());
        environment
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.temp.path().join("cargo.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Tracked files that differ from the commit, then untracked files.
    fn changed_files(&self) -> Vec<String> {
        let root = self.root();
        let mut files: Vec<String> = git(&root, &["diff", "--name-only"])
            .lines()
            .map(str::to_owned)
            .collect();
        files.extend(
            git(&root, &["ls-files", "--others", "--exclude-standard"])
                .lines()
                .map(str::to_owned),
        );
        files
    }
}

#[test]
fn first_party_moves_branch_pins_and_named_registry_packages_and_nothing_else() {
    let fixture = Fixture::new();
    let packages = vec!["cgrade".to_owned(), "not-a-dependency".to_owned()];
    assert_eq!(
        refresh(&fixture.root(), &packages, false, &fixture.environment()).unwrap(),
        0
    );
    // HR2: exactly the first-party spec and the named registry version, never a plain update,
    // never the third-party git package of another owner.
    let spec = "git+https://forge.example/org/dep?branch=main#dep@0.1.0";
    let expected = format!("update -p cgrade@0.3.0 -p {spec}");
    let calls = fixture.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    for call in &calls {
        assert!(call.ends_with(&format!("|{expected}")), "{call}");
    }
    // HR1: only the lock files changed in the checkout.
    assert_eq!(
        fixture.changed_files(),
        ["Cargo.lock", "tools/x/Cargo.lock"]
    );
    let text = fs::read_to_string(fixture.root().join("Cargo.lock")).unwrap();
    assert!(text.contains(&first_party('2')));
    assert!(text.contains("version = \"0.3.1\""));
    assert!(text.contains("version = \"1.0.0\""), "serde stays");
}

#[test]
fn third_party_is_one_plain_update_per_lock() {
    let fixture = Fixture::new();
    refresh(&fixture.root(), &[], true, &fixture.environment()).unwrap();
    let mut calls = fixture.calls();
    calls.sort();
    assert_eq!(calls, ["lock-refresh|update", "x|update"]);
    assert_eq!(
        fixture.changed_files(),
        ["Cargo.lock", "tools/x/Cargo.lock"]
    );
    let text = fs::read_to_string(fixture.root().join("Cargo.lock")).unwrap();
    assert!(text.contains("version = \"1.0.1\""));
}

#[test]
fn a_lock_with_nothing_to_move_runs_no_cargo_and_writes_nothing() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let only_registry = format!("version = 4\n\n{}", package("serde", "1.0.0", CRATES));
    for lock in ["Cargo.lock", "tools/x/Cargo.lock"] {
        fs::write(root.join(lock), &only_registry).unwrap();
    }
    git(&root, &["commit", "-qam", "registry only"]);
    refresh(&root, &["cgrade".to_owned()], false, &fixture.environment()).unwrap();
    assert!(fixture.calls().is_empty());
    assert!(fixture.changed_files().is_empty());
}

#[test]
fn refuses_inside_a_check_without_fleet_data_or_with_a_bad_name() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let mut inside = fixture.environment();
    inside.insert("CCID_TARGET_LOCK_HELD".into(), "/target".into());
    let error = refresh(&root, &[], false, &inside).unwrap_err().to_string();
    assert!(error.contains("never runs inside a ccid check"), "{error}");
    let mut child = fixture.environment();
    child.insert("CCID_CACHE_CHILD".into(), "1".into());
    assert!(refresh(&root, &[], false, &child).is_err());
    let mut no_fleet = fixture.environment();
    no_fleet.remove(OsStr::new(fleet::FLEET_FILE_ENV));
    let error = refresh(&root, &[], false, &no_fleet)
        .unwrap_err()
        .to_string();
    assert!(error.contains("needs fleet data"), "{error}");
    for bad in ["", "-p", "a b", "../x", "9lives"] {
        assert!(
            refresh(&root, &[bad.to_owned()], false, &fixture.environment()).is_err(),
            "{bad:?}"
        );
    }
    assert!(fixture.calls().is_empty());
    assert!(fixture.changed_files().is_empty());
}

#[test]
fn the_plan_adds_registry_versions_to_the_first_party_selection() {
    let committed = BTreeMap::from([("Cargo.lock".to_owned(), lock().into_bytes())]);
    let selection = float::Selection::from([(
        "Cargo.lock".to_owned(),
        BTreeSet::from(["git+x#dep@0.1.0".to_owned()]),
    )]);
    let planned = plan(&committed, &selection, &["serde".to_owned()], false).unwrap();
    assert_eq!(
        planned["Cargo.lock"].arguments(),
        ["update", "-p", "git+x#dep@0.1.0", "-p", "serde@1.0.0"]
    );
    assert!(plan(&committed, &float::Selection::new(), &[], false)
        .unwrap()
        .is_empty());
    assert_eq!(
        plan(&committed, &selection, &[], true).unwrap()["Cargo.lock"],
        Update::All
    );
}

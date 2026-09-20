#![forbid(unsafe_code)]
//! One isolated Cargo experiment covering the verified archive execution path.
//! No network dependencies, no existing caches, and no product suite duplication.
#[cfg(target_os = "linux")]
mod linux {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
    };
    use tempfile::TempDir;

    fn output(command: &mut Command) -> String {
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    }
    fn git(repo: &Path, args: &[&str]) -> String {
        output(Command::new("git").current_dir(repo).args(args))
            .trim()
            .into()
    }
    fn commit(repo: &Path) -> String {
        git(repo, &["add", "--all"]);
        git(
            repo,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"],
        );
        git(repo, &["rev-parse", "HEAD"])
    }
    struct Fixture {
        _temp: TempDir,
        root: PathBuf,
        repo: PathBuf,
        target: PathBuf,
    }
    impl Fixture {
        fn command(&self) -> Command {
            let mut command = Command::new(env!("CARGO_BIN_EXE_ccid"));
            command
                .env(
                    "CI_REPOSITORY_URL",
                    "https://fixture.invalid/owner/freshness",
                )
                .env("CARGO_HOME", self.root.join("cargo-home"))
                .env("CARGO_TARGET_DIR", &self.target)
                .env("TMPDIR", self.root.join("scratch"))
                .env("CI_JOBS", "2")
                .env("CI_NIX_JOBS", "1")
                .env("CI_TIMEOUT", "30")
                .env("CARGO_PROFILE_DEV_DEBUG", "0")
                .env("CARGO_INCREMENTAL", "0")
                .env_remove("CARGO_BUILD_TARGET_DIR")
                .env_remove("CARGO_BUILD_BUILD_DIR")
                .env_remove("RUSTC_WRAPPER")
                .env_remove("RUSTC_WORKSPACE_WRAPPER")
                .env_remove("RUSTFLAGS")
                .env_remove("CARGO_ENCODED_RUSTFLAGS")
                .env_remove("CI_MEMORY_MB")
                .env_remove("CI_MEMORY_PER_JOB_MB")
                .env_remove("CI_MIN_AVAILABLE_MB")
                .env(
                    "CCID_FIXTURE_COMMIT_REPORT",
                    self.root.join("verified-commit"),
                )
                .env(
                    "CCID_FIXTURE_OUTPUT_REPORT",
                    self.root.join("fixture-output"),
                );
            command
        }
        fn local(&self) {
            let result = self
                .command()
                .args(["check", "--repo"])
                .arg(&self.repo)
                .args(["--check", "build"])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(!self.target.join(".ccid/source-state.json").exists());
        }
        fn run(&self, revision: &str, selector: &str, succeeds: bool) -> (usize, usize, String) {
            let archive = self.root.join("source.tar");
            git(
                &self.repo,
                &[
                    "archive",
                    "--format=tar",
                    "--output",
                    archive.to_str().unwrap(),
                    revision,
                ],
            );
            let result = self
                .command()
                .args(["check", "--archive"])
                .arg(&archive)
                .args([
                    "--sha256",
                    &ccid::sha256_file(&archive).unwrap(),
                    "--commit",
                    revision,
                    "--check",
                    selector,
                ])
                .output()
                .unwrap();
            let text = String::from_utf8(result.stdout).unwrap();
            assert_eq!(
                result.status.success(),
                succeeds,
                "{text}\n{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(
                fs::read_to_string(self.root.join("verified-commit")).unwrap(),
                revision
            );
            let (mut fresh, mut rebuilt) = (0, 0);
            for row in text
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            {
                if row["reason"] == "compiler-artifact" {
                    if row["fresh"] == true {
                        fresh += 1;
                    } else {
                        rebuilt += 1;
                    }
                }
            }
            assert_eq!(
                fs::read_dir(self.root.join("scratch")).unwrap().count(),
                0,
                "owned source and command scratch cleaned"
            );
            let value = fs::read_to_string(self.root.join("fixture-output"))
                .unwrap()
                .trim()
                .into();
            (fresh, rebuilt, value)
        }
    }

    #[test]
    fn verified_archives_rebuild_moved_roots_and_invalidate_changed_inputs_and_failure() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = root.join("repo");
        for path in [
            repo.join("src"),
            repo.join("inputs"),
            repo.join(".ci"),
            root.join("scratch"),
            root.join("cargo-home"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Fixture"]);
        git(&repo, &["config", "user.email", "fixture@example.invalid"]);
        fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname='cache-freshness'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
        )
        .unwrap();
        fs::write(
            repo.join("Cargo.lock"),
            "version = 4\n[[package]]\nname = \"cache-freshness\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(repo.join("src/main.rs"), "fn main() { let runtime = std::fs::read_to_string(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/runtime.txt\")).unwrap(); println!(\"1:{}:{}\", include!(concat!(env!(\"OUT_DIR\"), \"/value.rs\")), runtime); }\n").unwrap();
        fs::write(repo.join("runtime.txt"), "runtime").unwrap();
        fs::write(repo.join("inputs/a"), "A").unwrap();
        fs::write(repo.join("build.rs"), r#"fn main() {
            println!("cargo:rerun-if-changed=inputs");
            let mut files: Vec<_> = std::fs::read_dir("inputs").unwrap().map(|f| f.unwrap().path()).collect(); files.sort();
            let value = files.iter().map(|f| std::fs::read_to_string(f).unwrap()).collect::<Vec<_>>().join(",");
            std::fs::write(std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("value.rs"), format!("{:?}", value)).unwrap();
        }"#).unwrap();
        fs::write(repo.join(".ci/ccid.toml"), "schema=1\nproject='same-label'\n[checks.build]\nkind='commands'\ncommands=[['sh','-c','printf \"%s\" \"$CI_COMMIT_SHA\" > \"$CCID_FIXTURE_COMMIT_REPORT\"'],['cargo','build','--locked','--offline','--message-format=json'],['sh','-c','\"$CARGO_TARGET_DIR/debug/cache-freshness\" > \"$CCID_FIXTURE_OUTPUT_REPORT\"']]\n[checks.fail]\nkind='commands'\ncommands=[['false']]\n[checks.mutate]\nkind='commands'\ncommands=[['sh','-c','printf changed > inputs/a']]\n").unwrap();
        let first = commit(&repo);
        let f = Fixture {
            _temp: temp,
            root: root.clone(),
            repo,
            target: root.join("target"),
        };
        let initial = f.run(&first, "build", true);
        assert!(initial.1 > 0);
        assert_eq!(initial.2, "1:A:runtime");
        let warm = f.run(&first, "build", true);
        assert!(
            warm.1 > 0,
            "a moved archive root must rebuild workspace outputs"
        );
        assert_eq!(warm.2, "1:A:runtime");
        fs::write(f.repo.join("src/main.rs"), "fn main() { let runtime = std::fs::read_to_string(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/runtime.txt\")).unwrap(); println!(\"2:{}:{}\", include!(concat!(env!(\"OUT_DIR\"), \"/value.rs\")), runtime); }\n").unwrap();
        fs::write(f.repo.join("inputs/b"), "B").unwrap();
        let newer = commit(&f.repo);
        let changed = f.run(&newer, "build", true);
        assert!(changed.1 > 0);
        assert_eq!(changed.2, "2:A,B:runtime");
        let rollback = f.run(&first, "build", true);
        assert!(rollback.1 > 0);
        assert_eq!(rollback.2, "1:A:runtime");
        fs::remove_file(f.repo.join("inputs/a")).unwrap();
        let deleted = commit(&f.repo);
        let deletion = f.run(&deleted, "build", true);
        assert!(deletion.1 > 0);
        assert_eq!(deletion.2, "2:B:runtime");
        assert!(f.run(&deleted, "build", true).1 > 0);
        fs::write(f.repo.join("inputs/b"), "C").unwrap();
        let input = commit(&f.repo);
        let input_changed = f.run(&input, "build", true);
        assert!(input_changed.1 > 0);
        assert_eq!(input_changed.2, "2:C:runtime");
        f.run(&input, "build,fail", false);
        assert!(!f.target.join(".ccid/source-state.json").exists());
        assert_eq!(f.run(&first, "build", true).2, "1:A:runtime");
        f.run(&first, "build,mutate", true);
        assert!(
            !f.target.join(".ccid/source-state.json").exists(),
            "mutated source must not publish freshness metadata"
        );
        let after_mutation = f.run(&first, "build", true);
        assert!(after_mutation.1 > 0);
        assert_eq!(after_mutation.2, "1:A:runtime");
        // Dirty local inputs must invalidate an archive ledger sharing the target.
        fs::write(f.repo.join("inputs/b"), "LOCAL").unwrap();
        f.local();
        assert_eq!(
            output(&mut Command::new(f.target.join("debug/cache-freshness"))).trim(),
            "2:LOCAL:runtime"
        );
        let after_local = f.run(&first, "build", true);
        assert!(after_local.1 > 0);
        assert_eq!(after_local.2, "1:A:runtime");
    }
}

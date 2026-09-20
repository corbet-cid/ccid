#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
mod linux {
    use std::{fs, path::Path, process::Command};
    use tempfile::TempDir;

    fn git(root: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(root)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn resolve(original: Option<&str>, generate: bool, check: &str) -> (bool, serde_json::Value) {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let source = root.join("source");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::create_dir_all(source.join(".ci")).unwrap();
        fs::write(source.join("src/lib.rs"), "pub fn value() -> u8 { 7 }\n").unwrap();
        fs::write(
            source.join("Cargo.toml"),
            "[package]\nname='lock-fixture'\nversion='0.2.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(source.join(".ci/ccid.toml"), "schema=1\nproject='lock-fixture'\n[checks.valid]\nkind='commands'\ncommands=[['cargo','metadata','--locked','--offline','--format-version','1']]\n[checks.mutate]\nkind='commands'\ncommands=[['sh','-c','printf changed >> Cargo.lock']]\n").unwrap();
        if let Some(bytes) = original {
            fs::write(source.join("Cargo.lock"), bytes).unwrap();
        }
        git(&source, &["init", "-q"]);
        git(&source, &["config", "user.name", "Fixture"]);
        git(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        );
        git(&source, &["add", "Cargo.toml", "src", ".ci"]);
        if original.is_some() {
            git(&source, &["add", "Cargo.lock"]);
        }
        git(
            &source,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"],
        );
        let commit = git(&source, &["rev-parse", "HEAD"]);
        let archive = root.join("source.tar");
        git(
            &source,
            &[
                "archive",
                "--format=tar",
                "--output",
                archive.to_str().unwrap(),
                "HEAD",
            ],
        );
        let output_dir = root.join("evidence");
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccid"));
        command
            .args(["cargo-resolve", "--repo"])
            .arg(&source)
            .arg("--output-dir")
            .arg(&output_dir)
            .args(["--check", check])
            .env("CCID_RESOLVE_CARGO", "1")
            .env("CI_REPOSITORY_URL", "https://fixture.invalid/owner/lock")
            .env("CI_COMMIT_SHA", &commit)
            .env("SOURCE_ARCHIVE", &archive)
            .env("SOURCE_SHA256", ccid::sha256_file(&archive).unwrap())
            .env("CARGO_HOME", root.join("cargo-home"))
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env("TMPDIR", root)
            .env("CI_JOBS", "1")
            .env("CI_TEST_THREADS", "1")
            .env("CI_NIX_JOBS", "1")
            .env("CI_TIMEOUT", "30")
            .env_remove("CI_MEMORY_MB")
            .env_remove("CI_MEMORY_PER_JOB_MB")
            .env_remove("CI_MIN_AVAILABLE_MB")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("CARGO_BUILD_TARGET_DIR");
        if generate {
            command.arg("--generate-lockfile");
        }
        let output = command.output().unwrap();
        assert_eq!(
            fs::read_to_string(source.join("Cargo.lock"))
                .ok()
                .as_deref(),
            original
        );
        let receipts: Vec<_> = fs::read_dir(&output_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        let receipt = if let Some(path) = receipts.first() {
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            let snapshot = value["lock_snapshot"].as_str().unwrap();
            assert_eq!(
                ccid::sha256_file(Path::new(snapshot)).unwrap(),
                value["candidate_lock_sha256"]
            );
            assert_eq!(value["source_commit"], commit);
            if let Some(original) = original {
                let retained = value["original_lock_snapshot"].as_str().unwrap();
                assert_eq!(fs::read_to_string(retained).unwrap(), original);
                assert_eq!(
                    ccid::sha256_file(Path::new(retained)).unwrap(),
                    value["original_lock_sha256"]
                );
            } else {
                assert!(value["original_lock_sha256"].is_null());
                assert!(value["original_lock_snapshot"].is_null());
            }
            value
        } else {
            assert!(
                !output.status.success(),
                "successful resolver produced no receipt"
            );
            serde_json::Value::Null
        };
        if generate && check == "valid" {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        (output.status.success(), receipt)
    }

    #[test]
    fn initial_and_stale_locks_require_explicit_generation() {
        let stale = "version=4\n[[package]]\nname='lock-fixture'\nversion='0.1.0'\n";
        for original in [None, Some(stale)] {
            assert!(!resolve(original, false, "valid").0);
            let (success, receipt) = resolve(original, true, "valid");
            assert!(success);
            assert_eq!(receipt["mode"], "generate");
            assert_eq!(
                receipt["compatibility_status"],
                "not-compared-explicit-generation"
            );
            assert_eq!(receipt["validation_status"], "passed");
            assert_eq!(receipt["accepted"], true);
        }
    }

    #[test]
    fn coherent_refresh_keeps_baseline_validation() {
        let lock = "version=4\n[[package]]\nname='lock-fixture'\nversion='0.2.0'\n";
        let (success, receipt) = resolve(Some(lock), false, "valid");
        assert!(success);
        assert_eq!(receipt["mode"], "refresh");
        assert_eq!(receipt["compatibility_status"], "checked-against-baseline");
        assert_eq!(receipt["validation_status"], "passed");
    }

    #[test]
    fn generation_still_rejects_candidate_mutation() {
        let (success, receipt) = resolve(None, true, "mutate");
        assert!(!success);
        assert_eq!(receipt["accepted"], false);
        assert_eq!(receipt["validation_status"], "failed");
        assert!(receipt["validation_error"]
            .as_str()
            .unwrap()
            .contains("candidate Cargo.lock changed"));
    }
}

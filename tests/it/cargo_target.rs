//! Real Cargo verifies routing without injecting storage paths into compiler env.
#![forbid(unsafe_code)]
use std::{fs, process::Command};

#[test]
fn typed_cargo_routes_owned_targets_without_overwriting_declared_environment() {
    let temp = tempfile::tempdir().unwrap();
    for explicit in [false, true] {
        let root = temp
            .path()
            .join(if explicit { "explicit" } else { "automatic" });
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='target-routing-fixture'\nversion='0.0.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.lock"),
            "version=4\n[[package]]\nname='target-routing-fixture'\nversion='0.0.0'\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn value() -> u32 { 7 }\n").unwrap();
        fs::write(
            root.join("build.rs"),
            r#"fn main() {
            std::fs::write(std::env::var_os("CCID_FIXTURE_TARGET_REPORT").unwrap(),
                std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "absent".into())).unwrap();
        }"#,
        )
        .unwrap();
        fs::write(root.join("ccid.toml"), "schema=1\nproject='target-routing-fixture'\n[checks.build]\nkind='cargo'\nactions=['build','clippy']\n").unwrap();
        let report = root.join("observed-target");
        let target = root.join("explicit-target");
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccid"));
        command
            .args(["check", "--repo"])
            .arg(&root)
            .args(["--manifest", "ccid.toml", "--check", "build"])
            .env(
                "CI_REPOSITORY_URL",
                "https://example.invalid/fixtures/target-routing",
            )
            .env("CI_CACHE_ROOT", root.join("cache"))
            .env("CCID_FIXTURE_TARGET_REPORT", &report)
            .env("CI_JOBS", "2")
            .env("CI_TIMEOUT", "90")
            .env_remove("CCID_RESULT_CACHE")
            .env_remove("CCID_CACHE_CHILD")
            .env_remove("CCID_TARGET_LOCK_HELD")
            .env_remove("CCID_RECEIPT")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .env_remove("CARGO_BUILD_BUILD_DIR")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER");
        if explicit {
            command.env("CARGO_TARGET_DIR", &target);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(&report).unwrap(),
            if explicit {
                target.to_string_lossy().into_owned()
            } else {
                "absent".into()
            }
        );
        let plan: serde_json::Value = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|v| v["event"] == "plan")
            .unwrap();
        assert!(
            std::path::Path::new(plan["target_directory"].as_str().unwrap())
                .join("debug/libtarget_routing_fixture.rlib")
                .is_file()
        );
    }
}

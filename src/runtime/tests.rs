use super::*;

fn config(tools: &[&str]) -> Config {
    Config {
        tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
        ..Config::default()
    }
}

#[test]
fn the_namespace_is_the_scripts_sha256sum_prefix() {
    let url = "https://forge.example/owner/repo";
    let script = std::process::Command::new("sh")
        .args([
            "-c",
            "printf '%s' \"$1\" | sha256sum | cut -c1-24",
            "sh",
            url,
        ])
        .output()
        .unwrap();
    assert!(script.status.success());
    let expected = String::from_utf8(script.stdout).unwrap();
    assert_eq!(namespace(url), expected.trim_end());
    assert_eq!(namespace(url).len(), 24);
}

#[test]
fn the_tool_pin_is_the_check_shells_pin() {
    let shell = include_str!("../render/check-shell.nix");
    assert!(
        shell.contains(&format!(
            "https://github.com/NixOS/nixpkgs/archive/{NIXPKGS_REVISION}.tar.gz"
        )),
        "the runtime's nixpkgs revision must be the check-shell's"
    );
    assert!(
        shell.contains(&format!("sha256 = \"{NIXPKGS_NAR_HASH}\"")),
        "the runtime's NAR hash must be the check-shell's"
    );
}

#[test]
fn tools_are_attribute_paths_listed_once_without_the_base_tools() {
    for good in [
        &["python3", "nodejs_24"][..],
        &["llvmPackages.clang-unwrapped", "openssl.dev"],
        &["gtk+3"],
        &[],
    ] {
        validate(Some(&config(good)), None).unwrap();
    }
    for bad in [
        &[""][..],
        &["a..b"],
        &[".a"],
        &["a b"],
        &["pkgs#moon"],
        &["$(id)"],
        &["moon"],
        &["proto"],
        &["jq", "jq"],
    ] {
        assert!(validate(Some(&config(bad)), None).is_err(), "{bad:?}");
    }
}

#[test]
fn state_env_names_a_free_variable_and_one_plain_directory() {
    let with = |name: &str, directory: &str| Config {
        state_env: [(name.to_owned(), directory.to_owned())].into(),
        ..Config::default()
    };
    validate(Some(&with("YARN_CACHE_FOLDER", "yarn-cache")), None).unwrap();
    validate(Some(&with("BUN_INSTALL_CACHE_DIR", "bun.cache_1")), None).unwrap();
    for (name, directory) in [
        ("yarn", "yarn-cache"),
        ("1YARN", "yarn-cache"),
        ("PATH", "bin"),
        ("CARGO_INSTALL_ROOT", "x"),
        ("ARTIFACT_ROOT", "x"),
        ("CI_CACHE_ROOT", "x"),
        ("CCID_RESULT_CACHE", "x"),
        ("GIT_CONFIG_COUNT", "x"),
        ("RUNNER_TEMP", "x"),
        ("PROTO_HOME", "x"),
        ("RUSTUP_HOME", "x"),
        ("YARN_CACHE_FOLDER", ""),
        ("YARN_CACHE_FOLDER", "."),
        ("YARN_CACHE_FOLDER", ".."),
        ("YARN_CACHE_FOLDER", "a/b"),
        ("YARN_CACHE_FOLDER", "lock"),
        ("YARN_CACHE_FOLDER", "moon-home"),
        ("YARN_CACHE_FOLDER", "install-bin"),
        ("YARN_CACHE_FOLDER", "release-artifacts"),
    ] {
        assert!(
            validate(Some(&with(name, directory)), None).is_err(),
            "{name}={directory}"
        );
    }
}

#[test]
fn toolchain_needs_the_runtime_and_well_formed_targets() {
    let toolchain = |targets: &[&str]| ToolchainConfig {
        nightly: true,
        targets: targets.iter().map(|target| (*target).to_owned()).collect(),
    };
    assert!(validate(None, Some(&toolchain(&[]))).is_err());
    validate(None, None).unwrap();
    let runtime = Config::default();
    validate(
        Some(&runtime),
        Some(&toolchain(&[
            "stable:wasm32-unknown-unknown",
            "1.92.0:wasm32-wasip1",
        ])),
    )
    .unwrap();
    for bad in ["wasm32-unknown-unknown", ":wasm32", "stable:", "stable:a b"] {
        assert!(
            validate(Some(&runtime), Some(&toolchain(&[bad]))).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn the_tables_deny_unknown_keys() {
    assert!(toml::from_str::<Config>("chrome_shim = true").is_err());
    assert!(toml::from_str::<ToolchainConfig>("wasm = true").is_err());
    let parsed: Config = toml::from_str(
        "tools = ['python3']\ninstall_root = true\nstate_env = { YARN_CACHE_FOLDER = 'yarn-cache' }",
    )
    .unwrap();
    assert!(parsed.install_root);
    assert_eq!(parsed.tools, ["python3"]);
}

#[cfg(unix)]
mod unix {
    use super::super::setup::{git_config, layout, lock_state, timeout};
    use crate::Environment;
    use std::{path::Path, time::Duration};

    fn environment(pairs: &[(&str, &str)]) -> Environment {
        pairs
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect()
    }

    #[test]
    fn the_state_root_is_ci_cache_root_then_cargo_home_and_absolute() {
        let url = "https://forge.example/o/r";
        let namespace = super::namespace(url);
        let (state, toolchains) = layout(
            &environment(&[("CI_CACHE_ROOT", "/cache"), ("CARGO_HOME", "/cargo")]),
            url,
        )
        .unwrap();
        assert_eq!(
            state,
            Path::new("/cache/ccid-cached").join(&namespace),
            "CI_CACHE_ROOT wins"
        );
        assert_eq!(toolchains, Path::new("/cache/ccid-cached/toolchains"));
        let (state, _) = layout(&environment(&[("CARGO_HOME", "/cargo")]), url).unwrap();
        assert_eq!(state, Path::new("/cargo/ccid-cached").join(&namespace));
        let (_, toolchains) = layout(
            &environment(&[("CARGO_HOME", "/cargo"), ("CI_TOOLCHAIN_ROOT", "/tc")]),
            url,
        )
        .unwrap();
        assert_eq!(toolchains, Path::new("/tc"));
        for bad in [
            &[][..],
            &[("CI_CACHE_ROOT", "relative")],
            &[("CI_CACHE_ROOT", ""), ("CARGO_HOME", "relative")],
            &[("CARGO_HOME", "/cargo"), ("CI_TOOLCHAIN_ROOT", "tc")],
        ] {
            assert!(layout(&environment(bad), url).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn git_config_pairs_follow_the_existing_entries() {
        let pairs = vec![(
            "url.https://forge.example/new/.insteadOf".to_owned(),
            "https://mirror.example/old/".to_owned(),
        )];
        let appended = git_config(&environment(&[("GIT_CONFIG_COUNT", "1")]), &pairs).unwrap();
        assert_eq!(appended["GIT_CONFIG_COUNT"], "2");
        assert_eq!(
            appended["GIT_CONFIG_KEY_1"],
            "url.https://forge.example/new/.insteadOf"
        );
        assert_eq!(
            appended["GIT_CONFIG_VALUE_1"],
            "https://mirror.example/old/"
        );
        assert!(!appended.contains_key("GIT_CONFIG_KEY_0"));
        let fresh = git_config(&environment(&[]), &pairs).unwrap();
        assert_eq!(fresh["GIT_CONFIG_COUNT"], "1");
        assert!(fresh.contains_key("GIT_CONFIG_KEY_0"));
        assert!(git_config(&environment(&[("GIT_CONFIG_COUNT", "x")]), &pairs).is_err());
        let none = git_config(&environment(&[("GIT_CONFIG_COUNT", "3")]), &[]).unwrap();
        assert_eq!(none.len(), 1);
        assert_eq!(none["GIT_CONFIG_COUNT"], "3");
    }

    #[test]
    fn the_state_lock_is_exclusive_and_times_out() {
        let state = tempfile::tempdir().unwrap();
        let held = lock_state(state.path(), Duration::from_secs(1))
            .unwrap()
            .expect("free lock");
        assert!(lock_state(state.path(), Duration::from_millis(200))
            .unwrap()
            .is_none());
        drop(held);
        assert!(lock_state(state.path(), Duration::from_millis(200))
            .unwrap()
            .is_some());
    }

    #[test]
    fn ci_timeout_is_whole_seconds_with_the_scripts_default() {
        assert_eq!(
            timeout(&environment(&[])).unwrap(),
            Duration::from_secs(2700)
        );
        assert_eq!(
            timeout(&environment(&[("CI_TIMEOUT", "7")])).unwrap(),
            Duration::from_secs(7)
        );
        assert!(timeout(&environment(&[("CI_TIMEOUT", "1h")])).is_err());
    }
}

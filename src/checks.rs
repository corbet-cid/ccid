//! Check-specific command builders and validation.
use crate::{event, failure, strings, validate_command, value, Check, Environment, Result, Runner};
use serde_json::json;
use std::{ffi::OsString, fs, path::Path};

pub(crate) fn cargo_prefix(check: &Check, runner: &Runner) -> Result<(Vec<String>, Vec<String>)> {
    let toolchain = check.toolchain.as_deref().unwrap_or("system");
    let mut cargo = if toolchain == "system" {
        strings(&["cargo"])
    } else {
        strings(&["rustup", "run", toolchain, "cargo"])
    };
    let rustc = if toolchain == "system" {
        strings(&["rustc", "--version"])
    } else {
        strings(&["rustup", "run", toolchain, "rustc", "--version"])
    };
    if value(&runner.environment, "CI_LINKER").as_deref() == Some("mold") {
        let driver = mold_driver(&runner.environment)?;
        runner.run(&[driver.clone(), "--version".into()], false)?;
        cargo.splice(0..0, [driver, "-run".into()]);
    }
    Ok((cargo, rustc))
}
pub(crate) fn mold_driver(environment: &Environment) -> Result<String> {
    let paths = environment
        .get(&OsString::from("PATH"))
        .ok_or_else(|| failure("CI_LINKER=mold requires PATH"))?;
    let executable = std::env::split_paths(paths)
        .map(|path| path.join(if cfg!(windows) { "mold.exe" } else { "mold" }))
        .find(|path| path.is_file())
        .ok_or_else(|| failure("CI_LINKER=mold requires an installed mold executable"))?;
    let resolved = executable.canonicalize()?;
    if let Some(root) = resolved.parent().and_then(Path::parent) {
        let metadata = root.join("nix-support/orig-bintools");
        if metadata.is_file() {
            return Ok(Path::new(fs::read_to_string(metadata)?.trim())
                .join("bin/mold")
                .to_string_lossy()
                .into_owned());
        }
    }
    Ok(executable.to_string_lossy().into_owned())
}
pub fn cargo_commands(
    check: &Check,
    cargo: &[String],
    test_threads: &str,
) -> Result<Vec<Vec<String>>> {
    let mut options = strings(&["--locked"]);
    if check.workspace {
        options.push("--workspace".into());
    }
    if check.all_features {
        options.push("--all-features".into());
    }
    for p in &check.packages {
        options.extend(["--package".into(), p.clone()]);
    }
    for p in &check.exclude {
        options.extend(["--exclude".into(), p.clone()]);
    }
    if !check.features.is_empty() {
        options.extend(["--features".into(), check.features.join(",")]);
    }
    if check.release {
        options.push("--release".into());
    }
    let defaults = strings(&["fmt", "test", "clippy"]);
    let actions = check.actions.as_ref().unwrap_or(&defaults);
    if actions.is_empty() {
        return Err(failure("Cargo action selection is empty"));
    }
    let mut commands = Vec::new();
    for action in actions {
        let mut command = cargo.to_vec();
        match action.as_str() {
            "fmt" => command.extend(strings(&["fmt", "--all", "--", "--check"])),
            "test" => match check.test_runner.as_deref().unwrap_or("cargo") {
                "cargo" => {
                    command.push("test".into());
                    command.extend(options.clone());
                    if check.all_targets {
                        command.push("--all-targets".into());
                    }
                }
                "nextest" => {
                    command.extend(strings(&["nextest", "run", "--test-threads", test_threads]));
                    command.extend(options.clone());
                    commands.push(command);
                    command = cargo.to_vec();
                    command.extend(strings(&["test", "--doc"]));
                    command.extend(options.clone());
                }
                _ => return Err(failure("Unknown Cargo test runner")),
            },
            "clippy" => {
                command.extend(strings(&["clippy", "--all-targets"]));
                command.extend(options.clone());
                command.extend(strings(&["--", "-D", "warnings"]));
            }
            "check" | "build" => {
                command.push(action.clone());
                command.extend(options.clone());
                if check.all_targets {
                    command.push("--all-targets".into());
                }
            }
            _ => return Err(failure(format!("Unknown Cargo action: {action}"))),
        }
        commands.push(command);
    }
    Ok(commands)
}
pub(crate) fn nix_check(check: &Check, runner: &mut Runner) -> Result<()> {
    if cfg!(windows) {
        return Err(failure(
            "Nix checks require a supported Nix host; Windows is unsupported",
        ));
    }
    let mode = nix_mode(check)?;
    if runner.nix_inventory.is_none() {
        let system = runner.run(
            &strings(&[
                "nix",
                "eval",
                "--impure",
                "--raw",
                "--expr",
                "builtins.currentSystem",
            ]),
            true,
        )?;
        let inventory = runner.run(
            &strings(&[
                "nix",
                "eval",
                "--no-update-lock-file",
                "--json",
                &format!(".#checks.{system}"),
                "--apply",
                "builtins.attrNames",
            ]),
            true,
        )?;
        runner.nix_inventory = Some((system, serde_json::from_str(&inventory)?));
    }
    let (system, inventory) = runner
        .nix_inventory
        .as_ref()
        .ok_or_else(|| failure("Missing Nix inventory"))?;
    if inventory.is_empty() {
        return Err(failure("No native checks declared; refusing empty success"));
    }
    if let Some(expected) = &check.expected_checks {
        let mut expected = expected.clone();
        let mut actual = inventory.clone();
        expected.sort();
        actual.sort();
        if actual != expected {
            return Err(failure("Native inventory differs from expected_checks"));
        }
    }
    event(json!({"event":"nix-inventory", "system":system, "checks":inventory, "mode":mode}));
    let mut command = strings(&[
        "nix",
        "flake",
        "check",
        "--no-update-lock-file",
        "--keep-going",
        "--print-build-logs",
    ]);
    match mode {
        "list" => return Ok(()),
        "native" => {}
        "eval" => command.extend(strings(&["--all-systems", "--no-build"])),
        "all-systems" => command.push("--all-systems".into()),
        "named" => {
            if check.checks.is_empty() || check.checks.iter().any(|c| !inventory.contains(c)) {
                return Err(failure(
                    "Named Nix checks must be a nonempty subset of native checks",
                ));
            }
            command = strings(&[
                "nix",
                "build",
                "--no-update-lock-file",
                "--no-link",
                "--keep-going",
                "--print-build-logs",
            ]);
            for name in &check.checks {
                command.push(format!(
                    ".#checks.{system}.{}",
                    serde_json::to_string(name)?
                ));
            }
        }
        _ => return Err(failure("Unknown Nix mode")),
    }
    runner.run(&command, false)?;
    Ok(())
}
pub(crate) fn javascript_executable(manager: &str, windows: bool) -> &str {
    match (manager, windows) {
        ("npm", true) => "npm.cmd",
        ("pnpm", true) => "pnpm.cmd",
        _ => manager,
    }
}
pub(crate) fn javascript_commands(check: &Check) -> Result<Vec<Vec<String>>> {
    let manager = check.manager.as_deref().unwrap_or("npm");
    let executable = javascript_executable(manager, cfg!(windows));
    let install = match manager {
        "npm" => strings(&[
            executable,
            "ci",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
        ]),
        "bun" => strings(&[executable, "install", "--frozen-lockfile"]),
        "pnpm" => strings(&[executable, "install", "--frozen-lockfile"]),
        "deno" => strings(&[executable, "install", "--frozen-lockfile"]),
        _ => {
            return Err(failure(
                "JavaScript manager must be npm, bun, pnpm, or deno",
            ))
        }
    };
    let defaults = strings(&["test"]);
    let scripts = check.scripts.as_ref().unwrap_or(&defaults);
    if scripts.is_empty() {
        return Err(failure("JavaScript script selection is empty"));
    }
    let mut commands = Vec::new();
    if check.install.unwrap_or(true) {
        commands.push(install);
    }
    for script in scripts {
        if script.is_empty() {
            return Err(failure("JavaScript script names must not be empty"));
        }
        let action = if manager == "deno" { "task" } else { "run" };
        commands.push(strings(&[executable, action, script]));
    }
    Ok(commands)
}

fn nix_mode(check: &Check) -> Result<&str> {
    let mode = check.mode.as_deref().unwrap_or("native");
    match mode {
        "list" | "native" | "eval" | "all-systems" => Ok(mode),
        "named" if !check.checks.is_empty() && check.checks.iter().all(|name| !name.is_empty()) => {
            Ok(mode)
        }
        "named" => Err(failure("Named Nix checks require nonempty check names")),
        _ => Err(failure("Unknown Nix mode")),
    }
}

pub(crate) fn validate_check(check: &Check) -> Result<()> {
    let commands = match check.kind.as_str() {
        "cargo" => {
            if check.toolchain.as_deref() == Some("") {
                return Err(failure("Cargo toolchain must not be empty"));
            }
            if !matches!(
                check.test_runner.as_deref(),
                None | Some("cargo" | "nextest")
            ) {
                return Err(failure("Unknown Cargo test runner"));
            }
            cargo_commands(check, &strings(&["cargo"]), "1")?
        }
        "javascript" => javascript_commands(check)?,
        "nix" => {
            nix_mode(check)?;
            return Ok(());
        }
        "commands" if !check.commands.is_empty() => check.commands.clone(),
        "commands" => return Err(failure("Custom command selection is empty")),
        _ => return Err(failure(format!("Unknown check kind: {}", check.kind))),
    };
    for command in commands {
        validate_command(&command)?;
    }
    Ok(())
}

/// Env names that name run metadata rather than content. They must never appear
/// in a cached check's key contract: reusing a result across runs while keying
/// on the producing run's identity is unsound, and silently stripping them would
/// hide a semantic input. Rejected explicitly; such checks stay available through
/// uncached `check`, which never calls this gate.
pub(crate) const FORBIDDEN_CACHED_ENV: &[&str] = &[
    "CI_COMMIT_SHA",
    "CI_COMMIT_BRANCH",
    "CI_JOB_ID",
    "RUNNER_TEMP",
    "TMPDIR",
    "TEMP",
    "TMP",
];

/// Central contract gate for the cached path only (`run_cached`). Uncached
/// `run_checks` does not call this and is unaffected.
pub(crate) fn validate_cached_contract(check: &Check) -> Result<()> {
    if check.cache_commit {
        return Err(failure(
            "cache_commit must not be used for cached checks: commit-sensitive results are never shared",
        ));
    }
    for name in &check.cache_env {
        if FORBIDDEN_CACHED_ENV.contains(&name.as_str()) {
            return Err(failure(format!(
                "Cached checks must not bind run metadata in cache_env: {name}"
            )));
        }
    }
    Ok(())
}

/// Deterministic checks are cacheable by default; `cache = false` and
/// `cache_pure = false` are the explicit opt-outs. Recognised use of the network
/// or the clock, and every condition that prevents keying a check, are decided
/// by the cache runtime and degrade to an uncached run with a receipt.
pub(crate) fn cache_eligible(check: &Check) -> bool {
    if check.cache == Some(false)
        || check.cache_pure == Some(false)
        || validate_cached_contract(check).is_err()
    {
        return false;
    }
    match check.kind.as_str() {
        "cargo" | "nix" | "commands" => true,
        "javascript" => check.install != Some(false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_check() -> Check {
        Check {
            kind: "commands".into(),
            commands: vec![vec!["true".into()]],
            cache_env: vec!["CI_JOBS".into()],
            ..Check::default()
        }
    }

    #[test]
    fn cached_contract_rejects_commit_sensitive_checks() {
        let mut check = cached_check();
        check.cache_commit = true;
        assert!(validate_cached_contract(&check).is_err());
        // Uncached validation is unaffected: the same check still plans.
        assert!(validate_check(&check).is_ok());
    }

    #[test]
    fn cached_contract_rejects_run_metadata_env() {
        for name in FORBIDDEN_CACHED_ENV {
            let mut check = cached_check();
            check.cache_env = vec![(*name).to_owned()];
            let before = check.cache_env.clone();
            assert!(
                validate_cached_contract(&check).is_err(),
                "{name} must be rejected, not stripped"
            );
            // Rejection never mutates: nothing is silently stripped.
            assert_eq!(check.cache_env, before);
        }
    }

    #[test]
    fn cached_contract_keeps_content_env_and_uncached_usable() {
        let check = cached_check();
        assert!(validate_cached_contract(&check).is_ok());
        assert!(validate_check(&check).is_ok());
        // A commit-sensitive check remains runnable outside the cached path.
        let mut uncached = cached_check();
        uncached.cache_commit = true;
        uncached.cache_env = vec!["CI_COMMIT_SHA".into()];
        assert!(validate_check(&uncached).is_ok());
    }

    #[test]
    fn deterministic_kinds_are_cacheable_without_any_declaration() {
        for kind in ["cargo", "javascript", "nix", "commands"] {
            let check = Check {
                kind: kind.into(),
                ..Check::default()
            };
            assert!(cache_eligible(&check), "{kind}");
        }
        let unknown = Check {
            kind: "other".into(),
            ..Check::default()
        };
        assert!(!cache_eligible(&unknown));
    }

    #[test]
    fn explicit_opt_outs_and_unlockable_installs_are_never_cacheable() {
        for kind in ["cargo", "javascript", "nix", "commands"] {
            for (cache, pure) in [
                (Some(false), None),
                (None, Some(false)),
                (Some(false), Some(true)),
            ] {
                let check = Check {
                    kind: kind.into(),
                    cache,
                    cache_pure: pure,
                    ..Check::default()
                };
                assert!(!cache_eligible(&check), "{kind} {cache:?} {pure:?}");
            }
            let declared = Check {
                kind: kind.into(),
                cache_pure: Some(true),
                ..Check::default()
            };
            assert!(cache_eligible(&declared));
        }
        let ambient = Check {
            kind: "javascript".into(),
            install: Some(false),
            ..Check::default()
        };
        assert!(!cache_eligible(&ambient));
        let commit = Check {
            kind: "commands".into(),
            cache_commit: true,
            ..Check::default()
        };
        assert!(!cache_eligible(&commit));
    }

    #[test]
    fn the_purity_declaration_is_not_part_of_the_serialized_check() {
        let check = Check {
            kind: "commands".into(),
            cache_pure: Some(true),
            ..Check::default()
        };
        let encoded = serde_json::to_value(&check).unwrap();
        assert!(encoded.get("cache_pure").is_none());
        let parsed: Check = toml::from_str("kind = 'commands'\ncache_pure = false").unwrap();
        assert_eq!(parsed.cache_pure, Some(false));
    }
}

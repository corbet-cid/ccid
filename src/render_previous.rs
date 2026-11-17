//! A channel bump must not turn every repository red.
//!
//! `job-run` refuses a repository whose generated adapters differ from what the running ccid renders. When a release changes the
//! rendered bytes and the channel moves to it, every repository not yet re-rendered would fail at once (2026-11-17: infra's render
//! job within seconds of the a37dbc7 bump, rolled back). The channel therefore also names the PREVIOUS release:
//!
//! ```json
//! "previous": { "default": {"revision": "...", "sha256": "..."}, "classes": { "linux": {...} } }
//! ```
//!
//! and the adapters pass when they equal the render of the current release OR of the previous one. The previous one is asked, not
//! reimplemented: its own binary (cached and sha256-verified by the shim at `$CCID_TOOL_ROOT/ccid/<rev>/<triple>/ccid`, or in the
//! runner's `ccid-channel/tools` cache) runs `render --check` on the repository. A missing binary, a wrong digest, no `previous`
//! entry, or an adapter set that matches neither refuses exactly as before. `previous` is dropped with the next bump.
use crate::{sha256_file, Environment, Result};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const DRIFT: &str = "Generated job adapters have drifted";

/// `render --check`, accepting the previous channel release's output as well.
pub fn check(
    repo: &Path,
    manifest: &Path,
    environment: &Environment,
) -> Result<crate::render::Report> {
    match crate::render::render(repo, manifest, true) {
        Ok(report) => Ok(report),
        Err(error) if error.to_string().starts_with(DRIFT) => {
            match previous_binary(environment, crate::SOURCE_REVISION) {
                Some(binary) if run_check(&binary, repo, manifest) => {
                    eprintln!(
                        "ccid: the adapters match the previous channel release ({}), not this one ({}): accepted until the repository re-renders",
                        binary.display(),
                        crate::SOURCE_REVISION
                    );
                    Ok(crate::render::Report {
                        checked: true,
                        files: Vec::new(),
                        removed: Vec::new(),
                        transitional: Vec::new(),
                    })
                }
                _ => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn run_check(binary: &Path, repo: &Path, manifest: &Path) -> bool {
    Command::new(binary)
        .args(["render", "--repo"])
        .arg(repo)
        .arg("--manifest")
        .arg(manifest)
        .arg("--check")
        .env_clear()
        .envs(
            ["PATH", "HOME", "CCID_FLEET_FILE", "CCID_PLACEMENT_FILE"]
                .iter()
                .filter_map(|k| std::env::var_os(k).map(|v| (*k, v))),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn channel_document(environment: &Environment) -> Option<Value> {
    let path = match crate::value(environment, "CCID_CHANNEL_FILE") {
        Some(path) => PathBuf::from(path),
        None => crate::fleet::shim_channel_copy(environment),
    };
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn is_hex(text: &str, length: usize) -> bool {
    text.len() == length && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The verified binary of the channel's previous release for the class `own` belongs to.
pub(crate) fn previous_binary(environment: &Environment, own: &str) -> Option<PathBuf> {
    let channel = channel_document(environment)?;
    let previous = channel.get("previous")?;
    // the class whose CURRENT entry is this binary; the previous release of that class (else the previous default)
    let class = channel
        .get("classes")
        .and_then(Value::as_object)
        .and_then(|classes| {
            classes
                .iter()
                .find(|(_, entry)| entry["revision"] == own)
                .map(|(name, _)| name.clone())
        });
    let entry = class
        .as_deref()
        .and_then(|name| previous.get("classes")?.get(name))
        .or_else(|| previous.get("default"))?;
    let revision = entry.get("revision")?.as_str()?;
    let sha256 = entry.get("sha256")?.as_str()?;
    if !is_hex(revision, 40) || !is_hex(sha256, 64) || revision == own {
        return None;
    }
    let triple = crate::value(environment, "CCID_TOOL_TRIPLE")
        .unwrap_or_else(|| format!("{}-unknown-linux-gnu", std::env::consts::ARCH));
    let mut candidates = Vec::new();
    if let Some(root) = crate::value(environment, "CCID_TOOL_ROOT") {
        candidates.push(
            PathBuf::from(root)
                .join("ccid")
                .join(revision)
                .join(&triple)
                .join("ccid"),
        );
    }
    candidates.push(
        crate::fleet::shim_channel_copy(environment)
            .parent()?
            .join("tools")
            .join(revision)
            .join(&triple)
            .join("ccid"),
    );
    candidates
        .into_iter()
        .find(|path| path.is_file() && sha256_file(path).is_ok_and(|have| have == sha256))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const OWN: &str = "a37dbc78f57ea1088588d0fcd35e7b333b2c2fe2";
    const OLD: &str = "0a46dc33f98a6820b789586ed7b5f2eff8db9aa2";

    fn world(script: &str, tamper: bool) -> (tempfile::TempDir, Environment) {
        let dir = tempfile::tempdir().unwrap();
        let triple = format!("{}-unknown-linux-gnu", std::env::consts::ARCH);
        let tool = dir.path().join("ccid").join(OLD).join(&triple);
        fs::create_dir_all(&tool).unwrap();
        let binary = tool.join("ccid");
        fs::write(&binary, script).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let sha = sha256_file(&binary).unwrap();
        let sha = if tamper { "0".repeat(64) } else { sha };
        let channel = serde_json::json!({
            "default": {"revision": OWN, "sha256": "a".repeat(64)},
            "classes": {"linux": {"revision": OWN, "sha256": "a".repeat(64)}},
            "previous": {"default": {"revision": OLD, "sha256": sha}, "classes": {"linux": {"revision": OLD, "sha256": sha}}}
        });
        let file = dir.path().join("channel.json");
        fs::write(&file, channel.to_string()).unwrap();
        let environment: Environment = [
            ("CCID_CHANNEL_FILE".into(), file.into()),
            ("CCID_TOOL_ROOT".into(), dir.path().into()),
        ]
        .into_iter()
        .collect();
        (dir, environment)
    }

    #[test]
    fn the_previous_release_is_found_when_its_digest_matches() {
        let (_dir, environment) = world("#!/bin/sh\nexit 0\n", false);
        assert!(previous_binary(&environment, OWN).is_some());
    }

    #[test]
    fn a_wrong_digest_or_a_missing_binary_or_no_previous_entry_finds_nothing() {
        let (_dir, environment) = world("#!/bin/sh\nexit 0\n", true);
        assert!(previous_binary(&environment, OWN).is_none(), "wrong digest");
        let (dir, environment) = world("#!/bin/sh\nexit 0\n", false);
        fs::remove_file(
            dir.path()
                .join("ccid")
                .join(OLD)
                .join(format!("{}-unknown-linux-gnu", std::env::consts::ARCH))
                .join("ccid"),
        )
        .unwrap();
        assert!(
            previous_binary(&environment, OWN).is_none(),
            "missing binary"
        );
        let (dir, environment) = world("#!/bin/sh\nexit 0\n", false);
        let file = dir.path().join("channel.json");
        let mut channel: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        channel.as_object_mut().unwrap().remove("previous");
        fs::write(&file, channel.to_string()).unwrap();
        assert!(
            previous_binary(&environment, OWN).is_none(),
            "no previous entry"
        );
        // the previous entry naming this very binary is no second opinion
        let (_dir, environment) = world("#!/bin/sh\nexit 0\n", false);
        assert!(previous_binary(&environment, OLD).is_none());
    }

    #[test]
    fn the_previous_binary_decides_by_its_exit_status() {
        let (dir, environment) = world("#!/bin/sh\nexit 0\n", false);
        let binary = previous_binary(&environment, OWN).unwrap();
        assert!(run_check(&binary, dir.path(), Path::new(".ci/ccid.toml")));
        let (dir, environment) = world("#!/bin/sh\nexit 2\n", false);
        let binary = previous_binary(&environment, OWN).unwrap();
        assert!(!run_check(&binary, dir.path(), Path::new(".ci/ccid.toml")));
    }
}

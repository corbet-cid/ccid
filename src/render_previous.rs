//! A channel bump must not turn every repository red.
//!
//! `job-run` refuses a repository whose generated adapters differ from what the running ccid renders. When a release changes the
//! rendered bytes and the channel moves to it, every repository not yet re-rendered would fail at once (2026-11-17: infra's render
//! job within seconds of the a37dbc7 bump, rolled back). The channel therefore also names the PREVIOUS releases, newest first, at most
//! three (`MAX_PREVIOUS`); the single-object form of the first version is read as a list of one:
//!
//! ```json
//! "previous": [ { "default": {"revision": "...", "sha256": "..."}, "classes": { "linux": {...} } }, ... ]
//! ```
//!
//! and the adapters pass when they equal the render of the current release OR of ANY listed one (an entry leaves the list when the fleet
//! has re-rendered past it). The previous one is asked, not
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
/// Older releases whose output is still accepted; further entries of the channel list are ignored.
const MAX_PREVIOUS: usize = 3;

/// `render --check`, accepting the previous channel release's output as well.
pub fn check(
    repo: &Path,
    manifest: &Path,
    environment: &Environment,
) -> Result<crate::render::Report> {
    match crate::render::render(repo, manifest, true) {
        Ok(report) => Ok(report),
        Err(error) if error.to_string().starts_with(DRIFT) => {
            match previous_binaries(environment, crate::SOURCE_REVISION)
                .into_iter()
                .find(|binary| run_check(binary, repo, manifest))
            {
                Some(binary) => {
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
                None => Err(error),
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

/// The verified binaries of the channel's previous releases (newest first, at most `MAX_PREVIOUS`) for the class `own` belongs to.
pub(crate) fn previous_binaries(environment: &Environment, own: &str) -> Vec<PathBuf> {
    let Some(channel) = channel_document(environment) else {
        return Vec::new();
    };
    // the class whose CURRENT entry is this binary
    let class = channel
        .get("classes")
        .and_then(Value::as_object)
        .and_then(|classes| {
            classes
                .iter()
                .find(|(_, entry)| entry["revision"] == own)
                .map(|(name, _)| name.clone())
        });
    let entries: Vec<&Value> = match channel.get("previous") {
        Some(Value::Array(list)) => list.iter().collect(),
        Some(single @ Value::Object(_)) => vec![single],
        _ => Vec::new(),
    };
    entries
        .into_iter()
        .take(MAX_PREVIOUS)
        .filter_map(|previous| {
            let entry = class
                .as_deref()
                .and_then(|name| previous.get("classes")?.get(name))
                .or_else(|| previous.get("default"))?;
            verified_binary(
                environment,
                own,
                entry.get("revision")?.as_str()?,
                entry.get("sha256")?.as_str()?,
            )
        })
        .collect()
}

fn verified_binary(
    environment: &Environment,
    own: &str,
    revision: &str,
    sha256: &str,
) -> Option<PathBuf> {
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
        assert!(previous_binaries(&environment, OWN).len() == 1);
    }

    #[test]
    fn a_wrong_digest_or_a_missing_binary_or_no_previous_entry_finds_nothing() {
        let (_dir, environment) = world("#!/bin/sh\nexit 0\n", true);
        assert!(
            previous_binaries(&environment, OWN).is_empty(),
            "wrong digest"
        );
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
            previous_binaries(&environment, OWN).is_empty(),
            "missing binary"
        );
        let (dir, environment) = world("#!/bin/sh\nexit 0\n", false);
        let file = dir.path().join("channel.json");
        let mut channel: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        channel.as_object_mut().unwrap().remove("previous");
        fs::write(&file, channel.to_string()).unwrap();
        assert!(
            previous_binaries(&environment, OWN).is_empty(),
            "no previous entry"
        );
        // the previous entry naming this very binary is no second opinion
        let (_dir, environment) = world("#!/bin/sh\nexit 0\n", false);
        assert!(previous_binaries(&environment, OLD).is_empty());
    }

    #[test]
    fn the_previous_binary_decides_by_its_exit_status() {
        let (dir, environment) = world("#!/bin/sh\nexit 0\n", false);
        let binary = previous_binaries(&environment, OWN).remove(0);
        assert!(run_check(&binary, dir.path(), Path::new(".ci/ccid.toml")));
        let (dir, environment) = world("#!/bin/sh\nexit 2\n", false);
        let binary = previous_binaries(&environment, OWN).remove(0);
        assert!(!run_check(&binary, dir.path(), Path::new(".ci/ccid.toml")));
    }

    #[test]
    fn the_list_is_read_newest_first_capped_at_three_and_a_single_object_is_a_list_of_one() {
        let (dir, environment) = world("#!/bin/sh\nexit 0\n", false);
        let file = dir.path().join("channel.json");
        let mut channel: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        let single = channel["previous"].clone();
        assert_eq!(
            previous_binaries(&environment, OWN).len(),
            1,
            "the single-object form"
        );
        // four copies of the same (valid) entry: only MAX_PREVIOUS are considered
        channel["previous"] =
            Value::Array(vec![single.clone(), single.clone(), single.clone(), single]);
        fs::write(&file, channel.to_string()).unwrap();
        assert_eq!(previous_binaries(&environment, OWN).len(), MAX_PREVIOUS);
    }
}

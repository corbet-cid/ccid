//! Locked content pins: fixture digests and native artifact versions.
//!
//! Replaces the copied `scripts/check-fixtures.py` (fixture SHA-256 manifest)
//! and `.ci/check-native-sdk.py` (locked `barretenberg-rs` version equals the
//! authenticated `.ci/native-sdk.json` version plus digest) with one generic
//! subcommand driven only by `.ci/ccid.toml` data.
//!
//! Exit statuses: 0 passes, 1 content refusal, 2 operational error
//! (unreadable or unparseable inputs, malformed pin declarations, unknown pin
//! selection). An empty pin table is refused, never a silent pass.

use crate::{failure, Result};
use clap::Args;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

/// One named content pin, selected with `--pin <name>`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Pin {
    /// Every file listed in the JSON manifest's map hashes to its value.
    /// `manifest` is repo-relative; `key` names the file-to-sha256 map.
    /// Listed paths are relative to the manifest's directory.
    #[serde(rename = "sha256-manifest")]
    Sha256Manifest {
        name: String,
        manifest: String,
        key: String,
    },
    /// The root `Cargo.lock` holds exactly one package of this name and its
    /// version equals the authenticated JSON artifact's version.
    /// `file` is the repo-relative JSON artifact with `version` and `sha256`.
    #[serde(rename = "lock-version")]
    LockVersion {
        name: String,
        file: String,
        package: String,
    },
}

impl Pin {
    pub fn name(&self) -> &str {
        match self {
            Pin::Sha256Manifest { name, .. } => name,
            Pin::LockVersion { name, .. } => name,
        }
    }
}

#[derive(Args, Debug)]
pub struct Options {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    #[arg(long, default_value = ".ci/ccid.toml")]
    pub manifest: PathBuf,
    /// Select pins by name; repeatable. Empty selects every declared pin.
    #[arg(long = "pin")]
    pub pin: Vec<String>,
}

/// The manifest file contributes only the pin table; every other table
/// belongs to other subcommands and is ignored here.
#[derive(Debug, Deserialize)]
struct FileManifest {
    #[serde(default)]
    content_pins: Vec<Pin>,
}

fn is_safe_relative(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return false;
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return false,
        }
    }
    true
}

fn is_stable_version(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

enum Outcome {
    Pass(String),
    Refuse(String),
}

fn verify_sha256_manifest(root: &Path, name: &str, manifest: &str, key: &str) -> Result<Outcome> {
    if !is_safe_relative(manifest) {
        return Err(failure(format!(
            "content pin {name}: manifest path must stay inside the repository"
        )));
    }
    if key.is_empty() {
        return Err(failure(format!(
            "content pin {name}: manifest key must not be empty"
        )));
    }
    let manifest_path = root.join(manifest);
    let bytes = fs::read(&manifest_path).map_err(|error| {
        failure(format!(
            "cannot read manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        failure(format!(
            "cannot parse manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let table = parsed.get(key).ok_or_else(|| {
        failure(format!(
            "manifest {} has no key {key:?}",
            manifest_path.display()
        ))
    })?;
    let entries = table.as_object().ok_or_else(|| {
        failure(format!(
            "manifest {} key {key:?} is not an object",
            manifest_path.display()
        ))
    })?;
    let manifest_dir = Path::new(manifest)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let mut count = 0usize;
    for (path, expected) in entries {
        let expected = expected.as_str().ok_or_else(|| {
            failure(format!(
                "manifest {} key {key:?} entry {path:?} is not a string",
                manifest_path.display()
            ))
        })?;
        if !is_safe_relative(path) {
            return Ok(Outcome::Refuse(format!(
                "content-pins: {name}: {path} does not match {manifest}"
            )));
        }
        let candidate = root.join(manifest_dir).join(path);
        let actual = match sha256_file(&candidate) {
            Ok(digest) => digest,
            Err(_) => {
                return Ok(Outcome::Refuse(format!(
                    "content-pins: {name}: {path} does not match {manifest}"
                )));
            }
        };
        if actual != expected {
            return Ok(Outcome::Refuse(format!(
                "content-pins: {name}: {path} does not match {manifest}"
            )));
        }
        count += 1;
    }
    Ok(Outcome::Pass(format!(
        "content-pins: {name}: {count} files match {manifest}"
    )))
}

fn verify_lock_version(root: &Path, name: &str, file: &str, package: &str) -> Result<Outcome> {
    if !is_safe_relative(file) {
        return Err(failure(format!(
            "content pin {name}: artifact path must stay inside the repository"
        )));
    }
    if package.is_empty() {
        return Err(failure(format!(
            "content pin {name}: package must not be empty"
        )));
    }
    let artifact_path = root.join(file);
    let bytes = fs::read(&artifact_path).map_err(|error| {
        failure(format!(
            "cannot read artifact {}: {error}",
            artifact_path.display()
        ))
    })?;
    let artifact: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        failure(format!(
            "cannot parse artifact {}: {error}",
            artifact_path.display()
        ))
    })?;
    let version = artifact
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !is_stable_version(version) {
        return Ok(Outcome::Refuse(format!(
            "content-pins: {name}: Native artifact needs a stable upstream version"
        )));
    }
    let digest = artifact
        .get("sha256")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !is_sha256_digest(digest) {
        return Ok(Outcome::Refuse(format!(
            "content-pins: {name}: Native artifact needs a SHA-256 digest"
        )));
    }
    let lock_path = root.join("Cargo.lock");
    let lock_bytes = fs::read(&lock_path)
        .map_err(|error| failure(format!("cannot read {}: {error}", lock_path.display())))?;
    let lock_text = std::str::from_utf8(&lock_bytes)
        .map_err(|error| failure(format!("cannot parse {}: {error}", lock_path.display())))?;
    let lock: toml::Value = toml::from_str(lock_text)
        .map_err(|error| failure(format!("cannot parse {}: {error}", lock_path.display())))?;
    let mut versions = Vec::new();
    if let Some(packages) = lock.get("package").and_then(|v| v.as_array()) {
        for entry in packages {
            let entry_name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if entry_name == package {
                versions.push(entry.get("version").and_then(|v| v.as_str()).unwrap_or(""));
            }
        }
    }
    if versions.as_slice() != [version] {
        return Ok(Outcome::Refuse(format!(
            "content-pins: {name}: Locked {package} must exactly match the authenticated native artifact"
        )));
    }
    Ok(Outcome::Pass(format!(
        "content-pins: {name}: {package} {version} matches {file}"
    )))
}

fn verify_pin(root: &Path, pin: &Pin) -> Result<Outcome> {
    match pin {
        Pin::Sha256Manifest {
            name,
            manifest,
            key,
        } => verify_sha256_manifest(root, name, manifest, key),
        Pin::LockVersion {
            name,
            file,
            package,
        } => verify_lock_version(root, name, file, package),
    }
}

/// Check the selected pins. Returns 0 when every selected pin passes and 1
/// when any pin refuses; operational failures are `Err` (exit 2).
pub fn run(options: &Options) -> Result<u8> {
    let root = options.repo.canonicalize().map_err(|error| {
        failure(format!(
            "cannot resolve repository {}: {error}",
            options.repo.display()
        ))
    })?;
    let manifest_path = root.join(&options.manifest);
    let bytes = fs::read(&manifest_path).map_err(|error| {
        failure(format!(
            "cannot read manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| failure(format!("cannot parse manifest: {error}")))?;
    let parsed: FileManifest =
        toml::from_str(text).map_err(|error| failure(format!("cannot parse manifest: {error}")))?;
    if parsed.content_pins.is_empty() {
        return Err(failure("no content pins declared"));
    }
    let mut seen = BTreeSet::new();
    for pin in &parsed.content_pins {
        if !seen.insert(pin.name().to_owned()) {
            return Err(failure(format!("duplicate content pin: {}", pin.name())));
        }
        if pin.name().is_empty() {
            return Err(failure("content pin name must not be empty"));
        }
    }
    let selected: Vec<&Pin> = if options.pin.is_empty() {
        parsed.content_pins.iter().collect()
    } else {
        let mut selected = Vec::new();
        for wanted in &options.pin {
            match parsed.content_pins.iter().find(|p| p.name() == wanted) {
                Some(pin) => selected.push(pin),
                None => return Err(failure(format!("unknown content pin: {wanted}"))),
            }
        }
        selected
    };
    let mut refused = false;
    for pin in selected {
        match verify_pin(&root, pin)? {
            Outcome::Pass(line) => println!("{line}"),
            Outcome::Refuse(line) => {
                eprintln!("{line}");
                refused = true;
            }
        }
    }
    Ok(u8::from(refused))
}

#[cfg(test)]
mod tests;

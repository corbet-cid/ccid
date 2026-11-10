//! Advisory exposure policy: the reviewed-exception map, the forbidden
//! transport crates and the transport version floors from `.ci/ccid.toml`,
//! checked offline against `deny.toml` and `dependency-metadata.json`.
//!
//! `cargo deny --locked check advisories` stays the advisory gate; this
//! command ports the repository policy in `.github/check-advisory-exposure.mjs`
//! (cblc, cpsd, cssr) into shared tooling. It runs no network fetch and no
//! cargo command; the caller generates the metadata file first with
//! `cargo metadata --locked --format-version 1`, exactly as `.ci/verify.sh`
//! does before invoking the script.

use crate::{failure, load_manifest, Result};
use regex::Regex;
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path};

/// The metadata file the policy reads when `[advisories]` names none.
fn default_metadata() -> String {
    "dependency-metadata.json".to_owned()
}

/// One reviewed advisory exception: the advisory id and the exact resolved
/// package version the review covers.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reviewed {
    pub id: String,
    pub package: String,
    pub version: String,
}

/// `[advisories]`: reviewed exceptions, forbidden crates, version floors.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Cargo metadata file the policy reads, relative to the repository root.
    #[serde(default = "default_metadata")]
    pub metadata: String,
    /// Crate names that must not appear among the resolved packages.
    #[serde(default)]
    pub forbid: Vec<String>,
    /// Minimum resolved versions by crate name.
    #[serde(default)]
    pub floors: BTreeMap<String, String>,
    /// Reviewed advisory exceptions.
    #[serde(default)]
    pub reviewed: Vec<Reviewed>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            metadata: default_metadata(),
            forbid: Vec::new(),
            floors: BTreeMap::new(),
            reviewed: Vec::new(),
        }
    }
}

/// A resolved package name and version from the metadata file.
struct Package {
    name: String,
    version: String,
}

/// The leading decimal digits of each of the first three dot-separated parts;
/// a missing or digit-less part is 0, so a pre-release suffix is ignored.
fn parts(version: &str) -> [u64; 3] {
    let mut out = [0u64; 3];
    for (slot, part) in out.iter_mut().zip(version.split('.')) {
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        *slot = digits.parse().unwrap_or(0);
    }
    out
}

/// The script semantics exactly: every `id = "RUSTSEC-..."` match in the
/// policy text must be a reviewed id whose package and version resolve, then
/// every package passes the forbid list and its version floor. The first
/// refusal wins; `Ok` means the tree is clean.
fn check(policy: &str, packages: &[Package], config: &Config) -> std::result::Result<(), String> {
    let pattern = Regex::new("id = \"(RUSTSEC-[0-9-]+)\"").expect("advisory id pattern is valid");
    let mut reviewed = BTreeMap::new();
    for entry in &config.reviewed {
        reviewed.insert(entry.id.as_str(), entry);
    }
    for capture in pattern.captures_iter(policy) {
        let id = &capture[1];
        let Some(entry) = reviewed.get(id) else {
            return Err(format!("Unreviewed advisory exception: {id}"));
        };
        if !packages
            .iter()
            .any(|package| package.name == entry.package && package.version == entry.version)
        {
            return Err(format!("Remove or reassess obsolete exception: {id}"));
        }
    }
    for package in packages {
        if config.forbid.iter().any(|name| name == &package.name) {
            return Err(format!("Legacy TLS dependency returned: {}", package.name));
        }
        if let Some(floor) = config.floors.get(&package.name) {
            if parts(&package.version) < parts(floor) {
                return Err(format!(
                    "Version floor violated: {} {} < {}",
                    package.name, package.version, floor
                ));
            }
        }
    }
    Ok(())
}

/// Read the metadata file's package list; `None` when it is not cargo
/// metadata output.
fn packages(metadata: &serde_json::Value) -> Option<Vec<Package>> {
    metadata
        .get("packages")?
        .as_array()?
        .iter()
        .map(|entry| {
            Some(Package {
                name: entry.get("name")?.as_str()?.to_owned(),
                version: entry.get("version")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

/// Check the advisory exposure policy. Prints the success line and returns 0
/// when the tree is clean, prints the refusal as `check-advisory-exposure: ...`
/// and returns 1 on a policy violation, and fails (exit 2 through the caller)
/// when the manifest, `deny.toml` or the metadata file is missing or malformed.
pub fn run(repo: &Path, manifest: &Path) -> Result<u8> {
    let (parsed, _) = load_manifest(repo, manifest)?;
    let config = parsed.advisories.unwrap_or_default();
    let policy = fs::read(repo.join("deny.toml"))
        .map_err(|error| failure(format!("Cannot read deny.toml: {error}")))?;
    let policy = std::str::from_utf8(&policy)
        .map_err(|error| failure(format!("deny.toml is not valid UTF-8: {error}")))?;
    let raw = fs::read(repo.join(&config.metadata))
        .map_err(|error| failure(format!("Cannot read {}: {error}", config.metadata)))?;
    let metadata: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|error| failure(format!("{} is not valid JSON: {error}", config.metadata)))?;
    let packages = packages(&metadata)
        .ok_or_else(|| failure(format!("{} has no packages array", config.metadata)))?;
    match check(policy, &packages, &config) {
        Ok(()) => {
            println!("Repaired transport and remaining reviewed advisory versions checked");
            Ok(0)
        }
        Err(message) => {
            eprintln!("check-advisory-exposure: {message}");
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests;

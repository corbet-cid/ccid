//! Strict typed manifest for frozen live Tor inputs (`.ci/live-inputs.toml`).
//!
//! Single parser shared by the render-time adapter emitter and the runtime
//! job: one schema, validated once, consumed twice. Serde rejects unknown
//! fields; semantic checks below reject drifted names, digests, paths and
//! workflow scopes. The dispatch helper never reads this file.
use crate::{failure, Result};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path};

/// One staged live input: a file the helper never builds, verified at
/// runtime against its committed digest. Crow variable names follow the
/// existing auxiliary pattern, including the frozen `V01_*` names.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LiveInput {
    #[serde(skip)]
    pub(crate) name: String,
    pub(crate) archive_variable: String,
    pub(crate) digest_variable: String,
    pub(crate) sha256: String,
    pub(crate) default_path: String,
    pub(crate) commit: Option<String>,
    pub(crate) workflows: Option<Vec<String>>,
}

/// One fixed ambient file (retained tools, receipts): absolute path,
/// verified in place by digest, never copied.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LiveFixed {
    pub(crate) path: String,
    pub(crate) sha256: String,
}

/// One frozen driver blob: archive-relative path, verified after
/// extraction from the staged frozen root archive.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LiveDriver {
    pub(crate) file: String,
    pub(crate) sha256: String,
}

/// Parsed `.ci/live-inputs.toml`. All three maps are required when the
/// file is present; unknown sections or fields fail closed.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct Manifest {
    pub(crate) schema: u32,
    pub(crate) live_inputs: BTreeMap<String, LiveInput>,
    pub(crate) live_tools: BTreeMap<String, LiveFixed>,
    pub(crate) live_drivers: BTreeMap<String, LiveDriver>,
    pub(crate) diag: BTreeMap<String, DiagWatch>,
}

/// Pinned instrumented library for the compiler-wrapper diagnostic: the
/// relative file inside the composed candidate tree plus the committed
/// original and deterministic diagnostic digests. The runtime requires
/// this table; render ignores it (no Crow variables involved).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiagWatch {
    pub(crate) file: String,
    pub(crate) sha256: String,
    pub(crate) diagnostic_sha256: String,
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    let mut len = 1;
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
            return false;
        }
        len += 1;
        if len > 48 {
            return false;
        }
    }
    true
}

fn valid_variable(value: &str, suffixes: &[&str]) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_uppercase() => {}
        _ => return false,
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return false;
    }
    if value.starts_with("CI_") || value.starts_with("CROW_") {
        return false;
    }
    suffixes.iter().any(|suffix| value.ends_with(suffix))
}

fn valid_workflow_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_absolute_path(value: &str) -> bool {
    value.starts_with('/')
        && !value.contains('\0')
        && !value.split('/').any(|part| part == "." || part == "..")
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('\0')
        && !Path::new(value).is_absolute()
        && !value.split('/').any(|part| part == "." || part == "..")
}

impl Manifest {
    fn validate(&self) -> Result<()> {
        if self.schema != 1 {
            return Err(failure("Invalid .ci/live-inputs.toml schema version"));
        }
        for (name, input) in &self.live_inputs {
            if !valid_name(name) {
                return Err(failure(format!("Invalid live input name: {name}")));
            }
            if !valid_variable(
                &input.archive_variable,
                &["_SOURCE_BUNDLE", "_SOURCE_ARCHIVE"],
            ) || !valid_variable(
                &input.digest_variable,
                &["_SOURCE_SHA256", "_BUNDLE_SHA256"],
            ) {
                return Err(failure(format!("Invalid live input variables: {name}")));
            }
            if !valid_sha256(&input.sha256) {
                return Err(failure(format!("Invalid live input digest: {name}")));
            }
            if !valid_absolute_path(&input.default_path) {
                return Err(failure(format!("Invalid live input default path: {name}")));
            }
            if let Some(commit) = &input.commit {
                if !valid_commit(commit) {
                    return Err(failure(format!("Invalid live input commit: {name}")));
                }
            }
            if let Some(workflows) = &input.workflows {
                if workflows.is_empty()
                    || workflows
                        .iter()
                        .any(|workflow| !valid_workflow_name(workflow))
                {
                    return Err(failure(format!("Invalid live input workflows: {name}")));
                }
                let mut sorted = workflows.clone();
                sorted.sort();
                sorted.dedup();
                if sorted.len() != workflows.len() {
                    return Err(failure(format!("Invalid live input workflows: {name}")));
                }
            }
        }
        for (name, fixed) in &self.live_tools {
            if !valid_name(name) {
                return Err(failure(format!("Invalid live fixed name: {name}")));
            }
            if !valid_absolute_path(&fixed.path) {
                return Err(failure(format!("Invalid live fixed path: {name}")));
            }
            if !valid_sha256(&fixed.sha256) {
                return Err(failure(format!("Invalid live fixed digest: {name}")));
            }
        }
        for (name, driver) in &self.live_drivers {
            if !valid_name(name) {
                return Err(failure(format!("Invalid live driver name: {name}")));
            }
            if !valid_relative_path(&driver.file) {
                return Err(failure(format!("Invalid live driver file: {name}")));
            }
            if !valid_sha256(&driver.sha256) {
                return Err(failure(format!("Invalid live driver digest: {name}")));
            }
        }
        for (name, watch) in &self.diag {
            if !valid_name(name) {
                return Err(failure(format!("Invalid diag watch name: {name}")));
            }
            if !valid_relative_path(&watch.file) {
                return Err(failure(format!("Invalid diag watch file: {name}")));
            }
            if !valid_sha256(&watch.sha256) {
                return Err(failure(format!("Invalid diag watch digest: {name}")));
            }
            if !valid_sha256(&watch.diagnostic_sha256) {
                return Err(failure(format!(
                    "Invalid diag watch diagnostic digest: {name}"
                )));
            }
        }
        Ok(())
    }
}

/// Load `.ci/live-inputs.toml`: `Ok(None)` when absent (render path), an
/// error on any schema or semantic drift. The runtime job requires `Some`.
pub(crate) fn load_manifest(repo: &Path) -> Result<Option<Manifest>> {
    let text = match fs::read_to_string(repo.join(".ci/live-inputs.toml")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut manifest: Manifest = toml::from_str(&text)
        .map_err(|error| failure(format!("Invalid .ci/live-inputs.toml: {error}")))?;
    manifest.validate()?;
    for (name, input) in &mut manifest.live_inputs {
        input.name.clone_from(name);
    }
    Ok(Some(manifest))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const FIXTURE: &str = r#"
schema = 1
[live-inputs.v01-source-bundle]
archive_variable = "V01_SOURCE_BUNDLE"
digest_variable = "V01_SOURCE_BUNDLE_SHA256"
sha256 = "a89129a0f4e827c807f1453ef40c5968d1a063267d4be8741783abdaa31e82b6"
default_path = "/workspaces/ci-sources/v01-tor-records/a89129a0f4e827c807f1453ef40c5968d1a063267d4be8741783abdaa31e82b6.tar"
workflows = ["v01-tor-live"]
[live-inputs.v01-chutney]
archive_variable = "V01_CHUTNEY_SOURCE_ARCHIVE"
digest_variable = "V01_CHUTNEY_SOURCE_SHA256"
sha256 = "3d8748142f5d1fc3243371b33ff5a431d444c69b836d22718898f1787eec00ee"
default_path = "/workspaces/ci-sources/10-source-chutney/3d8748142f5d1fc3243371b33ff5a431d444c69b836d22718898f1787eec00ee.tar"
workflows = ["v01-tor-live"]
[live-inputs.v01-root-source]
archive_variable = "V01_ROOT_SOURCE_ARCHIVE"
digest_variable = "V01_ROOT_SOURCE_SHA256"
sha256 = "a84b41fcf68d71507db9e46072c3c329ec1b2667114c38fe3ff275bd7ab6cb1d"
default_path = "/workspaces/ci-sources/146/a84b41fcf68d71507db9e46072c3c329ec1b2667114c38fe3ff275bd7ab6cb1d.tar"
commit = "bb0f72d89dbfc5323580205740139fc84cc7a284"
workflows = ["v01-tor-live"]
[live-tools.receipt]
path = "/workspaces/component-releases/cmsg/20a55ac8159811bbac7c8370ae7f28fc075f2153/tor-tools/tools.json"
sha256 = "b3a64bbc373b886dc164e859c1d26e7a4055a7d65b8c2556a0bd2cff4ac152e0"
[live-tools.tor]
path = "/workspaces/component-tools/cmsg/tor-0.4.9.12-20a55ac8159811bbac7c8370ae7f28fc075f2153/bin/tor"
sha256 = "3b03c797db84b76bcde96f68997656cc42368b173c0da88b40d5acc2d666cc1b"
[live-tools.tor-gencert]
path = "/workspaces/component-tools/cmsg/tor-0.4.9.12-20a55ac8159811bbac7c8370ae7f28fc075f2153/bin/tor-gencert"
sha256 = "68df7a29ae0bed669df63b4c8d8658240cb951618860623212fdbd8993923114"
[live-tools.python]
path = "/workspaces/component-tools/cmsg/python-20a55ac8159811bbac7c8370ae7f28fc075f2153/bin/python"
sha256 = "f5cce9ecc914b0c2eee78056c1c816aa02ae4c73c0aaf958eb5e0bb9281f34ea"
[live-tools.rustc]
path = "/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin/rustc"
sha256 = "8f40f2f394f7ff470ccfac7da1f03661756216a77fdb3181bfb81bbf01e07cdb"
[live-tools.cargo]
path = "/nix/store/w20n3pmhhd1av9llykxa3gd21c9jsm8l-cargo-1.98.1/bin/cargo"
sha256 = "88a18d3c29700de42bc2a2f5590e919e36557964cf4e1dd2f60f892376f6c102"
[live-tools.rustdoc]
path = "/nix/store/vy0xilifxb02fwal0wihsrwc8s69rlyk-rustc-wrapper-1.98.1/bin/rustdoc"
sha256 = "999b2098594b2b8d91db71e11bfbb2c0bd8cf24acac792308ac08c267f813bcb"
[live-tools.rustfmt]
path = "/nix/store/jpcqlbhkwxwvq507mq0hkacpxbxcdwjj-rustfmt-1.98.1/bin/rustfmt"
sha256 = "9fc2eff4ce7281f2a77c0164ab8e8b55fcef98f3c8ba96d4e4179ba8f3abc7fd"
[live-tools.cargo-clippy]
path = "/nix/store/n88pdyarvdpyw98fahxcb03733nsclhq-clippy-1.98.1/bin/cargo-clippy"
sha256 = "4396915d16e54967584fb1730560ca5c92bbea823398a8749c1e05109ad8e39c"
[live-tools.clippy-driver]
path = "/nix/store/n88pdyarvdpyw98fahxcb03733nsclhq-clippy-1.98.1/bin/clippy-driver"
sha256 = "0616995d944bbdd0b2bc9aee822d4a0cb07b05f8eabf01367a67fc94de05089f"
[live-tools.wasm-libcore]
path = "/nix/store/xvp6nfxayb07si2jaggqwvx3iykw89g2-rustc-1.98.1/lib/rustlib/wasm32-unknown-unknown/lib/libcore-e6b063672db74229.rlib"
sha256 = "e917c01a724e0622601835f8a6f1c8bff102f5f40a61152ce95a099a4bc7ba61"
[live-tools.wasm-libstd]
path = "/nix/store/xvp6nfxayb07si2jaggqwvx3iykw89g2-rustc-1.98.1/lib/rustlib/wasm32-unknown-unknown/lib/libstd-0d5130a4ee2cc288.rlib"
sha256 = "61ce675fface73dbbf431603767a3aa7f05bf9d6995d0555855fa5e4ead667e6"
[live-drivers.harness]
file = ".ci/v01-tor-live.py"
sha256 = "20314ee07fc2adaae05c4018268abec49865c2a78e0710519fa55de0b1c5cc5b"
[live-drivers.components]
file = ".ci/v01-tor-records.py"
sha256 = "6ad575e38543b9668379894ca8752361aa4b76bc28b66cf69a7c6b66fa4a9e22"
[live-drivers.helpers]
file = ".ci/v01-records.py"
sha256 = "64796854b4d018d455b06d3ac67db5cc78a3058440c758a1161fb52d7f7e7e0a"
[diag.discovery-lib]
file = "src/tor_discovery.rs"
sha256 = "fc6c8db74254e6246bb6da69228e4afe2c17ff42575133bb3ab1b530d2c30041"
diagnostic_sha256 = "f67ec53ed7f19e9bf7293eca631a24687b14fc2558109269edcbd901fade91d7"
"#;

    fn manifest(text: &str) -> Manifest {
        let manifest: Manifest = toml::from_str(text).unwrap();
        manifest.validate().unwrap();
        manifest
    }

    #[test]
    fn real_manifest_shape_parses_with_all_tables() {
        let parsed = manifest(FIXTURE);
        assert_eq!(parsed.schema, 1);
        assert_eq!(parsed.live_inputs.len(), 3);
        assert_eq!(parsed.live_tools.len(), 12);
        assert_eq!(parsed.live_drivers.len(), 3);
        assert_eq!(parsed.diag.len(), 1);
        assert_eq!(
            parsed.diag["discovery-lib"].sha256,
            "fc6c8db74254e6246bb6da69228e4afe2c17ff42575133bb3ab1b530d2c30041"
        );
        assert!(parsed.live_inputs["v01-source-bundle"]
            .digest_variable
            .ends_with("_BUNDLE_SHA256"));
        assert!(parsed.live_inputs["v01-root-source"].commit.is_some());
    }

    #[test]
    fn drifted_manifest_fails_closed() {
        // Unknown field, missing input digest, bad digest, traversal path,
        // wrong workflow scope, unknown section: every drift refuses.
        let unknown = FIXTURE.replace("sha256 = ", "digest = ");
        assert!(toml::from_str::<Manifest>(&unknown).is_err());
        let tampered = FIXTURE.replace("a89129a0", "b89129a0");
        assert!(manifest(&tampered).live_inputs["v01-source-bundle"]
            .sha256
            .starts_with("b89129a0"));
        let relative = FIXTURE.replace("/workspaces/component-releases", "workspaces/x");
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".ci")).unwrap();
        fs::write(root.path().join(".ci/live-inputs.toml"), &tampered).unwrap();
        // Shape-valid tampering parses (runtime digest verification catches
        // it); schema drift below refuses at load.
        assert!(load_manifest(root.path()).unwrap().is_some());
        for bad in [
            unknown,
            relative,
            FIXTURE.replace("workflows = [\"v01-tor-live\"]", "workflows = []"),
            FIXTURE.replace("[live-tools.receipt]", "[live-tools.receipt]\nextra = 1"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            fs::create_dir_all(directory.path().join(".ci")).unwrap();
            fs::write(directory.path().join(".ci/live-inputs.toml"), bad).unwrap();
            assert!(load_manifest(directory.path()).is_err());
        }
        let absent = tempfile::tempdir().unwrap();
        assert!(load_manifest(absent.path()).unwrap().is_none());
    }
}

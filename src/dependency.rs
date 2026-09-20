//! Crow-only Cargo dependency resolution with reviewable lock snapshots.
use crate::{
    admission, budget, cache, event, failure, value, verify_source, Environment, Result, Runner,
};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::Builder;

const RESOLVE_ENV: &str = "CCID_RESOLVE_CARGO";

#[derive(Debug, Serialize)]
struct Receipt {
    schema: u32,
    status: &'static str,
    accepted: bool,
    resolved_at_unix: u64,
    source_commit: String,
    repository: String,
    mode: &'static str,
    compatibility_status: &'static str,
    source_archive_sha256: String,
    original_lock_sha256: Option<String>,
    original_lock_snapshot: Option<String>,
    candidate_lock_sha256: String,
    cargo_version: String,
    rustc_version: String,
    lock_format_before: Option<u64>,
    lock_format_after: u64,
    direct_breaking_changes: Vec<String>,
    removed_packages: Vec<String>,
    added_packages: Vec<String>,
    validation_checks: Vec<String>,
    validation_status: &'static str,
    validation_error: Option<String>,
    validation_source_sha256: Option<String>,
    validation_manifest_sha256: Option<String>,
    lock_snapshot: String,
}

pub fn resolve_cargo(
    repo: &Path,
    output_dir: &Path,
    validation_checks: &[String],
    generate_lockfile: bool,
    plan: bool,
) -> Result<()> {
    let mut environment: Environment = std::env::vars_os().collect();
    if value(&environment, RESOLVE_ENV).as_deref() != Some("1") {
        return Err(failure(format!(
            "Cargo resolution is Crow-only; set {RESOLVE_ENV}=1 in the explicit Crow lane"
        )));
    }
    let repository = value(&environment, "CI_REPOSITORY_URL")
        .ok_or_else(|| failure("Cargo resolution requires CI_REPOSITORY_URL from Crow"))?;
    let source_commit = value(&environment, "CI_COMMIT_SHA")
        .ok_or_else(|| failure("Cargo resolution requires CI_COMMIT_SHA from Crow"))?;
    let source_archive = value(&environment, "SOURCE_ARCHIVE")
        .ok_or_else(|| failure("Cargo resolution requires Crow's verified SOURCE_ARCHIVE"))?;
    let source_sha256 = value(&environment, "SOURCE_SHA256")
        .ok_or_else(|| failure("Cargo resolution requires Crow's SOURCE_SHA256"))?;
    let cargo_home = value(&environment, "CARGO_HOME")
        .ok_or_else(|| failure("Cargo resolution requires an explicit inherited CARGO_HOME"))?;
    if cargo_home.is_empty() {
        return Err(failure("Cargo resolution requires a nonempty CARGO_HOME"));
    }
    for variable in ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"] {
        if environment
            .get(&OsString::from(variable))
            .is_some_and(|value| value.is_empty())
        {
            environment.remove(&OsString::from(variable));
        }
    }
    let requested_root = repo.canonicalize()?;
    let requested_output = if output_dir.is_absolute() {
        output_dir.to_path_buf()
    } else {
        std::env::current_dir()?.join(output_dir)
    };
    if requested_output == requested_root || requested_output.starts_with(&requested_root) {
        return Err(failure(
            "Cargo resolution output must be outside the source worktree",
        ));
    }
    if let Some(parent) = requested_output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir_all(&requested_output)?;
    let output_dir = requested_output.canonicalize()?;
    if output_dir == requested_root || output_dir.starts_with(&requested_root) {
        return Err(failure(
            "Cargo resolution output resolves inside the source worktree",
        ));
    }
    let verified = Builder::new().prefix("ccid-cargo-source-").tempdir()?;
    let verified_root = verified.path().join("source");
    verify_source(
        Path::new(&source_archive),
        &source_sha256,
        &source_commit,
        &verified_root,
    )?;
    let root = verified_root.canonicalize()?;
    if output_dir == root || output_dir.starts_with(&root) {
        return Err(failure(
            "Cargo resolution output resolves inside the verified source worktree",
        ));
    }
    let original_path = root.join("Cargo.lock");
    let original = match fs::read(&original_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if generate_lockfile && error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(failure(format!(
                "cannot read {}: {error}",
                original_path.display()
            )))
        }
    };
    let before = original.as_deref().map(lock_summary).transpose()?;
    let mode = if generate_lockfile {
        "generate"
    } else {
        "refresh"
    };
    let compatibility_status = if generate_lockfile {
        "not-compared-explicit-generation"
    } else {
        "checked-against-baseline"
    };
    let resources = budget(&environment)?;
    admission::admit(&mut environment)?;
    let resolution_deadline = std::time::Instant::now()
        .checked_add(std::time::Duration::from_secs(resources.timeout))
        .ok_or_else(|| failure("Cargo resolution deadline is out of range"))?;
    fs::create_dir_all(&output_dir)?;
    if plan {
        event(json!({
            "event": "cargo-resolution-plan",
            "repository": repository,
            "source_commit": source_commit,
            "output_dir": output_dir,
            "cargo_home": cargo_home,
            "timeout": resources.timeout,
            "mode": mode,
            "compatibility_status": compatibility_status,
            "source_mutation": false,
        }));
        return Ok(());
    }

    let runner = Runner::until(root.clone(), environment, resolution_deadline)?;
    let cargo_version = runner.run(&strings(&["cargo", "--version"]), true)?;
    let rustc_version = runner.run(&strings(&["rustc", "--version"]), true)?;
    let metadata_command = strings(&[
        "cargo",
        "--config",
        "net.offline=false",
        "metadata",
        "--locked",
        "--format-version",
        "1",
    ]);
    let before_metadata = if generate_lockfile {
        None
    } else {
        Some(runner.run(&metadata_command, true)?)
    };
    runner.run(
        &strings(&[
            "cargo",
            "--config",
            "net.offline=false",
            if generate_lockfile {
                "generate-lockfile"
            } else {
                "update"
            },
        ]),
        false,
    )?;
    let after_metadata = runner.run(&metadata_command, true)?;
    let candidate_path = root.join("Cargo.lock");
    let candidate = fs::read(&candidate_path)?;
    let after = lock_summary(&candidate)?;
    let direct_after = direct_package_versions(&after_metadata)?;
    let direct_breaking_changes = match before_metadata {
        Some(metadata) => {
            direct_breaking_changes(&direct_package_versions(&metadata)?, &direct_after)
        }
        None => Vec::new(),
    };
    let before_packages = before
        .as_ref()
        .map(|summary| package_names(&summary.packages))
        .unwrap_or_default();
    let mut removed_packages = before_packages.clone();
    for name in package_names(&after.packages) {
        removed_packages.remove(&name);
    }
    let mut added_packages = package_names(&after.packages);
    for name in before_packages {
        added_packages.remove(&name);
    }
    let direct_accepted = generate_lockfile
        || (before
            .as_ref()
            .is_some_and(|summary| summary.format == after.format)
            && direct_breaking_changes.is_empty());
    let mut validation_status = if validation_checks.is_empty() {
        "not-requested"
    } else if direct_accepted {
        "pending"
    } else {
        "skipped"
    };
    let mut validation_error = None;
    let mut validation_source_sha256 = None;
    let mut validation_manifest_sha256 = None;
    if direct_accepted && !validation_checks.is_empty() {
        let manifest_path = root.join(".ci/ccid.toml");
        let manifest_before = fs::read(&manifest_path).map_err(|error| {
            failure(format!("cannot read {}: {error}", manifest_path.display()))
        })?;
        let source_before = source_tree_digest(&root)?;
        validation_source_sha256 = Some(source_before.clone());
        validation_manifest_sha256 = Some(digest(&manifest_before));
        let mut validation_environment = runner.environment.clone();
        let remaining = resolution_deadline.saturating_duration_since(std::time::Instant::now());
        let check_result = if remaining.is_zero() {
            Err(failure(
                "resolver deadline elapsed before candidate validation",
            ))
        } else {
            crate::set(
                &mut validation_environment,
                "CI_TIMEOUT",
                remaining.as_secs().max(1).to_string(),
            );
            drop(runner);
            crate::run_checks_with_environment(
                &root,
                Path::new(".ci/ccid.toml"),
                validation_checks,
                false,
                validation_environment,
                &source_commit,
                resolution_deadline,
            )
        };
        let integrity_result = candidate_integrity(
            &root,
            &candidate_path,
            &candidate,
            &manifest_path,
            &manifest_before,
            &source_before,
        );
        (validation_status, validation_error) = validation_outcome(check_result, integrity_result);
    }
    let accepted = direct_accepted && validation_error.is_none();
    let status = if accepted { "candidate" } else { "rejected" };
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let (snapshot, original_snapshot, receipt_path) =
        artifact_paths(&output_dir, timestamp, status, original.is_some());
    write_atomic(&snapshot, &candidate)?;
    if let (Some(path), Some(bytes)) = (&original_snapshot, &original) {
        write_atomic(path, bytes)?;
    }
    let receipt = Receipt {
        schema: 3,
        status,
        accepted,
        resolved_at_unix: timestamp,
        source_commit,
        repository,
        mode,
        compatibility_status,
        source_archive_sha256: source_sha256,
        original_lock_sha256: original.as_deref().map(digest),
        original_lock_snapshot: original_snapshot.map(|path| path.display().to_string()),
        candidate_lock_sha256: digest(&candidate),
        cargo_version: cargo_version.trim().to_owned(),
        rustc_version: rustc_version.trim().to_owned(),
        lock_format_before: before.as_ref().map(|summary| summary.format),
        lock_format_after: after.format,
        direct_breaking_changes,
        removed_packages: removed_packages.into_iter().collect(),
        added_packages: added_packages.into_iter().collect(),
        validation_checks: validation_checks.to_vec(),
        validation_status,
        validation_error,
        validation_source_sha256,
        validation_manifest_sha256,
        lock_snapshot: snapshot.display().to_string(),
    };
    let receipt_bytes = serde_json::to_vec_pretty(&receipt)?;
    write_atomic(&receipt_path, &receipt_bytes)?;
    event(json!({
        "event": "cargo-resolution",
        "status": status,
        "accepted": accepted,
        "source_commit": receipt.source_commit,
        "repository": receipt.repository,
        "mode": receipt.mode,
        "compatibility_status": receipt.compatibility_status,
        "source_archive_sha256": receipt.source_archive_sha256,
        "original_lock_snapshot": receipt.original_lock_snapshot,
        "original_lock_sha256": receipt.original_lock_sha256,
        "candidate_lock_sha256": receipt.candidate_lock_sha256,
        "validation_checks": receipt.validation_checks,
        "validation_status": receipt.validation_status,
        "validation_error": receipt.validation_error,
        "validation_source_sha256": receipt.validation_source_sha256,
        "validation_manifest_sha256": receipt.validation_manifest_sha256,
        "cargo_version": receipt.cargo_version,
        "rustc_version": receipt.rustc_version,
        "lock_snapshot": receipt.lock_snapshot,
        "receipt": receipt_path,
        "source_mutation": false,
    }));
    if accepted {
        Ok(())
    } else {
        Err(failure(
            "Cargo resolution rejected: compatibility or candidate validation failed",
        ))
    }
}

fn artifact_paths(
    output_dir: &Path,
    timestamp: u64,
    status: &str,
    has_original: bool,
) -> (PathBuf, Option<PathBuf>, PathBuf) {
    let mut suffix = 0u64;
    loop {
        let suffix_text = if suffix == 0 {
            String::new()
        } else {
            format!(".{suffix}")
        };
        let snapshot = output_dir.join(format!("Cargo.lock.{timestamp}.{status}{suffix_text}"));
        let original_snapshot = has_original.then(|| {
            output_dir.join(format!(
                "Cargo.lock.{timestamp}.{status}{suffix_text}.original"
            ))
        });
        let receipt = output_dir.join(format!("cargo-resolution-{timestamp}{suffix_text}.json"));
        if !snapshot.exists()
            && !receipt.exists()
            && !original_snapshot.as_ref().is_some_and(|path| path.exists())
        {
            return (snapshot, original_snapshot, receipt);
        }
        suffix = suffix.saturating_add(1);
    }
}

#[derive(Debug)]
struct LockSummary {
    format: u64,
    packages: BTreeMap<String, BTreeSet<String>>,
}

fn lock_summary(bytes: &[u8]) -> Result<LockSummary> {
    let value: toml::Value = toml::from_str(std::str::from_utf8(bytes)?)?;
    let format = value
        .get("version")
        .and_then(toml::Value::as_integer)
        .ok_or_else(|| failure("Cargo.lock has no numeric format version"))?;
    let array = value
        .get("package")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| failure("Cargo.lock has no package table"))?;
    let mut packages = BTreeMap::new();
    for package in array {
        let name = package
            .get("name")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| failure("Cargo.lock package has no name"))?;
        let version = package
            .get("version")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| failure(format!("Cargo.lock package {name} has no version")))?;
        packages
            .entry(name.to_owned())
            .or_insert_with(BTreeSet::new)
            .insert(version.to_owned());
    }
    if format < 0 {
        return Err(failure("Cargo.lock format version cannot be negative"));
    }
    Ok(LockSummary {
        format: format as u64,
        packages,
    })
}

fn package_names(packages: &BTreeMap<String, BTreeSet<String>>) -> BTreeSet<String> {
    packages.keys().cloned().collect()
}

fn compatibility(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next()?;
    let mut fields = core.split('.');
    let major = fields.next()?.parse().ok()?;
    let minor = fields.next()?.parse().ok()?;
    let patch = fields.next()?.parse().ok()?;
    Some(if major != 0 {
        (major, 0, 0)
    } else if minor != 0 {
        (0, minor, 0)
    } else {
        (0, 0, patch)
    })
}

fn direct_package_versions(metadata: &str) -> Result<BTreeMap<String, BTreeSet<(u64, u64, u64)>>> {
    let document: serde_json::Value = serde_json::from_str(metadata)?;
    let members = document
        .get("workspace_members")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| failure("Cargo metadata has no workspace members"))?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<BTreeSet<_>>();
    let mut package_versions = BTreeMap::new();
    for package in document
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| failure("Cargo metadata has no packages"))?
    {
        let id = package
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| failure("Cargo metadata package has no id"))?;
        let name = package
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| failure("Cargo metadata package has no name"))?;
        let version = package
            .get("version")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| failure(format!("Cargo metadata package {name} has no version")))?;
        let compatibility = compatibility(version)
            .ok_or_else(|| failure(format!("Cargo metadata package {name} has invalid version")))?;
        package_versions.insert(id.to_owned(), (name.to_owned(), compatibility));
    }
    let nodes = document
        .get("resolve")
        .and_then(|resolve| resolve.get("nodes"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| failure("Cargo metadata has no dependency resolution graph"))?;
    let mut direct = BTreeMap::new();
    for node in nodes {
        let Some(id) = node.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !members.contains(id) {
            continue;
        }
        for dependency in node
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| failure("Cargo metadata workspace node has no dependencies"))?
        {
            let dependency_id = dependency
                .as_str()
                .or_else(|| dependency.get("pkg").and_then(serde_json::Value::as_str));
            let Some(dependency_id) = dependency_id else {
                continue;
            };
            let Some((name, version)) = package_versions.get(dependency_id) else {
                return Err(failure(format!(
                    "Cargo metadata dependency {dependency_id} is not in packages"
                )));
            };
            direct
                .entry(name.clone())
                .or_insert_with(BTreeSet::new)
                .insert(*version);
        }
    }
    Ok(direct)
}

fn direct_breaking_changes(
    before: &BTreeMap<String, BTreeSet<(u64, u64, u64)>>,
    after: &BTreeMap<String, BTreeSet<(u64, u64, u64)>>,
) -> Vec<String> {
    let mut changes = Vec::new();
    for (name, previous) in before {
        let Some(versions) = after.get(name) else {
            changes.push(format!("{name}: removed"));
            continue;
        };
        for version in versions {
            if !previous.contains(version) {
                changes.push(format!(
                    "{name}: direct compatibility changed to {}.{}.{}",
                    version.0, version.1, version.2
                ));
            }
        }
    }
    changes
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn source_tree_digest(root: &Path) -> Result<String> {
    cache::source_tree_digest(root)
}

fn candidate_integrity(
    root: &Path,
    candidate_path: &Path,
    candidate: &[u8],
    manifest_path: &Path,
    manifest_before: &[u8],
    source_before: &str,
) -> Result<()> {
    if fs::read(candidate_path)? != candidate {
        return Err(failure("candidate Cargo.lock changed during validation"));
    }
    if fs::read(manifest_path)? != manifest_before {
        return Err(failure(
            "validation manifest changed during candidate checks",
        ));
    }
    if source_tree_digest(root)? != source_before {
        return Err(failure("isolated source changed during candidate checks"));
    }
    Ok(())
}

fn validation_outcome(
    check_result: Result<()>,
    integrity_result: Result<()>,
) -> (&'static str, Option<String>) {
    let errors = [check_result, integrity_result]
        .into_iter()
        .filter_map(|result| result.err().map(|error| error.to_string()))
        .collect::<Vec<_>>();
    if errors.is_empty() {
        ("passed", None)
    } else {
        ("failed", Some(errors.join("; ")))
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temporary = path.with_extension(format!("partial-{}-{nonce}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    let result = fs::hard_link(&temporary, path);
    let _ = fs::remove_file(&temporary);
    result?;
    Ok(())
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_snapshot_names_preserve_every_original_and_skip_partial_outputs() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut originals = BTreeSet::new();
        for _ in 0..4 {
            let (candidate, original, receipt) = artifact_paths(temp.path(), 42, "candidate", true);
            let original = original.unwrap();
            assert!(originals.insert(original.clone()));
            write_atomic(&candidate, b"candidate").unwrap();
            write_atomic(&original, b"original").unwrap();
            write_atomic(&receipt, b"receipt").unwrap();
        }
        let (candidate, original, receipt) = artifact_paths(temp.path(), 42, "candidate", true);
        write_atomic(&original.unwrap(), b"partial-original").unwrap();
        let next = artifact_paths(temp.path(), 42, "candidate", true);
        assert_ne!(candidate, next.0);
        assert_ne!(receipt, next.2);
        for original in originals {
            assert_eq!(fs::read(original).unwrap(), b"original");
        }
    }

    #[test]
    fn major_upgrade_is_rejected_but_patch_updates_are_allowed() {
        let before = BTreeMap::from([("serde".to_owned(), BTreeSet::from([(1, 0, 0)]))]);
        let patch = BTreeMap::from([("serde".to_owned(), BTreeSet::from([(1, 0, 0)]))]);
        let major = BTreeMap::from([("serde".to_owned(), BTreeSet::from([(2, 0, 0)]))]);
        assert!(direct_breaking_changes(&before, &patch).is_empty());
        assert_eq!(
            direct_breaking_changes(&before, &major),
            vec!["serde: direct compatibility changed to 2.0.0"]
        );
    }

    #[test]
    fn a_zero_major_minor_break_is_rejected() {
        let before = BTreeMap::from([("serde".to_owned(), BTreeSet::from([(0, 3, 0)]))]);
        let after = BTreeMap::from([("serde".to_owned(), BTreeSet::from([(0, 4, 0)]))]);
        assert_eq!(
            direct_breaking_changes(&before, &after),
            vec!["serde: direct compatibility changed to 0.4.0"]
        );
    }

    #[test]
    fn compatibility_preserves_zero_zero_patch_boundaries() {
        assert_eq!(compatibility("1.2.3"), Some((1, 0, 0)));
        assert_eq!(compatibility("0.4.2"), Some((0, 4, 0)));
        assert_eq!(compatibility("0.0.3"), Some((0, 0, 3)));
        assert_eq!(compatibility("0.0.3+build"), Some((0, 0, 3)));
        assert_ne!(compatibility("0.0.3"), compatibility("0.0.4"));
    }

    #[test]
    fn metadata_parser_tracks_only_workspace_direct_edges() {
        let metadata = r#"{
          "workspace_members": ["path+file:///repo#app@1.0.0"],
          "packages": [
            {"id":"path+file:///repo#app@1.0.0","name":"app","version":"1.0.0"},
            {"id":"registry+https://example#serde@1.0.0","name":"serde","version":"1.0.0"},
            {"id":"registry+https://example#syn@2.0.0","name":"syn","version":"2.0.0"}
          ],
          "resolve": {"nodes": [
            {"id":"path+file:///repo#app@1.0.0","dependencies":["registry+https://example#serde@1.0.0"]}
          ]}
        }"#;
        let parsed = direct_package_versions(metadata).unwrap();
        assert_eq!(parsed["serde"], BTreeSet::from([(1, 0, 0)]));
        assert!(!parsed.contains_key("syn"));
    }

    #[test]
    fn lock_summary_keeps_duplicate_package_versions() {
        let lock = br#"version = 3

[[package]]
name = "foo"
version = "1.0.0"

[[package]]
name = "foo"
version = "2.0.0"
"#;
        let summary = lock_summary(lock).unwrap();
        assert_eq!(summary.format, 3);
        assert_eq!(summary.packages["foo"].len(), 2);
    }

    #[test]
    fn selected_check_failure_rejects_candidate_validation() {
        let (status, error) = validation_outcome(Err(failure("selected check failed")), Ok(()));
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("selected check failed"));
    }

    #[test]
    fn source_mutation_rejects_candidate_validation() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let candidate_path = root.join("Cargo.lock");
        let manifest_path = root.join("ccid.toml");
        let candidate = b"candidate lock";
        let manifest = b"manifest";
        fs::write(&candidate_path, candidate).unwrap();
        fs::write(&manifest_path, manifest).unwrap();
        fs::write(root.join("source.rs"), "before").unwrap();
        let source_before = source_tree_digest(root).unwrap();
        fs::write(root.join("source.rs"), "after").unwrap();
        let error = candidate_integrity(
            root,
            &candidate_path,
            candidate,
            &manifest_path,
            manifest,
            &source_before,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "isolated source changed during candidate checks"
        );
    }

    #[test]
    fn candidate_lock_mutation_has_a_specific_failure() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let candidate_path = root.join("Cargo.lock");
        let manifest_path = root.join("ccid.toml");
        fs::write(&candidate_path, "candidate lock").unwrap();
        fs::write(&manifest_path, "manifest").unwrap();
        let source_before = source_tree_digest(root).unwrap();
        fs::write(&candidate_path, "changed lock").unwrap();
        let error = candidate_integrity(
            root,
            &candidate_path,
            b"candidate lock",
            &manifest_path,
            b"manifest",
            &source_before,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "candidate Cargo.lock changed during validation"
        );
    }
}

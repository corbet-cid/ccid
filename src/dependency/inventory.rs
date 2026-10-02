//! One verified source, a committed graph inventory, and one frozen candidate.
use super::*;
use serde::Deserialize;
use std::path::Component;

const INVENTORY: &str = ".ci/resolve.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    schema: u32,
    checks: Vec<String>,
    graphs: Vec<Graph>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Graph {
    id: String,
    kind: Kind,
    manifest: PathBuf,
    lock: PathBuf,
    /// A previously resolved Cargo graph. Its complete package tuples must survive.
    seed: Option<String>,
    #[serde(default)]
    prepare: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Cargo,
    Npm,
}

#[derive(Debug)]
struct ResolvedGraph {
    original: Option<Vec<u8>>,
    candidate: Vec<u8>,
    before_format: Option<u64>,
    after_format: u64,
    breaking: Vec<String>,
    removed: Vec<String>,
    added: Vec<String>,
    compatible: bool,
    seed_sha256: Option<String>,
    resolved_at_unix: u64,
}

fn relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || path.to_str().is_none()
        || path.to_string_lossy().contains('\\')
        || path
            .to_string_lossy()
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(failure(
            "inventory paths must be plain relative UTF-8 paths",
        ));
    }
    Ok(())
}

fn plain_file(root: &Path, path: &Path) -> Result<PathBuf> {
    relative(path)?;
    let mut current = root.to_path_buf();
    for part in path.components() {
        current.push(part);
        if fs::symlink_metadata(&current)?.is_symlink() {
            return Err(failure("inventory paths cannot traverse symlinks"));
        }
    }
    if !current.is_file() {
        return Err(failure("inventory input is not a regular file"));
    }
    Ok(current)
}

impl Inventory {
    fn parse(bytes: &[u8]) -> Result<Self> {
        let inventory: Self = toml::from_str(std::str::from_utf8(bytes)?)?;
        if inventory.schema != 1 || inventory.graphs.is_empty() || inventory.checks.is_empty() {
            return Err(failure(
                "inventory requires schema 1, graphs, and fixed checks",
            ));
        }
        let mut checks = BTreeSet::new();
        for check in &inventory.checks {
            if check.is_empty() || !checks.insert(check) {
                return Err(failure("inventory checks must be nonempty and unique"));
            }
        }
        let mut ids = BTreeMap::new();
        let mut paths = BTreeSet::new();
        for graph in &inventory.graphs {
            if graph.id.is_empty()
                || !graph
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                || ids.contains_key(&graph.id)
            {
                return Err(failure("inventory graph IDs must be plain and unique"));
            }
            relative(&graph.manifest)?;
            relative(&graph.lock)?;
            let (manifest_name, lock_name) = match graph.kind {
                Kind::Cargo => ("Cargo.toml", "Cargo.lock"),
                Kind::Npm => ("package.json", "package-lock.json"),
            };
            if graph.manifest.file_name().and_then(|s| s.to_str()) != Some(manifest_name)
                || graph.lock.file_name().and_then(|s| s.to_str()) != Some(lock_name)
                || graph.manifest.parent() != graph.lock.parent()
                || !paths.insert(&graph.manifest)
                || !paths.insert(&graph.lock)
            {
                return Err(failure(
                    "each graph must own a distinct manifest and adjacent lock",
                ));
            }
            if let Some(seed) = &graph.seed {
                if graph.kind != Kind::Cargo
                    || ids.get(seed) != Some(&Kind::Cargo)
                    || graph.prepare.is_empty()
                {
                    return Err(failure("derived Cargo graphs require an earlier Cargo seed and preparation command"));
                }
                crate::validate_command(&graph.prepare)?;
            } else if !graph.prepare.is_empty() {
                return Err(failure(
                    "preparation is only allowed for a derived Cargo graph",
                ));
            }
            ids.insert(graph.id.clone(), graph.kind);
        }
        Ok(inventory)
    }
}

/// File and directory metadata, including symlink targets; never follows links.
fn entries(root: &Path) -> Result<BTreeMap<PathBuf, String>> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, String>) -> Result<()> {
        let full = root.join(path);
        let metadata = fs::symlink_metadata(&full)?;
        if metadata.is_dir() {
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode().to_string()
            };
            #[cfg(not(unix))]
            let mode = metadata.permissions().readonly().to_string();
            out.insert(path.to_path_buf(), format!("directory:{mode}"));
            for child in fs::read_dir(full)? {
                walk(root, &path.join(child?.file_name()), out)?;
            }
        } else if metadata.file_type().is_symlink() {
            // The link text is the identity; the target is never followed.
            out.insert(
                path.to_path_buf(),
                format!("symlink:{}", fs::read_link(&full)?.display()),
            );
        } else {
            out.insert(
                path.to_path_buf(),
                format!("file:{}", digest(&fs::read(&full)?)),
            );
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, Path::new(""), &mut out)?;
    Ok(out)
}

fn allowed_changes(
    before: &BTreeMap<PathBuf, String>,
    after: &BTreeMap<PathBuf, String>,
    allowed: &[&Path],
) -> Result<()> {
    for path in before.keys().chain(after.keys()) {
        if before.get(path) == after.get(path) || allowed.contains(&path.as_path()) {
            continue;
        }
        // Generated graph parents may be added, but existing directory modes cannot change.
        if !before.contains_key(path)
            && after.get(path).is_some_and(|v| v.starts_with("directory:"))
            && allowed.iter().any(|allowed| allowed.starts_with(path))
        {
            continue;
        }
        return Err(failure(format!(
            "resolution changed undeclared source path {}",
            path.display()
        )));
    }
    Ok(())
}

fn cargo_command(graph: &Graph, verb: &str, locked: bool) -> Vec<String> {
    let mut command = strings(&[
        "cargo",
        "--config",
        "net.offline=false",
        verb,
        "--manifest-path",
    ]);
    command.push(graph.manifest.to_string_lossy().into_owned());
    if locked {
        command.push("--locked".to_owned());
    }
    if verb == "metadata" {
        command.extend(strings(&["--format-version", "1"]));
    }
    command
}

fn verify_workspace(root: &Path, graph: &Graph, metadata: &str) -> Result<()> {
    let metadata: serde_json::Value = serde_json::from_str(metadata)?;
    let workspace = metadata
        .get("workspace_root")
        .and_then(|v| v.as_str())
        .ok_or_else(|| failure("Cargo metadata has no workspace root"))?;
    let expected = root
        .join(graph.manifest.parent().unwrap_or(Path::new("")))
        .canonicalize()?;
    if Path::new(workspace).canonicalize()? != expected {
        return Err(failure(
            "inventory Cargo manifest is not an independent workspace root",
        ));
    }
    Ok(())
}

fn package_tuples(bytes: &[u8]) -> Result<BTreeSet<(String, String, String)>> {
    let document: toml::Value = toml::from_str(std::str::from_utf8(bytes)?)?;
    let packages = document
        .get("package")
        .and_then(|v| v.as_array())
        .ok_or_else(|| failure("Cargo lock has no package table"))?;
    packages
        .iter()
        .map(|package| {
            let name = package
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| failure("Cargo package has no name"))?;
            let version = package
                .get("version")
                .and_then(|v| v.as_str())
                .ok_or_else(|| failure("Cargo package has no version"))?;
            let source = package.get("source").and_then(|v| v.as_str()).unwrap_or("");
            Ok((name.to_owned(), version.to_owned(), source.to_owned()))
        })
        .collect()
}

fn resolve_graph(runner: &Runner, graph: &Graph, seed: Option<&[u8]>) -> Result<ResolvedGraph> {
    let before_entries = entries(&runner.root)?;
    let original = if let Some(seed) = seed {
        if runner.root.join(&graph.manifest).exists() || runner.root.join(&graph.lock).exists() {
            return Err(failure(
                "derived graph manifest and lock must be absent from original source",
            ));
        }
        runner.run(&graph.prepare, false)?;
        plain_file(&runner.root, &graph.manifest)?;
        let lock = fs::read(plain_file(&runner.root, &graph.lock)?)?;
        if lock != seed {
            return Err(failure(
                "derived preparation must copy exact resolved seed lock",
            ));
        }
        allowed_changes(
            &before_entries,
            &entries(&runner.root)?,
            &[&graph.manifest, &graph.lock],
        )?;
        None
    } else {
        plain_file(&runner.root, &graph.manifest)?;
        Some(fs::read(plain_file(&runner.root, &graph.lock)?)?)
    };
    let prepared = entries(&runner.root)?;
    let (before_format, after_format, breaking, removed, added, candidate) = match graph.kind {
        Kind::Cargo => {
            let before_metadata = if original.is_some() {
                let metadata = runner.run(&cargo_command(graph, "metadata", true), true)?;
                verify_workspace(&runner.root, graph, &metadata)?;
                Some(metadata)
            } else {
                None
            };
            // Metadata resolves the new harness root without refreshing its seeded dependencies.
            if seed.is_some() {
                let metadata = runner.run(&cargo_command(graph, "metadata", false), true)?;
                verify_workspace(&runner.root, graph, &metadata)?;
            } else {
                runner.run(&cargo_command(graph, "update", false), false)?;
            }
            let metadata = runner.run(&cargo_command(graph, "metadata", true), true)?;
            verify_workspace(&runner.root, graph, &metadata)?;
            let candidate = fs::read(plain_file(&runner.root, &graph.lock)?)?;
            let before = original.as_deref().map(lock_summary).transpose()?;
            let after = lock_summary(&candidate)?;
            let breaking = before_metadata
                .as_deref()
                .map(|before| {
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(direct_breaking_changes(
                        &direct_package_versions(before)?,
                        &direct_package_versions(&metadata)?,
                    ))
                })
                .transpose()?
                .unwrap_or_default();
            if let Some(seed) = seed {
                if !package_tuples(seed)?.is_subset(&package_tuples(&candidate)?) {
                    return Err(failure(
                        "derived Cargo graph did not preserve all seed package tuples",
                    ));
                }
            }
            let before_packages = before
                .as_ref()
                .map(|s| package_names(&s.packages))
                .unwrap_or_default();
            let after_packages = package_names(&after.packages);
            (
                before.as_ref().map(|s| s.format),
                after.format,
                breaking,
                before_packages
                    .difference(&after_packages)
                    .cloned()
                    .collect(),
                after_packages
                    .difference(&before_packages)
                    .cloned()
                    .collect(),
                candidate,
            )
        }
        Kind::Npm => {
            let manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(plain_file(&runner.root, &graph.manifest)?)?)?;
            if manifest.get("workspaces").is_some() {
                return Err(failure(
                    "npm workspaces are not supported by this inventory version",
                ));
            }
            let original = original
                .as_ref()
                .ok_or_else(|| failure("npm requires committed baseline"))?;
            let before = npm_summary(original)?;
            let prefix = graph.manifest.parent().unwrap_or(Path::new(""));
            let prefix = if prefix.as_os_str().is_empty() {
                Path::new(".")
            } else {
                prefix
            };
            let mut command = strings(&["npm", "update", "--prefix"]);
            command.push(prefix.to_string_lossy().into_owned());
            command.extend(strings(&["--package-lock-only", "--ignore-scripts"]));
            runner.run(&command, false)?;
            let candidate = fs::read(plain_file(&runner.root, &graph.lock)?)?;
            let after = npm_summary(&candidate)?;
            (
                Some(before.0),
                after.0,
                direct_breaking_changes(&before.1, &after.1),
                vec![],
                vec![],
                candidate,
            )
        }
    };
    allowed_changes(&prepared, &entries(&runner.root)?, &[&graph.lock])?;
    Ok(ResolvedGraph {
        original,
        candidate,
        before_format,
        after_format,
        compatible: before_format.is_none_or(|f| f == after_format) && breaking.is_empty(),
        breaking,
        removed,
        added,
        seed_sha256: seed.map(digest),
        resolved_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    })
}

fn npm_summary(bytes: &[u8]) -> Result<(u64, PackageCompatibility)> {
    let document: serde_json::Value = serde_json::from_slice(bytes)?;
    let format = document
        .get("lockfileVersion")
        .and_then(|v| v.as_u64())
        .filter(|v| [2, 3].contains(v))
        .ok_or_else(|| failure("npm requires lockfileVersion 2 or 3"))?;
    let packages = document
        .get("packages")
        .and_then(|v| v.as_object())
        .ok_or_else(|| failure("npm lock requires packages"))?;
    let root = packages
        .get("")
        .and_then(|v| v.as_object())
        .ok_or_else(|| failure("npm lock requires root package"))?;
    if root.contains_key("workspaces")
        || packages
            .values()
            .any(|p| p.get("link").and_then(|v| v.as_bool()) == Some(true))
    {
        return Err(failure(
            "npm workspace/link locks are not supported by this inventory version",
        ));
    }
    let mut direct = BTreeMap::new();
    for section in ["dependencies", "devDependencies", "optionalDependencies"] {
        if let Some(dependencies) = root.get(section) {
            for name in dependencies
                .as_object()
                .ok_or_else(|| failure("npm dependencies must be an object"))?
                .keys()
            {
                let version = packages
                    .get(&format!("node_modules/{name}"))
                    .and_then(|p| p.get("version"))
                    .and_then(|v| v.as_str())
                    .and_then(compatibility)
                    .ok_or_else(|| {
                        failure("npm direct dependency has no comparable locked version")
                    })?;
                direct
                    .entry(name.clone())
                    .or_insert_with(BTreeSet::new)
                    .insert(version);
            }
        }
    }
    Ok((format, direct))
}

/// Additive mode: existing single-root resolution is deliberately unchanged.
pub fn resolve_inventory(repo: &Path, output_dir: &Path, plan: bool) -> Result<()> {
    let mut environment: Environment = std::env::vars_os().collect();
    if value(&environment, RESOLVE_ENV).as_deref() != Some("1") {
        return Err(failure(
            "inventory resolution requires explicit Crow resolver context",
        ));
    }
    let required = |name: &str| {
        value(&environment, name)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| failure(format!("inventory resolution requires {name}")))
    };
    if value(&environment, "CHECKS").is_some_and(|v| !v.is_empty()) {
        return Err(failure("inventory mode refuses caller CHECKS"));
    }
    let repository = required("CI_REPOSITORY_URL")?;
    cache::canonical_repository(&repository)?;
    let source_commit = required("CI_COMMIT_SHA")?;
    let source_archive = required("SOURCE_ARCHIVE")?;
    let source_sha256 = required("SOURCE_SHA256")?;
    required("CARGO_HOME")?;
    let requested_root = repo.canonicalize()?;
    fs::create_dir_all(output_dir)?;
    let output = output_dir.canonicalize()?;
    if output.starts_with(&requested_root) {
        return Err(failure(
            "inventory output must be outside the requested source",
        ));
    }
    let verified = Builder::new().prefix("ccid-inventory-source-").tempdir()?;
    let root = verified.path().join("source");
    verify_source(
        Path::new(&source_archive),
        &source_sha256,
        &source_commit,
        &root,
    )?;
    let root = root.canonicalize()?;
    let inventory_bytes = fs::read(plain_file(&root, Path::new(INVENTORY))?)?;
    let inventory = Inventory::parse(&inventory_bytes)?;
    let manifest_bytes = fs::read(plain_file(&root, Path::new(".ci/ccid.toml"))?)?;
    let manifest: crate::Manifest = toml::from_str(std::str::from_utf8(&manifest_bytes)?)?;
    for check in &inventory.checks {
        let check = manifest
            .checks
            .get(check)
            .ok_or_else(|| failure("fixed inventory check is absent from manifest"))?;
        crate::checks::validate_check(check)?;
    }
    // Discover malformed, aliased and missing inputs before starting any resolver.
    for graph in &inventory.graphs {
        if graph.seed.is_none() {
            plain_file(&root, &graph.manifest)?;
            plain_file(&root, &graph.lock)?;
        } else {
            absent_plain_path(&root, &graph.manifest)?;
            absent_plain_path(&root, &graph.lock)?;
        }
    }
    if manifest.schema != 1 {
        return Err(failure("unsupported check manifest schema"));
    }
    let dependency_closure_sha256 = value(&environment, "DEPENDENCY_CLOSURE_SHA256");
    for variable in ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"] {
        if environment
            .get(&OsString::from(variable))
            .is_some_and(|v| v.is_empty())
        {
            environment.remove(&OsString::from(variable));
        }
    }
    let resources = budget(&environment)?;
    admission::admit(&mut environment)?;
    if plan {
        event(
            json!({"event":"inventory-resolution-plan", "source_commit":source_commit,
            "source_archive_sha256":source_sha256, "inventory_sha256":digest(&inventory_bytes),
            "graphs":inventory.graphs.iter().map(|g| &g.id).collect::<Vec<_>>(),
            "checks":inventory.checks, "source_mutation":false}),
        );
        return Ok(());
    }
    let deadline = std::time::Instant::now()
        .checked_add(std::time::Duration::from_secs(resources.timeout))
        .ok_or_else(|| failure("inventory resolution deadline is out of range"))?;
    let artifacts = Builder::new()
        .prefix("inventory-resolution-")
        .tempdir_in(&output)?
        .keep();
    let original_source_sha256 = source_tree_digest(&root)?;
    let runner = Runner::until(root.clone(), environment, deadline)?;
    let cargo_version = runner.run(&strings(&["cargo", "--version"]), true)?;
    let rustc_version = runner.run(&strings(&["rustc", "--version"]), true)?;
    let npm_version = if inventory.graphs.iter().any(|g| g.kind == Kind::Npm) {
        Some(runner.run(&strings(&["npm", "--version"]), true)?)
    } else {
        None
    };
    let node_version = if npm_version.is_some() {
        Some(runner.run(&strings(&["node", "--version"]), true)?)
    } else {
        None
    };
    let mut resolved: BTreeMap<String, ResolvedGraph> = BTreeMap::new();
    for graph in &inventory.graphs {
        let seed = graph
            .seed
            .as_ref()
            .map(|id| {
                resolved
                    .get(id)
                    .map(|r| r.candidate.as_slice())
                    .ok_or_else(|| failure("missing resolved seed"))
            })
            .transpose()?;
        let result = resolve_graph(&runner, graph, seed)?;
        // Retain before validation, including candidates that later fail another graph or gate.
        let graph_dir = artifacts.join(&graph.id);
        fs::create_dir(&graph_dir)?;
        write_atomic(&graph_dir.join("candidate.lock"), &result.candidate)?;
        if let Some(original) = &result.original {
            write_atomic(&graph_dir.join("original.lock"), original)?;
        }
        resolved.insert(graph.id.clone(), result);
    }
    let candidate_source_sha256 = source_tree_digest(&root)?;
    let lock_set: BTreeMap<_, _> = inventory
        .graphs
        .iter()
        .map(|graph| {
            (
                graph.lock.to_string_lossy().into_owned(),
                digest(&resolved[&graph.id].candidate),
            )
        })
        .collect();
    let lock_set_bytes = serde_json::to_vec(&lock_set)?;
    let lock_set_path = artifacts.join("lock-set.json");
    write_atomic(&lock_set_path, &lock_set_bytes)?;
    let lock_set_sha256 = digest(&lock_set_bytes);
    write_atomic(&artifacts.join("resolve.toml"), &inventory_bytes)?;
    let candidate_archive = artifacts.join("candidate-source.tar");
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&candidate_archive)?;
    let mut archive = tar::Builder::new(file);
    archive.append_dir_all("source", &root)?;
    archive.finish()?;
    drop(archive);
    let candidate_archive_sha256 = crate::source::sha256_file(&candidate_archive)?;
    let mut validation_environment = runner.environment.clone();
    crate::set(
        &mut validation_environment,
        "CCID_LOCK_SET",
        lock_set_path.display().to_string(),
    );
    crate::set(
        &mut validation_environment,
        "CCID_LOCK_SET_SHA256",
        lock_set_sha256.clone(),
    );
    drop(runner);
    let mut completed = Vec::new();
    let mut validation_error = None;
    let compatible = resolved.values().all(|graph| graph.compatible);
    if compatible {
        for check in &inventory.checks {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let check_result = if remaining.is_zero() {
                Err(failure(
                    "inventory deadline elapsed before candidate validation",
                ))
            } else {
                crate::set(
                    &mut validation_environment,
                    "CI_TIMEOUT",
                    remaining.as_secs().max(1).to_string(),
                );
                crate::run_checks_with_environment(
                    &root,
                    Path::new(".ci/ccid.toml"),
                    std::slice::from_ref(check),
                    false,
                    validation_environment.clone(),
                    &source_commit,
                    deadline,
                )
            };
            let integrity_result =
                frozen_integrity(&root, &candidate_source_sha256, &inventory, &resolved);
            let (status, error) = validation_outcome(check_result, integrity_result);
            if status != "passed" {
                validation_error = error;
                break;
            }
            completed.push(check.clone());
        }
    } else {
        validation_error =
            Some("one or more graphs changed direct compatibility or lock format".to_owned());
    }
    let artifact_integrity = (|| -> Result<()> {
        if fs::read(&lock_set_path)? != lock_set_bytes
            || crate::source::sha256_file(&candidate_archive)? != candidate_archive_sha256
            || fs::read(artifacts.join("resolve.toml"))? != inventory_bytes
        {
            return Err(failure(
                "retained aggregate artifacts changed during validation",
            ));
        }
        for graph in &inventory.graphs {
            let result = &resolved[&graph.id];
            if fs::read(artifacts.join(&graph.id).join("candidate.lock"))? != result.candidate {
                return Err(failure("retained candidate lock changed during validation"));
            }
            if let Some(original) = &result.original {
                if fs::read(artifacts.join(&graph.id).join("original.lock"))? != *original {
                    return Err(failure("retained original lock changed during validation"));
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = artifact_integrity {
        validation_error = Some(match validation_error {
            Some(prior) => format!("{prior}; {error}"),
            None => error.to_string(),
        });
    }
    let accepted = compatible && validation_error.is_none() && completed == inventory.checks;
    let status = if accepted { "candidate" } else { "rejected" };
    let validation_status = if accepted { "passed" } else { "failed" };
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut graph_receipts = BTreeMap::new();
    for graph in &inventory.graphs {
        let result = &resolved[&graph.id];
        let graph_dir = artifacts.join(&graph.id);
        let mut receipt = if graph.kind == Kind::Cargo {
            serde_json::to_value(Receipt {
                schema: 3,
                status,
                accepted,
                resolved_at_unix: result.resolved_at_unix,
                source_commit: source_commit.clone(),
                repository: repository.clone(),
                mode: if graph.seed.is_some() {
                    "derive-from-candidate"
                } else {
                    "refresh"
                },
                compatibility_status: if graph.seed.is_some() {
                    "checked-seed-package-subset"
                } else {
                    "checked-against-baseline"
                },
                source_archive_sha256: source_sha256.clone(),
                original_lock_sha256: result.original.as_deref().map(digest),
                original_lock_snapshot: result
                    .original
                    .as_ref()
                    .map(|_| graph_dir.join("original.lock").display().to_string()),
                candidate_lock_sha256: digest(&result.candidate),
                cargo_version: cargo_version.trim().to_owned(),
                rustc_version: rustc_version.trim().to_owned(),
                lock_format_before: result.before_format,
                lock_format_after: result.after_format,
                direct_breaking_changes: result.breaking.clone(),
                removed_packages: result.removed.clone(),
                added_packages: result.added.clone(),
                validation_checks: inventory.checks.clone(),
                validation_status,
                validation_error: validation_error.clone(),
                validation_source_sha256: Some(candidate_source_sha256.clone()),
                validation_manifest_sha256: Some(digest(&manifest_bytes)),
                lock_snapshot: graph_dir.join("candidate.lock").display().to_string(),
            })?
        } else {
            json!({"schema":1,"kind":"npm-resolution","status":status,"accepted":accepted,
                "resolved_at_unix":result.resolved_at_unix,"source_commit":source_commit,"repository":repository,
                "source_archive_sha256":source_sha256,"original_lock_sha256":result.original.as_deref().map(digest),
                "original_lock_snapshot":graph_dir.join("original.lock"),"candidate_lock_sha256":digest(&result.candidate),
                "lock_snapshot":graph_dir.join("candidate.lock"),"node_version":node_version,"npm_version":npm_version,
                "mode":"refresh","lock_format_before":result.before_format,"lock_format_after":result.after_format,
                "direct_breaking_changes":result.breaking,"validation_checks":inventory.checks,
                "validation_status":validation_status,"validation_error":validation_error,
                "validation_source_sha256":candidate_source_sha256,"validation_manifest_sha256":digest(&manifest_bytes)})
        };
        let object = receipt
            .as_object_mut()
            .ok_or_else(|| failure("receipt is not an object"))?;
        object.insert("graph_id".to_owned(), json!(graph.id));
        object.insert("manifest_path".to_owned(), json!(graph.manifest));
        object.insert("lock_path".to_owned(), json!(graph.lock));
        object.insert(
            "inventory_sha256".to_owned(),
            json!(digest(&inventory_bytes)),
        );
        object.insert("lock_set_sha256".to_owned(), json!(lock_set_sha256));
        object.insert("seed_lock_sha256".to_owned(), json!(result.seed_sha256));
        let bytes = serde_json::to_vec_pretty(&receipt)?;
        write_atomic(&graph_dir.join("receipt.json"), &bytes)?;
        graph_receipts.insert(
            graph.id.clone(),
            json!({"path":format!("{}/receipt.json",graph.id),"sha256":digest(&bytes)}),
        );
    }
    let receipt = json!({"schema":1,"kind":"fixed-lock-inventory-resolution","tool_revision":crate::SOURCE_REVISION,
        "status":status,"accepted":accepted,"resolved_at_unix":timestamp,"source_commit":source_commit,"repository":repository,
        "source_archive_sha256":source_sha256,"original_source_sha256":original_source_sha256,"dependency_closure_sha256":dependency_closure_sha256,
        "candidate_source_sha256":candidate_source_sha256,"candidate_archive_sha256":candidate_archive_sha256,
        "candidate_archive":"candidate-source.tar","inventory_sha256":digest(&inventory_bytes),
        "validation_manifest_sha256":digest(&manifest_bytes),"lock_set_sha256":lock_set_sha256,
        "graphs":graph_receipts,"validation_checks":inventory.checks,"completed_checks":completed,
        "validation_status":validation_status,"validation_error":validation_error});
    let receipt_path = artifacts.join("receipt.json");
    write_atomic(&receipt_path, &serde_json::to_vec_pretty(&receipt)?)?;
    event(
        json!({"event":"inventory-resolution","accepted":accepted,"receipt":receipt_path,"lock_set_sha256":lock_set_sha256}),
    );
    if accepted {
        Ok(())
    } else {
        Err(failure(
            "inventory candidate failed compatibility, checks, or aggregate source integrity",
        ))
    }
}

fn absent_plain_path(root: &Path, path: &Path) -> Result<()> {
    relative(path)?;
    let mut current = root.to_path_buf();
    for part in path.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_symlink() => {
                return Err(failure("derived path traverses a symlink"))
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Err(failure(
        "derived graph paths must be absent from original source",
    ))
}

fn frozen_integrity(
    root: &Path,
    source: &str,
    inventory: &Inventory,
    resolved: &BTreeMap<String, ResolvedGraph>,
) -> Result<()> {
    for graph in &inventory.graphs {
        if fs::read(plain_file(root, &graph.lock)?)? != resolved[&graph.id].candidate {
            return Err(failure(format!(
                "candidate lock changed during checks: {}",
                graph.lock.display()
            )));
        }
    }
    if source_tree_digest(root)? != source {
        return Err(failure("aggregate candidate source changed during checks"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = r#"
schema = 1
checks = ["acceptance"]
[[graphs]]
id = "root"
kind = "cargo"
manifest = "Cargo.toml"
lock = "Cargo.lock"
"#;
    fn lock(version: &str) -> Vec<u8> {
        format!("version = 4\n[[package]]\nname = \"fixture\"\nversion = \"{version}\"\n")
            .into_bytes()
    }
    fn source() -> tempfile::TempDir {
        let source = tempfile::tempdir().unwrap();
        fs::write(
            source.path().join("Cargo.toml"),
            b"[package]\nname='fixture'\nversion='1.0.0'\n",
        )
        .unwrap();
        fs::write(source.path().join("Cargo.lock"), lock("1.0.0")).unwrap();
        source
    }

    #[test]
    fn inventory_refuses_missing_checks_unknown_fields_and_aliased_paths() {
        for bytes in [
            ROOT.replace("checks = [\"acceptance\"]", "checks = []"),
            ROOT.replace("checks = [\"acceptance\"]", "checks = [\"acceptance\", \"acceptance\"]"),
            ROOT.replace("schema = 1", "schema = 2"),
            ROOT.replace("schema = 1", "schema = 1\nextra = true"),
            ROOT.replace("manifest = \"Cargo.toml\"", "manifest = \"../Cargo.toml\""),
            ROOT.replace("manifest = \"Cargo.toml\"", "manifest = \"./Cargo.toml\""),
            ROOT.replace("manifest = \"Cargo.toml\"", "manifest = \"/Cargo.toml\""),
            ROOT.replace("lock = \"Cargo.lock\"", "lock = \"other/Cargo.lock\""),
            format!("{ROOT}\n[[graphs]]\nid='other'\nkind='cargo'\nmanifest='Cargo.toml'\nlock='Cargo.lock'\n"),
        ] {
            assert!(Inventory::parse(bytes.as_bytes()).is_err(), "{bytes}");
        }
    }

    #[test]
    fn generated_graph_must_follow_cargo_seed_and_have_preparation() {
        let suffix = "\n[[graphs]]\nid='toy'\nkind='cargo'\nmanifest='toy/.build/Cargo.toml'\nlock='toy/.build/Cargo.lock'\nseed='root'\nprepare=['python3','toy/prepare.py']\n";
        let text = format!("{ROOT}{suffix}");
        assert!(Inventory::parse(text.as_bytes()).is_ok());
        assert!(
            Inventory::parse(text.replace("seed='root'", "seed='missing'").as_bytes()).is_err()
        );
        assert!(Inventory::parse(
            text.replace("prepare=['python3','toy/prepare.py']", "prepare=[]")
                .as_bytes()
        )
        .is_err());
    }

    #[test]
    fn preparation_may_only_add_declared_outputs_and_parent_directories() {
        let source = source();
        let before = entries(source.path()).unwrap();
        fs::create_dir_all(source.path().join("toy/.build")).unwrap();
        fs::write(source.path().join("toy/.build/Cargo.toml"), b"fixture").unwrap();
        fs::write(source.path().join("toy/.build/Cargo.lock"), lock("1.0.0")).unwrap();
        let allowed = [
            Path::new("toy/.build/Cargo.toml"),
            Path::new("toy/.build/Cargo.lock"),
        ];
        assert!(allowed_changes(&before, &entries(source.path()).unwrap(), &allowed).is_ok());
        fs::write(source.path().join("Cargo.toml"), b"modified").unwrap();
        assert!(allowed_changes(&before, &entries(source.path()).unwrap(), &allowed).is_err());
    }

    #[test]
    fn resolution_may_not_remove_or_change_another_graph_lock() {
        let source = source();
        fs::create_dir(source.path().join("browser")).unwrap();
        fs::write(source.path().join("browser/Cargo.lock"), lock("2.0.0")).unwrap();
        let before = entries(source.path()).unwrap();
        fs::write(source.path().join("Cargo.lock"), lock("1.1.0")).unwrap();
        assert!(allowed_changes(
            &before,
            &entries(source.path()).unwrap(),
            &[Path::new("Cargo.lock")]
        )
        .is_ok());
        fs::remove_file(source.path().join("browser/Cargo.lock")).unwrap();
        assert!(allowed_changes(
            &before,
            &entries(source.path()).unwrap(),
            &[Path::new("Cargo.lock")]
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn existing_and_generated_manifest_paths_refuse_symlink_parents() {
        let source = source();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("Cargo.toml"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), source.path().join("linked")).unwrap();
        assert!(plain_file(source.path(), Path::new("linked/Cargo.toml")).is_err());
        assert!(absent_plain_path(source.path(), Path::new("linked/new/Cargo.toml")).is_err());
        assert!(absent_plain_path(source.path(), Path::new("fresh/new/Cargo.toml")).is_ok());
    }

    #[test]
    fn nested_member_is_not_mislabeled_as_an_independent_workspace() {
        let source = source();
        fs::create_dir(source.path().join("browser")).unwrap();
        let graph = Graph {
            id: "browser".into(),
            kind: Kind::Cargo,
            manifest: "browser/Cargo.toml".into(),
            lock: "browser/Cargo.lock".into(),
            seed: None,
            prepare: vec![],
        };
        let wrong = json!({"workspace_root":source.path()}).to_string();
        assert!(verify_workspace(source.path(), &graph, &wrong).is_err());
        let right = json!({"workspace_root":source.path().join("browser")}).to_string();
        assert!(verify_workspace(source.path(), &graph, &right).is_ok());
    }

    #[test]
    fn seed_subset_preserves_exact_source_as_well_as_version() {
        let seed = b"version=4\n[[package]]\nname='dep'\nversion='1.0.0'\nsource='git+https://example.invalid/a#123'\n";
        let moved = String::from_utf8(seed.to_vec())
            .unwrap()
            .replace("/a#123", "/b#123");
        assert!(!package_tuples(seed)
            .unwrap()
            .is_subset(&package_tuples(moved.as_bytes()).unwrap()));
        let extended = format!(
            "{}\n[[package]]\nname='toy'\nversion='0.0.0'\n",
            String::from_utf8_lossy(seed)
        );
        assert!(package_tuples(seed)
            .unwrap()
            .is_subset(&package_tuples(extended.as_bytes()).unwrap()));
    }

    #[test]
    fn npm_compatibility_uses_actual_direct_versions_and_refuses_workspace_links() {
        let original = br#"{"lockfileVersion":3,"packages":{"":{"devDependencies":{"tool":"^1.0"}},"node_modules/tool":{"version":"1.2.0"}}}"#;
        let before = npm_summary(original).unwrap();
        let changed = String::from_utf8(original.to_vec())
            .unwrap()
            .replace("1.2.0", "2.0.0");
        assert!(
            !direct_breaking_changes(&before.1, &npm_summary(changed.as_bytes()).unwrap().1)
                .is_empty()
        );
        let linked = String::from_utf8(original.to_vec())
            .unwrap()
            .replace("\"version\":\"1.2.0\"", "\"link\":true");
        assert!(npm_summary(linked.as_bytes()).is_err());
        assert!(npm_summary(br#"{"lockfileVersion":1}"#).is_err());
    }

    #[test]
    fn frozen_candidate_rejects_nested_lock_and_non_lock_source_mutation() {
        let source = source();
        let inventory = Inventory::parse(ROOT.as_bytes()).unwrap();
        let resolved = BTreeMap::from([(
            "root".to_owned(),
            ResolvedGraph {
                original: Some(lock("1.0.0")),
                candidate: lock("1.0.0"),
                before_format: Some(4),
                after_format: 4,
                breaking: vec![],
                removed: vec![],
                added: vec![],
                compatible: true,
                seed_sha256: None,
                resolved_at_unix: 0,
            },
        )]);
        let frozen = source_tree_digest(source.path()).unwrap();
        assert!(frozen_integrity(source.path(), &frozen, &inventory, &resolved).is_ok());
        fs::write(source.path().join("Cargo.lock"), lock("1.1.0")).unwrap();
        assert!(frozen_integrity(source.path(), &frozen, &inventory, &resolved).is_err());
        fs::write(source.path().join("Cargo.lock"), lock("1.0.0")).unwrap();
        fs::write(source.path().join("Cargo.toml"), b"tampered").unwrap();
        assert!(frozen_integrity(source.path(), &frozen, &inventory, &resolved).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn actual_resolver_commands_refresh_independent_locks_and_reject_cross_graph_write() {
        use std::os::unix::fs::PermissionsExt;
        let source = source();
        let tools = tempfile::tempdir().unwrap();
        fs::create_dir(source.path().join("browser")).unwrap();
        fs::write(source.path().join("browser/Cargo.toml"), b"[workspace]").unwrap();
        fs::write(source.path().join("browser/Cargo.lock"), lock("1.0.0")).unwrap();
        fs::write(tools.path().join("candidate"), lock("1.1.0")).unwrap();
        let script = r#"#!/bin/sh
set -eu
case "$5" in Cargo.toml) sub='' ;; browser/Cargo.toml) sub='browser/' ;; *) exit 64 ;; esac
case "$3" in
metadata) printf '{"workspace_root":"%s/%s","workspace_members":[],"packages":[],"resolve":{"nodes":[]}}' "$PWD" "$sub" ;;
update) while IFS= read -r line; do printf '%s\n' "$line"; done < "$FIXTURE_TOOLS/candidate" > "${sub}Cargo.lock"
 if [ "${CROSS_WRITE:-}" = 1 ]; then printf 'tampered' > browser/Cargo.lock; fi ;;
*) exit 65 ;;
esac
"#;
        let executable = tools.path().join("cargo");
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let mut environment = Environment::new();
        crate::set(&mut environment, "PATH", tools.path().display().to_string());
        crate::set(
            &mut environment,
            "FIXTURE_TOOLS",
            tools.path().display().to_string(),
        );
        let mut runner = Runner::new(
            source.path().into(),
            environment,
            std::time::Duration::from_secs(15),
        )
        .unwrap();
        let inventory = Inventory::parse(format!("{ROOT}\n[[graphs]]\nid='browser'\nkind='cargo'\nmanifest='browser/Cargo.toml'\nlock='browser/Cargo.lock'\n").as_bytes()).unwrap();
        let root = resolve_graph(&runner, &inventory.graphs[0], None).unwrap();
        assert_eq!(root.candidate, lock("1.1.0"));
        assert_eq!(
            fs::read(source.path().join("browser/Cargo.lock")).unwrap(),
            lock("1.0.0")
        );
        let browser = resolve_graph(&runner, &inventory.graphs[1], None).unwrap();
        assert_eq!(browser.candidate, lock("1.1.0"));
        crate::set(&mut runner.environment, "CROSS_WRITE", "1");
        assert!(resolve_graph(&runner, &inventory.graphs[0], None)
            .unwrap_err()
            .to_string()
            .contains("undeclared source"));
    }

    #[cfg(unix)]
    fn fixture_executable(root: &Path, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn derived_command_path_resolves_metadata_only_and_preserves_exact_seed() {
        let source = source();
        let tools = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("toy/.build")).unwrap();
        fs::write(
            source.path().join("prepare.sh"),
            r#"set -eu
printf '[workspace]\n' > toy/.build/Cargo.toml
while IFS= read -r line; do printf '%s\n' "$line"; done < Cargo.lock > toy/.build/Cargo.lock
if [ "${BAD_SEED:-}" = 1 ]; then printf '\nchanged\n' >> toy/.build/Cargo.lock; fi
"#,
        )
        .unwrap();
        fixture_executable(
            tools.path(),
            "cargo",
            r#"#!/bin/sh
set -eu
[ "$3" = metadata ]
[ "$5" = toy/.build/Cargo.toml ]
if [ "$6" != --locked ]; then
  if [ "${DROP_SEED:-}" = 1 ]; then printf 'version=4\n' > toy/.build/Cargo.lock; fi
  printf '[[package]]\nname="toy"\nversion="0.0.0"\n' >> toy/.build/Cargo.lock
fi
printf '{"workspace_root":"%s/toy/.build","workspace_members":[],"packages":[],"resolve":{"nodes":[]}}' "$PWD"
"#,
        );
        let mut environment = Environment::new();
        crate::set(&mut environment, "PATH", tools.path().display().to_string());
        let mut runner = Runner::new(
            source.path().into(),
            environment,
            std::time::Duration::from_secs(15),
        )
        .unwrap();
        let graph = Graph {
            id: "toy".into(),
            kind: Kind::Cargo,
            manifest: "toy/.build/Cargo.toml".into(),
            lock: "toy/.build/Cargo.lock".into(),
            seed: Some("root".into()),
            prepare: strings(&["/bin/sh", "prepare.sh"]),
        };
        let seed = lock("1.0.0");
        let result = resolve_graph(&runner, &graph, Some(&seed)).unwrap();
        assert!(result.original.is_none());
        assert_eq!(result.seed_sha256, Some(digest(&seed)));
        assert!(package_tuples(&seed)
            .unwrap()
            .is_subset(&package_tuples(&result.candidate).unwrap()));
        assert_eq!(package_tuples(&result.candidate).unwrap().len(), 2);
        for name in [&graph.manifest, &graph.lock] {
            fs::remove_file(source.path().join(name)).unwrap();
        }
        crate::set(&mut runner.environment, "BAD_SEED", "1");
        assert!(resolve_graph(&runner, &graph, Some(&seed))
            .unwrap_err()
            .to_string()
            .contains("copy exact resolved seed"));
        for name in [&graph.manifest, &graph.lock] {
            fs::remove_file(source.path().join(name)).unwrap();
        }
        runner.environment.remove(&OsString::from("BAD_SEED"));
        crate::set(&mut runner.environment, "DROP_SEED", "1");
        assert!(resolve_graph(&runner, &graph, Some(&seed))
            .unwrap_err()
            .to_string()
            .contains("preserve all seed package tuples"));
    }

    #[cfg(unix)]
    #[test]
    fn npm_command_refreshes_only_declared_lock_and_reports_incompatible_direct_change() {
        let source = source();
        let tools = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("clients/ts")).unwrap();
        fs::write(source.path().join("clients/ts/package.json"), b"{}").unwrap();
        let baseline = br#"{"lockfileVersion":3,"packages":{"":{"dependencies":{"dep":"^1"}},"node_modules/dep":{"version":"1.0.0"}}}"#;
        fs::write(source.path().join("clients/ts/package-lock.json"), baseline).unwrap();
        fixture_executable(
            tools.path(),
            "npm",
            r#"#!/bin/sh
set -eu
[ "$#" = 5 ]
[ "$1" = update ]
[ "$2" = --prefix ]
[ "$3" = clients/ts ]
[ "$4" = --package-lock-only ]
[ "$5" = --ignore-scripts ]
printf '{"lockfileVersion":3,"packages":{"":{"dependencies":{"dep":"^1"}},"node_modules/dep":{"version":"%s"}}}' "${VERSION:-1.1.0}" > clients/ts/package-lock.json
if [ "${MUTATE_MANIFEST:-}" = 1 ]; then printf '{"changed":true}' > clients/ts/package.json; fi
"#,
        );
        let mut environment = Environment::new();
        crate::set(&mut environment, "PATH", tools.path().display().to_string());
        let mut runner = Runner::new(
            source.path().into(),
            environment,
            std::time::Duration::from_secs(15),
        )
        .unwrap();
        let graph = Graph {
            id: "typescript".into(),
            kind: Kind::Npm,
            manifest: "clients/ts/package.json".into(),
            lock: "clients/ts/package-lock.json".into(),
            seed: None,
            prepare: vec![],
        };
        let result = resolve_graph(&runner, &graph, None).unwrap();
        assert!(result.compatible);
        assert_eq!(result.original.as_deref(), Some(baseline.as_slice()));
        assert_ne!(result.candidate, baseline);
        crate::set(&mut runner.environment, "VERSION", "2.0.0");
        let result = resolve_graph(&runner, &graph, None).unwrap();
        assert!(!result.compatible);
        assert!(!result.breaking.is_empty());
        crate::set(&mut runner.environment, "MUTATE_MANIFEST", "1");
        assert!(resolve_graph(&runner, &graph, None)
            .unwrap_err()
            .to_string()
            .contains("undeclared source"));
    }
}

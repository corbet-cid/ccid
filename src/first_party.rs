//! Require main declarations and one resolved revision per first-party crate.
//!
//! A port of the copied `.github/check-first-party.py` scripts (63 repositories,
//! 25 variants) into one shared check. Every first-party git dependency must
//! declare `branch = "main"` (or the branch of a matching retained entry) and
//! every first-party lock entry must resolve to exactly one full 40-hex
//! revision. The fleet data (`src/fleet.rs`) decides what is first-party; no
//! owner list lives here.
//!
//! `--metadata FILE` adds the same policy on the resolved `cargo metadata`
//! graph (the port of `check-pins.mjs`, the `--metadata` modes of the
//! `check-first-party.py` variants and their `--metadata-issuers` helper).
//!
//! Exit statuses: 0 passes, 1 refusal (`check-first-party: <message> (<path>)`),
//! 2 operational error (`ccid first-party: <error>`).

use crate::{failure, fleet::Fleet, fleet::Origin, Result};
use clap::Args;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn is_hex40(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One deliberately retained original owner on another line.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Retained {
    /// Locked package name.
    pub package: String,
    /// Exact repository spelling as written, with or without `.git`.
    pub repository: String,
    /// The retained line, for example `v01`.
    pub branch: String,
    /// Exact locked version, when the exception is versioned.
    #[serde(default)]
    pub version: Option<String>,
}

/// The `[first_party.graph]` table: data for `--metadata` only.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Graph {
    /// First-party packages that must appear exactly once, with a first-party
    /// source, in every resolved graph (a shared library every repository of the
    /// fleet depends on). A graph without one is refused.
    #[serde(default)]
    pub required: Vec<String>,
}

/// The `[first_party]` table of `.ci/ccid.toml`. Absent table means defaults.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Accept `rev=<fragment>` as a lock query (un-migrated upstreams).
    #[serde(default)]
    pub rev_selectors: bool,
    /// Refuse sources under a fleet alias prefix.
    #[serde(default)]
    pub canonical_only: bool,
    /// A root `Cargo.toml` needs a root `Cargo.lock`.
    #[serde(default)]
    pub require_root_lock: bool,
    /// Every tracked `Cargo.toml` needs a sibling `Cargo.lock`, tracked or not.
    #[serde(default)]
    pub lock_per_manifest: bool,
    /// Extra directory names skipped by the archive walk.
    #[serde(default)]
    pub skip: Vec<String>,
    /// Retained original owners.
    #[serde(default)]
    pub retained: Vec<Retained>,
    /// Data for `--metadata`.
    #[serde(default)]
    pub graph: Graph,
}

impl Config {
    fn validate(&self) -> Result<()> {
        let mut required = BTreeSet::new();
        for name in &self.graph.required {
            if name.is_empty() {
                return Err(failure("first_party graph required name must not be empty"));
            }
            if !required.insert(name.as_str()) {
                return Err(failure(format!(
                    "duplicate first_party graph required name {name}"
                )));
            }
        }
        for name in &self.skip {
            if name.is_empty()
                || name.contains('/')
                || name.contains('\\')
                || *name == "."
                || *name == ".."
            {
                return Err(failure(format!(
                    "first_party skip {name:?} must be a plain directory name"
                )));
            }
        }
        let mut seen = BTreeSet::new();
        for entry in &self.retained {
            if entry.package.is_empty() {
                return Err(failure("first_party retained package must not be empty"));
            }
            if entry.repository.is_empty() {
                return Err(failure("first_party retained repository must not be empty"));
            }
            if entry.branch.is_empty() {
                return Err(failure("first_party retained branch must not be empty"));
            }
            if entry.version.as_deref().is_some_and(str::is_empty) {
                return Err(failure(format!(
                    "first_party retained version for {} must not be empty",
                    entry.package
                )));
            }
            if !seen.insert((entry.package.clone(), entry.repository.clone())) {
                return Err(failure(format!(
                    "duplicate first_party retained entry for {} {}",
                    entry.package, entry.repository
                )));
            }
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
pub struct Options {
    /// Repository root to scan.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// Manifest carrying `[first_party]`.
    #[arg(long, default_value = ".ci/ccid.toml")]
    pub manifest: PathBuf,
    /// Walk the tree instead of `git ls-files`, skipping build directories.
    #[arg(long)]
    pub archive: bool,
    /// Check manifests only, no locks.
    #[arg(long)]
    pub manifest_only: bool,
    /// Print the self-test pointer and exit 0.
    #[arg(long)]
    pub self_test: bool,
    /// Also check the resolved `cargo metadata --format-version 1` graph in FILE
    /// (read relative to the working directory) after the manifest and lock scan.
    #[arg(long)]
    pub metadata: Option<PathBuf>,
}

/// The manifest file contributes only the first-party table; every other table
/// belongs to other subcommands and is ignored here.
#[derive(Debug, Deserialize)]
struct FileManifest {
    #[serde(default)]
    first_party: Option<Config>,
}

/// Split a lock source by plain string operations: strip one `git+`, the
/// fragment is the text after the first `#`, the query the text between the
/// first `?` and the `#`, the repository the text before the `?`.
fn split_source(source: &str) -> (&str, &str, &str) {
    let rest = source.strip_prefix("git+").unwrap_or(source);
    let (before_hash, fragment) = match rest.find('#') {
        Some(index) => (&rest[..index], &rest[index + 1..]),
        None => (rest, ""),
    };
    let (repository, query) = match before_hash.find('?') {
        Some(index) => (&before_hash[..index], &before_hash[index + 1..]),
        None => (before_hash, ""),
    };
    (repository, query, fragment)
}

fn package_name(entry: &toml::Value) -> Option<&str> {
    entry.get("name").and_then(|v| v.as_str())
}

fn package_version(entry: &toml::Value) -> Option<&str> {
    entry.get("version").and_then(|v| v.as_str())
}

fn package_source(entry: &toml::Value) -> &str {
    entry.get("source").and_then(|v| v.as_str()).unwrap_or("")
}

/// The retained entry a locked package matches: its source equals
/// `git+<repository>?branch=<branch>#<40-hex>` exactly and the version agrees when
/// the entry names one.
fn retained_match<'a>(
    config: &'a Config,
    name: &str,
    version: Option<&str>,
    raw: &str,
) -> Option<&'a Retained> {
    if !raw.starts_with("git+") {
        return None;
    }
    let (repository, query, fragment) = split_source(raw);
    config
        .retained
        .iter()
        .filter(|entry| entry.package == name)
        .find(|entry| {
            if let Some(expected) = entry.version.as_deref() {
                if version != Some(expected) {
                    return false;
                }
            }
            repository == entry.repository
                && query == format!("branch={}", entry.branch)
                && is_hex40(fragment)
        })
}

/// Pure lock rule, testable without files. The `Err` message is the text after
/// `check-first-party: `.
pub fn check_lock(
    packages: &[toml::Value],
    fleet: &Fleet,
    config: &Config,
) -> std::result::Result<(), String> {
    let retained_names: BTreeSet<&str> = config
        .retained
        .iter()
        .map(|entry| entry.package.as_str())
        .collect();
    let mut names = BTreeSet::new();
    for package in packages {
        let Some(name) = package_name(package) else {
            continue;
        };
        let source = package_source(package);
        if fleet.classify(source).is_some() || retained_names.contains(name) {
            names.insert(name);
        }
    }
    for name in names {
        let entries: Vec<&toml::Value> = packages
            .iter()
            .filter(|package| package_name(package) == Some(name))
            .collect();
        if entries.len() != 1 {
            return Err(format!("Duplicate first-party crate: {name}"));
        }
        let package = entries[0];
        let raw = package_source(package);
        if config.canonical_only && matches!(fleet.classify(raw), Some(Origin::Alias { .. })) {
            return Err(format!(
                "First-party source must use its canonical owner URL: {raw}"
            ));
        }
        if retained_match(config, name, package_version(package), raw).is_some() {
            continue;
        }
        let Some(_) = fleet.classify(raw) else {
            let (_, _, fragment) = split_source(raw);
            if !is_hex40(fragment) {
                return Err(format!("Missing resolved revision for {name}"));
            }
            return Err(format!("Invalid first-party source selector for {name}"));
        };
        let (_, query, fragment) = split_source(raw);
        if !is_hex40(fragment) {
            return Err(format!("Missing resolved revision for {name}"));
        }
        if query == "branch=main" {
            continue;
        }
        if config.rev_selectors && query == format!("rev={fragment}") {
            continue;
        }
        return Err(format!("Invalid first-party source selector for {name}"));
    }
    Ok(())
}

fn check_manifest_inner(
    value: &toml::Value,
    fleet: &Fleet,
    config: &Config,
    alias: Option<&str>,
) -> std::result::Result<(), String> {
    if let Some(table) = value.as_table() {
        if let Some(git) = table.get("git").and_then(|v| v.as_str()) {
            let package = table
                .get("package")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| alias.unwrap_or(""));
            // Versioned retained entries are lock-only exceptions: manifests
            // still declare main. Unversioned retained entries constrain both.
            let retained = config.retained.iter().find(|entry| {
                entry.version.is_none() && entry.repository == git && entry.package == package
            });
            if let Some(entry) = retained {
                let branch = table.get("branch").and_then(|v| v.as_str());
                if branch != Some(entry.branch.as_str())
                    || table.contains_key("rev")
                    || table.contains_key("tag")
                {
                    return Err(format!(
                        "First-party dependency must declare branch = \"{}\"",
                        entry.branch
                    ));
                }
            } else if fleet.classify(git).is_some() {
                if config.canonical_only
                    && matches!(fleet.classify(git), Some(Origin::Alias { .. }))
                {
                    return Err(format!(
                        "First-party source must use its canonical owner URL: {git}"
                    ));
                }
                let branch = table.get("branch").and_then(|v| v.as_str());
                if branch != Some("main") || table.contains_key("rev") || table.contains_key("tag")
                {
                    return Err("First-party dependency must declare branch = \"main\"".to_owned());
                }
            }
        }
        for (key, item) in table {
            check_manifest_inner(item, fleet, config, Some(key))?;
        }
        return Ok(());
    }
    if let Some(items) = value.as_array() {
        for item in items {
            check_manifest_inner(item, fleet, config, None)?;
        }
    }
    Ok(())
}

/// Pure manifest rule, testable without files. The `Err` message is the text
/// after `check-first-party: `.
pub fn check_manifest(
    value: &toml::Value,
    fleet: &Fleet,
    config: &Config,
) -> std::result::Result<(), String> {
    check_manifest_inner(value, fleet, config, None)
}

/// A refusal message: the text after `check-first-party: `.
type Refusal<T> = std::result::Result<T, String>;

/// Target kinds whose target name is the name a dependent crate uses.
const LIBRARY_KINDS: [&str; 6] = ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];

/// One package of the resolved graph, with only the declarations its selected
/// dependency edges expose (an unused dev, optional or target-specific
/// declaration stays under its owner's policy).
struct Selected<'a> {
    name: &'a str,
    version: Option<&'a str>,
    source: Option<&'a str>,
    declarations: Vec<&'a Value>,
}

/// The exact original source an issuer package resolved to and the declaration it
/// must be reached through; both `None` when the package follows main.
struct Issuer {
    resolved: Option<String>,
    declaration: Option<String>,
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn array<'a>(value: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    value.get(key).and_then(Value::as_array)
}

/// The resolved graph spells every git source `git+<url>`; a bare URL is not one.
fn graph_first_party(fleet: &Fleet, source: Option<&str>) -> bool {
    source.is_some_and(|source| source.starts_with("git+") && fleet.classify(source).is_some())
}

/// A package as the lock rule sees it: name, version and source.
fn lock_entry(name: &str, version: Option<&str>, source: Option<&str>) -> toml::Value {
    let mut table = toml::Table::new();
    table.insert("name".to_owned(), toml::Value::String(name.to_owned()));
    if let Some(version) = version {
        table.insert(
            "version".to_owned(),
            toml::Value::String(version.to_owned()),
        );
    }
    if let Some(source) = source {
        table.insert("source".to_owned(), toml::Value::String(source.to_owned()));
    }
    toml::Value::Table(table)
}

/// Whether `declaration` (a package's dependency record) is the one the edge
/// `edge_name` -> `selected` resolved through: same package, the same name in the
/// dependent's code (the rename, else the library target name), the same kind and
/// target.
fn declares(
    declaration: &Value,
    selected: &Value,
    edge_name: Option<&str>,
    kind: Option<&Value>,
    target: Option<&Value>,
) -> bool {
    if text(declaration, "name") != text(selected, "name") {
        return false;
    }
    let named = match declaration.get("rename") {
        None | Some(Value::Null) => array(selected, "targets").is_some_and(|targets| {
            targets.iter().any(|candidate| {
                array(candidate, "kind").is_some_and(|kinds| {
                    kinds
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|kind| LIBRARY_KINDS.contains(&kind))
                }) && text(candidate, "name")
                    .is_some_and(|name| Some(name.replace('-', "_")).as_deref() == edge_name)
            })
        }),
        Some(rename) => rename
            .as_str()
            .is_some_and(|rename| Some(rename.replace('-', "_")).as_deref() == edge_name),
    };
    named && declaration.get("kind") == kind && declaration.get("target") == target
}

/// The packages of the resolved graph with their selected declarations: package
/// ids unique, nodes = packages, root and workspace members resolved, `deps` and
/// `dependencies` consistent, every selected edge backed by a declaration of the
/// same kind and target, and a first-party declaration whose source is the
/// selected package's source without its fragment.
fn resolved_packages<'a>(metadata: &'a Value, fleet: &Fleet) -> Refusal<Vec<Selected<'a>>> {
    let resolve = metadata
        .get("resolve")
        .filter(|resolve| array(resolve, "nodes").is_some())
        .ok_or("Missing resolved dependency graph")?;
    let nodes = array(resolve, "nodes").ok_or("Missing resolved dependency graph")?;
    let packages = array(metadata, "packages").ok_or("Missing original Cargo metadata graph")?;
    let mut by_id: BTreeMap<&str, &Value> = BTreeMap::new();
    for package in packages {
        let Some(id) = text(package, "id") else {
            return Err("Missing or duplicated package ID".to_owned());
        };
        if by_id.insert(id, package).is_some() {
            return Err("Missing or duplicated package ID".to_owned());
        }
        if text(package, "name").is_none() {
            return Err("Missing package name".to_owned());
        }
        // A source of another JSON type would read as "no source" and leave the package
        // unpoliced: refuse it instead.
        if !matches!(
            package.get("source"),
            None | Some(Value::Null | Value::String(_))
        ) {
            return Err("Invalid package source".to_owned());
        }
    }
    let mut by_node: BTreeMap<&str, &Value> = BTreeMap::new();
    for node in nodes {
        let id = text(node, "id")
            .filter(|id| by_id.contains_key(id) && !by_node.contains_key(id))
            .ok_or("Unknown or duplicated resolved package ID")?;
        by_node.insert(id, node);
    }
    if by_node.len() != by_id.len() {
        return Err("Incomplete resolved package graph".to_owned());
    }
    match resolve.get("root") {
        Some(Value::Null) => {}
        Some(Value::String(root)) if by_node.contains_key(root.as_str()) => {}
        _ => return Err("Unknown resolved root package".to_owned()),
    }
    let members = array(metadata, "workspace_members")
        .filter(|members| {
            members
                .iter()
                .all(|id| id.as_str().is_some_and(|id| by_node.contains_key(id)))
        })
        .ok_or("Unknown or missing resolved workspace members")?;
    if members.is_empty() {
        return Err("Missing actual workspace package".to_owned());
    }
    let mut selected = Vec::with_capacity(nodes.len());
    for node in nodes {
        let package = text(node, "id")
            .and_then(|id| by_id.get(id))
            .copied()
            .ok_or("Unknown or duplicated resolved package ID")?;
        let (Some(deps), Some(dependencies)) = (array(node, "deps"), array(node, "dependencies"))
        else {
            return Err("Missing resolved dependency edges".to_owned());
        };
        let edge_ids: BTreeSet<Option<&str>> = deps.iter().map(|edge| text(edge, "pkg")).collect();
        let dependency_ids: BTreeSet<Option<&str>> =
            dependencies.iter().map(Value::as_str).collect();
        if edge_ids.len() != dependency_ids.len()
            || dependency_ids.iter().any(|id| !edge_ids.contains(id))
        {
            return Err("Inconsistent resolved dependency edges".to_owned());
        }
        let declared =
            array(package, "dependencies").ok_or("Missing package dependency declarations")?;
        let mut declarations = Vec::new();
        for edge in deps {
            let chosen = text(edge, "pkg")
                .filter(|id| by_node.contains_key(id))
                .and_then(|id| by_id.get(id))
                .copied();
            let kinds = array(edge, "dep_kinds").filter(|kinds| !kinds.is_empty());
            let (Some(chosen), Some(kinds)) = (chosen, kinds) else {
                return Err("Unknown or incomplete resolved dependency edge".to_owned());
            };
            let edge_name = text(edge, "name");
            let expected_source =
                text(chosen, "source").map(|source| source.split('#').next().unwrap_or(source));
            for kind in kinds {
                let selected_kind = kind.get("kind");
                let selected_target = kind.get("target");
                let valid_kind = match selected_kind {
                    Some(Value::Null) => true,
                    Some(Value::String(kind)) => kind == "dev" || kind == "build",
                    _ => false,
                };
                let valid_target = matches!(selected_target, Some(Value::Null | Value::String(_)));
                if !valid_kind || !valid_target {
                    return Err("Invalid resolved dependency kind".to_owned());
                }
                let matching: Vec<&Value> = declared
                    .iter()
                    .filter(|declaration| {
                        declares(
                            declaration,
                            chosen,
                            edge_name,
                            selected_kind,
                            selected_target,
                        )
                    })
                    .collect();
                if matching.is_empty() {
                    return Err("Missing selected dependency declaration".to_owned());
                }
                let mismatched = matching.iter().any(|declaration| {
                    let source = text(declaration, "source");
                    (graph_first_party(fleet, source)
                        || graph_first_party(fleet, text(chosen, "source")))
                        && source != expected_source
                });
                if mismatched {
                    return Err("Mismatched selected dependency source".to_owned());
                }
                declarations.extend(matching);
            }
        }
        selected.push(Selected {
            name: text(package, "name").unwrap_or_default(),
            version: text(package, "version"),
            source: text(package, "source"),
            declarations,
        });
    }
    Ok(selected)
}

/// The retained entries in play in this graph. A retained package named by a
/// package or a selected declaration must exist once, pass the lock rule, and
/// every selected declaration of it must be the exact retained declaration or
/// follow main; a package that matched its retained entry exactly is an issuer.
fn retained_issuers<'a>(
    selected: &[Selected<'_>],
    fleet: &Fleet,
    config: &'a Config,
) -> Refusal<BTreeMap<&'a str, Issuer>> {
    let mut issuers = BTreeMap::new();
    let names: BTreeSet<&'a str> = config
        .retained
        .iter()
        .map(|entry| entry.package.as_str())
        .collect();
    for name in names {
        let named = selected.iter().any(|package| {
            package.name == name
                || package
                    .declarations
                    .iter()
                    .any(|declaration| text(declaration, "name") == Some(name))
        });
        if !named {
            continue;
        }
        let packages: Vec<&Selected<'_>> = selected
            .iter()
            .filter(|package| package.name == name)
            .collect();
        let [package] = packages.as_slice() else {
            return Err(format!(
                "Missing, invalid or duplicated first-party dependency: {name}"
            ));
        };
        check_lock(
            &[lock_entry(name, package.version, package.source)],
            fleet,
            config,
        )?;
        let matched = package
            .source
            .filter(|source| retained_match(config, name, package.version, source).is_some());
        let declaration = matched.map(|source| source.split('#').next().unwrap_or(source));
        for owner in selected {
            for dependency in &owner.declarations {
                if text(dependency, "name") != Some(name) {
                    continue;
                }
                let raw = text(dependency, "source").unwrap_or("");
                if Some(raw) == declaration {
                    continue;
                }
                if !graph_first_party(fleet, Some(raw)) || split_source(raw).1 != "branch=main" {
                    return Err(format!("Invalid first-party source selector for {name}"));
                }
            }
        }
        issuers.insert(
            name,
            Issuer {
                resolved: matched.map(str::to_owned),
                declaration: declaration.map(str::to_owned),
            },
        );
    }
    Ok(issuers)
}

/// A first-party package source (`resolved`) or declaration source must follow
/// main, a resolved one with a complete 40-hex revision; an issuer's exact
/// retained source is accepted as is. `rev_selectors` also accepts a revision pin
/// (`rev=<40-hex>`, its resolution the same revision).
fn check_graph_source(
    source: &str,
    resolved: bool,
    name: &str,
    issuers: &BTreeMap<&str, Issuer>,
    fleet: &Fleet,
    config: &Config,
) -> Refusal<()> {
    if let Some(issuer) = issuers.get(name) {
        let exact = if resolved {
            issuer.resolved.as_deref()
        } else {
            issuer.declaration.as_deref()
        };
        if exact == Some(source) {
            return Ok(());
        }
    }
    if config.canonical_only && matches!(fleet.classify(source), Some(Origin::Alias { .. })) {
        return Err(format!(
            "First-party source must use its canonical owner URL: {source}"
        ));
    }
    let (_, query, fragment) = split_source(source);
    let pin = query
        .strip_prefix("rev=")
        .filter(|revision| config.rev_selectors && is_hex40(revision));
    let accepted = query == "branch=main" || pin.is_some();
    let complete =
        !resolved || (is_hex40(fragment) && pin.is_none_or(|revision| revision == fragment));
    if accepted && complete {
        return Ok(());
    }
    Err(if config.rev_selectors {
        format!("Expected main or a matching transitive revision pin: {source}")
    } else {
        format!("Expected main and a complete resolved revision: {source}")
    })
}

/// Pure graph rule, testable without files: the resolved `cargo metadata` graph
/// is consistent, every first-party package and selected declaration follows
/// main (retained issuers excepted) with a complete revision, and every
/// first-party name, with every `[first_party.graph] required` name, resolves
/// exactly once. The `Err` message is the text after `check-first-party: `.
pub fn check_metadata(
    metadata: &Value,
    fleet: &Fleet,
    config: &Config,
) -> std::result::Result<(), String> {
    let present = metadata.get("version") == Some(&Value::from(1))
        && array(metadata, "packages").is_some_and(|packages| !packages.is_empty());
    if !present {
        return Err("Missing original Cargo metadata graph".to_owned());
    }
    let selected = resolved_packages(metadata, fleet)?;
    let issuers = retained_issuers(&selected, fleet, config)?;
    let mut names: Vec<&str> = config.graph.required.iter().map(String::as_str).collect();
    let mut seen: BTreeSet<&str> = names.iter().copied().collect();
    for package in &selected {
        if graph_first_party(fleet, package.source) || issuers.contains_key(package.name) {
            if seen.insert(package.name) {
                names.push(package.name);
            }
            let source = package
                .source
                .ok_or_else(|| format!("Missing resolved revision for {}", package.name))?;
            check_graph_source(source, true, package.name, &issuers, fleet, config)?;
        }
        for declaration in &package.declarations {
            let Some(source) = text(declaration, "source") else {
                continue;
            };
            if graph_first_party(fleet, Some(source)) {
                let name = text(declaration, "name").unwrap_or_default();
                check_graph_source(source, false, name, &issuers, fleet, config)?;
            }
        }
    }
    for name in names {
        let named: Vec<&Selected<'_>> = selected
            .iter()
            .filter(|package| package.name == name)
            .collect();
        let exact = issuers
            .get(name)
            .and_then(|issuer| issuer.resolved.as_deref());
        let valid = matches!(named.as_slice(), [package]
            if graph_first_party(fleet, package.source)
                || (package.source.is_some() && package.source == exact));
        if !valid {
            return Err(format!(
                "Missing, invalid or duplicated first-party dependency: {name}"
            ));
        }
    }
    Ok(())
}

fn git_files(root: &Path) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
        .arg("ls-files")
        .arg("-z")
        .current_dir(root)
        .output()
        .map_err(|error| failure(format!("cannot run git ls-files: {error}")))?;
    if !output.status.success() {
        return Err(failure(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|error| failure(format!("git ls-files is not UTF-8: {error}")))?;
    let mut files = Vec::new();
    for name in text.split('\0') {
        if name.is_empty() {
            continue;
        }
        let path = PathBuf::from(name);
        if path
            .file_name()
            .is_some_and(|n| n == "Cargo.toml" || n == "Cargo.lock")
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn walk_files(root: &Path, skip: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        let absolute = root.join(&relative);
        let entries = fs::read_dir(&absolute)
            .map_err(|error| failure(format!("cannot read {}: {error}", absolute.display())))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| failure(format!("cannot read {}: {error}", absolute.display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = entry
                .file_type()
                .map_err(|error| failure(format!("cannot stat {}: {error}", absolute.display())))?;
            if file_type.is_dir() {
                if name == ".git"
                    || name == "target"
                    || name == "node_modules"
                    || skip.contains(&name)
                {
                    continue;
                }
                stack.push(relative.join(&name));
            } else if file_type.is_file() && (name == "Cargo.toml" || name == "Cargo.lock") {
                out.push(relative.join(&name));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn read_toml(root: &Path, path: &Path) -> Result<toml::Value> {
    let bytes = fs::read(root.join(path))
        .map_err(|error| failure(format!("cannot read {}: {error}", path.display())))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| failure(format!("cannot parse {}: {error}", path.display())))?;
    toml::from_str(text)
        .map_err(|error| failure(format!("cannot parse {}: {error}", path.display())))
}

/// Run the first-party check. `Ok(code)` is the process exit code; refusals
/// print `check-first-party: <message> (<path>)` and return `Ok(1)`.
pub fn run(options: &Options) -> Result<u8> {
    if options.self_test {
        println!("first-party: the self-test is ccid's own test suite");
        return Ok(0);
    }
    if options.metadata.is_some() && options.manifest_only {
        return Err(failure("--metadata and --manifest-only are exclusive"));
    }
    let environment: crate::Environment = std::env::vars_os().collect();
    let (fleet, _) = crate::fleet::load(&environment)?;
    crate::fleet::require_keyed(&environment, &fleet)?;
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
    let config = parsed.first_party.unwrap_or_default();
    config.validate()?;
    // Unreadable or unparseable graph input is an operational error whatever the
    // scan finds, so it is read before the scan.
    let graph = options.metadata.as_deref().map(read_metadata).transpose()?;
    let mut files = if !options.archive && root.join(".git").exists() {
        git_files(&root)?
    } else {
        walk_files(&root, &config.skip)?
    };
    if config.require_root_lock
        && (!files.iter().any(|p| p == Path::new("Cargo.toml"))
            || !files.iter().any(|p| p == Path::new("Cargo.lock")))
    {
        let missing = if !files.iter().any(|p| p == Path::new("Cargo.toml")) {
            "Cargo.toml"
        } else {
            "Cargo.lock"
        };
        eprintln!("check-first-party: Missing committed root manifest or lock ({missing})");
        return Ok(1);
    }
    if config.lock_per_manifest {
        let mut extra = Vec::new();
        for manifest in files
            .iter()
            .filter(|p| p.file_name().is_some_and(|n| n == "Cargo.toml"))
        {
            let lock = manifest
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.join("Cargo.lock"))
                .unwrap_or_else(|| PathBuf::from("Cargo.lock"));
            if !root.join(&lock).is_file() {
                eprintln!(
                    "check-first-party: Missing resolved snapshot: {} ({})",
                    lock.display(),
                    lock.display()
                );
                return Ok(1);
            }
            if !files.contains(&lock) {
                extra.push(lock);
            }
        }
        files.extend(extra);
        files.sort();
    }
    // One pass in sorted path order, manifests and locks interleaved as the
    // scripts walked them, so the first refusal is the same one.
    files.dedup();
    for path in &files {
        let manifest = path.file_name().is_some_and(|n| n == "Cargo.toml");
        if !manifest && options.manifest_only {
            continue;
        }
        let value = read_toml(&root, path)?;
        let verdict = if manifest {
            check_manifest(&value, &fleet, &config)
        } else {
            let packages = match value.get("package") {
                None => Vec::new(),
                Some(toml::Value::Array(items)) => items.clone(),
                Some(_) => {
                    return Err(failure(format!(
                        "cannot parse {}: package is not an array",
                        path.display()
                    )));
                }
            };
            check_lock(&packages, &fleet, &config)
        };
        if let Err(message) = verdict {
            eprintln!("check-first-party: {message} ({})", path.display());
            return Ok(1);
        }
    }
    println!("First-party declarations follow main; locked crate revisions are unique");
    if let (Some(graph), Some(path)) = (&graph, &options.metadata) {
        if let Err(message) = check_metadata(graph, &fleet, &config) {
            eprintln!("check-first-party: {message} ({})", path.display());
            return Ok(1);
        }
        println!("First-party dependencies resolve once each to a complete source revision");
    }
    Ok(0)
}

/// The resolved graph: a JSON object, as `cargo metadata --format-version 1` prints it.
fn read_metadata(path: &Path) -> Result<Value> {
    let bytes = fs::read(path)
        .map_err(|error| failure(format!("cannot read {}: {error}", path.display())))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| failure(format!("cannot parse {}: {error}", path.display())))?;
    if !value.is_object() {
        return Err(failure(format!(
            "cannot parse {}: not a JSON object",
            path.display()
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests;

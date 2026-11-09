//! Result caching around selected checks through moon (the cmnp adapter), with the shared
//! verdict store in front of it.
//!
//! Each selected check becomes one moon task whose command is the ordinary
//! `ccid check` for that check. ccid owns the manifest and check definitions;
//! cmnp owns how moon runs them (workspace/task generation, tool identity,
//! remote cache, receipts).
//!
//! Before moon runs, the content-keyed verdict store (`docs/verdicts.md`) is
//! asked whether this exact check already passed on this exact content. A hit
//! skips the task entirely, which is what lets CI reuse a land's work. A miss
//! runs the ordinary cached transaction and, on success, records a verdict for
//! the next producer. `--plan` prints the verdict and the bypass reason without
//! running anything.
use crate::{
    cached_verdict, checks, event, failure, load_manifest, select_checks, value, Environment,
    Result, Runner, SOURCE_REVISION,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}

/// Run the selected checks through moon so unchanged inputs reuse earlier results.
pub fn run_cached(
    repo: &Path,
    manifest: &Path,
    selectors: &[String],
    plan: bool,
    force: bool,
) -> Result<()> {
    run_cached_with_environment(
        repo,
        manifest,
        selectors,
        plan,
        force,
        std::env::vars_os().collect(),
    )
}

/// The manifest check as cmnp's executor owns it (the two declarations must serialize alike).
pub(crate) fn executor_check(check: &crate::Check) -> cmnp::executor::Check {
    cmnp::executor::Check {
        kind: check.kind.clone(),
        actions: check.actions.clone(),
        toolchain: check.toolchain.clone(),
        workspace: check.workspace,
        all_features: check.all_features,
        all_targets: check.all_targets,
        release: check.release,
        features: check.features.clone(),
        packages: check.packages.clone(),
        exclude: check.exclude.clone(),
        test_runner: check.test_runner.clone(),
        mode: check.mode.clone(),
        expected_checks: check.expected_checks.clone(),
        checks: check.checks.clone(),
        manager: check.manager.clone(),
        scripts: check.scripts.clone(),
        install: check.install,
        commands: check.commands.clone(),
        cache_outputs: check.cache_outputs.clone(),
        cache_tools: check.cache_tools.clone(),
        cache_env: check.cache_env.clone(),
        cache_commit: check.cache_commit,
        cache_inputs: check.cache_inputs.clone(),
        cache: check.cache,
        cache_pure: check.cache_pure,
    }
}

/// `ccid cached --plan`: one line per selected check with the verdict and the reason it would
/// run uncached. Executes nothing.
fn plan(
    root: &Path,
    manifest_checks: &BTreeMap<String, crate::Check>,
    executor_checks: &BTreeMap<String, cmnp::executor::Check>,
    selected: &[String],
    environment: &Environment,
) -> Result<()> {
    let keyable: Vec<String> = selected
        .iter()
        .filter(|name| {
            cached_verdict::plan_bypass(&manifest_checks[*name], &executor_checks[*name], root)
                .is_none()
        })
        .cloned()
        .collect();
    // The key names the resolved dependency commits, so a plan resolves exactly like a run
    // (Cargo only, no check) before it can answer hit or miss.
    let prepared = cached_verdict::resolve_git_dependencies(root, environment)
        .and_then(|_| cached_verdict::commit(root, None, environment))
        .map(|commit| {
            cached_verdict::prepare_selection(root, manifest_checks, &keyable, environment, &commit)
        })
        .unwrap_or_default();
    for name in selected {
        if let Some(bypass) =
            cached_verdict::plan_bypass(&manifest_checks[name], &executor_checks[name], root)
        {
            event(json!({
                "event": "cache-plan",
                "check": name,
                "cacheable": false,
                "bypass": bypass,
                "verdict": "uncached",
            }));
            continue;
        }
        let entry = prepared.iter().find(|entry| &entry.check == name);
        let (status, reason) = match entry {
            Some(entry) => {
                let lookup = cached_verdict::lookup_ready(entry, environment);
                (lookup.status.as_str().to_owned(), lookup.reason)
            }
            None => (
                "unavailable".to_owned(),
                Some("the verdict key cannot be computed here".to_owned()),
            ),
        };
        event(json!({
            "event": "cache-plan",
            "check": name,
            "cacheable": true,
            "bypass": null,
            "verdict": status,
            "reason": reason,
        }));
        if let Some(entry) = entry {
            cached_verdict::plan_event(entry, environment);
        }
    }
    Ok(())
}

/// The receipt line for a verdict hit. `run-cached.sh` asserts that the declared receipt exists
/// after `ccid cached`; a hit must leave the same evidence as a computed run.
fn write_hit_receipt(
    root: &Path,
    name: &str,
    entry: &crate::cached_verdict::Prepared,
) -> Result<()> {
    let directory = root.join(".ccid/results");
    std::fs::create_dir_all(&directory)?;
    let receipt = json!({
        "event": "verdict-hit",
        "check": name,
        "key": entry.key,
        "result": "success",
    });
    std::fs::write(
        directory.join(format!("{name}.jsonl")),
        format!("{receipt}\n"),
    )?;
    Ok(())
}

/// Merge the verdict hits in front of the ordinary cache measurements, so both `ccid cached`
/// and a nested `ccid check` see one complete selection.
fn write_metrics(
    root: &Path,
    hits: &[serde_json::Value],
    bypassed: &BTreeSet<String>,
) -> Result<()> {
    let path = root.join(".ccid/cache-metrics.json");
    let mut checks: Vec<serde_json::Value> = hits.to_vec();
    let mut all_bypassed: BTreeSet<String> = bypassed.clone();
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(metrics) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            checks.extend(metrics["checks"].as_array().into_iter().flatten().cloned());
            for item in metrics["bypassed"].as_array().into_iter().flatten() {
                if let Some(name) = item["check"].as_str() {
                    all_bypassed.insert(name.to_owned());
                }
            }
        }
    }
    let requests = checks.len();
    let hit_count = checks.iter().filter(|entry| entry["computed"] == 0).count();
    let bypassed: Vec<serde_json::Value> = all_bypassed
        .into_iter()
        .map(|check| json!({"check": check}))
        .collect();
    let summary = json!({
        "event": "cache-run",
        "checks": checks,
        "bypassed": bypassed,
        "result": {
            "hits": hit_count,
            "requests": requests,
            "hit_rate": if requests == 0 { None } else { Some(hit_count as f64 / requests as f64) },
        },
        "compile": null,
        "nix_eval": null,
        "fetch": null,
    });
    std::fs::create_dir_all(root.join(".ccid"))?;
    std::fs::write(&path, serde_json::to_vec_pretty(&summary)?)?;
    event(summary);
    Ok(())
}

pub(crate) fn run_cached_with_environment(
    repo: &Path,
    manifest: &Path,
    selectors: &[String],
    plan_only: bool,
    force: bool,
    mut environment: Environment,
) -> Result<()> {
    // Adapters legitimately invoke the verified runtime by absolute path.
    // Re-enter that same executable for cache misses even if PATH has no ccid
    // (or an older one). Keep the task command logical; tool bytes are hashed.
    let current = std::env::current_exe()?;
    if current
        .file_name()
        .is_some_and(|name| name == "ccid" || name == "ccid.exe")
    {
        let mut paths = vec![current
            .parent()
            .ok_or_else(|| failure("Missing runtime directory"))?
            .to_owned()];
        if let Some(path) = environment.get(std::ffi::OsStr::new("PATH")) {
            paths.extend(std::env::split_paths(path));
        }
        environment.insert("PATH".into(), std::env::join_paths(paths)?);
    }
    crate::safe_relative(manifest)?;
    let root = repo.canonicalize()?;
    let (parsed, bytes) = load_manifest(&root, manifest)?;
    let selected = select_checks(&parsed, selectors)?;
    let mut executor_checks = BTreeMap::new();
    for name in &selected {
        executor_checks.insert(name.clone(), executor_check(&parsed.checks[name]));
    }
    let project = cmnp::executor::project_id(&parsed.project);
    let remote = cmnp::executor::remote_cache(&environment)?;
    let manifest_arg = manifest
        .to_str()
        .ok_or_else(|| failure("Manifest path must be valid UTF-8"))?;
    event(
        json!({"event":"cached-plan","project":project,"checks":selected,
        "remote_cache":remote.is_some(),"force":force,
        "source_commit":value(&environment,"CI_COMMIT_SHA"),
        "manifest_sha256":format!("{:x}",Sha256::digest(&bytes)),"tool_revision":SOURCE_REVISION}),
    );
    if plan_only {
        return plan(
            &root,
            &parsed.checks,
            &executor_checks,
            &selected,
            &environment,
        );
    }
    // A check that declares an opt-out, or that the purity scan finds reading run metadata or
    // live state, has no content key by construction: it always runs uncached.
    let verdict_names: Vec<String> = selected
        .iter()
        .filter(|name| {
            cached_verdict::plan_bypass(&parsed.checks[*name], &executor_checks[*name], &root)
                .is_none()
        })
        .cloned()
        .collect();
    let use_verdicts =
        value(&environment, "CCID_RESULT_CACHE").is_some() && !verdict_names.is_empty();
    let producer = if use_verdicts {
        Some(cached_verdict::producer(None, &environment)?)
    } else {
        None
    };
    let mut prepared = Vec::new();
    let mut hits = BTreeSet::new();
    let mut hit_metrics = Vec::new();
    if use_verdicts {
        // Resolve before keying: the key names the commits the check will build. Anything that
        // prevents an exact key disables the verdict layer for this run, never guesses.
        match cached_verdict::resolve_git_dependencies(&root, &environment).and_then(|_| {
            let commit = cached_verdict::commit(&root, None, &environment)?;
            Ok(cached_verdict::prepare_selection(
                &root,
                &parsed.checks,
                &verdict_names,
                &environment,
                &commit,
            ))
        }) {
            Ok(entries) => {
                prepared = entries;
                if !force {
                    for entry in &prepared {
                        let lookup = cached_verdict::lookup(entry, &environment);
                        if lookup.status == cached_verdict::Status::Hit {
                            // The verdict answers for the check; its declared outputs are the
                            // evidence later steps archive. Restore them byte-for-byte first, or
                            // treat the lookup as a miss so a real run produces them.
                            match cached_verdict::restore_outputs(entry, &environment) {
                                Ok(()) => {
                                    event(json!({
                                        "event": "cache-check",
                                        "check": entry.check,
                                        "key": entry.key,
                                        "status": "cached",
                                        "computed": 0,
                                        "verdict": "hit",
                                    }));
                                    write_hit_receipt(&root, &entry.check, entry)?;
                                    hit_metrics.push(json!({
                                        "event": "cache-check",
                                        "check": entry.check,
                                        "key": entry.key,
                                        "status": "cached",
                                        "computed": 0,
                                    }));
                                    hits.insert(entry.check.clone());
                                }
                                Err(error) => event(json!({
                                    "event": "verdict-inspect",
                                    "check": entry.check,
                                    "key": entry.key,
                                    "status": "miss",
                                    "reason": format!(
                                        "the verdict's stored outputs are unavailable: {error}"
                                    ),
                                })),
                            }
                        } else if lookup.status != cached_verdict::Status::Miss {
                            event(json!({
                                "event": "verdict-inspect",
                                "check": entry.check,
                                "key": entry.key,
                                "status": lookup.status.as_str(),
                                "reason": lookup.reason,
                            }));
                        }
                    }
                }
            }
            Err(error) => event(json!({
                "event": "verdict-disabled",
                "reason": error.to_string(),
            })),
        }
    }
    let misses: Vec<String> = selected
        .iter()
        .filter(|name| !hits.contains(*name))
        .cloned()
        .collect();
    if misses.is_empty() {
        // Every selected check answered from the shared store: no moon, no build.
        write_metrics(&root, &hit_metrics, &BTreeSet::new())?;
        return Ok(());
    }
    // Ineligible checks are not sent to the cached executor; run them as ordinary checks so a
    // broader selection still behaves like `ccid check`. `run_checks` degrades them itself.
    let (runnable, direct): (Vec<String>, Vec<String>) = misses
        .into_iter()
        .partition(|name| checks::cache_eligible(&parsed.checks[name]));
    for name in &direct {
        crate::run_checks(&root, manifest, std::slice::from_ref(name), false)?;
    }
    if runnable.is_empty() {
        if use_verdicts {
            write_metrics(&root, &hit_metrics, &BTreeSet::new())?;
        }
        return Ok(());
    }
    let runner = Runner::new(root.clone(), environment.clone(), Duration::from_secs(120))?;
    let path_ccid = runner.run(&argv(&["ccid", "source-revision"]), true)?;
    if path_ccid != SOURCE_REVISION {
        return Err(failure(format!(
            "The ccid on PATH ({path_ccid}) must be this verified revision ({SOURCE_REVISION})"
        )));
    }
    // The ordinary result cache still owns the check execution and its attempt ledger; the
    // verdict store only answers before and records after it.
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let outcome = cmnp::executor::execute(&cmnp::executor::Request {
        repo: root.clone(),
        project,
        manifest_arg: manifest_arg.to_owned(),
        checks: executor_checks,
        selected: runnable.clone(),
        plan: false,
        force,
        tool: "ccid".into(),
        tool_revision: verdict_epoch_label(),
        environment: environment
            .clone()
            .into_iter()
            .collect::<cmnp::Environment>(),
    });
    let bypassed = cached_verdict::bypassed(&root);
    if outcome.is_ok() {
        if let Some(producer) = &producer {
            cached_verdict::record_success(
                &prepared,
                &bypassed,
                producer,
                started_at,
                &environment,
            )?;
        }
    }
    if use_verdicts {
        write_metrics(&root, &hit_metrics, &bypassed)?;
    }
    outcome
}

/// The version of what a cached verdict MEANS. Cached results are keyed by this number, not by the binary revision: a new
/// ccid build that checks the same things the same way reuses every existing verdict, so a release does not invalidate the
/// whole result cache. Raise it ONLY when the semantics of a check change (what a check runs, or what counts as passing),
/// so older verdicts can no longer answer for the new meaning.
pub const VERDICT_EPOCH: u32 = 1;

/// What the result cache binds into every verdict key (cmnp's `tool_revision`, the caller's version of the semantics).
pub(crate) fn verdict_epoch_label() -> String {
    format!("verdict-epoch-{VERDICT_EPOCH}")
}

#[cfg(test)]
mod epoch_tests {
    use super::*;

    #[test]
    fn the_verdict_key_binds_the_epoch_and_never_the_build_revision() {
        let label = verdict_epoch_label();
        assert!(label.ends_with(&VERDICT_EPOCH.to_string()), "{label}");
        assert!(!label.contains(SOURCE_REVISION), "{label}");
    }
}

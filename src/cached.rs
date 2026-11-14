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
    cached_verdict::{
        self,
        read_sets::{Marker, SetProof, Slot, State},
    },
    checks, event, failure, load_manifest, select_checks, value, Environment, Result, Runner,
    SOURCE_REVISION,
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
    require_hit: bool,
) -> Result<()> {
    run_cached_with_environment(
        repo,
        manifest,
        selectors,
        plan,
        force,
        require_hit,
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
    resolved: Option<&BTreeMap<String, String>>,
) -> Result<bool> {
    let mut all_hit = true;
    let keyable: Vec<String> = selected
        .iter()
        .filter(|name| {
            cached_verdict::plan_bypass(&manifest_checks[*name], &executor_checks[*name], root)
                .is_none()
        })
        .cloned()
        .collect();
    // The key names the resolved dependency commits, so the caller floated exactly like a run
    // (Cargo only, in a copy, no check) before a plan can answer hit or miss.
    let prepared = cached_verdict::commit(root, None, environment)
        .map(|commit| {
            cached_verdict::prepare_selection(
                root,
                manifest_checks,
                &keyable,
                environment,
                &commit,
                resolved,
            )
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
            all_hit = false;
            continue;
        }
        let entry = prepared.iter().find(|entry| &entry.check == name);
        let (status, reason) = match entry {
            Some(entry) => {
                let (lookup, _) = cached_verdict::lookup_chain(entry, environment, true);
                (lookup.status.as_str().to_owned(), lookup.reason)
            }
            None => (
                "unavailable".to_owned(),
                Some("the verdict key cannot be computed here".to_owned()),
            ),
        };
        all_hit &= status == "hit";
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
    Ok(all_hit)
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
    require_hit: bool,
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
    let (mut parsed, bytes) = load_manifest(&root, manifest)?;
    let selected = select_checks(&parsed, selectors)?;
    // Before any verdict key or moon task is computed: the digest of the fleet data a
    // `fleet = true` check will read (declared in its cache_env by the manifest load).
    crate::fleet::bind_selection(&parsed.checks, &selected, &mut environment, false)?;
    // One float for the whole selection, in a copy (the checkout is never written), or the one
    // an enclosing `ccid check` handed over. Its digest becomes declared environment of every
    // selected check, so the verdict store and the moon tasks key the locks the checks build;
    // the moon tasks receive the locks and build exactly them.
    let float = crate::float::establish(&root, &mut environment, false)?;
    if float.is_some() {
        crate::float::bind(&mut parsed.checks, &selected);
    }
    let resolved = float.as_ref().map(|handover| &handover.resolution.commits);
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
        let all_hit = plan(
            &root,
            &parsed.checks,
            &executor_checks,
            &selected,
            &environment,
            resolved,
        )?;
        if require_hit && !all_hit {
            return Err(failure(
                "--require-hit: not every selected check has a verdict for this tree",
            ));
        }
        return Ok(());
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
    if !use_verdicts && !verdict_names.is_empty() {
        // Without a store no key is computed, so a key that cannot be built (a tool that cannot
        // be identified) shows only where the store exists. Say so: a run here is not evidence
        // that the same run on CI keys cleanly.
        event(json!({
            "event": "verdict-disabled",
            "reason": "CCID_RESULT_CACHE is not provisioned: no verdict key is computed in this run",
            "checks": verdict_names,
        }));
    }
    let producer = if use_verdicts {
        Some(cached_verdict::producer(None, &environment)?)
    } else {
        None
    };
    let mut prepared = Vec::new();
    let mut hits = BTreeSet::new();
    let mut hit_metrics = Vec::new();
    if use_verdicts {
        // The float above already resolved: the key names the commits the check will build.
        // Anything that prevents an exact key disables the verdict layer for this run, never
        // guesses.
        match cached_verdict::commit(&root, None, &environment).map(|commit| {
            cached_verdict::prepare_selection(
                &root,
                &parsed.checks,
                &verdict_names,
                &environment,
                &commit,
                resolved,
            )
        }) {
            Ok(entries) => {
                prepared = entries;
                if !force {
                    for slot in prepared.iter_mut() {
                        // A narrowed key is asked first and the whole-tree key second: a run that
                        // could not prove the read-set stored its pass under the latter.
                        let (lookup, answered) =
                            cached_verdict::lookup_chain(slot, &environment, false);
                        let entry = answered.as_ref().unwrap_or(&*slot);
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
    // They build this run's float too (the hand-over is in `environment`), never a second one.
    for name in &direct {
        crate::run_checks_in(&root, manifest, std::slice::from_ref(name), &environment)?;
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
    // A narrowed key is a claim made from an earlier measurement. The run that stores a verdict
    // under it proves the claim on that run: the check executes in a copy that holds its
    // read-set and nothing else (`docs/verdicts.md#narrowed-keys`). A check that cannot be
    // proven runs on the whole tree below, and its pass is stored under the whole-tree key.
    // A read-set the store already knows to be stale is not proven again: it is measured again
    // by one traced whole-tree run (`docs/verdicts.md#re-measuring-a-stale-read-set`).
    let proofs = prove_runnable(
        &root,
        &mut prepared,
        &runnable,
        &parsed.checks,
        &executor_checks,
        &project,
        manifest_arg,
        &environment,
    );
    let executed = proofs.executed;
    let mut remaining = proofs.remaining;
    hit_metrics.extend(proofs.metrics);
    for (name, marker) in proofs.measure {
        let Some(entry) = prepared.iter().find(|entry| entry.check == name) else {
            remaining.push(name);
            continue;
        };
        match measure_whole_tree(
            &root,
            entry,
            &parsed.checks[&name],
            &marker,
            manifest_arg,
            &environment,
            producer.as_ref(),
        ) {
            Measure::Unavailable(reason) => {
                event(json!({
                    "event": "read-set-measure",
                    "check": name,
                    "status": "unavailable",
                    "reason": reason,
                }));
                remaining.push(name);
            }
            // The traced run is the check's run: a failure is the check's failure, as for any
            // check that runs outside the cached executor.
            Measure::Ran(Err(error)) => return Err(error),
            Measure::Ran(Ok(())) => hit_metrics.push(json!({
                "event": "cache-check",
                "check": entry.check,
                "key": entry.key,
                "status": "computed",
                "computed": 1,
            })),
        }
    }
    let executor_ran = !remaining.is_empty();
    let outcome = if remaining.is_empty() {
        Ok(())
    } else {
        cmnp::executor::execute(&cmnp::executor::Request {
            repo: root.clone(),
            project,
            manifest_arg: manifest_arg.to_owned(),
            checks: executor_checks,
            selected: remaining,
            plan: false,
            force,
            tool: "ccid".into(),
            tool_revision: verdict_epoch_label(),
            environment: environment
                .clone()
                .into_iter()
                .collect::<cmnp::Environment>(),
        })
    };
    // Only the executor's run decides what ran uncached: without one, the metrics file in the
    // checkout is a previous invocation's.
    let bypassed = if executor_ran {
        cached_verdict::bypassed(&root)
    } else {
        BTreeSet::new()
    };
    if outcome.is_ok() {
        if let Some(producer) = &producer {
            cached_verdict::record_success(
                &prepared,
                &bypassed,
                &executed,
                producer,
                started_at,
                &environment,
            )?;
            // The whole-tree runs passed: what the failed proofs found is now known to be
            // staleness, and not a red check.
            apply_follow_ups(&proofs.follow_up, producer);
        }
    }
    if use_verdicts {
        write_metrics(&root, &hit_metrics, &bypassed)?;
    }
    outcome
}

/// What the proving runs of one invocation decided.
pub(crate) struct Proofs {
    /// Every narrowed check that was asked to run: `true` when its pass proved the read-set.
    pub executed: BTreeMap<String, bool>,
    /// The checks still to run on the whole tree: every check that has no narrowed key, and
    /// every narrowed one that could not be proven.
    pub remaining: Vec<String>,
    /// The checks whose read-set is known to be stale and not yet measured again: each runs once
    /// on the whole tree under the tracer instead of in the executor, with the marker that says so.
    pub measure: Vec<(String, Box<Marker>)>,
    /// The metrics lines of the checks the proving runs computed.
    pub metrics: Vec<serde_json::Value>,
    /// What to put in the store once the whole-tree runs have passed.
    pub follow_up: Vec<FollowUp>,
}

/// A change of the store's read-set state that waits for the whole-tree run of the same check:
/// only a pass there shows that the failed proof meant a stale read-set and not a red check.
pub(crate) enum FollowUp {
    /// The read-set with `inputs` failed its proof and the whole-tree run passed.
    Stale {
        slot: Slot,
        inputs: String,
        base: String,
        since: String,
    },
    /// The traced read-set failed its proof as well.
    Refuse { slot: Slot, marker: Box<Marker> },
}

/// How one proving run ended.
enum Proof {
    /// The check passed in the proving copy.
    Proven,
    /// The check failed there: the read-set is stale, or the check is red (the whole-tree run
    /// tells which).
    Failed,
    /// No proof could be made (no copy, outputs that could not be moved): says nothing about the
    /// read-set.
    Unavailable,
}

/// Run each narrowed check of `runnable` that is not known to be stale in its proving copy, in
/// order (see [`prove_narrowed`]). A key whose read-set is stale is not proven again: it waits for
/// its traced measurement; the measurement that arrived is proven instead of the key's own.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prove_runnable(
    root: &Path,
    prepared: &mut [cached_verdict::Prepared],
    runnable: &[String],
    declared: &BTreeMap<String, crate::Check>,
    executor_checks: &BTreeMap<String, cmnp::executor::Check>,
    project: &str,
    manifest_arg: &str,
    environment: &Environment,
) -> Proofs {
    let mut proofs = Proofs {
        executed: BTreeMap::new(),
        remaining: Vec::new(),
        measure: Vec::new(),
        metrics: Vec::new(),
        follow_up: Vec::new(),
    };
    for name in runnable {
        let Some(index) = prepared
            .iter()
            .position(|entry| &entry.check == name && entry.is_narrowed())
        else {
            proofs.remaining.push(name.clone());
            continue;
        };
        let entry = prepared[index].clone();
        let prove = |keyed: &cached_verdict::Prepared| {
            prove_narrowed(
                root,
                keyed,
                executor_checks,
                project,
                manifest_arg,
                environment,
            )
        };
        let computed = |keyed: &cached_verdict::Prepared| {
            json!({
                "event": "cache-check",
                "check": keyed.check,
                "key": keyed.key,
                "status": "computed",
                "computed": 1,
            })
        };
        let state = match (entry.slot(), entry.read_set_inputs()) {
            (Some(slot), Some(inputs)) => slot.state(inputs, environment),
            _ => State::Open,
        };
        // Every narrowed check that runs is recorded: under the narrowed key when its read-set
        // was proven, under the whole-tree key otherwise.
        proofs.executed.insert(name.clone(), false);
        match state {
            State::Open => match prove(&entry) {
                Proof::Proven => {
                    proofs.executed.insert(name.clone(), true);
                    proofs.metrics.push(computed(&entry));
                }
                Proof::Failed => {
                    proofs.remaining.push(name.clone());
                    if let (Some(slot), Some(inputs), Some(base)) =
                        (entry.slot(), entry.read_set_inputs(), entry.lineage())
                    {
                        proofs.follow_up.push(FollowUp::Stale {
                            slot,
                            inputs: inputs.to_owned(),
                            base: base.to_owned(),
                            since: entry.commit.clone(),
                        });
                    }
                }
                Proof::Unavailable => proofs.remaining.push(name.clone()),
            },
            State::Refused => {
                event(json!({
                    "event": "read-set-skip",
                    "check": name,
                    "state": "refused",
                    "reason": "the read-set is stale and its automatic measurement did not prove; measure the check by hand",
                }));
                proofs.remaining.push(name.clone());
            }
            State::Stale {
                marker,
                candidate: None,
            } => {
                event(json!({
                    "event": "read-set-skip",
                    "check": name,
                    "state": "stale",
                    "since": marker.since,
                    "reason": "the read-set is stale: no proving attempt, one traced whole-tree run measures it again",
                }));
                proofs.measure.push((name.clone(), marker));
            }
            State::Stale {
                marker,
                candidate: Some(candidate),
            } => {
                let Some(slot) = entry.slot() else {
                    proofs.remaining.push(name.clone());
                    continue;
                };
                let mut measured = declared[name].clone();
                measured.cache_inputs = Some(candidate.cache_inputs.clone());
                measured.cache_inputs_measured = Some(candidate.measured.clone());
                match entry.rekeyed(&measured) {
                    Err(reason) => {
                        // The measurement no longer describes this tree (its layout moved on):
                        // it cannot be proven, so it is measured again.
                        event(json!({
                            "event": "read-set-skip",
                            "check": name,
                            "state": "stale",
                            "reason": format!("the measured candidate does not narrow this tree: {reason}"),
                        }));
                        if let Err(error) = slot.drop_candidate() {
                            event(json!({
                                "event": "verdict-store-error",
                                "check": name,
                                "reason": error.to_string(),
                            }));
                        }
                        proofs.measure.push((name.clone(), marker));
                    }
                    Ok(proving) => match prove(&proving) {
                        Proof::Proven => {
                            let (files, of) = proving.size().unwrap_or((0, 0));
                            let proof = SetProof {
                                method: crate::narrowing::PROOF.to_owned(),
                                files,
                                of,
                                commit: proving.commit.clone(),
                            };
                            if let Err(error) = slot.promote(&candidate, proof) {
                                event(json!({
                                    "event": "verdict-store-error",
                                    "check": name,
                                    "reason": error.to_string(),
                                }));
                            }
                            proofs.executed.insert(name.clone(), true);
                            proofs.metrics.push(computed(&proving));
                            prepared[index] = proving;
                        }
                        Proof::Failed => {
                            proofs.remaining.push(name.clone());
                            proofs.follow_up.push(FollowUp::Refuse { slot, marker });
                        }
                        Proof::Unavailable => proofs.remaining.push(name.clone()),
                    },
                }
            }
        }
    }
    proofs
}

/// Record what the failed proofs meant, now that the whole-tree runs passed.
fn apply_follow_ups(follow_up: &[FollowUp], producer: &cached_verdict::Producer) {
    for change in follow_up {
        let (check, outcome) = match change {
            FollowUp::Stale {
                slot,
                inputs,
                base,
                since,
            } => (slot.check(), slot.mark_stale(inputs, base, since, producer)),
            FollowUp::Refuse { slot, marker } => (slot.check(), slot.refuse(marker, producer)),
        };
        if let Err(error) = outcome {
            event(json!({
                "event": "verdict-store-error",
                "check": check,
                "reason": error.to_string(),
            }));
        }
    }
}

/// Run one narrowed check in a copy of the tree that holds its read-set and nothing else
/// (`Prepared::proving_copy`). `Proven`: it passed there, so the read-set suffices for this
/// tree; its declared outputs and receipt are now in the checkout, where the store archives them
/// from. `Failed`: it could not be proven, and the caller runs the check on the whole tree. A
/// failure in the copy is not a failure of the check (the measurement may be stale): the reason
/// is reported with `remeasure`, and the whole-tree run decides.
///
/// The run is forced past the moon result cache on purpose: a replayed result executes nothing,
/// so it would prove nothing on this run.
fn prove_narrowed(
    root: &Path,
    entry: &cached_verdict::Prepared,
    executor_checks: &BTreeMap<String, cmnp::executor::Check>,
    project: &str,
    manifest_arg: &str,
    environment: &Environment,
) -> Proof {
    let unproven = |reason: String, outcome: Proof| {
        event(json!({
            "event": "verdict-narrowing",
            "check": entry.check,
            "scope": "tree",
            "remeasure": matches!(outcome, Proof::Failed),
            "reason": reason,
        }));
        outcome
    };
    let copy = match entry.proving_copy() {
        Ok(copy) => copy,
        Err(error) => {
            return unproven(format!("no proving copy: {error}"), Proof::Unavailable);
        }
    };
    event(json!({
        "event": "verdict-proof",
        "check": entry.check,
        "method": crate::narrowing::PROOF,
        "files": copy.files,
        "placeholders": copy.placeholders,
    }));
    let mut proving = environment.clone();
    // The copy is a repository of its own: a verified commit names its HEAD, not the commit judged.
    if proving.contains_key(std::ffi::OsStr::new("CI_COMMIT_SHA")) {
        proving.insert("CI_COMMIT_SHA".into(), copy.commit.clone().into());
    }
    let outcome = cmnp::executor::execute(&cmnp::executor::Request {
        repo: copy.path().to_path_buf(),
        project: project.to_owned(),
        manifest_arg: manifest_arg.to_owned(),
        checks: executor_checks.clone(),
        selected: vec![entry.check.clone()],
        plan: false,
        force: true,
        tool: "ccid".into(),
        tool_revision: verdict_epoch_label(),
        environment: proving.into_iter().collect::<cmnp::Environment>(),
    });
    if let Err(error) = outcome {
        return unproven(
            format!(
                "the check failed in a copy holding only its measured inputs, so the measurement may be stale: {error}"
            ),
            Proof::Failed,
        );
    }
    match copy_back(copy.path(), root, &entry.check, entry.outputs()) {
        Ok(()) => Proof::Proven,
        Err(error) => unproven(
            format!("the proving run's outputs could not be moved into the checkout: {error}"),
            Proof::Unavailable,
        ),
    }
}

/// How the traced whole-tree run of a check with a stale read-set ended.
#[cfg_attr(not(unix), allow(dead_code))]
enum Measure {
    /// The tracer cannot run here; the check runs as usual and the next executing run tries again.
    Unavailable(String),
    /// The check ran, traced. `Ok`: it passed.
    Ran(Result<()>),
}

/// The one traced whole-tree run per staleness event: `check` runs once on the whole tree under
/// the measure-inputs tracer (the seccomp-bpf variant, which stops only at the traced system
/// calls) with every compile cache off, and the read-set it touched goes into the verdict store
/// as the check's candidate set. It is the check's run: its pass is recorded under the whole-tree
/// key by the caller. The next miss proves the candidate; nothing is written to the repository.
#[cfg(unix)]
fn measure_whole_tree(
    root: &Path,
    entry: &cached_verdict::Prepared,
    declared: &crate::Check,
    marker: &Marker,
    manifest_arg: &str,
    environment: &Environment,
    producer: Option<&cached_verdict::Producer>,
) -> Measure {
    use crate::read_set::{trace_check, tracer_works, Trace, Traced, CACHES};
    let (Some(slot), Some(producer)) = (entry.slot(), producer) else {
        return Measure::Unavailable("the verdict store is not provisioned".into());
    };
    if !crate::narrowing::can_measure() {
        return Measure::Unavailable(
            "this ccid is a local build; a measurement names a released revision".into(),
        );
    }
    if environment.contains_key(std::ffi::OsStr::new("CCID_TARGET_LOCK_HELD")) {
        return Measure::Unavailable("it runs inside a ccid check, which cannot be traced".into());
    }
    let Some(strace) = crate::compile_cache::executable(environment, "strace") else {
        return Measure::Unavailable("strace is not on PATH".into());
    };
    let scratch = match tempfile::Builder::new().prefix("ccid-measure-").tempdir() {
        Ok(scratch) => scratch,
        Err(error) => return Measure::Unavailable(format!("no scratch directory: {error}")),
    };
    if let Err(reason) = tracer_works(&strace, true, scratch.path()) {
        return Measure::Unavailable(reason);
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return Measure::Unavailable(format!("cannot locate ccid: {error}")),
    };
    // The traced run is the check as the executor would run it, minus the caches (a cache hit
    // would hide the compiler's reads) and with the receipt the executor would declare.
    let mut traced = environment.clone();
    for name in CACHES {
        traced.remove(std::ffi::OsStr::new(name));
    }
    let receipt = root
        .join(".ccid/results")
        .join(format!("{}.jsonl", entry.check));
    let prepared_receipt = receipt
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::File::create(&receipt).map(drop));
    if let Err(error) = prepared_receipt {
        return Measure::Unavailable(format!("no receipt directory: {error}"));
    }
    traced.insert("CCID_RECEIPT".into(), receipt.into_os_string());
    event(json!({
        "event": "read-set-measure",
        "check": entry.check,
        "status": "tracing",
        "since": marker.since,
    }));
    let outcome = trace_check(&Trace {
        exe: &exe,
        root,
        manifest: Path::new(manifest_arg),
        check: &entry.check,
        environment: &traced,
        strace: &strace,
        seccomp_bpf: true,
        depth: 1,
        outputs: &declared.cache_outputs,
        scratch: scratch.path(),
    });
    match outcome {
        Err(error) => Measure::Unavailable(error.to_string()),
        Ok(Traced::Failed(status)) => Measure::Ran(Err(failure(format!(
            "Check {} failed under the read-set tracer ({status})",
            entry.check
        )))),
        Ok(Traced::Unmeasured(reason)) => {
            event(json!({
                "event": "read-set-measure",
                "check": entry.check,
                "status": "unmeasured",
                "reason": reason,
            }));
            Measure::Ran(Ok(()))
        }
        Ok(Traced::Measured {
            observations,
            globs,
        }) => {
            let stored = slot
                .record(
                    globs.clone(),
                    entry.layout(),
                    &marker.base,
                    &entry.commit,
                    observations,
                    producer,
                )
                .and_then(|record| {
                    if globs.is_empty() {
                        slot.store_whole_tree(&record)
                    } else {
                        slot.store_candidate(&record)
                    }
                });
            if let Err(error) = stored {
                event(json!({
                    "event": "verdict-store-error",
                    "check": entry.check,
                    "reason": error.to_string(),
                }));
            }
            Measure::Ran(Ok(()))
        }
    }
}

#[cfg(not(unix))]
fn measure_whole_tree(
    _root: &Path,
    _entry: &cached_verdict::Prepared,
    _declared: &crate::Check,
    _marker: &Marker,
    _manifest_arg: &str,
    _environment: &Environment,
    _producer: Option<&cached_verdict::Producer>,
) -> Measure {
    Measure::Unavailable("a read-set is measured on unix hosts only".into())
}

/// Move what a proving run left to the checkout: the declared outputs and the check's receipt.
/// An output the run did not produce stays missing, and the store then refuses the record.
fn copy_back(from: &Path, to: &Path, check: &str, outputs: &[String]) -> Result<()> {
    let receipt = format!(".ccid/results/{check}.jsonl");
    for relative in outputs.iter().map(String::as_str).chain([receipt.as_str()]) {
        let source = from.join(relative);
        if std::fs::symlink_metadata(&source).is_err() {
            continue;
        }
        copy_tree(&source, &to.join(relative))?;
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(source)?;
    if meta.is_dir() {
        std::fs::create_dir_all(destination)?;
        for child in std::fs::read_dir(source)? {
            let child = child?;
            copy_tree(&child.path(), &destination.join(child.file_name()))?;
        }
    } else if meta.is_file() {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, destination)?;
    } else {
        return Err(failure(
            "A proving run left a link or special file as output",
        ));
    }
    Ok(())
}

/// The version of what a cached verdict MEANS. Cached results are keyed by this number, not by the binary revision: a new
/// ccid build that checks the same things the same way reuses every existing verdict, so a release does not invalidate the
/// whole result cache. Raise it ONLY when the semantics of a check change (what a check runs, or what counts as passing),
/// so older verdicts can no longer answer for the new meaning.
pub const VERDICT_EPOCH: u32 = 2;

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

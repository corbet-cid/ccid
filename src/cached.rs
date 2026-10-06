//! Result caching around selected checks through moon (the cmnp adapter).
//!
//! Each selected check becomes one moon task whose command is the ordinary
//! `ccid check` for that check. ccid owns the manifest and check definitions;
//! cmnp owns how moon runs them (workspace/task generation, tool identity,
//! remote cache, receipts).
use crate::{
    event, failure, load_manifest, select_checks, value, Environment, Result, Runner,
    SOURCE_REVISION,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path, time::Duration};

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

pub(crate) fn run_cached_with_environment(
    repo: &Path,
    manifest: &Path,
    selectors: &[String],
    plan: bool,
    force: bool,
    environment: Environment,
) -> Result<()> {
    crate::safe_relative(manifest)?;
    let root = repo.canonicalize()?;
    let (parsed, bytes) = load_manifest(&root, manifest)?;
    let selected = select_checks(&parsed, selectors)?;
    let mut checks = BTreeMap::new();
    for name in &selected {
        let check = &parsed.checks[name];
        crate::checks::validate_cached_contract(check)
            .map_err(|error| failure(format!("Cached check {name} is not shareable: {error}")))?;
        checks.insert(
            name.clone(),
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
            },
        );
    }
    cmnp::executor::validate_selection(&checks, &selected)?;
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
    if plan {
        return Ok(());
    }
    let runner = Runner::new(root.clone(), environment.clone(), Duration::from_secs(120))?;
    let path_ccid = runner.run(&argv(&["ccid", "source-revision"]), true)?;
    if path_ccid != SOURCE_REVISION {
        return Err(failure(format!(
            "The ccid on PATH ({path_ccid}) must be this verified revision ({SOURCE_REVISION})"
        )));
    }
    cmnp::executor::execute(&cmnp::executor::Request {
        repo: root,
        project,
        manifest_arg: manifest_arg.to_owned(),
        checks,
        selected,
        plan: false,
        force,
        tool: "ccid".into(),
        tool_revision: SOURCE_REVISION.into(),
        environment: environment.into_iter().collect::<cmnp::Environment>(),
    })
}

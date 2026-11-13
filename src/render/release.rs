//! The release surface of `render_schema = 3` (`[release]`): the Forgejo workflow that holds the whole release
//! (`.forgejo/workflows/release.yml`). Nothing of it runs on a mirror forge (R6, R7): the GitHub publisher for JSR that
//! earlier releases rendered is retired (R46, `retired.rs`).
//!
//! The workflow is four jobs in one run, ordered by `needs` (no dispatch chain, no polling):
//!
//! ```text
//! release (linux) -> release-windows (windows) -> release-macos (macos) -> release-promote (linux)
//! ```
//!
//! A leg exists only for the platforms the manifest declares; it runs only when the version holds that platform
//! (the release job says so in its outputs, `legs`), and promotion runs only when the release job released
//! something and every leg that ran is green. The jobs are not declared: `ccid job-run --job release` plans them
//! from `[release]` (see `release::job_plan`).

use super::Templates;
use crate::release;
use crate::{failure, Result};
use std::{collections::BTreeMap, path::PathBuf};

pub(super) const FORGEJO: &str = ".forgejo/workflows/release.yml";

/// One job of the workflow: its shim step. The release job's step has an id: the outputs are read from it.
fn job(templates: &Templates, name: &str, label: &str) -> String {
    let step = templates
        .forgejo_shim
        .replace("CCID_JOB", name)
        .replace("CCID_CLASS", label);
    if name != release::JOB {
        return step;
    }
    let header = format!("      - name: Run {name}\n");
    step.replacen(&header, &format!("{header}        id: run\n"), 1)
}

/// `needs.<job>.result` is green or the job was skipped (a leg the version does not hold).
fn done(job: &str) -> String {
    format!("(needs.{job}.result == 'success' || needs.{job}.result == 'skipped')")
}

fn workflow(templates: &Templates, config: &release::Config) -> String {
    let legs = config.declared_legs();
    let mut out = String::from(templates.release_workflow);
    out.push_str(&format!(
        "  {}:\n    runs-on: linux\n    outputs:\n      legs: ${{{{ steps.run.outputs.legs }}}}\n      released: ${{{{ steps.run.outputs.released }}}}\n    steps:\n",
        release::JOB
    ));
    out.push_str(&job(templates, release::JOB, "linux"));
    let mut before: Vec<String> = Vec::new();
    for os in &legs {
        let name = release::leg_job(os);
        let mut needs = vec![release::JOB.to_owned()];
        needs.extend(before.iter().cloned());
        let earlier: Vec<String> = before.iter().map(|b| format!(" && {}", done(b))).collect();
        out.push_str(&format!(
            "  {name}:\n    needs: [{}]\n    if: ${{{{ !cancelled() && needs.{}.result == 'success' && needs.{}.outputs.released == 'true' && contains(needs.{}.outputs.legs, '{os}'){} }}}}\n    runs-on: {os}\n    steps:\n",
            needs.join(", "),
            release::JOB,
            release::JOB,
            release::JOB,
            earlier.concat(),
        ));
        out.push_str(&job(templates, &name, os));
        before.push(name);
    }
    let mut needs = vec![release::JOB.to_owned()];
    needs.extend(before.iter().cloned());
    let finished: Vec<String> = before.iter().map(|b| format!(" && {}", done(b))).collect();
    out.push_str(&format!(
        "  {}:\n    needs: [{}]\n    if: ${{{{ !cancelled() && needs.{}.result == 'success' && needs.{}.outputs.released == 'true'{} }}}}\n    runs-on: linux\n    steps:\n",
        release::PROMOTE,
        needs.join(", "),
        release::JOB,
        release::JOB,
        finished.concat(),
    ));
    out.push_str(&job(templates, release::PROMOTE, "linux"));
    out
}

/// The files `[release]` renders; none without the table.
pub(super) fn outputs(
    parsed: &crate::Manifest,
    templates: &Templates,
) -> Result<BTreeMap<PathBuf, String>> {
    let mut outputs = BTreeMap::new();
    let Some(config) = &parsed.release else {
        return Ok(outputs);
    };
    config.validate(parsed)?;
    outputs.insert(PathBuf::from(FORGEJO), workflow(templates, config));
    Ok(outputs)
}

/// The jobs `[release]` plans, by name (for the inventory).
pub(super) fn plans(parsed: &crate::Manifest) -> Result<BTreeMap<String, crate::jobs::Plan>> {
    let mut plans = BTreeMap::new();
    let Some(config) = &parsed.release else {
        return Ok(plans);
    };
    let mut names = vec![release::JOB.to_owned(), release::PROMOTE.to_owned()];
    names.extend(config.declared_legs().iter().map(|os| release::leg_job(os)));
    for name in names {
        let plan = release::job_plan(parsed, &name)?
            .ok_or_else(|| failure(format!("the release job {name} has no plan")))?;
        plans.insert(name, plan);
    }
    Ok(plans)
}

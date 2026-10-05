//! Repository-owned job selection. Submission remains in scheduler adapters.
use crate::{failure, load_manifest, select_checks, validate_command, Result, Runner};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Scheduler {
    #[default]
    Crow,
    Argo,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    #[serde(default)]
    scheduler: Scheduler,
    checks: Vec<String>,
    workflow: String,
    command: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub job: String,
    pub scheduler: Scheduler,
    pub checks: Vec<String>,
    pub workflow: String,
    pub command: Vec<String>,
}

/// Exact inputs staged by a scheduler adapter. Credentials never belong here.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub archive: PathBuf,
    pub sha256: String,
    pub commit: String,
    pub bundle: PathBuf,
    pub bundle_sha256: String,
    pub tool_revision: String,
    pub job: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

/// Run the repository's entrypoint from verified source and exact local Git history.
/// Crow and Argo call this same function with the same request.
pub fn execute(request: &Request) -> Result<()> {
    if request.tool_revision != crate::SOURCE_REVISION {
        return Err(failure("Job request does not match this ccid revision"));
    }
    let mut environment: crate::Environment = std::env::vars_os().collect();
    for (key, value) in &request.environment {
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            return Err(failure("Invalid job environment"));
        }
        environment.insert(key.into(), value.into());
    }
    environment.insert("CI_COMMIT_SHA".into(), request.commit.clone().into());
    environment.insert("CCID_BIN".into(), std::env::current_exe()?.into_os_string());
    crate::admission::admit(&mut environment)?;
    let scratch = crate::cache::scratch(&mut environment)?;
    let bundle = scratch.path().join("source.bundle");
    std::fs::copy(&request.bundle, &bundle)?;
    if crate::sha256_file(&bundle)? != request.bundle_sha256 {
        return Err(failure("Job Git bundle SHA-256 mismatch"));
    }
    let root = scratch.path().join("source");
    crate::verify_source(&request.archive, &request.sha256, &request.commit, &root)?;
    let planned = plan(&root, Path::new(".ci/ccid.toml"), &request.job, None)?;
    environment.insert("CHECKS".into(), planned.checks.join(",").into());
    environment.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    environment.insert("GIT_LFS_SKIP_SMUDGE".into(), "1".into());
    let timeout = crate::budget(&environment)?.timeout;
    let runner = Runner::new(root, environment, Duration::from_secs(timeout))?;
    let git = |args: Vec<String>| {
        let mut command = vec!["git".into(), "-c".into(), "core.hooksPath=/dev/null".into()];
        command.extend(args);
        runner.run(&command, true)
    };
    git(vec!["init".into(), "--quiet".into()])?;
    git(vec![
        "fetch".into(),
        "--quiet".into(),
        "--no-tags".into(),
        "--no-recurse-submodules".into(),
        bundle.to_string_lossy().into_owned(),
        "refs/heads/source".into(),
    ])?;
    if git(vec!["rev-parse".into(), "FETCH_HEAD".into()])? != request.commit {
        return Err(failure(
            "Job bundle does not contain the requested source commit",
        ));
    }
    // Populate only the index/HEAD; the verified archive owns worktree bytes,
    // including materialized LFS and submodule files. Never smudge or fetch them.
    git(vec![
        "reset".into(),
        "--mixed".into(),
        "--quiet".into(),
        request.commit.clone(),
    ])?;
    runner.run(&planned.command, false)?;
    Ok(())
}

/// Plan one declared job without installing tools, contacting a forge or submitting work.
pub fn plan(
    repo: &Path,
    manifest: &Path,
    name: &str,
    scheduler: Option<Scheduler>,
) -> Result<Plan> {
    let (manifest, _) = load_manifest(repo, manifest)?;
    let job = manifest
        .jobs
        .get(name)
        .ok_or_else(|| failure(format!("Unknown job: {name}")))?;
    let checks = select_checks(&manifest, &job.checks)?;
    validate_command(&job.command)?;
    if job.workflow.is_empty()
        || !job
            .workflow
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        return Err(failure("Job workflow must be a plain workflow name"));
    }
    Ok(Plan {
        job: name.into(),
        scheduler: scheduler.unwrap_or(job.scheduler),
        checks,
        workflow: job.workflow.clone(),
        command: job.command.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(job: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("ci.toml"), format!(
            "schema = 1\nproject = 'demo'\n[checks.test]\nkind = 'commands'\ncommands = [['true']]\n[jobs.verify]\n{job}\n"
        )).unwrap();
        root
    }

    #[test]
    fn default_and_override_keep_identical_work() {
        let root =
            repository("checks = ['test']\nworkflow = 'verify'\ncommand = ['bash', '.ci/run.sh']");
        let crow = plan(root.path(), Path::new("ci.toml"), "verify", None).unwrap();
        let argo = plan(
            root.path(),
            Path::new("ci.toml"),
            "verify",
            Some(Scheduler::Argo),
        )
        .unwrap();
        assert_eq!(crow.scheduler, Scheduler::Crow);
        assert_eq!(argo.scheduler, Scheduler::Argo);
        assert_eq!(crow.checks, argo.checks);
        assert_eq!(crow.command, argo.command);
    }

    #[test]
    fn declared_scheduler_and_invalid_selections() {
        let root = repository(
            "scheduler = 'argo'\nchecks = ['test']\nworkflow = 'verify'\ncommand = ['true']",
        );
        assert_eq!(
            plan(root.path(), Path::new("ci.toml"), "verify", None)
                .unwrap()
                .scheduler,
            Scheduler::Argo
        );
        assert!(plan(root.path(), Path::new("ci.toml"), "missing", None).is_err());
        for job in [
            "checks = ['missing']\nworkflow = 'verify'\ncommand = ['true']",
            "checks = []\nworkflow = 'verify'\ncommand = ['true']",
            "checks = ['test']\nworkflow = '../verify'\ncommand = ['true']",
            "checks = ['test']\nworkflow = 'verify'\ncommand = []",
            "scheduler = 'unknown'\nchecks = ['test']\nworkflow = 'verify'\ncommand = ['true']",
        ] {
            let bad = repository(job);
            assert!(plan(bad.path(), Path::new("ci.toml"), "verify", None).is_err());
        }
    }
}

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
    /// Branches whose push events may execute this job through a generated
    /// push adapter. Empty (the default) refuses all push execution.
    #[serde(default)]
    push_branches: Vec<String>,
    /// Newest-head branch refresh applied once before push-executed gates.
    /// Absent (the default) refuses push execution: without a declared
    /// refresh scope the frozen graph cannot be proven current.
    #[serde(default)]
    refresh: Option<Refresh>,
    /// Optional manifest-owned per-job environment applied to
    /// prepare/refresh/check execution (both push and manual sentinel
    /// routes). Default empty. Reserved identity/status keys cannot be
    /// overridden (validated).
    #[serde(default)]
    environment: BTreeMap<String, String>,
    /// Secret environment variables this job may see: names declared in
    /// `[render].secret_environment`. Every declared variable a job does not
    /// list is withheld from it. Crow jobs only.
    #[serde(default)]
    secrets: Vec<String>,
}

/// Manifest-owned newest-head refresh scope: update every first-party git
/// dependency that declares `branch` equal to this branch and whose
/// canonical source starts with one of these host/path prefixes, then
/// freeze gates offline. Example prefix: `git.corbet.ch/corbet-libs`.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Refresh {
    pub branch: String,
    pub sources: Vec<String>,
    /// Optional manifest-owned lock-preparation commands (existing checked-in
    /// drivers only, e.g. a compatibility selection), run once AFTER
    /// `cargo update` and BEFORE the freeze (`cargo fetch --locked`) and
    /// gates, under the same bounded planned Runner. Default empty (no
    /// preparation); old manifests without this key plan byte-identically.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prepare_commands: Vec<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub job: String,
    pub scheduler: Scheduler,
    pub checks: Vec<String>,
    pub workflow: String,
    pub command: Vec<String>,
    /// Push opt-in branches; skipped when empty so manual-only inventories
    /// render byte-identical to before.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub push_branches: Vec<String>,
    /// Refresh scope evidence; skipped when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh: Option<Refresh>,
    /// Declared per-job environment evidence; skipped when empty so old
    /// manifests render byte-identical to before.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    /// Declared secret variables this job may see; skipped when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// Declared secret variables this job must not see; `execute` strips them.
    #[serde(skip)]
    pub withheld: Vec<String>,
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
    /// Adapter-staged manual explicit Git URLs (`url`, `url#<sha>`,
    /// `url?branch=X`, `url?tag=X`). Covers manual jobs whose fetches are not
    /// declared in manifests. Empty by default; unknown entries fail closed.
    #[serde(default)]
    pub source_urls: Vec<String>,
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
    // Reporting is a distinct adapter step. Never hand its tokens to checks.
    environment.retain(|key, _| !is_reporter_status_key(&key.to_string_lossy()));
    environment.insert("CI_COMMIT_SHA".into(), request.commit.clone().into());
    environment.insert("CCID_BIN".into(), std::env::current_exe()?.into_os_string());
    crate::admission::admit(&mut environment)?;
    let scratch = crate::cache::scratch(&mut environment)?;
    // Job-owned temporary directory for entrypoint fixtures. This always
    // overrides any incoming RUNNER_TEMP: checks must never write outside
    // the owned scratch directory, and adapters require the variable.
    environment.insert("RUNNER_TEMP".into(), scratch.path().as_os_str().into());
    let bundle = scratch.path().join("source.bundle");
    std::fs::copy(&request.bundle, &bundle)?;
    if crate::sha256_file(&bundle)? != request.bundle_sha256 {
        return Err(failure("Job Git bundle SHA-256 mismatch"));
    }
    let root = scratch.path().join("source");
    crate::verify_source(&request.archive, &request.sha256, &request.commit, &root)?;
    let planned = plan(&root, Path::new(".ci/ccid.toml"), &request.job, None)?;
    // Manifest-owned per-job environment applies consistently to manual and
    // push routes. Re-run admission and budget afterwards so declared values
    // (e.g. CI_JOBS) are bound/validated exactly like scheduler-provided ones.
    withhold_secrets(&mut environment, &planned.withheld);
    crate::jobs::apply_job_environment(&mut environment, &planned.environment);
    crate::admission::admit(&mut environment)?;
    environment.insert("CHECKS".into(), planned.checks.join(",").into());
    environment.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    environment.insert("GIT_LFS_SKIP_SMUDGE".into(), "1".into());
    // Resolver pre-step: runs AFTER the verified source unpack above, so
    // manifest/lock scanning reads checked source, and installs config into
    // the environment moved into Runner::new below so commands consume it.
    // No runner config -> existing operation. Resolve errors fail closed.
    let timeout = crate::budget(&environment)?.timeout;
    crate::resolver::maybe_prepare_runner_env(
        &mut environment,
        &root,
        scratch.path(),
        &request.source_urls,
        timeout,
    )?;
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
    // Generic sentinel: run declared checks through the pinned binary under
    // the existing warm target/lock, exactly like push does (push adds frozen
    // lock enforcement on top). Arbitrary commands still run directly so
    // manual-only adapters stay byte stable.
    if is_self_checks_command(&planned.command) {
        let deadline = std::time::Instant::now()
            .checked_add(Duration::from_secs(timeout))
            .ok_or_else(|| failure("Check deadline is out of range"))?;
        let repo = runner.root.clone();
        let env = runner.environment.clone();
        // run_checks_with_environment reloads the manifest, re-validates the
        // selection and executes each declared check generically (cargo, nix,
        // javascript or explicit commands). No shell/Python is added here.
        crate::run_checks_with_environment(
            &repo,
            Path::new(".ci/ccid.toml"),
            &planned.checks,
            false,
            env,
            &request.commit,
            deadline,
        )?;
        return Ok(());
    }
    runner.run(&planned.command, false)?;
    Ok(())
}
/// Generic explicit self-command for push-executed jobs: run the declared
/// checks through the pinned ccid binary instead of an arbitrary shell
/// wrapper. Single-element sentinel so it can never collide with a real
/// executable; validated in planning and executed consistently by
/// execute-job/manual and push paths. Existing manual-only jobs with
/// arbitrary commands keep working byte-identically (no new validation).
pub const SELF_CHECKS_SENTINEL: &str = "ccid:run-declared-checks";

/// True iff this job command explicitly opts into generic declared-checks
/// execution (exactly the sentinel, nothing else).
pub fn is_self_checks_command(command: &[String]) -> bool {
    command.len() == 1 && command[0] == SELF_CHECKS_SENTINEL
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
    for branch in &job.push_branches {
        if branch.is_empty()
            || branch.contains("CCID")
            || !branch
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        {
            return Err(failure("Job push branches must be plain branch names"));
        }
    }
    if let Some(refresh) = &job.refresh {
        validate_refresh(refresh)?;
    }
    validate_job_environment(&job.environment)?;
    let declared: Vec<&String> = manifest
        .render
        .as_ref()
        .map(|render| render.secret_environment.keys().collect())
        .unwrap_or_default();
    if let Some(unknown) = job.secrets.iter().find(|name| !declared.contains(name)) {
        return Err(failure(format!(
            "Job names a secret that [render].secret_environment does not declare: {unknown}"
        )));
    }
    if let Some(clash) = job.environment.keys().find(|key| declared.contains(key)) {
        return Err(failure(format!(
            "Job environment cannot set a declared secret variable: {clash}"
        )));
    }
    if !job.secrets.is_empty() && scheduler.unwrap_or(job.scheduler) != Scheduler::Crow {
        return Err(failure("Jobs with secrets must run on Crow"));
    }
    let withheld = declared
        .into_iter()
        .filter(|name| !job.secrets.contains(name))
        .cloned()
        .collect();
    // Push-executed jobs must explicitly opt into generic declared-checks
    // execution. Arbitrary commands are refused here (fail closed) so push
    // can never silently ignore jobs.command. Manual-only jobs (no push
    // branches, no refresh scope) skip this check and stay byte stable.
    let push_enabled = !job.push_branches.is_empty() || job.refresh.is_some();
    if push_enabled && !is_self_checks_command(&job.command) {
        return Err(failure(
            "Push jobs require explicit ccid:run-declared-checks command; arbitrary commands are refused",
        ));
    }
    Ok(Plan {
        job: name.into(),
        scheduler: scheduler.unwrap_or(job.scheduler),
        checks,
        workflow: job.workflow.clone(),
        command: job.command.clone(),
        push_branches: manifest.jobs[name].push_branches.clone(),
        refresh: manifest.jobs[name].refresh.clone(),
        environment: manifest.jobs[name].environment.clone(),
        secrets: job.secrets.clone(),
        withheld,
    })
}

/// Manifest-owned newest-head refresh scope, shared by planning and the
/// push-executed resolver so both agree on what "first-party" means.
pub(crate) fn validate_refresh(refresh: &Refresh) -> Result<()> {
    if refresh.branch.is_empty()
        || refresh.branch.contains("CCID")
        || !refresh
            .branch
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        return Err(failure("Refresh branch must be a plain branch name"));
    }
    if refresh.sources.is_empty() {
        return Err(failure("Refresh sources must name at least one prefix"));
    }
    for prefix in &refresh.sources {
        if prefix.is_empty()
            || prefix.contains("CCID")
            || prefix.starts_with('/')
            || prefix.ends_with('/')
            || prefix.contains("//")
            || !prefix
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-/".contains(&c))
        {
            return Err(failure(
                "Refresh sources must be plain lowercase host/path prefixes",
            ));
        }
    }
    validate_prepare_commands(&refresh.prepare_commands)?;
    Ok(())
}

/// Reporter status prefixes that must never reach repository code, gate
/// commands, or manifest overlays. `CFRG_STATUS_*` is the migrated Argo
/// reporter contract; `CCID_STATUS_*` stays stripped as defensive
/// sanitization of the legacy prefix.
pub(crate) fn is_reporter_status_key(key: &str) -> bool {
    key.starts_with("CFRG_STATUS_") || key.starts_with("CCID_STATUS_")
}

/// Reserved identity/status keys a manifest-owned per-job environment must
/// never override. The scheduler/adapter owns these (provenance, tool
/// identity, reporting tokens, target-lock guard), plus the job-owned
/// scratch directory: `execute` assigns `RUNNER_TEMP` to the owned scratch
/// after planning, and the manifest overlay must never undo that ownership.
pub(crate) fn reserved_env_key(key: &str) -> bool {
    matches!(
        key,
        "CCID_BIN"
            | "CCID_REVISION"
            | "CHECKS"
            | "CI_COMMIT_SHA"
            | "CI_COMMIT_BRANCH"
            | "CI_REPOSITORY_URL"
            | "RUNNER_TEMP"
    ) || is_reporter_status_key(key)
        || key.starts_with("CCID_TARGET_")
        || key.starts_with("CCID_JOB_")
        || key.starts_with("CCID_PUSH_")
        || key.starts_with("CCID_DEP_")
        || key.starts_with("CCID_TOOL_")
        || key.starts_with("CCID_SECRET_")
}

/// Generic manifest-owned lock-preparation commands, shared by planning and
/// push execution. Each argv must be a nonempty executable with NUL-free
/// arguments (same rule as all ccid commands). Empty (the default) means no
/// preparation; old manifests without this key behave byte-identically.
pub(crate) fn validate_prepare_commands(commands: &[Vec<String>]) -> Result<()> {
    for argv in commands {
        crate::validate_command(argv)?;
    }
    Ok(())
}

/// Remove the declared secret variables a job did not ask for.
fn withhold_secrets(environment: &mut crate::Environment, withheld: &[String]) {
    for name in withheld {
        environment.remove(std::ffi::OsStr::new(name));
    }
}

/// Generic per-job environment, shared by planning, push staging and manual
/// execution. Keys must be plain `NAME` assignments with NUL-free values;
/// reserved identity/status keys fail closed.
pub(crate) fn validate_job_environment(environment: &BTreeMap<String, String>) -> Result<()> {
    for (key, value) in environment {
        if key.is_empty()
            || key.contains(['=', '\0'])
            || value.contains('\0')
            || reserved_env_key(key)
        {
            return Err(failure(format!(
                "Job environment key is reserved or invalid: {key}"
            )));
        }
    }
    Ok(())
}

/// Overlay a validated manifest-owned per-job environment onto a working
/// environment. Reserved keys are never overwritten even if validation
/// missed them (defense in depth); validated maps are disjoint by
/// construction so this is a pure addition.
pub(crate) fn apply_job_environment(
    working: &mut crate::Environment,
    declared: &BTreeMap<String, String>,
) {
    for (key, value) in declared {
        if reserved_env_key(key) {
            continue;
        }
        working.insert(key.into(), value.into());
    }
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

    fn secret_repository(jobs: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("ci.toml"),
            format!(
                "schema = 1\nproject = 'demo'\n[render]\ntool_revision = '{}'\n\
                 secret_environment = {{ DEMO_TOKEN = 'demo_token', OTHER_KEY = 'other_key' }}\n\
                 [checks.test]\nkind = 'commands'\ncommands = [['true']]\n{jobs}",
                "0".repeat(40)
            ),
        )
        .unwrap();
        root
    }

    #[test]
    fn a_job_sees_only_the_declared_secrets_it_lists() {
        let root = secret_repository(
            "[jobs.verify]\nchecks = ['test']\nworkflow = 'verify'\ncommand = ['true']\n\
             [jobs.release]\nchecks = ['test']\nworkflow = 'verify'\ncommand = ['true']\n\
             secrets = ['DEMO_TOKEN']\n",
        );
        let verify = plan(root.path(), Path::new("ci.toml"), "verify", None).unwrap();
        assert!(verify.secrets.is_empty());
        assert_eq!(verify.withheld, ["DEMO_TOKEN", "OTHER_KEY"]);
        let release = plan(root.path(), Path::new("ci.toml"), "release", None).unwrap();
        assert_eq!(release.secrets, ["DEMO_TOKEN"]);
        assert_eq!(release.withheld, ["OTHER_KEY"]);

        let mut environment = crate::Environment::new();
        for name in ["DEMO_TOKEN", "OTHER_KEY", "PATH"] {
            environment.insert(name.into(), "value".into());
        }
        withhold_secrets(&mut environment, &release.withheld);
        assert!(environment.contains_key(std::ffi::OsStr::new("DEMO_TOKEN")));
        assert!(!environment.contains_key(std::ffi::OsStr::new("OTHER_KEY")));
        assert!(environment.contains_key(std::ffi::OsStr::new("PATH")));
        withhold_secrets(&mut environment, &verify.withheld);
        assert_eq!(environment.len(), 1);
    }

    #[test]
    fn secrets_must_be_declared_unshadowed_and_on_crow() {
        for job in [
            // Not declared in [render].secret_environment.
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\nsecrets = ['UNDECLARED']",
            // The job's own environment may not set a declared secret variable.
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\nenvironment = { DEMO_TOKEN = 'x' }",
            // Argo has no secret projection.
            "scheduler = 'argo'\nchecks = ['test']\nworkflow = 'verify'\ncommand = ['true']\nsecrets = ['DEMO_TOKEN']",
        ] {
            let root = secret_repository(&format!("[jobs.verify]\n{job}\n"));
            assert!(plan(root.path(), Path::new("ci.toml"), "verify", None).is_err(), "{job}");
        }
        // Without any declaration nothing is withheld and `secrets` is refused.
        let bare = repository(
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\nsecrets = ['DEMO_TOKEN']",
        );
        assert!(plan(bare.path(), Path::new("ci.toml"), "verify", None).is_err());
        let plain = repository("checks = ['test']\nworkflow = 'verify'\ncommand = ['true']");
        assert!(plan(plain.path(), Path::new("ci.toml"), "verify", None)
            .unwrap()
            .withheld
            .is_empty());
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
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\npush_branches = ['bad branch']",
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\npush_branches = ['CCID_V01']",
        ] {
            let bad = repository(job);
            assert!(plan(bad.path(), Path::new("ci.toml"), "verify", None).is_err());
        }
    }

    #[test]
    fn refresh_scope_validates_branch_and_prefixes() {
        let root = repository(
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['ccid:run-declared-checks']\n\
             refresh = { branch = 'v01', sources = ['forge.example.invalid/cpkg'] }",
        );
        let planned = plan(root.path(), Path::new("ci.toml"), "verify", None).unwrap();
        let refresh = planned.refresh.unwrap();
        assert_eq!(refresh.branch, "v01");
        assert_eq!(refresh.sources, ["forge.example.invalid/cpkg".to_owned()]);
        // Push-enabled (refresh) with arbitrary command is refused: explicit
        // sentinel required so push never silently ignores jobs.command.
        // Manual-only (no refresh, no push) with arbitrary command still plans.
        let push_arbitrary = repository(
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\n\
             refresh = { branch = 'v01', sources = ['forge.example.invalid/cpkg'] }",
        );
        assert!(plan(push_arbitrary.path(), Path::new("ci.toml"), "verify", None).is_err());
        for job in [
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\n\
             refresh = { branch = '', sources = ['forge.example.invalid/cpkg'] }",
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\n\
             refresh = { branch = 'v01', sources = [] }",
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\n\
             refresh = { branch = 'v01', sources = ['https://forge.example.invalid/cpkg'] }",
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\n\
             refresh = { branch = 'v01', sources = ['forge.example.invalid/cpkg/'] }",
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\n\
             refresh = { branch = 'v01', unknown = 1 }",
        ] {
            let bad = repository(job);
            assert!(plan(bad.path(), Path::new("ci.toml"), "verify", None).is_err());
        }
    }

    #[test]
    fn manifest_environment_refuses_runner_temp() {
        // The job-owned scratch directory is assigned by `execute` after
        // planning; a manifest-owned RUNNER_TEMP would overwrite that
        // ownership through the later overlay, so planning refuses it.
        // Ordinary compile/cache keys still plan.
        let ok = repository(
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\n\
             environment = { CI_JOBS = '2' }",
        );
        let planned = plan(ok.path(), Path::new("ci.toml"), "verify", None).unwrap();
        assert_eq!(
            planned.environment.get("CI_JOBS").map(String::as_str),
            Some("2")
        );
        let hostile = repository(
            "checks = ['test']\nworkflow = 'verify'\ncommand = ['true']\n\
             environment = { RUNNER_TEMP = '/tmp/elsewhere' }",
        );
        assert!(plan(hostile.path(), Path::new("ci.toml"), "verify", None).is_err());
        // The overlay itself also skips reserved keys (defense in depth), so
        // even an unvalidated map can never undo the owned assignment.
        let mut working: crate::Environment = BTreeMap::new();
        working.insert("RUNNER_TEMP".into(), "/owned/scratch".into());
        let mut declared = BTreeMap::new();
        declared.insert("RUNNER_TEMP".to_owned(), "/tmp/elsewhere".to_owned());
        apply_job_environment(&mut working, &declared);
        assert_eq!(
            working
                .get(&std::ffi::OsString::from("RUNNER_TEMP"))
                .map(|value| value.to_string_lossy().into_owned()),
            Some("/owned/scratch".to_owned())
        );
    }

    #[test]
    fn reporter_status_prefixes_stay_scheduler_owned() {
        // The migrated Argo reporter contract plus legacy sanitization.
        assert!(is_reporter_status_key("CFRG_STATUS_TOKEN"));
        assert!(is_reporter_status_key("CFRG_STATUS_ENDPOINT"));
        assert!(is_reporter_status_key("CCID_STATUS_TOKEN"));
        // Ordinary keys and near-miss prefixes are untouched.
        assert!(!is_reporter_status_key("CI_JOBS"));
        assert!(!is_reporter_status_key("CFRGSTATUS_TOKEN"));
        assert!(!is_reporter_status_key("CFRG_STATUS"));
        // Declared new-prefix keys fail closed like the legacy ones.
        for key in ["CFRG_STATUS_TOKEN", "CCID_STATUS_TOKEN"] {
            let declared = BTreeMap::from([(key.to_owned(), "x".to_owned())]);
            assert!(
                validate_job_environment(&declared).is_err(),
                "{key} must stay scheduler-owned"
            );
        }
        // The overlay itself also skips them (defense in depth), while
        // ordinary keys still apply.
        let mut working: crate::Environment = BTreeMap::new();
        working.insert("CFRG_STATUS_TOKEN".into(), "scheduler".into());
        working.insert("CI_JOBS".into(), "1".into());
        let mut declared = BTreeMap::new();
        declared.insert("CFRG_STATUS_TOKEN".to_owned(), "evil".to_owned());
        declared.insert("CI_JOBS".to_owned(), "2".to_owned());
        apply_job_environment(&mut working, &declared);
        assert_eq!(
            working
                .get(&std::ffi::OsString::from("CFRG_STATUS_TOKEN"))
                .map(|value| value.to_string_lossy().into_owned()),
            Some("scheduler".to_owned())
        );
        assert_eq!(
            working
                .get(&std::ffi::OsString::from("CI_JOBS"))
                .map(|value| value.to_string_lossy().into_owned()),
            Some("2".to_owned())
        );
    }

    /// The job entrypoint must see a writable owned scratch directory as
    /// RUNNER_TEMP even when the request carries a hostile value, and the
    /// owned directory must be gone after the job completes.
    #[test]
    fn execute_exports_owned_writable_runner_temp() {
        use std::process::Command;
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("source");
        std::fs::create_dir_all(source.join(".ci")).unwrap();
        std::fs::write(
            source.join(".ci/ccid.toml"),
            "schema = 1\nproject = 'demo'\n[checks.test]\nkind = 'commands'\ncommands = [['true']]\n[jobs.job]\nchecks = ['test']\nworkflow = 'verify'\ncommand = ['sh', '-c', 'test -d \"$RUNNER_TEMP\" && test -w \"$RUNNER_TEMP\" && touch \"$RUNNER_TEMP/probe\" && test -z \"${CFRG_STATUS_TOKEN:-}\" && test -z \"${CCID_STATUS_TOKEN:-}\" && echo \"$RUNNER_TEMP\" > \"$TEST_OUT\"']\n",
        )
        .unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&source)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .unwrap();
            assert!(status.success());
        };
        git(&["init", "--quiet"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ]);
        let commit = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&source)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let commit = commit.trim().to_owned();
        assert_eq!(commit.len(), 40);
        git(&["update-ref", "refs/heads/source", "HEAD"]);
        let archive = work.path().join("source.tar");
        git(&[
            "archive",
            "--format=tar",
            "--output",
            &archive.to_string_lossy(),
            "HEAD",
        ]);
        let bundle = work.path().join("source.bundle");
        git(&[
            "bundle",
            "create",
            &bundle.to_string_lossy(),
            "refs/heads/source",
        ]);
        let out = work.path().join("runner-temp.txt");
        let hostile = work.path().join("hostile");
        std::fs::create_dir_all(&hostile).unwrap();
        let mut job_env = std::collections::BTreeMap::new();
        job_env.insert("TEST_OUT".to_owned(), out.to_string_lossy().into_owned());
        job_env.insert(
            "RUNNER_TEMP".to_owned(),
            hostile.to_string_lossy().into_owned(),
        );
        // Reporter tokens must never reach checks, new prefix or legacy.
        job_env.insert("CFRG_STATUS_TOKEN".to_owned(), "fixture-secret".to_owned());
        job_env.insert("CCID_STATUS_TOKEN".to_owned(), "fixture-secret".to_owned());
        let request = Request {
            archive: archive.clone(),
            sha256: crate::sha256_file(&archive).unwrap(),
            commit: commit.clone(),
            bundle: bundle.clone(),
            bundle_sha256: crate::sha256_file(&bundle).unwrap(),
            tool_revision: crate::SOURCE_REVISION.to_owned(),
            job: "job".to_owned(),
            environment: job_env,
            source_urls: Vec::new(),
        };
        execute(&request).unwrap();
        let recorded = std::fs::read_to_string(&out).unwrap();
        let recorded = recorded.trim().to_owned();
        assert!(!recorded.is_empty());
        assert_ne!(recorded, hostile.to_string_lossy());
        let path = std::path::Path::new(&recorded);
        assert!(path.is_absolute());
        assert!(path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("ccid-job-")));
        assert!(
            !path.exists(),
            "owned scratch must be cleaned up after the job"
        );
        assert!(
            !hostile.join("probe").exists(),
            "hostile directory must stay untouched"
        );
    }
}

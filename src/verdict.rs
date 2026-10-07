//! One aggregated verdict per commit.
//!
//! Crow posts one commit status per workflow, so a failed side job (release,
//! publish) on the same workflow overwrites the result of the checks that gate
//! landing. Generated adapters instead report through this command:
//!
//! * `ccid/<job>`: the job's own result, one context per job, never gating;
//! * `ccid/verdict`: the ONE context landing waits on. It is `success` only
//!   when every gating job declared for the repository (`[verdict] jobs` in
//!   the manifest, default `verify`) has succeeded for this exact commit,
//!   `failure` as soon as one failed, and `pending` otherwise.
//!
//! The pending state is posted by a generated adapter step. The terminal state
//! is posted by `execute-job` itself ([`report_terminal`]) from the exit status
//! of the job it supervised: Crow provides no pipeline-status variable to
//! steps, so the result never travels through the scheduler.
//!
//! Statuses are posted through the operator's status reporter (cfrg's status
//! procedure); this command never talks to a forge itself. Per-job results are
//! recorded per repository and commit in the reporter's shared state directory
//! so that jobs running in separate pipelines contribute to the same verdict.
use crate::{failure, Result};
use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

/// Status context of the aggregated verdict that gates landing.
pub const VERDICT_CONTEXT: &str = "ccid/verdict";
/// Records of commits older than this are pruned when a new one is written.
const RECORD_LIFETIME: Duration = Duration::from_secs(30 * 24 * 3600);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Pending,
    Success,
    Failure,
}
impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Success => "success",
            State::Failure => "failure",
        }
    }
}

#[derive(Args, Debug)]
pub struct Options {
    #[arg(long)]
    pub commit: String,
    /// The job this invocation reports.
    #[arg(long)]
    pub job: String,
    /// Comma-separated jobs that gate landing. Empty: the commit has no verdict.
    #[arg(long, default_value = "")]
    pub gating: String,
    #[arg(long, value_enum)]
    pub state: State,
    #[arg(long)]
    pub url: String,
    /// Scheduler creation time (Unix seconds), as for the status reporter.
    #[arg(long)]
    pub started: u64,
    /// The operator's status reporter executable.
    #[arg(long, env = "CCID_STATUS_BINARY")]
    pub binary: PathBuf,
    /// SHA-256 the reporter executable must have.
    #[arg(long, env = "CCID_STATUS_BINARY_SHA256")]
    pub binary_sha256: String,
    /// Persistent directory shared by all reporter invocations.
    #[arg(long, env = "CFRG_STATUS_STATE_DIR")]
    pub state_dir: Option<PathBuf>,
    /// Repository identity (`owner/name`), provided by the scheduler.
    #[arg(long, env = "CI_REPO")]
    pub repo: Option<String>,
}

fn plain_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}
fn exact_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn repository_path(value: &str) -> bool {
    let parts: Vec<&str> = value.split('/').collect();
    (2..=4).contains(&parts.len()) && parts.iter().all(|p| plain_name(p))
}

/// The verdict of a commit from the recorded state of its gating jobs. A
/// missing job has not reported yet, which keeps the verdict pending.
pub fn aggregate(gating: &[String], jobs: &BTreeMap<String, State>) -> State {
    if gating.iter().any(|g| jobs.get(g) == Some(&State::Failure)) {
        State::Failure
    } else if !gating.is_empty() && gating.iter().all(|g| jobs.get(g) == Some(&State::Success)) {
        State::Success
    } else {
        State::Pending
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Record {
    jobs: BTreeMap<String, State>,
}

/// Record one job's state and return every recorded state of the commit.
fn record(
    state_dir: &Path,
    repo: &str,
    commit: &str,
    job: &str,
    state: State,
) -> Result<BTreeMap<String, State>> {
    let directory = state_dir.join("ccid-verdict").join(repo);
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{commit}.json"));
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(format!("{commit}.lock")))?;
    lock.lock()?;
    let mut current: Record = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    current.jobs.insert(job.to_owned(), state);
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    file.write_all(&serde_json::to_vec(&current)?)?;
    file.as_file().sync_all()?;
    file.persist(&path)?;
    prune(&directory);
    drop(lock);
    Ok(current.jobs)
}

fn prune(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .is_some_and(|age| age > RECORD_LIFETIME);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn verified(binary: &Path, expected: &str) -> Result<()> {
    use std::io::Read;
    let mut hash = Sha256::new();
    let mut file = File::open(binary)?;
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    if format!("{:x}", hash.finalize()) != expected {
        return Err(failure("Status reporter differs from its declared SHA-256"));
    }
    Ok(())
}

fn post(options: &Options, name: &str, state: State) -> Result<()> {
    let status = Command::new(&options.binary)
        .args(["--commit", &options.commit, "--name", name, "--url"])
        .arg(&options.url)
        .args(["--started", &options.started.to_string()])
        .args(["--state", state.as_str()])
        .stdin(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(failure(format!(
            "Status reporter failed for {name}: {status}"
        )))
    }
}

/// Report one job and, when it gates landing, the commit's aggregated verdict.
pub fn run(options: &Options) -> Result<()> {
    if !exact_commit(&options.commit) {
        return Err(failure("Commit must be an exact 40-hex Git SHA"));
    }
    if !plain_name(&options.job) {
        return Err(failure("Job name must be a plain name"));
    }
    if options.url.contains(['\r', '\n']) {
        return Err(failure("Run URL must be a single line"));
    }
    let gating: Vec<String> = options
        .gating
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if gating.iter().any(|g| !plain_name(g)) {
        return Err(failure("Gating jobs must be plain job names"));
    }
    verified(&options.binary, &options.binary_sha256)?;
    post(options, &format!("ccid/{}", options.job), options.state)?;
    if !gating.contains(&options.job) {
        return Ok(());
    }
    let verdict = match (&options.state_dir, options.repo.as_deref()) {
        (Some(directory), Some(repo)) if repository_path(repo) => {
            let jobs = record(
                directory,
                repo,
                &options.commit,
                &options.job,
                options.state,
            )?;
            aggregate(&gating, &jobs)
        }
        // Without a shared record only a lone gating job can speak for the commit.
        _ if gating.len() == 1 => options.state,
        _ => State::Pending,
    };
    post(options, VERDICT_CONTEXT, verdict)?;
    println!("{VERDICT_CONTEXT} {}", verdict.as_str());
    Ok(())
}

/// Options of the job's own terminal report, from the step environment the
/// adapter provides, or `None` when the operator did not enable reporting
/// (`CCID_STATUS_CONFIG` empty or absent), exactly like the pending step.
pub fn terminal_options(
    commit: &str,
    job: &str,
    state: State,
    environment: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Options>> {
    let value = |name: &str| environment(name).filter(|v| !v.is_empty());
    if value("CCID_STATUS_CONFIG").is_none() {
        return Ok(None);
    }
    let required =
        |name: &str| value(name).ok_or_else(|| failure(format!("Status reporting needs {name}")));
    Ok(Some(Options {
        commit: commit.to_owned(),
        job: job.to_owned(),
        gating: value("CCID_VERDICT_JOBS").unwrap_or_default(),
        state,
        url: required("CI_PIPELINE_URL")?,
        started: required("CI_PIPELINE_CREATED")?
            .parse()
            .map_err(|_| failure("CI_PIPELINE_CREATED must be Unix seconds"))?,
        binary: PathBuf::from(required("CCID_STATUS_BINARY")?),
        binary_sha256: required("CCID_STATUS_BINARY_SHA256")?,
        state_dir: value("CFRG_STATUS_STATE_DIR").map(PathBuf::from),
        repo: value("CI_REPO"),
    }))
}

/// Report the terminal state of the job whose request is in `request`: success
/// when its supervised execution succeeded, failure otherwise. Does nothing
/// unless reporting is enabled; a reporter failure is an error.
pub fn report_terminal(request: &Path, succeeded: bool) -> Result<()> {
    let request: crate::jobs::Request = serde_json::from_slice(&fs::read(request)?)?;
    let state = if succeeded {
        State::Success
    } else {
        State::Failure
    };
    let environment = |name: &str| std::env::var(name).ok();
    match terminal_options(&request.commit, &request.job, state, &environment)? {
        Some(options) => run(&options),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;

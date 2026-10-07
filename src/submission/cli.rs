use super::core::Api;
use super::*;
use clap::{Args, Subcommand};

#[derive(Clone, Debug, Args)]
pub struct SubmitArgs {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    #[arg(long, default_value = "main")]
    pub branch: String,
    #[arg(long)]
    pub expect_commit: Option<String>,
    #[arg(long = "workflow", required = true)]
    pub workflows: Vec<String>,
    #[arg(long = "var")]
    pub variables: Vec<String>,
    #[arg(long, default_value_t = 120)]
    pub queue_timeout: u64,
    #[arg(long, conflicts_with = "cached_rerun")]
    pub rerun: bool,
    #[arg(long)]
    pub cached_rerun: bool,
}
#[derive(Clone, Debug, Args)]
pub struct DependentsArgs {
    /// Checkout of the repository that just landed (its origin names the identity).
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    #[arg(long, default_value = "main")]
    pub branch: String,
    /// Landed commit; defaults to the published branch head.
    #[arg(long)]
    pub commit: Option<String>,
    /// Repository job submitted for every dependent.
    #[arg(long, default_value = "verify")]
    pub job: String,
    /// Runs in flight at once; never more than the eight Crow slots.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=8))]
    pub slots: u8,
    /// Seconds a worker waits for host admission instead of failing.
    #[arg(long, default_value_t = 900)]
    pub admission_wait: u64,
    /// Overall seconds to wait for results before reporting timeouts.
    #[arg(long, default_value_t = 5400)]
    pub wait: u64,
    /// Only list the dependents; submit nothing.
    #[arg(long)]
    pub plan: bool,
    /// The cfrg executable that reads the forge for the dependents scan.
    #[arg(long, env = "CFRG_BIN", default_value = "cfrg")]
    pub cfrg: String,
}
#[derive(Subcommand)]
pub enum Action {
    /// Explicit process-cleanup probe; requires external cancellation evidence.
    ProbeCancellation {
        #[arg(long, hide = true)]
        child: bool,
    },
    /// Validate reviewed adapter pins and manifests without executing checks.
    ValidateAdapters {
        #[arg(long, env = "ADAPTER_CORPUS")]
        corpus: PathBuf,
        #[arg(long, env = "ADAPTER_CORPUS_SHA256")]
        sha256: String,
        #[arg(long)]
        propose_pin: bool,
    },
    /// Create the exact committed source closure without network access.
    Archive {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        commit: String,
        #[arg(long)]
        destination: PathBuf,
    },
    Repos,
    Workflows {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = "main")]
        branch: String,
    },
    Plan(SubmitArgs),
    Run(SubmitArgs),
    Status {
        repo_id: u64,
        number: u64,
    },
    Logs {
        repo_id: u64,
        number: u64,
        step_id: u64,
    },
    /// Move the fleet to one verdict per commit: repin, re-render, land, then switch the land policy.
    RolloutVerdict(rollout::RolloutArgs),
    /// After a landing: submit every dependent's job on its main and print one line each.
    CheckDependents(DependentsArgs),
    /// Read a result: one line when green, otherwise the errors of each failed step.
    Digest {
        repo_id: u64,
        number: u64,
        /// Cap for the whole output.
        #[arg(long, default_value_t = 120)]
        max_lines: usize,
        /// Last log lines kept per failed step.
        #[arg(long, default_value_t = 30)]
        tail: usize,
    },
    Cancel {
        repo_id: u64,
        number: u64,
        #[arg(long)]
        commit: String,
    },
    RetryWorkflow {
        repo_id: u64,
        number: u64,
        workflow_id: u64,
        #[arg(long)]
        commit: String,
        #[arg(long)]
        reason: String,
    },
    Resolve {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long)]
        tool_repo: Option<PathBuf>,
        #[arg(long, default_value = "main")]
        tool_branch: String,
        #[arg(long = "check")]
        checks: Vec<String>,
        #[arg(long)]
        plan: bool,
    },
    /// Receive a checksum-bound source object atomically on the worker host.
    Receive {
        #[arg(long)]
        target: PathBuf,
        #[arg(long)]
        sha256: String,
    },
    /// Verify a pinned executable against its worker build receipt.
    BinaryReceipt {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        target: String,
    },
}
#[derive(Clone, Args)]
pub struct JobArgs {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    #[arg(long, default_value = "main")]
    pub branch: String,
    #[arg(long)]
    pub job: String,
    #[arg(long,value_parser=["crow","argo"])]
    pub scheduler: Option<String>,
    #[arg(long = "var")]
    pub variables: Vec<String>,
    #[arg(long)]
    pub rerun: bool,
}
#[derive(Subcommand)]
pub enum JobAction {
    Plan(JobArgs),
    Run(JobArgs),
    Status { state: PathBuf },
    Logs { state: PathBuf },
}

pub fn run(action: Action, path: Option<&Path>) -> Result<()> {
    match action {
        Action::ProbeCancellation { child } => return probe::run(child),
        Action::ValidateAdapters {
            corpus,
            sha256,
            propose_pin,
        } => return adapter::validate(&corpus, &sha256, propose_pin),
        Action::Archive {
            repo,
            commit,
            destination,
        } => {
            let closure = archive::create(&repo, &commit, &destination)?;
            return emit(
                &json!({"source_closure":closure,"source_sha256":digest(&destination)?,"source_bytes":destination.metadata()?.len()}),
            );
        }
        Action::Receive { target, sha256 } => {
            return transport::receive(&target, &sha256, std::io::stdin().lock());
        }
        Action::BinaryReceipt {
            root,
            revision,
            target,
        } => return emit(&transport::binary_receipt(&root, &revision, &target)?),
        _ => {}
    }
    let config = Config::load(path)?;
    match action {
        Action::Plan(args) => submit::route(&config, &args, true),
        Action::Run(args) => submit::route(&config, &args, false),
        Action::RolloutVerdict(args) => rollout::run(&config, &args),
        Action::CheckDependents(args) => dependents::run(&config, &Config::locate(path)?, &args),
        Action::Resolve {
            repo,
            branch,
            tool_repo,
            tool_branch,
            checks,
            plan,
        } => submit::resolve(
            &config,
            &repo,
            &branch,
            tool_repo.as_deref().unwrap_or(&config.tool_repo),
            &tool_branch,
            &checks,
            plan,
        ),
        action => {
            let api = core::Crow::new(&config)?;
            match action {
                Action::Repos => emit(
                    &core::pages(&api, "/repos?active=true")?
                        .iter()
                        .map(|r| core::pick(r, &["id", "full_name", "active", "timeout"]))
                        .collect(),
                ),
                Action::Workflows { repo, branch } => {
                    let record = core::resolve_repo(&config, &api, &repo)?;
                    let branch: String = url::form_urlencoded::Serializer::new(String::new())
                        .append_pair("branch", &branch)
                        .finish();
                    let configs = api.call(
                        &format!("/repos/{}/configs?{branch}", number(&record, "id")),
                        None,
                    )?;
                    emit(
                        &json!({"repo_id":record["id"],"workflows":rows(&configs["workflows"]).iter().map(|w|json!({"name":w["pipeline_name"],"file":w["name"],"manual":w["has_manual"],"depends_on":w.get("depends_on").cloned().unwrap_or(json!([])),"variables":rows(&w["variables"]).iter().map(|v|v["name"].clone()).collect::<Vec<_>>()})).collect::<Vec<_>>()}),
                    )
                }
                Action::Cancel {
                    repo_id,
                    number,
                    commit,
                } => submit::cancel(&api, repo_id, number, &commit),
                Action::RetryWorkflow {
                    repo_id,
                    number,
                    workflow_id,
                    commit,
                    reason,
                } => submit::retry(
                    &config,
                    &api,
                    repo_id,
                    number,
                    workflow_id,
                    &commit,
                    &reason,
                ),
                Action::Status { repo_id, number } => {
                    positive(repo_id, number)?;
                    emit(&core::summarize(&api.call(
                        &format!("/repos/{repo_id}/pipelines/{number}"),
                        None,
                    )?))
                }
                Action::Digest {
                    repo_id,
                    number,
                    max_lines,
                    tail,
                } => {
                    let redact = |text: &str| api.redact(text).unwrap_or_default();
                    for line in digest::digest(
                        &api,
                        &redact,
                        repo_id,
                        number,
                        digest::Options { max_lines, tail },
                    )? {
                        println!("{line}");
                    }
                    Ok(())
                }
                Action::Logs {
                    repo_id,
                    number,
                    step_id,
                } => {
                    use base64::Engine;
                    positive(repo_id, number)?;
                    let run = api.call(&format!("/repos/{repo_id}/pipelines/{number}"), None)?;
                    if !rows(&run["workflows"])
                        .iter()
                        .any(|w| rows(&w["children"]).iter().any(|s| s["id"] == step_id))
                    {
                        return Err("Step does not belong to selected run".into());
                    }
                    let entries =
                        api.call(&format!("/repos/{repo_id}/logs/{number}/{step_id}"), None)?;
                    for entry in rows(&entries) {
                        if let Some(data) = entry["data"].as_str() {
                            let decoded = base64::engine::general_purpose::STANDARD.decode(data)?;
                            let data = api.redact(&String::from_utf8_lossy(&decoded))?;
                            print!("{data}{}", if data.ends_with('\n') { "" } else { "\n" });
                        }
                    }
                    Ok(())
                }
                _ => Err("Invalid submission operation".into()),
            }
        }
    }
}
pub(super) fn positive(repo: u64, run: u64) -> Result<()> {
    if repo == 0 || run == 0 {
        Err("Repository and run identifiers must be positive".into())
    } else {
        Ok(())
    }
}

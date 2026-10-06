use super::core::Api;
use super::*;
use cli::SubmitArgs;

pub(super) fn variables(items: &[String]) -> Result<Value> {
    let mut values = json!({});
    for item in items {
        let (key, value) = item.split_once('=').ok_or("Variables must be NAME=value")?;
        if !matches(r"^[A-Z][A-Z0-9_]*$", key) || values.get(key).is_some() {
            return Err("Variables must be unique uppercase NAME=value entries".into());
        }
        if ((key.starts_with("CI_") || key.starts_with("CROW_")) && !core::BUDGETS.contains(&key))
            || ["SOURCE_ARCHIVE", "SOURCE_SHA256", "CCID_REVISION"].contains(&key)
        {
            return Err("Reserved source/runtime variable cannot be overridden".into());
        }
        values[key] = json!(value);
    }
    Ok(values)
}
pub(super) struct Prepared {
    _temporary: tempfile::TempDir,
    pub repo: PathBuf,
    pub repo_id: u64,
    pub commit: String,
    pub workflows: Vec<String>,
    pub variables: Value,
    pub plan: Value,
    archive: PathBuf,
    digest: String,
    dependencies: Vec<pinned::Source>,
    tool: Option<(PathBuf, String)>,
}
impl Prepared {
    pub fn new(config: &Config, args: &SubmitArgs, api: &dyn Api) -> Result<Self> {
        if args.rerun && args.cached_rerun {
            return Err("Cached rerun and rerun are mutually exclusive".into());
        }
        let repo = args.repo.canonicalize()?;
        let record = core::resolve_repo(config, api, &repo)?;
        let repo_id = number(&record, "id");
        let workflows: Vec<_> = args
            .workflows
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if repo_id == 0 || workflows.is_empty() || workflows.iter().any(|w| !name(w)) {
            return Err("Workflow names must be plain names without paths".into());
        }
        let mut variables = variables(&args.variables)?;
        let (commit, ignored) = core::source_identity(&repo, &args.branch, args.cached_rerun)?;
        if args.expect_commit.as_ref().is_some_and(|e| e != &commit) {
            return Err("Committed source changed from --expect-commit; no work submitted".into());
        }
        let tool = pinned::identify(config, &repo, &commit, &workflows)?;
        if let Some(tool) = &tool {
            for (key, values) in &tool.defaults {
                if key == "CI_MIN_AVAILABLE_MB" && !tool.memory_admission {
                    continue;
                }
                if variables.get(key).is_some() {
                    continue;
                }
                if values.len() != 1 {
                    return Err(format!("Selected workflows have different {key} defaults; supply an explicit value").into());
                }
                variables[key] = json!(values.iter().next());
            }
            if tool.repository_identity {
                variables["CI_REPOSITORY_URL"] = json!(config.origin(&repo)?);
            }
        }
        let temporary = tempfile::Builder::new()
            .prefix("transfer-")
            .tempdir_in(config.directory("crow-ci-transfers")?)?;
        let archive = temporary.path().join("source.tar");
        let closure = archive::create(&repo, &commit, &archive)?;
        let hash = digest(&archive)?;
        variables["SOURCE_ARCHIVE"] =
            json!(format!("{}/{repo_id}/{hash}.tar", config.worker_sources));
        variables["SOURCE_SHA256"] = json!(hash);
        let dependencies = pinned::prepare(
            config,
            &repo,
            &commit,
            temporary.path(),
            repo_id,
            &mut variables,
            &workflows,
        )?;
        let mut plan = json!({"repo":record["full_name"],"repo_id":repo_id,"commit":commit,"source_sha256":hash,"source_bytes":archive.metadata()?.len(),"source_closure":closure,"pinned_sources":dependencies.iter().map(|d|d.info.clone()).collect::<Vec<_>>(),"workflows":workflows,"ignored_worktree_changes":ignored});
        let tool = if let Some(tool) = tool {
            let archive = temporary.path().join("ccid.tar");
            git(
                &config.tool_repo,
                &[
                    "archive",
                    "--format=tar",
                    &format!("--output={}", archive.display()),
                    &tool.revision,
                ],
            )?;
            let hash = digest(&archive)?;
            variables["CI_TOOL_ARCHIVE"] =
                json!(format!("{}/ccid/{hash}.tar", config.worker_sources));
            variables["CI_TOOL_SHA256"] = json!(hash);
            plan["ccid_revision"] = json!(tool.revision);
            plan["ccid_sha256"] = json!(hash);
            if tool.binary {
                let binary = pinned::executable(config, &tool.revision)?;
                plan["ccid_binary_sha256"] = binary["CI_TOOL_BINARY_SHA256"].clone();
                variables
                    .as_object_mut()
                    .ok_or("Variables missing")?
                    .extend(binary.as_object().ok_or("Binary identity missing")?.clone());
            }
            Some((archive, hash))
        } else {
            None
        };
        plan["variable_names"] = variables
            .as_object()
            .ok_or("Variables missing")?
            .keys()
            .cloned()
            .collect();
        Ok(Self {
            _temporary: temporary,
            repo,
            repo_id,
            commit,
            workflows,
            variables,
            plan,
            archive,
            digest: hash,
            dependencies,
            tool,
        })
    }
    pub fn matching(&self, api: &dyn Api, args: &SubmitArgs) -> Result<Vec<Value>> {
        core::matching_runs(
            api,
            self.repo_id,
            &self.commit,
            &self.variables,
            &self.workflows,
            &args.branch,
        )
    }
    pub fn inspection(&self, api: &dyn Api, args: &SubmitArgs) -> Result<Value> {
        let mut plan = self.plan.clone();
        plan["matching_runs"] = self
            .matching(api, args)?
            .iter()
            .map(core::summarize)
            .collect();
        Ok(plan)
    }
    pub fn run(
        &mut self,
        config: &Config,
        args: &SubmitArgs,
        api: &dyn Api,
        plan_only: bool,
        before: &mut dyn FnMut(&Value) -> Result<()>,
        after: &mut dyn FnMut(&Value) -> Result<()>,
    ) -> Result<()> {
        if plan_only {
            return emit(&self.inspection(api, args)?);
        }
        let _lock = lock(
            &config
                .directory("crow-ci-locks")?
                .join(format!("{}.lock", self.repo_id)),
        )?;
        let existing = self.matching(api, args)?;
        if existing.iter().any(|r| {
            number(r, "number") == 0
                || !core::ACTIVE.contains(&text(r, "status").as_str())
                    && !core::TERMINAL.contains(&text(r, "status").as_str())
        }) {
            return Err("Matching Crow inventory contains unknown run identity or status".into());
        }
        if let Some(active) = existing
            .iter()
            .find(|r| core::ACTIVE.contains(&text(r, "status").as_str()))
        {
            return emit(
                &json!({"action":"attached","repo_id":self.repo_id,"run":core::summarize(active)}),
            );
        }
        let mut restart = None;
        if args.cached_rerun {
            let prior = existing.iter().max_by_key(|r| number(r, "number")).ok_or(
                "No matching stored Crow run; cached rerun refuses without forge metadata",
            )?;
            let cached = core::cached_restart(
                prior,
                &self.commit,
                &args.branch,
                &self.variables,
                &self.workflows,
            )?;
            self.plan
                .as_object_mut()
                .ok_or("Plan missing")?
                .extend(cached.as_object().ok_or("Restart plan missing")?.clone());
            restart = Some(number(prior, "number"));
        } else if !existing.is_empty() && !args.rerun {
            emit(
                &json!({"action":"existing-results","repo_id":self.repo_id,"runs":existing.iter().map(core::summarize).collect::<Vec<_>>()}),
            )?;
            return Err(
                "Matching completed work exists; inspect it or use --rerun after diagnosis".into(),
            );
        }
        self.plan["host_admission"] = core::admission(config)?;
        transport::stage(
            config,
            &self.archive,
            &self.repo_id.to_string(),
            &self.digest,
            "tar",
        )?;
        for dependency in &self.dependencies {
            transport::stage(
                config,
                &dependency.archive,
                &dependency.namespace,
                &dependency.digest,
                &dependency.extension,
            )?;
        }
        if let Some((archive, hash)) = &self.tool {
            transport::stage(config, archive, "ccid", hash, "tar")?;
        }
        if core::source_identity(&self.repo, &args.branch, args.cached_rerun)?.0 != self.commit {
            return Err("Source changed during staging; no pipeline submitted".into());
        }
        before(
            &json!({"repo_id":self.repo_id,"commit":self.commit,"prior_run_numbers":existing.iter().map(|r|r["number"].clone()).collect::<Vec<_>>()}),
        )?;
        let path = format!("/repos/{}/pipelines", self.repo_id);
        let run = if let Some(prior) = restart {
            api.call(&format!("{path}/{prior}"), Some(&json!({})))?
        } else {
            api.call(&path,Some(&json!({"branch":args.branch,"workflows":self.workflows,"variables":self.variables})))?
        };
        if run["commit"] != self.commit
            || number(&run, "number") == 0
            || restart == Some(number(&run, "number"))
        {
            return Err(
                "Crow dispatch returned unexpected source identity; inspect before resubmitting"
                    .into(),
            );
        }
        after(&core::summarize(&run))?;
        let mut report = self.plan.clone();
        report["action"] = json!(if restart.is_some() {
            "cached-rerun"
        } else {
            "submitted"
        });
        report["run"] = core::summarize(&run);
        emit(&report)
    }
}

pub(super) fn cancel(api: &dyn Api, repo: u64, run: u64, commit: &str) -> Result<()> {
    cli::positive(repo, run)?;
    if !matches(r"^[0-9a-fA-F]{40}$", commit) {
        return Err("--commit must be a full commit".into());
    }
    let path = format!("/repos/{repo}/pipelines/{run}");
    let before = api.call(&path, None)?;
    if before["commit"] != commit {
        return Err("Stored run commit differs; no cancellation sent".into());
    }
    let cancellable = ["pending", "running", "blocked"];
    if !cancellable.contains(&text(&before, "status").as_str()) {
        if !core::TERMINAL.contains(&text(&before, "status").as_str()) {
            return Err("Stored run status is unknown".into());
        }
        return emit(&json!({"action":"already-terminal","run":core::summarize(&before)}));
    }
    api.call(&format!("{path}/cancel"), Some(&json!({})))?;
    let after = api.call(&path, None)?;
    if after["commit"] != commit || !core::TERMINAL.contains(&text(&after, "status").as_str()) {
        return Err("Cancellation did not prove exact terminal state".into());
    }
    emit(
        &json!({"action":"cancelled","before":core::summarize(&before),"run":core::summarize(&after),"process_cleanup_verified":false,"process_cleanup_limit":"Crow 6.4 local cancellation does not prove child process-group cleanup"}),
    )
}
pub(super) fn select_retry(run: &Value, number: u64, id: u64, commit: &str) -> Result<Value> {
    if run["number"] != number || run["commit"] != commit {
        return Err("Stored run identity differs; no retry sent".into());
    }
    let workflows = rows(&run["workflows"]);
    let targets: Vec<_> = workflows.iter().filter(|w| w["id"] == id).collect();
    if targets.len() != 1 {
        return Err("Workflow ID does not belong uniquely to this run".into());
    }
    let target = targets[0];
    if workflows.iter().any(|w| {
        w["name"] == target["name"]
            && super::number(w, "attempt") > super::number(target, "attempt")
    }) {
        return Err("Newer workflow attempt exists".into());
    }
    if !["failure", "error"].contains(&text(run, "status").as_str())
        || !["failure", "error"].contains(&text(target, "state").as_str())
    {
        return Err("Retry requires failed terminal pipeline and workflow".into());
    }
    for dependency in rows(&target["depends_on"]) {
        let latest = workflows
            .iter()
            .filter(|w| w["name"] == *dependency)
            .max_by_key(|w| super::number(w, "attempt"));
        if latest.is_none_or(|w| w["state"] != "success") {
            return Err("Workflow dependency lacks successful latest attempt".into());
        }
    }
    Ok(target.clone())
}
pub(super) fn retry(
    config: &Config,
    api: &dyn Api,
    repo: u64,
    run: u64,
    id: u64,
    commit: &str,
    reason: &str,
) -> Result<()> {
    cli::positive(repo, run)?;
    if id == 0 || !matches(r"^[0-9a-fA-F]{40}$", commit) || reason.trim().is_empty() {
        return Err("Retry requires exact workflow, commit and nonempty reason".into());
    }
    let directory = config.directory("crow-ci-locks")?;
    let _lock = lock(&directory.join(format!("{repo}.lock")))?;
    let path = format!("/repos/{repo}/pipelines/{run}");
    let target = select_retry(&api.call(&path, None)?, run, id, commit)?;
    let queue = api.call("/queue/info", None)?;
    for group in ["running", "pending", "waiting_on_deps"] {
        let rows = queue[group]
            .as_array()
            .ok_or("Crow queue inventory incomplete")?;
        if rows.iter().any(|r| r["repo_id"] == repo) {
            return Err("Repository already has queued or active work".into());
        }
    }
    let admission = core::admission(config)?;
    let intent = directory.join(format!("retry-{repo}-{run}-{id}.json"));
    let mut record = json!({"repo_id":repo,"number":run,"workflow_id":id,"commit":commit,"reason":reason,"state":"submitted-outcome-unresolved"});
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&intent)
        .map_err(|_| "Prior retry intent exists; no POST repeated")?;
    file.write_all(encode(&record)?.as_bytes())?;
    file.sync_all()?;
    File::open(&directory)?.sync_all()?;
    let after = api.call(&format!("{path}/workflows/{id}/rerun"), Some(&json!({})))?;
    if after["number"] != run || after["commit"] != commit {
        return Err("Retry response identity changed; inspect Crow".into());
    }
    let successors: Vec<_> = rows(&after["workflows"])
        .iter()
        .filter(|w| {
            w["name"] == target["name"] && number(w, "attempt") > number(&target, "attempt")
        })
        .collect();
    if successors.len() != 1 {
        return Err("Retry response lacks unique new attempt".into());
    }
    record["state"] = json!("confirmed");
    record["next_workflow_id"] = successors[0]["id"].clone();
    save(&intent, &record)?;
    emit(
        &json!({"action":"workflow-retried","reason":reason,"workflow_id":successors[0]["id"],"host_admission":admission,"run":core::summarize(&after)}),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn resolve(
    config: &Config,
    repo: &Path,
    branch: &str,
    tool_repo: &Path,
    tool_branch: &str,
    checks: &[String],
    plan: bool,
) -> Result<()> {
    let api = core::Crow::new(config)?;
    resolve_with(
        config,
        repo,
        branch,
        tool_repo,
        tool_branch,
        checks,
        plan,
        &api,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_with(
    config: &Config,
    repo: &Path,
    branch: &str,
    tool_repo: &Path,
    tool_branch: &str,
    checks: &[String],
    plan: bool,
    api: &dyn Api,
) -> Result<()> {
    let (commit, ignored) = core::source_identity(repo, branch, false)?;
    let url = config.origin(repo)?;
    if !config.tool_origins.contains(&config.origin(tool_repo)?) {
        return Err("Resolver must come from canonical ccid repository".into());
    }
    let (tool_commit, _) = core::source_identity(tool_repo, tool_branch, false)?;
    let paths = git(repo, &["ls-tree", "-r", "--name-only", &commit])?;
    if !["Cargo.toml", "Cargo.lock"]
        .iter()
        .all(|p| paths.lines().any(|l| l == *p))
    {
        return Err("Cargo refresh requires committed root manifests and lock".into());
    }
    let workflow = git(
        tool_repo,
        &["show", &format!("{tool_commit}:.crow/resolve.yaml")],
    )?;
    if ![
        "RESOLVE_SOURCE_ARCHIVE",
        "RESOLVE_SOURCE_SHA256",
        "RESOLVE_SOURCE_COMMIT",
        "RESOLVE_REPOSITORY_URL",
    ]
    .iter()
    .all(|k| workflow.contains(&format!("  {k}:")))
    {
        return Err("Update resolver workflow first".into());
    }
    let checks: Vec<_> = checks
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if checks.iter().any(|c| !name(c))
        || !checks.is_empty() && !workflow.contains("  RESOLVE_CHECKS:")
    {
        return Err("Invalid or unsupported candidate checks".into());
    }
    let namespace = format!("cargo-{}", sha(&url));
    let temporary = tempfile::tempdir_in(config.directory("crow-ci-transfers")?)?;
    let archive = temporary.path().join("source.tar");
    let closure = archive::create(repo, &commit, &archive)?;
    let hash = digest(&archive)?;
    let mut variables = vec![
        format!(
            "RESOLVE_SOURCE_ARCHIVE={}/{namespace}/{hash}.tar",
            config.worker_sources
        ),
        format!("RESOLVE_SOURCE_SHA256={hash}"),
        format!("RESOLVE_SOURCE_COMMIT={commit}"),
        format!("RESOLVE_REPOSITORY_URL={url}"),
    ];
    if !checks.is_empty() {
        variables.push(format!("RESOLVE_CHECKS={}", checks.join(",")));
    }
    emit(
        &json!({"action":"dependency-resolution-plan","provider":"crow","repository":url,"source_commit":commit,"source_sha256":hash,"resolver_commit":tool_commit,"source_closure":closure,"ignored_worktree_changes":ignored,"candidate_checks":checks,"updates_source_worktree":false}),
    )?;
    if !plan {
        transport::stage(config, &archive, &namespace, &hash, "tar")?;
    }
    let args = SubmitArgs {
        repo: tool_repo.into(),
        branch: tool_branch.into(),
        expect_commit: Some(tool_commit),
        workflows: vec!["resolve".into()],
        variables,
        provider: "crow".into(),
        provider_wait: 0,
        queue_timeout: 120,
        rerun: true,
        cached_rerun: false,
    };
    Prepared::new(config, &args, api)?.run(
        config,
        &args,
        api,
        plan,
        &mut |_| {
            if core::source_identity(repo, branch, false)?.0 != commit {
                Err("Consumer source changed before resolver dispatch".into())
            } else {
                Ok(())
            }
        },
        &mut |_| Ok(()),
    )
}

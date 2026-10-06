use super::*;
use cli::SubmitArgs;
use github::GitHub;

const CONFIG: &str = ".ci/providers.toml";
#[derive(Debug)]
struct RoutingBlocked(&'static str);
impl std::fmt::Display for RoutingBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for RoutingBlocked {}
pub(super) fn route(config: &Config, args: &SubmitArgs, plan: bool) -> Result<()> {
    if args.provider == "github" {
        return Err("GitHub is frozen; writes are prohibited".into());
    }
    let api = core::Crow::new(config)?;
    route_with(config, args, plan, &api, &mut || {
        Ok(Box::new(GitHub::new()?))
    })
}
const TOOL_INPUTS: &[&str] = &[
    "tool_revision",
    "tool_run_id",
    "tool_asset_id",
    "tool_archive_sha256",
    "tool_binary_sha256",
    "tool_bootstrap_sha256",
];

pub(super) fn contract(
    config: &Config,
    repo: &Path,
    commit: &str,
    checks: &[String],
) -> Result<Option<Value>> {
    let paths = git(repo, &["ls-tree", "-r", "--name-only", commit])?;
    let paths: BTreeSet<_> = paths.lines().collect();
    if !paths.contains(CONFIG) {
        return Ok(None);
    }
    let raw = git(repo, &["show", &format!("{commit}:{CONFIG}")])?;
    let declared: toml::Value = toml::from_str(&raw)?;
    let declared = serde_json::to_value(declared)?;
    if declared["schema"] != 1 {
        return Err("Unsupported provider contract schema".into());
    }
    let gha = &declared["github"];
    if !checks
        .iter()
        .all(|c| rows(&gha["checks"]).iter().any(|v| v == c))
        || gha["secret_free"] != true
        || gha["free_eligible"] != true
        || gha["platform"] != "linux-x86_64"
    {
        return Ok(None);
    }
    let repository = text(gha, "repository");
    if !matches(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$", &repository) {
        return Err("Provider contract requires canonical GitHub repository".into());
    }
    if config.origin(repo)? != format!("https://github.com/{repository}") {
        return Err("Hosted provider repository differs from source origin".into());
    }
    let workflow = text(gha, "workflow");
    if !matches(r"^[A-Za-z0-9_-]+\.ya?ml$", &workflow) {
        return Err("Invalid hosted workflow path".into());
    }
    if paths.contains(".gitmodules")
        || paths.contains(".ci/archives.toml")
        || git(repo, &["ls-tree", "-r", commit])?
            .lines()
            .any(|l| l.starts_with("160000 "))
    {
        return Ok(None);
    }
    for path in &paths {
        if (*path == ".gitattributes" || path.ends_with("/.gitattributes"))
            && git(repo, &["show", &format!("{commit}:{path}")])?.contains("filter=lfs")
        {
            return Ok(None);
        }
    }
    let dependencies = declared["dependencies"]["files"]
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or("Provider contract must name dependency inputs")?;
    if dependencies
        .iter()
        .any(|p| p.as_str().is_none_or(|p| !paths.contains(p)))
    {
        return Err("Provider dependency inputs must be committed".into());
    }
    let workflow_path = format!(".github/workflows/{workflow}");
    let mut tracked = BTreeSet::from([
        CONFIG.to_string(),
        ".ci/ccid.toml".into(),
        ".crow/ccid.yaml".into(),
        workflow_path.clone(),
    ]);
    tracked.extend(
        dependencies
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string),
    );
    let mut hashes = json!({});
    for path in tracked {
        hashes[&path] = json!(sha(git_bytes(
            repo,
            &["show", &format!("{commit}:{path}")]
        )?));
    }
    if hashes[&workflow_path] != gha["workflow_sha256"] {
        return Err("Hosted workflow changed since provider review".into());
    }
    let tool = pinned::identify(config, repo, commit, &["ccid".into()])?
        .ok_or("Missing reviewed ccid tool")?;
    if gha["tool_revision"] != tool.revision {
        return Err("Providers must use same reviewed ccid revision".into());
    }
    let dependency_hashes: Value = dependencies
        .iter()
        .filter_map(Value::as_str)
        .map(|p| (p.to_string(), hashes[p].clone()))
        .collect();
    Ok(Some(
        json!({"github":gha,"hashes":hashes,"dependency_snapshot":sha(encode(&dependency_hashes)?)}),
    ))
}
pub(super) fn validate_run(
    run: &Value,
    contract: &Value,
    commit: &str,
    key: &str,
    branch: &str,
) -> Result<()> {
    if run["head_sha"] != commit
        || run["display_title"] != format!("ccid/{key}")
        || run["path"]
            != format!(
                ".github/workflows/{}",
                text(&contract["github"], "workflow")
            )
        || run["event"] != "workflow_dispatch"
        || run["head_branch"] != branch
        || number(run, "id") == 0
    {
        return Err("Hosted metadata differs from exact request".into());
    }
    Ok(())
}
fn matching(
    api: &dyn github::Hosted,
    contract: &Value,
    commit: &str,
    key: &str,
) -> Result<Option<Value>> {
    let runs = api.pages(
        &format!(
            "/repos/{}/actions/runs?head_sha={commit}",
            text(&contract["github"], "repository")
        ),
        "workflow_runs",
    )?;
    let mut selected = Vec::new();
    for run in runs {
        if run["display_title"] == format!("ccid/{key}")
            && run["path"]
                == format!(
                    ".github/workflows/{}",
                    text(&contract["github"], "workflow")
                )
        {
            selected.push(run);
        } else if run["status"] != "completed" {
            return Err(RoutingBlocked(
                "Other hosted work exists for commit; reconcile coverage first",
            )
            .into());
        }
    }
    if selected.len() > 1 {
        return Err(RoutingBlocked("Multiple hosted runs have same request identity").into());
    }
    Ok(selected.pop())
}
pub(super) fn next_action(
    state: &Value,
    run: Option<&Value>,
    now: f64,
    queue_seconds: f64,
) -> &'static str {
    let phase = text(state, "phase");
    let Some(run) = run else {
        return if ["gha-intent", "cancel-intent", "crow-intent"].contains(&phase.as_str()) {
            "uncertain"
        } else {
            "choose"
        };
    };
    if run["status"] == "completed" {
        return if ["cancel-intent", "gha-retired"].contains(&phase.as_str())
            && run["conclusion"] == "cancelled"
        {
            "crow"
        } else {
            "result"
        };
    }
    if phase == "cancel-intent" {
        return "wait";
    }
    match text(run, "status").as_str() {
        "queued" | "requested" | "waiting" | "pending" => {
            if now - state["submitted_at"].as_f64().unwrap_or(now) >= queue_seconds {
                "cancel"
            } else {
                "wait"
            }
        }
        "in_progress" => "wait",
        _ => "uncertain",
    }
}
pub(super) fn jobs_may_have_started(jobs: &Value, run: u64, commit: &str) -> bool {
    let Some(jobs) = jobs.as_array().filter(|j| !j.is_empty()) else {
        return true;
    };
    for job in jobs {
        if job["run_id"] != run
            || job["head_sha"] != commit
            || job.get("started_at").is_none()
            || job["status"] != "completed"
            || !["cancelled", "skipped"].contains(&text(job, "conclusion").as_str())
            || !job["steps"].is_array()
            || !job["started_at"].is_null() && job["started_at"] != ""
        {
            return true;
        }
        for step in rows(&job["steps"]) {
            if step.get("started_at").is_none()
                || step["status"] != "completed"
                || !["cancelled", "skipped"].contains(&text(step, "conclusion").as_str())
                || !step["started_at"].is_null() && step["started_at"] != ""
                || !step["completed_at"].is_null()
                    && step["completed_at"] != ""
                    && step["conclusion"] != "skipped"
            {
                return true;
            }
        }
    }
    false
}
pub(super) fn portable_tool(
    config: &Config,
    api: &dyn github::Hosted,
    contract: &Value,
) -> Result<Value> {
    use base64::Engine;
    let gha = &contract["github"];
    let repository = config
        .github_tool_repository
        .as_deref()
        .ok_or("Declare historical GitHub tool repository to reconcile hosted receipts")?;
    let revision = text(gha, "tool_revision");
    let workflow = api.get(&format!(
        "/repos/{repository}/actions/workflows/bootstrap.yml"
    ))?;
    let (bytes, asset) = api.release_asset(repository, &revision)?;
    if number(&asset, "id") == 0 || asset["name"] != format!("ccid-{revision}-linux-x86_64.zip") {
        return Err("Release asset identity is not exact".into());
    }
    let receipt = github::tool_receipt(&bytes, &revision)?;
    let id = number(&receipt, "workflow_run_id");
    if id == 0 {
        return Err("Tool receipt lacks producing run identity".into());
    }
    let run = api.get(&format!("/repos/{repository}/actions/runs/{id}"))?;
    if run["workflow_id"] != workflow["id"]
        || run["status"] != "completed"
        || run["conclusion"] != "success"
        || run["event"] != "workflow_dispatch"
        || !exact_sha(&text(&run, "head_sha"))
    {
        return Err("Tool asset lacks verified bootstrap run".into());
    }
    let source = api.get(&format!(
        "/repos/{repository}/contents/.github/workflows/bootstrap.yml?ref={}",
        text(&run, "head_sha")
    ))?;
    let source = base64::engine::general_purpose::STANDARD
        .decode(text(&source, "content").replace(['\r', '\n'], ""))?;
    if gha["tool_bootstrap_sha256"] != sha(source)
        || receipt["workflow_revision"] != run["head_sha"]
        || receipt["workflow_sha256"] != gha["tool_bootstrap_sha256"]
    {
        return Err("Tool receipt differs from producing workflow".into());
    }
    Ok(
        json!({"tool_revision":revision,"tool_run_id":id.to_string(),"tool_asset_id":number(&asset,"id").to_string(),"tool_archive_sha256":sha(bytes),"tool_binary_sha256":receipt["binary_sha256"]}),
    )
}
fn use_crow(
    config: &Config,
    args: &SubmitArgs,
    api: &dyn core::Api,
    prepared: &mut submit::Prepared,
    plan: bool,
    path: &Path,
    state: &Value,
) -> Result<()> {
    if state["phase"] == "crow-intent" {
        return Err("Crow dispatch outcome unknown; reconcile recorded request".into());
    }
    let state = std::cell::RefCell::new(state.clone());
    prepared.run(
        config,
        args,
        api,
        plan,
        &mut |context| {
            let mut state = state.borrow_mut();
            state["phase"] = json!("crow-intent");
            state["crow_context"] = context.clone();
            state["prior_run_numbers"] = context["prior_run_numbers"].clone();
            save(path, &state)
        },
        &mut |run| {
            let mut state = state.borrow_mut();
            state["phase"] = json!("crow");
            state["crow_run"] = run.clone();
            save(path, &state)
        },
    )
}
pub(super) fn route_with(
    config: &Config,
    args: &SubmitArgs,
    plan: bool,
    api: &dyn core::Api,
    hosted: &mut dyn FnMut() -> Result<Box<dyn github::Hosted>>,
) -> Result<()> {
    // Explicit GitHub requests fail before credentials, inventory or mutations.
    if args.provider == "github" {
        return Err("GitHub is frozen; use Crow after reconciling existing hosted work".into());
    }
    let mapped =
        args.workflows.iter().collect::<BTreeSet<_>>() == BTreeSet::from([&"ccid".to_string()]);
    let variables = submit::variables(&args.variables)?;
    let selected = if mapped {
        variables["CHECKS"].as_str()
    } else {
        None
    };
    let mut prepared = submit::Prepared::new(config, args, api)?;
    let contract = if let Some(selected) = selected {
        let checks: Vec<String> = selected
            .split(',')
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if checks.is_empty() || checks.iter().any(|c| !name(c)) {
            return Err("Invalid check selector".into());
        }
        contract(config, &prepared.repo, &prepared.commit, &checks)?
            .map(|contract| (checks, contract))
    } else {
        None
    };
    let Some((checks, contract)) = contract else {
        return prepared.run(config, args, api, plan, &mut |_| Ok(()), &mut |_| Ok(()));
    };
    let overrides = variables
        .as_object()
        .ok_or("Variables missing")?
        .keys()
        .filter(|k| k.as_str() != "CHECKS")
        .collect::<Vec<_>>();
    if overrides
        .iter()
        .any(|k| !core::BUDGETS.contains(&k.as_str()))
    {
        return Err("Mapped extra variables must be documented resource controls".into());
    }
    let commit = prepared.commit.clone();
    let identity = json!({"commit":commit,"checks":checks,"contract":contract,"branch":args.branch,"protocol":1});
    let mut key = sha(encode(&identity)?);
    let directory = config.directory("ci-provider-requests")?;
    let _lock = lock(&directory.join(format!(
        "{}.lock",
        sha(text(&contract["github"], "repository"))
    )))?;
    let mut path = directory.join(format!("{key}.json"));
    let read_state = |path: &Path, key: &str| -> Result<Value> {
        Ok(if path.exists() {
            serde_json::from_slice(&fs::read(path)?)?
        } else {
            json!({"request_id":key,"identity":identity})
        })
    };
    let mut state = read_state(&path, &key)?;
    if state["phase"] == "crow-intent" {
        let matches = prepared.matching(api, args)?;
        let context = &state["crow_context"];
        let prior = state["prior_run_numbers"]
            .as_array()
            .ok_or("Missing Crow dispatch inventory")?;
        if context["repo_id"] != prepared.repo_id
            || context["commit"] != commit
            || context["prior_run_numbers"] != state["prior_run_numbers"]
            || prior.iter().any(|v| v.as_u64().is_none_or(|n| n == 0))
            || prior
                .iter()
                .map(Value::to_string)
                .collect::<BTreeSet<_>>()
                .len()
                != prior.len()
        {
            return Err("Crow dispatch context invalid; reconcile manually".into());
        }
        let candidates: Vec<_> = matches
            .iter()
            .filter(|r| !prior.contains(&r["number"]))
            .collect();
        if candidates.len() != 1 {
            return Err("Crow dispatch unresolved; no duplicate submission".into());
        }
        state["phase"] = json!("crow");
        state["crow_run"] = core::summarize(candidates[0]);
        save(&path, &state)?;
        return emit(
            &json!({"provider":"crow","action":"attached","repo_id":prepared.repo_id,"request_id":key,"runs":[core::summarize(candidates[0])]}),
        );
    }
    if state["phase"] == "crow" {
        return use_crow(config, args, api, &mut prepared, plan, &path, &state);
    }
    let unresolved = |s: &Value| {
        ["gha-intent", "gha", "cancel-intent", "observed"].contains(&text(s, "phase").as_str())
    };
    if args.cached_rerun {
        if unresolved(&state) {
            return Err("Hosted request must be reconciled before cached recovery".into());
        }
        return use_crow(config, args, api, &mut prepared, plan, &path, &state);
    }
    let github = match hosted() {
        Ok(api) => api,
        Err(error) => {
            if unresolved(&state) {
                return Err(error);
            }
            return use_crow(config, args, api, &mut prepared, plan, &path, &state);
        }
    };
    let mut run = match matching(github.as_ref(), &contract, &commit, &key) {
        Ok(run) => run,
        Err(error) => {
            if unresolved(&state) || error.is::<RoutingBlocked>() {
                return Err(error);
            }
            return use_crow(config, args, api, &mut prepared, plan, &path, &state);
        }
    };
    for depth in 0..=10 {
        if run.is_none() && unresolved(&state) {
            return Err("Recorded hosted request missing; no duplicate dispatch".into());
        }
        if !args.rerun || run.as_ref().is_none_or(|r| r["status"] != "completed") {
            break;
        }
        if depth == 10 {
            return Err("Rerun history too deep; reconcile selected request".into());
        }
        let previous = run.as_ref().ok_or("Run missing")?;
        validate_run(previous, &contract, &commit, &key, &args.branch)?;
        let mut next = identity.clone();
        next["previous_github_run"] = previous["id"].clone();
        key = sha(encode(&next)?);
        path = directory.join(format!("{key}.json"));
        state = read_state(&path, &key)?;
        if ["crow", "crow-intent"].contains(&text(&state, "phase").as_str()) {
            return use_crow(config, args, api, &mut prepared, plan, &path, &state);
        }
        run = matching(github.as_ref(), &contract, &commit, &key)?;
    }
    let Some(run) = run else {
        return use_crow(config, args, api, &mut prepared, plan, &path, &state);
    };
    validate_run(&run, &contract, &commit, &key, &args.branch)?;
    if state
        .get("run_id")
        .is_some_and(|id| !id.is_null() && *id != run["id"])
    {
        return Err("Hosted run differs from persisted dispatch".into());
    }
    if state.get("run_id").is_none() {
        state["phase"] = json!(if state["phase"] == "gha-intent" {
            "gha"
        } else {
            "observed"
        });
        state["run_id"] = run["id"].clone();
        save(&path, &state)?;
    }
    let action = next_action(
        &state,
        Some(&run),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs_f64(),
        args.queue_timeout as f64,
    );
    if action == "crow" {
        let jobs = github.pages(
            &format!(
                "/repos/{}/actions/runs/{}/jobs?filter=all",
                text(&contract["github"], "repository"),
                number(&run, "id")
            ),
            "jobs",
        )?;
        if jobs_may_have_started(&json!(jobs), number(&run, "id"), &commit) {
            return Err("Cancelled hosted run may have started; no fallback".into());
        }
        state["phase"] = json!("gha-retired");
        state["retired_run"] = run["id"].clone();
        save(&path, &state)?;
        return use_crow(config, args, api, &mut prepared, plan, &path, &state);
    }
    if action == "result" {
        if run["conclusion"] != "success" {
            return Err("Hosted check did not succeed; this is not provider unavailability".into());
        }
        if !overrides.is_empty() {
            return use_crow(config, args, api, &mut prepared, plan, &path, &state);
        }
        let mut current = portable_tool(config, github.as_ref(), &contract)?;
        current["tool_bootstrap_sha256"] = contract["github"]["tool_bootstrap_sha256"].clone();
        if TOOL_INPUTS
            .iter()
            .any(|k| state["inputs"][*k] != current[*k])
        {
            return Err("Portable asset changed since hosted run; use --rerun".into());
        }
        let mut expected = json!({"source_commit":commit,"checks":checks.join(","),"request_id":key,"dependency_snapshot":contract["dependency_snapshot"],"tool_revision":contract["github"]["tool_revision"],"tool_bootstrap_sha256":contract["github"]["tool_bootstrap_sha256"],"manifest_sha256":contract["hashes"][".ci/ccid.toml"],"config_sha256":contract["hashes"][CONFIG]});
        expected
            .as_object_mut()
            .ok_or("Receipt identity missing")?
            .extend(
                state["inputs"]
                    .as_object()
                    .ok_or("Recorded tool inputs missing")?
                    .clone(),
            );
        let artifacts = github.pages(
            &format!(
                "/repos/{}/actions/runs/{}/artifacts",
                text(&contract["github"], "repository"),
                number(&run, "id")
            ),
            "artifacts",
        )?;
        let selected: Vec<_> = artifacts
            .iter()
            .filter(|a| a["name"] == format!("ccid-result-{key}") && a["expired"] != true)
            .collect();
        if selected.len() != 1 {
            return Err("Hosted success lacks one current receipt".into());
        }
        let receipt = github::result_receipt(
            &github
                .as_ref()
                .artifact(&text(&contract["github"], "repository"), selected[0])?,
            &expected,
        )?;
        return emit(
            &json!({"provider":"github","action":"result","run_id":run["id"],"conclusion":run["conclusion"],"url":run["html_url"],"request_id":key,"dependency_snapshot":contract["dependency_snapshot"],"runtime":core::pick(&receipt,&["rustc","node","bun"])}),
        );
    }
    if action == "uncertain" {
        return Err("Hosted state unknown; no fallback".into());
    }
    // During the freeze even owned queued work stays read-only. Never send a
    // cancellation merely to enable fallback; subsequent calls can reconcile it.
    emit(
        &json!({"provider":"github","action":"attached","run_id":run["id"],"status":run["status"],"request_id":key,"reason":"GitHub is frozen; existing work remains read-only","resume":"Repeat the same command to continue monitoring"}),
    )
}

use super::core::Api;
use super::*;
use cli::{JobAction, JobArgs, SubmitArgs};

pub(super) fn identity(request: &Value) -> Result<String> {
    Ok(sha(encode(request)?))
}
pub(super) fn workflow(
    config: &Config,
    name: &str,
    request: &Value,
    binary: &str,
    digest: &str,
) -> Result<Value> {
    Ok(
        json!({"apiVersion":"argoproj.io/v1alpha1","kind":"Workflow","metadata":{"name":name,"namespace":config.argo_namespace,"annotations":{"ccid/source-commit":request["commit"],"ccid/request-sha256":identity(request)?}},"spec":{"workflowTemplateRef":{"name":config.argo_template},"arguments":{"parameters":[{"name":"request","value":encode(request)?},{"name":"binary","value":binary},{"name":"binary-sha256","value":digest}]}}}),
    )
}
fn kubectl(config: &Config, args: &[&str], body: Option<&Value>) -> Result<Value> {
    let mut argv = strings(&[
        "k3s",
        "kubectl",
        "-n",
        &config.argo_namespace,
        "--request-timeout=45s",
    ]);
    argv.extend(strings(args));
    let body = body.map(encode).transpose()?;
    let result = config.ssh(&argv, body.as_deref().map(str::as_bytes))?;
    if result.iter().all(u8::is_ascii_whitespace) {
        Ok(Value::Null)
    } else {
        Ok(serde_json::from_slice(&result)?)
    }
}
fn prior(config: &Config, record: &Value) -> Result<(String, Value)> {
    match text(record, "scheduler").as_str() {
        "argo" => {
            let run = kubectl(
                config,
                &[
                    "get",
                    "workflow",
                    &text(record, "name"),
                    "--ignore-not-found",
                    "-o",
                    "json",
                ],
                None,
            )?;
            if run.is_null() {
                return Ok(("unknown".into(), Value::Null));
            }
            if run["metadata"]["annotations"]["ccid/request-sha256"] != record["request"] {
                return Err("Argo workflow identity differs from saved request".into());
            }
            Ok((
                run["status"]["phase"].as_str().unwrap_or("Pending").into(),
                run,
            ))
        }
        "crow" => {
            if number(record, "number") == 0 {
                return Ok(("unknown".into(), Value::Null));
            }
            let run = core::Crow::new(config)?.call(
                &format!(
                    "/repos/{}/pipelines/{}",
                    number(record, "repo_id"),
                    number(record, "number")
                ),
                None,
            )?;
            if run["commit"] != record["commit"] {
                return Err("Crow run source differs from saved request".into());
            }
            Ok((text(&run, "status"), core::summarize(&run)))
        }
        _ => Err("Unknown saved job scheduler".into()),
    }
}
pub(super) fn admit_previous(phase: &str, rerun: bool) -> Result<()> {
    if ![
        "success",
        "failure",
        "error",
        "killed",
        "Succeeded",
        "Failed",
        "Error",
    ]
    .contains(&phase)
    {
        return Err(format!(
            "Existing job is {phase}; scheduler switching cannot duplicate unresolved work"
        )
        .into());
    }
    if !rerun {
        return Err("Matching completed job exists; inspect result or use --rerun".into());
    }
    Ok(())
}
fn planner(config: &Config, revision: &str, remote: &Value) -> Result<PathBuf> {
    let root = config.directory("ci-job-tools")?.join(revision);
    fs::create_dir_all(&root)?;
    let _lock = lock(&root.join("download.lock"))?;
    let binary = root.join("ccid");
    let expected = text(remote, "CI_TOOL_BINARY_SHA256");
    if binary.exists() && digest(&binary)? == expected {
        return Ok(binary);
    }
    let suffix = text(remote, "CI_TOOL_BINARY")
        .strip_prefix(&config.worker_tools)
        .ok_or("Planner path outside declared tool store")?
        .to_string();
    let bytes = config.ssh(
        &strings(&["cat", &format!("{}{suffix}", config.host_tools)]),
        None,
    )?;
    if sha(&bytes) != expected {
        return Err("Downloaded planner differs from verified receipt".into());
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&root)?;
    temporary.write_all(&bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(&binary)?;
    Ok(binary)
}
fn run(config: &Config, args: &JobArgs, plan_only: bool) -> Result<()> {
    let repo = args.repo.canonicalize()?;
    let (commit, ignored) = core::source_identity(&repo, &args.branch, true)?;
    let manifest = git(&repo, &["show", &format!("{commit}:.ci/ccid.toml")])?;
    let raw: toml::Value = toml::from_str(&manifest)?;
    let workflow_name = raw
        .get("jobs")
        .and_then(|j| j.get(&args.job))
        .and_then(|j| j.get("workflow"))
        .and_then(toml::Value::as_str)
        .filter(|w| name(w))
        .ok_or("Select committed job with plain Crow workflow name")?;
    let tool = pinned::identify(config, &repo, &commit, &[workflow_name.into()])?
        .filter(|t| t.binary)
        .ok_or("Job workflow must pin verified ccid binary")?;
    let remote = pinned::executable(config, &tool.revision)?;
    let binary = planner(config, &tool.revision, &remote)?;
    let mut variables = json!({});
    for item in &args.variables {
        let (key, value) = item
            .split_once('=')
            .ok_or("Job variables must be NAME=value")?;
        if !matches(r"^[A-Z][A-Z0-9_]*$", key) || variables.get(key).is_some() {
            return Err("Job variables must be unique uppercase entries".into());
        }
        if key.starts_with("CI_TOOL_")
            || key.starts_with("CROW_")
            || [
                "CI_COMMIT_SHA",
                "CI_REPOSITORY_URL",
                "CHECKS",
                "CCID_JOB_REQUEST",
            ]
            .contains(&key)
        {
            return Err("Source, tool and selection identity cannot be overridden".into());
        }
        variables[key] = json!(value);
    }
    let url = config.origin(&repo)?;
    let namespace = format!("job-{}", &sha(&url)[..24]);
    let temporary = tempfile::Builder::new()
        .prefix("ci-job-")
        .tempdir_in(config.directory("crow-ci-transfers")?)?;
    let root = temporary.path();
    fs::create_dir(root.join(".ci"))?;
    fs::write(root.join(".ci/ccid.toml"), manifest)?;
    let mut command = vec![
        binary.to_string_lossy().to_string(),
        "job".into(),
        "--repo".into(),
        root.to_string_lossy().to_string(),
        "--job".into(),
        args.job.clone(),
    ];
    if let Some(scheduler) = &args.scheduler {
        command.extend(["--scheduler".into(), scheduler.clone()]);
    }
    let plan: Value = serde_json::from_slice(&output(&command, None, None)?)?;
    let archive = root.join("source.tar");
    let bundle = root.join("source.bundle");
    let closure = archive::create(&repo, &commit, &archive)?;
    archive::bundle(&repo, &commit, &bundle)?;
    let hash = digest(&archive)?;
    let bundle_hash = digest(&bundle)?;
    variables
        .as_object_mut()
        .ok_or("Variables missing")?
        .extend(remote.as_object().ok_or("Tool identity missing")?.clone());
    variables["CI_REPOSITORY_URL"] = json!(url);
    let request = json!({"archive":format!("{}/{namespace}/{hash}.tar",config.worker_sources),"sha256":hash,"bundle":format!("{}/{namespace}/{bundle_hash}.bundle",config.worker_sources),"bundle_sha256":bundle_hash,"commit":commit,"tool_revision":tool.revision,"job":args.job,"environment":variables});
    let key = identity(&request)?;
    let state_root = config.directory("ci-job-state")?;
    let state_path = state_root.join(format!("{key}.json"));
    let mut info = plan.clone();
    info.as_object_mut().ok_or("Invalid planner output")?.extend(json!({"commit":commit,"request":key,"tool_revision":tool.revision,"source_closure":closure,"ignored_worktree_changes":ignored}).as_object().ok_or("Invalid job identity")?.clone());
    let _lock = lock(&state_root.join(format!("{key}.lock")))?;
    let previous: Value = if state_path.exists() {
        serde_json::from_slice(&fs::read(&state_path)?)?
    } else {
        Value::Null
    };
    if plan_only {
        info["previous"] = previous;
        return emit(&info);
    }
    if !previous.is_null() {
        let (phase, run) = prior(config, &previous)?;
        emit(&json!({"previous":run,"phase":phase}))?;
        admit_previous(&phase, args.rerun)?;
    }
    info["host_admission"] = core::admission(config)?;
    transport::stage(config, &archive, &namespace, &hash, "tar")?;
    transport::stage(config, &bundle, &namespace, &bundle_hash, "bundle")?;
    if core::source_identity(&repo, &args.branch, true)?.0 != commit {
        return Err("Source changed while staging; no work submitted".into());
    }
    let attempt = number(&previous, "attempt") + 1;
    let mut state =
        json!({"request":key,"commit":commit,"scheduler":plan["scheduler"],"attempt":attempt});
    match text(&plan, "scheduler").as_str() {
        "crow" => {
            let api = core::Crow::new(config)?;
            state["repo_id"] = core::resolve_repo(config, &api, &repo)?["id"].clone();
            let dispatch = SubmitArgs {
                repo,
                branch: args.branch.clone(),
                expect_commit: Some(commit),
                workflows: vec![text(&plan, "workflow")],
                variables: vec![format!("CCID_JOB_REQUEST={}", encode(&request)?)],
                provider: "crow".into(),
                provider_wait: 0,
                queue_timeout: 120,
                rerun: args.rerun,
                cached_rerun: false,
            };
            let pending = state.clone();
            submit::Prepared::new(config, &dispatch, &api)?.run(
                config,
                &dispatch,
                &api,
                false,
                &mut |_| save(&state_path, &pending),
                &mut |run| {
                    state["number"] = run["number"].clone();
                    save(&state_path, &state)
                },
            )?;
        }
        "argo" => {
            if kubectl(
                config,
                &[
                    "get",
                    "workflowtemplate",
                    &config.argo_template,
                    "-o",
                    "json",
                ],
                None,
            )?
            .is_null()
            {
                return Err("Declarative Argo WorkflowTemplate missing".into());
            }
            let name = format!("ccid-{}-{attempt}", &key[..24]);
            state["name"] = json!(name);
            save(&state_path, &state)?;
            let result = kubectl(
                config,
                &["create", "-f", "-", "-o", "json"],
                Some(&workflow(
                    config,
                    &name,
                    &request,
                    &text(&remote, "CI_TOOL_BINARY"),
                    &text(&remote, "CI_TOOL_BINARY_SHA256"),
                )?),
            )?;
            info["action"] = json!("submitted");
            info["name"] = result["metadata"]["name"].clone();
            emit(&info)?;
        }
        _ => return Err("Unknown planned scheduler".into()),
    }
    state["state_file"] = json!(state_path);
    emit(&state)
}
pub fn run_cli(action: JobAction, path: Option<&Path>) -> Result<()> {
    let config = Config::load(path)?;
    match action {
        JobAction::Plan(args) => run(&config, &args, true),
        JobAction::Run(args) => run(&config, &args, false),
        action => {
            let (state, logs) = match action {
                JobAction::Status { state } => (state, false),
                JobAction::Logs { state } => (state, true),
                _ => return Err("Invalid job operation".into()),
            };
            let record: Value = serde_json::from_slice(&fs::read(state)?)?;
            let (phase, run) = prior(&config, &record)?;
            if !logs {
                emit(&json!({"phase":phase,"run":run}))
            } else if record["scheduler"] == "argo" {
                let argv = strings(&[
                    "k3s",
                    "kubectl",
                    "-n",
                    &config.argo_namespace,
                    "logs",
                    "-l",
                    &format!("workflows.argoproj.io/workflow={}", text(&record, "name")),
                    "-c",
                    "main",
                    "--tail=-1",
                ]);
                let data = config.ssh(&argv, None)?;
                std::io::stdout().write_all(&data)?;
                Ok(())
            } else {
                emit(
                    &json!({"run":run,"logs_command":format!("crow-ci logs {} {} STEP_ID",number(&record,"repo_id"),number(&record,"number"))}),
                )
            }
        }
    }
}

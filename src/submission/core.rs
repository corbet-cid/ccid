use super::*;

pub(super) const ACTIVE: &[&str] = &["pending", "running", "blocked", "waiting", "started"];
pub(super) const TERMINAL: &[&str] = &[
    "success", "failure", "killed", "error", "declined", "skipped",
];
pub(super) const BUDGETS: &[&str] = &[
    "CI_JOBS",
    "CI_TEST_THREADS",
    "CI_MEMORY_MB",
    "CI_MEMORY_PER_JOB_MB",
    "CI_TIMEOUT",
    "CI_CACHE_ROOT",
    "CI_NIX_JOBS",
    "CI_LINKER",
    "CI_MIN_AVAILABLE_MB",
];

pub(super) fn canonical_remote(origin: &str, aliases: &BTreeMap<String, String>) -> Result<String> {
    if let Some((_, tail)) = origin.split_once("://") {
        if let Some((_, raw_path)) = tail.split_once('/') {
            if raw_path
                .split(['?', '#'])
                .next()
                .unwrap_or("")
                .split('/')
                .any(|p| p == "." || p == "..")
            {
                return Err("Invalid canonical forge identity".into());
            }
        }
    }
    let (mut host, path) = if origin.contains("://") {
        let parsed = url::Url::parse(origin)?;
        if !["https", "http", "ssh"].contains(&parsed.scheme()) {
            return Err("Unsupported forge transport".into());
        }
        let host = parsed.host_str().ok_or("Forge host missing")?;
        let default = match parsed.scheme() {
            "ssh" => 22,
            "http" => 80,
            _ => 443,
        };
        (
            match parsed.port() {
                Some(p) if p != default => format!("{host}:{p}"),
                _ => host.to_string(),
            },
            parsed.path().to_string(),
        )
    } else {
        let (authority, path) = origin
            .split_once(':')
            .ok_or("Repository origin is not a forge remote")?;
        (
            authority.rsplit('@').next().unwrap_or("").to_lowercase(),
            path.to_string(),
        )
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if !matches(r"^[A-Za-z0-9.-]+(?::[0-9]+)?$", &host)
        || !matches(r"^[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)+$", path)
        || path.split('/').any(|p| p == "." || p == "..")
    {
        return Err("Invalid canonical forge identity".into());
    }
    host.make_ascii_lowercase();
    if let Some(alias) = aliases.get(&host) {
        host = alias.clone();
    }
    Ok(format!("https://{host}/{path}"))
}
pub(super) fn source_identity(repo: &Path, branch: &str, local: bool) -> Result<(String, usize)> {
    let commit = git(repo, &["rev-parse", "HEAD^{commit}"])?;
    let branch_ref = format!("refs/heads/{branch}");
    git(repo, &["check-ref-format", &branch_ref])?;
    if !local
        && git(repo, &["ls-remote", "--exit-code", "origin", &branch_ref])?
            .split_whitespace()
            .next()
            != Some(&commit)
    {
        return Err(
            "Local HEAD must match the published branch; commit and push owned changes first"
                .into(),
        );
    }
    Ok((
        commit,
        git(repo, &["status", "--porcelain"])?.lines().count(),
    ))
}
pub(super) fn summarize(run: &Value) -> Value {
    let mut value = pick(
        run,
        &[
            "number", "status", "commit", "created", "started", "finished",
        ],
    );
    value["workflows"] = rows(&run["workflows"])
        .iter()
        .map(|w| {
            let mut selected = pick(w, &["id", "attempt", "name", "state"]);
            selected["steps"] = rows(&w["children"])
                .iter()
                .map(|s| pick(s, &["id", "name", "state", "exit_code"]))
                .collect();
            selected
        })
        .collect();
    value
}
pub(super) fn pick(value: &Value, keys: &[&str]) -> Value {
    keys.iter()
        .map(|k| ((*k).to_string(), value[*k].clone()))
        .collect()
}
pub(super) fn assess(snapshot: &str, minimum: u64) -> Result<Value> {
    let re = regex::Regex::new(r"(?m)^MemAvailable:\s+(\d+)\s+kB$")?;
    let available: u64 = re
        .captures(snapshot)
        .ok_or("Cannot establish host memory headroom")?[1]
        .parse::<u64>()?
        / 1024;
    if available < minimum {
        return Err(format!(
            "Host has {available} MiB available; {minimum} MiB required before new CI"
        )
        .into());
    }
    let re = regex::Regex::new(r"(?m)^full\s+avg10=([0-9.]+)\b")?;
    let pressure = re
        .captures(snapshot)
        .map(|c| c[1].parse::<f64>())
        .transpose()?;
    if pressure.is_some_and(|p| !p.is_finite() || p >= 5.0) {
        return Err("Host has sustained full memory pressure; no work submitted".into());
    }
    Ok(json!({"available_mb":available,"minimum_mb":minimum,"full_memory_psi_avg10":pressure}))
}
pub(super) fn admission(config: &Config) -> Result<Value> {
    assess(
        &String::from_utf8(config.ssh(
            &strings(&["cat", "/proc/meminfo", "/proc/pressure/memory"]),
            None,
        )?)?,
        8192,
    )
}

pub(super) trait Api {
    fn call(&self, path: &str, body: Option<&Value>) -> Result<Value>;
}
pub(super) struct Crow {
    api: String,
    token: String,
}
impl Crow {
    pub fn new(config: &Config) -> Result<Self> {
        let token = String::from_utf8(output(&config.token_command, None, None)?)?
            .trim()
            .to_owned();
        if token.is_empty() || token.contains(['\r', '\n']) {
            return Err("Invalid Crow credential".into());
        }
        Ok(Self {
            api: config.api.clone(),
            token,
        })
    }
    pub fn redact(&self, data: &str) -> Result<String> {
        let data = data.replace(&self.token, "[redacted]");
        let data =
            regex::Regex::new(r"(https?://)[^\s/@]+@")?.replace_all(&data, "${1}[redacted]@");
        Ok(
            regex::Regex::new(r"(?i)([?&](?:token|sig|signature|key|secret)=)[^\s&]+")?
                .replace_all(&data, "${1}[redacted]")
                .into_owned(),
        )
    }
}
impl Api for Crow {
    fn call(&self, path: &str, body: Option<&Value>) -> Result<Value> {
        if !path.starts_with('/') || path.contains(['\r', '\n', '#']) {
            return Err("Invalid Crow API path".into());
        }
        let response = transport::http(
            &format!("{}{path}", self.api),
            Some(&self.token),
            body,
            8 * 1024 * 1024,
        )?;
        if !(200..300).contains(&response.status) {
            return Err(format!("Crow request failed: HTTP {}", response.status).into());
        }
        if response.data.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&response.data)
            .map_err(|_| "Crow response unavailable; inspect existing runs before retrying".into())
    }
}
pub(super) fn pages(api: &dyn Api, path: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut previous = Vec::new();
    for page in 1..=100 {
        let value = api.call(
            &format!(
                "{path}{}page={page}&perPage=50",
                if path.contains('?') { "&" } else { "?" }
            ),
            None,
        )?;
        let batch = value
            .as_array()
            .ok_or("Crow returned invalid pagination metadata")?;
        let ids: Vec<Value> = batch
            .iter()
            .map(|r| r.get("id").unwrap_or(&r["number"]).clone())
            .collect();
        if !batch.is_empty() && ids == previous {
            return Err("Crow repeated a page; refusing incomplete inventory".into());
        }
        out.extend(batch.clone());
        if batch.len() < 50 {
            return Ok(out);
        }
        previous = ids;
    }
    Err("Crow pagination exceeded supported inventory size".into())
}
pub(super) fn resolve_repo(config: &Config, api: &dyn Api, repo: &Path) -> Result<Value> {
    let identity = config.origin(repo)?;
    let selected: Vec<_> = pages(api, "/repos?active=true")?
        .into_iter()
        .filter(|r| {
            r["active"] == true
                && ["clone_url", "clone_url_ssh"].iter().any(|k| {
                    canonical_remote(&text(r, k), &config.origin_aliases)
                        .ok()
                        .as_ref()
                        == Some(&identity)
                })
        })
        .collect();
    if selected.len() != 1 {
        return Err(format!("Expected one active Crow repository for {identity}; resolve its full forge origin first").into());
    }
    Ok(selected[0].clone())
}
pub(super) fn transport_key(key: &str) -> bool {
    ["SOURCE_ARCHIVE", "CI_TOOL_ARCHIVE", "CI_TOOL_BINARY"].contains(&key)
        || key.ends_with("_SOURCE_ARCHIVE")
        || key.ends_with("_SOURCE_BUNDLE")
}
pub(super) fn variables_identity(value: &Value) -> Value {
    value
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| !transport_key(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or(Value::Null)
}
pub(super) fn selected(run: &Value) -> BTreeSet<String> {
    rows(&run["workflows"])
        .iter()
        .map(|w| text(w, "name"))
        .collect()
}
pub(super) fn matching_runs(
    api: &dyn Api,
    repo: u64,
    commit: &str,
    variables: &Value,
    workflows: &[String],
    branch: &str,
) -> Result<Vec<Value>> {
    let mut matched = Vec::new();
    for run in pages(api, &format!("/repos/{repo}/pipelines?event=manual"))? {
        if run["commit"] != commit || run["event"] != "manual" {
            continue;
        }
        let detail = api.call(
            &format!("/repos/{repo}/pipelines/{}", number(&run, "number")),
            None,
        )?;
        if variables_identity(&detail["variables"]) == variables_identity(variables)
            && selected(&detail) == workflows.iter().cloned().collect()
            && detail["branch"] == branch
        {
            matched.push(detail);
        }
    }
    Ok(matched)
}
pub(super) fn cached_restart(
    prior: &Value,
    commit: &str,
    branch: &str,
    variables: &Value,
    workflows: &[String],
) -> Result<Value> {
    if !TERMINAL.contains(&text(prior, "status").as_str())
        || prior["branch"] != branch
        || prior["commit"] != commit
        || !prior["variables"].is_object()
        || rows(&prior["workflows"]).is_empty()
        || rows(&prior["workflows"])
            .iter()
            .any(|w| text(w, "name").is_empty())
        || selected(prior) != workflows.iter().cloned().collect()
    {
        return Err(
            "Cached restart requires an exact terminal stored run and persisted workflow config"
                .into(),
        );
    }
    for (key, value) in variables.as_object().ok_or("Variables must be an object")? {
        if transport_key(key) && prior["variables"][key] != *value {
            return Err(format!("Cached restart archive path differs for {key}").into());
        }
    }
    Ok(
        json!({"restart_of":prior["number"],"restart_mode":"stored-config","source_sha256":variables["SOURCE_SHA256"],"ccid_sha256":variables["CI_TOOL_SHA256"]}),
    )
}

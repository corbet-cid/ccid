use super::*;

pub(super) fn memory_defaults(source: &str) -> Result<BTreeMap<String, String>> {
    let mut values: BTreeMap<_, _> = [
        ("CI_MEMORY_MB", "16384"),
        ("CI_MEMORY_PER_JOB_MB", "2048"),
        ("CI_MIN_AVAILABLE_MB", "8192"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    let lines: Vec<_> = source.lines().collect();
    let starts: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| **l == "variables:")
        .map(|(i, _)| i)
        .collect();
    if starts.is_empty() {
        return Ok(values);
    }
    if starts.len() != 1 {
        return Err("Expected one Crow variables section".into());
    }
    let mut seen = BTreeSet::new();
    for line in &lines[starts[0] + 1..] {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            break;
        }
        for key in [
            "CI_MEMORY_MB",
            "CI_MEMORY_PER_JOB_MB",
            "CI_MIN_AVAILABLE_MB",
        ] {
            if !line.starts_with(&format!("  {key}:")) {
                continue;
            }
            let re = regex::Regex::new(&format!(
                r#"^  {key}:\s*\{{default:\s*(?:"([0-9]*)"|'([0-9]*)'|([0-9]+))\s*\}}\s*(?:#.*)?$"#
            ))?;
            let captures = re
                .captures(line)
                .ok_or("Memory default must be one literal numeric value")?;
            if !seen.insert(key) {
                return Err("Duplicate memory default".into());
            }
            let value = (1..=3)
                .find_map(|i| captures.get(i))
                .ok_or("Missing memory default")?
                .as_str();
            if !value.is_empty() {
                let value: u64 = value.parse()?;
                if value == 0 {
                    return Err("Memory default must be positive".into());
                }
                values.insert(key.into(), value.to_string());
            }
        }
    }
    Ok(values)
}
pub(super) struct Tool {
    pub revision: String,
    pub binary: bool,
    pub repository_identity: bool,
    pub memory_admission: bool,
    pub defaults: BTreeMap<String, BTreeSet<String>>,
}
pub(super) fn identify(
    config: &Config,
    repo: &Path,
    commit: &str,
    workflows: &[String],
) -> Result<Option<Tool>> {
    let paths = git(repo, &["ls-tree", "-r", "--name-only", commit, ".crow"])?;
    let paths: BTreeSet<_> = paths.lines().collect();
    let mut pins = BTreeSet::new();
    let mut tool = Tool {
        revision: String::new(),
        binary: false,
        repository_identity: false,
        memory_admission: false,
        defaults: BTreeMap::new(),
    };
    let pin = regex::Regex::new(r#"(?m)^\s+CCID_REVISION:\s*['"]?([0-9a-f]{40})['"]?\s*$"#)?;
    for workflow in workflows {
        for extension in ["yaml", "yml"] {
            let path = format!(".crow/{workflow}.{extension}");
            if !paths.contains(path.as_str()) {
                continue;
            }
            let source = git(repo, &["show", &format!("{commit}:{path}")])?;
            if !source.contains("CI_TOOL_ARCHIVE") {
                continue;
            }
            let matches: Vec<_> = pin.captures_iter(&source).collect();
            if matches.len() != 1 {
                return Err(
                    format!("{path} must declare exactly one literal CCID_REVISION").into(),
                );
            }
            pins.insert(matches[0][1].to_string());
            tool.binary |= source.contains("CI_TOOL_BINARY");
            tool.repository_identity |= source
                .lines()
                .any(|l| l.starts_with("  CI_REPOSITORY_URL:"));
            tool.memory_admission |= source
                .lines()
                .any(|l| l.starts_with("  CI_MIN_AVAILABLE_MB:"));
            for (k, v) in memory_defaults(&source)? {
                tool.defaults.entry(k).or_default().insert(v);
            }
        }
    }
    if pins.is_empty() {
        return Ok(None);
    }
    if pins.len() != 1 {
        return Err("Selected workflows pin different ccid revisions".into());
    }
    if !config
        .tool_origins
        .contains(&config.origin(&config.tool_repo)?)
    {
        return Err("Shared tool checkout must have a canonical ccid origin".into());
    }
    tool.revision = pins.into_iter().next().ok_or("Missing tool revision")?;
    git(
        &config.tool_repo,
        &["cat-file", "-e", &format!("{}^{{commit}}", tool.revision)],
    )?;
    if git(&config.tool_repo, &["ls-tree", "-r", &tool.revision])?
        .lines()
        .any(|l| l.starts_with("160000 "))
    {
        return Err("Shared tool archive cannot contain unresolved submodules".into());
    }
    Ok(Some(tool))
}
pub(super) fn executable(config: &Config, revision: &str) -> Result<Value> {
    if !exact_sha(revision) {
        return Err("Invalid tool revision".into());
    }
    let target = "x86_64-unknown-linux-gnu";
    let suffix = format!("/ccid/{revision}/{target}");
    let args = vec![
        config.remote_binary.clone(),
        "crow-ci".into(),
        "binary-receipt".into(),
        "--root".into(),
        format!("{}{suffix}", config.host_tools),
        "--revision".into(),
        revision.into(),
        "--target".into(),
        target.into(),
    ];
    let receipt: Value = serde_json::from_slice(&config.ssh(&args, None)?)?;
    if !exact_digest(&text(&receipt, "binary_sha256"))
        || receipt["source_revision"] != revision
        || receipt["target"] != target
    {
        return Err("Invalid pinned tool executable receipt".into());
    }
    Ok(
        json!({"CI_TOOL_BINARY":format!("{}{suffix}/ccid",config.worker_tools),"CI_TOOL_BINARY_SHA256":receipt["binary_sha256"]}),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Declaration {
    #[serde(skip)]
    pub name: String,
    pub revision: String,
    pub archive_variable: String,
    pub digest_variable: String,
    #[serde(default = "archive_kind")]
    pub kind: String,
    pub workflows: Option<Vec<String>>,
}
fn archive_kind() -> String {
    "archive".into()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Declarations {
    schema: u32,
    archives: BTreeMap<String, Declaration>,
}
pub(super) fn declarations(raw: &str) -> Result<Vec<Declaration>> {
    let config: Declarations = toml::from_str(raw)?;
    if config.schema != 1 {
        return Err("Invalid archives schema".into());
    }
    let mut variables = BTreeSet::new();
    let mut result = Vec::new();
    for (name, mut item) in config.archives {
        if !matches(r"^[a-z][a-z0-9_-]{0,47}$", &name)
            || !["archive", "git-bundle"].contains(&item.kind.as_str())
            || !(exact_sha(&item.revision)
                || item.kind == "git-bundle" && item.revision == "source")
        {
            return Err("Invalid pinned source declaration".into());
        }
        if let Some(workflows) = &item.workflows {
            if workflows.is_empty()
                || workflows.iter().any(|w| !super::name(w))
                || workflows.iter().collect::<BTreeSet<_>>().len() != workflows.len()
            {
                return Err("Pinned source workflows must be unique plain names".into());
            }
        }
        for (key, suffix) in [
            (
                &item.archive_variable,
                if item.kind == "git-bundle" {
                    "_SOURCE_BUNDLE"
                } else {
                    "_SOURCE_ARCHIVE"
                },
            ),
            (&item.digest_variable, "_SOURCE_SHA256"),
        ] {
            if !matches(r"^[A-Z][A-Z0-9_]*$", key)
                || !key.ends_with(suffix)
                || key.starts_with("CI_")
                || key.starts_with("CROW_")
                || !variables.insert(key.clone())
            {
                return Err(
                    "Pinned source variables must be unique and cannot override CI identity".into(),
                );
            }
        }
        item.name = name;
        result.push(item);
    }
    Ok(result)
}
pub(super) struct Source {
    pub info: Value,
    pub archive: PathBuf,
    pub namespace: String,
    pub digest: String,
    pub extension: String,
}
pub(super) fn prepare(
    config: &Config,
    repo: &Path,
    commit: &str,
    temporary: &Path,
    repo_id: u64,
    variables: &mut Value,
    workflows: &[String],
) -> Result<Vec<Source>> {
    let path = ".ci/archives.toml";
    if git(repo, &["ls-tree", "--name-only", commit, "--", path])?.is_empty() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for mut item in declarations(&git(repo, &["show", &format!("{commit}:{path}")])?)? {
        if variables.get(&item.archive_variable).is_some()
            || variables.get(&item.digest_variable).is_some()
        {
            return Err("Declared pinned source identities cannot be overridden".into());
        }
        if item
            .workflows
            .as_ref()
            .is_some_and(|w| !w.iter().any(|w| workflows.contains(w)))
        {
            continue;
        }
        if item.revision == "source" {
            item.revision = commit.into();
        }
        let extension = if item.kind == "git-bundle" {
            "bundle"
        } else {
            "tar"
        };
        let archive = temporary.join(format!("source-{}.{extension}", item.name));
        if item.kind == "git-bundle" {
            archive::bundle(repo, &item.revision, &archive)?;
        } else {
            archive::create(repo, &item.revision, &archive)?;
        }
        let hash = digest(&archive)?;
        let namespace = format!("{repo_id}-source-{}", item.name);
        variables[&item.archive_variable] = json!(format!(
            "{}/{namespace}/{hash}.{extension}",
            config.worker_sources
        ));
        variables[&item.digest_variable] = json!(hash);
        let info = json!({"name":item.name,"kind":item.kind,"revision":item.revision,"sha256":hash,"bytes":archive.metadata()?.len()});
        result.push(Source {
            info,
            archive,
            namespace,
            digest: hash,
            extension: extension.into(),
        });
    }
    Ok(result)
}

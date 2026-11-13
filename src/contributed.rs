//! Contributed projects (operating model R10, R11).
//!
//! A contributed project is someone else's project that we contribute to. Its own CI and conventions apply, nothing
//! of ours enters its repository, its CI never runs on our compute, and its releases and issues are not mirrored or
//! relayed. We may use our own compute and caches for our own work on it.
//!
//! The class is declared **outside** the project: the placement document (`lib/placement.json`, the one declared
//! placement of the fleet) lists the projects in a `contributed` list of its native view. ccid reads a copy that
//! `CCID_PLACEMENT_FILE` names and never writes it. See `docs/contributed.md`.
//!
//! Everything here is a refusal or a guard, except [`check`], the way to run a project's checks as our own work.
use crate::{event, failure, value, Environment, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// The environment variable that names a copy of the placement document.
pub const PLACEMENT_FILE_ENV: &str = "CCID_PLACEMENT_FILE";

/// The placement is a few hundred kilobytes; a file far beyond that is not one.
const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Variables that describe OUR infrastructure or carry a credential of a forge or registry. A contributed check runs
/// the project's own commands, so none of these reaches them: the result caches (they key committed content, and a
/// contributed project has no committed manifest of ours), forge and registry tokens, the fleet and channel data,
/// the runner's tool root and the job's repository identity. A list, not a rule: the rest of the environment is the
/// developer's own, as it is for any command run by hand.
const OURS: &[&str] = &[
    // Forge and registry credentials: a project's build scripts and tests are third-party code.
    "BITBUCKET_TOKEN",
    "CARGO_REGISTRY_TOKEN",
    "CCID_STATUS_BINARY",
    "CCID_STATUS_BINARY_SHA256",
    "CCID_SUBMISSION_CONFIG",
    "FORGEJO_TOKEN",
    "GH_TOKEN",
    "GITEA_TOKEN",
    "GITHUB_TOKEN",
    "GITLAB_TOKEN",
    "NODE_AUTH_TOKEN",
    "NPM_TOKEN",
    "CCID_CHANNEL_FILE",
    "CCID_CHANNEL_URL",
    "CCID_FLEET_DIGEST",
    "CCID_FLEET_FILE",
    "CCID_FLEET_KEYED",
    "CCID_FORGE_HOSTS",
    "CCID_FORGE_TOKEN",
    "CCID_REMOTE_CACHE",
    "CCID_RESULT_CACHE",
    "CCID_TOOL_ROOT",
    "CCID_TOOL_TRIPLE",
    "CI_REPOSITORY_URL",
    PLACEMENT_FILE_ENV,
];
/// Prefix of the forge tooling's own variables (credentials, status state).
const OURS_PREFIX: &str = "CFRG_";

/// Variables a forge's CI sets in a job. Their presence means a CI job is running this command.
const CI_JOB: &[&str] = &[
    "BITBUCKET_BUILD_NUMBER",
    "CIRCLECI",
    "FORGEJO_ACTIONS",
    "GITEA_ACTIONS",
    "GITHUB_ACTIONS",
    "GITLAB_CI",
];

/// One declared contributed project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// A plain lowercase name, unique in the list.
    pub name: String,
    /// The project's own repository (canonical `https://host/owner/repo`).
    pub upstream: String,
    /// Other locations of the same project: our copies on any forge (ours under every host spelling it is cloned by), or a second
    /// home of the project itself.
    pub forks: Vec<String>,
}

/// The contributed projects of one placement document.
#[derive(Debug)]
pub struct Catalog {
    projects: Vec<Project>,
    /// Lowercase `host/path` of every location → index of its project.
    keys: BTreeMap<String, usize>,
    /// Every host spelling of our forge: a location on one of them is also known under all the others.
    forge_hosts: Vec<String>,
    digest: String,
}

impl Catalog {
    /// Read the `contributed` list of a placement document: the whole `lib/placement.json` or its native view.
    /// `extra_forge_hosts` are more spellings of our forge (the fleet data's). Unknown fields are refused.
    pub fn parse(bytes: &[u8], extra_forge_hosts: &[String]) -> Result<Self> {
        let document: Value = serde_json::from_slice(bytes)
            .map_err(|error| failure(format!("placement is not valid JSON: {error}")))?;
        let view = document.get("native").unwrap_or(&document);
        if !view.get("repositories").is_some_and(Value::is_array) {
            return Err(failure(
                "not a placement: no `repositories` list (this is neither lib/placement.json nor its native view)",
            ));
        }
        let mut forge_hosts: Vec<String> = Vec::new();
        let mut add_host = |host: Option<String>| {
            if let Some(host) = host {
                if !forge_hosts.contains(&host) {
                    forge_hosts.push(host);
                }
            }
        };
        // Our forge is the placement's source. (`default` names the default PRIMARY, which a primary switch can
        // point at another forge: it is no spelling of ours.)
        add_host(
            view.pointer("/source/origin")
                .and_then(Value::as_str)
                .and_then(|url| key_of(&format!("{}/x/y", url.trim_end_matches('/'))))
                .and_then(|key| key.split('/').next().map(str::to_owned)),
        );
        for host in extra_forge_hosts {
            add_host(Some(host.to_ascii_lowercase()));
        }
        let list = match view.get("contributed") {
            None => Vec::new(),
            Some(Value::Array(list)) => list.clone(),
            Some(_) => return Err(failure("placement `contributed` must be a list")),
        };
        let mut projects: Vec<Project> = Vec::new();
        let mut keys: BTreeMap<String, usize> = BTreeMap::new();
        for (index, item) in list.iter().enumerate() {
            let project = project_of(item, index)?;
            if let Some(previous) = projects.last() {
                if previous.name >= project.name {
                    return Err(failure(format!(
                        "placement `contributed`: {} must come after {} (strictly ascending names)",
                        project.name, previous.name
                    )));
                }
            }
            let locations = std::iter::once(&project.upstream).chain(&project.forks);
            for location in locations {
                let key = canonical_key(location).map_err(|error| {
                    failure(format!("placement `contributed` {}: {error}", project.name))
                })?;
                let host = key.split('/').next().unwrap_or_default().to_owned();
                let spellings: Vec<String> = if forge_hosts.contains(&host) {
                    let path = &key[host.len()..];
                    forge_hosts.iter().map(|h| format!("{h}{path}")).collect()
                } else {
                    vec![key]
                };
                for spelling in spellings {
                    if let Some(other) = keys.insert(spelling.clone(), index) {
                        if other != index {
                            return Err(failure(format!(
                                "placement `contributed`: {spelling} belongs to both {} and {}",
                                projects[other].name, project.name
                            )));
                        }
                    }
                }
            }
            projects.push(project);
        }
        let mut canonical = String::new();
        for project in &projects {
            canonical.push_str(&format!("{}\n{}\n", project.name, project.upstream));
            let mut forks = project.forks.clone();
            forks.sort();
            for fork in forks {
                canonical.push_str(&format!("{fork}\n"));
            }
            canonical.push('\n');
        }
        let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        Ok(Self {
            projects,
            keys,
            forge_hosts,
            digest,
        })
    }

    /// The catalog of the placement copy `CCID_PLACEMENT_FILE` names, or `None` when the variable is not set (a
    /// host that classifies nothing). A file that is set but unreadable or invalid is an error: the guard never
    /// silently turns itself off.
    pub fn load(environment: &Environment) -> Result<Option<Self>> {
        match environment.get(std::ffi::OsStr::new(PLACEMENT_FILE_ENV)) {
            None => return Ok(None),
            Some(set) if set.is_empty() => {
                return Err(failure(format!(
                    "{PLACEMENT_FILE_ENV} is set but empty: unset it, or name the placement copy"
                )))
            }
            Some(_) => {}
        }
        let path = PathBuf::from(value(environment, PLACEMENT_FILE_ENV).unwrap_or_default());
        let bytes = read_bounded(&path).map_err(|error| {
            failure(format!("{PLACEMENT_FILE_ENV} {}: {error}", path.display()))
        })?;
        let extra = forge_host_spellings(environment)?;
        Self::parse(&bytes, &extra)
            .map(Some)
            .map_err(|error| failure(format!("{error} ({PLACEMENT_FILE_ENV} {})", path.display())))
    }

    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn forge_hosts(&self) -> &[String] {
        &self.forge_hosts
    }

    /// The project a repository URL belongs to, in any spelling git accepts (https, ssh, scp-like, `.git`).
    pub fn classify_url(&self, url: &str) -> Option<&Project> {
        self.keys
            .get(&key_of(url)?)
            .map(|index| &self.projects[*index])
    }

    /// The project of `owner/name` on the forge at `server_url` (the form a forge job carries).
    pub fn classify_repository(&self, server_url: &str, repository: &str) -> Option<&Project> {
        self.classify_url(&format!(
            "{}/{repository}",
            server_url.trim_end_matches('/')
        ))
    }

    /// The project of `owner/name` on OUR forge, in any of its host spellings (the form a scheduler reports).
    pub fn classify_forge_path(&self, path: &str) -> Option<&Project> {
        self.forge_hosts
            .iter()
            .find_map(|host| self.classify_url(&format!("https://{host}/{path}")))
    }

    /// The project a working copy belongs to: any of its remotes, or the job's `CI_REPOSITORY_URL`.
    pub fn classify_working_copy(
        &self,
        repo: &Path,
        environment: &Environment,
    ) -> Result<Option<&Project>> {
        let mut urls = remote_urls(repo)?;
        urls.extend(value(environment, "CI_REPOSITORY_URL"));
        // The Actions spelling of the same fact (a Forgejo runner sets it as GitHub's do).
        if let (Some(server), Some(repository)) = (
            value(environment, "GITHUB_SERVER_URL"),
            value(environment, "GITHUB_REPOSITORY"),
        ) {
            urls.push(format!("{}/{repository}", server.trim_end_matches('/')));
        }
        Ok(urls.iter().find_map(|url| self.classify_url(url)))
    }
}

/// More spellings of our forge than the placement names: the fleet data's (when `CCID_FLEET_FILE` is set: a copy that
/// is unreadable, not JSON or carries a fleet member that does not validate is an error, never a quietly smaller list; a
/// channel without a fleet member is an ordinary state and names no more spellings) and this machine's
/// `CCID_FORGE_HOSTS` (an in-cluster name a runner reaches the forge by).
fn forge_host_spellings(environment: &Environment) -> Result<Vec<String>> {
    let mut hosts: Vec<String> = Vec::new();
    if let Some(path) = value(environment, crate::fleet::FLEET_FILE_ENV) {
        let path = PathBuf::from(path);
        let unusable = |error: &dyn std::fmt::Display| {
            failure(format!(
                "{} {}: {error}: the fleet data names the spellings of our forge that {PLACEMENT_FILE_ENV} needs",
                crate::fleet::FLEET_FILE_ENV,
                path.display()
            ))
        };
        let bytes = crate::fleet::read_regular(&path).map_err(|error| unusable(&error))?;
        let document: Value = serde_json::from_slice(&bytes).map_err(|error| unusable(&error))?;
        if document.get("fleet").is_some() {
            let fleet = crate::fleet::from_channel(&bytes).map_err(|error| unusable(&error))?;
            hosts.extend(fleet.all_forge_hosts().map(str::to_owned));
        }
    }
    if let Some(list) = value(environment, crate::fleet::FORGE_HOSTS_ENV) {
        for host in list.split(',').map(str::trim).filter(|h| !h.is_empty()) {
            if !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b))
            {
                return Err(failure(format!(
                    "{}: {host:?} is not a host name",
                    crate::fleet::FORGE_HOSTS_ENV
                )));
            }
            hosts.push(host.to_ascii_lowercase());
        }
    }
    Ok(hosts)
}

/// The refusal for `operation` on `project`.
fn refusal(operation: &str, project: &Project) -> Box<dyn std::error::Error + Send + Sync> {
    failure(format!(
        "{operation} refused: {} is a contributed project (operating model R11): nothing of ours is written into it, \
         its CI does not run on our compute, and its releases and issues are not mirrored or relayed",
        project.name
    ))
}

/// Refuse `operation` when the working copy at `repo` is a contributed project. Does nothing on a host without a
/// placement copy.
pub fn refuse_working_copy(operation: &str, repo: &Path) -> Result<()> {
    let environment: Environment = std::env::vars_os().collect();
    let Some(catalog) = Catalog::load(&environment)? else {
        return Ok(());
    };
    match catalog.classify_working_copy(repo, &environment)? {
        Some(project) => Err(refusal(operation, project)),
        None => Ok(()),
    }
}

/// Refuse `operation` when the forge job's repository (`owner/name` on `server_url`) is a contributed project.
pub fn refuse_repository(operation: &str, server_url: &str, repository: &str) -> Result<()> {
    let environment: Environment = std::env::vars_os().collect();
    let Some(catalog) = Catalog::load(&environment)? else {
        return Ok(());
    };
    match catalog.classify_repository(server_url, repository) {
        Some(project) => Err(refusal(operation, project)),
        None => Ok(()),
    }
}

/// Refuse `operation` when `url` names a contributed project.
pub fn refuse_url(operation: &str, url: &str) -> Result<()> {
    let environment: Environment = std::env::vars_os().collect();
    let Some(catalog) = Catalog::load(&environment)? else {
        return Ok(());
    };
    match catalog.classify_url(url) {
        Some(project) => Err(refusal(operation, project)),
        None => Ok(()),
    }
}

/// Refuse `operation` when `owner/name` on OUR forge is a contributed project.
pub fn refuse_forge_path(operation: &str, path: &str) -> Result<()> {
    let environment: Environment = std::env::vars_os().collect();
    let Some(catalog) = Catalog::load(&environment)? else {
        return Ok(());
    };
    match catalog.classify_forge_path(path) {
        Some(project) => Err(refusal(operation, project)),
        None => Ok(()),
    }
}

/// Run the overlay manifest's checks against a contributed project's working copy: our own work on their project.
///
/// * The manifest is a file OUTSIDE the working copy; nothing of ours is written into it. (The result cache keys
///   committed content and writes beside the sources, so it is not used; the compile caches are, when provisioned.)
/// * Nothing of our infrastructure is assumed or passed on ([`OURS`]): the checks run with the caller's
///   environment, and a machine without any cache or fleet data runs them the same way.
/// * A forge's CI job never runs it: their CI does not run on our compute.
pub fn check(repo: &Path, manifest: &Path, selectors: &[String], plan: bool) -> Result<()> {
    let mut environment: Environment = std::env::vars_os().collect();
    let catalog = Catalog::load(&environment)?.ok_or_else(|| {
        failure(format!(
            "contributed checks need the placement: set {PLACEMENT_FILE_ENV} to a copy of lib/placement.json"
        ))
    })?;
    let root = repo.canonicalize()?;
    let project = catalog
        .classify_working_copy(&root, &environment)?
        .ok_or_else(|| {
            failure(format!(
                "{} is not a declared contributed project (none of its remotes is listed in the placement's `contributed`): \
                 use `ccid check` for an owned project",
                root.display()
            ))
        })?;
    if let Some(name) = CI_JOB
        .iter()
        .find(|name| value(&environment, name).is_some())
    {
        return Err(failure(format!(
            "contributed check refused: {name} is set, and the CI of a contributed project never runs on our compute (R11)"
        )));
    }
    let meta = fs::metadata(manifest)
        .map_err(|error| failure(format!("overlay manifest {}: {error}", manifest.display())))?;
    if !meta.is_file() {
        return Err(failure(format!(
            "overlay manifest {} is not a file",
            manifest.display()
        )));
    }
    let overlay = manifest.canonicalize()?;
    if project_roots(&root)
        .iter()
        .any(|inside| overlay.starts_with(inside))
    {
        return Err(failure(format!(
            "overlay manifest {} lies inside the contributed project; it must live outside it (R11: nothing of ours enters their repository)",
            overlay.display()
        )));
    }
    // Only kinds whose commands the overlay spells out run here. `javascript` and `nix` checks install and write
    // into the working copy (node_modules, result links) by what they are.
    let (declared, _) = crate::load_manifest(Path::new("/"), &overlay)?;
    for name in crate::select_checks(&declared, selectors)? {
        let kind = declared.checks[&name].kind.as_str();
        if !["commands", "cargo"].contains(&kind) {
            return Err(failure(format!(
                "overlay check {name} is of kind {kind:?}: a contributed project takes `commands` and `cargo` checks only, \
                 since the others write into the working copy"
            )));
        }
    }
    environment.retain(|key, _| {
        let key = key.to_string_lossy();
        !OURS.contains(&key.as_ref())
            && !key.starts_with(OURS_PREFIX)
            && !(key.starts_with("CARGO_REGISTRIES_") && key.ends_with("_TOKEN"))
    });
    event(json!({
        "event": "contributed",
        "project": project.name,
        "upstream": project.upstream,
        "overlay_manifest": overlay,
        "result_cache": false,
    }));
    crate::run_overlay_checks(&root, &overlay, selectors, plan, environment)
}

/// `host/path` in lowercase, for matching: scheme, user information, port, query, a trailing slash and one `.git`
/// are dropped; scp-like (`git@host:owner/repo`) is understood. `None` when `url` names no `host/owner/repo`.
pub fn key_of(url: &str) -> Option<String> {
    let url = url.trim();
    let url = url.strip_prefix("git+").unwrap_or(url);
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !["https", "http", "ssh", "git"].contains(&scheme.to_ascii_lowercase().as_str()) {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host_port = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (host_port.split(':').next()?, path)
    } else {
        let (left, path) = url.split_once(':')?;
        if left.contains('/') {
            return None;
        }
        (left.rsplit_once('@').map_or(left, |(_, host)| host), path)
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b))
    {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() < 2
        || segments
            .iter()
            .any(|s| s.is_empty() || *s == "." || *s == "..")
    {
        return None;
    }
    Some(format!("{host}/{path}").to_ascii_lowercase())
}

/// The key of a location as the catalog writes it: exactly `https://host/owner/repo` (lowercase host, no user
/// information, port, `.git`, trailing slash, query or fragment), so a spelling mistake is found when the file is
/// read, not when a project is misclassified.
fn canonical_key(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| failure(format!("{url}: a location is an https URL")))?;
    let valid = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    let (host, path) = rest
        .split_once('/')
        .ok_or_else(|| failure(format!("{url}: a location names host/owner/repo")))?;
    let segments: Vec<&str> = path.split('/').collect();
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-.".contains(&b))
        || segments.len() < 2
        || !segments.iter().all(|s| valid(s))
        || path.ends_with(".git")
    {
        return Err(failure(format!(
            "{url}: not a canonical location (lowercase host, host/owner/repo, no port, user, .git or trailing slash)"
        )));
    }
    Ok(format!("{host}/{path}").to_ascii_lowercase())
}

fn project_of(item: &Value, index: usize) -> Result<Project> {
    let object = item.as_object().ok_or_else(|| {
        failure(format!(
            "placement `contributed` entry {index} must be an object"
        ))
    })?;
    if let Some(unknown) = object
        .keys()
        .find(|key| !["name", "upstream", "forks"].contains(&key.as_str()))
    {
        return Err(failure(format!(
            "placement `contributed` entry {index}: unknown field {unknown}"
        )));
    }
    let name = object.get("name").and_then(Value::as_str).ok_or_else(|| {
        failure(format!(
            "placement `contributed` entry {index} needs a name"
        ))
    })?;
    if name.is_empty()
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        return Err(failure(format!(
            "placement `contributed` entry {index}: {name:?} is not a plain lowercase name"
        )));
    }
    let upstream = object
        .get("upstream")
        .and_then(Value::as_str)
        .ok_or_else(|| failure(format!("placement `contributed` {name} needs an upstream")))?;
    let mut forks = Vec::new();
    match object.get("forks") {
        None => {}
        Some(Value::Array(list)) => {
            for fork in list {
                let fork = fork.as_str().ok_or_else(|| {
                    failure(format!("placement `contributed` {name}: forks are strings"))
                })?;
                if forks.iter().any(|seen| seen == fork) || fork == upstream {
                    return Err(failure(format!(
                        "placement `contributed` {name}: {fork} is listed twice"
                    )));
                }
                forks.push(fork.to_owned());
            }
        }
        Some(_) => {
            return Err(failure(format!(
                "placement `contributed` {name}: forks must be a list"
            )))
        }
    }
    Ok(Project {
        name: name.to_owned(),
        upstream: upstream.to_owned(),
        forks,
    })
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::metadata(path)?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err(failure("must be a regular file of reasonable size"));
    }
    Ok(fs::read(path)?)
}

/// Run git in `repo`; `None` when `repo` is not a git working copy. Any other failure (a repository git refuses to
/// read, such as one with dubious ownership) is an error: the guard never reads it as "no remotes".
fn git_in(repo: &Path, arguments: &[&str]) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(arguments)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|error| failure(format!("cannot run git in {}: {error}", repo.display())))?;
    if output.status.success() {
        return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(128) && stderr.contains("not a git repository") {
        return Ok(None);
    }
    Err(failure(format!(
        "git could not read {}: {}",
        repo.display(),
        stderr.trim()
    )))
}

/// Every remote URL (fetch and push) of the working copy at `repo`, as git resolves it: `url.<base>.insteadOf`
/// is applied. Empty when it is not a git working copy or has no remote.
fn remote_urls(repo: &Path) -> Result<Vec<String>> {
    let Some(names) = git_in(repo, &["remote"])? else {
        return Ok(Vec::new());
    };
    let mut seen = BTreeSet::new();
    let mut urls = Vec::new();
    for name in names.lines().map(str::trim).filter(|n| !n.is_empty()) {
        for arguments in [
            ["remote", "get-url", "--all", name],
            ["remote", "get-url", "--push", name],
        ] {
            for url in git_in(repo, &arguments)?.unwrap_or_default().lines() {
                let url = url.trim();
                if !url.is_empty() && seen.insert(url.to_owned()) {
                    urls.push(url.to_owned());
                }
            }
        }
    }
    Ok(urls)
}

/// The roots a contributed project's files may be under: the working copy's top level and, for a linked worktree, the
/// main checkout. An overlay manifest under any of them is inside the project.
fn project_roots(repo: &Path) -> Vec<PathBuf> {
    let mut roots = vec![repo.to_owned()];
    if let Ok(Some(text)) = git_in(repo, &["rev-parse", "--show-toplevel", "--git-common-dir"]) {
        let mut lines = text.lines();
        if let Some(top) = lines.next() {
            roots.extend(Path::new(top).canonicalize());
        }
        if let Some(common) = lines.next() {
            let common = if Path::new(common).is_absolute() {
                PathBuf::from(common)
            } else {
                repo.join(common)
            };
            if let Some(main) = common
                .canonicalize()
                .ok()
                .and_then(|c| c.parent().map(Path::to_owned))
            {
                roots.push(main);
            }
        }
    }
    roots
}

/// The action of `ccid contributed`.
#[derive(clap::Subcommand)]
pub enum Action {
    /// The declared contributed projects (JSON line: digest, forge hosts, projects). Exit 2 without a placement copy.
    Show,
    /// Whether a working copy is a contributed project (JSON line). Exit 0: it is; 1: it is not; 2: cannot tell.
    Which {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Run an overlay manifest's checks on a contributed project's working copy: our own work on their project.
    /// The manifest lives outside the working copy; nothing is written into it.
    Check {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// The overlay manifest: a ccid manifest outside the contributed project's repository.
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long = "check", required = true)]
        checks: Vec<String>,
        #[arg(long)]
        plan: bool,
        /// Supervision hook, set only by the supervisor's re-execution.
        #[arg(long, hide = true, conflicts_with = "plan")]
        parent_watch: bool,
    },
}

/// Run one `ccid contributed` command; the exit code.
pub fn run(action: &Action) -> Result<u8> {
    let environment: Environment = std::env::vars_os().collect();
    match action {
        Action::Show => {
            let catalog = Catalog::load(&environment)?.ok_or_else(|| {
                failure(format!(
                    "placement unavailable: set {PLACEMENT_FILE_ENV} to a copy of lib/placement.json"
                ))
            })?;
            let projects: Vec<Value> = catalog
                .projects()
                .iter()
                .map(|p| json!({"name": p.name, "upstream": p.upstream, "forks": p.forks}))
                .collect();
            println!(
                "{}",
                json!({"digest": catalog.digest(), "forge_hosts": catalog.forge_hosts(), "projects": projects})
            );
            Ok(0)
        }
        Action::Which { repo } => {
            let catalog = Catalog::load(&environment)?.ok_or_else(|| {
                failure(format!(
                    "placement unavailable: set {PLACEMENT_FILE_ENV} to a copy of lib/placement.json"
                ))
            })?;
            match catalog.classify_working_copy(repo, &environment)? {
                Some(project) => {
                    println!(
                        "{}",
                        json!({"contributed": true, "project": project.name, "upstream": project.upstream})
                    );
                    Ok(0)
                }
                None => {
                    println!("{}", json!({"contributed": false}));
                    Ok(1)
                }
            }
        }
        Action::Check {
            repo,
            manifest,
            checks,
            plan,
            ..
        } => {
            check(repo, manifest, checks, *plan)?;
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests;

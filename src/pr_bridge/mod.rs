//! Opt-in, one-shot contribution import. This module never executes imported code.
mod approval;
mod git;
mod http;
mod lifecycle;
mod sandbox;

use crate::{failure, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
#[command(about = "Read GitLab MRs and explicitly import one into Forgejo; no CI dispatch")]
pub struct Options {
    /// Enable this experimental, otherwise inert binary.
    #[arg(long)]
    pub enable_gitlab_import: bool,
    #[arg(long)]
    pub config: PathBuf,
    /// Omit to list open MRs. With this option, default behavior is a read-only plan.
    #[arg(long)]
    pub mr: Option<u64>,
    #[arg(long, requires = "mr")]
    pub apply: bool,
    /// Required for writes: instance hooks and external CI have been audited as disabled.
    #[arg(long, requires = "apply")]
    pub confirm_ci_disabled: bool,
    /// Persistent private state, shared by every invocation for this mapping.
    #[arg(long)]
    pub state_dir: PathBuf,
    /// Reconcile a journaled primary merge/close back to its source MR.
    #[arg(long, requires = "apply", conflicts_with = "replace_head")]
    pub feedback: bool,
    /// Explicit replacement after a rebase; preserves the old branch and PR history.
    #[arg(long, requires = "apply")]
    pub replace_head: Option<String>,
    /// Trusted approval/check request; emit a credential-free isolated Job after live review validation.
    #[arg(long, requires = "mr", conflicts_with = "apply")]
    pub ci_plan: Option<PathBuf>,
    /// Print the exact primary-review text for a trusted CI request, without contacting forges.
    #[arg(long, requires = "ci_plan")]
    pub approval_message: bool,
    /// Submit the approved CI plan after auditing the live isolated namespace.
    #[arg(long, requires_all = ["ci_plan", "kubernetes_command"], conflicts_with = "approval_message")]
    pub dispatch_ci: bool,
    /// Trusted executable wrapper for the deployment's Kubernetes CLI.
    #[arg(long, requires = "dispatch_ci")]
    pub kubernetes_command: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    /// HTTPS origin, without a path or trailing slash.
    pub gitlab: String,
    pub gitlab_project: u64,
    pub gitlab_repository: String,
    pub forgejo: String,
    /// Primary repository must belong to an organization.
    pub primary_repository: String,
    /// Pre-created direct fork owned by the authenticated bridge account.
    pub import_repository: String,
    pub target_branch: String,
}

impl Config {
    fn validate(&self) -> Result<()> {
        origin(&self.gitlab)?;
        origin(&self.forgejo)?;
        repository(&self.gitlab_repository, false)?;
        repository(&self.primary_repository, true)?;
        repository(&self.import_repository, true)?;
        if self.schema != 1
            || self.gitlab_project == 0
            || self.primary_repository == self.import_repository
            || self.primary_repository.split('/').nth(1) != self.import_repository.split('/').nth(1)
            || !branch(&self.target_branch)
            || self.target_branch.starts_with("ccid-import/")
        {
            return Err(failure("Invalid bridge mapping"));
        }
        Ok(())
    }

    fn key(&self, iid: u64) -> String {
        // Bind state, marker and branch to the complete trusted mapping.
        format!(
            "{:x}",
            Sha256::digest(format!("{}:{iid}", serde_json::to_string(self).unwrap()))
        )
    }

    fn source_url(&self, iid: u64) -> String {
        format!(
            "{}/{}/-/merge_requests/{iid}",
            self.gitlab, self.gitlab_repository
        )
    }
}

fn origin(value: &str) -> Result<()> {
    let host = value.strip_prefix("https://").unwrap_or_default();
    let parts: Vec<_> = host.split(':').collect();
    if parts.len() > 2
        || parts[0].is_empty()
        || parts[0].starts_with('.')
        || !parts[0]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        || (parts.len() == 2 && parts[1].parse::<u16>().ok().filter(|p| *p > 0).is_none())
    {
        return Err(failure(
            "Bridge origins must be credential-free HTTPS origins",
        ));
    }
    Ok(())
}

fn segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn repository(value: &str, pair: bool) -> Result<()> {
    let parts: Vec<_> = value.split('/').collect();
    if value.len() > 512
        || parts.len() < 2
        || (pair && parts.len() != 2)
        || !parts.iter().all(|p| segment(p))
    {
        return Err(failure("Invalid repository path"));
    }
    Ok(())
}

fn branch(value: &str) -> bool {
    value.len() <= 200
        && value.split('/').all(|p| {
            segment(p) && !p.starts_with('.') && !p.ends_with('.') && !p.ends_with(".lock")
        })
        && !value.contains("..")
}

fn oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Author {
    id: u64,
    username: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct MergeRequest {
    iid: u64,
    project_id: u64,
    target_project_id: u64,
    state: String,
    title: String,
    sha: String,
    target_branch: String,
    author: Author,
}

impl MergeRequest {
    fn validate(&self, config: &Config, iid: u64) -> Result<()> {
        if iid == 0
            || self.iid != iid
            || self.project_id != config.gitlab_project
            || self.target_project_id != config.gitlab_project
            || !oid(&self.sha)
            || self.target_branch != config.target_branch
            || self.author.id == 0
            || !segment(&self.author.username)
            || self.author.username.len() > 255
            || self.title.is_empty()
            || self.title.len() > 1024
            || self.title.chars().any(char::is_control)
        {
            return Err(failure(
                "MR identity, head, author or target does not match the mapping",
            ));
        }
        if self.state != "opened" {
            return Err(failure("Only open GitLab merge requests can be imported"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Forge {
    Gitlab,
    Forgejo,
}

trait Api {
    fn request(
        &mut self,
        forge: Forge,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value>;
}

trait Objects {
    /// Copy exact commits only, creating or advancing the one import ref by fast-forward.
    fn publish(&mut self, config: &Config, iid: u64, sha: &str, branch: &str) -> Result<()>;
}

fn get(api: &mut impl Api, forge: Forge, path: &str) -> Result<Value> {
    api.request(forge, "GET", path, None)
}

fn pages(api: &mut impl Api, forge: Forge, path: &str) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    let mut previous = Vec::new();
    let separator = if path.contains('?') { '&' } else { '?' };
    for page in 1..=20 {
        let limit = if forge == Forge::Gitlab {
            "per_page"
        } else {
            "limit"
        };
        let value = get(
            api,
            forge,
            &format!("{path}{separator}{limit}=50&page={page}"),
        )?;
        let values = value
            .as_array()
            .ok_or_else(|| failure("Expected a paginated array"))?;
        if values.is_empty() {
            return Ok(result);
        }
        // Instance limits can cap a requested page below 50. Only an empty
        // page proves completion; repeated pages must not hide an older PR.
        if values == &previous {
            return Err(failure("Forge repeated an inventory page"));
        }
        previous = values.clone();
        result.extend(values.iter().cloned());
    }
    Err(failure(
        "Bridge inventory exceeds 20 pages; refusing incomplete reconciliation",
    ))
}

fn mr(api: &mut impl Api, config: &Config, iid: u64) -> Result<MergeRequest> {
    let value = get(
        api,
        Forge::Gitlab,
        &format!("projects/{}/merge_requests/{iid}", config.gitlab_project),
    )?;
    let result: MergeRequest = serde_json::from_value(value)?;
    result.validate(config, iid)?;
    Ok(result)
}

/// A locked journal survives a crash between mutation intent and response.
struct Journal {
    root: PathBuf,
    _lock: File,
}

impl Journal {
    fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("lock"))?;
        lock.try_lock()
            .map_err(|_| failure("Bridge state is busy; no work started"))?;
        Ok(Self {
            root: root.to_owned(),
            _lock: lock,
        })
    }

    fn load(&self, key: &str) -> Result<Entry> {
        match fs::read(self.root.join(format!("{key}.json"))) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Entry::default()),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, key: &str, value: &impl Serialize) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(self.root.join(format!("{key}.json")))?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    fn admit(&self) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let path = self.root.join("next-poll.json");
        if path.exists() && serde_json::from_slice::<u64>(&fs::read(path)?)? > now {
            return Err(failure("Bridge polling cooldown is active; retry later"));
        }
        self.save("next-poll", &(now + 60))
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    create_attempted: bool,
    pull_number: Option<u64>,
    sha: Option<String>,
    #[serde(default)]
    generation: Option<String>,
    #[serde(default)]
    superseded_pull: Option<u64>,
    #[serde(default)]
    feedback_attempted: bool,
    #[serde(default)]
    feedback_note: Option<u64>,
}

fn number(value: &Value, name: &str) -> Result<u64> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .ok_or_else(|| failure(format!("Missing positive {name}")))
}

struct Destination {
    primary_id: u64,
    fork_id: u64,
    actor_id: u64,
}

fn preflight(api: &mut impl Api, config: &Config) -> Result<Destination> {
    let source = get(
        api,
        Forge::Gitlab,
        &format!("projects/{}", config.gitlab_project),
    )?;
    if number(&source, "id")? != config.gitlab_project
        || source["path_with_namespace"] != config.gitlab_repository
    {
        return Err(failure("GitLab project ID and repository path disagree"));
    }
    let primary = get(
        api,
        Forge::Forgejo,
        &format!("repos/{}", config.primary_repository),
    )?;
    let fork = get(
        api,
        Forge::Forgejo,
        &format!("repos/{}", config.import_repository),
    )?;
    let actor = get(api, Forge::Forgejo, "user")?;
    let primary_id = number(&primary, "id")?;
    let actor_id = number(&actor, "id")?;
    if !primary["private"].is_boolean()
        || !fork["private"].is_boolean()
        || !matches!(
            source["visibility"].as_str(),
            Some("public" | "internal" | "private")
        )
        || (source["visibility"] != "public"
            && (primary["private"] != true || fork["private"] != true))
        || (primary["private"] == true && fork["private"] != true)
    {
        return Err(failure(
            "Import would weaken repository visibility or visibility is unknown",
        ));
    }
    if primary["full_name"] != config.primary_repository
        || fork["full_name"] != config.import_repository
        || primary["has_actions"] != false
        || fork["has_actions"] != false
        || fork["fork"] != true
        || fork["parent"]["id"] != primary_id
        || fork["owner"]["id"] != actor_id
    {
        return Err(failure(
            "Import requires a bot-owned direct fork and Actions disabled on both repositories",
        ));
    }
    let owner = config.primary_repository.split('/').next().unwrap();
    for path in [
        format!("repos/{}/hooks", config.primary_repository),
        format!("repos/{}/hooks", config.import_repository),
        format!("orgs/{owner}/hooks"),
        "user/hooks".into(),
    ] {
        if !pages(api, Forge::Forgejo, &path)?.is_empty() {
            return Err(failure(
                "Import requires repository, organization and bridge-user webhooks to be absent",
            ));
        }
    }
    Ok(Destination {
        primary_id,
        fork_id: number(&fork, "id")?,
        actor_id,
    })
}

fn match_pull<'a>(
    pulls: &'a [Value],
    marker: &str,
    branch: &str,
    config: &Config,
    dest: &Destination,
) -> Result<Option<&'a Value>> {
    let mut found = None;
    for pull in pulls {
        let marked = pull["body"]
            .as_str()
            .is_some_and(|body| body.starts_with(marker));
        let same_head = pull["head"]["ref"] == branch && pull["head"]["repo"]["id"] == dest.fork_id;
        if !marked && !same_head {
            continue;
        }
        if !marked
            || !same_head
            || pull["user"]["id"] != dest.actor_id
            || pull["base"]["repo"]["id"] != dest.primary_id
            || pull["base"]["ref"] != config.target_branch
            || found.is_some()
        {
            return Err(failure(
                "Ambiguous or altered bridge PR identity; manual reconciliation required",
            ));
        }
        found = Some(pull);
    }
    Ok(found)
}

fn import(
    api: &mut impl Api,
    objects: &mut impl Objects,
    config: &Config,
    iid: u64,
    journal: &Journal,
) -> Result<Value> {
    let current = journal.load(&config.key(iid))?;
    import_generation(
        api,
        objects,
        config,
        iid,
        journal,
        current.generation.as_deref(),
    )
}

fn generation_key(config: &Config, iid: u64, generation: Option<&str>) -> String {
    let key = config.key(iid);
    generation.map_or(key.clone(), |sha| format!("{key}-{sha}"))
}

fn import_generation(
    api: &mut impl Api,
    objects: &mut impl Objects,
    config: &Config,
    iid: u64,
    journal: &Journal,
    generation: Option<&str>,
) -> Result<Value> {
    let source = mr(api, config, iid)?;
    if generation.is_some_and(|sha| sha != source.sha) {
        return Err(failure(
            "Replacement head changed; explicitly select the new head",
        ));
    }
    let key = generation_key(config, iid, generation);
    let branch = format!("ccid-import/gitlab/{key}");
    let marker = format!("<!-- ccid-pr-bridge:v1:{key} -->\n");
    let mut entry = journal.load(&key)?;
    let dest = preflight(api, config)?;
    let pulls = pages(
        api,
        Forge::Forgejo,
        &format!("repos/{}/pulls?state=all", config.primary_repository),
    )?;
    let existing = match_pull(&pulls, &marker, &branch, config, &dest)?;
    if let Some(pull) = existing {
        let index = number(pull, "number")?;
        if entry.pull_number.is_some_and(|n| n != index) {
            return Err(failure("Stored PR and remote PR disagree"));
        }
        if pull["state"] == "closed" || pull["merged"] == true {
            entry.pull_number = Some(index);
            journal.save(&key, &entry)?;
            return Ok(
                json!({"status":"primary_closed", "pull_number":index, "source":config.source_url(iid), "source_close_pending":true}),
            );
        }
        if pull["state"] != "open" {
            return Err(failure("Unknown primary PR state"));
        }
    } else if entry.create_attempted || entry.pull_number.is_some() {
        return Err(failure(
            "Unresolved PR creation intent or missing PR; refusing a second POST",
        ));
    }
    // Fetch through the trusted target project MR ref, never a fork URL from JSON.
    objects.publish(config, iid, &source.sha, &branch)?;
    // Do not create a PR from a source that closed, retargeted or changed mid-import.
    if mr(api, config, iid)? != source {
        return Err(failure(
            "MR changed during import; reconcile on the next poll",
        ));
    }
    let index = if let Some(pull) = existing {
        number(pull, "number")?
    } else {
        let owner = config.import_repository.split('/').next().unwrap();
        let body = json!({
            "head": format!("{owner}:{branch}"), "base": config.target_branch,
            "title": format!("[WIP] GitLab !{iid}: {}", source.title.chars().take(180).collect::<String>()),
            "body": format!("{marker}Imported from {}\n\nOriginal author: GitLab `{}` (user ID {}). Original Git commits and authorship are preserved.\n\nUntrusted contribution. CI requires maintainer approval of the current PR head and an isolated runner. CI has not been dispatched by this bridge. Initial imported head: `{}`.\n\nReview the original discussion at the source link; this bridge does not copy reviews or approvals.", config.source_url(iid), source.author.username, source.author.id, source.sha)
        });
        entry.create_attempted = true;
        entry.sha = Some(source.sha.clone());
        journal.save(&key, &entry)?;
        let created = api.request(
            Forge::Forgejo,
            "POST",
            &format!("repos/{}/pulls", config.primary_repository),
            Some(&body),
        )?;
        let values = [created];
        let pull = match_pull(&values, &marker, &branch, config, &dest)?
            .ok_or_else(|| failure("Created PR identity was not confirmed"))?;
        number(pull, "number")?
    };
    entry.pull_number = Some(index);
    entry.sha = Some(source.sha.clone());
    journal.save(&key, &entry)?;
    Ok(
        json!({"status":"imported", "source":config.source_url(iid), "head":source.sha, "pull_number":index,
        "pull_url":format!("{}/{}/pulls/{index}", config.forgejo, config.primary_repository), "ci":"not_dispatched"}),
    )
}

pub fn run(options: Options) -> Result<Value> {
    if !options.enable_gitlab_import {
        return Err(failure(
            "Contribution bridge is disabled; pass --enable-gitlab-import",
        ));
    }
    if options.apply && (!options.confirm_ci_disabled || options.mr.is_none()) {
        return Err(failure("Import requires an MR and --confirm-ci-disabled"));
    }
    let config: Config = toml::from_str(&fs::read_to_string(options.config)?)?;
    config.validate()?;
    if options.approval_message {
        return Ok(
            json!({"review_body":approval::message(&fs::read(options.ci_plan.as_ref().unwrap())?)?}),
        );
    }
    let journal = Journal::open(&options.state_dir)?;
    journal.admit()?;
    let mut api = http::Client::new(&config)?;
    if let Some(iid) = options.mr {
        if let Some(path) = options.ci_plan {
            let bytes = fs::read(path)?;
            let plan = approval::plan(&mut api, &config, iid, &journal, &bytes)?;
            if options.dispatch_ci {
                let mut kubernetes = sandbox::Client::new(options.kubernetes_command.unwrap())?;
                sandbox::audit(&mut kubernetes, &plan)?;
                // Re-read review, permissions and contribution immediately before submission.
                let current = approval::plan(&mut api, &config, iid, &journal, &bytes)?;
                if current != plan {
                    return Err(failure("Approved CI plan changed before dispatch"));
                }
                let submitted = sandbox::dispatch(&mut kubernetes, &journal, &plan)?;
                return sandbox::wait(&mut kubernetes, &plan, submitted);
            }
            return Ok(plan);
        }
        if options.apply {
            if options.feedback {
                return lifecycle::feedback(&mut api, &config, iid, &journal);
            }
            let mut objects = git::Git::new(Duration::from_secs(300))?;
            if let Some(head) = options.replace_head {
                lifecycle::replace(&mut api, &mut objects, &config, iid, &journal, &head)
            } else {
                import(&mut api, &mut objects, &config, iid, &journal)
            }
        } else {
            let source = mr(&mut api, &config, iid)?;
            Ok(
                json!({"status":"planned", "source":config.source_url(iid), "head":source.sha, "author":source.author.username, "target":config.primary_repository, "ci":"not_dispatched"}),
            )
        }
    } else {
        let values = pages(
            &mut api,
            Forge::Gitlab,
            &format!(
                "projects/{}/merge_requests?state=opened&scope=all&order_by=created_at&sort=asc",
                config.gitlab_project
            ),
        )?;
        let mut entries = Vec::new();
        for value in values {
            let iid = number(&value, "iid")?;
            if entries.iter().any(|v: &Value| v["iid"] == iid) {
                return Err(failure("Duplicate MR in paginated inventory"));
            }
            entries.push(json!({"iid":iid,"source":config.source_url(iid)}));
        }
        Ok(json!({"status":"listed", "merge_requests":entries}))
    }
}

#[cfg(test)]
mod tests;

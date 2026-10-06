//! Forge adapters and CLI for the cqlt quality library.
mod prose;
mod semantic;
use ccid::{Environment, Result, Runner};
use clap::{Subcommand, ValueEnum};
use cqlt::{Document, Organization, Policy, Repository, Severity, Snapshot, Status, Visibility};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn failure(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::other(message.into()).into()
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum Forge {
    Github,
    Forgejo,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Threshold {
    Error,
    Warning,
}

#[derive(Subcommand)]
pub enum Action {
    /// Prepare, replay, or explicitly execute cqlt's Jev semantic review.
    Semantic(semantic::Options),
    /// Check writing with cqlt's subordinate Vale backend. Requires installed Vale.
    Prose(prose::Options),
    /// Collect visible organizations and repositories using read-only forge APIs.
    Collect {
        #[arg(long, value_enum)]
        forge: Forge,
        /// API root. Defaults to GitHub; required for Forgejo. HTTPS only.
        #[arg(long)]
        api_url: Option<String>,
        /// Omit to discover every organization accessible through membership.
        #[arg(long = "org")]
        organizations: Vec<String>,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 900, value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    },
    /// Evaluate saved evidence without network access. Exit 0/1/2: pass/fail/unknown.
    Check {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        policy: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "error")]
        fail_on: Threshold,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(action: Action) -> Result<u8> {
    match action {
        Action::Semantic(options) => semantic::run(options),
        Action::Prose(options) => prose::run(options),
        Action::Check {
            snapshot,
            policy,
            fail_on,
            json,
        } => {
            let snapshot: Snapshot = serde_json::from_slice(&fs::read(snapshot)?)?;
            let policy = policy
                .map(|p| -> Result<Policy> { Ok(serde_json::from_slice(&fs::read(p)?)?) })
                .transpose()?
                .unwrap_or_default();
            let report = cqlt::evaluate(&snapshot, &policy).map_err(failure)?;
            let threshold = match fail_on {
                Threshold::Error => Severity::Error,
                Threshold::Warning => Severity::Warning,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "{}: {} organizations, {} repositories",
                    report.ruleset, report.organizations, report.repositories
                );
                for c in &report.checks {
                    if matches!(c.status, Status::Fail | Status::Unknown | Status::Waived) {
                        // Debug escaping prevents remote descriptions injecting terminal controls.
                        println!(
                            "{:?} {:?} {} {}: {:?}",
                            c.status, c.severity, c.subject, c.rule, c.evidence
                        );
                    }
                }
                println!(
                    "snapshot={} policy={}",
                    report.snapshot_sha256, report.policy_sha256
                );
            }
            Ok(report.exit_code(threshold))
        }
        Action::Collect {
            forge,
            api_url,
            organizations,
            output,
            timeout,
        } => {
            let base = match (forge, api_url) {
                (Forge::Github, None) => "https://api.github.com".into(),
                (Forge::Forgejo, None) => {
                    return Err(failure(
                        "Forgejo requires --api-url https://forge.example/api/v1",
                    ))
                }
                (_, Some(url)) => url.trim_end_matches('/').to_owned(),
            };
            validate_base(&base)?;
            for org in &organizations {
                if !cqlt::valid_login(org) {
                    return Err(failure("Invalid organization name"));
                }
            }
            let mut environment: Environment = std::env::vars_os().collect();
            let token = if let Ok(token) = std::env::var("CQLT_TOKEN") {
                token
            } else if forge == Forge::Github && base == "https://api.github.com" {
                let runner = Runner::new(
                    std::env::current_dir()?,
                    environment.clone(),
                    Duration::from_secs(30),
                )?;
                runner.run(
                    &[
                        "gh".into(),
                        "auth".into(),
                        "token".into(),
                        "--hostname".into(),
                        "github.com".into(),
                    ],
                    true,
                )?
            } else {
                return Err(failure("Set CQLT_TOKEN for the selected forge instance"));
            };
            if token.trim().is_empty() || token.contains(['\r', '\n']) {
                return Err(failure("Invalid CQLT_TOKEN"));
            }
            environment.insert("CCID_QUALITY_TOKEN".into(), token.into());
            let runner = Runner::new(
                std::env::current_dir()?,
                environment,
                Duration::from_secs(timeout),
            )?;
            let client = Client {
                runner,
                base,
                forge,
            };
            let snapshot = collect(&client, organizations)?;
            let complete = snapshot.complete;
            write_snapshot(&output, &snapshot)?;
            eprintln!(
                "Saved {} organizations to {} (complete={complete})",
                snapshot.organizations.len(),
                output.display()
            );
            Ok(if complete { 0 } else { 2 })
        }
    }
}

fn validate_base(base: &str) -> Result<()> {
    let host = base
        .strip_prefix("https://")
        .ok_or_else(|| failure("API URL must use HTTPS"))?;
    if host.is_empty()
        || host.starts_with('/')
        || base.contains(['@', '?', '#', '\\'])
        || base.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(failure(
            "API URL must identify an HTTPS instance without credentials, query or fragment",
        ));
    }
    Ok(())
}

fn component(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

trait Api {
    fn forge(&self) -> Forge;
    fn source(&self) -> String;
    fn get(&self, path: &str) -> Result<Option<Value>>;
}

struct Client {
    runner: Runner,
    base: String,
    forge: Forge,
}
impl Api for Client {
    fn forge(&self) -> Forge {
        self.forge
    }
    fn source(&self) -> String {
        format!("{:?}:{}", self.forge, self.base)
    }
    fn get(&self, path: &str) -> Result<Option<Value>> {
        let body = tempfile::NamedTempFile::new()?;
        // curl expands the inherited variable internally; tokens never appear in
        // argv, logs, snapshots or files. Redirects are deliberately not followed.
        let args = vec![
            "curl".into(),
            "--disable".into(),
            "--silent".into(),
            "--show-error".into(),
            "--globoff".into(),
            "--connect-timeout".into(),
            "10".into(),
            "--max-time".into(),
            "30".into(),
            "--max-filesize".into(),
            "16777216".into(),
            "--proto".into(),
            "=https".into(),
            "--variable".into(),
            "%CCID_QUALITY_TOKEN".into(),
            "--expand-header".into(),
            "Authorization: token {{CCID_QUALITY_TOKEN}}".into(),
            "--header".into(),
            "Accept: application/json".into(),
            "--output".into(),
            body.path().to_string_lossy().into_owned(),
            "--write-out".into(),
            "%{http_code}".into(),
            "--url".into(),
            format!("{}/{}", self.base, path),
        ];
        let status = self.runner.run(&args, true)?;
        match status.as_str() {
            "200" => Ok(Some(serde_json::from_slice(&fs::read(body.path())?)?)),
            "404" => Ok(None),
            _ => Err(failure(format!("Forge GET {path} returned HTTP {status}"))),
        }
    }
}

fn required(api: &impl Api, path: &str) -> Result<Value> {
    api.get(path)?
        .ok_or_else(|| failure(format!("Required forge resource unavailable: {path}")))
}

fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| failure(format!("Missing string field {key}")))
}
fn optional(v: &Value, key: &str) -> Result<Option<String>> {
    match v.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(failure(format!("Missing nullable string field {key}"))),
    }
}
fn boolean(v: &Value, key: &str) -> Result<bool> {
    v.get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| failure(format!("Missing boolean field {key}")))
}

fn pages(api: &impl Api, path: &str) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    let mut identities = std::collections::BTreeSet::new();
    // Request until an EMPTY page, not a short page: instances may cap page size.
    for page in 1..=10000 {
        let size = if api.forge() == Forge::Github {
            "per_page"
        } else {
            "limit"
        };
        let separator = if path.contains('?') { '&' } else { '?' };
        let data = required(api, &format!("{path}{separator}{size}=100&page={page}"))?;
        let rows = data
            .as_array()
            .ok_or_else(|| failure("Expected a paginated array"))?;
        if rows.is_empty() {
            return Ok(all);
        }
        for row in rows {
            let id = row
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| failure("Missing paginated identity"))?;
            if !identities.insert(id) {
                return Err(failure(
                    "Pagination repeated an identity; recollect a stable inventory",
                ));
            }
            all.push(row.clone());
        }
    }
    Err(failure("Pagination limit exceeded"))
}

fn collect(api: &impl Api, mut scope: Vec<String>) -> Result<Snapshot> {
    if scope.is_empty() {
        scope = pages(api, "user/orgs")?
            .iter()
            .map(|o| {
                text(
                    o,
                    if api.forge() == Forge::Github {
                        "login"
                    } else {
                        "name"
                    },
                )
                .map(str::to_owned)
            })
            .collect::<Result<_>>()?;
    }
    scope.sort();
    scope.dedup();
    if scope.is_empty() {
        return Err(failure(
            "No organizations discovered; refusing an empty passing audit",
        ));
    }
    let mut snapshot = Snapshot {
        schema: 1,
        source: api.source(),
        collected_at: format!(
            "unix:{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
        ),
        scope,
        complete: true,
        errors: Vec::new(),
        organizations: Vec::new(),
    };
    for login in &snapshot.scope {
        if !cqlt::valid_login(login) {
            return Err(failure("Invalid organization identity from forge"));
        }
        match organization(api, login) {
            Ok(org) => {
                if matches!(org.profile, Document::Unknown { .. })
                    || org.repositories.iter().any(|r| {
                        matches!(r.readme, Document::Unknown { .. })
                            || matches!(r.license, Document::Unknown { .. })
                    })
                {
                    snapshot.complete = false;
                }
                snapshot.organizations.push(org);
            }
            Err(error) => {
                snapshot.complete = false;
                snapshot.errors.push(format!("{login}: {error}"));
            }
        }
    }
    Ok(snapshot)
}

fn organization(api: &impl Api, login: &str) -> Result<Organization> {
    let meta = required(api, &format!("orgs/{login}"))?;
    let rows = pages(
        api,
        &format!(
            "orgs/{login}/repos{}",
            if api.forge() == Forge::Github {
                "?type=all"
            } else {
                ""
            }
        ),
    )?;
    let count = rows.len();
    let mut repositories = Vec::new();
    let profile_repo = if api.forge() == Forge::Github {
        ".github"
    } else {
        ".profile"
    };
    let mut profile = Document::Missing;
    for row in rows {
        let mut repo = Repository {
            name: text(&row, "name")?.into(),
            full_name: text(&row, "full_name")?.into(),
            description: optional(&row, "description")?,
            homepage: optional(
                &row,
                if api.forge() == Forge::Github {
                    "homepage"
                } else {
                    "website"
                },
            )?,
            visibility: if boolean(&row, "private")? {
                Visibility::Private
            } else if row.get("visibility").and_then(Value::as_str) == Some("internal") {
                Visibility::Internal
            } else {
                Visibility::Public
            },
            fork: boolean(&row, "fork")?,
            archived: boolean(&row, "archived")?,
            revision: None,
            topics: Vec::new(),
            readme: Document::Unknown {
                reason: "Not collected".into(),
            },
            license: Document::Unknown {
                reason: "Not collected".into(),
            },
        };
        if repo.full_name != format!("{login}/{}", repo.name) {
            return Err(failure("Repository belongs to a different organization"));
        }
        let path = format!("repos/{}/{}", component(login), component(&repo.name));
        let topics = if api.forge() == Forge::Github {
            row.get("topics")
                .cloned()
                .ok_or_else(|| failure("Missing topics"))?
        } else {
            required(api, &format!("{path}/topics"))?
                .get("topics")
                .cloned()
                .ok_or_else(|| failure("Missing topics"))?
        };
        repo.topics = topics
            .as_array()
            .ok_or_else(|| failure("Expected topics array"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| failure("Invalid topic"))
            })
            .collect::<Result<_>>()?;
        let branch = optional(&row, "default_branch")?.unwrap_or_default();
        if branch.is_empty() || row.get("empty") == Some(&Value::Bool(true)) {
            repo.readme = Document::Missing;
            repo.license = Document::Missing;
        } else {
            match repository_documents(api, &path, &branch, repo.name == profile_repo) {
                Ok((revision, readme, license, org_profile)) => {
                    repo.revision = Some(revision);
                    repo.readme = readme;
                    repo.license = license;
                    if repo.name == profile_repo {
                        profile = org_profile;
                    }
                }
                Err(error) => {
                    let unknown = Document::Unknown {
                        reason: error.to_string(),
                    };
                    repo.readme = unknown.clone();
                    repo.license = unknown.clone();
                    if repo.name == profile_repo {
                        profile = unknown;
                    }
                }
            }
        }
        repositories.push(repo);
    }
    let org = Organization {
        login: login.into(),
        name: optional(
            &meta,
            if api.forge() == Forge::Github {
                "name"
            } else {
                "full_name"
            },
        )?,
        description: optional(&meta, "description")?,
        website: optional(
            &meta,
            if api.forge() == Forge::Github {
                "blog"
            } else {
                "website"
            },
        )?,
        profile,
        repository_count: count,
        repositories,
    };
    Ok(org)
}

fn listed_document(rows: &[Value], prefix: &str, names: &[&str]) -> Result<Document> {
    let mut candidates: Vec<_> = rows
        .iter()
        .filter(|e| {
            e.get("type").and_then(Value::as_str) == Some("file")
                && e.get("name").and_then(Value::as_str).is_some_and(|name| {
                    names.iter().any(|stem| {
                        let name = name.to_ascii_uppercase();
                        name == *stem
                            || name
                                .strip_prefix(stem)
                                .is_some_and(|suffix| suffix.starts_with(['.', '-', '_']))
                    })
                })
        })
        .collect();
    candidates.sort_by_key(|e| e.get("name").and_then(Value::as_str));
    for entry in candidates {
        let bytes = entry
            .get("size")
            .and_then(Value::as_u64)
            .ok_or_else(|| failure("Missing document size"))?;
        if bytes > 0 {
            return Ok(Document::Present {
                path: format!("{prefix}{}", text(entry, "name")?),
                bytes,
            });
        }
    }
    Ok(Document::Missing)
}

fn directory(api: &impl Api, path: &str) -> Result<Vec<Value>> {
    required(api, path)?
        .as_array()
        .cloned()
        .ok_or_else(|| failure("Expected contents directory"))
}

fn repository_documents(
    api: &impl Api,
    path: &str,
    branch: &str,
    is_profile: bool,
) -> Result<(String, Document, Document, Document)> {
    let branch = required(api, &format!("{path}/branches/{}", component(branch)))?;
    let commit = branch
        .get("commit")
        .ok_or_else(|| failure("Missing branch commit"))?;
    let revision = text(
        commit,
        if api.forge() == Forge::Github {
            "sha"
        } else {
            "id"
        },
    )?
    .to_owned();
    if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(failure("Invalid branch revision"));
    }
    let root = directory(api, &format!("{path}/contents?ref={revision}"))?;
    let license = listed_document(&root, "", &["LICENSE", "LICENCE", "COPYING"])?;
    let mut readme = if api.forge() == Forge::Github {
        match api.get(&format!("{path}/readme?ref={revision}"))? {
            Some(value) => Document::Present {
                path: text(&value, "path")?.into(),
                bytes: value
                    .get("size")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| failure("Missing README size"))?,
            },
            None => Document::Missing,
        }
    } else {
        listed_document(&root, "", &["README"])?
    };
    let mut profile = Document::Missing;
    if is_profile {
        if api.forge() == Forge::Github {
            if root
                .iter()
                .any(|e| e["name"] == "profile" && e["type"] == "dir")
            {
                let entries = directory(api, &format!("{path}/contents/profile?ref={revision}"))?;
                // GitHub requires this exact filename for organization profiles.
                let entries = entries
                    .into_iter()
                    .filter(|e| e["name"] == "README.md")
                    .collect::<Vec<_>>();
                profile = listed_document(&entries, "profile/", &["README"])?;
            }
        } else {
            let entries = root
                .into_iter()
                .filter(|e| e["name"] == "README.md")
                .collect::<Vec<_>>();
            profile = listed_document(&entries, "", &["README"])?;
        }
        if matches!(readme, Document::Missing) {
            readme = profile.clone();
        }
    }
    Ok((revision, readme, license, profile))
}

fn write_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut file, snapshot)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests;

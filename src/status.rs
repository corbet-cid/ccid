//! Native commit statuses, deliberately outside the check executor.
use crate::{failure, Environment, Result, Runner};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
pub struct Options {
    /// Trusted operator configuration; never read from a contribution checkout.
    #[arg(long, env = "CCID_STATUS_CONFIG")]
    pub config: PathBuf,
    /// Shared by all reporter invocations to serialize provider requests.
    #[arg(long, env = "CCID_STATUS_STATE_DIR")]
    pub state_dir: PathBuf,
    #[arg(long)]
    pub commit: String,
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub url: String,
    #[arg(long, value_enum)]
    pub state: State,
    /// Scheduler creation time, not the time the reporting process started.
    #[arg(long)]
    pub started: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Pending,
    Success,
    Failure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Provider {
    Forgejo,
    Gitlab,
    Bitbucket,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Target {
    provider: Provider,
    origin: String,
    repository: String,
    token_env: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: u32,
    targets: Vec<Target>,
}

fn origin(value: &str) -> Result<()> {
    let host = value.strip_prefix("https://").unwrap_or_default();
    if host.is_empty()
        || !host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-:".contains(&c))
        || host
            .to_ascii_lowercase()
            .split(':')
            .next()
            .is_some_and(|h| h == "github.com" || h.ends_with(".github.com"))
    {
        return Err(failure(
            "Status origins require HTTPS; GitHub is not a supported destination",
        ));
    }
    Ok(())
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

impl Target {
    fn validate(&self) -> Result<()> {
        origin(&self.origin)?;
        let parts: Vec<_> = self.repository.split('/').collect();
        if parts.len() < 2
            || (self.provider != Provider::Gitlab && parts.len() != 2)
            || parts.iter().any(|p| {
                p.is_empty()
                    || *p == "."
                    || *p == ".."
                    || !p
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            })
            || !self.token_env.starts_with("CCID_STATUS_")
            || !self
                .token_env
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
        {
            return Err(failure(
                "Invalid status repository or credential environment name",
            ));
        }
        // Cloud build statuses are the only supported Bitbucket API.
        if self.provider == Provider::Bitbucket && self.origin != "https://api.bitbucket.org" {
            return Err(failure("Bitbucket requires its Cloud API origin"));
        }
        Ok(())
    }

    fn paths(&self, sha: &str) -> (String, String) {
        match self.provider {
            Provider::Forgejo => (
                format!("/api/v1/repos/{}/git/commits/{sha}", self.repository),
                format!("/api/v1/repos/{}/statuses/{sha}", self.repository),
            ),
            Provider::Gitlab => (
                format!(
                    "/api/v4/projects/{}/repository/commits/{sha}",
                    encode(&self.repository)
                ),
                format!(
                    "/api/v4/projects/{}/statuses/{sha}",
                    encode(&self.repository)
                ),
            ),
            Provider::Bitbucket => (
                format!("/2.0/repositories/{}/commit/{sha}", self.repository),
                format!(
                    "/2.0/repositories/{}/commit/{sha}/statuses/build",
                    self.repository
                ),
            ),
        }
    }

    fn body(&self, options: &Options) -> Value {
        let state = match options.state {
            State::Pending => "pending",
            State::Success => "success",
            State::Failure => "failure",
        };
        match self.provider {
            Provider::Forgejo => {
                json!({"state":state,"context":options.name,"target_url":options.url,"description":format!("{}: {state}", options.name)})
            }
            Provider::Gitlab => {
                json!({"state":if options.state == State::Failure {"failed"} else {state},"name":options.name,"target_url":options.url,"description":format!("{}: {state}", options.name)})
            }
            Provider::Bitbucket => {
                json!({"state":match options.state {State::Pending => "INPROGRESS", State::Success => "SUCCESSFUL", State::Failure => "FAILED"},"key":format!("ccid-{:x}", Sha256::digest(&options.name))[..40],"name":options.name,"url":options.url})
            }
        }
    }
}

trait Api {
    fn request(
        &mut self,
        target: &Target,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value)>;
}

#[derive(Serialize, Deserialize)]
struct Posted {
    started: u64,
    url: String,
    state: State,
}

struct Journal {
    root: PathBuf,
    _lock: File,
}
impl Journal {
    fn open(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("lock"))?;
        lock.try_lock()
            .map_err(|_| failure("Status reporter busy; retry from scheduler"))?;
        Ok(Self { root, _lock: lock })
    }
    fn save(&self, key: &str, value: &impl Serialize) -> Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut temp, value)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(self.root.join(format!("{key}.json")))?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    fn load<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match fs::read(self.root.join(format!("{key}.json"))) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

struct Curl<'a> {
    journal: &'a Journal,
}
impl Api for Curl<'_> {
    fn request(
        &mut self,
        target: &Target,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value)> {
        // Persist the next request BEFORE contact, including across process crashes.
        let key = format!("pace-{:x}", Sha256::digest(&target.origin));
        let next: u64 = self.journal.load(&key)?.unwrap_or_default();
        let delay = next.saturating_sub(now()?);
        if delay > 30 {
            return Err(failure("Provider cooldown active; retry later"));
        }
        std::thread::sleep(Duration::from_secs(delay));
        self.journal.save(&key, &(now()? + 2))?;
        let token =
            std::env::var(&target.token_env).map_err(|_| failure("Missing status credential"))?;
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(failure("Invalid status credential"));
        }
        let mut environment = Environment::new();
        for name in ["PATH", "TMPDIR", "SSL_CERT_FILE", "SSL_CERT_DIR"] {
            if let Some(value) = std::env::var_os(name) {
                environment.insert(name.into(), value);
            }
        }
        environment.insert("CCID_STATUS_TOKEN".into(), token.into());
        let runner = Runner::until(
            std::env::current_dir()?,
            environment,
            Instant::now() + Duration::from_secs(40),
        )?
        .with_stderr_events()
        .without_child_stderr();
        let output = tempfile::NamedTempFile::new()?;
        let headers = tempfile::NamedTempFile::new()?;
        let mut input = tempfile::NamedTempFile::new()?;
        let header = match target.provider {
            Provider::Forgejo => "Authorization: token {{CCID_STATUS_TOKEN}}",
            Provider::Gitlab => "PRIVATE-TOKEN: {{CCID_STATUS_TOKEN}}",
            Provider::Bitbucket => "Authorization: Bearer {{CCID_STATUS_TOKEN}}",
        };
        let mut args: Vec<String> = [
            "curl",
            "--disable",
            "--silent",
            "--globoff",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--max-filesize",
            "4194304",
            "--proto",
            "=https",
            "--variable",
            "%CCID_STATUS_TOKEN",
            "--expand-header",
            header,
            "--header",
            "Accept: application/json",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        args.extend([
            "--output".into(),
            output.path().display().to_string(),
            "--dump-header".into(),
            headers.path().display().to_string(),
            "--write-out".into(),
            "%{http_code}".into(),
            "--url".into(),
            format!("{}{path}", target.origin),
        ]);
        if let Some(body) = body {
            serde_json::to_writer(&mut input, body)?;
            input.flush()?;
            args.extend([
                "--header".into(),
                "Content-Type: application/json".into(),
                "--data-binary".into(),
                format!("@{}", input.path().display()),
            ]);
        }
        let code = runner.run(&args, true)?.parse::<u16>()?;
        if code == 429 {
            let retry = fs::read_to_string(headers.path())?
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("retry-after")
                        .then(|| value.trim().parse::<u64>().ok())
                        .flatten()
                })
                .unwrap_or(300)
                .max(60);
            self.journal.save(&key, &(now()?.saturating_add(retry)))?;
        }
        // Do not surface response bodies, redirects, tokens or arbitrary server errors.
        let value = if (200..300).contains(&code) {
            serde_json::from_slice(&fs::read(output.path())?)?
        } else {
            Value::Null
        };
        Ok((code, value))
    }
}

fn report(
    api: &mut impl Api,
    journal: &Journal,
    config: &Config,
    options: &Options,
) -> Result<Value> {
    let mut results = Vec::new();
    let mut complete = true;
    for target in &config.targets {
        let key = format!(
            "status-{:x}",
            Sha256::digest(format!(
                "{}:{}:{}",
                serde_json::to_string(target)?,
                options.commit,
                options.name
            ))
        );
        let outcome = (|| -> Result<&str> {
            if let Some(previous) = journal.load::<Posted>(&key)? {
                if previous.started > options.started {
                    return Ok("superseded");
                }
                if previous.started == options.started {
                    if previous.url != options.url {
                        return Err(failure(
                            "Ambiguous scheduler identity at the same start time",
                        ));
                    }
                    if previous.state == options.state {
                        return Ok("already_reported");
                    }
                    if previous.state != State::Pending {
                        return Err(failure("Terminal result cannot be replaced"));
                    }
                }
            }
            let (commit_path, status_path) = target.paths(&options.commit);
            let (code, commit) = api.request(target, &commit_path, None)?;
            if code == 404 {
                return Ok("commit_absent");
            }
            let field = match target.provider {
                Provider::Forgejo => "sha",
                Provider::Gitlab => "id",
                Provider::Bitbucket => "hash",
            };
            if code != 200 || commit[field] != options.commit {
                return Err(failure(format!(
                    "Commit identity unavailable (HTTP {code})"
                )));
            }
            let intent = format!("intent-{key}");
            if let Some(previous) = journal.load::<Posted>(&intent)? {
                if previous.started == options.started && previous.state == options.state {
                    return Err(failure(
                        "Unresolved status POST; reconcile remotely before retrying",
                    ));
                }
            }
            journal.save(
                &intent,
                &Posted {
                    started: options.started,
                    url: options.url.clone(),
                    state: options.state,
                },
            )?;
            let (code, _) = api.request(target, &status_path, Some(&target.body(options)))?;
            if ![200, 201].contains(&code) {
                return Err(failure(format!(
                    "Status rejected (HTTP {code}); no automatic retry"
                )));
            }
            journal.save(
                &key,
                &Posted {
                    started: options.started,
                    url: options.url.clone(),
                    state: options.state,
                },
            )?;
            Ok("reported")
        })();
        match outcome {
            Ok(status) => results.push(
                json!({"provider":target.provider,"repository":target.repository,"status":status}),
            ),
            Err(error) => {
                complete = false;
                results.push(json!({"provider":target.provider,"repository":target.repository,"error":error.to_string()}));
            }
        }
    }
    Ok(
        json!({"complete":complete,"commit":options.commit,"name":options.name,"state":options.state,"results":results}),
    )
}

pub fn run(options: Options) -> Result<Value> {
    let config: Config = toml::from_str(&fs::read_to_string(&options.config)?)?;
    if config.schema != 1 || config.targets.is_empty() || config.targets.len() > 32 {
        return Err(failure("Status config requires schema 1 and 1..32 targets"));
    }
    if options.commit.len() != 40
        || !options
            .commit
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        || options.name.is_empty()
        || options.name.len() > 100
        || options.name.chars().any(char::is_control)
        || options.started == 0
        || options.started > now()?.saturating_add(60)
    {
        return Err(failure(
            "Invalid commit, check name or scheduler creation time",
        ));
    }
    let (scheme, rest) = options
        .url
        .split_once("://")
        .ok_or_else(|| failure("Run URL requires HTTPS"))?;
    let host = rest.split('/').next().unwrap_or_default();
    origin(&format!("{scheme}://{host}"))?;
    if options.url.len() > 2000
        || options.url.chars().any(char::is_whitespace)
        || options.url.contains('@')
    {
        return Err(failure("Invalid run URL"));
    }
    for (index, target) in config.targets.iter().enumerate() {
        target.validate()?;
        if config.targets[..index]
            .iter()
            .any(|t| t.origin == target.origin && t.repository == target.repository)
        {
            return Err(failure("Duplicate status target"));
        }
    }
    let journal = Journal::open(options.state_dir.clone())?;
    report(&mut Curl { journal: &journal }, &journal, &config, &options)
}

#[cfg(test)]
mod tests;

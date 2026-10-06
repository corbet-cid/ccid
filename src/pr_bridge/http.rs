use super::{Api, Config, Forge};
use crate::{failure, Environment, Result, Runner};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    time::{Duration, Instant},
};

pub(super) struct Client {
    config: Config,
    last_request: Option<Instant>,
    deadline: Instant,
    count: usize,
}

/// A child receives only its own credential, never the other forge's token or
/// ambient Git/curl configuration. Credentials are expanded inside curl.
pub(super) fn environment() -> Environment {
    let mut result = Environment::new();
    for key in ["PATH", "TMPDIR", "SSL_CERT_FILE", "SSL_CERT_DIR"] {
        if let Some(value) = std::env::var_os(key) {
            result.insert(key.into(), value);
        }
    }
    result
}

pub(super) fn token(name: &str) -> Result<String> {
    let value = std::env::var(name).map_err(|_| failure(format!("Set {name}")))?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(failure("Invalid credential format"));
    }
    Ok(value)
}

impl Client {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            config: config.clone(),
            last_request: None,
            deadline: Instant::now() + Duration::from_secs(600),
            count: 0,
        })
    }
}

impl Api for Client {
    fn request(
        &mut self,
        forge: Forge,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let source = format!("projects/{}/merge_requests/", self.config.gitlab_project);
        let primary = format!("repos/{}/pulls/", self.config.primary_repository);
        let numbered = |value: &str| value.parse::<u64>().is_ok_and(|n| n > 0);
        let source_write = forge == Forge::Gitlab
            && path.strip_prefix(&source).is_some_and(|rest| {
                (method == "PUT" && numbered(rest))
                    || (method == "POST" && rest.strip_suffix("/notes").is_some_and(numbered))
            });
        let primary_write = forge == Forge::Forgejo
            && ((method == "POST"
                && path == format!("repos/{}/pulls", self.config.primary_repository))
                || (method == "PATCH" && path.strip_prefix(&primary).is_some_and(numbered)));
        if method != "GET" && !source_write && !primary_write {
            return Err(failure("Unsupported bridge API mutation"));
        }
        if self.count >= 200 {
            return Err(failure("Bridge API request budget exhausted"));
        }
        if let Some(last) = self.last_request {
            std::thread::sleep(Duration::from_secs(1).saturating_sub(last.elapsed()));
        }
        self.count += 1;
        self.last_request = Some(Instant::now());
        let (base, name, header) = match forge {
            Forge::Gitlab => (
                format!("{}/api/v4", self.config.gitlab),
                "CCID_BRIDGE_GITLAB_TOKEN",
                "PRIVATE-TOKEN: {{CCID_BRIDGE_TOKEN}}",
            ),
            Forge::Forgejo => (
                format!("{}/api/v1", self.config.forgejo),
                "CCID_BRIDGE_FORGEJO_TOKEN",
                "Authorization: token {{CCID_BRIDGE_TOKEN}}",
            ),
        };
        let mut environment = environment();
        environment.insert("CCID_BRIDGE_TOKEN".into(), token(name)?.into());
        let runner = Runner::until(std::env::current_dir()?, environment, self.deadline)?
            .with_stderr_events()
            .without_child_stderr();
        let output = tempfile::NamedTempFile::new()?;
        let mut input = tempfile::NamedTempFile::new()?;
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
            "%CCID_BRIDGE_TOKEN",
            "--expand-header",
            header,
            "--header",
            "Accept: application/json",
            "--output",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        args.push(output.path().to_string_lossy().into_owned());
        args.extend([
            "--write-out".into(),
            "%{http_code}".into(),
            "--request".into(),
            method.into(),
            "--url".into(),
            format!("{base}/{path}"),
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
        // No redirects, retry flags, response bodies in errors, or ambient proxy.
        let status = runner.run(&args, true)?;
        if status != if method == "POST" { "201" } else { "200" } {
            return Err(failure(format!(
                "Bridge API {method} failed with HTTP {status}; no automatic retry"
            )));
        }
        Ok(serde_json::from_slice(&fs::read(output.path())?)?)
    }
}

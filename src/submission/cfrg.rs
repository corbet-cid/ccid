//! Forge reads through cfrg. ccid never talks to a forge itself: file
//! contents, branch heads and commit statuses come from the `cfrg` command
//! line (`cfrg contents`, `cfrg observe`), which owns the forge dialects, the
//! pacing and the rate windows.
use super::*;
use base64::Engine;

/// Variable that carries the forge token to cfrg. Never an argument.
const TOKEN_ENV: &str = "CFRG_FORGE_TOKEN";

/// One line of `cfrg contents`: what was found for one repository branch.
#[derive(Debug, Deserialize)]
pub(super) struct Line {
    pub repository: String,
    pub state: String,
    #[serde(default)]
    pub files: Vec<LineFile>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct LineFile {
    pub path: String,
    pub blob: String,
    /// Base64 of the bytes; absent for a blob the caller listed as known.
    #[serde(default)]
    pub content: Option<String>,
}

impl LineFile {
    pub fn text(&self) -> Result<Option<String>> {
        self.content
            .as_deref()
            .map(|packed| {
                let bytes = base64::engine::general_purpose::STANDARD.decode(packed)?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            })
            .transpose()
    }
}

/// The forge as cfrg sees it: one origin, one token.
pub(super) struct Cfrg {
    binary: String,
    origin: String,
    token: String,
}

impl Cfrg {
    /// Reads the forge token with the submission configuration's command. The
    /// origin is the host of a repository's canonical clone URL.
    pub fn open(config: &Config, binary: &str, clone_url: &str) -> Result<Self> {
        if config.forge_token_command.is_empty() {
            return Err(
                "Reading the forge needs forge_token_command in the submission configuration"
                    .into(),
            );
        }
        let token = String::from_utf8(output(&config.forge_token_command, None, None)?)?
            .trim()
            .to_owned();
        Ok(Self {
            binary: binary.to_owned(),
            origin: origin(clone_url)?,
            token,
        })
    }

    /// The token also authorises `cfrg land`.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Run cfrg. Exit 0 and 2 both carry output (2: some targets were not
    /// read); anything else is a failure with cfrg's own last message.
    fn run(&self, subcommand: &str, tail: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut command = Command::new(&self.binary);
        command
            .arg(subcommand)
            .args(["--forge", "forgejo", "--origin", self.origin.as_str()])
            .args(["--token-env", TOKEN_ENV])
            .args(tail)
            .env(TOKEN_ENV, &self.token)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        // cfrg reads its whole query before it prints, but feed concurrently
        // anyway so neither side can block the other.
        let result = std::thread::scope(|scope| {
            let writer = input.map(|bytes| {
                let mut stdin = child.stdin.take().expect("piped input");
                scope.spawn(move || stdin.write_all(bytes))
            });
            let result = child.wait_with_output();
            if let Some(writer) = writer {
                writer.join().map_err(|_| "Input writer panicked")??;
            }
            Result::Ok(result?)
        })?;
        match result.status.code() {
            Some(0 | 2) if result.status.success() || !result.stdout.is_empty() => {
                Ok(result.stdout)
            }
            _ => {
                let message = String::from_utf8_lossy(&result.stderr)
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("no message")
                    .to_owned();
                Err(format!("cfrg failed ({}): {message}", result.status).into())
            }
        }
    }

    /// Read `paths` of every `(repository, branch)` target in one call. Blobs
    /// listed in `known` come back without their bytes.
    pub fn contents(
        &self,
        targets: &[(String, String)],
        paths: &[&str],
        known: &BTreeSet<String>,
    ) -> Result<Vec<Line>> {
        let query = json!({
            "targets": targets
                .iter()
                .map(|(repository, branch)| json!({"repository": repository, "branch": branch}))
                .collect::<Vec<_>>(),
            "paths": paths,
            "known": known,
        });
        let stdout = self.run("contents", &[], Some(&serde_json::to_vec(&query)?))?;
        String::from_utf8_lossy(&stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| Ok(serde_json::from_str(line)?))
            .collect()
    }

    /// Head commit of a branch; `None` when the branch does not exist.
    pub fn head(&self, repository: &str, branch: &str) -> Result<Option<String>> {
        let stdout = self.run("observe", &["head", repository, branch], None)?;
        let line: Value = serde_json::from_slice(&stdout)?;
        Ok(line["head"].as_str().map(str::to_owned))
    }

    /// Latest status per context of one exact commit, as `(context, state)`
    /// with the state one of `success`, `failure`, `pending`.
    pub fn statuses(&self, repository: &str, commit: &str) -> Result<Vec<(String, String)>> {
        let stdout = self.run("observe", &["status", repository, commit], None)?;
        let line: Value = serde_json::from_slice(&stdout)?;
        Ok(rows(&line["statuses"])
            .iter()
            .map(|s| (text(s, "context"), text(s, "state")))
            .collect())
    }
}

/// `https://host[:port]` of a canonical HTTPS clone URL.
pub(super) fn origin(clone_url: &str) -> Result<String> {
    let url = url::Url::parse(clone_url)?;
    if url.scheme() != "https" {
        return Err("The forge needs an https clone URL".into());
    }
    let host = url.host_str().ok_or("Forge host missing")?;
    Ok(match url.port() {
        Some(port) => format!("https://{host}:{port}"),
        None => format!("https://{host}"),
    })
}

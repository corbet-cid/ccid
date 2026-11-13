//! The forge's release API, spoken through `curl` (like every other ccid transfer): credentials are
//! private files handed to curl with `-K`, never command-line arguments.
//!
//! Two credentials exist. The job token writes releases and assets but cannot read the assets of a
//! DRAFT back (the forge answers 404); the runner's read-only credential can, and is used for reading
//! and nothing else (`CCID_FORGE_READ_TOKEN_FILE`, else `$CREDENTIALS_DIRECTORY/forgejo-ci-read`).

use crate::{failure, Result};
use serde::Deserialize;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

/// One file of a release as the forge lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub id: u64,
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub browser_download_url: String,
}

/// A release as the forge lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub id: u64,
    pub tag_name: String,
    #[serde(default)]
    pub target_commitish: String,
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// The body of a request.
pub enum Body<'a> {
    Json(&'a serde_json::Value),
    File(&'a Path),
}

#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The release API of one repository on one forge.
pub struct Forge {
    server: String,
    api: String,
    repository: String,
    scratch: tempfile::TempDir,
    auth: PathBuf,
    reads: Vec<PathBuf>,
    /// The pause before a repeated upload grows with each attempt (`CCID_RELEASE_RETRY_DELAY`, seconds).
    pub retry_delay: Duration,
}

fn plain_token(token: &str) -> bool {
    token.len() >= 8
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn plain_repository(repository: &str) -> bool {
    let mut parts = repository.split('/');
    let plain = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    };
    matches!((parts.next(), parts.next(), parts.next()), (Some(o), Some(n), None) if plain(o) && plain(n))
}

fn secret_file(dir: &Path, name: &str, token: &str) -> Result<PathBuf> {
    let path = dir.join(name);
    let mut file = fs::File::create(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    writeln!(file, "header = \"Authorization: token {token}\"")?;
    Ok(path)
}

impl Forge {
    /// The forge the job runs against, from the variables the runner exports
    /// (`GITHUB_SERVER_URL`, `GITHUB_REPOSITORY`, `GITHUB_API_URL`) and the job token
    /// (`CCID_FORGE_TOKEN`, else `FORGEJO_TOKEN`, else `GITHUB_TOKEN`).
    pub fn from_env() -> Result<Forge> {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let server = var("GITHUB_SERVER_URL")
            .ok_or_else(|| failure("GITHUB_SERVER_URL (the forge's address) is required"))?;
        let repository = var("GITHUB_REPOSITORY")
            .ok_or_else(|| failure("GITHUB_REPOSITORY (owner/name) is required"))?;
        let token = ["CCID_FORGE_TOKEN", "FORGEJO_TOKEN", "GITHUB_TOKEN"]
            .into_iter()
            .find_map(var)
            .ok_or_else(|| failure("the Forgejo job token (CCID_FORGE_TOKEN) is required"))?;
        let read_file = var("CCID_FORGE_READ_TOKEN_FILE").or_else(|| {
            var("CREDENTIALS_DIRECTORY").map(|dir| format!("{dir}/forgejo-ci-read"))
        });
        let read_token = read_file
            .and_then(|path| fs::read_to_string(path).ok())
            .map(|text| text.trim().to_owned())
            .filter(|text| plain_token(text));
        let mut forge = Forge::new(
            &server,
            var("GITHUB_API_URL").as_deref(),
            &repository,
            &token,
            read_token.as_deref(),
        )?;
        if let Some(seconds) = var("CCID_RELEASE_RETRY_DELAY").and_then(|v| v.parse().ok()) {
            forge.retry_delay = Duration::from_secs(seconds);
        }
        Ok(forge)
    }

    pub fn new(
        server: &str,
        api: Option<&str>,
        repository: &str,
        token: &str,
        read_token: Option<&str>,
    ) -> Result<Forge> {
        let server = server.trim_end_matches('/').to_owned();
        // The loopback form exists for the offline fixtures only.
        if !(server.starts_with("https://") || server.starts_with("http://127.0.0.1:")) {
            return Err(failure("the forge address must be an https:// URL"));
        }
        if !plain_repository(repository) {
            return Err(failure(format!(
                "{repository:?} is not a repository (owner/name)"
            )));
        }
        if !plain_token(token) {
            return Err(failure("the Forgejo job token is not a plain token"));
        }
        let scratch = tempfile::tempdir()?;
        let auth = secret_file(scratch.path(), "auth", token)?;
        let mut reads = vec![auth.clone()];
        if let Some(read) = read_token.filter(|read| plain_token(read)) {
            reads.insert(0, secret_file(scratch.path(), "read-auth", read)?);
        }
        Ok(Forge {
            api: api
                .map(|api| api.trim_end_matches('/').to_owned())
                .unwrap_or_else(|| format!("{server}/api/v1")),
            server,
            repository: repository.to_owned(),
            scratch,
            auth,
            reads,
            retry_delay: Duration::from_secs(20),
        })
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    /// Whether a credential that can read the assets of a draft is configured.
    pub fn can_read_drafts(&self) -> bool {
        self.reads.len() > 1
    }

    fn url(&self, path: &str) -> String {
        format!("{}/repos/{}/{}", self.api, self.repository, path)
    }

    fn curl(
        &self,
        auth: &Path,
        method: &str,
        url: &str,
        body: Option<Body<'_>>,
        max_time: u64,
        output: &Path,
    ) -> Result<u16> {
        let mut command = Command::new("curl");
        command
            .args(["-q", "--silent", "--show-error", "--max-time"])
            .arg(max_time.to_string())
            .arg("-K")
            .arg(auth)
            .args(["-X", method, "-o"])
            .arg(output)
            .args(["-w", "%{http_code}"]);
        let staged;
        match body {
            Some(Body::Json(value)) => {
                staged = self.scratch.path().join("request.json");
                fs::write(&staged, serde_json::to_vec(value)?)?;
                command
                    .args(["-H", "Content-Type: application/json", "--data-binary"])
                    .arg(format!("@{}", staged.display()));
            }
            Some(Body::File(path)) => {
                command
                    .args([
                        "-H",
                        "Content-Type: application/octet-stream",
                        "--data-binary",
                    ])
                    .arg(format!("@{}", path.display()));
            }
            None => {}
        }
        command.arg(url);
        let result = command
            .output()
            .map_err(|error| failure(format!("cannot run curl: {error}")))?;
        if !result.status.success() {
            return Err(failure(format!(
                "request failed: {method} {}: {}",
                url.strip_prefix(&self.api).unwrap_or(url),
                String::from_utf8_lossy(&result.stderr).trim()
            )));
        }
        String::from_utf8_lossy(&result.stdout)
            .trim()
            .parse()
            .map_err(|_| failure(format!("curl reported no HTTP status for {method} {url}")))
    }

    /// One request with the job token.
    pub fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<Body<'_>>,
        max_time: u64,
    ) -> Result<Reply> {
        let out = self.scratch.path().join("response");
        let status = self.curl(&self.auth, method, &self.url(path), body, max_time, &out)?;
        let body = fs::read(&out).unwrap_or_default();
        Ok(Reply { status, body })
    }

    /// The reply, or an error naming the request unless the forge answered 2xx.
    fn expect(&self, method: &str, path: &str, body: Option<Body<'_>>, max_time: u64) -> Result<Reply> {
        let reply = self.call(method, path, body, max_time)?;
        if reply.ok() {
            Ok(reply)
        } else {
            Err(failure(format!(
                "the forge answered {} to {method} {path}",
                reply.status
            )))
        }
    }

    fn json(reply: &Reply) -> Result<serde_json::Value> {
        serde_json::from_slice(&reply.body).map_err(|error| failure(format!("the forge answered with no JSON: {error}")))
    }

    /// The commit a branch points at.
    pub fn branch_head(&self, branch: &str) -> Result<String> {
        let reply = self.expect("GET", &format!("branches/{branch}"), None, 60)?;
        Self::json(&reply)?
            .pointer("/commit/id")
            .and_then(|id| id.as_str())
            .map(str::to_owned)
            .ok_or_else(|| failure(format!("the forge reports no commit for branch {branch}")))
    }

    /// The release carrying `tag` (draft or public); `None` only after the whole list was scanned.
    pub fn release(&self, tag: &str) -> Result<Option<Release>> {
        for page in 1..=20 {
            let reply = self.expect("GET", &format!("releases?limit=50&page={page}"), None, 60)?;
            let list: Vec<serde_json::Value> = serde_json::from_slice(&reply.body)
                .map_err(|_| failure("the forge's release list is not a JSON array"))?;
            let mut found = list
                .iter()
                .filter(|item| item.get("tag_name").and_then(|t| t.as_str()) == Some(tag));
            if let Some(item) = found.next() {
                if found.next().is_some() {
                    return Err(failure(format!("more than one release carries the tag {tag}")));
                }
                let release: Release = serde_json::from_value(item.clone())
                    .map_err(|error| failure(format!("release {tag} is malformed: {error}")))?;
                if release.id == 0 {
                    return Err(failure(format!("release {tag} has no id")));
                }
                return Ok(Some(release));
            }
            if list.is_empty() {
                return Ok(None);
            }
        }
        Err(failure(format!("too many releases to scan for {tag}")))
    }

    /// The commit a tag points at, if the tag exists.
    pub fn tag_commit(&self, tag: &str) -> Result<Option<String>> {
        let reply = self.call("GET", &format!("tags/{tag}"), None, 60)?;
        match reply.status {
            404 => Ok(None),
            s if (200..300).contains(&s) => Ok(Self::json(&reply)?
                .pointer("/commit/sha")
                .and_then(|sha| sha.as_str())
                .map(str::to_owned)),
            other => Err(failure(format!("the forge answered {other} to GET tags/{tag}"))),
        }
    }

    /// The state of the commit status carrying `context` (`success`, `pending`, `failure`, `error`);
    /// `None` when the commit has no such status.
    pub fn context_status(&self, sha: &str, context: &str) -> Result<Option<String>> {
        let reply = self.call("GET", &format!("commits/{sha}/status?limit=100"), None, 60)?;
        match reply.status {
            404 => Ok(None),
            s if (200..300).contains(&s) => {
                let value = Self::json(&reply)?;
                Ok(value
                    .get("statuses")
                    .and_then(|list| list.as_array())
                    .into_iter()
                    .flatten()
                    .find(|item| item.get("context").and_then(|c| c.as_str()) == Some(context))
                    .and_then(|item| item.get("status").or_else(|| item.get("state")))
                    .and_then(|state| state.as_str())
                    .map(str::to_owned))
            }
            other => Err(failure(format!("the forge answered {other} to GET commits/{sha}/status"))),
        }
    }

    pub fn create_draft(&self, tag: &str, sha: &str, title: &str, notes: &str) -> Result<()> {
        let body = serde_json::json!({
            "tag_name": tag, "target_commitish": sha, "name": title, "body": notes,
            "draft": true, "prerelease": false
        });
        self.expect("POST", "releases", Some(Body::Json(&body)), 120)
            .map(|_| ())
    }

    /// Make the draft the public release `tag`.
    pub fn publish(&self, id: u64, tag: &str, name: &str, notes: &str) -> Result<()> {
        let body = serde_json::json!({
            "tag_name": tag, "name": name, "body": notes, "draft": false, "prerelease": false
        });
        self.expect("PATCH", &format!("releases/{id}"), Some(Body::Json(&body)), 120)
            .map(|_| ())
    }

    pub fn delete_release(&self, id: u64) -> Result<()> {
        self.expect("DELETE", &format!("releases/{id}"), None, 120)
            .map(|_| ())
    }

    pub fn delete_asset(&self, release: u64, asset: u64) -> Result<()> {
        self.expect(
            "DELETE",
            &format!("releases/{release}/assets/{asset}"),
            None,
            120,
        )
        .map(|_| ())
    }

    /// The seconds an upload of `size` bytes may take: 600 s, plus 1 s per 20 KB (a slow uplink).
    pub fn upload_limit(size: u64) -> u64 {
        size / 20_000 + 600
    }

    /// One upload attempt.
    pub fn upload(&self, release: u64, name: &str, path: &Path, size: u64) -> Result<()> {
        self.expect(
            "POST",
            &format!("releases/{release}/assets?name={name}"),
            Some(Body::File(path)),
            Self::upload_limit(size),
        )
        .map(|_| ())
    }

    /// Download `asset` to `destination` with the read credential first, the job token second.
    pub fn download(&self, asset: &Asset, destination: &Path) -> Result<()> {
        let path = asset
            .browser_download_url
            .strip_prefix(&self.server)
            .map(str::to_owned)
            .or_else(|| {
                // The forge may know itself under another address: keep the path.
                let rest = asset.browser_download_url.split_once("://")?.1;
                Some(rest[rest.find('/')?..].to_owned())
            })
            .ok_or_else(|| failure(format!("asset {} has no download address", asset.name)))?;
        let url = format!("{}{}", self.server, path);
        let part = destination.with_extension("ccid-part");
        let mut last = 0;
        for auth in &self.reads {
            let status = self.curl(auth, "GET", &url, None, Self::upload_limit(asset.size), &part)?;
            if status == 200 {
                let got = fs::metadata(&part)?.len();
                if got != asset.size {
                    let _ = fs::remove_file(&part);
                    return Err(failure(format!(
                        "asset {} arrived with {got} of {} bytes",
                        asset.name, asset.size
                    )));
                }
                fs::rename(&part, destination)?;
                return Ok(());
            }
            last = status;
        }
        let _ = fs::remove_file(&part);
        Err(failure(format!(
            "the forge answered {last} reading the asset {} (the job token cannot read the assets of a draft; the runner's read credential can)",
            asset.name
        )))
    }

    /// Start a workflow of this repository on `git_ref`.
    pub fn dispatch(&self, workflow: &str, git_ref: &str) -> Result<()> {
        let body = serde_json::json!({ "ref": git_ref });
        self.expect(
            "POST",
            &format!("actions/workflows/{workflow}/dispatches"),
            Some(Body::Json(&body)),
            60,
        )
        .map(|_| ())
    }
}

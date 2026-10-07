//! Dependency discovery from the forge.
//!
//! Which active repositories pin or track a given canonical git identity?
//! The answer is read from each repository's default branch on the forge
//! (root manifests `Cargo.lock`, `Cargo.toml`, `flake.lock`, `flake.nix`),
//! never from local checkouts. A blob is fetched once ever: extracted
//! references are cached by git blob id, so a warm scan costs one tree
//! request per repository.
use super::core::Api;
use super::*;
use regex::Regex;
use std::{fmt, sync::LazyLock, time::Duration};

pub(super) const MANIFESTS: [&str; 4] = ["Cargo.lock", "Cargo.toml", "flake.lock", "flake.nix"];
const BACKOFF_SECONDS: [u64; 4] = [2, 4, 8, 16];

static QUOTED_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""((?:git\+)?(?:https?|ssh)://[^"\s]+)""#).expect("static reference pattern")
});

/// A non-2xx forge answer.
#[derive(Debug)]
pub(super) struct HttpStatus(pub u16);
impl fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Forge request failed: HTTP {}", self.0)
    }
}
impl std::error::Error for HttpStatus {}
fn status_of(error: &(dyn std::error::Error + 'static)) -> Option<u16> {
    error.downcast_ref::<HttpStatus>().map(|s| s.0)
}

/// Read-only forge API client: paced, and patient with rate limits (429).
pub(super) struct Forge {
    base: String,
    token: Option<String>,
    pause: Duration,
}
impl Forge {
    pub fn new(base: String, token: Option<String>) -> Self {
        Self {
            base,
            token,
            pause: Duration::from_millis(100),
        }
    }
    /// The API root for a repository's clone URL (`https://host/api/v1`).
    pub fn root(clone_url: &str) -> Result<String> {
        let url = url::Url::parse(clone_url)?;
        if url.scheme() != "https" {
            return Err("Forge API needs an https clone URL".into());
        }
        Ok(format!(
            "https://{}/api/v1",
            url.host_str().ok_or("Forge host missing")?
        ))
    }
}
impl Api for Forge {
    fn call(&self, path: &str, _: Option<&Value>) -> Result<Value> {
        if !path.starts_with('/') || path.contains(['\r', '\n', '#']) {
            return Err("Invalid forge API path".into());
        }
        let mut waits = BACKOFF_SECONDS.iter();
        loop {
            std::thread::sleep(self.pause);
            let response = transport::http(
                &format!("{}{path}", self.base),
                self.token.as_deref(),
                None,
                16 * 1024 * 1024,
            )?;
            match response.status {
                200..=299 => return Ok(serde_json::from_slice(&response.data)?),
                429 => match waits.next() {
                    Some(seconds) => std::thread::sleep(Duration::from_secs(*seconds)),
                    None => return Err(Box::new(HttpStatus(429))),
                },
                other => return Err(Box::new(HttpStatus(other))),
            }
        }
    }
}

/// Every git URL a manifest mentions. Cargo and Nix text is scanned for
/// quoted URLs; a `flake.lock` is read as JSON so locked entries carry their
/// revision as `url#rev`.
pub(super) fn references(file: &str, content: &str) -> Vec<String> {
    let mut found = BTreeSet::new();
    if file == "flake.lock" {
        if let Ok(lock) = serde_json::from_str::<Value>(content) {
            for node in lock["nodes"]
                .as_object()
                .into_iter()
                .flat_map(|n| n.values())
            {
                for side in ["original", "locked"] {
                    if let Some(url) = node[side]["url"].as_str() {
                        match node[side]["rev"].as_str() {
                            Some(rev) => found.insert(format!("{url}#{rev}")),
                            None => found.insert(url.to_string()),
                        };
                    }
                }
            }
            return found.into_iter().collect();
        }
    }
    for capture in QUOTED_URL.captures_iter(content) {
        found.insert(capture[1].to_string());
    }
    found.into_iter().collect()
}

/// Decides whether a reference points at the landed repository.
pub(super) struct Matcher<'a> {
    pub identity: &'a str,
    pub aliases: &'a BTreeMap<String, String>,
    /// Hosts whose references match by repository name only (legacy mirrors).
    pub legacy_hosts: &'a [String],
}
impl Matcher<'_> {
    /// `Some(pin)` when the reference points at the landed repository; the pin
    /// is the exact commit when the reference carries one.
    pub fn pin(&self, reference: &str) -> Option<Option<String>> {
        let raw = reference.strip_prefix("git+").unwrap_or(reference);
        let (base, fragment) = raw
            .split_once('#')
            .map_or((raw, None), |(b, f)| (b, Some(f)));
        let pin = fragment.filter(|f| exact_sha(f)).map(str::to_string);
        if let Ok(canonical) = core::canonical_remote(base, self.aliases) {
            if canonical == self.identity || canonical.starts_with(&format!("{}/", self.identity)) {
                return Some(pin);
            }
        }
        let url = url::Url::parse(base).ok()?;
        if !["https", "http", "ssh"].contains(&url.scheme()) {
            return None;
        }
        let host = url.host_str()?;
        let name = self.identity.rsplit('/').next()?;
        let last = url
            .path_segments()?
            .filter(|s| !s.is_empty())
            .nth(1)?
            .trim_end_matches(".git");
        (self.legacy_hosts.iter().any(|h| h == host) && last == name).then_some(pin)
    }
}

/// References cached by git blob id (content addressed, never stale).
#[derive(Default, Serialize, Deserialize)]
pub(super) struct Cache {
    blobs: BTreeMap<String, Vec<String>>,
}
impl Cache {
    pub fn load(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        save(path, &serde_json::to_value(self)?)
    }
}

pub(super) struct Found {
    pub record: Value,
    pub files: Vec<String>,
    pub pins: BTreeSet<String>,
}
#[derive(Default)]
pub(super) struct Scan {
    pub dependents: Vec<Found>,
    pub scanned: usize,
    pub fetched: usize,
    pub unreadable: Vec<String>,
}

fn blob_text(forge: &dyn Api, full_name: &str, sha: &str) -> Result<String> {
    use base64::Engine;
    let blob = forge.call(&format!("/repos/{full_name}/git/blobs/{sha}"), None)?;
    let packed: String = text(&blob, "content").split_whitespace().collect();
    Ok(
        String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD.decode(packed)?)
            .into_owned(),
    )
}

/// Scan every active repository except the landed one.
pub(super) fn scan(
    forge: &dyn Api,
    records: &[Value],
    matcher: &Matcher,
    cache: &mut Cache,
) -> Scan {
    let mut result = Scan::default();
    for record in records {
        let full_name = text(record, "full_name");
        let branch = match text(record, "default_branch").as_str() {
            "" => "main".to_string(),
            b => b.to_string(),
        };
        if record["active"] != true
            || core::canonical_remote(&text(record, "clone_url"), matcher.aliases)
                .ok()
                .as_deref()
                == Some(matcher.identity)
        {
            continue;
        }
        result.scanned += 1;
        let tree = match forge.call(&format!("/repos/{full_name}/git/trees/{branch}"), None) {
            Ok(tree) => tree,
            Err(e) if matches!(status_of(e.as_ref()), Some(404 | 409)) => continue,
            Err(e) => {
                result.unreadable.push(format!("{full_name} ({e})"));
                continue;
            }
        };
        let mut found = Found {
            record: record.clone(),
            files: vec![],
            pins: BTreeSet::new(),
        };
        for entry in rows(&tree["tree"]) {
            let path = text(entry, "path");
            let sha = text(entry, "sha");
            if text(entry, "type") != "blob"
                || !MANIFESTS.contains(&path.as_str())
                || !exact_sha(&sha)
            {
                continue;
            }
            if !cache.blobs.contains_key(&sha) {
                match blob_text(forge, &full_name, &sha) {
                    Ok(content) => {
                        result.fetched += 1;
                        cache.blobs.insert(sha.clone(), references(&path, &content));
                    }
                    Err(e) => {
                        result.unreadable.push(format!("{full_name}/{path} ({e})"));
                        continue;
                    }
                }
            }
            let mut hit = false;
            for reference in &cache.blobs[&sha] {
                if let Some(pin) = matcher.pin(reference) {
                    hit = true;
                    found.pins.extend(pin);
                }
            }
            if hit {
                found.files.push(path);
            }
        }
        if !found.files.is_empty() {
            result.dependents.push(found);
        }
    }
    result
        .dependents
        .sort_by_key(|f| text(&f.record, "full_name"));
    result
}

//! Read-only GitHub transport and provider decision primitives. The freeze is
//! enforced before transport, including dispatch and cancellation requests.
#![cfg_attr(not(test), allow(dead_code))]
use super::*;
use std::io::Cursor;

const MAX_ARTIFACT: u64 = 32 * 1024 * 1024;
pub(super) trait Hosted {
    fn get(&self, path: &str) -> Result<Value>;
    fn pages(&self, path: &str, key: &str) -> Result<Vec<Value>>;
    fn artifact(&self, repository: &str, metadata: &Value) -> Result<Vec<u8>>;
    fn release_asset(&self, repository: &str, revision: &str) -> Result<(Vec<u8>, Value)>;
}
impl Hosted for GitHub {
    fn get(&self, path: &str) -> Result<Value> {
        self.get(path)
    }
    fn pages(&self, path: &str, key: &str) -> Result<Vec<Value>> {
        self.pages(path, key)
    }
    fn artifact(&self, repository: &str, metadata: &Value) -> Result<Vec<u8>> {
        self.artifact(repository, metadata)
    }
    fn release_asset(&self, repository: &str, revision: &str) -> Result<(Vec<u8>, Value)> {
        self.release_asset(repository, revision)
    }
}
pub(super) struct GitHub {
    token: String,
}
impl GitHub {
    #[cfg(test)]
    pub(super) fn fixture() -> Self {
        Self {
            token: "fixture".into(),
        }
    }
    pub fn new() -> Result<Self> {
        let token = String::from_utf8(output(
            &strings(&["gh", "auth", "token", "--hostname", "github.com"]),
            None,
            None,
        )?)?
        .trim()
        .to_owned();
        if token.is_empty() {
            return Err("GitHub authentication unavailable".into());
        }
        Ok(Self { token })
    }
    pub fn request(&self, method: &str, path: &str) -> Result<transport::Response> {
        if method != "GET" {
            return Err("GitHub is frozen; writes are prohibited".into());
        }
        if !path.starts_with("/repos/") || path.contains(['\r', '\n', '#']) {
            return Err("Unsupported GitHub API path".into());
        }
        transport::http(
            &format!("https://api.github.com{path}"),
            Some(&self.token),
            None,
            8 * 1024 * 1024,
        )
    }
    pub fn get(&self, path: &str) -> Result<Value> {
        let response = self.request("GET", path)?;
        if response.status != 200 {
            return Err(format!("GitHub metadata unavailable (HTTP {})", response.status).into());
        }
        Ok(serde_json::from_slice(&response.data)?)
    }
    pub fn pages(&self, path: &str, key: &str) -> Result<Vec<Value>> {
        let mut all = Vec::new();
        let mut previous = Vec::new();
        for page in 1..=10 {
            let data = self.get(&format!(
                "{path}{}per_page=100&page={page}",
                if path.contains('?') { "&" } else { "?" }
            ))?;
            let batch = data[key]
                .as_array()
                .ok_or("Invalid GitHub pagination metadata")?;
            let ids: Vec<_> = batch.iter().map(|r| r["id"].clone()).collect();
            if !batch.is_empty() && ids == previous {
                return Err("GitHub repeated a page".into());
            }
            all.extend(batch.clone());
            if batch.len() < 100 {
                return Ok(all);
            }
            previous = ids;
        }
        Err("GitHub pagination inventory incomplete".into())
    }
    pub fn artifact(&self, repository: &str, metadata: &Value) -> Result<Vec<u8>> {
        let id = number(metadata, "id");
        let hash = text(metadata, "digest");
        if id == 0
            || metadata["expired"] == true
            || !hash.starts_with("sha256:")
            || !exact_digest(&hash[7..])
        {
            return Err("Artifact lacks verified identity".into());
        }
        let response = self.request(
            "GET",
            &format!("/repos/{repository}/actions/artifacts/{id}/zip"),
        )?;
        if ![301, 302, 303, 307].contains(&response.status) {
            return Err("Artifact location unavailable".into());
        }
        let location = response.location.ok_or("Artifact redirect missing")?;
        let url = url::Url::parse(&location)?;
        let host = url.host_str().unwrap_or("");
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some_and(|p| p != 443)
            || ![
                ".blob.core.windows.net",
                ".githubusercontent.com",
                ".github.com",
            ]
            .iter()
            .any(|s| host.ends_with(s))
        {
            return Err("Artifact redirect outside GitHub storage".into());
        }
        let response = transport::http(&location, None, None, MAX_ARTIFACT)?;
        if response.status != 200 || sha(&response.data) != hash[7..] {
            return Err("Artifact digest or size verification failed".into());
        }
        Ok(response.data)
    }
    pub fn public_download(&self, url: &str) -> Result<Vec<u8>> {
        let mut location = public_url(url)?;
        for _ in 0..4 {
            let response = transport::http(location.as_str(), None, None, MAX_ARTIFACT)?;
            if response.status == 200 {
                return Ok(response.data);
            }
            if ![301, 302, 303, 307, 308].contains(&response.status) {
                return Err("Public asset download failed".into());
            }
            let next = location.join(&response.location.ok_or("Asset redirect missing")?)?;
            location = public_url(next.as_str())?;
        }
        Err("Public asset redirect chain too long".into())
    }
    pub fn release_asset(&self, repository: &str, revision: &str) -> Result<(Vec<u8>, Value)> {
        if !exact_sha(revision) {
            return Err("Release requires immutable ccid revision".into());
        }
        let tag = format!("ccid-{revision}");
        let name = format!("{tag}-linux-x86_64.zip");
        let release = self.get(&format!("/repos/{repository}/releases/tags/{tag}"))?;
        if release["tag_name"] != tag || release["draft"] == true || release["prerelease"] == true {
            return Err("Release does not match bootstrap tag".into());
        }
        let selected: Vec<_> = release["assets"]
            .as_array()
            .ok_or("Release asset inventory missing")?
            .iter()
            .filter(|a| a["name"] == name)
            .collect();
        if selected.len() != 1 {
            return Err("Release lacks one exact asset".into());
        }
        let asset = selected[0];
        let hash = text(asset, "digest");
        if number(asset, "id") == 0
            || number(asset, "size") == 0
            || number(asset, "size") > MAX_ARTIFACT
            || asset["state"] != "uploaded"
            || !matches(r"^sha256:[0-9a-f]{64}$", &hash)
        {
            return Err("Release asset lacks verified identity".into());
        }
        let bytes = self.public_download(&text(asset, "browser_download_url"))?;
        if bytes.len() as u64 != number(asset, "size") || sha(&bytes) != hash[7..] {
            return Err("Release digest or size verification failed".into());
        }
        Ok((bytes, asset.clone()))
    }
}
pub(super) fn public_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|p| p != 443)
        || ![
            "github.com",
            "objects.githubusercontent.com",
            "release-assets.githubusercontent.com",
        ]
        .contains(&url.host_str().unwrap_or(""))
        || url.path().is_empty()
    {
        return Err("Release URL outside public GitHub download hosts".into());
    }
    Ok(url)
}
fn zip_entry(
    bundle: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    limit: u64,
) -> Result<Vec<u8>> {
    let entry = bundle.by_name(name)?;
    if entry.size() > limit {
        return Err("Receipt entry too large".into());
    }
    let mut data = Vec::new();
    entry.take(limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return Err("Receipt exceeds bounded size".into());
    }
    Ok(data)
}
pub(super) fn tool_receipt(content: &[u8], revision: &str) -> Result<Value> {
    let mut bundle = zip::ZipArchive::new(Cursor::new(content))?;
    if bundle.len() != 2
        || bundle.file_names().collect::<BTreeSet<_>>() != BTreeSet::from(["ccid", "receipt.json"])
    {
        return Err("Tool artifact has unexpected or duplicate entries".into());
    }
    let receipt: Value =
        serde_json::from_slice(&zip_entry(&mut bundle, "receipt.json", MAX_ARTIFACT)?)?;
    let binary = zip_entry(&mut bundle, "ccid", MAX_ARTIFACT)?;
    if receipt["source_revision"] != revision
        || receipt["binary_sha256"] != sha(binary)
        || receipt["target"] != "x86_64-unknown-linux-gnu"
        || text(&receipt, "rustc").trim().is_empty()
    {
        return Err("Tool artifact receipt source/platform mismatch".into());
    }
    Ok(receipt)
}
pub(super) fn result_receipt(content: &[u8], expected: &Value) -> Result<Value> {
    let mut bundle = zip::ZipArchive::new(Cursor::new(content))?;
    let names: Vec<_> = bundle.file_names().map(str::to_string).collect();
    if names.len() != bundle.len()
        || names.iter().collect::<BTreeSet<_>>().len() != names.len()
        || !names.iter().any(|n| n == "result.json")
        || names.iter().any(|n| {
            ![
                "result.json",
                "check.log",
                "status",
                "guard_status",
                "guard.log",
            ]
            .contains(&n.as_str())
        })
    {
        return Err("Invalid check receipt layout".into());
    }
    let receipt: Value =
        serde_json::from_slice(&zip_entry(&mut bundle, "result.json", 128 * 1024)?)?;
    for (key, value) in expected
        .as_object()
        .ok_or("Expected receipt identity must be object")?
    {
        let actual = &receipt[key];
        if actual != value
            && actual.as_str() != Some(&value.to_string())
            && value.as_str() != Some(&actual.to_string())
        {
            return Err("Result does not cover requested immutable inputs".into());
        }
    }
    if receipt["exit_code"].as_i64() != Some(0)
        || receipt["guard_status"].as_i64() != Some(0)
        || ["rustc", "node", "bun"]
            .iter()
            .any(|k| !receipt[*k].is_string())
    {
        return Err("Result failed or lacks runtime provenance".into());
    }
    Ok(receipt)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Eligible,
    Ineligible,
    Unavailable,
    Ambiguous,
}
pub(super) fn classify(status: Option<u16>, sent: bool) -> Decision {
    match status {
        Some(200..=299) => Decision::Eligible,
        Some(401 | 403 | 429) => Decision::Unavailable,
        Some(400 | 404 | 405 | 409 | 422) => Decision::Ineligible,
        _ if sent => Decision::Ambiguous,
        _ => Decision::Unavailable,
    }
}
pub(super) fn eligible(reference: &Value) -> bool {
    reference["secret_free"] == true && reference["free_eligible"] == true
}
pub(super) fn exact_match(reference: &Value, run: &Value) -> bool {
    [
        "repository",
        "workflow",
        "source_ref",
        "source_commit",
        "check",
        "request_id",
        "platform",
        "workflow_digest",
        "manifest_digest",
        "config_digest",
        "dependency_snapshot",
        "toolchain",
        "environment",
    ]
    .iter()
    .all(|k| {
        reference[*k].is_string() && !text(reference, k).is_empty() && reference[*k] == run[*k]
    })
}
pub(super) fn find_exact(reference: &Value, runs: &[Value]) -> Decision {
    if !eligible(reference) {
        return Decision::Ineligible;
    }
    match runs.iter().filter(|r| exact_match(reference, r)).count() {
        0 => Decision::Unavailable,
        1 => Decision::Eligible,
        _ => Decision::Ambiguous,
    }
}
pub(super) fn kind(run: &Value) -> &str {
    match text(run, "status").as_str() {
        "queued" | "requested" | "waiting" | "pending" => "queued",
        "in_progress" => "running",
        "completed" => match text(run, "conclusion").as_str() {
            "success" => "success",
            "cancelled" => "cancelled",
            "failure" | "timed_out" | "action_required" | "startup_failure" | "stale" => "failure",
            _ => "unknown",
        },
        _ => "unknown",
    }
}

// Kept as pure response classifiers for historical provider contracts; the
// transport above cannot dispatch or cancel during the freeze.
pub(super) fn dispatch_response(reference: &Value, response: &Value) -> Decision {
    if !eligible(reference) {
        return Decision::Ineligible;
    }
    let decision = classify(
        response["status_code"]
            .as_u64()
            .and_then(|s| u16::try_from(s).ok()),
        true,
    );
    if decision != Decision::Eligible {
        return decision;
    }
    if response["status_code"] != 200
        || number(response, "workflow_run_id") == 0
        || response
            .get("run")
            .is_some_and(|r| !exact_match(reference, r))
    {
        return Decision::Ambiguous;
    }
    Decision::Eligible
}

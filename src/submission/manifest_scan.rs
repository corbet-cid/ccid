//! Dependency discovery from the forge.
//!
//! Which active repositories pin or track a given canonical git identity?
//! The answer is read from each repository's default branch on the forge
//! (root manifests `Cargo.lock`, `Cargo.toml`, `flake.lock`, `flake.nix`),
//! never from local checkouts, and always through cfrg. A blob is fetched
//! once ever: extracted references are cached by git blob id and handed to
//! cfrg as known blobs, so a warm scan costs one tree request per repository.
use super::cfrg::Line;
use super::*;
use regex::Regex;
use std::sync::LazyLock;

pub(super) const MANIFESTS: [&str; 4] = ["Cargo.lock", "Cargo.toml", "flake.lock", "flake.nix"];

static QUOTED_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""((?:git\+)?(?:https?|ssh)://[^"\s]+)""#).expect("static reference pattern")
});

/// The manifests of many repositories, as the forge holds them.
pub(super) trait Manifests {
    /// One line per `(repository, branch)` target. Blobs in `known` come back
    /// without their bytes.
    fn read(&self, targets: &[(String, String)], known: &BTreeSet<String>) -> Result<Vec<Line>>;
}
impl Manifests for super::cfrg::Cfrg {
    fn read(&self, targets: &[(String, String)], known: &BTreeSet<String>) -> Result<Vec<Line>> {
        self.contents(targets, &MANIFESTS, known)
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

/// Scan every active repository except the landed one.
pub(super) fn scan(
    manifests: &dyn Manifests,
    records: &[Value],
    matcher: &Matcher,
    cache: &mut Cache,
) -> Scan {
    let mut result = Scan::default();
    let mut targets = Vec::new();
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
        targets.push((record, full_name, branch));
    }
    let wanted: Vec<(String, String)> = targets
        .iter()
        .map(|(_, name, branch)| (name.clone(), branch.clone()))
        .collect();
    let known: BTreeSet<String> = cache.blobs.keys().cloned().collect();
    let lines = match manifests.read(&wanted, &known) {
        Ok(lines) => lines,
        Err(e) => {
            result
                .unreadable
                .extend(targets.iter().map(|(_, name, _)| format!("{name} ({e})")));
            return result;
        }
    };
    let answers: BTreeMap<&str, &Line> = lines.iter().map(|l| (l.repository.as_str(), l)).collect();
    for (record, full_name, _) in targets {
        let Some(line) = answers.get(full_name.as_str()) else {
            result.unreadable.push(format!("{full_name} (no answer)"));
            continue;
        };
        match line.state.as_str() {
            "found" => {}
            // No such branch, or an empty repository: nothing to depend on.
            "absent" => continue,
            _ => {
                let reason = line.error.as_deref().unwrap_or("not read");
                result.unreadable.push(format!("{full_name} ({reason})"));
                continue;
            }
        }
        let mut found = Found {
            record: record.clone(),
            files: vec![],
            pins: BTreeSet::new(),
        };
        for file in &line.files {
            if !MANIFESTS.contains(&file.path.as_str()) || !exact_sha(&file.blob) {
                continue;
            }
            if !cache.blobs.contains_key(&file.blob) {
                match file.text() {
                    Ok(Some(content)) => {
                        result.fetched += 1;
                        cache
                            .blobs
                            .insert(file.blob.clone(), references(&file.path, &content));
                    }
                    Ok(None) => {
                        result
                            .unreadable
                            .push(format!("{full_name}/{} (bytes not delivered)", file.path));
                        continue;
                    }
                    Err(e) => {
                        result
                            .unreadable
                            .push(format!("{full_name}/{} ({e})", file.path));
                        continue;
                    }
                }
            }
            let mut hit = false;
            for reference in &cache.blobs[&file.blob] {
                if let Some(pin) = matcher.pin(reference) {
                    hit = true;
                    found.pins.extend(pin);
                }
            }
            if hit {
                found.files.push(file.path.clone());
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

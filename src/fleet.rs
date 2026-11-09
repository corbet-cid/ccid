//! Fleet data: the first-party owner prefixes and their legacy aliases (`docs/fleet.md`).
//!
//! The data is never compiled in. It rides on the release channel document (`channel.json`,
//! member `fleet`), which every runner already fetches; a check reads it from the file
//! `CCID_FLEET_FILE` names. A check that reads it declares `fleet = true`: its result keys (the
//! verdict store and the moon cache, through `cache_env`) then include the digest of the canonical
//! fleet bytes, so an edit of the fleet data can never replay a result computed with other data.

use crate::{failure, set, value, Environment, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

/// Path of a release channel document that carries fleet data.
pub const FLEET_FILE_ENV: &str = "CCID_FLEET_FILE";
/// Lowercase sha256 hex of the canonical fleet bytes the running check was keyed with.
pub const FLEET_DIGEST_ENV: &str = "CCID_FLEET_DIGEST";
/// `1` inside a check that declares `fleet = true`.
pub const FLEET_KEYED_ENV: &str = "CCID_FLEET_KEYED";
/// The fleet data schema this ccid understands.
pub const SCHEMA: u32 = 1;

/// Set by `ccid check` in the environment of every check command it runs.
const INSIDE_CHECK_ENV: &str = "CCID_TARGET_LOCK_HELD";
const NOT_DECLARED: &str =
    "this check reads fleet data; declare fleet = true on it in .ci/ccid.toml";
const CHANGED: &str = "fleet data changed after the check was keyed";

/// The `fleet` member of the release channel.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fleet {
    pub schema: u32,
    /// Canonical owner prefixes `https://<host>/<owner>/`, strictly ascending.
    pub first_party: Vec<String>,
    /// Legacy owner prefix -> the canonical prefix it moved to.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
}

/// Where a URL belongs according to the fleet data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin<'a> {
    /// Under a canonical first-party owner prefix.
    Canonical { prefix: &'a str },
    /// Under a legacy prefix that moved to `canonical`.
    Alias { legacy: &'a str, canonical: &'a str },
}

/// A fleet prefix: `https://<lowercase host>/<segment>/` in canonical URL form, no credentials,
/// port, query or fragment. The trailing `/` makes byte-prefix matching exact per owner.
fn validate_prefix(kind: &str, entry: &str) -> Result<()> {
    let refuse = |reason: String| failure(format!("fleet {kind} {entry:?}: {reason}"));
    let url = url::Url::parse(entry).map_err(|error| refuse(format!("is not a URL ({error})")))?;
    if url.scheme() != "https" {
        return Err(refuse("must use https".into()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(refuse("must not carry credentials".into()));
    }
    if url.port().is_some() {
        return Err(refuse("must not name a port".into()));
    }
    if url.query().is_some() {
        return Err(refuse("must not carry a query".into()));
    }
    if url.fragment().is_some() {
        return Err(refuse("must not carry a fragment".into()));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(refuse("must name a host".into()));
    }
    if let Some(authority) = entry
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
    {
        if authority != authority.to_ascii_lowercase() {
            return Err(refuse("the host must be lowercase".into()));
        }
    }
    if !entry.ends_with('/') {
        return Err(refuse("must end with /".into()));
    }
    let segment = url
        .path()
        .strip_prefix('/')
        .and_then(|path| path.strip_suffix('/'))
        .unwrap_or_default();
    if segment.is_empty()
        || segment == "."
        || segment == ".."
        || !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(refuse(
            "the path must be exactly /<owner>/ with an owner of [A-Za-z0-9._-]".into(),
        ));
    }
    // Byte-prefix matching compares the spelling, so the spelling must be the one the URL
    // parser produces (no default port, no surrounding whitespace, no case or escape variants).
    if url.as_str() != entry {
        return Err(refuse(format!(
            "is not in canonical URL form (it parses as {:?})",
            url.as_str()
        )));
    }
    Ok(())
}

impl Fleet {
    pub fn validate(&self) -> Result<()> {
        if self.schema != SCHEMA {
            return Err(failure(format!(
                "fleet schema {} is not supported by this ccid (supported: {SCHEMA})",
                self.schema
            )));
        }
        if self.first_party.is_empty() {
            return Err(failure(
                "fleet first_party must name at least one owner prefix",
            ));
        }
        for pair in self.first_party.windows(2) {
            if pair[0] >= pair[1] {
                return Err(failure(format!(
                    "fleet first_party must be sorted ascending without duplicates: {:?} is not before {:?}",
                    pair[0], pair[1]
                )));
            }
        }
        for prefix in &self.first_party {
            validate_prefix("first_party prefix", prefix)?;
        }
        for (legacy, canonical) in &self.aliases {
            validate_prefix("alias", legacy)?;
            validate_prefix("alias target", canonical)?;
            if !self.first_party.contains(canonical) {
                return Err(failure(format!(
                    "fleet alias {legacy:?} -> {canonical:?}: the target is not a first_party prefix"
                )));
            }
            if self.first_party.contains(legacy) {
                return Err(failure(format!(
                    "fleet alias {legacy:?} is itself a first_party prefix"
                )));
            }
        }
        Ok(())
    }

    /// The bytes the digest names: the validated data serialized in field order (`first_party`
    /// is sorted by validation, `aliases` is ordered by key), so JSON spelling never matters.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }

    pub fn digest(&self) -> Result<String> {
        Ok(format!("{:x}", Sha256::digest(self.canonical_bytes()?)))
    }

    /// Classify a dependency or remote URL by byte prefix after one optional `git+`.
    pub fn classify<'a>(&'a self, url: &str) -> Option<Origin<'a>> {
        let url = url.strip_prefix("git+").unwrap_or(url);
        if let Some(prefix) = self
            .first_party
            .iter()
            .find(|prefix| url.starts_with(prefix.as_str()))
        {
            return Some(Origin::Canonical { prefix });
        }
        self.aliases
            .iter()
            .find(|(legacy, _)| url.starts_with(legacy.as_str()))
            .map(|(legacy, canonical)| Origin::Alias { legacy, canonical })
    }

    /// Git configuration that routes every legacy prefix to its canonical one:
    /// `("url.<canonical>.insteadOf", "<legacy>")`, ordered by legacy prefix.
    pub fn insteadof_pairs(&self) -> Vec<(String, String)> {
        self.aliases
            .iter()
            .map(|(legacy, canonical)| (format!("url.{canonical}.insteadOf"), legacy.clone()))
            .collect()
    }
}

/// The validated fleet data of a release channel document.
pub fn from_channel(bytes: &[u8]) -> Result<Fleet> {
    let channel: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(bytes)
        .map_err(|error| {
            failure(format!(
                "the release channel is not a JSON object ({error})"
            ))
        })?;
    let fleet = channel
        .get("fleet")
        .ok_or_else(|| failure("the release channel carries no fleet data"))?;
    let fleet: Fleet = serde_json::from_value(fleet.clone()).map_err(|error| {
        failure(format!(
            "the release channel's fleet data is malformed: {error}"
        ))
    })?;
    fleet.validate()?;
    Ok(fleet)
}

/// The fleet data of the channel document `CCID_FLEET_FILE` names, and that path.
pub fn load(environment: &Environment) -> Result<(Fleet, PathBuf)> {
    let path = value(environment, FLEET_FILE_ENV).ok_or_else(|| {
        failure(format!(
            "fleet data unavailable: set {FLEET_FILE_ENV} to a copy of the release channel (channel.json)"
        ))
    })?;
    let path = PathBuf::from(path);
    let bytes = read_regular(&path).map_err(|error| {
        failure(format!(
            "fleet data unavailable: {FLEET_FILE_ENV} {}: {error}",
            path.display()
        ))
    })?;
    let fleet = from_channel(&bytes)
        .map_err(|error| failure(format!("{error} ({FLEET_FILE_ENV} {})", path.display())))?;
    Ok((fleet, path))
}

/// The bytes of a regular file; a symbolic link is refused like the runner's channel file.
pub(crate) fn read_regular(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(failure(
            "must be a regular file (symbolic links are refused)",
        ));
    }
    Ok(fs::read(path)?)
}

/// A consumer of fleet data inside a ccid check proves that the check was keyed with exactly
/// the data it read: the check declares `fleet = true` and its key digest is this fleet's.
/// Outside a ccid check nothing is keyed, so nothing is required.
pub fn require_keyed(environment: &Environment, fleet: &Fleet) -> Result<()> {
    if !environment.contains_key(OsStr::new(INSIDE_CHECK_ENV)) {
        return Ok(());
    }
    if value(environment, FLEET_KEYED_ENV).as_deref() != Some("1") {
        return Err(failure(NOT_DECLARED));
    }
    if value(environment, FLEET_DIGEST_ENV).as_deref() != Some(fleet.digest()?.as_str()) {
        return Err(failure(CHANGED));
    }
    Ok(())
}

/// `fleet = true` declares the digest as keyed environment: the verdict store and cmnp's moon
/// tasks both key `cache_env`. The digest variable is reserved for such checks, so a moon task
/// declaration with it in `cache_env` always belongs to a check that requires fleet data.
pub(crate) fn normalise(checks: &mut BTreeMap<String, crate::Check>) -> Result<()> {
    for (name, check) in checks.iter_mut() {
        let declared = check
            .cache_env
            .iter()
            .any(|entry| entry == FLEET_DIGEST_ENV);
        if check.fleet && !declared {
            check.cache_env.push(FLEET_DIGEST_ENV.to_owned());
        } else if !check.fleet && declared {
            return Err(failure(format!(
                "Check {name} lists {FLEET_DIGEST_ENV} in cache_env without fleet = true"
            )));
        }
    }
    Ok(())
}

/// Before anything of a selection is keyed or run: when a selected check declares
/// `fleet = true`, load the fleet data and bind its digest into the environment passed on.
///
/// An inherited digest is the key an enclosing computation already used, so it must name the
/// same data (never silently replaced). A moon task (`cache_child`) must have inherited one:
/// its cache key was computed by the parent, and a task keyed without the digest never runs.
pub(crate) fn bind_selection(
    checks: &BTreeMap<String, crate::Check>,
    selected: &[String],
    environment: &mut Environment,
    cache_child: bool,
) -> Result<()> {
    let Some(name) = selected.iter().find(|name| checks[*name].fleet) else {
        return Ok(());
    };
    let (fleet, path) = load(environment).map_err(|error| {
        failure(format!(
            "Check {name} declares fleet = true but no fleet data is available: {error}"
        ))
    })?;
    let digest = fleet.digest()?;
    match value(environment, FLEET_DIGEST_ENV) {
        Some(keyed) if keyed != digest => Err(failure(format!(
            "{CHANGED} ({FLEET_DIGEST_ENV} is {keyed}, {FLEET_FILE_ENV} {} holds {digest})",
            path.display()
        ))),
        Some(_) => Ok(()),
        None if cache_child => Err(failure(format!(
            "Check {name} declares fleet = true but its cached task was keyed without {FLEET_DIGEST_ENV}"
        ))),
        None => {
            set(environment, FLEET_DIGEST_ENV, digest);
            Ok(())
        }
    }
}

/// `ccid fleet` subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum Action {
    /// Print the fleet data CCID_FLEET_FILE names as one JSON line with its digest and source.
    /// Inside a ccid check it requires fleet = true on that check.
    Show,
    /// Validate the fleet data of a release channel document and print its digest.
    Validate {
        /// The release channel document (channel.json).
        #[arg(long)]
        channel: PathBuf,
    },
}

pub fn run(action: &Action) -> Result<u8> {
    match action {
        Action::Show => {
            let environment: Environment = std::env::vars_os().collect();
            let (fleet, source) = load(&environment)?;
            require_keyed(&environment, &fleet)?;
            // `fleet` is the canonical bytes themselves, so the line's own member hashes to `digest`.
            let canonical = String::from_utf8(fleet.canonical_bytes()?)?;
            println!(
                "{{\"fleet\":{canonical},\"digest\":{},\"source\":{}}}",
                serde_json::Value::from(fleet.digest()?),
                serde_json::Value::from(source.to_string_lossy())
            );
        }
        Action::Validate { channel } => {
            let bytes = fs::read(channel)
                .map_err(|error| failure(format!("cannot read {}: {error}", channel.display())))?;
            println!("fleet data valid: {}", from_channel(&bytes)?.digest()?);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests;

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
    borrow::Cow,
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
/// Further spellings of the forge host that this machine uses (comma-separated, e.g. an
/// in-cluster name). Set by the runner, never by a repository; merged after the channel's
/// `forge_hosts` and never part of the fleet digest.
pub const FORGE_HOSTS_ENV: &str = "CCID_FORGE_HOSTS";
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
    /// Every host name of the one forge that serves `first_party`; the first is canonical.
    #[serde(default)]
    pub forge_hosts: Vec<String>,
    /// Canonical owner prefixes `https://<host>/<owner>/`, strictly ascending.
    pub first_party: Vec<String>,
    /// Legacy owner prefix -> the canonical prefix it moved to.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// This machine's further spellings of the forge host (`CCID_FORGE_HOSTS`, added by `load`).
    /// Not channel data: never serialized, so never part of the canonical bytes or the digest.
    #[serde(skip)]
    runtime_hosts: Vec<String>,
}

/// Where a URL belongs according to the fleet data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin<'a> {
    /// Under a canonical first-party owner prefix.
    Canonical { prefix: &'a str },
    /// Under a legacy prefix that moved to `canonical`.
    Alias { legacy: &'a str, canonical: &'a str },
}

/// A forge host name: a lowercase DNS name (`[a-z0-9.-]+`), no scheme, port or path.
fn validate_host(kind: &str, host: &str) -> Result<()> {
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-.".contains(&b))
    {
        return Err(failure(format!(
            "fleet {kind} {host:?}: must be a lowercase DNS name ([a-z0-9.-], no scheme, port or path)"
        )));
    }
    Ok(())
}

/// The host of a validated prefix (`https://<host>/<owner>/`).
fn prefix_host(prefix: &str) -> &str {
    prefix
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default()
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
        for (index, host) in self.forge_hosts.iter().enumerate() {
            validate_host("forge host", host)?;
            if self.forge_hosts[..index].contains(host) {
                return Err(failure(format!(
                    "fleet forge host {host:?} is listed twice"
                )));
            }
        }
        if let Some(canonical) = self.forge_hosts.first() {
            for prefix in &self.first_party {
                if prefix_host(prefix) != canonical {
                    return Err(failure(format!(
                        "fleet first_party prefix {prefix:?} is not on the canonical forge host {canonical:?} (forge_hosts[0])"
                    )));
                }
            }
        }
        for host in &self.runtime_hosts {
            validate_host(&format!("forge host from {FORGE_HOSTS_ENV}"), host)?;
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
            if self
                .all_forge_hosts()
                .any(|host| host == prefix_host(legacy))
            {
                return Err(failure(format!(
                    "fleet alias {legacy:?} is on a forge host: aliases name other forges"
                )));
            }
        }
        Ok(())
    }

    /// The bytes the digest names: the validated channel data serialized in field order
    /// (`schema`, `forge_hosts`, `first_party` sorted by validation, `aliases` ordered by key), so
    /// JSON spelling never matters. This machine's `CCID_FORGE_HOSTS` are not part of them: a host
    /// spelling names a route to the forge, never what a check computes.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }

    pub fn digest(&self) -> Result<String> {
        Ok(format!("{:x}", Sha256::digest(self.canonical_bytes()?)))
    }

    /// The forge's host names: the channel's `forge_hosts`, then this machine's
    /// `CCID_FORGE_HOSTS`. The first is canonical.
    pub fn all_forge_hosts(&self) -> impl Iterator<Item = &str> {
        self.forge_hosts
            .iter()
            .chain(&self.runtime_hosts)
            .map(String::as_str)
    }

    /// Whether two host names name the same forge: equal, or both spellings of the fleet's forge.
    pub fn same_forge(&self, a: &str, b: &str) -> bool {
        a == b
            || (self.all_forge_hosts().any(|host| host == a)
                && self.all_forge_hosts().any(|host| host == b))
    }

    /// Add this machine's further spellings of the forge host (`CCID_FORGE_HOSTS`: comma-separated
    /// lowercase DNS names). They need a canonical host in the channel's `forge_hosts`; a spelling
    /// the fleet already knows is skipped. The canonical bytes and the digest never change.
    pub fn add_forge_hosts(&mut self, list: &str) -> Result<()> {
        if self.forge_hosts.is_empty() {
            return Err(failure(format!(
                "{FORGE_HOSTS_ENV} needs a canonical forge host, but the release channel's fleet data has no forge_hosts"
            )));
        }
        let mut added = self.clone();
        for host in list.split(',').map(str::trim) {
            validate_host(&format!("forge host from {FORGE_HOSTS_ENV}"), host)?;
            if !added.all_forge_hosts().any(|known| known == host) {
                added.runtime_hosts.push(host.to_owned());
            }
        }
        added.validate()?;
        *self = added;
        Ok(())
    }

    /// One spelling per forge: `https://<any forge host>/` becomes `https://<forge_hosts[0]>/`.
    fn canonical_spelling<'u>(&self, url: &'u str) -> Cow<'u, str> {
        let Some(canonical) = self.forge_hosts.first() else {
            return Cow::Borrowed(url);
        };
        match url
            .strip_prefix("https://")
            .and_then(|rest| rest.split_once('/'))
        {
            Some((host, path))
                if host != canonical && self.all_forge_hosts().any(|known| known == host) =>
            {
                Cow::Owned(format!("https://{canonical}/{path}"))
            }
            _ => Cow::Borrowed(url),
        }
    }

    /// Classify a dependency or remote URL by byte prefix after one optional `git+`, with every
    /// spelling of the forge host read as the canonical one.
    pub fn classify<'a>(&'a self, url: &str) -> Option<Origin<'a>> {
        let url = url.strip_prefix("git+").unwrap_or(url);
        let url = self.canonical_spelling(url);
        let url = url.as_ref();
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

/// The fleet data of the channel document `CCID_FLEET_FILE` names, and that path; this
/// machine's `CCID_FORGE_HOSTS` are added to its forge host names.
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
    let mut fleet = from_channel(&bytes)
        .map_err(|error| failure(format!("{error} ({FLEET_FILE_ENV} {})", path.display())))?;
    if let Some(hosts) = value(environment, FORGE_HOSTS_ENV) {
        fleet
            .add_forge_hosts(&hosts)
            .map_err(|error| failure(format!("fleet data unusable: {error}")))?;
    }
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

/// The release channel copy the rendered Forgejo shim keeps:
/// `${XDG_CACHE_HOME:-${HOME:-/tmp}/.cache}/ccid-channel/last-good.json`.
pub(crate) fn shim_channel_copy(environment: &Environment) -> PathBuf {
    channel_state(environment).join("last-good.json")
}

/// The shim's channel state directory; job runners keep their fleet snapshots beside its copy.
fn channel_state(environment: &Environment) -> PathBuf {
    let base = match value(environment, "XDG_CACHE_HOME") {
        Some(cache) => PathBuf::from(cache),
        None => PathBuf::from(value(environment, "HOME").unwrap_or_else(|| "/tmp".into()))
            .join(".cache"),
    };
    base.join("ccid-channel")
}

/// What a job runner (`job-run`, `execute-job`) hands its job about fleet data.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum JobFleet {
    /// The runner or the job already names `CCID_FLEET_FILE`: nothing is added.
    Provided,
    /// No channel copy exists: `fleet = true` checks fail closed.
    Absent,
    /// A channel copy (`source`) carries valid fleet data; the job gets `CCID_FLEET_FILE=<path>`,
    /// the immutable snapshot of that data (or the copy itself when no snapshot can be written).
    Found {
        path: String,
        digest: String,
        source: String,
    },
    /// A channel copy exists but carries no valid fleet data.
    Invalid { error: String },
}

/// Reads of a channel copy that does not parse as JSON yet: another job's shim may be rewriting it.
const TORN_READ_ATTEMPTS: u32 = 5;
const TORN_READ_PAUSE: std::time::Duration = std::time::Duration::from_millis(50);

/// The fleet data a job runner hands its job: from the runner's `CCID_CHANNEL_FILE`, else the
/// shim's last fetched channel. `CCID_CHANNEL_FILE` itself is never set here. `declared` is the
/// job's own environment, which wins.
pub(crate) fn job_fleet(
    environment: &Environment,
    declared: &BTreeMap<String, String>,
) -> JobFleet {
    if value(environment, FLEET_FILE_ENV).is_some()
        || declared.get(FLEET_FILE_ENV).is_some_and(|v| !v.is_empty())
    {
        return JobFleet::Provided;
    }
    let source = value(environment, "CCID_CHANNEL_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| shim_channel_copy(environment));
    if fs::symlink_metadata(&source).is_err() {
        return JobFleet::Absent;
    }
    let Some(text) = source.to_str().map(str::to_owned) else {
        return JobFleet::Invalid {
            error: format!("{} is not a UTF-8 path", source.display()),
        };
    };
    let mut attempt = 1;
    let bytes = loop {
        match read_regular(&source) {
            Ok(bytes)
                if serde_json::from_slice::<serde_json::Value>(&bytes).is_err()
                    && attempt < TORN_READ_ATTEMPTS => {}
            other => break other,
        }
        attempt += 1;
        std::thread::sleep(TORN_READ_PAUSE);
    };
    let fleet = match bytes.and_then(|bytes| from_channel(&bytes)) {
        Ok(fleet) => fleet,
        Err(error) => {
            return JobFleet::Invalid {
                error: format!("{text}: {error}"),
            }
        }
    };
    let digest = match fleet.digest() {
        Ok(digest) => digest,
        Err(error) => {
            return JobFleet::Invalid {
                error: format!("{text}: {error}"),
            }
        }
    };
    let path = write_snapshot(&channel_state(environment), &fleet, &digest)
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or_else(|| text.clone());
    JobFleet::Found {
        path,
        digest,
        source: text,
    }
}

/// One immutable file per distinct fleet, `fleet-<digest>.json` holding `{"fleet": <canonical
/// bytes>}`: written once through a temporary file renamed in the same directory, so a reader
/// never sees it partial and a later channel fetch (another job's shim rewriting its copy) never
/// changes the data a running job was keyed with.
fn write_snapshot(directory: &Path, fleet: &Fleet, digest: &str) -> Result<PathBuf> {
    use std::io::Write;
    let path = directory.join(format!("fleet-{digest}.json"));
    let mut bytes = b"{\"fleet\":".to_vec();
    bytes.extend(fleet.canonical_bytes()?);
    bytes.extend(b"}\n");
    if read_regular(&path).is_ok_and(|existing| existing == bytes) {
        return Ok(path);
    }
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path)?;
    Ok(path)
}

/// `job_fleet` with its report on stderr: the `CCID_FLEET_FILE` value to add to the job, if any.
/// Only `fleet = true` checks need fleet data and they fail closed without it, so a missing or
/// invalid channel copy never stops the job here.
pub(crate) fn provide_for_job(
    runner: &str,
    environment: &Environment,
    declared: &BTreeMap<String, String>,
) -> Option<String> {
    match job_fleet(environment, declared) {
        JobFleet::Found {
            path,
            digest,
            source,
        } => {
            eprintln!("ccid {runner}: fleet data {digest} from {source} ({FLEET_FILE_ENV}={path})");
            Some(path)
        }
        JobFleet::Invalid { error } => {
            eprintln!("ccid {runner}: the release channel carries no fleet data ({error})");
            None
        }
        JobFleet::Provided | JobFleet::Absent => None,
    }
}

/// `ccid fleet` subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum Action {
    /// Print the fleet data CCID_FLEET_FILE names as one JSON line with its digest, its source and
    /// the forge host spellings CCID_FORGE_HOSTS adds.
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
                "{{\"fleet\":{canonical},\"digest\":{},\"source\":{},\"runtime_forge_hosts\":{}}}",
                serde_json::Value::from(fleet.digest()?),
                serde_json::Value::from(source.to_string_lossy()),
                serde_json::Value::from(fleet.runtime_hosts.clone())
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

//! Resolver job-runtime pre-step: invoke `cfrg resolve` and install its config.
//!
//! Consumer of RESOLVER-API.md v1 (owned by the clmr worker; this crate never
//! reimplements selection, placement, rendering, or credential-helper logic).
//! Flow:
//!
//! 1. Read runner config path from `CFRG_RESOLVER_CONFIG`. Absent -> resolve is
//!    never invoked; existing operation preserved.
//! 2. Scan the already-unpacked verified source (`Cargo.toml` + `Cargo.lock`)
//!    plus adapter-staged `source_urls` into a deterministic declared
//!    inventory, grouped by canonical path with merged source-URL forms.
//! 3. Derive EACH repo's primary individually through the authoritative cfrg
//!    placement Policy (`cfrg plan --policy <file> --repository <id>`);
//!    missing placement repo -> primary null. Invalid policy fails closed.
//! 4. Always invoke `cfrg resolve` when configured (even with zero stores;
//!    its library performs no probes then). A missing `cfrg` binary with
//!    active config is a clear fatal error, never a silent fallback.
//! 5. Validate the response (completeness, shape, no secret values) and
//!    install its `instead_of` pairs plus credential entries into the Runner
//!    child env.
//!
//! Git `insteadOf` is a RAW byte prefix: a `.git` pair also matches
//! `<repo>.git-evil`. Only declared `source_urls` forms are ever rewritten;
//! undeclared URLs are not safely routable (a matching undeclared URL may
//! select an existing secondary; self-rewrites and longer keys protect the
//! DECLARED inventory only). Git config is NOT a sandbox:
//! system/global config still applies.
//!
//! Failures (invalid config/policy, ambiguous refs, conflicting routes,
//! resolve errors) fail the job closed. Pointer outcomes are normal: the fetch
//! proceeds via canonical. Probe bodies and secrets are never printed: the
//! child stderr is discarded and errors carry status codes only.
use crate::{failure, Environment, Result};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

/// Env var holding the runner config FILE PATH (JSON, Infra-provided).
pub const CONFIG_ENV: &str = "CFRG_RESOLVER_CONFIG";
/// Env var optionally holding an explicit `cfrg` binary path.
pub const CFRG_BIN_ENV: &str = "CFRG_BIN";
/// Idempotency marker: set once config is installed so nested jobs->ccid
/// execution never applies a second layer.
pub const APPLIED_ENV: &str = "CFRG_RESOLVER_APPLIED";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunnerConfig {
    pub schema: u32,
    pub canonical_base: String,
    #[serde(default)]
    pub aliases: Vec<Alias>,
    #[serde(default)]
    pub stores: Vec<Store>,
    #[serde(default)]
    pub placement: Option<PlacementCfg>,
    #[serde(default)]
    pub tool: Option<ToolPin>,
    #[serde(default)]
    pub primary_source: Option<PrimarySource>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

/// Pinned resolver tool: the exact binary the pre-step locates AND verifies.
/// `binary` is the absolute worker path (existing cache/workspaces mount);
/// `source_revision`/`binary_sha256` are the expected identities. Absent =
/// explicit generic unpinned mode (child-PATH lookup, no verification;
/// suitable for development only — Infra must pin).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ToolPin {
    pub binary: String,
    pub source_revision: String,
    pub binary_sha256: String,
}

/// Authoritative placement source. `policy` is the validated cfrg Policy
/// file; `ids` maps canonical repo paths to Policy repository IDs when they
/// differ (default: the canonical path itself is the Policy ID).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlacementCfg {
    pub policy: String,
    #[serde(default)]
    pub ids: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    pub url_prefix: String,
    pub canonical_owner: String,
}

/// Store declaration; serialized into the request exactly as RESOLVER-API.md
/// `stores[]`. `credential_env` names a per-job env var; values never appear.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Store {
    pub kind: String,
    pub location: String,
    pub identity: String,
    pub scope: Vec<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub credential_env: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub trusted_single_user: bool,
}

/// Declared live primary feed, passed through to `cfrg resolve` verbatim
/// (same JSON shape). The CLI reads it with a bounded HEAD per moving repo
/// with unknown primary; ccid itself performs no lookup. Absent = no feed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrimarySource {
    pub pointer_base: String,
    pub identities: BTreeMap<String, String>,
    #[serde(default = "default_lookup_timeout")]
    pub timeout_secs: u64,
}

fn default_lookup_timeout() -> u64 {
    10
}

/// One declared repo: canonical path, exactly one ref kind, and the full set
/// of declared fetch-URL forms. Same path + same ref across aliases merge;
/// differing refs for one path fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredRepo {
    pub path: String,
    pub owner: String,
    pub pinned: Option<String>,
    pub moving: Option<String>,
    pub sources: BTreeSet<String>,
}

/// Scanned git source: (base URL, pinned hash, moving ref).
type ScannedGit = (String, Option<String>, Option<String>);
/// Inventory entry under construction: (owner, pinned, moving, source forms).
type PendingRepo = (String, Option<String>, Option<String>, BTreeSet<String>);

// ---------------------------------------------------------------------------
// Validation (mirrors RESOLVER-API.md + SECURITY-REVIEW; fail closed)
// ---------------------------------------------------------------------------

fn is_hex_hash(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn valid_identity(value: &str) -> bool {
    let bytes = value.as_bytes();
    !value.is_empty()
        && value.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

fn valid_cred_env(value: &str) -> bool {
    value.strip_prefix("CFRG_RESOLVER_").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    })
}

/// Alphanumeric-start plain ASCII: safe unquoted in git-config and `!shell`.
fn valid_username(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn valid_scope_entry(value: &str) -> bool {
    if value.is_empty() || value.len() > 256 {
        return false;
    }
    value.split('/').all(valid_slug)
}

fn bad_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let Some(pair) = bytes.get(i + 1..i + 3) else {
                return true;
            };
            if !pair.iter().all(|b| b.is_ascii_hexdigit()) {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

fn clean_url_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 2048
        && !value.bytes().any(|b| {
            b.is_ascii_control()
                || b" \t\"'`<>{}|\\^$;&()!*?".contains(&b)
                || b == b'['
                || b == b']'
        })
        && !bad_percent_encoding(value)
}

/// Canonical port spelling: nonzero u16, no leading zeros.
fn parse_port(port: &str) -> Option<u16> {
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u16 = port.parse().ok()?;
    if value == 0 || value.to_string() != port {
        return None;
    }
    Some(value)
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
}

/// Bare origin `scheme://host[:port]`, no path/query/fragment/userinfo.
fn valid_bare_origin(value: &str, https_only: bool) -> bool {
    if !clean_url_text(value) {
        return false;
    }
    let (scheme, rest) = match value
        .strip_prefix("https://")
        .map(|r| ("https", r))
        .or_else(|| value.strip_prefix("http://").map(|r| ("http", r)))
    {
        Some(pair) => pair,
        None => return false,
    };
    if https_only && scheme != "https" {
        return false;
    }
    if rest.contains('/') || rest.contains('@') {
        return false;
    }
    let (host, port) = match rest.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (rest, None),
    };
    if !valid_host(host) {
        return false;
    }
    if let Some(port) = port {
        if parse_port(port).is_none() {
            return false;
        }
    }
    true
}

fn valid_canonical_base(value: &str) -> bool {
    valid_bare_origin(value, true) && !value.ends_with('/')
}

fn valid_alias_prefix(value: &str) -> bool {
    (value.starts_with("https://") || value.starts_with("http://"))
        && !value.ends_with('/')
        && !value.contains(['@', '?', '#', ' ', '\t', '\n', '\0', '\\'])
        && !bad_percent_encoding(value)
}

fn valid_repo_url(value: &str) -> bool {
    if !clean_url_text(value) {
        return false;
    }
    let rest = match value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    {
        Some(rest) => rest,
        None => return false,
    };
    let host = rest.split('/').next().unwrap_or_default();
    if host.contains('@') {
        return false;
    }
    let (host, port) = match host.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (host, None),
    };
    if !valid_host(host) {
        return false;
    }
    if let Some(port) = port {
        if parse_port(port).is_none() {
            return false;
        }
    }
    !value.contains(['?', '#'])
}

fn valid_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 256 || path.ends_with(".git") {
        return false;
    }
    let mut parts = path.split('/');
    match parts.next() {
        Some(first) if valid_slug(first) => (),
        _ => return false,
    }
    let mut count = 1;
    for part in parts {
        count += 1;
        if !valid_slug(part) || part == "." || part == ".." {
            return false;
        }
    }
    count >= 2
}

fn valid_ref_name(value: &str) -> bool {
    if value.is_empty() || value.len() > 256 {
        return false;
    }
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b) || b == b'+')
        && !value.contains("..")
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.ends_with(".lock")
        && !value.bytes().any(|b| b" ~^:?*[]\\".contains(&b))
}

/// Absolute binary path safe for `!shell` interpolation (no quoting needed).
fn valid_abs_bin(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && path.starts_with('/')
        && !path.contains([
            ' ', '\t', '\n', '\0', '"', '\'', '\\', '$', ';', '&', '|', '(', ')', '`', '*', '?',
            '#', '~',
        ])
}

fn validate_store(store: &Store) -> Result<()> {
    if !valid_identity(&store.identity) {
        return Err(failure("Invalid store identity"));
    }
    if store.scope.is_empty() || store.scope.len() > 256 {
        return Err(failure("Store needs an explicit nonempty scope allowlist"));
    }
    for entry in &store.scope {
        if !valid_scope_entry(entry) {
            return Err(failure("Invalid store scope entry"));
        }
    }
    match store.kind.as_str() {
        "http-forge" => {
            if !valid_bare_origin(&store.location, false) {
                return Err(failure("http-forge location must be a bare origin"));
            }
            let provider_ok = matches!(&store.provider, Some(p) if !p.is_empty()
                && p.len() <= 64
                && p.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b"_-".contains(&b)));
            if !provider_ok {
                return Err(failure("HTTP store provider must name a probe kind"));
            }
            match &store.credential_env {
                None => (), // Public stores may probe anonymously, matching clmr.
                Some(env) if valid_cred_env(env) => (),
                _ => {
                    return Err(failure(
                        "HTTP store credential reference must use CFRG_RESOLVER_*",
                    ))
                }
            }
            if let Some(user) = &store.username {
                if !valid_username(user) {
                    return Err(failure("Invalid store username"));
                }
            }
            if store.trusted_single_user {
                return Err(failure("trusted_single_user is filesystem-only"));
            }
        }
        "filesystem" => {
            if !store.location.starts_with('/')
                || store.location.len() > 2048
                || store
                    .location
                    .bytes()
                    .any(|b| b.is_ascii_control() || b" \t\n\"'`<>{}|\\$;&()!*?[]#%".contains(&b))
                || store.location.split('/').any(|s| s == "..")
            {
                return Err(failure("Invalid filesystem store root"));
            }
            if store.provider.is_some()
                || store.credential_env.is_some()
                || store.username.is_some()
            {
                return Err(failure("Filesystem store takes no provider credentials"));
            }
            if !store.trusted_single_user {
                return Err(failure("Filesystem store requires trusted_single_user"));
            }
        }
        _ => return Err(failure("Unsupported store kind")),
    }
    Ok(())
}

pub fn validate_runner_config(config: &RunnerConfig) -> Result<()> {
    if config.schema != 1 {
        return Err(failure("Unsupported runner config schema"));
    }
    if !valid_canonical_base(&config.canonical_base) {
        return Err(failure("Invalid canonical_base"));
    }
    for alias in &config.aliases {
        if !valid_alias_prefix(&alias.url_prefix) {
            return Err(failure("Invalid alias url_prefix"));
        }
        if !valid_slug(&alias.canonical_owner) {
            return Err(failure("Invalid alias canonical_owner"));
        }
    }
    if config.stores.len() > 64 {
        return Err(failure("Too many stores"));
    }
    for store in &config.stores {
        validate_store(store)?;
    }
    if let Some(placement) = &config.placement {
        if placement.policy.is_empty()
            || placement.policy.len() > 4096
            || placement.policy.contains('\0')
            || !placement.policy.starts_with('/')
        {
            return Err(failure("Invalid placement policy path"));
        }
        for (path, id) in &placement.ids {
            if !valid_path(path) || id.is_empty() || id.len() > 256 || id.contains('\0') {
                return Err(failure("Invalid placement id mapping"));
            }
        }
    }
    if let Some(tool) = &config.tool {
        if !valid_abs_bin(&tool.binary) {
            return Err(failure("Invalid tool binary path"));
        }
        if tool.source_revision.len() != 40
            || !tool
                .source_revision
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(failure("Invalid tool source revision"));
        }
        if tool.binary_sha256.len() != 64
            || !tool
                .binary_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(failure("Invalid tool binary digest"));
        }
    }
    if let Some(source) = &config.primary_source {
        validate_primary_source(source)?;
    }
    if config.timeout_secs == 0 || config.timeout_secs > 300 {
        return Err(failure("Invalid timeout_secs"));
    }
    Ok(())
}

/// Declared live primary feed validation (mirrors the resolver contract:
/// bare-origin pointer base, exact bare-origin identity keys with opaque
/// identity values, bounded timeout).
fn validate_primary_source(source: &PrimarySource) -> Result<()> {
    if !valid_bare_origin(&source.pointer_base, false) {
        return Err(failure("Primary source must be a bare origin"));
    }
    if source.pointer_base.ends_with('/') {
        return Err(failure("Primary source must not end with a slash"));
    }
    if source.identities.is_empty() || source.identities.len() > 64 {
        return Err(failure(
            "Primary source needs an explicit nonempty identity map",
        ));
    }
    for (base, identity) in &source.identities {
        if !valid_bare_origin(base, false) || base.ends_with('/') {
            return Err(failure("Identity base must be a bare origin"));
        }
        if !valid_identity(identity) {
            return Err(failure("Identity must be an opaque validated ID"));
        }
    }
    if source.timeout_secs == 0 || source.timeout_secs > 60 {
        return Err(failure("Primary source timeout must be 1..=60"));
    }
    Ok(())
}

pub fn load_runner_config(path: &Path) -> Result<RunnerConfig> {
    let bytes = std::fs::read(path)?;
    if bytes.len() > 1024 * 1024 {
        return Err(failure("Runner config too large"));
    }
    let config: RunnerConfig = serde_json::from_slice(&bytes)?;
    validate_runner_config(&config)?;
    Ok(config)
}

// ---------------------------------------------------------------------------
// URL normalization: canonical_base + explicit aliases, segment boundaries
// ---------------------------------------------------------------------------

fn strip_suffixes(mut rest: &str) -> &str {
    rest = rest.trim_matches('/');
    if let Some(stripped) = rest.strip_suffix(".git") {
        rest = stripped;
    }
    rest.trim_matches('/')
}

/// True when a declared fetch URL normalizes back to the expected repo path.
fn normalizes_to(url: &str, path: &str, config: &RunnerConfig) -> bool {
    normalize_url(url, config)
        .as_ref()
        .is_some_and(|(_, p)| p == path)
}

/// Map a declared fetch URL to `(owner, path)`. `None` for non-owned URLs
/// (left untouched, never probed, never rewritten).
pub fn normalize_url(url: &str, config: &RunnerConfig) -> Option<(String, String)> {
    if !clean_url_text(url) || url.contains('@') {
        return None;
    }
    let base = config.canonical_base.trim_end_matches('/');
    if let Some(rest) = url.strip_prefix(base).filter(|r| r.starts_with('/')) {
        let path = strip_suffixes(rest);
        if valid_path(path) {
            let owner = path.split('/').next().unwrap_or("").to_owned();
            return Some((owner, path.to_owned()));
        }
        return None;
    }
    for alias in &config.aliases {
        if let Some(rest) = url
            .strip_prefix(alias.url_prefix.as_str())
            .filter(|r| r.starts_with('/'))
        {
            let sub = strip_suffixes(rest);
            if sub.is_empty() || !sub.split('/').all(valid_slug) {
                return None;
            }
            let path = format!("{}/{}", alias.canonical_owner, sub);
            if valid_path(&path) {
                return Some((alias.canonical_owner.clone(), path));
            }
            return None;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Inventory: Cargo.toml + Cargo.lock from the unpacked verified source
// ---------------------------------------------------------------------------

/// Split `git+https://host/path?query#fragment` into base + query + fragment.
pub fn split_git_url(url: &str) -> Result<(String, Option<String>, Option<String>)> {
    if url.is_empty() || url.len() > 2048 || url.contains('\0') {
        return Err(failure("Invalid Git URL"));
    }
    let rest = url.strip_prefix("git+").unwrap_or(url);
    if !(rest.starts_with("https://") || rest.starts_with("http://")) {
        return Err(failure("Git URL must be http(s)"));
    }
    if rest.contains('@') {
        return Err(failure("Git URL must not carry credentials"));
    }
    let fragment = rest.rsplit_once('#').map(|(_, f)| f.to_owned());
    let no_frag = rest.split('#').next().unwrap_or(rest);
    let query = no_frag.rsplit_once('?').map(|(_, q)| q.to_owned());
    let base = no_frag.split('?').next().unwrap_or(no_frag).to_owned();
    Ok((base, query, fragment))
}

/// Lock `?query` -> moving ref. `rev=<hash>` is pinned-by-hash (no extra ref);
/// `rev=<name>` is ambiguous -> error. Returns the moving ref, if any.
fn lock_query_moving(query: Option<&str>) -> Result<Option<String>> {
    let Some(query) = query else {
        return Ok(None);
    };
    let mut moving: Option<String> = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "branch" if !v.is_empty() => moving = Some(format!("refs/heads/{v}")),
            "tag" if !v.is_empty() => moving = Some(format!("refs/tags/{v}")),
            "rev" if is_hex_hash(v) => {
                // Pinned-by-hash: the fragment carries the ref, nothing more.
            }
            "rev" if !v.is_empty() => {
                return Err(failure("Ambiguous lock rev is not a hash"));
            }
            _ => return Err(failure("Unsupported lock query")),
        }
    }
    Ok(moving)
}

fn join_key(url: &str) -> String {
    let base = url.strip_prefix("git+").unwrap_or(url);
    let base = base.split(['?', '#']).next().unwrap_or(base);
    base.trim_end_matches('/').to_owned()
}

fn collect_dep_table(
    deps: &toml::map::Map<String, toml::Value>,
    out: &mut BTreeMap<String, (Option<String>, Option<String>)>,
) -> Result<()> {
    for (_, spec) in deps {
        let Some(table) = spec.as_table() else {
            continue;
        };
        let Some(git) = table.get("git").and_then(|v| v.as_str()) else {
            continue;
        };
        let branch = table.get("branch").and_then(|v| v.as_str());
        let tag = table.get("tag").and_then(|v| v.as_str());
        let rev = table.get("rev").and_then(|v| v.as_str());
        let declared = match (branch, tag, rev) {
            (Some(b), None, None) => (None, Some(format!("refs/heads/{b}"))),
            (None, Some(t), None) => (None, Some(format!("refs/tags/{t}"))),
            (None, None, Some(r)) if is_hex_hash(r) => (Some(r.to_owned()), None),
            // rev naming a branch/tag records no ref here: the lock join
            // resolves it (branch query -> moving, hash-only -> pinned).
            (None, None, Some(_)) => continue,
            (None, None, None) => (None, None),
            // Two ref kinds for one dependency is a genuine conflict.
            _ => return Err(failure("Conflicting manifest refs for same URL")),
        };
        let key = join_key(git);
        match out.get(&key) {
            Some(prev) if prev != &declared => {
                return Err(failure("Conflicting manifest refs for same URL"));
            }
            _ => {
                out.insert(key, declared);
            }
        }
    }
    Ok(())
}

fn manifest_git_deps(
    doc: &toml::Value,
    out: &mut BTreeMap<String, (Option<String>, Option<String>)>,
) -> Result<()> {
    let tables = ["dependencies", "dev-dependencies", "build-dependencies"];
    let Some(root) = doc.as_table() else {
        return Ok(());
    };
    for name in tables {
        if let Some(deps) = root.get(name).and_then(|v| v.as_table()) {
            collect_dep_table(deps, out)?;
        }
    }
    // Workspace-level dependency table: same shape, same rules.
    if let Some(deps) = root
        .get("workspace")
        .and_then(|v| v.as_table())
        .and_then(|w| w.get("dependencies"))
        .and_then(|v| v.as_table())
    {
        collect_dep_table(deps, out)?;
    }
    if let Some(targets) = root.get("target").and_then(|v| v.as_table()) {
        for (_, cfg) in targets {
            if let Some(cfg) = cfg.as_table() {
                for name in tables {
                    if let Some(deps) = cfg.get(name).and_then(|v| v.as_table()) {
                        collect_dep_table(deps, out)?;
                    }
                }
            }
        }
    }
    if let Some(patch) = root.get("patch").and_then(|v| v.as_table()) {
        for (_, sources) in patch {
            if let Some(deps) = sources.as_table() {
                collect_dep_table(deps, out)?;
            }
        }
    }
    Ok(())
}

/// Scan one workspace member manifest (literal path only, one level).
/// Glob patterns, escapes, and unreadable members fail closed: silently
/// skipping a member would silently narrow the inventory.
fn scan_member_manifest(
    root: &Path,
    member: &str,
    out: &mut BTreeMap<String, (Option<String>, Option<String>)>,
) -> Result<()> {
    if member.is_empty()
        || member.starts_with('/')
        || member.contains(['*', '?', '[', ']', '{', '}', '!', '\\'])
        || member
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return Err(failure("Unsupported workspace member pattern"));
    }
    let path = root.join(member).join("Cargo.toml");
    if !path.is_file() {
        return Err(failure("Workspace member manifest missing"));
    }
    let text = std::fs::read_to_string(&path)?;
    if text.len() > 1024 * 1024 {
        return Err(failure("Workspace member manifest too large"));
    }
    let doc: toml::Value =
        toml::from_str(&text).map_err(|_| failure("Invalid workspace member manifest"))?;
    manifest_git_deps(&doc, out)?;
    Ok(())
}

fn workspace_members(doc: &toml::Value) -> Vec<String> {
    doc.get("workspace")
        .and_then(|v| v.as_table())
        .and_then(|w| w.get("members"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Scan `root/Cargo.toml` (+ `[workspace.dependencies]` + literal workspace
/// member manifests) and `root/Cargo.lock`. Manifest declarations win; lock
/// branch/tag metadata takes precedence over a lock-resolved hash for deps
/// without a manifest-declared ref (transitive / member-only / bare).
/// Present-but-unreadable files fail closed; only absent files are skipped.
fn scan_refs(root: &Path) -> Result<BTreeMap<String, ScannedGit>> {
    let mut manifest: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    let manifest_path = root.join("Cargo.toml");
    if manifest_path.is_file() {
        let text = std::fs::read_to_string(&manifest_path)?;
        if text.len() > 1024 * 1024 {
            return Err(failure("Cargo manifest too large"));
        }
        let doc: toml::Value =
            toml::from_str(&text).map_err(|_| failure("Invalid Cargo manifest"))?;
        manifest_git_deps(&doc, &mut manifest)?;
        for member in workspace_members(&doc) {
            scan_member_manifest(root, &member, &mut manifest)?;
        }
    }
    // key -> (base url, pinned, moving)
    let mut joined: BTreeMap<String, ScannedGit> = BTreeMap::new();
    let lock_path = root.join("Cargo.lock");
    if lock_path.is_file() {
        let text = std::fs::read_to_string(&lock_path)?;
        if text.len() > 16 * 1024 * 1024 {
            return Err(failure("Cargo lockfile too large"));
        }
        let doc: toml::Value =
            toml::from_str(&text).map_err(|_| failure("Invalid Cargo lockfile"))?;
        if let Some(pkgs) = doc.get("package").and_then(|v| v.as_array()) {
            for pkg in pkgs {
                let source = pkg.get("source").and_then(|v| v.as_str()).unwrap_or("");
                if !source.starts_with("git+") {
                    continue;
                }
                let (base, query, frag) = split_git_url(source)?;
                let hash = match frag {
                    Some(h) if is_hex_hash(&h) => h,
                    _ => return Err(failure("Invalid lock hash")),
                };
                let moving = lock_query_moving(query.as_deref())?;
                let key = join_key(&base);
                // Lock branch/tag metadata wins over the bare hash for deps
                // without a manifest-declared ref; a declared ref wins outright.
                let (pinned, moving) = match manifest.get(&key) {
                    Some((mp, mm)) if mp.is_some() || mm.is_some() => (mp.clone(), mm.clone()),
                    _ if moving.is_some() => (None, moving),
                    _ => (Some(hash), None),
                };
                if pinned.is_some() && moving.is_some() {
                    return Err(failure("Ambiguous ref: both pinned and moving"));
                }
                match joined.get(&key) {
                    Some(prev) if prev != &(base.clone(), pinned.clone(), moving.clone()) => {
                        return Err(failure("Conflicting lock entries for same URL"));
                    }
                    _ => {
                        joined.insert(key, (base, pinned, moving));
                    }
                }
            }
        }
    }
    for (key, (mp, mm)) in &manifest {
        if joined.contains_key(key) {
            continue;
        }
        // Manifest-only entries keep their declared ref (possibly neither:
        // the inventory join below fails owned bare URLs closed).
        joined.insert(key.clone(), (key.clone(), mp.clone(), mm.clone()));
    }
    Ok(joined)
}

/// Parse a manual explicit URL (`url`, `url#<sha>`, `url?branch=X`,
/// `url?tag=X`) into (base, pinned, moving). Ref-less URLs fail: the request
/// needs exactly one ref kind per repo.
pub fn parse_manual_url(url: &str) -> Result<(String, Option<String>, Option<String>)> {
    let (base, query, frag) = split_git_url(url)?;
    let pinned = match frag {
        Some(h) if is_hex_hash(&h) => Some(h),
        Some(_) => return Err(failure("Invalid manual pinned hash")),
        None => None,
    };
    let moving = lock_query_moving(query.as_deref())?;
    if pinned.is_some() && moving.is_some() {
        return Err(failure("Ambiguous manual ref"));
    }
    if pinned.is_none() && moving.is_none() {
        return Err(failure("Manual URL needs #<sha> or ?branch|tag="));
    }
    Ok((base, pinned, moving))
}

/// Declared fetch forms for one repo: canonical bare + `.git`, plus alias
/// bare + `.git` for every alias covering the owner. Only forms normalizing
/// back to the path are kept (never guessed).
fn declared_forms(url: &str, owner: &str, path: &str, config: &RunnerConfig) -> BTreeSet<String> {
    let mut forms = BTreeSet::new();
    let base = config.canonical_base.trim_end_matches('/');
    for candidate in [format!("{base}/{path}"), format!("{base}/{path}.git")] {
        if normalizes_to(&candidate, path, config) {
            forms.insert(candidate);
        }
    }
    if normalizes_to(url, path, config) {
        forms.insert(url.to_owned());
    }
    for alias in &config.aliases {
        if alias.canonical_owner != owner {
            continue;
        }
        let sub = match path.split_once('/') {
            Some((_, rest)) => rest,
            None => continue,
        };
        for candidate in [
            format!("{}/{sub}", alias.url_prefix),
            format!("{}/{sub}.git", alias.url_prefix),
        ] {
            if normalizes_to(&candidate, path, config) {
                forms.insert(candidate);
            }
        }
    }
    forms
}

/// Build the deterministic complete inventory grouped by canonical path.
/// Same path + same ref across URL forms merges sources; differing refs for
/// one path fail closed. Non-owned URLs are ignored (never requested).
pub fn build_inventory(
    root: &Path,
    manual_urls: &[String],
    config: &RunnerConfig,
) -> Result<Vec<DeclaredRepo>> {
    // path -> (owner, pinned, moving, sources)
    let mut by_path: BTreeMap<String, PendingRepo> = BTreeMap::new();
    let mut insert = |url: String, pinned: Option<String>, moving: Option<String>| -> Result<()> {
        let Some((owner, path)) = normalize_url(&url, config) else {
            return Ok(());
        };
        if pinned.is_some() && moving.is_some() {
            return Err(failure("Ambiguous ref: both pinned and moving"));
        }
        if pinned.is_none() && moving.is_none() {
            // Owned manifest-declared URL with no resolvable ref (bare dep,
            // no lock coverage): fail closed instead of silently leaving it
            // out of the declared inventory. (Non-owned URLs return above.)
            return Err(failure(
                "Unresolvable ref for owned dependency: declare branch/tag/rev or provide Cargo.lock",
            ));
        }
        match by_path.get_mut(&path) {
            Some(entry) => {
                if entry.1 != pinned || entry.2 != moving {
                    return Err(failure("Conflicting refs for same path"));
                }
                for form in declared_forms(&url, &owner, &path, config) {
                    entry.3.insert(form);
                }
            }
            None => {
                by_path.insert(
                    path.clone(),
                    (
                        owner.clone(),
                        pinned,
                        moving,
                        declared_forms(&url, &owner, &path, config),
                    ),
                );
            }
        }
        Ok(())
    };
    for (url, pinned, moving) in scan_refs(root)?.into_values() {
        insert(url, pinned, moving)?;
    }
    for raw in manual_urls {
        let (base, pinned, moving) = parse_manual_url(raw)?;
        insert(base, pinned, moving)?;
    }
    Ok(by_path
        .into_iter()
        .map(|(path, (owner, pinned, moving, sources))| DeclaredRepo {
            path,
            owner,
            pinned,
            moving,
            sources,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Request building (RESOLVER-API.md §Request, incl. source_urls)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ApiRequest<'a> {
    schema: u32,
    canonical_base: &'a str,
    aliases: &'a [Alias],
    repositories: Vec<ApiRepo<'a>>,
    stores: &'a [Store],
    timeout_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_source: Option<&'a PrimarySource>,
}

#[derive(Debug, Serialize)]
struct ApiRepo<'a> {
    id: &'a str,
    path: &'a str,
    #[serde(rename = "ref")]
    reference: ApiRef<'a>,
    primary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_url: Option<String>,
    source_urls: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum ApiRef<'a> {
    Pinned(&'a str),
    Moving(&'a str),
}

pub struct ResolvedPrimary {
    pub primary: Option<String>,
    pub primary_url: Option<String>,
}

pub fn build_request_json(
    inventory: &[DeclaredRepo],
    primaries: &BTreeMap<String, ResolvedPrimary>,
    config: &RunnerConfig,
) -> Result<serde_json::Value> {
    if inventory.len() > 1024 {
        return Err(failure("Too many repositories"));
    }
    if inventory.is_empty() {
        return Err(failure("Request needs at least one repository"));
    }
    let mut repositories = Vec::with_capacity(inventory.len());
    for dep in inventory {
        if !valid_path(&dep.path) {
            return Err(failure("Invalid repository path"));
        }
        let Some(resolved) = primaries.get(&dep.path) else {
            return Err(failure("Missing primary lookup"));
        };
        if let Some(primary) = &resolved.primary {
            if !valid_identity(primary) {
                return Err(failure("Invalid primary identity"));
            }
        }
        if let Some(url) = &resolved.primary_url {
            if !valid_repo_url(url) {
                return Err(failure("Invalid primary_url"));
            }
        }
        let reference = match (&dep.pinned, &dep.moving) {
            (Some(sha), None) => ApiRef::Pinned(sha),
            (None, Some(m)) => {
                if !m.starts_with("refs/") || !valid_ref_name(m) {
                    return Err(failure("Invalid moving ref"));
                }
                ApiRef::Moving(m)
            }
            _ => return Err(failure("Dep needs exactly one ref kind")),
        };
        if dep.sources.is_empty() || dep.sources.len() > 64 {
            return Err(failure("Repository needs declared source_urls"));
        }
        repositories.push(ApiRepo {
            id: &dep.path,
            path: &dep.path,
            reference,
            primary: resolved.primary.clone(),
            primary_url: resolved.primary_url.clone(),
            source_urls: dep.sources.iter().cloned().collect(),
        });
    }
    let request = ApiRequest {
        schema: 1,
        canonical_base: &config.canonical_base,
        aliases: &config.aliases,
        repositories,
        stores: &config.stores,
        timeout_secs: config.timeout_secs,
        primary_source: config.primary_source.as_ref(),
    };
    serde_json::to_value(&request).map_err(|_| failure("Request serialization"))
}

// ---------------------------------------------------------------------------
// Bounded cfrg subprocesses (Runner; stderr discarded, status-only errors)
// ---------------------------------------------------------------------------

fn child_path_search(name: &str, environment: &Environment) -> Option<String> {
    if name.contains(['/', '\0']) {
        return None;
    }
    let path = environment.get(&OsString::from("PATH"))?;
    let path = path.to_string_lossy();
    for dir in path.split(':') {
        if dir.is_empty() || !dir.starts_with('/') {
            continue;
        }
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// Resolve the `cfrg` binary: explicit `CFRG_BIN` (absolute path, validated)
/// wins; otherwise search the CHILD environment's PATH. Never the parent's.
fn find_cfrg(environment: &Environment) -> Result<String> {
    if let Some(explicit) = environment
        .get(&OsString::from(CFRG_BIN_ENV))
        .map(|v| v.to_string_lossy().into_owned())
        .filter(|v| !v.is_empty())
    {
        if explicit.contains('\0') {
            return Err(failure("Invalid CFRG_BIN"));
        }
        if explicit.starts_with('/') {
            if !valid_abs_bin(&explicit) {
                return Err(failure("Invalid CFRG_BIN"));
            }
            return Ok(explicit);
        }
        if explicit.contains('/') {
            return Err(failure("Invalid CFRG_BIN"));
        }
        return child_path_search(&explicit, environment)
            .ok_or_else(|| failure("cfrg resolve unavailable: binary not on job PATH"));
    }
    child_path_search("cfrg", environment)
        .ok_or_else(|| failure("cfrg resolve unavailable: binary not on job PATH"))
}

/// Verify the located binary against the configured pin: exact path, sha256
/// digest (existing `sha2` dependency), and embedded source revision via the
/// tool's own `source-revision` subcommand. Any mismatch fails closed.
fn verify_tool_pin(
    root: &Path,
    environment: &Environment,
    binary: &str,
    pin: &ToolPin,
    bound_secs: u64,
) -> Result<()> {
    if binary != pin.binary {
        return Err(failure("cfrg binary does not match pinned path"));
    }
    let bytes = std::fs::read(binary)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(failure("cfrg binary too large"));
    }
    let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
    if digest != pin.binary_sha256 {
        return Err(failure("cfrg binary digest mismatch"));
    }
    let argv = vec![binary.to_owned(), "source-revision".to_owned()];
    let revision = run_cfrg(root, environment, &argv, bound_secs)?;
    if revision.trim() != pin.source_revision {
        return Err(failure("cfrg source revision mismatch"));
    }
    Ok(())
}

/// Run one bounded `cfrg` invocation; capture stdout (16 MiB cap inside
/// Runner), discard stderr (may carry credential-bearing bodies).
fn run_cfrg(
    root: &Path,
    environment: &Environment,
    argv: &[String],
    bound_secs: u64,
) -> Result<String> {
    let runner = crate::Runner::until(
        root.to_path_buf(),
        environment.clone(),
        Instant::now() + Duration::from_secs(bound_secs.max(1)),
    )?
    .without_child_stderr();
    let stdout = runner
        .run(argv, true)
        .map_err(|_| failure("cfrg invocation failed"))?;
    if stdout.len() > 16 * 1024 * 1024 {
        return Err(failure("cfrg output too large"));
    }
    Ok(stdout)
}

/// Parse `cfrg plan` output into (primary_forge, primary clone URL).
/// Shape-owned by cfrg; unknown shapes fail closed (never guessed).
pub fn parse_plan_output(text: &str) -> Result<(String, Option<String>)> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| failure("Invalid cfrg plan output"))?;
    let primary = value
        .get("primary_forge")
        .and_then(|v| v.as_str())
        .ok_or_else(|| failure("Invalid cfrg plan output"))?;
    if !valid_identity(primary) {
        return Err(failure("Invalid primary identity from plan"));
    }
    let clone0 = value
        .get("clone_urls")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .filter(|url| valid_repo_url(url))
        .map(str::to_owned);
    Ok((primary.to_owned(), clone0))
}

/// Per-repo primary via the authoritative Policy. Missing placement repo ->
/// null; invalid policy (established by `validate_policy`) fails closed.
fn lookup_primary(
    root: &Path,
    environment: &Environment,
    binary: &str,
    policy: &str,
    policy_id: &str,
    bound_secs: u64,
) -> Result<ResolvedPrimary> {
    let argv = vec![
        binary.to_owned(),
        "plan".to_owned(),
        "--policy".to_owned(),
        policy.to_owned(),
        "--repository".to_owned(),
        policy_id.to_owned(),
    ];
    match run_cfrg(root, environment, &argv, bound_secs) {
        Ok(text) => {
            let (primary, primary_url) = parse_plan_output(&text)?;
            Ok(ResolvedPrimary {
                primary: Some(primary),
                primary_url,
            })
        }
        Err(_) => {
            // Policy was validated up front, so a failed plan means the repo
            // is not placed there -> primary null (pointer fallback, the safe
            // direction). Interruption still aborts the job.
            if crate::INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(failure("Check interrupted"));
            }
            Ok(ResolvedPrimary {
                primary: None,
                primary_url: None,
            })
        }
    }
}

fn validate_policy(
    root: &Path,
    environment: &Environment,
    binary: &str,
    policy: &str,
    bound_secs: u64,
) -> Result<()> {
    let argv = vec![
        binary.to_owned(),
        "validate".to_owned(),
        "--policy".to_owned(),
        policy.to_owned(),
    ];
    run_cfrg(root, environment, &argv, bound_secs)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Response handling: validate, then install into the child env
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ApiResponse {
    schema: u32,
    canonical_base: String,
    decisions: Vec<ApiDecision>,
    #[serde(default)]
    credentials: Vec<ApiCredential>,
}

#[derive(Debug, Deserialize)]
struct ApiDecision {
    id: String,
    path: String,
    outcome: String,
    #[serde(default)]
    instead_of: Vec<(String, String)>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiCredential {
    #[serde(rename = "match")]
    match_origin: String,
    env: String,
    #[serde(default)]
    username: Option<String>,
}

fn valid_pair(from: &str, to: &str) -> bool {
    for url in [from, to] {
        if !clean_url_text(url)
            || url.contains('@')
            || !(url.starts_with("https://") || url.starts_with("http://"))
        {
            return false;
        }
    }
    true
}

fn credential_helper(cfrg_bin: &str, c: &ApiCredential) -> Result<String> {
    if !valid_bare_origin(&c.match_origin, false) {
        return Err(failure("Invalid credential origin"));
    }
    if cfrg_bin != "cfrg" && !valid_abs_bin(cfrg_bin) {
        return Err(failure("Invalid credential helper binary"));
    }
    if !valid_cred_env(&c.env) {
        return Err(failure("Invalid credential env"));
    }
    let mut helper = format!(
        "!{cfrg_bin} credential-helper --env {} --expect-origin {}",
        c.env, c.match_origin
    );
    if let Some(user) = &c.username {
        if !valid_username(user) {
            return Err(failure("Invalid credential username"));
        }
        helper.push_str(&format!(" --username {user}"));
    }
    if helper.bytes().any(|b| b.is_ascii_control()) {
        return Err(failure("Invalid credential helper"));
    }
    Ok(helper)
}

/// Assert the response carries no secret VALUES for the credential env names
/// it references (names only are expected). Reads env for comparison only.
fn assert_no_secrets(text: &str, env_names: &[String], env: &Environment) -> Result<()> {
    for name in env_names {
        if let Some(value) = env
            .get(&OsString::from(name))
            .map(|v| v.to_string_lossy().into_owned())
            .filter(|v| !v.is_empty())
        {
            if text.contains(&value) {
                return Err(failure("Resolver response carries secret material"));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn apply_response(
    environment: &mut Environment,
    response_text: &str,
    requested_ids: &BTreeSet<String>,
    cfrg_bin: &str,
    config: &RunnerConfig,
) -> Result<usize> {
    if response_text.len() > 16 * 1024 * 1024 {
        return Err(failure("Resolver response too large"));
    }
    let response: ApiResponse =
        serde_json::from_str(response_text).map_err(|_| failure("Invalid resolver response"))?;
    if response.schema != 1 || response.canonical_base != config.canonical_base {
        return Err(failure("Resolver response mismatch"));
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for d in &response.decisions {
        // I request id == path; both must echo back equal, otherwise the
        // decision cannot be attributed to a requested repository.
        if d.path != d.id {
            return Err(failure("Mismatched resolver decision identity"));
        }
        if !seen.insert(d.id.clone()) {
            return Err(failure("Duplicate resolver decision"));
        }
        if d.outcome != "routed" && d.outcome != "canonical-pointer" {
            return Err(failure("Invalid resolver outcome"));
        }
    }
    if seen != *requested_ids {
        return Err(failure("Incomplete resolver response"));
    }
    let env_names: Vec<String> = response.credentials.iter().map(|c| c.env.clone()).collect();
    assert_no_secrets(response_text, &env_names, environment)?;
    // Transport instead_of pairs; same source -> same destination or fail.
    let mut pairs: BTreeMap<String, String> = BTreeMap::new();
    for d in &response.decisions {
        for (from, to) in &d.instead_of {
            if !valid_pair(from, to) {
                return Err(failure("Invalid insteadOf pair"));
            }
            match pairs.get(from) {
                Some(prev) if prev != to => {
                    return Err(failure("Conflicting routing for same source"));
                }
                _ => {
                    pairs.insert(from.clone(), to.clone());
                }
            }
        }
    }
    // Longest source first: longer declared neighbors override shorter routes.
    let mut ordered: Vec<(String, String)> = pairs.into_iter().collect();
    ordered.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
    // Credential entries: FIRST an empty helper (clears inherited helpers for
    // the matched origin), then the scoped helper. Exact-origin scope.
    let mut keys: Vec<(String, String)> =
        Vec::with_capacity(ordered.len() + 2 * response.credentials.len());
    for (from, to) in &ordered {
        keys.push((format!("url.{to}.insteadOf"), from.clone()));
    }
    for c in &response.credentials {
        keys.push((
            format!("credential.{}.helper", c.match_origin),
            String::new(),
        ));
        keys.push((
            format!("credential.{}.helper", c.match_origin),
            credential_helper(cfrg_bin, c)?,
        ));
    }
    if keys.is_empty() {
        return Ok(0);
    }
    environment.insert(
        OsString::from("GIT_CONFIG_COUNT"),
        OsString::from(keys.len().to_string()),
    );
    for (index, (k, v)) in keys.iter().enumerate() {
        environment.insert(
            OsString::from(format!("GIT_CONFIG_KEY_{index}")),
            OsString::from(k),
        );
        environment.insert(
            OsString::from(format!("GIT_CONFIG_VALUE_{index}")),
            OsString::from(v),
        );
    }
    environment.insert(OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0"));
    Ok(ordered.len())
}

// ---------------------------------------------------------------------------
// Entry point: after unpack, before Runner::new
// ---------------------------------------------------------------------------

/// Fail-closed pre-step. `root` is the already-unpacked verified source;
/// `scratch` holds the per-job request file; `source_urls` are adapter-staged
/// manual explicit URLs. Installs into `environment`, which must be the map
/// later moved into `Runner::new` so commands actually consume it.
pub fn maybe_prepare_runner_env(
    environment: &mut Environment,
    root: &Path,
    scratch: &Path,
    source_urls: &[String],
    budget_secs: u64,
) -> Result<usize> {
    let config_path = environment
        .get(&OsString::from(CONFIG_ENV))
        .map(|v| v.to_string_lossy().into_owned())
        .filter(|v| !v.is_empty());
    let Some(config_path) = config_path else {
        return Ok(0); // No runner config: existing operation preserved.
    };
    if environment.contains_key(&OsString::from(APPLIED_ENV)) {
        return Ok(0); // Nested execution: config already installed above.
    }
    let config = load_runner_config(Path::new(&config_path))?;
    let inventory = build_inventory(root, source_urls, &config)?;
    if inventory.is_empty() {
        return Ok(0); // Nothing declared: canonical fetches proceed untouched.
    }
    // Always invoke cfrg when configured (even with zero stores: no probes).
    // A missing binary with active config fails clearly, never silently.
    let binary = find_cfrg(environment)?;
    let bound = config.timeout_secs.min(budget_secs.max(1)).max(1);
    // Pinned tool: verify exact path, digest, and embedded revision before
    // any resolve call. Unpinned (no `tool` section) is explicit generic
    // mode — Infra must pin; development may not.
    if let Some(pin) = &config.tool {
        verify_tool_pin(root, environment, &binary, pin, bound)?;
    }
    // Per-repo primaries through the authoritative Policy (or all null).
    let mut primaries: BTreeMap<String, ResolvedPrimary> = BTreeMap::new();
    if let Some(placement) = &config.placement {
        validate_policy(root, environment, &binary, &placement.policy, bound)?;
        for dep in &inventory {
            let policy_id = placement
                .ids
                .get(&dep.path)
                .map_or(dep.path.as_str(), String::as_str);
            primaries.insert(
                dep.path.clone(),
                lookup_primary(
                    root,
                    environment,
                    &binary,
                    &placement.policy,
                    policy_id,
                    bound,
                )?,
            );
        }
    } else {
        for dep in &inventory {
            primaries.insert(
                dep.path.clone(),
                ResolvedPrimary {
                    primary: None,
                    primary_url: None,
                },
            );
        }
    }
    let request = build_request_json(&inventory, &primaries, &config)?;
    let request_file = scratch.join("cfrg-resolve-request.json");
    let bytes = serde_json::to_vec(&request).map_err(|_| failure("Request serialization"))?;
    std::fs::write(&request_file, &bytes)?;
    let argv = vec![
        binary.clone(),
        "resolve".to_owned(),
        "--request".to_owned(),
        request_file.to_string_lossy().into_owned(),
        "--emit".to_owned(),
        "json".to_owned(),
    ];
    let stdout = run_cfrg(root, environment, &argv, bound)?;
    let requested_ids: BTreeSet<String> = inventory.iter().map(|d| d.path.clone()).collect();
    let helper_bin = if binary.contains('/') {
        binary.as_str()
    } else {
        "cfrg"
    };
    let pairs = apply_response(environment, &stdout, &requested_ids, helper_bin, &config)?;
    if pairs > 0 {
        environment.insert(OsString::from(APPLIED_ENV), OsString::from("1"));
    }
    Ok(inventory.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn test_config() -> RunnerConfig {
        RunnerConfig {
            schema: 1,
            canonical_base: "https://git.example".into(),
            aliases: vec![Alias {
                url_prefix: "https://github.com/acme".into(),
                canonical_owner: "acme".into(),
            }],
            stores: vec![Store {
                kind: "http-forge".into(),
                location: "http://forgejo.example:3001".into(),
                identity: "forgejo".into(),
                scope: vec!["acme".into()],
                provider: Some("forgejo".into()),
                credential_env: Some("CFRG_RESOLVER_FORGEJO_TOKEN".into()),
                username: Some("oauth2".into()),
                trusted_single_user: false,
            }],
            placement: None,
            tool: None,
            primary_source: None,
            timeout_secs: 30,
        }
    }

    #[test]
    fn runner_config_validation() {
        assert!(validate_runner_config(&test_config()).is_ok());
        let mut public = test_config();
        public.stores[0].credential_env = None;
        assert!(validate_runner_config(&public).is_ok());
        for mutate in [
            |c: &mut RunnerConfig| c.canonical_base = "http://git.example".into(),
            |c: &mut RunnerConfig| c.canonical_base = "https://git.example/".into(),
            |c: &mut RunnerConfig| {
                c.stores[0].location = "http://forgejo.example:3001/prefix".into()
            },
            |c: &mut RunnerConfig| c.stores[0].location = "http://forgejo.example:007".into(),
            |c: &mut RunnerConfig| c.stores[0].credential_env = Some("FORGEJO_TOKEN".into()),
            |c: &mut RunnerConfig| c.stores[0].username = Some("-bad".into()),
            |c: &mut RunnerConfig| c.stores[0].kind = "filesystem".into(),
        ] {
            let mut bad = test_config();
            mutate(&mut bad);
            assert!(validate_runner_config(&bad).is_err());
        }
        let mut fs = test_config();
        fs.stores[0] = Store {
            kind: "filesystem".into(),
            location: "/srv/repos".into(),
            identity: "forgejo".into(),
            scope: vec!["acme/widget".into()],
            provider: None,
            credential_env: None,
            username: None,
            trusted_single_user: true,
        };
        assert!(validate_runner_config(&fs).is_ok());
        fs.stores[0].location = "/srv/re#pos".into();
        assert!(validate_runner_config(&fs).is_err());
    }

    #[test]
    fn alias_segment_boundaries() {
        let config = test_config();
        assert_eq!(
            normalize_url("https://github.com/acme/widget", &config),
            Some(("acme".into(), "acme/widget".into()))
        );
        assert_eq!(
            normalize_url("https://github.com/acme/widget.git", &config),
            Some(("acme".into(), "acme/widget".into()))
        );
        assert_eq!(
            normalize_url("https://github.com/acme-evil/x", &config),
            None
        );
        assert_eq!(
            normalize_url("https://other.example/acme/widget", &config),
            None
        );
        assert_eq!(
            normalize_url("https://git.example/acme/widget", &config),
            Some(("acme".into(), "acme/widget".into()))
        );
        assert_eq!(
            normalize_url("https://user@git.example/acme/widget", &config),
            None
        );
    }

    #[test]
    fn lock_moving_takes_precedence_over_hash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[dependencies]\nwidget = { git = \"https://git.example/acme/widget\", branch = \"main\" }\n",
        )
        .unwrap();
        let sha = "a".repeat(40);
        std::fs::write(
            dir.path().join("Cargo.lock"),
            format!("[[package]]\nname = \"widget\"\nversion = \"0.1.0\"\nsource = \"git+https://git.example/acme/widget?branch=main#{sha}\"\n"),
        )
        .unwrap();
        let refs = scan_refs(dir.path()).unwrap();
        let (_, pinned, moving) = &refs["https://git.example/acme/widget"];
        assert_eq!(*pinned, None);
        assert_eq!(*moving, Some("refs/heads/main".into()));
    }

    #[test]
    fn same_repo_alias_forms_merge_conflicting_refs_fail() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let sha = "b".repeat(40);
        let config = test_config();
        // Same repo via canonical + alias URL, same hash: merged, not conflict.
        let manual = vec![
            format!("https://git.example/acme/widget#{sha}"),
            format!("https://github.com/acme/widget#{sha}"),
        ];
        let inv = build_inventory(dir.path(), &manual, &config).unwrap();
        assert_eq!(inv.len(), 1);
        assert!(inv[0].sources.contains("https://git.example/acme/widget"));
        assert!(inv[0]
            .sources
            .contains("https://git.example/acme/widget.git"));
        assert!(inv[0].sources.contains("https://github.com/acme/widget"));
        // Same path, different hash: fail closed.
        let manual2 = vec![
            format!("https://git.example/acme/widget#{sha}"),
            format!("https://git.example/acme/widget#{}", "c".repeat(40)),
        ];
        assert!(build_inventory(dir.path(), &manual2, &config).is_err());
    }

    #[test]
    fn request_carries_source_urls_and_per_repo_primary() {
        let config = test_config();
        let inv = vec![
            DeclaredRepo {
                path: "acme/widget".into(),
                owner: "acme".into(),
                pinned: Some("c".repeat(40)),
                moving: None,
                sources: BTreeSet::from([
                    "https://git.example/acme/widget".into(),
                    "https://github.com/acme/widget.git".into(),
                ]),
            },
            DeclaredRepo {
                path: "acme/gadget".into(),
                owner: "acme".into(),
                pinned: None,
                moving: Some("refs/heads/main".into()),
                sources: BTreeSet::from(["https://git.example/acme/gadget".into()]),
            },
        ];
        // Same owner, DIFFERENT primaries per repository.
        let primaries = BTreeMap::from([
            (
                "acme/widget".to_owned(),
                ResolvedPrimary {
                    primary: Some("forgejo".into()),
                    primary_url: None,
                },
            ),
            (
                "acme/gadget".to_owned(),
                ResolvedPrimary {
                    primary: None,
                    primary_url: None,
                },
            ),
        ]);
        let value = build_request_json(&inv, &primaries, &config).unwrap();
        assert_eq!(value["schema"], 1);
        assert_eq!(value["repositories"][0]["primary"], "forgejo");
        assert!(value["repositories"][1]["primary"].is_null());
        assert_eq!(
            value["repositories"][0]["source_urls"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(value["stores"][0]["trusted_single_user"], false);
    }

    #[test]
    fn plan_output_parsing_supports_distinct_primaries() {
        // Two repos, SAME owner, different primaries from two plan outputs.
        let (p1, u1) = parse_plan_output(
            r#"{"repository":"w","primary_ci":"c","primary_forge":"forgejo","trigger_forge":"forgejo","clone_urls":["https://git.example/acme/widget"],"execution_fallbacks":[],"promotion":"manual"}"#,
        )
        .unwrap();
        let (p2, u2) = parse_plan_output(
            r#"{"repository":"g","primary_ci":"c2","primary_forge":"hub","trigger_forge":"hub","clone_urls":[],"execution_fallbacks":[],"promotion":"manual"}"#,
        )
        .unwrap();
        assert_eq!((p1.as_str(), p2.as_str()), ("forgejo", "hub"));
        assert_eq!(u1, Some("https://git.example/acme/widget".into()));
        assert_eq!(u2, None);
        assert!(parse_plan_output(r#"{"primary_forge":"BAD NAME"}"#).is_err());
        assert!(parse_plan_output("not json").is_err());
    }

    #[test]
    fn child_path_lookup_uses_child_env_not_parent() {
        let mut env: Environment = BTreeMap::new();
        env.insert(OsString::from("PATH"), OsString::from("/nonexistent-xyz"));
        assert!(find_cfrg(&env).is_err());
        // Explicit absolute CFRG_BIN wins without any PATH search.
        env.insert(OsString::from("CFRG_BIN"), OsString::from("/usr/bin/cfrg"));
        assert_eq!(find_cfrg(&env).unwrap(), "/usr/bin/cfrg");
        env.insert(OsString::from("CFRG_BIN"), OsString::from("rel/path"));
        assert!(find_cfrg(&env).is_err());
    }

    #[test]
    fn credential_helper_uses_tested_binary_and_strict_origin() {
        let c = ApiCredential {
            match_origin: "http://forgejo.example:3001".into(),
            env: "CFRG_RESOLVER_FORGEJO_TOKEN".into(),
            username: Some("oauth2".into()),
        };
        let helper = credential_helper("/opt/cfrg/bin/cfrg", &c).unwrap();
        assert!(helper.starts_with("!/opt/cfrg/bin/cfrg credential-helper "));
        assert!(helper.contains("--expect-origin http://forgejo.example:3001"));
        assert!(credential_helper(
            "cfrg",
            &ApiCredential {
                match_origin: "http://forgejo.example:3001/extra".into(),
                ..c.clone()
            }
        )
        .is_err());
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;
    use crate::resolver::tests::test_config;

    #[test]
    fn request_serialization_matches_api_shapes() {
        let config = test_config();
        let inv = vec![DeclaredRepo {
            path: "acme/widget".into(),
            owner: "acme".into(),
            pinned: Some("e".repeat(40)),
            moving: None,
            sources: BTreeSet::from([
                "https://git.example/acme/widget".into(),
                "https://git.example/acme/widget.git".into(),
                "https://github.com/acme/widget".into(),
                "https://github.com/acme/widget.git".into(),
            ]),
        }];
        let primaries = BTreeMap::from([(
            "acme/widget".to_owned(),
            ResolvedPrimary {
                primary: None,
                primary_url: None,
            },
        )]);
        let value = build_request_json(&inv, &primaries, &config).unwrap();
        let obj = value.as_object().unwrap();
        let obj_keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            obj_keys,
            [
                "aliases",
                "canonical_base",
                "repositories",
                "schema",
                "stores",
                "timeout_secs"
            ]
            .into_iter()
            .collect::<BTreeSet<_>>()
        );
        let repo = &value["repositories"][0];
        let repo_obj = repo.as_object().unwrap();
        // primary_url omitted when absent; primary null is accepted by default.
        assert!(!repo_obj.contains_key("primary_url"));
        assert!(repo["primary"].is_null());
        assert_eq!(repo["source_urls"].as_array().unwrap().len(), 4);
        assert_eq!(repo["ref"]["pinned"], "e".repeat(40));
        let store = &value["stores"][0];
        // bool serialized as false, never null; no extra keys.
        assert_eq!(store["trusted_single_user"], false);
        let store_keys: BTreeSet<&str> = store
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            store_keys,
            [
                "credential_env",
                "identity",
                "kind",
                "location",
                "provider",
                "scope",
                "trusted_single_user",
                "username"
            ]
            .into_iter()
            .collect::<BTreeSet<_>>()
        );
    }
}

#[cfg(test)]
mod inventory_tests {
    use super::*;
    use crate::resolver::tests::test_config;

    fn write_file(dir: &std::path::Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn lock_only_branch_wins_over_hash() {
        // Transitive-style dep: lock `?branch=v01#HASH`, no manifest entry.
        let dir = tempfile::tempdir().unwrap();
        let sha = "a".repeat(40);
        write_file(
            dir.path(),
            "Cargo.lock",
            &format!("[[package]]\nname = \"w\"\nversion = \"0.1.0\"\nsource = \"git+https://git.example/acme/w?branch=v01#{sha}\"\n"),
        );
        let config = test_config();
        let inv = build_inventory(dir.path(), &[], &config).unwrap();
        assert_eq!(inv.len(), 1);
        assert_eq!(inv[0].moving, Some("refs/heads/v01".into()));
        assert_eq!(inv[0].pinned, None);
        // Non-Forgejo fallback request shape: null primary, moving ref kept.
        let primaries = BTreeMap::from([(
            "acme/w".to_owned(),
            ResolvedPrimary {
                primary: None,
                primary_url: None,
            },
        )]);
        let value = build_request_json(&inv, &primaries, &config).unwrap();
        assert!(value["repositories"][0]["primary"].is_null());
        assert_eq!(value["repositories"][0]["ref"]["moving"], "refs/heads/v01");
    }

    #[test]
    fn workspace_tables_and_members_scan() {
        let dir = tempfile::tempdir().unwrap();
        let sha = "b".repeat(40);
        write_file(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[workspace]\nmembers = [\"member\"]\n[workspace.dependencies]\nwsdep = { git = \"https://git.example/acme/wsdep\", branch = \"main\" }\n",
        );
        std::fs::create_dir(dir.path().join("member")).unwrap();
        write_file(
            &dir.path().join("member"),
            "Cargo.toml",
            &format!("[package]\nname = \"m\"\nversion = \"0.1.0\"\n[dependencies]\nmdep = {{ git = \"https://git.example/acme/mdep\", rev = \"{sha}\" }}\n"),
        );
        write_file(
            dir.path(),
            "Cargo.lock",
            &format!("[[package]]\nname = \"wsdep\"\nversion = \"0.1.0\"\nsource = \"git+https://git.example/acme/wsdep?branch=main#{sha}\"\n[[package]]\nname = \"mdep\"\nversion = \"0.2.0\"\nsource = \"git+https://git.example/acme/mdep?rev={sha}#{sha}\"\n"),
        );
        let config = test_config();
        let inv = build_inventory(dir.path(), &[], &config).unwrap();
        let paths: Vec<&str> = inv.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["acme/mdep", "acme/wsdep"]);
        // Workspace branch dep: moving wins over the lock hash.
        assert_eq!(inv[1].moving, Some("refs/heads/main".into()));
        // Member rev dep: pinned.
        assert_eq!(inv[0].pinned, Some(sha.clone()));
    }

    #[test]
    fn malformed_and_unresolvable_sources_fail_closed() {
        let config = test_config();
        // Malformed manifest: error, never partial config.
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "Cargo.toml", "not toml [[[\n");
        assert!(build_inventory(dir.path(), &[], &config).is_err());
        // Malformed lock: error.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        );
        write_file(dir.path(), "Cargo.lock", "[[[broken\n");
        assert!(build_inventory(dir.path(), &[], &config).is_err());
        // Owned bare dep without lock: explicit error, not silent skip.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[dependencies]\nbare = { git = \"https://git.example/acme/bare\" }\n",
        );
        assert!(build_inventory(dir.path(), &[], &config).is_err());
        // Foreign bare dep without lock: unaffected (ignored).
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[dependencies]\nforeign = { git = \"https://other.example/o/r\" }\n",
        );
        let inv = build_inventory(dir.path(), &[], &config).unwrap();
        assert!(inv.is_empty());
    }

    #[test]
    fn conflicting_and_unsupported_members_fail_closed() {
        let config = test_config();
        // Same URL, different refs across tables: conflict.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[dependencies]\nd = { git = \"https://git.example/acme/d\", branch = \"main\" }\n[dev-dependencies]\nd = { git = \"https://git.example/acme/d\", tag = \"v1\" }\n",
        );
        assert!(build_inventory(dir.path(), &[], &config).is_err());
        // Same URL, same ref twice: merged, not a conflict.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[dependencies]\nd = { git = \"https://git.example/acme/d\", branch = \"main\" }\n[dev-dependencies]\nd = { git = \"https://git.example/acme/d\", branch = \"main\" }\n",
        );
        write_file(
            dir.path(),
            "Cargo.lock",
            &format!("[[package]]\nname = \"d\"\nversion = \"0.1.0\"\nsource = \"git+https://git.example/acme/d?branch=main#{}\"\n", "c".repeat(40)),
        );
        let inv = build_inventory(dir.path(), &[], &config).unwrap();
        assert_eq!(inv.len(), 1);
        // Glob member pattern: unsupported, explicit error.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[workspace]\nmembers = [\"crates/*\"]\n",
        );
        assert!(build_inventory(dir.path(), &[], &config).is_err());
        // Listed but missing member manifest: explicit error.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n[workspace]\nmembers = [\"ghost\"]\n",
        );
        assert!(build_inventory(dir.path(), &[], &config).is_err());
    }
}

#[cfg(test)]
mod tool_pin_tests {
    use super::*;
    use crate::resolver::tests::test_config;

    fn pin() -> ToolPin {
        ToolPin {
            binary: "/workspaces/ci-tools/cfrg/abc/x86_64-unknown-linux-gnu/cfrg".into(),
            source_revision: "d".repeat(40),
            binary_sha256: "e".repeat(64),
        }
    }

    #[test]
    fn tool_pin_validation() {
        let mut config = test_config();
        config.tool = Some(pin());
        assert!(validate_runner_config(&config).is_ok());
        for mutate in [
            |p: &mut ToolPin| p.binary = "relative/path".into(),
            |p: &mut ToolPin| p.source_revision = "short".into(),
            |p: &mut ToolPin| p.source_revision = "D".repeat(40),
            |p: &mut ToolPin| p.binary_sha256 = "e".repeat(63),
        ] {
            let mut bad = pin();
            mutate(&mut bad);
            let mut config = test_config();
            config.tool = Some(bad);
            assert!(validate_runner_config(&config).is_err());
        }
    }

    #[test]
    fn pin_path_mismatch_fails_before_any_probe() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let env: Environment = BTreeMap::new();
        let pin = pin();
        // Resolved binary differs from the pinned path: fail closed without
        // touching the filesystem or spawning anything.
        let err = verify_tool_pin(root, &env, "/other/cfrg", &pin, 5).unwrap_err();
        assert!(err.to_string().contains("pinned path"));
        // Digest mismatch against a real small file.
        let candidate = root.join("cfrg");
        std::fs::write(&candidate, b"not the pinned binary").unwrap();
        let mut pin2 = pin.clone();
        pin2.binary = candidate.to_string_lossy().into_owned();
        let err = verify_tool_pin(root, &env, &pin2.binary, &pin2, 5).unwrap_err();
        assert!(err.to_string().contains("digest mismatch"));
    }
}

#[cfg(test)]
mod primary_source_tests {
    use super::*;
    use crate::resolver::tests::test_config;

    fn source() -> PrimarySource {
        PrimarySource {
            pointer_base: "https://pointer.example".into(),
            identities: BTreeMap::from([("https://forge.example".into(), "forgejo".into())]),
            timeout_secs: 10,
        }
    }

    #[test]
    fn primary_source_validation_and_passthrough() {
        let mut config = test_config();
        config.primary_source = Some(source());
        assert!(validate_runner_config(&config).is_ok());
        for mutate in [
            |s: &mut PrimarySource| s.pointer_base = "https://pointer.example/prefix".into(),
            |s: &mut PrimarySource| s.identities.clear(),
            |s: &mut PrimarySource| {
                s.identities
                    .insert("https://other.example/x".into(), "hub".into());
            },
            |s: &mut PrimarySource| s.timeout_secs = 61,
        ] {
            let mut bad = source();
            mutate(&mut bad);
            let mut config = test_config();
            config.primary_source = Some(bad);
            assert!(validate_runner_config(&config).is_err());
        }
        // Verbatim passthrough into the wire request (no ccid-side lookup).
        let inv = vec![DeclaredRepo {
            path: "acme/widget".into(),
            owner: "acme".into(),
            pinned: None,
            moving: Some("refs/heads/main".into()),
            sources: BTreeSet::from(["https://git.example/acme/widget".into()]),
        }];
        let primaries = BTreeMap::from([(
            "acme/widget".to_owned(),
            ResolvedPrimary {
                primary: None,
                primary_url: None,
            },
        )]);
        let value = build_request_json(&inv, &primaries, &config).unwrap();
        assert_eq!(
            value["primary_source"]["pointer_base"],
            "https://pointer.example"
        );
        assert_eq!(
            value["primary_source"]["identities"]["https://forge.example"],
            "forgejo"
        );
        // Absent feed serializes to nothing (backcompat).
        let mut bare = test_config();
        bare.primary_source = None;
        let value = build_request_json(&inv, &primaries, &bare).unwrap();
        assert!(value.get("primary_source").is_none());
    }
}

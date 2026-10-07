//! `rollout-verdict`: move the fleet to one aggregated verdict per commit.
//!
//! One unattended, resumable run. It waits (one blocking wait) until the
//! operator has provisioned the status reporter, then walks every eligible
//! repository through
//!
//! 1. repin the generated adapters to the verdict-capable ccid revision and
//!    re-render them on a branch,
//! 2. land that branch through `cfrg land` (exact green commits only),
//! 3. wait for the first green `ccid/verdict` on the landed commit,
//! 4. only then switch the repository's land policy from `ci/crow/*` to
//!    `ccid/verdict`, in the agents' copy and, when the rollout configuration
//!    names a declared policy source, in that repository too (batched, landed
//!    through `cfrg land`), because the landing lane reads the declared source.
//!
//! Repositories owned by other lanes are skipped unless the runtime they
//! already pin supports the verdict. State is persisted after every step, so a
//! restart continues where it stopped and repeats nothing that is done.
use super::cfrg::Cfrg;
use super::core;
use super::*;
use clap::Args;
use std::{
    process::Output,
    sync::{atomic::AtomicUsize, atomic::Ordering, Mutex},
    time::Duration,
};

/// First word of the LANES.md line the infrastructure owner appends.
pub(super) const MARKER: &str = "VERDICT-PROVISIONED";
const BRANCH: &str = "ci/verdict-rollout";
/// Branch of the declared policy source that carries the switched gates.
const GATE_BRANCH: &str = "ci/verdict-gates";
/// The message `cfrg land` answers (exit 2) when the branch is already in main.
const ALREADY_CONTAINED: &str = "already contained in the default branch";
const VERDICT: &str = "ccid/verdict";
const OLD_CONTEXT: &str = "ci/crow/*";

#[derive(Clone, Debug, Args)]
pub struct RolloutArgs {
    /// Verdict-capable ccid revision every adapter is pinned to.
    #[arg(long)]
    pub revision: String,
    /// Declared skip list and runtime support boundary.
    #[arg(long)]
    pub config: PathBuf,
    /// The land policy read by `cfrg land` and switched per repository.
    #[arg(long)]
    pub land_policy: PathBuf,
    #[arg(long)]
    pub land_state: PathBuf,
    /// LANES.md; the rollout starts when a line begins with the marker.
    #[arg(long)]
    pub lanes: PathBuf,
    /// Seconds to wait for the marker before giving up.
    #[arg(long, default_value_t = 172_800)]
    pub gate_wait: u64,
    /// Do not wait for the marker.
    #[arg(long)]
    pub no_gate: bool,
    /// Restrict the run to repositories matching these patterns.
    #[arg(long = "only")]
    pub only: Vec<String>,
    /// Repositories processed at once.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=8))]
    pub concurrency: u8,
    /// Seconds to wait for each landing and each first verdict.
    #[arg(long, default_value_t = 5400)]
    pub step_wait: u64,
    /// List what would happen; change nothing.
    #[arg(long)]
    pub plan: bool,
    #[arg(long, env = "CFRG_BIN", default_value = "cfrg")]
    pub cfrg: String,
    #[arg(long, default_value = "brain-commit")]
    pub brain_commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub schema: u32,
    /// First ccid revision that contains `ccid verdict`.
    pub supports_from: String,
    pub skip: Vec<Skip>,
    /// The repository that declares the land policy the landing lane reads.
    #[serde(default)]
    pub gate_source: Option<GateSource>,
}
/// Where the land policy is declared. Every switched gate must reach it, or
/// the landing lane keeps gating on the old context.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GateSource {
    /// `owner/name` of the declaring repository (an active Crow repository).
    pub repository: String,
    /// The declared policy inside it.
    pub file: String,
    /// Run in the checkout once the edit is committed; what it changes joins
    /// the commit (derived files). Empty: nothing derives from the policy.
    #[serde(default)]
    pub render: Vec<String>,
    /// Switched gates collected before a landing is started. A landing also
    /// happens when the run starts (gates left over) and when it ends.
    #[serde(default = "default_batch")]
    pub batch: usize,
}
fn default_batch() -> usize {
    20
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Skip {
    pub pattern: String,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub(super) struct Repo {
    pub full_name: String,
    pub clone_url: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Manifest {
    pub pin: String,
    /// Jobs that gate landing: `[verdict] jobs`, else `verify` when declared.
    pub gating: Vec<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Success,
    Failure,
    Pending,
}
/// What `cfrg land` did with the pushed branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Landing {
    /// Queued; the forge merges it when the head is green.
    Enqueued,
    /// The branch is already part of the default branch: the repin is in.
    AlreadyContained,
}
pub(super) enum Repin {
    /// The adapters already carry the revision.
    Unchanged,
    Pushed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Switch {
    Switched,
    Already,
    /// The repository declares its own contexts; left alone.
    Custom,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct RepoState {
    /// done | skipped | failed | awaiting
    pub status: String,
    pub note: String,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct State {
    pub revision: String,
    pub repos: BTreeMap<String, RepoState>,
}

pub(super) trait Fleet: Sync {
    fn manifest(&self, repo: &Repo) -> Result<Option<Manifest>>;
    /// Whether an already pinned runtime contains `ccid verdict`.
    fn supports(&self, revision: &str) -> bool;
    fn repin(&self, repo: &Repo, revision: &str) -> Result<Repin>;
    fn land(&self, repo: &Repo) -> Result<Landing>;
    fn head(&self, repo: &Repo) -> Result<String>;
    fn verdict(&self, repo: &Repo, sha: &str) -> Result<Option<Verdict>>;
    fn switch_policy(&self, repo: &Repo) -> Result<Switch>;
    fn sleep(&self, duration: Duration);
    /// Called once when every repository has been walked: the declared policy
    /// source must carry every gate the run switched. `waits` bounds the wait
    /// for its landing.
    fn finish(&self, _waits: u64) -> Result<()> {
        Ok(())
    }
}

pub(super) fn matches_pattern(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(org) => name.split('/').next() == Some(org),
        None => pattern == name,
    }
}

/// Block until a line of `lanes` begins with the marker.
pub(super) fn wait_for_marker(
    lanes: &Path,
    attempts: u64,
    interval: Duration,
    sleep: &dyn Fn(Duration),
) -> bool {
    for attempt in 0..=attempts {
        let found = fs::read_to_string(lanes)
            .is_ok_and(|text| text.lines().any(|l| l.trim_start().starts_with(MARKER)));
        if found {
            return true;
        }
        if attempt < attempts {
            sleep(interval);
        }
    }
    false
}

/// The outcome of one repository's rollout.
fn advance(
    fleet: &dyn Fleet,
    config: &Config,
    revision: &str,
    repo: &Repo,
    waits: u64,
) -> RepoState {
    let state = |status: &str, note: String| RepoState {
        status: status.into(),
        note,
    };
    let manifest = match fleet.manifest(repo) {
        Ok(Some(m)) => m,
        Ok(None) => return state("skipped", "no repository jobs (.ci/ccid.toml)".into()),
        Err(e) => return state("failed", format!("cannot read the manifest: {e}")),
    };
    if manifest.gating.is_empty() {
        return state(
            "skipped",
            "no verify job and no [verdict] declaration: the owner declares the gate first".into(),
        );
    }
    if let Some(skip) = config
        .skip
        .iter()
        .find(|s| matches_pattern(&s.pattern, &repo.full_name))
    {
        if !fleet.supports(&manifest.pin) {
            return state(
                "skipped",
                format!("{} (its runtime has no verdict)", skip.reason),
            );
        }
    }
    if manifest.pin != revision && !fleet.supports(&manifest.pin) {
        match fleet.repin(repo, revision) {
            Ok(Repin::Pushed) => match fleet.land(repo) {
                Err(e) => return state("failed", format!("landing was not enqueued: {e}")),
                // The branch is already part of main: nothing to wait for,
                // go on to the verdict.
                Ok(Landing::AlreadyContained) => {}
                Ok(Landing::Enqueued) => {
                    let mut landed = false;
                    for _ in 0..waits {
                        if fleet
                            .manifest(repo)
                            .is_ok_and(|m| m.is_some_and(|m| m.pin == revision))
                        {
                            landed = true;
                            break;
                        }
                        fleet.sleep(INTERVAL);
                    }
                    if !landed {
                        return state(
                            "awaiting",
                            "the branch is queued; the landing has not completed (red or waiting head)"
                                .into(),
                        );
                    }
                }
            },
            Ok(Repin::Unchanged) => {}
            Err(e) => return state("failed", format!("repin failed: {e}")),
        }
    }
    let head = match fleet.head(repo) {
        Ok(h) => h,
        Err(e) => return state("failed", format!("cannot read main: {e}")),
    };
    let mut seen = None;
    for _ in 0..waits.max(1) {
        match fleet.verdict(repo, &head) {
            Ok(Some(Verdict::Success)) => {
                seen = Some(Verdict::Success);
                break;
            }
            Ok(Some(Verdict::Failure)) => {
                return state("failed", format!("the first {VERDICT} on {head} is red"));
            }
            Ok(other) => seen = other,
            Err(e) => return state("failed", format!("cannot read statuses: {e}")),
        }
        fleet.sleep(INTERVAL);
    }
    if seen != Some(Verdict::Success) {
        return state(
            "awaiting",
            format!("no green {VERDICT} on {head} yet; the land policy is unchanged"),
        );
    }
    match fleet.switch_policy(repo) {
        Ok(Switch::Switched) => state("done", format!("landing now gates on {VERDICT}")),
        Ok(Switch::Already) => state("done", format!("landing already gates on {VERDICT}")),
        Ok(Switch::Custom) => state("done", "verdict reported; custom land contexts kept".into()),
        Err(e) => state("failed", format!("policy switch failed: {e}")),
    }
}
const INTERVAL: Duration = Duration::from_secs(30);

/// What a rollout walk needs besides the repositories and the state.
pub(super) struct Run<'a> {
    pub fleet: &'a dyn Fleet,
    pub config: &'a Config,
    pub revision: &'a str,
    pub waits: u64,
    pub concurrency: usize,
}

/// Walk every repository, `concurrency` at a time, persisting after each.
pub(super) fn drive(
    run: &Run,
    repos: &[Repo],
    state: &Mutex<State>,
    persist: &(dyn Fn(&State) -> Result<()> + Sync),
    emit: &(dyn Fn(&str) + Sync),
) {
    let queue = Mutex::new(repos.iter().collect::<std::collections::VecDeque<_>>());
    std::thread::scope(|scope| {
        for _ in 0..run.concurrency.max(1) {
            scope.spawn(|| loop {
                let Some(repo) = queue.lock().unwrap().pop_front() else {
                    break;
                };
                let done = state
                    .lock()
                    .unwrap()
                    .repos
                    .get(&repo.full_name)
                    .is_some_and(|s| s.status == "done");
                if done {
                    emit(&format!(
                        "done     {} (from an earlier run)",
                        repo.full_name
                    ));
                    continue;
                }
                let outcome = advance(run.fleet, run.config, run.revision, repo, run.waits);
                emit(&format!(
                    "{:<8} {} {}",
                    outcome.status, repo.full_name, outcome.note
                ));
                let mut guard = state.lock().unwrap();
                guard.repos.insert(repo.full_name.clone(), outcome);
                let _ = persist(&guard);
            });
        }
    });
}

/// Replace the pinned revision in a manifest, nothing else.
pub(super) fn pin_manifest(text: &str, revision: &str) -> Result<String> {
    let mut replaced = 0;
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("tool_revision") && line.contains('=') {
                replaced += 1;
                format!("tool_revision = \"{revision}\"")
            } else {
                line.to_owned()
            }
        })
        .collect();
    if replaced != 1 {
        return Err("Expected exactly one tool_revision line".into());
    }
    Ok(format!("{}\n", lines.join("\n")))
}

/// Parse the pieces of `.ci/ccid.toml` the rollout needs.
pub(super) fn parse_manifest(text: &str) -> Option<Manifest> {
    let value: toml::Value = toml::from_str(text).ok()?;
    let pin = value
        .get("render")?
        .get("tool_revision")?
        .as_str()?
        .to_owned();
    let jobs = value.get("jobs")?.as_table()?;
    if jobs.is_empty() {
        return None;
    }
    let gating: Vec<String> = match value.get("verdict").and_then(|v| v.get("jobs")) {
        Some(declared) => declared
            .as_array()?
            .iter()
            .filter_map(|j| j.as_str().map(str::to_owned))
            .collect(),
        None if jobs.contains_key("verify") => vec!["verify".into()],
        None => vec![],
    };
    Some(Manifest { pin, gating })
}

/// Point a repository's land policy at the verdict. Returns the new document
/// and what was done.
pub(super) fn switch_policy_document(policy: &mut Value, repository: &str) -> Result<Switch> {
    let repositories = policy["repositories"]
        .as_array_mut()
        .ok_or("Land policy lacks a repositories list")?;
    let Some(entry) = repositories.iter_mut().find(|e| e["path"] == repository) else {
        repositories.push(json!({"path":repository,"contexts":[VERDICT]}));
        return Ok(Switch::Switched);
    };
    match entry.get("contexts").and_then(Value::as_array) {
        None => {
            entry["contexts"] = json!([VERDICT]);
            Ok(Switch::Switched)
        }
        Some(contexts) if contexts == &[json!(VERDICT)] => Ok(Switch::Already),
        Some(contexts) if contexts == &[json!(OLD_CONTEXT)] => {
            entry["contexts"] = json!([VERDICT]);
            Ok(Switch::Switched)
        }
        Some(_) => Ok(Switch::Custom),
    }
}

/// Repositories whose land policy entry gates on the verdict.
pub(super) fn verdict_gates(policy: &Value) -> BTreeSet<String> {
    policy["repositories"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["contexts"] == json!([VERDICT]))
        .filter_map(|e| e["path"].as_str().map(str::to_owned))
        .collect()
}

/// Add the verdict gate of every `wanted` repository to the text of the
/// declared policy, editing only the lines it needs (the file is reviewed by
/// people: no reordering, no reformatting). `None` when nothing is missing.
/// The result is checked against the same change made on the parsed document,
/// so an edit that did not do exactly that is refused.
pub(super) fn declare_gates(text: &str, wanted: &BTreeSet<String>) -> Result<Option<String>> {
    let mut expected: Value = serde_json::from_str(text)?;
    let mut missing = Vec::new();
    for repository in wanted {
        if switch_policy_document(&mut expected, repository)? == Switch::Switched {
            missing.push(repository.as_str());
        }
    }
    if missing.is_empty() {
        return Ok(None);
    }
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    for repository in &missing {
        put_gate(&mut lines, repository)?;
    }
    let edited = format!("{}\n", lines.join("\n"));
    if serde_json::from_str::<Value>(&edited)? != expected {
        return Err("The declared policy could not be edited faithfully".into());
    }
    Ok(Some(edited))
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Make the entry of `repository` gate on the verdict, in the pretty-printed
/// layout the declared policy uses (one key per line, arrays one element per
/// line).
fn put_gate(lines: &mut Vec<String>, repository: &str) -> Result<()> {
    let key = format!("\"path\": \"{repository}\"");
    let at = lines
        .iter()
        .position(|l| l.trim() == key || l.trim() == format!("{key},"));
    let Some(at) = at else {
        return append_entry(lines, repository);
    };
    let key_indent = indent_of(&lines[at]).to_owned();
    let open = (0..at)
        .rev()
        .find(|&k| lines[k].trim() == "{")
        .ok_or("Declared policy entry has no opening brace")?;
    let object_indent = indent_of(&lines[open]).to_owned();
    let close = (at + 1..lines.len())
        .find(|&k| indent_of(&lines[k]) == object_indent && matches!(lines[k].trim(), "}" | "},"))
        .ok_or("Declared policy entry has no closing brace")?;
    let contexts = (at + 1..close).find(|&k| {
        indent_of(&lines[k]) == key_indent && lines[k].trim_start().starts_with("\"contexts\": [")
    });
    match contexts {
        Some(first) if lines[first].contains(']') => {
            let comma = if lines[first].trim_end().ends_with(',') {
                ","
            } else {
                ""
            };
            lines[first] = format!("{key_indent}\"contexts\": [\"{VERDICT}\"]{comma}");
        }
        Some(first) => {
            let last = (first + 1..close)
                .find(|&k| {
                    indent_of(&lines[k]) == key_indent && lines[k].trim_start().starts_with(']')
                })
                .ok_or("Declared policy contexts are not closed")?;
            lines.splice(first + 1..last, [format!("{key_indent}  \"{VERDICT}\"")]);
        }
        None => {
            let more = at + 1 < close;
            if !lines[at].trim_end().ends_with(',') {
                lines[at].push(',');
            }
            lines.splice(
                at + 1..at + 1,
                [
                    format!("{key_indent}\"contexts\": ["),
                    format!("{key_indent}  \"{VERDICT}\""),
                    format!("{key_indent}]{}", if more { "," } else { "" }),
                ],
            );
        }
    }
    Ok(())
}

/// A new entry at the end of the repositories list.
fn append_entry(lines: &mut Vec<String>, repository: &str) -> Result<()> {
    let list = lines
        .iter()
        .position(|l| l.trim_start().starts_with("\"repositories\": ["))
        .ok_or("Declared policy has no repositories list")?;
    let list_indent = indent_of(&lines[list]).to_owned();
    let end = (list + 1..lines.len())
        .find(|&k| indent_of(&lines[k]) == list_indent && lines[k].trim_start().starts_with(']'))
        .ok_or("Declared policy repositories list is not closed")?;
    if end > list + 1 && lines[end - 1].trim() == "}" {
        lines[end - 1].push(',');
    }
    let object = format!("{list_indent}  ");
    let key = format!("{object}  ");
    lines.splice(
        end..end,
        [
            format!("{object}{{"),
            format!("{key}\"path\": \"{repository}\","),
            format!("{key}\"contexts\": ["),
            format!("{key}  \"{VERDICT}\""),
            format!("{key}]"),
            format!("{object}}}"),
        ],
    );
    Ok(())
}

/// The declared policy source and what the run still owes it.
struct Gates<'a> {
    source: &'a GateSource,
    repo: Repo,
    /// One preparation or landing at a time.
    lock: Mutex<()>,
    /// Gates switched since the last landing was started.
    pending: AtomicUsize,
    /// Everything this run switched (the agents' copy may be regenerated).
    switched: Mutex<BTreeSet<String>>,
}
/// What a gate publication did.
#[derive(Debug, PartialEq, Eq)]
enum Published {
    /// The declared source already carries every gate.
    Current,
    /// Another publication is under way; it will pick these up.
    Busy,
    /// A branch with this many more gates was pushed and handed to `cfrg land`.
    Enqueued(usize),
}

struct Live<'a> {
    config: &'a super::Config,
    cfrg: Cfrg,
    args: &'a RolloutArgs,
    min: &'a str,
    root: PathBuf,
    policy_lock: Mutex<()>,
    /// The pinned planner is staged under a non-blocking file lock: workers of
    /// this run take turns instead of failing against each other.
    planner_lock: Mutex<()>,
    gates: Option<Gates<'a>>,
}
impl Live<'_> {
    /// A file on the repository's main, `None` when it does not exist.
    fn contents(&self, repo: &Repo, path: &str) -> Result<Option<String>> {
        let target = (repo.full_name.clone(), "main".to_owned());
        let line = self
            .cfrg
            .contents(&[target], &[path], &BTreeSet::new())?
            .into_iter()
            .next()
            .ok_or("cfrg returned no answer")?;
        match line.state.as_str() {
            "found" => line.files.first().map_or(Ok(None), |file| file.text()),
            "absent" => Ok(None),
            _ => Err(line.error.unwrap_or_else(|| "not read".into()).into()),
        }
    }
    fn checkout(&self, repo: &Repo) -> Result<PathBuf> {
        if !matches(
            r"^[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*$",
            &repo.full_name,
        ) {
            return Err("Invalid repository name".into());
        }
        Ok(self.root.join("checkouts").join(&repo.full_name))
    }
}
impl Live<'_> {
    /// `cfrg land` for one pushed branch. Exit 2 with the "already contained"
    /// message means the branch is part of main already, which is as good as
    /// landed.
    fn cfrg_land(&self, repository: &str, branch: &str) -> Result<Landing> {
        let result: Output = Command::new(&self.args.cfrg)
            .args(["land", "--policy"])
            .arg(&self.args.land_policy)
            .arg("--state-dir")
            .arg(&self.args.land_state)
            .arg(repository)
            .arg(branch)
            .env("CFRG_LAND_TOKEN", self.cfrg.token())
            .stdin(Stdio::null())
            .output()?;
        land_outcome(&result.status, &String::from_utf8_lossy(&result.stderr))
    }

    /// A clean checkout of the declared source at its main, on the gate branch.
    fn gate_checkout(&self, gates: &Gates) -> Result<PathBuf> {
        let dir = self.checkout(&gates.repo)?;
        if !dir.join(".git").exists() {
            fs::create_dir_all(dir.parent().ok_or("Checkout needs a parent")?)?;
            output(
                &strings(&[
                    "git",
                    "clone",
                    "--quiet",
                    "--no-tags",
                    &gates.repo.clone_url,
                    &dir.to_string_lossy(),
                ]),
                None,
                None,
            )?;
        }
        git(&dir, &["fetch", "--quiet", "--no-tags", "origin", "main"])?;
        git(
            &dir,
            &[
                "checkout",
                "--quiet",
                "--force",
                "-B",
                GATE_BRANCH,
                "origin/main",
            ],
        )?;
        git(&dir, &["clean", "-fdxq"])?;
        Ok(dir)
    }

    fn gate_file(gates: &Gates, dir: &Path) -> Result<PathBuf> {
        let file = Path::new(&gates.source.file);
        if file.is_absolute()
            || file
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("The declared policy must be a path inside its repository".into());
        }
        Ok(dir.join(file))
    }

    /// Every repository the agents' copy or this run gates on the verdict.
    fn wanted_gates(&self, gates: &Gates) -> Result<BTreeSet<String>> {
        let policy: Value = serde_json::from_slice(&fs::read(&self.args.land_policy)?)?;
        let mut wanted = verdict_gates(&policy);
        wanted.extend(
            gates
                .switched
                .lock()
                .map_err(|_| "Gate set poisoned")?
                .iter()
                .cloned(),
        );
        Ok(wanted)
    }

    /// The gates the declared source does not carry on its main.
    fn missing_gates(&self, gates: &Gates) -> Result<BTreeSet<String>> {
        let dir = self.gate_checkout(gates)?;
        let text = fs::read_to_string(Self::gate_file(gates, &dir)?)?;
        let wanted = self.wanted_gates(gates)?;
        let mut policy: Value = serde_json::from_str(&text)?;
        let mut missing = BTreeSet::new();
        for repository in wanted {
            if switch_policy_document(&mut policy, &repository)? == Switch::Switched {
                missing.insert(repository);
            }
        }
        Ok(missing)
    }

    /// Carry the gates over to the declared source: edit its policy on a branch
    /// off main, regenerate what derives from it, push and hand the branch to
    /// `cfrg land`. The landing itself is not awaited here.
    fn publish_gates(&self, wait: bool) -> Result<Published> {
        let Some(gates) = &self.gates else {
            return Ok(Published::Current);
        };
        let _guard = if wait {
            gates.lock.lock().map_err(|_| "Gate lock poisoned")?
        } else {
            match gates.lock.try_lock() {
                Ok(guard) => guard,
                Err(_) => return Ok(Published::Busy),
            }
        };
        gates.pending.store(0, Ordering::SeqCst);
        let dir = self.gate_checkout(gates)?;
        let file = Self::gate_file(gates, &dir)?;
        let wanted = self.wanted_gates(gates)?;
        let Some(edited) = declare_gates(&fs::read_to_string(&file)?, &wanted)? else {
            return Ok(Published::Current);
        };
        let before = serde_json::from_str::<Value>(&fs::read_to_string(&file)?)?;
        let after = serde_json::from_str::<Value>(&edited)?;
        let added = verdict_gates(&after).len() - verdict_gates(&before).len();
        fs::write(&file, &edited)?;
        git(&dir, &["add", "--", &gates.source.file])?;
        git(
            &dir,
            &[
                "commit",
                "--quiet",
                "-m",
                &format!("Land policy: {added} more repositories gate on {VERDICT}"),
            ],
        )?;
        if !gates.source.render.is_empty() {
            output(&gates.source.render, Some(&dir), None)?;
            git(&dir, &["add", "-A"])?;
            if !git(&dir, &["diff", "--cached", "--name-only"])?.is_empty() {
                git(&dir, &["commit", "--quiet", "--amend", "--no-edit"])?;
            }
        }
        output(
            &strings(&[
                "git",
                "push",
                "--quiet",
                "--force",
                &gates.repo.clone_url,
                &format!("{GATE_BRANCH}:{GATE_BRANCH}"),
            ]),
            Some(&dir),
            None,
        )?;
        self.cfrg_land(&gates.repo.full_name, GATE_BRANCH)?;
        Ok(Published::Enqueued(added))
    }
}

/// Interpret the end of one `cfrg land` call.
pub(super) fn land_outcome(status: &std::process::ExitStatus, stderr: &str) -> Result<Landing> {
    if status.success() {
        return Ok(Landing::Enqueued);
    }
    if status.code() == Some(2) && stderr.contains(ALREADY_CONTAINED) {
        return Ok(Landing::AlreadyContained);
    }
    let last = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no message");
    Err(format!("cfrg land exited with {status}: {last}").into())
}

impl Fleet for Live<'_> {
    fn manifest(&self, repo: &Repo) -> Result<Option<Manifest>> {
        Ok(self
            .contents(repo, ".ci/ccid.toml")?
            .and_then(|t| parse_manifest(&t)))
    }
    fn supports(&self, revision: &str) -> bool {
        exact_sha(revision)
            && git(
                &self.config.tool_repo,
                &["merge-base", "--is-ancestor", self.min, revision],
            )
            .is_ok()
    }
    fn repin(&self, repo: &Repo, revision: &str) -> Result<Repin> {
        let dir = self.checkout(repo)?;
        if !dir.join(".git").exists() {
            fs::create_dir_all(dir.parent().ok_or("Checkout needs a parent")?)?;
            output(
                &strings(&[
                    "git",
                    "clone",
                    "--quiet",
                    "--no-tags",
                    &repo.clone_url,
                    &dir.to_string_lossy(),
                ]),
                None,
                None,
            )?;
        }
        git(&dir, &["fetch", "--quiet", "--no-tags", "origin", "main"])?;
        git(
            &dir,
            &[
                "checkout",
                "--quiet",
                "--force",
                "-B",
                BRANCH,
                "origin/main",
            ],
        )?;
        git(&dir, &["clean", "-fdxq"])?;
        let manifest_path = dir.join(".ci/ccid.toml");
        let pinned = pin_manifest(&fs::read_to_string(&manifest_path)?, revision)?;
        fs::write(&manifest_path, pinned)?;
        let renderer = {
            let _turn = self
                .planner_lock
                .lock()
                .map_err(|_| "Planner lock poisoned")?;
            let remote = pinned::executable(self.config, revision)?;
            jobs::planner(self.config, revision, &remote)?
        };
        output(
            &[
                renderer.to_string_lossy().into_owned(),
                "render".into(),
                "--repo".into(),
                dir.to_string_lossy().into_owned(),
            ],
            None,
            None,
        )?;
        git(&dir, &["add", "-A", ".ci", ".crow"])?;
        if git(&dir, &["diff", "--cached", "--name-only"])?.is_empty() {
            return Ok(Repin::Unchanged);
        }
        git(
            &dir,
            &[
                "commit",
                "--quiet",
                "-m",
                "Pin the shared-cache runtime that reports one verdict per commit",
            ],
        )?;
        output(
            &strings(&[
                "git",
                "push",
                "--quiet",
                "--force",
                &repo.clone_url,
                &format!("{BRANCH}:{BRANCH}"),
            ]),
            Some(&dir),
            None,
        )?;
        Ok(Repin::Pushed)
    }
    fn land(&self, repo: &Repo) -> Result<Landing> {
        self.cfrg_land(&repo.full_name, BRANCH)
    }
    fn head(&self, repo: &Repo) -> Result<String> {
        match self.cfrg.head(&repo.full_name, "main")? {
            Some(sha) if exact_sha(&sha) => Ok(sha),
            _ => Err("Forge returned no head commit".into()),
        }
    }
    fn verdict(&self, repo: &Repo, sha: &str) -> Result<Option<Verdict>> {
        let statuses = self.cfrg.statuses(&repo.full_name, sha)?;
        Ok(statuses
            .iter()
            .find(|(context, _)| context == VERDICT)
            .map(|(_, state)| match state.as_str() {
                "success" => Verdict::Success,
                "failure" => Verdict::Failure,
                _ => Verdict::Pending,
            }))
    }
    fn switch_policy(&self, repo: &Repo) -> Result<Switch> {
        let outcome = {
            let _guard = self
                .policy_lock
                .lock()
                .map_err(|_| "Policy lock poisoned")?;
            let path = &self.args.land_policy;
            let mut policy: Value = serde_json::from_slice(&fs::read(path)?)?;
            let outcome = switch_policy_document(&mut policy, &repo.full_name)?;
            if outcome == Switch::Switched {
                let mut file = tempfile::NamedTempFile::new_in(
                    path.parent().ok_or("Policy needs a directory")?,
                )?;
                writeln!(file, "{}", serde_json::to_string_pretty(&policy)?)?;
                file.as_file().sync_all()?;
                file.persist(path)?;
                output(
                    &[
                        self.args.brain_commit.clone(),
                        "-m".into(),
                        format!("Land policy: {} gates on {VERDICT}", repo.full_name),
                        path.to_string_lossy().into_owned(),
                    ],
                    None,
                    None,
                )?;
            }
            outcome
        };
        // The landing lane reads the declared source: the gate must get there
        // too. A failure here is reported, not charged to this repository: the
        // next batch and the end of the run carry it over again.
        if let (Some(gates), Switch::Switched | Switch::Already) = (&self.gates, outcome) {
            if let Ok(mut switched) = gates.switched.lock() {
                switched.insert(repo.full_name.clone());
            }
            let pending = gates.pending.fetch_add(1, Ordering::SeqCst) + 1;
            if pending >= gates.source.batch.max(1) {
                match self.publish_gates(false) {
                    Ok(published) => println!("gates    {published:?}"),
                    Err(e) => println!("gates    not published yet: {e}"),
                }
            }
        }
        Ok(outcome)
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
    fn finish(&self, waits: u64) -> Result<()> {
        let Some(gates) = &self.gates else {
            return Ok(());
        };
        let published = self.publish_gates(true)?;
        println!("gates    {published:?}");
        // The landing lane must carry every gate before the run is complete.
        for _ in 0..waits.max(1) {
            if self.missing_gates(gates)?.is_empty() {
                return Ok(());
            }
            self.sleep(INTERVAL);
        }
        Err(format!(
            "The gates are not in {} yet: {}; rerun to resume",
            gates.source.repository, gates.source.file
        )
        .into())
    }
}

pub(super) fn run(config: &super::Config, args: &RolloutArgs) -> Result<()> {
    if !exact_sha(&args.revision) {
        return Err("--revision must be an exact 40-hex ccid revision".into());
    }
    let rollout: Config = serde_json::from_slice(&fs::read(&args.config)?)?;
    if rollout.schema != 1 || !exact_sha(&rollout.supports_from) {
        return Err("Unsupported rollout configuration".into());
    }
    let crow = core::Crow::new(config)?;
    let records = core::pages(&crow, "/repos?active=true")?;
    let first = records
        .first()
        .ok_or("Crow reports no active repositories")?;
    let cfrg = Cfrg::open(config, &args.cfrg, &text(first, "clone_url"))?;
    let directory = config.directory("crow-ci-rollout")?;
    let gates = rollout
        .gate_source
        .as_ref()
        .map(|source| -> Result<Gates> {
            let record = records
                .iter()
                .find(|r| text(r, "full_name") == source.repository)
                .ok_or("The gate source repository is not an active Crow repository")?;
            Ok(Gates {
                source,
                repo: Repo {
                    full_name: text(record, "full_name"),
                    clone_url: text(record, "clone_url"),
                },
                lock: Mutex::new(()),
                pending: AtomicUsize::new(0),
                switched: Mutex::new(BTreeSet::new()),
            })
        })
        .transpose()?;
    let live = Live {
        config,
        cfrg,
        args,
        min: &rollout.supports_from,
        root: directory.clone(),
        policy_lock: Mutex::new(()),
        planner_lock: Mutex::new(()),
        gates,
    };
    let mut repos: Vec<Repo> = records
        .iter()
        .filter(|r| r["active"] == true)
        .map(|r| Repo {
            full_name: text(r, "full_name"),
            clone_url: text(r, "clone_url"),
        })
        .filter(|r| {
            args.only.is_empty() || args.only.iter().any(|p| matches_pattern(p, &r.full_name))
        })
        .collect();
    repos.sort_by(|a, b| a.full_name.cmp(&b.full_name));
    let waits = (args.step_wait / INTERVAL.as_secs()).max(1);
    if args.plan {
        for repo in &repos {
            let line = match live.manifest(repo) {
                Ok(None) => "skip no repository jobs".to_owned(),
                Ok(Some(m)) if m.gating.is_empty() => {
                    "skip no verify job and no [verdict]".to_owned()
                }
                Ok(Some(m)) => {
                    let skip = rollout
                        .skip
                        .iter()
                        .find(|s| matches_pattern(&s.pattern, &repo.full_name));
                    let supported = live.supports(&m.pin);
                    match (skip, m.pin == args.revision, supported) {
                        (Some(s), _, false) => format!("skip {}", s.reason),
                        (_, true, _) => {
                            format!("repinned already; verdict gate {}", m.gating.join(","))
                        }
                        (_, _, true) => format!(
                            "runtime {} already supports the verdict",
                            &m.pin[..8.min(m.pin.len())]
                        ),
                        _ => format!(
                            "repin {} -> {}",
                            &m.pin[..8.min(m.pin.len())],
                            &args.revision[..8]
                        ),
                    }
                }
                Err(e) => format!("unreadable: {e}"),
            };
            println!("{} {line}", repo.full_name);
        }
        return Ok(());
    }
    if !args.no_gate {
        println!(
            "waiting for a line beginning with {MARKER} in {}",
            args.lanes.display()
        );
        let attempts = args.gate_wait / INTERVAL.as_secs();
        if !wait_for_marker(&args.lanes, attempts, INTERVAL, &std::thread::sleep) {
            return Err(format!(
                "{MARKER} did not appear within the gate wait; nothing was changed"
            )
            .into());
        }
    }
    let state_path = directory.join(format!("{}.json", &args.revision[..16]));
    let state: State = fs::read(&state_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|s: &State| s.revision == args.revision)
        .unwrap_or_else(|| State {
            revision: args.revision.clone(),
            repos: BTreeMap::new(),
        });
    let state = Mutex::new(state);
    // Gates a former run switched but never carried to the declared source.
    if live.gates.is_some() {
        match live.publish_gates(true) {
            Ok(published) => println!("gates    {published:?}"),
            Err(e) => println!("gates    not published yet: {e}"),
        }
    }
    drive(
        &Run {
            fleet: &live,
            config: &rollout,
            revision: &args.revision,
            waits,
            concurrency: usize::from(args.concurrency),
        },
        &repos,
        &state,
        &|s| save(&state_path, &serde_json::to_value(s)?),
        &|line| println!("{line}"),
    );
    let gates = live.finish(waits);
    if let Err(e) = &gates {
        println!("gates    {e}");
    }
    let state = state.lock().map_err(|_| "State lock poisoned")?;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for outcome in state.repos.values() {
        *counts.entry(outcome.status.as_str()).or_default() += 1;
    }
    println!("summary {} {counts:?}", &args.revision[..12]);
    if state
        .repos
        .values()
        .all(|s| matches!(s.status.as_str(), "done" | "skipped"))
        && gates.is_ok()
    {
        Ok(())
    } else {
        Err("Some repositories are failed or awaiting, or the gates are not declared; rerun to resume".into())
    }
}

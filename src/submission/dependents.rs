//! `check-dependents`: the step after every landing.
//!
//! After a repository's main moves, find the active repositories that depend
//! on it (read from the forge), submit each one's own repository job on its
//! current main, and print one digest line per dependent. Unaffected
//! dependents hit the shared result cache and cost almost nothing. At most
//! `slots` (never more than eight) runs are in flight, workers wait for host
//! admission instead of failing, and a dependent is submitted at most once
//! per landing: the landing's record survives restarts and a second call only
//! resumes and reports.
use super::cli::DependentsArgs;
use super::core::{self, Api};
use super::forge_scan::{self, Cache, Forge, Matcher};
use super::log_digest::{self, CacheStats};
use super::*;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_secs(10);
const RETRY: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub(super) struct Dependent {
    pub full_name: String,
    pub repo_id: u64,
    pub clone_url: String,
    pub branch: String,
    pub files: Vec<String>,
    pub pins: BTreeSet<String>,
}

pub(super) enum Prepared {
    Ready(String),
    Skip(String),
}
pub(super) enum Submitted {
    Run(u64),
    Existing(u64),
    Deferred(String),
    Failed(String),
}
pub(super) trait Submitter {
    /// Bring the dependent's checkout to its current default branch head.
    fn prepare(&self, dependent: &Dependent) -> Result<Prepared>;
    fn submit(&self, dependent: &Dependent) -> Result<Submitted>;
}
pub(super) trait Clock {
    fn elapsed(&self) -> Duration;
    fn sleep(&mut self, duration: Duration);
}
pub(super) struct Wall(Instant);
impl Wall {
    pub fn start() -> Self {
        Self(Instant::now())
    }
}
impl Clock for Wall {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// What was submitted for one dependent during this landing.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Entry {
    pub commit: String,
    pub repo_id: u64,
    pub number: u64,
    /// An identical run already existed; nothing new was computed or queued.
    pub reused: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Record {
    pub identity: String,
    pub landed: String,
    pub runs: BTreeMap<String, Entry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Class {
    Green,
    Red,
    Infra,
    Error,
    Timeout,
    Skipped,
}
pub(super) struct Outcome {
    pub class: Class,
    pub line: String,
    pub cache: Option<CacheStats>,
    pub reused: bool,
}

fn pin_text(dependent: &Dependent, landed: &str) -> String {
    if dependent.pins.contains(landed) {
        "pin=landed".into()
    } else if let Some(pin) = dependent.pins.iter().next() {
        format!("pin={}", digest::short(pin))
    } else {
        "pin=-".into()
    }
}
fn cache_text(stats: Option<CacheStats>) -> String {
    match stats {
        Some(s) if s.requests > 0 => format!("cache {}/{}", s.hits, s.requests),
        Some(s) if s.bypassed > 0 => "cache bypassed".into(),
        _ => "cache -".into(),
    }
}
fn clip(text: &str, width: usize) -> String {
    let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= width {
        text
    } else {
        format!("{}…", text.chars().take(width - 1).collect::<String>())
    }
}
fn line(class: &str, dependent: &Dependent, commit: &str, rest: String) -> String {
    format!(
        "{class:<7}{} {} {rest}",
        dependent.full_name,
        digest::short(commit)
    )
}

/// Result line of a finished run.
fn finish(
    api: &dyn Api,
    redact: &dyn Fn(&str) -> String,
    dependent: &Dependent,
    landed: &str,
    entry: &Entry,
    run: &Value,
) -> Outcome {
    let id = format!("{}#{}", entry.repo_id, entry.number);
    let pin = pin_text(dependent, landed);
    let reused = if entry.reused { " (existing run)" } else { "" };
    let failed = digest::failed_steps(run);
    if text(run, "status") == "success" && failed.is_empty() {
        let stats = rows(&run["workflows"])
            .iter()
            .flat_map(|w| rows(&w["children"]))
            .filter(|s| text(s, "name") == "repository-job")
            .find_map(|s| {
                digest::step_log(api, redact, entry.repo_id, entry.number, number(s, "id"))
                    .ok()
                    .and_then(|lines| log_digest::cache_stats(&lines))
            });
        let seconds = digest::seconds(run).map_or(String::new(), |s| format!(" {s}s"));
        return Outcome {
            class: Class::Green,
            line: line(
                "green",
                dependent,
                &entry.commit,
                format!("{id} {pin} {}{seconds}{reused}", cache_text(stats)),
            ),
            cache: stats,
            reused: entry.reused,
        };
    }
    let pointer = format!("-> crow-ci digest {} {}", entry.repo_id, entry.number);
    let headline = failed.first().and_then(|(_, step, ..)| {
        let lines = digest::step_log(api, redact, entry.repo_id, entry.number, *step).ok()?;
        let (infrastructure, why) = log_digest::headline(&lines)?;
        Some((infrastructure, clip(&why, 120)))
    });
    let (class, label, why) = match headline {
        Some((true, why)) => (Class::Infra, "infra", why),
        Some((false, why)) => (Class::Red, "red", why),
        None => (
            Class::Red,
            "red",
            format!("pipeline {}, log unavailable or empty", text(run, "status")),
        ),
    };
    Outcome {
        class,
        line: line(
            label,
            dependent,
            &entry.commit,
            format!("{id} {pin} {why} {pointer}{reused}"),
        ),
        cache: None,
        reused: entry.reused,
    }
}

pub(super) struct Drive<'a> {
    pub api: &'a dyn Api,
    pub redact: &'a dyn Fn(&str) -> String,
    pub submitter: &'a dyn Submitter,
    pub clock: &'a mut dyn Clock,
    pub slots: usize,
    pub wait: Duration,
    pub landed: String,
}

/// Submit and watch every dependent. `emit` receives each result line as
/// soon as it is known; `persist` stores the record after every submission.
pub(super) fn drive(
    drive: &mut Drive,
    dependents: Vec<Dependent>,
    record: &mut Record,
    persist: &mut dyn FnMut(&Record) -> Result<()>,
    emit: &mut dyn FnMut(&str),
) -> Result<Vec<Outcome>> {
    struct Active {
        dependent: Dependent,
        entry: Entry,
    }
    let mut queue: VecDeque<Dependent> = dependents.into();
    let mut active: Vec<Active> = vec![];
    let mut outcomes: Vec<Outcome> = vec![];
    let report = |outcome: Outcome, emit: &mut dyn FnMut(&str), outcomes: &mut Vec<Outcome>| {
        emit(&outcome.line);
        outcomes.push(outcome);
    };
    loop {
        while active.len() < drive.slots {
            let Some(dependent) = queue.pop_front() else {
                break;
            };
            if let Some(entry) = record.runs.get(&dependent.full_name) {
                active.push(Active {
                    dependent,
                    entry: entry.clone(),
                });
                continue;
            }
            let commit = match drive.submitter.prepare(&dependent) {
                Ok(Prepared::Ready(commit)) => commit,
                Ok(Prepared::Skip(reason)) => {
                    let line = line(
                        "skip",
                        &dependent,
                        "",
                        format!("{reason}; submit its checks by hand"),
                    );
                    report(
                        Outcome {
                            class: Class::Skipped,
                            line,
                            cache: None,
                            reused: false,
                        },
                        emit,
                        &mut outcomes,
                    );
                    continue;
                }
                Err(e) => {
                    let line = line(
                        "error",
                        &dependent,
                        "",
                        format!("cannot prepare checkout: {}", clip(&e.to_string(), 140)),
                    );
                    report(
                        Outcome {
                            class: Class::Error,
                            line,
                            cache: None,
                            reused: false,
                        },
                        emit,
                        &mut outcomes,
                    );
                    continue;
                }
            };
            let submitted = drive
                .submitter
                .submit(&dependent)
                .unwrap_or_else(|e| Submitted::Failed(e.to_string()));
            let (number, reused) = match submitted {
                Submitted::Run(number) => (number, false),
                Submitted::Existing(number) => (number, true),
                Submitted::Deferred(reason) => {
                    // The host refused admission: wait for capacity instead of failing.
                    queue.push_front(dependent);
                    if active.is_empty() {
                        emit(&format!("waiting: {}", clip(&reason, 140)));
                        drive.clock.sleep(RETRY);
                    }
                    break;
                }
                Submitted::Failed(reason) => {
                    let line = line(
                        "error",
                        &dependent,
                        &commit,
                        format!("submission failed: {}", clip(&reason, 140)),
                    );
                    report(
                        Outcome {
                            class: Class::Error,
                            line,
                            cache: None,
                            reused: false,
                        },
                        emit,
                        &mut outcomes,
                    );
                    continue;
                }
            };
            let entry = Entry {
                commit,
                repo_id: dependent.repo_id,
                number,
                reused,
            };
            record
                .runs
                .insert(dependent.full_name.clone(), entry.clone());
            persist(record)?;
            active.push(Active { dependent, entry });
        }
        if active.is_empty() && queue.is_empty() {
            break;
        }
        if drive.clock.elapsed() >= drive.wait {
            for a in active.drain(..) {
                let line = line(
                    "timeout",
                    &a.dependent,
                    &a.entry.commit,
                    format!(
                        "{}#{} still running; crow-ci digest {} {}",
                        a.entry.repo_id, a.entry.number, a.entry.repo_id, a.entry.number
                    ),
                );
                report(
                    Outcome {
                        class: Class::Timeout,
                        line,
                        cache: None,
                        reused: a.entry.reused,
                    },
                    emit,
                    &mut outcomes,
                );
            }
            for d in queue.drain(..) {
                let line = line(
                    "timeout",
                    &d,
                    "",
                    "never submitted: waiting time exhausted".into(),
                );
                report(
                    Outcome {
                        class: Class::Timeout,
                        line,
                        cache: None,
                        reused: false,
                    },
                    emit,
                    &mut outcomes,
                );
            }
            break;
        }
        let mut progressed = false;
        let mut still = vec![];
        for a in active.drain(..) {
            let run = drive.api.call(
                &format!("/repos/{}/pipelines/{}", a.entry.repo_id, a.entry.number),
                None,
            );
            match run {
                Ok(run) if core::TERMINAL.contains(&text(&run, "status").as_str()) => {
                    progressed = true;
                    let outcome = finish(
                        drive.api,
                        drive.redact,
                        &a.dependent,
                        &drive.landed,
                        &a.entry,
                        &run,
                    );
                    report(outcome, emit, &mut outcomes);
                }
                _ => still.push(a),
            }
        }
        active = still;
        if !progressed && !active.is_empty() {
            drive.clock.sleep(POLL);
        }
    }
    Ok(outcomes)
}
/// Final summary line.
pub(super) fn summary(
    identity: &str,
    landed: &str,
    outcomes: &[Outcome],
    wall: Duration,
    scan: &forge_scan::Scan,
) -> String {
    let count = |c: Class| outcomes.iter().filter(|o| o.class == c).count();
    let (hits, requests) = outcomes
        .iter()
        .filter_map(|o| o.cache)
        .fold((0, 0), |(h, r), s| (h + s.hits, r + s.requests));
    let rate = (hits * 100)
        .checked_div(requests)
        .map_or("result cache n/a".to_string(), |percent| {
            format!("result cache {hits}/{requests} ({percent}%)")
        });
    format!(
        "summary {} @{}: {} dependents, {} green, {} red, {} infra, {} error, {} timeout, {} skipped; {} reused existing runs; {rate}; wall {}s; scan {} repositories ({} blobs fetched, {} unreadable)",
        identity.trim_start_matches("https://"),
        digest::short(landed),
        outcomes.len(),
        count(Class::Green),
        count(Class::Red),
        count(Class::Infra),
        count(Class::Error),
        count(Class::Timeout),
        count(Class::Skipped),
        outcomes.iter().filter(|o| o.reused).count(),
        wall.as_secs(),
        scan.scanned,
        scan.fetched,
        scan.unreadable.len(),
    )
}

/// Interpret one child `ci-job run` result.
pub(super) fn classify(success: bool, stdout: &str, stderr: &str) -> Submitted {
    let events: Vec<Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let message = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if success {
        let number = events.iter().rev().find_map(|v| {
            [&v["number"], &v["run"]["number"]]
                .iter()
                .find_map(|n| n.as_u64().filter(|n| *n > 0))
        });
        return number.map_or_else(
            || Submitted::Failed("submitted, but no run number was reported".into()),
            Submitted::Run,
        );
    }
    if ["MiB available", "full memory pressure"]
        .iter()
        .any(|m| message.contains(m))
    {
        return Submitted::Deferred(message.to_string());
    }
    if let Some(number) = events
        .iter()
        .rev()
        .find_map(|v| v["previous"]["number"].as_u64().filter(|n| *n > 0))
    {
        return Submitted::Existing(number);
    }
    Submitted::Failed(if message.is_empty() {
        "ci-job run failed".into()
    } else {
        message.to_string()
    })
}

struct Child<'a> {
    config_path: &'a Path,
    job: &'a str,
    admission_wait: u64,
    root: PathBuf,
}
impl Child<'_> {
    fn checkout(&self, dependent: &Dependent) -> Result<PathBuf> {
        if !matches(
            r"^[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*$",
            &dependent.full_name,
        ) {
            return Err("Invalid repository name".into());
        }
        Ok(self.root.join(&dependent.full_name))
    }
}
impl Submitter for Child<'_> {
    fn prepare(&self, dependent: &Dependent) -> Result<Prepared> {
        let dir = self.checkout(dependent)?;
        if !dir.join(".git").exists() {
            fs::create_dir_all(dir.parent().ok_or("Checkout needs a parent directory")?)?;
            output(
                &strings(&[
                    "git",
                    "clone",
                    "--quiet",
                    "--no-tags",
                    &dependent.clone_url,
                    &dir.to_string_lossy(),
                ]),
                None,
                None,
            )?;
        }
        git(
            &dir,
            &["fetch", "--quiet", "--no-tags", "origin", &dependent.branch],
        )?;
        git(
            &dir,
            &["checkout", "--quiet", "--force", "--detach", "FETCH_HEAD"],
        )?;
        git(&dir, &["clean", "-fdxq"])?;
        if dir.join(".gitmodules").exists() {
            git(
                &dir,
                &["submodule", "update", "--init", "--recursive", "--quiet"],
            )?;
        }
        let manifest = match git(&dir, &["show", "HEAD:.ci/ccid.toml"]) {
            Ok(manifest) => manifest,
            Err(_) => return Ok(Prepared::Skip("no .ci/ccid.toml".into())),
        };
        let declared = toml::from_str::<toml::Value>(&manifest)
            .ok()
            .and_then(|m| m.get("jobs")?.get(self.job).cloned());
        if declared.is_none() {
            return Ok(Prepared::Skip(format!("no [jobs.{}] declared", self.job)));
        }
        Ok(Prepared::Ready(git(&dir, &["rev-parse", "HEAD^{commit}"])?))
    }
    fn submit(&self, dependent: &Dependent) -> Result<Submitted> {
        let dir = self.checkout(dependent)?;
        let result = Command::new(std::env::current_exe()?)
            .arg("--submission-config")
            .arg(self.config_path)
            .args(["ci-job", "run", "--repo"])
            .arg(&dir)
            .args(["--branch", &dependent.branch, "--job", self.job])
            .arg("--var")
            .arg(format!("CI_ADMISSION_WAIT_SECONDS={}", self.admission_wait))
            .stdin(Stdio::null())
            .output()?;
        Ok(classify(
            result.status.success(),
            &String::from_utf8_lossy(&result.stdout),
            &String::from_utf8_lossy(&result.stderr),
        ))
    }
}

pub(super) fn run(config: &Config, config_path: &Path, args: &DependentsArgs) -> Result<()> {
    let clock = Wall::start();
    let repo = args.repo.canonicalize()?;
    let identity = config.origin(&repo)?;
    let landed = match &args.commit {
        Some(commit) if exact_sha(commit) => commit.clone(),
        Some(_) => return Err("--commit must be an exact 40-hex commit".into()),
        None => core::source_identity(&repo, &args.branch, false)?.0,
    };
    let crow = core::Crow::new(config)?;
    let records = core::pages(&crow, "/repos?active=true")?;
    let first = records
        .first()
        .ok_or("Crow reports no active repositories")?;
    let token = if config.forge_token_command.is_empty() {
        None
    } else {
        Some(
            String::from_utf8(output(&config.forge_token_command, None, None)?)?
                .trim()
                .to_owned(),
        )
    };
    let forge = Forge::new(Forge::root(&text(first, "clone_url"))?, token);
    let directory = config.directory("crow-ci-dependents")?;
    let cache_path = directory.join("manifest-references.json");
    let mut cache = Cache::load(&cache_path);
    let matcher = Matcher {
        identity: &identity,
        aliases: &config.origin_aliases,
        legacy_hosts: &config.legacy_hosts,
    };
    let scan = forge_scan::scan(&forge, &records, &matcher, &mut cache);
    cache.save(&cache_path)?;
    for note in &scan.unreadable {
        println!("note: unreadable {note}");
    }
    let dependents: Vec<Dependent> = scan
        .dependents
        .iter()
        .map(|f| Dependent {
            full_name: text(&f.record, "full_name"),
            repo_id: number(&f.record, "id"),
            clone_url: text(&f.record, "clone_url"),
            branch: match text(&f.record, "default_branch").as_str() {
                "" => "main".into(),
                b => b.into(),
            },
            files: f.files.clone(),
            pins: f.pins.clone(),
        })
        .collect();
    println!(
        "dependents of {} @{}: {}",
        identity.trim_start_matches("https://"),
        digest::short(&landed),
        if dependents.is_empty() {
            "none".into()
        } else {
            dependents
                .iter()
                .map(|d| format!("{} ({})", d.full_name, d.files.join("+")))
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    if args.plan || dependents.is_empty() {
        return Ok(());
    }
    let key = format!("{}-{landed}", &sha(&identity)[..16]);
    let _lock = lock(&directory.join(format!("{key}.lock")))?;
    let record_path = directory.join(format!("{key}.json"));
    let mut record: Record = fs::read(&record_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| Record {
            identity: identity.clone(),
            landed: landed.clone(),
            runs: BTreeMap::new(),
        });
    let submitter = Child {
        config_path,
        job: &args.job,
        admission_wait: args.admission_wait,
        root: directory.join("checkouts"),
    };
    let redact = |text: &str| crow.redact(text).unwrap_or_default();
    let mut wall = clock;
    let outcomes = {
        let mut context = Drive {
            api: &crow,
            redact: &redact,
            submitter: &submitter,
            clock: &mut wall,
            slots: usize::from(args.slots.min(8)),
            wait: Duration::from_secs(args.wait),
            landed: landed.clone(),
        };
        drive(
            &mut context,
            dependents,
            &mut record,
            &mut |r| save(&record_path, &serde_json::to_value(r)?),
            &mut |line| println!("{line}"),
        )?
    };
    println!(
        "{}",
        summary(&identity, &landed, &outcomes, wall.elapsed(), &scan)
    );
    if outcomes
        .iter()
        .all(|o| matches!(o.class, Class::Green | Class::Skipped))
    {
        Ok(())
    } else {
        Err("Some dependents are not green; see the lines above".into())
    }
}

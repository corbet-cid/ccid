//! Dependent discovery and the post-landing check driver.
use super::failure_digest::{entries, pipeline};
use super::*;
use crate::submission::dependents::{
    self, Class, Clock, Dependent, Drive, Prepared, Record, Submitted, Submitter,
};
use crate::submission::forge_scan::{self, Cache, HttpStatus, Matcher};
use base64::Engine;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    time::Duration,
};

const CARGO_LOCK: &str = include_str!("fixtures/manifest-cargo-lock.txt");
const CARGO_TOML: &str = include_str!("fixtures/manifest-cargo-toml.txt");
const FLAKE_LOCK: &str = include_str!("fixtures/manifest-flake-lock.json");
const FLAKE_NIX: &str = include_str!("fixtures/manifest-flake-nix.nix");
const GREEN: &str = include_str!("fixtures/green-cache-hit.log");
const QUOTA: &str = include_str!("fixtures/shell-trace-forge-quota.log");
const ASSERT: &str = include_str!("fixtures/cargo-test-failed-assert.log");

const LANDED: &str = "6e99d662f5620d69362ae4b17a3bd17293289dc6";
const IDENTITY: &str = "https://forge.example.invalid/example-libs/cgbl";

fn aliases() -> BTreeMap<String, String> {
    BTreeMap::from([(
        "alias.example.invalid".into(),
        "forge.example.invalid".into(),
    )])
}
fn matcher<'a>(
    identity: &'a str,
    aliases: &'a BTreeMap<String, String>,
    legacy: &'a [String],
) -> Matcher<'a> {
    Matcher {
        identity,
        aliases,
        legacy_hosts: legacy,
    }
}

#[test]
fn references_come_from_cargo_text_and_flake_lock_json() {
    let lock = forge_scan::references("Cargo.lock", CARGO_LOCK);
    assert!(lock.iter().any(|r| r
        == &format!(
            "git+https://forge.example.invalid/example-libs/cgbl.git?rev={LANDED}#{LANDED}"
        )));
    assert!(lock.iter().all(|r| !r.starts_with("registry")), "{lock:?}");
    let toml = forge_scan::references("Cargo.toml", CARGO_TOML);
    assert!(toml.contains(&"https://forge.example.invalid/example-libs/cgbl.git".to_string()));
    let flake = forge_scan::references("flake.lock", FLAKE_LOCK);
    assert!(flake.iter().any(|r| {
        r.starts_with("https://forge.example.invalid/example-nix/nixscroll.git#8c1310a2")
    }));
    assert!(flake.contains(&"https://forge.example.invalid/example-nix/nixscroll.git".to_string()));
    let nix = forge_scan::references("flake.nix", FLAKE_NIX);
    assert!(nix.contains(
        &"git+https://forge.example.invalid/example-nix/nixscroll.git?allRefs=1".to_string()
    ));
    assert!(
        forge_scan::references("flake.lock", "not json \"https://a.invalid/x/y\"")
            .contains(&"https://a.invalid/x/y".to_string())
    );
}

#[test]
fn matching_follows_the_canonical_identity_and_reports_exact_pins() {
    let aliases = aliases();
    let legacy = vec!["legacy.example.invalid".to_string()];
    let m = matcher(IDENTITY, &aliases, &legacy);
    let locked =
        format!("git+https://forge.example.invalid/example-libs/cgbl.git?rev={LANDED}#{LANDED}");
    assert_eq!(m.pin(&locked), Some(Some(LANDED.to_string())));
    assert_eq!(
        m.pin("https://forge.example.invalid/example-libs/cgbl.git"),
        Some(None)
    );
    assert_eq!(
        m.pin("git+https://alias.example.invalid/example-libs/cgbl?branch=main"),
        Some(None)
    );
    assert_eq!(
        m.pin("https://forge.example.invalid/example-libs/cgbl/archive/abc.tar.gz"),
        Some(None)
    );
    assert_eq!(
        m.pin("https://forge.example.invalid/example-libs/cgbl-other.git"),
        None
    );
    assert_eq!(
        m.pin("https://forge.example.invalid/other-org/cgbl.git"),
        None
    );
    assert_eq!(
        m.pin("https://legacy.example.invalid/example-foss/cgbl"),
        Some(None)
    );
    assert_eq!(
        m.pin("https://elsewhere.example.invalid/example-foss/cgbl"),
        None
    );
    assert_eq!(
        m.pin("registry+https://legacy.example.invalid/example-foss/cgbl"),
        None
    );
    let strict = matcher(IDENTITY, &aliases, &[]);
    assert_eq!(
        strict.pin("https://legacy.example.invalid/example-foss/cgbl"),
        None
    );
}

struct FakeForge {
    responses: HashMap<String, std::result::Result<Value, u16>>,
    calls: RefCell<Vec<String>>,
}
impl Api for FakeForge {
    fn call(&self, path: &str, _: Option<&Value>) -> Result<Value> {
        self.calls.borrow_mut().push(path.to_string());
        match self.responses.get(path) {
            Some(Ok(value)) => Ok(value.clone()),
            Some(Err(code)) => Err(Box::new(HttpStatus(*code))),
            None => Err(Box::new(HttpStatus(404))),
        }
    }
}
fn blob(content: &str) -> Value {
    let packed = base64::engine::general_purpose::STANDARD.encode(content);
    let wrapped: Vec<&str> = packed
        .as_bytes()
        .chunks(60)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    json!({"encoding":"base64","content":wrapped.join("\n")})
}
fn record(id: u64, full_name: &str) -> Value {
    json!({"id":id,"active":true,"full_name":full_name,"default_branch":"main",
        "clone_url":format!("https://forge.example.invalid/{full_name}.git")})
}
fn forge() -> FakeForge {
    let sha = |c: char| c.to_string().repeat(40);
    let tree = |files: &[(&str, char)]| json!({"tree":files.iter().map(|(p, c)| json!({"path":p,"type":"blob","sha":sha(*c)})).collect::<Vec<_>>()});
    FakeForge {
        responses: HashMap::from([
            (
                "/repos/example-libs/cmty/git/trees/main".into(),
                Ok(tree(&[
                    ("Cargo.lock", 'a'),
                    ("Cargo.toml", 'b'),
                    ("README.md", 'f'),
                ])),
            ),
            (
                format!("/repos/example-libs/cmty/git/blobs/{}", sha('a')),
                Ok(blob(CARGO_LOCK)),
            ),
            (
                format!("/repos/example-libs/cmty/git/blobs/{}", sha('b')),
                Ok(blob(CARGO_TOML)),
            ),
            (
                "/repos/example-nix/nixlaunch/git/trees/main".into(),
                Ok(tree(&[("flake.lock", 'c'), ("flake.nix", 'd')])),
            ),
            (
                format!("/repos/example-nix/nixlaunch/git/blobs/{}", sha('c')),
                Ok(blob(FLAKE_LOCK)),
            ),
            (
                format!("/repos/example-nix/nixlaunch/git/blobs/{}", sha('d')),
                Ok(blob(FLAKE_NIX)),
            ),
            ("/repos/example-libs/empty/git/trees/main".into(), Err(409)),
            ("/repos/example-libs/broken/git/trees/main".into(), Err(500)),
        ]),
        calls: RefCell::new(vec![]),
    }
}
fn records() -> Vec<Value> {
    vec![
        record(1, "example-libs/cgbl"),
        record(2, "example-libs/cmty"),
        record(3, "example-nix/nixlaunch"),
        record(4, "example-libs/empty"),
        record(5, "example-libs/broken"),
        json!({"id":6,"active":false,"full_name":"example-libs/off","clone_url":"https://forge.example.invalid/example-libs/off.git"}),
    ]
}

#[test]
fn scan_finds_dependents_skips_self_and_degrades_on_errors() {
    let aliases = aliases();
    let forge = forge();
    let mut cache = Cache::default();
    let scan = forge_scan::scan(
        &forge,
        &records(),
        &matcher(IDENTITY, &aliases, &[]),
        &mut cache,
    );
    assert_eq!(scan.dependents.len(), 1);
    let found = &scan.dependents[0];
    assert_eq!(text(&found.record, "full_name"), "example-libs/cmty");
    assert_eq!(found.files, vec!["Cargo.lock", "Cargo.toml"]);
    assert_eq!(found.pins, BTreeSet::from([LANDED.to_string()]));
    assert_eq!(
        scan.scanned, 4,
        "self and inactive repositories are not scanned"
    );
    assert_eq!(scan.unreadable.len(), 1);
    assert!(
        scan.unreadable[0].starts_with("example-libs/broken"),
        "{:?}",
        scan.unreadable
    );
    assert_eq!(scan.fetched, 4);
    assert!(!forge.calls.borrow().iter().any(|c| c.contains("README")));
}

#[test]
fn flake_inputs_are_dependents_too_and_a_warm_scan_fetches_no_blob() {
    let aliases = aliases();
    let forge = forge();
    let mut cache = Cache::default();
    let nix = "https://forge.example.invalid/example-nix/nixscroll";
    let cold = forge_scan::scan(&forge, &records(), &matcher(nix, &aliases, &[]), &mut cache);
    assert_eq!(cold.dependents.len(), 1);
    assert_eq!(cold.dependents[0].files, vec!["flake.lock", "flake.nix"]);
    assert!(cold.dependents[0]
        .pins
        .iter()
        .any(|p| p.starts_with("8c1310a2")));
    let before = forge.calls.borrow().len();
    let warm = forge_scan::scan(&forge, &records(), &matcher(nix, &aliases, &[]), &mut cache);
    assert_eq!(warm.fetched, 0);
    assert_eq!(warm.dependents.len(), 1);
    let blobs = forge.calls.borrow()[before..]
        .iter()
        .filter(|c| c.contains("/blobs/"))
        .count();
    assert_eq!(blobs, 0, "cached references must not be fetched again");
    // The cache round-trips through its file.
    let dir = tempfile::tempdir().unwrap();
    cache.save(&dir.path().join("refs.json")).unwrap();
    let mut reloaded = Cache::load(&dir.path().join("refs.json"));
    let again = forge_scan::scan(
        &forge,
        &records(),
        &matcher(nix, &aliases, &[]),
        &mut reloaded,
    );
    assert_eq!(again.fetched, 0);
    Cache::load(&dir.path().join("missing.json"))
        .save(&dir.path().join("x.json"))
        .unwrap();
}

#[test]
fn forge_root_comes_from_the_clone_url() {
    assert_eq!(
        forge_scan::Forge::root("https://forge.example.invalid/o/r.git").unwrap(),
        "https://forge.example.invalid/api/v1"
    );
    assert!(forge_scan::Forge::root("http://forge.example.invalid/o/r.git").is_err());
}

#[test]
fn child_results_are_classified() {
    let submitted = "{\"action\":\"submitted\",\"run\":{\"number\":17,\"status\":\"pending\"}}\n{\"attempt\":1,\"number\":17,\"repo_id\":284,\"scheduler\":\"crow\"}\n";
    assert!(matches!(
        dependents::classify(true, submitted, ""),
        Submitted::Run(17)
    ));
    let attached = "{\"action\":\"attached\",\"repo_id\":284,\"run\":{\"number\":9}}\n{\"attempt\":1,\"repo_id\":284}\n";
    assert!(matches!(
        dependents::classify(true, attached, ""),
        Submitted::Run(9)
    ));
    assert!(matches!(
        dependents::classify(true, "", ""),
        Submitted::Failed(_)
    ));
    let existing = "{\"phase\":\"success\",\"previous\":{\"number\":12,\"status\":\"success\"}}\n";
    assert!(matches!(
        dependents::classify(
            false,
            existing,
            "ccid: Matching completed job exists; inspect result or use --rerun\n"
        ),
        Submitted::Existing(12)
    ));
    let busy = "Host has 4096 MiB available; 8192 MiB required before new CI\n";
    assert!(matches!(
        dependents::classify(false, "", busy),
        Submitted::Deferred(_)
    ));
    match dependents::classify(
        false,
        "",
        "\nccid: Select committed job with plain Crow workflow name\n",
    ) {
        Submitted::Failed(why) => assert!(why.contains("Select committed job")),
        _ => panic!("expected failure"),
    }
}

struct Wall {
    now: Cell<Duration>,
}
impl Clock for Wall {
    fn elapsed(&self) -> Duration {
        self.now.get()
    }
    fn sleep(&mut self, duration: Duration) {
        self.now.set(self.now.get() + duration);
    }
}

/// Fake Crow: a run finishes after `polls` status requests.
struct Crow {
    polls: RefCell<HashMap<(u64, u64), u32>>,
    done_after: u32,
    failing: HashMap<u64, &'static str>,
    live: Cell<usize>,
    peak: Cell<usize>,
}
impl Api for Crow {
    fn call(&self, path: &str, _: Option<&Value>) -> Result<Value> {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let (repo, number): (u64, u64) = (parts[1].parse()?, parts[3].parse()?);
        match parts[2] {
            "pipelines" => {
                let mut polls = self.polls.borrow_mut();
                let seen = polls.entry((repo, number)).or_insert(0);
                *seen += 1;
                let done = *seen > self.done_after;
                if *seen == self.done_after + 1 {
                    self.live.set(self.live.get().saturating_sub(1));
                }
                let mut run = if !done {
                    pipeline("running", &[(11, "repository-job", "running", 0)])
                } else if self.failing.contains_key(&repo) {
                    pipeline("failure", &[(12, "repository-job", "failure", 2)])
                } else {
                    pipeline("success", &[(11, "repository-job", "success", 0)])
                };
                run["number"] = json!(number);
                Ok(run)
            }
            "logs" => Ok(entries(match self.failing.get(&repo) {
                Some(&"infra") => QUOTA,
                Some(_) => ASSERT,
                None => GREEN,
            })),
            _ => Err("unexpected".into()),
        }
    }
}
struct Fixture<'a> {
    crow: &'a Crow,
    submitted: RefCell<Vec<String>>,
    defer: Cell<u32>,
    existing: Vec<&'static str>,
    skip: Vec<&'static str>,
}
impl Submitter for Fixture<'_> {
    fn prepare(&self, dependent: &Dependent) -> Result<Prepared> {
        if self.skip.contains(&dependent.full_name.as_str()) {
            return Ok(Prepared::Skip("no [jobs.verify] declared".into()));
        }
        Ok(Prepared::Ready(format!("{:040x}", dependent.repo_id)))
    }
    fn submit(&self, dependent: &Dependent) -> Result<Submitted> {
        if self.defer.get() > 0 {
            self.defer.set(self.defer.get() - 1);
            return Ok(Submitted::Deferred(
                "Host has 1 MiB available; 8192 MiB required".into(),
            ));
        }
        self.crow.live.set(self.crow.live.get() + 1);
        self.crow
            .peak
            .set(self.crow.peak.get().max(self.crow.live.get()));
        self.submitted
            .borrow_mut()
            .push(dependent.full_name.clone());
        let number = 1000 + dependent.repo_id;
        Ok(if self.existing.contains(&dependent.full_name.as_str()) {
            Submitted::Existing(number)
        } else {
            Submitted::Run(number)
        })
    }
}
fn fixture(crow: &Crow) -> Fixture<'_> {
    Fixture {
        crow,
        submitted: RefCell::new(vec![]),
        defer: Cell::new(0),
        existing: vec![],
        skip: vec![],
    }
}
fn dependent(i: u64) -> Dependent {
    Dependent {
        full_name: format!("example-libs/d{i:02}"),
        repo_id: 100 + i,
        clone_url: format!("https://forge.example.invalid/example-libs/d{i:02}.git"),
        branch: "main".into(),
        files: vec!["Cargo.lock".into()],
        pins: BTreeSet::from([LANDED.to_string()]),
    }
}
fn crow(done_after: u32, failing: &[(u64, &'static str)]) -> Crow {
    Crow {
        polls: RefCell::new(HashMap::new()),
        done_after,
        failing: failing.iter().copied().collect(),
        live: Cell::new(0),
        peak: Cell::new(0),
    }
}
struct Run {
    outcomes: Vec<dependents::Outcome>,
    lines: Vec<String>,
    record: Record,
    elapsed: Duration,
}
fn drive(
    crow: &Crow,
    submitter: &Fixture<'_>,
    count: u64,
    slots: usize,
    wait: u64,
    record: Record,
) -> Run {
    let mut clock = Wall {
        now: Cell::new(Duration::ZERO),
    };
    let mut record = record;
    let mut lines = vec![];
    let outcomes = {
        let identity = |t: &str| t.to_string();
        let mut context = Drive {
            api: crow,
            redact: &identity,
            submitter,
            clock: &mut clock,
            slots,
            wait: Duration::from_secs(wait),
            landed: LANDED.to_string(),
        };
        dependents::drive(
            &mut context,
            (1..=count).map(dependent).collect(),
            &mut record,
            &mut |_| Ok(()),
            &mut |line| lines.push(line.to_string()),
        )
        .unwrap()
    };
    Run {
        outcomes,
        lines,
        record,
        elapsed: clock.now.get(),
    }
}

#[test]
fn never_more_runs_in_flight_than_the_slots() {
    let crow = crow(2, &[]);
    let submitter = fixture(&crow);
    let run = drive(&crow, &submitter, 10, 3, 3600, Record::default());
    assert_eq!(run.outcomes.len(), 10);
    assert!(run.outcomes.iter().all(|o| o.class == Class::Green));
    assert!(crow.peak.get() <= 3, "peak {}", crow.peak.get());
    assert_eq!(crow.peak.get(), 3);
    assert_eq!(submitter.submitted.borrow().len(), 10);
    assert_eq!(run.record.runs.len(), 10);
    assert!(
        run.lines[0].starts_with("green  example-libs/d"),
        "{:?}",
        run.lines[0]
    );
    assert!(
        run.lines[0].contains("pin=landed") && run.lines[0].contains("cache 1/1"),
        "{:?}",
        run.lines[0]
    );
}
#[test]
fn a_dependent_recorded_for_this_landing_is_never_submitted_again() {
    let crow = crow(1, &[]);
    let submitter = fixture(&crow);
    let mut record = Record {
        identity: IDENTITY.into(),
        landed: LANDED.into(),
        runs: BTreeMap::new(),
    };
    for i in [2u64, 5] {
        record.runs.insert(
            format!("example-libs/d{i:02}"),
            dependents::Entry {
                commit: format!("{:040x}", 100 + i),
                repo_id: 100 + i,
                number: 1100 + i,
                reused: false,
            },
        );
    }
    let run = drive(&crow, &submitter, 6, 8, 3600, record);
    assert_eq!(run.outcomes.len(), 6);
    let sent = submitter.submitted.borrow();
    assert_eq!(sent.len(), 4);
    assert!(
        !sent.contains(&"example-libs/d02".to_string())
            && !sent.contains(&"example-libs/d05".to_string())
    );
    assert!(run
        .lines
        .iter()
        .any(|l| l.contains("example-libs/d02") && l.contains("102#1102")));
}

#[test]
fn host_admission_refusals_wait_instead_of_failing() {
    let crow = crow(1, &[]);
    let submitter = fixture(&crow);
    submitter.defer.set(2);
    let run = drive(&crow, &submitter, 1, 8, 3600, Record::default());
    assert_eq!(run.outcomes[0].class, Class::Green);
    assert!(run.elapsed >= Duration::from_secs(60), "{:?}", run.elapsed);
    assert!(run
        .lines
        .iter()
        .any(|l| l.starts_with("waiting: Host has 1 MiB")));
    assert_eq!(submitter.submitted.borrow().len(), 1);
}

#[test]
fn waiting_time_is_bounded_and_unfinished_work_is_reported() {
    let crow = crow(1_000_000, &[]);
    let submitter = fixture(&crow);
    let run = drive(&crow, &submitter, 4, 2, 60, Record::default());
    assert_eq!(run.outcomes.len(), 4);
    assert!(run.outcomes.iter().all(|o| o.class == Class::Timeout));
    assert!(run
        .lines
        .iter()
        .any(|l| l.contains("still running; crow-ci digest 101 1101")));
    assert!(run.lines.iter().any(|l| l.contains("never submitted")));
    assert!(run.elapsed < Duration::from_secs(120));
}

#[test]
fn red_and_infrastructure_results_point_at_the_digest() {
    let crow = crow(0, &[(102, "infra"), (103, "red")]);
    let submitter = fixture(&crow);
    let run = drive(&crow, &submitter, 3, 8, 3600, Record::default());
    let by = |name: &str| run.lines.iter().find(|l| l.contains(name)).unwrap().clone();
    assert!(by("d01").starts_with("green"));
    let infra = by("d02");
    assert!(
        infra.starts_with("infra  ") && infra.contains("rate limit (429/1027)"),
        "{infra}"
    );
    assert!(infra.ends_with("-> crow-ci digest 102 1102"), "{infra}");
    let red = by("d03");
    assert!(
        red.starts_with("red    ") && red.contains("failed tests: 1 failed: declared_placement"),
        "{red}"
    );
    assert!(red.contains("-> crow-ci digest 103 1103"), "{red}");
    let classes: Vec<_> = run.outcomes.iter().map(|o| o.class).collect();
    assert!(classes.contains(&Class::Infra) && classes.contains(&Class::Red));
}

#[test]
fn existing_runs_are_reused_and_unsubmittable_repositories_are_skipped() {
    let crow = crow(0, &[]);
    let mut submitter = fixture(&crow);
    submitter.existing = vec!["example-libs/d01"];
    submitter.skip = vec!["example-libs/d02"];
    let run = drive(&crow, &submitter, 3, 8, 3600, Record::default());
    assert!(run.outcomes.iter().any(|o| o.class == Class::Skipped));
    let reused = run
        .lines
        .iter()
        .find(|l| l.contains("example-libs/d01"))
        .unwrap();
    assert!(reused.ends_with("(existing run)"), "{reused}");
    assert!(run.record.runs["example-libs/d01"].reused);
    assert!(run
        .lines
        .iter()
        .any(|l| l.starts_with("skip") && l.contains("no [jobs.verify] declared")));
    let scan = forge_scan::Scan {
        scanned: 190,
        fetched: 4,
        ..Default::default()
    };
    let summary = dependents::summary(
        IDENTITY,
        LANDED,
        &run.outcomes,
        Duration::from_secs(41),
        &scan,
    );
    assert!(
        summary.contains("3 dependents, 2 green, 0 red, 0 infra, 0 error, 0 timeout, 1 skipped"),
        "{summary}"
    );
    assert!(
        summary.contains("1 reused existing runs; result cache 2/2 (100%)"),
        "{summary}"
    );
    assert!(
        summary.contains("wall 41s")
            && summary.contains("scan 190 repositories (4 blobs fetched, 0 unreadable)"),
        "{summary}"
    );
    assert!(
        summary.starts_with("summary forge.example.invalid/example-libs/cgbl @6e99d662:"),
        "{summary}"
    );
}

//! The fleet rollout: gate, skips, ordering, resume.
use super::*;
use crate::submission::rollout::{
    self, drive, matches_pattern, parse_manifest, pin_manifest, switch_policy_document,
    Config as Plan, Fleet, Manifest, Repin, Repo, RepoState, Run, Skip, State, Switch, Verdict,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Mutex,
    time::Duration,
};

const REVISION: &str = "1111111111111111111111111111111111111111";
const OLD: &str = "2222222222222222222222222222222222222222";
const SUPPORTED: &str = "3333333333333333333333333333333333333333";

fn plan() -> Plan {
    Plan {
        schema: 1,
        supports_from: "0a610405e1e8dbd4f917b4e80d29b2b6d21fc7b0".into(),
        skip: vec![
            Skip {
                pattern: "corbet-nix/*".into(),
                reason: "Nix lane".into(),
            },
            Skip {
                pattern: "corbet-libs/cfrg".into(),
                reason: "cfrg lane".into(),
            },
        ],
    }
}
fn repo(name: &str) -> Repo {
    Repo {
        full_name: name.into(),
        clone_url: format!("https://forge.example.invalid/{name}.git"),
    }
}
fn manifest(pin: &str, gating: &[&str]) -> Manifest {
    Manifest {
        pin: pin.into(),
        gating: gating.iter().map(|s| (*s).to_owned()).collect(),
    }
}

struct Fake {
    manifests: Mutex<HashMap<String, Option<Manifest>>>,
    calls: Mutex<Vec<String>>,
    verdicts: Mutex<HashMap<String, VecDeque<Option<Verdict>>>>,
    stuck: HashSet<String>,
    policy: Mutex<Value>,
}
impl Fake {
    fn new(entries: &[(&str, Option<Manifest>)]) -> Self {
        Self {
            manifests: Mutex::new(
                entries
                    .iter()
                    .map(|(n, m)| ((*n).to_owned(), m.clone()))
                    .collect(),
            ),
            calls: Mutex::new(vec![]),
            verdicts: Mutex::new(HashMap::new()),
            stuck: HashSet::new(),
            policy: Mutex::new(
                json!({"contexts":["ci/crow/*"],"repositories":[{"path":"corbet-libs/cfrg"}]}),
            ),
        }
    }
    fn log(&self, what: &str, repo: &Repo) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{what} {}", repo.full_name));
    }
    fn called(&self, what: &str, name: &str) -> bool {
        self.calls
            .lock()
            .unwrap()
            .contains(&format!("{what} {name}"))
    }
    fn order(&self, name: &str) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.ends_with(&format!(" {name}")))
            .map(|c| c.split(' ').next().unwrap().to_owned())
            .collect()
    }
}
impl Fleet for Fake {
    fn manifest(&self, repo: &Repo) -> Result<Option<Manifest>> {
        Ok(self
            .manifests
            .lock()
            .unwrap()
            .get(&repo.full_name)
            .cloned()
            .flatten())
    }
    fn supports(&self, revision: &str) -> bool {
        revision == SUPPORTED || revision == REVISION
    }
    fn repin(&self, repo: &Repo, revision: &str) -> Result<Repin> {
        self.log("repin", repo);
        if !self.stuck.contains(&repo.full_name) {
            // The pin moves when the branch lands, not now.
            let _ = revision;
        }
        Ok(Repin::Pushed)
    }
    fn land(&self, repo: &Repo) -> Result<()> {
        self.log("land", repo);
        if !self.stuck.contains(&repo.full_name) {
            let mut manifests = self.manifests.lock().unwrap();
            if let Some(Some(m)) = manifests.get_mut(&repo.full_name) {
                m.pin = REVISION.into();
            }
        }
        Ok(())
    }
    fn head(&self, repo: &Repo) -> Result<String> {
        Ok(format!("{:0>40}", repo.full_name.len()))
    }
    fn verdict(&self, repo: &Repo, _: &str) -> Result<Option<Verdict>> {
        self.log("verdict", repo);
        let mut verdicts = self.verdicts.lock().unwrap();
        Ok(match verdicts.get_mut(&repo.full_name) {
            Some(queue) => queue.pop_front().unwrap_or(None),
            None => Some(Verdict::Success),
        })
    }
    fn switch_policy(&self, repo: &Repo) -> Result<Switch> {
        self.log("switch", repo);
        switch_policy_document(&mut self.policy.lock().unwrap(), &repo.full_name)
    }
    fn sleep(&self, _: Duration) {}
}

fn walk(fake: &Fake, names: &[&str], state: &Mutex<State>, concurrency: usize) {
    let plan = plan();
    let repos: Vec<Repo> = names.iter().map(|n| repo(n)).collect();
    drive(
        &Run {
            fleet: fake,
            config: &plan,
            revision: REVISION,
            waits: 3,
            concurrency,
        },
        &repos,
        state,
        &|_| Ok(()),
        &|_| {},
    );
}
fn fresh() -> Mutex<State> {
    Mutex::new(State {
        revision: REVISION.into(),
        repos: BTreeMap::new(),
    })
}
fn status(state: &Mutex<State>, name: &str) -> String {
    state.lock().unwrap().repos[name].status.clone()
}
fn contexts(fake: &Fake, name: &str) -> Option<Value> {
    fake.policy.lock().unwrap()["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == name)
        .map(|e| e["contexts"].clone())
}

#[test]
fn the_policy_switches_only_after_repin_landing_and_a_green_verdict_in_that_order() {
    let fake = Fake::new(&[("org/app", Some(manifest(OLD, &["verify"])))]);
    let state = fresh();
    walk(&fake, &["org/app"], &state, 1);
    assert_eq!(status(&state, "org/app"), "done");
    assert_eq!(
        fake.order("org/app"),
        ["repin", "land", "verdict", "switch"]
    );
    assert_eq!(contexts(&fake, "org/app"), Some(json!(["ccid/verdict"])));
}

#[test]
fn no_green_verdict_means_no_policy_change_and_a_rerun_resumes() {
    let fake = Fake::new(&[("org/app", Some(manifest(OLD, &["verify"])))]);
    fake.verdicts.lock().unwrap().insert(
        "org/app".into(),
        VecDeque::from([None, Some(Verdict::Pending), None]),
    );
    let state = fresh();
    walk(&fake, &["org/app"], &state, 1);
    assert_eq!(status(&state, "org/app"), "awaiting");
    assert_eq!(contexts(&fake, "org/app"), None);
    assert!(!fake.called("switch", "org/app"));
    // The reporter comes up: the next run finds the verdict and finishes, with no second repin.
    fake.verdicts.lock().unwrap().clear();
    walk(&fake, &["org/app"], &state, 1);
    assert_eq!(status(&state, "org/app"), "done");
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("repin"))
            .count(),
        1
    );
    assert_eq!(contexts(&fake, "org/app"), Some(json!(["ccid/verdict"])));
}

#[test]
fn a_red_first_verdict_fails_the_repository_and_leaves_its_policy() {
    let fake = Fake::new(&[("org/app", Some(manifest(OLD, &["verify"])))]);
    fake.verdicts
        .lock()
        .unwrap()
        .insert("org/app".into(), VecDeque::from([Some(Verdict::Failure)]));
    let state = fresh();
    walk(&fake, &["org/app"], &state, 1);
    assert_eq!(status(&state, "org/app"), "failed");
    assert!(!fake.called("switch", "org/app"));
}

#[test]
fn a_landing_that_never_completes_stops_before_any_verdict_or_policy_step() {
    let mut fake = Fake::new(&[("org/red", Some(manifest(OLD, &["verify"])))]);
    fake.stuck.insert("org/red".into());
    let state = fresh();
    walk(&fake, &["org/red"], &state, 1);
    assert_eq!(status(&state, "org/red"), "awaiting");
    assert_eq!(fake.order("org/red"), ["repin", "land"]);
}

#[test]
fn other_lanes_are_skipped_unless_their_runtime_supports_the_verdict() {
    let fake = Fake::new(&[
        ("corbet-nix/old", Some(manifest(OLD, &["verify"]))),
        ("corbet-nix/new", Some(manifest(SUPPORTED, &["verify"]))),
        ("corbet-libs/cfrg", Some(manifest(OLD, &["verify"]))),
    ]);
    let state = fresh();
    walk(
        &fake,
        &["corbet-nix/old", "corbet-nix/new", "corbet-libs/cfrg"],
        &state,
        2,
    );
    assert_eq!(status(&state, "corbet-nix/old"), "skipped");
    assert_eq!(status(&state, "corbet-libs/cfrg"), "skipped");
    assert!(state.lock().unwrap().repos["corbet-nix/old"]
        .note
        .contains("Nix lane"));
    assert!(fake.order("corbet-nix/old").is_empty());
    // Supported runtime: not repinned, still gated on its own first verdict.
    assert_eq!(fake.order("corbet-nix/new"), ["verdict", "switch"]);
    assert_eq!(status(&state, "corbet-nix/new"), "done");
}

#[test]
fn repositories_without_adapters_or_a_gate_are_skipped_without_side_effects() {
    let fake = Fake::new(&[("org/docs", None), ("org/nogate", Some(manifest(OLD, &[])))]);
    let state = fresh();
    walk(&fake, &["org/docs", "org/nogate"], &state, 2);
    assert_eq!(status(&state, "org/docs"), "skipped");
    assert_eq!(status(&state, "org/nogate"), "skipped");
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn finished_repositories_are_never_touched_again() {
    let fake = Fake::new(&[("org/app", Some(manifest(OLD, &["verify"])))]);
    let state = fresh();
    state.lock().unwrap().repos.insert(
        "org/app".into(),
        RepoState {
            status: "done".into(),
            note: "earlier".into(),
        },
    );
    walk(&fake, &["org/app"], &state, 1);
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn many_repositories_are_each_processed_exactly_once_in_parallel() {
    let names: Vec<String> = (0..24).map(|i| format!("org/r{i:02}")).collect();
    let entries: Vec<(&str, Option<Manifest>)> = names
        .iter()
        .map(|n| (n.as_str(), Some(manifest(OLD, &["verify"]))))
        .collect();
    let fake = Fake::new(&entries);
    let state = fresh();
    walk(
        &fake,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
        &state,
        8,
    );
    for name in &names {
        assert_eq!(status(&state, name), "done");
        assert_eq!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| **c == format!("switch {name}"))
                .count(),
            1
        );
    }
    assert_eq!(
        fake.policy.lock().unwrap()["repositories"]
            .as_array()
            .unwrap()
            .len(),
        25
    );
}

#[test]
fn the_pin_is_replaced_in_place_and_nothing_else_changes() {
    let text = "schema = 1\n[render]\ntool_revision = \"aaaa\"\n[jobs.verify]\nchecks = []\n";
    let pinned = pin_manifest(text, REVISION).unwrap();
    assert_eq!(
        pinned,
        format!(
            "schema = 1\n[render]\ntool_revision = \"{REVISION}\"\n[jobs.verify]\nchecks = []\n"
        )
    );
    assert!(pin_manifest("schema = 1\n", REVISION).is_err());
    assert!(pin_manifest("tool_revision = \"a\"\ntool_revision = \"b\"\n", REVISION).is_err());
}

#[test]
fn the_manifest_yields_the_pin_and_the_gating_jobs() {
    let base = format!("[render]\ntool_revision = \"{OLD}\"\n[jobs.verify]\nchecks=[]\n[jobs.release]\nchecks=[]\n");
    assert_eq!(parse_manifest(&base), Some(manifest(OLD, &["verify"])));
    let declared = format!("{base}[verdict]\njobs = [\"verify\", \"release\"]\n");
    assert_eq!(
        parse_manifest(&declared),
        Some(manifest(OLD, &["verify", "release"]))
    );
    let no_verify = format!("[render]\ntool_revision = \"{OLD}\"\n[jobs.release]\nchecks=[]\n");
    assert_eq!(parse_manifest(&no_verify), Some(manifest(OLD, &[])));
    assert_eq!(
        parse_manifest("[render]\ntool_revision = \"x\"\n"),
        None,
        "no jobs: no adapters"
    );
    assert_eq!(parse_manifest("not toml ["), None);
}

#[test]
fn the_land_policy_edit_is_per_repository_and_respects_custom_contexts() {
    let mut policy = json!({"contexts":["ci/crow/*"],"repositories":[
        {"path":"a/default"},
        {"path":"a/old","contexts":["ci/crow/*"]},
        {"path":"a/done","contexts":["ccid/verdict"]},
        {"path":"a/custom","contexts":["ci/crow/manual/ccid"],"retest":["x"]}]});
    assert_eq!(
        switch_policy_document(&mut policy, "a/default").unwrap(),
        Switch::Switched
    );
    assert_eq!(
        switch_policy_document(&mut policy, "a/old").unwrap(),
        Switch::Switched
    );
    assert_eq!(
        switch_policy_document(&mut policy, "a/done").unwrap(),
        Switch::Already
    );
    assert_eq!(
        switch_policy_document(&mut policy, "a/custom").unwrap(),
        Switch::Custom
    );
    assert_eq!(
        switch_policy_document(&mut policy, "a/new").unwrap(),
        Switch::Switched
    );
    let entries = policy["repositories"].as_array().unwrap();
    assert_eq!(entries[0]["contexts"], json!(["ccid/verdict"]));
    assert_eq!(entries[1]["contexts"], json!(["ccid/verdict"]));
    assert_eq!(entries[3]["contexts"], json!(["ci/crow/manual/ccid"]));
    assert_eq!(entries[3]["retest"], json!(["x"]), "other fields survive");
    assert_eq!(
        entries[4],
        json!({"path":"a/new","contexts":["ccid/verdict"]})
    );
    assert_eq!(
        policy["contexts"],
        json!(["ci/crow/*"]),
        "the global default is untouched"
    );
    assert!(switch_policy_document(&mut json!({}), "a/x").is_err());
}

#[test]
fn skip_patterns_match_an_organization_or_one_repository() {
    assert!(matches_pattern("corbet-nix/*", "corbet-nix/anything"));
    assert!(!matches_pattern("corbet-nix/*", "corbet-nixish/x"));
    assert!(matches_pattern("corbet-libs/cfrg", "corbet-libs/cfrg"));
    assert!(!matches_pattern("corbet-libs/cfrg", "corbet-libs/cfrg2"));
}

#[test]
fn the_gate_opens_on_a_line_that_begins_with_the_marker_and_only_then() {
    let dir = tempfile::tempdir().unwrap();
    let lanes = dir.path().join("LANES.md");
    fs::write(
        &lanes,
        "ccid lane: wait for VERDICT-PROVISIONED before starting\n",
    )
    .unwrap();
    let slept = std::cell::Cell::new(0);
    let sleep = |_: Duration| {
        slept.set(slept.get() + 1);
        if slept.get() == 2 {
            fs::write(&lanes, "ccid lane: wait for VERDICT-PROVISIONED before starting\nVERDICT-PROVISIONED by infra: reporter live\n").unwrap();
        }
    };
    assert!(rollout::wait_for_marker(
        &lanes,
        10,
        Duration::from_secs(30),
        &sleep
    ));
    assert_eq!(
        slept.get(),
        2,
        "one blocking wait that returns the moment the line appears"
    );
    let never = dir.path().join("never.md");
    fs::write(&never, "prose mentioning VERDICT-PROVISIONED mid-line\n").unwrap();
    assert!(!rollout::wait_for_marker(
        &never,
        3,
        Duration::from_secs(30),
        &|_| {}
    ));
    assert!(!rollout::wait_for_marker(
        &dir.path().join("absent.md"),
        1,
        Duration::from_secs(1),
        &|_| {}
    ));
}

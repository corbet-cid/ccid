use super::core::Api;
use super::*;
use std::cell::RefCell;

mod archives;
mod contracts;
mod dependent_checks;
mod failure_digest;
mod provider_routes;
mod providers;
mod resolution;
mod retry;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
fn config(root: &Path) -> Config {
    Config {
        api: "https://ci.example.invalid/api/v1".into(),
        token_command: strings(&["false"]),
        ssh: strings(&["printf", "MemAvailable: 33554432 kB\nfull avg10=0.00\n"]),
        state_root: root.join("state"),
        host_sources: "/host/sources".into(),
        worker_sources: "/worker/sources".into(),
        host_tools: "/host/tools".into(),
        worker_tools: "/worker/tools".into(),
        remote_binary: "ccid".into(),
        tool_repo: root.join("repo"),
        tool_origins: vec!["https://forge.example.invalid/owner/repo".into()],
        origin_aliases: BTreeMap::from([(
            "alias.example.invalid".into(),
            "forge.example.invalid".into(),
        )]),
        argo_namespace: "ci-fixture".into(),
        argo_template: "ccid-job".into(),
        github_tool_repository: Some("owner/ccid".into()),
        forge_token_command: vec![],
        legacy_hosts: vec![],
    }
}
struct Fixture {
    root: tempfile::TempDir,
    repo: PathBuf,
    commit: String,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "--initial-branch=main"]).unwrap();
        git(&repo, &["config", "user.name", "fixture"]).unwrap();
        git(&repo, &["config", "user.email", "fixture@example.invalid"]).unwrap();
        let mut fixture = Self {
            root,
            repo,
            commit: String::new(),
        };
        fixture.commit(&[("source.txt", b"committed")]);
        let bare = fixture.root.path().join("remote.git");
        git(
            fixture.root.path(),
            &[
                "clone",
                "--bare",
                "--quiet",
                &fixture.repo.to_string_lossy(),
                &bare.to_string_lossy(),
            ],
        )
        .unwrap();
        git(
            &fixture.repo,
            &[
                "remote",
                "add",
                "origin",
                "https://forge.example.invalid/owner/repo",
            ],
        )
        .unwrap();
        git(
            &fixture.repo,
            &[
                "config",
                &format!("url.{}.insteadOf", bare.display()),
                "https://forge.example.invalid/owner/repo",
            ],
        )
        .unwrap();
        fixture
    }
    fn commit(&mut self, files: &[(&str, &[u8])]) {
        for (name, data) in files {
            let path = self.repo.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, data).unwrap();
            git(&self.repo, &["add", "--", name]).unwrap();
        }
        git(&self.repo, &["commit", "-qm", "fixture"]).unwrap();
        self.commit = git(&self.repo, &["rev-parse", "HEAD"]).unwrap();
    }
    fn args(&self) -> cli::SubmitArgs {
        cli::SubmitArgs {
            repo: self.repo.clone(),
            branch: "main".into(),
            expect_commit: None,
            workflows: vec!["verify".into()],
            variables: vec![],
            provider: "crow".into(),
            provider_wait: 0,
            queue_timeout: 120,
            rerun: false,
            cached_rerun: false,
        }
    }
    fn config(&self) -> Config {
        config(self.root.path())
    }
    fn publish(&self) {
        git(&self.repo, &["push", "-q", "origin", "HEAD:main"]).unwrap();
    }
}
struct Client {
    runs: RefCell<Vec<Value>>,
    posts: RefCell<Vec<(String, Value)>>,
    commit: String,
    fail_post: bool,
    repos: Vec<Value>,
}
impl Client {
    fn new(commit: &str) -> Self {
        Self {
            runs: RefCell::new(vec![]),
            posts: RefCell::new(vec![]),
            commit: commit.into(),
            fail_post: false,
            repos: vec![
                json!({"id":17,"full_name":"owner/repo","active":true,"clone_url":"https://forge.example.invalid/owner/repo"}),
            ],
        }
    }
}
impl Api for Client {
    fn call(&self, path: &str, body: Option<&Value>) -> Result<Value> {
        if let Some(body) = body {
            self.posts.borrow_mut().push((path.into(), body.clone()));
            if self.fail_post {
                return Err("Unknown POST outcome".into());
            }
            return Ok(json!({"number":9,"commit":self.commit,"status":"pending"}));
        }
        if path.starts_with("/repos?active") {
            return Ok(json!(self.repos));
        }
        if path.contains("/pipelines?") {
            return Ok(json!(*self.runs.borrow()));
        }
        if path.contains("/pipelines/") {
            return Ok(self.runs.borrow().first().cloned().unwrap_or(Value::Null));
        }
        Err("Unexpected test API request".into())
    }
}
fn stored(prepared: &submit::Prepared, status: &str) -> Value {
    json!({"number":1,"event":"manual","commit":prepared.commit,"status":status,"branch":"main","variables":prepared.variables,"workflows":[{"name":"verify","attempt":1,"state":status,"children":[]}]})
}
fn invoke(
    prepared: &mut submit::Prepared,
    config: &Config,
    args: &cli::SubmitArgs,
    api: &Client,
) -> Result<()> {
    prepared.run(config, args, api, false, &mut |_| Ok(()), &mut |_| Ok(()))
}

#[test]
fn inspection_returns_exact_runs_without_upload_or_post() {
    let f = Fixture::new();
    let a = f.args();
    let c = f.config();
    let api = Client::new(&f.commit);
    let p = submit::Prepared::new(&c, &a, &api).unwrap();
    api.runs.borrow_mut().push(stored(&p, "running"));
    let plan = p.inspection(&api, &a).unwrap();
    assert_eq!(plan["matching_runs"][0]["number"], 1);
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn inspection_does_not_match_another_branch() {
    let f = Fixture::new();
    let a = f.args();
    let api = Client::new(&f.commit);
    let p = submit::Prepared::new(&f.config(), &a, &api).unwrap();
    let mut r = stored(&p, "running");
    r["branch"] = json!("release");
    api.runs.borrow_mut().push(r);
    assert!(p.matching(&api, &a).unwrap().is_empty());
}
#[test]
fn active_identical_content_attaches_without_upload_or_post() {
    let f = Fixture::new();
    let a = f.args();
    let mut c = f.config();
    c.ssh = strings(&["false"]);
    let api = Client::new(&f.commit);
    let mut p = submit::Prepared::new(&c, &a, &api).unwrap();
    let mut r = stored(&p, "running");
    r["variables"]["SOURCE_ARCHIVE"] = json!("/old/path");
    api.runs.borrow_mut().push(r);
    invoke(&mut p, &c, &a, &api).unwrap();
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn completed_work_requires_deliberate_rerun() {
    let f = Fixture::new();
    let a = f.args();
    let c = f.config();
    let api = Client::new(&f.commit);
    let mut p = submit::Prepared::new(&c, &a, &api).unwrap();
    api.runs.borrow_mut().push(stored(&p, "success"));
    assert!(invoke(&mut p, &c, &a, &api).is_err());
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn dispatch_intent_captures_previous_runs_before_post() {
    let f = Fixture::new();
    let mut a = f.args();
    a.rerun = true;
    let c = f.config();
    let api = Client::new(&f.commit);
    let mut p = submit::Prepared::new(&c, &a, &api).unwrap();
    api.runs.borrow_mut().push(stored(&p, "success"));
    let mut observed = Value::Null;
    p.run(
        &c,
        &a,
        &api,
        false,
        &mut |v| {
            assert!(api.posts.borrow().is_empty());
            observed = v.clone();
            Ok(())
        },
        &mut |_| Ok(()),
    )
    .unwrap();
    assert_eq!(
        observed,
        json!({"repo_id":17,"commit":f.commit,"prior_run_numbers":[1]})
    );
    assert_eq!(api.posts.borrow().len(), 1);
}
#[test]
fn cached_rerun_restarts_exact_stored_run() {
    let f = Fixture::new();
    let mut a = f.args();
    a.cached_rerun = true;
    let c = f.config();
    let api = Client::new(&f.commit);
    let mut p = submit::Prepared::new(&c, &a, &api).unwrap();
    api.runs.borrow_mut().push(stored(&p, "success"));
    invoke(&mut p, &c, &a, &api).unwrap();
    assert_eq!(
        api.posts.borrow().as_slice(),
        [("/repos/17/pipelines/1".into(), json!({}))]
    );
}
#[test]
fn cached_rerun_rejects_noncanonical_prior_archive() {
    let f = Fixture::new();
    let mut a = f.args();
    a.cached_rerun = true;
    let c = f.config();
    let api = Client::new(&f.commit);
    let mut p = submit::Prepared::new(&c, &a, &api).unwrap();
    let mut r = stored(&p, "success");
    r["variables"]["SOURCE_ARCHIVE"] = json!("/old/path");
    api.runs.borrow_mut().push(r);
    assert!(invoke(&mut p, &c, &a, &api).is_err());
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn cached_rerun_without_matching_run_refuses_before_upload() {
    let f = Fixture::new();
    let mut a = f.args();
    a.cached_rerun = true;
    let mut c = f.config();
    c.ssh = strings(&["false"]);
    let api = Client::new(&f.commit);
    let mut p = submit::Prepared::new(&c, &a, &api).unwrap();
    assert!(invoke(&mut p, &c, &a, &api)
        .unwrap_err()
        .to_string()
        .contains("No matching stored"));
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn cached_rerun_rejects_branch_mismatch() {
    let f = Fixture::new();
    let mut a = f.args();
    a.cached_rerun = true;
    a.branch = "release".into();
    let c = f.config();
    let api = Client::new(&f.commit);
    let mut p = submit::Prepared::new(&c, &a, &api).unwrap();
    api.runs.borrow_mut().push(stored(&p, "success"));
    assert!(invoke(&mut p, &c, &a, &api).is_err());
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn cached_rerun_rejects_missing_persisted_workflow_config() {
    let r = json!({"status":"success","branch":"main","commit":SHA,"variables":{}});
    assert!(core::cached_restart(&r, SHA, "main", &json!({}), &["verify".into()]).is_err());
}
#[test]
fn cached_rerun_rejects_unknown_stored_status() {
    let r = json!({"status":"future","branch":"main","commit":SHA,"variables":{},"workflows":[{"name":"verify"}]});
    assert!(core::cached_restart(&r, SHA, "main", &json!({}), &["verify".into()]).is_err());
}
#[test]
fn cached_restart_requires_exact_bundle_transport_path() {
    let r = json!({"status":"success","branch":"main","commit":SHA,"variables":{"UPSTREAM_SOURCE_BUNDLE":"/old"},"workflows":[{"name":"verify"}]});
    assert!(core::cached_restart(
        &r,
        SHA,
        "main",
        &json!({"UPSTREAM_SOURCE_BUNDLE":"/new"}),
        &["verify".into()]
    )
    .is_err());
}
#[test]
fn bundle_digest_is_identity_but_bundle_location_is_transport() {
    let a = json!({"UPSTREAM_SOURCE_BUNDLE":"/old","UPSTREAM_SOURCE_SHA256":"aaa"});
    let mut b = json!({"UPSTREAM_SOURCE_BUNDLE":"/new","UPSTREAM_SOURCE_SHA256":"aaa"});
    assert_eq!(core::variables_identity(&a), core::variables_identity(&b));
    b["UPSTREAM_SOURCE_SHA256"] = json!("bbb");
    assert_ne!(core::variables_identity(&a), core::variables_identity(&b));
}
#[test]
fn cached_rerun_and_fresh_rerun_are_mutually_exclusive() {
    let f = Fixture::new();
    let mut a = f.args();
    a.rerun = true;
    a.cached_rerun = true;
    assert!(submit::Prepared::new(&f.config(), &a, &Client::new(&f.commit)).is_err());
}
#[test]
fn changed_check_selection_is_a_new_request() {
    let f = Fixture::new();
    let mut a = f.args();
    let c = f.config();
    let api = Client::new(&f.commit);
    let p = submit::Prepared::new(&c, &a, &api).unwrap();
    api.runs.borrow_mut().push(stored(&p, "running"));
    a.variables.push("CHECK_TARGET=eval".into());
    let mut p = submit::Prepared::new(&c, &a, &api).unwrap();
    invoke(&mut p, &c, &a, &api).unwrap();
    assert_eq!(api.posts.borrow()[0].1["variables"]["CHECK_TARGET"], "eval");
}
#[test]
fn reserved_source_override_is_rejected() {
    assert!(submit::variables(&["SOURCE_SHA256=forged".into()]).is_err());
}
#[test]
fn operator_budget_is_allowed_but_tool_and_provider_identity_are_reserved() {
    assert_eq!(
        submit::variables(&["CI_JOBS=8".into(), "CI_LINKER=mold".into()]).unwrap()["CI_JOBS"],
        "8"
    );
    for key in [
        "CI_COMMIT_SHA",
        "CI_TOOL_ARCHIVE",
        "CI_TOOL_SHA256",
        "CI_TOOL_BINARY",
        "CI_TOOL_BINARY_SHA256",
        "CCID_REVISION",
        "CROW_SERVER",
    ] {
        assert!(submit::variables(&[format!("{key}=forged")]).is_err());
    }
}
#[test]
fn explicit_cache_override_is_preserved() {
    assert_eq!(
        submit::variables(&["CI_CACHE_ROOT=/intentional/cache".into()]).unwrap()["CI_CACHE_ROOT"],
        "/intentional/cache"
    );
}
#[test]
fn ambiguous_post_failure_is_not_retried() {
    let f = Fixture::new();
    let a = f.args();
    let c = f.config();
    let mut api = Client::new(&f.commit);
    api.fail_post = true;
    let mut p = submit::Prepared::new(&c, &a, &api).unwrap();
    assert!(invoke(&mut p, &c, &a, &api).is_err());
    assert_eq!(api.posts.borrow().len(), 1);
}

#[test]
fn bad_checksum_never_installs_archive() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("object");
    assert!(transport::receive(&path, &sha(b"expected"), b"wrong".as_slice()).is_err());
    assert!(!path.exists());
    assert_eq!(fs::read_dir(d.path()).unwrap().count(), 0);
}
#[test]
fn verified_upload_is_idempotent_and_never_overwrites_other_bytes() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("object");
    let bytes = b"valid";
    let hash = sha(bytes);
    transport::receive(&path, &hash, bytes.as_slice()).unwrap();
    transport::receive(&path, &hash, bytes.as_slice()).unwrap();
    fs::write(&path, b"changed").unwrap();
    assert!(transport::receive(&path, &hash, bytes.as_slice()).is_err());
    assert_eq!(fs::read(path).unwrap(), b"changed");
}
#[test]
fn json_uses_atomic_digest_bound_transport() {
    let d = tempfile::tempdir().unwrap();
    let bytes = encode(&json!({"request":"value"})).unwrap();
    let target = d.path().join("request.json");
    transport::receive(&target, &sha(&bytes), bytes.as_bytes()).unwrap();
    assert_eq!(fs::read_to_string(target).unwrap(), bytes);
}
#[test]
fn unknown_format_never_opens_or_transfers_input() {
    let d = tempfile::tempdir().unwrap();
    assert!(transport::stage(
        &config(d.path()),
        Path::new("absent"),
        "17",
        &"a".repeat(64),
        "sh"
    )
    .unwrap_err()
    .to_string()
    .contains("Invalid source transport"));
}
#[test]
fn changed_executable_or_source_revision_is_rejected() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("ccid"), b"binary").unwrap();
    fs::write(d.path().join("receipt.json"),encode(&json!({"source_revision":SHA,"target":"x86_64-unknown-linux-gnu","binary_sha256":sha(b"binary")})).unwrap()).unwrap();
    assert!(transport::binary_receipt(d.path(), SHA, "x86_64-unknown-linux-gnu").is_ok());
    assert!(
        transport::binary_receipt(d.path(), &"a".repeat(40), "x86_64-unknown-linux-gnu").is_err()
    );
    fs::write(d.path().join("ccid"), b"tampered").unwrap();
    assert!(transport::binary_receipt(d.path(), SHA, "x86_64-unknown-linux-gnu").is_err());
}
#[test]
fn cold_full_swap_does_not_reject_available_memory() {
    assert_eq!(
        core::assess(
            "MemAvailable: 16777216 kB\nSwapFree: 0 kB\nfull avg10=0.00\n",
            8192
        )
        .unwrap()["available_mb"],
        16384
    );
}
#[test]
fn low_headroom_pressure_and_missing_measurement_fail_closed() {
    for snapshot in [
        "MemAvailable: 1 kB\n",
        "SwapFree: 10000 kB\n",
        "MemAvailable: 16777216 kB\nfull avg10=5.00\n",
    ] {
        assert!(core::assess(snapshot, 8192).is_err());
    }
}

struct Pages {
    calls: RefCell<usize>,
    repeat: bool,
}
impl Api for Pages {
    fn call(&self, _: &str, _: Option<&Value>) -> Result<Value> {
        let mut n = self.calls.borrow_mut();
        *n += 1;
        if *n == 1 || self.repeat {
            Ok(json!((1..=50)
                .map(|id| json!({"id":id}))
                .collect::<Vec<_>>()))
        } else {
            Ok(json!([{"id":51}]))
        }
    }
}
#[test]
fn repository_on_second_page_is_not_lost() {
    let api = Pages {
        calls: RefCell::new(0),
        repeat: false,
    };
    assert_eq!(core::pages(&api, "/repos?active=true").unwrap().len(), 51);
}
#[test]
fn server_ignoring_page_is_reported() {
    let api = Pages {
        calls: RefCell::new(0),
        repeat: true,
    };
    assert!(core::pages(&api, "/repos").is_err());
    assert_eq!(*api.calls.borrow(), 2);
}

#[test]
fn request_is_data_and_identity_changes_with_inputs() {
    let d = tempfile::tempdir().unwrap();
    let request = json!({"commit":SHA,"environment":{"ARG":"$(touch bad)"}});
    let workflow = jobs::workflow(&config(d.path()), "fixture", &request, "/tool", "hash").unwrap();
    assert_eq!(
        workflow["spec"]["arguments"]["parameters"][0]["value"],
        encode(&request).unwrap()
    );
    let mut changed = request.clone();
    changed["commit"] = json!("b".repeat(40));
    assert_ne!(
        jobs::identity(&request).unwrap(),
        jobs::identity(&changed).unwrap()
    );
}
#[test]
fn active_unknown_and_finished_requests_require_deliberate_handling() {
    for phase in ["Pending", "Running", "unknown", "pending", "waiting"] {
        assert!(jobs::admit_previous(phase, true).is_err());
    }
    for phase in ["success", "failure", "Succeeded", "Error"] {
        assert!(jobs::admit_previous(phase, false).is_err());
        assert!(jobs::admit_previous(phase, true).is_ok());
    }
}
#[test]
fn canonical_request_matches_python_ascii_encoding() {
    assert_eq!(
        encode(&json!({"z":"é😀","a":"value"})).unwrap(),
        "{\"a\":\"value\",\"z\":\"\\u00e9\\ud83d\\ude00\"}"
    );
}
#[test]
fn state_write_and_lock_are_atomic() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("state.json");
    let guard = lock(&d.path().join("request.lock")).unwrap();
    assert!(lock(&d.path().join("request.lock")).is_err());
    save(&path, &json!({"phase":"gha-intent"})).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap()["phase"],
        "gha-intent"
    );
    let inherited = guard.0.try_clone().unwrap();
    drop(guard);
    assert!(lock(&d.path().join("request.lock")).is_ok());
    drop(inherited);
}

#[test]
fn github_freeze_rejects_before_transport() {
    assert!(transport::http(
        "https://api.github.com/repos/fixture/test",
        None,
        Some(&json!({})),
        1024
    )
    .unwrap_err()
    .to_string()
    .contains("frozen"));
}

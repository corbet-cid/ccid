use super::*;
use std::rc::Rc;

#[test]
fn unrelated_active_hosted_work_blocks_fallback() {
    let f = RouteFixture::new();
    let api = f.api();
    let mut run = f.hosted("queued", Value::Null);
    run["display_title"] = json!("other work");
    assert!(f
        .run(
            &api,
            &Hosted {
                runs: vec![run],
                ..Default::default()
            },
            false
        )
        .is_err());
    assert!(api.posts.borrow().is_empty());
}

#[derive(Clone, Default)]
struct Hosted {
    runs: Vec<Value>,
    fail: bool,
    calls: Rc<RefCell<usize>>,
}
impl github::Hosted for Hosted {
    fn get(&self, _: &str) -> Result<Value> {
        Err("Unexpected receipt fetch".into())
    }
    fn pages(&self, _: &str, key: &str) -> Result<Vec<Value>> {
        *self.calls.borrow_mut() += 1;
        if self.fail {
            return Err("Unavailable".into());
        }
        if key != "workflow_runs" {
            return Err("Missing receipt".into());
        }
        Ok(self.runs.clone())
    }
    fn artifact(&self, _: &str, _: &Value) -> Result<Vec<u8>> {
        Err("No artifact".into())
    }
    fn release_asset(&self, _: &str, _: &str) -> Result<(Vec<u8>, Value)> {
        Err("Portable asset unavailable".into())
    }
}
struct RouteFixture {
    source: Fixture,
    config: Config,
    args: cli::SubmitArgs,
    key: String,
    path: PathBuf,
    identity: Value,
}
impl RouteFixture {
    fn new() -> Self {
        let mut source = Fixture::new();
        let tool = source.commit.clone();
        let yaml = format!(
            "variables:\n  CI_TOOL_ARCHIVE: {{default: \"\"}}\nsteps:\n  CCID_REVISION: '{tool}'\n"
        );
        let workflow = "name: fixture\n";
        let providers=format!("schema=1\n[github]\nrepository='owner/repo'\nworkflow='verify.yml'\nchecks=['linux']\nsecret_free=true\nfree_eligible=true\nplatform='linux-x86_64'\nworkflow_sha256='{}'\ntool_revision='{tool}'\ntool_bootstrap_sha256='{}'\n[dependencies]\nfiles=['Cargo.lock']\n",sha(workflow),"a".repeat(64));
        source.commit(&[
            (".crow/ccid.yaml", yaml.as_bytes()),
            (".github/workflows/verify.yml", workflow.as_bytes()),
            (".ci/providers.toml", providers.as_bytes()),
            (".ci/ccid.toml", b"schema=1\n"),
            ("Cargo.lock", b"fixture"),
        ]);
        let bare = source.root.path().join("remote.git");
        git(
            &source.repo,
            &[
                "config",
                "--add",
                &format!("url.{}.insteadOf", bare.display()),
                "https://github.com/owner/repo",
            ],
        )
        .unwrap();
        git(
            &source.repo,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/owner/repo",
            ],
        )
        .unwrap();
        source.publish();
        let mut config = source.config();
        config.tool_origins = vec!["https://github.com/owner/repo".into()];
        let mut args = source.args();
        args.workflows = vec!["ccid".into()];
        args.variables = vec!["CHECKS=linux".into()];
        args.provider = "auto".into();
        let contract = routing::contract(&config, &source.repo, &source.commit, &["linux".into()])
            .unwrap()
            .unwrap();
        let identity = json!({"commit":source.commit,"checks":["linux"],"contract":contract,"branch":"main","protocol":1});
        let key = sha(encode(&identity).unwrap());
        let path = config
            .directory("ci-provider-requests")
            .unwrap()
            .join(format!("{key}.json"));
        Self {
            source,
            config,
            args,
            key,
            path,
            identity,
        }
    }
    fn api(&self) -> Client {
        let mut api = Client::new(&self.source.commit);
        api.repos = vec![
            json!({"id":17,"active":true,"full_name":"owner/repo","clone_url":"https://github.com/owner/repo"}),
        ];
        api
    }
    fn run(&self, api: &Client, hosted: &Hosted, plan: bool) -> Result<()> {
        routing::route_with(&self.config, &self.args, plan, api, &mut || {
            Ok(Box::new(hosted.clone()))
        })
    }
    fn state(&self, phase: &str) {
        save(
            &self.path,
            &json!({"request_id":self.key,"identity":self.identity,"phase":phase}),
        )
        .unwrap();
    }
    fn hosted(&self, status: &str, conclusion: Value) -> Value {
        json!({"id":7,"head_sha":self.source.commit,"display_title":format!("ccid/{}",self.key),"path":".github/workflows/verify.yml","event":"workflow_dispatch","head_branch":"main","status":status,"conclusion":conclusion})
    }
    fn prior(&self, api: &Client, status: &str) -> Value {
        let p = submit::Prepared::new(&self.config, &self.args, api).unwrap();
        let mut r = stored(&p, status);
        r["workflows"][0]["name"] = json!("ccid");
        r
    }
}
#[test]
fn eligible_gha_checks_crow_inventory_without_crow_submission() {
    let f = RouteFixture::new();
    let api = f.api();
    let hosted = Hosted::default();
    f.run(&api, &hosted, true).unwrap();
    assert!(api.posts.borrow().is_empty());
    assert_eq!(*hosted.calls.borrow(), 1);
}
#[test]
fn exact_active_crow_work_attaches_before_hosted_post() {
    let f = RouteFixture::new();
    let api = f.api();
    api.runs.borrow_mut().push(f.prior(&api, "running"));
    f.run(&api, &Hosted::default(), false).unwrap();
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn exact_completed_crow_work_blocks_without_rerun() {
    let f = RouteFixture::new();
    let api = f.api();
    api.runs.borrow_mut().push(f.prior(&api, "success"));
    assert!(f.run(&api, &Hosted::default(), false).is_err());
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn crow_inventory_outage_blocks_new_hosted_post() {
    struct Broken;
    impl Api for Broken {
        fn call(&self, _: &str, _: Option<&Value>) -> Result<Value> {
            Err("Inventory unavailable".into())
        }
    }
    let f = RouteFixture::new();
    let mut called = false;
    assert!(
        routing::route_with(&f.config, &f.args, false, &Broken, &mut || {
            called = true;
            Ok(Box::new(Hosted::default()))
        })
        .is_err()
    );
    assert!(!called);
}
#[test]
fn crow_intent_reconciles_only_a_new_run() {
    let f = RouteFixture::new();
    let api = f.api();
    let mut prior = f.prior(&api, "success");
    prior["number"] = json!(2);
    api.runs.borrow_mut().push(prior);
    save(&f.path,&json!({"phase":"crow-intent","crow_context":{"repo_id":17,"commit":f.source.commit,"prior_run_numbers":[1]},"prior_run_numbers":[1]})).unwrap();
    f.run(&api, &Hosted::default(), false).unwrap();
    assert!(api.posts.borrow().is_empty());
    let state: Value = serde_json::from_slice(&fs::read(f.path).unwrap()).unwrap();
    assert_eq!(state["phase"], "crow");
    assert_eq!(state["crow_run"]["number"], 2);
}
#[test]
fn post_server_error_persists_intent_and_blocks_next_attempt() {
    let f = RouteFixture::new();
    let mut api = f.api();
    api.fail_post = true;
    assert!(f.run(&api, &Hosted::default(), false).is_err());
    assert_eq!(api.posts.borrow().len(), 1);
    assert!(f.run(&api, &Hosted::default(), false).is_err());
    assert_eq!(api.posts.borrow().len(), 1);
    let state: Value = serde_json::from_slice(&fs::read(f.path).unwrap()).unwrap();
    assert_eq!(state["phase"], "crow-intent");
}
#[test]
fn explicit_crow_honors_ambiguous_gha_intent_without_posting_crow() {
    let mut f = RouteFixture::new();
    f.args.provider = "crow".into();
    f.state("gha-intent");
    let api = f.api();
    assert!(f.run(&api, &Hosted::default(), false).is_err());
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn definite_dispatch_rejection_uses_crow_once() {
    let f = RouteFixture::new();
    f.state("gha-rejected");
    let api = f.api();
    f.run(&api, &Hosted::default(), false).unwrap();
    assert_eq!(api.posts.borrow().len(), 1);
}
#[test]
fn later_exact_run_reconciles_ambiguous_dispatch_without_retry() {
    let f = RouteFixture::new();
    f.state("gha-intent");
    let api = f.api();
    let hosted = Hosted {
        runs: vec![f.hosted("in_progress", Value::Null)],
        ..Default::default()
    };
    f.run(&api, &hosted, false).unwrap();
    assert!(api.posts.borrow().is_empty());
    let state: Value = serde_json::from_slice(&fs::read(f.path).unwrap()).unwrap();
    assert_eq!(state["phase"], "gha");
    assert_eq!(state["run_id"], 7);
}
#[test]
fn preflight_outage_can_fall_back_without_post() {
    let f = RouteFixture::new();
    let api = f.api();
    f.run(
        &api,
        &Hosted {
            fail: true,
            ..Default::default()
        },
        false,
    )
    .unwrap();
    assert_eq!(api.posts.borrow().len(), 1);
}
#[test]
fn outage_after_dispatch_cannot_fall_back() {
    let f = RouteFixture::new();
    f.state("gha");
    let api = f.api();
    assert!(f
        .run(
            &api,
            &Hosted {
                fail: true,
                ..Default::default()
            },
            false
        )
        .is_err());
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn check_failure_stays_failed() {
    let f = RouteFixture::new();
    let api = f.api();
    assert!(f
        .run(
            &api,
            &Hosted {
                runs: vec![f.hosted("completed", json!("failure"))],
                ..Default::default()
            },
            false
        )
        .is_err());
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn resource_controls_use_crow_after_inventory_without_hosted_post_or_tool_download() {
    let mut f = RouteFixture::new();
    f.args.variables.push("CI_JOBS=2".into());
    let api = f.api();
    f.run(&api, &Hosted::default(), false).unwrap();
    assert_eq!(api.posts.borrow().len(), 1);
    assert_eq!(api.posts.borrow()[0].1["variables"]["CI_JOBS"], "2");
}
#[test]
fn resource_controls_cannot_hide_ambiguous_hosted_intent_during_outage() {
    let mut f = RouteFixture::new();
    f.args.variables.push("CI_JOBS=2".into());
    f.state("gha-intent");
    let api = f.api();
    assert!(f
        .run(
            &api,
            &Hosted {
                fail: true,
                ..Default::default()
            },
            false
        )
        .is_err());
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn resource_controls_attach_active_hosted_work_before_crow() {
    let mut f = RouteFixture::new();
    f.args.variables.push("CI_JOBS=2".into());
    let api = f.api();
    f.run(
        &api,
        &Hosted {
            runs: vec![f.hosted("in_progress", Value::Null)],
            ..Default::default()
        },
        false,
    )
    .unwrap();
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn terminal_hosted_success_does_not_satisfy_new_resource_controls() {
    let mut f = RouteFixture::new();
    f.args.variables.push("CI_JOBS=2".into());
    let api = f.api();
    f.run(
        &api,
        &Hosted {
            runs: vec![f.hosted("completed", json!("success"))],
            ..Default::default()
        },
        false,
    )
    .unwrap();
    assert_eq!(api.posts.borrow().len(), 1);
}
#[test]
fn terminal_hosted_failure_with_resource_controls_stays_failed() {
    let mut f = RouteFixture::new();
    f.args.variables.push("CI_JOBS=2".into());
    let api = f.api();
    assert!(f
        .run(
            &api,
            &Hosted {
                runs: vec![f.hosted("completed", json!("failure"))],
                ..Default::default()
            },
            false
        )
        .is_err());
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn resource_control_plan_does_not_dispatch() {
    let mut f = RouteFixture::new();
    f.args.variables.push("CI_JOBS=2".into());
    let api = f.api();
    f.run(&api, &Hosted::default(), true).unwrap();
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn explicit_crow_attaches_to_existing_exact_gha_run() {
    let mut f = RouteFixture::new();
    f.args.provider = "crow".into();
    let api = f.api();
    f.run(
        &api,
        &Hosted {
            runs: vec![f.hosted("queued", Value::Null)],
            ..Default::default()
        },
        false,
    )
    .unwrap();
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn existing_success_rejects_a_replaced_portable_asset() {
    let f = RouteFixture::new();
    let api = f.api();
    assert!(f
        .run(
            &api,
            &Hosted {
                runs: vec![f.hosted("completed", json!("success"))],
                ..Default::default()
            },
            false
        )
        .is_err());
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn rerun_uses_new_identity_without_retrying_ambiguous_post() {
    let mut f = RouteFixture::new();
    f.args.rerun = true;
    let api = f.api();
    let previous = f.hosted("completed", json!("success"));
    let mut next_identity = f.identity.clone();
    next_identity["previous_github_run"] = json!(7);
    let key = sha(encode(&next_identity).unwrap());
    save(
        &f.path.with_file_name(format!("{key}.json")),
        &json!({"phase":"gha-intent"}),
    )
    .unwrap();
    assert!(f
        .run(
            &api,
            &Hosted {
                runs: vec![previous],
                ..Default::default()
            },
            false
        )
        .is_err());
    assert!(api.posts.borrow().is_empty());
}

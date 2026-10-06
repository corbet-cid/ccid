use super::*;

fn run() -> Value {
    json!({"number":1,"commit":SHA,"status":"failure","workflows":[{"id":10,"name":"prepare","attempt":1,"state":"success"},{"id":11,"name":"build","attempt":1,"state":"failure","depends_on":["prepare"]}]})
}
struct RetryApi {
    before: Value,
    after: Value,
    queue: Value,
    posts: RefCell<Vec<String>>,
    fail: bool,
}
impl RetryApi {
    fn new() -> Self {
        let before = run();
        let mut after = before.clone();
        after["workflows"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":12,"name":"build","attempt":2,"state":"pending"}));
        Self {
            before,
            after,
            queue: json!({"running":[],"pending":[],"waiting_on_deps":[]}),
            posts: RefCell::new(vec![]),
            fail: false,
        }
    }
}
impl Api for RetryApi {
    fn call(&self, path: &str, body: Option<&Value>) -> Result<Value> {
        if body.is_some() {
            self.posts.borrow_mut().push(path.into());
            if self.fail {
                return Err("Unknown response".into());
            }
            return Ok(self.after.clone());
        }
        if path == "/queue/info" {
            return Ok(self.queue.clone());
        }
        if self.posts.borrow().is_empty() {
            Ok(self.before.clone())
        } else {
            Ok(self.after.clone())
        }
    }
}
#[test]
fn native_workflow_retry_keeps_successful_dependency() {
    let d = tempfile::tempdir().unwrap();
    let api = RetryApi::new();
    submit::retry(&config(d.path()), &api, 17, 1, 11, SHA, "fixed").unwrap();
    assert_eq!(
        api.posts.borrow().as_slice(),
        ["/repos/17/pipelines/1/workflows/11/rerun"]
    );
    assert_eq!(api.after["workflows"][0]["state"], "success");
}
#[test]
fn old_attempt_cannot_be_retried_again() {
    let api = RetryApi::new();
    assert!(submit::select_retry(&api.after, 1, 11, SHA).is_err());
}
#[test]
fn identity_success_cancellation_and_active_pipeline_are_refused() {
    assert!(submit::select_retry(&run(), 2, 11, SHA).is_err());
    assert!(submit::select_retry(&run(), 1, 11, &"a".repeat(40)).is_err());
    for status in ["success", "killed", "running"] {
        let mut r = run();
        r["status"] = json!(status);
        assert!(submit::select_retry(&r, 1, 11, SHA).is_err());
    }
    assert!(submit::select_retry(&run(), 1, 10, SHA).is_err());
}
#[test]
fn dependency_latest_attempt_must_succeed() {
    let mut r = run();
    r["workflows"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":20,"name":"prepare","attempt":2,"state":"failure"}));
    assert!(submit::select_retry(&r, 1, 11, SHA).is_err());
}
#[test]
fn lost_response_leaves_intent_and_never_repeats_post() {
    let d = tempfile::tempdir().unwrap();
    let c = config(d.path());
    let mut api = RetryApi::new();
    api.fail = true;
    assert!(submit::retry(&c, &api, 17, 1, 11, SHA, "fixed").is_err());
    assert!(submit::retry(&c, &api, 17, 1, 11, SHA, "fixed").is_err());
    assert_eq!(api.posts.borrow().len(), 1);
    let intent = c.state_root.join("crow-ci-locks/retry-17-1-11.json");
    let saved: Value = serde_json::from_slice(&fs::read(intent).unwrap()).unwrap();
    assert_eq!(saved["state"], "submitted-outcome-unresolved");
}
#[test]
fn active_repository_and_incomplete_queue_block_post() {
    let d = tempfile::tempdir().unwrap();
    for queue in [
        json!({"running":[],"pending":[]}),
        json!({"running":[{"repo_id":17}],"pending":[],"waiting_on_deps":[]}),
    ] {
        let mut api = RetryApi::new();
        api.queue = queue;
        assert!(submit::retry(&config(d.path()), &api, 17, 1, 11, SHA, "fixed").is_err());
        assert!(api.posts.borrow().is_empty());
    }
}
#[test]
fn response_must_prove_same_source_and_new_attempt() {
    for after in [run(), json!({"number":2,"commit":SHA})] {
        let d = tempfile::tempdir().unwrap();
        let mut api = RetryApi::new();
        api.after = after;
        assert!(submit::retry(&config(d.path()), &api, 17, 1, 11, SHA, "fixed").is_err());
        assert_eq!(api.posts.borrow().len(), 1);
    }
}
#[test]
fn wrong_commit_never_posts() {
    let api = RetryApi::new();
    assert!(submit::cancel(&api, 17, 1, &"a".repeat(40)).is_err());
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn terminal_run_is_a_noop() {
    let api = RetryApi::new();
    submit::cancel(&api, 17, 1, SHA).unwrap();
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn empty_successful_cancel_is_verified() {
    let mut api = RetryApi::new();
    api.before["status"] = json!("running");
    api.after["status"] = json!("killed");
    submit::cancel(&api, 17, 1, SHA).unwrap();
    assert_eq!(
        api.posts.borrow().as_slice(),
        ["/repos/17/pipelines/1/cancel"]
    );
}
#[test]
fn cancel_requires_terminal_post_state() {
    let mut api = RetryApi::new();
    api.before["status"] = json!("running");
    api.after["status"] = json!("running");
    assert!(submit::cancel(&api, 17, 1, SHA).is_err());
    assert_eq!(api.posts.borrow().len(), 1);
}
#[test]
fn empty_http_response_is_accepted_for_cancel() {
    struct Empty {
        gets: RefCell<u64>,
    }
    impl Api for Empty {
        fn call(&self, _: &str, body: Option<&Value>) -> Result<Value> {
            if body.is_some() {
                return Ok(Value::Null);
            }
            let mut n = self.gets.borrow_mut();
            *n += 1;
            Ok(json!({"number":1,"commit":SHA,"status":if *n==1 {"running"}else{"killed"}}))
        }
    }
    let api = Empty {
        gets: RefCell::new(0),
    };
    submit::cancel(&api, 17, 1, SHA).unwrap();
    assert_eq!(*api.gets.borrow(), 2);
}
#[test]
fn branch_change_after_upload_never_submits() {
    let mut f = Fixture::new();
    let a = f.args();
    let c = f.config();
    let api = Client::new(&f.commit);
    let mut p = submit::Prepared::new(&c, &a, &api).unwrap();
    f.commit(&[("source.txt", b"changed")]);
    f.publish();
    assert!(invoke(&mut p, &c, &a, &api).is_err());
    assert!(api.posts.borrow().is_empty());
}
#[test]
fn cached_rerun_rejects_changed_head_after_staging() {
    let mut f = Fixture::new();
    let mut a = f.args();
    a.cached_rerun = true;
    let c = f.config();
    let api = Client::new(&f.commit);
    let mut p = submit::Prepared::new(&c, &a, &api).unwrap();
    api.runs.borrow_mut().push(stored(&p, "success"));
    f.commit(&[("source.txt", b"changed")]);
    assert!(invoke(&mut p, &c, &a, &api).is_err());
    assert!(api.posts.borrow().is_empty());
}

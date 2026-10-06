use super::*;

struct Resolution {
    consumer: Fixture,
    tool: Fixture,
    config: Config,
    api: Client,
}
impl Resolution {
    fn new(checks: bool) -> Self {
        let mut consumer = Fixture::new();
        consumer.commit(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.1.0'\n",
            ),
            ("Cargo.lock", b"version=4\n"),
        ]);
        consumer.publish();
        let mut tool = Fixture::new();
        let workflow=format!("variables:\n  RESOLVE_SOURCE_ARCHIVE: {{default: ''}}\n  RESOLVE_SOURCE_SHA256: {{default: ''}}\n  RESOLVE_SOURCE_COMMIT: {{default: ''}}\n  RESOLVE_REPOSITORY_URL: {{default: ''}}\n{}",if checks {"  RESOLVE_CHECKS: {default: ''}\n"} else {""});
        tool.commit(&[(".crow/resolve.yaml", workflow.as_bytes())]);
        tool.publish();
        let config = tool.config();
        let api = Client::new(&tool.commit);
        Self {
            consumer,
            tool,
            config,
            api,
        }
    }
    fn run(&self, checks: &[String], plan: bool) -> Result<()> {
        submit::resolve_with(
            &self.config,
            &self.consumer.repo,
            "main",
            &self.tool.repo,
            "main",
            checks,
            plan,
            &self.api,
        )
    }
}
#[test]
fn consumer_archive_and_core_revision_are_bound_separately() {
    let f = Resolution::new(true);
    f.run(&[], false).unwrap();
    let posts = f.api.posts.borrow();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].1["variables"]["RESOLVE_SOURCE_COMMIT"],
        f.consumer.commit
    );
    assert_ne!(
        posts[0].1["variables"]["RESOLVE_SOURCE_SHA256"],
        posts[0].1["variables"]["SOURCE_SHA256"]
    );
    assert_ne!(f.consumer.commit, f.tool.commit);
}
#[test]
fn plan_does_not_upload_or_submit_an_executable_request() {
    let mut f = Resolution::new(true);
    f.config.ssh = strings(&["false"]);
    f.run(&[], true).unwrap();
    assert!(f.api.posts.borrow().is_empty());
}
#[test]
fn changed_consumer_source_stops_before_job_submission() {
    let mut f = Resolution::new(true);
    f.consumer
        .commit(&[("Cargo.lock", b"version=4\n# changed\n")]);
    assert!(f.run(&[], false).is_err());
    assert!(f.api.posts.borrow().is_empty());
}
#[test]
fn other_tool_repository_is_rejected_before_staging() {
    let mut f = Resolution::new(true);
    f.config.tool_origins = vec!["https://other.invalid/owner/tool".into()];
    f.config.ssh = strings(&["false"]);
    assert!(f
        .run(&[], false)
        .unwrap_err()
        .to_string()
        .contains("canonical"));
    assert!(f.api.posts.borrow().is_empty());
}
#[test]
fn selected_candidate_checks_are_bound_to_the_dispatch() {
    let f = Resolution::new(true);
    f.run(&["linux".into(), "release".into()], false).unwrap();
    assert_eq!(
        f.api.posts.borrow()[0].1["variables"]["RESOLVE_CHECKS"],
        "linux,release"
    );
}
#[test]
fn malformed_candidate_check_stops_before_upload() {
    let mut f = Resolution::new(true);
    f.config.ssh = strings(&["false"]);
    assert!(f.run(&["linux;false".into()], false).is_err());
    assert!(f.api.posts.borrow().is_empty());
}
#[test]
fn old_resolver_cannot_silently_skip_requested_validation() {
    let f = Resolution::new(false);
    assert!(f.run(&["linux".into()], false).is_err());
    assert!(f.api.posts.borrow().is_empty());
}
#[test]
fn consumer_movement_during_core_staging_stops_final_dispatch() {
    let f = Resolution::new(true);
    struct Moving<'a> {
        api: &'a Client,
        consumer: &'a Fixture,
        moved: RefCell<bool>,
    }
    impl Api for Moving<'_> {
        fn call(&self, path: &str, body: Option<&Value>) -> Result<Value> {
            if path.contains("/pipelines?") && !*self.moved.borrow() {
                fs::write(
                    self.consumer.repo.join("Cargo.lock"),
                    b"version=4\n# changed\n",
                )?;
                git(&self.consumer.repo, &["commit", "-qam", "changed"])?;
                git(&self.consumer.repo, &["push", "-q", "origin", "HEAD:main"])?;
                *self.moved.borrow_mut() = true;
            }
            self.api.call(path, body)
        }
    }
    let api = Moving {
        api: &f.api,
        consumer: &f.consumer,
        moved: RefCell::new(false),
    };
    assert!(submit::resolve_with(
        &f.config,
        &f.consumer.repo,
        "main",
        &f.tool.repo,
        "main",
        &[],
        false,
        &api
    )
    .is_err());
    assert!(*api.moved.borrow());
    assert!(f.api.posts.borrow().is_empty());
}

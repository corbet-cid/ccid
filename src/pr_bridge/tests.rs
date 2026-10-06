use super::*;
use std::process::Command;

fn config() -> Config {
    Config {
        schema: 1,
        gitlab: "https://gitlab.example".into(),
        gitlab_project: 42,
        gitlab_repository: "team/project".into(),
        forgejo: "https://forge.example".into(),
        primary_repository: "team/project".into(),
        import_repository: "bridge/project".into(),
        target_branch: "main".into(),
    }
}

#[test]
fn isolated_ci_requires_live_exact_maintainer_approval_and_removes_credentials() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let mut api = FakeApi::default();
    let mut objects = FakeObjects::default();
    import(&mut api, &mut objects, &config(), 7, &journal).unwrap();
    let head = "a".repeat(40);
    let base = "b".repeat(40);
    api.pulls[0]["head"]["sha"] = json!(head);
    api.pulls[0]["base"]["sha"] = json!(base);
    let request = json!({"head":head,"base":base,"review":1,"maintainer":"maintainer","maintainer_id":50,
        "sandbox":{"namespace":"ccid-untrusted-test","image":format!("example/image@sha256:{}","c".repeat(64)),"archive_sha256":"d".repeat(64),"command":["sh",".ci/test.sh"]}});
    let bytes = serde_json::to_vec(&request).unwrap();
    let message = approval::message(&bytes).unwrap();
    api.review = json!({"id":1,"user":{"id":50,"login":"maintainer"},"state":"APPROVED","dismissed":false,"stale":false,"commit_id":head,"body":message});
    let job = approval::plan(&mut api, &config(), 7, &journal, &bytes).unwrap();
    let pod = &job["spec"]["template"]["spec"];
    assert_eq!(pod["automountServiceAccountToken"], false);
    assert_eq!(
        pod["containers"][0]["securityContext"]["readOnlyRootFilesystem"],
        true
    );
    assert_eq!(pod["volumes"].as_array().unwrap().len(), 2);
    assert!(pod["volumes"][0]["hostPath"].is_null());
    assert_eq!(pod["containers"][0]["volumeMounts"][0]["readOnly"], true);
    assert!(pod["containers"][0]["envFrom"].is_null());
    api.permission = "read".into();
    assert!(approval::plan(&mut api, &config(), 7, &journal, &bytes).is_err());
    api.permission = "write".into();
    api.review["dismissed"] = json!(true);
    assert!(approval::plan(&mut api, &config(), 7, &journal, &bytes).is_err());
    api.review["dismissed"] = json!(false);
    api.pulls[0]["base"]["sha"] = json!("e".repeat(40));
    assert!(approval::plan(&mut api, &config(), 7, &journal, &bytes).is_err());
}

#[test]
fn closure_recovers_lost_comment_and_close_responses_without_duplicates() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let mut api = FakeApi::default();
    let mut objects = FakeObjects::default();
    import(&mut api, &mut objects, &config(), 7, &journal).unwrap();
    api.pulls[0]["state"] = json!("closed");
    api.pulls[0]["merged"] = json!(true);
    api.lose_note = true;
    assert!(lifecycle::feedback(&mut api, &config(), 7, &journal).is_err());
    assert_eq!(api.notes.len(), 1);
    assert_eq!(api.source["state"], "opened");
    api.lose_note = false;
    api.lose_close = true;
    assert!(lifecycle::feedback(&mut api, &config(), 7, &journal).is_err());
    assert_eq!(api.source["state"], "closed");
    api.lose_close = false;
    let result = lifecycle::feedback(&mut api, &config(), 7, &journal).unwrap();
    assert_eq!(result["status"], "source_closed");
    assert_eq!(api.notes.len(), 1);
    assert!(api.notes[0]["body"]
        .as_str()
        .unwrap()
        .contains("https://forge.example/team/project/pulls/20"));
}

#[test]
fn closure_refuses_to_discard_new_source_work() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let mut api = FakeApi::default();
    let mut objects = FakeObjects::default();
    import(&mut api, &mut objects, &config(), 7, &journal).unwrap();
    api.pulls[0]["state"] = json!("closed");
    api.source["sha"] = json!("b".repeat(40));
    assert!(lifecycle::feedback(&mut api, &config(), 7, &journal).is_err());
    assert!(api.notes.is_empty());
    assert_eq!(api.source["state"], "opened");
}

#[test]
fn replacement_creates_new_generation_and_preserves_history_on_retry() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let mut api = FakeApi::default();
    let mut objects = FakeObjects::default();
    import(&mut api, &mut objects, &config(), 7, &journal).unwrap();
    let previous = api.pulls[0]["head"].clone();
    let head = "b".repeat(40);
    api.source["sha"] = json!(head);
    let result = lifecycle::replace(&mut api, &mut objects, &config(), 7, &journal, &head).unwrap();
    assert_eq!(result["pull_number"], 21);
    assert_eq!(api.pulls[0]["state"], "closed");
    assert_eq!(api.pulls[0]["head"], previous);
    assert_eq!(api.pulls[1]["state"], "open");
    lifecycle::replace(&mut api, &mut objects, &config(), 7, &journal, &head).unwrap();
    assert_eq!(api.posts, 2);
    assert_eq!(
        import(&mut api, &mut objects, &config(), 7, &journal).unwrap()["pull_number"],
        21
    );
}

fn source() -> Value {
    json!({"iid":7,"project_id":42,"target_project_id":42,"state":"opened","title":"A contribution",
        "sha":"a".repeat(40),"target_branch":"main","author":{"id":8,"username":"contributor"}})
}

struct FakeApi {
    source: Value,
    pulls: Vec<Value>,
    posts: usize,
    requests: Vec<(Forge, String, String)>,
    lost_response: bool,
    reject_post: bool,
    actions: bool,
    hooks: bool,
    moved_source: bool,
    private_source: bool,
    reads: usize,
    notes: Vec<Value>,
    lose_note: bool,
    lose_close: bool,
    review: Value,
    permission: String,
}

impl Default for FakeApi {
    fn default() -> Self {
        Self {
            source: source(),
            pulls: Vec::new(),
            posts: 0,
            requests: Vec::new(),
            lost_response: false,
            reject_post: false,
            actions: false,
            hooks: false,
            moved_source: false,
            private_source: false,
            reads: 0,
            notes: Vec::new(),
            lose_note: false,
            lose_close: false,
            review: Value::Null,
            permission: "write".into(),
        }
    }
}

impl Api for FakeApi {
    fn request(
        &mut self,
        forge: Forge,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        self.requests.push((forge, method.into(), path.into()));
        if path == "repos/team/project/pulls/20/reviews/1" {
            return Ok(self.review.clone());
        }
        if path == "repos/team/project/collaborators/maintainer/permission" {
            return Ok(json!({"user":{"id":50},"permission":self.permission}));
        }
        if forge == Forge::Gitlab && path == "user" {
            return Ok(json!({"id":99}));
        }
        if path.starts_with("projects/42/merge_requests/7/notes") {
            if method == "POST" {
                let note = json!({"id":100,"body":body.unwrap()["body"],"author":{"id":99}});
                self.notes.push(note.clone());
                if self.lose_note {
                    return Err(failure("Lost note response"));
                }
                return Ok(note);
            }
            return Ok(if path.ends_with("page=1") {
                json!(self.notes)
            } else {
                json!([])
            });
        }
        if method == "PUT" && path == "projects/42/merge_requests/7" {
            assert_eq!(body.unwrap()["state_event"], "close");
            self.source["state"] = json!("closed");
            if self.lose_close {
                return Err(failure("Lost close response"));
            }
            return Ok(self.source.clone());
        }
        if let Some(index) = path.strip_prefix("repos/team/project/pulls/") {
            let index = index.parse::<u64>().unwrap();
            let pull = self
                .pulls
                .iter_mut()
                .find(|p| p["number"] == index)
                .unwrap();
            if method == "PATCH" {
                pull["state"] = body.unwrap()["state"].clone();
                pull["body"] = body.unwrap()["body"].clone();
            }
            return Ok(pull.clone());
        }
        if path == "projects/42/merge_requests/7" {
            self.reads += 1;
            if self.moved_source && self.reads > 1 {
                self.source["sha"] = json!("b".repeat(40));
            }
            return Ok(self.source.clone());
        }
        if path == "projects/42" {
            return Ok(
                json!({"id":42,"path_with_namespace":"team/project","visibility":if self.private_source {"private"} else {"public"}}),
            );
        }
        if path == "repos/team/project" {
            return Ok(
                json!({"id":10,"full_name":"team/project","has_actions":self.actions,"private":false}),
            );
        }
        if path == "repos/bridge/project" {
            return Ok(
                json!({"id":11,"full_name":"bridge/project","has_actions":false,"fork":true,"parent":{"id":10},"owner":{"id":12},"private":false}),
            );
        }
        if path == "user" {
            return Ok(json!({"id":12}));
        }
        if path.contains("hooks?") {
            return Ok(if self.hooks {
                json!([{"id":1}])
            } else {
                json!([])
            });
        }
        if method == "GET" && path.starts_with("repos/team/project/pulls?") {
            return Ok(if path.ends_with("page=1") {
                json!(self.pulls)
            } else {
                json!([])
            });
        }
        if method == "POST" && path == "repos/team/project/pulls" {
            self.posts += 1;
            if self.reject_post {
                return Err(failure("HTTP 503"));
            }
            let body = body.unwrap();
            let branch = body["head"]
                .as_str()
                .unwrap()
                .strip_prefix("bridge:")
                .unwrap();
            let pull = json!({"number":20+self.pulls.len(),"state":"open","merged":false,"body":body["body"],
                "head":{"ref":branch,"repo":{"id":11}}, "base":{"ref":"main","repo":{"id":10}}, "user":{"id":12}});
            self.pulls.push(pull.clone());
            if self.lost_response {
                return Err(failure("Lost HTTP response"));
            }
            return Ok(pull);
        }
        panic!("Unexpected API request: {method} {path}");
    }
}

#[derive(Default)]
struct FakeObjects {
    heads: Vec<String>,
    fail: bool,
}
impl Objects for FakeObjects {
    fn publish(&mut self, _: &Config, iid: u64, sha: &str, branch: &str) -> Result<()> {
        assert_eq!(iid, 7);
        assert!(branch.starts_with("ccid-import/gitlab/"));
        self.heads.push(sha.into());
        if self.fail {
            Err(failure("Git transport refused"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn import_credits_author_and_is_idempotent_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let mut api = FakeApi::default();
    let mut objects = FakeObjects::default();
    {
        let journal = Journal::open(root.path()).unwrap();
        let report = import(&mut api, &mut objects, &config(), 7, &journal).unwrap();
        assert_eq!(report["pull_number"], 20);
        let body = api.pulls[0]["body"].as_str().unwrap();
        assert!(body.contains("Original author: GitLab `contributor` (user ID 8)"));
        assert!(body.contains("https://gitlab.example/team/project/-/merge_requests/7"));
        assert_eq!(report["ci"], "not_dispatched");
    }
    let journal = Journal::open(root.path()).unwrap();
    import(&mut api, &mut objects, &config(), 7, &journal).unwrap();
    assert_eq!(api.posts, 1);
    assert!(api
        .requests
        .iter()
        .all(|(forge, method, _)| *forge != Forge::Gitlab || method == "GET"));
}

#[test]
fn lost_post_response_is_reconciled_without_another_post() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let mut api = FakeApi {
        lost_response: true,
        ..Default::default()
    };
    let mut objects = FakeObjects::default();
    assert!(import(&mut api, &mut objects, &config(), 7, &journal).is_err());
    assert!(journal.load(&config().key(7)).unwrap().create_attempted);
    let result = import(&mut api, &mut objects, &config(), 7, &journal).unwrap();
    assert_eq!(result["pull_number"], 20);
    assert_eq!(api.posts, 1);
}

#[test]
fn unknown_creation_without_visible_pr_blocks_retry() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let mut api = FakeApi {
        reject_post: true,
        ..Default::default()
    };
    let mut objects = FakeObjects::default();
    assert!(import(&mut api, &mut objects, &config(), 7, &journal).is_err());
    let error = import(&mut api, &mut objects, &config(), 7, &journal).unwrap_err();
    assert!(error.to_string().contains("refusing a second POST"));
    assert_eq!(api.posts, 1);
    assert_eq!(objects.heads.len(), 1);
}

#[test]
fn closed_primary_is_not_reopened_or_pushed_and_source_is_not_closed() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let mut api = FakeApi::default();
    let mut objects = FakeObjects::default();
    import(&mut api, &mut objects, &config(), 7, &journal).unwrap();
    api.pulls[0]["state"] = json!("closed");
    let report = import(&mut api, &mut objects, &config(), 7, &journal).unwrap();
    assert_eq!(report["status"], "primary_closed");
    assert_eq!(report["source_close_pending"], true);
    assert_eq!(objects.heads.len(), 1);
    assert_eq!(api.posts, 1);
}

#[test]
fn unsafe_ci_preflight_stops_before_git_or_post() {
    for (actions, hooks) in [(true, false), (false, true)] {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::open(root.path()).unwrap();
        let mut api = FakeApi {
            actions,
            hooks,
            ..Default::default()
        };
        let mut objects = FakeObjects::default();
        assert!(import(&mut api, &mut objects, &config(), 7, &journal).is_err());
        assert!(objects.heads.is_empty());
        assert_eq!(api.posts, 0);
    }
}

#[test]
fn private_source_cannot_be_imported_into_public_destination() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let mut api = FakeApi {
        private_source: true,
        ..Default::default()
    };
    let mut objects = FakeObjects::default();
    let error = import(&mut api, &mut objects, &config(), 7, &journal).unwrap_err();
    assert!(error.to_string().contains("visibility"));
    assert!(objects.heads.is_empty());
    assert_eq!(api.posts, 0);
}

#[test]
fn changed_source_or_failed_git_never_creates_pr() {
    for moved_source in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::open(root.path()).unwrap();
        let mut api = FakeApi {
            moved_source,
            ..Default::default()
        };
        let mut objects = FakeObjects {
            fail: !moved_source,
            ..Default::default()
        };
        assert!(import(&mut api, &mut objects, &config(), 7, &journal).is_err());
        assert_eq!(api.posts, 0);
        assert!(!journal.load(&config().key(7)).unwrap().create_attempted);
    }
}

#[test]
fn forged_marker_wrong_actor_retarget_or_duplicate_pr_fails_closed() {
    for alteration in ["actor", "fork", "base", "marker", "duplicate"] {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::open(root.path()).unwrap();
        let mut api = FakeApi::default();
        let mut objects = FakeObjects::default();
        import(&mut api, &mut objects, &config(), 7, &journal).unwrap();
        match alteration {
            "actor" => api.pulls[0]["user"]["id"] = json!(99),
            "fork" => api.pulls[0]["head"]["repo"]["id"] = json!(99),
            "base" => api.pulls[0]["base"]["ref"] = json!("other"),
            "marker" => api.pulls[0]["body"] = json!("altered"),
            _ => api.pulls.push(api.pulls[0].clone()),
        }
        assert!(
            import(&mut api, &mut objects, &config(), 7, &journal).is_err(),
            "{alteration}"
        );
        assert_eq!(objects.heads.len(), 1);
        assert_eq!(api.posts, 1);
    }
}

#[test]
fn rejects_bad_source_identity_and_closed_mrs() {
    for (field, value) in [
        ("state", json!("closed")),
        ("iid", json!(8)),
        ("project_id", json!(1)),
        ("target_project_id", json!(1)),
        ("target_branch", json!("release")),
        ("sha", json!("-option")),
        ("title", json!("bad\ntitle")),
    ] {
        let mut value_source = source();
        value_source[field] = value;
        let parsed: MergeRequest = serde_json::from_value(value_source).unwrap();
        assert!(parsed.validate(&config(), 7).is_err(), "{field}");
    }
}

#[test]
fn validates_origins_and_repository_paths_before_any_io() {
    for value in [
        "http://forge.example",
        "https://user@forge.example",
        "https://forge.example/path",
        "https://forge.example?x",
        "https://forge.example\n",
        "https://forge.example:0",
    ] {
        assert!(origin(value).is_err(), "{value:?}");
    }
    for value in [
        "team/../repo",
        "team/repo?x",
        "team/%2F",
        "-o/repo",
        "team//repo",
    ] {
        assert!(repository(value, false).is_err());
    }
    config().validate().unwrap();
    let mut other = config();
    other.gitlab = "https://other.example".into();
    assert_ne!(config().key(7), other.key(7));
}

#[test]
fn journal_serializes_invocations_and_persists_poll_cooldown() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    assert!(Journal::open(root.path()).is_err());
    journal.admit().unwrap();
    assert!(journal.admit().is_err());
    drop(journal);
    assert!(Journal::open(root.path()).unwrap().admit().is_err());
}

#[test]
fn full_pagination_is_required() {
    struct Pages {
        calls: usize,
    }
    impl Api for Pages {
        fn request(&mut self, _: Forge, _: &str, path: &str, _: Option<&Value>) -> Result<Value> {
            self.calls += 1;
            assert!(path.ends_with(&format!("page={}", self.calls)));
            Ok(json!(vec![json!({"id":self.calls}); 50]))
        }
    }
    let mut api = Pages { calls: 0 };
    assert!(pages(&mut api, Forge::Forgejo, "pulls?state=all").is_err());
    assert_eq!(api.calls, 20);
}

#[test]
fn server_capped_short_pages_are_followed_and_repeated_pages_are_rejected() {
    struct Pages {
        calls: usize,
        repeat: bool,
    }
    impl Api for Pages {
        fn request(&mut self, _: Forge, _: &str, _: &str, _: Option<&Value>) -> Result<Value> {
            self.calls += 1;
            Ok(match self.calls {
                1 => json!([{"number":1}]),
                2 if self.repeat => json!([{"number":1}]),
                2 => json!([{"number":2}]),
                _ => json!([]),
            })
        }
    }
    let values = pages(
        &mut Pages {
            calls: 0,
            repeat: false,
        },
        Forge::Forgejo,
        "pulls?state=all",
    )
    .unwrap();
    assert_eq!(values.len(), 2);
    assert_eq!(values[1]["number"], 2);
    assert!(pages(
        &mut Pages {
            calls: 0,
            repeat: true
        },
        Forge::Forgejo,
        "pulls?state=all"
    )
    .is_err());
}

fn git_command(root: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Contributor")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Contributor")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().into()
}

#[test]
fn git_preserves_objects_fast_forwards_and_refuses_rebases_or_sha_races() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    git_command(source.path(), &["init", "--initial-branch=main"]);
    git_command(destination.path(), &["init", "--bare"]);
    fs::write(source.path().join("payload.sh"), "exit 99\n").unwrap();
    git_command(source.path(), &["add", "payload.sh"]);
    git_command(source.path(), &["commit", "-m", "Original contribution"]);
    let first = git_command(source.path(), &["rev-parse", "HEAD"]);
    git_command(
        source.path(),
        &["update-ref", "refs/merge-requests/7/head", &first],
    );
    let make_transport = || {
        let mut transport = git::Git::new(Duration::from_secs(60)).unwrap();
        transport.local_urls = Some((
            source.path().to_string_lossy().into(),
            destination.path().to_string_lossy().into(),
        ));
        transport
    };
    let branch = format!("ccid-import/gitlab/{}", config().key(7));
    let reference = format!("refs/heads/{branch}");
    make_transport()
        .publish(&config(), 7, &"b".repeat(40), &branch)
        .unwrap_err();
    make_transport()
        .publish(&config(), 7, &first, &branch)
        .unwrap();
    make_transport()
        .publish(&config(), 7, &first, &branch)
        .unwrap();
    assert_eq!(
        git_command(destination.path(), &["cat-file", "-p", &first]),
        git_command(source.path(), &["cat-file", "-p", &first])
    );
    git_command(
        source.path(),
        &["commit", "--allow-empty", "-m", "Follow-up"],
    );
    let second = git_command(source.path(), &["rev-parse", "HEAD"]);
    git_command(
        source.path(),
        &["update-ref", "refs/merge-requests/7/head", &second],
    );
    make_transport()
        .publish(&config(), 7, &second, &branch)
        .unwrap();
    assert_eq!(
        git_command(destination.path(), &["rev-parse", &reference]),
        second
    );
    git_command(
        source.path(),
        &["commit", "--amend", "--allow-empty", "-m", "Rebased"],
    );
    let rebased = git_command(source.path(), &["rev-parse", "HEAD"]);
    git_command(
        source.path(),
        &["update-ref", "refs/merge-requests/7/head", &rebased],
    );
    let error = make_transport()
        .publish(&config(), 7, &rebased, &branch)
        .unwrap_err();
    assert!(error.to_string().contains("no force push"));
    assert_eq!(
        git_command(destination.path(), &["rev-parse", &reference]),
        second
    );
    assert!(!destination.path().join("payload.sh").exists());
    assert!(
        !git_command(destination.path(), &["for-each-ref", "--format=%(refname)"])
            .contains("refs/heads/main")
    );
}

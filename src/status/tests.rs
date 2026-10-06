use super::*;

#[derive(Default)]
struct Fake {
    calls: Vec<(String, Option<Value>)>,
    code: u16,
    absent: bool,
    wrong_sha: bool,
}
impl Api for Fake {
    fn request(
        &mut self,
        target: &Target,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value)> {
        self.calls.push((path.into(), body.cloned()));
        if body.is_some() {
            return Ok((if self.code == 0 { 201 } else { self.code }, json!({})));
        }
        let field = match target.provider {
            Provider::Forgejo => "sha",
            Provider::Gitlab => "id",
            Provider::Bitbucket => "hash",
        };
        Ok((
            if self.absent { 404 } else { 200 },
            json!({field:if self.wrong_sha {"b"} else {"a"}.repeat(40)}),
        ))
    }
}
fn config() -> Config {
    Config {
        schema: 1,
        targets: vec![
            Target {
                provider: Provider::Forgejo,
                origin: "https://forge.example".into(),
                repository: "team/repo".into(),
                token_env: "CCID_STATUS_FORGEJO_TOKEN".into(),
            },
            Target {
                provider: Provider::Gitlab,
                origin: "https://gitlab.example".into(),
                repository: "group/sub/repo".into(),
                token_env: "CCID_STATUS_GITLAB_TOKEN".into(),
            },
            Target {
                provider: Provider::Bitbucket,
                origin: "https://api.bitbucket.org".into(),
                repository: "workspace/repo".into(),
                token_env: "CCID_STATUS_BITBUCKET_TOKEN".into(),
            },
        ],
    }
}
fn options() -> Options {
    Options {
        config: PathBuf::new(),
        state_dir: PathBuf::new(),
        commit: "a".repeat(40),
        name: "ccid/verify".into(),
        url: "https://ci.example/runs/1".into(),
        state: State::Pending,
        started: 100,
    }
}

#[test]
fn provider_payloads_exact_commit_and_idempotent_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path().into()).unwrap();
    let mut fake = Fake::default();
    let mut opts = options();
    let cfg = config();
    assert_eq!(
        report(&mut fake, &journal, &cfg, &opts).unwrap()["complete"],
        true
    );
    assert!(fake.calls[2].0.contains("group%2Fsub%2Frepo"));
    assert_eq!(fake.calls[1].1.as_ref().unwrap()["context"], "ccid/verify");
    assert_eq!(fake.calls[5].1.as_ref().unwrap()["state"], "INPROGRESS");
    report(&mut fake, &journal, &cfg, &opts).unwrap();
    assert_eq!(fake.calls.len(), 6);
    opts.state = State::Failure;
    report(&mut fake, &journal, &cfg, &opts).unwrap();
    assert_eq!(fake.calls[9].1.as_ref().unwrap()["state"], "failed");
    assert_eq!(fake.calls[11].1.as_ref().unwrap()["state"], "FAILED");
    opts.state = State::Pending;
    assert_eq!(
        report(&mut fake, &journal, &cfg, &opts).unwrap()["complete"],
        false
    );
    assert_eq!(fake.calls.len(), 12);
    opts.started = 99;
    assert_eq!(
        report(&mut fake, &journal, &cfg, &opts).unwrap()["results"][0]["status"],
        "superseded"
    );
}

#[test]
fn absent_and_wrong_commit_never_receive_status() {
    for absent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().into()).unwrap();
        let mut fake = Fake {
            absent,
            wrong_sha: true,
            ..Default::default()
        };
        let result = report(&mut fake, &journal, &config(), &options()).unwrap();
        assert_eq!(result["complete"], absent);
        assert_eq!(fake.calls.len(), 3);
        assert!(fake.calls.iter().all(|(_, body)| body.is_none()));
    }
}

#[test]
fn failed_post_is_visible_other_targets_continue_and_uncertainty_blocks_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path().into()).unwrap();
    let mut fake = Fake {
        code: 429,
        ..Default::default()
    };
    assert_eq!(
        report(&mut fake, &journal, &config(), &options()).unwrap()["complete"],
        false
    );
    assert_eq!(fake.calls.len(), 6);
    fake.code = 201;
    assert_eq!(
        report(&mut fake, &journal, &config(), &options()).unwrap()["complete"],
        false
    );
    assert_eq!(fake.calls.iter().filter(|(_, b)| b.is_some()).count(), 3);
}

#[test]
fn reject_github_credentials_in_urls_and_untrusted_token_names() {
    assert!(origin("https://api.github.com").is_err());
    assert!(origin("https://token@forge.example").is_err());
    assert!(serde_json::from_value::<Target>(json!({"provider":"github","origin":"https://github.com","repository":"team/repo","token_env":"CCID_STATUS_GITHUB_TOKEN"})).is_err());
    let mut t = config().targets.remove(0);
    t.token_env = "HOME".into();
    assert!(t.validate().is_err());
    t.token_env = "CCID_STATUS_TOKEN".into();
    t.repository = "team/../repo".into();
    assert!(t.validate().is_err());
}

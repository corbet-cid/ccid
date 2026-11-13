use super::*;
use std::process::Command;

const PLACEMENT: &str = r#"{
  "version": 1,
  "default": "https://forge.example",
  "primaries": {},
  "native": {
    "schema": 1,
    "source": {"origin": "https://forge.example", "token_env": "T"},
    "repositories": [],
    "contributed": [
      {"name": "alpha", "upstream": "https://github.com/someone/alpha",
       "forks": ["https://github.com/me/alpha", "https://forge.example/me/alpha"]},
      {"name": "beta", "upstream": "https://gitlab.example/group/sub/beta"}
    ]
  }
}"#;

fn catalog() -> Catalog {
    Catalog::parse(PLACEMENT.as_bytes(), &[]).unwrap()
}

fn error_of(text: &str) -> String {
    Catalog::parse(text.as_bytes(), &[])
        .unwrap_err()
        .to_string()
}

fn with_list(list: &str) -> String {
    format!(r#"{{"native":{{"schema":1,"repositories":[],"contributed":{list}}}}}"#)
}

#[test]
fn every_spelling_of_a_location_has_one_key() {
    let expected = Some("github.com/someone/alpha".to_owned());
    for url in [
        "https://github.com/someone/alpha",
        "https://github.com/someone/alpha.git",
        "https://github.com/someone/alpha/",
        "HTTPS://GitHub.com/Someone/Alpha.git",
        "http://github.com/someone/alpha",
        "https://token@github.com/someone/alpha.git",
        "https://user:secret@github.com:8443/someone/alpha.git",
        "ssh://git@github.com/someone/alpha.git",
        "ssh://git@github.com:2222/someone/alpha",
        "git@github.com:someone/alpha.git",
        "github.com:someone/alpha",
        "git+https://github.com/someone/alpha",
        "  https://github.com/someone/alpha  ",
    ] {
        assert_eq!(key_of(url), expected, "{url}");
    }
    assert_eq!(
        key_of("https://gitlab.example/group/sub/beta.git").as_deref(),
        Some("gitlab.example/group/sub/beta")
    );
}

#[test]
fn what_names_no_repository_has_no_key() {
    for url in [
        "",
        "/home/me/alpha",
        "../alpha",
        "file:///srv/git/owner/alpha",
        "https://github.com/someone",
        "https://github.com/",
        "https://github.com/someone/../alpha",
        "https://github.com/some one/alpha",
        "ftp://github.com/someone/alpha",
        "https://[::1]/owner/alpha",
        "https:///owner/alpha",
    ] {
        assert_eq!(key_of(url), None, "{url:?}");
    }
}

#[test]
fn a_placement_without_a_list_declares_nothing() {
    for text in [
        r#"{"native":{"schema":1,"repositories":[]}}"#,
        r#"{"schema":1,"repositories":[]}"#,
        &with_list("[]"),
    ] {
        let catalog = Catalog::parse(text.as_bytes(), &[]).unwrap();
        assert!(catalog.projects().is_empty());
        assert!(catalog
            .classify_url("https://github.com/someone/alpha")
            .is_none());
    }
}

#[test]
fn the_native_view_and_the_whole_document_read_the_same() {
    let whole = catalog();
    let native =
        serde_json::from_str::<serde_json::Value>(PLACEMENT).unwrap()["native"].to_string();
    let view = Catalog::parse(native.as_bytes(), &[]).unwrap();
    assert_eq!(whole.projects(), view.projects());
    assert_eq!(whole.digest(), view.digest());
    // The view has no `default`, but its source origin names the forge.
    assert_eq!(view.forge_hosts(), ["forge.example"]);
}

#[test]
fn a_location_is_recognised_in_any_spelling_of_the_project() {
    let catalog = catalog();
    for url in [
        "https://github.com/someone/alpha.git",
        "git@github.com:me/alpha.git",
        "https://forge.example/me/alpha",
        "https://FORGE.example/Me/Alpha.git",
    ] {
        assert_eq!(
            catalog.classify_url(url).map(|p| p.name.as_str()),
            Some("alpha"),
            "{url}"
        );
    }
    assert_eq!(
        catalog
            .classify_url("https://gitlab.example/group/sub/beta")
            .map(|p| p.name.as_str()),
        Some("beta")
    );
    for url in [
        "https://github.com/someone/alpha-other",
        "https://github.com/someone/alph",
        "https://github.com/other/alpha",
        "https://gitlab.example/group/beta",
        "https://forge.example/me/other",
        "https://forge.example/someone/alpha",
    ] {
        assert!(catalog.classify_url(url).is_none(), "{url}");
    }
}

#[test]
fn a_forge_job_is_classified_by_server_and_name() {
    let catalog = catalog();
    assert!(catalog
        .classify_repository("https://forge.example/", "me/alpha")
        .is_some());
    assert!(catalog
        .classify_repository("https://forge.example", "me/alpha")
        .is_some());
    assert!(catalog
        .classify_repository("https://forge.example", "me/other")
        .is_none());
    // A scheduler that names only owner/name means OUR forge.
    assert!(catalog.classify_forge_path("me/alpha").is_some());
    assert!(catalog.classify_forge_path("me/other").is_none());
    // The same path on another forge is another repository.
    assert!(catalog
        .classify_repository("https://elsewhere.example", "me/alpha")
        .is_none());
}

#[test]
fn our_forge_is_known_under_all_its_spellings() {
    let aliased = Catalog::parse(PLACEMENT.as_bytes(), &["git.example".to_owned()]).unwrap();
    assert_eq!(aliased.forge_hosts(), ["forge.example", "git.example"]);
    assert!(aliased
        .classify_url("https://git.example/me/alpha.git")
        .is_some());
    assert!(aliased.classify_forge_path("me/alpha").is_some());
    // Another forge's host is not made a spelling of ours.
    assert!(aliased
        .classify_url("https://other.example/me/alpha")
        .is_none());
    // Without the extra spelling the other host matches nothing.
    assert!(catalog()
        .classify_url("https://git.example/me/alpha")
        .is_none());
}

#[test]
fn the_digest_follows_the_declaration() {
    let digest = catalog().digest().to_owned();
    assert_eq!(digest.len(), 64);
    assert_eq!(catalog().digest(), digest);
    // Fork order is not part of the declaration.
    let reordered = PLACEMENT.replace(
        r#""forks": ["https://github.com/me/alpha", "https://forge.example/me/alpha"]"#,
        r#""forks": ["https://forge.example/me/alpha", "https://github.com/me/alpha"]"#,
    );
    assert_eq!(
        Catalog::parse(reordered.as_bytes(), &[]).unwrap().digest(),
        digest
    );
    let changed = PLACEMENT.replace("someone/alpha", "someone/gamma");
    assert_ne!(
        Catalog::parse(changed.as_bytes(), &[]).unwrap().digest(),
        digest
    );
}

#[test]
fn a_malformed_declaration_is_refused_with_the_entry_named() {
    let ok = r#"{"name":"alpha","upstream":"https://github.com/o/alpha"}"#;
    assert!(Catalog::parse(with_list(&format!("[{ok}]")).as_bytes(), &[]).is_ok());
    for (list, wanted) in [
        ("{}", "must be a list"),
        (r#"["alpha"]"#, "must be an object"),
        (r#"[{"upstream":"https://github.com/o/a"}]"#, "needs a name"),
        (
            r#"[{"name":"Alpha","upstream":"https://github.com/o/a"}]"#,
            "plain lowercase name",
        ),
        (
            r#"[{"name":"-a","upstream":"https://github.com/o/a"}]"#,
            "plain lowercase name",
        ),
        (r#"[{"name":"a"}]"#, "needs an upstream"),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a","extra":1}]"#,
            "unknown field extra",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a","forks":"x"}]"#,
            "forks must be a list",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a","forks":[1]}]"#,
            "forks are strings",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a","forks":["https://github.com/o/a"]}]"#,
            "listed twice",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a","forks":["https://x.example/o/a","https://x.example/o/a"]}]"#,
            "listed twice",
        ),
        // Canonical spelling only.
        (
            r#"[{"name":"a","upstream":"http://github.com/o/a"}]"#,
            "an https URL",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a.git"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a/"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://GitHub.com/o/a"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://git@github.com/o/a"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com:443/o/a"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/../a"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a?x=1"}]"#,
            "not a canonical location",
        ),
        // Order and uniqueness.
        (
            r#"[{"name":"b","upstream":"https://github.com/o/b"},{"name":"a","upstream":"https://github.com/o/a"}]"#,
            "must come after",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a"},{"name":"a","upstream":"https://github.com/o/b"}]"#,
            "must come after",
        ),
        (
            // Owner and repository names match case-insensitively, as every forge of the set does.
            r#"[{"name":"a","upstream":"https://github.com/o/a"},{"name":"b","upstream":"https://github.com/O/A"}]"#,
            "belongs to both a and b",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a"},{"name":"b","upstream":"https://x.example/p/b","forks":["https://github.com/o/a"]}]"#,
            "belongs to both a and b",
        ),
    ] {
        let message = error_of(&with_list(list));
        assert!(
            message.contains(wanted),
            "{list}\n  -> {message}\n  wanted {wanted:?}"
        );
    }
    assert!(error_of("not json").contains("not valid JSON"));
}

#[test]
fn a_project_whose_forks_overlap_a_forge_alias_is_refused_once() {
    // Two projects may not claim the same forge path through two spellings of the forge.
    let text = with_list(
        r#"[{"name":"a","upstream":"https://x.example/o/a","forks":["https://forge.example/me/a"]},
            {"name":"b","upstream":"https://y.example/o/b","forks":["https://git.example/me/a"]}]"#,
    )
    .replace(
        r#"{"schema":1,"repositories":[],"contributed""#,
        r#"{"schema":1,"repositories":[],"source":{"origin":"https://forge.example","token_env":"T"},"contributed""#,
    );
    let message = Catalog::parse(text.as_bytes(), &["git.example".to_owned()])
        .unwrap_err()
        .to_string();
    assert!(message.contains("belongs to both a and b"), "{message}");
}

fn git(directory: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {arguments:?}");
}

#[test]
fn a_working_copy_is_classified_by_any_remote() {
    let catalog = catalog();
    let environment = Environment::new();
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    git(repo, &["init", "-q"]);
    // A repository without a remote is nobody's.
    assert!(catalog
        .classify_working_copy(repo, &environment)
        .unwrap()
        .is_none());
    git(
        repo,
        &[
            "remote",
            "add",
            "origin",
            "https://forge.example/own/thing.git",
        ],
    );
    assert!(catalog
        .classify_working_copy(repo, &environment)
        .unwrap()
        .is_none());
    git(
        repo,
        &[
            "remote",
            "add",
            "upstream",
            "git@github.com:someone/alpha.git",
        ],
    );
    assert_eq!(
        catalog
            .classify_working_copy(repo, &environment)
            .unwrap()
            .map(|p| p.name.as_str()),
        Some("alpha")
    );
}

#[test]
fn a_push_url_counts_too() {
    let catalog = catalog();
    let temp = tempfile::tempdir().unwrap();
    git(temp.path(), &["init", "-q"]);
    git(
        temp.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://forge.example/own/thing.git",
        ],
    );
    git(
        temp.path(),
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            "https://github.com/me/alpha.git",
        ],
    );
    assert!(catalog
        .classify_working_copy(temp.path(), &Environment::new())
        .unwrap()
        .is_some());
}

#[test]
fn a_job_names_its_repository_in_the_environment() {
    let catalog = catalog();
    let temp = tempfile::tempdir().unwrap();
    let mut environment = Environment::new();
    environment.insert(
        "CI_REPOSITORY_URL".into(),
        "https://forge.example/me/alpha.git".into(),
    );
    assert!(catalog
        .classify_working_copy(temp.path(), &environment)
        .unwrap()
        .is_some());
    environment.insert(
        "CI_REPOSITORY_URL".into(),
        "https://forge.example/own/thing".into(),
    );
    assert!(catalog
        .classify_working_copy(temp.path(), &environment)
        .unwrap()
        .is_none());
}

#[test]
fn a_directory_that_is_no_repository_has_no_remote() {
    let temp = tempfile::tempdir().unwrap();
    assert!(remote_urls(temp.path()).unwrap().is_empty());
    assert!(catalog()
        .classify_working_copy(temp.path(), &Environment::new())
        .unwrap()
        .is_none());
}

#[test]
fn an_unset_placement_classifies_nothing_and_a_bad_one_is_an_error() {
    let temp = tempfile::tempdir().unwrap();
    assert!(Catalog::load(&Environment::new()).unwrap().is_none());
    let mut environment = Environment::new();
    // Set but missing, a directory, a symbolic target of nothing, and invalid content: each refuses, none passes.
    environment.insert(
        PLACEMENT_FILE_ENV.into(),
        temp.path().join("missing.json").into(),
    );
    assert!(Catalog::load(&environment).is_err());
    environment.insert(PLACEMENT_FILE_ENV.into(), temp.path().into());
    assert!(Catalog::load(&environment).is_err());
    let file = temp.path().join("placement.json");
    fs::write(&file, "{").unwrap();
    environment.insert(PLACEMENT_FILE_ENV.into(), file.clone().into());
    let message = Catalog::load(&environment).unwrap_err().to_string();
    assert!(
        message.contains("not valid JSON") && message.contains("placement.json"),
        "{message}"
    );
    fs::write(&file, PLACEMENT).unwrap();
    assert_eq!(
        Catalog::load(&environment)
            .unwrap()
            .unwrap()
            .projects()
            .len(),
        2
    );
}

#[cfg(unix)]
#[test]
fn a_placement_behind_a_symbolic_link_is_read() {
    // A deployed copy is typically a link into a store.
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("real.json");
    fs::write(&target, PLACEMENT).unwrap();
    let link = temp.path().join("placement.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let mut environment = Environment::new();
    environment.insert(PLACEMENT_FILE_ENV.into(), link.into());
    assert!(Catalog::load(&environment).unwrap().is_some());
}

#[test]
fn the_real_placement_shape_is_accepted() {
    // The fleet's placement carries many repositories with fields this module does not read.
    let text = r#"{"version":1,"default":"https://forge.example","primaries":{},"external_content":{},
        "native":{"schema":1,"source":{"origin":"https://forge.example","token_env":"T"},
        "repositories":[{"path":"org/repo","source_id":1,"private":false,"default_branch":"main","content":"native-git","hold":null,
        "destinations":[{"provider":"github","path":"org/repo","mirror_user":"m","interval_seconds":600}]}],
        "contributed":[{"name":"seaweedfs","upstream":"https://github.com/seaweedfs/seaweedfs","forks":["https://github.com/me/seaweedfs"]}]}}"#;
    let catalog = Catalog::parse(text.as_bytes(), &[]).unwrap();
    assert_eq!(catalog.projects().len(), 1);
    assert!(catalog
        .classify_url("git@github.com:me/seaweedfs.git")
        .is_some());
    // An owned repository of the same placement is not classified.
    assert!(catalog
        .classify_url("https://forge.example/org/repo")
        .is_none());
}

#[test]
fn only_the_source_is_our_forge_not_the_default_primary() {
    // `default` names the default PRIMARY, which a primary switch can point at another forge.
    let text = PLACEMENT.replace(
        r#""default": "https://forge.example""#,
        r#""default": "https://github.com""#,
    );
    let catalog = Catalog::parse(text.as_bytes(), &[]).unwrap();
    assert_eq!(catalog.forge_hosts(), ["forge.example"]);
    assert!(catalog.classify_forge_path("someone/alpha").is_none());
}

#[test]
fn a_document_that_is_not_a_placement_is_refused() {
    for text in [
        "{}",
        "[]",
        r#"{"version":1,"default":"https://forge.example","primaries":{}}"#,
        r#"{"native":{"schema":1}}"#,
        r#"{"native":{"schema":1,"repositories":{}}}"#,
    ] {
        let message = Catalog::parse(text.as_bytes(), &[])
            .unwrap_err()
            .to_string();
        assert!(message.contains("not a placement"), "{text}: {message}");
    }
}

#[test]
fn an_empty_variable_is_not_an_unset_one() {
    let mut environment = Environment::new();
    environment.insert(PLACEMENT_FILE_ENV.into(), "".into());
    let message = Catalog::load(&environment).unwrap_err().to_string();
    assert!(message.contains("set but empty"), "{message}");
}

fn loadable(temp: &Path) -> Environment {
    let file = temp.join("placement.json");
    fs::write(&file, PLACEMENT).unwrap();
    let mut environment = Environment::new();
    environment.insert(PLACEMENT_FILE_ENV.into(), file.into());
    environment
}

#[test]
fn a_runners_own_spelling_of_the_forge_counts() {
    let temp = tempfile::tempdir().unwrap();
    let mut environment = loadable(temp.path());
    assert!(Catalog::load(&environment)
        .unwrap()
        .unwrap()
        .classify_url("https://forge.internal/me/alpha")
        .is_none());
    environment.insert(
        "CCID_FORGE_HOSTS".into(),
        " Forge.Internal , git.example ".into(),
    );
    let catalog = Catalog::load(&environment).unwrap().unwrap();
    assert!(catalog
        .classify_url("https://forge.internal/me/alpha")
        .is_some());
    assert!(catalog
        .classify_repository("https://git.example/", "me/alpha")
        .is_some());
    assert!(catalog.classify_forge_path("me/alpha").is_some());
    environment.insert("CCID_FORGE_HOSTS".into(), "not a host".into());
    assert!(Catalog::load(&environment).is_err());
}

#[test]
fn fleet_data_that_is_set_but_unusable_is_an_error_not_a_smaller_list() {
    let temp = tempfile::tempdir().unwrap();
    let mut environment = loadable(temp.path());
    environment.insert(
        "CCID_FLEET_FILE".into(),
        temp.path().join("missing.json").into(),
    );
    let message = Catalog::load(&environment).unwrap_err().to_string();
    assert!(message.contains("fleet data"), "{message}");
    // A good copy adds its hosts.
    let channel = temp.path().join("channel.json");
    fs::write(
        &channel,
        r#"{"default":{"revision":"r","sha256":"s"},"fleet":{"schema":1,"forge_hosts":["forge.example","git.example"],"first_party":["https://forge.example/org/"]}}"#,
    )
    .unwrap();
    environment.insert("CCID_FLEET_FILE".into(), channel.into());
    let catalog = Catalog::load(&environment).unwrap().unwrap();
    assert!(catalog
        .classify_url("https://git.example/me/alpha.git")
        .is_some());
}

#[test]
fn a_remote_is_read_as_git_resolves_it() {
    let catalog = catalog();
    let temp = tempfile::tempdir().unwrap();
    git(temp.path(), &["init", "-q"]);
    git(temp.path(), &["remote", "add", "origin", "gh:me/alpha"]);
    // Raw, the shorthand names nothing we know.
    assert!(catalog
        .classify_working_copy(temp.path(), &Environment::new())
        .unwrap()
        .is_none());
    git(
        temp.path(),
        &["config", "url.https://github.com/.insteadOf", "gh:"],
    );
    assert_eq!(
        catalog
            .classify_working_copy(temp.path(), &Environment::new())
            .unwrap()
            .map(|p| p.name.as_str()),
        Some("alpha")
    );
    // A remote with no push address of its own is pushed to where `pushInsteadOf` sends it (fetched from, it
    // names nothing).
    let other = tempfile::tempdir().unwrap();
    git(other.path(), &["init", "-q"]);
    git(other.path(), &["remote", "add", "origin", "ph:alpha"]);
    git(
        other.path(),
        &["config", "url.https://github.com/me/.pushInsteadOf", "ph:"],
    );
    assert!(catalog
        .classify_working_copy(other.path(), &Environment::new())
        .unwrap()
        .is_some());
    // `insteadOf` rewrites an explicit push address too.
    let third = tempfile::tempdir().unwrap();
    git(third.path(), &["init", "-q"]);
    git(
        third.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://forge.example/own/thing.git",
        ],
    );
    git(
        third.path(),
        &["remote", "set-url", "--push", "origin", "ph:alpha"],
    );
    assert!(catalog
        .classify_working_copy(third.path(), &Environment::new())
        .unwrap()
        .is_none());
    git(
        third.path(),
        &["config", "url.https://github.com/me/.insteadOf", "ph:"],
    );
    assert!(catalog
        .classify_working_copy(third.path(), &Environment::new())
        .unwrap()
        .is_some());
}

#[test]
fn a_job_names_its_repository_the_way_actions_do() {
    let catalog = catalog();
    let temp = tempfile::tempdir().unwrap();
    let mut environment = Environment::new();
    environment.insert("GITHUB_SERVER_URL".into(), "https://forge.example/".into());
    environment.insert("GITHUB_REPOSITORY".into(), "me/alpha".into());
    assert!(catalog
        .classify_working_copy(temp.path(), &environment)
        .unwrap()
        .is_some());
    environment.insert("GITHUB_REPOSITORY".into(), "own/thing".into());
    assert!(catalog
        .classify_working_copy(temp.path(), &environment)
        .unwrap()
        .is_none());
}

#[test]
fn a_repository_git_refuses_to_read_is_an_error_not_an_empty_answer() {
    // A repository git cannot read (here a corrupt config; a foreign owner is the same to git) is not "no remotes".
    let temp = tempfile::tempdir().unwrap();
    git(temp.path(), &["init", "-q"]);
    fs::write(temp.path().join(".git/config"), "[core\n").unwrap();
    let message = remote_urls(temp.path()).unwrap_err().to_string();
    assert!(message.contains("git could not read"), "{message}");
    assert!(catalog()
        .classify_working_copy(temp.path(), &Environment::new())
        .is_err());
}

#[test]
fn the_project_roots_include_the_main_checkout_of_a_linked_worktree() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    git(
        &main,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "i",
        ],
    );
    let linked = temp.path().join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().unwrap(),
            "-b",
            "other",
        ],
    );
    let roots = project_roots(&linked.canonicalize().unwrap());
    assert!(roots.contains(&linked.canonicalize().unwrap()));
    assert!(roots.contains(&main.canonicalize().unwrap()), "{roots:?}");
    // From a subdirectory, the top level is still a root.
    fs::create_dir(linked.join("sub")).unwrap();
    let roots = project_roots(&linked.join("sub").canonicalize().unwrap());
    assert!(roots.contains(&linked.canonicalize().unwrap()), "{roots:?}");
}

#[test]
fn a_channel_without_a_fleet_member_names_no_more_spellings_and_is_no_error() {
    // An ordinary state: a release channel that carries no fleet data (a runner prints that and goes on).
    let temp = tempfile::tempdir().unwrap();
    let mut environment = loadable(temp.path());
    let channel = temp.path().join("channel.json");
    fs::write(&channel, r#"{"default":{"revision":"r","sha256":"s"}}"#).unwrap();
    environment.insert("CCID_FLEET_FILE".into(), channel.clone().into());
    let catalog = Catalog::load(&environment).unwrap().unwrap();
    assert_eq!(catalog.forge_hosts(), ["forge.example"]);
    assert!(catalog.classify_forge_path("me/alpha").is_some());
    // A file that is not JSON, and a fleet member that does not validate, are errors.
    fs::write(&channel, "not json").unwrap();
    assert!(Catalog::load(&environment).is_err());
    fs::write(
        &channel,
        r#"{"default":{"revision":"r","sha256":"s"},"fleet":{"schema":2,"first_party":[]}}"#,
    )
    .unwrap();
    assert!(Catalog::load(&environment).is_err());
}

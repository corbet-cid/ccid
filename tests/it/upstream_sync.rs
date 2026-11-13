//! The upstream sync: the rendered workflow keeps a downstream repository on its upstream.
//!
//! The rendered scripts run for real. `git` works on local bare repositories (a `url.insteadOf` rewrite maps the
//! forge's https URLs onto them) and the few Forgejo API calls are answered by a fake `curl` from files
//! (`tests/it/fixtures/upstream-sync-fake-curl`). The tests need `bash`, `git` and `jq`.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const MANIFEST: &str = "schema=1\nproject='demo'\n[render]\nrender_schema=2\n\
    [checks.downstream-check]\nkind='commands'\ncommands=[['true']]\n\
    [jobs.downstream-check]\nscheduler='forgejo'\nruns_on='linux'\nchecks=['downstream-check']\nworkflow='downstream-check'\ncommand=['ccid:run-declared-checks']\nevents=['manual']\n";
const DECLARATION: &str = "{\n  \"schema_version\": 1,\n  \"upstream\": {\"remote\": \"upstream\", \"branch\": \"main\", \"url\": \"https://forge.example/team/up.git\"},\n  \"allowed_paths\": [\"ccvl-downstream.json\", \"opportunities/\"]\n}\n";
const SYNC: &str = ".forgejo/workflows/upstream-sync.yml";

fn render(root: &Path, check: bool) -> ccid::Result<ccid::render::Report> {
    ccid::render::render(root, Path::new(".ci/ccid.toml"), check)
}

fn sh(dir: &Path, program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.test")
        .env("GIT_COMMITTER_NAME", "Test Author")
        .env("GIT_COMMITTER_EMAIL", "author@example.test")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    sh(dir, "git", args)
}

/// A repository carrying the sync declaration and manifest, rendered.
fn declared(declaration: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".ci")).unwrap();
    fs::write(root.path().join(".ci/ccid.toml"), MANIFEST).unwrap();
    if !declaration.is_empty() {
        fs::write(root.path().join("ccvl-downstream.json"), declaration).unwrap();
    }
    render(root.path(), false).unwrap();
    root
}

/// The `run:` script of the step called `name` in a rendered workflow, dedented.
fn step_script(workflow: &str, name: &str) -> String {
    let mut lines = workflow.lines();
    lines
        .find(|line| line.trim() == format!("- name: {name}"))
        .unwrap();
    lines.find(|line| line.trim() == "run: |").unwrap();
    let mut script = String::new();
    for line in lines {
        if !line.is_empty() && !line.starts_with("          ") {
            break;
        }
        script.push_str(line.get(10..).unwrap_or(""));
        script.push('\n');
    }
    script
}

struct Forge {
    dir: tempfile::TempDir,
    up_work: PathBuf,
    down_work: PathBuf,
    merge: String,
    publish: String,
}

struct Outcome {
    output: Output,
    env: String,
}

impl Outcome {
    fn ok(&self) -> bool {
        self.output.status.success()
    }
    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }
    fn var(&self, name: &str) -> Option<String> {
        self.env
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix(&format!("{name}=")).map(str::to_owned))
    }
}

impl Forge {
    /// An upstream with one commit and a downstream cloned from it with one commit of its own.
    fn new() -> Forge {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let fix = root.join("fix");
        fs::create_dir_all(fix.join("forge/team")).unwrap();
        fs::create_dir_all(fix.join("green")).unwrap();
        fs::create_dir_all(fix.join("bin")).unwrap();
        let curl = fix.join("bin/curl");
        fs::copy("tests/it/fixtures/upstream-sync-fake-curl", &curl).unwrap();
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
        let up_bare = fix.join("forge/team/up.git");
        let down_bare = fix.join("forge/team/down.git");
        git(
            root,
            &[
                "init",
                "--quiet",
                "--bare",
                "--initial-branch=main",
                up_bare.to_str().unwrap(),
            ],
        );
        git(
            root,
            &[
                "init",
                "--quiet",
                "--bare",
                "--initial-branch=main",
                down_bare.to_str().unwrap(),
            ],
        );
        let up_work = root.join("up");
        let down_work = root.join("down");
        git(
            root,
            &[
                "clone",
                "--quiet",
                up_bare.to_str().unwrap(),
                up_work.to_str().unwrap(),
            ],
        );
        fs::write(up_work.join("code.txt"), "one\n").unwrap();
        fs::write(up_work.join(".gitattributes"), "* text=auto\n").unwrap();
        git(&up_work, &["add", "."]);
        git(&up_work, &["commit", "--quiet", "-m", "upstream: one"]);
        git(
            &up_work,
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        );
        git(
            root,
            &[
                "clone",
                "--quiet",
                up_bare.to_str().unwrap(),
                down_work.to_str().unwrap(),
            ],
        );
        fs::create_dir_all(down_work.join("opportunities")).unwrap();
        fs::write(down_work.join("opportunities/job.txt"), "mine\n").unwrap();
        fs::write(down_work.join("ccvl-downstream.json"), DECLARATION).unwrap();
        git(&down_work, &["add", "."]);
        git(
            &down_work,
            &["commit", "--quiet", "-m", "downstream: opportunity"],
        );
        git(
            &down_work,
            &["remote", "set-url", "origin", down_bare.to_str().unwrap()],
        );
        git(
            &down_work,
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        );
        let rendered = declared(DECLARATION);
        let workflow = fs::read_to_string(rendered.path().join(SYNC)).unwrap();
        Forge {
            merge: step_script(&workflow, "Merge the upstream commit"),
            publish: step_script(&workflow, "Push the merge, the tags and the releases"),
            dir,
            up_work,
            down_work,
        }
    }

    fn fix(&self) -> PathBuf {
        self.dir.path().join("fix")
    }

    /// A commit on the upstream; returns its id. `green` records a successful ci run for it.
    fn upstream_commit(&self, file: &str, text: &str, green: bool) -> String {
        fs::write(self.up_work.join(file), text).unwrap();
        git(&self.up_work, &["add", "."]);
        git(
            &self.up_work,
            &["commit", "--quiet", "-m", &format!("upstream: {file}")],
        );
        git(
            &self.up_work,
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        );
        let id = git(&self.up_work, &["rev-parse", "HEAD"]);
        if green {
            fs::write(self.fix().join("green").join(&id), "").unwrap();
        }
        id
    }

    fn upstream_tag(&self, tag: &str, commit: &str) {
        git(&self.up_work, &["tag", tag, commit]);
        git(
            &self.up_work,
            &["push", "--quiet", "origin", &format!("refs/tags/{tag}")],
        );
    }

    fn upstream_release(&self, tag: &str, draft: bool) {
        let dir = self.fix().join("up-releases");
        fs::create_dir_all(&dir).unwrap();
        let release = serde_json::json!({
            "tag_name": tag, "draft": draft, "prerelease": false,
            "html_url": format!("https://forge.example/team/up/releases/tag/{tag}"),
            "assets": [{"name": "tool-linux-x86_64", "browser_download_url": format!("https://forge.example/team/up/releases/download/{tag}/tool-linux-x86_64")}]
        });
        fs::write(dir.join(format!("{tag}.json")), release.to_string()).unwrap();
    }

    fn downstream_commit(&self, file: &str, text: &str) -> String {
        fs::create_dir_all(self.down_work.join(file).parent().unwrap()).unwrap();
        fs::write(self.down_work.join(file), text).unwrap();
        git(&self.down_work, &["add", "."]);
        git(
            &self.down_work,
            &["commit", "--quiet", "-m", &format!("downstream: {file}")],
        );
        git(
            &self.down_work,
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        );
        git(&self.down_work, &["rev-parse", "HEAD"])
    }

    fn down_main(&self) -> String {
        git(
            &self.fix().join("forge/team/down.git"),
            &["rev-parse", "refs/heads/main"],
        )
    }

    fn env(&self, upstream_commit: &str, upstream_url: &str) -> Vec<(String, String)> {
        let rewrite = format!("file://{}/forge/", self.fix().display());
        vec![
            (
                "PATH".into(),
                format!(
                    "{}:{}",
                    self.fix().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            ),
            ("FIX".into(), self.fix().display().to_string()),
            ("HOME".into(), self.dir.path().display().to_string()),
            ("TMPDIR".into(), self.dir.path().display().to_string()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
            ("GIT_CONFIG_COUNT".into(), "2".into()),
            (
                "GIT_CONFIG_KEY_0".into(),
                format!("url.{rewrite}.insteadOf"),
            ),
            ("GIT_CONFIG_VALUE_0".into(), "https://forge.example/".into()),
            (
                "GIT_CONFIG_KEY_1".into(),
                format!("url.{rewrite}.insteadOf"),
            ),
            (
                "GIT_CONFIG_VALUE_1".into(),
                "https://public.example/".into(),
            ),
            ("SYNC_SERVER".into(), "https://forge.example".into()),
            ("SYNC_REPOSITORY".into(), "team/down".into()),
            ("SYNC_TOKEN".into(), "job-token".into()),
            ("SYNC_UPSTREAM_COMMIT".into(), upstream_commit.into()),
            ("SYNC_UPSTREAM_URL".into(), upstream_url.into()),
            ("SYNC_UPSTREAM_BRANCH".into(), "main".into()),
            ("SYNC_BRANCH".into(), "main".into()),
        ]
    }

    fn run(&self, script: &str, env: Vec<(String, String)>, env_file: &Path) -> Outcome {
        let file = self.dir.path().join("step.sh");
        fs::write(&file, script).unwrap();
        let output = Command::new("bash")
            .arg(&file)
            .env_clear()
            .envs(env)
            .env("GITHUB_ENV", env_file)
            .output()
            .unwrap();
        Outcome {
            output,
            env: fs::read_to_string(env_file).unwrap_or_default(),
        }
    }

    /// Step 1 for `commit`; the environment file of the run is returned with the outcome.
    fn merge_step(&self, commit: &str) -> Outcome {
        let env_file = self.dir.path().join(format!("env-{commit}"));
        let _ = fs::remove_file(&env_file);
        fs::write(&env_file, "").unwrap();
        self.run(
            &self.merge,
            self.env(commit, "https://forge.example/team/up.git"),
            &env_file,
        )
    }

    /// Step 3 with what step 1 produced.
    fn publish_step(&self, merged: &Outcome) -> Outcome {
        let mut env = self.env("", "https://forge.example/team/up.git");
        for name in ["SYNC_WORK", "SYNC_MERGE", "SYNC_PUSH"] {
            env.push((name.into(), merged.var(name).unwrap_or_default()));
        }
        let env_file = self.dir.path().join("env-publish");
        fs::write(&env_file, "").unwrap();
        self.run(&self.publish, env, &env_file)
    }

    fn requests(&self) -> String {
        fs::read_to_string(self.fix().join("requests.log")).unwrap_or_default()
    }

    fn down_tags(&self) -> String {
        git(&self.fix().join("forge/team/down.git"), &["tag", "--list"])
    }
}

#[test]
fn only_a_declared_downstream_gets_the_workflow() {
    let none = declared("");
    assert!(!none.path().join(SYNC).exists());
    let some = declared(DECLARATION);
    let workflow = fs::read_to_string(some.path().join(SYNC)).unwrap();
    assert!(
        workflow.starts_with("# Generated by ccid render v1; edit ccvl-downstream.json instead.\n")
    );
    // Dispatch only: nothing polls, nothing is triggered by a push, a schedule or an event.
    let triggers =
        &workflow[workflow.find("\non:\n").unwrap() + 1..workflow.find("\nconcurrency:").unwrap()];
    let keys: Vec<&str> = triggers
        .lines()
        .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
        .collect();
    assert_eq!(keys, ["  workflow_dispatch:"], "{triggers}");
    assert!(!workflow.contains("cron:") && !workflow.contains("schedule:"));
    assert!(workflow.contains("runs-on: linux"));
    assert!(workflow.contains("'https://forge.example/team/up.git'"));
    // The checks run on the merge before anything is pushed, through the central ccid.
    assert!(workflow.contains("job-run --job 'downstream-check'"));
    assert!(
        workflow.contains("--sha \"$SYNC_MERGE\"")
            && workflow.contains("--local --source \"$SYNC_WORK\"")
    );
    assert!(workflow.contains("if: ${{ env.SYNC_CHECK == '1' }}"));
    let check = workflow.find("Run downstream-check").unwrap();
    let merge = workflow.find("Merge the upstream commit").unwrap();
    let push = workflow.find("Push the merge").unwrap();
    assert!(merge < check && check < push, "merge, check, push");
    // A second render changes nothing and a check is clean.
    assert!(render(some.path(), true).unwrap().transitional.is_empty());
}

#[test]
fn a_check_accepts_the_missing_workflow_as_transitional_and_a_declaration_without_check_job_is_refused(
) {
    let root = declared(DECLARATION);
    fs::remove_file(root.path().join(SYNC)).unwrap();
    let report = render(root.path(), true).unwrap();
    assert!(report
        .transitional
        .iter()
        .any(|path| path.ends_with("upstream-sync.yml")));
    // The declaration is gone: the generated workflow is obsolete and a check says so.
    fs::write(
        root.path().join(SYNC),
        "# Generated by ccid render v1; edit ccvl-downstream.json instead.\nname: upstream-sync\n",
    )
    .unwrap();
    fs::remove_file(root.path().join("ccvl-downstream.json")).unwrap();
    assert!(render(root.path(), true).is_err());
    render(root.path(), false).unwrap();
    assert!(!root.path().join(SYNC).exists());

    let bare = tempfile::tempdir().unwrap();
    fs::create_dir(bare.path().join(".ci")).unwrap();
    fs::write(
        bare.path().join(".ci/ccid.toml"),
        "schema=1\nproject='demo'\n[render]\nrender_schema=2\n[checks.test]\nkind='commands'\ncommands=[['true']]\n[jobs.verify]\nscheduler='forgejo'\nruns_on='linux'\nchecks=['test']\nworkflow='ci'\ncommand=['ccid:run-declared-checks']\n",
    )
    .unwrap();
    fs::write(bare.path().join("ccvl-downstream.json"), DECLARATION).unwrap();
    let error = render(bare.path(), false).unwrap_err().to_string();
    assert!(error.contains("downstream-check"), "{error}");
}

#[test]
fn an_older_ccid_does_not_take_the_workflow_for_an_obsolete_job_adapter() {
    // The marker differs from the one of the job adapters, so a release that does not know the file leaves it alone.
    let root = declared(DECLARATION);
    let workflow = fs::read_to_string(root.path().join(SYNC)).unwrap();
    assert!(!workflow.starts_with("# Generated by ccid render v1; edit .ci/ccid.toml instead.\n"));
}

#[test]
fn declarations_that_could_reach_a_shell_are_refused() {
    for url in [
        "http://forge.example/team/up.git",
        "https://forge.example/team/up",
        "https://forge.example/team/u'p.git",
        "https://forge.example/$(id)/up.git",
    ] {
        let bad = DECLARATION.replace("https://forge.example/team/up.git", url);
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".ci")).unwrap();
        fs::write(root.path().join(".ci/ccid.toml"), MANIFEST).unwrap();
        fs::write(root.path().join("ccvl-downstream.json"), bad).unwrap();
        assert!(render(root.path(), false).is_err(), "{url}");
    }
}

#[test]
fn a_clean_merge_is_pushed_only_by_the_last_step() {
    let forge = Forge::new();
    let before = forge.down_main();
    let commit = forge.upstream_commit("code.txt", "two\n", true);
    let merged = forge.merge_step(&commit);
    assert!(merged.ok(), "{}", merged.stderr());
    assert_eq!(merged.var("SYNC_CHECK").as_deref(), Some("1"));
    assert_eq!(merged.var("SYNC_PUSH").as_deref(), Some("1"));
    let merge = merged.var("SYNC_MERGE").unwrap();
    assert_ne!(merge, before);
    // Nothing reached the forge yet: the checks run on the local merge first.
    assert_eq!(forge.down_main(), before);
    let work = PathBuf::from(merged.var("SYNC_WORK").unwrap());
    assert_eq!(git(&work, &["rev-parse", "HEAD"]), merge);
    assert_eq!(
        git(&work, &["show", "-s", "--format=%an <%ae>"],),
        "Test Author <author@example.test>"
    );
    let published = forge.publish_step(&merged);
    assert!(published.ok(), "{}", published.stderr());
    assert_eq!(forge.down_main(), merge);
    // The downstream's own commit and the upstream's are both parents' ancestry; the downstream's file is intact.
    let bare = forge.fix().join("forge/team/down.git");
    assert_eq!(git(&bare, &["show", "refs/heads/main:code.txt"]), "two");
    assert_eq!(
        git(&bare, &["show", "refs/heads/main:opportunities/job.txt"]),
        "mine"
    );
    assert_eq!(
        git(
            &bare,
            &["rev-list", "--parents", "-n", "1", "refs/heads/main"]
        )
        .split(' ')
        .count(),
        3
    );
}

#[test]
fn a_conflict_pushes_nothing_and_names_the_files() {
    let forge = Forge::new();
    forge.downstream_commit("code.txt", "downstream edit\n");
    let before = forge.down_main();
    let commit = forge.upstream_commit("code.txt", "upstream edit\n", true);
    let merged = forge.merge_step(&commit);
    assert!(!merged.ok());
    assert!(
        merged.stderr().contains("conflicts") && merged.stderr().contains("code.txt"),
        "{}",
        merged.stderr()
    );
    assert_eq!(merged.var("SYNC_PUSH"), None);
    assert_eq!(forge.down_main(), before);
    assert_eq!(forge.down_tags(), "");
}

#[test]
fn a_commit_without_a_green_ci_run_is_not_merged() {
    let forge = Forge::new();
    let before = forge.down_main();
    let commit = forge.upstream_commit("code.txt", "two\n", false);
    let merged = forge.merge_step(&commit);
    assert!(!merged.ok());
    assert!(
        merged.stderr().contains("no successful ci run"),
        "{}",
        merged.stderr()
    );
    assert_eq!(forge.down_main(), before);
    // And a malformed id never reaches the forge.
    let bad = forge.merge_step("not-a-commit");
    assert!(!bad.ok());
}

#[test]
fn a_repeated_or_superseded_event_changes_nothing() {
    let forge = Forge::new();
    let first = forge.upstream_commit("code.txt", "two\n", true);
    let second = forge.upstream_commit("code.txt", "three\n", true);
    for commit in [&second, &first] {
        let merged = forge.merge_step(commit);
        assert!(merged.ok(), "{}", merged.stderr());
        assert!(forge.publish_step(&merged).ok());
    }
    let main = forge.down_main();
    // The second event had merged both; the first, arriving late, finds its commit contained.
    let again = forge.merge_step(&first);
    assert!(again.ok());
    assert_eq!(again.var("SYNC_CHECK").as_deref(), Some("0"));
    assert_eq!(again.var("SYNC_PUSH").as_deref(), Some("0"));
    assert!(forge.publish_step(&again).ok());
    assert_eq!(forge.down_main(), main);
}

#[test]
fn a_moved_branch_is_refused_not_overwritten() {
    let forge = Forge::new();
    let commit = forge.upstream_commit("code.txt", "two\n", true);
    let merged = forge.merge_step(&commit);
    assert!(merged.ok());
    let moved = forge.downstream_commit("opportunities/other.txt", "later\n");
    let published = forge.publish_step(&merged);
    assert!(!published.ok());
    assert!(
        published.stderr().contains("refused"),
        "{}",
        published.stderr()
    );
    assert_eq!(forge.down_main(), moved);
}

#[test]
fn release_tags_and_releases_follow_the_commit_that_contains_them() {
    let forge = Forge::new();
    let commit = forge.upstream_commit("code.txt", "two\n", true);
    forge.upstream_tag("v0.1.0", &commit);
    forge.upstream_release("v0.1.0", false);
    // Not a release tag: nothing about them travels.
    forge.upstream_tag("runtime-abc", &commit);
    forge.upstream_tag("v0.1.0-rc1", &commit);
    let merged = forge.merge_step(&commit);
    assert!(merged.ok(), "{}", merged.stderr());
    let published = forge.publish_step(&merged);
    assert!(published.ok(), "{}", published.stderr());
    assert_eq!(forge.down_tags(), "v0.1.0");
    let bare = forge.fix().join("forge/team/down.git");
    assert_eq!(
        git(&bare, &["rev-parse", "refs/tags/v0.1.0"]),
        merged.var("SYNC_MERGE").unwrap()
    );
    // The release has no build and no copy of its own: its notes point at the upstream's assets.
    let release: serde_json::Value =
        serde_json::from_slice(&fs::read(forge.fix().join("down-releases/v0.1.0.json")).unwrap())
            .unwrap();
    assert_eq!(release["tag_name"], "v0.1.0");
    assert_eq!(release["name"], "0.1.0");
    assert_eq!(release["draft"], false);
    let body = release["body"].as_str().unwrap();
    assert!(
        body.contains("https://forge.example/team/up/releases/tag/v0.1.0"),
        "{body}"
    );
    assert!(
        body.contains("https://forge.example/team/up/releases/download/v0.1.0/tool-linux-x86_64"),
        "{body}"
    );
    assert!(release.get("assets").is_none());
    // Repeating it creates nothing twice and never moves the tag.
    let again = forge.merge_step(&commit);
    assert!(again.ok());
    assert!(forge.publish_step(&again).ok());
    assert_eq!(
        fs::read_to_string(forge.fix().join("posts.log"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(
        git(&bare, &["rev-parse", "refs/tags/v0.1.0"]),
        merged.var("SYNC_MERGE").unwrap()
    );
}

#[test]
fn a_tag_that_arrives_after_its_commit_lands_on_the_first_commit_that_contains_it() {
    let forge = Forge::new();
    let first = forge.upstream_commit("code.txt", "two\n", true);
    let merged = forge.merge_step(&first);
    assert!(forge.publish_step(&merged).ok());
    let first_merge = merged.var("SYNC_MERGE").unwrap();
    // The release is promoted later: its tag exists only now, on the commit that was already merged.
    forge.upstream_tag("v0.2.0", &first);
    forge.upstream_release("v0.2.0", false);
    let next = forge.upstream_commit("code.txt", "three\n", true);
    let merged = forge.merge_step(&next);
    assert!(merged.ok(), "{}", merged.stderr());
    assert!(forge.publish_step(&merged).ok());
    let bare = forge.fix().join("forge/team/down.git");
    assert_eq!(git(&bare, &["rev-parse", "refs/tags/v0.2.0"]), first_merge);
    assert_ne!(first_merge, merged.var("SYNC_MERGE").unwrap());
}

#[test]
fn a_tag_the_merge_does_not_contain_and_a_draft_release_wait() {
    let forge = Forge::new();
    let first = forge.upstream_commit("code.txt", "two\n", true);
    let later = forge.upstream_commit("code.txt", "three\n", true);
    forge.upstream_tag("v0.3.0", &later);
    forge.upstream_release("v0.3.0", false);
    forge.upstream_tag("v0.2.0", &first);
    forge.upstream_release("v0.2.0", true);
    let merged = forge.merge_step(&first);
    assert!(merged.ok(), "{}", merged.stderr());
    let published = forge.publish_step(&merged);
    assert!(published.ok(), "{}", published.stderr());
    // v0.3.0 is not part of the downstream yet; v0.2.0 is, but its release is still a draft upstream: tag yes, release no.
    assert_eq!(forge.down_tags(), "v0.2.0");
    assert!(!forge.fix().join("posts.log").exists());
}

#[test]
fn the_declared_host_only_names_the_upstream_it_is_read_from_the_forge_that_runs_the_job() {
    let forge = Forge::new();
    let commit = forge.upstream_commit("code.txt", "two\n", true);
    let env_file = forge.dir.path().join("env-other");
    fs::write(&env_file, "").unwrap();
    // The declaration names the public home of the upstream; jobs have no route to it, so the forge that runs the job serves it.
    let outcome = forge.run(
        &forge.merge,
        forge.env(&commit, "https://public.example/team/up.git"),
        &env_file,
    );
    assert!(outcome.ok(), "{}", outcome.stderr());
    let requests = forge.requests();
    assert!(!requests.contains("public.example"), "{requests}");
    assert!(
        requests.contains("GET https://forge.example/api/v1/repos/team/up/actions/runs?head_sha="),
        "{requests}"
    );
    // Every request carries the job token, and only the forge that runs the job ever sees it.
    for line in requests.lines() {
        assert!(
            line.contains("https://forge.example/") && line.ends_with("auth=yes"),
            "{line}"
        );
    }
}

#[test]
fn a_change_outside_the_downstreams_own_paths_is_refused_by_git_alone() {
    let forge = Forge::new();
    // The downstream has altered a file it does not own; the upstream commit itself merges cleanly.
    forge.downstream_commit("tampered.txt", "not mine\n");
    let before = forge.down_main();
    let commit = forge.upstream_commit("code.txt", "two\n", true);
    let merged = forge.merge_step(&commit);
    assert!(!merged.ok());
    assert!(
        merged.stderr().contains("outside the downstream's own")
            && merged.stderr().contains("tampered.txt"),
        "{}",
        merged.stderr()
    );
    assert_eq!(merged.var("SYNC_PUSH"), None);
    assert_eq!(forge.down_main(), before);
    // A path the policy names (a directory prefix or an exact file) is the downstream's own.
    let forge = Forge::new();
    forge.downstream_commit("opportunities/deep/more.txt", "mine too\n");
    let commit = forge.upstream_commit("code.txt", "two\n", true);
    let merged = forge.merge_step(&commit);
    assert!(merged.ok(), "{}", merged.stderr());
}

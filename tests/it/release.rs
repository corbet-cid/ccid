//! `ccid release` against a fake forge: the whole flow (build, legs, promote) with the real binary, the
//! draft rules (size-checked uploads, replacement only of the declared manifests, a version released once)
//! and the refusals of the preflight.

use crate::fake_forge::{Forge, JOB_TOKEN, OWNER_REPO, READ_TOKEN};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const LIBRARY: &str = "schema = 1\nproject = \"demo\"\n\n[render]\nrender_schema = 3\n\n\
    [checks.pack]\nkind = \"commands\"\ncache = false\ncommands = [[\"sh\", \"-c\", \"printf crate-bytes > \\\"$CCID_RELEASE_DIR/demo-1.2.0.crate\\\"; printf notes > \\\"$CCID_RELEASE_DIR/NOTES.txt\\\"\"]]\n\n\
    [checks.test]\nkind = \"commands\"\ncommands = [[\"true\"]]\n\n\
    [jobs.verify]\nscheduler = \"forgejo\"\nchecks = [\"test\"]\nworkflow = \"ci\"\ncommand = [\"ccid:run-declared-checks\"]\n\n\
    [release]\nkind = \"library\"\nversion = \"Cargo.toml\"\nbuild = [\"pack\"]\n";

/// A binary release with both legs and a seal that derives `manifest.json` from the receipts of the legs.
fn binary() -> String {
    String::from(
        "schema = 1\nproject = \"app\"\n\n[render]\nrender_schema = 3\n\n\
    [checks.build]\nkind = \"commands\"\ncache = false\ncommands = [[\"sh\", \"-c\", \"for p in $CCID_RELEASE_PLATFORMS; do printf \\\"bin $p\\\" > \\\"$CCID_RELEASE_DIR/app-$p\\\"; done; printf '{}' > \\\"$CCID_RELEASE_DIR/manifest.json\\\"\"]]\n\n\
    [checks.win]\nkind = \"commands\"\ncache = false\ncommands = [[\"sh\", \"-c\", \"\\\"$CCID_BIN\\\" release fetch app-windows-x86_64 --into \\\"$CCID_RELEASE_DIR\\\" && printf 'windows tested' > \\\"$CCID_RELEASE_DIR/windows.receipt\\\" && \\\"$CCID_BIN\\\" release put \\\"$CCID_RELEASE_DIR/windows.receipt\\\"\"]]\n\n\
    [checks.mac]\nkind = \"commands\"\ncache = false\ncommands = [[\"sh\", \"-c\", \"printf 'macos tested' > \\\"$CCID_RELEASE_DIR/macos.receipt\\\" && \\\"$CCID_BIN\\\" release put \\\"$CCID_RELEASE_DIR/macos.receipt\\\"\"]]\n\n\
    [checks.seal]\nkind = \"commands\"\ncache = false\ncommands = [[\"sh\", \"-c\", \"test -f \\\"$CCID_RELEASE_DIR/windows.receipt\\\" && test -f \\\"$CCID_RELEASE_DIR/macos.receipt\\\" && cat \\\"$CCID_RELEASE_DIR\\\"/*.receipt > \\\"$CCID_RELEASE_DIR/manifest.json\\\"\"]]\n\n\
    [checks.test]\nkind = \"commands\"\ncommands = [[\"true\"]]\n\n\
    [jobs.verify]\nscheduler = \"forgejo\"\nchecks = [\"test\"]\nworkflow = \"ci\"\ncommand = [\"ccid:run-declared-checks\"]\n\n\
    [release]\nkind = \"binary\"\nversion = \"Cargo.toml\"\nplatforms = [\"linux-x86_64\", \"windows-x86_64\", \"macos-x86_64\"]\n\
    build = [\"build\"]\nlegs = { windows = \"win\", macos = \"mac\" }\nseal = [\"seal\"]\nmanifests = [\"manifest.json\"]\n"
    )
}

struct Fixture {
    repo: tempfile::TempDir,
    forge: Forge,
    sha: String,
    state: tempfile::TempDir,
    read_token: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=release-test",
            "-c",
            "user.email=release-test@example.invalid",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

impl Fixture {
    fn new(manifest: &str, version: &str) -> Fixture {
        Fixture::with_files(manifest, version, &[])
    }

    /// [`Fixture::new`] with more files in the one commit.
    fn with_files(manifest: &str, version: &str, files: &[(&str, &str)]) -> Fixture {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        fs::create_dir_all(repo.path().join(".ci")).unwrap();
        fs::write(repo.path().join(".ci/ccid.toml"), manifest).unwrap();
        fs::write(
            repo.path().join("Cargo.toml"),
            format!("[package]\nname = \"demo\"\nversion = \"{version}\"\n"),
        )
        .unwrap();
        for (path, text) in files {
            let full = repo.path().join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, text).unwrap();
        }
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "fixture"]);
        let sha = git(repo.path(), &["rev-parse", "HEAD"]);
        let state = tempfile::tempdir().unwrap();
        let read_token = state.path().join("read-token");
        fs::write(&read_token, format!("{READ_TOKEN}\n")).unwrap();
        Fixture {
            forge: Forge::start(&sha),
            repo,
            sha,
            state,
            read_token,
        }
    }

    /// The fixture with its CI files rendered and committed (what `ccid job-run` checks before it runs).
    fn rendered(manifest: &str, version: &str) -> Fixture {
        let mut fixture = Fixture::new(manifest, version);
        ccid::render::render(fixture.repo.path(), Path::new(".ci/ccid.toml"), false).unwrap();
        git(fixture.repo.path(), &["add", "-A"]);
        git(fixture.repo.path(), &["commit", "-q", "-m", "rendered"]);
        fixture.sha = git(fixture.repo.path(), &["rev-parse", "HEAD"]);
        fixture.forge.state.lock().unwrap().head = fixture.sha.clone();
        fixture
    }

    /// `ccid job-run --local` of one job, as the rendered workflow's shim starts it.
    fn job_run(&self, job: &str) -> Output {
        let source = self.repo.path().display().to_string();
        let owner = OWNER_REPO.split('/').next().unwrap();
        self.ccid(
            &[
                "job-run",
                "--local",
                "--source",
                &source,
                "--sha",
                &self.sha,
                "--job",
                job,
                "--repository",
                OWNER_REPO,
                "--server-url",
                &self.forge.url,
                "--owner",
                owner,
            ],
            &[],
        )
    }

    fn ccid(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let outputs = self.state.path().join("github-output");
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccid"));
        command
            .args(args)
            .current_dir(self.repo.path())
            .env("GITHUB_SERVER_URL", &self.forge.url)
            .env("GITHUB_REPOSITORY", OWNER_REPO)
            .env("CCID_FORGE_TOKEN", JOB_TOKEN)
            .env("CCID_FORGE_READ_TOKEN_FILE", &self.read_token)
            .env("CI_COMMIT_SHA", &self.sha)
            .env("GITHUB_OUTPUT", &outputs)
            .env("CCID_RELEASE_RETRY_DELAY", "0")
            // A runner exports the real forge's API address: the fake forge must be named explicitly.
            .env("GITHUB_API_URL", format!("{}/api/v1", self.forge.url))
            .env("CI_CACHE_ROOT", self.state.path().join("targets"))
            .env("CARGO_TARGET_DIR", self.state.path().join("targets"))
            .env("CI_TIMEOUT", "60")
            .env_remove("CCID_RESULT_CACHE")
            .env_remove("CCID_CACHE_CHILD")
            .env_remove("CCID_TARGET_LOCK_HELD")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            .env_remove("FORGEJO_TOKEN");
        for (key, value) in extra {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn outputs(&self) -> String {
        fs::read_to_string(self.state.path().join("github-output")).unwrap_or_default()
    }

    fn draft_tag(&self) -> String {
        format!("build-{}", self.sha)
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn a_library_release_is_built_drafted_and_promoted_with_its_sums() {
    let fixture = Fixture::new(LIBRARY, "1.2.0");
    let built = fixture.ccid(&["release", "build"], &[]);
    assert!(built.status.success(), "{}", text(&built));
    // One draft named after the commit, holding exactly the files the build left.
    let draft = fixture
        .forge
        .release(&fixture.draft_tag())
        .expect("a draft");
    assert!(draft.draft && draft.target == fixture.sha);
    let mut names = fixture.forge.names(&fixture.draft_tag());
    names.sort();
    assert_eq!(names, vec!["NOTES.txt", "demo-1.2.0.crate"]);
    let outputs = fixture.outputs();
    assert!(
        outputs.contains("released=true") && outputs.contains("legs=\n"),
        "{outputs}"
    );
    // Promotion: the draft becomes v1.2.0, with SHA256SUMS over every other asset.
    let promoted = fixture.ccid(&["release", "promote"], &[]);
    assert!(promoted.status.success(), "{}", text(&promoted));
    assert!(fixture.forge.release(&fixture.draft_tag()).is_none());
    let release = fixture.forge.release("v1.2.0").expect("the public release");
    assert!(!release.draft && release.name == "1.2.0");
    assert_eq!(
        fixture.forge.state.lock().unwrap().tags.get("v1.2.0"),
        Some(&fixture.sha)
    );
    let sums = String::from_utf8(fixture.forge.asset("v1.2.0", "SHA256SUMS").unwrap()).unwrap();
    assert_eq!(
        sums,
        format!(
            "{}  NOTES.txt\n{}  demo-1.2.0.crate\n",
            sha256(b"notes"),
            sha256(b"crate-bytes")
        )
    );
    // A repeat is a no-op, not an error and not a second release.
    let again = fixture.ccid(&["release", "promote"], &[]);
    assert!(again.status.success(), "{}", text(&again));
    assert!(text(&again).contains("public already"), "{}", text(&again));
    // A version is released once.
    let second = fixture.ccid(&["release", "build"], &[]);
    assert!(!second.status.success());
    assert!(text(&second).contains("released once"), "{}", text(&second));
}

#[test]
fn a_minor_release_runs_its_legs_and_promotion_seals_the_manifest() {
    let fixture = Fixture::new(&binary(), "0.3.0");
    let built = fixture.ccid(&["release", "build"], &[]);
    assert!(built.status.success(), "{}", text(&built));
    // The cross files of the set are in the draft; the legs are named for the workflow's `if`.
    let mut names = fixture.forge.names(&fixture.draft_tag());
    names.sort();
    assert_eq!(
        names,
        vec![
            "app-linux-x86_64",
            "app-macos-x86_64",
            "app-windows-x86_64",
            "manifest.json"
        ]
    );
    assert!(
        fixture.outputs().contains("legs=windows macos\n"),
        "{}",
        fixture.outputs()
    );
    // Promotion before the legs ran: the seal refuses the draft, nothing is public.
    let early = fixture.ccid(&["release", "promote"], &[]);
    assert_eq!(early.status.code(), Some(1), "{}", text(&early));
    assert!(fixture.forge.release("v0.3.0").is_none());
    // The legs read the draft with the read credential and leave their evidence in it.
    let windows = fixture.ccid(&["release", "leg", "windows"], &[]);
    assert!(windows.status.success(), "{}", text(&windows));
    let mac = fixture.ccid(&["release", "leg", "macos"], &[]);
    assert!(mac.status.success(), "{}", text(&mac));
    assert_eq!(
        fixture
            .forge
            .asset(&fixture.draft_tag(), "windows.receipt")
            .unwrap(),
        b"windows tested"
    );
    let promoted = fixture.ccid(&["release", "promote"], &[]);
    assert!(promoted.status.success(), "{}", text(&promoted));
    // Only the manifest and the sums were replaced; every other asset kept its bytes.
    assert_eq!(
        fixture.forge.asset("v0.3.0", "manifest.json").unwrap(),
        b"macos testedwindows tested"
    );
    assert_eq!(
        fixture.forge.asset("v0.3.0", "app-linux-x86_64").unwrap(),
        b"bin linux-x86_64"
    );
    let sums = String::from_utf8(fixture.forge.asset("v0.3.0", "SHA256SUMS").unwrap()).unwrap();
    assert_eq!(sums.lines().count(), 6, "{sums}");
    assert!(sums.contains(&format!(
        "{}  manifest.json",
        sha256(b"macos testedwindows tested")
    )));
    assert!(!sums.contains("SHA256SUMS"));
}

#[test]
fn a_patch_release_holds_linux_alone_and_has_no_leg() {
    let fixture = Fixture::new(&binary(), "0.3.1");
    let built = fixture.ccid(&["release", "build"], &[]);
    assert!(built.status.success(), "{}", text(&built));
    let mut names = fixture.forge.names(&fixture.draft_tag());
    names.sort();
    assert_eq!(names, vec!["app-linux-x86_64", "manifest.json"]);
    assert!(
        fixture.outputs().contains("legs=\n"),
        "{}",
        fixture.outputs()
    );
    let leg = fixture.ccid(&["release", "leg", "windows"], &[]);
    assert!(leg.status.success());
    assert!(text(&leg).contains("no windows leg"), "{}", text(&leg));
    // With no leg evidence the seal of this fixture refuses; a patch has nothing to wait for in general,
    // so the version, the class and the platform set are what the commands report.
    let version = fixture.ccid(&["release", "version"], &[]);
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "0.3.1 patch"
    );
    let platforms = fixture.ccid(&["release", "platforms"], &[]);
    assert_eq!(
        String::from_utf8_lossy(&platforms.stdout).trim(),
        "linux-x86_64"
    );
}

#[test]
fn the_preflight_refuses_a_stale_source_a_taken_version_and_a_red_verdict() {
    let fixture = Fixture::new(LIBRARY, "1.2.0");
    assert!(fixture
        .ccid(&["release", "preflight"], &[])
        .status
        .success());
    // Main moved on.
    fixture.forge.state.lock().unwrap().head = "f".repeat(40);
    let stale = fixture.ccid(&["release", "build"], &[]);
    assert!(text(&stale).contains("no longer main"), "{}", text(&stale));
    fixture.forge.state.lock().unwrap().head = fixture.sha.clone();
    // A red ccid/verdict.
    fixture
        .forge
        .state
        .lock()
        .unwrap()
        .statuses
        .push(("ccid/verdict".into(), "failure".into()));
    let red = fixture.ccid(&["release", "preflight"], &[]);
    assert!(text(&red).contains("verdict"), "{}", text(&red));
    // A status of another context is not the verdict.
    fixture.forge.state.lock().unwrap().statuses = vec![("ci / release".into(), "failure".into())];
    assert!(fixture
        .ccid(&["release", "preflight"], &[])
        .status
        .success());
    // The tag alone (no release) already makes the version taken.
    fixture
        .forge
        .state
        .lock()
        .unwrap()
        .tags
        .insert("v1.2.0".into(), "e".repeat(40));
    let taken = fixture.ccid(&["release", "preflight"], &[]);
    assert!(
        text(&taken).contains("a tag v1.2.0 exists"),
        "{}",
        text(&taken)
    );
}

#[test]
fn an_interrupted_upload_leaves_a_part_that_is_replaced_and_a_rerun_resumes() {
    let fixture = Fixture::new(LIBRARY, "1.2.0");
    fixture.forge.state.lock().unwrap().truncate_next_upload = Some(3);
    let built = fixture.ccid(&["release", "build"], &[]);
    assert!(built.status.success(), "{}", text(&built));
    assert!(
        text(&built).contains("removing the truncated draft asset"),
        "{}",
        text(&built)
    );
    assert_eq!(
        fixture
            .forge
            .asset(&fixture.draft_tag(), "NOTES.txt")
            .unwrap(),
        b"notes"
    );
    // The same commit again: the draft's bytes are the run's bytes, nothing is uploaded twice.
    let uploads = |fixture: &Fixture| {
        fixture
            .forge
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r.starts_with("POST") && r.ends_with("/assets"))
            .count()
    };
    let before = uploads(&fixture);
    let again = fixture.ccid(&["release", "build"], &[]);
    assert!(again.status.success(), "{}", text(&again));
    assert_eq!(uploads(&fixture), before);
}

#[test]
fn a_draft_holding_other_bytes_is_refused_and_discarding_it_clears_the_way() {
    let fixture = Fixture::new(LIBRARY, "1.2.0");
    assert!(fixture.ccid(&["release", "build"], &[]).status.success());
    {
        let mut state = fixture.forge.state.lock().unwrap();
        let release = state.releases.iter_mut().find(|r| r.draft).unwrap();
        release
            .assets
            .iter_mut()
            .find(|a| a.name == "NOTES.txt")
            .unwrap()
            .bytes = b"other".to_vec();
    }
    let refused = fixture.ccid(&["release", "build"], &[]);
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("differs from this run's"),
        "{}",
        text(&refused)
    );
    let discarded = fixture.ccid(&["release", "discard"], &[]);
    assert!(discarded.status.success(), "{}", text(&discarded));
    assert!(fixture.forge.release(&fixture.draft_tag()).is_none());
    assert!(fixture.ccid(&["release", "build"], &[]).status.success());
}

#[test]
fn a_seal_that_touches_an_asset_other_than_its_manifests_stops_the_promotion() {
    let manifest = binary().replace(
        "cat \\\"$CCID_RELEASE_DIR\\\"/*.receipt > \\\"$CCID_RELEASE_DIR/manifest.json\\\"",
        "printf tampered > \\\"$CCID_RELEASE_DIR/app-linux-x86_64\\\"",
    );
    assert_ne!(manifest, binary(), "the fixture's seal was rewritten");
    let fixture = Fixture::new(&manifest, "0.3.0");
    assert!(fixture.ccid(&["release", "build"], &[]).status.success());
    assert!(fixture
        .ccid(&["release", "leg", "windows"], &[])
        .status
        .success());
    assert!(fixture
        .ccid(&["release", "leg", "macos"], &[])
        .status
        .success());
    let promoted = fixture.ccid(&["release", "promote"], &[]);
    assert!(!promoted.status.success());
    assert!(
        text(&promoted).contains("changed app-linux-x86_64"),
        "{}",
        text(&promoted)
    );
    assert!(fixture.forge.release("v0.3.0").is_none());
    assert_eq!(
        fixture
            .forge
            .asset(&fixture.draft_tag(), "app-linux-x86_64")
            .unwrap(),
        b"bin linux-x86_64"
    );
}

#[test]
fn the_job_token_cannot_read_a_draft_and_the_read_credential_can() {
    let fixture = Fixture::new(&binary(), "0.3.0");
    assert!(fixture.ccid(&["release", "build"], &[]).status.success());
    let into = fixture.state.path().join("fetched");
    let denied = fixture.ccid(
        &[
            "release",
            "fetch",
            "app-linux-x86_64",
            "--into",
            into.to_str().unwrap(),
        ],
        &[("CCID_FORGE_READ_TOKEN_FILE", "/nonexistent")],
    );
    assert!(!denied.status.success());
    assert!(
        text(&denied).contains("read credential"),
        "{}",
        text(&denied)
    );
    let fetched = fixture.ccid(
        &[
            "release",
            "fetch",
            "app-linux-x86_64",
            "--into",
            into.to_str().unwrap(),
        ],
        &[],
    );
    assert!(fetched.status.success(), "{}", text(&fetched));
    assert_eq!(
        fs::read(into.join("app-linux-x86_64")).unwrap(),
        b"bin linux-x86_64"
    );
}

#[test]
fn a_leg_never_sees_a_published_release_and_a_public_draft_name_is_final() {
    let fixture = Fixture::new(&binary(), "0.3.0");
    assert!(fixture.ccid(&["release", "build"], &[]).status.success());
    assert!(fixture
        .ccid(&["release", "leg", "windows"], &[])
        .status
        .success());
    assert!(fixture
        .ccid(&["release", "leg", "macos"], &[])
        .status
        .success());
    assert!(fixture.ccid(&["release", "promote"], &[]).status.success());
    // After promotion the draft is gone: a leg run again finds nothing to leave evidence in.
    let late = fixture.ccid(&["release", "leg", "windows"], &[]);
    assert!(!late.status.success());
    assert!(text(&late).contains("no draft"), "{}", text(&late));
    // And a public release is never written to.
    let file = fixture.state.path().join("late.receipt");
    fs::write(&file, "late").unwrap();
    let put = fixture.ccid(&["release", "put", file.to_str().unwrap()], &[]);
    assert!(!put.status.success());
}

#[test]
fn the_release_jobs_of_a_rendered_workflow_run_as_ccid_release_commands() {
    let fixture = Fixture::rendered(&binary(), "0.3.0");
    let built = fixture.job_run("release");
    assert!(built.status.success(), "{}", text(&built));
    assert!(fixture.forge.release(&fixture.draft_tag()).is_some());
    assert!(
        fixture.outputs().contains("legs=windows macos\n"),
        "{}",
        fixture.outputs()
    );
    for leg in ["release-windows", "release-macos"] {
        let ran = fixture.job_run(leg);
        assert!(ran.status.success(), "{leg}: {}", text(&ran));
    }
    let promoted = fixture.job_run("release-promote");
    assert!(promoted.status.success(), "{}", text(&promoted));
    assert!(fixture.forge.release("v0.3.0").is_some());
    // A job the manifest does not plan is unknown, not a release job.
    let unknown = fixture.job_run("release-linux");
    assert!(!unknown.status.success());
    assert!(text(&unknown).contains("Unknown job"), "{}", text(&unknown));
}

#[test]
fn a_successful_upload_is_never_followed_by_a_pause() {
    let fixture = Fixture::new(LIBRARY, "1.2.0");
    let started = std::time::Instant::now();
    // A long retry delay: only a failed attempt may wait for it.
    let built = fixture.ccid(&["release", "build"], &[("CCID_RELEASE_RETRY_DELAY", "30")]);
    assert!(built.status.success(), "{}", text(&built));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "the build paused after successful uploads: {:?}",
        started.elapsed()
    );
}

// ---- the pack check (kind = "pack"): one pack, before anything is public ----------------------------------------------
// Unix only: the fake package tools are shell scripts (ccid's Windows coverage is a compile gate, not a test leg).
#[cfg(unix)]
mod pack_release {
    use super::*;

    const PACKED: &str = "schema = 1\nproject = \"demo\"\n\n[render]\nrender_schema = 3\n\n\
        [checks.pack]\nkind = \"pack\"\ncrates = \".\"\nnpm = \"js\"\npypi = \"py\"\n\n\
        [checks.test]\nkind = \"commands\"\ncommands = [[\"true\"]]\n\n\
        [jobs.verify]\nscheduler = \"forgejo\"\nchecks = [\"test\"]\nworkflow = \"ci\"\ncommand = [\"ccid:run-declared-checks\"]\n\n\
        [release]\nkind = \"library\"\nversion = \"Cargo.toml\"\nbuild = [\"pack\"]\nregistries = [\"crates\", \"npm\", \"pypi\"]\n";

    /// The package files of the fixture: an npm package with a bun lock and a Python package with its hash-locked backend.
    fn package_files(npm_version: &str) -> Vec<(&'static str, String)> {
        vec![
            (
                "js/package.json",
                format!("{{\"name\":\"@demo/demo\",\"version\":\"{npm_version}\"}}"),
            ),
            ("js/bun.lock", String::new()),
            (
                "py/pyproject.toml",
                "[project]\nname = \"demo\"\nversion = \"1.2.0\"\n".to_owned(),
            ),
            (
                "py/build-constraints.txt",
                "hatchling==1.0.0 --hash=sha256:00\n".to_owned(),
            ),
        ]
    }

    /// Fake package tools that leave the files the real ones leave. `FAKE_FAIL=<tool>` makes one tool fail and
    /// `FAKE_WHEEL` names the wheel the fake `uv` writes.
    fn fake_tools(dir: &Path) -> String {
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            fs::write(
                &path,
                format!("#!/bin/sh\n[ \"${{FAKE_FAIL:-}}\" = {name} ] && exit 1\n{body}"),
            )
            .unwrap();
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
            fs::set_permissions(&path, permissions).unwrap();
        };
        script(
            "cargo",
            "case \"$1\" in fetch) exit 0;; package) mkdir -p \"$CARGO_TARGET_DIR/package\"; printf crate-bytes > \"$CARGO_TARGET_DIR/package/demo-1.2.0.crate\"; exit 0;; esac\nexit 3\n",
        );
        script(
            "bun",
            "printf '%s\\n' \"$BUN_INSTALL_CACHE_DIR\" >> \"$0.env\"\nexit 0\n",
        );
        script(
            "npm",
            "printf '%s\\n' \"$npm_config_cache\" >> \"$0.env\"\n\
             dest=; while [ $# -gt 0 ]; do [ \"$1\" = --pack-destination ] && dest=$2; shift; done\n\
             d=$(mktemp -d); mkdir \"$d/package\"; printf '{\"name\":\"@demo/demo\",\"version\":\"1.2.0\"}' > \"$d/package/package.json\"\n\
             tar -czf \"$dest/demo-demo-1.2.0.tgz\" -C \"$d\" package\n",
        );
        script(
            "uv",
            "printf '%s %s %s\\n' \"$UV_CACHE_DIR\" \"$UV_PYTHON_DOWNLOADS\" \"$UV_PYTHON_PREFERENCE\" >> \"$0.env\"\n\
             out=; while [ $# -gt 0 ]; do [ \"$1\" = --out-dir ] && out=$2; shift; done\n\
             printf '*' > \"$out/.gitignore\"; printf sdist > \"$out/demo-1.2.0.tar.gz\"; printf wheel > \"$out/${FAKE_WHEEL:-demo-1.2.0-py3-none-any.whl}\"\n",
        );
        format!(
            "{}:{}",
            dir.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    /// A fixture, the extra environment of the build, and the text the refusal must name.
    type Case<'a> = (Fixture, Vec<(&'a str, &'a str)>, &'a str);

    fn packed_fixture(npm_version: &str, with_constraints: bool) -> Fixture {
        let files = package_files(npm_version);
        let files: Vec<(&str, &str)> = files
            .iter()
            .filter(|(path, _)| with_constraints || !path.ends_with("build-constraints.txt"))
            .map(|(path, text)| (*path, text.as_str()))
            .collect();
        Fixture::with_files(PACKED, "1.2.0", &files)
    }

    #[test]
    fn a_pack_release_drafts_exactly_the_packed_files_of_every_registry() {
        let tools = tempfile::tempdir().unwrap();
        let path = fake_tools(tools.path());
        let fixture = packed_fixture("1.2.0", true);
        // A runner that names shared package caches and lets uv fetch interpreters: the pack ignores both.
        let built = fixture.ccid(
            &["release", "build"],
            &[
                ("PATH", &path),
                ("BUN_INSTALL_CACHE_DIR", "/shared/bun"),
                ("UV_CACHE_DIR", "/shared/uv"),
                ("npm_config_cache", "/shared/npm"),
                ("UV_PYTHON_DOWNLOADS", "automatic"),
                ("UV_PYTHON_PREFERENCE", "managed"),
            ],
        );
        assert!(built.status.success(), "{}", text(&built));
        let seen = |tool: &str| {
            fs::read_to_string(tools.path().join(format!("{tool}.env")))
                .unwrap_or_else(|error| panic!("the fake {tool} was not run: {error}"))
        };
        let private = |line: &str, directory: &str| {
            let path = line.trim_end();
            path.contains("/ccid-pack-") && path.ends_with(directory)
        };
        assert!(private(&seen("bun"), "/bun-cache"), "{}", seen("bun"));
        assert!(private(&seen("npm"), "/npm-cache"), "{}", seen("npm"));
        let uv = seen("uv");
        let uv: Vec<&str> = uv.split_whitespace().collect();
        assert!(
            uv.len() == 3
                && private(uv[0], "/uv-cache")
                && uv[1] == "never"
                && uv[2] == "only-system",
            "{uv:?}"
        );
        let mut names = fixture.forge.names(&fixture.draft_tag());
        names.sort();
        assert_eq!(
            names,
            vec![
                "demo-1.2.0-py3-none-any.whl",
                "demo-1.2.0.crate",
                "demo-1.2.0.tar.gz",
                "demo-demo-1.2.0.tgz"
            ]
        );
        assert_eq!(
            fixture
                .forge
                .asset(&fixture.draft_tag(), "demo-1.2.0.crate")
                .unwrap(),
            b"crate-bytes"
        );
        // The scratch marker uv leaves is not a release file.
        assert!(!names.iter().any(|name| name.starts_with('.')));
    }

    #[test]
    fn a_pack_that_cannot_pack_stops_the_release_before_a_draft_exists() {
        let tools = tempfile::tempdir().unwrap();
        let path = fake_tools(tools.path());
        let cases: Vec<Case> = vec![
            // a bump forgotten in package.json
            (
                packed_fixture("1.1.0", true),
                vec![("PATH", &path)],
                "js/package.json says 1.1.0",
            ),
            // the hash-locked backend pins are missing
            (
                packed_fixture("1.2.0", false),
                vec![("PATH", &path)],
                "py/build-constraints.txt is missing",
            ),
            // a tool fails (a broken verify build of the crate)
            (
                packed_fixture("1.2.0", true),
                vec![("PATH", &path), ("FAKE_FAIL", "cargo")],
                "cargo",
            ),
            // a native wheel is a binary with platforms, not a library pack
            (
                packed_fixture("1.2.0", true),
                vec![
                    ("PATH", &path),
                    ("FAKE_WHEEL", "demo-1.2.0-cp312-linux_x86_64.whl"),
                ],
                "cp312",
            ),
        ];
        for (fixture, environment, named) in cases {
            let built = fixture.ccid(&["release", "build"], &environment);
            assert!(!built.status.success(), "{named}: {}", text(&built));
            assert!(text(&built).contains(named), "{named}: {}", text(&built));
            assert!(
                fixture.forge.release(&fixture.draft_tag()).is_none(),
                "{named}: a draft was created"
            );
        }
    }

    #[test]
    fn a_pack_check_is_refused_in_a_job_and_the_release_rules_hold() {
        let in_job = PACKED.replace("checks = [\"test\"]", "checks = [\"test\", \"pack\"]");
        let fixture = Fixture::new(&in_job, "1.2.0");
        let refused = fixture.ccid(
            &[
                "job-run",
                "--local",
                "--source",
                &fixture.repo.path().display().to_string(),
                "--sha",
                &fixture.sha,
                "--job",
                "verify",
                "--repository",
                OWNER_REPO,
                "--server-url",
                &fixture.forge.url,
                "--owner",
                "o",
            ],
            &[],
        );
        assert!(!refused.status.success());
        assert!(
            text(&refused).contains("a pack runs in the release run only"),
            "{}",
            text(&refused)
        );
        // registries and keys must match: pypi is published but has no key.
        let unpacked = PACKED.replace("pypi = \"py\"\n", "");
        let fixture = Fixture::new(&unpacked, "1.2.0");
        let version = fixture.ccid(&["release", "version"], &[]);
        assert!(!version.status.success());
        assert!(
            text(&version).contains("published, never packed"),
            "{}",
            text(&version)
        );
    }
}

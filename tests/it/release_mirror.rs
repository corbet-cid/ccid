//! The release mirror: the rendered provider files copy a Forgejo release downstream.
//!
//! The rendered scripts run for real against a fake `curl` (a small Forgejo, GitHub, GitLab and
//! Bitbucket in files, `tests/it/fixtures/release-mirror-fake-curl`), so the whole path is
//! exercised without a network: the pointer, the checksum verification, the publication and its
//! repetition. The tests need `bash`, `git` and `jq`.

use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

const SOURCE: &str = "https://git.example/team/demo";
const MANIFEST: &str = "schema=1\nproject='demo'\n[render]\nrender_schema=2\n\
    [checks.test]\nkind='commands'\ncommands=[['true']]\n\
    [jobs.verify]\nscheduler='forgejo'\nruns_on='linux'\nchecks=['test']\nworkflow='ci'\ncommand=['ccid:run-declared-checks']\n";

fn render(root: &Path, check: bool) -> ccid::Result<ccid::render::Report> {
    ccid::render::render(root, Path::new(".ci/ccid.toml"), check)
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A repository whose `origin` is `url` (none when empty), manifest committed.
fn repository(origin: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".ci")).unwrap();
    fs::write(root.path().join(".ci/ccid.toml"), MANIFEST).unwrap();
    git(root.path(), &["init", "--quiet"]);
    if !origin.is_empty() {
        git(root.path(), &["remote", "add", "origin", origin]);
    }
    root
}

fn rendered(origin: &str) -> tempfile::TempDir {
    let root = repository(origin);
    render(root.path(), false).unwrap();
    root
}

/// The script of the first `run: |` / `- |` block of a rendered provider file, dedented.
fn script(root: &Path, relative: &str) -> String {
    let text = fs::read_to_string(root.join(relative)).unwrap();
    let mut lines = text.lines();
    let _ = lines
        .by_ref()
        .find(|line| line.trim_end().ends_with(": |") || line.trim() == "- |")
        .unwrap_or_else(|| panic!("{relative} has no script block"));
    let body: Vec<&str> = lines.collect();
    let indent = body
        .iter()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .unwrap();
    body.iter()
        .map(|line| line.get(indent..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

struct World {
    dir: tempfile::TempDir,
}

struct Outcome {
    code: i32,
    stderr: String,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        for sub in [
            "fix/forge/tags",
            "fix/forge/files",
            "fix/gh",
            "fix/gl",
            "fix/bb",
            "bin",
        ] {
            fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        // The pointer redirects to the forge that serves the API.
        fs::write(
            dir.path().join("fix/redirect"),
            "https://forge.example/team/demo",
        )
        .unwrap();
        let shim = dir.path().join("bin/curl");
        fs::copy("tests/it/fixtures/release-mirror-fake-curl", &shim).unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        World { dir }
    }

    fn fix(&self) -> PathBuf {
        self.dir.path().join("fix")
    }

    /// A published Forgejo release `tag` with these files, `SHA256SUMS` over the listed ones.
    fn release(&self, tag: &str, files: &[(&str, &[u8])], summed: &[&str], prerelease: bool) {
        let mut sums = String::new();
        let mut assets = Vec::new();
        for (name, bytes) in files {
            fs::write(self.fix().join("forge/files").join(name), bytes).unwrap();
            if summed.contains(name) {
                sums.push_str(&format!("{}  {name}\n", sha(bytes)));
            }
        }
        let mut all: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|(name, bytes)| ((*name).to_owned(), bytes.to_vec()))
            .collect();
        if !sums.is_empty() {
            fs::write(self.fix().join("forge/files/SHA256SUMS"), &sums).unwrap();
            all.push(("SHA256SUMS".to_owned(), sums.into_bytes()));
        }
        for (name, bytes) in &all {
            assets.push(json!({
                "name": name,
                "size": bytes.len(),
                "browser_download_url": format!("https://forge.example/team/demo/releases/download/{tag}/{name}"),
            }));
        }
        let release = json!({
            "tag_name": tag, "name": format!("demo {tag}"), "body": "Notes for the release.\n",
            "html_url": format!("https://forge.example/team/demo/releases/tag/{tag}"),
            "draft": false, "prerelease": prerelease, "assets": assets,
        });
        fs::write(
            self.fix().join(format!("forge/tags/{tag}.json")),
            release.to_string(),
        )
        .unwrap();
    }

    fn edit_release(&self, tag: &str, edit: impl FnOnce(&mut serde_json::Value)) {
        let path = self.fix().join(format!("forge/tags/{tag}.json"));
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        edit(&mut value);
        fs::write(path, value.to_string()).unwrap();
    }

    fn run(&self, shell: &[&str], script: &str, env: &[(&str, &str)]) -> Outcome {
        let file = self.dir.path().join("script.sh");
        fs::write(&file, script).unwrap();
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let output = Command::new(shell[0])
            .args(&shell[1..])
            .arg(&file)
            .env("PATH", path)
            .env("FIX", self.fix())
            .env("TMPDIR", self.dir.path())
            .envs(env.iter().copied())
            .output()
            .unwrap();
        Outcome {
            code: output.status.code().unwrap(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.fix().join("calls.log")).unwrap_or_default()
    }

    fn forge_auth(&self) -> String {
        fs::read_to_string(self.fix().join("forge-auth.log")).unwrap_or_default()
    }

    fn written(&self) -> usize {
        self.calls()
            .lines()
            .filter(|line| {
                (line.starts_with("POST ")
                    || line.starts_with("PUT ")
                    || line.starts_with("PATCH ")
                    || line.starts_with("DELETE "))
                    && !line.contains("forge.example")
            })
            .count()
    }
}

const GITHUB_SHELL: &[&str] = &["bash", "-e", "-o", "pipefail"];
const GITHUB_ENV: &[(&str, &str)] = &[
    ("GH_TOKEN", "downstream-secret"),
    ("GITHUB_REPOSITORY", "o/r"),
    ("GITHUB_API_URL", "https://api.github.test"),
    ("GITHUB_UPLOAD_URL", "https://uploads.github.test"),
    ("MIRROR_TAG", "v0.1.0"),
];

fn github_script() -> String {
    let root = rendered(SOURCE);
    script(root.path(), ".github/workflows/release-mirror.yml")
}

const BINARY: &[u8] = b"binary bytes of the release\n";
const OTHER: &[u8] = b"a second file\n";

#[test]
fn the_rendered_files_name_the_forge_source_and_are_checked_like_the_other_adapters() {
    let root = repository(SOURCE);
    let report = render(root.path(), false).unwrap();
    for relative in [".github/workflows/release-mirror.yml", ".gitlab-ci.yml"] {
        assert!(
            report.files.iter().any(|path| path == Path::new(relative)),
            "{relative}"
        );
        let text = fs::read_to_string(root.path().join(relative)).unwrap();
        assert!(text.starts_with("# Generated by ccid render v1; edit .ci/ccid.toml instead.\n"));
        assert!(
            text.contains(&format!("MIRROR_SOURCE='{SOURCE}'")),
            "{relative}"
        );
        assert!(
            !text.contains("CCID_MIRROR"),
            "{relative} keeps a placeholder"
        );
    }
    assert!(!root.path().join("bitbucket-pipelines.yml").exists());
    let after = render(root.path(), true).unwrap();
    assert!(after.transitional.is_empty());
    // Any edit of a generated file is drift.
    let path = root.path().join(".github/workflows/release-mirror.yml");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{text}# edited\n")).unwrap();
    let error = render(root.path(), true).unwrap_err().to_string();
    assert!(
        error.contains("drifted") && error.contains("release-mirror.yml"),
        "{error}"
    );
}

#[test]
fn a_check_accepts_the_absence_of_the_introduced_files_and_a_render_writes_them() {
    let root = rendered(SOURCE);
    fs::remove_file(root.path().join(".github/workflows/release-mirror.yml")).unwrap();
    fs::remove_file(root.path().join(".gitlab-ci.yml")).unwrap();
    let report = render(root.path(), true).unwrap();
    assert_eq!(
        report.transitional,
        [
            PathBuf::from(".github/workflows/release-mirror.yml"),
            PathBuf::from(".gitlab-ci.yml")
        ]
    );
    assert!(
        !root
            .path()
            .join(".github/workflows/release-mirror.yml")
            .exists(),
        "a check writes nothing"
    );
    render(root.path(), false).unwrap();
    assert!(render(root.path(), true).unwrap().transitional.is_empty());
}

#[test]
fn the_forge_comes_from_origin_and_a_recorded_host_stays_whoever_renders() {
    let none = rendered("");
    assert!(
        !none.path().join(".github").exists(),
        "no origin, no source, no files"
    );
    assert!(!none.path().join(".gitlab-ci.yml").exists());
    for origin in [
        "git@git.example:team/demo.git",
        "https://token@git.example/team/demo.git",
    ] {
        let root = rendered(origin);
        let text =
            fs::read_to_string(root.path().join(".github/workflows/release-mirror.yml")).unwrap();
        assert!(
            text.contains(&format!("MIRROR_SOURCE='{SOURCE}'")),
            "{origin}"
        );
        assert!(
            !text.contains("token@"),
            "credentials never reach a rendered file"
        );
    }
    // A runner's origin spells the forge differently; the recorded spelling is kept and the check passes.
    let root = rendered(SOURCE);
    git(
        root.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "https://forge.example/team/demo.git",
        ],
    );
    assert!(render(root.path(), true).unwrap().transitional.is_empty());
    render(root.path(), false).unwrap();
    let text =
        fs::read_to_string(root.path().join(".github/workflows/release-mirror.yml")).unwrap();
    assert!(text.contains(&format!("MIRROR_SOURCE='{SOURCE}'")));
    // A repository that moved to another path is rendered for its new path.
    git(
        root.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "https://forge.example/other/renamed.git",
        ],
    );
    assert!(render(root.path(), true).is_err());
    render(root.path(), false).unwrap();
    let text =
        fs::read_to_string(root.path().join(".github/workflows/release-mirror.yml")).unwrap();
    assert!(text.contains("MIRROR_SOURCE='https://forge.example/other/renamed'"));
}

#[test]
fn a_hand_written_gitlab_pipeline_is_left_alone() {
    let root = repository(SOURCE);
    fs::write(
        root.path().join(".gitlab-ci.yml"),
        "build:\n  script: [make]\n",
    )
    .unwrap();
    let report = render(root.path(), false).unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join(".gitlab-ci.yml")).unwrap(),
        "build:\n  script: [make]\n"
    );
    assert!(!report
        .files
        .iter()
        .any(|path| path == Path::new(".gitlab-ci.yml")));
    assert!(root
        .path()
        .join(".github/workflows/release-mirror.yml")
        .exists());
    assert!(render(root.path(), true).is_ok());
}

#[test]
fn the_scripts_are_valid_shell() {
    let root = rendered(SOURCE);
    for (relative, shell) in [
        (".github/workflows/release-mirror.yml", "bash"),
        (".gitlab-ci.yml", "sh"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("script.sh");
        fs::write(&file, script(root.path(), relative)).unwrap();
        let status = Command::new(shell).arg("-n").arg(&file).status().unwrap();
        assert!(status.success(), "{relative}");
    }
}

#[test]
fn github_copies_a_verified_release_and_repeating_it_changes_nothing() {
    let script = github_script();
    let world = World::new();
    world.release(
        "v0.1.0",
        &[("demo-linux", BINARY), ("demo-windows.exe", OTHER)],
        &["demo-linux", "demo-windows.exe"],
        false,
    );
    let first = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(first.code, 0, "{}", first.stderr);
    let assets: serde_json::Value =
        serde_json::from_slice(&fs::read(world.fix().join("gh/assets.json")).unwrap()).unwrap();
    let mut names: Vec<&str> = assets
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["SHA256SUMS", "demo-linux", "demo-windows.exe"]);
    let release: serde_json::Value =
        serde_json::from_slice(&fs::read(world.fix().join("gh/releases.json")).unwrap()).unwrap();
    assert_eq!(
        release[0]["draft"], false,
        "published only after every asset is here"
    );
    assert_eq!(release[0]["name"], "demo v0.1.0");
    assert_eq!(release[0]["prerelease"], false);
    assert_eq!(release[0]["body"], "Notes for the release.\n");
    let written = world.written();
    let again = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert_eq!(
        world.written(),
        written,
        "a second run writes nothing: {}",
        again.stderr
    );
}

#[test]
fn github_carries_the_prerelease_flag() {
    let script = github_script();
    let world = World::new();
    world.release(
        "v0.2.0-rc.1",
        &[("demo-linux", BINARY)],
        &["demo-linux"],
        true,
    );
    let env: Vec<(&str, &str)> = GITHUB_ENV
        .iter()
        .map(|&(k, v)| {
            if k == "MIRROR_TAG" {
                (k, "v0.2.0-rc.1")
            } else {
                (k, v)
            }
        })
        .collect();
    let outcome = world.run(GITHUB_SHELL, &script, &env);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let release: serde_json::Value =
        serde_json::from_slice(&fs::read(world.fix().join("gh/releases.json")).unwrap()).unwrap();
    assert_eq!(release[0]["prerelease"], true);
}

#[test]
fn a_tag_without_a_published_forgejo_release_ends_at_once() {
    let script = github_script();
    let world = World::new();
    let none = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(none.code, 0, "{}", none.stderr);
    assert_eq!(world.written(), 0);
    assert!(
        !world.calls().contains("api.github.test"),
        "{}",
        world.calls()
    );
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    world.edit_release("v0.1.0", |value| value["draft"] = json!(true));
    let draft = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(draft.code, 0, "{}", draft.stderr);
    assert_eq!(world.written(), 0);
}

#[test]
fn nothing_is_published_when_the_checksums_do_not_hold() {
    let script = github_script();
    // The file differs from its checksum.
    let world = World::new();
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    fs::write(
        world.fix().join("forge/files/demo-linux"),
        b"BINARY bytes of the release\n",
    )
    .unwrap();
    let tampered = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(tampered.code, 1, "{}", tampered.stderr);
    assert!(tampered.stderr.contains("sha256"), "{}", tampered.stderr);
    assert_eq!(world.written(), 0);
    // A file without an entry in the checksum file.
    let world = World::new();
    world.release(
        "v0.1.0",
        &[("demo-linux", BINARY), ("extra", OTHER)],
        &["demo-linux"],
        false,
    );
    let unlisted = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(unlisted.code, 1, "{}", unlisted.stderr);
    assert!(
        unlisted.stderr.contains("extra has no entry"),
        "{}",
        unlisted.stderr
    );
    assert_eq!(world.written(), 0);
    // Assets and no checksum file at all.
    let world = World::new();
    world.release("v0.1.0", &[("demo-linux", BINARY)], &[], false);
    let bare = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(bare.code, 1, "{}", bare.stderr);
    assert!(bare.stderr.contains("no checksum file"), "{}", bare.stderr);
    assert_eq!(world.written(), 0);
    // A truncated download.
    let world = World::new();
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    world.edit_release("v0.1.0", |value| value["assets"][0]["size"] = json!(9999));
    let short = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(short.code, 1, "{}", short.stderr);
    assert!(
        short.stderr.contains("bytes downloaded"),
        "{}",
        short.stderr
    );
    assert_eq!(world.written(), 0);
    // A download from a host the forge does not declare for itself.
    let world = World::new();
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    world.edit_release("v0.1.0", |value| {
        value["assets"][0]["browser_download_url"] =
            json!("https://evil.example/team/demo/releases/download/v0.1.0/demo-linux")
    });
    let foreign = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(foreign.code, 1, "{}", foreign.stderr);
    assert!(
        foreign.stderr.contains("not on the forge"),
        "{}",
        foreign.stderr
    );
    assert!(!world.forge_auth().contains("evil.example"));
    // A name that could leave the work directory.
    let world = World::new();
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    world.edit_release("v0.1.0", |value| {
        value["assets"][0]["name"] = json!("../escape")
    });
    let name = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(name.code, 1, "{}", name.stderr);
    assert_eq!(world.written(), 0);
}

#[test]
fn an_existing_asset_is_never_replaced_and_an_interrupted_upload_is_redone() {
    let script = github_script();
    let world = World::new();
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    // The release is here already, with the file under other content.
    fs::write(
        world.fix().join("gh").join("releases.json"),
        json!([{"id": 11, "tag_name": "v0.1.0", "draft": false}]).to_string(),
    )
    .unwrap();
    fs::write(world.fix().join("gh").join("assets.json"), json!([{"id": 5, "name": "demo-linux", "size": 3, "state": "uploaded", "digest": "sha256:00"}]).to_string()).unwrap();
    let clash = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(clash.code, 1, "{}", clash.stderr);
    assert!(clash.stderr.contains("never replaced"), "{}", clash.stderr);
    assert!(!world.calls().contains("DELETE"), "{}", world.calls());
    // A starter stub of an interrupted run is removed and uploaded again.
    fs::write(
        world.fix().join("gh").join("assets.json"),
        json!([{"id": 6, "name": "demo-linux", "size": 0, "state": "starter"}]).to_string(),
    )
    .unwrap();
    let redo = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(redo.code, 0, "{}", redo.stderr);
    assert!(
        world
            .calls()
            .contains("DELETE https://api.github.test/repos/o/r/releases/assets/6"),
        "{}",
        world.calls()
    );
    let assets = fs::read_to_string(world.fix().join("gh/assets.json")).unwrap();
    assert!(
        assets.contains("\"uploaded\"") && !assets.contains("starter"),
        "{assets}"
    );
}

#[test]
fn an_upload_that_comes_back_different_stops_the_run() {
    let script = github_script();
    let world = World::new();
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    fs::write(world.fix().join("gh/corrupt"), "").unwrap();
    let outcome = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(outcome.code, 1, "{}", outcome.stderr);
    let release: serde_json::Value =
        serde_json::from_slice(&fs::read(world.fix().join("gh/releases.json")).unwrap()).unwrap();
    assert_eq!(release[0]["draft"], true, "never published");
}

#[test]
fn a_private_repository_reads_with_its_token_and_a_public_one_without() {
    let script = github_script();
    let world = World::new();
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    let anonymous = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(anonymous.code, 0, "{}", anonymous.stderr);
    assert!(
        !world.forge_auth().contains(" token"),
        "{}",
        world.forge_auth()
    );
    let world = World::new();
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    let mut env = GITHUB_ENV.to_vec();
    env.push(("FORGE_RELEASE_TOKEN", "read-only-secret"));
    let private = world.run(GITHUB_SHELL, &script, &env);
    assert_eq!(private.code, 0, "{}", private.stderr);
    let log = world.forge_auth();
    assert!(
        log.contains(
            "https://forge.example/api/v1/repos/team/demo/releases/tags/v0.1.0 token"
        ),
        "{log}"
    );
    assert!(
        log.contains("releases/download/v0.1.0/demo-linux token"),
        "{log}"
    );
    assert!(!private.stderr.contains("read-only-secret"));
    // The pointer is asked without the token.
    assert!(
        log.contains("https://git.example/team/demo none"),
        "{log}"
    );
    // A repository the token cannot see answers like a missing release, with the reason named.
    let world = World::new();
    fs::write(world.fix().join("forge/status"), "404").unwrap();
    let hidden = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(hidden.code, 0, "{}", hidden.stderr);
    assert!(
        hidden.stderr.contains("FORGE_RELEASE_TOKEN"),
        "{}",
        hidden.stderr
    );
    // Any other answer is a failure, not a missing release.
    fs::write(world.fix().join("forge/status"), "500").unwrap();
    let broken = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(broken.code, 1, "{}", broken.stderr);
}

#[test]
fn the_pointer_names_the_forge_that_serves_the_api() {
    let root = rendered(SOURCE);
    let script = script(root.path(), ".github/workflows/release-mirror.yml");
    let world = World::new();
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    fs::write(
        world.fix().join("redirect"),
        "https://forge.example/team/demo",
    )
    .unwrap();
    let outcome = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(
        world.calls().contains(
            "GET https://forge.example/api/v1/repos/team/demo/releases/tags/v0.1.0"
        ),
        "{}",
        world.calls()
    );
    // A login redirect is not a forge: the baked host serves the API (and answers 599 in this fake).
    let world = World::new();
    fs::write(
        world.fix().join("redirect"),
        "https://forge.example/user/login?redirect_to=%2Fteam%2Fdemo",
    )
    .unwrap();
    let outcome = world.run(GITHUB_SHELL, &script, GITHUB_ENV);
    assert!(
        world
            .calls()
            .contains("GET https://git.example/api/v1/repos/team/demo/releases/tags/v0.1.0"),
        "{}\n{}",
        world.calls(),
        outcome.stderr
    );
}

#[test]
fn only_release_tags_are_mirrored() {
    let script = github_script();
    let world = World::new();
    for tag in ["main", "v", "v1;rm", "v$(id)"] {
        let env: Vec<(&str, &str)> = GITHUB_ENV
            .iter()
            .map(|&(k, v)| if k == "MIRROR_TAG" { (k, tag) } else { (k, v) })
            .collect();
        let outcome = world.run(GITHUB_SHELL, &script, &env);
        assert_eq!(outcome.code, 1, "{tag}: {}", outcome.stderr);
    }
    assert_eq!(world.calls(), "");
}

const GITLAB_ENV: &[(&str, &str)] = &[
    ("CI_COMMIT_TAG", "v0.1.0"),
    ("CI_API_V4_URL", "https://gl.test/api/v4"),
    ("CI_PROJECT_ID", "7"),
    ("CI_PROJECT_NAME", "demo"),
    ("CI_JOB_TOKEN", "job-secret"),
];

#[test]
fn gitlab_stores_the_files_and_creates_the_release_once() {
    let root = rendered(SOURCE);
    let script = script(root.path(), ".gitlab-ci.yml");
    let world = World::new();
    world.release(
        "v0.1.0",
        &[("demo-linux", BINARY), ("demo-windows.exe", OTHER)],
        &["demo-linux", "demo-windows.exe"],
        true,
    );
    let first = world.run(&["sh", "-eu"], &script, GITLAB_ENV);
    assert_eq!(first.code, 0, "{}", first.stderr);
    for name in ["demo-linux", "demo-windows.exe", "SHA256SUMS"] {
        assert!(
            world.fix().join("gl").join(format!("pkg-{name}")).exists(),
            "{name}"
        );
    }
    assert_eq!(
        fs::read(world.fix().join("gl/pkg-demo-linux")).unwrap(),
        BINARY
    );
    let release: serde_json::Value =
        serde_json::from_slice(&fs::read(world.fix().join("gl/release.json")).unwrap()).unwrap();
    assert_eq!(release["tag_name"], "v0.1.0");
    assert_eq!(release["name"], "demo v0.1.0");
    assert!(
        release["description"]
            .as_str()
            .unwrap()
            .starts_with("Pre-release.\n\nNotes for the release."),
        "{release}"
    );
    assert_eq!(release["assets"]["links"].as_array().unwrap().len(), 3);
    assert_eq!(
        release["assets"]["links"][0]["url"],
        "https://gl.test/api/v4/projects/7/packages/generic/demo/v0.1.0/SHA256SUMS".replace(
            "SHA256SUMS",
            release["assets"]["links"][0]["name"].as_str().unwrap()
        )
    );
    let written = world.written();
    let again = world.run(&["sh", "-eu"], &script, GITLAB_ENV);
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert_eq!(
        world.written(),
        written,
        "a second run writes nothing: {}",
        again.stderr
    );
}

#[test]
fn gitlab_adds_the_links_a_release_is_missing_and_never_replaces_a_file() {
    let root = rendered(SOURCE);
    let script = script(root.path(), ".gitlab-ci.yml");
    let world = World::new();
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    fs::create_dir_all(world.fix().join("gl")).unwrap();
    fs::write(
        world.fix().join("gl/release.json"),
        json!({"tag_name": "v0.1.0"}).to_string(),
    )
    .unwrap();
    let linked = world.run(&["sh", "-eu"], &script, GITLAB_ENV);
    assert_eq!(linked.code, 0, "{}", linked.stderr);
    let links = fs::read_to_string(world.fix().join("gl/links.json")).unwrap();
    assert!(
        links.contains("demo-linux") && links.contains("SHA256SUMS"),
        "{links}"
    );
    fs::write(world.fix().join("gl/pkg-demo-linux"), b"another content\n").unwrap();
    let clash = world.run(&["sh", "-eu"], &script, GITLAB_ENV);
    assert_eq!(clash.code, 1, "{}", clash.stderr);
    assert!(clash.stderr.contains("never replaced"), "{}", clash.stderr);
}

#[test]
fn gitlab_ends_at_once_without_a_forgejo_release() {
    let root = rendered(SOURCE);
    let script = script(root.path(), ".gitlab-ci.yml");
    let world = World::new();
    let outcome = world.run(&["sh", "-eu"], &script, GITLAB_ENV);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(world.written(), 0);
    // A manual pipeline names its tag by variable.
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    let env: Vec<(&str, &str)> = GITLAB_ENV
        .iter()
        .map(|&(k, v)| {
            if k == "CI_COMMIT_TAG" {
                (k, "")
            } else {
                (k, v)
            }
        })
        .chain([("RELEASE_TAG", "v0.1.0")])
        .collect();
    let manual = world.run(&["sh", "-eu"], &script, &env);
    assert_eq!(manual.code, 0, "{}", manual.stderr);
    assert!(world.fix().join("gl/release.json").exists());
}

// The Bitbucket pipeline is not rendered while Downloads need a paid plan; its template is tested here.
fn bitbucket_script() -> String {
    let core = include_str!("../../src/render/release-mirror-core.sh")
        .replace("CCID_MIRROR_SOURCE", SOURCE);
    let template = include_str!("../../src/render/release-mirror-bitbucket.yml");
    let mut out = String::new();
    for line in template.lines() {
        if line.trim() == "CCID_MIRROR_CORE" {
            let indent = &line[..line.len() - line.trim_start().len()];
            for part in core.lines() {
                if !part.is_empty() {
                    out.push_str(indent);
                    out.push_str(part);
                }
                out.push('\n');
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pipeline.yml"), out).unwrap();
    script(root.path(), "pipeline.yml")
}

#[test]
fn bitbucket_uploads_each_file_under_its_tag_once() {
    let script = bitbucket_script();
    let world = World::new();
    world.release("v0.1.0", &[("demo-linux", BINARY)], &["demo-linux"], false);
    let env = [
        ("BITBUCKET_TAG", "v0.1.0"),
        ("BITBUCKET_WORKSPACE", "w"),
        ("BITBUCKET_REPO_SLUG", "s"),
        ("BITBUCKET_DOWNLOADS_TOKEN", "bb-secret"),
    ];
    let first = world.run(&["bash", "-e"], &script, &env);
    assert_eq!(first.code, 0, "{}", first.stderr);
    let downloads = fs::read_to_string(world.fix().join("bb/downloads.json")).unwrap();
    assert!(
        downloads.contains("v0.1.0-demo-linux") && downloads.contains("v0.1.0-SHA256SUMS"),
        "{downloads}"
    );
    let written = world.written();
    let again = world.run(&["bash", "-e"], &script, &env);
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert_eq!(world.written(), written, "{}", again.stderr);
    let without = world.run(&["bash", "-e"], &script, &env[..3]);
    assert_eq!(without.code, 1, "{}", without.stderr);
}

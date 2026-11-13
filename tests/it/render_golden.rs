//! Golden renders of `render_schema` 1 and 2: every file `ccid render` writes for one repository of
//! each manifest shape the fleet uses (verify on push plus a separate pull-request job, manual-only
//! with a remote provider job, a dead runner label with siblings and the check-shell, a pinned
//! schema-1 Crow repository with archives and secrets, the default events with an upstream sync).
//!
//! The digests in `fixtures/render-golden.json` were taken from the release before
//! `render_schema = 3` existed. A later release must render these schemas byte for byte the same,
//! so a channel bump re-stales no repository that stays on its schema. Regenerate the file only for
//! a deliberate, declared template transition: `CCID_BLESS_RENDER_GOLDEN=1 cargo test --test it render_golden`.

use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const GOLDEN: &str = "tests/it/fixtures/render-golden.json";
const PIN: &str = "1234567890abcdef1234567890abcdef12345678";
const PIN_SHA256: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Verify on push and by hand, plus a separate pull-request job with the same checks.
const PUSH_PR: &str = r#"schema = 1
project = "push-pr"
repository = "https://forge.example/org/push-pr"

[render]
render_schema = 2

[checks.verify]
kind = "commands"
commands = [["bash", ".ci/verify.sh"]]

[checks.published]
kind = "commands"
commands = [["true"]]

[jobs.verify]
scheduler = "forgejo"
runs_on = "linux"
events = ["push", "manual"]
checks = ["verify", "published"]
workflow = "ci"
command = ["bash", ".ci/run-cached.sh"]

[jobs.verify-pr]
scheduler = "forgejo"
runs_on = "linux"
events = ["pull_request"]
checks = ["verify", "published"]
workflow = "pr"
command = ["bash", ".ci/run-cached.sh"]
"#;

/// Manual-only verify and a remote provider job whose hand-written GitLab pipeline exists.
const MANUAL_REMOTE: &str = r#"schema = 1
project = "manual-remote"
repository = "https://forge.example/org/manual-remote"

[render]
render_schema = 2

[checks.native]
kind = "nix"

[checks.eval]
kind = "commands"
commands = [["nix", "flake", "check", "--no-build"]]

[jobs.verify]
scheduler = "forgejo"
runs_on = "linux"
checks = ["native", "eval"]
workflow = "ci"
command = ["bash", ".ci/run-cached.sh"]
events = ["manual"]

[jobs.linux-arm64]
scheduler = "remote"
provider = "gitlab"
platform = "linux-arm64"
workflow = "ccid-remote"
checks = ["native"]
push_branches = ["main"]
paths = ["**.nix", "flake.lock", ".gitlab-ci.yml"]
timeout_minutes = 45
"#;

const REMOTE_PIPELINE: &str = "workflow:\n  rules:\n    - if: '$CI_PIPELINE_SOURCE == \"api\" && $CCID_REMOTE_JOB'\n    - when: never\nlinux-arm64:\n  script: [nix flake check]\n";

/// A dead runner label, siblings, a job environment, the check-shell and a second path-filtered job.
const DEAD_LABEL: &str = r#"schema = 1
project = "dead-label"

[render]
render_schema = 2

[check_shell]

[checks.verify]
kind = "commands"
commands = [["cargo", "test", "--locked"]]

[checks.deep]
kind = "commands"
commands = [["cargo", "test", "--locked", "--", "--ignored"]]

[jobs.verify]
scheduler = "forgejo"
runs_on = "heavy"
events = ["manual"]
checks = ["verify"]
workflow = "ci"
command = ["ccid:run-declared-checks"]
environment = { FEATURE_SET = "full" }
siblings = { other = "main", "org/pinned" = "0123456789abcdef0123456789abcdef01234567" }

[jobs.deep]
scheduler = "forgejo"
runs_on = "light"
events = ["push"]
push_branches = ["main", "next"]
paths = ["src/**", "Cargo.lock"]
archive_inputs = false
checks = ["deep"]
workflow = "deep"
command = ["ccid:run-declared-checks"]
"#;

/// A schema-1 repository: its own pin, Crow allowed, a secret projection and an archived source.
fn pinned_crow() -> String {
    format!(
        r#"schema = 1
project = "pinned-crow"

[render]
tool_revision = "{PIN}"
tool_sha256 = "{PIN_SHA256}"
allow_crow = true
resolver_token_secret = "forgejo_token"
secret_environment = {{ API_TOKEN = "api_token" }}

[checks.test]
kind = "commands"
commands = [["true"]]

[jobs.legacy]
checks = ["test"]
workflow = "legacy"
command = ["sh", ".ci/run.sh"]
secrets = ["API_TOKEN"]

[jobs.verify]
scheduler = "forgejo"
checks = ["test"]
workflow = "ci"
command = ["ccid:run-declared-checks"]
"#
    )
}

const ARCHIVES: &str = r#"schema = 1
[archives.shared]
revision = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
archive_variable = "SHARED_SOURCE_ARCHIVE"
digest_variable = "SHARED_SOURCE_SHA256"
"#;

/// No events key (all three), a path filter, a declared verdict and an upstream sync.
const DEFAULT_UPSTREAM: &str = r#"schema = 1
project = "default-upstream"
repository = "https://forge.example/org/default-upstream"

[render]
render_schema = 2

[checks.test]
kind = "commands"
commands = [["true"]]

[jobs.verify]
scheduler = "forgejo"
runs_on = "linux"
push_branches = ["main"]
paths = ["src/**", "Cargo.lock"]
checks = ["test"]
workflow = "ci"
command = ["ccid:run-declared-checks"]

[jobs.downstream-check]
scheduler = "forgejo"
runs_on = "linux"
events = ["manual"]
checks = ["test"]
workflow = "downstream-check"
command = ["ccid:run-declared-checks"]

[verdict]
jobs = ["verify"]
"#;

const UPSTREAM: &str =
    "{\"schema_version\": 1, \"upstream\": {\"url\": \"https://forge.example/org/up.git\", \"branch\": \"main\"}}\n";

/// The input files of every golden case.
fn cases() -> BTreeMap<&'static str, Vec<(&'static str, String)>> {
    BTreeMap::from([
        ("push-pr", vec![(".ci/ccid.toml", PUSH_PR.to_owned())]),
        (
            "manual-remote",
            vec![
                (".ci/ccid.toml", MANUAL_REMOTE.to_owned()),
                (".gitlab-ci.yml", REMOTE_PIPELINE.to_owned()),
            ],
        ),
        ("dead-label", vec![(".ci/ccid.toml", DEAD_LABEL.to_owned())]),
        (
            "pinned-crow",
            vec![
                (".ci/ccid.toml", pinned_crow()),
                (".ci/archives.toml", ARCHIVES.to_owned()),
            ],
        ),
        (
            "default-upstream",
            vec![
                (".ci/ccid.toml", DEFAULT_UPSTREAM.to_owned()),
                ("ccvl-downstream.json", UPSTREAM.to_owned()),
            ],
        ),
    ])
}

/// Render one case into a fresh directory and return every generated file's sha256.
fn digests(files: &[(&str, String)]) -> BTreeMap<String, String> {
    let root = tempfile::tempdir().unwrap();
    for (relative, content) in files {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let report = ccid::render::render(root.path(), Path::new(".ci/ccid.toml"), false)
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(report.removed.is_empty());
    let mut out = BTreeMap::new();
    for relative in &report.files {
        let bytes = fs::read(root.path().join(relative)).unwrap();
        out.insert(
            relative.display().to_string(),
            format!("{:x}", Sha256::digest(&bytes)),
        );
    }
    // A second render changes nothing and the check is clean.
    ccid::render::render(root.path(), Path::new(".ci/ccid.toml"), true).unwrap();
    out
}

#[test]
fn schema_one_and_two_render_byte_for_byte_like_the_release_before_schema_three() {
    let rendered: BTreeMap<String, BTreeMap<String, String>> = cases()
        .into_iter()
        .map(|(name, files)| (name.to_owned(), digests(&files)))
        .collect();
    let path = PathBuf::from(GOLDEN);
    if std::env::var_os("CCID_BLESS_RENDER_GOLDEN").is_some() {
        fs::write(
            &path,
            serde_json::to_string_pretty(&rendered).unwrap() + "\n",
        )
        .unwrap();
    }
    let golden: BTreeMap<String, BTreeMap<String, String>> =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(rendered, golden);
}

//! `render_schema = 3`: ccid owns the whole CI surface. Every test repository is a real git work tree, because
//! render deletes or replaces a hand-written file only when git can restore it.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const BINARY: &str = env!("CARGO_BIN_EXE_ccid");

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=surface-test",
            "-c",
            "user.email=surface-test@example.invalid",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A committed repository with `files`.
fn repository(files: &[(&str, &str)]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    for (relative, content) in files {
        write(root.path(), relative, content);
    }
    commit(root.path());
    root
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn commit(root: &Path) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "fixture"]);
}

fn render(root: &Path, check: bool) -> ccid::Result<ccid::render::Report> {
    ccid::render::render(root, Path::new(".ci/ccid.toml"), check)
}

fn error(root: &Path, check: bool) -> String {
    render(root, check).unwrap_err().to_string()
}

fn read(root: &Path, relative: &str) -> String {
    fs::read_to_string(root.join(relative)).unwrap()
}

/// A schema-3 manifest: a verdict job, a second job, extra TOML appended.
fn manifest(extra: &str) -> String {
    format!(
        "schema = 1\nproject = \"surface\"\n\n[render]\nrender_schema = 3\n\n\
         [checks.test]\nkind = \"commands\"\ncommands = [[\"true\"]]\n\n\
         [jobs.verify]\nscheduler = \"forgejo\"\nchecks = [\"test\"]\nworkflow = \"ci\"\ncommand = [\"ccid:run-declared-checks\"]\n\n\
         [jobs.deep]\nscheduler = \"forgejo\"\nruns_on = \"windows\"\nchecks = [\"test\"]\nworkflow = \"deep\"\ncommand = [\"ccid:run-declared-checks\"]\n{extra}"
    )
}

#[test]
fn the_verdict_job_runs_per_commit_and_every_other_job_by_hand() {
    let root = repository(&[(".ci/ccid.toml", &manifest(""))]);
    render(root.path(), false).unwrap();
    let ci = read(root.path(), ".forgejo/workflows/ci.yml");
    assert!(ci.contains("  push:\n    branches: [main]\n"), "{ci}");
    assert!(ci.contains("  pull_request:\n"));
    assert!(ci.contains("  workflow_dispatch:\n"));
    assert!(
        ci.contains("runs-on: linux\n"),
        "the default label is linux"
    );
    let deep = read(root.path(), ".forgejo/workflows/deep.yml");
    assert!(!deep.contains("  push:") && !deep.contains("pull_request"));
    assert!(deep.contains("  workflow_dispatch:\n"));
    assert!(deep.contains("runs-on: windows\n"));
    let inventory: serde_json::Value =
        serde_json::from_str(&read(root.path(), ".ci/jobs.json")).unwrap();
    assert_eq!(
        inventory["jobs"]["deep"]["events"],
        serde_json::json!(["manual"])
    );
    assert!(inventory["jobs"]["verify"].get("events").is_none());
    assert_eq!(inventory["jobs"]["verify"]["runs_on"], "linux");
    commit(root.path());
    assert!(render(root.path(), true).unwrap().removed.is_empty());
}

#[test]
fn schema_three_refuses_what_it_no_longer_has() {
    let base = manifest("");
    for (case, manifest, expected) in [
        (
            "verdict events",
            base.replace(
                "workflow = \"ci\"",
                "workflow = \"ci\"\nevents = [\"manual\"]",
            ),
            "remove events from [jobs.verify]",
        ),
        (
            "dead label",
            base.replace("runs_on = \"windows\"", "runs_on = \"heavy\""),
            "asks for heavy",
        ),
        (
            "remote job",
            format!("{base}\n[jobs.win]\nscheduler = \"remote\"\nprovider = \"gitlab\"\nplatform = \"windows-x64\"\nworkflow = \"ccid-remote\"\nchecks = [\"test\"]\n"),
            "has no remote scheduler",
        ),
        (
            "crow job",
            format!("{base}\n[jobs.old]\nchecks = [\"test\"]\nworkflow = \"old\"\ncommand = [\"true\"]\n"),
            "runs Forgejo jobs only",
        ),
        (
            "allow_crow",
            base.replace("render_schema = 3", "render_schema = 3\nallow_crow = true"),
            "remove [render].allow_crow",
        ),
        (
            "own pin",
            base.replace(
                "render_schema = 3",
                "render_schema = 3\ntool_revision = \"1234567890abcdef1234567890abcdef12345678\"",
            ),
            "render_schema = 3 pins no release",
        ),
        (
            "no verdict job",
            base.replace("[jobs.verify]", "[jobs.other]"),
            "verifies every commit",
        ),
        (
            "mixed workflow",
            base.replace("workflow = \"deep\"", "workflow = \"ci\""),
            "workflow ci mixes them",
        ),
    ] {
        let root = repository(&[(".ci/ccid.toml", &manifest)]);
        let message = error(root.path(), false);
        assert!(message.contains(expected), "{case}: {message}");
        assert!(
            !root.path().join(".forgejo").exists() && !root.path().join(".ci/jobs.json").exists(),
            "{case}: nothing is written"
        );
    }
}

#[test]
fn a_retired_family_goes_once_its_replacement_is_live_and_nothing_uses_it() {
    let root = repository(&[
        (".ci/ccid.toml", &manifest("")),
        (".github/dependabot.yml", "version: 2\n"),
        (
            ".github/workflows/dependabot-auto-merge.yml",
            "on: pull_request_target\n",
        ),
        (".ci/providers.toml", "[gitlab]\n"),
        (
            ".ci/README.md",
            "See .ci/providers.toml and .github/dependabot.yml.\n",
        ),
        (".github/ISSUE_TEMPLATE/bug.md", "Bug\n"),
        (".github/CODEOWNERS", "* @owner\n"),
    ]);
    let message = error(root.path(), true);
    assert!(
        message.contains(".github/dependabot.yml (retired, render removes it: nothing: dependencies move by event"),
        "{message}"
    );
    let report = render(root.path(), false).unwrap();
    assert_eq!(
        report.removed,
        [
            ".ci/providers.toml",
            ".github/dependabot.yml",
            ".github/workflows/dependabot-auto-merge.yml"
        ]
        .map(PathBuf::from)
    );
    for kept in [
        ".ci/README.md",
        ".github/ISSUE_TEMPLATE/bug.md",
        ".github/CODEOWNERS",
    ] {
        assert!(root.path().join(kept).exists(), "{kept}");
    }
    commit(root.path());
    render(root.path(), true).unwrap();
}

#[test]
fn unknown_files_and_families_without_a_live_replacement_are_refused_before_anything_is_written() {
    let root = repository(&[
        (".ci/ccid.toml", &manifest("")),
        (".github/workflows/deploy.yml", "on: push\n"),
        (".github/workflows/refresh-lock.yml", "on: schedule\n"),
        (".ci/run-cached.sh", "#!/bin/sh\n"),
        (".github/dependabot.yml", "version: 2\n"),
    ]);
    for check in [false, true] {
        let message = error(root.path(), check);
        assert!(
            message.contains(
                ".github/workflows/deploy.yml (unknown: list it in [surface] keep or upstream"
            ),
            "{message}"
        );
        assert!(
            message.contains(
                ".github/workflows/refresh-lock.yml (legacy lock-refresh: render deletes it once"
            ),
            "{message}"
        );
        assert!(
            message.contains(".ci/run-cached.sh (legacy job-script"),
            "{message}"
        );
    }
    assert!(
        root.path().join(".github/dependabot.yml").exists(),
        "a refused render deletes nothing"
    );
    assert!(
        !root.path().join(".forgejo").exists(),
        "a refused render writes nothing"
    );
    // The repository's own data settles it: a kept script, a fork's own file; the job script while the manifest runs it.
    fs::write(
        root.path().join(".ci/ccid.toml"),
        manifest("\n[surface]\nkeep = { \".github/workflows/deploy.yml\" = \"the hosted deploy, until the CD design\" }\nupstream = [\".github/workflows/refresh-lock.yml\"]\n")
            .replace("command = [\"ccid:run-declared-checks\"]\n\n[jobs.deep]", "command = [\"sh\", \".ci/run-cached.sh\"]\n\n[jobs.deep]"),
    )
    .unwrap();
    commit(root.path());
    let report = render(root.path(), false).unwrap();
    assert_eq!(report.removed, [PathBuf::from(".github/dependabot.yml")]);
    for kept in [
        ".github/workflows/deploy.yml",
        ".github/workflows/refresh-lock.yml",
        ".ci/run-cached.sh",
    ] {
        assert!(root.path().join(kept).exists(), "{kept}");
    }
}

#[test]
fn a_legacy_file_that_something_still_uses_stays() {
    let root = repository(&[
        (
            ".ci/ccid.toml",
            &manifest("").replace(
                "command = [\"ccid:run-declared-checks\"]\n\n[jobs.deep]",
                "command = [\"bash\", \".ci/run-cached.sh\"]\n\n[jobs.deep]",
            ),
        ),
        (
            ".ci/run-cached.sh",
            "#!/bin/sh\n. \"$(dirname \"$0\")/toolchain-lock.sh\"\npython3 .ci/check-extra.py\n",
        ),
        (".ci/toolchain-lock.sh", "#!/bin/sh\n"),
        (".ci/check-extra.py", "print('extra')\n"),
        (".ci/update-our-crates.sh", "#!/bin/sh\n"),
        (".ci/our-crates.txt", "dep\n"),
        ("Makefile", "float:\n\tsh .ci/update-our-crates.sh\n"),
        (".ci/verify.sh", "#!/bin/sh\n"),
        ("README.md", "Run .ci/verify.sh by hand.\n"),
    ]);
    let report = render(root.path(), false).unwrap();
    // Prose only mentions verify.sh; nothing uses our-crates.txt.
    assert_eq!(
        report.removed,
        [".ci/our-crates.txt", ".ci/verify.sh"].map(PathBuf::from)
    );
    for kept in [
        ".ci/run-cached.sh",
        ".ci/toolchain-lock.sh",
        ".ci/check-extra.py",
        ".ci/update-our-crates.sh",
    ] {
        assert!(root.path().join(kept).exists(), "{kept}");
    }
    let report = ccid::render::surface_report(root.path(), Path::new(".ci/ccid.toml")).unwrap();
    let note = |path: &str| {
        report
            .files
            .iter()
            .find(|item| item.path == Path::new(path))
            .map(|item| (item.class, item.action, item.note.clone()))
            .unwrap()
    };
    use ccid::render::surface::{Action, Class};
    assert_eq!(
        note(".ci/toolchain-lock.sh"),
        (
            Class::Legacy,
            Action::Keep,
            "still used by .ci/run-cached.sh".to_owned()
        )
    );
    assert_eq!(
        note(".ci/check-extra.py"),
        (
            Class::Script,
            Action::Keep,
            "used by .ci/run-cached.sh".to_owned()
        )
    );
    assert_eq!(
        note(".ci/update-our-crates.sh"),
        (
            Class::Legacy,
            Action::Keep,
            "still used by Makefile".to_owned()
        )
    );
}

#[test]
fn render_deletes_or_replaces_only_what_git_can_restore() {
    let root = repository(&[(".ci/ccid.toml", &manifest(""))]);
    // Untracked: git could not bring it back.
    write(root.path(), ".github/dependabot.yml", "version: 2\n");
    let message = error(root.path(), false);
    assert!(message.contains("only files git can restore"), "{message}");
    assert!(root.path().join(".github/dependabot.yml").exists());
    // Committed, then changed in the work tree: the change would be lost.
    commit(root.path());
    write(
        root.path(),
        ".github/dependabot.yml",
        "version: 2\nupdates: []\n",
    );
    assert!(error(root.path(), false).contains("only files git can restore"));
    // Committed as it is: render deletes it.
    commit(root.path());
    assert_eq!(
        render(root.path(), false).unwrap().removed,
        [PathBuf::from(".github/dependabot.yml")]
    );
}

#[test]
fn ignored_files_and_symlinks_are_never_judged_as_ci_files() {
    let root = repository(&[
        (".ci/ccid.toml", &manifest("")),
        (".gitignore", "__pycache__/\n"),
        ("CONTRIBUTING.md", "Contributing\n"),
    ]);
    write(
        root.path(),
        ".ci/__pycache__/check.cpython-314.pyc",
        "\0bytecode",
    );
    fs::create_dir_all(root.path().join(".github")).unwrap();
    std::os::unix::fs::symlink(
        "../CONTRIBUTING.md",
        root.path().join(".github/CONTRIBUTING.md"),
    )
    .unwrap();
    commit(root.path());
    render(root.path(), false).unwrap();
    assert!(root
        .path()
        .join(".ci/__pycache__/check.cpython-314.pyc")
        .exists());
    assert!(
        fs::symlink_metadata(root.path().join(".github/CONTRIBUTING.md"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // A symlink of a retired family is refused, never followed or deleted.
    std::os::unix::fs::symlink(
        "../CONTRIBUTING.md",
        root.path().join(".github/dependabot.yml"),
    )
    .unwrap();
    commit(root.path());
    assert!(
        error(root.path(), false).contains(".github/dependabot.yml (legacy dependabot: a symlink")
    );
}

#[test]
fn the_remote_scheduler_pipeline_goes_at_schema_three_and_a_fork_keeps_its_own() {
    let remote =
        "workflow:\n  rules:\n    - if: '$CCID_REMOTE_JOB'\nwindows:\n  script: [cargo test]\n";
    let declared = manifest("").replace(
        "project = \"surface\"",
        "project = \"surface\"\nrepository = \"https://forge.example/org/surface\"",
    );
    let root = repository(&[(".ci/ccid.toml", &declared), (".gitlab-ci.yml", remote)]);
    // Schema 2 leaves the hand-written pipeline alone and renders nothing for GitLab.
    let schema_two = repository(&[
        (
            ".ci/ccid.toml",
            &declared
                .replace("render_schema = 3", "render_schema = 2")
                .replace(
                    "runs_on = \"windows\"",
                    "runs_on = \"windows\"\nevents = [\"manual\"]",
                ),
        ),
        (".gitlab-ci.yml", remote),
    ]);
    render(schema_two.path(), false).unwrap();
    assert_eq!(read(schema_two.path(), ".gitlab-ci.yml"), remote);
    // Schema 3 retires it: a mirror forge runs no CI (R6, R7), and nothing takes its place.
    let report = render(root.path(), false).unwrap();
    assert!(!report.files.contains(&PathBuf::from(".gitlab-ci.yml")));
    assert_eq!(report.removed, [PathBuf::from(".gitlab-ci.yml")]);
    assert!(!root.path().join(".gitlab-ci.yml").exists());
    // A fork's own pipeline, declared upstream, is never touched.
    let fork = repository(&[
        (
            ".ci/ccid.toml",
            &format!("{declared}\n[surface]\nupstream = [\".gitlab-ci.yml\"]\n"),
        ),
        (".gitlab-ci.yml", "include: upstream.yml\n"),
    ]);
    render(fork.path(), false).unwrap();
    assert_eq!(
        read(fork.path(), ".gitlab-ci.yml"),
        "include: upstream.yml\n"
    );
    // Undeclared, the same file is refused.
    let undeclared = repository(&[
        (".ci/ccid.toml", &declared),
        (".gitlab-ci.yml", "include: upstream.yml\n"),
    ]);
    assert!(error(undeclared.path(), false).contains(".gitlab-ci.yml (unknown"));
}

#[test]
fn an_owned_file_that_is_no_longer_rendered_goes() {
    let root = repository(&[(".ci/ccid.toml", &manifest(""))]);
    render(root.path(), false).unwrap();
    write(
        root.path(),
        ".github/workflows/old-check.yml",
        "# Generated by ccid render v1; edit .ci/ccid.toml instead.\nname: old\n",
    );
    commit(root.path());
    assert!(error(root.path(), true).contains(".github/workflows/old-check.yml"));
    assert_eq!(
        render(root.path(), false).unwrap().removed,
        [PathBuf::from(".github/workflows/old-check.yml")]
    );
}

#[test]
fn surface_keep_names_existing_plain_paths_with_a_reason() {
    for (keep, expected) in [
        ("\".ci/missing.py\" = \"why\"", "not in the repository"),
        ("\".ci/publish.py\" = \"\"", "needs a reason"),
        ("\"src/main.rs\" = \"why\"", "plain path under"),
    ] {
        let root = repository(&[
            (
                ".ci/ccid.toml",
                &manifest(&format!("\n[surface]\nkeep = {{ {keep} }}\n")),
            ),
            (".ci/publish.py", ""),
            ("src/main.rs", ""),
        ]);
        let message = error(root.path(), false);
        assert!(message.contains(expected), "{keep}: {message}");
    }
}

#[test]
fn surface_plan_edits_a_schema_two_manifest_in_memory_and_writes_nothing() {
    let schema_two = "schema = 1\nproject = \"old\"\nrepository = \"https://forge.example/org/old\"\n\n[render]\nrender_schema = 2\n\n\
        [checks.test]\nkind = \"commands\"\ncommands = [[\"true\"]]\n\n\
        [jobs.verify]\nscheduler = \"forgejo\"\nruns_on = \"heavy\"\nevents = [\"push\", \"manual\"]\nchecks = [\"test\"]\nworkflow = \"ci\"\ncommand = [\"bash\", \".ci/run-cached.sh\"]\n\n\
        [jobs.verify-pr]\nscheduler = \"forgejo\"\nruns_on = \"heavy\"\nevents = [\"pull_request\"]\nchecks = [\"test\"]\nworkflow = \"pr\"\ncommand = [\"bash\", \".ci/run-cached.sh\"]\n\n\
        [jobs.deep]\nscheduler = \"forgejo\"\nruns_on = \"linux\"\nevents = [\"manual\"]\nchecks = [\"test\"]\nworkflow = \"deep\"\ncommand = [\"ccid:run-declared-checks\"]\n\n\
        [jobs.win]\nscheduler = \"remote\"\nprovider = \"gitlab\"\nplatform = \"windows-x64\"\nworkflow = \"ccid-remote\"\nchecks = [\"test\"]\n";
    let root = repository(&[
        (".ci/ccid.toml", schema_two),
        (".ci/run-cached.sh", "#!/bin/sh\n"),
        (
            ".gitlab-ci.yml",
            "workflow:\n  rules:\n    - if: '$CCID_REMOTE_JOB'\n",
        ),
        (".github/dependabot.yml", "version: 2\n"),
        (".github/workflows/deploy.yml", "on: push\n"),
    ]);
    render(root.path(), false).unwrap();
    commit(root.path());
    let output = Command::new(BINARY)
        .args(["surface", "plan", "--repo"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &out["plans"][0];
    let kinds: Vec<&str> = plan["edits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edit| edit["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "render_schema",
            "remote_job",
            "runs_on",
            "runs_on",
            "pull_request_job",
            "manual_events",
            "verdict_events"
        ]
    );
    assert_eq!(
        plan["ready"], false,
        "the remote job and the unknown deploy are open"
    );
    assert!(plan["decisions"][0]
        .as_str()
        .unwrap()
        .starts_with("remote job win (gitlab, windows-x64)"));
    let paths = |key: &str| -> Vec<String> {
        plan[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["path"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        paths("deletes"),
        [
            ".forgejo/workflows/pr.yml",
            ".github/dependabot.yml",
            ".gitlab-ci.yml"
        ]
    );
    assert_eq!(paths("refused"), [".github/workflows/deploy.yml"]);
    assert_eq!(paths("kept"), [".ci/run-cached.sh"]);
    let writes: Vec<&str> = plan["writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    assert!(!writes.contains(&".gitlab-ci.yml") && writes.contains(&".ci/jobs.json"));
    assert_eq!(out["totals"]["repos"], 1);
    assert_eq!(out["totals"]["edits"]["runs_on"], 1);
    // Read only: the repository is exactly as committed.
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(
        status.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
    assert_eq!(read(root.path(), ".ci/ccid.toml"), schema_two);
}

#[test]
fn surface_report_classifies_an_older_schema_without_changing_it() {
    let root = repository(&[
        (
            ".ci/ccid.toml",
            &manifest("")
                .replace("render_schema = 3", "render_schema = 2")
                .replace(
                    "runs_on = \"windows\"",
                    "runs_on = \"windows\"\nevents = [\"manual\"]",
                ),
        ),
        (".github/dependabot.yml", "version: 2\n"),
    ]);
    let output = Command::new(BINARY)
        .args(["surface", "report", "--repo"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["render_schema"], 2);
    let dependabot = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["path"] == ".github/dependabot.yml")
        .unwrap();
    assert_eq!(dependabot["class"], "legacy");
    assert_eq!(dependabot["action"], "delete");
    assert_eq!(dependabot["family"], "dependabot");
    assert!(root.path().join(".github/dependabot.yml").exists());
}

#[test]
fn the_legacy_github_ci_goes_and_stays_where_a_check_reads_it() {
    let root = repository(&[
        (".ci/ccid.toml", &manifest("")),
        (".github/workflows/ci.yml", "on: push\njobs: {}\n"),
    ]);
    let report = render(root.path(), false).unwrap();
    assert_eq!(report.removed, [PathBuf::from(".github/workflows/ci.yml")]);
    // R6: nothing replaces it on the mirror forge; the per-commit verify runs on the forge.
    assert!(!root.path().join(".github/workflows/pr-check.yml").exists());
    commit(root.path());
    render(root.path(), true).unwrap();
    // A declared check that reads the file keeps it, with the user named.
    let reader = repository(&[
        (
            ".ci/ccid.toml",
            &manifest("").replace(
                "commands = [[\"true\"]]",
                "commands = [[\"grep\", \"-q\", \"push\", \".github/workflows/ci.yml\"]]",
            ),
        ),
        (".github/workflows/ci.yml", "on: push\njobs: {}\n"),
    ]);
    let report = render(reader.path(), false).unwrap();
    assert!(report.removed.is_empty());
    assert!(reader.path().join(".github/workflows/ci.yml").exists());
}

/// The plan of a nix repository whose only remote job is the native aarch64 `nix flake check` on a hosted runner.
fn nix_plan(checks: &str, verify_checks: &str) -> serde_json::Value {
    let manifest = format!(
        "schema = 1\nproject = \"nixy\"\nrepository = \"https://forge.example/org/nixy\"\n\n[render]\nrender_schema = 2\n\n{checks}\n\
         [jobs.verify]\nscheduler = \"forgejo\"\nruns_on = \"heavy\"\nchecks = [{verify_checks}]\nworkflow = \"ci\"\ncommand = [\"ccid:run-declared-checks\"]\nevents = [\"manual\"]\n\n\
         [jobs.linux-arm64]\nscheduler = \"remote\"\nprovider = \"gitlab\"\nplatform = \"linux-arm64\"\nworkflow = \"ccid-remote\"\nchecks = [\"native\"]\npush_branches = [\"main\"]\n"
    );
    let root = repository(&[
        (".ci/ccid.toml", &manifest),
        (
            ".gitlab-ci.yml",
            "workflow:\n  rules:\n    - if: '$CCID_REMOTE_JOB'\nlinux-arm64:\n  script:\n    - nix flake check\n",
        ),
    ]);
    render(root.path(), false).unwrap();
    commit(root.path());
    let output = Command::new(BINARY)
        .args(["surface", "plan", "--repo"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    out["plans"][0].clone()
}

fn kinds(plan: &serde_json::Value) -> Vec<&str> {
    plan["edits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edit| edit["kind"].as_str().unwrap())
        .collect()
}

#[test]
fn the_native_aarch64_job_of_a_nix_repository_becomes_an_evaluation_on_linux() {
    // The repository already evaluates every system without building: dropping the remote job is the whole edit.
    let declared = nix_plan(
        "[checks.native]\nkind = \"nix\"\nmode = \"native\"\n\n[checks.eval]\nkind = \"nix\"\nmode = \"eval\"\n",
        "\"native\", \"eval\"",
    );
    assert_eq!(declared["error"], serde_json::Value::Null, "{declared}");
    assert_eq!(
        kinds(&declared),
        ["render_schema", "remote_job", "runs_on", "verdict_events"]
    );
    assert!(
        declared.get("decisions").is_none(),
        "nothing is left to decide: {declared}"
    );
    assert!(declared["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note.as_str().unwrap().contains("evaluation only")));
    assert_eq!(declared["ready"], true, "{declared}");
    // A nix repository without that check gets it: declared and listed in the verdict job.
    let added = nix_plan(
        "[checks.native]\nkind = \"nix\"\nmode = \"native\"\n",
        "\"native\"",
    );
    assert_eq!(added["error"], serde_json::Value::Null, "{added}");
    assert!(kinds(&added).contains(&"aarch64_eval"), "{added}");
    assert_eq!(
        kinds(&added)
            .iter()
            .filter(|kind| **kind == "aarch64_eval")
            .count(),
        2
    );
    assert!(added.get("decisions").is_none(), "{added}");
    assert_eq!(added["ready"], true, "{added}");
    // A repository with no nix check at all has a decision to make.
    let other = nix_plan(
        "[checks.native]\nkind = \"commands\"\ncommands = [[\"true\"]]\n",
        "\"native\"",
    );
    assert!(!kinds(&other).contains(&"aarch64_eval"), "{other}");
    assert!(other["decisions"][0]
        .as_str()
        .unwrap()
        .starts_with("remote job linux-arm64 (gitlab, linux-arm64)"));
}

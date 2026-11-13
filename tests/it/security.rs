//! R47: the security and dependency status is rendered into the README, the policy file and the release
//! notes from two committed data files, at `render_schema = 3` only, and never for a fork.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn manifest(extra: &str) -> String {
    format!(
        "schema = 1\nproject = \"demo\"\n\n[render]\nrender_schema = 3\n\n\
         [checks.build]\nkind = \"commands\"\ncommands = [[\"true\"]]\n\n\
         [checks.test]\nkind = \"commands\"\ncommands = [[\"true\"]]\n\n\
         [jobs.verify]\nscheduler = \"forgejo\"\nchecks = [\"test\"]\nworkflow = \"ci\"\ncommand = [\"ccid:run-declared-checks\"]\n\n{extra}"
    )
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn read(root: &Path, relative: &str) -> String {
    fs::read_to_string(root.join(relative)).unwrap()
}

fn repository(manifest: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    write(root.path(), ".ci/ccid.toml", manifest);
    write(
        root.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    );
    for (relative, content) in files {
        write(root.path(), relative, content);
    }
    git(root.path(), &["add", "-A"]);
    git(root.path(), &["commit", "-q", "-m", "fixture"]);
    root
}

fn render(root: &Path, check: bool) -> ccid::Result<ccid::render::Report> {
    ccid::render::render(root, Path::new(".ci/ccid.toml"), check)
}

fn commit(root: &Path) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "render"]);
}

#[test]
fn a_render_adds_the_status_and_creates_the_policy_and_a_check_then_passes() {
    let root = repository(&manifest(""), &[("README.md", "# Demo\n\nWhat it does.\n")]);
    let report = render(root.path(), false).unwrap();
    assert!(report.files.contains(&PathBuf::from("README.md")));
    assert!(report.files.contains(&PathBuf::from("SECURITY.md")));
    assert!(read(root.path(), "README.md").starts_with("# Demo\n\n<!-- ccid:security:begin"));
    let policy = read(root.path(), "SECURITY.md");
    assert!(!policy.contains('@'), "the policy names no contact address");
    commit(root.path());
    assert!(render(root.path(), true).unwrap().transitional.is_empty());
    // A README that loses its block is a declared transition, not drift.
    write(root.path(), "README.md", "# Demo\n");
    assert!(render(root.path(), true)
        .unwrap()
        .transitional
        .contains(&PathBuf::from("README.md")));
    render(root.path(), false).unwrap();
    commit(root.path());
    // A hand edit inside the block is drift, repaired by the next render.
    write(
        root.path(),
        "SECURITY.md",
        &policy.replace("Renovate", "Dependabot"),
    );
    let error = render(root.path(), true).unwrap_err().to_string();
    assert!(error.contains("SECURITY.md"), "{error}");
    render(root.path(), false).unwrap();
    assert_eq!(read(root.path(), "SECURITY.md"), policy);
}

#[test]
fn schema_two_gets_nothing() {
    let two = manifest("").replace("render_schema = 3", "render_schema = 2");
    let root = repository(&two, &[("README.md", "# Demo\n")]);
    let _ = render(root.path(), false);
    assert_eq!(read(root.path(), "README.md"), "# Demo\n");
    assert!(!root.path().join("SECURITY.md").exists());
}

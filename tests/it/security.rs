//! R47: the security and dependency status is rendered into the README, the policy file and the release
//! notes from two committed data files, at `render_schema = 3` only, and never for a fork.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const BINARY: &str = env!("CARGO_BIN_EXE_ccid");

fn manifest(extra: &str) -> String {
    format!(
        "schema = 1\nproject = \"demo\"\n\n[render]\nrender_schema = 3\n\n\
         [checks.build]\nkind = \"commands\"\ncommands = [[\"true\"]]\n\n\
         [checks.test]\nkind = \"commands\"\ncommands = [[\"true\"]]\n\n\
         [jobs.verify]\nscheduler = \"forgejo\"\nchecks = [\"test\"]\nworkflow = \"ci\"\ncommand = [\"ccid:run-declared-checks\"]\n\n{extra}"
    )
}

const LEDGER: &str = "schema = 1\n\n[[advisory]]\nid = \"GHSA-aaaa-bbbb-cccc\"\npackage = \"hyper\"\necosystem = \"crates.io\"\naffected = \"0.14.10\"\nseverity = \"high\"\nfound = \"2026-11-10\"\nstate = \"open\"\n";

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

fn paths(items: &[&str]) -> Vec<PathBuf> {
    items.iter().map(PathBuf::from).collect()
}

#[test]
fn a_render_adds_the_status_to_the_readme_and_creates_the_policy_and_a_check_then_passes() {
    let root = repository(&manifest(""), &[("README.md", "# Demo\n\nWhat it does.\n")]);
    let report = render(root.path(), false).unwrap();
    assert!(report.files.contains(&PathBuf::from("README.md")));
    assert!(report.files.contains(&PathBuf::from("SECURITY.md")));
    let readme = read(root.path(), "README.md");
    assert!(
        readme.starts_with("# Demo\n\n<!-- ccid:security:begin"),
        "{readme}"
    );
    assert!(readme.contains("Advisories on record for dependencies: none."));
    assert!(readme.ends_with("\nWhat it does.\n"));
    let policy = read(root.path(), "SECURITY.md");
    assert!(policy.starts_with("# Security policy\n\n<!-- ccid:security:begin"));
    assert!(!policy.contains('@'), "the policy names no contact address");
    commit(root.path());
    let checked = render(root.path(), true).unwrap();
    assert!(
        checked.transitional.is_empty(),
        "{:?}",
        checked.transitional
    );
}

#[test]
fn a_check_accepts_a_repository_without_the_blocks_as_a_declared_transition() {
    let root = repository(&manifest(""), &[("README.md", "# Demo\n")]);
    // Everything else rendered, the blocks not: what a repository looks like after the release that
    // introduces them and before its next render.
    render(root.path(), false).unwrap();
    fs::write(root.path().join("README.md"), "# Demo\n").unwrap();
    fs::remove_file(root.path().join("SECURITY.md")).unwrap();
    let report = render(root.path(), true).unwrap();
    assert_eq!(report.transitional, paths(&["README.md", "SECURITY.md"]));
}

#[test]
fn a_ledger_change_is_drift_until_the_next_render_and_the_data_file_is_not_an_unknown_ci_file() {
    let root = repository(&manifest(""), &[("README.md", "# Demo\n")]);
    render(root.path(), false).unwrap();
    commit(root.path());
    write(root.path(), ".ci/security.toml", LEDGER);
    // The ledger is data: the surface does not refuse it as an unknown file under `.ci/`.
    let error = render(root.path(), true).unwrap_err().to_string();
    assert!(error.contains("drifted"), "{error}");
    assert!(
        error.contains("README.md") && error.contains("SECURITY.md"),
        "{error}"
    );
    render(root.path(), false).unwrap();
    let readme = read(root.path(), "README.md");
    assert!(readme.contains(
        "1 open ([GHSA-aaaa-bbbb-cccc](https://osv.dev/vulnerability/GHSA-aaaa-bbbb-cccc))."
    ));
    assert!(read(root.path(), "SECURITY.md").contains("| open | 2026-11-10 | - |"));
    commit(root.path());
    assert!(render(root.path(), true).unwrap().transitional.is_empty());
}

#[test]
fn a_hand_written_policy_and_the_text_around_the_block_are_kept() {
    let root = repository(
        &manifest(""),
        &[
            ("README.md", "# Demo\n\nIntro.\n\n## Usage\n\nRun it.\n"),
            (
                "SECURITY.md",
                "# Security\n\nWrite to the maintainers' forge.\n",
            ),
        ],
    );
    render(root.path(), false).unwrap();
    let policy = read(root.path(), "SECURITY.md");
    assert!(policy
        .starts_with("# Security\n\nWrite to the maintainers' forge.\n\n<!-- ccid:security:begin"));
    let readme = read(root.path(), "README.md");
    assert!(readme.contains("\n\nIntro.\n\n## Usage\n\nRun it.\n"));
    // A person edits the text outside the block; the check stays green.
    write(
        root.path(),
        "README.md",
        &readme.replace("Intro.", "A better intro."),
    );
    commit(root.path());
    assert!(render(root.path(), true).unwrap().transitional.is_empty());
}

#[test]
fn nothing_changes_below_schema_3_and_nothing_is_added_to_a_fork() {
    let older = repository(
        &manifest("").replace("render_schema = 3", "render_schema = 2"),
        &[("README.md", "# Demo\n")],
    );
    let report = render(older.path(), false).unwrap();
    assert!(!report.files.contains(&PathBuf::from("README.md")));
    assert_eq!(read(older.path(), "README.md"), "# Demo\n");
    assert!(!older.path().join("SECURITY.md").exists());

    let fork = repository(
        &manifest("[surface]\nupstream = [\".gitlab-ci.yml\"]\n"),
        &[
            ("README.md", "# Demo\n"),
            (".gitlab-ci.yml", "upstream: {}\n"),
        ],
    );
    let report = render(fork.path(), false).unwrap();
    assert!(!report.files.contains(&PathBuf::from("README.md")));
    assert_eq!(read(fork.path(), "README.md"), "# Demo\n");
    assert!(!fork.path().join("SECURITY.md").exists());
}

#[test]
fn a_broken_ledger_or_block_stops_the_render_and_writes_nothing() {
    let root = repository(&manifest(""), &[("README.md", "# Demo\n")]);
    write(
        root.path(),
        ".ci/security.toml",
        &LEDGER.replace("hyper", "hy](http://x)[per"),
    );
    let error = render(root.path(), false).unwrap_err().to_string();
    assert!(
        error.contains(".ci/security.toml") && error.contains("package"),
        "{error}"
    );
    assert_eq!(read(root.path(), "README.md"), "# Demo\n");
    assert!(!root.path().join("SECURITY.md").exists());

    fs::remove_file(root.path().join(".ci/security.toml")).unwrap();
    write(
        root.path(),
        "README.md",
        "# Demo\n\n<!-- ccid:security:begin (x) -->\nhalf a block\n",
    );
    let error = render(root.path(), false).unwrap_err().to_string();
    assert!(error.contains("unbalanced or repeated"), "{error}");
}

#[cfg(unix)]
#[test]
fn a_render_keeps_the_permissions_of_the_readme() {
    use std::os::unix::fs::PermissionsExt;
    let root = repository(&manifest(""), &[("README.md", "# Demo\n")]);
    fs::set_permissions(
        root.path().join("README.md"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    render(root.path(), false).unwrap();
    let mode = fs::metadata(root.path().join("README.md"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o644);
}

#[test]
fn the_release_notes_carry_the_section_and_a_non_schema_3_release_does_not() {
    let library = "[release]\nkind = \"library\"\nversion = \"Cargo.toml\"\nbuild = [\"build\"]\nregistries = [\"crates\"]\n";
    let root = repository(&manifest(library), &[(".ci/security.toml", LEDGER)]);
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let job = ccid::release::flow::Job::load(root.path(), Some(sha)).unwrap();
    let notes = job.notes().unwrap();
    assert!(
        notes.starts_with("demo 0.1.0 (minor release), built from 0123456789abcdef"),
        "{notes}"
    );
    assert!(notes.contains("\n\n## Security and dependencies\n\n"));
    assert!(notes.contains("- Open advisories in dependencies: 1 ("));
    // Deterministic: the draft and the promotion build the same text.
    assert_eq!(notes, job.notes().unwrap());

    // A fork (it lists an upstream's CI files) gets nothing of ours, in the release notes either.
    let fork = repository(
        &manifest(&format!(
            "{library}\n[surface]\nupstream = [\".gitlab-ci.yml\"]\n"
        )),
        &[
            (".ci/security.toml", LEDGER),
            (".gitlab-ci.yml", "upstream: {}\n"),
        ],
    );
    let notes = ccid::release::flow::Job::load(fork.path(), Some(sha))
        .unwrap()
        .notes()
        .unwrap();
    assert!(!notes.contains("Security and dependencies"), "{notes}");
}

#[test]
fn the_plan_command_prints_json_and_sends_nothing() {
    let root = repository(&manifest(""), &[]);
    let output = Command::new(BINARY)
        .args(["security", "plan", "--repo"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["sends"], false);
    assert_eq!(plan["forges"]["github"]["publish"], false);

    write(root.path(), ".ci/own-advisories.toml", "schema = 9\n");
    let output = Command::new(BINARY)
        .args(["security", "plan", "--repo"])
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("own-advisories.toml"));
}

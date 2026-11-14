//! The jobs of the release workflow: `build` (the release job), `leg` (one platform leg) and `promote`.
//!
//! Each runs in the checkout of the exact commit that `ccid job-run` staged, with the forge's job token
//! in the environment. Nothing here polls: the workflow's `needs` order the jobs.

use super::draft::{self, Draft};
use super::forge::Forge;
use super::{Config, Kind, Plan, SUMS};
use crate::{failure, Result};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

/// The variables that carry a forge credential; the build and seal checks never see them.
const CREDENTIALS: [&str; 5] = [
    "CCID_FORGE_TOKEN",
    "FORGEJO_TOKEN",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "CCID_FORGE_READ_TOKEN_FILE",
];
/// The commit status that says a commit passed its per-commit checks.
const VERDICT: &str = "ccid/verdict";
/// The branch a release is cut from.
const BRANCH: &str = "main";
/// The trailer of a lock-only commit that a release preflight made.
const REFRESH_TRAILER: &str = "CCID-Lock-Refresh: third-party";

/// The seconds a check of a release may run: a release builds everything on a cold cache and cross compiles, so
/// two hours unless the job already allows more.
const CHECK_DEADLINE: u64 = 7_200;

fn check_deadline(current: Option<&str>) -> String {
    current
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(CHECK_DEADLINE, |value| value.max(CHECK_DEADLINE))
        .to_string()
}

/// One release of one repository at one commit.
pub struct Job {
    pub repo: PathBuf,
    pub project: String,
    pub config: Config,
    pub plan: Plan,
    pub sha: String,
    /// The release notes carry the security and dependency section (R47): `render_schema = 3`, no fork.
    pub security: bool,
    /// `[security] dependencies` of the manifest: which dependency sentence the notes carry.
    pub dependencies: crate::security::Dependencies,
}

fn git_head(repo: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|error| failure(format!("cannot run git: {error}")))?;
    if !output.status.success() {
        return Err(failure(
            "git rev-parse HEAD failed: name the commit with --commit or CI_COMMIT_SHA",
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

impl Job {
    /// The release `repo`'s manifest declares, at `commit` (default `CI_COMMIT_SHA`, else the checkout's HEAD).
    pub fn load(repo: &Path, commit: Option<&str>) -> Result<Job> {
        let (manifest, _) = crate::load_manifest(repo, Path::new(".ci/ccid.toml"))?;
        let config = manifest
            .release
            .clone()
            .ok_or_else(|| failure("the manifest declares no [release]"))?;
        config.validate(&manifest)?;
        let version = super::version::read(repo, &config.version)?;
        let sha = match commit {
            Some(commit) => commit.to_owned(),
            None => match std::env::var("CI_COMMIT_SHA")
                .ok()
                .filter(|v| !v.is_empty())
            {
                Some(sha) => sha,
                None => git_head(repo)?,
            },
        };
        if !draft::valid_sha(&sha) {
            return Err(failure(format!("{sha:?} is not a full commit id")));
        }
        Ok(Job {
            repo: repo.to_owned(),
            project: manifest.project.clone(),
            plan: config.plan(version),
            security: crate::render::security_applies(&manifest),
            dependencies: manifest
                .security
                .as_ref()
                .map(|config| config.dependencies)
                .unwrap_or_default(),
            config,
            sha,
        })
    }

    pub fn public_tag(&self) -> String {
        format!("v{}", self.plan.version)
    }

    fn title(&self) -> String {
        format!("{} (draft of {})", self.plan.version, &self.sha[..12])
    }

    /// The body of the release: the notes file when `[release] notes` names one, else one line,
    /// then the security and dependency section (R47) built from the data at the released commit.
    pub fn notes(&self) -> Result<String> {
        let base = self.base_notes()?;
        if !self.security {
            return Ok(base);
        }
        let section = crate::security::release_section(self.dependencies);
        Ok(format!("{}\n\n{section}", base.trim_end()))
    }

    fn base_notes(&self) -> Result<String> {
        match &self.config.notes {
            Some(file) => {
                let text = fs::read_to_string(self.repo.join(file)).map_err(|error| {
                    failure(format!("cannot read the release notes {file}: {error}"))
                })?;
                if text.len() > 65_536 {
                    return Err(failure(format!(
                        "the release notes {file} are longer than 64 KiB"
                    )));
                }
                Ok(text)
            }
            None => Ok(format!(
                "{} {} ({} release), built from {}.",
                self.project, self.plan.version, self.plan.class, self.sha
            )),
        }
    }

    /// What a check of this release sees besides the process environment.
    pub fn environment(&self, dir: &Path) -> Vec<(String, String)> {
        let kind = match self.config.kind {
            Kind::Binary => "binary",
            Kind::Library => "library",
        };
        let exe = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        vec![
            ("CCID_RELEASE_DIR".into(), dir.display().to_string()),
            ("CCID_RELEASE_VERSION".into(), self.plan.version.to_string()),
            ("CCID_RELEASE_CLASS".into(), self.plan.class.to_string()),
            ("CCID_RELEASE_KIND".into(), kind.into()),
            ("CCID_RELEASE_COMMIT".into(), self.sha.clone()),
            (
                "CCID_RELEASE_PLATFORMS".into(),
                self.plan.platforms.join(" "),
            ),
            ("CCID_RELEASE_LEGS".into(), self.plan.legs.join(" ")),
            (
                "CCID_RELEASE_MANIFESTS".into(),
                self.config.manifests.join(" "),
            ),
            ("CCID_BIN".into(), exe),
        ]
    }

    /// Run declared checks (`ccid check`) with the release environment. `true` when they passed.
    fn run_checks(&self, checks: &[String], dir: &Path, credentials: bool) -> Result<bool> {
        let exe = std::env::current_exe()?;
        let mut command = Command::new(exe);
        command
            .current_dir(&self.repo)
            .args(["check", "--repo", "."]);
        for check in checks {
            command.arg("--check").arg(check);
        }
        command.envs(self.environment(dir));
        command.env(
            "CI_TIMEOUT",
            check_deadline(std::env::var("CI_TIMEOUT").ok().as_deref()),
        );
        if !credentials {
            for name in CREDENTIALS {
                command.env_remove(name);
            }
        }
        let status = command
            .status()
            .map_err(|error| failure(format!("cannot run the checks {checks:?}: {error}")))?;
        Ok(status.success())
    }
}

/// Append `key=value` lines to the job's outputs (`$GITHUB_OUTPUT`), when the runner provides the file.
fn outputs(pairs: &[(&str, String)]) {
    let Some(path) = std::env::var_os("GITHUB_OUTPUT").filter(|path| !path.is_empty()) else {
        return;
    };
    if let Ok(mut file) = fs::OpenOptions::new().append(true).create(true).open(path) {
        for (key, value) in pairs {
            let _ = writeln!(file, "{key}={value}");
        }
    }
}

/// A version is released once: no release (draft or public) and no tag may carry it yet.
pub fn version_is_free(forge: &Forge, tag: &str) -> Result<()> {
    if forge.release(tag)?.is_some() {
        return Err(failure(format!(
            "release {tag} exists: a version is released once, raise the version for a new release"
        )));
    }
    if forge.tag_commit(tag)?.is_some() {
        return Err(failure(format!(
            "a tag {tag} exists already: a version is released once"
        )));
    }
    Ok(())
}

fn require_current(forge: &Forge, job: &Job) -> Result<()> {
    let head = forge.branch_head(BRANCH)?;
    if head == job.sha {
        Ok(())
    } else {
        Err(failure(format!(
            "source {} is no longer {BRANCH} (now {head}); the release is refused",
            job.sha
        )))
    }
}

/// Refuse a release that must not start: another source than main, a version already released, a red verdict.
pub fn preflight(job: &Job, forge: &Forge) -> Result<()> {
    require_current(forge, job)?;
    version_is_free(forge, &job.public_tag())?;
    match forge.context_status(&job.sha, VERDICT)?.as_deref() {
        Some("failure" | "error") => Err(failure(format!(
            "the verdict of {} is red ({VERDICT}); a release is made of a green commit",
            job.sha
        ))),
        Some(_) => Ok(()),
        None => {
            eprintln!("ccid release: the commit has no {VERDICT} status (not red); the build checks prove it");
            Ok(())
        }
    }
}

fn describe(job: &Job) -> String {
    format!(
        "{} {} ({} release) of {}",
        job.project,
        job.plan.version,
        job.plan.class,
        &job.sha[..12]
    )
}

// ---- the third-party lock refresh ------------------------------------------------------------------------------

enum Refresh {
    Unchanged,
    /// A lock-only commit was pushed to main and the release dispatched again on it.
    Moved(String),
}

fn tracked_locks(repo: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "-z", "--", "Cargo.lock", "**/Cargo.lock"])
        .output()
        .map_err(|error| failure(format!("cannot run git: {error}")))?;
    Ok(output.status.success() && !output.stdout.is_empty())
}

fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// The identity a refresh commit carries: the one the environment names, else the repository's own.
fn identity(repo: &Path) -> Result<(String, String)> {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let name =
        env("GIT_AUTHOR_NAME").or_else(|| git_output(repo, &["config", "--get", "user.name"]));
    let email =
        env("GIT_AUTHOR_EMAIL").or_else(|| git_output(repo, &["config", "--get", "user.email"]));
    match (name, email) {
        (Some(name), Some(email)) => Ok((name, email)),
        _ => Err(failure(
            "the third-party lock refresh changed Cargo.lock but git has no identity to commit it with (GIT_AUTHOR_NAME and GIT_AUTHOR_EMAIL, or user.name and user.email); nothing was committed",
        )),
    }
}

/// The lock files `ccid lock-refresh --third-party` moved, from its one-line JSON report.
fn changed_locks(report: &str) -> Result<Vec<String>> {
    let report: serde_json::Value = report
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str(line).ok())
        .ok_or_else(|| failure("lock-refresh printed no report"))?;
    Ok(report
        .get("changed")
        .and_then(|list| list.as_array())
        .into_iter()
        .flatten()
        .filter_map(|path| path.as_str().map(str::to_owned))
        .collect())
}

/// Commit the refreshed locks as one lock-only commit; returns the new commit.
fn commit_locks(
    repo: &Path,
    changed: &[String],
    version: &super::version::Version,
) -> Result<String> {
    // A refresh that moves again right after a refresh would loop: stop instead.
    let last = git_output(repo, &["log", "-1", "--format=%B"]).unwrap_or_default();
    if last.lines().any(|line| line.trim() == REFRESH_TRAILER) {
        return Err(failure(format!(
            "the third-party locks moved again ({}) although the last commit is a lock refresh already; nothing is committed",
            changed.join(", ")
        )));
    }
    let (name, email) = identity(repo)?;
    let mut add = Command::new("git");
    add.arg("-C").arg(repo).args(["add", "--"]).args(changed);
    if !add.status()?.success() {
        return Err(failure("git add of the refreshed locks failed"));
    }
    let message =
        format!("lock-refresh: third-party dependencies for {version}\n\n{REFRESH_TRAILER}\n");
    let committed = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            &format!("user.name={name}"),
            "-c",
            &format!("user.email={email}"),
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            &message,
        ])
        .status()?;
    if !committed.success() {
        return Err(failure("git commit of the refreshed locks failed"));
    }
    git_head(repo)
}

/// Push HEAD to `url`'s main with the job token. The system git configuration is switched off: a runner
/// may route the forge's URL to a local copy, and a push must reach the forge itself.
fn push_head(repo: &Path, url: &str, token: &str) -> Result<()> {
    let pushed = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["push", "--quiet", url, &format!("HEAD:refs/heads/{BRANCH}")])
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: token {token}"),
        )
        .status()?;
    if pushed.success() {
        Ok(())
    } else {
        Err(failure(format!(
            "the refreshed locks could not be pushed to {BRANCH} (moved, protected or refused); commit them through the repository's flow and release again"
        )))
    }
}

/// The refresh of the third-party locks (`ccid lock-refresh --third-party`) that every release run starts with.
/// A moved lock must be in the commit that is released, so it is committed to main and the release starts
/// again on that commit: this run releases nothing.
fn refresh_locks(job: &Job, forge: &Forge, server_url: &str) -> Result<Refresh> {
    if !tracked_locks(&job.repo)? {
        return Ok(Refresh::Unchanged);
    }
    let output = Command::new(std::env::current_exe()?)
        .current_dir(&job.repo)
        .args(["lock-refresh", "--repo", ".", "--third-party"])
        .env_remove("CCID_TARGET_LOCK_HELD")
        .env_remove("CCID_CACHE_CHILD")
        .output()
        .map_err(|error| failure(format!("cannot run lock-refresh: {error}")))?;
    if !output.status.success() {
        return Err(failure(format!(
            "the third-party lock refresh failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let changed = changed_locks(&String::from_utf8_lossy(&output.stdout))?;
    if changed.is_empty() {
        eprintln!("ccid release: the third-party locks are current");
        return Ok(Refresh::Unchanged);
    }
    let new = commit_locks(&job.repo, &changed, &job.plan.version)?;
    let token = ["CCID_FORGE_TOKEN", "FORGEJO_TOKEN", "GITHUB_TOKEN"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
        .ok_or_else(|| failure("no job token to push the refreshed locks with"))?;
    let url = format!(
        "{}/{}.git",
        server_url.trim_end_matches('/'),
        forge.repository()
    );
    push_head(&job.repo, &url, &token)?;
    forge.dispatch("release.yml", BRANCH)?;
    Ok(Refresh::Moved(new))
}

// ---- build -----------------------------------------------------------------------------------------------------

fn collect(dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let names = draft::snapshot(dir)?;
    if names.is_empty() {
        return Err(failure(format!(
            "the build checks left no file in {}: a release is made of the files they leave in $CCID_RELEASE_DIR",
            dir.display()
        )));
    }
    if names.contains_key(SUMS) {
        return Err(failure(format!(
            "the build left a {SUMS}: ccid writes that file itself"
        )));
    }
    Ok(names
        .into_keys()
        .map(|name| {
            let path = dir.join(&name);
            (name, path)
        })
        .collect())
}

/// `ccid release build`: the release job.
pub fn build(repo: &Path) -> Result<ExitCode> {
    let job = Job::load(repo, None)?;
    let forge = Forge::from_env()?;
    let server = std::env::var("GITHUB_SERVER_URL").unwrap_or_default();
    eprintln!("ccid release: {}", describe(&job));
    preflight(&job, &forge)?;
    if let Refresh::Moved(new) = refresh_locks(&job, &forge, &server)? {
        eprintln!(
            "ccid release: the third-party locks moved; they are committed as {new} and the release starts again on it. This run releases nothing."
        );
        outputs(&[("released", "false".into()), ("legs", String::new())]);
        return Ok(ExitCode::SUCCESS);
    }
    let dir = tempfile::Builder::new().prefix("ccid-release-").tempdir()?;
    if !job.run_checks(&job.config.build, dir.path(), false)? {
        return Ok(ExitCode::from(1));
    }
    let files = collect(dir.path())?;
    let mut draft = Draft::ensure(&forge, &job.sha, &job.title(), &job.notes()?)?;
    draft.settle_existing(&files)?;
    draft.upload_missing(&files)?;
    for (name, path) in &files {
        let size = fs::metadata(path)?.len();
        if draft.release().asset(name).map(|asset| asset.size) != Some(size) {
            return Err(failure(format!(
                "the draft does not hold {name} as it should"
            )));
        }
    }
    eprintln!(
        "ccid release: {} files in the draft {}; platform legs: {}",
        files.len(),
        draft.release().tag_name,
        if job.plan.legs.is_empty() {
            "none".to_owned()
        } else {
            job.plan.legs.join(", ")
        }
    );
    outputs(&[
        ("released", "true".into()),
        ("legs", job.plan.legs.join(" ")),
        ("version", job.plan.version.to_string()),
        ("class", job.plan.class.to_string()),
    ]);
    Ok(ExitCode::SUCCESS)
}

// ---- legs ------------------------------------------------------------------------------------------------------

/// `ccid release leg <os>`: the declared leg check, run on its label against the draft.
pub fn leg(repo: &Path, os: &str) -> Result<ExitCode> {
    let job = Job::load(repo, None)?;
    let check = job
        .config
        .legs
        .get(os)
        .ok_or_else(|| failure(format!("[release] declares no {os} leg")))?
        .clone();
    if !job.plan.legs.contains(&os) {
        eprintln!(
            "ccid release: {} is a {} release: it has no {os} leg",
            job.plan.version, job.plan.class
        );
        return Ok(ExitCode::SUCCESS);
    }
    let forge = Forge::from_env()?;
    let Some(draft) = Draft::find(&forge, &job.sha)? else {
        return Err(failure(format!(
            "there is no draft build-{}: the release run has not left one",
            job.sha
        )));
    };
    if !draft.is_draft() {
        eprintln!(
            "ccid release: build-{} is public already: nothing is left to run",
            &job.sha[..12]
        );
        return Ok(ExitCode::SUCCESS);
    }
    require_current(&forge, &job)?;
    let dir = tempfile::Builder::new()
        .prefix("ccid-release-leg-")
        .tempdir()?;
    Ok(if job.run_checks(&[check], dir.path(), true)? {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

// ---- promote ---------------------------------------------------------------------------------------------------

/// What a seal check may have changed: nothing but the declared manifests (and the sums file).
fn verify_seal(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    manifests: &[String],
) -> Result<()> {
    let free = |name: &str| name == SUMS || manifests.iter().any(|m| m == name);
    for (name, sha) in before {
        match after.get(name) {
            Some(now) if now == sha => {}
            Some(_) if free(name) => {}
            Some(_) => {
                return Err(failure(format!(
                    "the seal checks changed {name}: they may replace the declared manifests only"
                )))
            }
            // The checksum file is derived again by ccid; any other file stays.
            None if name == SUMS => {}
            None => {
                return Err(failure(format!(
                    "the seal checks removed {name}: a release loses no file"
                )))
            }
        }
    }
    for name in after.keys() {
        if !before.contains_key(name) && !free(name) {
            return Err(failure(format!(
                "the seal checks added {name}, which is not a declared manifest"
            )));
        }
    }
    Ok(())
}

/// `ccid release promote`: the draft of this commit becomes the public release `v<version>`.
pub fn promote(repo: &Path) -> Result<ExitCode> {
    let job = Job::load(repo, None)?;
    let forge = Forge::from_env()?;
    let public = job.public_tag();
    eprintln!("ccid release: promoting {}", describe(&job));
    require_current(&forge, &job)?;
    if let Some(release) = forge.release(&public)? {
        // A repeat: it must be this commit's, public and complete.
        if release.draft {
            return Err(failure(format!(
                "release {public} is a draft of another run; nothing is promoted"
            )));
        }
        check_public(&forge, &job, &public)?;
        let work = tempfile::tempdir()?;
        for asset in &release.assets {
            forge.download(asset, &work.path().join(&asset.name))?;
        }
        let held = draft::snapshot(work.path())?;
        let expected = draft::sums_text(&held);
        let sums = fs::read_to_string(work.path().join(SUMS)).unwrap_or_default();
        if sums != expected {
            return Err(failure(format!(
                "the {SUMS} of {public} does not cover its assets"
            )));
        }
        eprintln!("ccid release: {public} is public already");
        return Ok(ExitCode::SUCCESS);
    }
    let mut draft = Draft::find(&forge, &job.sha)?
        .ok_or_else(|| failure(format!("there is no release build-{} to promote", job.sha)))?;
    draft.require_draft()?;
    let work = tempfile::Builder::new()
        .prefix("ccid-release-seal-")
        .tempdir()?;
    draft.fetch_all(work.path())?;
    let before = draft::snapshot(work.path())?;
    if !job.config.seal.is_empty() && !job.run_checks(&job.config.seal, work.path(), false)? {
        eprintln!("ccid release: the seal checks refused the draft; nothing is promoted");
        return Ok(ExitCode::from(1));
    }
    let after = draft::snapshot(work.path())?;
    verify_seal(&before, &after, &job.config.manifests)?;
    for name in &job.config.manifests {
        if after.contains_key(name) && before.get(name) != after.get(name) {
            eprintln!(
                "ccid release: the draft's {name} is not what its evidence says; replacing it"
            );
            draft.put(&work.path().join(name))?;
        }
    }
    // The checksum file covers every asset as it is now, so it comes last.
    let sums = draft::sums_text(&after);
    let sums_path = work.path().join(SUMS);
    fs::write(&sums_path, &sums)?;
    let current = draft::sha256_file(&sums_path)?;
    if before.get(SUMS) != Some(&current) {
        eprintln!("ccid release: writing {SUMS}");
        draft.put(&sums_path)?;
    }
    draft.reload()?;
    let mut expected: BTreeMap<String, u64> = BTreeMap::new();
    for name in after.keys().chain([&SUMS.to_owned()]) {
        expected.insert(name.clone(), fs::metadata(work.path().join(name))?.len());
    }
    let held: BTreeMap<String, u64> = draft
        .release()
        .assets
        .iter()
        .map(|asset| (asset.name.clone(), asset.size))
        .collect();
    if held != expected {
        return Err(failure(
            "the draft does not hold exactly the sealed files; nothing is promoted",
        ));
    }
    version_is_free(&forge, &public)?;
    require_current(&forge, &job)?;
    forge.publish(
        draft.release().id,
        &public,
        &job.plan.version.to_string(),
        &job.notes()?,
    )?;
    check_public(&forge, &job, &public)?;
    eprintln!(
        "ccid release: promoted build-{} as {public}",
        &job.sha[..12]
    );
    Ok(ExitCode::SUCCESS)
}

/// The public release `tag` is public and its tag names this commit.
fn check_public(forge: &Forge, job: &Job, tag: &str) -> Result<()> {
    let release = forge
        .release(tag)?
        .ok_or_else(|| failure(format!("release {tag} is not there")))?;
    if release.draft {
        return Err(failure(format!("release {tag} is still a draft")));
    }
    match forge.tag_commit(tag)? {
        Some(commit) if commit == job.sha => Ok(()),
        Some(commit) => Err(failure(format!(
            "the tag {tag} names {commit}, not {}",
            job.sha
        ))),
        None => Err(failure(format!("the tag {tag} does not exist"))),
    }
}

// ---- the commands for scripts ----------------------------------------------------------------------------------

/// `ccid release preflight`.
pub fn preflight_command(repo: &Path, commit: Option<&str>) -> Result<ExitCode> {
    let job = Job::load(repo, commit)?;
    let forge = Forge::from_env()?;
    preflight(&job, &forge)?;
    println!(
        "{} may proceed. Platforms: {}.",
        describe(&job),
        if job.plan.platforms.is_empty() {
            "none (a library)".to_owned()
        } else {
            job.plan.platforms.join(" ")
        }
    );
    Ok(ExitCode::SUCCESS)
}

/// `ccid release put FILE...`: files into the draft of the commit, added or replaced (a draft only).
pub fn put(repo: &Path, commit: Option<&str>, files: &[PathBuf]) -> Result<ExitCode> {
    let job = Job::load(repo, commit)?;
    let forge = Forge::from_env()?;
    let mut draft = Draft::find(&forge, &job.sha)?
        .ok_or_else(|| failure(format!("there is no draft build-{}", job.sha)))?;
    for file in files {
        if file.file_name().and_then(|n| n.to_str()) == Some(SUMS) {
            return Err(failure(format!(
                "{SUMS} is written by ccid release promote"
            )));
        }
        let outcome = draft.put(file)?;
        println!("{}: {outcome:?}", file.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// `ccid release fetch NAME... --into DIR` (no names: every asset).
pub fn fetch(repo: &Path, commit: Option<&str>, names: &[String], into: &Path) -> Result<ExitCode> {
    let job = Job::load(repo, commit)?;
    let forge = Forge::from_env()?;
    let draft = Draft::find(&forge, &job.sha)?
        .ok_or_else(|| failure(format!("there is no release build-{}", job.sha)))?;
    fs::create_dir_all(into)?;
    if names.is_empty() {
        draft.fetch_all(into)?;
    }
    for name in names {
        if !super::plain_asset_name(name) {
            return Err(failure(format!("{name:?} is not an asset name")));
        }
        draft.fetch(name, &into.join(name))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `ccid release discard`: delete the draft of the commit (never a public release).
pub fn discard(repo: &Path, commit: Option<&str>) -> Result<ExitCode> {
    let job = Job::load(repo, commit)?;
    let forge = Forge::from_env()?;
    match Draft::find(&forge, &job.sha)? {
        Some(draft) => {
            draft.discard()?;
            println!("deleted the draft build-{}", job.sha);
        }
        None => println!("there is no draft build-{}", job.sha),
    }
    Ok(ExitCode::SUCCESS)
}

/// `ccid release sums DIR`: the checksum file of a directory's files, on standard output.
pub fn sums(dir: &Path) -> Result<ExitCode> {
    print!("{}", draft::sums_text(&draft::snapshot(dir)?));
    Ok(ExitCode::SUCCESS)
}

/// `ccid release version` and `ccid release platforms`.
pub fn describe_version(repo: &Path, platforms: bool) -> Result<ExitCode> {
    let (manifest, _) = crate::load_manifest(repo, Path::new(".ci/ccid.toml"))?;
    let config = manifest
        .release
        .clone()
        .ok_or_else(|| failure("the manifest declares no [release]"))?;
    config.validate(&manifest)?;
    let plan = config.plan(super::version::read(repo, &config.version)?);
    if platforms {
        for platform in &plan.platforms {
            println!("{platform}");
        }
    } else {
        println!("{} {}", plan.version, plan.class);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_seal_may_replace_a_declared_manifest_and_the_sums_only() {
        let manifests = vec!["manifest.json".to_owned()];
        let before = map(&[("a.bin", "1"), ("manifest.json", "2")]);
        assert!(verify_seal(
            &before,
            &map(&[("a.bin", "1"), ("manifest.json", "3")]),
            &manifests
        )
        .is_ok());
        assert!(verify_seal(
            &before,
            &map(&[("a.bin", "1"), ("manifest.json", "2"), (SUMS, "9")]),
            &manifests
        )
        .is_ok());
        let changed = verify_seal(
            &before,
            &map(&[("a.bin", "9"), ("manifest.json", "2")]),
            &manifests,
        );
        assert!(changed.unwrap_err().to_string().contains("changed a.bin"));
        let removed = verify_seal(&before, &map(&[("manifest.json", "2")]), &manifests);
        assert!(removed.unwrap_err().to_string().contains("removed a.bin"));
        let added = verify_seal(
            &before,
            &map(&[("a.bin", "1"), ("manifest.json", "2"), ("x", "1")]),
            &manifests,
        );
        assert!(added.unwrap_err().to_string().contains("added x"));
    }

    #[test]
    fn a_seal_may_delete_the_checksum_file_it_will_not_see_again_but_no_other_file() {
        let manifests = vec!["manifest.json".to_owned()];
        let before = map(&[("a.bin", "1"), ("manifest.json", "2"), (SUMS, "9")]);
        assert!(verify_seal(
            &before,
            &map(&[("a.bin", "1"), ("manifest.json", "2")]),
            &manifests
        )
        .is_ok());
        let manifest = verify_seal(&before, &map(&[("a.bin", "1"), (SUMS, "9")]), &manifests);
        assert!(manifest
            .unwrap_err()
            .to_string()
            .contains("removed manifest.json"));
    }

    #[test]
    fn a_manifest_the_draft_lacks_may_be_added_by_the_seal() {
        let manifests = vec!["manifest.json".to_owned()];
        let before = map(&[("a.bin", "1")]);
        assert!(verify_seal(
            &before,
            &map(&[("a.bin", "1"), ("manifest.json", "2")]),
            &manifests
        )
        .is_ok());
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
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

    fn repository_with_lock() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.name", "Lock Test"]);
        git(
            dir.path(),
            &["config", "user.email", "lock@example.invalid"],
        );
        fs::write(dir.path().join("Cargo.lock"), "one\n").unwrap();
        fs::write(dir.path().join("other.txt"), "x\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "fixture"]);
        dir
    }

    #[test]
    fn a_release_check_gets_two_hours_or_what_the_job_allows() {
        assert_eq!(check_deadline(None), "7200");
        assert_eq!(check_deadline(Some("2700")), "7200");
        assert_eq!(check_deadline(Some("10000")), "10000");
        assert_eq!(check_deadline(Some("soon")), "7200");
    }

    #[test]
    fn the_report_line_names_the_changed_locks() {
        let report =
            "noise\n{\"mode\":\"third-party\",\"changed\":[\"Cargo.lock\",\"x/Cargo.lock\"]}\n";
        assert_eq!(
            changed_locks(report).unwrap(),
            vec!["Cargo.lock", "x/Cargo.lock"]
        );
        assert!(changed_locks("{\"changed\":[]}\n").unwrap().is_empty());
        assert!(changed_locks("not json\n").is_err());
    }

    #[test]
    fn a_refresh_is_one_lock_only_commit_pushed_to_main_and_never_two_in_a_row() {
        let repo = repository_with_lock();
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "-q", "--bare"]);
        fs::write(repo.path().join("Cargo.lock"), "two\n").unwrap();
        let version = crate::release::version::Version::parse("1.2.0").unwrap();
        let new = commit_locks(repo.path(), &["Cargo.lock".to_owned()], &version).unwrap();
        assert_eq!(new, git(repo.path(), &["rev-parse", "HEAD"]));
        assert_eq!(
            git(repo.path(), &["show", "--name-only", "--format=", "HEAD"]),
            "Cargo.lock"
        );
        let message = git(repo.path(), &["log", "-1", "--format=%B"]);
        assert!(
            message.contains(REFRESH_TRAILER) && message.contains("1.2.0"),
            "{message}"
        );
        push_head(
            repo.path(),
            &remote.path().display().to_string(),
            "token12345",
        )
        .unwrap();
        assert_eq!(git(remote.path(), &["rev-parse", "main"]), new);
        // The lock moves again straight after a refresh: refused, nothing committed.
        fs::write(repo.path().join("Cargo.lock"), "three\n").unwrap();
        let again = commit_locks(repo.path(), &["Cargo.lock".to_owned()], &version);
        assert!(again
            .unwrap_err()
            .to_string()
            .contains("lock refresh already"));
        assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), new);
    }

    #[test]
    fn a_push_that_the_remote_refuses_is_an_error_naming_the_way_out() {
        let repo = repository_with_lock();
        let error = push_head(repo.path(), "/nonexistent/remote.git", "token12345").unwrap_err();
        assert!(error.to_string().contains("could not be pushed"), "{error}");
    }
}

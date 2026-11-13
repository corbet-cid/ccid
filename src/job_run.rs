//! `ccid job-run`: what a rendered Forgejo workflow step does, kept in the binary.
//!
//! The rendered workflow is a short shim that resolves a ccid (centrally pointed at by the runner or manifest) and runs
//! `ccid job-run`. Everything that used to be shell inside every repository's workflow lives here, so a fix to it is one
//! ccid release, not one push per repository:
//!
//! 1. optionally claim a stable source directory (`CCID_SOURCE_ROOT`, slots per repository and job, claimed by a lock
//!    directory that holds the pid and start time of this process; the process becomes the check by `exec`, so the pid
//!    lives as long as the job and no file descriptor is inherited by daemons),
//! 2. fetch the exact commit (once through the runner's own routing, which may send it to a local copy; if that does not have it,
//!    once from the forge itself), Git LFS
//!    content through a credential helper, sibling repositories read-only,
//! 3. check that the generated adapters match this binary (`render --check`),
//! 4. run the declared command of the job.
//!
//! The module does not know the forge's name or any operator address: the caller passes the server URL and the token
//! comes from the environment.

use crate::{failure, jobs, render, Result};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    thread::sleep,
    time::{Duration, Instant},
};

/// Everything the workflow knows about the job; the shim passes these from the workflow context.
#[derive(Debug, Clone)]
pub struct Options {
    pub job: String,
    /// `owner/name`.
    pub repository: String,
    /// Full commit id to check out.
    pub sha: String,
    /// The forge's base URL, e.g. `https://forge.example/`.
    pub server_url: String,
    /// The owner of the repository (for unqualified sibling names).
    pub owner: String,
    /// Run the job against this existing source export instead of fetching the commit. The land
    /// gate is the second producer of CI's job and must use the same entry point (P5).
    pub local: Option<PathBuf>,
    /// Mirror mode (`--mirror`): the pull-request or merge-request check of a mirror platform. The
    /// checkout is the platform's own (`local`, else the current directory); `repository`, `sha`,
    /// `server_url` and `owner` are read from it, not passed.
    pub mirror: bool,
}

/// Timing knobs; tests shorten them.
#[derive(Debug, Clone)]
pub struct Pace {
    /// The git program (tests substitute a fake).
    pub git: std::ffi::OsString,
    /// A shared Git LFS object store (`CCID_LFS_STORAGE`, named by the runner): every slot and repository of that runner then fetches an object once.
    pub lfs_storage: Option<PathBuf>,
    pub slot_poll: Duration,
    pub slot_timeout: Duration,
}

impl Default for Pace {
    fn default() -> Self {
        Self {
            git: "git".into(),
            lfs_storage: std::env::var_os("CCID_LFS_STORAGE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            slot_poll: Duration::from_secs(5),
            slot_timeout: Duration::from_secs(
                std::env::var("CI_TIMEOUT")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(2700),
            ),
        }
    }
}

/// Start time of a process (field 22 of `/proc/<pid>/stat`), the pid's identity across reuse.
fn start_time(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses: fields start after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    // After ')' the first field is the state (field 3), so field 22 is index 19.
    rest.split_whitespace().nth(19).map(str::to_owned)
}

fn owner_line(pid: u32) -> String {
    format!("{pid} {}", start_time(pid).unwrap_or_default())
}

/// Claim one of `slots` source directories below `base`; returns the claimed `slot-N` directory.
pub fn claim_slot(base: &Path, slots: usize, pid: u32, pace: &Pace) -> Result<PathBuf> {
    fs::create_dir_all(base)?;
    let deadline = Instant::now() + pace.slot_timeout;
    loop {
        for n in 1..=slots {
            let lock = base.join(format!("slot-{n}.lock"));
            loop {
                match fs::create_dir(&lock) {
                    Ok(()) => {
                        fs::write(lock.join("owner"), owner_line(pid))?;
                        let dir = base.join(format!("slot-{n}"));
                        fs::create_dir_all(&dir)?;
                        return Ok(dir);
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        let held = fs::read_to_string(lock.join("owner")).unwrap_or_default();
                        let mut parts = held.split_whitespace();
                        let owner = parts.next().and_then(|text| text.parse::<u32>().ok());
                        let started = parts.next().map(str::to_owned);
                        let gone = match owner {
                            // An empty owner file is a claim in progress, not a dead one.
                            None => false,
                            Some(owner) => start_time(owner) != started,
                        };
                        if !gone {
                            break;
                        }
                        // The owner crashed or was killed: one process wins the rename and reclaims, the others retry.
                        let dead = base.join(format!("slot-{n}.dead.{pid}"));
                        if fs::rename(&lock, &dead).is_ok() {
                            let _ = fs::remove_dir_all(&dead);
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(failure(format!(
                "no free source slot under {}",
                base.display()
            )));
        }
        sleep(pace.slot_poll);
    }
}

fn git(pace: &Pace, dir: &Path) -> Command {
    let mut command = Command::new(&pace.git);
    command.current_dir(dir);
    command
}

fn run_ok(command: &mut Command, what: &str) -> Result<()> {
    let status = command
        .status()
        .map_err(|error| failure(format!("{what}: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(failure(format!("{what} failed ({status})")))
    }
}

/// Fetch `want` (a commit id or a ref) into `dir`: once through the runner's own routing and, if that fails, once from the forge itself.
///
/// A runner may send a fetch of the forge's repositories to a local copy (its system git configuration rewrites the URL). That copy is
/// a push mirror and lags a push by seconds, while the job a push starts is picked up within a fraction of a second, so the commit
/// the event names is often not in the copy yet ("upload-pack: not our ref"). The forge is authoritative and has every commit a push
/// event names: when the nearer copy cannot serve the fetch the system and counted git configuration that route it are switched off
/// and the same fetch goes to the forge. There is no waiting and no repetition: a copy that is behind costs one failed local fetch,
/// never a pause, and a failure of the forge itself is a failure of the job.
pub fn fetch(dir: &Path, want: &str, token: &str, pace: &Pace) -> Result<()> {
    let header = format!("http.extraHeader=Authorization: token {token}");
    let attempt = |forge_itself: bool| {
        let mut command = git(pace, dir);
        command.args([
            "-c", &header, "fetch", "--quiet", "--depth", "1", "origin", want,
        ]);
        if forge_itself {
            command
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_COUNT", "0");
        }
        command
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };
    if attempt(false) {
        return Ok(());
    }
    eprintln!("local copy lacks {want}, fetching from the forge");
    if attempt(true) {
        return Ok(());
    }
    Err(failure(format!("cannot fetch {want}")))
}

/// Check out the exact commit in `dir` (initialising it), with Git LFS content when the repository tracks files with LFS.
pub fn checkout(dir: &Path, options: &Options, token: &str, pace: &Pace) -> Result<()> {
    run_ok(git(pace, dir).args(["init", "--quiet", "."]), "git init")?;
    let _ = git(pace, dir).args(["remote", "remove", "origin"]).status();
    let url = format!(
        "{}/{}.git",
        options.server_url.trim_end_matches('/'),
        options.repository
    );
    run_ok(
        git(pace, dir).args(["remote", "add", "origin", &url]),
        "git remote add",
    )?;
    fetch(dir, &options.sha, token, pace)?;
    run_ok(
        git(pace, dir).env("GIT_LFS_SKIP_SMUDGE", "1").args([
            "checkout",
            "--quiet",
            "--force",
            "--detach",
            "FETCH_HEAD",
        ]),
        "git checkout",
    )?;
    let attributes = git(pace, dir)
        .args(["show", "FETCH_HEAD:.gitattributes"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    if attributes.contains("filter=lfs") {
        // The LFS endpoint wants HTTP Basic, and its batch answer carries a credential for the object downloads: a credential helper
        // supplies Basic exactly once (a fixed extra header would be sent twice and the forge answers 400).
        run_ok(
            git(pace, dir).args(["lfs", "install", "--local", "--skip-smudge"]),
            "git lfs install",
        )?;
        if let Some(storage) = &pace.lfs_storage {
            // Objects are content-addressed, so one store serves all slots and repositories; an object already in it is not downloaded again.
            fs::create_dir_all(storage)?;
            run_ok(
                git(pace, dir).args(["config", "lfs.storage"]).arg(storage),
                "git config lfs.storage",
            )?;
        }
        // With a shared store several accounts write to it: the objects and their directories are group-writable (umask 002; the store's setgid
        // directory keeps the group). No shell glue beyond the umask.
        let mut pull = if pace.lfs_storage.is_some() {
            let mut command = Command::new("sh");
            command
                .current_dir(dir)
                .args(["-c", "umask 002; exec \"$0\" \"$@\""])
                .arg(&pace.git);
            command
        } else {
            git(pace, dir)
        };
        run_ok(
            pull.env("CCID_LFS_TOKEN", token).args([
                "-c",
                "credential.helper=!f() { echo username=x-access-token; echo \"password=$CCID_LFS_TOKEN\"; }; f",
                "lfs",
                "pull",
                "origin",
            ]),
            "git lfs pull",
        )?;
    }
    run_ok(
        git(pace, dir).args(["clean", "--quiet", "-ffdx"]),
        "git clean",
    )
}

/// Stage sibling repositories read-only as `<checkout>.siblings/<name>`; returns `(dir, "name=sha,...")`.
pub fn stage_siblings(
    checkout_dir: &Path,
    siblings: &BTreeMap<String, String>,
    options: &Options,
    token: &str,
    pace: &Pace,
) -> Result<(PathBuf, String)> {
    let mut name = checkout_dir.as_os_str().to_owned();
    name.push(".siblings");
    let root = PathBuf::from(name);
    if root.exists() {
        fs::remove_dir_all(&root)?;
    }
    fs::create_dir_all(&root)?;
    let mut revisions = Vec::new();
    for (repository, want) in siblings {
        let (qualified, short) = match repository.split_once('/') {
            Some((_, short)) => (repository.clone(), short.to_owned()),
            None => (
                format!("{}/{repository}", options.owner),
                repository.clone(),
            ),
        };
        let dir = root.join(&short);
        fs::create_dir_all(&dir)?;
        run_ok(
            git(pace, &dir).args(["init", "--quiet"]),
            "git init (sibling)",
        )?;
        let url = format!(
            "{}/{qualified}.git",
            options.server_url.trim_end_matches('/')
        );
        run_ok(
            git(pace, &dir).args(["remote", "add", "origin", &url]),
            "git remote add (sibling)",
        )?;
        let reference = if want == "main" {
            "HEAD"
        } else {
            want.as_str()
        };
        fetch(&dir, reference, token, pace)?;
        run_ok(
            git(pace, &dir).args(["checkout", "--quiet", "--force", "--detach", "FETCH_HEAD"]),
            "git checkout (sibling)",
        )?;
        let have = String::from_utf8_lossy(
            &git(pace, &dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .map_err(|error| failure(format!("git rev-parse: {error}")))?
                .stdout,
        )
        .trim()
        .to_owned();
        if want != "main" && &have != want {
            return Err(failure(format!(
                "sibling {qualified} is {have}, not the pinned {want}"
            )));
        }
        eprintln!("sibling {qualified} at {have}");
        revisions.push(format!("{short}={have}"));
    }
    Ok((root, revisions.join(",")))
}

/// The environment variables a job's command runs with, besides the process environment.
fn command_environment(
    plan: &jobs::Plan,
    options: &Options,
    exe: &Path,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for (key, value) in &plan.environment {
        env.insert(key.clone(), value.clone());
    }
    env.insert("CI_COMMIT_SHA".into(), options.sha.clone());
    // A mirror checkout whose origin is unknown has no repository URL; nothing is invented for it.
    if !options.server_url.is_empty() {
        env.insert(
            "CI_REPOSITORY_URL".into(),
            format!(
                "{}/{}",
                options.server_url.trim_end_matches('/'),
                options.repository
            ),
        );
    }
    if !jobs::is_self_checks_command(&plan.command) {
        env.insert("CCID_BIN".into(), exe.display().to_string());
        env.insert("CHECKS".into(), plan.checks.join(","));
    }
    env
}

/// The profile variables (debug levels, colour) an older rendered workflow exports and the shared table replaces; a job that declares one itself keeps it.
fn stripped_profile_variables(declared: &BTreeMap<String, String>) -> Vec<&'static str> {
    [
        "CARGO_PROFILE_DEV_DEBUG",
        "CARGO_PROFILE_TEST_DEBUG",
        "CARGO_PROFILE_RELEASE_DEBUG",
        // The shared profile table carries the colour setting as well (a workstation shell and CI must key the same).
        "CARGO_TERM_COLOR",
    ]
    .into_iter()
    .filter(|key| !declared.contains_key(*key))
    .collect()
}

/// Run the whole job: claim, fetch, check the adapters, exec the declared command. Does not return on success of the exec.
#[cfg(unix)]
pub fn run(options: &Options, pace: &Pace) -> Result<ExitCode> {
    if options.mirror {
        return run_mirror(options, pace);
    }
    let token = std::env::var("CCID_FORGE_TOKEN").ok();
    if token.is_none() && options.local.is_none() {
        return Err(failure("CCID_FORGE_TOKEN (the job token) is required"));
    }
    let exe = std::env::current_exe()?;
    eprintln!(
        "ccid job-run: tool revision {} for {} at {}",
        crate::SOURCE_REVISION,
        options.job,
        options.sha
    );
    if let Some(source) = &options.local {
        if !source.is_dir() {
            return Err(failure(format!(
                "--source {} is not a directory",
                source.display()
            )));
        }
        if source.join(".git").exists() {
            let head = git(pace, source)
                .args(["rev-parse", "HEAD"])
                .output()
                .map_err(|error| failure(format!("git rev-parse: {error}")))?;
            let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();
            if !head.is_empty() && head != options.sha {
                return Err(failure(format!(
                    "--source is {head}, not the declared {}",
                    options.sha
                )));
            }
        }
        std::env::set_current_dir(source)?;
        let plan = jobs::plan(
            Path::new("."),
            Path::new(".ci/ccid.toml"),
            &options.job,
            None,
        )?;
        return execute_job(options, &exe, plan, None, pace);
    }
    let dir = match std::env::var_os("CCID_SOURCE_ROOT").filter(|value| !value.is_empty()) {
        Some(root) => {
            let slots = std::env::var("CCID_SOURCE_SLOTS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(2);
            let base = Path::new(&root)
                .join(&options.repository)
                .join(&options.job);
            let dir = claim_slot(&base, slots, std::process::id(), pace)?;
            if let Some(env_file) = std::env::var_os("GITHUB_ENV") {
                use io::Write;
                if let Ok(mut file) = fs::OpenOptions::new().append(true).open(env_file) {
                    let _ = writeln!(file, "CCID_SOURCE_DIR={}", dir.display());
                }
            }
            dir
        }
        None => std::env::current_dir()?,
    };
    let token = token.ok_or_else(|| failure("CCID_FORGE_TOKEN (the job token) is required"))?;
    std::env::set_current_dir(&dir)?;
    checkout(&dir, options, &token, pace)?;
    let manifest = Path::new(".ci/ccid.toml");
    let plan = jobs::plan(Path::new("."), manifest, &options.job, None)?;
    execute_job(options, &exe, plan, Some((&token, &dir)), pace)
}

/// Variables a mirror check never sees: the forge's tokens, caches and tool locations (a platform's
/// own job token is scrubbed too: a check has no use for it), and the compiler-cache wrappers.
fn mirror_scrubbed(key: &str) -> bool {
    const NAMES: &[&str] = &[
        "RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "CI_JOB_TOKEN",
        "CI_BUILD_TOKEN",
        "CI_REGISTRY_PASSWORD",
        "CI_DEPLOY_PASSWORD",
    ];
    const PREFIXES: &[&str] = &["SCCACHE_", "KACHE_", "CCID_REMOTE_"];
    // Everything of ccid's own that points at the forge's infrastructure; the check's own plumbing
    // (`CCID_BIN`, `CCID_SIBLINGS_*`, `CCID_FLEET_FILE`, `CCID_CHANNEL_FILE`) is set by this process or by the script.
    const CCID: &[&str] = &[
        "CCID_FORGE_TOKEN",
        "CCID_FORGE_HOSTS",
        "CCID_RESULT_CACHE",
        "CCID_COMPILE_CACHE_CONFIG",
        "CCID_FLAKE_ARCHIVE_PUSH",
        "CCID_LFS_STORAGE",
        "CCID_SOURCE_ROOT",
        "CCID_SOURCE_SLOTS",
        "CCID_TOOL_ROOT",
        "CCID_TOOL_URL",
        "CCID_TOOL_SHA256",
        "CCID_TOOL_REVISION",
        "CCID_TOOL_TRIPLE",
        "CCID_CHANNEL_URL",
    ];
    NAMES.contains(&key)
        || CCID.contains(&key)
        || PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

/// `https://host/path` of a git remote URL without credentials, or none for what is not an https URL.
fn plain_remote(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("https://")?;
    let (authority, path) = rest.split_once('/')?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    (!host.is_empty() && path.split('/').count() == 2 && !path.contains(char::is_whitespace))
        .then(|| format!("https://{host}/{path}"))
}

/// `ccid job-run --mirror`: the pull-request or merge-request check of a mirror platform. The platform
/// has checked the commit out; this runs one verdict job's declared checks against it, with no token,
/// no result cache, no compile-cache wrapper and nothing else of ours: render decided at render time
/// that the job needs none (`mirror::forge_only_reason`), and this refuses a job that does.
#[cfg(unix)]
fn run_mirror(options: &Options, pace: &Pace) -> Result<ExitCode> {
    let exe = std::env::current_exe()?;
    let source = options.local.clone().unwrap_or_else(|| PathBuf::from("."));
    if !source.is_dir() {
        return Err(failure(format!(
            "--source {} is not a directory",
            source.display()
        )));
    }
    std::env::set_current_dir(&source)?;
    let git_output = |args: &[&str]| {
        git(pace, Path::new("."))
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|text| !text.is_empty())
    };
    let sha = git_output(&["rev-parse", "HEAD"])
        .ok_or_else(|| failure("--mirror needs a git checkout of the commit to check"))?;
    if let Some(reason) =
        render::mirror_refusal(Path::new("."), Path::new(".ci/ccid.toml"), &options.job)?
    {
        eprintln!(
            "ccid job-run --mirror: {} runs on the forge only: {reason}",
            options.job
        );
        return Ok(ExitCode::from(2));
    }
    // The repository's identity is what the platform's checkout says about itself (its `origin`, without credentials).
    let origin = git_output(&["remote", "get-url", "origin"]).and_then(|url| plain_remote(&url));
    let (server_url, repository, owner) = origin
        .as_deref()
        .and_then(|url| {
            let rest = url.strip_prefix("https://")?;
            let (host, path) = rest.split_once('/')?;
            let (owner, _) = path.split_once('/')?;
            Some((format!("https://{host}"), path.to_owned(), owner.to_owned()))
        })
        .unwrap_or_default();
    eprintln!(
        "ccid job-run --mirror: tool revision {} for {} at {sha}",
        crate::SOURCE_REVISION,
        options.job
    );
    let options = Options {
        job: options.job.clone(),
        repository,
        sha,
        server_url,
        owner,
        local: Some(source),
        mirror: true,
    };
    let plan = jobs::plan(
        Path::new("."),
        Path::new(".ci/ccid.toml"),
        &options.job,
        None,
    )?;
    execute_job(&options, &exe, plan, None, pace)
}

/// Run the declared job command after the source is staged. One definition for the scheduler
/// (`job-run`) and the land gate (`job-run --local`).
fn execute_job(
    options: &Options,
    exe: &Path,
    plan: jobs::Plan,
    push: Option<(&str, &Path)>,
    pace: &Pace,
) -> Result<ExitCode> {
    use std::os::unix::process::CommandExt;
    let manifest = Path::new(".ci/ccid.toml");
    let mut extra = command_environment(&plan, options, exe);
    // Fleet data rides on the release channel the shim fetched; only `fleet = true` checks need
    // it, and they fail closed without it, so a missing or invalid copy never stops the job here.
    if let Some(path) =
        crate::fleet::provide_for_job("job-run", &std::env::vars_os().collect(), &plan.environment)
    {
        extra.insert(crate::fleet::FLEET_FILE_ENV.into(), path);
    }
    if !plan.siblings.is_empty() {
        let Some((token, dir)) = push else {
            return Err(failure(
                "--local cannot stage sibling repositories without a job token",
            ));
        };
        let (root, revisions) = stage_siblings(dir, &plan.siblings, options, token, pace)?;
        extra.insert("CCID_SIBLINGS_DIR".into(), root.display().to_string());
        extra.insert("CCID_SIBLINGS_REVISIONS".into(), revisions);
    }
    // The generated adapters must be the ones this binary renders (a stale render fails here, with the fix named).
    // A mirror check does not ask: the copy's `origin` is the platform's, not the forge's, and the forge's
    // own per-commit verify already holds every commit to its adapters.
    if !options.mirror {
        render::render(Path::new("."), manifest, true)?;
    }
    // [runtime]: the persistent job environment the repository's job script used to set up. Its
    // locks are held from here until the job command exits (they outlive the exec below).
    let runtime = match &plan.runtime {
        Some(runtime) => {
            let mut environment: crate::Environment = std::env::vars_os().collect();
            for (key, value) in &extra {
                environment.insert(key.into(), value.into());
            }
            // Fleet aliases become git `insteadOf` rules, as the script's pairs did; without
            // fleet data the job refuses rather than fetching through a legacy URL.
            let (fleet, _) = crate::fleet::load(&environment).map_err(|error| {
                failure(format!(
                    "[runtime] needs the release channel's fleet data: {error}"
                ))
            })?;
            let repository_url = extra.get("CI_REPOSITORY_URL").cloned().ok_or_else(|| {
                failure("[runtime] needs the repository's URL: the checkout has no origin remote")
            })?;
            match crate::runtime::prepare(
                runtime,
                &environment,
                &repository_url,
                exe,
                &pace.git,
                &fleet.insteadof_pairs(),
            )? {
                crate::runtime::Setup::Ready(prepared) => {
                    extra.extend(prepared.environment.clone());
                    Some(prepared)
                }
                crate::runtime::Setup::Refused(message) => {
                    eprintln!("ccid job-run: {message}");
                    return Ok(ExitCode::from(1));
                }
            }
        }
        None => None,
    };
    // The job command as one argv, so the rendered check-shell can run it as a
    // whole. Without a generated shell this is byte-identical to before.
    let mut argv: Vec<std::ffi::OsString> = Vec::new();
    if jobs::is_self_checks_command(&plan.command) {
        argv.push(exe.into());
        argv.extend(["check", "--repo", "."].into_iter().map(Into::into));
        for check in &plan.checks {
            argv.push("--check".into());
            argv.push(check.into());
        }
    } else {
        argv.extend(plan.command.iter().map(Into::into));
    }
    let mut command = match crate::check_shell::command(Path::new("."), &argv)? {
        Some(command) => command,
        None => {
            let (program, args) = argv
                .split_first()
                .ok_or_else(|| failure("the job declares no command"))?;
            let mut command = Command::new(program);
            command.args(args);
            command
        }
    };
    command
        .envs(&extra)
        .env("CCID_SOURCE_REVISION", &options.sha);
    // The profile (debug level of dev, test and release builds) is ccid's shared table now (profiles/dev.toml, applied as cargo --config, which sccache keys the same for
    // lanes and CI). Workflows rendered earlier still export CARGO_PROFILE_*_DEBUG: those are dropped here, before ccid decides, unless the job declares them itself.
    for key in stripped_profile_variables(&plan.environment) {
        command.env_remove(key);
    }
    if options.mirror {
        for (key, _) in std::env::vars_os() {
            if mirror_scrubbed(&key.to_string_lossy()) {
                command.env_remove(key);
            }
        }
    }
    if let Some(prepared) = &runtime {
        // One ordered log, as the job scripts' `exec 2>&1` gave.
        use std::os::fd::AsFd;
        command.stderr(std::process::Stdio::from(
            io::stdout().as_fd().try_clone_to_owned()?,
        ));
        prepared.keep_across_exec()?;
    }
    let error = command.exec();
    // Still here: the locks go with this process.
    drop(runtime);
    Err(failure(format!("cannot start the job command: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write an executable stub through a child process. A stub this process wrote itself would be
    /// held open for writing, for a moment, by any child a concurrent test forks then, and the
    /// stub's own exec fails with ETXTBSY ("Text file busy") while that child has not exec'd yet.
    #[cfg(unix)]
    fn write_stub(path: &std::path::Path, script: &str) {
        use std::io::Write;
        let mut child = std::process::Command::new("sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "stub {}", path.display());
    }

    fn quick() -> Pace {
        Pace {
            git: "git".into(),
            lfs_storage: None,
            slot_poll: Duration::from_millis(5),
            slot_timeout: Duration::from_millis(200),
        }
    }

    #[test]
    fn start_time_reads_this_process_and_rejects_a_missing_one() {
        assert!(start_time(std::process::id()).is_some());
        assert!(start_time(u32::MAX).is_none());
    }

    #[test]
    fn slots_are_distinct_then_exhausted_then_a_dead_owner_is_reclaimed() {
        let base = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let first = claim_slot(base.path(), 2, me, &quick()).unwrap();
        let second = claim_slot(base.path(), 2, me, &quick()).unwrap();
        assert_ne!(first, second);
        // Both slots are held by a live process (this one): a third claim times out.
        assert!(claim_slot(base.path(), 2, me, &quick()).is_err());
        // Slot 1's owner dies (a start time that matches no process): the next claim takes it over.
        fs::write(base.path().join("slot-1.lock/owner"), format!("{me} 1")).unwrap();
        let third = claim_slot(base.path(), 2, me, &quick()).unwrap();
        assert_eq!(third, first);
    }

    #[test]
    fn an_empty_owner_file_is_a_claim_in_progress_not_a_dead_one() {
        let base = tempfile::tempdir().unwrap();
        fs::create_dir_all(base.path().join("slot-1.lock")).unwrap();
        assert!(claim_slot(base.path(), 1, std::process::id(), &quick()).is_err());
    }

    /// A fake `git` records the git configuration each call runs with; its `fetch` succeeds only when the routing rules are switched off
    /// (the system configuration is /dev/null), the way a fetch the runner's local copy cannot serve is answered by the forge.
    #[cfg(unix)]
    fn fake_git(dir: &Path, fetch_succeeds_with_rules: bool) -> (PathBuf, PathBuf) {
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let log = dir.join("log");
        let rule = if fetch_succeeds_with_rules {
            "exit 0"
        } else {
            "[ \"$GIT_CONFIG_SYSTEM\" = /dev/null ] && exit 0 || exit 1"
        };
        let script = format!(
            "#!/bin/sh\necho \"$GIT_CONFIG_SYSTEM|$GIT_CONFIG_COUNT|$*\" >> {}\n\
             case \"$*\" in *fetch*) {rule};; esac\nexit 0\n",
            log.display()
        );
        write_stub(&bin.join("git"), &script);
        (bin.join("git"), log)
    }

    #[cfg(unix)]
    fn fetch_calls(log: &Path) -> Vec<String> {
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("fetch"))
            .map(str::to_owned)
            .collect()
    }

    /// A copy that cannot serve the fetch costs one failed local fetch and one fetch from the forge with the rules switched off:
    /// no second try on the copy, no pause between the two.
    #[cfg(unix)]
    #[test]
    fn fetch_asks_the_copy_once_then_the_forge_once_with_the_rules_switched_off() {
        let dir = tempfile::tempdir().unwrap();
        let (git, log) = fake_git(dir.path(), false);
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let mut pace = quick();
        pace.git = git.into();
        assert!(fetch(&work, "abc", "token", &pace).is_ok());
        let fetches = fetch_calls(&log);
        assert_eq!(
            fetches.len(),
            2,
            "one failed fetch through the rules, one success on the forge: {fetches:?}"
        );
        assert!(!fetches[0].starts_with("/dev/null|"), "{fetches:?}");
        assert!(fetches[1].starts_with("/dev/null|0|"), "{fetches:?}");
    }

    /// A copy that has the commit is the only place asked.
    #[cfg(unix)]
    #[test]
    fn fetch_stops_at_the_copy_when_the_copy_has_the_commit() {
        let dir = tempfile::tempdir().unwrap();
        let (git, log) = fake_git(dir.path(), true);
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let mut pace = quick();
        pace.git = git.into();
        assert!(fetch(&work, "abc", "token", &pace).is_ok());
        assert_eq!(fetch_calls(&log).len(), 1);
    }

    /// A forge that cannot serve the commit either is a failure after exactly two fetches: the forge is authoritative, so nothing is repeated.
    #[cfg(unix)]
    #[test]
    fn fetch_fails_after_one_fetch_on_the_copy_and_one_on_the_forge() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let log = dir.path().join("log");
        let script = format!(
            "#!/bin/sh\necho \"$GIT_CONFIG_SYSTEM|$GIT_CONFIG_COUNT|$*\" >> {}\ncase \"$*\" in *fetch*) exit 1;; esac\nexit 0\n",
            log.display()
        );
        write_stub(&bin.join("git"), &script);
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let mut pace = quick();
        pace.git = bin.join("git").into();
        let error = fetch(&work, "abc", "token", &pace).unwrap_err();
        assert!(error.to_string().contains("cannot fetch abc"), "{error}");
        assert_eq!(fetch_calls(&log).len(), 2);
    }

    /// The real race, with real git: the runner's system configuration rewrites the forge's URL to a local copy (`insteadOf`, which has no
    /// fallback of its own), the copy is a push behind, and the job asks for the commit the push created. The first fetch fails inside the
    /// copy ("not our ref"); the second must reach the forge. `GIT_CONFIG_COUNT=0` alone would not leave the copy: the rewrite lives in
    /// the system configuration the runner names with GIT_CONFIG_SYSTEM.
    #[cfg(unix)]
    #[test]
    fn a_copy_that_lags_the_push_is_bypassed_by_one_fetch_from_the_forge() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let run = |dir: &Path, args: &[&str]| -> String {
            let output = Command::new("git")
                .current_dir(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
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
        };
        let forge = root.join("forge/o/r.git");
        let copy = root.join("copy/o/r.git");
        let source = root.join("source");
        for bare in [&forge, &copy] {
            fs::create_dir_all(bare).unwrap();
            run(bare, &["init", "--quiet", "--bare"]);
        }
        fs::create_dir_all(&source).unwrap();
        run(&source, &["init", "--quiet", "-b", "main"]);
        fs::write(source.join("f"), "one").unwrap();
        run(&source, &["add", "f"]);
        run(&source, &["commit", "--quiet", "-m", "one"]);
        let first = run(&source, &["rev-parse", "HEAD"]);
        run(
            &source,
            &["push", "--quiet", copy.to_str().unwrap(), "main"],
        );
        fs::write(source.join("f"), "two").unwrap();
        run(&source, &["commit", "--quiet", "-am", "two"]);
        let pushed = run(&source, &["rev-parse", "HEAD"]);
        run(
            &source,
            &["push", "--quiet", forge.to_str().unwrap(), "main"],
        );
        assert_ne!(first, pushed);
        // The runner: the forge's URL is rewritten to the copy, which has the first commit and not the pushed one.
        let system = root.join("runner-gitconfig");
        fs::write(
            &system,
            format!(
                "[url \"file://{copy}/\"]\n\tinsteadOf = file://{forge}/\n[uploadpack]\n\tallowReachableSHA1InWant = true\n",
                copy = root.join("copy").display(),
                forge = root.join("forge").display()
            ),
        )
        .unwrap();
        // The runner names its system configuration in the environment of every job, so the wrapper only sets it when the caller did not.
        let wrapper = root.join("bin/git");
        fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        write_stub(
            &wrapper,
            &format!(
                "#!/bin/sh\n: \"${{GIT_CONFIG_SYSTEM:={}}}\"\nexport GIT_CONFIG_SYSTEM\nexec git \"$@\"\n",
                system.display()
            ),
        );
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        run(&work, &["init", "--quiet"]);
        run(
            &work,
            &[
                "remote",
                "add",
                "origin",
                &format!("file://{}", forge.display()),
            ],
        );
        let mut pace = quick();
        pace.git = wrapper.into();
        // The control: through the runner's rule the pushed commit is not there, so the test does exercise the copy.
        let through_the_rule = Command::new(&pace.git)
            .current_dir(&work)
            .args(["fetch", "--quiet", "--depth", "1", "origin", &pushed])
            .output()
            .unwrap();
        assert!(
            !through_the_rule.status.success(),
            "the copy must lack the pushed commit"
        );
        assert!(fetch(&work, &pushed, "token", &pace).is_ok());
        assert_eq!(run(&work, &["rev-parse", "FETCH_HEAD"]), pushed);
    }

    /// A repository that tracks files with LFS gets the runner's shared object store configured before the pull, and none without one.
    #[cfg(unix)]
    #[test]
    fn lfs_pull_uses_the_shared_store_when_the_runner_names_one() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("git");
        let log = dir.path().join("log");
        let script = format!(
            "#!/bin/sh\necho \"$*\" >> {}\ncase \"$*\" in *show*) echo '*.bin filter=lfs';; esac\nexit 0\n",
            log.display()
        );
        write_stub(&bin, &script);
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let store = dir.path().join("lfs-store");
        let options = Options {
            job: "verify".into(),
            repository: "o/r".into(),
            sha: "a".repeat(40),
            server_url: "https://forge.example/".into(),
            owner: "o".into(),
            local: None,
            mirror: false,
        };
        let mut pace = quick();
        pace.git = bin.clone().into();
        pace.lfs_storage = Some(store.clone());
        checkout(&work, &options, "token", &pace).unwrap();
        let calls = fs::read_to_string(&log).unwrap();
        assert!(
            calls.contains(&format!("config lfs.storage {}", store.display())),
            "{calls}"
        );
        assert!(store.is_dir());
        // The configuration comes before the pull.
        assert!(calls.find("config lfs.storage").unwrap() < calls.find("lfs pull").unwrap());
        fs::remove_file(&log).unwrap();
        pace.lfs_storage = None;
        checkout(&work, &options, "token", &pace).unwrap();
        assert!(!fs::read_to_string(&log).unwrap().contains("lfs.storage"));
    }

    #[test]
    fn profile_variables_are_dropped_unless_the_job_declares_them() {
        let mut declared = BTreeMap::new();
        assert_eq!(stripped_profile_variables(&declared).len(), 4);
        declared.insert("CARGO_PROFILE_DEV_DEBUG".to_owned(), "1".to_owned());
        let stripped = stripped_profile_variables(&declared);
        assert_eq!(stripped.len(), 3);
        assert!(!stripped.contains(&"CARGO_PROFILE_DEV_DEBUG"));
    }
}

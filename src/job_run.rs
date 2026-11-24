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

use crate::{failure, jobs, Result};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
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
    /// The wait before round N+1 of a fetch is N times this (a commit a push created seconds ago may not be visible yet).
    pub fetch_wait: Duration,
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
            fetch_wait: Duration::from_secs(5),
        }
    }
}

/// Start time of a process: field 22 of `/proc/<pid>/stat` where there is a /proc, else what `ps -o lstart=` prints
/// (macOS has no /proc). It is the pid's identity across reuse.
fn start_time(pid: u32) -> Option<String> {
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        // The command name may hold spaces and parentheses: fields start after the last ')'.
        let rest = &stat[stat.rfind(')')? + 1..];
        // After ')' the first field is the state (field 3), so field 22 is index 19.
        return rest.split_whitespace().nth(19).map(str::to_owned);
    }
    if Path::new("/proc/self/stat").exists() {
        return None; // there is a /proc and this pid is not in it
    }
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.split_whitespace().collect::<Vec<_>>().join("_");
    (output.status.success() && !text.is_empty()).then_some(text)
}

/// When this boot began, in seconds since the epoch (`btime` of /proc/stat, else `sysctl kern.boottime`).
fn boot_seconds() -> Option<u64> {
    if let Ok(stat) = fs::read_to_string("/proc/stat") {
        if let Some(line) = stat.lines().find_map(|line| line.strip_prefix("btime ")) {
            return line.trim().parse().ok();
        }
    }
    let output = Command::new("sysctl")
        .args(["-n", "kern.boottime"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // `{ sec = 1763500000, usec = 123456 } Wed Nov 19 ...`
    let after = &text[text.find("sec =")? + 5..];
    after
        .trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// The identity of this boot: Linux's boot id, else the boot time (macOS).
fn boot_identity() -> Option<String> {
    if let Ok(id) = fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        let id = id.trim();
        if !id.is_empty() {
            return Some(id.to_owned());
        }
    }
    boot_seconds().map(|seconds| format!("boot-{seconds}"))
}

/// Whether a process with this pid exists (`kill(pid, 0)`: EPERM still means it exists).
#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
    use rustix::io::Errno;
    let Some(pid) = rustix::process::Pid::from_raw(pid as i32) else {
        return false;
    };
    !matches!(rustix::process::test_kill_process(pid), Err(Errno::SRCH))
}

#[cfg(not(unix))]
fn pid_exists(pid: u32) -> bool {
    start_time(pid).is_some()
}

/// What the liveness of a slot owner is judged against; tests replace it.
struct Host {
    boot: Option<String>,
    boot_seconds: Option<u64>,
    start_time: fn(u32) -> Option<String>,
    pid_exists: fn(u32) -> bool,
}

impl Host {
    fn real() -> Host {
        Host {
            boot: boot_identity(),
            boot_seconds: boot_seconds(),
            start_time,
            pid_exists,
        }
    }
}

/// `v2 <pid> <start time or -> <boot identity or ->`: the owner of a slot lock.
fn owner_line(pid: u32, host: &Host) -> String {
    let start = (host.start_time)(pid).unwrap_or_else(|| "-".into());
    let boot = host.boot.clone().unwrap_or_else(|| "-".into());
    format!("v2 {pid} {start} {boot}")
}

/// Whether the owner recorded in `held` is gone. An empty line is a claim in progress, not a dead one. A lock from another
/// boot is dead, so is a pid that does not exist, and a pid whose start time differs (the pid was reused). A line without a
/// boot identity (the old `<pid> <start time>` format) with no start time cannot name a process: it is dead when the pid
/// is absent or when the lock was made before this boot (`lock_mtime`, seconds since the epoch).
fn owner_gone(held: &str, lock_mtime: Option<u64>, host: &Host) -> bool {
    let mut parts = held.split_whitespace().peekable();
    let versioned = parts.peek() == Some(&"v2");
    if versioned {
        parts.next();
    }
    let Some(owner) = parts.next().and_then(|text| text.parse::<u32>().ok()) else {
        return false;
    };
    let started = parts.next().filter(|text| *text != "-").map(str::to_owned);
    let recorded_boot = if versioned {
        parts.next().filter(|text| *text != "-").map(str::to_owned)
    } else {
        None
    };
    if let (Some(recorded), Some(current)) = (&recorded_boot, &host.boot) {
        if recorded != current {
            return true;
        }
    }
    if !(host.pid_exists)(owner) {
        return true;
    }
    match (&started, (host.start_time)(owner)) {
        (Some(then), now) => now.as_ref() != Some(then),
        (None, _) => match (recorded_boot, lock_mtime, host.boot_seconds) {
            // An old line cannot tell the boot: a lock older than this boot is dead.
            (None, Some(made), Some(boot)) => made + 2 < boot,
            _ => false,
        },
    }
}

/// Claim one of `slots` source directories below `base`; returns the claimed `slot-N` directory.
pub fn claim_slot(base: &Path, slots: usize, pid: u32, pace: &Pace) -> Result<PathBuf> {
    fs::create_dir_all(base)?;
    let deadline = Instant::now() + pace.slot_timeout;
    let host = Host::real();
    loop {
        for n in 1..=slots {
            let lock = base.join(format!("slot-{n}.lock"));
            loop {
                match fs::create_dir(&lock) {
                    Ok(()) => {
                        fs::write(lock.join("owner"), owner_line(pid, &host))?;
                        let dir = base.join(format!("slot-{n}"));
                        fs::create_dir_all(&dir)?;
                        return Ok(dir);
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        let held = fs::read_to_string(lock.join("owner")).unwrap_or_default();
                        let made = fs::metadata(&lock)
                            .and_then(|meta| meta.modified())
                            .ok()
                            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|elapsed| elapsed.as_secs());
                        let gone = owner_gone(&held, made, &host);
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

/// A push run whose commit is no longer the tip of its branch is superseded: the newer push has its own run of this workflow, and that run checks everything the older one would
/// (one checkout of the whole tree). On the default branch a new push does not cancel the runs before it (cancel-in-progress is off there on purpose), so with many pushes every
/// one queued and waited its turn serially (careervector main, 2026-11-24: 31 waiting runs, the revert's checks 3 h behind them). Such a run now ends successfully at once, before the
/// checkout, saying so. Only `push` events of a branch (`refs/heads/...`) are looked at; an unreadable answer never skips a run; `CCID_SUPERSEDE=0` turns it off.
pub fn superseded_by(event: &str, git_ref: &str, sha: &str, tip: Option<&str>) -> Option<String> {
    if event != "push" || !git_ref.starts_with("refs/heads/") {
        return None;
    }
    let tip = tip
        .map(str::trim)
        .filter(|t| t.len() >= 40 && t.bytes().all(|b| b.is_ascii_hexdigit()))?;
    (tip != sha).then(|| tip.to_string())
}

/// The workflow file of this run (`GITHUB_WORKFLOW_REF` = `owner/repo/.forgejo/workflows/ci.yml@refs/heads/main`), the key a run list names a workflow by.
fn workflow_file(workflow_ref: &str) -> Option<String> {
    let path = workflow_ref.split('@').next()?;
    let name = path.rsplit('/').next()?;
    (name.ends_with(".yml") || name.ends_with(".yaml")).then(|| name.to_string())
}

/// Does the run list of the branch tip (`GET /repos/{r}/actions/runs?head_sha=<tip>`) hold a run of this workflow? A workflow with a `paths:` filter may have no run for the tip at
/// all (a docs-only push does not trigger it): then the older run is the only one that checks its commit and must not be skipped. Anything unreadable answers false.
fn runs_include_workflow(json: &str, file: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|doc| {
            doc.get("workflow_runs")?.as_array().map(|runs| {
                runs.iter().any(|run| {
                    run.get("workflow_id").and_then(|v| v.as_str()) == Some(file)
                        || run
                            .get("path")
                            .and_then(|v| v.as_str())
                            .is_some_and(|p| p.split('@').next() == Some(file))
                })
            })
        })
        .unwrap_or(false)
}

/// One authenticated GET against the forge API (the job token the runner already holds). `None` when the forge cannot be asked.
fn forge_api_get(options: &Options, path: &str, token: &str) -> Option<String> {
    let url = format!(
        "{}/api/v1/repos/{}/{path}",
        options.server_url.trim_end_matches('/'),
        options.repository
    );
    Command::new("curl")
        .args([
            "-q",
            "--silent",
            "--fail",
            "--max-time",
            "30",
            "-H",
            &format!("Authorization: token {token}"),
            &url,
        ])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn tip_has_workflow_run(options: &Options, tip: &str, file: &str, token: &str) -> bool {
    forge_api_get(
        options,
        &format!("actions/runs?head_sha={tip}&limit=50"),
        token,
    )
    .map(|json| runs_include_workflow(&json, file))
    .unwrap_or(false)
}

/// The commit id of a branch from the forge API's `GET branches/{name}` answer. Anything unreadable answers `None`.
fn branch_tip_from_json(json: &str) -> Option<String> {
    let doc = serde_json::from_str::<serde_json::Value>(json).ok()?;
    let id = doc.get("commit")?.get("id")?.as_str()?;
    (id.len() >= 40 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_string())
}

/// Is `sha` an ancestor of the tip? The forge's compare `tip...sha` lists the commits of `sha` the tip lacks: none means `sha` is contained in the tip. Unreadable or absent: false.
fn compare_says_contained(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|doc| doc.get("total_commits")?.as_u64())
        == Some(0)
}

/// The current tip of the branch, asked of the forge API itself. A git mirror or copy of the repository may lag the forge (nixnet runs 64 and 65, 2026-11-24: the copy
/// showed the previous push as the tip, so the newest commit was skipped as "superseded" and never verified), so git is never consulted for this decision.
fn remote_tip(options: &Options, git_ref: &str, token: &str) -> Option<String> {
    let branch = git_ref.strip_prefix("refs/heads/")?;
    let encoded: String = branch
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    forge_api_get(options, &format!("branches/{encoded}"), token)
        .and_then(|json| branch_tip_from_json(&json))
}

fn tip_contains(options: &Options, tip: &str, sha: &str, token: &str) -> bool {
    forge_api_get(options, &format!("compare/{tip}...{sha}"), token)
        .is_some_and(|json| compare_says_contained(&json))
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
    fetch_depth(dir, want, token, pace, "1")
}

/// Deepen the checkout until `before` is an ancestor of the commit, so `before..HEAD` resolves, asking for more history in
/// growing steps (a push is a handful of commits) and for everything when a step does not reach it.
pub fn fetch_push_range(
    dir: &Path,
    want: &str,
    before: &str,
    token: &str,
    pace: &Pace,
) -> Result<()> {
    let reached = |dir: &Path| {
        git(pace, dir)
            .args(["merge-base", "--is-ancestor", before, want])
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };
    for depth in ["50", "500", "5000"] {
        fetch_depth(dir, want, token, pace, depth)?;
        if reached(dir) {
            return Ok(());
        }
    }
    // The previous tip is not an ancestor (a force push) or far away: the whole history, as `history = "full"`.
    fetch_depth(dir, want, token, pace, "2147483647")
}

/// `fetch` with a chosen depth: `1` for the checkout, `2147483647` (git's "everything", it also converts a shallow copy
/// into a complete one) for a check that declares `history = true`. Same routing: the near copy once, then the forge once.
pub fn fetch_depth(dir: &Path, want: &str, token: &str, pace: &Pace, depth: &str) -> Result<()> {
    let header = format!("http.extraHeader=Authorization: token {token}");
    let attempt = |forge_itself: bool| {
        let mut command = git(pace, dir);
        command.args([
            "-c", &header, "fetch", "--quiet", "--depth", depth, "origin", want,
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
    // A commit a push created seconds ago may not be visible to the fetch yet ("upload-pack: not our ref": the nightly Renovate branches were red for exactly that on 2026-11-19 to
    // 11-23, 12 pull requests, each run started 4-6 s after its push, nothing ever reran them). Each round asks the near copy, then the forge itself; the wait before the next round
    // grows (5, 10, 15, 20, 25 s: a wait, not a limit), so a commit that appears within about 75 s is fetched and only a commit that is really gone fails the job.
    for round in 0..FETCH_ROUNDS {
        if round > 0 {
            std::thread::sleep(pace.fetch_wait * round as u32);
        }
        if attempt(false) {
            return Ok(());
        }
        if round == 0 {
            eprintln!("local copy lacks {want}, fetching from the forge");
        }
        if attempt(true) {
            return Ok(());
        }
    }
    Err(failure(format!("cannot fetch {want}")))
}

/// Rounds of `fetch_depth` (near copy, then the forge): see there.
const FETCH_ROUNDS: u64 = 6;

/// Check out the exact commit in `dir` (initialising it), with Git LFS content when the repository tracks files with LFS.
pub fn checkout(dir: &Path, options: &Options, token: &str, pace: &Pace) -> Result<()> {
    checkout_tree(dir, options, token, pace)?;
    checkout_submodules(dir, token, pace)
}

/// [`checkout`] without the submodules: the tree, LFS content and the clean. `run` reads the job's plan from it before any submodule is cloned.
pub fn checkout_tree(dir: &Path, options: &Options, token: &str, pace: &Pace) -> Result<()> {
    run_ok(git(pace, dir).args(["init", "--quiet", "."]), "git init")?;
    // A fresh directory has no origin yet: the removal is allowed to fail, and its "No such remote" message is noise.
    let _ = git(pace, dir)
        .args(["remote", "remove", "origin"])
        .stderr(Stdio::null())
        .status();
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
    )?;
    Ok(())
}

/// The commit's submodules, shallow, with the job token for the forge's own URLs; after the tree's checkout and clean (which would remove them). A job declares `submodules = false` when its
/// checks never read them (the plan is read from the checked-out tree first, so the declaration is known before any submodule is cloned).
pub fn checkout_submodules(dir: &Path, token: &str, pace: &Pace) -> Result<()> {
    if dir.join(".gitmodules").is_file() {
        let header = format!("http.extraHeader=Authorization: token {token}");
        run_ok(
            // The runner's near-copy rewrite (a file URL) is for the checkout itself; submodules come from the forge.
            git(pace, dir)
                .env("GIT_LFS_SKIP_SMUDGE", "1")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_COUNT", "0")
                .args([
                    "-c",
                    &header,
                    "submodule",
                    "update",
                    "--init",
                    "--recursive",
                    "--depth",
                    "1",
                    "--quiet",
                ]),
            "git submodule update",
        )?;
    }
    Ok(())
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
/// The previous tip of the push this run was started by: the `before` of the event payload `GITHUB_EVENT_PATH` names, when the
/// event's `after` is the commit being checked and `before` is a real commit (a new branch has the all-zero id). Never a key input.
fn push_before(sha: &str) -> Option<String> {
    let path = std::env::var_os("GITHUB_EVENT_PATH")?;
    let event: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    parse_push_before(&event, sha)
}

fn parse_push_before(event: &serde_json::Value, sha: &str) -> Option<String> {
    if event.get("after")?.as_str()? != sha {
        return None;
    }
    let before = event.get("before")?.as_str()?;
    let real = before.len() == 40
        && before.bytes().all(|b| b.is_ascii_hexdigit())
        && before.bytes().any(|b| b != b'0');
    real.then(|| before.to_owned())
}

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
    // The retained store (ccid retained): content-addressed, repository-independent, under the runner's persistent cache.
    if let Some(root) = std::env::var_os("CI_RETAINED_ROOT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("CI_CACHE_ROOT")
                .filter(|v| !v.is_empty())
                .map(|v| PathBuf::from(v).join("retained"))
        })
        .filter(|root| root.is_absolute())
    {
        env.insert("CI_RETAINED_ROOT".into(), root.display().to_string());
    }
    if plan.history {
        if let Some(before) = push_before(&options.sha) {
            env.insert("CCID_PUSH_BEFORE".into(), before);
        }
    }
    // A `--local` run of a tree without an origin has no repository URL; nothing is invented for it.
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
    let release = crate::release::command_arguments(&plan.command, &plan.job).is_some();
    if !jobs::is_self_checks_command(&plan.command) {
        env.insert("CCID_BIN".into(), exe.display().to_string());
        if !release {
            env.insert("CHECKS".into(), plan.checks.join(","));
        }
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

/// The read credential for staging siblings in a `--local` run: `CCID_FORGE_TOKEN` when set, else what `git credential fill`
/// answers for the forge host without prompting (the helper the workstation already uses for it).
#[cfg(unix)]
fn local_read_token(server_url: &str, pace: &Pace) -> Result<String> {
    use std::io::Write;
    if let Some(token) = std::env::var("CCID_FORGE_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        return Ok(token);
    }
    let (scheme, rest) = server_url
        .split_once("://")
        .unwrap_or(("https", server_url));
    let host = rest.split('/').next().unwrap_or(rest);
    let mut child = git(pace, Path::new("."))
        .args(["credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| failure(format!("git credential fill: {error}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = write!(stdin, "protocol={scheme}\nhost={host}\n\n");
    }
    let output = child
        .wait_with_output()
        .map_err(|error| failure(format!("git credential fill: {error}")))?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("password=").map(str::to_owned))
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            failure("--local cannot stage sibling repositories: no read credential (set CCID_FORGE_TOKEN, or let the git credential helper answer for the forge host)")
        })
}

/// Run the whole job: claim, fetch, check the adapters, exec the declared command. Does not return on success of the exec.
#[cfg(unix)]
/// The reason a Renovate pull request run must stop before any work, from the pull request event and the commit's statuses; `None` for every other run (not a pull request, not a `renovate/` branch,
/// no failing `renovate/artifacts` status, or the forge cannot be asked: a guard that cannot look lets the job run).
fn renovate_artifacts_failure(options: &Options, token: &str) -> Option<String> {
    let path = std::env::var_os("GITHUB_EVENT_PATH")?;
    let event: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    if !is_renovate_pull_request(&event) {
        return None;
    }
    let url = format!(
        "{}/api/v1/repos/{}/commits/{}/statuses?limit=50",
        options.server_url.trim_end_matches('/'),
        options.repository,
        options.sha
    );
    let output = Command::new("curl")
        .args([
            "-fsS",
            "-m",
            "20",
            "-H",
            &format!("Authorization: token {token}"),
            &url,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let statuses: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    artifacts_failure_reason(&statuses)
}

fn is_renovate_pull_request(event: &serde_json::Value) -> bool {
    event
        .pointer("/pull_request/head/ref")
        .and_then(|value| value.as_str())
        .is_some_and(|branch| branch.starts_with("renovate/"))
}

/// The newest `renovate/artifacts` status (by id) of a statuses listing, as a reason when it is a failure.
fn artifacts_failure_reason(statuses: &serde_json::Value) -> Option<String> {
    let newest = statuses
        .as_array()?
        .iter()
        .filter(|status| {
            status.get("context").and_then(|c| c.as_str()) == Some("renovate/artifacts")
        })
        .max_by_key(|status| status.get("id").and_then(|i| i.as_u64()).unwrap_or(0))?;
    (newest.get("status").and_then(|s| s.as_str()) == Some("failure")).then(|| {
        "renovate/artifacts failed on this commit: Renovate could not regenerate the lock file (its comment on the pull request names the error), so the manifest and the lock disagree and this run would fail after a full checkout and build; not running it".to_owned()
    })
}

/// The result of a job that ran its own command and then the folded `portable-paths` check: the command's own failure wins; in `enforce` mode a failing check fails a job that passed; `report`
/// never fails it.
fn folded_exit_code(command_code: i32, mode: &str, check_ok: bool) -> i32 {
    if command_code != 0 {
        command_code
    } else if mode == "enforce" && !check_ok {
        1
    } else {
        0
    }
}

pub fn run(options: &Options, pace: &Pace) -> Result<ExitCode> {
    // R10/R11: the CI of a contributed project never runs on our compute, from a forge event or from a source export.
    crate::contributed::refuse_repository("job-run", &options.server_url, &options.repository)?;
    if let Some(source) = &options.local {
        crate::contributed::refuse_working_copy("job-run", source)?;
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
        // A job that declares siblings stages them read-only from the forge, with the READ credential the workstation already has
        // (CCID_FORGE_TOKEN, else the git credential helper's answer for the forge host); never a write token, never a prompt.
        let staged = if plan.siblings.is_empty() {
            None
        } else {
            Some((local_read_token(&options.server_url, pace)?, source.clone()))
        };
        return execute_job(
            options,
            &exe,
            plan,
            staged
                .as_ref()
                .map(|(token, dir)| (token.as_str(), dir.as_path())),
            pace,
        );
    }
    // A Renovate pull request whose lock-file update failed carries a failing `renovate/artifacts` status on its head commit; its manifest and its lock disagree, so every check would fail
    // after minutes of checkout and compile (2026-11-23: about 30 such pull requests in a wave of 150 runs that pushed srv into swap). The run fails at once, with that reason, before a slot is taken.
    if let Some(token) = token.as_deref() {
        if let Some(reason) = renovate_artifacts_failure(options, token) {
            return Err(failure(reason));
        }
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
    if std::env::var("CCID_SUPERSEDE")
        .map(|v| v != "0")
        .unwrap_or(true)
    {
        let event = std::env::var("GITHUB_EVENT_NAME").unwrap_or_default();
        let git_ref = std::env::var("GITHUB_REF").unwrap_or_default();
        if event == "push" && git_ref.starts_with("refs/heads/") {
            let tip = remote_tip(options, &git_ref, &token);
            let file = std::env::var("GITHUB_WORKFLOW_REF")
                .ok()
                .and_then(|r| workflow_file(&r));
            let newer =
                superseded_by(&event, &git_ref, &options.sha, tip.as_deref()).filter(|newer| {
                    tip_contains(options, newer, &options.sha, &token)
                        && file
                            .as_deref()
                            .is_some_and(|f| tip_has_workflow_run(options, newer, f, &token))
                });
            if let Some(newer) = newer {
                eprintln!("ccid job-run: superseded: {git_ref} is at {newer} now, whose own run of this workflow checks everything this one would; nothing to do for {}", options.sha);
                return Ok(ExitCode::SUCCESS);
            }
        }
    }
    checkout_tree(&dir, options, &token, pace)?;
    let manifest = Path::new(".ci/ccid.toml");
    let plan = match jobs::plan(Path::new("."), manifest, &options.job, None) {
        Ok(plan) => plan,
        Err(error) => {
            // A workflow rendered before the fold still has the job `portable-paths`; the manifest no longer declares it (the check runs inside verify): that run succeeds at once instead of
            // failing every pull request until the repository is re-rendered.
            if options.job == crate::portable_paths::CHECK
                && jobs::plan(Path::new("."), manifest, "verify", None)
                    .map(|verify| verify.fold_portable_paths.is_some())
                    .unwrap_or(false)
            {
                eprintln!("ccid job-run: portable-paths runs inside the verify job of this repository now; this workflow is stale (a re-render removes it): nothing to do");
                return Ok(ExitCode::SUCCESS);
            }
            return Err(error);
        }
    };
    if plan.submodules {
        checkout_submodules(&dir, &token, pace)?;
    }
    if plan.history {
        // A check that reads git history (push range, signatures) gets history, not the depth-1 checkout: only the commits
        // of the push when every such check says `"push"` and the event names the previous tip, else the whole history.
        match push_before(&options.sha).filter(|_| plan.history_push_only) {
            Some(before) => fetch_push_range(&dir, &options.sha, &before, &token, pace)?,
            None => fetch_depth(&dir, &options.sha, &token, pace, "2147483647")?,
        }
    }
    execute_job(options, &exe, plan, Some((&token, &dir)), pace)
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
    // (or by the channel's previous release: a channel bump must not turn every repository red, see render_previous.rs)
    crate::render_previous::check(Path::new("."), manifest, &std::env::vars_os().collect())?;
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
    } else if let Some(words) = crate::release::command_arguments(&plan.command, &plan.job) {
        // A job of the release flow: `ccid release build|leg <os>|promote`, which runs the declared checks itself.
        argv.push(exe.into());
        argv.extend(words.into_iter().map(std::ffi::OsString::from));
        argv.extend(["--repo", "."].into_iter().map(std::ffi::OsString::from));
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
    // `CCID_SANDBOX=<program>`: the job command (not ccid's own git and checkout steps) runs as `<program> <command...>`, e.g. the
    // csandbox wrapper that hides this account's credentials from build and test code. Unset: byte-identical to before.
    if let Some(wrapper) = std::env::var_os("CCID_SANDBOX").filter(|value| !value.is_empty()) {
        let mut wrapped = Command::new(wrapper);
        wrapped.arg(command.get_program()).args(command.get_args());
        command = wrapped;
    }
    command
        .envs(&extra)
        .env("CCID_SOURCE_REVISION", &options.sha);
    // The profile (debug level of dev, test and release builds) is ccid's shared table now (profiles/dev.toml, applied as cargo --config, which sccache keys the same for
    // lanes and CI). Workflows rendered earlier still export CARGO_PROFILE_*_DEBUG: those are dropped here, before ccid decides, unless the job declares them itself.
    for key in stripped_profile_variables(&plan.environment) {
        command.env_remove(key);
    }
    if let Some(prepared) = &runtime {
        // One ordered log, as the job scripts' `exec 2>&1` gave.
        use std::os::fd::AsFd;
        command.stderr(std::process::Stdio::from(
            io::stdout().as_fd().try_clone_to_owned()?,
        ));
        prepared.keep_across_exec()?;
    }
    if let Some(mode) = plan.fold_portable_paths.as_deref() {
        // The standard `portable-paths` check runs INSIDE this job, after its own command (one checkout, one slot): the command is not exec'd but run to its end, then the check, in this binary.
        // The job's result is the command's; in `enforce` mode a failing check fails a job that passed. Report mode never fails the job.
        let status = command.status();
        let command_code = match &status {
            Ok(status) => status.code().unwrap_or(1),
            Err(error) => {
                eprintln!("ccid job-run: cannot start the job command: {error}");
                drop(runtime);
                return Err(failure(format!("cannot start the job command: {error}")));
            }
        };
        let check_ok = Command::new(exe)
            .args(["portable-paths", "--mode", mode])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !check_ok && mode == "report" {
            eprintln!("portable-paths: not run or failed (report mode never fails the job)");
        }
        drop(runtime);
        let code = folded_exit_code(command_code, mode, check_ok);
        return Ok(ExitCode::from(
            u8::try_from(code.clamp(0, 255)).unwrap_or(1),
        ));
    }
    let error = command.exec();
    // Still here: the locks go with this process.
    drop(runtime);
    Err(failure(format!("cannot start the job command: {error}")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_run_is_only_superseded_when_the_tip_has_a_run_of_the_same_workflow() {
        assert_eq!(
            workflow_file("corbet/repo/.forgejo/workflows/ci.yml@refs/heads/main").as_deref(),
            Some("ci.yml")
        );
        assert_eq!(workflow_file("nonsense"), None);
        let runs = r#"{"workflow_runs":[{"id":1,"workflow_id":"unit-tests.yml","status":"waiting"},{"id":2,"workflow_id":"ci.yml","status":"success"}]}"#;
        assert!(runs_include_workflow(runs, "ci.yml"));
        assert!(
            !runs_include_workflow(runs, "workers-bundle.yml"),
            "a paths-filtered workflow the tip did not trigger: the older run must run"
        );
        assert!(runs_include_workflow(
            r#"{"workflow_runs":[{"path":"ci.yml@refs/heads/main"}]}"#,
            "ci.yml"
        ));
        assert!(!runs_include_workflow("not json", "ci.yml"));
        assert!(!runs_include_workflow(r#"{"workflow_runs":[]}"#, "ci.yml"));
    }

    #[test]
    fn the_branch_tip_is_read_from_the_forge_answer_only() {
        let id = "c".repeat(40);
        assert_eq!(
            branch_tip_from_json(&format!(r#"{{"name":"main","commit":{{"id":"{id}"}}}}"#)),
            Some(id)
        );
        assert_eq!(branch_tip_from_json("{}"), None);
        assert_eq!(branch_tip_from_json(r#"{"commit":{"id":"short"}}"#), None);
        assert_eq!(branch_tip_from_json("not json"), None);
    }

    #[test]
    fn only_a_tip_that_contains_the_commit_may_supersede_it() {
        assert!(compare_says_contained(
            r#"{"total_commits":0,"commits":[]}"#
        ));
        assert!(
            !compare_says_contained(r#"{"total_commits":2,"commits":[]}"#),
            "the commit has work the tip lacks (an ancestor tip, or a forced branch)"
        );
        assert!(!compare_says_contained("{}"));
        assert!(!compare_says_contained("garbage"));
    }

    /// The lag that skipped nixnet runs 64 and 65: the older commit shown as the tip while the forge is at the run's own commit.
    #[test]
    fn a_lagging_tip_cannot_supersede_the_newest_commit() {
        let newest = "d".repeat(40);
        // The forge's tip is the run's own commit: nothing supersedes it.
        assert_eq!(
            superseded_by("push", "refs/heads/main", &newest, Some(&newest)),
            None
        );
        // A stale (ancestor) tip lacks the run's work: the compare says not contained, so the run is real.
        assert!(!compare_says_contained(
            r#"{"total_commits":1,"commits":[{"sha":"x"}]}"#
        ));
    }

    #[test]
    fn a_push_whose_commit_is_not_the_branch_tip_is_superseded() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        assert_eq!(
            superseded_by("push", "refs/heads/main", &a, Some(&b)),
            Some(b.clone())
        );
        assert_eq!(
            superseded_by("push", "refs/heads/main", &a, Some(&a)),
            None,
            "the tip itself runs"
        );
        assert_eq!(
            superseded_by("push", "refs/heads/main", &a, Some(&format!("{b}\n"))),
            Some(b.clone()),
            "a trailing newline is no part of the id"
        );
        assert_eq!(
            superseded_by("push", "refs/heads/main", &a, None),
            None,
            "an unreadable answer never skips a run"
        );
        assert_eq!(superseded_by("push", "refs/heads/main", &a, Some("")), None);
        assert_eq!(
            superseded_by("push", "refs/heads/main", &a, Some("not a sha")),
            None
        );
        assert_eq!(
            superseded_by("pull_request", "refs/pull/3/merge", &a, Some(&b)),
            None,
            "pull requests have their own cancellation"
        );
        assert_eq!(
            superseded_by("workflow_dispatch", "refs/heads/main", &a, Some(&b)),
            None,
            "a dispatched run is asked for"
        );
        assert_eq!(
            superseded_by("push", "refs/tags/v1", &a, Some(&b)),
            None,
            "a tag is not a branch"
        );
    }

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
            fetch_wait: Duration::from_millis(1),
        }
    }

    #[test]
    fn push_before_needs_a_matching_after_and_a_real_previous_tip() {
        let sha = "a".repeat(40);
        let good = "b".repeat(40);
        let ev = |before: &str, after: &str| serde_json::json!({"before": before, "after": after});
        assert_eq!(
            parse_push_before(&ev(&good, &sha), &sha),
            Some(good.clone())
        );
        assert_eq!(parse_push_before(&ev(&"0".repeat(40), &sha), &sha), None);
        assert_eq!(parse_push_before(&ev(&good, &good), &sha), None);
        assert_eq!(parse_push_before(&ev("short", &sha), &sha), None);
        assert_eq!(parse_push_before(&serde_json::json!({}), &sha), None);
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

    /// A commit neither the copy nor the forge ever serves is a failure after FETCH_ROUNDS rounds of two fetches each.
    #[cfg(unix)]
    #[test]
    fn fetch_fails_after_the_rounds_on_the_copy_and_on_the_forge() {
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
        assert_eq!(
            fetch_calls(&log).len(),
            2 * FETCH_ROUNDS as usize,
            "every round asks the copy, then the forge"
        );
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
        // The runner names its system configuration in the environment of every job, and so does the CI runner this test itself runs on: whatever
        // the test process inherited, a git call gets the runner's rules, except the call that switched them off (GIT_CONFIG_SYSTEM=/dev/null).
        let wrapper = root.join("bin/git");
        fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        write_stub(
            &wrapper,
            &format!(
                "#!/bin/sh\ncase \"${{GIT_CONFIG_SYSTEM:-}}\" in\n  /dev/null) ;;\n  *) GIT_CONFIG_SYSTEM={}; unset GIT_CONFIG_COUNT ;;\nesac\nexport GIT_CONFIG_SYSTEM\nexec git \"$@\"\n",
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

    fn host(boot: Option<&str>, alive: bool, start: Option<&'static str>) -> Host {
        // fn pointers: the fake process table is two constants picked by the test.
        fn alive_yes(_: u32) -> bool {
            true
        }
        fn alive_no(_: u32) -> bool {
            false
        }
        fn start_a(_: u32) -> Option<String> {
            Some("A".into())
        }
        fn start_b(_: u32) -> Option<String> {
            Some("B".into())
        }
        fn start_none(_: u32) -> Option<String> {
            None
        }
        Host {
            boot: boot.map(str::to_owned),
            boot_seconds: Some(1_000),
            start_time: match start {
                Some("A") => start_a,
                Some(_) => start_b,
                None => start_none,
            },
            pid_exists: if alive { alive_yes } else { alive_no },
        }
    }

    #[test]
    fn an_owner_of_another_boot_or_a_missing_pid_is_gone() {
        // the same boot and a live pid with the recorded start time: held
        assert!(!owner_gone(
            "v2 7 A boot-1",
            Some(2_000),
            &host(Some("boot-1"), true, Some("A"))
        ));
        // another boot: gone, whatever the pid says
        assert!(owner_gone(
            "v2 7 A boot-1",
            Some(2_000),
            &host(Some("boot-2"), true, Some("A"))
        ));
        // no such pid (macOS has no /proc: kill(pid, 0) is the test)
        assert!(owner_gone(
            "v2 7 - boot-1",
            Some(2_000),
            &host(Some("boot-1"), false, None)
        ));
        // the pid was reused: another start time
        assert!(owner_gone(
            "v2 7 A boot-1",
            Some(2_000),
            &host(Some("boot-1"), true, Some("B"))
        ));
        // a start time that cannot be read now (no ps entry) while one was recorded: gone
        assert!(owner_gone(
            "v2 7 A boot-1",
            Some(2_000),
            &host(Some("boot-1"), true, None)
        ));
        // no start time recorded (macOS without ps data), same boot, live pid: held
        assert!(!owner_gone(
            "v2 7 - boot-1",
            Some(2_000),
            &host(Some("boot-1"), true, None)
        ));
        // a claim in progress is never dead
        assert!(!owner_gone("", Some(1), &host(Some("boot-1"), false, None)));
    }

    #[test]
    fn an_old_format_line_is_dead_by_pid_or_by_a_lock_older_than_the_boot() {
        // `<pid> ` written on macOS: the start time is empty
        assert!(owner_gone(
            "7 ",
            Some(2_000),
            &host(Some("boot-1"), false, None)
        ));
        assert!(!owner_gone(
            "7 ",
            Some(2_000),
            &host(Some("boot-1"), true, None)
        ));
        // the pid exists (reused after a reboot) but the lock was made before this boot (1000 s)
        assert!(owner_gone(
            "7 ",
            Some(500),
            &host(Some("boot-1"), true, None)
        ));
        // old Linux line with a start time: the start time decides, as before
        assert!(!owner_gone(
            "7 A",
            Some(2_000),
            &host(None, true, Some("A"))
        ));
        assert!(owner_gone("7 A", Some(2_000), &host(None, true, Some("B"))));
    }

    #[test]
    fn the_owner_line_names_the_pid_the_start_time_and_the_boot() {
        assert_eq!(
            owner_line(7, &host(Some("boot-1"), true, Some("A"))),
            "v2 7 A boot-1"
        );
        assert_eq!(owner_line(7, &host(None, true, None)), "v2 7 - -");
    }

    #[cfg(unix)]
    #[test]
    fn a_push_range_is_reached_by_deepening_a_shallow_checkout() {
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
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        run(&source, &["init", "--quiet", "-b", "main"]);
        let mut shas = Vec::new();
        for n in 0..4 {
            fs::write(source.join("f"), n.to_string()).unwrap();
            run(&source, &["add", "f"]);
            run(&source, &["commit", "--quiet", "-m", &format!("c{n}")]);
            shas.push(run(&source, &["rev-parse", "HEAD"]));
        }
        run(
            &source,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        let checkout = root.join("checkout");
        fs::create_dir_all(&checkout).unwrap();
        run(&checkout, &["init", "--quiet"]);
        run(
            &checkout,
            &[
                "remote",
                "add",
                "origin",
                &format!("file://{}", source.display()),
            ],
        );
        fetch_depth(&checkout, &shas[3], "t", &quick(), "1").unwrap();
        run(&checkout, &["checkout", "--quiet", "--detach", &shas[3]]);
        // the shallow checkout has no parent: the range does not resolve
        assert!(Command::new("git")
            .current_dir(&checkout)
            .args(["rev-list", &format!("{}..HEAD", shas[1])])
            .output()
            .map(|o| !o.status.success() || o.stdout.is_empty())
            .unwrap());
        fetch_push_range(&checkout, &shas[3], &shas[1], "t", &quick()).unwrap();
        let range = run(&checkout, &["rev-list", &format!("{}..HEAD", shas[1])]);
        assert_eq!(range.lines().count(), 2, "{range}");
    }

    #[cfg(unix)]
    #[test]
    fn a_commit_that_appears_seconds_after_the_first_ask_is_still_fetched() {
        // "upload-pack: not our ref" right after a push: the commit reaches the origin a moment after the job started (2026-11-19 to 11-23, the nightly Renovate pull requests).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let git = |cwd: &Path, args: &[&str]| -> String {
            let output = Command::new("git")
                .current_dir(cwd)
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
        let source = root.join("source");
        let staging = root.join("staging");
        for repo in [&source, &staging] {
            fs::create_dir_all(repo).unwrap();
            git(repo, &["init", "--quiet", "-b", "main"]);
        }
        git(
            &source,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        fs::write(staging.join("f"), "late").unwrap();
        git(&staging, &["add", "f"]);
        git(&staging, &["commit", "--quiet", "-m", "late"]);
        let late = git(&staging, &["rev-parse", "HEAD"]);
        let checkout = root.join("checkout");
        fs::create_dir_all(&checkout).unwrap();
        git(&checkout, &["init", "--quiet"]);
        git(
            &checkout,
            &[
                "remote",
                "add",
                "origin",
                &format!("file://{}", source.display()),
            ],
        );
        let (source_for_push, staging_for_push, sha) =
            (source.clone(), staging.clone(), late.clone());
        let pusher = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let ok = Command::new("git")
                .current_dir(&source_for_push)
                .args(["fetch", "--quiet", staging_for_push.to_str().unwrap(), &sha])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap()
                .success();
            assert!(ok);
        });
        let mut pace = quick();
        pace.fetch_wait = std::time::Duration::from_secs(1);
        fetch_depth(&checkout, &late, "t", &pace, "1").unwrap();
        pusher.join().unwrap();
        git(&checkout, &["checkout", "--quiet", "--detach", &late]);
    }

    #[test]
    fn a_renovate_pull_request_with_a_failed_lock_update_stops_before_any_work() {
        let pr = |branch: &str| serde_json::json!({"pull_request": {"head": {"ref": branch}}});
        assert!(is_renovate_pull_request(&pr("renovate/all")));
        assert!(!is_renovate_pull_request(&pr("feature/x")));
        assert!(!is_renovate_pull_request(
            &serde_json::json!({"after": "a"})
        ));
        let failed = serde_json::json!([
            {"id": 3, "context": "ci / verify (pull_request)", "status": "pending"},
            {"id": 2, "context": "renovate/artifacts", "status": "failure"}
        ]);
        assert!(artifacts_failure_reason(&failed)
            .unwrap()
            .contains("renovate/artifacts failed"));
        // a later success of the same status wins over the earlier failure
        let healed = serde_json::json!([
            {"id": 5, "context": "renovate/artifacts", "status": "success"},
            {"id": 2, "context": "renovate/artifacts", "status": "failure"}
        ]);
        assert!(artifacts_failure_reason(&healed).is_none());
        assert!(artifacts_failure_reason(&serde_json::json!([])).is_none());
        assert!(artifacts_failure_reason(&serde_json::json!({"message": "not found"})).is_none());
    }

    #[test]
    fn a_folded_portable_paths_check_never_hides_the_command_and_fails_only_in_enforce_mode() {
        assert_eq!(folded_exit_code(0, "report", true), 0);
        assert_eq!(
            folded_exit_code(0, "report", false),
            0,
            "report mode never fails the job"
        );
        assert_eq!(folded_exit_code(0, "enforce", false), 1);
        assert_eq!(folded_exit_code(0, "enforce", true), 0);
        assert_eq!(
            folded_exit_code(2, "report", true),
            2,
            "the job's own failure is the result"
        );
        assert_eq!(folded_exit_code(2, "enforce", false), 2);
    }

    #[test]
    fn a_job_declares_whether_its_checkout_fetches_the_submodules() {
        let base = "schema = 1\nproject = \"demo\"\n\n[checks.test]\nkind = \"commands\"\ncommands = [[\"true\"]]\n\n[jobs.verify]\nscheduler = \"forgejo\"\nchecks = [\"test\"]\nworkflow = \"ci\"\ncommand = [\"ccid:run-declared-checks\"]\n";
        let default = crate::parse_manifest(base.as_bytes()).unwrap();
        let plan = jobs::plan_parsed(&default, "verify", None).unwrap();
        assert!(plan.submodules, "the default fetches them");
        assert!(
            !serde_json::to_string(&plan).unwrap().contains("submodules"),
            "the default leaves the inventory byte-identical"
        );
        let off = crate::parse_manifest(format!("{base}submodules = false\n").as_bytes()).unwrap();
        let plan = jobs::plan_parsed(&off, "verify", None).unwrap();
        assert!(!plan.submodules);
        assert!(serde_json::to_string(&plan)
            .unwrap()
            .contains("\"submodules\":false"));
    }
}

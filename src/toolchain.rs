//! Shared-first Rust toolchain lock and the wasm-bindgen test runner.
//!
//! The protocol is a port of the shell lock the cached runners source today and
//! uses the same advisory `flock(2)` locks (std's `File::lock*` on Linux), so
//! shell jobs and ccid jobs interoperate on the same lock files.
//!
//! Hard rules:
//! - SHARED first. Readiness is probed under a shared lock; the exclusive lock
//!   is taken only when something is actually missing, and readiness is checked
//!   again after the upgrade (a peer may have provisioned meanwhile).
//! - NEVER request exclusive while holding shared (flock upgrade deadlock):
//!   shared is always released before the exclusive request. NEVER take
//!   exclusive unconditionally and downgrade afterwards.
//! - An admission gate (`<lock file>.gate`) prevents writer starvation. It is
//!   taken before main-shared and held ONLY until readiness is decided (warm:
//!   released, main shared kept) or provisioning finishes. A waiting writer
//!   holds it, so later readers queue at the gate instead of piling fresh
//!   shared holds onto main. The gate is never held during product checks.
//!   Global lock order is always gate then main.
//! - On ANY failure the locks are released (the gate exactly once) and the
//!   existing toolchains are left intact: there is deliberately no cleanup of
//!   toolchain state.
//! - A missing tool or a failing probe is never a pass. A recheck that still
//!   fails after a successful install is an error.
//! - Lock lifetime: [`Held`] owns the main lock file. Its descriptor is
//!   close-on-exec like every file std opens, so child processes spawned
//!   during probes and installs never inherit the lock. [`Held::keep_across_exec`]
//!   clears that flag with the safe `fcntl(F_SETFD)` wrapper (no raw descriptor
//!   adoption, no `unsafe`); after `exec` the new program holds the same open
//!   file description, hence the shared lock, until it exits. A crash or kill
//!   closes the descriptor and the kernel drops the lock.

use crate::{failure, sha256_file, Environment, Result};
use clap::Subcommand;
use rustix::io::{fcntl_setfd, FdFlags};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions, TryLockError},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

const MAX_ROUNDS: u32 = 3;
const POLL: Duration = Duration::from_millis(100);
const DEFAULT_TIMEOUT_SECONDS: u64 = 2700;

/// What a job needs besides stable with rustfmt and clippy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Want {
    pub nightly: bool,
    /// `(toolchain, target)` pairs.
    pub targets: Vec<(String, String)>,
}

/// Parse `TOOLCHAIN:TARGET`, split at the first colon.
pub fn parse_target(text: &str) -> Result<(String, String)> {
    match text.split_once(':') {
        Some((toolchain, target)) if !toolchain.is_empty() && !target.is_empty() => {
            Ok((toolchain.to_owned(), target.to_owned()))
        }
        _ => Err(failure(format!(
            "toolchain-lock: target must be TOOLCHAIN:TARGET, got {text:?}"
        ))),
    }
}

/// Accept exactly `llvm-tools-<target>` and the legacy
/// `llvm-tools-preview[-<target>]` shapes on any line of a component list.
/// A loose `llvm-tools` prefix would bless arbitrary names and bare
/// `llvm-tools` is refused: installed components carry a target suffix.
pub fn has_llvm_tools(components: &str) -> bool {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(
            r"^llvm-tools(-preview(-[a-z][a-z0-9_]*(-[a-z0-9_]+){2,})?|-[a-z][a-z0-9_]*(-[a-z0-9_]+){2,})( |$)",
        )
        .expect("static pattern")
    });
    components.lines().any(|line| pattern.is_match(line))
}

fn program(name: &str, environment: &Environment) -> Option<PathBuf> {
    if name.contains('/') {
        return Some(PathBuf::from(name));
    }
    let path = environment.get(OsStr::new("PATH"))?;
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn command(name: &str, args: &[&str], environment: &Environment) -> Result<Command> {
    let path = program(name, environment).ok_or_else(|| {
        failure(format!(
            "toolchain-lock: {name} not found on PATH of the job environment"
        ))
    })?;
    let mut command = Command::new(path);
    command.args(args).env_clear().envs(environment);
    Ok(command)
}

/// Read-only probe: true only for a started command that exited 0.
fn succeeds(name: &str, args: &[&str], environment: &Environment) -> bool {
    command(name, args, environment)
        .and_then(|mut command| {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(Into::into)
        })
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Read-only probe returning stdout of a command that exited 0.
fn listing(name: &str, args: &[&str], environment: &Environment) -> Option<String> {
    let output = command(name, args, environment)
        .ok()?
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn stable_ready(environment: &Environment) -> bool {
    if !succeeds(
        "rustup",
        &["run", "stable", "rustc", "--version"],
        environment,
    ) {
        return false;
    }
    let Some(components) = listing(
        "rustup",
        &["component", "list", "--toolchain", "stable", "--installed"],
        environment,
    ) else {
        return false;
    };
    components.lines().any(|line| line.starts_with("rustfmt"))
        && components.lines().any(|line| line.starts_with("clippy"))
}

fn nightly_ready(environment: &Environment) -> bool {
    succeeds(
        "rustup",
        &["run", "nightly", "rustc", "--version"],
        environment,
    ) && listing(
        "rustup",
        &["component", "list", "--toolchain", "nightly", "--installed"],
        environment,
    )
    .is_some_and(|components| has_llvm_tools(&components))
}

fn target_ready(toolchain: &str, target: &str, environment: &Environment) -> bool {
    listing(
        "rustup",
        &["target", "list", "--toolchain", toolchain, "--installed"],
        environment,
    )
    .is_some_and(|targets| targets.lines().any(|line| line.starts_with(target)))
}

/// Read-only readiness probes (`toolchain_ready`). A missing tool is not ready.
pub fn ready(want: &Want, environment: &Environment) -> Result<bool> {
    if !stable_ready(environment) {
        return Ok(false);
    }
    if want.nightly && !nightly_ready(environment) {
        return Ok(false);
    }
    Ok(want
        .targets
        .iter()
        .all(|(toolchain, target)| target_ready(toolchain, target, environment)))
}

fn absolute(environment: &Environment, name: &str) -> Result<PathBuf> {
    match environment.get(OsStr::new(name)).map(PathBuf::from) {
        Some(path) if path.is_absolute() => Ok(path),
        _ => Err(failure(format!("toolchain-lock: {name} must be absolute"))),
    }
}

/// The euid's home from the passwd database (6th field). rustup refuses
/// installs when `HOME` differs from it; a failed lookup fails closed.
fn euid_home(environment: &Environment) -> Result<OsString> {
    let uid = rustix::process::geteuid().as_raw().to_string();
    let entry = listing("getent", &["passwd", &uid], environment)
        .ok_or_else(|| failure("toolchain-lock: cannot resolve euid home (getent failed)"))?;
    match entry.trim_end().split(':').nth(5) {
        Some(home) if !home.is_empty() => Ok(home.into()),
        _ => Err(failure(
            "toolchain-lock: cannot resolve euid home (empty entry)",
        )),
    }
}

/// One mutating install/update with the euid home for that command only.
fn mutate(name: &str, args: &[&str], environment: &Environment) -> Result<()> {
    absolute(environment, "CARGO_HOME")?;
    absolute(environment, "RUSTUP_HOME")?;
    let home = euid_home(environment)?;
    let mut command = command(name, args, environment)?;
    if environment.get(OsStr::new("HOME")) != Some(&home) {
        command.env("HOME", &home);
    }
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(failure(format!(
            "toolchain-lock: {name} {} failed ({status})",
            args.join(" ")
        )))
    }
}

/// Provision what is missing (`toolchain_install`); call under the exclusive
/// lock only. Never reinstalls a working stable: guarded component adds only.
pub fn install(want: &Want, environment: &Environment) -> Result<()> {
    fs::create_dir_all(absolute(environment, "CARGO_HOME")?)?;
    if !succeeds(
        "rustup",
        &["run", "stable", "rustc", "--version"],
        environment,
    ) {
        mutate("proto", &["install", "rust", "stable"], environment)?;
    }
    let components = listing(
        "rustup",
        &["component", "list", "--toolchain", "stable", "--installed"],
        environment,
    )
    .ok_or_else(|| failure("toolchain-lock: cannot list stable components"))?;
    for component in ["rustfmt", "clippy"] {
        if !components.lines().any(|line| line.starts_with(component)) {
            mutate(
                "rustup",
                &["component", "add", component, "--toolchain", "stable"],
                environment,
            )?;
        }
    }
    if want.nightly {
        if !succeeds(
            "rustup",
            &["run", "nightly", "rustc", "--version"],
            environment,
        ) {
            mutate(
                "rustup",
                &[
                    "toolchain",
                    "install",
                    "nightly",
                    "--component",
                    "llvm-tools-preview",
                ],
                environment,
            )?;
        } else {
            let components = listing(
                "rustup",
                &["component", "list", "--toolchain", "nightly", "--installed"],
                environment,
            )
            .ok_or_else(|| failure("toolchain-lock: cannot list nightly components"))?;
            if !has_llvm_tools(&components) {
                mutate(
                    "rustup",
                    &[
                        "component",
                        "add",
                        "llvm-tools-preview",
                        "--toolchain",
                        "nightly",
                    ],
                    environment,
                )?;
            }
        }
    }
    for (toolchain, target) in &want.targets {
        let targets = listing(
            "rustup",
            &["target", "list", "--toolchain", toolchain, "--installed"],
            environment,
        )
        .ok_or_else(|| failure(format!("toolchain-lock: cannot list {toolchain} targets")))?;
        if !targets
            .lines()
            .any(|line| line.starts_with(target.as_str()))
        {
            mutate(
                "rustup",
                &["target", "add", target, "--toolchain", toolchain],
                environment,
            )?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Mode {
    Shared,
    Exclusive,
}

/// `flock -w`: poll the non-blocking form every 100 ms until the deadline.
fn acquire(file: &File, mode: Mode, timeout: Duration, what: &str) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let attempt = match mode {
            Mode::Shared => file.try_lock_shared(),
            Mode::Exclusive => file.try_lock(),
        };
        match attempt {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(failure(format!(
                "toolchain-lock: timed out waiting for the {what} lock"
            )));
        }
        thread::sleep(POLL.min(deadline - now));
    }
}

fn open_lock(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?)
}

/// The main toolchain lock, held shared. Dropping it releases the lock.
#[derive(Debug)]
pub struct Held {
    file: File,
}

impl Held {
    /// Let the lock outlive `exec`: clear close-on-exec on the locked
    /// descriptor. The exec'd program then holds the same open file
    /// description, so the shared lock lasts until it exits.
    pub fn keep_across_exec(&self) -> Result<()> {
        fcntl_setfd(&self.file, FdFlags::empty())?;
        Ok(())
    }
}

/// `toolchain_ensure`: returns holding the main lock SHARED, provisioning
/// under the exclusive lock when needed. Every failure path drops both files
/// (releasing their locks) and leaves the toolchains as they are.
pub fn ensure(
    lock_file: &Path,
    want: &Want,
    timeout: Duration,
    environment: &Environment,
) -> Result<Held> {
    let gate_path = {
        let mut name = lock_file.as_os_str().to_owned();
        name.push(".gate");
        PathBuf::from(name)
    };
    let gate = open_lock(&gate_path)?;
    let main = open_lock(lock_file)?;
    acquire(&gate, Mode::Exclusive, timeout, "admission gate")?;
    for _ in 0..MAX_ROUNDS {
        acquire(&main, Mode::Shared, timeout, "shared toolchain")?;
        if ready(want, environment)? {
            // Warm: the gate goes, main stays shared for the job.
            gate.unlock()?;
            drop(gate);
            return Ok(Held { file: main });
        }
        // Missing state: keep the gate, drop main shared (never upgrade), then
        // take main exclusive. New readers now queue at the gate.
        main.unlock()?;
        acquire(&main, Mode::Exclusive, timeout, "exclusive toolchain")?;
        if !ready(want, environment)? {
            install(want, environment)?;
            if !ready(want, environment)? {
                return Err(failure(
                    "toolchain-lock: toolchain still not ready after install",
                ));
            }
        }
        main.unlock()?;
    }
    Err(failure(format!(
        "toolchain-lock: toolchain not ready after {MAX_ROUNDS} rounds"
    )))
}

/// `rustup which --toolchain stable cargo`: the stable cargo a job's `PATH` should find first
/// (the cached runners' `cargo_bin`). A missing or non-executable answer is an error.
pub fn stable_cargo(environment: &Environment) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = listing(
        "rustup",
        &["which", "--toolchain", "stable", "cargo"],
        environment,
    )
    .map(|output| PathBuf::from(output.trim_end()))
    .filter(|path| path.is_absolute())
    .ok_or_else(|| failure("toolchain-lock: rustup which --toolchain stable cargo failed"))?;
    match fs::metadata(&path) {
        Ok(meta) if meta.is_file() && meta.permissions().mode() & 0o111 != 0 => Ok(path),
        _ => Err(failure(format!(
            "toolchain-lock: the stable cargo {} is not an executable file",
            path.display()
        ))),
    }
}

/// `repo_install_exact`: the binary under `CARGO_INSTALL_ROOT/bin` must report
/// exactly `<binary> <version>`; otherwise `cargo install --locked` and check
/// again. Equality is exact: a substring match would accept 0.2.1000 for 0.2.100.
pub fn install_exact(
    binary: &str,
    package: &str,
    version: &str,
    environment: &Environment,
) -> Result<()> {
    let root = environment
        .get(OsStr::new("CARGO_INSTALL_ROOT"))
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| failure("CARGO_INSTALL_ROOT must be a per-repo install root"))?;
    let wanted = format!("{binary} {version}");
    let path = root.join("bin").join(binary);
    let have = |path: &Path| {
        Command::new(path)
            .arg("--version")
            .env_clear()
            .envs(environment)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .trim_end_matches('\n')
                    .to_owned()
            })
    };
    if have(&path).as_deref() == Some(wanted.as_str()) {
        return Ok(());
    }
    let status = command(
        "cargo",
        &[
            "install",
            "--locked",
            "--version",
            version,
            "--root",
            &root.to_string_lossy(),
            package,
        ],
        environment,
    )?
    .status()?;
    if !status.success() {
        return Err(failure(format!(
            "cargo install {package} failed ({status})"
        )));
    }
    if have(&path).as_deref() == Some(wanted.as_str()) {
        Ok(())
    } else {
        Err(failure(format!(
            "{} does not report {wanted:?} after install",
            path.display()
        )))
    }
}

fn wasm_bindgen_version(repo: &Path) -> Result<String> {
    let lock = repo.join("Cargo.lock");
    let text = fs::read_to_string(&lock)
        .map_err(|error| failure(format!("cannot read {}: {error}", lock.display())))?;
    let document: toml::Table = toml::from_str(&text)
        .map_err(|error| failure(format!("cannot parse {}: {error}", lock.display())))?;
    let mut versions: Vec<&str> = document
        .get("package")
        .and_then(|packages| packages.as_array())
        .into_iter()
        .flatten()
        .filter(|package| {
            package.get("name").and_then(|name| name.as_str()) == Some("wasm-bindgen")
        })
        .filter_map(|package| package.get("version").and_then(|version| version.as_str()))
        .collect();
    versions.sort_unstable();
    versions.dedup();
    match versions.as_slice() {
        [version] => Ok((*version).to_owned()),
        [] => Err(failure("no wasm-bindgen package in Cargo.lock")),
        several => Err(failure(format!(
            "several wasm-bindgen versions in Cargo.lock: {}",
            several.join(", ")
        ))),
    }
}

fn runner_reports(directory: &Path, version: &str) -> bool {
    Command::new(directory.join("wasm-bindgen-test-runner"))
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            String::from_utf8_lossy(&output.stdout).trim_end()
                == format!("wasm-bindgen-test-runner {version}")
        })
}

fn fetch(url: &str, output: &Path, environment: &Environment) -> Result<()> {
    let status = command(
        "curl",
        &[
            "-A",
            "ci-dependency-fetch/1.0",
            "--fail",
            "--location",
            "--retry",
            "3",
            "--output",
            &output.to_string_lossy(),
            url,
        ],
        environment,
    )?
    .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(failure(format!("download of {url} failed ({status})")))
    }
}

/// Fetch, verify and unpack the wasm-bindgen test runner pinned by the
/// repository's `Cargo.lock` into `<into>/<version>`; an existing directory
/// that reports the right version is reused without network.
pub fn wasm_bindgen(repo: &Path, into: &Path, environment: &Environment) -> Result<PathBuf> {
    let version = wasm_bindgen_version(repo)?;
    if std::env::consts::ARCH != "x86_64" {
        return Err(failure(format!(
            "unsupported host architecture {} (x86_64 only)",
            std::env::consts::ARCH
        )));
    }
    fs::create_dir_all(into)?;
    let target = into.join(&version);
    if runner_reports(&target, &version) {
        return Ok(target);
    }
    let base = environment
        .get(OsStr::new("WASM_BINDGEN_DOWNLOAD_BASE"))
        .filter(|base| !base.is_empty())
        .map(|base| base.to_string_lossy().into_owned())
        .unwrap_or_else(|| "https://github.com/wasm-bindgen/wasm-bindgen/releases/download".into());
    let archive_name = format!("wasm-bindgen-{version}-x86_64-unknown-linux-musl.tar.gz");
    let url = format!("{}/{version}/{archive_name}", base.trim_end_matches('/'));
    let work = into.join(format!(
        ".work-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    fs::create_dir(&work)?;
    let result = unpack(&work, &url, &archive_name, &version, &target, environment);
    let _ = fs::remove_dir_all(&work);
    result.map(|()| target)
}

fn unpack(
    work: &Path,
    url: &str,
    archive_name: &str,
    version: &str,
    target: &Path,
    environment: &Environment,
) -> Result<()> {
    let downloads = work.join("download");
    let output = work.join("out");
    fs::create_dir(&downloads)?;
    fs::create_dir(&output)?;
    let archive = downloads.join(archive_name);
    let sum = downloads.join(format!("{archive_name}.sha256sum"));
    fetch(url, &archive, environment)?;
    fetch(&format!("{url}.sha256sum"), &sum, environment)?;
    let expected = fs::read_to_string(&sum)?
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| failure("empty .sha256sum"))?;
    let actual = sha256_file(&archive)?;
    if expected != actual {
        return Err(failure(format!(
            "sha256 mismatch for {archive_name}: expected {expected}, got {actual}"
        )));
    }
    let status = command(
        "tar",
        &[
            "-xzf",
            &archive.to_string_lossy(),
            "--strip-components=1",
            "-C",
            &output.to_string_lossy(),
        ],
        environment,
    )?
    .status()?;
    if !status.success() {
        return Err(failure(format!("tar extraction failed ({status})")));
    }
    if !runner_reports(&output, version) {
        return Err(failure(format!(
            "wasm-bindgen-test-runner does not report version {version}"
        )));
    }
    match fs::rename(&output, target) {
        Ok(()) => Ok(()),
        // A peer renamed first: keep theirs (ours is deleted by the caller).
        Err(_) if runner_reports(target, version) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Subcommand)]
pub enum Action {
    /// Exit 0 when the toolchain is ready, 1 when not (read-only probes).
    Ready {
        #[arg(long)]
        no_nightly: bool,
        /// TOOLCHAIN:TARGET, repeatable.
        #[arg(long = "target")]
        targets: Vec<String>,
    },
    /// Take the shared toolchain lock (provisioning when needed), then exec
    /// COMMAND holding it shared until the command exits.
    Ensure {
        #[arg(long)]
        lock_file: PathBuf,
        /// Seconds to wait for each lock.
        #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECONDS)]
        timeout: u64,
        #[arg(long)]
        no_nightly: bool,
        /// TOOLCHAIN:TARGET, repeatable.
        #[arg(long = "target")]
        targets: Vec<String>,
        #[arg(last = true, required = true)]
        command: Vec<OsString>,
    },
    /// Install a cargo binary at an exact version under CARGO_INSTALL_ROOT.
    InstallExact {
        binary: String,
        package: String,
        version: String,
    },
    /// Fetch the wasm-bindgen test runner pinned by Cargo.lock; print its directory.
    WasmBindgen {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Default: $RUNNER_TEMP or $TMPDIR or /tmp, plus /ccid-wasm-bindgen.
        #[arg(long)]
        into: Option<PathBuf>,
    },
}

fn want(no_nightly: bool, targets: &[String]) -> Result<Want> {
    Ok(Want {
        nightly: !no_nightly,
        targets: targets
            .iter()
            .map(|target| parse_target(target))
            .collect::<Result<_>>()?,
    })
}

fn report(result: Result<()>) -> u8 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("ccid toolchain: {error}");
            1
        }
    }
}

/// Run one subcommand. Usage errors are `Err` (exit 2); operational failures
/// print a message and return 1.
pub fn run(action: Action) -> Result<u8> {
    let environment: Environment = std::env::vars_os().collect();
    match action {
        Action::Ready {
            no_nightly,
            targets,
        } => Ok(u8::from(!ready(
            &want(no_nightly, &targets)?,
            &environment,
        )?)),
        Action::Ensure {
            lock_file,
            timeout,
            no_nightly,
            targets,
            command,
        } => {
            let want = want(no_nightly, &targets)?;
            let held = match ensure(
                &lock_file,
                &want,
                Duration::from_secs(timeout),
                &environment,
            ) {
                Ok(held) => held,
                Err(error) => return Ok(report(Err(error))),
            };
            if let Err(error) = held.keep_across_exec() {
                return Ok(report(Err(error)));
            }
            let mut parts = command.into_iter();
            let name = parts.next().ok_or_else(|| failure("missing COMMAND"))?;
            let error = Command::new(&name).args(parts).exec();
            drop(held);
            Ok(report(Err(failure(format!(
                "cannot execute {}: {error}",
                name.to_string_lossy()
            )))))
        }
        Action::InstallExact {
            binary,
            package,
            version,
        } => Ok(report(install_exact(
            &binary,
            &package,
            &version,
            &environment,
        ))),
        Action::WasmBindgen { repo, into } => {
            let into = into.unwrap_or_else(|| {
                let base = ["RUNNER_TEMP", "TMPDIR"]
                    .iter()
                    .find_map(|name| environment.get(OsStr::new(name)).filter(|v| !v.is_empty()))
                    .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
                base.join("ccid-wasm-bindgen")
            });
            Ok(report(wasm_bindgen(&repo, &into, &environment).map(
                |directory| {
                    println!("{}", directory.display());
                },
            )))
        }
    }
}

#[cfg(test)]
mod tests;

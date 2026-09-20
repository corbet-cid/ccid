#![forbid(unsafe_code)]

#[cfg(unix)]
use process_wrap::std::{ChildWrapper, CommandWrap};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Read, Seek, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::{process::Stdio, sync::mpsc};

pub const SOURCE_REVISION: &str = env!("CCID_SOURCE_REVISION");
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub type Environment = BTreeMap<OsString, OsString>;

mod admission;
mod cache;
mod dependency;

pub use dependency::resolve_cargo;

fn failure(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::other(message.into()).into()
}
fn event(value: serde_json::Value) {
    println!("{value}");
}
fn value(environment: &Environment, name: &str) -> Option<String> {
    environment
        .get(&OsString::from(name))
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
}
fn set(environment: &mut Environment, name: &str, v: impl Into<OsString>) {
    environment.insert(name.into(), v.into());
}
fn positive(v: &str, name: &str) -> Result<u64> {
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) || v.starts_with('0') {
        return Err(failure(format!("{name} must be a positive integer")));
    }
    Ok(v.parse()?)
}
fn cpu_budget() -> u64 {
    let available = thread::available_parallelism().map_or(1, |n| n.get() as u64);
    if let Ok(quota) = fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        let fields: Vec<_> = quota.split_whitespace().collect();
        if fields.first() == Some(&"max") {
            return available;
        }
        if let [q, p] = fields.as_slice() {
            if let (Ok(q), Ok(p)) = (q.parse::<u64>(), p.parse::<u64>()) {
                if p > 0 {
                    return available.min((q / p).max(1));
                }
            }
        }
    }
    available.min(4).max(1)
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Budget {
    pub jobs: u64,
    pub test_threads: u64,
    pub nix_jobs: u64,
    pub nix_cores: u64,
    pub memory_mb: Option<u64>,
    pub timeout: u64,
}
pub fn budget(environment: &Environment) -> Result<Budget> {
    let jobs = value(environment, "CI_JOBS").or_else(|| value(environment, "CARGO_BUILD_JOBS"));
    let mut jobs = positive(&jobs.unwrap_or_else(|| cpu_budget().to_string()), "CI_JOBS")?;
    let memory = value(environment, "CI_MEMORY_MB")
        .map(|v| positive(&v, "CI_MEMORY_MB"))
        .transpose()?;
    if let Some(per_job) = value(environment, "CI_MEMORY_PER_JOB_MB") {
        let per_job = positive(&per_job, "CI_MEMORY_PER_JOB_MB")?;
        let allocation =
            memory.ok_or_else(|| failure("CI_MEMORY_PER_JOB_MB requires CI_MEMORY_MB"))?;
        if allocation < per_job {
            return Err(failure("Memory allocation cannot fit one job"));
        }
        jobs = jobs.min(allocation / per_job);
    }
    let nix_jobs = positive(
        &value(environment, "CI_NIX_JOBS").unwrap_or_else(|| "1".into()),
        "CI_NIX_JOBS",
    )?;
    if nix_jobs > jobs {
        return Err(failure("CI_NIX_JOBS exceeds the allocated CPU budget"));
    }
    Ok(Budget {
        jobs,
        test_threads: positive(
            &value(environment, "CI_TEST_THREADS").unwrap_or_else(|| jobs.to_string()),
            "CI_TEST_THREADS",
        )?,
        nix_jobs,
        nix_cores: jobs / nix_jobs,
        memory_mb: memory,
        timeout: positive(
            &value(environment, "CI_TIMEOUT").unwrap_or_else(|| "2700".into()),
            "CI_TIMEOUT",
        )?,
    })
}

pub struct Runner {
    pub root: PathBuf,
    pub environment: Environment,
    deadline: Instant,
    nix_inventory: Option<(String, Vec<String>)>,
}
#[cfg(unix)]
struct OwnedChild(Box<dyn ChildWrapper>);
#[cfg(unix)]
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}
impl Runner {
    pub fn new(root: PathBuf, environment: Environment, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| failure("Deadline is out of range"))?;
        Self::until(root, environment, deadline)
    }

    pub(crate) fn until(
        root: PathBuf,
        environment: Environment,
        deadline: Instant,
    ) -> Result<Self> {
        Ok(Self {
            root,
            environment,
            deadline,
            nix_inventory: None,
        })
    }
    pub fn run(&self, argv: &[String], capture: bool) -> Result<String> {
        validate_command(argv)?;
        if INTERRUPTED.load(Ordering::SeqCst) || Instant::now() >= self.deadline {
            return Err(failure("Check interrupted or total deadline exceeded"));
        }
        #[cfg(all(windows, feature = "windows-experimental"))]
        {
            windows::run(self, argv, capture)
        }
        #[cfg(all(windows, not(feature = "windows-experimental")))]
        {
            Err(failure("Windows check execution requires an experimental native build; process-tree cancellation remains unverified"))
        }
        #[cfg(unix)]
        {
            self.run_unix(argv, capture)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(failure(
                "Owned process cancellation is unsupported on this platform",
            ))
        }
    }
    #[cfg(unix)]
    fn run_unix(&self, argv: &[String], capture: bool) -> Result<String> {
        let started = Instant::now();
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(&self.root)
            .env_clear()
            .envs(&self.environment);
        command.stdin(Stdio::null()).stderr(Stdio::inherit());
        command.stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
        let mut command = CommandWrap::from(command);
        #[cfg(unix)]
        command.wrap(process_wrap::std::ProcessGroup::leader());
        let mut child = OwnedChild(command.spawn()?);
        let reader = if capture {
            let stdout = child
                .0
                .stdout()
                .take()
                .ok_or_else(|| failure("Missing captured stdout"))?;
            let (send, receive) = mpsc::channel();
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = stdout
                    .take(16 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes);
                let _ = send.send(result);
            });
            Some(receive)
        } else {
            None
        };
        let status = loop {
            if INTERRUPTED.load(Ordering::SeqCst) || Instant::now() >= self.deadline {
                #[cfg(unix)]
                {
                    let _ = child.0.signal(15);
                    let grace = Instant::now() + Duration::from_secs(10);
                    while Instant::now() < grace {
                        if child.0.try_wait()?.is_some() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                }
                let _ = child.0.start_kill();
                let _ = child.0.wait();
                return Err(failure(
                    "Check interrupted or timed out; its owned process group/job was terminated",
                ));
            }
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            thread::sleep(Duration::from_millis(20));
        };
        // Do not let detached descendants retain the shared project cache or pipes.
        let _ = child.0.start_kill();
        event(
            json!({"event":"command", "executable":Path::new(&argv[0]).file_name().map(|s|s.to_string_lossy()), "seconds":started.elapsed().as_secs_f64(), "exit_code":status.code()}),
        );
        if !status.success() {
            return Err(failure(format!(
                "{} failed: {status}",
                Path::new(&argv[0])
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            )));
        }
        if let Some(reader) = reader {
            let bytes = reader.recv_timeout(Duration::from_secs(10)).map_err(|_| {
                failure("Captured output did not close after command termination")
            })??;
            if bytes.len() > 16 * 1024 * 1024 {
                return Err(failure("Captured command output exceeded 16 MiB"));
            }
            return Ok(String::from_utf8(bytes)?.trim().to_owned());
        }
        Ok(String::new())
    }
}
fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}

fn validate_command(argv: &[String]) -> Result<()> {
    if argv.first().is_none_or(String::is_empty) || argv.iter().any(|arg| arg.contains('\0')) {
        return Err(failure(
            "Commands require a nonempty executable and arguments without NUL bytes",
        ));
    }
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn hex_identity(value: &str, sizes: &[usize]) -> bool {
    sizes.contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn safe_relative(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => normalized.push(name),
            Component::CurDir => {}
            Component::ParentDir if normalized.pop() => {}
            _ => return Err(failure("Source archive contains an escaping path or link")),
        }
    }
    Ok(normalized)
}
// Resolve every link using the complete archive map, including links encountered
// before a `..` component. Pure lexical normalization misses chained escapes.
fn resolve_archive_path(
    path: &Path,
    links: &BTreeMap<PathBuf, PathBuf>,
    active: &mut BTreeSet<PathBuf>,
) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if resolved.pop() => {}
            Component::Normal(name) => {
                resolved.push(name);
                if let Some(target) = links.get(&resolved) {
                    let link = resolved.clone();
                    if active.len() >= 64 {
                        return Err(failure("Source symbolic-link chain exceeds 64 entries"));
                    }
                    if !active.insert(link.clone()) {
                        return Err(failure("Source archive contains a cyclic symbolic link"));
                    }
                    resolved = resolve_archive_path(
                        &link.parent().unwrap_or_else(|| Path::new("")).join(target),
                        links,
                        active,
                    )?;
                    active.remove(&link);
                }
            }
            _ => return Err(failure("Source archive contains an escaping path or link")),
        }
    }
    Ok(resolved)
}

pub fn verify_source(archive: &Path, digest: &str, commit: &str, destination: &Path) -> Result<()> {
    if !hex_identity(digest, &[64]) || !hex_identity(commit, &[40, 64]) {
        return Err(failure(
            "Source digest and commit must be complete lowercase identities",
        ));
    }
    // Hash the bytes while copying into a private, automatically removed snapshot.
    // Every later pass uses this handle, so pathname replacement or in-place writes
    // to the supplied archive cannot change the verified extraction input.
    let mut input = File::open(archive)?;
    let mut snapshot = tempfile::tempfile()?;
    let mut actual = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let size = input.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        snapshot.write_all(&buffer[..size])?;
        actual.update(&buffer[..size]);
    }
    if format!("{:x}", actual.finalize()) != digest {
        return Err(failure("Source archive SHA-256 mismatch"));
    }
    snapshot.rewind()?;
    let result = Command::new("git")
        .arg("get-tar-commit-id")
        .stdin(snapshot.try_clone()?)
        .output()?;
    if !result.status.success() || String::from_utf8(result.stdout)?.trim() != commit {
        return Err(failure("Source archive embedded commit mismatch"));
    }
    if destination.exists() && fs::read_dir(destination)?.next().is_some() {
        return Err(failure(
            "Source destination must be empty; existing work is never overwritten",
        ));
    }
    snapshot.rewind()?;
    let mut paths = Vec::new();
    let mut links = BTreeMap::new();
    for entry in tar::Archive::new(&mut snapshot).entries()? {
        let entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        let path = entry.path()?;
        if path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(failure("Source archive contains an unsafe path"));
        }
        let path = safe_relative(&path)?;
        if kind.is_symlink() {
            let target = entry
                .link_name()?
                .ok_or_else(|| failure("Source link has no target"))?
                .into_owned();
            links.insert(path.clone(), target);
        } else if !kind.is_file() && !kind.is_dir() {
            return Err(failure(
                "Source archive contains a special file or hard link",
            ));
        }
        paths.push(path);
    }
    for path in &paths {
        resolve_archive_path(path, &links, &mut BTreeSet::new())?;
        // Git archives never contain children beneath a symbolic-link member.
        if path
            .ancestors()
            .skip(1)
            .any(|parent| links.contains_key(parent))
        {
            return Err(failure("Source archive writes beneath a symbolic link"));
        }
    }
    snapshot.rewind()?;
    fs::create_dir_all(destination)?;
    let mut archive = tar::Archive::new(snapshot);
    archive.set_preserve_mtime(false);
    archive.set_preserve_permissions(false);
    archive.unpack(destination)?;
    event(json!({"event":"source", "commit":commit, "sha256":digest, "files":paths.len()}));
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    project: String,
    checks: BTreeMap<String, Check>,
}
#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Check {
    kind: String,
    actions: Option<Vec<String>>,
    toolchain: Option<String>,
    workspace: bool,
    all_features: bool,
    all_targets: bool,
    release: bool,
    features: Vec<String>,
    packages: Vec<String>,
    exclude: Vec<String>,
    test_runner: Option<String>,
    mode: Option<String>,
    expected_checks: Option<Vec<String>>,
    checks: Vec<String>,
    manager: Option<String>,
    scripts: Option<Vec<String>>,
    install: Option<bool>,
    commands: Vec<Vec<String>>,
}
fn cargo_prefix(check: &Check, runner: &Runner) -> Result<(Vec<String>, Vec<String>)> {
    let toolchain = check.toolchain.as_deref().unwrap_or("system");
    let mut cargo = if toolchain == "system" {
        strings(&["cargo"])
    } else {
        strings(&["rustup", "run", toolchain, "cargo"])
    };
    let rustc = if toolchain == "system" {
        strings(&["rustc", "--version"])
    } else {
        strings(&["rustup", "run", toolchain, "rustc", "--version"])
    };
    if value(&runner.environment, "CI_LINKER").as_deref() == Some("mold") {
        let driver = mold_driver(&runner.environment)?;
        runner.run(&[driver.clone(), "--version".into()], false)?;
        cargo.splice(0..0, [driver, "-run".into()]);
    }
    Ok((cargo, rustc))
}
fn mold_driver(environment: &Environment) -> Result<String> {
    let paths = environment
        .get(&OsString::from("PATH"))
        .ok_or_else(|| failure("CI_LINKER=mold requires PATH"))?;
    let executable = std::env::split_paths(paths)
        .map(|path| path.join(if cfg!(windows) { "mold.exe" } else { "mold" }))
        .find(|path| path.is_file())
        .ok_or_else(|| failure("CI_LINKER=mold requires an installed mold executable"))?;
    let resolved = executable.canonicalize()?;
    if let Some(root) = resolved.parent().and_then(Path::parent) {
        let metadata = root.join("nix-support/orig-bintools");
        if metadata.is_file() {
            return Ok(Path::new(fs::read_to_string(metadata)?.trim())
                .join("bin/mold")
                .to_string_lossy()
                .into_owned());
        }
    }
    Ok(executable.to_string_lossy().into_owned())
}
pub fn cargo_commands(
    check: &Check,
    cargo: &[String],
    test_threads: &str,
) -> Result<Vec<Vec<String>>> {
    let mut options = strings(&["--locked"]);
    if check.workspace {
        options.push("--workspace".into());
    }
    if check.all_features {
        options.push("--all-features".into());
    }
    for p in &check.packages {
        options.extend(["--package".into(), p.clone()]);
    }
    for p in &check.exclude {
        options.extend(["--exclude".into(), p.clone()]);
    }
    if !check.features.is_empty() {
        options.extend(["--features".into(), check.features.join(",")]);
    }
    if check.release {
        options.push("--release".into());
    }
    let defaults = strings(&["fmt", "test", "clippy"]);
    let actions = check.actions.as_ref().unwrap_or(&defaults);
    if actions.is_empty() {
        return Err(failure("Cargo action selection is empty"));
    }
    let mut commands = Vec::new();
    for action in actions {
        let mut command = cargo.to_vec();
        match action.as_str() {
            "fmt" => command.extend(strings(&["fmt", "--all", "--", "--check"])),
            "test" => match check.test_runner.as_deref().unwrap_or("cargo") {
                "cargo" => {
                    command.push("test".into());
                    command.extend(options.clone());
                    if check.all_targets {
                        command.push("--all-targets".into());
                    }
                }
                "nextest" => {
                    command.extend(strings(&["nextest", "run", "--test-threads", test_threads]));
                    command.extend(options.clone());
                    commands.push(command);
                    command = cargo.to_vec();
                    command.extend(strings(&["test", "--doc"]));
                    command.extend(options.clone());
                }
                _ => return Err(failure("Unknown Cargo test runner")),
            },
            "clippy" => {
                command.extend(strings(&["clippy", "--all-targets"]));
                command.extend(options.clone());
                command.extend(strings(&["--", "-D", "warnings"]));
            }
            "check" | "build" => {
                command.push(action.clone());
                command.extend(options.clone());
                if check.all_targets {
                    command.push("--all-targets".into());
                }
            }
            _ => return Err(failure(format!("Unknown Cargo action: {action}"))),
        }
        commands.push(command);
    }
    Ok(commands)
}
fn nix_check(check: &Check, runner: &mut Runner) -> Result<()> {
    if cfg!(windows) {
        return Err(failure(
            "Nix checks require a supported Nix host; Windows is unsupported",
        ));
    }
    let mode = nix_mode(check)?;
    if runner.nix_inventory.is_none() {
        let system = runner.run(
            &strings(&[
                "nix",
                "eval",
                "--impure",
                "--raw",
                "--expr",
                "builtins.currentSystem",
            ]),
            true,
        )?;
        let inventory = runner.run(
            &strings(&[
                "nix",
                "eval",
                "--no-update-lock-file",
                "--json",
                &format!(".#checks.{system}"),
                "--apply",
                "builtins.attrNames",
            ]),
            true,
        )?;
        runner.nix_inventory = Some((system, serde_json::from_str(&inventory)?));
    }
    let (system, inventory) = runner
        .nix_inventory
        .as_ref()
        .ok_or_else(|| failure("Missing Nix inventory"))?;
    if inventory.is_empty() {
        return Err(failure("No native checks declared; refusing empty success"));
    }
    if let Some(expected) = &check.expected_checks {
        let mut expected = expected.clone();
        let mut actual = inventory.clone();
        expected.sort();
        actual.sort();
        if actual != expected {
            return Err(failure("Native inventory differs from expected_checks"));
        }
    }
    event(json!({"event":"nix-inventory", "system":system, "checks":inventory, "mode":mode}));
    let mut command = strings(&[
        "nix",
        "flake",
        "check",
        "--no-update-lock-file",
        "--keep-going",
        "--print-build-logs",
    ]);
    match mode {
        "list" => return Ok(()),
        "native" => {}
        "eval" => command.extend(strings(&["--all-systems", "--no-build"])),
        "all-systems" => command.push("--all-systems".into()),
        "named" => {
            if check.checks.is_empty() || check.checks.iter().any(|c| !inventory.contains(c)) {
                return Err(failure(
                    "Named Nix checks must be a nonempty subset of native checks",
                ));
            }
            command = strings(&[
                "nix",
                "build",
                "--no-update-lock-file",
                "--no-link",
                "--keep-going",
                "--print-build-logs",
            ]);
            for name in &check.checks {
                command.push(format!(
                    ".#checks.{system}.{}",
                    serde_json::to_string(name)?
                ));
            }
        }
        _ => return Err(failure("Unknown Nix mode")),
    }
    runner.run(&command, false)?;
    Ok(())
}
fn javascript_executable(manager: &str, windows: bool) -> &str {
    match (manager, windows) {
        ("npm", true) => "npm.cmd",
        ("pnpm", true) => "pnpm.cmd",
        _ => manager,
    }
}
fn javascript_commands(check: &Check) -> Result<Vec<Vec<String>>> {
    let manager = check.manager.as_deref().unwrap_or("npm");
    let executable = javascript_executable(manager, cfg!(windows));
    let install = match manager {
        "npm" => strings(&[
            executable,
            "ci",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
        ]),
        "bun" => strings(&[executable, "install", "--frozen-lockfile"]),
        "pnpm" => strings(&[executable, "install", "--frozen-lockfile"]),
        _ => return Err(failure("JavaScript manager must be npm, bun, or pnpm")),
    };
    let defaults = strings(&["test"]);
    let scripts = check.scripts.as_ref().unwrap_or(&defaults);
    if scripts.is_empty() {
        return Err(failure("JavaScript script selection is empty"));
    }
    let mut commands = Vec::new();
    if check.install.unwrap_or(true) {
        commands.push(install);
    }
    for script in scripts {
        if script.is_empty() {
            return Err(failure("JavaScript script names must not be empty"));
        }
        commands.push(strings(&[executable, "run", script]));
    }
    Ok(commands)
}

fn nix_mode(check: &Check) -> Result<&str> {
    let mode = check.mode.as_deref().unwrap_or("native");
    match mode {
        "list" | "native" | "eval" | "all-systems" => Ok(mode),
        "named" if !check.checks.is_empty() && check.checks.iter().all(|name| !name.is_empty()) => {
            Ok(mode)
        }
        "named" => Err(failure("Named Nix checks require nonempty check names")),
        _ => Err(failure("Unknown Nix mode")),
    }
}

fn validate_check(check: &Check) -> Result<()> {
    let commands = match check.kind.as_str() {
        "cargo" => {
            if check.toolchain.as_deref() == Some("") {
                return Err(failure("Cargo toolchain must not be empty"));
            }
            if !matches!(
                check.test_runner.as_deref(),
                None | Some("cargo" | "nextest")
            ) {
                return Err(failure("Unknown Cargo test runner"));
            }
            cargo_commands(check, &strings(&["cargo"]), "1")?
        }
        "javascript" => javascript_commands(check)?,
        "nix" => {
            nix_mode(check)?;
            return Ok(());
        }
        "commands" if !check.commands.is_empty() => check.commands.clone(),
        "commands" => return Err(failure("Custom command selection is empty")),
        _ => return Err(failure(format!("Unknown check kind: {}", check.kind))),
    };
    for command in commands {
        validate_command(&command)?;
    }
    Ok(())
}

pub fn run_checks(repo: &Path, manifest: &Path, selectors: &[String], plan: bool) -> Result<()> {
    run_checks_inner(repo, manifest, selectors, plan, None, None, None)
}

pub(crate) fn run_checks_with_environment(
    repo: &Path,
    manifest: &Path,
    selectors: &[String],
    plan: bool,
    environment: Environment,
    verified_commit: &str,
    deadline: Instant,
) -> Result<()> {
    run_checks_inner(
        repo,
        manifest,
        selectors,
        plan,
        Some(verified_commit),
        Some(environment),
        Some(deadline),
    )
}

pub fn run_archive_checks(
    archive: &Path,
    digest: &str,
    commit: &str,
    manifest: &Path,
    selectors: &[String],
    plan: bool,
) -> Result<()> {
    safe_relative(manifest)?;
    let mut environment: Environment = std::env::vars_os().collect();
    cache::repository_identity(Path::new("."), &environment, true)?;
    let source = cache::scratch(&mut environment)?;
    verify_source(archive, digest, commit, source.path())?;
    run_checks_inner(
        source.path(),
        manifest,
        selectors,
        plan,
        Some(commit),
        None,
        None,
    )
}

fn run_checks_inner(
    repo: &Path,
    manifest: &Path,
    selectors: &[String],
    plan: bool,
    verified_commit: Option<&str>,
    base_environment: Option<Environment>,
    enclosing_deadline: Option<Instant>,
) -> Result<()> {
    let archive = verified_commit.is_some();
    let root = repo.canonicalize()?;
    let manifest_path = root.join(manifest);
    let bytes = fs::read(&manifest_path)?;
    let manifest: Manifest = toml::from_str(std::str::from_utf8(&bytes)?)?;
    let slug = &manifest.project;
    if manifest.schema != 1
        || slug.is_empty()
        || !slug.as_bytes()[0].is_ascii_alphanumeric()
        || !slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(failure(
            "Manifest requires schema=1 and a plain project slug",
        ));
    }
    let mut selected = Vec::new();
    for selector in selectors {
        for name in selector.split(',').filter(|s| !s.is_empty()) {
            if !selected.contains(&name.to_owned()) {
                selected.push(name.to_owned());
            }
        }
    }
    if selected.is_empty()
        || selected
            .iter()
            .any(|name| !manifest.checks.contains_key(name))
    {
        return Err(failure("Select one or more declared nonempty check names"));
    }
    // Validate the complete selection before any earlier check can execute.
    // Planning and execution share the same command builders and static rules.
    for name in &selected {
        validate_check(&manifest.checks[name])
            .map_err(|error| failure(format!("Invalid check {name}: {error}")))?;
    }
    let mut environment = base_environment.unwrap_or_else(|| std::env::vars_os().collect());
    if let Some(commit) = verified_commit {
        set(&mut environment, "CI_COMMIT_SHA", commit);
    }
    let resources = budget(&environment)?;
    let requested_deadline = Instant::now()
        .checked_add(Duration::from_secs(resources.timeout))
        .ok_or_else(|| failure("Check deadline is out of range"))?;
    let deadline = enclosing_deadline.map_or(requested_deadline, |deadline| {
        deadline.min(requested_deadline)
    });
    let linker = value(&environment, "CI_LINKER").unwrap_or_else(|| "system".into());
    if !["system", "mold"].contains(&linker.as_str()) {
        return Err(failure("CI_LINKER must be system or mold"));
    }
    let identity = cache::repository_identity(&root, &environment, archive)?;
    let target = cache::target_directory(&root, &identity, &environment)?;
    event(
        json!({"event":"plan", "project":slug,"checks":selected,"budget":resources,"linker_request":linker,"repository":identity,"target_directory":target,"source_commit":value(&environment,"CI_COMMIT_SHA"),"manifest_sha256":format!("{:x}",Sha256::digest(&bytes)),"tool_revision":SOURCE_REVISION}),
    );
    if plan {
        return Ok(());
    }
    if cfg!(all(windows, not(feature = "windows-experimental"))) {
        return Err(failure("Windows check execution requires an experimental native build; process-tree cancellation remains unverified"));
    }
    let (target, _lock) = cache::lock_target(&target, deadline)?;
    admission::admit(&mut environment)?;
    let resources = budget(&environment)?;
    set(
        &mut environment,
        "CARGO_BUILD_JOBS",
        resources.jobs.to_string(),
    );
    set(
        &mut environment,
        "RUST_TEST_THREADS",
        resources.test_threads.to_string(),
    );
    set(&mut environment, "CARGO_TARGET_DIR", target.as_os_str());
    let nix_config = format!(
        "{}\nmax-jobs = {}\ncores = {}\n",
        value(&environment, "NIX_CONFIG").unwrap_or_default(),
        resources.nix_jobs,
        resources.nix_cores
    );
    set(&mut environment, "NIX_CONFIG", nix_config);
    event(json!({"event":"allocation", "budget":resources}));
    if let Some(build) = value(&environment, "CARGO_BUILD_BUILD_DIR") {
        if root.join(build).canonicalize().ok().as_ref() != Some(&target) {
            return Err(failure("A distinct CARGO_BUILD_BUILD_DIR is unsupported: intermediates must share the locked target directory"));
        }
    }
    set(&mut environment, "CARGO_TARGET_DIR", target.as_os_str());
    set(
        &mut environment,
        "CCID_TARGET_LOCK_HELD",
        target.as_os_str(),
    );
    let _scratch = cache::scratch(&mut environment)?;
    let freshness = if archive {
        Some(cache::Freshness::prepare(&root, &target, &identity)?)
    } else {
        // A local check may compile uncommitted inputs into this same target.
        cache::Freshness::invalidate(&target)?;
        None
    };
    let mut runner = Runner::until(root, environment, deadline)?;
    for name in selected {
        let check = &manifest.checks[&name];
        let started = Instant::now();
        event(json!({"event":"check-start","check":name}));
        match check.kind.as_str() {
            "cargo" => {
                let (cargo, rustc) = cargo_prefix(check, &runner)?;
                let commands = cargo_commands(check, &cargo, &resources.test_threads.to_string())?;
                runner.run(&rustc, false)?;
                for command in commands {
                    runner.run(&command, false)?;
                }
            }
            "nix" => nix_check(check, &mut runner)?,
            "javascript" => {
                for command in javascript_commands(check)? {
                    runner.run(&command, false)?;
                }
            }
            "commands" => {
                runner.nix_inventory = None;
                if check.commands.is_empty() {
                    return Err(failure("Custom command selection is empty"));
                }
                for command in &check.commands {
                    runner.run(command, false)?;
                }
            }
            _ => return Err(failure(format!("Unknown check kind: {}", check.kind))),
        }
        event(
            json!({"event":"check-success","check":name,"seconds":started.elapsed().as_secs_f64()}),
        );
    }
    if INTERRUPTED.load(Ordering::SeqCst) {
        return Err(failure(
            "Check interrupted before publishing freshness metadata",
        ));
    }
    if let Some(freshness) = freshness {
        freshness.complete()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(all(windows, feature = "windows-experimental"))]
mod windows;

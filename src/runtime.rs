//! `[runtime]`: the persistent job environment of a `ccid:run-declared-checks` job.
//!
//! Before this table a repository's jobs ran `.ci/run-cached.sh`: a script that found the
//! runner's persistent state root, took a per-repository lock, put moon, proto and further
//! tools from one nixpkgs pin on `PATH`, took the shared toolchain lock, exported the build
//! environment and then ran `ccid cached` / `ccid check`. With `[runtime]` declared, `ccid
//! job-run` does exactly that around the job's `ccid check`, so the script and its copies go.
//!
//! The state layout is the script's, byte for byte, so persistent state carries over:
//!
//! ```text
//! <CI_CACHE_ROOT or CARGO_HOME>/ccid-cached/<sha256(CI_REPOSITORY_URL)[..24]>/   state, per repository
//!     lock              held exclusive for the whole job (flock, as the script's `flock 9`)
//!     moon-home/        MOON_HOME
//!     install-bin/      CARGO_INSTALL_ROOT (install_root = true)
//!     release-artifacts/ ARTIFACT_ROOT (unless the runner names one)
//!     <state_env dirs>/
//! <CI_TOOLCHAIN_ROOT or …/ccid-cached/toolchains>/                               shared by all repositories
//!     proto/            PROTO_HOME
//!     install.lock      the shared-first toolchain lock ([toolchain], see `toolchain`)
//! ```
//!
//! What the script did and the runtime does not:
//! - it never creates `.moon/cache/hashes` or `outputs` in the checkout: `ccid cached` links them
//!   into the result cache itself, and a link or directory made by anyone else makes the cached
//!   executor refuse the cache ("belongs to a different coordinator"), so every check ran
//!   uncached;
//! - it prints no tool versions (a check keys its tools through `cache_tools`; a missing tool
//!   fails its check) and guards no check selection (the job's declared checks are the selection).
//!
//! The locks outlive `exec`: their descriptors are inherited by the job command and everything it
//! starts, as with the script, and the kernel drops them when the last holder exits.
//!
//! The data structures parse on every platform (the manifest is the same everywhere); setting the
//! runtime up is Unix only, like `job-run`.
use crate::{failure, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

/// `[runtime]` in `.ci/ccid.toml`.
#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// nixpkgs attribute paths (`"python3"`, `"nodejs_24"`, `"llvmPackages.clang-unwrapped"`)
    /// built at the rendered check-shell's nixpkgs revision and put on `PATH` after moon and
    /// proto, which every runtime gets.
    pub tools: Vec<String>,
    /// `CARGO_INSTALL_ROOT` is the repository's own `<state>/install-bin` and its `bin` is on
    /// `PATH`, so a helper a check installs never lands in the shared cargo home.
    pub install_root: bool,
    /// Further persistent directories of this repository, by variable:
    /// `{ YARN_CACHE_FOLDER = "yarn-cache" }` exports `<state>/yarn-cache`.
    pub state_env: BTreeMap<String, String>,
}

/// `[toolchain]` in `.ci/ccid.toml`: what the job's Rust toolchain needs besides stable with
/// rustfmt and clippy. Read by the job runtime only.
#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ToolchainConfig {
    /// nightly with llvm-tools (coverage).
    pub nightly: bool,
    /// `TOOLCHAIN:TARGET` pairs, e.g. `"stable:wasm32-unknown-unknown"`.
    pub targets: Vec<String>,
}

/// The runtime of one job, as declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    pub config: Config,
    pub toolchain: Option<ToolchainConfig>,
}

/// The nixpkgs revision and NAR hash of the rendered check-shell (`src/render/check-shell.nix`).
/// One pin for the shell and the runtime's tools; a test keeps the two equal.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) const NIXPKGS_REVISION: &str = "b7c2ada94fe99c15b0dbcf4d11fd7850b957a436";
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) const NIXPKGS_NAR_HASH: &str = "sha256-IItrdb7Puk05RqOBWZYFC5X6Wl1sJmCfh5MWVHw5iMM=";

/// Tools every runtime provides: `ccid cached` runs checks through moon, and the toolchain
/// lock provisions Rust through proto.
pub(crate) const BASE_TOOLS: [&str; 2] = ["moon", "proto"];

/// Directory names below the state root that the runtime itself uses.
const RESERVED_STATE_DIRS: [&str; 6] = [
    "lock",
    "moon-home",
    "moon-cache",
    "install-bin",
    "release-artifacts",
    "toolchains",
];

/// Variables the runtime sets itself (or the runner owns); `state_env` cannot name them.
fn runtime_variable(name: &str) -> bool {
    matches!(
        name,
        "PATH"
            | "HOME"
            | "PROTO_HOME"
            | "MOON_HOME"
            | "CARGO_HOME"
            | "RUSTUP_HOME"
            | "CARGO_INSTALL_ROOT"
            | "ARTIFACT_ROOT"
            | "SOURCE_SHA256"
            | "TMPDIR"
    ) || name.starts_with("GIT_CONFIG")
        || name.starts_with("CI_")
        || name.starts_with("CCID_")
        || name.starts_with("PROTO_")
        || name.starts_with("RUSTUP_")
        || crate::jobs::reserved_env_key(name)
}

/// A nixpkgs attribute path: non-empty segments of `[A-Za-z0-9_+-]` joined by `.`.
fn valid_attribute(tool: &str) -> bool {
    !tool.is_empty()
        && tool.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_+-".contains(&b))
        })
}

/// Check the declared data before anything runs; `toolchain` alone is refused because nothing
/// else reads it.
pub(crate) fn validate(config: Option<&Config>, toolchain: Option<&ToolchainConfig>) -> Result<()> {
    let Some(config) = config else {
        if toolchain.is_some() {
            return Err(failure(
                "[toolchain] is read by the job runtime only: declare [runtime] as well",
            ));
        }
        return Ok(());
    };
    for (index, tool) in config.tools.iter().enumerate() {
        if !valid_attribute(tool) {
            return Err(failure(format!(
                "[runtime] tools must be nixpkgs attribute paths ([A-Za-z0-9_+-] segments joined by .), got {tool:?}"
            )));
        }
        if BASE_TOOLS.contains(&tool.as_str()) {
            return Err(failure(format!(
                "[runtime] tools: {tool} is always provided, do not list it"
            )));
        }
        if config.tools[..index].contains(tool) {
            return Err(failure(format!("[runtime] tools lists {tool} twice")));
        }
    }
    for (name, directory) in &config.state_env {
        let plain_name = name.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
            && name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        if !plain_name || runtime_variable(name) {
            return Err(failure(format!(
                "[runtime] state_env cannot set {name:?}: use an uppercase variable the runtime and the runner do not own"
            )));
        }
        let plain_directory = !directory.is_empty()
            && directory != "."
            && directory != ".."
            && directory
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if !plain_directory || RESERVED_STATE_DIRS.contains(&directory.as_str()) {
            return Err(failure(format!(
                "[runtime] state_env {name}: {directory:?} must be one plain directory name the runtime does not use itself"
            )));
        }
    }
    if let Some(toolchain) = toolchain {
        for target in &toolchain.targets {
            let plain = |part: &str| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            };
            match target.split_once(':') {
                Some((toolchain, triple)) if plain(toolchain) && plain(triple) => {}
                _ => {
                    return Err(failure(format!(
                        "[toolchain] targets must be TOOLCHAIN:TARGET, got {target:?}"
                    )))
                }
            }
        }
    }
    Ok(())
}

/// `sha256(repository URL)` as lowercase hex, first 24 characters: the script's
/// `printf '%s' "$CI_REPOSITORY_URL" | sha256sum | cut -c1-24`.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn namespace(repository_url: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(repository_url.as_bytes()))[..24].to_owned()
}

#[cfg(unix)]
pub(crate) use setup::{prepare, Setup};

#[cfg(unix)]
mod setup {
    use super::{namespace, Runtime, BASE_TOOLS, NIXPKGS_NAR_HASH, NIXPKGS_REVISION};
    use crate::{failure, toolchain, Environment, Result};
    use std::{
        collections::BTreeMap,
        ffi::{OsStr, OsString},
        fs::{self, File, OpenOptions, TryLockError},
        io::{self, Read},
        path::{Path, PathBuf},
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    const DEFAULT_TIMEOUT_SECONDS: u64 = 2700;
    const POLL: Duration = Duration::from_millis(100);

    /// The job's runtime, set up: the variables to add to the job command and the locks it
    /// must hold until it exits.
    #[derive(Debug)]
    pub struct Prepared {
        pub environment: BTreeMap<String, String>,
        state_lock: File,
        toolchain_lock: Option<toolchain::Held>,
    }

    impl Prepared {
        /// Let both locks outlive `exec` (close-on-exec cleared, no descriptor adoption).
        pub fn keep_across_exec(&self) -> Result<()> {
            rustix::io::fcntl_setfd(&self.state_lock, rustix::io::FdFlags::empty())?;
            if let Some(held) = &self.toolchain_lock {
                held.keep_across_exec()?;
            }
            Ok(())
        }
    }

    /// The outcome of [`prepare`]: ready, or an operational refusal the job reports with exit 1
    /// (a lock that did not come in time, a toolchain that could not be provisioned).
    #[derive(Debug)]
    pub enum Setup {
        Ready(Prepared),
        Refused(String),
    }

    fn get<'a>(environment: &'a Environment, name: &str) -> Option<&'a OsStr> {
        environment
            .get(OsStr::new(name))
            .map(OsString::as_os_str)
            .filter(|value| !value.is_empty())
    }

    fn text(path: &Path) -> Result<String> {
        path.to_str().map(str::to_owned).ok_or_else(|| {
            failure(format!(
                "the runtime path {} is not valid UTF-8",
                path.display()
            ))
        })
    }

    /// `<root>/ccid-cached/<namespace>` and the toolchain root, both absolute.
    pub(crate) fn layout(
        environment: &Environment,
        repository_url: &str,
    ) -> Result<(PathBuf, PathBuf)> {
        let root = get(environment, "CI_CACHE_ROOT")
            .or_else(|| get(environment, "CARGO_HOME"))
            .map(PathBuf::from)
            .filter(|root| root.is_absolute())
            .ok_or_else(|| {
                failure("[runtime] needs an absolute persistent CI_CACHE_ROOT or CARGO_HOME")
            })?;
        let state = root.join("ccid-cached").join(namespace(repository_url));
        let toolchains = match get(environment, "CI_TOOLCHAIN_ROOT") {
            Some(path) if Path::new(path).is_absolute() => PathBuf::from(path),
            Some(_) => return Err(failure("CI_TOOLCHAIN_ROOT must be absolute")),
            None => root.join("ccid-cached").join("toolchains"),
        };
        Ok((state, toolchains))
    }

    pub(crate) fn timeout(environment: &Environment) -> Result<Duration> {
        match get(environment, "CI_TIMEOUT") {
            None => Ok(Duration::from_secs(DEFAULT_TIMEOUT_SECONDS)),
            Some(value) => value
                .to_str()
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_secs)
                .ok_or_else(|| failure("CI_TIMEOUT must be a whole number of seconds")),
        }
    }

    /// The per-repository state lock, exclusive (`flock -w`): `None` when it did not come in time.
    pub(crate) fn lock_state(state: &Path, timeout: Duration) -> Result<Option<File>> {
        let path = state.join("lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        let deadline = Instant::now() + timeout;
        let mut told = false;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Some(file)),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            if !told {
                eprintln!(
                    "ccid job-run: another job of this repository holds {}; waiting",
                    path.display()
                );
                told = true;
            }
            thread::sleep(POLL.min(deadline - now));
        }
    }

    /// `nix build --no-link --print-out-paths` of the tools at the check-shell's pin: the store
    /// paths in nix's order.
    pub(crate) fn build_tools(tools: &[String], environment: &Environment) -> Result<Vec<PathBuf>> {
        let source = format!("github:NixOS/nixpkgs/{NIXPKGS_REVISION}?narHash={NIXPKGS_NAR_HASH}");
        let mut command = Command::new("nix");
        command
            .env_clear()
            .envs(environment)
            .args(["build", "--no-link", "--print-out-paths"])
            .stdin(Stdio::null())
            .stderr(Stdio::inherit());
        for tool in BASE_TOOLS
            .iter()
            .copied()
            .chain(tools.iter().map(String::as_str))
        {
            command.arg(format!("{source}#{tool}"));
        }
        let output = command
            .output()
            .map_err(|error| failure(format!("[runtime] cannot run nix build: {error}")))?;
        if !output.status.success() {
            return Err(failure(format!(
                "[runtime] nix build of the job's tools failed ({})",
                output.status
            )));
        }
        let paths: Vec<PathBuf> = String::from_utf8(output.stdout)?
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();
        if paths.is_empty() || paths.iter().any(|path| !path.is_absolute()) {
            return Err(failure(
                "[runtime] nix build printed no absolute store paths for the job's tools",
            ));
        }
        Ok(paths)
    }

    /// `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n` for every pair, after the `GIT_CONFIG_COUNT`
    /// entries the environment already carries, and the new count.
    pub(crate) fn git_config(
        environment: &Environment,
        pairs: &[(String, String)],
    ) -> Result<BTreeMap<String, String>> {
        let base = match get(environment, "GIT_CONFIG_COUNT") {
            None => 0,
            Some(count) => count
                .to_str()
                .and_then(|count| count.parse::<usize>().ok())
                .ok_or_else(|| failure("GIT_CONFIG_COUNT must be a whole number"))?,
        };
        let mut variables = BTreeMap::new();
        for (offset, (key, value)) in pairs.iter().enumerate() {
            variables.insert(format!("GIT_CONFIG_KEY_{}", base + offset), key.clone());
            variables.insert(format!("GIT_CONFIG_VALUE_{}", base + offset), value.clone());
        }
        variables.insert("GIT_CONFIG_COUNT".into(), (base + pairs.len()).to_string());
        Ok(variables)
    }

    /// The sha256 of `git archive --format=tar HEAD` (the script's `SOURCE_SHA256`), when the job
    /// runs in a git checkout; `None` for a plain source export.
    fn source_sha256(git: &OsStr, environment: &Environment) -> Result<Option<String>> {
        use sha2::{Digest, Sha256};
        if !Path::new(".git").exists() {
            return Ok(None);
        }
        let mut child = Command::new(git)
            .env_clear()
            .envs(environment)
            .args(["archive", "--format=tar", "HEAD"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| failure(format!("git archive: {error}")))?;
        let mut hasher = Sha256::new();
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| failure("git archive: no output"))?;
        let mut buffer = vec![0u8; 1 << 16];
        loop {
            let read = match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            hasher.update(&buffer[..read]);
        }
        let status = child.wait()?;
        if !status.success() {
            return Err(failure(format!("git archive HEAD failed ({status})")));
        }
        Ok(Some(format!("{:x}", hasher.finalize())))
    }

    fn prepend(path: &Path, rest: &OsStr) -> Result<String> {
        let mut entries = vec![path.to_owned()];
        entries.extend(std::env::split_paths(rest));
        text(Path::new(&std::env::join_paths(entries)?))
    }

    /// Set the runtime up for a job of `repository_url` whose process environment (with the
    /// job's own variables applied) is `environment`. Creates the state directories, takes the
    /// state lock, builds the tools and, with `[toolchain]`, takes the shared toolchain lock.
    pub fn prepare(
        runtime: &Runtime,
        environment: &Environment,
        repository_url: &str,
        exe: &Path,
        git: &OsStr,
        fleet_pairs: &[(String, String)],
    ) -> Result<Setup> {
        let (state, toolchains) = layout(environment, repository_url)?;
        fs::create_dir_all(&state)?;
        fs::create_dir_all(&toolchains)?;
        let timeout = timeout(environment)?;
        let Some(state_lock) = lock_state(&state, timeout)? else {
            return Ok(Setup::Refused(format!(
                "timed out after {} s waiting for the job state lock {}",
                timeout.as_secs(),
                state.join("lock").display()
            )));
        };
        let moon_home = state.join("moon-home");
        fs::create_dir_all(&moon_home)?;
        let tool_paths = build_tools(&runtime.config.tools, environment)?;
        let proto_home = toolchains.join("proto");
        let mut variables: BTreeMap<String, String> = BTreeMap::new();
        // PATH as the script built it: each tool's bin in front (the last one first), then the
        // directory of this ccid and proto's shims and bin.
        let mut path: OsString = get(environment, "PATH").unwrap_or_default().to_owned();
        for tool in &tool_paths {
            path = prepend(&tool.join("bin"), &path)?.into();
        }
        for entry in [proto_home.join("bin"), proto_home.join("shims")] {
            path = prepend(&entry, &path)?.into();
        }
        let exe_dir = exe
            .parent()
            .ok_or_else(|| failure("the ccid executable has no directory"))?;
        path = prepend(exe_dir, &path)?.into();
        variables.insert("PROTO_HOME".into(), text(&proto_home)?);
        variables.insert("MOON_HOME".into(), text(&moon_home)?);
        for (name, value) in [
            ("PROTO_AUTO_CLEAN", "false"),
            ("PROTO_TELEMETRY", "false"),
            ("PROTO_YES", "true"),
        ] {
            variables.insert(name.into(), value.into());
        }
        if runtime.toolchain.is_some() {
            let jobs = get(environment, "CI_JOBS")
                .map(|jobs| jobs.to_string_lossy().into_owned())
                .unwrap_or_else(|| "2".into());
            let test_threads = get(environment, "CI_TEST_THREADS")
                .map(|threads| threads.to_string_lossy().into_owned())
                .unwrap_or_else(|| "2".into());
            for (name, value) in [
                ("RUSTUP_INIT_SKIP_PATH_CHECK", "yes".to_owned()),
                ("RUSTUP_TOOLCHAIN", "stable".to_owned()),
                ("CI_JOBS", jobs.clone()),
                ("CARGO_BUILD_JOBS", jobs),
                ("RUST_TEST_THREADS", test_threads),
                ("CARGO_NET_GIT_FETCH_WITH_CLI", "true".to_owned()),
            ] {
                variables.insert(name.into(), value);
            }
        }
        variables.extend(git_config(environment, fleet_pairs)?);
        let mut toolchain_lock = None;
        if let Some(declared) = &runtime.toolchain {
            // The job's environment so far, with the Rust homes `ccid check` will use.
            let mut probe = environment.clone();
            for (name, value) in &variables {
                probe.insert(name.into(), value.into());
            }
            probe.insert("PATH".into(), path.clone());
            crate::cache::apply_canonical_rust_homes(&mut probe);
            let want = toolchain::Want {
                nightly: declared.nightly,
                targets: declared
                    .targets
                    .iter()
                    .map(|target| toolchain::parse_target(target))
                    .collect::<Result<_>>()?,
            };
            match toolchain::ensure(&toolchains.join("install.lock"), &want, timeout, &probe) {
                Ok(held) => toolchain_lock = Some(held),
                Err(error) => return Ok(Setup::Refused(error.to_string())),
            }
            if runtime.config.install_root {
                let root = state.join("install-bin");
                fs::create_dir_all(root.join("bin"))?;
                variables.insert("CARGO_INSTALL_ROOT".into(), text(&root)?);
                path = prepend(&root.join("bin"), &path)?.into();
                probe.insert("PATH".into(), path.clone());
            }
            let cargo = match toolchain::stable_cargo(&probe) {
                Ok(cargo) => cargo,
                Err(error) => return Ok(Setup::Refused(error.to_string())),
            };
            let directory = cargo
                .parent()
                .ok_or_else(|| failure("the stable cargo has no directory"))?;
            path = prepend(directory, &path)?.into();
        } else if runtime.config.install_root {
            let root = state.join("install-bin");
            fs::create_dir_all(root.join("bin"))?;
            variables.insert("CARGO_INSTALL_ROOT".into(), text(&root)?);
            path = prepend(&root.join("bin"), &path)?.into();
        }
        let artifacts = match get(environment, "ARTIFACT_ROOT") {
            Some(root) if Path::new(root).is_absolute() => PathBuf::from(root),
            Some(_) => return Err(failure("ARTIFACT_ROOT must be absolute")),
            None => state.join("release-artifacts"),
        };
        fs::create_dir_all(&artifacts)?;
        variables.insert("ARTIFACT_ROOT".into(), text(&artifacts)?);
        for (name, directory) in &runtime.config.state_env {
            let directory = state.join(directory);
            fs::create_dir_all(&directory)?;
            variables.insert(name.clone(), text(&directory)?);
        }
        if let Some(digest) = source_sha256(git, environment)? {
            variables.insert("SOURCE_SHA256".into(), digest);
        }
        variables.insert(
            "PATH".into(),
            path.into_string()
                .map_err(|_| failure("PATH is not valid UTF-8"))?,
        );
        eprintln!(
            "ccid job-run: runtime state {} (tools: {})",
            state.display(),
            BASE_TOOLS
                .iter()
                .copied()
                .chain(runtime.config.tools.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        );
        Ok(Setup::Ready(Prepared {
            environment: variables,
            state_lock,
            toolchain_lock,
        }))
    }
}

#[cfg(test)]
mod tests;

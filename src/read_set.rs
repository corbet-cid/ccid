//! `ccid measure-inputs`: a check's input set, measured from what the check actually touched.
//!
//! A check keyed on the whole tree reruns for every commit, including one that only changes a
//! README. `cache_inputs` narrows the key to the files a check reads, but a guessed list is a
//! false skip waiting to happen: a test that asserts a directory exists, a build script that
//! probes for an optional file, a harness that lists a directory. A false skip is worse than a
//! slow run, so the list is measured, never guessed:
//!
//! 1. **Trace.** The check runs uncached under `strace -ff -y -v`, with every compile cache off (a
//!    cache hit would hide the compiler's reads). Only the check's own processes count: those whose
//!    `execve` environment carries `CCID_TARGET_LOCK_HELD` (set by `ccid check` for every command it
//!    runs) and their descendants. ccid's own work around the check (keying, staging, the float) is
//!    not the check's input.
//! 2. **Observe.** Every path inside the checkout (or the staged copy `ccid check` runs it in) that a
//!    check process opened, stat'ed, listed, executed, changed into or probed and did not find. An
//!    absent file the check looked for is an input too: adding it changes what the check sees.
//! 3. **Generalise.** Each observation becomes the top-level entry it lies under (`--depth 1`): a
//!    file at the top is itself, anything below a directory is `<dir>/**`, an absent top-level name
//!    is both. New files in a directory the check already reads are therefore covered. Listing the
//!    repository root itself means every top-level entry matters: no narrowing then. `.ci/**`
//!    (ccid's own inputs, the check shell) is always included.
//! 4. **Verify.** The check runs again in a copy that holds only the matching files. If it fails
//!    there, the trace missed something (a read by a daemon outside the traced tree, for instance)
//!    and the measurement is refused (exit 1).
//!
//! The result is printed as a `cache_inputs` line for the check; nothing is written to the
//! repository. The key mechanism adds every lock and ecosystem manifest to a narrowed declaration
//! by itself.
use crate::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
};

/// The variable `ccid check` sets in the environment of every check command.
const MARKER: &str = "CCID_TARGET_LOCK_HELD";
/// Where `ccid check` stages a copy of the checkout below its target directory.
const STAGE: &str = ".ccid/source-v1";
/// Tool state inside a checkout, never a check input.
const TOOL_STATE: [&str; 3] = [".git", ".ccid", ".moon"];

/// How a path was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Seen {
    /// Opened, stat'ed, executed: it exists and was looked at.
    Present,
    /// Looked for and not found.
    Absent,
    /// Changed into, or opened as a directory.
    Directory,
    /// Listed: a directory whose entries matter.
    Listed,
}

/// Undo strace's C-style string escapes.
pub(crate) fn unescape(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte != b'\\' || index + 1 == bytes.len() {
            out.push(byte);
            index += 1;
            continue;
        }
        let next = bytes[index + 1];
        index += 2;
        match next {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'v' => out.push(0x0b),
            b'f' => out.push(0x0c),
            b'x' => {
                let hex = std::str::from_utf8(&bytes[index..(index + 2).min(bytes.len())])
                    .ok()
                    .and_then(|digits| u8::from_str_radix(digits, 16).ok());
                match hex {
                    Some(value) => {
                        out.push(value);
                        index += 2;
                    }
                    None => out.push(b'x'),
                }
            }
            b'0'..=b'7' => {
                let mut value = u32::from(next - b'0');
                let mut taken = 0;
                while taken < 2 && index < bytes.len() && (b'0'..=b'7').contains(&bytes[index]) {
                    value = value * 8 + u32::from(bytes[index] - b'0');
                    index += 1;
                    taken += 1;
                }
                out.push(value as u8);
            }
            other => out.push(other),
        }
    }
    out
}

/// The end of a quoted string that starts at `start` (an opening `"`): the index after the
/// closing quote.
fn quoted_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}

/// The top-level arguments of a syscall line's argument list (the text between the first `(`
/// and its matching `)`), split at commas outside quotes, brackets and braces.
fn arguments(line: &str) -> Option<(Vec<&str>, &str)> {
    let open = line.find('(')?;
    let bytes = line.as_bytes();
    let mut depth = 0usize;
    let mut start = open + 1;
    let mut index = open + 1;
    let mut parts = Vec::new();
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index = quoted_end(bytes, index)?;
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' if depth == 0 => {
                parts.push(line[start..index].trim());
                return Some((parts, &line[index + 1..]));
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(line[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// `"…"` as bytes.
fn string_argument(argument: &str) -> Option<Vec<u8>> {
    let inner = argument.strip_prefix('"')?;
    // A truncated string (`"…"...`) is not a path.
    let inner = inner.strip_suffix('"')?;
    Some(unescape(inner))
}

/// The path of a decorated descriptor (`AT_FDCWD</dir>`, `3</file>`). Descriptors that are not
/// files (`pipe:[…]`, `socket:[…]`, `anon_inode:[…]`) have no path.
fn decoration(argument: &str) -> Option<Vec<u8>> {
    let open = argument.find('<')?;
    let inner = argument[open + 1..].strip_suffix('>')?;
    inner.starts_with('/').then(|| unescape(inner))
}

/// `a` joined with `b` (absolute `b` wins) and `.`/`..` resolved lexically.
fn join(base: &Path, relative: &[u8]) -> PathBuf {
    let relative = Path::new(OsStr::from_bytes(relative));
    let joined = if relative.is_absolute() {
        relative.to_path_buf()
    } else {
        base.join(relative)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The result of a syscall line: `Some(false)` for a failed lookup (ENOENT, ENOTDIR), `Some(true)`
/// otherwise; `None` when the line has no result (unfinished, a signal).
fn outcome(rest: &str) -> Option<bool> {
    let result = rest.trim_start().strip_prefix('=')?.trim_start();
    Some(!(result.starts_with("-1 ENOENT") || result.starts_with("-1 ENOTDIR")))
}

/// The returned descriptor's decoration (`= 3</path>`).
fn returned_path(rest: &str) -> Option<Vec<u8>> {
    let result = rest.trim_start().strip_prefix('=')?.trim_start();
    let digits = result.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    decoration(result[digits..].split_whitespace().next()?)
}

/// One process's trace, as events in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    /// A child process was created.
    Clone(u32),
    /// A successful `execve`; `Some(stage)` when its environment carries the check marker.
    Exec(Option<PathBuf>),
    /// The current directory changed (relative: to the previous one).
    Cwd(PathBuf),
    /// A path observation (relative: to the current directory).
    Path(PathBuf, Seen),
}

/// The events of one per-process trace file (`strace -ff -y -v`).
pub(crate) fn events(text: &str) -> Vec<Event> {
    let mut out = Vec::new();
    let mut cwd: Option<PathBuf> = None;
    for line in text.lines() {
        let Some(name_end) = line.find('(') else {
            continue;
        };
        let name = &line[..name_end];
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            continue;
        }
        let Some((arguments, rest)) = arguments(line) else {
            continue;
        };
        let Some(succeeded) = outcome(rest) else {
            continue;
        };
        // Any AT_FDCWD decoration names this process's current directory exactly.
        for argument in &arguments {
            if argument.starts_with("AT_FDCWD<") {
                if let Some(directory) = decoration(argument) {
                    let directory = PathBuf::from(OsString::from_vec(directory));
                    if cwd.as_ref() != Some(&directory) {
                        out.push(Event::Cwd(directory.clone()));
                        cwd = Some(directory);
                    }
                }
            }
        }
        match name {
            "clone" | "clone3" | "fork" | "vfork" => {
                let result = rest.trim_start().strip_prefix('=').unwrap_or("").trim();
                if let Ok(child) = result
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .parse::<u32>()
                {
                    out.push(Event::Clone(child));
                }
                continue;
            }
            "getdents" | "getdents64" => {
                if let Some(directory) = arguments.first().and_then(|first| decoration(first)) {
                    out.push(Event::Path(
                        PathBuf::from(OsString::from_vec(directory)),
                        Seen::Listed,
                    ));
                }
                continue;
            }
            "fchdir" => {
                if succeeded {
                    if let Some(directory) = arguments.first().and_then(|first| decoration(first)) {
                        let directory = PathBuf::from(OsString::from_vec(directory));
                        out.push(Event::Path(directory.clone(), Seen::Directory));
                        out.push(Event::Cwd(directory.clone()));
                        cwd = Some(directory);
                    }
                }
                continue;
            }
            _ => {}
        }
        // The first path argument, relative to the decorated descriptor before it, else to the
        // current directory.
        let Some(position) = arguments
            .iter()
            .position(|argument| argument.starts_with('"'))
        else {
            continue;
        };
        let Some(raw) = string_argument(arguments[position]) else {
            continue;
        };
        let previous = position.checked_sub(1).map(|before| arguments[before]);
        let decorated = previous.and_then(decoration);
        // Relative to a descriptor that is no file (a pipe, a socket): not a path in the tree.
        if decorated.is_none() && previous.is_some_and(|before| before.contains('<')) {
            continue;
        }
        // An empty path (AT_EMPTY_PATH) is the descriptor itself, which must then be a file.
        if raw.is_empty() && decorated.is_none() {
            continue;
        }
        let base = decorated
            .map(|bytes| PathBuf::from(OsString::from_vec(bytes)))
            .or_else(|| cwd.clone());
        let path = match &base {
            Some(base) => join(base, &raw),
            None if raw.first() == Some(&b'/') => join(Path::new("/"), &raw),
            // Relative before this process named its directory: it stays relative here and is
            // placed against the directory it inherited (`check_observations`), never dropped.
            None => PathBuf::from(OsString::from_vec(raw.clone())),
        };
        let seen = if !succeeded {
            Seen::Absent
        } else if name == "chdir" || arguments.iter().any(|a| a.contains("O_DIRECTORY")) {
            Seen::Directory
        } else {
            Seen::Present
        };
        if name == "chdir" && succeeded {
            out.push(Event::Path(path.clone(), seen));
            out.push(Event::Cwd(path.clone()));
            cwd = Some(path);
            continue;
        }
        if name == "execve" || name == "execveat" {
            if succeeded {
                let stage = arguments
                    .get(position + 2)
                    .and_then(|environment| marker_stage(environment));
                out.push(Event::Path(path, Seen::Present));
                out.push(Event::Exec(stage));
            }
            continue;
        }
        out.push(Event::Path(path, seen));
        if let Some(resolved) = returned_path(rest) {
            out.push(Event::Path(
                PathBuf::from(OsString::from_vec(resolved)),
                Seen::Present,
            ));
        }
    }
    out
}

/// The stage root named by the check marker in an `execve` environment array, if present.
fn marker_stage(environment: &str) -> Option<PathBuf> {
    let prefix = format!("\"{MARKER}=");
    let start = environment.find(&prefix)?;
    let end = quoted_end(environment.as_bytes(), start)?;
    let entry = unescape(&environment[start + 1..end - 1]);
    let value = entry.splitn(2, |b| *b == b'=').nth(1)?.to_vec();
    Some(PathBuf::from(OsString::from_vec(value)).join(STAGE))
}

/// The observations of the check's own processes, from per-process traces keyed by pid. The
/// initial process starts in `start_cwd`, outside the check.
pub(crate) fn check_observations(
    traces: &BTreeMap<u32, Vec<Event>>,
    start_cwd: &Path,
) -> (Vec<(PathBuf, Seen)>, BTreeSet<PathBuf>) {
    let children: BTreeSet<u32> = traces
        .values()
        .flatten()
        .filter_map(|event| match event {
            Event::Clone(child) => Some(*child),
            _ => None,
        })
        .collect();
    let mut observations = Vec::new();
    let mut stages = BTreeSet::new();
    // (pid, cwd, inside the check)
    let mut pending: Vec<(u32, PathBuf, bool)> = traces
        .keys()
        .filter(|pid| !children.contains(pid))
        .map(|pid| (*pid, start_cwd.to_path_buf(), false))
        .collect();
    let mut visited = BTreeSet::new();
    while let Some((pid, mut cwd, mut inside)) = pending.pop() {
        if !visited.insert(pid) {
            continue;
        }
        let Some(events) = traces.get(&pid) else {
            continue;
        };
        for event in events {
            match event {
                Event::Clone(child) => pending.push((*child, cwd.clone(), inside)),
                Event::Exec(stage) => {
                    if let Some(stage) = stage {
                        inside = true;
                        stages.insert(stage.clone());
                    }
                }
                Event::Cwd(directory) => cwd = join(&cwd, directory.as_os_str().as_bytes()),
                Event::Path(path, seen) => {
                    if inside {
                        observations.push((join(&cwd, path.as_os_str().as_bytes()), *seen));
                    }
                }
            }
        }
    }
    (observations, stages)
}

/// What a check's inputs generalise to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Inputs {
    /// The check enumerates the repository root: every top-level entry matters.
    Everything,
    /// Git glob pathspecs, sorted.
    Globs(BTreeSet<String>),
}

/// A literal path component as a git glob (metacharacters escaped).
fn glob_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

/// The repository-relative form of `path` under one of `roots`, `None` when outside all of them.
fn relative_to<'a>(path: &'a Path, roots: &[PathBuf]) -> Option<&'a Path> {
    roots.iter().find_map(|root| path.strip_prefix(root).ok())
}

/// Generalise observations to git globs at `depth` path components.
pub(crate) fn generalise(
    observations: &[(PathBuf, Seen)],
    roots: &[PathBuf],
    depth: usize,
) -> Result<Inputs> {
    let depth = depth.max(1);
    let mut globs = BTreeSet::from([".ci/**".to_owned()]);
    let mut directories = BTreeSet::new();
    let mut files: BTreeMap<String, BTreeSet<Seen>> = BTreeMap::new();
    for (path, seen) in observations {
        let Some(relative) = relative_to(path, roots) else {
            continue;
        };
        let components: Vec<String> = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect();
        let Some(first) = components.first() else {
            // Listing the root itself: every top-level entry is an input.
            if *seen == Seen::Listed {
                return Ok(Inputs::Everything);
            }
            continue;
        };
        if TOOL_STATE.contains(&first.as_str()) {
            continue;
        }
        let head: Vec<String> = components
            .iter()
            .take(depth)
            .map(|c| glob_literal(c))
            .collect();
        let head = head.join("/");
        if components.len() > depth {
            directories.insert(head);
        } else {
            files.entry(head).or_default().insert(*seen);
        }
    }
    for (path, kinds) in files {
        if kinds.contains(&Seen::Directory)
            || kinds.contains(&Seen::Listed)
            || directories.contains(&path)
        {
            directories.insert(path);
            continue;
        }
        if kinds.contains(&Seen::Absent) {
            globs.insert(format!("{path}/**"));
        }
        globs.insert(path);
    }
    for directory in directories {
        globs.insert(format!("{directory}/**"));
    }
    Ok(Inputs::Globs(globs))
}

#[cfg(unix)]
pub use run::{run, Options};

#[cfg(unix)]
mod run {
    use super::{check_observations, events, generalise, Inputs, MARKER};
    use crate::{failure, Environment, Result};
    use serde_json::json;
    use std::{
        collections::BTreeMap,
        ffi::OsStr,
        fs,
        path::{Path, PathBuf},
        process::{Command, Stdio},
    };

    /// `ccid measure-inputs`.
    #[derive(Debug, Clone)]
    pub struct Options {
        pub repo: PathBuf,
        pub check: String,
        pub depth: usize,
        pub strace: PathBuf,
        pub verify: bool,
    }

    /// Variables that would route the measured run through a cache or mark it as nested.
    const CACHES: [&str; 8] = [
        "CCID_RESULT_CACHE",
        "CCID_REMOTE_CACHE",
        "CCID_COMPILE_CACHE_CONFIG",
        "RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        "CCID_CACHE_CHILD",
    ];

    fn check_command(exe: &Path, repo: &Path, check: &str) -> Command {
        let mut command = Command::new(exe);
        command
            .args(["check", "--repo"])
            .arg(repo)
            .args(["--check", check]);
        command
    }

    fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let output = Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(repo)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| failure(format!("git: {error}")))?;
        if !output.status.success() {
            return Err(failure(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }

    /// Copy the files of `repo` that `globs` select (git's own pathspec matcher, the one the key
    /// uses) into `into`, and commit them there.
    fn stripped_copy(repo: &Path, globs: &[String], into: &Path) -> Result<usize> {
        let mut args = vec![
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
        ];
        let specs: Vec<String> = globs.iter().map(|glob| format!(":(glob){glob}")).collect();
        args.extend(specs.iter().map(String::as_str));
        let listing = git(repo, &args)?;
        let mut copied = 0;
        for name in listing
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            let relative = Path::new(<OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(name));
            let source = repo.join(relative);
            let destination = into.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let meta = fs::symlink_metadata(&source)?;
            if meta.file_type().is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(&source)?, &destination)?;
            } else if meta.is_file() {
                fs::copy(&source, &destination)?;
            } else {
                continue;
            }
            copied += 1;
        }
        let identity = [
            "-c",
            "user.name=measure-inputs",
            "-c",
            "user.email=measure-inputs@invalid",
            "-c",
            "commit.gpgsign=false",
        ];
        git(into, &["init", "-q"])?;
        git(into, &["add", "-A"])?;
        let mut commit = identity.to_vec();
        commit.extend(["commit", "-q", "--allow-empty", "-m", "measured inputs"]);
        git(into, &commit)?;
        Ok(copied)
    }

    /// Measure, generalise and verify one check's inputs; print one JSON line.
    pub fn run(options: &Options) -> Result<u8> {
        let mut environment: Environment = std::env::vars_os().collect();
        if environment.contains_key(OsStr::new(MARKER)) {
            return Err(failure(
                "measure-inputs runs a check itself; it never runs inside a ccid check",
            ));
        }
        for name in CACHES {
            environment.remove(OsStr::new(name));
        }
        let root = options.repo.canonicalize()?;
        let (manifest, _) = crate::load_manifest(&root, Path::new(".ci/ccid.toml"))?;
        let declared = manifest
            .checks
            .get(&options.check)
            .ok_or_else(|| failure(format!("Unknown check: {}", options.check)))?;
        // What the check writes and declares as its outputs is not an input.
        let outputs: Vec<String> = declared.cache_outputs.clone();
        if !root.join(".git").exists() {
            return Err(failure(
                "measure-inputs needs a git checkout (the stripped copy is selected by git)",
            ));
        }
        let exe = std::env::current_exe()?;
        let scratch = crate::cache::quiet_scratch(&mut environment.clone())?;
        let traces = scratch.path().join("trace");
        fs::create_dir(&traces)?;
        let status = Command::new(&options.strace)
            .args(["-ff", "-qq", "-y", "-v", "-s", "65535", "-o"])
            .arg(traces.join("t"))
            .args([
                "-e",
                "trace=%file,getdents,getdents64,fchdir,clone,clone3,fork,vfork",
                "--",
            ])
            .arg(&exe)
            .args(["check", "--repo"])
            .arg(&root)
            .args(["--check", &options.check])
            .current_dir(&root)
            .env_clear()
            .envs(&environment)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| {
                failure(format!(
                    "cannot run {} (strace is required to measure): {error}",
                    options.strace.display()
                ))
            })?;
        if !status.success() {
            println!(
                "{}",
                json!({"check": options.check, "measured": false, "reason": format!("the check fails on the full tree ({status}); measure a passing check")})
            );
            return Ok(1);
        }
        let mut parsed = BTreeMap::new();
        for entry in fs::read_dir(&traces)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(pid) = name
                .strip_prefix("t.")
                .and_then(|pid| pid.parse::<u32>().ok())
            else {
                continue;
            };
            let text = String::from_utf8_lossy(&fs::read(entry.path())?).into_owned();
            parsed.insert(pid, events(&text));
        }
        let (observations, stages) = check_observations(&parsed, &root);
        if observations.is_empty() {
            return Err(failure(format!(
                "no process of check {} was traced (no {MARKER} in any execve environment)",
                options.check
            )));
        }
        let mut roots = vec![root.clone()];
        roots.extend(stages.iter().cloned());
        let inputs = generalise(&observations, &roots, options.depth)?;
        let globs: Vec<String> = match &inputs {
            Inputs::Everything => Vec::new(),
            Inputs::Globs(globs) => globs
                .iter()
                .filter(|glob| {
                    !outputs.iter().any(|output| {
                        glob.as_str() == output || glob.as_str() == format!("{output}/**")
                    })
                })
                .cloned()
                .collect(),
        };
        let mut verified = None;
        let mut copied = None;
        if options.verify && !globs.is_empty() {
            let stripped = scratch.path().join("stripped");
            fs::create_dir(&stripped)?;
            copied = Some(stripped_copy(&root, &globs, &stripped)?);
            let status = check_command(&exe, &stripped, &options.check)
                .current_dir(&stripped)
                .env_clear()
                .envs(&environment)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()?;
            verified = Some(status.success());
        }
        // The provenance that lets the verdict key narrow to these inputs. It binds them to the
        // check as declared now and to the tree's top-level layout, and names this ccid.
        let measured = if globs.is_empty() {
            None
        } else {
            Some(crate::cached_verdict::measured_provenance(
                &root, declared, &globs,
            )?)
        };
        let line = json!({
            "check": options.check,
            "measured": true,
            "observations": observations.len(),
            "narrowed": !globs.is_empty(),
            "cache_inputs": if globs.is_empty() { serde_json::Value::Null } else { json!(globs) },
            "cache_inputs_measured": json!(measured),
            "stripped_files": copied,
            "verified": verified,
            "manifest": match &measured {
                None => "the check lists the repository root: keep the whole tree (no cache_inputs)".to_owned(),
                Some(measured) => format!(
                    "cache_inputs = {}\ncache_inputs_measured = {{ tool = {}, commands = {}, inputs = {}, layout = {}, date = {} }}",
                    serde_json::to_string(&globs)?,
                    serde_json::to_string(&measured.tool)?,
                    serde_json::to_string(&measured.commands)?,
                    serde_json::to_string(&measured.inputs)?,
                    serde_json::to_string(&measured.layout)?,
                    serde_json::to_string(&measured.date)?,
                ),
            },
        });
        println!("{line}");
        Ok(if verified == Some(false) { 1 } else { 0 })
    }
}

#[cfg(test)]
mod tests;

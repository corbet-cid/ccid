//! Exact-source submission and compatibility with existing scheduler receipts.
#![forbid(unsafe_code)]

mod adapter;
mod archive;
mod cli;
mod core;
mod github;
mod jobs;
mod pinned;
mod probe;
mod routing;
mod submit;
mod transport;

use crate::Result;
pub use cli::{run, Action, JobAction};
pub use jobs::run_cli as run_job;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub api: String,
    pub token_command: Vec<String>,
    pub ssh: Vec<String>,
    pub state_root: PathBuf,
    pub host_sources: String,
    pub worker_sources: String,
    pub host_tools: String,
    pub worker_tools: String,
    pub remote_binary: String,
    pub tool_repo: PathBuf,
    pub tool_origins: Vec<String>,
    #[serde(default)]
    pub origin_aliases: BTreeMap<String, String>,
    pub argo_namespace: String,
    pub argo_template: String,
    pub github_tool_repository: Option<String>,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("CCID_SUBMISSION_CONFIG").map(PathBuf::from))
            .or_else(|| {
                let argument = PathBuf::from(std::env::args_os().next()?);
                let executable = if argument.components().count() > 1 {
                    Some(argument)
                } else {
                    std::env::var_os("PATH").and_then(|paths| {
                        std::env::split_paths(&paths)
                            .map(|p| p.join(&argument))
                            .find(|p| p.is_file())
                    })
                }?;
                let config = executable.parent()?.join("submission.json");
                config.is_file().then_some(config)
            })
            .or_else(|| {
                let directory = std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config"))
                    })?;
                let path = directory.join("ccid/submission.json");
                path.is_file().then_some(path)
            })
            .ok_or("Set CCID_SUBMISSION_CONFIG to the declared submission configuration")?;
        let config: Self = serde_json::from_slice(&fs::read(path)?)?;
        let api = url::Url::parse(&config.api)?;
        if api.scheme() != "https"
            || !api.username().is_empty()
            || api.password().is_some()
            || api.query().is_some()
            || api.fragment().is_some()
        {
            return Err("Submission API must be an uncredentialed HTTPS endpoint".into());
        }
        if config.ssh.is_empty()
            || config.token_command.is_empty()
            || !config.state_root.is_absolute()
        {
            return Err(
                "Submission configuration requires transport commands and an absolute state root"
                    .into(),
            );
        }
        Ok(config)
    }
    fn directory(&self, name: &str) -> Result<PathBuf> {
        let root = self.state_root.join(name);
        fs::create_dir_all(&root)?;
        Ok(root)
    }
    fn origin(&self, repo: &Path) -> Result<String> {
        core::canonical_remote(
            &git(repo, &["config", "--get", "remote.origin.url"])?,
            &self.origin_aliases,
        )
    }
    fn ssh(&self, args: &[String], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut command = self.ssh.clone();
        command.push(args.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" "));
        output(&command, None, input)
    }
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or("").to_string()
}
fn number(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}
fn rows(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn sha(data: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(data.as_ref()))
}
fn digest(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn matches(pattern: &str, value: &str) -> bool {
    regex::Regex::new(pattern)
        .expect("static submission pattern")
        .is_match(value)
}
fn exact_sha(value: &str) -> bool {
    matches(r"^[0-9a-f]{40}$", value)
}
fn exact_digest(value: &str) -> bool {
    matches(r"^[0-9a-f]{64}$", value)
}
fn name(value: &str) -> bool {
    matches(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$", value)
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
fn emit(value: &Value) -> Result<()> {
    println!("{}", encode(value)?);
    Ok(())
}

// Python's canonical JSON uses ensure_ascii=True. This matters for request hashes.
fn encode(value: &Value) -> Result<String> {
    let raw = serde_json::to_string(value)?;
    let mut encoded = String::new();
    for c in raw.chars() {
        if c.is_ascii() {
            encoded.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]).iter() {
                encoded.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    Ok(encoded)
}
fn save(path: &Path, value: &Value) -> Result<()> {
    let parent = path.parent().ok_or("State needs a parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(file, "{}", encode(value)?)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
struct RequestLock(File);
impl Drop for RequestLock {
    fn drop(&mut self) {
        // A concurrently spawning child can briefly inherit this description
        // before exec closes it. Release ownership explicitly at scope exit.
        #[cfg(unix)]
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}
fn lock(path: &Path) -> Result<RequestLock> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    #[cfg(unix)]
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| "Another submission owns this request")?;
    #[cfg(not(unix))]
    return Err("Submission locking requires Unix".into());
    Ok(RequestLock(file))
}
fn output(argv: &[String], cwd: Option<&Path>, input: Option<&[u8]>) -> Result<Vec<u8>> {
    let (program, args) = argv.split_first().ok_or("Empty command")?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if Path::new(program).file_name().is_some_and(|p| p == "git") {
        command
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
    }
    let mut child = command.spawn()?;
    // Feed concurrently so a child producing output while consuming input cannot deadlock.
    let result = std::thread::scope(|scope| {
        let writer = input.map(|bytes| {
            let mut stdin = child.stdin.take().expect("piped input");
            scope.spawn(move || stdin.write_all(bytes))
        });
        let result = child.wait_with_output();
        if let Some(writer) = writer {
            writer.join().map_err(|_| "Input writer panicked")??;
        }
        Result::Ok(result?)
    })?;
    if !result.status.success() {
        return Err(format!(
            "{} failed (exit {:?})",
            Path::new(program)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            result.status.code()
        )
        .into());
    }
    Ok(result.stdout)
}
fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}
fn git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut argv = strings(&["git", "--no-replace-objects", "--no-lazy-fetch"]);
    argv.extend(strings(args));
    output(&argv, Some(repo), None)
}
fn git(repo: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git_bytes(repo, args)?)?.trim().to_owned())
}

#[cfg(test)]
mod tests;

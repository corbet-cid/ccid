//! Persist native TypeScript no-emit incremental state across job workspaces.
//! State is advisory: tsc always executes and owns dependency validation.
use crate::{event, failure, value, Environment, Result, Runner};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

const CONFIG: &str = "CCID_TYPESCRIPT_CACHE_CONFIG";

#[derive(Serialize, Deserialize)]
struct Config {
    root: PathBuf,
    cache: PathBuf,
    compiler: PathBuf,
    node: PathBuf,
}

pub(crate) struct TypeScriptCache {
    original: PathBuf,
    alias: PathBuf,
    _directory: tempfile::TempDir,
}

impl Drop for TypeScriptCache {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.alias);
        let _ = fs::rename(&self.original, &self.alias);
    }
}

impl TypeScriptCache {
    pub(crate) fn prepare(runner: &mut Runner) -> Option<Self> {
        match Self::try_prepare(&runner.root, &mut runner.environment) {
            Ok(guard) => guard,
            Err(error) => {
                event(
                    json!({"event":"compile-cache-bypass","language":"typescript","reason":error.to_string()}),
                );
                None
            }
        }
    }

    fn try_prepare(root: &Path, environment: &mut Environment) -> Result<Option<Self>> {
        let Some(cache) = value(environment, "TSC_CACHE_DIR").map(PathBuf::from) else {
            return Ok(None);
        };
        if !cache.is_absolute() || !cache.is_dir() {
            return Err(failure("TypeScript cache storage is not provisioned"));
        }
        let compiler = root.join("node_modules/typescript/bin/tsc");
        let alias = root.join("node_modules/.bin/tsc");
        if !compiler.is_file() || !alias.is_file() {
            return Ok(None);
        }
        let node = crate::compile_cache::executable(environment, "node")
            .ok_or_else(|| failure("TypeScript requires its selected node runtime"))?;
        #[cfg(not(unix))]
        return Ok(None);
        #[cfg(unix)]
        {
            // Place the backup beside the launcher: renaming cannot cross mounts
            // and never modifies a pnpm/store symlink target.
            let directory = tempfile::Builder::new().prefix(".ccid-tsc-").tempdir_in(
                alias
                    .parent()
                    .ok_or_else(|| failure("Missing launcher parent"))?,
            )?;
            let original = directory.path().join("original");
            let file = directory.path().join("config.json");
            let config = Config {
                root: root.to_owned(),
                cache,
                compiler,
                node,
            };
            fs::write(&file, serde_json::to_vec(&config)?)?;
            let binary = std::env::current_exe()?;
            fs::rename(&alias, &original)?;
            let guard = Self {
                original,
                alias,
                _directory: directory,
            };
            std::os::unix::fs::symlink(binary, &guard.alias)?;
            environment.insert(CONFIG.into(), file.into_os_string());
            Ok(Some(guard))
        }
    }
}

fn eligible(arguments: &[OsString]) -> bool {
    let args: Option<Vec<_>> = arguments.iter().map(|arg| arg.to_str()).collect();
    let Some(args) = args else { return false };
    args.contains(&"--noEmit")
        && !args.windows(2).any(|p| p == ["--noEmit", "false"])
        && !args.iter().any(|arg| {
            arg.starts_with('@')
                || [
                    "--build",
                    "-b",
                    "--watch",
                    "-w",
                    "--incremental",
                    "-i",
                    "--tsBuildInfoFile",
                    "--composite",
                    "--help",
                    "-h",
                    "--version",
                    "-v",
                ]
                .contains(arg)
        })
}

fn invoke(config: &Config, args: &[OsString]) -> Result<std::process::ExitStatus> {
    Ok(Command::new(&config.node)
        .arg(&config.compiler)
        .args(args)
        .status()?)
}

fn run(config: &Config, arguments: Vec<OsString>) -> Result<ExitCode> {
    let status = if !eligible(&arguments) {
        invoke(config, &arguments)?
    } else {
        // Preparation errors are cache bypasses. Once tsc starts, never retry it.
        match prepare_state(config, &arguments) {
            Err(error) => {
                eprintln!(
                    "{}",
                    json!({"event":"compile-cache-bypass","language":"typescript","reason":error.to_string()})
                );
                invoke(config, &arguments)?
            }
            Ok((_lock, state, destination, restored)) => {
                let mut args = arguments;
                args.extend([
                    OsString::from("--incremental"),
                    "--tsBuildInfoFile".into(),
                    state.clone().into_os_string(),
                ]);
                let status = invoke(config, &args)?;
                let mut published = false;
                if status.success() && state.is_file() {
                    let persist = (|| -> Result<()> {
                        let temporary = tempfile::NamedTempFile::new_in(
                            destination
                                .parent()
                                .ok_or_else(|| failure("Missing cache parent"))?,
                        )?;
                        fs::copy(&state, temporary.path())?;
                        temporary.persist(&destination)?;
                        Ok(())
                    })();
                    published = persist.is_ok();
                    if let Err(error) = persist {
                        eprintln!("TypeScript cache write unavailable: {error}");
                    }
                }
                eprintln!(
                    "{}",
                    json!({"event":"compile-cache","language":"typescript","backend":"tsc-incremental","state_restored":restored,"state_published":published,"native_hit_rate":null})
                );
                status
            }
        }
    };
    Ok(ExitCode::from(
        status
            .code()
            .and_then(|c| u8::try_from(c).ok())
            .unwrap_or(1),
    ))
}

fn prepare_state(
    config: &Config,
    arguments: &[OsString],
) -> Result<(fs::File, PathBuf, PathBuf, bool)> {
    let cwd = std::env::current_dir()?;
    let relative = cwd.strip_prefix(&config.root)?;
    let args: Vec<_> = arguments
        .iter()
        .map(|arg| {
            arg.to_str()
                .ok_or_else(|| failure("Non-UTF8 TypeScript argument"))
        })
        .collect::<Result<_>>()?;
    if args.iter().any(|arg| {
        Path::new(arg).is_absolute() || arg.contains(config.root.to_string_lossy().as_ref())
    }) {
        return Err(failure(
            "Absolute job paths in TypeScript arguments are not portable",
        ));
    }
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(&(1, relative, &args))?);
    // Partition advisory state by content, never repository URL or job path.
    for name in [
        "package.json",
        "package-lock.json",
        "pnpm-lock.yaml",
        "bun.lock",
        "bun.lockb",
        "yarn.lock",
        "tsconfig.json",
    ] {
        let file = config.root.join(name);
        if file.is_file() {
            hash.update(name.as_bytes());
            hash.update(fs::read(file)?);
        }
    }
    // Compiler version alone is insufficient for patched or local compilers.
    hash.update(fs::read(&config.node)?);
    hash.update(fs::read(&config.compiler)?);
    let library = config.root.join("node_modules/typescript/lib");
    for name in ["typescript.js", "tsc.js", "_tsc.js"] {
        let file = library.join(name);
        if file.is_file() {
            hash.update(name.as_bytes());
            hash.update(fs::read(file)?);
        }
    }
    let key = format!("{:x}", hash.finalize());
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(config.cache.join(format!("{key}.lock")))?;
    lock.lock()?;
    let state = config
        .root
        .join(".ccid/typescript")
        .join(&key)
        .join("state.tsbuildinfo");
    fs::create_dir_all(
        state
            .parent()
            .ok_or_else(|| failure("Missing state parent"))?,
    )?;
    let destination = config.cache.join(format!("{key}.tsbuildinfo"));
    let restored = destination.is_file();
    if restored {
        fs::copy(&destination, &state)?;
    } else if state.is_file() {
        fs::remove_file(&state)?;
    }
    Ok((lock, state, destination, restored))
}

pub fn dispatch() -> Option<Result<ExitCode>> {
    let name = std::env::args_os()
        .next()
        .and_then(|p| PathBuf::from(p).file_name().map(|s| s.to_owned()))?;
    if name != "tsc" {
        return None;
    }
    let config = std::env::var_os(CONFIG)?;
    Some((|| {
        let config: Config = serde_json::from_slice(&fs::read(config)?)?;
        run(&config, std::env::args_os().skip(1).collect())
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn emitting_or_user_managed_incremental_builds_are_not_overridden() {
        for args in [
            vec!["--build"],
            vec!["--noEmit", "false"],
            vec!["--noEmit", "--incremental", "false"],
            vec!["--noEmit", "@args"],
            vec!["--noEmit", "--tsBuildInfoFile", "owned"],
        ] {
            assert!(!eligible(
                &args.into_iter().map(OsString::from).collect::<Vec<_>>()
            ));
        }
        assert!(eligible(&[
            "--noEmit".into(),
            "--project".into(),
            "tsconfig.json".into()
        ]));
    }
}

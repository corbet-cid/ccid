//! Native compiler caches, activated only when provisioned by the executor.
//! The compiler still validates every input; this module never caches an exit code.
use crate::{event, failure, value, Environment, Result, Runner};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::Duration,
};

const CONFIG: &str = "CCID_COMPILE_CACHE_CONFIG";
/// Nesting depth of compiler aliases; a configuration cycle fails instead of
/// forking forever.
const DEPTH: &str = "CCID_COMPILE_CACHE_DEPTH";
const MAX_DEPTH: u32 = 8;

fn next_depth(current: Option<&std::ffi::OsStr>) -> Result<u32> {
    let depth = current
        .and_then(|d| d.to_str())
        .and_then(|d| d.parse::<u32>().ok())
        .unwrap_or(0);
    if depth >= MAX_DEPTH {
        return Err(failure("Compiler alias recursion detected"));
    }
    Ok(depth + 1)
}
const COMPILERS: &[&str] = &["cc", "c++", "gcc", "g++", "clang", "clang++"];

/// Go's native cache remains responsible for source, toolchain and flag keys.
/// Its portable build mode removes the transient source directory from objects.
pub(crate) fn prepare_go(environment: &mut Environment) {
    let flags = value(environment, "GOFLAGS").unwrap_or_default();
    // An explicit caller setting (including false) takes precedence.
    if !flags
        .split_whitespace()
        .any(|flag| flag == "-trimpath" || flag.starts_with("-trimpath="))
    {
        environment.insert("GOFLAGS".into(), format!("{flags} -trimpath").trim().into());
    }
}

#[derive(Serialize, Deserialize)]
struct Config {
    root: PathBuf,
    cache: Option<PathBuf>,
    compilers: BTreeMap<String, PathBuf>,
}

/// A directory of job-owned compiler aliases (symlinks to a ccid binary).
fn alias_directory(directory: &Path) -> bool {
    directory
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("ccid-compiler-"))
}

/// The first executable named `name` on PATH. Compiler aliases of an enclosing
/// ccid are skipped: a nested check must find the real compiler, never the
/// outer job's alias, which would dispatch back to itself through the nested
/// configuration forever.
pub(crate) fn executable(environment: &Environment, name: &str) -> Option<PathBuf> {
    let path = environment.get(std::ffi::OsStr::new("PATH"))?;
    std::env::split_paths(path)
        .filter(|directory| !alias_directory(directory))
        .map(|p| p.join(name))
        .find(|p| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                p.is_file()
            }
        })
}

/// The owned directory lives until the checked process tree has terminated.
pub(crate) struct CompilerCache {
    _directory: tempfile::TempDir,
}

impl CompilerCache {
    pub(crate) fn prepare(root: &Path, environment: &mut Environment) -> Option<Self> {
        match Self::try_prepare(root, environment) {
            Ok(guard) => guard,
            Err(error) => {
                event(
                    json!({"event":"compile-cache-bypass","language":"c/c++","reason":error.to_string()}),
                );
                None
            }
        }
    }

    fn try_prepare(root: &Path, environment: &mut Environment) -> Result<Option<Self>> {
        let cache = value(environment, "CCACHE_DIR")
            .filter(|root| Path::new(root).is_absolute() && Path::new(root).is_dir())
            .and_then(|_| executable(environment, "ccache"));
        let probe = Runner::new(root.to_owned(), environment.clone(), Duration::from_secs(5))?;
        let mut identities = BTreeMap::new();
        let compilers: BTreeMap<_, _> = COMPILERS
            .iter()
            .filter_map(|name| {
                let path = executable(environment, name)?;
                let canonical = path.canonicalize().ok()?;
                let supported = *identities.entry(canonical).or_insert_with(|| {
                    probe
                        .run(
                            &[path.to_string_lossy().into_owned(), "--version".into()],
                            true,
                        )
                        .is_ok_and(|version| {
                            version.contains("clang")
                                || version.contains("Free Software Foundation")
                        })
                });
                supported.then(|| ((*name).to_owned(), path))
            })
            .collect();
        if compilers.is_empty() {
            return Ok(None);
        }
        #[cfg(not(unix))]
        return Ok(None);
        #[cfg(unix)]
        {
            let directory = tempfile::Builder::new()
                .prefix("ccid-compiler-")
                .tempdir_in(
                    value(environment, "TMPDIR")
                        .map(PathBuf::from)
                        .unwrap_or_else(std::env::temp_dir),
                )?;
            let config = Config {
                root: root.to_owned(),
                cache,
                compilers,
            };
            let file = directory.path().join("config.json");
            fs::write(&file, serde_json::to_vec(&config)?)?;
            let binary = std::env::current_exe()?;
            for name in config.compilers.keys() {
                std::os::unix::fs::symlink(&binary, directory.path().join(name))?;
            }
            let mut paths = vec![directory.path().to_owned()];
            paths.extend(std::env::split_paths(
                environment
                    .get(std::ffi::OsStr::new("PATH"))
                    .ok_or_else(|| failure("Missing compiler PATH"))?,
            ));
            let path = std::env::join_paths(paths)?;
            // Mutate the execution environment only after preparation succeeds.
            environment.insert("PATH".into(), path);
            environment.insert(CONFIG.into(), file.into_os_string());
            environment.insert("CCACHE_BASEDIR".into(), root.as_os_str().to_owned());
            environment.insert("CCACHE_COMPILERCHECK".into(), "content".into());
            event(
                json!({"event":"compile-cache","language":"c/c++","backend":config.cache.as_ref().map(|_| "ccache"),"path_remapping":true,"compilers":config.compilers.keys().collect::<Vec<_>>() }),
            );
            Ok(Some(Self {
                _directory: directory,
            }))
        }
    }
}

fn remapped_arguments(root: &Path, arguments: impl Iterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<_> = arguments.collect();
    // GCC and Clang understand these for __FILE__ and DWARF, including -g builds.
    // Keep hash_dir enabled: ccache must still detect other unremapped paths.
    args.push(format!("-ffile-prefix-map={}=/source", root.display()).into());
    args.push(format!("-fdebug-prefix-map={}=/source", root.display()).into());
    args
}

/// Invoked through job-owned compiler symlinks before ordinary CLI parsing.
pub fn dispatch() -> Option<Result<ExitCode>> {
    let name = std::env::args_os()
        .next()
        .and_then(|p| PathBuf::from(p).file_name().map(|s| s.to_owned()))?;
    let name = name.to_str()?;
    if !COMPILERS.contains(&name) {
        return None;
    }
    let config = std::env::var_os(CONFIG)?;
    Some((|| {
        let depth = next_depth(std::env::var_os(DEPTH).as_deref())?;
        let config: Config = serde_json::from_slice(&fs::read(config)?)?;
        let compiler = config
            .compilers
            .get(name)
            .ok_or_else(|| failure("Undeclared compiler alias"))?;
        // Path semantics are identical with and without the optional cache.
        let Some(cache) = &config.cache else {
            let status = Command::new(compiler)
                .env(DEPTH, depth.to_string())
                .args(remapped_arguments(
                    &config.root,
                    std::env::args_os().skip(1),
                ))
                .status()?;
            return Ok(ExitCode::from(
                status
                    .code()
                    .and_then(|code| u8::try_from(code).ok())
                    .unwrap_or(1),
            ));
        };
        let log = tempfile::NamedTempFile::new().ok();
        let mut command = Command::new(cache);
        command.env(DEPTH, depth.to_string());
        command.arg(compiler).args(remapped_arguments(
            &config.root,
            std::env::args_os().skip(1),
        ));
        if let Some(log) = &log {
            command.env("CCACHE_STATSLOG", log.path());
        }
        let status = command.status()?;
        let counters = log.and_then(|log| {
            Command::new(cache)
                .args(["--print-log-stats", "--format=json"])
                .env("CCACHE_STATSLOG", log.path())
                .output()
                .ok()
                .filter(|out| out.status.success())
                .and_then(|out| serde_json::from_slice::<serde_json::Value>(&out.stdout).ok())
        });
        let hits = counters.as_ref().and_then(|c| {
            Some(c["direct_cache_hit"].as_u64()? + c["preprocessed_cache_hit"].as_u64()?)
        });
        let requests = counters
            .as_ref()
            .and_then(|c| Some(hits? + c["cache_miss"].as_u64()?));
        eprintln!(
            "{}",
            json!({"event":"compile-cache","language":"c/c++","backend":"ccache","hits":hits,"requests":requests,"hit_rate":hits.zip(requests).and_then(|(h,r)| (r>0).then_some(h as f64/r as f64)),"counters":counters})
        );
        Ok(ExitCode::from(
            status
                .code()
                .and_then(|c| u8::try_from(c).ok())
                .unwrap_or(1),
        ))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn go_portable_paths_preserve_declared_flags_and_opt_out() {
        let mut env = Environment::new();
        env.insert("GOCACHE".into(), "/cache/go".into());
        env.insert("GOFLAGS".into(), "-tags=noassets".into());
        prepare_go(&mut env);
        assert_eq!(
            value(&env, "GOFLAGS").as_deref(),
            Some("-tags=noassets -trimpath")
        );
        env.insert("GOFLAGS".into(), "-trimpath=false".into());
        prepare_go(&mut env);
        assert_eq!(value(&env, "GOFLAGS").as_deref(), Some("-trimpath=false"));
    }
    #[test]
    fn remaps_both_source_macros_and_debug_paths_without_relaxing_native_keys() {
        let args = remapped_arguments(
            Path::new("/work/job one"),
            [OsString::from("-g"), "-c".into(), "source.c".into()].into_iter(),
        );
        assert_eq!(args[0], "-g");
        assert_eq!(args[3], "-ffile-prefix-map=/work/job one=/source");
        assert_eq!(args[4], "-fdebug-prefix-map=/work/job one=/source");
    }
    #[test]
    fn absent_storage_does_not_change_the_check_environment() {
        let mut environment = Environment::new();
        environment.insert("CCACHE_DIR".into(), "/absent/ccid-test-cache".into());
        let before = environment.clone();
        assert!(CompilerCache::prepare(Path::new("/repo"), &mut environment).is_none());
        assert_eq!(before, environment);
    }
    #[test]
    fn a_nested_check_skips_the_enclosing_jobs_compiler_aliases() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let alias = root.path().join("ccid-compiler-AbC123");
        let real = root.path().join("real");
        for directory in [&alias, &real] {
            fs::create_dir(directory).unwrap();
            let tool = directory.join("cc");
            fs::write(&tool, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut environment = Environment::new();
        environment.insert(
            "PATH".into(),
            std::env::join_paths([&alias, &real]).unwrap(),
        );
        assert_eq!(executable(&environment, "cc").unwrap(), real.join("cc"));
        environment.insert("PATH".into(), alias.as_os_str().to_owned());
        assert_eq!(executable(&environment, "cc"), None);
    }

    #[test]
    fn alias_recursion_is_cut_off() {
        assert_eq!(next_depth(None).unwrap(), 1);
        assert_eq!(next_depth(Some("3".as_ref())).unwrap(), 4);
        assert_eq!(next_depth(Some("junk".as_ref())).unwrap(), 1);
        assert!(next_depth(Some("8".as_ref())).is_err());
    }
}

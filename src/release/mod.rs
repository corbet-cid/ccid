//! `ccid release`: the release flow every repository shares (`render_schema = 3`, `[release]`).
//!
//! One workflow run, `release.yml`, holds the whole flow:
//!
//! ```text
//! dispatch on main ─ release (linux) ──┬─ release-windows (windows)  if the version holds Windows
//!                                      ├─ release-macos   (macos)    if the version holds macOS
//!                                      └─ release-promote (linux)    after every leg that ran
//! ```
//!
//! * `release` (`ccid release build`): preflight (a version is released once; the source is main; no red
//!   verdict), the third-party lock refresh, the declared build checks into one directory, ONE draft
//!   `build-<commit>` that holds every file, size-checked and resumable.
//! * a leg (`ccid release leg <os>`): the declared leg check runs on that label against the draft and
//!   leaves its evidence there (`ccid release fetch` and `ccid release put`).
//! * `release-promote` (`ccid release promote`): the derived manifests and `SHA256SUMS` are written again
//!   from the complete draft (these are the only files promotion may replace, and only while the release is
//!   a draft), then the draft becomes the public release `v<version>`.
//!
//! The version is read from the repository's version file, the platform set follows from it
//! ([`version`]), and nothing else is a knob. See `docs/release.md`.

pub mod draft;
pub mod flow;
pub mod forge;
pub mod version;

use crate::{failure, Result};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

/// The only platform every release holds.
pub const BASE_PLATFORM: &str = "linux-x86_64";
/// The legs a release can have, in the order they run.
pub const LEGS: [&str; 2] = ["windows", "macos"];
/// The name of the file that lists the checksum of every other asset.
pub const SUMS: &str = "SHA256SUMS";
/// The draft of a commit is `build-<commit>`; the public release is `v<version>`.
pub const DRAFT_PREFIX: &str = "build-";
/// The jobs of the rendered `release` workflow (planned from `[release]`, never declared).
pub const JOB: &str = "release";
pub const PROMOTE: &str = "release-promote";
/// The workflow all of them belong to.
pub const WORKFLOW: &str = "release";
/// The commands of those jobs (`ccid job-run` turns them into `ccid release ...`).
pub const BUILD_COMMAND: &str = "ccid:release-build";
pub const LEG_COMMAND: &str = "ccid:release-leg";
pub const PROMOTE_COMMAND: &str = "ccid:release-promote";

/// The job that runs the leg of `os`.
pub fn leg_job(os: &str) -> String {
    format!("release-{os}")
}

/// Which release command a job command stands for: `["release", "build"]`, `["release", "leg", "windows"]`, ...
/// `job` is the job's name (a leg's name carries its operating system).
pub fn command_arguments(command: &[String], job: &str) -> Option<Vec<String>> {
    let [only] = command else { return None };
    let words: &[&str] = match only.as_str() {
        BUILD_COMMAND => &["release", "build"],
        PROMOTE_COMMAND => &["release", "promote"],
        LEG_COMMAND => {
            let os = job.strip_prefix("release-")?;
            return LEGS
                .contains(&os)
                .then(|| vec!["release".to_owned(), "leg".to_owned(), os.to_owned()]);
        }
        _ => return None,
    };
    Some(words.iter().map(|word| (*word).to_owned()).collect())
}

/// The plan of a release job (`release`, `release-<os>`, `release-promote`) of a manifest with `[release]`.
pub(crate) fn job_plan(
    manifest: &crate::Manifest,
    name: &str,
) -> Result<Option<crate::jobs::Plan>> {
    let Some(config) = &manifest.release else {
        return Ok(None);
    };
    let (label, command, checks) = if name == JOB {
        ("linux", BUILD_COMMAND, config.build.clone())
    } else if name == PROMOTE {
        ("linux", PROMOTE_COMMAND, config.seal.clone())
    } else if let Some(os) = name
        .strip_prefix("release-")
        .filter(|os| LEGS.contains(os) && config.legs.contains_key(*os))
    {
        (
            LEGS.into_iter().find(|leg| leg == &os).unwrap_or("linux"),
            LEG_COMMAND,
            vec![config.legs[os].clone()],
        )
    } else {
        return Ok(None);
    };
    // The linux jobs run in the persistent job environment the manifest declares, like every verdict job.
    let runtime = match &manifest.runtime {
        Some(config) if label == "linux" => Some(crate::runtime::Runtime {
            config: config.clone(),
            toolchain: manifest.toolchain.clone(),
        }),
        _ => None,
    };
    Ok(Some(crate::jobs::Plan {
        job: name.to_owned(),
        scheduler: crate::jobs::Scheduler::Forgejo,
        checks,
        workflow: WORKFLOW.to_owned(),
        command: vec![command.to_owned()],
        push_branches: Vec::new(),
        refresh: None,
        environment: BTreeMap::new(),
        secrets: Vec::new(),
        runs_on: Some(label.to_owned()),
        paths: Vec::new(),
        events: vec!["manual".to_owned()],
        archive_inputs: false,
        siblings: BTreeMap::new(),
        provider: None,
        platform: None,
        timeout_minutes: None,
        withheld: Vec::new(),
        runtime,
        mirror_optout: None,
    }))
}

/// What a release is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Executables for the declared platforms.
    Binary,
    /// Registry packages built once (no platforms, no legs).
    Library,
}

/// The registries a release goes to. Which publisher serves each is platform policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Registry {
    Crates,
    Npm,
    Pypi,
    Aur,
    /// Published from the GitHub copy through OIDC (`jsr-publish.yml`).
    Jsr,
}

/// `[release]`: absent means no release surface is rendered.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub kind: Kind,
    /// The version file: `Cargo.toml`, `package.json` or `pyproject.toml` (a path inside the repository).
    pub version: String,
    /// Binary only: every platform a full release holds, `<os>-<arch>`; `linux-x86_64` is required.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Checks the release job runs; they leave the release's files in `$CCID_RELEASE_DIR`.
    #[serde(default)]
    pub build: Vec<String>,
    /// Binary only: the check each leg runs on its label, `windows` and `macos`.
    #[serde(default)]
    pub legs: BTreeMap<String, String>,
    /// Checks promotion runs on the complete draft to derive `manifests` (and to refuse a release whose
    /// evidence is incomplete).
    #[serde(default)]
    pub seal: Vec<String>,
    /// The files `seal` derives: the only assets (besides `SHA256SUMS`) promotion may replace.
    #[serde(default)]
    pub manifests: Vec<String>,
    #[serde(default)]
    pub registries: Vec<Registry>,
    /// A file of the repository whose text is the body of the release (default: one generated line).
    #[serde(default)]
    pub notes: Option<String>,
}

/// What one version of one repository releases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub version: version::Version,
    pub class: version::Class,
    /// The platforms this release holds (empty for a library).
    pub platforms: Vec<String>,
    /// The legs this release runs, in order.
    pub legs: Vec<&'static str>,
}

fn os_of(platform: &str) -> &str {
    platform.split('-').next().unwrap_or("")
}

fn plain_platform(platform: &str) -> bool {
    let mut parts = platform.splitn(2, '-');
    let (os, arch) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    ["linux", "windows", "macos"].contains(&os)
        && !arch.is_empty()
        && arch
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A file name an asset can carry: plain, never the checksum file (ccid writes that one).
pub fn plain_asset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

impl Config {
    /// The table's own rules, against the manifest's declared checks.
    pub(crate) fn validate(&self, manifest: &crate::Manifest) -> Result<()> {
        version::check_file(&self.version)?;
        for (name, job) in &manifest.jobs {
            let planned = name == JOB
                || name == PROMOTE
                || name
                    .strip_prefix("release-")
                    .is_some_and(|os| LEGS.contains(&os));
            if planned || job.workflow() == WORKFLOW {
                return Err(failure(format!(
                    "[release] plans the jobs release, release-windows, release-macos and release-promote and owns the workflow release: [jobs.{name}] cannot be declared (it goes into [release])"
                )));
            }
        }
        let known = |names: &[String], what: &str| -> Result<()> {
            if names.is_empty() {
                return Ok(());
            }
            crate::select_checks(manifest, names)
                .map(|_| ())
                .map_err(|error| failure(format!("[release] {what}: {error}")))
        };
        if self.build.is_empty() {
            return Err(failure(
                "[release] build names the checks that leave the release's files in $CCID_RELEASE_DIR",
            ));
        }
        known(&self.build, "build")?;
        known(&self.seal, "seal")?;
        if self.seal.is_empty() != self.manifests.is_empty() {
            return Err(failure(
                "[release] seal and manifests go together: seal derives the manifests, and the manifests are what seal may replace",
            ));
        }
        for name in &self.manifests {
            if !plain_asset_name(name) || name == SUMS {
                return Err(failure(format!(
                    "[release] manifests: {name:?} is not a plain asset name (and {SUMS} is written by ccid)"
                )));
            }
        }
        match self.kind {
            Kind::Library => {
                if !self.platforms.is_empty() || !self.legs.is_empty() {
                    return Err(failure(
                        "[release] kind = \"library\" has no platforms and no legs: its release is the registry packages, built once",
                    ));
                }
            }
            Kind::Binary => {
                if !self.platforms.iter().any(|p| p == BASE_PLATFORM) {
                    return Err(failure(format!(
                        "[release] platforms must list {BASE_PLATFORM}: every release holds it"
                    )));
                }
                let mut seen = Vec::new();
                for platform in &self.platforms {
                    if !plain_platform(platform) || seen.contains(&platform) {
                        return Err(failure(format!(
                            "[release] platforms: {platform:?} is not a platform (<linux|windows|macos>-<arch>) or is listed twice"
                        )));
                    }
                    seen.push(platform);
                }
                for leg in self.legs.keys() {
                    if !LEGS.contains(&leg.as_str()) {
                        return Err(failure(format!(
                            "[release] legs: {leg:?} is not a leg (windows or macos)"
                        )));
                    }
                }
                for os in LEGS {
                    let platforms = self.platforms.iter().any(|p| os_of(p) == os);
                    match (platforms, self.legs.get(os)) {
                        (true, None) => {
                            return Err(failure(format!(
                                "[release] platforms holds a {os} platform: legs.{os} names the check that tests it on the {os} label"
                            )))
                        }
                        (false, Some(_)) => {
                            return Err(failure(format!(
                                "[release] legs.{os} is declared but no platform of {os} is listed"
                            )))
                        }
                        _ => {}
                    }
                }
                let checks: Vec<String> = self.legs.values().cloned().collect();
                known(&checks, "legs")?;
            }
        }
        if let Some(notes) = &self.notes {
            version::check_relative(notes)?;
        }
        Ok(())
    }

    /// What `version` releases: a patch is `linux-x86_64`, every other version the declared platforms.
    pub fn plan(&self, version: version::Version) -> Plan {
        let platforms = match self.kind {
            Kind::Library => Vec::new(),
            Kind::Binary if version.full_set() => self.platforms.clone(),
            Kind::Binary => vec![BASE_PLATFORM.to_owned()],
        };
        let legs = LEGS
            .into_iter()
            .filter(|os| platforms.iter().any(|p| os_of(p) == *os))
            .collect();
        Plan {
            class: version.class(),
            version,
            platforms,
            legs,
        }
    }

    /// The legs a full release can have, whatever the version.
    pub fn declared_legs(&self) -> Vec<&'static str> {
        LEGS.into_iter()
            .filter(|os| self.legs.contains_key(*os))
            .collect()
    }
}

/// `ccid release`: the commands behind the rendered release workflow, and for the scripts of a leg.
#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Print the version the manifest's version file holds and its class (`1.2.0 minor`).
    Version {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Print the platforms this version releases, one per line.
    Platforms {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Refuse a release that must not start: a source that is not main, a version already released, a red verdict.
    Preflight {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        commit: Option<String>,
    },
    /// The release job: preflight, the third-party lock refresh, the build checks, the draft with every file.
    Build {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// A platform leg (windows or macos): the declared leg check, run against the draft of this commit.
    Leg {
        /// `windows` or `macos`.
        os: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Put files into the draft of the commit: added, replaced when other bytes are held, or left alone when identical.
    Put {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        commit: Option<String>,
    },
    /// Download assets of the draft of the commit (every asset when none is named).
    Fetch {
        names: Vec<String>,
        #[arg(long)]
        into: PathBuf,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        commit: Option<String>,
    },
    /// Print the SHA256SUMS text of the files of a directory.
    Sums { dir: PathBuf },
    /// Make the draft of this commit the public release: seal, SHA256SUMS, rename.
    Promote {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Delete the draft of the commit (never a public release).
    Discard {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        commit: Option<String>,
    },
}

/// Run one `ccid release` command; the exit code.
pub fn run(command: &Command) -> Result<std::process::ExitCode> {
    match command {
        Command::Version { repo } => flow::describe_version(repo, false),
        Command::Platforms { repo } => flow::describe_version(repo, true),
        Command::Preflight { repo, commit } => flow::preflight_command(repo, commit.as_deref()),
        Command::Build { repo } => flow::build(repo),
        Command::Leg { os, repo } => flow::leg(repo, os),
        Command::Put {
            files,
            repo,
            commit,
        } => flow::put(repo, commit.as_deref(), files),
        Command::Fetch {
            names,
            into,
            repo,
            commit,
        } => flow::fetch(repo, commit.as_deref(), names, into),
        Command::Sums { dir } => flow::sums(dir),
        Command::Promote { repo } => flow::promote(repo),
        Command::Discard { repo, commit } => flow::discard(repo, commit.as_deref()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    const BINARY: &str = "kind = \"binary\"\nversion = \"Cargo.toml\"\n\
        platforms = [\"linux-x86_64\", \"linux-aarch64\", \"macos-x86_64\", \"macos-arm64\", \"windows-x86_64\"]\n\
        build = [\"b\"]\nlegs = { windows = \"w\", macos = \"m\" }\n";

    #[test]
    fn a_patch_holds_linux_alone_and_runs_no_leg() {
        let config = config(BINARY);
        let plan = config.plan(version::Version::parse("0.1.1").unwrap());
        assert_eq!(plan.platforms, vec![BASE_PLATFORM]);
        assert!(plan.legs.is_empty());
        assert_eq!(plan.class, version::Class::Patch);
    }

    #[test]
    fn a_minor_and_a_major_hold_every_platform_and_both_legs() {
        let config = config(BINARY);
        for text in ["0.1.0", "1.0.0", "3.2.0"] {
            let plan = config.plan(version::Version::parse(text).unwrap());
            assert_eq!(plan.platforms.len(), 5, "{text}");
            assert_eq!(plan.legs, vec!["windows", "macos"], "{text}");
        }
    }

    #[test]
    fn a_library_holds_no_platform_and_runs_no_leg() {
        let config = config("kind = \"library\"\nversion = \"Cargo.toml\"\nbuild = [\"pack\"]\n");
        let plan = config.plan(version::Version::parse("1.2.0").unwrap());
        assert!(plan.platforms.is_empty() && plan.legs.is_empty());
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(toml::from_str::<Config>(
            "kind = \"binary\"\nversion = \"Cargo.toml\"\nknob = 1\n"
        )
        .is_err());
    }

    #[test]
    fn registries_are_a_closed_set() {
        let ok = config("kind = \"library\"\nversion = \"Cargo.toml\"\nbuild = [\"p\"]\nregistries = [\"crates\", \"jsr\"]\n");
        assert_eq!(ok.registries, vec![Registry::Crates, Registry::Jsr]);
        assert!(toml::from_str::<Config>(
            "kind = \"library\"\nversion = \"Cargo.toml\"\nregistries = [\"homebrew\"]\n"
        )
        .is_err());
    }
}

#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use std::{path::PathBuf, process::ExitCode, sync::atomic::Ordering};

#[cfg(unix)]
mod supervision;

mod quality;

#[derive(Parser)]
#[command(about = "Shared checks and repository jobs for Crow or Argo", version)]
struct Cli {
    #[arg(long, global = true, env = "CCID_SUBMISSION_CONFIG")]
    submission_config: Option<PathBuf>,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Submit exact committed source and inspect Crow runs.
    CrowCi {
        #[command(subcommand)]
        action: ccid::submission::Action,
    },
    /// Submit repository-owned jobs to Crow or Argo.
    CiJob {
        #[command(subcommand)]
        action: ccid::submission::JobAction,
    },
    /// Execute one exact archived job supplied by a scheduler adapter.
    ExecuteJob {
        #[arg(long)]
        request: PathBuf,
        /// Require the archived job to match the scheduler's source identity.
        #[arg(long)]
        expect_commit: Option<String>,
        #[arg(long, hide = true)]
        parent_watch: bool,
    },
    /// Run one repository job as a Forgejo workflow step does: claim a source slot, fetch the exact commit, check the adapters, run the command.
    #[cfg(unix)]
    JobRun {
        #[arg(long)]
        job: String,
        /// `owner/name` of the repository.
        #[arg(long)]
        repository: String,
        #[arg(long)]
        sha: String,
        #[arg(long)]
        server_url: String,
        /// Owner of the repository (qualifies sibling names).
        #[arg(long)]
        owner: String,
        /// Run the job against an existing source export instead of fetching the commit: the land gate's entry point.
        #[arg(long)]
        local: bool,
        /// Source directory for --local (an export of the exact commit).
        #[arg(long, requires = "local")]
        source: Option<PathBuf>,
    },
    /// Generate owned scheduler adapters from the repository job manifest.
    Render {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = ".ci/ccid.toml")]
        manifest: PathBuf,
        /// Detect missing or changed generated adapters without writing.
        #[arg(long)]
        check: bool,
    },
    /// Report a job's commit status and the commit's aggregated verdict context.
    Verdict(ccid::verdict::Options),
    /// Plan a repository job for Crow or Argo; adapters submit the returned command.
    Job {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = ".ci/ccid.toml")]
        manifest: PathBuf,
        #[arg(long)]
        job: String,
        #[arg(long, value_enum)]
        scheduler: Option<ccid::jobs::Scheduler>,
    },
    /// Deterministic organization and repository quality checks across forges.
    Quality {
        #[command(subcommand)]
        action: quality::Action,
    },
    /// Tor milestone orchestration: private network, Records, Snowflake browser.
    Tor {
        #[command(subcommand)]
        action: ccid::tor::Action,
    },
    SourceRevision,
    /// Print the shared build profile (profiles/dev.toml) as a cargo config fragment, for the include file of a CARGO_HOME.
    DevProfile,
    VerifySource {
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        sha256: String,
        #[arg(long)]
        commit: String,
        #[arg(long)]
        destination: PathBuf,
    },
    Check {
        #[arg(long, requires_all = ["sha256", "commit"])]
        archive: Option<PathBuf>,
        #[arg(long, requires = "archive")]
        sha256: Option<String>,
        #[arg(long, requires = "archive")]
        commit: Option<String>,
        #[arg(long, default_value = ".", conflicts_with = "archive")]
        repo: PathBuf,
        #[arg(long, default_value = ".ci/ccid.toml")]
        manifest: PathBuf,
        #[arg(long = "check", required = true)]
        checks: Vec<String>,
        #[arg(long)]
        plan: bool,
        #[arg(long, hide = true, conflicts_with = "plan")]
        parent_watch: bool,
    },
    /// Run selected checks through moon so unchanged inputs reuse earlier results.
    /// Requires `moon` and this ccid revision on PATH; `CCID_REMOTE_CACHE` enables
    /// a shared remote cache. The moon configuration is generated per run.
    Cached {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = ".ci/ccid.toml")]
        manifest: PathBuf,
        #[arg(long = "check", required = true)]
        checks: Vec<String>,
        #[arg(long)]
        plan: bool,
        /// Ignore cached results and run every selected check (periodic uncached runs).
        #[arg(long, conflicts_with = "plan")]
        force: bool,
    },
    CargoResolve {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
        #[arg(long = "check")]
        checks: Vec<String>,
        /// Generate a lock from the committed manifests when no coherent baseline exists.
        #[arg(long)]
        generate_lockfile: bool,
        #[arg(long)]
        plan: bool,
        #[arg(long, hide = true, conflicts_with = "plan")]
        parent_watch: bool,
    },
    /// Execute a push-opted-in job through shared newest-head coalescing.
    /// All push pipelines for one consumer share one admitted build per
    /// burst; triggers attach with proof instead of rebuilding.
    PushRun {
        #[arg(long)]
        consumer_url: String,
        #[arg(long)]
        consumer_branch: String,
        #[arg(long)]
        job: String,
        /// self for consumer pushes, dep for dependency pushes.
        #[arg(long, default_value = "self")]
        trigger_kind: String,
        /// Event commit (provenance; coverage proven against the receipt).
        #[arg(long)]
        trigger_sha: String,
        /// Dependency package name; required for dep triggers.
        #[arg(long, default_value = "")]
        trigger_name: String,
        /// Event branch (provenance only).
        #[arg(long, default_value = "")]
        trigger_branch: String,
        /// Canonical event repository URL (provenance; required for dep,
        /// defaults to the consumer for self).
        #[arg(long, default_value = "")]
        trigger_repo: String,
        /// Supervision hook, set only by the supervisor's re-execution.
        #[arg(long, hide = true)]
        parent_watch: bool,
    },
    /// The CI surface: the class of every CI file and the plan of the move to render_schema = 3.
    Surface {
        #[command(subcommand)]
        command: ccid::render::surface::Command,
    },
    /// Fleet data from the release channel: first-party owner prefixes and their legacy aliases.
    Fleet {
        #[command(subcommand)]
        action: ccid::fleet::Action,
    },
    /// Shared Rust toolchain lock and the wasm-bindgen test runner (Unix only).
    #[cfg(unix)]
    Toolchain {
        #[command(subcommand)]
        action: ccid::toolchain::Action,
    },
    /// Coverage gates over LLVM reports with manifest-declared exclusions.
    Coverage {
        #[command(subcommand)]
        action: ccid::coverage::Action,
    },
    /// Check locked content pins (fixture digests, native artifact versions).
    ContentPins(ccid::content_pins::Options),
    /// Require main declarations and one resolved revision per first-party crate.
    FirstParty(ccid::first_party::Options),
    /// Float the first-party branch-tracking git dependencies outside the repository: resolve
    /// them in a copy (`cargo update -p <spec>` of exactly those packages, selected by the fleet
    /// data CCID_FLEET_FILE names) and write the resolved locks under DIR at their relative paths.
    /// Prints one JSON line {"packages","digest","commits","third_party"}; never writes the
    /// repository and refuses a DIR inside it.
    Float {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Directory for the resolved locks; must be outside the repository.
        #[arg(long)]
        into: PathBuf,
    },
    /// Resolve the npm lock outside the declared source and retain it as an artifact (manual job, never a verdict).
    ResolveLock {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = ".ci/ccid.toml")]
        manifest: PathBuf,
        /// Resolve the npm lock (npm install --package-lock-only); without it Rust resolution is refused as retired.
        #[arg(long)]
        npm: bool,
    },
    /// Check reviewed advisory exceptions and transport version floors
    /// offline against deny.toml and dependency-metadata.json.
    Advisories {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = ".ci/ccid.toml")]
        manifest: PathBuf,
    },
}

fn main() -> ExitCode {
    if let Some(result) = ccid::typescript_cache::dispatch() {
        return result.unwrap_or_else(|error| {
            eprintln!("ccid: TypeScript execution failed: {error}");
            ExitCode::from(2)
        });
    }
    if let Some(result) = ccid::compile_cache::dispatch() {
        return result.unwrap_or_else(|error| {
            eprintln!("ccid: compiler execution failed: {error}");
            ExitCode::from(2)
        });
    }
    // Hidden compiler-wrapper diagnostic mode for one frozen library
    // build: Cargo invokes this binary as RUSTC_WORKSPACE_WRAPPER with
    // the real compiler first (workspace members only; artifacts cache
    // separately by filename hash). Env-gated (cargo cannot inject
    // subcommand names); all other invocations parse the CLI below as usual.
    if std::env::var_os(ccid::rustc_diag::MODE_ENV).is_some() {
        return match ccid::rustc_diag::run_wrapped() {
            Ok(code) => code,
            Err(error) => {
                eprintln!("ccid: compiler diagnostic failed: {error}");
                ExitCode::from(2)
            }
        };
    }
    let mut arguments: Vec<_> = std::env::args_os().collect();
    // Compatibility entry points may be symlinks to this same Rust executable.
    if let Some(name) = arguments
        .first()
        .and_then(|p| std::path::Path::new(p).file_name())
        .and_then(|p| p.to_str())
        .map(str::to_owned)
    {
        if matches!(name.as_str(), "crow-ci" | "ci-job") {
            arguments.insert(1, name.into());
        }
    }
    let cli = Cli::parse_from(arguments);
    let outcome = match cli.action {
        Action::CrowCi { action } => {
            ccid::submission::run(action, cli.submission_config.as_deref())
        }
        Action::CiJob { action } => {
            ccid::submission::run_job(action, cli.submission_config.as_deref())
        }
        Action::ExecuteJob {
            request,
            expect_commit,
            parent_watch,
        } => {
            if let Err(error) =
                ctrlc::set_handler(|| ccid::INTERRUPTED.store(true, Ordering::SeqCst))
            {
                eprintln!("ccid: cannot install cancellation handler: {error}");
                return ExitCode::from(2);
            }
            #[cfg(unix)]
            if parent_watch {
                if let Err(error) = supervision::watch_parent() {
                    eprintln!("ccid: cannot watch enclosing process: {error}");
                    return ExitCode::from(2);
                }
            } else {
                return match supervision::execute() {
                    Ok(status) => {
                        // The job's own result becomes the commit's terminal
                        // status; a missing report must be visible, so a green
                        // job whose report failed ends red.
                        let reported = ccid::verdict::report_terminal(&request, status.success());
                        if let Err(error) = &reported {
                            eprintln!("ccid: cannot report the job's result: {error}");
                        }
                        if status.success() && reported.is_err() {
                            ExitCode::from(2)
                        } else {
                            ExitCode::from(status.code().unwrap_or(2) as u8)
                        }
                    }
                    Err(error) => {
                        eprintln!("ccid: cannot supervise job: {error}");
                        ExitCode::from(2)
                    }
                };
            }
            std::fs::read(request)
                .map_err(Into::into)
                .and_then(|bytes| {
                    serde_json::from_slice::<ccid::jobs::Request>(&bytes).map_err(Into::into)
                })
                .and_then(|request| {
                    if expect_commit
                        .as_ref()
                        .is_some_and(|expected| expected != &request.commit)
                    {
                        return Err("Job source does not match the scheduler commit".into());
                    }
                    ccid::jobs::execute(&request)
                })
        }
        Action::Render {
            repo,
            manifest,
            check,
        } => ccid::render::render(&repo, &manifest, check).and_then(|report| {
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }),
        Action::Verdict(options) => ccid::verdict::run(&options),
        Action::Job {
            repo,
            manifest,
            job,
            scheduler,
        } => ccid::jobs::plan(&repo, &manifest, &job, scheduler).and_then(|plan| {
            println!("{}", serde_json::to_string(&plan)?);
            Ok(())
        }),
        Action::Quality { action } => {
            if let Err(error) =
                ctrlc::set_handler(|| ccid::INTERRUPTED.store(true, Ordering::SeqCst))
            {
                eprintln!("ccid: cannot install cancellation handler: {error}");
                return ExitCode::from(2);
            }
            return match quality::run(action) {
                Ok(code) => ExitCode::from(code),
                Err(error) => {
                    eprintln!("ccid quality: {error}");
                    ExitCode::from(2)
                }
            };
        }
        Action::Tor { action } => {
            if let Err(error) =
                ctrlc::set_handler(|| ccid::INTERRUPTED.store(true, Ordering::SeqCst))
            {
                eprintln!("ccid: cannot install cancellation handler: {error}");
                return ExitCode::from(2);
            }
            #[cfg(unix)]
            {
                let parent_watch = action.parent_watch();
                if parent_watch {
                    if let Err(error) = supervision::watch_parent() {
                        eprintln!("ccid: cannot watch enclosing process: {error}");
                        return ExitCode::from(2);
                    }
                } else {
                    return match supervision::execute() {
                        Ok(status) => ExitCode::from(status.code().unwrap_or(2) as u8),
                        Err(error) => {
                            eprintln!("ccid: cannot supervise Tor job: {error}");
                            ExitCode::from(2)
                        }
                    };
                }
            }
            #[cfg(not(unix))]
            {
                let _ = action.parent_watch();
                eprintln!("ccid tor: Tor milestone orchestration requires Unix");
                return ExitCode::from(2);
            }
            ccid::tor::run(&action)
        }
        #[cfg(unix)]
        Action::JobRun {
            job,
            repository,
            sha,
            server_url,
            owner,
            local,
            source,
        } => {
            let local = if local {
                match source {
                    Some(source) => Some(source),
                    None => {
                        eprintln!("ccid: --local requires --source");
                        return ExitCode::from(2);
                    }
                }
            } else {
                None
            };
            match ccid::job_run::run(
                &ccid::job_run::Options {
                    job,
                    repository,
                    sha,
                    server_url,
                    owner,
                    local,
                },
                &ccid::job_run::Pace::default(),
            ) {
                // An exec'd job never returns; a returned code is the job's own refusal.
                Ok(code) => return code,
                Err(error) => Err(error),
            }
        }
        Action::SourceRevision => {
            println!("{}", ccid::SOURCE_REVISION);
            Ok(())
        }
        Action::DevProfile => {
            print!("{}", ccid::DEV_PROFILE);
            Ok(())
        }
        Action::VerifySource {
            archive,
            sha256,
            commit,
            destination,
        } => ccid::verify_source(&archive, &sha256, &commit, &destination),
        Action::Check {
            archive,
            sha256,
            commit,
            repo,
            manifest,
            checks,
            plan,
            parent_watch,
        } => {
            // A repository with a rendered check-shell builds inside it: one C
            // toolchain and one set of native libraries for CI and for a lane
            // that sources the same file. Plans stay outside (they run nothing).
            if !plan && archive.is_none() {
                if let Err(error) = ccid::check_shell::enter(&repo) {
                    eprintln!("ccid: {error}");
                    return ExitCode::from(2);
                }
            }
            if let Err(error) =
                ctrlc::set_handler(|| ccid::INTERRUPTED.store(true, Ordering::SeqCst))
            {
                eprintln!("ccid: cannot install cancellation handler: {error}");
                return ExitCode::from(2);
            }
            #[cfg(unix)]
            if !plan {
                if parent_watch {
                    if let Err(error) = supervision::watch_parent() {
                        eprintln!("ccid: cannot watch enclosing process: {error}");
                        return ExitCode::from(2);
                    }
                } else {
                    return match supervision::execute() {
                        Ok(status) => ExitCode::from(status.code().unwrap_or(2) as u8),
                        Err(error) => {
                            eprintln!("ccid: cannot supervise checks: {error}");
                            ExitCode::from(2)
                        }
                    };
                }
            }
            #[cfg(not(unix))]
            if parent_watch {
                eprintln!("ccid: parent liveness supervision requires Unix");
                return ExitCode::from(2);
            }
            if let Some(archive) = archive {
                ccid::run_archive_checks(
                    &archive,
                    sha256.as_deref().unwrap_or(""),
                    commit.as_deref().unwrap_or(""),
                    &manifest,
                    &checks,
                    plan,
                )
            } else {
                ccid::run_checks(&repo, &manifest, &checks, plan)
            }
        }
        Action::Cached {
            repo,
            manifest,
            checks,
            plan,
            force,
        } => {
            // The moon tasks this starts are ordinary `ccid check` runs; enter
            // the one rendered shell here so every task inherits it.
            if !plan {
                if let Err(error) = ccid::check_shell::enter(&repo) {
                    eprintln!("ccid: {error}");
                    return ExitCode::from(2);
                }
            }
            if let Err(error) = ctrlc::set_handler(|| {
                ccid::INTERRUPTED.store(true, Ordering::SeqCst);
                cmnp::INTERRUPTED.store(true, Ordering::SeqCst);
            }) {
                eprintln!("ccid: cannot install cancellation handler: {error}");
                return ExitCode::from(2);
            }
            // Each check moon starts is an ordinary, separately supervised `ccid check`.
            ccid::run_cached(&repo, &manifest, &checks, plan, force)
        }
        Action::CargoResolve {
            repo,
            output_dir,
            checks,
            generate_lockfile,
            plan,
            parent_watch,
        } => {
            if let Err(error) =
                ctrlc::set_handler(|| ccid::INTERRUPTED.store(true, Ordering::SeqCst))
            {
                eprintln!("ccid: cannot install cancellation handler: {error}");
                return ExitCode::from(2);
            }
            #[cfg(unix)]
            if !plan {
                if parent_watch {
                    if let Err(error) = supervision::watch_parent() {
                        eprintln!("ccid: cannot watch enclosing process: {error}");
                        return ExitCode::from(2);
                    }
                } else {
                    return match supervision::execute() {
                        Ok(status) => ExitCode::from(status.code().unwrap_or(2) as u8),
                        Err(error) => {
                            eprintln!("ccid: cannot supervise Cargo resolution: {error}");
                            ExitCode::from(2)
                        }
                    };
                }
            }
            ccid::resolve_cargo(&repo, &output_dir, &checks, generate_lockfile, plan)
        }
        Action::PushRun {
            consumer_url,
            consumer_branch,
            job,
            trigger_kind,
            trigger_sha,
            trigger_name,
            trigger_branch,
            trigger_repo,
            parent_watch,
        } => {
            if let Err(error) =
                ctrlc::set_handler(|| ccid::INTERRUPTED.store(true, Ordering::SeqCst))
            {
                eprintln!("ccid: cannot install cancellation handler: {error}");
                return ExitCode::from(2);
            }
            // Long-running coalesced command: own cancellation and
            // parent-death cleanup exactly like execute-job, so a dead Crow
            // shell cannot orphan gate commands holding the shared locks.
            #[cfg(unix)]
            if parent_watch {
                if let Err(error) = supervision::watch_parent() {
                    eprintln!("ccid: cannot watch enclosing process: {error}");
                    return ExitCode::from(2);
                }
            } else {
                return match supervision::execute() {
                    Ok(status) => ExitCode::from(status.code().unwrap_or(2) as u8),
                    Err(error) => {
                        eprintln!("ccid: cannot supervise push: {error}");
                        return ExitCode::from(2);
                    }
                };
            }
            #[cfg(not(unix))]
            if parent_watch {
                eprintln!("ccid: parent liveness supervision requires Unix");
                return ExitCode::from(2);
            }
            let environment: ccid::Environment = std::env::vars_os().collect();
            let timeout = match ccid::budget(&environment) {
                Ok(budget) => budget.timeout,
                Err(error) => {
                    eprintln!("ccid: {error}");
                    return ExitCode::from(2);
                }
            };
            let cache_root = std::env::var_os("CI_CACHE_ROOT")
                .or_else(|| std::env::var_os("CARGO_HOME"))
                .map(PathBuf::from);
            let Some(cache_root) = cache_root else {
                eprintln!("ccid: push coalescing requires CI_CACHE_ROOT or CARGO_HOME");
                return ExitCode::from(2);
            };
            match ccid::push::push_run(
                consumer_url,
                consumer_branch,
                job,
                trigger_kind,
                trigger_sha,
                trigger_name,
                trigger_branch,
                trigger_repo,
                cache_root,
                timeout,
            ) {
                Ok(outcome) => {
                    eprintln!("ccid: push {outcome:?}");
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
        Action::Surface { command } => {
            return match ccid::render::surface::run(&command) {
                Ok(code) => ExitCode::from(code),
                Err(error) => {
                    eprintln!("ccid surface: {error}");
                    ExitCode::from(2)
                }
            };
        }
        Action::Fleet { action } => {
            return match ccid::fleet::run(&action) {
                Ok(code) => ExitCode::from(code),
                Err(error) => {
                    eprintln!("ccid fleet: {error}");
                    ExitCode::from(2)
                }
            };
        }
        #[cfg(unix)]
        Action::Toolchain { action } => {
            return match ccid::toolchain::run(action) {
                Ok(code) => ExitCode::from(code),
                Err(error) => {
                    eprintln!("ccid toolchain: {error}");
                    ExitCode::from(2)
                }
            };
        }
        Action::Coverage { action } => {
            // `coverage run` outlives its cargo children on a signal, so it still
            // removes its private target directory.
            if matches!(action, ccid::coverage::Action::Run { .. }) {
                if let Err(error) =
                    ctrlc::set_handler(|| ccid::INTERRUPTED.store(true, Ordering::SeqCst))
                {
                    eprintln!("ccid: cannot install cancellation handler: {error}");
                    return ExitCode::from(2);
                }
            }
            return match ccid::coverage::run(action) {
                Ok(code) => ExitCode::from(code),
                Err(error) => {
                    eprintln!("ccid coverage: {error}");
                    ExitCode::from(2)
                }
            };
        }
        Action::ContentPins(options) => {
            return match ccid::content_pins::run(&options) {
                Ok(code) => ExitCode::from(code),
                Err(error) => {
                    eprintln!("ccid content-pins: {error}");
                    ExitCode::from(2)
                }
            };
        }
        Action::FirstParty(options) => {
            return match ccid::first_party::run(&options) {
                Ok(code) => ExitCode::from(code),
                Err(error) => {
                    eprintln!("ccid first-party: {error}");
                    ExitCode::from(2)
                }
            };
        }
        Action::Float { repo, into } => {
            if let Err(error) =
                ctrlc::set_handler(|| ccid::INTERRUPTED.store(true, Ordering::SeqCst))
            {
                eprintln!("ccid: cannot install cancellation handler: {error}");
                return ExitCode::from(2);
            }
            return match ccid::float::run(&repo, &into) {
                Ok(code) => ExitCode::from(code),
                Err(error) => {
                    eprintln!("ccid float: {error}");
                    ExitCode::from(2)
                }
            };
        }
        Action::ResolveLock {
            repo,
            manifest,
            npm,
        } => {
            if let Err(error) =
                ctrlc::set_handler(|| ccid::INTERRUPTED.store(true, Ordering::SeqCst))
            {
                eprintln!("ccid: cannot install cancellation handler: {error}");
                return ExitCode::from(2);
            }
            return match ccid::resolve_lock::run(&repo, &manifest, npm) {
                Ok(code) => ExitCode::from(code),
                Err(error) => {
                    eprintln!("ccid resolve-lock: {error}");
                    ExitCode::from(2)
                }
            };
        }
        Action::Advisories { repo, manifest } => {
            return match ccid::advisories::run(&repo, &manifest) {
                Ok(code) => ExitCode::from(code),
                Err(error) => {
                    eprintln!("ccid advisories: {error}");
                    ExitCode::from(2)
                }
            };
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ccid: {error}");
            ExitCode::from(2)
        }
    }
}

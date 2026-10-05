#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use std::{path::PathBuf, process::ExitCode, sync::atomic::Ordering};

#[cfg(unix)]
mod supervision;

mod forge_cli;
mod quality;

#[derive(Parser)]
#[command(about = "Shared checks and repository jobs for Crow or Argo", version)]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
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
    /// Repository placement, exact cloning and execution failover policy.
    Forge {
        #[command(subcommand)]
        action: forge_cli::Action,
    },
    /// Deterministic organization and repository quality checks across forges.
    Quality {
        #[command(subcommand)]
        action: quality::Action,
    },
    SourceRevision,
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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let outcome = match cli.action {
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
                    Ok(status) => ExitCode::from(status.code().unwrap_or(2) as u8),
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
        Action::Job {
            repo,
            manifest,
            job,
            scheduler,
        } => ccid::jobs::plan(&repo, &manifest, &job, scheduler).and_then(|plan| {
            println!("{}", serde_json::to_string(&plan)?);
            Ok(())
        }),
        Action::Forge { action } => {
            if let Err(error) =
                ctrlc::set_handler(|| ccid::INTERRUPTED.store(true, Ordering::SeqCst))
            {
                eprintln!("ccid: cannot install cancellation handler: {error}");
                return ExitCode::from(2);
            }
            forge_cli::run(action)
        }
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
        Action::SourceRevision => {
            println!("{}", ccid::SOURCE_REVISION);
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
            if let Err(error) =
                ctrlc::set_handler(|| ccid::INTERRUPTED.store(true, Ordering::SeqCst))
            {
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
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ccid: {error}");
            ExitCode::from(2)
        }
    }
}

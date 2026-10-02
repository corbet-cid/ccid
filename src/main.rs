#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use std::{path::PathBuf, process::ExitCode, sync::atomic::Ordering};

#[cfg(unix)]
mod supervision;

mod forge_cli;
mod quality;

#[derive(Parser)]
#[command(about = "Shared check commands invoked by Crow", version)]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
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
    CargoResolve {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
        #[arg(long = "check")]
        checks: Vec<String>,
        /// Resolve every graph and run every check in committed .ci/resolve.toml.
        #[arg(long, conflicts_with_all = ["checks", "generate_lockfile"])]
        inventory: bool,
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
        Action::CargoResolve {
            repo,
            output_dir,
            checks,
            inventory,
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
            if inventory {
                ccid::resolve_inventory(&repo, &output_dir, plan)
            } else {
                ccid::resolve_cargo(&repo, &output_dir, &checks, generate_lockfile, plan)
            }
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

#[cfg(test)]
mod inventory_cli_tests {
    use super::*;

    #[test]
    fn fixed_inventory_refuses_caller_check_and_generation_overrides() {
        for suffix in [vec!["--check", "native"], vec!["--generate-lockfile"]] {
            let mut args = vec![
                "ccid",
                "cargo-resolve",
                "--output-dir",
                "artifacts",
                "--inventory",
            ];
            args.extend(suffix);
            assert!(Cli::try_parse_from(args).is_err());
        }
        assert!(Cli::try_parse_from([
            "ccid",
            "cargo-resolve",
            "--output-dir",
            "artifacts",
            "--inventory"
        ])
        .is_ok());
    }

    #[test]
    fn existing_single_root_selection_remains_available() {
        assert!(Cli::try_parse_from([
            "ccid",
            "cargo-resolve",
            "--output-dir",
            "artifacts",
            "--check",
            "native",
            "--generate-lockfile",
        ])
        .is_ok());
    }
}

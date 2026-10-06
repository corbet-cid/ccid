//! Organization and repository quality checks through the cqlt policy library.
//!
//! Evidence collection lives in cfrg (`cfrg collect`); this command evaluates
//! saved evidence and checks prose through cqlt without network access.
mod prose;
mod semantic;
use ccid::Result;
use clap::{Subcommand, ValueEnum};
use cqlt::{Policy, Severity, Snapshot, Status};
use std::{fs, path::PathBuf};

fn failure(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::other(message.into()).into()
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Threshold {
    Error,
    Warning,
}

#[derive(Subcommand)]
pub enum Action {
    /// Prepare, replay, or explicitly execute cqlt's Jev semantic review.
    Semantic(semantic::Options),
    /// Check writing with cqlt's subordinate Vale backend. Requires installed Vale.
    Prose(prose::Options),
    /// Evaluate saved evidence without network access. Exit 0/1/2: pass/fail/unknown.
    Check {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        policy: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "error")]
        fail_on: Threshold,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(action: Action) -> Result<u8> {
    match action {
        Action::Semantic(options) => semantic::run(options),
        Action::Prose(options) => prose::run(options),
        Action::Check {
            snapshot,
            policy,
            fail_on,
            json,
        } => {
            let snapshot: Snapshot = serde_json::from_slice(&fs::read(snapshot)?)?;
            let policy = policy
                .map(|p| -> Result<Policy> { Ok(serde_json::from_slice(&fs::read(p)?)?) })
                .transpose()?
                .unwrap_or_default();
            let report = cqlt::evaluate(&snapshot, &policy).map_err(failure)?;
            let threshold = match fail_on {
                Threshold::Error => Severity::Error,
                Threshold::Warning => Severity::Warning,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "{}: {} organizations, {} repositories",
                    report.ruleset, report.organizations, report.repositories
                );
                for c in &report.checks {
                    if matches!(c.status, Status::Fail | Status::Unknown | Status::Waived) {
                        // Debug escaping prevents remote descriptions injecting terminal controls.
                        println!(
                            "{:?} {:?} {} {}: {:?}",
                            c.status, c.severity, c.subject, c.rule, c.evidence
                        );
                    }
                }
                println!(
                    "snapshot={} policy={}",
                    report.snapshot_sha256, report.policy_sha256
                );
            }
            Ok(report.exit_code(threshold))
        }
    }
}

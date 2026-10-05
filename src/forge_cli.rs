//! User-facing policy inspection and exact-revision Git transport.
use ccid::{
    forge::{execution_decision, Policy, RunState},
    Environment, Result, Runner,
};
use clap::Subcommand;
use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};

#[derive(Subcommand)]
pub enum Action {
    /// Validate all placements, costs, primary CI/forge relationships and rules.
    Validate {
        #[arg(long)]
        policy: PathBuf,
    },
    /// Ordered clone sources and explicit execution fallback policy; no API writes.
    Plan {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        repository: String,
    },
    /// Evaluate exact-request observations supplied by a provider adapter.
    Decide {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        repository: String,
        #[arg(long)]
        observations: PathBuf,
        #[arg(long = "require", required = true)]
        capabilities: Vec<String>,
    },
    /// Fetch one exact commit, trying only declared mirrors, into a NEW directory.
    /// Submodules and LFS payloads require separate exact source staging.
    Clone {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        repository: String,
        #[arg(long)]
        commit: String,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    },
    /// Reconcile declared secondaries from the primary; writes require --apply.
    Sync {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        repository: String,
        #[arg(
            long = "ref",
            required_unless_present = "all_refs",
            conflicts_with = "all_refs"
        )]
        refs: Vec<String>,
        #[arg(long)]
        all_refs: bool,
        #[arg(long = "to", visible_alias = "destination", required = true)]
        destinations: Vec<String>,
        #[arg(long)]
        apply: bool,
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    },
}

fn read(path: PathBuf) -> Result<Policy> {
    let policy: Policy = serde_json::from_slice(&fs::read(path)?)?;
    policy.validate()?;
    Ok(policy)
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::Validate { policy } => {
            let policy = read(policy)?;
            println!(
                "{}",
                serde_json::json!({"valid":true,"repositories":policy.repositories.len()})
            );
        }
        Action::Sync {
            policy,
            repository,
            refs,
            all_refs,
            destinations,
            apply,
            timeout,
        } => {
            let report = ccid::forge_sync::reconcile(
                &read(policy)?,
                &repository,
                &refs,
                all_refs,
                &destinations,
                apply,
                Duration::from_secs(timeout),
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            let blocked = report.replicas.iter().any(|replica| {
                matches!(
                    replica.state,
                    ccid::forge_sync::State::Blocked | ccid::forge_sync::State::Pending
                )
            });
            let source_blocked = matches!(
                report.source_state,
                ccid::forge_sync::State::Blocked | ccid::forge_sync::State::Pending
            );
            if source_blocked || blocked || (apply && !report.completed()) {
                return Err("Reconciliation is incomplete; inspect the JSON report".into());
            }
        }
        Action::Plan { policy, repository } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&read(policy)?.plan(&repository)?)?
            );
        }
        Action::Decide {
            policy,
            repository,
            observations,
            capabilities,
        } => {
            let states: BTreeMap<String, RunState> =
                serde_json::from_slice(&fs::read(observations)?)?;
            println!(
                "{}",
                serde_json::to_string(&execution_decision(
                    &read(policy)?,
                    &repository,
                    &capabilities,
                    &states
                )?)?
            );
        }
        Action::Clone {
            policy,
            repository,
            commit,
            destination,
            timeout,
        } => {
            if ![40, 64].contains(&commit.len())
                || !commit
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            {
                return Err(
                    "Clone requires a complete lowercase commit identity, never a moving branch"
                        .into(),
                );
            }
            let plan = read(policy)?.plan(&repository)?;
            // create_dir is the exclusive ownership claim. Existing data, even
            // an empty directory or dangling symlink, is never replaced.
            fs::create_dir(&destination)?;
            let destination = destination.canonicalize()?;
            let result = clone_sources(&plan.clone_urls, &commit, &destination, timeout);
            if result.is_err() {
                let _ = fs::remove_dir_all(&destination);
            }
            result?;
        }
    }
    Ok(())
}

fn clone_sources(
    urls: &[String],
    commit: &str,
    destination: &std::path::Path,
    timeout: u64,
) -> Result<()> {
    for url in urls {
        // Each failed attempt has its own disposable object database. Never
        // combine incomplete data from different remotes or mutate a mirror.
        let attempt = tempfile::Builder::new()
            .prefix(".ccid-fetch-")
            .tempdir_in(destination)?;
        let mut environment: Environment = std::env::vars_os().collect();
        environment.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
        environment.insert("GIT_LFS_SKIP_SMUDGE".into(), "1".into());
        let runner = Runner::new(
            attempt.path().into(),
            environment,
            Duration::from_secs(timeout),
        )?;
        let git = |args: &[&str], capture| {
            let argv = [
                "git",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "protocol.file.allow=never",
            ]
            .into_iter()
            .chain(args.iter().copied())
            .map(String::from)
            .collect::<Vec<_>>();
            runner.run(&argv, capture)
        };
        git(&["init", "--quiet"], false)?;
        if git(
            &[
                "fetch",
                "--no-recurse-submodules",
                "--no-tags",
                "--depth=1",
                "--",
                url,
                commit,
            ],
            false,
        )
        .is_err()
        {
            if ccid::INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("Clone interrupted".into());
            }
            eprintln!("ccid: exact commit unavailable from {url}; checking next declared source");
            continue;
        }
        if git(&["rev-parse", "FETCH_HEAD^{commit}"], true)?.trim() != commit {
            return Err("Fetched commit identity mismatch".into());
        }
        git(&["checkout", "--quiet", "--detach", commit], false)?;
        git(&["remote", "add", "origin", url], false)?;
        for entry in fs::read_dir(attempt.path())? {
            let entry = entry?;
            fs::rename(entry.path(), destination.join(entry.file_name()))?;
        }
        println!(
            "{}",
            serde_json::json!({"event":"clone","url":url,"commit":commit,"destination":destination,"submodules":false,"lfs":false})
        );
        return Ok(());
    }
    Err("No declared source supplied the requested exact commit".into())
}

//! Worker CPU, memory and deadline allocation.
use crate::{failure, value, Environment, Result};
use serde::Serialize;
use std::{fs, thread};

pub(crate) fn positive(v: &str, name: &str) -> Result<u64> {
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) || v.starts_with('0') {
        return Err(failure(format!("{name} must be a positive integer")));
    }
    Ok(v.parse()?)
}
fn cpu_budget() -> u64 {
    let available = thread::available_parallelism().map_or(1, |n| n.get() as u64);
    if let Ok(quota) = fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        let fields: Vec<_> = quota.split_whitespace().collect();
        if fields.first() == Some(&"max") {
            return available;
        }
        if let [q, p] = fields.as_slice() {
            if let (Ok(q), Ok(p)) = (q.parse::<u64>(), p.parse::<u64>()) {
                if let Some(jobs) = q.checked_div(p) {
                    return available.min(jobs.max(1));
                }
            }
        }
    }
    available.clamp(1, 4)
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Budget {
    pub jobs: u64,
    pub test_threads: u64,
    pub nix_jobs: u64,
    pub nix_cores: u64,
    pub memory_mb: Option<u64>,
    pub timeout: u64,
}
pub fn budget(environment: &Environment) -> Result<Budget> {
    let jobs = value(environment, "CI_JOBS").or_else(|| value(environment, "CARGO_BUILD_JOBS"));
    let mut jobs = positive(&jobs.unwrap_or_else(|| cpu_budget().to_string()), "CI_JOBS")?;
    let memory = value(environment, "CI_MEMORY_MB")
        .map(|v| positive(&v, "CI_MEMORY_MB"))
        .transpose()?;
    if let Some(per_job) = value(environment, "CI_MEMORY_PER_JOB_MB") {
        let per_job = positive(&per_job, "CI_MEMORY_PER_JOB_MB")?;
        let allocation =
            memory.ok_or_else(|| failure("CI_MEMORY_PER_JOB_MB requires CI_MEMORY_MB"))?;
        if allocation < per_job {
            return Err(failure("Memory allocation cannot fit one job"));
        }
        jobs = jobs.min(allocation / per_job);
    }
    let nix_jobs = positive(
        &value(environment, "CI_NIX_JOBS").unwrap_or_else(|| "1".into()),
        "CI_NIX_JOBS",
    )?;
    if nix_jobs > jobs {
        return Err(failure("CI_NIX_JOBS exceeds the allocated CPU budget"));
    }
    Ok(Budget {
        jobs,
        test_threads: positive(
            &value(environment, "CI_TEST_THREADS").unwrap_or_else(|| jobs.to_string()),
            "CI_TEST_THREADS",
        )?,
        nix_jobs,
        nix_cores: jobs / nix_jobs,
        memory_mb: memory,
        timeout: positive(
            &value(environment, "CI_TIMEOUT").unwrap_or_else(|| "2700".into()),
            "CI_TIMEOUT",
        )?,
    })
}

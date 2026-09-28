//! Bounded host adapter for cqlt's pure Jev protocol.
use super::failure;
use ccid::{Environment, Result, Runner};
use clap::Args;
use cqlt::semantic::{Case, Plan, Recorded, MAX_CASES, MAX_RESPONSE_BYTES};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Args)]
pub struct Options {
    /// JSON array of explicit {subject,state} cases.
    #[arg(long)]
    input: PathBuf,
    /// Replay a JSON array of exact recorded responses without network access.
    #[arg(long, conflicts_with = "live")]
    replay: Option<PathBuf>,
    /// Send each case to TypeSafe AI; requires TYPESAFE_API_KEY and --record.
    #[arg(long)]
    live: bool,
    /// Private JSON file to retain exact successful responses after each call.
    #[arg(long, requires = "live")]
    record: Option<PathBuf>,
    /// Maximum allowed HTTP requests for this invocation.
    #[arg(long, requires = "live")]
    max_requests: Option<usize>,
    /// Maximum cumulative serialized request bytes for this invocation.
    #[arg(long, requires = "live")]
    max_request_bytes: Option<usize>,
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=120))]
    timeout: u64,
}

fn read(path: &Path, max: usize) -> Result<String> {
    let file = fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(failure("Semantic input must be a regular file"));
    }
    let mut text = String::new();
    file.take((max + 1) as u64).read_to_string(&mut text)?;
    if text.len() > max {
        return Err(failure("Semantic input exceeds size limit"));
    }
    Ok(text)
}

fn save(path: &Path, records: &[Recorded]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut tmp, records)?;
    tmp.write_all(b"\n")?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    Ok(())
}

fn preflight(
    requests: usize,
    bytes: usize,
    max_requests: Option<usize>,
    max_bytes: Option<usize>,
    has_key: bool,
) -> Result<()> {
    let max_requests = max_requests.ok_or_else(|| failure("--live requires --max-requests"))?;
    let max_bytes = max_bytes.ok_or_else(|| failure("--live requires --max-request-bytes"))?;
    if requests > max_requests || bytes > max_bytes {
        return Err(failure("Planned requests exceed explicit invocation caps"));
    }
    if !has_key {
        return Err(failure("TYPESAFE_API_KEY is required"));
    }
    Ok(())
}

pub fn run(options: Options) -> Result<u8> {
    let cases: Vec<Case> =
        serde_json::from_str(&read(&options.input, MAX_CASES * 16 * 1024 + 65536)?)?;
    let validation_cases = cases.clone();
    let plan = Plan::new(cases).map_err(failure)?;
    let requests = plan.requests();
    if let Some(path) = options.replay {
        let records: Vec<Recorded> =
            serde_json::from_str(&read(&path, MAX_CASES * (MAX_RESPONSE_BYTES + 256))?)?;
        let report = plan.report(&records).map_err(failure)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(0);
    }
    if !options.live {
        for (subject, request, digest) in requests {
            println!(
                "{:?}: request_sha256={} bytes={}",
                subject,
                digest,
                request.len()
            );
        }
        return Ok(0);
    }
    let record_path = options
        .record
        .ok_or_else(|| failure("--live requires --record"))?;
    let total_bytes: usize = requests.iter().map(|(_, r, _)| r.len()).sum();
    preflight(
        requests.len(),
        total_bytes,
        options.max_requests,
        options.max_request_bytes,
        std::env::var_os("TYPESAFE_API_KEY").is_some(),
    )?;
    // Reserve the private destination before spending; a failure here cannot
    // leave paid responses only in memory. Later writes use atomic replacement.
    let parent = record_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    tempfile::NamedTempFile::new_in(parent)?.persist_noclobber(&record_path)?;
    let environment: Environment = std::env::vars_os().collect();
    let runner = Runner::new(
        std::env::current_dir()?,
        environment,
        Duration::from_secs(options.timeout),
    )?
    .with_stderr_events();
    let mut records = Vec::new();
    for (subject, request, digest) in requests {
        let request_file = tempfile::NamedTempFile::new()?;
        fs::write(request_file.path(), request)?;
        let response_file = tempfile::NamedTempFile::new()?;
        // curl expands the inherited key internally; it never appears in argv.
        let args = vec![
            "curl".into(),
            "--disable".into(),
            "--silent".into(),
            "--show-error".into(),
            "--connect-timeout".into(),
            "10".into(),
            "--max-time".into(),
            options.timeout.to_string(),
            "--max-filesize".into(),
            MAX_RESPONSE_BYTES.to_string(),
            "--proto".into(),
            "=https".into(),
            "--variable".into(),
            "%TYPESAFE_API_KEY".into(),
            "--expand-header".into(),
            "Authorization: Bearer {{TYPESAFE_API_KEY}}".into(),
            "--header".into(),
            "Content-Type: application/json".into(),
            "--output".into(),
            response_file.path().to_string_lossy().into_owned(),
            "--write-out".into(),
            "%{http_code}".into(),
            "--data-binary".into(),
            format!("@{}", request_file.path().display()),
            "--url".into(),
            "https://api.typesafe.ai/v1/systemone".into(),
        ];
        let status = runner.run(&args, true)?;
        if status != "200" {
            return Err(failure(format!(
                "TypeSafe returned HTTP {status}; {} successful responses retained",
                records.len()
            )));
        }
        let response = read(response_file.path(), MAX_RESPONSE_BYTES)?;
        records.push(Recorded {
            subject: subject.to_owned(),
            request_sha256: digest,
            response,
        });
        save(&record_path, &records)?;
        let case = validation_cases
            .iter()
            .find(|c| c.subject == subject)
            .expect("planned subject");
        Plan::new(vec![case.clone()])
            .map_err(failure)?
            .report(&records[records.len() - 1..])
            .map_err(failure)?;
    }
    let report = plan.report(&records).map_err(failure)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_needs_no_key_or_transport() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("cases.json");
        fs::write(&input, r#"[{"subject":"example","state":{"audience":"maintainers","description":"Offline checks","facts":{}}}]"#).unwrap();
        assert_eq!(
            run(Options {
                input,
                replay: None,
                live: false,
                record: None,
                max_requests: None,
                max_request_bytes: None,
                timeout: 30
            })
            .unwrap(),
            0
        );
    }

    #[test]
    fn live_preflight_rejects_missing_key_and_caps() {
        assert!(preflight(1, 100, Some(1), Some(100), false).is_err());
        assert!(preflight(2, 100, Some(1), Some(100), true).is_err());
        assert!(preflight(1, 101, Some(1), Some(100), true).is_err());
        assert!(preflight(1, 100, None, Some(100), true).is_err());
        assert!(preflight(1, 100, Some(1), Some(100), true).is_ok());
    }
}

//! Thin host adapter. cqlt owns input normalization, Vale rules and reporting.
use super::{failure, Threshold};
use ccid::{Environment, Result, Runner};
use clap::Args;
use cqlt::prose::{Format, Level, Plan, Text, MAX_DOCUMENTS, MAX_INPUT_BYTES};
use std::{fs, io::Read, path::PathBuf, time::Duration};

#[derive(Args)]
pub struct Options {
    /// Check every organization and repository description in saved forge evidence.
    #[arg(long)]
    snapshot: Option<PathBuf>,
    /// Explicit UTF-8 Markdown or text file. Repeat for multiple files.
    #[arg(long = "file")]
    files: Vec<PathBuf>,
    /// JSON array of cqlt prose texts: subject, format (markdown/text), text.
    #[arg(long)]
    input: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "warning")]
    fail_on: Threshold,
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
    timeout: u64,
    #[arg(long)]
    json: bool,
}

fn read(path: &PathBuf) -> Result<String> {
    if !fs::metadata(path)?.is_file() {
        return Err(failure("Prose input must be a regular file"));
    }
    let file = fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(failure("Prose input must be a regular file"));
    }
    let mut text = String::new();
    file.take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_string(&mut text)?;
    if text.len() > MAX_INPUT_BYTES {
        return Err(failure("Prose input exceeds 16 MiB"));
    }
    Ok(text)
}

pub fn run(options: Options) -> Result<u8> {
    let mut documents = Vec::new();
    if let Some(path) = options.snapshot {
        let snapshot = serde_json::from_str(&read(&path)?)?;
        documents.extend(cqlt::prose::descriptions(&snapshot).map_err(failure)?);
    }
    if let Some(path) = options.input {
        documents.extend(serde_json::from_str::<Vec<Text>>(&read(&path)?)?);
    }
    let mut input_bytes: usize = documents.iter().map(|d| d.text.len()).sum();
    for path in options.files {
        if documents.len() >= MAX_DOCUMENTS || input_bytes > MAX_INPUT_BYTES {
            return Err(failure("Prose input exceeds the document or byte limit"));
        }
        let format =
            match path
                .extension()
                .and_then(|x| x.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("md" | "markdown") => Format::Markdown,
                Some("txt") => Format::Text,
                _ => return Err(failure(
                    "Prose files must use .md, .markdown or .txt; use --input for explicit formats",
                )),
            };
        let text = read(&path)?;
        input_bytes = input_bytes.saturating_add(text.len());
        if input_bytes > MAX_INPUT_BYTES {
            return Err(failure("Prose input exceeds 16 MiB"));
        }
        documents.push(Text {
            subject: path
                .to_str()
                .ok_or_else(|| failure("Prose paths must be UTF-8"))?
                .into(),
            format,
            text,
        });
    }
    let plan = Plan::new(documents).map_err(failure)?;
    let workspace = tempfile::tempdir()?;
    for (path, content) in plan.files() {
        let path = workspace.path().join(path);
        fs::create_dir_all(path.parent().expect("generated relative path"))?;
        fs::write(path, content)?;
    }
    let environment: Environment = std::env::vars_os().collect();
    let runner = Runner::new(
        workspace.path().into(),
        environment,
        Duration::from_secs(options.timeout),
    )?
    .with_stderr_events();
    let report = plan
        .run(|argv| runner.run(argv, true).map_err(|e| e.to_string()))
        .map_err(failure)?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "{}: {} documents, {} findings ({})",
            report.ruleset,
            report.documents.len(),
            report.findings.len(),
            report.engine
        );
        for finding in &report.findings {
            println!(
                "{:?} {:?}:{}:{} {}: {:?}",
                finding.severity,
                finding.subject,
                finding.line,
                finding.span[0],
                finding.rule,
                finding.message
            );
        }
        println!(
            "input={} policy={}",
            report.input_sha256, report.policy_sha256
        );
    }
    Ok(report.exit_code(match options.fail_on {
        Threshold::Error => Level::Error,
        Threshold::Warning => Level::Warning,
    }))
}

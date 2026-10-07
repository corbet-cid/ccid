//! `digest`: the default way to read a Crow result.
//!
//! Green pipelines print one line. For failed pipelines each failed step
//! prints only what is needed to act (see `log_digest`), within a fixed line
//! budget, plus the command that prints the full log. A log that Crow no
//! longer retains degrades to the step's recorded exit code.
use super::core::{self, Api};
use super::log_digest;
use super::*;

pub(super) const FAILED: &[&str] = &["failure", "error", "killed"];
const MIN_STEP_LINES: usize = 20;

/// Print options for `digest`.
#[derive(Clone, Copy)]
pub(super) struct Options {
    pub max_lines: usize,
    pub tail: usize,
}

pub(super) fn short(commit: &str) -> &str {
    commit.get(..8).unwrap_or(commit)
}
pub(super) fn seconds(run: &Value) -> Option<u64> {
    let (started, finished) = (number(run, "started"), number(run, "finished"));
    (started > 0 && finished >= started).then_some(finished - started)
}
/// Failed steps of a run as (workflow, step id, step name, state, exit code).
pub(super) fn failed_steps(run: &Value) -> Vec<(String, u64, String, String, i64)> {
    rows(&run["workflows"])
        .iter()
        .flat_map(|w| {
            rows(&w["children"])
                .iter()
                .filter(|s| FAILED.contains(&text(s, "state").as_str()))
                .map(move |s| {
                    (
                        text(w, "name"),
                        number(s, "id"),
                        text(s, "name"),
                        text(s, "state"),
                        s["exit_code"].as_i64().unwrap_or(-1),
                    )
                })
        })
        .collect()
}
/// The one-line verdict of a run, without any log access.
pub(super) fn verdict(repo_id: u64, run: &Value) -> String {
    let status = text(run, "status");
    let id = format!("{repo_id}#{}", number(run, "number"));
    let commit = short(&text(run, "commit")).to_string();
    let names: Vec<String> = rows(&run["workflows"])
        .iter()
        .map(|w| text(w, "name"))
        .collect();
    match status.as_str() {
        "success" => format!(
            "green {id} {commit} {}{}",
            names.join(","),
            seconds(run).map_or(String::new(), |s| format!(" in {s}s"))
        ),
        s if core::ACTIVE.contains(&s) => {
            let steps: Vec<&Value> = rows(&run["workflows"])
                .iter()
                .flat_map(|w| rows(&w["children"]))
                .collect();
            let done = steps
                .iter()
                .filter(|s| core::TERMINAL.contains(&text(s, "state").as_str()))
                .count();
            format!(
                "running {id} {commit} {}: {done}/{} steps done",
                names.join(","),
                steps.len()
            )
        }
        other => format!("red {id} {commit} {}: pipeline {other}", names.join(",")),
    }
}

/// Decode a Crow log into cleaned lines, ordered by line number. Blank
/// entries carry no data and become empty lines.
pub(super) fn log_lines(entries: &Value, redact: &dyn Fn(&str) -> String) -> Vec<String> {
    use base64::Engine;
    let mut ordered: Vec<(u64, String)> = rows(entries)
        .iter()
        .map(|e| {
            let data = e["data"]
                .as_str()
                .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            (number(e, "line"), data)
        })
        .collect();
    ordered.sort_by_key(|(line, _)| *line);
    let joined: Vec<String> = ordered.into_iter().map(|(_, d)| d).collect();
    log_digest::clean(&redact(&joined.join("\n")))
}
/// Fetch and decode one step log; `Err` carries the reason it is unavailable.
pub(super) fn step_log(
    api: &dyn Api,
    redact: &dyn Fn(&str) -> String,
    repo_id: u64,
    number: u64,
    step: u64,
) -> std::result::Result<Vec<String>, String> {
    let entries = api
        .call(&format!("/repos/{repo_id}/logs/{number}/{step}"), None)
        .map_err(|e| format!("log unavailable ({e}); it may have been pruned or be too large"))?;
    if rows(&entries).is_empty() {
        return Err("log is empty or no longer retained".into());
    }
    Ok(log_lines(&entries, redact))
}

/// Digest of one run as print-ready lines.
pub(super) fn digest(
    api: &dyn Api,
    redact: &dyn Fn(&str) -> String,
    repo_id: u64,
    run_number: u64,
    options: Options,
) -> Result<Vec<String>> {
    cli::positive(repo_id, run_number)?;
    let run = api.call(&format!("/repos/{repo_id}/pipelines/{run_number}"), None)?;
    let mut out = vec![verdict(repo_id, &run)];
    let failed = failed_steps(&run);
    if failed.is_empty() {
        return Ok(out);
    }
    let max = options.max_lines.max(MIN_STEP_LINES);
    let shown = failed.len().min(((max - 2) / MIN_STEP_LINES).max(1));
    let per_step = (max - 2) / shown;
    if text(&run, "status") == "success" {
        out[0].push_str(" (a step failed but the pipeline passed)");
    }
    for (workflow, step, name, state, code) in &failed[..shown] {
        let header = format!("FAIL {workflow}/{name} (step {step}) {state}, exit {code}");
        let footer = format!("  full log: crow-ci logs {repo_id} {run_number} {step}");
        match step_log(api, redact, repo_id, run_number, *step) {
            Ok(lines) => out.extend(log_digest::render_step(
                &header,
                &footer,
                &lines,
                per_step,
                options.tail,
            )),
            Err(reason) => out.extend([header, format!("  {reason}"), footer]),
        }
    }
    if failed.len() > shown {
        out.push(format!(
            "also failed: {}",
            failed[shown..]
                .iter()
                .map(|(w, id, n, _, c)| format!("{w}/{n} (step {id}, exit {c})"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    out.truncate(max);
    Ok(out)
}

//! Extractor and `digest` tests on real (trimmed, sanitized) Crow step logs.
use super::*;
use crate::submission::digest::{self, Options};
use crate::submission::log_digest::{self, CacheStats};
use base64::Engine;
use std::collections::HashMap;

const RUSTC: &str = include_str!("fixtures/rustc-error.log");
const CLIPPY: &str = include_str!("fixtures/clippy-duplicate-errors.log");
const TEST_ASSERT: &str = include_str!("fixtures/cargo-test-failed-assert.log");
const TEST_PANIC: &str = include_str!("fixtures/cargo-test-panic.log");
const NIX: &str = include_str!("fixtures/nix-eval-error.log");
const PYTHON: &str = include_str!("fixtures/python-traceback.log");
const UNITTEST: &str = include_str!("fixtures/unittest-failures.log");
const NODE: &str = include_str!("fixtures/node-assertion.log");
const QUOTA: &str = include_str!("fixtures/shell-trace-forge-quota.log");
const TIMEOUT: &str = include_str!("fixtures/moon-timeout.log");
const MISSING: &str = include_str!("fixtures/ccid-missing-file.log");
const GREEN: &str = include_str!("fixtures/green-cache-hit.log");

fn found(log: &str) -> Vec<log_digest::Finding> {
    log_digest::extract(&log_digest::clean(log))
}
fn labelled<'a>(findings: &'a [log_digest::Finding], label: &str) -> &'a log_digest::Finding {
    findings
        .iter()
        .find(|f| f.label.starts_with(label))
        .unwrap_or_else(|| panic!("no {label} finding in {findings:?}"))
}
fn rendered(log: &str, budget: usize) -> Vec<String> {
    log_digest::render_step("HEADER", "FOOTER", &log_digest::clean(log), budget, 30)
}

#[test]
fn rustc_error_keeps_code_location_and_snippet() {
    let findings = found(RUSTC);
    let block = labelled(&findings, "rust error");
    assert!(block.lines[0].starts_with("error[E0562]: `impl Trait` is not allowed"));
    assert!(
        block.lines[1].contains("--> src/door/../../tests/support/native_composition.rs:137:24")
    );
    assert!(block
        .lines
        .iter()
        .any(|l| l.contains("= note: see issue #99697")));
    assert!(!block
        .lines
        .iter()
        .any(|l| l.contains("For more information")));
}

#[test]
fn repeated_compiler_errors_collapse_into_one_block_with_a_count() {
    let findings = found(CLIPPY);
    let block = labelled(&findings, "rust error");
    assert_eq!(block.label, "rust error x3");
    assert_eq!(
        findings
            .iter()
            .filter(|f| f.label.starts_with("rust error"))
            .count(),
        1
    );
    assert!(block.lines.last().unwrap().starts_with("also at: "));
    // The 'could not compile' summary stays in the log tail, not in a block.
    assert!(block.lines.iter().all(|l| !l.contains("could not compile")));
}

#[test]
fn failed_assertion_reports_test_name_result_line_and_panic() {
    let findings = found(TEST_ASSERT);
    let names = labelled(&findings, "failed tests");
    assert_eq!(
        names.lines[0],
        "1 failed: declared_placement_routes_primary_and_preserves_foreign_pins"
    );
    assert!(names.lines[1].starts_with("test result: FAILED. 10 passed; 1 failed"));
    let panic = labelled(&findings, "panic");
    assert!(panic.lines[0].contains("resolve_lookup.rs:695:9"));
    assert!(panic
        .lines
        .iter()
        .any(|l| l.contains("left: String(\"canonical-pointer\")")));
    assert!(panic.lines.iter().all(|l| !l.contains("RUST_BACKTRACE")));
}

#[test]
fn panic_message_follows_the_location_line() {
    let findings = found(TEST_PANIC);
    let panic = labelled(&findings, "panic");
    assert_eq!(panic.lines.len(), 2);
    assert!(panic.lines[1].contains("preinstalled moon binary"));
}

#[test]
fn nix_error_trace_keeps_the_root_cause_line() {
    let findings = found(NIX);
    let block = labelled(&findings, "nix error");
    assert_eq!(block.lines[0], "error:");
    assert!(block
        .lines
        .last()
        .unwrap()
        .contains("error: 'poppler_utils' has been renamed to/replaced by 'poppler-utils'"));
    assert!(block
        .lines
        .iter()
        .any(|l| l.contains("aliases.nix:1957:19")));
}

#[test]
fn python_traceback_ends_with_the_exception() {
    let findings = found(PYTHON);
    let block = labelled(&findings, "python traceback");
    assert_eq!(block.lines[0], "Traceback (most recent call last):");
    assert!(block
        .lines
        .last()
        .unwrap()
        .starts_with("AssertionError: no CI_NETRC credentials"));
}

#[test]
fn unittest_failures_show_name_and_final_assertion_per_test() {
    let findings = found(UNITTEST);
    let blocks: Vec<_> = findings
        .iter()
        .filter(|f| f.label.starts_with("unittest failure"))
        .collect();
    assert!(blocks.len() >= 2, "{findings:?}");
    assert!(blocks[0].lines[0].starts_with("FAIL: test_rejects_orphaned_and_duplicate"));
    assert!(blocks[0]
        .lines
        .last()
        .unwrap()
        .starts_with("AssertionError: 'no release package'"));
    // Tracebacks inside a unittest block are not reported a second time.
    assert!(findings.iter().all(|f| f.label != "python traceback"));
}

#[test]
fn node_assertion_is_an_exception_with_message_and_first_frames() {
    let findings = found(NODE);
    let block = labelled(&findings, "exception");
    assert!(
        block.lines[0].starts_with("AssertionError [ERR_ASSERTION]: Issuer init: CryptoProvider")
    );
    assert!(block.lines.iter().any(|l| l.trim() == "false !== true"));
    assert!(block
        .lines
        .iter()
        .any(|l| l.contains("extensions-proof.mjs:56:10")));
}

#[test]
fn forge_quota_is_flagged_as_infrastructure_with_the_traced_command() {
    let findings = found(QUOTA);
    let infra = labelled(&findings, "infrastructure");
    assert!(infra.lines[0].contains("returned error: 429"));
    assert!(infra.lines[1].contains("not a code failure"));
    let shell = labelled(&findings, "last traced shell command");
    assert_eq!(
        shell.lines[0],
        "+ bash kubernetes/ops/scripts/fleet-render.sh"
    );
}

#[test]
fn timeout_is_infrastructure_and_progress_noise_is_dropped() {
    let lines = log_digest::clean(TIMEOUT);
    let findings = log_digest::extract(&lines);
    assert!(labelled(&findings, "infrastructure").lines[1].contains("time budget"));
    assert!(
        lines.iter().all(|l| !l.contains('\u{1b}')),
        "terminal escapes remain"
    );
    let (_, covered) = log_digest::analyze(&lines);
    let tail = log_digest::tail(&lines, &covered, 30);
    assert!(tail.iter().all(|l| !l.contains("Compiling")));
}

#[test]
fn a_failure_without_a_known_pattern_still_shows_the_ccid_exit_chain() {
    let out = rendered(MISSING, 40);
    assert!(found(MISSING).is_empty());
    assert!(out
        .iter()
        .any(|l| l.contains("ccid: No such file or directory (os error 2)")));
    assert!(out
        .iter()
        .any(|l| l.trim() == "[ccid] bash exited 2 after 0.0s"));
    assert!(
        out.iter().all(|l| !l.contains("\"event\"")),
        "raw events leak into the digest"
    );
}

#[test]
fn cache_hit_rate_comes_from_the_structured_events() {
    let lines = log_digest::clean(GREEN);
    assert_eq!(
        log_digest::cache_stats(&lines),
        Some(CacheStats {
            hits: 1,
            requests: 1,
            bypassed: 0
        })
    );
    assert_eq!(log_digest::cache_stats(&log_digest::clean(MISSING)), None);
    let bypass = log_digest::clean(
        "{\"check\":\"verify\",\"event\":\"cache-bypass\",\"execution\":\"uncached\",\"reason\":\"x\"}",
    );
    assert_eq!(
        log_digest::cache_stats(&bypass),
        Some(CacheStats {
            hits: 0,
            requests: 0,
            bypassed: 1
        })
    );
}

#[test]
fn clean_strips_escapes_and_keeps_the_last_carriage_return_segment() {
    let lines = log_digest::clean(
        "\u{1b}[1m\u{1b}[92m   Compiling\u{1b}[0m x v1\nprogress 10%\rprogress 100%\r\n",
    );
    assert_eq!(lines, vec!["   Compiling x v1", "progress 100%"]);
}

#[test]
fn rendering_respects_the_budget_for_huge_logs() {
    let mut huge = String::new();
    for i in 0..3000 {
        huge.push_str(&format!("   Compiling crate{i} v1.0.0\n"));
        huge.push_str(&format!(
            "error[E0425]: cannot find value `v{i}` in this scope\n"
        ));
        huge.push_str(&format!(
            "  --> src/lib.rs:{i}:1\n   |\n{i} | v{i}\n   | ^^ not found\n\n"
        ));
    }
    huge.push_str("error: could not compile `x` (lib) due to 3000 previous errors\n");
    for budget in [6, 20, 60, 118] {
        let out = rendered(&huge, budget);
        assert!(
            out.len() <= budget.max(6),
            "{} lines for {budget}",
            out.len()
        );
        assert_eq!(out.first().unwrap(), "HEADER");
        assert_eq!(out.last().unwrap(), "FOOTER");
    }
    let out = rendered(&huge, 118);
    assert!(out.iter().any(|l| l.contains("cannot find value `v0`")));
}

struct Logs {
    responses: HashMap<String, std::result::Result<Value, String>>,
}
impl Api for Logs {
    fn call(&self, path: &str, _: Option<&Value>) -> Result<Value> {
        match self.responses.get(path) {
            Some(Ok(value)) => Ok(value.clone()),
            Some(Err(message)) => Err(message.clone().into()),
            None => Err(format!("unexpected request {path}").into()),
        }
    }
}
pub(super) fn entries(log: &str) -> Value {
    let engine = base64::engine::general_purpose::STANDARD;
    Value::Array(
        log.lines()
            .enumerate()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|(line, text)| {
                json!({"step_id":7,"line":line,"type":0,"data":if text.is_empty() {Value::Null} else {json!(engine.encode(text))}})
            })
            .collect(),
    )
}
pub(super) fn pipeline(status: &str, steps: &[(u64, &str, &str, i64)]) -> Value {
    json!({"number":9,"status":status,"commit":SHA,"branch":"main","started":100,"finished":160,
        "workflows":[{"id":1,"name":"repository-jobs","state":status,
            "children":steps.iter().map(|(id,name,state,code)|json!({"id":id,"name":name,"state":state,"exit_code":code})).collect::<Vec<_>>()}]})
}
fn api(run: Value, logs: &[(u64, std::result::Result<Value, String>)]) -> Logs {
    let mut responses = HashMap::from([("/repos/5/pipelines/9".to_string(), Ok(run))]);
    for (step, log) in logs {
        responses.insert(format!("/repos/5/logs/9/{step}"), log.clone());
    }
    Logs { responses }
}
fn options() -> Options {
    Options {
        max_lines: 120,
        tail: 30,
    }
}
fn identity(text: &str) -> String {
    text.to_string()
}

#[test]
fn green_pipeline_is_one_line_without_log_access() {
    let api = api(
        pipeline("success", &[(7, "repository-job", "success", 0)]),
        &[],
    );
    let out = digest::digest(&api, &identity, 5, 9, options()).unwrap();
    assert_eq!(out, vec!["green 5#9 01234567 repository-jobs in 60s"]);
}

#[test]
fn failed_pipeline_prints_findings_footer_and_stays_within_the_cap() {
    let run = pipeline(
        "failure",
        &[
            (6, "clone", "success", 0),
            (7, "repository-job", "failure", 2),
        ],
    );
    let api = api(run, &[(7, Ok(entries(TEST_ASSERT)))]);
    let out = digest::digest(
        &api,
        &identity,
        5,
        9,
        Options {
            max_lines: 40,
            tail: 10,
        },
    )
    .unwrap();
    assert!(
        out[0].starts_with("red 5#9 01234567 repository-jobs"),
        "{out:?}"
    );
    assert_eq!(
        out[1],
        "FAIL repository-jobs/repository-job (step 7) failure, exit 2"
    );
    assert!(out
        .iter()
        .any(|l| l.contains("1 failed: declared_placement_routes")));
    assert_eq!(out.last().unwrap(), "  full log: crow-ci logs 5 9 7");
    assert!(out.len() <= 40, "{} lines", out.len());
}

#[test]
fn a_pruned_log_degrades_to_the_recorded_exit_code() {
    let run = pipeline("failure", &[(7, "repository-job", "failure", 2)]);
    let gone = api(
        run.clone(),
        &[(7, Err("Crow request failed: HTTP 404".into()))],
    );
    let out = digest::digest(&gone, &identity, 5, 9, options()).unwrap();
    assert_eq!(out.len(), 4);
    assert!(out[2].contains("log unavailable"), "{out:?}");
    assert!(out[1].ends_with("failure, exit 2"));
    let empty = api(run, &[(7, Ok(json!([])))]);
    let out = digest::digest(&empty, &identity, 5, 9, options()).unwrap();
    assert!(out[2].contains("no longer retained"), "{out:?}");
}

#[test]
fn log_text_is_redacted_before_extraction() {
    let run = pipeline("failure", &[(7, "repository-job", "failure", 1)]);
    let log =
        "+ one\n+ curl -H 'Authorization: Bearer SECRET-VALUE' https://x.invalid\nfatal: boom\n";
    let api = api(run, &[(7, Ok(entries(log)))]);
    let redact = |text: &str| text.replace("SECRET-VALUE", "[redacted]");
    let out = digest::digest(&api, &redact, 5, 9, options())
        .unwrap()
        .join("\n");
    assert!(out.contains("[redacted]") && !out.contains("SECRET-VALUE"));
}

#[test]
fn many_failed_steps_share_the_cap_and_the_rest_is_listed() {
    let steps: Vec<(u64, &str, &str, i64)> = (1..=9).map(|id| (id, "job", "failure", 1)).collect();
    let mut logs = vec![];
    for id in 1..=9 {
        logs.push((id, Ok(entries("error: boom\n"))));
    }
    let api = api(pipeline("failure", &steps), &logs);
    let out = digest::digest(
        &api,
        &identity,
        5,
        9,
        Options {
            max_lines: 60,
            tail: 10,
        },
    )
    .unwrap();
    assert!(out.len() <= 60, "{} lines", out.len());
    assert!(out.last().unwrap().starts_with("also failed: "));
    assert_eq!(out.iter().filter(|l| l.starts_with("FAIL ")).count(), 2);
}

#[test]
fn running_pipeline_reports_progress_on_one_line() {
    let run = pipeline(
        "running",
        &[
            (6, "clone", "success", 0),
            (7, "repository-job", "running", 0),
        ],
    );
    let out = digest::digest(&api(run, &[]), &identity, 5, 9, options()).unwrap();
    assert_eq!(
        out,
        vec!["running 5#9 01234567 repository-jobs: 1/2 steps done"]
    );
}

#[test]
fn digest_rejects_zero_identifiers() {
    let api = api(pipeline("success", &[]), &[]);
    assert!(digest::digest(&api, &identity, 0, 9, options()).is_err());
}

#[test]
fn headline_prefers_findings_then_the_last_informative_line() {
    let lines = log_digest::clean(TEST_ASSERT);
    let (infra, why) = log_digest::headline(&lines).unwrap();
    assert!(
        !infra && why.starts_with("failed tests: 1 failed: declared_placement"),
        "{why}"
    );
    let quota = log_digest::clean(QUOTA);
    let (infra, why) = log_digest::headline(&quota).unwrap();
    assert!(infra && why.contains("rate limit (429/1027)"), "{why}");
    let gate = log_digest::clean(
        "lines: 2312/2568\nbranches: 190/274\n{\"event\":\"command\",\"executable\":\"bash\",\"exit_code\":1,\"seconds\":4.0}\nccid: bash failed: exit status: 2\n",
    );
    assert_eq!(
        log_digest::headline(&gate),
        Some((false, "last output: branches: 190/274".to_string()))
    );
    assert_eq!(log_digest::headline(&log_digest::clean("")), None);
}

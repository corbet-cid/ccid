//! Failure digests for CI step logs.
//!
//! Pure text extractors: given the lines of one failed step they find the
//! few lines an agent needs (the first compiler errors, failed tests and
//! panics, Nix evaluation traces, Python and Node exceptions, the last traced
//! shell command, known infrastructure signatures) and drop everything else.
//! Nothing here performs I/O; `digest.rs` fetches logs and prints.
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

const LINE_WIDTH: usize = 200;
const RUST_BLOCK_LINES: usize = 12;
const PANIC_BLOCK_LINES: usize = 8;
const NAMES_SHOWN: usize = 12;
const LOCATIONS_SHOWN: usize = 4;

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("static digest pattern")
}
static ANSI: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][A-Za-z0-9]")
});
static RUST_ERROR: LazyLock<Regex> = LazyLock::new(|| pattern(r"^error(?:\[E\d{4}\])?: (.+)$"));
static RUST_CONTINUATION: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(?:\s|\.\.\.|\d+ \||(?:note|help)(?:\[\w+\])?: )"));
static RUST_CODE: LazyLock<Regex> = LazyLock::new(|| pattern(r"^error\[E\d{4}\]"));
static LOCATION: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s*--> (.+)$"));
static PANIC: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^thread '.*' (?:\(\d+\) )?panicked at "));
static FAILED_TEST: LazyLock<Regex> = LazyLock::new(|| pattern(r"^test (\S+) \.\.\. FAILED$"));
static TEST_RESULT: LazyLock<Regex> = LazyLock::new(|| pattern(r"^test result: FAILED\."));
static PASSED_TEST: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(?:test .+ \.\.\. (?:ok|ignored)|\s*[✔✓] .*)$"));
static UNITTEST_RULE: LazyLock<Regex> = LazyLock::new(|| pattern(r"^={20,}$"));
static UNITTEST_FAIL: LazyLock<Regex> = LazyLock::new(|| pattern(r"^(?:FAIL|ERROR): .+"));
static UNITTEST_END: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(?:={20,}|FAILED \(|Ran \d+ tests?)"));
static TRACEBACK: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^Traceback \(most recent call last\):$"));
static EXCEPTION: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r"^[A-Za-z][A-Za-z0-9_.]*(?:Error|Exception)(?: \[[A-Z0-9_]+\])?: .+")
});
static NOISE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"^(?:\s+(?:Compiling|Checking|Downloaded|Downloading|Fresh|Updating|Locking|Adding) .*|▮▮▮▮ .*\(running for \d+s\)|running \d+ tests?|failures:|note: run with `RUST_BACKTRACE=.*)$",
    )
});
static MOON_PROGRESS: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^[A-Za-z][A-Za-z0-9_.-]*:[A-Za-z0-9_.-]+ \|(?: |$)"));

/// Known infrastructure signatures: a match means "rerun", not "fix code".
const INFRASTRUCTURE: &[(&str, &str)] = &[
    (
        r"returned error: 429|HTTP[ /]\S* 429|error code: 1027|status code: 429",
        "forge or CDN rate limit (429/1027): not a code failure; rerun after the quota resets",
    ),
    (
        r"Check interrupted or timed out",
        "the check hit its time budget or was interrupted: read the last progress line, not a code error",
    ),
    (
        r"\$HOME differs from euid-obtained home",
        "known rustup HOME flake on toolchain installs: rerun once",
    ),
    (
        r"No space left on device",
        "worker disk is full: not a code failure; report it, do not retry blindly",
    ),
];

/// One group of lines worth showing, in the order they should be printed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Finding {
    pub label: String,
    pub lines: Vec<String>,
}

/// Cache outcome of a ccid check run, read from its structured events.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheStats {
    pub hits: u64,
    pub requests: u64,
    pub bypassed: u64,
}

/// Strip terminal control sequences, keep only the final carriage-return
/// segment of progress lines, trim trailing blanks.
pub fn clean(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|line| {
            let line = line
                .trim_end_matches('\r')
                .rsplit('\r')
                .next()
                .unwrap_or("");
            ANSI.replace_all(line, "").trim_end().to_string()
        })
        .collect()
}

fn shorten(line: &str) -> String {
    let count = line.chars().count();
    if count <= LINE_WIDTH {
        return line.to_string();
    }
    if let Some(at) = line.find("--> ") {
        let head: String = line[..at + 4].chars().take(40).collect();
        let tail: String = line
            .chars()
            .skip(count.saturating_sub(LINE_WIDTH - 50))
            .collect();
        return format!("{head}…{tail}");
    }
    let head: String = line.chars().take(LINE_WIDTH - 1).collect();
    format!("{head}…")
}
fn fit(lines: &[String], limit: usize) -> Vec<String> {
    lines.iter().take(limit).map(|l| shorten(l)).collect()
}
fn continues_rust(line: &str) -> bool {
    !line.is_empty() && RUST_CONTINUATION.is_match(line)
}

enum Event {
    Noise,
    Failure(String),
    Other,
}
/// Classify a ccid structured event line (`{"event": ...}`); anything that is
/// not such a line is `Other`.
fn event(line: &str) -> Event {
    if !(line.starts_with('{') && line.ends_with('}')) {
        return Event::Other;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Event::Other;
    };
    let Some(kind) = value["event"].as_str() else {
        return Event::Other;
    };
    if kind == "command" {
        return match value["exit_code"].as_i64() {
            Some(0) => Event::Noise,
            Some(code) => Event::Failure(format!(
                "[ccid] {} exited {code} after {:.1}s",
                value["executable"].as_str().unwrap_or("command"),
                value["seconds"].as_f64().unwrap_or(0.0)
            )),
            None => Event::Noise,
        };
    }
    if value["success"] == false {
        return Event::Failure(format!(
            "[ccid] {kind} reported failure{}",
            value["check"]
                .as_str()
                .map(|c| format!(" for check {c}"))
                .unwrap_or_default()
        ));
    }
    Event::Noise
}

/// Structured cache outcome of a run: the last `cache-selection` event, or a
/// bypass count when every check ran uncached. `None` without ccid events.
pub fn cache_stats(lines: &[String]) -> Option<CacheStats> {
    let mut stats = None;
    let mut bypassed = 0;
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        match value["event"].as_str() {
            Some("cache-selection") => {
                if let (Some(hits), Some(requests)) = (
                    value["result"]["hits"].as_u64(),
                    value["result"]["requests"].as_u64(),
                ) {
                    stats = Some(CacheStats {
                        hits,
                        requests,
                        bypassed: value["bypassed"].as_array().map_or(0, |b| b.len() as u64),
                    });
                }
            }
            Some("cache-bypass") => bypassed += 1,
            _ => {}
        }
    }
    stats.or((bypassed > 0).then_some(CacheStats {
        hits: 0,
        requests: 0,
        bypassed,
    }))
}

struct Scan<'a> {
    lines: &'a [String],
    covered: Vec<bool>,
    found: Vec<Finding>,
}
impl Scan<'_> {
    fn free(&self, from: usize, to: usize) -> bool {
        !self.covered[from..to.min(self.lines.len())]
            .iter()
            .any(|c| *c)
    }
    fn take(&mut self, from: usize, to: usize) {
        for c in &mut self.covered[from..to.min(self.lines.len())] {
            *c = true;
        }
    }
    fn push(&mut self, label: impl Into<String>, lines: Vec<String>) {
        self.found.push(Finding {
            label: label.into(),
            lines,
        });
    }
}

fn infrastructure(scan: &mut Scan) {
    for (needle, advice) in INFRASTRUCTURE {
        let needle = pattern(needle);
        if let Some(at) = scan.lines.iter().position(|l| needle.is_match(l)) {
            let line = shorten(&scan.lines[at]);
            scan.push("infrastructure", vec![line, format!("-> {advice}")]);
        }
    }
}

fn unittest(scan: &mut Scan) {
    let mut seen: Vec<(String, usize)> = vec![];
    let mut blocks: Vec<(String, Vec<String>)> = vec![];
    let n = scan.lines.len();
    let mut i = 0;
    while i + 2 < n {
        if UNITTEST_RULE.is_match(&scan.lines[i])
            && UNITTEST_FAIL.is_match(&scan.lines[i + 1])
            && scan.free(i, i + 2)
        {
            let mut end = i + 2;
            while end < n && !(end > i + 2 && UNITTEST_END.is_match(&scan.lines[end])) {
                end += 1;
            }
            let body: Vec<String> = scan.lines[i + 2..end]
                .iter()
                .filter(|l| !l.is_empty() && !l.starts_with("-----"))
                .cloned()
                .collect();
            scan.take(i, end);
            let head = scan.lines[i + 1].clone();
            if let Some(entry) = seen.iter_mut().find(|(h, _)| *h == head) {
                entry.1 += 1;
            } else {
                seen.push((head.clone(), 1));
                blocks.push((head, body));
            }
            i = end;
        } else {
            i += 1;
        }
    }
    for (head, body) in blocks {
        let count = seen.iter().find(|(h, _)| *h == head).map_or(1, |(_, c)| *c);
        let keep = body.len().saturating_sub(5);
        let mut lines = vec![shorten(&head)];
        if keep > 0 {
            lines.push(format!("... ({keep} earlier traceback lines omitted)"));
        }
        lines.extend(fit(&body[keep..], 5));
        let label = if count > 1 {
            format!("unittest failure x{count}")
        } else {
            "unittest failure".into()
        };
        scan.push(label, lines);
    }
}

/// Compiler diagnostics with the same header line, collapsed.
struct Group {
    key: String,
    block: Vec<String>,
    locations: Vec<String>,
    count: usize,
    rustc: bool,
}

fn rust_errors(scan: &mut Scan) {
    let mut groups: Vec<Group> = vec![];
    let n = scan.lines.len();
    let mut i = 0;
    while i < n {
        let Some(found) = RUST_ERROR.captures(&scan.lines[i]) else {
            i += 1;
            continue;
        };
        let message = found[1].to_string();
        let mut end = i + 1;
        while end < n && continues_rust(&scan.lines[end]) {
            end += 1;
        }
        let summary = message.starts_with("could not compile")
            || message.starts_with("aborting due to")
            || message.starts_with("test failed")
            || message.starts_with("build failed");
        let nix_style = scan
            .lines
            .get(i + 1)
            .is_some_and(|l| l.starts_with("       ") && !l.starts_with("        "))
            && !scan.lines[i + 1..end.min(i + 4)]
                .iter()
                .any(|l| LOCATION.is_match(l));
        if summary || nix_style || !scan.free(i, end) {
            i = end.max(i + 1);
            continue;
        }
        let block: Vec<String> = scan.lines[i..end].to_vec();
        let location = block
            .iter()
            .find_map(|l| LOCATION.captures(l).map(|c| shorten(&c[1])));
        let location_found = location.is_some();
        scan.take(i, end);
        let key = scan.lines[i].clone();
        if let Some(group) = groups.iter_mut().find(|g| g.key == key) {
            group.count += 1;
            group.locations.extend(location);
        } else {
            let rustc = location_found || RUST_CODE.is_match(&key);
            groups.push(Group {
                key,
                block,
                locations: location.into_iter().collect(),
                count: 1,
                rustc,
            });
        }
        i = end;
    }
    for Group {
        block,
        locations,
        count,
        rustc,
        ..
    } in groups
    {
        let kind = if rustc { "rust error" } else { "error" };
        let mut lines = fit(&block, RUST_BLOCK_LINES);
        if block.len() > RUST_BLOCK_LINES {
            lines.push(format!(
                "... ({} more lines)",
                block.len() - RUST_BLOCK_LINES
            ));
        }
        if count > 1 {
            let shown: Vec<_> = locations
                .iter()
                .skip(1)
                .take(LOCATIONS_SHOWN)
                .cloned()
                .collect();
            let more = locations.len().saturating_sub(1 + shown.len());
            lines.push(format!(
                "also at: {}{}",
                shown.join(", "),
                if more > 0 {
                    format!(" (+{more} more)")
                } else {
                    String::new()
                }
            ));
        }
        scan.push(
            if count > 1 {
                format!("{kind} x{count}")
            } else {
                kind.to_string()
            },
            lines,
        );
    }
}

fn tests_and_panics(scan: &mut Scan) {
    let names: Vec<String> = scan
        .lines
        .iter()
        .filter_map(|l| FAILED_TEST.captures(l).map(|c| c[1].to_string()))
        .collect();
    if !names.is_empty() {
        let mut lines = vec![format!(
            "{} failed: {}{}",
            names.len(),
            names
                .iter()
                .take(NAMES_SHOWN)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            if names.len() > NAMES_SHOWN {
                ", ..."
            } else {
                ""
            }
        )];
        lines.extend(
            scan.lines
                .iter()
                .filter(|l| TEST_RESULT.is_match(l))
                .map(|l| shorten(l)),
        );
        scan.push("failed tests", lines);
    }
    let n = scan.lines.len();
    let mut i = 0;
    while i < n {
        if !(PANIC.is_match(&scan.lines[i]) && scan.free(i, i + 1)) {
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < n
            && end - i < PANIC_BLOCK_LINES
            && !scan.lines[end].is_empty()
            && !scan.lines[end].starts_with("note: run with")
            && !scan.lines[end].starts_with("stack backtrace")
            && !scan.lines[end].starts_with("---- ")
            && !scan.lines[end].starts_with("failures:")
            && !scan.lines[end].starts_with("test ")
        {
            end += 1;
        }
        let block = scan.lines[i..end].to_vec();
        scan.take(i, end);
        scan.push("panic", fit(&block, PANIC_BLOCK_LINES));
        i = end;
    }
}

fn nix_errors(scan: &mut Scan) {
    let n = scan.lines.len();
    let mut i = 0;
    while i < n {
        let line = &scan.lines[i];
        let bare = line == "error:";
        let indented = |at: usize| {
            scan.lines
                .get(at)
                .is_some_and(|l| l.starts_with("       ") && !l.starts_with("        "))
        };
        let headed = line.starts_with("error: ") && indented(i + 1);
        if !(bare || headed) || !scan.free(i, i + 1) {
            i += 1;
            continue;
        }
        let mut end = i + 1;
        let mut last = i;
        while end < n {
            if scan.lines[end].starts_with("       ") {
                last = end;
            } else if !scan.lines[end].is_empty() {
                break;
            }
            end += 1;
        }
        let block: Vec<String> = scan.lines[i..=last]
            .iter()
            .filter(|l| !l.is_empty())
            .cloned()
            .collect();
        scan.take(i, last + 1);
        let lines = if block.len() > 16 {
            let mut shown = fit(&block[..1], 1);
            shown.push(format!("... ({} trace lines omitted)", block.len() - 15));
            shown.extend(fit(&block[block.len() - 14..], 14));
            shown
        } else {
            fit(&block, 16)
        };
        scan.push("nix error", lines);
        i = last + 1;
    }
}

fn exceptions(scan: &mut Scan) {
    let n = scan.lines.len();
    let mut i = 0;
    while i < n {
        if TRACEBACK.is_match(&scan.lines[i]) && scan.free(i, i + 1) {
            let mut end = i + 1;
            while end < n && (scan.lines[end].starts_with(' ') || scan.lines[end].is_empty()) {
                end += 1;
            }
            let end = (end + 1).min(n);
            let block: Vec<String> = scan.lines[i..end]
                .iter()
                .filter(|l| !l.is_empty())
                .cloned()
                .collect();
            scan.take(i, end);
            let lines = if block.len() > 9 {
                let mut shown = fit(&block[..1], 1);
                shown.push(format!("... ({} frame lines omitted)", block.len() - 9));
                shown.extend(fit(&block[block.len() - 8..], 8));
                shown
            } else {
                fit(&block, 9)
            };
            scan.push("python traceback", lines);
            i = end;
        } else if EXCEPTION.is_match(&scan.lines[i]) && scan.free(i, i + 1) {
            let mut end = i + 1;
            let mut shown = vec![scan.lines[i].clone()];
            while end < n && end - i < 10 {
                let line = &scan.lines[end];
                if line.starts_with("Node.js v")
                    || line.starts_with("ccid:")
                    || line.starts_with('{') && line.contains("\"event\"")
                {
                    break;
                }
                if !line.is_empty() {
                    shown.push(line.clone());
                }
                end += 1;
                if shown.len() >= 6 {
                    break;
                }
            }
            scan.take(i, end);
            scan.push("exception", fit(&shown, 6));
            i = end;
        } else {
            i += 1;
        }
    }
}

fn shell(scan: &mut Scan) {
    let traced: Vec<usize> = scan
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("+ "))
        .map(|(i, _)| i)
        .collect();
    if traced.len() < 2 {
        return;
    }
    let from = traced[traced.len() - 1];
    let mut lines = vec![];
    for line in &scan.lines[from..] {
        match event(line) {
            Event::Noise => continue,
            Event::Failure(text) => lines.push(text),
            Event::Other => {
                if !line.is_empty() {
                    lines.push(shorten(line))
                }
            }
        }
        if lines.len() >= 8 {
            break;
        }
    }
    scan.push("last traced shell command", lines);
}

/// Findings in the order they should be printed, plus the mask of source
/// lines those findings already account for.
pub fn analyze(lines: &[String]) -> (Vec<Finding>, Vec<bool>) {
    let mut scan = Scan {
        lines,
        covered: vec![false; lines.len()],
        found: vec![],
    };
    infrastructure(&mut scan);
    unittest(&mut scan);
    rust_errors(&mut scan);
    tests_and_panics(&mut scan);
    nix_errors(&mut scan);
    exceptions(&mut scan);
    shell(&mut scan);
    (scan.found, scan.covered)
}
/// Findings only.
#[cfg(test)]
pub fn extract(lines: &[String]) -> Vec<Finding> {
    analyze(lines).0
}

/// The last `count` informative lines outside `covered`: successful tests,
/// build progress, blank lines and healthy ccid events are dropped, failing
/// ccid events are made readable.
pub fn tail(lines: &[String], covered: &[bool], count: usize) -> Vec<String> {
    let mut kept: Vec<String> = vec![];
    for (at, line) in lines.iter().enumerate() {
        if covered.get(at).copied().unwrap_or(false) || line.is_empty() {
            continue;
        }
        match event(line) {
            Event::Noise => {}
            Event::Failure(text) => kept.push(text),
            Event::Other => {
                if NOISE.is_match(line) || PASSED_TEST.is_match(line) {
                    continue;
                }
                let text = MOON_PROGRESS.replace(line, "");
                if !text.trim().is_empty() {
                    kept.push(shorten(&text));
                }
            }
        }
    }
    let skip = kept.len().saturating_sub(count);
    kept.split_off(skip)
}

/// One-line reason for a failed step: the first finding, otherwise the last
/// informative output line before the ccid exit markers. The flag marks a
/// known infrastructure signature (rerun, not a code failure).
pub fn headline(lines: &[String]) -> Option<(bool, String)> {
    let (findings, covered) = analyze(lines);
    if let Some(first) = findings.first() {
        let infrastructure = first.label == "infrastructure";
        let detail = if infrastructure {
            first.lines.get(1)?.trim_start_matches("-> ")
        } else {
            first.lines.first()?
        };
        return Some((infrastructure, format!("{}: {detail}", first.label)));
    }
    tail(lines, &covered, 8)
        .into_iter()
        .rev()
        .find(|line| !line.starts_with("[ccid]") && !line.starts_with("ccid:"))
        .map(|line| (false, format!("last output: {line}")))
}

/// Print-ready lines for one failed step, at most `budget` lines in total
/// (header and footer included). Findings come first, then the log tail
/// without the lines the findings already showed.
pub fn render_step(
    header: &str,
    footer: &str,
    lines: &[String],
    budget: usize,
    tail_lines: usize,
) -> Vec<String> {
    let budget = budget.max(6);
    let mut out = vec![header.to_string()];
    let (findings, covered) = analyze(lines);
    let tail_lines = tail_lines.min(budget.saturating_sub(4).max(1));
    let reserve = tail_lines + 2;
    let mut room = budget.saturating_sub(2 + reserve.min(budget / 2));
    let mut omitted = 0;
    for finding in &findings {
        let need = finding.lines.len() + 1;
        if need > room {
            omitted += 1;
            continue;
        }
        out.push(format!("  [{}]", finding.label));
        out.extend(finding.lines.iter().map(|l| format!("    {l}")));
        room -= need;
    }
    if omitted > 0 {
        out.push(format!("  ({omitted} more finding(s) omitted for length)"));
    }
    let shown: Vec<String> = tail(lines, &covered, tail_lines + findings.len())
        .into_iter()
        .filter(|l| {
            !out.iter()
                .any(|o| o.trim_start() == l.trim_start() && !l.is_empty())
        })
        .collect();
    let take = shown.len().min(budget.saturating_sub(out.len() + 2));
    if take > 0 {
        out.push(format!("  [last {take} lines]"));
        out.extend(
            shown[shown.len() - take..]
                .iter()
                .map(|l| format!("    {l}")),
        );
    }
    out.push(footer.to_string());
    out
}

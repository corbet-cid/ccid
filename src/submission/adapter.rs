//! Data-only validation of existing reviewed adapter corpora.
use super::*;

pub(super) fn manifest(raw: &str) -> Result<Value> {
    let value: toml::Value = toml::from_str(raw)?;
    let value = serde_json::to_value(value)?;
    if value["schema"] != 1
        || value["checks"]
            .as_object()
            .is_none_or(|o| o.is_empty() || o.keys().any(|k| k.is_empty() || k.contains(',')))
    {
        return Err("Invalid manifest schema or selectors".into());
    }
    Ok(value)
}
pub(super) fn propose_pin(source: &str, pin: &str) -> Result<String> {
    if !exact_sha(pin) {
        return Err("Invalid proposed pin".into());
    }
    let re =
        regex::Regex::new(r#"(?m)^      CCID_REVISION: (?:'([0-9a-f]{40})'|"([0-9a-f]{40})")$"#)?;
    let matches: Vec<_> = re.captures_iter(source).collect();
    if matches.len() != 1 {
        return Err("Expected exactly one immutable runtime pin".into());
    }
    let range = matches[0]
        .get(1)
        .or_else(|| matches[0].get(2))
        .ok_or("Pin missing")?;
    Ok(format!(
        "{}{pin}{}",
        &source[..range.start()],
        &source[range.end()..]
    ))
}
const PROFILES: &[&str] = &[
    "CI_RUST_TOOLCHAIN",
    "CARGO_PROFILE_DEV_DEBUG",
    "CARGO_PROFILE_TEST_DEBUG",
    "CARGO_INCREMENTAL",
];
pub(super) fn reviewed(
    row: &Value,
    variables: &Value,
    steps: &[String],
) -> Result<(String, Value, Vec<String>, Value)> {
    // Historical corpus compatibility, not a runtime policy for new adapters.
    let legacy = row["repository"] == "corbet-labs/ctypst";
    let selector =
        row["reviewed_selector"]
            .as_str()
            .unwrap_or(if legacy { "CHECK_TARGET" } else { "CHECKS" });
    if !["CHECKS", "CHECK_TARGET"].contains(&selector) {
        return Err("Unreviewed selector alias".into());
    }
    let environment=row.get("reviewed_step_environment").cloned().unwrap_or_else(||if legacy {json!({"CI_RUST_TOOLCHAIN":"system","CARGO_PROFILE_DEV_DEBUG":"0","CARGO_PROFILE_TEST_DEBUG":"0","CARGO_INCREMENTAL":"0"})} else {json!({})});
    for (key, value) in environment
        .as_object()
        .ok_or("Invalid reviewed environment")?
    {
        let allowed = match key.as_str() {
            "CI_RUST_TOOLCHAIN" => vec!["system"],
            "CARGO_INCREMENTAL" => vec!["0", "1"],
            "CARGO_PROFILE_DEV_DEBUG" | "CARGO_PROFILE_TEST_DEBUG" => vec!["0", "1", "2"],
            _ => return Err("Unreviewed step environment".into()),
        };
        if !allowed.contains(&value.as_str().unwrap_or("")) {
            return Err("Unreviewed step environment value".into());
        }
    }
    let mut variables = variables.clone();
    let mut steps = steps.to_vec();
    if selector != "CHECKS" {
        variables[selector] = variables
            .as_object_mut()
            .ok_or("Variables missing")?
            .remove("CHECKS")
            .ok_or("Selector missing")?;
        steps = steps
            .iter()
            .map(|l| l.replace("$CHECKS", &format!("${selector}")))
            .collect();
    }
    let index = steps
        .iter()
        .position(|s| s == "    environment:")
        .ok_or("Step environment section missing")?
        + 1;
    steps.splice(
        index..index,
        PROFILES.iter().filter_map(|k| {
            environment
                .get(*k)
                .map(|v| format!("      {k}: {}", encode(v).expect("JSON scalar")))
        }),
    );
    Ok((selector.into(), variables, steps, environment))
}
pub(super) fn parts(source: &str) -> Result<(Value, Vec<String>)> {
    let lines: Vec<String> = source
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| l.trim_end().into())
        .collect();
    if lines.iter().filter(|l| *l == "variables:").count() != 1
        || lines.iter().filter(|l| *l == "steps:").count() != 1
    {
        return Err("Expected one variables and steps section".into());
    }
    let start = lines
        .iter()
        .position(|l| l == "variables:")
        .ok_or("Missing variables")?;
    let end = lines
        .iter()
        .position(|l| l == "steps:")
        .ok_or("Missing steps")?;
    if start >= end
        || lines[..start] != strings(&["when:", "  - event: manual", "skip_clone: true"])
    {
        return Err("Expected manual-only verified-source adapter".into());
    }
    let re = regex::Regex::new(r"^  ([A-Za-z_][A-Za-z_0-9]*): \{default: (.+)\}$")?;
    let mut variables = json!({});
    for line in &lines[start + 1..end] {
        let fields = re
            .captures(line)
            .ok_or("Noncanonical variable declaration")?;
        let name = &fields[1];
        let raw = &fields[2];
        if variables.get(name).is_some() {
            return Err("Duplicate adapter variable".into());
        }
        let value = if raw.starts_with('"') {
            serde_json::from_str::<Value>(raw)?
                .as_str()
                .ok_or("Default must be string")?
                .to_string()
        } else if raw.starts_with('\'') {
            raw.strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
                .ok_or("Unterminated scalar")?
                .replace("''", "'")
        } else {
            if !matches(r"^[A-Za-z0-9_./-]+$", raw) {
                return Err("Noncanonical variable scalar".into());
            }
            raw.into()
        };
        variables[name] = json!(value);
    }
    Ok((variables, lines[end..].to_vec()))
}
fn repository(row: &Value) -> Result<()> {
    if !matches(
        r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$",
        &text(row, "repository"),
    ) || !exact_sha(&text(row, "source_commit"))
        || row["remote_verified"] != true
    {
        return Err("Repository source must be immutable and remotely verified".into());
    }
    Ok(())
}
fn check_selectors(row: &Value, manifest: &Value) -> Result<Vec<String>> {
    let checks = manifest["checks"].as_object().ok_or("Missing checks")?;
    let expected = row["expected_checks"]
        .as_array()
        .ok_or("Missing expected selectors")?;
    if expected
        .iter()
        .any(|v| v.as_str().is_none_or(|c| !checks.contains_key(c)))
    {
        return Err("Expected selector missing from manifest".into());
    }
    Ok(checks.keys().cloned().collect())
}
pub(super) fn custom(
    corpus: &Value,
    mut plan: impl FnMut(&Value, &str, &[String]) -> Result<()>,
) -> Result<usize> {
    let mut seen = BTreeSet::new();
    let mut count = 0;
    for row in rows(&corpus["pin_updates"]) {
        repository(row)?;
        if !seen.insert(text(row, "repository")) {
            return Err("Duplicate custom repository".into());
        }
        let raw = corpus["sources"][text(row, "manifest_sha256")]
            .as_str()
            .ok_or("Missing manifest blob")?;
        let selectors = check_selectors(row, &manifest(raw)?)?;
        let mut paths = BTreeSet::new();
        for workflow in rows(&row["workflows"]) {
            let path = text(workflow, "path");
            if !matches(r"^\.crow/[A-Za-z0-9_-]+\.ya?ml$", &path) || !paths.insert(path) {
                return Err("Invalid or duplicate custom workflow path".into());
            }
            let before = corpus["sources"][text(workflow, "before_sha256")]
                .as_str()
                .ok_or("Missing prior workflow")?;
            let after = corpus["sources"][text(workflow, "after_sha256")]
                .as_str()
                .ok_or("Missing candidate workflow")?;
            if before == after || propose_pin(before, &text(corpus, "tool_commit"))? != after {
                return Err("Custom workflow changes beyond runtime pin".into());
            }
            count += 1;
        }
        plan(row, raw, &selectors)?;
    }
    Ok(count)
}
fn plan(
    binary: &Path,
    root: &Path,
    row: &Value,
    raw: &str,
    checks: &[String],
    environment: &Value,
) -> Result<()> {
    fs::create_dir_all(root)?;
    fs::write(root.join("ccid.toml"), raw)?;
    let mut command = Command::new("timeout");
    command
        .arg("15")
        .arg(binary)
        .args(["check", "--repo"])
        .arg(root)
        .args([
            "--manifest",
            "ccid.toml",
            "--check",
            &checks.join(","),
            "--plan",
        ]);
    for key in [
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET_DIR",
        "CCID_TARGET_LOCK_HELD",
    ] {
        command.env_remove(key);
    }
    command
        .env(
            "CI_REPOSITORY_URL",
            format!("https://github.com/{}", text(row, "repository")),
        )
        .env("CI_COMMIT_SHA", text(row, "source_commit"));
    for (key, value) in environment.as_object().ok_or("Invalid plan environment")? {
        if let Some(value) = value.as_str().filter(|s| !s.is_empty()) {
            command.env(key, value);
        }
    }
    if !command.output()?.status.success() {
        return Err("Pinned ccid manifest plan rejected".into());
    }
    Ok(())
}
fn unescaped(source: &str) -> bool {
    source
        .as_bytes()
        .windows(2)
        .enumerate()
        .any(|(i, w)| w == b"${" && (i == 0 || source.as_bytes()[i - 1] != b'$'))
}
pub(super) fn validate(corpus_path: &Path, expected: &str, candidate: bool) -> Result<()> {
    if !exact_digest(expected) || digest(corpus_path)? != expected {
        return Err("Corpus digest mismatch".into());
    }
    let corpus: Value = serde_json::from_slice(&fs::read(corpus_path)?)?;
    for (hash, value) in corpus["sources"]
        .as_object()
        .ok_or("Missing source corpus")?
    {
        if value.as_str().is_none_or(|s| sha(s) != *hash) {
            return Err("Source corpus blob digest mismatch".into());
        }
    }
    let pin = text(&corpus, "tool_commit");
    if !exact_sha(&pin) {
        return Err("Invalid core pin".into());
    }
    let binary = PathBuf::from(std::env::var("CI_TOOL_BINARY")?);
    if String::from_utf8(output(
        &[
            binary.to_string_lossy().into_owned(),
            "source-revision".into(),
        ],
        None,
        None,
    )?)?
    .trim()
        != pin
    {
        return Err("Running binary differs from corpus pin".into());
    }
    let template = corpus["canonical_template"]
        .as_str()
        .ok_or("Missing canonical template")?;
    let mut archive = tar::Archive::new(File::open(std::env::var("CI_TOOL_ARCHIVE")?)?);
    let mut templates = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_ref() == Path::new("adapters/crow.yaml") {
            if !entry.header().entry_type().is_file() || entry.size() >= 32768 {
                return Err("Invalid template member".into());
            }
            let mut data = String::new();
            entry.read_to_string(&mut data)?;
            templates.push(data);
        }
    }
    if templates != [template] || unescaped(template) {
        return Err("Corpus template differs from verified archive".into());
    }
    let (base_variables, base_steps) = parts(template)?;
    if !base_steps.iter().any(|l|l.contains("check --archive \"$SOURCE_ARCHIVE\" --sha256 \"$SOURCE_SHA256\" --commit \"$CI_COMMIT_SHA\"")) || base_steps.iter().any(|l|l.contains("verify-source") || l.contains("mktemp")) {return Err("Template lacks combined verified archive check".into());}
    let repositories = corpus["repositories"]
        .as_array()
        .ok_or("Missing repository corpus")?;
    if corpus["expected_consumer_count"].as_u64() != Some(repositories.len() as u64) {
        return Err("Consumer count mismatch".into());
    }
    for key in ["repository", "crow_id"] {
        if repositories
            .iter()
            .map(|r| r[key].to_string())
            .collect::<BTreeSet<_>>()
            .len()
            != repositories.len()
        {
            return Err("Duplicate corpus repository identity".into());
        }
    }
    for (key, value) in corpus["worker_environment"]
        .as_object()
        .ok_or("Missing worker environment")?
    {
        if ![
            "CARGO_HOME",
            "RUSTUP_HOME",
            "BUN_INSTALL_CACHE_DIR",
            "npm_config_cache",
            "UV_CACHE_DIR",
            "SCCACHE_DIR",
            "CI_MIN_AVAILABLE_MB",
        ]
        .contains(&key.as_str())
            || std::env::var(key).ok().as_deref() != value.as_str()
        {
            return Err(format!("Worker cache environment mismatch for {key}").into());
        }
    }
    let temporary = tempfile::tempdir()?;
    for row in repositories {
        repository(row)?;
        let published = corpus["sources"][text(row, "workflow_sha256")]
            .as_str()
            .ok_or("Missing workflow source")?;
        let raw = corpus["sources"][text(row, "manifest_sha256")]
            .as_str()
            .ok_or("Missing manifest source")?;
        let yaml = if candidate {
            propose_pin(published, &pin)?
        } else {
            published.into()
        };
        if unescaped(&yaml) {
            return Err("Unescaped Crow interpolation".into());
        }
        let (variables, steps) = parts(&yaml)?;
        let budgets = row
            .get("reviewed_variable_defaults")
            .cloned()
            .unwrap_or(json!({}));
        let mut expected = base_variables.clone();
        for (key, value) in budgets.as_object().ok_or("Invalid budget defaults")? {
            if ![
                "CI_JOBS",
                "CI_TEST_THREADS",
                "CI_MEMORY_MB",
                "CI_MEMORY_PER_JOB_MB",
                "CI_MIN_AVAILABLE_MB",
                "CI_TIMEOUT",
                "CI_NIX_JOBS",
                "CI_LINKER",
            ]
            .contains(&key.as_str())
            {
                return Err("Unreviewed variable override".into());
            }
            expected[key] = value.clone();
        }
        let (selector, expected, steps_expected, mut environment) = reviewed(
            row,
            &expected,
            &base_steps
                .iter()
                .map(|s| s.replace("CCID_PIN", &pin))
                .collect::<Vec<_>>(),
        )?;
        if steps != steps_expected
            || expected
                .as_object()
                .ok_or("Missing expected defaults")?
                .iter()
                .any(|(k, v)| !variables.get(k).is_some_and(|a| k == &selector || a == v))
        {
            return Err("Workflow differs from reviewed core template".into());
        }
        if variables[&selector]
            .as_str()
            .map(|s| json!(s.split(',').collect::<Vec<_>>()))
            != Some(row["expected_checks"].clone())
            || [
                "CARGO_HOME",
                "RUSTUP_HOME",
                "BUN_INSTALL_CACHE_DIR",
                "npm_config_cache",
                "UV_CACHE_DIR",
            ]
            .iter()
            .any(|k| variables.get(*k).is_some())
        {
            return Err("Invalid selector coverage or overridden package cache".into());
        }
        let checks = check_selectors(row, &manifest(raw)?)?;
        for (key, value) in budgets.as_object().ok_or("Invalid budget defaults")? {
            environment[key] = value.clone();
        }
        plan(
            &binary,
            &temporary.path().join(number(row, "crow_id").to_string()),
            row,
            raw,
            &checks,
            &environment,
        )?;
        emit(
            &json!({"repository":row["repository"],"source_commit":row["source_commit"],"crow_id":row["crow_id"],"default_checks":row["expected_checks"],"declared_checks":checks,"manifest_sha256":sha(raw),"workflow_sha256":sha(&yaml),"published_workflow_sha256":sha(published),"status":if candidate {"candidate-pin-validated-against-published-source"}else{"configuration-validated-no-repository-commands-executed"}}),
        )?;
    }
    let mut index = 0;
    let custom_count = custom(&corpus, |row, raw, checks| {
        index += 1;
        plan(
            &binary,
            &temporary.path().join(format!("custom-{index}")),
            row,
            raw,
            checks,
            &json!({}),
        )
    })?;
    emit(
        &json!({"event":"adapter-contracts-success","tool_commit":pin,"candidate":candidate,"validated_consumers":repositories.len(),"custom_pin_updates":custom_count,"corpus_sha256":expected,"worker_environment":corpus["worker_environment"]}),
    )
}

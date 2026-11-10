//! Ports of the four Python gate test files, plus decision-table coverage.
use super::*;
use std::fs;

const ROOT: &str = "/tmp/policy-coverage-fixture";

fn root() -> PathBuf {
    PathBuf::from(ROOT)
}

fn raw() -> serde_json::Value {
    serde_json::json!({
        "type": "llvm.coverage.json.export",
        "data": [{"files": [{
            "filename": format!("{ROOT}/src/lib.rs"),
            "branches": [[1, 1, 1, 2, 2, 1, 0, 0, 4]],
            "summary": {
                "lines": {"count": 2, "covered": 2},
                "branches": {"count": 2, "covered": 2},
            },
        }]}],
    })
}

fn lcov() -> String {
    format!(
        "SF:{ROOT}/src/lib.rs\nDA:1,3\nDA:2,1\nLF:2\nLH:2\n\
         BRDA:1,0,0,2\nBRDA:1,0,1,1\nBRF:2\nBRH:2\nend_of_record\n"
    )
}

fn text() -> String {
    format!("{ROOT}/src/lib.rs:\n 1| 3|source\n 2| 1|source\n")
}

fn bare(
    lcov: &str,
    raw: &serde_json::Value,
    annotated: &str,
) -> std::result::Result<Report, String> {
    gate(lcov, raw, annotated, &root(), &[], Target::Native)
}

fn refused(result: &std::result::Result<Report, String>) -> String {
    result.as_ref().expect_err("gate must refuse").clone()
}

// clbs:.github/test-source-coverage.py

#[test]
fn clbs_complete_source_coverage() {
    let report = bare(&lcov(), &raw(), &text()).expect("complete coverage passes");
    assert_eq!(report.output, ["lines: 2/2", "branches: 2/2"]);
}

#[test]
fn clbs_merged_generic_source_is_not_instantiation_coverage() {
    let mut raw = raw();
    let file = &mut raw["data"][0]["files"][0];
    let arm = file["branches"][0].clone();
    file["branches"]
        .as_array_mut()
        .expect("branches array")
        .push(arm);
    file["summary"]["lines"] = serde_json::json!({"count": 3, "covered": 2});
    file["summary"]["branches"] = serde_json::json!({"count": 4, "covered": 3});
    let report = lcov()
        .replace("LF:2", "LF:3")
        .replace("BRF:2", "BRF:4")
        .replace("BRH:2", "BRH:3");
    let outcome = bare(&report, &raw, &text());
    assert!(outcome.is_ok(), "{outcome:?}");
}

#[test]
fn clbs_no_instrumentable_branches() {
    let value: String = lcov()
        .split('\n')
        .filter(|row| !row.starts_with("BR"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut raw = raw();
    raw["data"][0]["files"][0]["summary"]["branches"] =
        serde_json::json!({"count": 0, "covered": 0});
    raw["data"][0]["files"][0]["branches"] = serde_json::json!([]);
    assert!(bare(&value, &raw, &text()).is_ok());
}

#[test]
fn clbs_refuses_missing_malformed_or_uncovered_records() {
    let base = lcov();
    let bad = [
        String::new(),
        base.replace("DA:2,1\n", ""),
        base.replace("DA:2,1", "DA:2,0"),
        base.replace("BRDA:1,0,1,1", "BRDA:1,0,1,0"),
        base.replace("BRDA:1,0,1,1\n", ""),
        base.replace("DA:2,1", "DA:2,0").replace("LH:2", "LH:1"),
        base.replace("BRDA:1,0,1,1", "BRDA:1,0,1,0")
            .replace("BRH:2", "BRH:1"),
        base.replace("BRDA:1,0,1,1", "BRDA:1,0,1,-")
            .replace("BRH:2", "BRH:1"),
        base.replace("DA:1,3", "DA:1,-1"),
        base.replace("BRDA:1,0,0,2", "BRDA:1,0,0,-2"),
        base.replace("DA:1,3", "DA:1,3\nDA:1,3"),
        base.replace("BRDA:1,0,0,2", "BRDA:1,0,0,2\nBRDA:1,0,0,2"),
        base.replace("LF:2", "LF:3"),
        base.replace("BRH:2", "BRH:1"),
        base.replace("LF:2", "LF:2\nLF:2"),
        base.replace("end_of_record\n", ""),
        base.clone() + &base,
        base.replace("/src/lib.rs", "/tests/lib.rs"),
        base.replace("/src/lib.rs", "/../outside.rs"),
        base.replace("DA:1,3", "DA:0,3"),
        base.clone() + "DA:4,1\n",
        base.replace("LF:2\n", ""),
        base.replace("BRF:2\n", ""),
    ];
    assert_eq!(bad.len(), 23);
    for (index, report) in bad.iter().enumerate() {
        assert!(
            bare(report, &raw(), &text()).is_err(),
            "case {index} must refuse"
        );
    }
}

#[test]
fn clbs_companion_inventory_cannot_be_missing_or_truncated() {
    let mut raw = raw();
    raw["data"][0]["files"]
        .as_array_mut()
        .expect("files array")
        .push(serde_json::json!({
            "filename": format!("{ROOT}/src/missing.rs"),
            "branches": [],
            "summary": {"lines": {"count": 2, "covered": 2}, "branches": {"count": 2, "covered": 2}},
        }));
    for report in [
        serde_json::json!({}),
        serde_json::json!({"type": "other", "data": []}),
        raw,
    ] {
        assert!(bare(&lcov(), &report, &text()).is_err());
    }
}

#[test]
fn clbs_upstream_single_file_report_omits_heading() {
    let heading_free = text().split_once('\n').expect("heading line").1.to_owned();
    assert!(bare(&lcov(), &raw(), &heading_free).is_ok());
}

#[test]
fn clbs_duplicate_raw_source_refused() {
    let mut raw = raw();
    let copy = raw["data"][0]["files"][0].clone();
    raw["data"][0]["files"]
        .as_array_mut()
        .expect("files array")
        .push(copy);
    assert!(bare(&lcov(), &raw, &text()).is_err());
}

#[test]
fn clbs_annotated_inventory_and_hit_consistency() {
    let base = text();
    for report in [
        String::new(),
        base.clone() + &base,
        base.replace(" 2| 1|source\n", ""),
        base.replace("2| 1|", "2| 0|"),
    ] {
        assert!(bare(&lcov(), &raw(), &report).is_err());
    }
}

// cgts:.github/test-source-coverage.py, GateTests (same algorithm as clbs)

#[test]
fn cgts_complete_source_coverage() {
    assert!(bare(&lcov(), &raw(), &text()).is_ok());
}

#[test]
fn cgts_merged_generic_source_is_not_instantiation_coverage() {
    let mut raw = raw();
    let file = &mut raw["data"][0]["files"][0];
    let arm = file["branches"][0].clone();
    file["branches"]
        .as_array_mut()
        .expect("branches array")
        .push(arm);
    file["summary"]["lines"] = serde_json::json!({"count": 3, "covered": 2});
    file["summary"]["branches"] = serde_json::json!({"count": 4, "covered": 3});
    let report = lcov()
        .replace("LF:2", "LF:3")
        .replace("BRF:2", "BRF:4")
        .replace("BRH:2", "BRH:3");
    let outcome = bare(&report, &raw, &text());
    assert!(outcome.is_ok(), "{outcome:?}");
}

#[test]
fn cgts_no_instrumentable_branches() {
    let value: String = lcov()
        .split('\n')
        .filter(|row| !row.starts_with("BR"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut raw = raw();
    raw["data"][0]["files"][0]["summary"]["branches"] =
        serde_json::json!({"count": 0, "covered": 0});
    raw["data"][0]["files"][0]["branches"] = serde_json::json!([]);
    assert!(bare(&value, &raw, &text()).is_ok());
}

#[test]
fn cgts_refuses_missing_malformed_or_uncovered_records() {
    let base = lcov();
    let bad = [
        String::new(),
        base.replace("DA:2,1\n", ""),
        base.replace("DA:2,1", "DA:2,0"),
        base.replace("BRDA:1,0,1,1", "BRDA:1,0,1,0"),
        base.replace("BRDA:1,0,1,1\n", ""),
        base.replace("DA:2,1", "DA:2,0").replace("LH:2", "LH:1"),
        base.replace("BRDA:1,0,1,1", "BRDA:1,0,1,0")
            .replace("BRH:2", "BRH:1"),
        base.replace("BRDA:1,0,1,1", "BRDA:1,0,1,-")
            .replace("BRH:2", "BRH:1"),
        base.replace("DA:1,3", "DA:1,-1"),
        base.replace("BRDA:1,0,0,2", "BRDA:1,0,0,-2"),
        base.replace("DA:1,3", "DA:1,3\nDA:1,3"),
        base.replace("BRDA:1,0,0,2", "BRDA:1,0,0,2\nBRDA:1,0,0,2"),
        base.replace("LF:2", "LF:3"),
        base.replace("BRH:2", "BRH:1"),
        base.replace("LF:2", "LF:2\nLF:2"),
        base.replace("end_of_record\n", ""),
        base.clone() + &base,
        base.replace("/src/lib.rs", "/tests/lib.rs"),
        base.replace("/src/lib.rs", "/../outside.rs"),
        base.replace("DA:1,3", "DA:0,3"),
        base.clone() + "DA:4,1\n",
        base.replace("LF:2\n", ""),
        base.replace("BRF:2\n", ""),
    ];
    for (index, report) in bad.iter().enumerate() {
        assert!(
            bare(report, &raw(), &text()).is_err(),
            "case {index} must refuse"
        );
    }
}

#[test]
fn cgts_companion_inventory_cannot_be_missing_or_truncated() {
    let mut raw = raw();
    raw["data"][0]["files"]
        .as_array_mut()
        .expect("files array")
        .push(serde_json::json!({
            "filename": format!("{ROOT}/src/missing.rs"),
            "branches": [],
            "summary": {"lines": {"count": 2, "covered": 2}, "branches": {"count": 2, "covered": 2}},
        }));
    for report in [
        serde_json::json!({}),
        serde_json::json!({"type": "other", "data": []}),
        raw,
    ] {
        assert!(bare(&lcov(), &report, &text()).is_err());
    }
}

#[test]
fn cgts_upstream_single_file_report_omits_heading() {
    let heading_free = text().split_once('\n').expect("heading line").1.to_owned();
    assert!(bare(&lcov(), &raw(), &heading_free).is_ok());
}

#[test]
fn cgts_duplicate_raw_source_refused() {
    let mut raw = raw();
    let copy = raw["data"][0]["files"][0].clone();
    raw["data"][0]["files"]
        .as_array_mut()
        .expect("files array")
        .push(copy);
    assert!(bare(&lcov(), &raw, &text()).is_err());
}

#[test]
fn cgts_annotated_inventory_and_hit_consistency() {
    let base = text();
    for report in [
        String::new(),
        base.clone() + &base,
        base.replace(" 2| 1|source\n", ""),
        base.replace("2| 1|", "2| 0|"),
    ] {
        assert!(bare(&lcov(), &raw(), &report).is_err());
    }
}

// cgts ExceptionTests: real repository roots with pinned sources.

struct ExceptionFixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    lcov: String,
    raw: serde_json::Value,
    text: String,
}

fn exception_fixture() -> ExceptionFixture {
    let directory = tempfile::tempdir().expect("fixture directory");
    let root = directory.path().to_path_buf();
    fs::create_dir_all(root.join("src")).expect("fixture src");
    fs::write(root.join("src/lib.rs"), "predicate\nmapper\n").expect("fixture source");
    let lcov = format!(
        "SF:{}/src/lib.rs\nDA:1,3\nDA:2,1\nLF:2\nLH:2\n\
         BRDA:1,0,0,2\nBRDA:1,0,1,0\nBRF:2\nBRH:1\nend_of_record\n",
        root.display()
    );
    let raw = serde_json::json!({
        "type": "llvm.coverage.json.export",
        "data": [{"files": [{
            "filename": format!("{}/src/lib.rs", root.display()),
            "branches": [[1, 1, 1, 2, 2, 0, 0, 0, 4]],
            "summary": {
                "lines": {"count": 2, "covered": 2},
                "branches": {"count": 2, "covered": 1},
            },
        }]}],
    });
    let text = format!(
        "{}/src/lib.rs:\n 1| 3|source\n 2| 1|source\n",
        root.display()
    );
    ExceptionFixture {
        _directory: directory,
        root,
        lcov,
        raw,
        text,
    }
}

fn branch_table(block: i64, id: toml::Value) -> toml::Value {
    let mut table = toml::map::Map::new();
    table.insert("block".to_owned(), toml::Value::Integer(block));
    table.insert("id".to_owned(), id);
    toml::Value::Table(table)
}

fn source_map(entries: &[(&str, &str)]) -> toml::Value {
    let mut table = toml::map::Map::new();
    for (name, digest) in entries {
        table.insert(
            (*name).to_owned(),
            toml::Value::String((*digest).to_owned()),
        );
    }
    toml::Value::Table(table)
}

fn branch_exclusion(fixture: &ExceptionFixture) -> Exclusion {
    let bytes = fs::read(fixture.root.join("src/lib.rs")).expect("fixture source");
    let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
    Exclusion {
        file: "src/lib.rs".to_owned(),
        line: 1,
        source: Some("predicate".to_owned()),
        context: None,
        reason: "Fixture invariant".to_owned(),
        evidence: Some("Fixture evidence only".to_owned()),
        targets: vec!["native".to_owned()],
        branch: Some(branch_table(0, toml::Value::Integer(1))),
        evidence_sources: Some(source_map(&[("src/lib.rs", &digest)])),
        evidence_packages: None,
    }
}

fn exception_check(
    fixture: &ExceptionFixture,
    entries: &[Exclusion],
    target: Target,
) -> std::result::Result<Report, String> {
    gate(
        &fixture.lcov,
        &fixture.raw,
        &fixture.text,
        &fixture.root,
        entries,
        target,
    )
}

#[test]
fn cgts_exact_unreachable_branch_keeps_the_other_arm_and_all_inventories() {
    let mut changed = exception_fixture();
    let entry = branch_exclusion(&changed);
    let report =
        exception_check(&changed, &[entry], Target::Native).expect("branch exclusion passes");
    assert!(report
        .output
        .iter()
        .any(|line| line.starts_with("Excluded branch: src/lib.rs:1:0:1")));
    changed.lcov = changed.lcov.replace("BRDA:1,0,0,2\n", "");
    let entry = branch_exclusion(&changed);
    assert!(exception_check(&changed, &[entry], Target::Native).is_err());
}

#[test]
fn cgts_refuses_executed_missing_duplicate_or_unpinned_branch() {
    let fixture = exception_fixture();
    let entry = branch_exclusion(&fixture);
    // Exercised arm, absent arm, negative and boolean coordinates, a branch
    // arm without both coordinates, a missing source hash, an empty reason.
    let mut variants: Vec<Exclusion> = Vec::new();
    let mut exercised = branch_exclusion(&fixture);
    exercised.branch = Some(branch_table(0, toml::Value::Integer(0)));
    variants.push(exercised);
    let mut absent = branch_exclusion(&fixture);
    absent.branch = Some(branch_table(1, toml::Value::Integer(1)));
    variants.push(absent);
    let mut negative = branch_exclusion(&fixture);
    negative.branch = Some(branch_table(0, toml::Value::Integer(-1)));
    variants.push(negative);
    let mut boolean = branch_exclusion(&fixture);
    boolean.branch = Some(branch_table(0, toml::Value::Boolean(true)));
    variants.push(boolean);
    let mut partial = branch_exclusion(&fixture);
    let mut table = toml::map::Map::new();
    table.insert("block".to_owned(), toml::Value::Integer(0));
    partial.branch = Some(toml::Value::Table(table));
    variants.push(partial);
    let mut unpinned = branch_exclusion(&fixture);
    unpinned.evidence_sources = Some(source_map(&[]));
    variants.push(unpinned);
    let mut unreasoned = branch_exclusion(&fixture);
    unreasoned.reason = String::new();
    variants.push(unreasoned);
    let mut moved = branch_exclusion(&fixture);
    moved.source = Some("changed".to_owned());
    variants.push(moved);
    let mut zero = branch_exclusion(&fixture);
    zero.line = 0;
    variants.push(zero);
    for (index, variant) in variants.iter().enumerate() {
        assert!(
            exception_check(&fixture, std::slice::from_ref(variant), Target::Native).is_err(),
            "variant {index} must refuse"
        );
    }
    // The legacy `evidence` field is optional now: an empty evidence string
    // passes where the script refused it.
    let mut no_evidence = branch_exclusion(&fixture);
    no_evidence.evidence = Some(String::new());
    assert!(exception_check(&fixture, &[no_evidence], Target::Native).is_ok());
    // A duplicated branch exclusion refuses.
    assert!(exception_check(&fixture, &[entry.clone(), entry], Target::Native).is_err());
}

#[test]
fn cgts_line_bool_is_not_a_line_at_manifest_level() {
    let parsed: std::result::Result<ManifestFile, _> = toml::from_str(
        "[[coverage.exclusions]]\nfile = \"src/lib.rs\"\nline = true\nsource = \"x\"\nreason = \"y\"\n",
    );
    assert!(parsed.is_err(), "a boolean line is no valid manifest");
}

#[test]
fn cgts_invariant_source_change_elsewhere_invalidates_exception() {
    let fixture = exception_fixture();
    let entry = branch_exclusion(&fixture);
    fs::write(
        fixture.root.join("src/lib.rs"),
        "predicate\nchanged earlier validation\n",
    )
    .expect("rewrite fixture source");
    // The pinned hash no longer matches the rewritten file.
    assert!(exception_check(&fixture, &[entry], Target::Native).is_err());
}

#[test]
fn cgts_wrong_target_cannot_hide_a_missing_arm() {
    let fixture = exception_fixture();
    let mut entry = branch_exclusion(&fixture);
    entry.targets = vec!["wasm".to_owned()];
    assert!(exception_check(&fixture, &[entry], Target::Native).is_err());
}

#[test]
fn cgts_line_exception_cannot_hide_a_branch_or_executed_line() {
    let fixture = exception_fixture();
    let mut line = branch_exclusion(&fixture);
    line.branch = None;
    line.evidence_sources = None;
    assert!(exception_check(&fixture, &[line], Target::Native).is_err());
    let mut fixture = exception_fixture();
    fixture.lcov = fixture
        .lcov
        .replace("DA:2,1", "DA:2,0")
        .replace("LH:2", "LH:1");
    fixture.raw["data"][0]["files"][0]["summary"]["lines"]["covered"] = serde_json::json!(1);
    fixture.text = fixture.text.replace("2| 1|", "2| 0|");
    let branch = branch_exclusion(&fixture);
    let mut second = branch_exclusion(&fixture);
    second.line = 2;
    second.source = Some("mapper".to_owned());
    second.branch = None;
    second.evidence_sources = None;
    let report = exception_check(&fixture, &[branch, second], Target::Native)
        .expect("line exclusion passes");
    assert!(report
        .output
        .iter()
        .any(|line| line.starts_with("Excluded line: src/lib.rs:2")));
    let mut broken = exception_fixture();
    broken.lcov = fixture.lcov.clone();
    broken.raw = fixture.raw.clone();
    broken.text = fixture.text.replace(" 2| 0|source\n", "");
    let branch = branch_exclusion(&broken);
    let mut second = branch_exclusion(&broken);
    second.line = 2;
    second.source = Some("mapper".to_owned());
    second.branch = None;
    second.evidence_sources = None;
    assert!(exception_check(&broken, &[branch, second], Target::Native).is_err());
}

#[test]
fn cgts_unreachable_condition_requires_all_source_bound_arms() {
    let mut fixture = exception_fixture();
    fixture.lcov = fixture
        .lcov
        .replace("DA:1,3", "DA:1,0")
        .replace("LH:2", "LH:1")
        .replace("BRDA:1,0,0,2", "BRDA:1,0,0,0")
        .replace("BRH:1", "BRH:0");
    let file = &mut fixture.raw["data"][0]["files"][0];
    file["summary"]["lines"]["covered"] = serde_json::json!(1);
    file["summary"]["branches"]["covered"] = serde_json::json!(0);
    file["branches"][0][4] = serde_json::json!(0);
    fixture.text = fixture.text.replace("1| 3|", "1| 0|");
    let branch = branch_exclusion(&fixture);
    let mut line = branch_exclusion(&fixture);
    line.branch = None;
    line.evidence_sources = None;
    let mut first = branch_exclusion(&fixture);
    first.branch = Some(branch_table(0, toml::Value::Integer(0)));
    for entries in [
        vec![line.clone()],
        vec![line.clone(), first.clone()],
        vec![line.clone(), branch.clone()],
    ] {
        assert!(exception_check(&fixture, &entries, Target::Native).is_err());
    }
    let report = exception_check(&fixture, &[line, first, branch], Target::Native)
        .expect("line plus both arms passes");
    assert!(report
        .output
        .iter()
        .any(|row| row.starts_with("Excluded line: src/lib.rs:1")));
    let mut dropped = exception_fixture();
    dropped.lcov = fixture.lcov.replace("BRDA:1,0,0,0\n", "");
    dropped.raw = fixture.raw.clone();
    dropped.text = fixture.text.clone();
    let branch = branch_exclusion(&dropped);
    let mut line = branch_exclusion(&dropped);
    line.branch = None;
    line.evidence_sources = None;
    let mut first = branch_exclusion(&dropped);
    first.branch = Some(branch_table(0, toml::Value::Integer(0)));
    assert!(exception_check(&dropped, &[line, first, branch], Target::Native).is_err());
}

fn proof_lock(root: &Path, source: &str) -> String {
    let text =
        format!("[[package]]\nname = \"proof\"\nversion = \"0.0.0\"\nsource = \"{source}\"\n");
    fs::write(root.join("Cargo.lock"), &text).expect("fixture lock");
    text
}

fn proof_entry(fixture: &ExceptionFixture, source: &str) -> Exclusion {
    let mut packages = toml::map::Map::new();
    let mut identity = toml::map::Map::new();
    identity.insert(
        "version".to_owned(),
        toml::Value::String("0.0.0".to_owned()),
    );
    identity.insert("source".to_owned(), toml::Value::String(source.to_owned()));
    packages.insert("proof".to_owned(), toml::Value::Table(identity));
    let mut entry = branch_exclusion(fixture);
    entry.evidence_packages = Some(toml::Value::Table(packages));
    entry
}

#[test]
fn cgts_exact_proof_dependency_ignores_unrelated_packages() {
    let fixture = exception_fixture();
    let source = "git+https://forge.example/org/proof?branch=main#".to_owned() + &"a".repeat(40);
    let entry = proof_entry(&fixture, &source);
    let text = proof_lock(&fixture.root, &source);
    assert!(exception_check(&fixture, std::slice::from_ref(&entry), Target::Native).is_ok());
    fs::write(
        fixture.root.join("Cargo.lock"),
        text.clone() + "\n[[package]]\nname = \"other\"\nversion = \"2\"\n",
    )
    .expect("extend fixture lock");
    assert!(exception_check(&fixture, &[entry], Target::Native).is_ok());
    fs::write(
        fixture.root.join("Cargo.lock"),
        text.replace(&"a".repeat(40), &"b".repeat(40)),
    )
    .expect("rotate fixture lock");
    assert!(exception_check(&fixture, &[proof_entry(&fixture, &source)], Target::Native).is_err());
}

#[test]
fn cgts_proof_dependency_refuses_missing_duplicate_or_wrong_version() {
    let fixture = exception_fixture();
    let source = "git+https://forge.example/org/proof?branch=main#".to_owned() + &"a".repeat(40);
    let entry = proof_entry(&fixture, &source);
    let text = proof_lock(&fixture.root, &source);
    for changed in [
        "[[package]]\nname=\"other\"\n".to_owned(),
        text.clone() + &text,
        text.replace("0.0.0", "1.0.0"),
    ] {
        fs::write(fixture.root.join("Cargo.lock"), &changed).expect("rewrite fixture lock");
        assert!(
            exception_check(&fixture, std::slice::from_ref(&entry), Target::Native).is_err(),
            "lock {changed:?} must refuse"
        );
    }
}

#[test]
fn cgts_proof_dependency_refuses_weak_or_malformed_identity() {
    let fixture = exception_fixture();
    let source = "git+https://forge.example/org/proof?branch=main#".to_owned() + &"a".repeat(40);
    proof_lock(&fixture.root, &source);
    // A non-table inventory, an empty identity and an identity without both
    // exact fields refuse.
    let mut empty_identity = toml::map::Map::new();
    empty_identity.insert(
        "proof".to_owned(),
        toml::Value::Table(toml::map::Map::new()),
    );
    for value in [
        toml::Value::Array(vec![]),
        toml::Value::Table(empty_identity),
    ] {
        let mut variant = proof_entry(&fixture, &source);
        variant.evidence_packages = Some(value);
        assert!(exception_check(&fixture, &[variant], Target::Native).is_err());
    }
    for (version, identity_source) in [
        (
            toml::Value::String("0.0.0".to_owned()),
            toml::Value::String("branch=main".to_owned()),
        ),
        (
            toml::Value::Boolean(true),
            toml::Value::String("a".repeat(40)),
        ),
    ] {
        let mut table = toml::map::Map::new();
        let mut identity = toml::map::Map::new();
        identity.insert("version".to_owned(), version);
        identity.insert("source".to_owned(), identity_source);
        table.insert("proof".to_owned(), toml::Value::Table(identity));
        let mut variant = proof_entry(&fixture, &source);
        variant.evidence_packages = Some(toml::Value::Table(table));
        assert!(exception_check(&fixture, &[variant], Target::Native).is_err());
    }
}

// cbcn:.github/test-check-source-coverage.py (context exclusions, nine cases)

struct ContextFixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    lcov: String,
    raw: serde_json::Value,
    annotated: String,
}

fn context_fixture() -> ContextFixture {
    let directory = tempfile::tempdir().expect("fixture directory");
    let root = directory.path().to_path_buf();
    fs::create_dir_all(root.join("src")).expect("fixture src");
    fs::create_dir_all(root.join("docs")).expect("fixture docs");
    fs::write(root.join("src/lib.rs"), "first\ndefensive mapping\nthird\n")
        .expect("fixture source");
    let lcov = format!(
        "SF:{}/src/lib.rs\nDA:1,1\nDA:2,0\nLF:2\nLH:1\nBRF:0\nBRH:0\nend_of_record\n",
        root.display()
    );
    let raw = serde_json::json!({
        "type": "llvm.coverage.json.export",
        "data": [{"files": [{
            "filename": format!("{}/src/lib.rs", root.display()),
            "branches": [],
            "summary": {"lines": {"count": 2, "covered": 1}, "branches": {"count": 0, "covered": 0}},
        }]}],
    });
    let annotated = format!(
        "{}/src/lib.rs:\n1| 1| first\n2| 0| defensive mapping\n",
        root.display()
    );
    ContextFixture {
        _directory: directory,
        root,
        lcov,
        raw,
        annotated,
    }
}

fn context_exclusion() -> Exclusion {
    Exclusion {
        file: "src/lib.rs".to_owned(),
        line: 2,
        source: None,
        context: Some(vec![
            "first".to_owned(),
            "defensive mapping".to_owned(),
            "third".to_owned(),
        ]),
        reason: "Concrete infallible serialization".to_owned(),
        evidence: None,
        targets: vec!["native".to_owned()],
        branch: None,
        evidence_sources: None,
        evidence_packages: None,
    }
}

#[test]
fn cbcn_exclusion_integrity_nine_cases() {
    for case in 0..9 {
        let fixture = context_fixture();
        let mut entries = vec![context_exclusion()];
        let mut lcov = fixture.lcov.clone();
        let mut raw = fixture.raw.clone();
        let mut annotated = fixture.annotated.clone();
        match case {
            1 => entries[0].context.as_mut().expect("context")[1] = "changed source".to_owned(),
            2 => entries[0].reason = String::new(),
            3 => entries.push(entries[0].clone()),
            4 => entries[0].line = 3,
            5 => {
                lcov = lcov.replace("DA:2,0", "DA:2,1").replace("LH:1", "LH:2");
                raw["data"][0]["files"][0]["summary"]["lines"]["covered"] = serde_json::json!(2);
                annotated = annotated.replace("2| 0|", "2| 1|");
            }
            6 => {
                lcov = lcov.replace("BRF:0\nBRH:0", "BRDA:2,0,0,1\nBRDA:2,0,1,1\nBRF:2\nBRH:2");
                raw["data"][0]["files"][0]["branches"] = serde_json::json!([[2, 1, 2, 2]]);
                raw["data"][0]["files"][0]["summary"]["branches"] =
                    serde_json::json!({"count": 2, "covered": 2});
            }
            7 => {
                lcov = lcov.replace("DA:2,0\n", "");
            }
            8 => entries.clear(),
            _ => {}
        }
        let result = gate(
            &lcov,
            &raw,
            &annotated,
            &fixture.root,
            &entries,
            Target::Native,
        );
        assert_eq!(result.is_ok(), case == 0, "context case {case}");
        if case == 0 {
            let report = result.expect("context case 0 passes");
            assert_eq!(
                report.output[0],
                "Excluded line: src/lib.rs:2 Concrete infallible serialization"
            );
            assert_eq!(report.output[1], "lines: 1/1; exclusions: 1");
            assert_eq!(report.output[2], "branches: 0/0");
        }
    }
}

// cnvl:.github/scripts/test-coverage.py

#[test]
fn cnvl_complete_source_coverage() {
    let annotated =
        format!("{ROOT}/src/lib.rs:\n    1| 3|first source line\n    2| 1|second source line\n");
    assert!(bare(&lcov(), &raw(), &annotated).is_ok());
}

#[test]
fn cnvl_merged_generic_source_is_not_instantiation_coverage() {
    let annotated =
        format!("{ROOT}/src/lib.rs:\n    1| 3|first source line\n    2| 1|second source line\n");
    let mut raw = raw();
    let file = &mut raw["data"][0]["files"][0];
    let arm = file["branches"][0].clone();
    file["branches"]
        .as_array_mut()
        .expect("branches array")
        .push(arm);
    file["summary"]["lines"] = serde_json::json!({"count": 3, "covered": 2});
    file["summary"]["branches"] = serde_json::json!({"count": 4, "covered": 3});
    let report = lcov()
        .replace("LF:2", "LF:3")
        .replace("BRF:2", "BRF:4")
        .replace("BRH:2", "BRH:3");
    let outcome = bare(&report, &raw, &annotated);
    assert!(outcome.is_ok(), "{outcome:?}");
}

#[test]
fn cnvl_no_instrumentable_branches() {
    let annotated =
        format!("{ROOT}/src/lib.rs:\n    1| 3|first source line\n    2| 1|second source line\n");
    let value: String = lcov()
        .split('\n')
        .filter(|row| !row.starts_with("BR"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut raw = raw();
    raw["data"][0]["files"][0]["summary"]["branches"] =
        serde_json::json!({"count": 0, "covered": 0});
    raw["data"][0]["files"][0]["branches"] = serde_json::json!([]);
    assert!(bare(&value, &raw, &annotated).is_ok());
}

#[test]
fn cnvl_refuses_missing_malformed_or_uncovered_records() {
    let annotated =
        format!("{ROOT}/src/lib.rs:\n    1| 3|first source line\n    2| 1|second source line\n");
    let base = lcov();
    let bad = [
        String::new(),
        base.replace("DA:2,1", "DA:2,0"),
        base.replace("BRDA:1,0,1,1", "BRDA:1,0,1,0"),
        base.replace("BRDA:1,0,1,1\n", ""),
        base.replace("DA:2,1", "DA:2,0").replace("LH:2", "LH:1"),
        base.replace("BRDA:1,0,1,1", "BRDA:1,0,1,0")
            .replace("BRH:2", "BRH:1"),
        base.replace("BRDA:1,0,1,1", "BRDA:1,0,1,-")
            .replace("BRH:2", "BRH:1"),
        base.replace("DA:1,3", "DA:1,-1"),
        base.replace("BRDA:1,0,0,2", "BRDA:1,0,0,-2"),
        base.replace("DA:1,3", "DA:1,3\nDA:1,3"),
        base.replace("BRDA:1,0,0,2", "BRDA:1,0,0,2\nBRDA:1,0,0,2"),
        base.replace("LF:2", "LF:3"),
        base.replace("BRH:2", "BRH:1"),
        base.replace("LF:2", "LF:2\nLF:2"),
        base.replace("end_of_record\n", ""),
        base.clone() + &base,
        base.replace("/src/lib.rs", "/tests/lib.rs"),
        base.replace("/src/lib.rs", "/../outside.rs"),
        base.replace("DA:1,3", "DA:0,3"),
        base.clone() + "DA:4,1\n",
        base.replace("LF:2\n", ""),
        base.replace("BRF:2\n", ""),
    ];
    for (index, report) in bad.iter().enumerate() {
        assert!(
            bare(report, &raw(), &annotated).is_err(),
            "case {index} must refuse"
        );
    }
}

#[test]
fn cnvl_companion_inventory_cannot_be_missing_or_truncated() {
    let annotated =
        format!("{ROOT}/src/lib.rs:\n    1| 3|first source line\n    2| 1|second source line\n");
    let mut raw = raw();
    raw["data"][0]["files"]
        .as_array_mut()
        .expect("files array")
        .push(serde_json::json!({
            "filename": format!("{ROOT}/src/missing.rs"),
            "branches": [],
            "summary": {"lines": {"count": 2, "covered": 2}, "branches": {"count": 2, "covered": 2}},
        }));
    for report in [
        serde_json::json!({}),
        serde_json::json!({"type": "other", "data": []}),
        raw,
    ] {
        assert!(bare(&lcov(), &report, &annotated).is_err());
    }
}

#[test]
fn cnvl_duplicate_companion_inventory_is_rejected() {
    let annotated =
        format!("{ROOT}/src/lib.rs:\n    1| 3|first source line\n    2| 1|second source line\n");
    let mut raw = raw();
    let copy = raw["data"][0]["files"][0].clone();
    raw["data"][0]["files"]
        .as_array_mut()
        .expect("files array")
        .push(copy);
    assert!(bare(&lcov(), &raw, &annotated).is_err());
}

#[test]
fn cnvl_single_source_llvm_output_omits_its_heading() {
    let annotated =
        format!("{ROOT}/src/lib.rs:\n    1| 3|first source line\n    2| 1|second source line\n");
    let heading_free = annotated
        .split_once('\n')
        .expect("heading line")
        .1
        .to_owned();
    assert!(bare(&lcov(), &raw(), &heading_free).is_ok());
}

#[test]
fn cnvl_each_emitted_line_requires_an_independent_location_and_hit() {
    let annotated =
        format!("{ROOT}/src/lib.rs:\n    1| 3|first source line\n    2| 1|second source line\n");
    let base = lcov();
    for (report, shown) in [
        (base.replace("DA:2,1\n", ""), annotated.clone()),
        (base.clone(), String::new()),
        (base.clone(), annotated.replace("2| 1|", "2| 0|")),
        (
            base.clone(),
            annotated.clone() + "    2| 1|duplicate source line\n",
        ),
    ] {
        assert!(bare(&report, &raw(), &shown).is_err());
    }
}

struct DocumentedFixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    lcov: String,
    raw: serde_json::Value,
    annotated: String,
}

fn documented_fixture() -> DocumentedFixture {
    let directory = tempfile::tempdir().expect("fixture directory");
    let root = directory.path().to_path_buf();
    fs::create_dir_all(root.join("src")).expect("fixture src");
    fs::write(
        root.join("src/lib.rs"),
        "first source line\nsecond source line\n",
    )
    .expect("fixture source");
    let lcov = format!(
        "SF:{}/src/lib.rs\nDA:1,3\nDA:2,0\nLF:2\nLH:1\n\
         BRDA:1,0,0,2\nBRDA:1,0,1,1\nBRF:2\nBRH:2\nend_of_record\n",
        root.display()
    );
    let raw = serde_json::json!({
        "type": "llvm.coverage.json.export",
        "data": [{"files": [{
            "filename": format!("{}/src/lib.rs", root.display()),
            "branches": [[1, 1, 1, 2, 2, 1, 0, 0, 4]],
            "summary": {"lines": {"count": 2, "covered": 1}, "branches": {"count": 2, "covered": 2}},
        }]}],
    });
    let annotated = format!(
        "{}/src/lib.rs:\n    1| 3|first source line\n    2| 0|second source line\n",
        root.display()
    );
    DocumentedFixture {
        _directory: directory,
        root,
        lcov,
        raw,
        annotated,
    }
}

fn documented_exclusion(line: i64, source: &str) -> Exclusion {
    Exclusion {
        file: "src/lib.rs".to_owned(),
        line,
        source: Some(source.to_owned()),
        context: None,
        reason: "upstream bound".to_owned(),
        evidence: Some("fixture only".to_owned()),
        targets: vec!["native".to_owned(), "wasm".to_owned()],
        branch: None,
        evidence_sources: None,
        evidence_packages: None,
    }
}

#[test]
fn cnvl_only_documented_uncovered_line_can_be_excluded() {
    let fixture = documented_fixture();
    let report = gate(
        &fixture.lcov,
        &fixture.raw,
        &fixture.annotated,
        &fixture.root,
        &[documented_exclusion(2, "second source line")],
        Target::Native,
    )
    .expect("documented line passes");
    assert_eq!(report.output[1], "lines: 1/1; exclusions: 1");
    // An exercised line, a missing line and a branch line all refuse.
    let full = lcov().replace(ROOT, &fixture.root.display().to_string());
    let full_raw = serde_json::json!({
        "type": "llvm.coverage.json.export",
        "data": [{"files": [{
            "filename": format!("{}/src/lib.rs", fixture.root.display()),
            "branches": [[1, 1, 1, 2, 2, 1, 0, 0, 4]],
            "summary": {"lines": {"count": 2, "covered": 2}, "branches": {"count": 2, "covered": 2}},
        }]}],
    });
    let full_text = format!(
        "{}/src/lib.rs:\n    1| 3|first source line\n    2| 1|second source line\n",
        fixture.root.display()
    );
    fs::write(
        fixture.root.join("src/lib.rs"),
        "first source line\nsecond source line\n",
    )
    .expect("fixture source");
    for entry in [
        documented_exclusion(2, "second source line"),
        documented_exclusion(3, "missing"),
        documented_exclusion(1, "first source line"),
    ] {
        assert!(
            gate(
                &full,
                &full_raw,
                &full_text,
                &fixture.root,
                &[entry],
                Target::Native
            )
            .is_err(),
            "undocumented exclusion must refuse"
        );
    }
}

// Decision-table rows and validation messages.

#[test]
fn decision_heading_free_report_needs_exactly_one_file() {
    let second = format!("{ROOT}/src/extra.rs");
    let two = lcov() + &lcov().replace("src/lib.rs", "src/extra.rs");
    let mut raw = raw();
    raw["data"][0]["files"]
        .as_array_mut()
        .expect("files array")
        .push(serde_json::json!({
            "filename": second,
            "branches": [],
            "summary": {"lines": {"count": 2, "covered": 2}, "branches": {"count": 0, "covered": 0}},
        }));
    let heading_free = " 1| 3|source\n 2| 1|source\n".to_owned();
    assert!(bare(&two, &raw, &heading_free).is_err());
}

#[test]
fn decision_exclusion_needs_exactly_one_of_source_or_context() {
    let fixture = documented_fixture();
    let mut neither = documented_exclusion(2, "second source line");
    neither.source = None;
    assert!(gate(
        &fixture.lcov,
        &fixture.raw,
        &fixture.annotated,
        &fixture.root,
        &[neither],
        Target::Native
    )
    .is_err());
    let mut both = documented_exclusion(2, "second source line");
    both.context = Some(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]);
    assert!(gate(
        &fixture.lcov,
        &fixture.raw,
        &fixture.annotated,
        &fixture.root,
        &[both],
        Target::Native
    )
    .is_err());
}

#[test]
fn decision_context_shape_is_three_lines_from_line_two() {
    let fixture = context_fixture();
    let mut short = context_exclusion();
    short.context = Some(vec!["first".to_owned(), "defensive mapping".to_owned()]);
    assert_eq!(
        refused(&gate(
            &fixture.lcov,
            &fixture.raw,
            &fixture.annotated,
            &fixture.root,
            &[short],
            Target::Native
        )),
        "Invalid exclusion context"
    );
    let mut early = context_exclusion();
    early.line = 1;
    assert_eq!(
        refused(&gate(
            &fixture.lcov,
            &fixture.raw,
            &fixture.annotated,
            &fixture.root,
            &[early],
            Target::Native
        )),
        "Invalid excluded source line"
    );
}

#[test]
fn decision_targets_default_to_native_and_filter_runs() {
    let parsed: ManifestFile = toml::from_str(
        "[[coverage.exclusions]]\nfile = \"src/lib.rs\"\nline = 2\nsource = \"second source line\"\nreason = \"r\"\n",
    )
    .expect("manifest without targets parses");
    assert_eq!(
        parsed.coverage.expect("coverage table").exclusions[0].targets,
        ["native"]
    );
    // A wasm-only entry is validated but does not apply to a native run.
    let fixture = documented_fixture();
    let mut wasm = documented_exclusion(2, "second source line");
    wasm.targets = vec!["wasm".to_owned()];
    assert!(gate(
        &fixture.lcov,
        &fixture.raw,
        &fixture.annotated,
        &fixture.root,
        &[wasm],
        Target::Native
    )
    .is_err());
    // A wasm entry with a moved source refuses even on a native run.
    let mut bad_wasm = documented_exclusion(2, "moved source");
    bad_wasm.targets = vec!["wasm".to_owned()];
    assert_eq!(
        refused(&gate(
            &fixture.lcov,
            &fixture.raw,
            &fixture.annotated,
            &fixture.root,
            &[bad_wasm],
            Target::Native
        )),
        "Excluded source moved or changed; review it"
    );
    // An unknown target refuses.
    let mut unknown = documented_exclusion(2, "second source line");
    unknown.targets = vec!["macos".to_owned()];
    assert_eq!(
        refused(&gate(
            &fixture.lcov,
            &fixture.raw,
            &fixture.annotated,
            &fixture.root,
            &[unknown],
            Target::Native
        )),
        "Invalid exclusion target"
    );
}

#[test]
fn decision_verbatim_refusal_messages() {
    assert_eq!(
        refused(&bare("", &raw(), &text())),
        "Incomplete or empty source coverage inventory"
    );
    assert_eq!(
        refused(&bare(&lcov().replace("LF:2", "LF:3"), &raw(), &text())),
        "Inconsistent LF for src/lib.rs"
    );
    let mut inventory = raw();
    let copy = inventory["data"][0]["files"][0].clone();
    inventory["data"][0]["files"]
        .as_array_mut()
        .expect("files array")
        .push(copy);
    assert_eq!(
        refused(&bare(&lcov(), &inventory, &text())),
        "Duplicate raw source"
    );
    assert_eq!(
        refused(&bare(&lcov(), &raw(), "")),
        "Incomplete emitted line inventory"
    );
    let fixture = documented_fixture();
    fs::write(
        fixture.root.join("src/lib.rs"),
        "first source line\nsecond source line\nthird source line\n",
    )
    .expect("extend fixture source");
    assert_eq!(
        refused(&gate(
            &fixture.lcov,
            &fixture.raw,
            &fixture.annotated,
            &fixture.root,
            &[documented_exclusion(2, "moved source")],
            Target::Native
        )),
        "Excluded source moved or changed; review it"
    );
    assert_eq!(
        refused(&gate(
            &fixture.lcov,
            &fixture.raw,
            &fixture.annotated,
            &fixture.root,
            &[documented_exclusion(3, "third source line")],
            Target::Native
        )),
        "Exclusion absent from actual LLVM evidence"
    );
    let full = lcov().replace(ROOT, &fixture.root.display().to_string());
    let full_raw = serde_json::json!({
        "type": "llvm.coverage.json.export",
        "data": [{"files": [{
            "filename": format!("{}/src/lib.rs", fixture.root.display()),
            "branches": [[1, 1, 1, 2, 2, 1, 0, 0, 4]],
            "summary": {"lines": {"count": 2, "covered": 2}, "branches": {"count": 2, "covered": 2}},
        }]}],
    });
    let full_text = format!(
        "{}/src/lib.rs:\n    1| 3|first source line\n    2| 1|second source line\n",
        fixture.root.display()
    );
    assert_eq!(
        refused(&gate(
            &full,
            &full_raw,
            &full_text,
            &fixture.root,
            &[documented_exclusion(2, "second source line")],
            Target::Native
        )),
        "Excluded line exercised; remove its exception"
    );
}

#[test]
fn decision_unknown_manifest_fields_refuse() {
    let table: std::result::Result<ManifestFile, _> = toml::from_str(
        "[[coverage.exclusions]]\nfile = \"src/lib.rs\"\nline = 1\nsource = \"x\"\nreason = \"y\"\nunknown = 1\n",
    );
    assert!(table.is_err(), "unknown exclusion fields refuse");
    let table: std::result::Result<ManifestFile, _> = toml::from_str("[coverage]\nunknown = 1\n");
    assert!(table.is_err(), "unknown coverage fields refuse");
}

#[test]
fn decision_production_source_guard() {
    let fixture = documented_fixture();
    let mut outside = documented_exclusion(2, "second source line");
    outside.file = "../outside.rs".to_owned();
    assert_eq!(
        refused(&gate(
            &fixture.lcov,
            &fixture.raw,
            &fixture.annotated,
            &fixture.root,
            &[outside],
            Target::Native
        )),
        "Exclusion must identify production Rust source"
    );
    let mut tests = documented_exclusion(2, "second source line");
    tests.file = "tests/lib.rs".to_owned();
    assert_eq!(
        refused(&gate(
            &fixture.lcov,
            &fixture.raw,
            &fixture.annotated,
            &fixture.root,
            &[tests],
            Target::Native
        )),
        "Exclusion must identify production Rust source"
    );
}

// Totals gate (ckmg plus cmbl --require-sources).

fn totals_raw() -> serde_json::Value {
    serde_json::json!({
        "type": "llvm.coverage.json.export",
        "data": [{"totals": {
            "lines": {"count": 2, "covered": 2},
            "branches": {"count": 2, "covered": 2},
        }}],
    })
}

#[test]
fn totals_complete_counts_pass() {
    assert_eq!(
        totals(&totals_raw(), None).expect("complete totals"),
        ["lines: 2/2", "branches: 2/2"]
    );
}

#[test]
fn totals_uncovered_invalid_or_empty_refuse() {
    let mut uncovered = totals_raw();
    uncovered["data"][0]["totals"]["lines"]["covered"] = serde_json::json!(1);
    assert!(totals(&uncovered, None).is_err());
    let mut invalid = totals_raw();
    invalid["data"][0]["totals"]["lines"]["covered"] = serde_json::json!(3);
    assert_eq!(
        refused(&totals(&invalid, None).map(|lines| Report { output: lines })),
        "Invalid lines counts"
    );
    let mut empty = totals_raw();
    empty["data"][0]["totals"]["lines"] = serde_json::json!({"count": 0, "covered": 0});
    empty["data"][0]["totals"]["branches"] = serde_json::json!({"count": 0, "covered": 0});
    assert_eq!(
        refused(&totals(&empty, None).map(|lines| Report { output: lines })),
        "Empty line coverage cannot pass"
    );
    assert!(totals(&serde_json::json!({}), None).is_err());
}

#[test]
fn totals_require_sources_lists_missing_files() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let root = directory.path();
    fs::create_dir_all(root.join("src")).expect("fixture src");
    fs::write(root.join("src/lib.rs"), "fn main() {}\n").expect("fixture source");
    fs::write(root.join("src/missing.rs"), "fn other() {}\n").expect("fixture source");
    // The walk resolves against the temporary directory without leaving it.
    let raw = serde_json::json!({
        "type": "llvm.coverage.json.export",
        "data": [
            {"totals": {"lines": {"count": 1, "covered": 1}, "branches": {"count": 0, "covered": 0}},
             "files": [{"filename": root.join("src/lib.rs").display().to_string()}]},
        ],
    });
    let error = totals(&raw, Some(root)).expect_err("missing source must refuse");
    assert!(error.starts_with("Missing production sources: "), "{error}");
    assert!(error.contains("src/missing.rs"), "{error}");
}

// Converter shapes and the three committed fixtures.

#[test]
fn convert_file_shape_targets() {
    let legacy = serde_json::json!([
        {"file": "src/a.rs", "line": 1, "source": "x", "reason": "r", "evidence": "e", "target": "wasm"},
        {"file": "src/b.rs", "line": 2, "source": "y", "reason": "r", "targets": ["native", "browser"]},
        {"file": "src/c.rs", "line": 3, "source": "z", "reason": "r"},
    ]);
    let text = serde_json::to_string(&legacy).expect("legacy text");
    let document =
        convert_exclusions(&text, &[Target::Native, Target::Wasm]).expect("conversion passes");
    let parsed: ManifestFile = toml::from_str(&document).expect("output parses back");
    let entries = parsed.coverage.expect("coverage table").exclusions;
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].targets, ["wasm"]);
    assert_eq!(entries[1].targets, ["native", "browser"]);
    assert_eq!(entries[2].targets, ["native", "wasm"]);
    assert_eq!(entries[0].file, "src/a.rs");
}

#[test]
fn convert_path_shapes() {
    let legacy = serde_json::json!([
        {"path": "src/a.rs", "line": 2, "context": ["a", "b", "c"], "reason": "r"},
        {"path": "src/b.rs", "line": 1, "source": "x", "reason": "r"},
    ]);
    let text = serde_json::to_string(&legacy).expect("legacy text");
    let document = convert_exclusions(&text, &[Target::Native]).expect("conversion passes");
    let parsed: ManifestFile = toml::from_str(&document).expect("output parses back");
    let entries = parsed.coverage.expect("coverage table").exclusions;
    assert_eq!(entries[0].file, "src/a.rs");
    assert_eq!(
        entries[0]
            .context
            .as_ref()
            .expect("context shape")
            .as_slice(),
        ["a", "b", "c"]
    );
    assert!(entries[0].source.is_none());
    assert_eq!(
        entries[1].source.as_ref().expect("source shape").as_str(),
        "x"
    );
    assert!(entries[1].context.is_none());
}

#[test]
fn convert_unknown_shape_names_its_index() {
    let legacy = serde_json::json!([
        {"file": "src/a.rs", "line": 1, "source": "x", "reason": "r"},
        {"line": 2},
    ]);
    let text = serde_json::to_string(&legacy).expect("legacy text");
    let error =
        convert_exclusions(&text, &[Target::Native]).expect_err("unknown shape must refuse");
    assert!(error.contains("index 1"), "{error}");
}

#[test]
fn convert_committed_fixtures_round_trip() {
    let defaults = [Target::Native, Target::Wasm];
    let cases = [
        (
            "cgts",
            include_str!("fixtures/cgts-coverage-exclusions.json"),
            10,
        ),
        (
            "cplc",
            include_str!("fixtures/cplc-coverage-exclusions.json"),
            4,
        ),
        (
            "cnvl",
            include_str!("fixtures/cnvl-coverage-exclusions.json"),
            2,
        ),
    ];
    for (name, legacy, count) in cases {
        let document = convert_exclusions(legacy, &defaults).expect("{name} converts");
        let parsed: ManifestFile = toml::from_str(&document).expect("{name} parses back");
        let entries = parsed.coverage.expect("{name} coverage table").exclusions;
        assert_eq!(entries.len(), count, "{name} keeps every entry");
        // Field order follows the schema table: file, line, source/context,
        // reason, evidence, targets, branch, evidence_sources, evidence_packages.
        let file_at = document.find("file = ").expect("{name} has files");
        let line_at = document.find("line = ").expect("{name} has lines");
        assert!(file_at < line_at, "{name} keeps field order");
        // Entries without legacy targets take the caller defaults.
        for entry in &entries {
            assert!(!entry.targets.is_empty(), "{name} assigns targets");
        }
        // Every converted entry validates against stub sources once the
        // hash and pin evidence (which points at the real repositories) is
        // set aside; that evidence is compared verbatim below instead.
        let directory = tempfile::tempdir().expect("fixture directory");
        let root = directory.path();
        let mut stubs: std::collections::BTreeMap<&str, Vec<String>> =
            std::collections::BTreeMap::new();
        for entry in &entries {
            let width = (entry.line as usize).saturating_add(1);
            let rows = stubs.entry(entry.file.as_str()).or_default();
            rows.resize(width.max(rows.len()), String::new());
            if let Some(source) = &entry.source {
                rows[entry.line as usize - 1] = format!("  {source}  ");
            }
            if let Some(context) = &entry.context {
                for (offset, row) in context.iter().enumerate() {
                    rows[entry.line as usize - 2 + offset] = row.clone();
                }
            }
        }
        for (file, rows) in &stubs {
            let path = root.join(file);
            fs::create_dir_all(path.parent().expect("parent")).expect("stub parent");
            fs::write(&path, rows.join("\n")).expect("stub source");
        }
        for entry in &entries {
            let mut stub = Exclusion {
                file: entry.file.clone(),
                line: entry.line,
                source: entry.source.clone(),
                context: entry.context.clone(),
                reason: entry.reason.clone(),
                evidence: entry.evidence.clone(),
                targets: entry.targets.clone(),
                branch: entry.branch.clone(),
                evidence_sources: None,
                evidence_packages: None,
            };
            if stub.branch.is_some() {
                let bytes = fs::read(root.join(&entry.file)).expect("stub bytes");
                let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
                let mut table = toml::map::Map::new();
                table.insert(entry.file.clone(), toml::Value::String(digest));
                stub.evidence_sources = Some(toml::Value::Table(table));
            }
            validate_exclusion(&stub, root)
                .unwrap_or_else(|error| panic!("{name} {} validates: {error}", entry.file));
        }
        // The converter preserves hash and pin evidence verbatim.
        let legacy_parsed: serde_json::Value =
            serde_json::from_str(legacy).expect("{name} legacy parses");
        for (legacy_entry, entry) in legacy_parsed
            .as_array()
            .expect("legacy array")
            .iter()
            .zip(entries.iter())
        {
            if let Some(sources) = legacy_entry.get("evidence_sources") {
                let kept = entry
                    .evidence_sources
                    .as_ref()
                    .expect("{name} keeps evidence_sources");
                for (path, digest) in sources.as_object().expect("sources object") {
                    assert_eq!(
                        kept[path].as_str().expect("digest string"),
                        digest.as_str().expect("legacy digest")
                    );
                }
            }
            if let Some(packages) = legacy_entry.get("evidence_packages") {
                assert!(
                    entry.evidence_packages.is_some(),
                    "{name} keeps evidence_packages"
                );
                let _ = packages;
            }
        }
    }
}

// Command layer: exit codes and manifest handling.

fn write_gate_tree(manifest: Option<&str>) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let directory = tempfile::tempdir().expect("command directory");
    let root = directory.path();
    fs::create_dir_all(root.join("src")).expect("command src");
    fs::create_dir_all(root.join(".ci")).expect("command manifest dir");
    fs::write(
        root.join("src/lib.rs"),
        "first source line\nsecond source line\n",
    )
    .expect("command source");
    let lcov = root.join("coverage.lcov");
    let json = root.join("coverage.json");
    let annotated = root.join("coverage.txt");
    fs::write(
        &lcov,
        format!(
            "SF:{}/src/lib.rs\nDA:1,3\nDA:2,1\nLF:2\nLH:2\n\
             BRDA:1,0,0,2\nBRDA:1,0,1,1\nBRF:2\nBRH:2\nend_of_record\n",
            root.display()
        ),
    )
    .expect("command lcov");
    fs::write(
        &json,
        serde_json::json!({
            "type": "llvm.coverage.json.export",
            "data": [{"files": [{
                "filename": format!("{}/src/lib.rs", root.display()),
                "branches": [[1, 1, 1, 2, 2, 1, 0, 0, 4]],
                "summary": {"lines": {"count": 2, "covered": 2}, "branches": {"count": 2, "covered": 2}},
            }]}],
        })
        .to_string(),
    )
    .expect("command json");
    fs::write(
        &annotated,
        format!(
            "{}/src/lib.rs:\n    1| 3|first source line\n    2| 1|second source line\n",
            root.display()
        ),
    )
    .expect("command annotated");
    if let Some(manifest) = manifest {
        fs::write(root.join(".ci/ccid.toml"), manifest).expect("command manifest");
    }
    (directory, lcov, json, annotated)
}

fn gate_action(root: &Path, lcov: PathBuf, json: PathBuf, annotated: PathBuf) -> Action {
    Action::Gate {
        lcov,
        json,
        annotated,
        target: "native".to_owned(),
        repo: root.to_path_buf(),
        manifest: PathBuf::from(".ci/ccid.toml"),
    }
}

#[test]
fn command_gate_without_manifest_passes() {
    let (_directory, lcov, json, annotated) = write_gate_tree(None);
    let root = _directory.path();
    assert_eq!(
        run(gate_action(root, lcov, json, annotated)).expect("gate passes"),
        0
    );
}

#[test]
fn command_gate_refusal_is_exit_one() {
    let (_directory, lcov, json, annotated) = write_gate_tree(None);
    fs::write(_directory.path().join("coverage.lcov"), "SF:missing\n").expect("break lcov");
    let root = _directory.path();
    assert_eq!(
        run(gate_action(root, lcov, json, annotated)).expect("refusal returns"),
        1
    );
}

#[test]
fn command_unparseable_manifest_is_exit_two() {
    let (_directory, lcov, json, annotated) = write_gate_tree(Some("not = [valid toml\n"));
    let root = _directory.path();
    assert!(run(gate_action(root, lcov, json, annotated)).is_err());
}

#[test]
fn command_unknown_target_is_exit_one() {
    let (_directory, lcov, json, annotated) = write_gate_tree(None);
    let root = _directory.path().to_path_buf();
    assert_eq!(
        run(Action::Gate {
            lcov,
            json,
            annotated,
            target: "macos".to_owned(),
            repo: root,
            manifest: PathBuf::from(".ci/ccid.toml"),
        })
        .expect("unknown target returns"),
        1
    );
}

#[test]
fn command_totals_and_convert() {
    let directory = tempfile::tempdir().expect("command directory");
    let json = directory.path().join("coverage.json");
    fs::write(
        json.clone(),
        serde_json::to_string(&totals_raw()).expect("totals text"),
    )
    .expect("totals file");
    assert_eq!(
        run(Action::Totals {
            json,
            require_sources: None,
        })
        .expect("totals passes"),
        0
    );
    let legacy = directory.path().join("legacy.json");
    fs::write(
        legacy.clone(),
        serde_json::to_string(&serde_json::json!([
            {"path": "src/a.rs", "line": 2, "context": ["a", "b", "c"], "reason": "r"},
        ]))
        .expect("legacy text"),
    )
    .expect("legacy file");
    assert_eq!(
        run(Action::ConvertExclusions {
            from: legacy,
            default_targets: "native,wasm".to_owned(),
        })
        .expect("conversion passes"),
        0
    );
}

// `ccid coverage run`: configuration, commands and the private target.

fn parse_config(text: &str) -> Config {
    toml::from_str::<ManifestFile>(text)
        .expect("coverage table")
        .coverage
        .unwrap_or_default()
}

#[test]
fn run_config_defaults_are_the_clbs_arguments() {
    // clbs:.ci/verify.sh lines 32-34.
    let llvm_cov = "--workspace --all-features --branch --no-cfg-coverage \
                    --no-cfg-coverage-nightly --ignore-filename-regex /tests/";
    for config in [Config::default(), parse_config("[coverage]\n")] {
        assert_eq!(config.llvm_cov_arguments().join(" "), llvm_cov);
        assert_eq!(
            config.report_arguments().join(" "),
            "--branch --ignore-filename-regex /tests/"
        );
        assert!(config.text.is_empty());
        assert!(config.require_sources.is_none());
        assert!(!config.artifacts);
        assert_eq!(config.validate_run(), Ok(vec![Gate::Source]));
    }
}

#[test]
fn run_commands_are_the_clbs_invocations() {
    let shown: Vec<String> = report_commands(Path::new("/nightly/cargo"), &Config::default())
        .iter()
        .map(|command| {
            command
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    assert_eq!(
        shown,
        [
            "/nightly/cargo llvm-cov --locked --workspace --all-features --branch --no-cfg-coverage \
             --no-cfg-coverage-nightly --ignore-filename-regex /tests/ --json --output-path coverage.json",
            "/nightly/cargo llvm-cov report --locked --branch --ignore-filename-regex /tests/ \
             --lcov --output-path coverage.lcov",
            "/nightly/cargo llvm-cov report --locked --branch --ignore-filename-regex /tests/ \
             --text --output-path coverage.txt",
        ]
    );
}

#[test]
fn run_gates_are_an_ordered_nonempty_set_of_known_gates() {
    let gates = |list: &str| parse_config(&format!("[coverage]\ngates = {list}\n")).validate_run();
    assert_eq!(
        gates(r#"["totals", "source"]"#),
        Ok(vec![Gate::Totals, Gate::Source])
    );
    assert_eq!(
        gates(r#"["source", "totals"]"#),
        Ok(vec![Gate::Source, Gate::Totals])
    );
    assert!(gates("[]")
        .expect_err("empty")
        .contains("at least one gate"));
    assert!(gates(r#"["source", "source"]"#)
        .expect_err("duplicate")
        .contains("listed twice"));
    assert!(gates(r#"["lines"]"#)
        .expect_err("unknown")
        .contains("unknown gate"));
}

#[test]
fn run_require_sources_needs_the_totals_gate_and_a_relative_path() {
    let config = |text: &str| parse_config(&format!("[coverage]\n{text}\n")).validate_run();
    assert_eq!(
        config("gates = [\"totals\"]\nrequire_sources = \"src\""),
        Ok(vec![Gate::Totals])
    );
    assert!(config("require_sources = \"src\"")
        .expect_err("no totals gate")
        .contains("needs the totals gate"));
    for path in ["/src", "../src", ""] {
        assert!(
            config(&format!(
                "gates = [\"totals\"]\nrequire_sources = \"{path}\""
            ))
            .is_err(),
            "{path}"
        );
    }
}

#[test]
fn run_refuses_arguments_it_sets_itself() {
    for (key, value) in [
        ("llvm_cov", r#"["--output-path", "elsewhere.json"]"#),
        ("llvm_cov", r#"["--locked"]"#),
        ("llvm_cov", r#"["--target-dir=inside"]"#),
        ("report", r#"["--json"]"#),
        ("text", r#"["--output-dir", "html"]"#),
    ] {
        let error = parse_config(&format!("[coverage]\n{key} = {value}\n"))
            .validate_run()
            .expect_err("owned argument");
        assert!(error.contains("set by ccid coverage run itself"), "{error}");
    }
}

#[test]
fn run_config_still_denies_unknown_keys() {
    assert!(toml::from_str::<ManifestFile>("[coverage]\nllvm-cov = []\n").is_err());
}

#[test]
fn run_private_target_parent_must_be_outside_the_repository() {
    let repo = tempfile::tempdir().expect("repository");
    let repo = repo.path().canonicalize().expect("canonical repository");
    fs::create_dir_all(repo.join("scratch")).expect("scratch");
    for inside in [repo.join("scratch"), repo.clone()] {
        let error = private_target_parent(&inside, &repo).expect_err("inside");
        assert!(
            error.to_string().contains("unsafe coverage target"),
            "{error}"
        );
    }
    assert!(private_target_parent(&repo.join("missing"), &repo).is_err());
    let outside = tempfile::tempdir().expect("outside");
    assert_eq!(
        private_target_parent(outside.path(), &repo).expect("outside passes"),
        outside.path().canonicalize().expect("canonical outside")
    );
    #[cfg(unix)]
    {
        let link = outside.path().join("link");
        std::os::unix::fs::symlink(repo.join("scratch"), &link).expect("link");
        assert!(private_target_parent(&link, &repo).is_err());
    }
}

// cplc (acceptance case 2): LLVM's per-file JSON branch list keeps branch
// regions whose counters are both folded to zero (here: an uninstantiated
// generic, recorded once per compilation of the crate, `functions[].branches`
// empty), while the per-function lists and LCOV drop them. Minimized from
// cplc src/policy.rs:263 (`while pending.len() < limit {`).

const CPLC_SOURCE: &str = "pub fn covered(flag: bool) -> u8 { if flag { 1 } else { 2 } }\n\
    pub fn other() -> u8 { 3 }\n\
    pub async fn upcoming(&self, limit: usize) -> Vec<u8> { while pending.len() < limit {} }\n";

struct CplcCase {
    _directory: tempfile::TempDir,
    root: PathBuf,
    raw: serde_json::Value,
    lcov: String,
    annotated: String,
}

fn cplc_case() -> CplcCase {
    let directory = tempfile::tempdir().expect("cplc fixture");
    let root = directory.path().canonicalize().expect("canonical root");
    fs::create_dir_all(root.join("src")).expect("src");
    fs::write(root.join("src/lib.rs"), CPLC_SOURCE).expect("source");
    let source = format!("{}/src/lib.rs", root.display());
    let executed = serde_json::json!([1, 38, 1, 42, 2, 1, 0, 0, 4]);
    let folded = serde_json::json!([3, 64, 3, 85, 0, 0, 0, 0, 4]);
    let uninstantiated = |crate_hash: &str| {
        serde_json::json!({
            "name": format!("_RNCNvMNtCs{crate_hash}_4cplc6policyINtB4_6PolicypppE8upcoming0B6_"),
            "count": 0,
            "filenames": [source],
            "regions": [[3, 64, 3, 85, 0, 0, 0, 0]],
            "branches": [],
            "mcdc_records": [],
        })
    };
    let raw = serde_json::json!({
        "type": "llvm.coverage.json.export",
        "data": [{
            "files": [{
                "filename": source,
                "branches": [executed, folded, folded],
                "summary": {
                    "lines": {"count": 3, "covered": 2},
                    "branches": {"count": 2, "covered": 2},
                },
            }],
            "functions": [
                {
                    "name": "covered",
                    "count": 3,
                    "filenames": [source],
                    "regions": [[1, 1, 1, 60, 3, 0, 0, 0]],
                    "branches": [executed],
                    "mcdc_records": [],
                },
                {
                    "name": "other",
                    "count": 1,
                    "filenames": [source],
                    "regions": [[2, 1, 2, 27, 1, 0, 0, 0]],
                    "branches": [],
                    "mcdc_records": [],
                },
                uninstantiated("bFxj2X9Ip2E"),
                uninstantiated("0a66LQETvWy"),
            ],
        }],
    });
    let lcov = format!(
        "SF:{source}\nDA:1,3\nDA:2,1\nDA:3,0\nLF:3\nLH:2\n\
         BRDA:1,0,0,2\nBRDA:1,0,1,1\nBRF:2\nBRH:2\nend_of_record\n"
    );
    let rows: Vec<&str> = CPLC_SOURCE.lines().collect();
    let annotated = format!(
        "{source}:\n    1| 3|{}\n    2| 1|{}\n    3| 0|{}\n",
        rows[0], rows[1], rows[2]
    );
    CplcCase {
        _directory: directory,
        root,
        raw,
        lcov,
        annotated,
    }
}

fn cplc_exclusion() -> Exclusion {
    Exclusion {
        file: "src/lib.rs".to_owned(),
        line: 3,
        source: Some(
            CPLC_SOURCE
                .lines()
                .nth(2)
                .expect("line 3")
                .trim()
                .to_owned(),
        ),
        context: None,
        reason: "uninstantiated generic".to_owned(),
        evidence: None,
        targets: vec!["native".to_owned()],
        branch: None,
        evidence_sources: None,
        evidence_packages: None,
    }
}

#[test]
fn cplc_folded_branch_of_an_uninstantiated_generic_is_no_emitted_location() {
    let case = cplc_case();
    let report = gate(
        &case.lcov,
        &case.raw,
        &case.annotated,
        &case.root,
        &[cplc_exclusion()],
        Target::Native,
    )
    .expect("the folded region is not an emitted branch");
    assert_eq!(
        report.output,
        [
            "Excluded line: src/lib.rs:3 uninstantiated generic",
            "lines: 2/2; exclusions: 1",
            "branches: 2/2",
        ]
    );
    // Without the exclusion the uninstantiated lines are a real gap, never a pass.
    let refusal = gate(
        &case.lcov,
        &case.raw,
        &case.annotated,
        &case.root,
        &[],
        Target::Native,
    );
    assert_eq!(
        refused(&refusal),
        "Uncovered source lines: [('src/lib.rs', 3)]; branches: []"
    );
}

#[test]
fn cplc_export_without_function_records_keeps_the_per_file_inventory() {
    let mut case = cplc_case();
    case.raw["data"][0]
        .as_object_mut()
        .expect("unit")
        .remove("functions");
    let refusal = gate(
        &case.lcov,
        &case.raw,
        &case.annotated,
        &case.root,
        &[cplc_exclusion()],
        Target::Native,
    );
    assert_eq!(refused(&refusal), "Missing emitted branch locations");
}

#[test]
fn missing_emitted_branch_location_still_refuses() {
    let case = cplc_case();
    let check = |lcov: &str, raw: &serde_json::Value| {
        refused(&gate(
            lcov,
            raw,
            &case.annotated,
            &case.root,
            &[cplc_exclusion()],
            Target::Native,
        ))
    };
    // LCOV lost the arms of a branch the function records emit.
    let without_arms = case.lcov.replace("BRDA:1,0,0,2\nBRDA:1,0,1,1\n", "");
    assert_eq!(
        check(&without_arms, &case.raw),
        "Missing emitted branch locations"
    );
    // LCOV carries arms for the folded region the function records omit.
    let folded_arms = case
        .lcov
        .replace("BRF:2", "BRDA:3,0,0,-\nBRDA:3,0,1,-\nBRF:2");
    assert_eq!(
        check(&folded_arms, &case.raw),
        "Missing emitted branch locations"
    );
    // A per-file location the function records omit was executed: not folded.
    let mut executed = case.raw.clone();
    executed["data"][0]["files"][0]["branches"][1] =
        serde_json::json!([3, 64, 3, 85, 1, 0, 0, 0, 4]);
    assert_eq!(
        check(&case.lcov, &executed),
        "Missing emitted branch locations"
    );
    // A folded region on an executed line (a never-called closure beside
    // executed code): the line gate cannot see it, so it stays a missing branch.
    let mut beside_executed = case.raw.clone();
    for index in [1, 2] {
        beside_executed["data"][0]["files"][0]["branches"][index] =
            serde_json::json!([2, 10, 2, 20, 0, 0, 0, 0, 4]);
    }
    assert_eq!(
        check(&case.lcov, &beside_executed),
        "Missing emitted branch locations"
    );
    // A folded region on a line LCOV does not report at all.
    let mut unreported = case.raw.clone();
    for index in [1, 2] {
        unreported["data"][0]["files"][0]["branches"][index] =
            serde_json::json!([5, 1, 5, 9, 0, 0, 0, 0, 4]);
    }
    assert_eq!(
        check(&case.lcov, &unreported),
        "Missing emitted branch locations"
    );
    // A per-function location absent from the per-file list.
    let mut unlisted = case.raw.clone();
    unlisted["data"][0]["functions"][1]["branches"] =
        serde_json::json!([[2, 1, 2, 5, 1, 0, 0, 0, 4]]);
    assert_eq!(
        check(&case.lcov, &unlisted),
        "Missing emitted branch locations"
    );
    // A per-function branch naming no file of its record.
    let mut nameless = case.raw.clone();
    nameless["data"][0]["functions"][0]["branches"] =
        serde_json::json!([[1, 38, 1, 42, 2, 1, 5, 5, 4]]);
    assert_eq!(
        check(&case.lcov, &nameless),
        "Missing emitted branch locations"
    );
    // A function record without its branch list.
    let mut listless = case.raw.clone();
    listless["data"][0]["functions"][0]
        .as_object_mut()
        .expect("function")
        .remove("branches");
    assert_eq!(
        check(&case.lcov, &listless),
        "Missing emitted branch locations"
    );
}

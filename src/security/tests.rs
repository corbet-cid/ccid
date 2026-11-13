use super::blocks::{apply, Place, END};
use super::text::{policy_block, readme_block, release_section_of};
use super::*;
use std::fs;

const BLOCK: &str = "<!-- ccid:security:begin (x) -->\nbody\n<!-- ccid:security:end -->";

fn ledger(text: &str) -> Result<Ledger> {
    let ledger: Ledger = toml::from_str(text).map_err(|e| failure(e.to_string()))?;
    ledger.validate()?;
    Ok(ledger)
}

fn own(text: &str) -> Result<OwnFile> {
    let own: OwnFile = toml::from_str(text).map_err(|e| failure(e.to_string()))?;
    own.validate()?;
    Ok(own)
}

const OPEN: &str = r#"
schema = 1
[[advisory]]
id = "GHSA-aaaa-bbbb-cccc"
aliases = ["CVE-2026-0001"]
package = "hyper"
ecosystem = "crates.io"
affected = "0.14.10"
severity = "high"
found = "2026-11-10"
state = "open"
"#;

const FIXED: &str = r#"
[[advisory]]
id = "RUSTSEC-2024-0388"
package = "derivative"
ecosystem = "crates.io"
affected = "2.2.0"
fixed = "2.2.1"
severity = "moderate"
found = "2026-11-01"
resolved = "2026-11-02"
state = "fixed"
"#;

const ACCEPTED: &str = r#"
[[advisory]]
id = "GO-2025-4017"
package = "github.com/quic-go/quic-go"
ecosystem = "Go"
affected = "0.54.0"
severity = "unknown"
found = "2026-10-01"
state = "accepted"
reason = "QUIC is off in every generated config"
"#;

// ---- blocks ------------------------------------------------------------------------------------

#[test]
fn a_block_goes_after_the_first_title_and_a_second_apply_changes_nothing() {
    let readme = "# Title\n\nSome words.\n\n## Usage\n";
    let first = apply(readme, BLOCK, Place::AfterTitle, "README.md").unwrap();
    assert!(!first.had_block);
    assert_eq!(
        first.text,
        format!("# Title\n\n{BLOCK}\n\nSome words.\n\n## Usage\n")
    );
    let second = apply(&first.text, BLOCK, Place::AfterTitle, "README.md").unwrap();
    assert!(second.had_block);
    assert_eq!(second.text, first.text);
}

#[test]
fn a_changed_block_replaces_only_the_text_between_its_markers() {
    let before = format!("# T\n\n{BLOCK}\n\nkept\n");
    let newer = "<!-- ccid:security:begin (y) -->\nnew body\n<!-- ccid:security:end -->";
    let applied = apply(&before, newer, Place::AfterTitle, "README.md").unwrap();
    assert_eq!(applied.text, format!("# T\n\n{newer}\n\nkept\n"));
}

#[test]
fn markers_inside_a_fenced_example_are_not_markers() {
    let readme = format!("# T\n\n```md\n{BLOCK}\n```\n");
    let applied = apply(&readme, BLOCK, Place::AfterTitle, "README.md").unwrap();
    assert!(!applied.had_block);
    assert_eq!(applied.text.matches(END).count(), 2);
    assert!(applied
        .text
        .starts_with(&format!("# T\n\n{BLOCK}\n\n```md")));
}

#[test]
fn an_unbalanced_or_repeated_block_is_refused_not_guessed() {
    let missing_end = "# T\n\n<!-- ccid:security:begin (x) -->\ntext\n";
    assert!(apply(missing_end, BLOCK, Place::AfterTitle, "README.md").is_err());
    let twice = format!("# T\n\n{BLOCK}\n\n{BLOCK}\n");
    let error = apply(&twice, BLOCK, Place::AfterTitle, "README.md")
        .unwrap_err()
        .to_string();
    assert!(error.contains("README.md") && error.contains("unbalanced or repeated"));
}

#[test]
fn front_matter_comments_and_missing_titles_are_stepped_over() {
    let with_front = "---\ntitle: x\n---\n# T\n\ntext\n";
    let applied = apply(with_front, BLOCK, Place::AfterTitle, "README.md").unwrap();
    assert_eq!(
        applied.text,
        format!("---\ntitle: x\n---\n# T\n\n{BLOCK}\n\ntext\n")
    );
    // A `# ` line inside a comment or a fence is not the title; the real one follows.
    let hidden = "<!--\n# not a title\n-->\n# Real\n";
    let applied = apply(hidden, BLOCK, Place::AfterTitle, "README.md").unwrap();
    assert!(applied.text.contains(&format!("# Real\n\n{BLOCK}\n")));
    // No title at all: the top of the body, after a leading one-line comment.
    let none = "<!-- SPDX-License-Identifier: MIT -->\n<p align=\"center\">x</p>\n";
    let applied = apply(none, BLOCK, Place::AfterTitle, "README.md").unwrap();
    assert_eq!(
        applied.text,
        format!("<!-- SPDX-License-Identifier: MIT -->\n\n{BLOCK}\n\n<p align=\"center\">x</p>\n")
    );
    let again = apply(&applied.text, BLOCK, Place::AfterTitle, "README.md").unwrap();
    assert_eq!(again.text, applied.text);
}

#[test]
fn crlf_files_keep_their_line_endings_and_a_bom_stays_first() {
    let readme = "\u{feff}# T\r\n\r\ntext\r\n";
    let applied = apply(readme, BLOCK, Place::AfterTitle, "README.md").unwrap();
    assert!(applied
        .text
        .starts_with("\u{feff}# T\r\n\r\n<!-- ccid:security:begin"));
    assert!(!applied.text.replace("\r\n", "").contains('\n'));
    let again = apply(&applied.text, BLOCK, Place::AfterTitle, "README.md").unwrap();
    assert_eq!(again.text, applied.text);
}

#[test]
fn the_policy_block_goes_to_the_end_and_a_title_without_a_trailing_newline_survives() {
    let applied = apply("# Policy\n\nOurs.", BLOCK, Place::End, "SECURITY.md").unwrap();
    assert_eq!(applied.text, format!("# Policy\n\nOurs.\n\n{BLOCK}\n"));
    let empty = apply("", BLOCK, Place::End, "SECURITY.md").unwrap();
    assert_eq!(empty.text, format!("{BLOCK}\n"));
    let h1_at_eof = apply("# T", BLOCK, Place::AfterTitle, "README.md").unwrap();
    assert_eq!(h1_at_eof.text, format!("# T\n\n{BLOCK}\n"));
}

// ---- the ledger --------------------------------------------------------------------------------

#[test]
fn a_valid_ledger_parses_and_orders_newest_first() {
    let ledger = ledger(&format!("{OPEN}{FIXED}{ACCEPTED}")).unwrap();
    assert_eq!(ledger.in_state(State::Open).len(), 1);
    assert_eq!(ledger.in_state(State::Fixed)[0].id, "RUSTSEC-2024-0388");
    assert_eq!(
        ledger.in_state(State::Accepted)[0].severity,
        Severity::Unknown
    );
}

#[test]
fn the_ledger_refuses_what_could_carry_markup_or_contradict_itself() {
    let broken = |find: &str, replace: &str| {
        ledger(&format!("{OPEN}{FIXED}").replace(find, replace))
            .unwrap_err()
            .to_string()
    };
    // Markup in a value that is rendered: refused, not escaped.
    assert!(broken("\"hyper\"", "\"hy](http://x)[per\"").contains("package"));
    assert!(broken("GHSA-aaaa-bbbb-cccc", "GHSA-a](http://x)").contains("plain id"));
    assert!(broken("\"0.14.10\"", "\"<script>\"").contains("affected"));
    // Not a calendar day.
    assert!(broken("2026-11-10", "2026-02-30").contains("found"));
    assert!(broken("2026-11-10", "2026-13-01").contains("found"));
    // Fixed needs its fix and day; open must not have a resolved day; resolved cannot precede found.
    assert!(broken("fixed = \"2.2.1\"\n", "").contains("needs fixed and resolved"));
    assert!(broken(
        "state = \"open\"",
        "state = \"open\"\nresolved = \"2026-11-11\""
    )
    .contains("only for state fixed"));
    assert!(
        broken("resolved = \"2026-11-02\"", "resolved = \"2026-10-02\"")
            .contains("before it was found")
    );
    // Unknown keys are typos, not extensions.
    assert!(broken(
        "severity = \"high\"",
        "severity = \"high\"\nsummary = \"x\""
    )
    .contains("unknown field"));
    // Accepted needs a plain reason; others must not have one.
    let accepted = format!("schema = 1\n{ACCEPTED}");
    assert!(ledger(&accepted).is_ok());
    assert!(
        ledger(&accepted.replace("reason = \"QUIC is off in every generated config\"\n", ""))
            .unwrap_err()
            .to_string()
            .contains("reason")
    );
    assert!(ledger(&accepted.replace("QUIC is off", "QUIC <b>is off"))
        .unwrap_err()
        .to_string()
        .contains("reason"));
    assert!(
        broken("severity = \"high\"", "severity = \"high\"\nreason = \"x\"")
            .contains("only for state accepted")
    );
}

#[test]
fn a_duplicate_and_a_wrong_schema_are_refused() {
    let twice = format!("{OPEN}{}", OPEN.replace("schema = 1\n", ""));
    assert!(ledger(&twice)
        .unwrap_err()
        .to_string()
        .contains("listed twice"));
    let same_id_other_package = format!(
        "{OPEN}{}",
        OPEN.replace("schema = 1\n", "").replace("hyper", "h2")
    );
    assert!(ledger(&same_id_other_package).is_ok());
    assert!(ledger("schema = 2\n")
        .unwrap_err()
        .to_string()
        .contains("schema 2"));
}

#[test]
fn dates_are_real_calendar_days() {
    for good in ["2026-11-13", "2028-02-29", "2000-01-31"] {
        assert!(valid_date(good), "{good}");
    }
    for bad in [
        "2026-02-29",
        "2026-04-31",
        "2026-00-10",
        "2026-1-10",
        "26-11-10",
        "2026-11-1x",
        "1999-01-01",
        "",
    ] {
        assert!(!valid_date(bad), "{bad}");
    }
}

#[test]
fn files_are_read_from_the_repository_and_an_absent_one_is_an_empty_ledger() {
    let repo = tempfile::tempdir().unwrap();
    assert!(load_ledger(repo.path()).unwrap().advisories.is_empty());
    assert!(load_own(repo.path()).unwrap().advisories.is_empty());
    fs::create_dir_all(repo.path().join(".ci")).unwrap();
    fs::write(repo.path().join(LEDGER), format!("{OPEN}{FIXED}")).unwrap();
    assert_eq!(load_ledger(repo.path()).unwrap().advisories.len(), 2);
    fs::write(repo.path().join(LEDGER), "schema = 1\n[[advisory]\n").unwrap();
    let error = load_ledger(repo.path()).unwrap_err().to_string();
    assert!(error.contains(".ci/security.toml"), "{error}");
    #[cfg(unix)]
    {
        fs::remove_file(repo.path().join(LEDGER)).unwrap();
        fs::write(repo.path().join("elsewhere.toml"), "schema = 1\n").unwrap();
        std::os::unix::fs::symlink("../elsewhere.toml", repo.path().join(LEDGER)).unwrap();
        assert!(load_ledger(repo.path())
            .unwrap_err()
            .to_string()
            .contains("regular file"));
    }
}

// ---- the three renderings ----------------------------------------------------------------------

#[test]
fn an_empty_ledger_says_none_and_never_invents_a_scan_date() {
    let empty = Ledger::default();
    let none = OwnFile::default();
    let readme = readme_block(&empty, &none, "SECURITY.md");
    assert!(readme.contains("Advisories on record for dependencies: none."));
    assert!(readme.contains("(SECURITY.md)"));
    assert!(readme.starts_with("<!-- ccid:security:begin") && readme.ends_with(END));
    let policy = policy_block(&empty, &none);
    assert!(policy.contains("None on record."));
    assert!(policy.contains("**Security** tab") && policy.contains("**confidential** issue"));
    assert!(
        !policy.contains('@'),
        "no contact address in the policy block"
    );
    let notes = release_section_of(&empty, &none);
    assert!(notes.contains("Open advisories in dependencies: none"));
    // Nothing the renderers print depends on the day: no digits of a year.
    for text in [&readme, &policy, &notes] {
        assert!(!text.contains("202"), "{text}");
    }
}

#[test]
fn counts_tables_and_links_follow_the_ledger() {
    let ledger = ledger(&format!("{OPEN}{FIXED}{ACCEPTED}")).unwrap();
    let none = OwnFile::default();
    let readme = readme_block(&ledger, &none, ".github/SECURITY.md");
    assert!(readme.contains(
        "1 open ([GHSA-aaaa-bbbb-cccc](https://osv.dev/vulnerability/GHSA-aaaa-bbbb-cccc)), 1 accepted after review, 1 fixed."
    ));
    assert!(readme.contains("(.github/SECURITY.md)"));
    let policy = policy_block(&ledger, &none);
    assert!(policy.contains("| Advisory | Package | Affected | Fixed in |"));
    assert!(policy.contains("| [GHSA-aaaa-bbbb-cccc](https://osv.dev/vulnerability/GHSA-aaaa-bbbb-cccc) | `hyper` (crates.io) | `0.14.10` | - | high | open | 2026-11-10 | - |"));
    assert!(policy.contains("accepted (QUIC is off in every generated config)"));
    // Open before accepted before fixed.
    let open = policy.find("GHSA-aaaa").unwrap();
    let accepted = policy.find("GO-2025-4017").unwrap();
    let fixed = policy.find("RUSTSEC-2024-0388").unwrap();
    assert!(open < accepted && accepted < fixed);
    let notes = release_section_of(&ledger, &none);
    assert!(notes.contains("- Open advisories in dependencies: 1 ("));
    assert!(notes.contains("- Accepted after review: 1 ("));
    assert!(notes.contains("`derivative` 2.2.0 to 2.2.1, 2026-11-02"));
}

#[test]
fn long_ledgers_are_cut_and_say_where_the_rest_is() {
    let mut text = String::from("schema = 1\n");
    for n in 0..80 {
        text.push_str(&format!(
            "[[advisory]]\nid = \"GHSA-{n:04}\"\npackage = \"p{n}\"\necosystem = \"npm\"\naffected = \"1.0.0\"\nseverity = \"low\"\nfound = \"2026-11-01\"\nstate = \"open\"\n"
        ));
    }
    for n in 0..40 {
        text.push_str(&format!(
            "[[advisory]]\nid = \"GHSA-f{n:03}\"\npackage = \"q{n}\"\necosystem = \"npm\"\naffected = \"1.0.0\"\nfixed = \"1.0.1\"\nseverity = \"low\"\nfound = \"2026-11-01\"\nresolved = \"2026-11-02\"\nstate = \"fixed\"\n"
        ));
    }
    let ledger = ledger(&text).unwrap();
    let policy = policy_block(&ledger, &OwnFile::default());
    assert_eq!(policy.matches("| open |").count(), 50);
    assert_eq!(policy.matches("| fixed |").count(), 25);
    assert!(policy.contains("45 more are recorded in `.ci/security.toml`."));
    let readme = readme_block(&ledger, &OwnFile::default(), "SECURITY.md");
    assert!(readme.contains("80 open ("));
    assert!(readme.contains("and 75 more"));
}

const OWN_ENTRY: &str = r#"
schema = 1
[[advisory]]
key = "bad-parse-2026-11"
summary = "A crafted manifest could make the parser read past its input"
description = "Versions before 0.3.5 read past the end of a crafted input.\n\nUpgrade to 0.3.5."
severity = "moderate"
ecosystem = "rust"
package = "ccvl"
affected = ">= 0.2.0, < 0.3.5"
patched = "0.3.5"
found = "2026-11-03"
resolved = "2026-11-06"
cve = "CVE-2026-12345"
"#;

#[test]
fn own_advisories_are_listed_and_counted_and_the_ledger_never_becomes_one() {
    let own = own(OWN_ENTRY).unwrap();
    let ledger = ledger(&format!("{OPEN}{FIXED}")).unwrap();
    let policy = policy_block(&ledger, &own);
    assert!(policy.contains("### Advisories for this project"));
    assert!(policy.contains("| A crafted manifest could make the parser read past its input | `ccvl` (rust) | `>= 0.2.0, < 0.3.5` | `0.3.5` | moderate | 2026-11-06 | CVE-2026-12345 |"));
    let readme = readme_block(&ledger, &own, "SECURITY.md");
    assert!(readme.contains("Published advisories for this project: 1."));
}

#[test]
fn an_own_advisory_needs_a_real_severity_a_known_ecosystem_and_plain_text() {
    let broken = |find: &str, replace: &str| {
        own(&OWN_ENTRY.replace(find, replace))
            .unwrap_err()
            .to_string()
    };
    assert!(broken("moderate", "unknown").contains("severity"));
    assert!(broken("\"rust\"", "\"cargo\"").contains("ecosystem"));
    assert!(broken("A crafted", "A [crafted](http://x)").contains("summary"));
    assert!(broken("\"CVE-2026-12345\"", "\"CVE-26-1\"").contains("cve"));
    assert!(broken("bad-parse-2026-11", "Bad Key").contains("key"));
    assert!(broken(">= 0.2.0, < 0.3.5", "`x`").contains("affected"));
    assert!(broken("2026-11-06", "2026-11-02").contains("before it was found"));
}

// ---- the publish plan --------------------------------------------------------------------------

#[test]
fn the_plan_sends_nothing_and_names_what_each_forge_offers() {
    let repo = tempfile::tempdir().unwrap();
    let empty: serde_json::Value = serde_json::from_str(&plan_json(repo.path()).unwrap()).unwrap();
    assert_eq!(empty["sends"], false);
    assert_eq!(empty["forges"]["github"]["publish"], false);
    assert_eq!(empty["forges"]["gitlab"]["publish"], false);
    assert_eq!(empty["forges"]["bitbucket"]["publish"], false);
    assert_eq!(empty["forges"]["forgejo"]["publish"], false);

    fs::create_dir_all(repo.path().join(".ci")).unwrap();
    fs::write(repo.path().join(OWN), OWN_ENTRY).unwrap();
    // A dependency ledger next to it contributes nothing to the plan.
    fs::write(repo.path().join(LEDGER), format!("{OPEN}{FIXED}")).unwrap();
    let plan: serde_json::Value = serde_json::from_str(&plan_json(repo.path()).unwrap()).unwrap();
    assert_eq!(plan["advisories"], 1);
    assert_eq!(plan["forges"]["github"]["publish"], true);
    let steps = &plan["forges"]["github"]["entries"][0]["steps"];
    assert_eq!(steps[0]["method"], "GET");
    assert_eq!(steps[1]["method"], "POST");
    assert_eq!(
        steps[1]["path"],
        "/repos/{owner}/{repo}/security-advisories"
    );
    let body = &steps[1]["body"];
    assert_eq!(
        body["severity"], "medium",
        "GitHub says medium for moderate"
    );
    assert_eq!(body["cve_id"], "CVE-2026-12345");
    assert_eq!(body["vulnerabilities"][0]["package"]["ecosystem"], "rust");
    assert_eq!(
        body["vulnerabilities"][0]["vulnerable_version_range"],
        ">= 0.2.0, < 0.3.5"
    );
    assert_eq!(body["vulnerabilities"][0]["patched_versions"], "0.3.5");
    assert_eq!(steps[2]["method"], "PATCH");
    assert_eq!(steps[2]["body"]["state"], "published");
    assert!(
        !plan.to_string().contains("GHSA-aaaa"),
        "the ledger is never published"
    );
}

// ---- the files ---------------------------------------------------------------------------------

fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (relative, text) in files {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    root
}

#[test]
fn a_readme_gains_its_block_and_a_missing_policy_is_created() {
    let root = tree(&[("README.md", "# Tool\n\nwords\n")]);
    let plans = plan_files(root.path()).unwrap();
    assert_eq!(plans.len(), 2);
    let readme = &plans[0];
    assert_eq!(readme.path.to_str(), Some("README.md"));
    assert!(readme.introduced && readme.drifted());
    assert!(readme
        .expected
        .starts_with("# Tool\n\n<!-- ccid:security:begin"));
    assert!(readme.expected.ends_with("\nwords\n"));
    let policy = &plans[1];
    assert_eq!(policy.path.to_str(), Some("SECURITY.md"));
    assert!(policy.current.is_none() && policy.introduced);
    assert!(policy
        .expected
        .starts_with("# Security policy\n\n<!-- ccid:security:begin"));
    assert!(policy.expected.ends_with(&format!("{END}\n")));
}

#[test]
fn a_repository_without_a_readme_gets_no_block_and_the_policy_follows_github_precedence() {
    let root = tree(&[
        (".github/SECURITY.md", "# Ours\n\nHand written.\n"),
        ("SECURITY.md", "# Root\n"),
    ]);
    let plans = plan_files(root.path()).unwrap();
    assert_eq!(plans.len(), 1, "no README: no block and no new README");
    assert_eq!(plans[0].path.to_str(), Some(".github/SECURITY.md"));
    assert!(plans[0]
        .expected
        .starts_with("# Ours\n\nHand written.\n\n<!-- ccid:security:begin"));
    let docs = tree(&[("docs/SECURITY.md", "# Docs\n")]);
    assert_eq!(
        plan_files(docs.path()).unwrap()[0].path.to_str(),
        Some("docs/SECURITY.md")
    );
}

#[test]
fn the_readme_links_the_policy_file_it_really_has_and_a_rendered_tree_is_stable() {
    let root = tree(&[("README.md", "# T\n"), (".github/SECURITY.md", "# P\n")]);
    let first = plan_files(root.path()).unwrap();
    assert!(first[0].expected.contains("(.github/SECURITY.md)"));
    for plan in &first {
        fs::write(root.path().join(&plan.path), &plan.expected).unwrap();
    }
    let second = plan_files(root.path()).unwrap();
    assert!(second
        .iter()
        .all(|plan| !plan.drifted() && !plan.introduced));
}

#[test]
fn a_ledger_change_moves_the_block_and_a_hand_edit_of_the_block_is_drift() {
    let root = tree(&[("README.md", "# T\n")]);
    for plan in plan_files(root.path()).unwrap() {
        fs::write(root.path().join(&plan.path), &plan.expected).unwrap();
    }
    fs::create_dir_all(root.path().join(".ci")).unwrap();
    fs::write(root.path().join(LEDGER), OPEN).unwrap();
    let plans = plan_files(root.path()).unwrap();
    assert!(plans.iter().all(|plan| plan.drifted() && !plan.introduced));
    for plan in &plans {
        fs::write(root.path().join(&plan.path), &plan.expected).unwrap();
    }
    let edited = fs::read_to_string(root.path().join("README.md"))
        .unwrap()
        .replace("Advisories in dependencies", "Nothing to see");
    fs::write(root.path().join("README.md"), edited).unwrap();
    let plans = plan_files(root.path()).unwrap();
    assert!(plans[0].drifted() && !plans[0].introduced);
}

#[cfg(unix)]
#[test]
fn a_symlinked_readme_is_refused() {
    let root = tree(&[("real.md", "# T\n")]);
    std::os::unix::fs::symlink("real.md", root.path().join("README.md")).unwrap();
    assert!(plan_files(root.path())
        .unwrap_err()
        .to_string()
        .contains("not a regular file"));
}

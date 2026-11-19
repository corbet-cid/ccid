//! `ccid portable-paths`: tracked file names that work on every system.
//!
//! Reads `git ls-files` (and `lstat` for symlinks) and judges every repo-relative path:
//!
//! | rule | refusal |
//! |---|---|
//! | `chars` | a component holds a byte outside `[A-Za-z0-9._-]` (space, non-ASCII, `/ \ : * ? " < > |`, controls, invalid UTF-8) |
//! | `case` | two tracked paths, or two of their directories, are equal when case is ignored |
//! | `reserved` | a Windows device stem (CON PRN AUX NUL COM0-9 LPT0-9, with the superscript forms), with or without an extension |
//! | `trailing` | a component ends with a dot or a space |
//! | `leading-dash` | a component begins with `-` |
//! | `component-length` | a component is longer than 255 bytes |
//! | `path-length` | the repo-relative path is longer than 200 characters |
//! | `symlink` | a tracked symlink |
//!
//! Exceptions come only from the committed allowlist `.ci/portable-paths.toml`: `[[allow]]` entries with a `pattern`
//! (a repo-relative glob: `*` and `?` inside one segment, `**` as a whole segment), a `reason` and optionally the
//! `rules` it waives (default: every rule). Mode `report` prints the violations and passes; `enforce` fails on any
//! violation the allowlist does not cover. An unreadable repository or allowlist is an operational error (exit 2) in both.

use crate::render::surface::glob_match;
use crate::{failure, Result};
use clap::{Args, ValueEnum};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    process::Command,
};

/// The name of the standard check.
pub const CHECK: &str = "portable-paths";

/// `[portable_paths]` of `.ci/ccid.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `report` (the default: prints, never fails) or `enforce`.
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String {
    "report".into()
}

/// Built-in allowlist presets, switched on by `presets = [...]` in `.ci/portable-paths.toml`.
pub const PRESETS: [&str; 1] = ["sveltekit-routes"];

pub const RULES: [&str; 8] = [
    "chars",
    "case",
    "reserved",
    "trailing",
    "leading-dash",
    "component-length",
    "path-length",
    "symlink",
];

const MAX_COMPONENT_BYTES: usize = 255;
const MAX_PATH_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// Print the violations; always pass.
    Report,
    /// Fail on any violation the allowlist does not cover.
    Enforce,
}

#[derive(Args, Debug)]
pub struct Options {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// The committed allowlist, relative to the repository.
    #[arg(long, default_value = ".ci/portable-paths.toml")]
    pub allowlist: PathBuf,
    #[arg(long, value_enum, default_value = "report")]
    pub mode: Mode,
    /// Print only the per-rule counts, not every violation.
    #[arg(long)]
    pub summary: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Allowlist {
    #[serde(default)]
    pub allow: Vec<Allow>,
    /// Built-in presets: `sveltekit-routes` allows `+`, `[`, `]`, `(` and `)` (and nothing else) in names under a
    /// `routes` directory: the SvelteKit routing convention (`+page.svelte`, `[slug]`, `(group)`).
    #[serde(default)]
    pub presets: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allow {
    pub pattern: String,
    pub reason: String,
    #[serde(default)]
    pub rules: Vec<String>,
}

impl Allowlist {
    pub fn parse(text: &str) -> Result<Allowlist> {
        let list: Allowlist = toml::from_str(text)
            .map_err(|error| failure(format!("the portable-paths allowlist: {error}")))?;
        if let Some(preset) = list.presets.iter().find(|p| !PRESETS.contains(&p.as_str())) {
            return Err(failure(format!(
                "the portable-paths allowlist: {preset:?} is not a preset ({})",
                PRESETS.join(", ")
            )));
        }
        for entry in &list.allow {
            if entry.pattern.trim().is_empty() || entry.reason.trim().is_empty() {
                return Err(failure(
                    "the portable-paths allowlist: every entry needs a pattern and a reason",
                ));
            }
            if let Some(rule) = entry
                .rules
                .iter()
                .find(|rule| !RULES.contains(&rule.as_str()))
            {
                return Err(failure(format!(
                    "the portable-paths allowlist: {rule:?} is not a rule ({})",
                    RULES.join(", ")
                )));
            }
        }
        Ok(list)
    }

    /// Whether a preset allows this violation (a `chars` violation whose every offending character is a
    /// SvelteKit routing character, under a `routes` directory).
    fn preset_covers(&self, violation: &Violation) -> bool {
        self.presets.iter().any(|preset| match preset.as_str() {
            "sveltekit-routes" => {
                violation.rule == "chars"
                    && !violation.chars.is_empty()
                    && violation
                        .chars
                        .iter()
                        .all(|c| matches!(c, '+' | '[' | ']' | '(' | ')'))
                    && violation
                        .path
                        .rsplit_once('/')
                        .is_some_and(|(dirs, _)| dirs.split('/').any(|part| part == "routes"))
            }
            _ => false,
        })
    }

    fn covers(&self, violation: &Violation) -> Option<usize> {
        self.allow.iter().position(|entry| {
            glob_match(&entry.pattern, &violation.path)
                && (entry.rules.is_empty() || entry.rules.iter().any(|rule| rule == violation.rule))
        })
    }
}

/// One tracked path.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The path as git prints it (lossy when it is not UTF-8).
    pub path: String,
    pub utf8: bool,
    pub symlink: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub rule: &'static str,
    pub path: String,
    pub detail: String,
    /// `chars` only: the offending characters.
    pub chars: Vec<char>,
}

fn is_reserved_stem(component: &str) -> bool {
    // Windows reads the part before the first dot as the device name; trailing spaces and dots of it do not matter.
    let stem = component.split('.').next().unwrap_or("");
    let stem = stem.trim_end_matches([' ', '.']).to_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            let chars: Vec<char> = stem.chars().collect();
            chars.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(chars[3], '0'..='9' | '\u{00B9}' | '\u{00B2}' | '\u{00B3}')
        }
    }
}

fn good_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Judge every entry; the result is ordered by path, then rule.
pub fn judge(entries: &[Entry]) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut push = |rule: &'static str, path: &str, detail: String| {
        out.push(Violation {
            rule,
            path: path.to_owned(),
            detail,
            chars: Vec::new(),
        });
    };
    // case: every distinct prefix (directories and files), keyed by its lowercase spelling
    let mut spelled: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entry in entries {
        let mut prefix = String::new();
        for component in entry.path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            spelled
                .entry(prefix.to_lowercase())
                .or_default()
                .insert(prefix.clone());
        }
    }
    for entry in entries {
        let path = entry.path.as_str();
        let mut bad_chars: Vec<char> = Vec::new();
        for component in path.split('/') {
            for c in component.chars() {
                if !good_char(c) && !bad_chars.contains(&c) {
                    bad_chars.push(c);
                }
            }
            if component.len() > MAX_COMPONENT_BYTES {
                push(
                    "component-length",
                    path,
                    format!(
                        "a component is {} bytes (at most {MAX_COMPONENT_BYTES})",
                        component.len()
                    ),
                );
            }
            if component.ends_with('.') || component.ends_with(' ') {
                push(
                    "trailing",
                    path,
                    format!("{component:?} ends with a dot or a space"),
                );
            }
            if component.starts_with('-') {
                push(
                    "leading-dash",
                    path,
                    format!("{component:?} begins with '-'"),
                );
            }
            if is_reserved_stem(component) {
                push(
                    "reserved",
                    path,
                    format!("{component:?} is a Windows device name"),
                );
            }
        }
        if !entry.utf8 {
            push("chars", path, "the name is not valid UTF-8".into());
        } else if !bad_chars.is_empty() {
            let shown: String = bad_chars
                .iter()
                .map(|c| c.escape_debug().to_string())
                .collect::<Vec<_>>()
                .join(" ");
            push("chars", path, format!("outside [A-Za-z0-9._-]: {shown}"));
        }
        let length = path.chars().count();
        if length > MAX_PATH_CHARS {
            push(
                "path-length",
                path,
                format!("{length} characters (at most {MAX_PATH_CHARS})"),
            );
        }
        if entry.symlink {
            push("symlink", path, "a tracked symlink".into());
        }
        // case: report the path once per colliding prefix spelling that is not the first in byte order
        let mut prefix = String::new();
        for component in path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            let others = &spelled[&prefix.to_lowercase()];
            if others.len() > 1 {
                push(
                    "case",
                    path,
                    format!(
                        "{prefix:?} collides with {:?}",
                        others.iter().find(|o| **o != prefix).unwrap()
                    ),
                );
                break;
            }
        }
    }
    for violation in out.iter_mut().filter(|v| v.rule == "chars") {
        let mut seen: Vec<char> = Vec::new();
        for c in violation
            .path
            .chars()
            .filter(|c| *c != '/' && !good_char(*c))
        {
            if !seen.contains(&c) {
                seen.push(c);
            }
        }
        violation.chars = seen;
    }
    out.sort_by(|a, b| (&a.path, a.rule).cmp(&(&b.path, b.rule)));
    out.dedup();
    out
}

fn tracked(repo: &std::path::Path) -> Result<Vec<Entry>> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["ls-files", "-s", "-z"])
        .output()
        .map_err(|error| failure(format!("git ls-files: {error}")))?;
    if !output.status.success() {
        return Err(failure(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut entries = Vec::new();
    for record in output.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        // `<mode> <object> <stage>\t<path>`
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| failure("git ls-files: a record without a path"))?;
        let meta = String::from_utf8_lossy(&record[..tab]).into_owned();
        let name = &record[tab + 1..];
        let mode = meta.split_whitespace().next().unwrap_or("");
        if mode == "160000" {
            continue; // a submodule is a pointer, not a name of this repository's tree
        }
        let (path, utf8) = match std::str::from_utf8(name) {
            Ok(text) => (text.to_owned(), true),
            Err(_) => (String::from_utf8_lossy(name).into_owned(), false),
        };
        let on_disk = fs::symlink_metadata(repo.join(&path))
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        entries.push(Entry {
            symlink: mode == "120000" || on_disk,
            path,
            utf8,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    Ok(entries)
}

pub fn run(options: &Options) -> Result<u8> {
    let list = match fs::read_to_string(options.repo.join(&options.allowlist)) {
        Ok(text) => Allowlist::parse(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Allowlist::default(),
        Err(error) => return Err(failure(format!("the portable-paths allowlist: {error}"))),
    };
    let entries = tracked(&options.repo)?;
    let all = judge(&entries);
    let mut used = BTreeSet::new();
    let mut open = Vec::new();
    for violation in &all {
        match list.covers(violation) {
            Some(index) => {
                used.insert(index);
            }
            None if list.preset_covers(violation) => {}
            None => open.push(violation),
        }
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for violation in &open {
        *counts.entry(violation.rule).or_default() += 1;
        if !options.summary {
            println!(
                "portable-paths: {}: {}: {}",
                violation.rule, violation.path, violation.detail
            );
        }
    }
    for (index, entry) in list.allow.iter().enumerate() {
        if !used.contains(&index) {
            eprintln!(
                "portable-paths: note: the allowlist entry {:?} matches no violation",
                entry.pattern
            );
        }
    }
    let summary = counts
        .iter()
        .map(|(rule, n)| format!("{rule}={n}"))
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "portable-paths: {} path(s) checked, {} violation(s){}{}, {} allowed ({} mode)",
        entries.len(),
        open.len(),
        if summary.is_empty() { "" } else { ": " },
        summary,
        all.len() - open.len(),
        match options.mode {
            Mode::Report => "report",
            Mode::Enforce => "enforce",
        }
    );
    Ok(if options.mode == Mode::Enforce && !open.is_empty() {
        1
    } else {
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(paths: &[&str]) -> Vec<Entry> {
        paths
            .iter()
            .map(|path| Entry {
                path: (*path).to_owned(),
                utf8: true,
                symlink: false,
            })
            .collect()
    }

    fn rules(paths: &[&str]) -> Vec<(&'static str, String)> {
        judge(&entries(paths))
            .into_iter()
            .map(|v| (v.rule, v.path))
            .collect()
    }

    #[test]
    fn plain_names_pass() {
        assert!(rules(&["src/main.rs", "a-b_c/D.e.f", ".github/x.yml", "Cargo.lock"]).is_empty());
    }

    #[test]
    fn characters_outside_the_portable_set_are_refused() {
        for bad in [
            "a b",
            "caf\u{e9}",
            "a:b",
            "a*b",
            "a?b",
            "a\"b",
            "a<b",
            "a>b",
            "a|b",
            "a\\b",
            "a\u{1}b",
            "+page.svelte",
            "[slug]",
            "a\u{308}",
        ] {
            let found = rules(&[bad]);
            assert!(
                found.iter().any(|(rule, _)| *rule == "chars"),
                "{bad:?}: {found:?}"
            );
        }
        let mut bytes = entries(&["x"]);
        bytes[0].utf8 = false;
        assert!(judge(&bytes).iter().any(|v| v.rule == "chars"));
    }

    #[test]
    fn paths_that_differ_only_in_case_collide_also_through_directories() {
        let found = rules(&["a/File.txt", "a/file.txt"]);
        assert_eq!(found.iter().filter(|(rule, _)| *rule == "case").count(), 2);
        assert!(rules(&["Docs/a.md", "docs/b.md"])
            .iter()
            .all(|(rule, _)| *rule == "case"));
        assert!(rules(&["a/b", "a/c"]).is_empty());
    }

    #[test]
    fn windows_device_names_are_reserved_with_or_without_an_extension() {
        for bad in [
            "con",
            "CON",
            "con.txt",
            "nul.tar.gz",
            "Aux",
            "com1",
            "LPT9.log",
            "d/prn",
            "com\u{b9}",
            "lpt\u{b2}.x",
        ] {
            let found = rules(&[bad]);
            assert!(
                found.iter().any(|(rule, _)| *rule == "reserved"),
                "{bad:?}: {found:?}"
            );
        }
        for fine in [
            "console",
            "com10",
            "com",
            "conf",
            "nulls.txt",
            "lpt",
            "a.con",
        ] {
            assert!(
                !rules(&[fine]).iter().any(|(rule, _)| *rule == "reserved"),
                "{fine:?}"
            );
        }
    }

    #[test]
    fn trailing_dots_and_spaces_and_leading_dashes_are_refused() {
        assert!(rules(&["a."]).iter().any(|(r, _)| *r == "trailing"));
        assert!(rules(&["a /b"]).iter().any(|(r, _)| *r == "trailing"));
        assert!(rules(&["-rf"]).iter().any(|(r, _)| *r == "leading-dash"));
        assert!(rules(&["d/-x/y"]).iter().any(|(r, _)| *r == "leading-dash"));
        assert!(rules(&[".hidden", "a-b"]).is_empty());
    }

    #[test]
    fn lengths_are_bounded_per_component_and_per_path() {
        let long_component = "a".repeat(256);
        assert!(rules(&[&long_component])
            .iter()
            .any(|(r, _)| *r == "component-length"));
        assert!(!rules(&[&"a".repeat(255)])
            .iter()
            .any(|(r, _)| *r == "component-length"));
        let deep = vec!["abcdefghi"; 21].join("/"); // 21 * 9 + 20 = 209
        assert!(rules(&[&deep]).iter().any(|(r, _)| *r == "path-length"));
        let ok = vec!["abcdefghi"; 20].join("/"); // 200
        assert!(!rules(&[&ok]).iter().any(|(r, _)| *r == "path-length"));
    }

    #[test]
    fn a_tracked_symlink_is_refused() {
        let mut one = entries(&["link"]);
        one[0].symlink = true;
        assert!(judge(&one).iter().any(|v| v.rule == "symlink"));
    }

    #[test]
    fn the_allowlist_needs_a_reason_and_known_rules_and_covers_by_glob() {
        assert!(Allowlist::parse("[[allow]]\npattern = \"a\"\nreason = \"\"\n").is_err());
        assert!(Allowlist::parse(
            "[[allow]]\npattern = \"a\"\nreason = \"r\"\nrules = [\"nope\"]\n"
        )
        .is_err());
        assert!(
            Allowlist::parse("[[allow]]\npattern = \"a\"\nreason = \"r\"\nextra = 1\n").is_err()
        );
        let list = Allowlist::parse(
            "[[allow]]\npattern = \"src/routes/**/+page.svelte\"\nreason = \"SvelteKit\"\nrules = [\"chars\"]\n",
        )
        .unwrap();
        let hit = judge(&entries(&["src/routes/a/b/+page.svelte"]));
        assert!(hit.iter().all(|v| list.covers(v).is_some()));
        // the entry waives `chars` only: a trailing space is still a violation
        let miss = judge(&entries(&["src/routes/a /+page.svelte"]));
        assert!(miss.iter().any(|v| list.covers(v).is_none()));
    }

    #[test]
    fn the_sveltekit_routes_preset_allows_routing_characters_under_routes_only() {
        let list = Allowlist::parse("presets = [\"sveltekit-routes\"]\n").unwrap();
        let open = |paths: &[&str]| -> Vec<String> {
            judge(&entries(paths))
                .into_iter()
                .filter(|v| list.covers(v).is_none() && !list.preset_covers(v))
                .map(|v| v.path)
                .collect()
        };
        assert!(open(&[
            "src/routes/+page.svelte",
            "src/routes/[slug]/+page.ts",
            "a/routes/(app)/x.ts"
        ])
        .is_empty());
        // outside a routes directory, or with any other bad character, the violation stays
        assert_eq!(open(&["src/lib/+page.svelte"]).len(), 1);
        assert_eq!(open(&["src/routes/a b/+page.svelte"]).len(), 1);
        assert_eq!(open(&["src/routes/+page me.svelte"]).len(), 1);
        assert!(Allowlist::parse("presets = [\"nope\"]\n").is_err());
    }

    #[test]
    fn the_standard_check_joins_the_verify_job_in_the_mode_of_the_manifest() {
        let base = "schema = 1\nproject = \"demo\"\n\n[render]\nrender_schema = 3\n\n[checks.test]\nkind = \"commands\"\ncommands = [[\"true\"]]\n\n[jobs.verify]\nscheduler = \"forgejo\"\nchecks = [\"test\"]\nworkflow = \"ci\"\ncommand = [\"ccid:run-declared-checks\"]\n";
        let m = crate::parse_manifest(base.as_bytes()).unwrap();
        assert!(m.checks.contains_key(CHECK));
        assert!(m.jobs["verify"].names_check(CHECK));
        let enforce = format!("{base}\n[portable_paths]\nmode = \"enforce\"\n");
        let m = crate::parse_manifest(enforce.as_bytes()).unwrap();
        assert!(format!("{:?}", m.checks[CHECK]).contains("enforce"));
        let bad = format!("{base}\n[portable_paths]\nmode = \"x\"\n");
        assert!(crate::parse_manifest(bad.as_bytes()).is_err());
        // a repository's own check of that name wins; a job of another kind is left alone
        let own = format!(
            "{base}\n[checks.portable-paths]\nkind = \"commands\"\ncommands = [[\"true\"]]\n"
        );
        let m = crate::parse_manifest(own.as_bytes()).unwrap();
        assert!(!m.jobs["verify"].names_check(CHECK));
        // a verify job with its own command: the check becomes a job of its own
        let own_command = base.replace("[\"ccid:run-declared-checks\"]", "[\"bash\", \"run.sh\"]");
        let m = crate::parse_manifest(own_command.as_bytes()).unwrap();
        assert!(!m.jobs["verify"].names_check(CHECK));
        assert!(m.jobs[CHECK].names_check(CHECK));
        // no render_schema (the repository pins its own ccid): left alone
        let none = "schema = 1\nproject = \"demo\"\n\n[checks.test]\nkind = \"commands\"\ncommands = [[\"true\"]]\n";
        assert!(!crate::parse_manifest(none.as_bytes())
            .unwrap()
            .checks
            .contains_key(CHECK));
    }

    fn git_repo(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        for file in files {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "x").unwrap();
        }
        git(&["add", "-A"]);
        dir
    }

    #[test]
    fn report_mode_passes_and_enforce_mode_fails_on_a_real_repository() {
        let repo = git_repo(&["ok.txt", "bad name.txt", "src/+page.svelte"]);
        let options = |mode| Options {
            repo: repo.path().to_path_buf(),
            allowlist: ".ci/portable-paths.toml".into(),
            mode,
            summary: true,
        };
        assert_eq!(run(&options(Mode::Report)).unwrap(), 0);
        assert_eq!(run(&options(Mode::Enforce)).unwrap(), 1);
        fs::create_dir_all(repo.path().join(".ci")).unwrap();
        fs::write(
            repo.path().join(".ci/portable-paths.toml"),
            "[[allow]]\npattern = \"bad name.txt\"\nreason = \"fixture\"\n[[allow]]\npattern = \"src/+page.svelte\"\nreason = \"SvelteKit\"\n",
        )
        .unwrap();
        assert_eq!(run(&options(Mode::Enforce)).unwrap(), 0);
    }

    #[test]
    fn an_unreadable_allowlist_or_a_non_repository_is_an_operational_error() {
        let plain = tempfile::tempdir().unwrap();
        let options = Options {
            repo: plain.path().to_path_buf(),
            allowlist: ".ci/portable-paths.toml".into(),
            mode: Mode::Report,
            summary: true,
        };
        assert!(run(&options).is_err());
    }
}

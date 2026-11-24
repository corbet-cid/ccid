//! `kind = "declarative"`: the repository holds declarative configuration only, so code turns the check red.
//!
//! ```toml
//! [checks.no-code]
//! kind = "declarative"
//! allow = ["scripts/", "*.py"]   # optional: tracked paths that may hold code
//! ```
//!
//! Code is a tracked file with a source-code extension (shell, Python, Rust, Go, JavaScript/TypeScript, Ruby, Perl,
//! Lua, C-family, JVM, PHP, PowerShell, batch) or a `#!` first line. Nix, TOML, YAML, JSON, Markdown and lock files
//! are configuration. `allow` entries are an exact path, a `dir/` prefix or a `*.ext` suffix. Code embedded in
//! `.nix` strings is outside this check. The result is never cached.
use crate::{failure, Check, Result};
use std::{fs, io::Read, path::Path, process::Command};

const CODE_EXTENSIONS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "ksh", "py", "pyw", "rs", "go", "js", "mjs", "cjs", "jsx", "ts",
    "tsx", "rb", "pl", "pm", "lua", "c", "h", "cc", "cpp", "cxx", "hpp", "java", "kt", "kts",
    "scala", "swift", "php", "ps1", "psm1", "bat", "cmd", "cs", "zig", "dart", "ex", "exs",
];

pub(crate) fn validate(check: &Check) -> Result<()> {
    let bare = Check {
        kind: "declarative".into(),
        allow: check.allow.clone(),
        ..Check::default()
    };
    if serde_json::to_value(check)? != serde_json::to_value(&bare)? {
        return Err(failure("kind = \"declarative\" takes allow only"));
    }
    if check.allow.iter().any(|entry| entry.is_empty()) {
        return Err(failure("allow entries must not be empty"));
    }
    Ok(())
}

fn allowed(path: &str, allow: &[String]) -> bool {
    allow.iter().any(|entry| {
        if let Some(suffix) = entry.strip_prefix('*') {
            path.ends_with(suffix)
        } else if entry.ends_with('/') {
            path.starts_with(entry.as_str())
        } else {
            path == entry
        }
    })
}

fn has_shebang(file: &Path) -> bool {
    let mut head = [0u8; 2];
    fs::File::open(file)
        .and_then(|mut f| f.read_exact(&mut head))
        .map(|_| &head == b"#!")
        .unwrap_or(false)
}

/// Tracked paths of `files` (relative to `root`) that are code and not allowed.
pub(crate) fn offenders(root: &Path, files: &[String], allow: &[String]) -> Vec<String> {
    files
        .iter()
        .filter(|path| !allowed(path, allow))
        .filter(|path| {
            let by_extension = Path::new(path.as_str())
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| CODE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
            by_extension || has_shebang(&root.join(path))
        })
        .cloned()
        .collect()
}

pub(crate) fn run(check: &Check, root: &Path) -> Result<()> {
    let listing = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()?;
    if !listing.status.success() {
        return Err(failure(
            "git ls-files failed: the declarative check needs a Git checkout",
        ));
    }
    let files: Vec<String> = String::from_utf8_lossy(&listing.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    let found = offenders(root, &files, &check.allow);
    if found.is_empty() {
        return Ok(());
    }
    Err(failure(format!(
        "declarative repository holds code ({}): {}",
        found.len(),
        found.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn config_files_are_not_code() {
        let files = names(&[
            "flake.nix",
            "flake.lock",
            "README.md",
            "a/b.toml",
            "x.yaml",
            "y.json",
        ]);
        assert!(offenders(Path::new("/nonexistent"), &files, &[]).is_empty());
    }

    #[test]
    fn code_extensions_are_offenders_and_allow_exempts() {
        let files = names(&["a.sh", "d/b.PY", "c.rs", "scripts/x.go", "flake.nix"]);
        let root = Path::new("/nonexistent");
        assert_eq!(
            offenders(root, &files, &[]),
            names(&["a.sh", "d/b.PY", "c.rs", "scripts/x.go"])
        );
        let allow = names(&["scripts/", "*.rs", "a.sh"]);
        assert_eq!(offenders(root, &files, &allow), names(&["d/b.PY"]));
    }

    #[test]
    fn shebang_without_extension_is_code() {
        let dir = std::env::temp_dir().join(format!("ccid-decl-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tool"), "#!/bin/sh\necho\n").unwrap();
        fs::write(dir.join("notes"), "plain\n").unwrap();
        let found = offenders(&dir, &names(&["tool", "notes"]), &[]);
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, names(&["tool"]));
    }

    #[test]
    fn validation_rejects_foreign_keys() {
        let ok: Check = toml::from_str("kind = 'declarative'\nallow = ['x/']").unwrap();
        assert!(validate(&ok).is_ok());
        let bad: Check = toml::from_str("kind = 'declarative'\ncache = true").unwrap();
        assert!(validate(&bad).is_err());
        let misplaced: Check = toml::from_str("kind = 'commands'\nallow = ['x']").unwrap();
        assert!(crate::checks::validate_check(&misplaced).is_err());
    }
}

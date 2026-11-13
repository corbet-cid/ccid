//! The version of a release: read from the repository's own version file, plain semver, and what the
//! number decides (the platform set).
//!
//! A release has exactly one rule and no knobs:
//!
//! | version | class | platforms |
//! |---|---|---|
//! | `X.Y.Z`, `Z > 0` | patch | `linux-x86_64` alone |
//! | `X.Y.0`, `Y > 0` | minor | every platform the manifest declares |
//! | `X.0.0` | major | every platform the manifest declares |
//!
//! A pre-release or build suffix, a leading zero, `0.0.0` and anything that is not three numbers are
//! refused: they name no release.

use crate::{failure, Result};
use serde::Serialize;
use std::{fmt, path::Path};

/// What the version number says about the release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Major,
    Minor,
    Patch,
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Class::Major => "major",
            Class::Minor => "minor",
            Class::Patch => "patch",
        })
    }
}

/// A plain `MAJOR.MINOR.PATCH` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Version {
    /// Parse a version; anything but three plain numbers (and not `0.0.0`) is refused.
    pub fn parse(text: &str) -> Result<Version> {
        let parts: Vec<&str> = text.split('.').collect();
        let number = |part: &str| -> Option<u64> {
            let plain = !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'));
            if plain {
                part.parse().ok()
            } else {
                None
            }
        };
        let numbers: Option<Vec<u64>> = parts.iter().map(|part| number(part)).collect();
        match numbers.as_deref() {
            Some(&[major, minor, patch]) if (major, minor, patch) != (0, 0, 0) => Ok(Version {
                major,
                minor,
                patch,
            }),
            _ => Err(failure(format!(
                "the version {text:?} names no release: a release is MAJOR.MINOR.PATCH (plain numbers, no suffix, not 0.0.0)"
            ))),
        }
    }

    pub fn class(&self) -> Class {
        if self.patch > 0 {
            Class::Patch
        } else if self.minor > 0 {
            Class::Minor
        } else {
            Class::Major
        }
    }

    /// A release whose version ends in `.0` holds every declared platform.
    pub fn full_set(&self) -> bool {
        self.patch == 0
    }
}

/// A plain path inside the repository: relative, no `..`, no backslash.
pub fn check_relative(file: &str) -> Result<()> {
    let plain = !file.is_empty()
        && !file.contains(['\\', '\0'])
        && Path::new(file).is_relative()
        && Path::new(file)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    if plain {
        Ok(())
    } else {
        Err(failure(format!(
            "{file:?} must be a path inside the repository"
        )))
    }
}

/// The version files a release can read, by file name.
pub const FILES: &str = "Cargo.toml, package.json or pyproject.toml";

/// Whether `file` is a version file this release knows how to read: a plain relative path whose last
/// component is one of [`FILES`].
pub fn check_file(file: &str) -> Result<()> {
    check_relative(file)?;
    match Path::new(file).file_name().and_then(|name| name.to_str()) {
        Some("Cargo.toml" | "package.json" | "pyproject.toml") => Ok(()),
        _ => Err(failure(format!(
            "[release] version {file:?} is not a version file this release can read ({FILES})"
        ))),
    }
}

/// Read the version of `file` (relative to `repo`).
pub fn read(repo: &Path, file: &str) -> Result<Version> {
    check_file(file)?;
    let path = repo.join(file);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| failure(format!("cannot read the version file {file}: {error}")))?;
    let found = match path.file_name().and_then(|name| name.to_str()) {
        Some("Cargo.toml") => cargo_version(&text),
        Some("package.json") => serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|error| failure(format!("{file} is not JSON: {error}")))?
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        _ => toml::from_str::<toml::Value>(&text)
            .map_err(|error| failure(format!("{file} is not TOML: {error}")))?
            .get("project")
            .and_then(|project| project.get("version"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
    };
    let found = found.ok_or_else(|| {
        failure(format!(
            "{file} holds no literal version (a release reads [package] or [workspace.package] version of Cargo.toml, the version of package.json, [project] version of pyproject.toml)"
        ))
    })?;
    Version::parse(&found)
}

/// `[package] version`, else `[workspace.package] version` (also when the package inherits it).
fn cargo_version(text: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(text).ok()?;
    let literal = |value: Option<&toml::Value>| value.and_then(|v| v.as_str()).map(str::to_owned);
    literal(
        value
            .get("package")
            .and_then(|package| package.get("version")),
    )
    .or_else(|| {
        literal(
            value
                .get("workspace")
                .and_then(|workspace| workspace.get("package"))
                .and_then(|package| package.get("version")),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_versions_parse_and_classify() {
        assert_eq!(Version::parse("0.1.0").unwrap().class(), Class::Minor);
        assert_eq!(Version::parse("0.1.1").unwrap().class(), Class::Patch);
        assert_eq!(Version::parse("1.0.0").unwrap().class(), Class::Major);
        assert_eq!(Version::parse("2.4.0").unwrap().class(), Class::Minor);
        assert_eq!(Version::parse("0.0.1").unwrap().class(), Class::Patch);
        assert_eq!(Version::parse("10.20.30").unwrap().to_string(), "10.20.30");
    }

    #[test]
    fn a_major_and_a_minor_hold_the_full_set_a_patch_does_not() {
        assert!(Version::parse("1.0.0").unwrap().full_set());
        assert!(Version::parse("1.2.0").unwrap().full_set());
        assert!(!Version::parse("1.2.3").unwrap().full_set());
    }

    #[test]
    fn what_names_no_release_is_refused() {
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "0.0.0",
            "1.2.3-rc.1",
            "1.2.3+build",
            "01.2.3",
            "1.02.3",
            "1.2.x",
            "v1.2.3",
            " 1.2.3",
            "1.2.3 ",
            "-1.2.3",
            "1..3",
        ] {
            let error = Version::parse(bad).unwrap_err().to_string();
            assert!(error.contains("names no release"), "{bad:?}: {error}");
        }
    }

    #[test]
    fn version_files_are_plain_relative_paths_of_a_known_kind() {
        for good in [
            "Cargo.toml",
            "crates/x/Cargo.toml",
            "package.json",
            "pyproject.toml",
        ] {
            assert!(check_file(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "/Cargo.toml",
            "../Cargo.toml",
            "a/../Cargo.toml",
            "VERSION",
            "Cargo.lock",
            "a\\Cargo.toml",
        ] {
            assert!(check_file(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn each_version_file_kind_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| std::fs::write(dir.path().join(name), body).unwrap();
        write("Cargo.toml", "[package]\nname='a'\nversion='1.2.3'\n");
        assert_eq!(read(dir.path(), "Cargo.toml").unwrap().to_string(), "1.2.3");
        write(
            "Cargo.toml",
            "[workspace.package]\nversion='0.4.0'\n[package]\nname='a'\nversion.workspace=true\n",
        );
        assert_eq!(read(dir.path(), "Cargo.toml").unwrap().to_string(), "0.4.0");
        write("package.json", "{\"name\":\"a\",\"version\":\"3.0.0\"}");
        assert_eq!(
            read(dir.path(), "package.json").unwrap().class(),
            Class::Major
        );
        write("pyproject.toml", "[project]\nname='a'\nversion='0.2.1'\n");
        assert_eq!(
            read(dir.path(), "pyproject.toml").unwrap().class(),
            Class::Patch
        );
        write(
            "pyproject.toml",
            "[project]\nname='a'\ndynamic=['version']\n",
        );
        assert!(read(dir.path(), "pyproject.toml").is_err());
        write("Cargo.toml", "[package]\nname='a'\nversion='1.0.0-rc.1'\n");
        assert!(read(dir.path(), "Cargo.toml").is_err());
    }
}

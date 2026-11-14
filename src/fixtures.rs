//! Declared native fixtures (`docs/fixtures.md`).
//!
//! A repository declares a fixture in `.ci/ccid.toml`: a `setup` command that produces
//! native inputs (an authenticated proof directory, a prebuilt worker binary, a started
//! service) and the environment variable names that tell a check where they are. A check
//! lists the fixtures it needs; ccid runs `setup` once before the check's commands, adds the
//! variables to the commands' environment, and runs the optional `teardown` afterwards, also
//! when the check failed. The fixture therefore lives exactly as long as the check, which for
//! `ccid coverage run` is the one coverage run.
//!
//! `setup` writes `NAME=VALUE` lines to the file `CCID_FIXTURE_ENV` names; it may keep its own
//! files in the private directory `CCID_FIXTURE_DIR`, which ccid creates before `setup` and
//! removes after `teardown`. Exactly the declared names must be written, each once, with a
//! non-empty value, so a fixture cannot widen the environment of the check behind its
//! declaration. The manifest bytes already key a check's result, so changing a fixture's
//! definition re-keys every check that uses it.

use crate::{failure, runner::Runner, Environment, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) const DIR_ENV: &str = "CCID_FIXTURE_DIR";
pub(crate) const FILE_ENV: &str = "CCID_FIXTURE_ENV";

/// One `[fixtures.<name>]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fixture {
    /// Produces the fixture and writes its `NAME=VALUE` lines.
    pub setup: Vec<String>,
    /// Optional; runs after the check with the fixture's variables.
    #[serde(default)]
    pub teardown: Option<Vec<String>>,
    /// The variable names `setup` must export, nonempty.
    pub env: Vec<String>,
}

/// Names a fixture may never set: they steer the toolchain, the loader or ccid itself.
fn reserved(name: &str) -> bool {
    matches!(
        name,
        "PATH"
            | "HOME"
            | "USER"
            | "SHELL"
            | "PWD"
            | "TMPDIR"
            | "TEMP"
            | "TMP"
            | "RUSTC"
            | "CARGO"
            | "RUSTUP_TOOLCHAIN"
            | "RUSTFLAGS"
            | "LD_LIBRARY_PATH"
            | "LD_PRELOAD"
            | "NIX_LDFLAGS"
            | "NIX_CFLAGS_COMPILE"
    ) || name.starts_with("CCID_")
        || name.starts_with("CI_")
        || name.starts_with("CARGO_")
        || name.starts_with("LD_")
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Manifest-load validation: shape of each fixture, and every reference of a check.
pub(crate) fn validate(
    fixtures: &BTreeMap<String, Fixture>,
    checks: &BTreeMap<String, crate::Check>,
) -> Result<()> {
    for (name, fixture) in fixtures {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(failure(format!("Invalid fixture name {name:?}")));
        }
        crate::validate_command(&fixture.setup)
            .map_err(|error| failure(format!("Fixture {name} setup: {error}")))?;
        if let Some(teardown) = &fixture.teardown {
            crate::validate_command(teardown)
                .map_err(|error| failure(format!("Fixture {name} teardown: {error}")))?;
        }
        if fixture.env.is_empty() {
            return Err(failure(format!("Fixture {name} declares no env names")));
        }
        for (index, variable) in fixture.env.iter().enumerate() {
            if !valid_name(variable) || reserved(variable) {
                return Err(failure(format!(
                    "Fixture {name} cannot export {variable:?}: use an uppercase name outside PATH, LD_*, CARGO_*, CI_* and CCID_*"
                )));
            }
            if fixture.env[..index].contains(variable) {
                return Err(failure(format!("Fixture {name} lists {variable} twice")));
            }
        }
    }
    let mut owner: BTreeMap<&str, &str> = BTreeMap::new();
    for (check_name, check) in checks {
        for (index, used) in check.fixtures.iter().enumerate() {
            let Some(fixture) = fixtures.get(used) else {
                return Err(failure(format!(
                    "Check {check_name} uses undeclared fixture {used}"
                )));
            };
            if check.fixtures[..index].contains(used) {
                return Err(failure(format!(
                    "Check {check_name} lists fixture {used} twice"
                )));
            }
            // Two fixtures of one check may not export the same name.
            for variable in &fixture.env {
                if let Some(other) = owner.insert(variable, used) {
                    if other != used.as_str() && check.fixtures.iter().any(|f| f == other) {
                        return Err(failure(format!(
                            "Check {check_name}: fixtures {other} and {used} both export {variable}"
                        )));
                    }
                }
            }
        }
        owner.clear();
    }
    Ok(())
}

/// A started fixture: its exported variables and the private directory.
pub(crate) struct Started {
    name: String,
    dir: tempfile::TempDir,
    exported: Vec<(String, String)>,
}

/// The fixtures of one check, started in declaration order and stopped in reverse.
pub(crate) struct Active {
    started: Vec<(Started, Option<Vec<String>>)>,
}

/// Parse the `NAME=VALUE` lines `setup` wrote: exactly the declared names, once, nonempty.
pub(crate) fn parse(name: &str, declared: &[String], text: &str) -> Result<Vec<(String, String)>> {
    let mut found: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            return Err(failure(format!(
                "Fixture {name} wrote a line that is not NAME=VALUE"
            )));
        };
        if !declared.iter().any(|d| d == key) {
            return Err(failure(format!("Fixture {name} exported undeclared {key}")));
        }
        if val.is_empty() || val.contains('\0') {
            return Err(failure(format!("Fixture {name} exported an empty {key}")));
        }
        if found.iter().any(|(k, _)| k == key) {
            return Err(failure(format!("Fixture {name} exported {key} twice")));
        }
        found.push((key.to_owned(), val.to_owned()));
    }
    for d in declared {
        if !found.iter().any(|(k, _)| k == d) {
            return Err(failure(format!("Fixture {name} did not export {d}")));
        }
    }
    Ok(found)
}

fn with_fixture_environment(runner: &Runner, started: &Started) -> Runner {
    let mut copy = runner.clone_for_fixture();
    for (key, val) in &started.exported {
        crate::set(&mut copy.environment, key, val.as_str());
    }
    crate::set(
        &mut copy.environment,
        DIR_ENV,
        started.dir.path().as_os_str(),
    );
    copy
}

/// Start the named fixtures with `runner`'s environment. On a failed `setup` the fixtures
/// already started are stopped before the error returns.
pub(crate) fn start(
    runner: &Runner,
    fixtures: &BTreeMap<String, Fixture>,
    used: &[String],
    scratch_root: Option<&Path>,
) -> Result<Active> {
    let mut active = Active {
        started: Vec::new(),
    };
    for name in used {
        let fixture = &fixtures[name];
        let dir = match scratch_root {
            Some(root) => tempfile::Builder::new()
                .prefix("ccid-fixture-")
                .tempdir_in(root),
            None => tempfile::Builder::new().prefix("ccid-fixture-").tempdir(),
        }
        .map_err(|error| failure(format!("Fixture {name}: no private directory: {error}")))?;
        let file = dir.path().join("env");
        let mut setup = runner.clone_for_fixture();
        crate::set(&mut setup.environment, DIR_ENV, dir.path().as_os_str());
        crate::set(&mut setup.environment, FILE_ENV, file.as_os_str());
        let outcome = setup.run(&fixture.setup, false).and_then(|_| {
            let text = std::fs::read_to_string(&file)
                .map_err(|error| failure(format!("Fixture {name} wrote no {FILE_ENV}: {error}")))?;
            parse(name, &fixture.env, &text)
        });
        match outcome {
            Ok(exported) => active.started.push((
                Started {
                    name: name.clone(),
                    dir,
                    exported,
                },
                fixture.teardown.clone(),
            )),
            Err(error) => {
                // Stop what is running; the setup failure is the one to report.
                let _ = active.stop(runner);
                return Err(error);
            }
        }
    }
    Ok(active)
}

impl Active {
    /// The variables to add to the check's environment.
    pub(crate) fn apply(&self, environment: &mut Environment) {
        for (started, _) in &self.started {
            for (key, val) in &started.exported {
                crate::set(environment, key, val.as_str());
            }
        }
    }

    /// The names `apply` set, to remove again.
    pub(crate) fn names(&self) -> Vec<String> {
        self.started
            .iter()
            .flat_map(|(started, _)| started.exported.iter().map(|(key, _)| key.clone()))
            .collect()
    }

    /// Run every teardown in reverse order and remove the private directories. Every
    /// teardown runs even when an earlier one fails; the first failure is returned.
    pub(crate) fn stop(&mut self, runner: &Runner) -> Result<()> {
        let mut first = None;
        while let Some((started, teardown)) = self.started.pop() {
            if let Some(command) = teardown {
                let copy = with_fixture_environment(runner, &started);
                if let Err(error) = copy.run(&command, false) {
                    first.get_or_insert_with(|| {
                        failure(format!("Fixture {} teardown failed: {error}", started.name))
                    });
                }
            }
        }
        first.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests;

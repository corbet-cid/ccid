//! `kind = "pack"`: the registry packages of a library, made once (operating model R8, R23, R24, U6, A8).
//!
//! ```toml
//! [checks.pack]
//! kind = "pack"
//! crates = "."                        # the directory of the published crate's Cargo.toml
//! npm = "js/@corbet-labs/cgreet"      # the directory of its package.json
//! pypi = "py"                         # the directory of its pyproject.toml
//!
//! [release]
//! kind = "library"
//! version = "Cargo.toml"
//! registries = ["crates", "npm", "pypi"]
//! build = ["pack"]
//! ```
//!
//! A pack is the first build check of a release run, so a library that cannot pack fails before anything is public
//! (R23). It is never a check of a job (a commit's verify does not pack: A8), and it is never cached: the release
//! run makes its files from the commit. It leaves exactly these files in `$CCID_RELEASE_DIR`, which become the assets
//! of the release, the files of every copy and the files the registries receive:
//!
//! | key | file |
//! |---|---|
//! | `crates` | `<name>-<version>.crate` (`cargo package --locked`, from the clean checkout) |
//! | `npm` | `<name>-<version>.tgz`, a scoped `@scope/name` as `scope-name-<version>.tgz` (`npm pack`, so `prepack` builds) |
//! | `pypi` | `<norm>-<version>.tar.gz` and `<norm>-<version>-py3-none-any.whl` (`uv build`) |
//!
//! The package versions must equal the release version. The Python build backend is pinned by hashes in
//! `<pypi>/build-constraints.txt`, a lock file refreshed with the other locks. `SOURCE_DATE_EPOCH` is the committer
//! time of the commit, so a repeated pack of the same commit is byte-identical. A pack runs with the release
//! build's environment (no credentials); the only network use is the fetch of what a lock names.
use crate::{failure, release, release::version, set, value, Check, Result, Runner};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// The `kind` of a pack check.
pub(crate) const KIND: &str = "pack";
/// The hash-locked build requirements of a Python package, beside its `pyproject.toml`.
pub(crate) const BUILD_CONSTRAINTS: &str = "build-constraints.txt";

/// The directory keys of a pack check, in packing order.
fn keys(check: &Check) -> [(&'static str, Option<&str>); 3] {
    [
        ("crates", check.crates.as_deref()),
        ("npm", check.npm.as_deref()),
        ("pypi", check.pypi.as_deref()),
    ]
}

/// The registries a pack check packs for.
pub(crate) fn registries(check: &Check) -> Vec<release::Registry> {
    keys(check)
        .into_iter()
        .filter(|(_, dir)| dir.is_some())
        .map(|(key, _)| match key {
            "crates" => release::Registry::Crates,
            "npm" => release::Registry::Npm,
            _ => release::Registry::Pypi,
        })
        .collect()
}

/// A directory of the repository: `.` or a plain relative path.
fn check_directory(key: &str, dir: &str) -> Result<()> {
    if dir == "." {
        return Ok(());
    }
    version::check_relative(dir).map_err(|_| {
        failure(format!(
            "pack {key} = {dir:?} must be a directory inside the repository (no .., no absolute path)"
        ))
    })
}

/// The static rules of a pack check (refused when the manifest is read).
pub(crate) fn validate(check: &Check) -> Result<()> {
    if keys(check).iter().all(|(_, dir)| dir.is_none()) {
        return Err(failure(
            "kind = \"pack\" names the package directories it packs: at least one of crates, npm and pypi",
        ));
    }
    for (key, dir) in keys(check) {
        if let Some(dir) = dir {
            check_directory(key, dir)?;
        }
    }
    // The outputs are not tree files and the result is never reused: no other key means anything here.
    let bare = Check {
        kind: KIND.into(),
        crates: check.crates.clone(),
        npm: check.npm.clone(),
        pypi: check.pypi.clone(),
        ..Check::default()
    };
    if serde_json::to_value(check)? != serde_json::to_value(&bare)? {
        return Err(failure(
            "kind = \"pack\" takes crates, npm and pypi only: no commands, cache_* or other keys (a pack is never cached)",
        ));
    }
    Ok(())
}

/// One package a pack makes, with the facts its files are named by.
#[derive(Debug, PartialEq, Eq)]
struct Package {
    registry: &'static str,
    /// The directory, relative to the repository.
    dir: String,
    name: String,
    /// Where the version was read, for messages.
    file: String,
    version: String,
}

fn join(dir: &str, file: &str) -> String {
    if dir == "." {
        file.to_owned()
    } else {
        format!("{dir}/{file}")
    }
}

fn read_text(root: &Path, file: &str) -> Result<String> {
    fs::read_to_string(root.join(file))
        .map_err(|error| failure(format!("pack cannot read {file}: {error}")))
}

fn string_at<'a>(value: &'a toml::Value, path: &[&str]) -> Option<&'a str> {
    let mut at = value;
    for key in path {
        at = at.get(key)?;
    }
    at.as_str()
}

/// Name and version of each package from its own manifest file. A version that is not a literal string
/// (`version.workspace = true`, `dynamic = ["version"]`) is refused: the release version must be readable.
fn packages(root: &Path, check: &Check) -> Result<Vec<Package>> {
    let mut found = Vec::new();
    for (key, dir) in keys(check) {
        let Some(dir) = dir else { continue };
        let (registry, file) = match key {
            "crates" => ("crates", join(dir, "Cargo.toml")),
            "npm" => ("npm", join(dir, "package.json")),
            _ => ("pypi", join(dir, "pyproject.toml")),
        };
        let text = read_text(root, &file)?;
        let (name, found_version) = match registry {
            "npm" => {
                let json: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|error| failure(format!("{file} is not JSON: {error}")))?;
                let field = |name: &str| json.get(name).and_then(|v| v.as_str()).map(str::to_owned);
                (field("name"), field("version"))
            }
            _ => {
                let table: toml::Value = toml::from_str(&text)
                    .map_err(|error| failure(format!("{file} is not TOML: {error}")))?;
                let section = if registry == "crates" {
                    "package"
                } else {
                    "project"
                };
                if registry == "pypi"
                    && table
                        .get("project")
                        .and_then(|project| project.get("dynamic"))
                        .and_then(|dynamic| dynamic.as_array())
                        .is_some_and(|names| names.iter().any(|n| n.as_str() == Some("version")))
                {
                    return Err(failure(format!(
                        "{file} declares dynamic = [\"version\"]: a release reads a literal [project] version"
                    )));
                }
                (
                    string_at(&table, &[section, "name"]).map(str::to_owned),
                    string_at(&table, &[section, "version"]).map(str::to_owned),
                )
            }
        };
        let (Some(name), Some(found_version)) = (name, found_version) else {
            return Err(failure(format!(
                "{file} holds no literal name and version (a workspace-inherited or dynamic version cannot be released)"
            )));
        };
        found.push(Package {
            registry,
            dir: dir.to_owned(),
            name,
            file,
            version: found_version,
        });
    }
    Ok(found)
}

/// Every package must carry the release version; the message names all of them.
fn agree(packages: &[Package], release_version: &str) -> Result<()> {
    if packages.iter().all(|p| p.version == release_version) {
        return Ok(());
    }
    let mut message = format!("the packages do not carry the release version {release_version}:");
    for package in packages {
        message.push_str(&format!(" {} says {};", package.file, package.version));
    }
    message.push_str(" bump them together before the release");
    Err(failure(message))
}

/// PyPI's file-name form of a project name: lower case, `-` and `.` as `_` (the rule the publisher uses).
fn normalised(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '-' | '.' => '_',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

/// The npm tarball name: a scoped `@scope/name` becomes `scope-name`.
fn tarball_stem(name: &str) -> String {
    name.trim_start_matches('@').replace('/', "-")
}

/// The files `package` must leave, exactly.
fn expected(package: &Package) -> Vec<String> {
    let v = &package.version;
    match package.registry {
        "crates" => vec![format!("{}-{v}.crate", package.name)],
        "npm" => vec![format!("{}-{v}.tgz", tarball_stem(&package.name))],
        _ => {
            let norm = normalised(&package.name);
            vec![
                format!("{norm}-{v}.tar.gz"),
                format!("{norm}-{v}-py3-none-any.whl"),
            ]
        }
    }
}

/// The files of `dir` must be exactly `expected` (a tool's own `.gitignore` marker aside). A platform wheel, a second
/// tarball or a stray file is refused by name.
fn exact_files(dir: &Path, expected: &[String], what: &str) -> Result<()> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name != ".gitignore" {
            found.push(name);
        }
    }
    found.sort();
    let mut wanted = expected.to_vec();
    wanted.sort();
    if found == wanted {
        return Ok(());
    }
    let unexpected: Vec<&String> = found.iter().filter(|n| !wanted.contains(n)).collect();
    let missing: Vec<&String> = wanted.iter().filter(|n| !found.contains(n)).collect();
    Err(failure(format!(
        "{what} left {} where {} was expected (unexpected: {unexpected:?}, missing: {missing:?}); a native wheel is a binary with platforms, not a library pack",
        found.join(", "),
        wanted.join(", ")
    )))
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// Run a pack check. Called with the release build's environment: the release is described by `CCID_RELEASE_*`.
pub(crate) fn run(check: &Check, runner: &Runner) -> Result<()> {
    let need = |name: &str| {
        value(&runner.environment, name)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                failure(format!(
                "a pack check runs inside a release run (ccid release build): ${name} is not set"
            ))
            })
    };
    let out = PathBuf::from(need("CCID_RELEASE_DIR")?);
    let release_version = need("CCID_RELEASE_VERSION")?;
    let commit = need("CCID_RELEASE_COMMIT")?;
    validate(check)?;
    // Everything that needs no tool first: the versions, the constraint file, the names.
    let packages = packages(&runner.root, check)?;
    agree(&packages, &release_version)?;
    for package in packages.iter().filter(|p| p.registry == "pypi") {
        let constraints = join(&package.dir, BUILD_CONSTRAINTS);
        if !runner.root.join(&constraints).is_file() {
            return Err(failure(format!(
                "pack pypi: {constraints} is missing: the build backend is pinned by hashes in that lock file (uv pip compile --universal --generate-hashes)"
            )));
        }
    }
    let wanted: Vec<(String, String)> = packages
        .iter()
        .flat_map(|p| expected(p).into_iter().map(move |f| (f, p.file.clone())))
        .collect();
    for (name, source) in &wanted {
        if out.join(name).exists() {
            return Err(failure(format!(
                "pack: {name} (from {source}) already exists in $CCID_RELEASE_DIR"
            )));
        }
    }
    let epoch = runner
        .run(&argv(&["git", "log", "-1", "--format=%ct", &commit]), true)?
        .trim()
        .to_owned();
    let scratch = tempfile::Builder::new().prefix("ccid-pack-").tempdir_in(
        value(&runner.environment, "TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from),
    )?;
    let mut environment = runner.environment.clone();
    set(&mut environment, "SOURCE_DATE_EPOCH", epoch);
    // The runner's own bun, uv and npm caches are shared by every job of the pool, and `prepack`, the build backend and the
    // dependency builds run from what they hold: a pack starts from empty caches of its own (gone with the scratch directory).
    // uv fetches no interpreter and uses none of its own managed ones (they sit in a directory the pool shares): the Python
    // it builds with is one the runner provides.
    for (name, directory) in [
        ("BUN_INSTALL_CACHE_DIR", "bun-cache"),
        ("UV_CACHE_DIR", "uv-cache"),
        ("npm_config_cache", "npm-cache"),
    ] {
        set(&mut environment, name, scratch.path().join(directory));
    }
    set(&mut environment, "UV_PYTHON_DOWNLOADS", "never");
    set(&mut environment, "UV_PYTHON_PREFERENCE", "only-system");
    // The clean source of the commit: untracked files cannot enter a package.
    let stage = scratch.path().join("source");
    let staged = packages.iter().any(|p| p.registry != "crates");
    if staged {
        fs::create_dir(&stage)?;
        let archive = scratch.path().join("source.tar");
        let archive_arg = archive.to_string_lossy().into_owned();
        runner.run(
            &argv(&[
                "git",
                "archive",
                "--format=tar",
                "--output",
                &archive_arg,
                &commit,
            ]),
            false,
        )?;
        runner.run(
            &argv(&["tar", "-xf", &archive_arg, "-C", &stage.to_string_lossy()]),
            false,
        )?;
    }
    let mut results: Vec<(PathBuf, String)> = Vec::new();
    for package in &packages {
        let made = scratch.path().join(package.registry);
        fs::create_dir(&made)?;
        let made_arg = made.to_string_lossy().into_owned();
        match package.registry {
            "crates" => {
                let target = scratch.path().join("cargo-target");
                let mut cargo_environment = environment.clone();
                set(
                    &mut cargo_environment,
                    "CARGO_TARGET_DIR",
                    target.as_os_str(),
                );
                let cargo = Runner::until(runner.root.clone(), cargo_environment, runner.deadline)?;
                let manifest = join(&package.dir, "Cargo.toml");
                cargo.run(
                    &argv(&["cargo", "fetch", "--locked", "--manifest-path", &manifest]),
                    false,
                )?;
                // Never --allow-dirty: the package must be what `cargo publish` uploads from the clean checkout.
                cargo.run(
                    &argv(&[
                        "cargo",
                        "package",
                        "--locked",
                        "--offline",
                        "--manifest-path",
                        &manifest,
                        "--package",
                        &package.name,
                    ]),
                    false,
                )?;
                for name in expected(package) {
                    fs::copy(target.join("package").join(&name), made.join(&name)).map_err(
                        |error| failure(format!("cargo package left no {name}: {error}")),
                    )?;
                }
            }
            "npm" => {
                let npm = Runner::until(
                    stage.join(&package.dir),
                    environment.clone(),
                    runner.deadline,
                )?;
                if npm.root.join("bun.lock").is_file() || npm.root.join("bun.lockb").is_file() {
                    npm.run(&argv(&["bun", "install", "--frozen-lockfile"]), false)?;
                } else if npm.root.join("package-lock.json").is_file() {
                    npm.run(
                        &argv(&["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]),
                        false,
                    )?;
                } else {
                    return Err(failure(format!(
                        "pack npm: {} has no bun.lock or package-lock.json to install from",
                        package.dir
                    )));
                }
                // `prepack` runs here: it is the package's build.
                npm.run(
                    &argv(&["npm", "pack", "--offline", "--pack-destination", &made_arg]),
                    false,
                )?;
                exact_files(&made, &expected(package), "npm pack")?;
                let tarball = made.join(&expected(package)[0]);
                let inside = npm.run(
                    &argv(&[
                        "tar",
                        "-xOzf",
                        &tarball.to_string_lossy(),
                        "package/package.json",
                    ]),
                    true,
                )?;
                let json: serde_json::Value = serde_json::from_str(&inside).map_err(|error| {
                    failure(format!("the npm tarball holds no package.json: {error}"))
                })?;
                let pair = |name: &str| {
                    json.get(name)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned()
                };
                if pair("name") != package.name || pair("version") != package.version {
                    return Err(failure(format!(
                        "the npm tarball is {} {}, {} says {} {}",
                        pair("name"),
                        pair("version"),
                        package.file,
                        package.name,
                        package.version
                    )));
                }
            }
            _ => {
                let pypi = Runner::until(
                    stage.join(&package.dir),
                    environment.clone(),
                    runner.deadline,
                )?;
                pypi.run(
                    &argv(&[
                        "uv",
                        "build",
                        "--no-config",
                        "--build-constraints",
                        BUILD_CONSTRAINTS,
                        "--require-build-hashes",
                        "--out-dir",
                        &made_arg,
                        ".",
                    ]),
                    false,
                )?;
                exact_files(&made, &expected(package), "uv build")?;
            }
        }
        for name in expected(package) {
            results.push((made.join(&name), name));
        }
    }
    // Only after every package is made does anything reach the release directory.
    for (from, name) in results {
        fs::copy(&from, out.join(&name)).map_err(|error| {
            failure(format!(
                "pack cannot place {name} in $CCID_RELEASE_DIR: {error}"
            ))
        })?;
        eprintln!("ccid pack: {name}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(crates: Option<&str>, npm: Option<&str>, pypi: Option<&str>) -> Check {
        Check {
            kind: KIND.into(),
            crates: crates.map(str::to_owned),
            npm: npm.map(str::to_owned),
            pypi: pypi.map(str::to_owned),
            ..Check::default()
        }
    }

    #[test]
    fn the_accepted_shapes_and_every_refusal() {
        assert!(validate(&check(Some("."), Some("js/@s/p"), Some("py"))).is_ok());
        assert!(validate(&check(None, None, Some("py"))).is_ok());
        for bad in [
            check(None, None, None),
            check(Some("/abs"), None, None),
            check(Some("../up"), None, None),
            check(None, Some("a/../b"), None),
            check(None, None, Some("")),
        ] {
            assert!(validate(&bad).is_err(), "{bad:?}");
        }
        // A pack takes no command, no cache key and no other kind's key.
        let mut with_commands = check(Some("."), None, None);
        with_commands.commands = vec![vec!["true".into()]];
        assert!(validate(&with_commands).is_err());
        let mut cached = check(Some("."), None, None);
        cached.cache_inputs = Some(vec!["src/**".into()]);
        assert!(validate(&cached).is_err());
        let mut tools = check(Some("."), None, None);
        tools.cache_tools = vec![vec!["cargo".into(), "--version".into()]];
        assert!(validate(&tools).is_err());
    }

    #[test]
    fn the_directory_keys_belong_to_kind_pack_only() {
        let mut stray = Check {
            kind: "commands".into(),
            commands: vec![vec!["true".into()]],
            ..Check::default()
        };
        stray.crates = Some(".".into());
        assert!(crate::checks::validate_check(&stray).is_err());
        assert!(crate::checks::validate_check(&check(Some("."), None, None)).is_ok());
    }

    #[test]
    fn the_registries_follow_the_keys() {
        assert_eq!(
            registries(&check(Some("."), None, Some("py"))),
            vec![release::Registry::Crates, release::Registry::Pypi]
        );
    }

    #[test]
    fn the_file_names_follow_each_registrys_rule() {
        let package = |registry: &'static str, name: &str| Package {
            registry,
            dir: ".".into(),
            name: name.into(),
            file: "f".into(),
            version: "1.2.0".into(),
        };
        assert_eq!(
            expected(&package("crates", "cgreet")),
            ["cgreet-1.2.0.crate"]
        );
        assert_eq!(
            expected(&package("npm", "@corbet-labs/cgreet")),
            ["corbet-labs-cgreet-1.2.0.tgz"]
        );
        assert_eq!(expected(&package("npm", "plain")), ["plain-1.2.0.tgz"]);
        assert_eq!(
            expected(&package("pypi", "My-Pkg.Name")),
            [
                "my_pkg_name-1.2.0.tar.gz",
                "my_pkg_name-1.2.0-py3-none-any.whl"
            ]
        );
    }

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, text).unwrap();
        }
        dir
    }

    #[test]
    fn versions_are_read_from_each_manifest_and_must_agree() {
        let dir = tree(&[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"1.2.0\"\n",
            ),
            (
                "js/package.json",
                "{\"name\":\"@s/demo\",\"version\":\"1.2.0\"}",
            ),
            (
                "py/pyproject.toml",
                "[project]\nname = \"demo\"\nversion = \"1.2.1\"\n",
            ),
        ]);
        let found = packages(dir.path(), &check(Some("."), Some("js"), Some("py"))).unwrap();
        assert_eq!(found.len(), 3);
        assert_eq!(found[1].name, "@s/demo");
        let refusal = agree(&found, "1.2.0").unwrap_err().to_string();
        assert!(
            refusal.contains("py/pyproject.toml says 1.2.1"),
            "{refusal}"
        );
        assert!(refusal.contains("Cargo.toml says 1.2.0"), "{refusal}");
        assert!(agree(&found[..2], "1.2.0").is_ok());
    }

    #[test]
    fn an_inherited_or_dynamic_version_is_refused() {
        let inherited = tree(&[(
            "Cargo.toml",
            "[package]\nname = \"d\"\nversion.workspace = true\n",
        )]);
        assert!(packages(inherited.path(), &check(Some("."), None, None)).is_err());
        let dynamic = tree(&[(
            "pyproject.toml",
            "[project]\nname = \"d\"\ndynamic = [\"version\"]\n",
        )]);
        let refusal = packages(dynamic.path(), &check(None, None, Some(".")))
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("dynamic"), "{refusal}");
        let missing = tempfile::tempdir().unwrap();
        assert!(packages(missing.path(), &check(Some("."), None, None)).is_err());
    }

    #[test]
    fn the_output_set_must_be_exact() {
        let wanted = vec![
            "d-1.0.0.tar.gz".to_owned(),
            "d-1.0.0-py3-none-any.whl".to_owned(),
        ];
        let good = tree(&[
            ("d-1.0.0.tar.gz", ""),
            ("d-1.0.0-py3-none-any.whl", ""),
            (".gitignore", "*"),
        ]);
        assert!(exact_files(good.path(), &wanted, "uv build").is_ok());
        let native = tree(&[
            ("d-1.0.0.tar.gz", ""),
            ("d-1.0.0-cp312-linux_x86_64.whl", ""),
        ]);
        let refusal = exact_files(native.path(), &wanted, "uv build")
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("cp312") && refusal.contains("missing"),
            "{refusal}"
        );
        let short = tree(&[("d-1.0.0.tar.gz", "")]);
        assert!(exact_files(short.path(), &wanted, "uv build").is_err());
    }
}

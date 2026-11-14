use super::*;

fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    dir
}

fn apply_all(dir: &Path) {
    for plan in plan_files(dir, Dependencies::Renovate).unwrap() {
        let path = dir.join(&plan.path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, plan.expected).unwrap();
    }
}

#[test]
fn the_text_names_no_address_and_no_forge() {
    for text in [
        readme_block("SECURITY.md", Dependencies::Renovate),
        policy_block(Dependencies::Renovate),
        release_section(Dependencies::Renovate),
    ] {
        assert!(!text.contains('@'), "{text}");
        assert!(!text.to_lowercase().contains("mailto"), "{text}");
        for forge in ["forge.corbet", "git.corbet", "gitlab.com", "bitbucket"] {
            assert!(!text.contains(forge), "{forge}");
        }
    }
}

#[test]
fn the_policy_covers_what_runs_the_copies_and_the_reporting_route() {
    let text = policy_block(Dependencies::Renovate);
    for needle in [
        "Renovate",
        "OSV",
        "grouped",
        "dependency dashboard",
        "Copies run no CI",
        "issue tracker",
        "private vulnerability reporting",
    ] {
        assert!(
            text.to_lowercase().contains(&needle.to_lowercase()),
            "{needle}"
        );
    }
}

#[test]
fn a_readme_gains_its_block_after_the_title_and_a_missing_policy_is_created() {
    let dir = tree(&[("README.md", "# Demo\n\nText.\n")]);
    let plans = plan_files(dir.path(), Dependencies::Renovate).unwrap();
    assert_eq!(plans.len(), 2);
    assert!(plans.iter().all(|plan| plan.introduced && plan.drifted()));
    apply_all(dir.path());
    let readme = fs::read_to_string(dir.path().join("README.md")).unwrap();
    assert!(
        readme.starts_with("# Demo\n\n<!-- ccid:security:begin"),
        "{readme}"
    );
    assert!(readme.ends_with("Text.\n"));
    let policy = fs::read_to_string(dir.path().join("SECURITY.md")).unwrap();
    assert!(policy.starts_with("# Security policy\n\n"));
    assert!(plan_files(dir.path(), Dependencies::Renovate)
        .unwrap()
        .iter()
        .all(|plan| !plan.drifted()));
}

#[test]
fn a_hand_written_policy_keeps_its_text_and_only_the_block_is_ours() {
    let hand = "# Security\n\nSupported versions: 2.x\n";
    let dir = tree(&[(".github/SECURITY.md", hand), ("README.md", "# D\n")]);
    apply_all(dir.path());
    assert!(!dir.path().join("SECURITY.md").exists());
    let policy = fs::read_to_string(dir.path().join(".github/SECURITY.md")).unwrap();
    assert!(policy.starts_with(hand));
    assert!(policy.contains("## Copies run no CI"));
    let readme = fs::read_to_string(dir.path().join("README.md")).unwrap();
    assert!(readme.contains("(.github/SECURITY.md)"));
    // A hand edit inside the block is drift; text outside it is not.
    let edited = policy.replace("Renovate", "Dependabot") + "\nMore text.\n";
    fs::write(dir.path().join(".github/SECURITY.md"), &edited).unwrap();
    let plans = plan_files(dir.path(), Dependencies::Renovate).unwrap();
    let plan = plans
        .iter()
        .find(|p| p.path.ends_with("SECURITY.md"))
        .unwrap();
    assert!(plan.drifted() && !plan.introduced);
    assert!(plan.expected.contains("More text.") && !plan.expected.contains("Dependabot"));
}

#[test]
fn a_repository_without_a_readme_gets_none_and_a_symlink_is_refused() {
    let dir = tree(&[]);
    let plans = plan_files(dir.path(), Dependencies::Renovate).unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].path, Path::new("SECURITY.md"));
    #[cfg(unix)]
    {
        let dir = tree(&[("real.md", "# x\n")]);
        std::os::unix::fs::symlink("real.md", dir.path().join("README.md")).unwrap();
        assert!(plan_files(dir.path(), Dependencies::Renovate).is_err());
    }
}

#[test]
fn the_dependency_sentence_follows_the_declared_value() {
    let renovate = policy_block(Dependencies::Renovate);
    assert!(renovate.contains("Renovate") && renovate.contains("only when its tests are green"));
    assert!(readme_block("SECURITY.md", Dependencies::Renovate)
        .contains("an update is merged only when its tests pass"));
    assert!(release_section(Dependencies::Renovate).contains("watched by Renovate"));

    let deny = policy_block(Dependencies::CargoDeny);
    assert!(deny.contains("RustSec advisories (cargo deny) in CI"));
    assert!(!deny.contains("Renovate") && !deny.contains("OSV"));
    assert!(readme_block("SECURITY.md", Dependencies::CargoDeny).contains("cargo deny"));
    assert!(!readme_block("SECURITY.md", Dependencies::CargoDeny).contains("Renovate"));
    let notes = release_section(Dependencies::CargoDeny);
    assert!(notes.contains("cargo deny") && !notes.contains("Renovate"));

    let none = policy_block(Dependencies::None);
    assert!(
        !none.contains("Renovate") && !none.contains("cargo deny") && !none.contains("ependenc")
    );
    let readme = readme_block("SECURITY.md", Dependencies::None);
    assert!(!readme.to_lowercase().contains("dependenc") && readme.contains("security policy"));
    let notes = release_section(Dependencies::None);
    assert!(!notes.contains("Renovate") && !notes.contains("cargo deny"));
    assert!(notes.contains("report a problem"));

    for value in [
        Dependencies::Renovate,
        Dependencies::CargoDeny,
        Dependencies::None,
    ] {
        let text = policy_block(value);
        assert!(text.contains("Copies run no CI"), "{value:?}");
        assert!(
            text.contains("Releases, where a project has them"),
            "{value:?}"
        );
    }
}

#[test]
fn a_changed_value_rewrites_only_the_block_and_the_manifest_key_parses() {
    let dir = tree(&[("README.md", "# Demo\n\nText.\n")]);
    for plan in plan_files(dir.path(), Dependencies::Renovate).unwrap() {
        fs::write(dir.path().join(&plan.path), &plan.expected).unwrap();
    }
    let plans = plan_files(dir.path(), Dependencies::CargoDeny).unwrap();
    assert!(plans.iter().all(|plan| plan.drifted() && !plan.introduced));
    let readme = plans
        .iter()
        .find(|plan| plan.path == Path::new("README.md"))
        .unwrap();
    assert!(readme.expected.contains("cargo deny") && readme.expected.ends_with("Text.\n"));

    let config: Config = toml::from_str("dependencies = \"cargo-deny\"").unwrap();
    assert_eq!(config.dependencies, Dependencies::CargoDeny);
    assert_eq!(
        toml::from_str::<Config>("").unwrap().dependencies,
        Dependencies::Renovate
    );
    assert!(toml::from_str::<Config>("dependencies = \"npm\"").is_err());
    assert!(toml::from_str::<Config>("other = 1").is_err());
}

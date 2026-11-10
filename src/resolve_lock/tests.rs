use super::*;

#[test]
fn config_defaults_to_printing_the_lock() {
    let config: Config = toml::from_str("").unwrap();
    assert!(config.print_lock);
}

#[test]
fn config_print_lock_can_be_suppressed() {
    let config: Config = toml::from_str("[resolve]\nprint_lock = false\n")
        .map(|wrapper: ConfigWrapper| wrapper.resolve)
        .unwrap();
    assert!(!config.print_lock);
}

#[test]
fn config_rejects_unknown_keys() {
    let result: std::result::Result<ConfigWrapper, toml::de::Error> =
        toml::from_str("[resolve]\npackages = []\n");
    assert!(result.is_err());
}

#[derive(Debug, Deserialize)]
struct ConfigWrapper {
    #[serde(default)]
    resolve: Config,
}

#[test]
fn revision_accepts_only_40_lowercase_hex() {
    assert!(valid_revision(&"a".repeat(40)));
    assert!(valid_revision("0123456789abcdef0123456789abcdef01234567"));
    for bad in [
        "",
        "short",
        &"a".repeat(39),
        &"a".repeat(41),
        &"A".repeat(40),
        "/bad",
    ] {
        assert!(!valid_revision(bad), "{bad}");
    }
}

#[test]
fn artifact_root_must_be_absolute_and_outside_the_repo() {
    let repo = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    assert!(check_artifact_root(repo.path(), outside.path()).is_ok());
    assert!(check_artifact_root(repo.path(), Path::new("relative")).is_err());
    assert!(
        check_artifact_root(repo.path(), &repo.path().join("artifacts")).is_err(),
        "inside the repository"
    );
    assert!(check_artifact_root(repo.path(), repo.path()).is_err());
    let escape = outside.path().join("..").join("x");
    assert!(
        check_artifact_root(repo.path(), &escape).is_err(),
        "'..' is refused"
    );
}

#[cfg(unix)]
#[test]
fn artifact_root_through_a_link_into_the_repo_is_refused() {
    let repo = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let link = outside.path().join("link");
    std::os::unix::fs::symlink(repo.path(), &link).unwrap();
    assert!(check_artifact_root(repo.path(), &link).is_err());
    assert!(
        check_artifact_root(repo.path(), &link.join("not-yet").join("there")).is_err(),
        "a missing rest below the link is still inside"
    );
    assert!(check_artifact_root(repo.path(), &outside.path().join("new").join("dir")).is_ok());
}

#[test]
fn layout_dir_is_pinned_to_revision_and_npm() {
    let root = Path::new("/artifacts");
    assert_eq!(
        layout_dir(root, &"a".repeat(40)),
        PathBuf::from(format!("/artifacts/{}/resolve-npm", "a".repeat(40)))
    );
}

#[test]
fn staged_write_replaces_atomically() {
    let root = tempfile::tempdir().unwrap();
    let revision = "b".repeat(40);
    write_layout(root.path(), &revision, b"first\n").unwrap();
    write_layout(root.path(), &revision, b"second\n").unwrap();
    assert_eq!(
        fs::read(layout_dir(root.path(), &revision).join(LOCK_NAME)).unwrap(),
        b"second\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(layout_dir(root.path(), &revision).join(LOCK_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644, "the retired scripts' cp left 0644");
    }
}

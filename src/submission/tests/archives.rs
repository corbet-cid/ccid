use super::*;
use std::io::Cursor;

fn read(archive: &Path, name: &str) -> Vec<u8> {
    let mut archive = tar::Archive::new(File::open(archive).unwrap());
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.path().unwrap() == Path::new(name) {
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            return data;
        }
    }
    panic!("missing archive member {name}");
}
fn create(f: &Fixture) -> PathBuf {
    let path = f.root.path().join("source.tar");
    archive::create(&f.repo, &f.commit, &path).unwrap();
    path
}

#[test]
fn archive_preserves_commit_identity_and_excludes_dirty_content() {
    let f = Fixture::new();
    fs::write(f.repo.join("source.txt"), b"dirty").unwrap();
    let path = create(&f);
    assert_eq!(read(&path, "source.txt"), b"committed");
    let output = output(
        &strings(&["git", "get-tar-commit-id"]),
        None,
        Some(&fs::read(path).unwrap()),
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap().trim(), f.commit);
}
#[test]
fn malformed_lfs_pointer_fails_closed() {
    for data in [
        b"version https://git-lfs.github.com/spec/v1\noid sha256:wrong\nsize 8\n".as_slice(),
        b"version https://git-lfs.github.com/spec/v1\noid sha256:wrong\n",
    ] {
        assert!(archive::pointer(data).is_err());
    }
    assert!(archive::pointer(b"ordinary file").unwrap().is_none());
}
fn lfs(f: &mut Fixture, data: &[u8]) -> PathBuf {
    let hash = sha(data);
    let path = f
        .repo
        .join(".git/lfs/objects")
        .join(&hash[..2])
        .join(&hash[2..4])
        .join(&hash);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, data).unwrap();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{hash}\nsize {}\n",
        data.len()
    );
    f.commit(&[("font.bin", pointer.as_bytes())]);
    path
}
#[test]
fn lfs_payload_is_verified_and_hydrated() {
    let mut f = Fixture::new();
    lfs(&mut f, b"exact font contents");
    git(&f.repo, &["config", "filter.fixture.smudge", "false"]).unwrap();
    git(&f.repo, &["config", "filter.fixture.required", "true"]).unwrap();
    f.commit(&[(".gitattributes", b"font.bin filter=fixture\n")]);
    let path = f.root.path().join("source.tar");
    let receipt = archive::create(&f.repo, &f.commit, &path).unwrap();
    assert_eq!(read(&path, "font.bin"), b"exact font contents");
    assert_eq!(receipt["lfs_objects"], 1);
    assert_eq!(receipt["lfs_bytes"], 19);
}
#[test]
fn missing_or_corrupt_lfs_never_installs_archive() {
    let mut f = Fixture::new();
    let cached = lfs(&mut f, b"expected");
    let destination = f.root.path().join("source.tar");
    fs::write(&cached, b"wrong!!!").unwrap();
    assert!(archive::create(&f.repo, &f.commit, &destination).is_err());
    assert!(!destination.exists());
    fs::remove_file(cached).unwrap();
    assert!(archive::create(&f.repo, &f.commit, &destination).is_err());
    assert!(!destination.exists());
}
#[test]
fn export_ignored_generated_outputs_do_not_require_lfs_objects() {
    let mut f = Fixture::new();
    let cached = lfs(&mut f, b"ignored");
    fs::remove_file(cached).unwrap();
    f.commit(&[(".gitattributes", b"font.bin export-ignore\n")]);
    let path = f.root.path().join("source.tar");
    assert_eq!(
        archive::create(&f.repo, &f.commit, &path).unwrap()["lfs_objects"],
        0
    );
}
fn submodule(f: &mut Fixture) -> (PathBuf, String) {
    let path = f.repo.join("dependency");
    fs::create_dir(&path).unwrap();
    git(&path, &["init", "-q"]).unwrap();
    git(&path, &["config", "user.name", "fixture"]).unwrap();
    git(&path, &["config", "user.email", "fixture@example.invalid"]).unwrap();
    fs::write(path.join("library.txt"), b"pinned dependency").unwrap();
    git(&path, &["add", "library.txt"]).unwrap();
    git(&path, &["commit", "-qm", "fixture"]).unwrap();
    let revision = git(&path, &["rev-parse", "HEAD"]).unwrap();
    git(
        &f.repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{revision},dependency"),
        ],
    )
    .unwrap();
    f.commit(&[("root.txt", b"root")]);
    (path, revision)
}
#[test]
fn submodule_uses_exact_gitlink_commit_and_ignores_dirty_work() {
    let mut f = Fixture::new();
    let (child, revision) = submodule(&mut f);
    fs::write(child.join("library.txt"), b"newer").unwrap();
    git(&child, &["commit", "-qam", "newer"]).unwrap();
    fs::write(child.join("library.txt"), b"dirty").unwrap();
    let path = f.root.path().join("source.tar");
    let receipt = archive::create(&f.repo, &f.commit, &path).unwrap();
    assert_eq!(read(&path, "dependency/library.txt"), b"pinned dependency");
    assert_eq!(
        receipt["submodules"],
        json!([{"path":"dependency","commit":revision}])
    );
}
#[test]
fn isolated_worktree_reuses_exact_dependency_from_parent_worktree() {
    let mut f = Fixture::new();
    let (_, revision) = submodule(&mut f);
    let isolated = f.root.path().join("isolated");
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "--detach",
            &isolated.to_string_lossy(),
            &f.commit,
        ],
    )
    .unwrap();
    let path = f.root.path().join("source.tar");
    let receipt = archive::create(&isolated, &f.commit, &path).unwrap();
    assert_eq!(read(&path, "dependency/library.txt"), b"pinned dependency");
    assert_eq!(receipt["submodules"][0]["commit"], revision);
}
#[test]
fn full_ancestry_and_exact_source_survive_without_unrelated_refs() {
    let mut f = Fixture::new();
    f.commit(&[("next.txt", b"next")]);
    git(&f.repo, &["branch", "unrelated"]).unwrap();
    let path = f.root.path().join("source.bundle");
    archive::bundle(&f.repo, &f.commit, &path).unwrap();
    assert_eq!(
        git(&f.repo, &["bundle", "list-heads", &path.to_string_lossy()]).unwrap(),
        format!("{} refs/heads/source", f.commit)
    );
    let clone = f.root.path().join("clone");
    git(
        f.root.path(),
        &[
            "clone",
            "-q",
            &path.to_string_lossy(),
            &clone.to_string_lossy(),
        ],
    )
    .unwrap();
    assert_eq!(
        git(&clone, &["rev-list", "--count", "source"]).unwrap(),
        "2"
    );
}
#[test]
fn selected_older_commit_excludes_newer_history() {
    let mut f = Fixture::new();
    let old = f.commit.clone();
    f.commit(&[("next.txt", b"next")]);
    let path = f.root.path().join("source.bundle");
    archive::bundle(&f.repo, &old, &path).unwrap();
    let clone = f.root.path().join("clone");
    git(
        f.root.path(),
        &[
            "clone",
            "-q",
            &path.to_string_lossy(),
            &clone.to_string_lossy(),
        ],
    )
    .unwrap();
    assert!(git(&clone, &["cat-file", "-e", &f.commit]).is_err());
}
#[test]
fn shallow_checkout_is_rejected() {
    let f = Fixture::new();
    fs::write(f.repo.join(".git/shallow"), format!("{}\n", f.commit)).unwrap();
    assert!(archive::bundle(&f.repo, &f.commit, &f.root.path().join("source.bundle")).is_err());
}
#[test]
fn missing_history_fails_without_fetching() {
    let f = Fixture::new();
    assert!(archive::bundle(
        &f.repo,
        &"a".repeat(40),
        &f.root.path().join("source.bundle")
    )
    .is_err());
}
#[test]
fn replace_refs_cannot_rewrite_selected_commit() {
    let mut f = Fixture::new();
    let old = f.commit.clone();
    f.commit(&[("source.txt", b"replaced")]);
    git(&f.repo, &["replace", &old, &f.commit]).unwrap();
    let path = f.root.path().join("source.bundle");
    archive::bundle(&f.repo, &old, &path).unwrap();
    let clone = f.root.path().join("clone");
    git(
        f.root.path(),
        &[
            "clone",
            "-q",
            &path.to_string_lossy(),
            &clone.to_string_lossy(),
        ],
    )
    .unwrap();
    assert_eq!(
        git(&clone, &["show", "source:source.txt"]).unwrap(),
        "committed"
    );
}
#[test]
fn unicode_long_names_and_symlinks_round_trip() {
    let mut f = Fixture::new();
    let long = format!("{}/é😀.txt", "a".repeat(110));
    f.commit(&[(&long, b"unicode")]);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&long, f.repo.join("link")).unwrap();
        git(&f.repo, &["add", "link"]).unwrap();
        f.commit(&[("trigger", b"symlink")]);
    }
    let path = create(&f);
    assert_eq!(read(&path, &long), b"unicode");
    let mut reader = tar::Archive::new(Cursor::new(fs::read(path).unwrap()));
    assert!(reader.entries().unwrap().all(|e| e.is_ok()));
}

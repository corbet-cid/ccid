use super::*;

const REPO: &str = "/work/repo";
const TARGET: &str = "/cache/targets/repo-1";

fn globs(inputs: &Inputs) -> Vec<&str> {
    match inputs {
        Inputs::Everything => panic!("not narrowed"),
        Inputs::Globs(globs) => globs.iter().map(String::as_str).collect(),
    }
}

/// The shapes strace 7 prints with `-ff -y -v`, as seen in a captured trace.
fn traces() -> BTreeMap<u32, Vec<Event>> {
    let marker = format!("[\"PATH=/usr/bin\", \"{MARKER}={TARGET}\", \"HOME=/home/x\"]");
    // pid 10: ccid itself, outside the check. It stages the whole tree, then runs the command.
    let ccid = format!(
        r#"execve("/bin/ccid", ["ccid", "check"], ["PATH=/usr/bin"]) = 0
openat(AT_FDCWD<{REPO}>, "README.md", O_RDONLY|O_CLOEXEC) = 3<{REPO}/README.md>
getdents64(3<{REPO}>, [{{d_ino=1, d_off=1, d_reclen=24, d_type=DT_REG, d_name="README.md"}}], 32768) = 72
clone3({{flags=CLONE_VM|CLONE_VFORK, exit_signal=SIGCHLD, stack=0x7f, stack_size=0x9000}}, 88) = 11
--- SIGCHLD {{si_signo=SIGCHLD, si_code=CLD_EXITED, si_pid=11, si_uid=1000, si_status=0}} ---
"#
    );
    // pid 11: the check command, marked by its environment; runs in the staged copy.
    let check = format!(
        r#"execve("/usr/bin/sh", ["sh", "-c", "cat src/a.rs"], {marker}) = 0
chdir("{TARGET}/.ccid/source-v1")          = 0
newfstatat(AT_FDCWD<{TARGET}/.ccid/source-v1>, "build.rs", 0x7ffe, 0) = -1 ENOENT (No such file or directory)
openat(AT_FDCWD<{TARGET}/.ccid/source-v1>, "src/a\x20b.rs", O_RDONLY) = 3<{TARGET}/.ccid/source-v1/src/a b.rs>
readlink("Cargo.toml", "/elsewhere/Cargo.toml", 4096) = 21
openat(AT_FDCWD<{TARGET}/.ccid/source-v1>, ".git/HEAD", O_RDONLY) = 3<{TARGET}/.ccid/source-v1/.git/HEAD>
openat(AT_FDCWD<{TARGET}/.ccid/source-v1>, "inputs", O_RDONLY|O_NONBLOCK|O_CLOEXEC|O_DIRECTORY) = 4<{TARGET}/.ccid/source-v1/inputs>
getdents64(4<{TARGET}/.ccid/source-v1/inputs>, [{{d_ino=2, d_off=2, d_reclen=24, d_type=DT_DIR, d_name="interviews"}}], 32768) = 48
newfstatat(AT_FDCWD<{TARGET}/.ccid/source-v1>, "/usr/lib/libc.so", {{st_mode=S_IFREG|0755, st_size=1}}, 0) = 0
clone(child_stack=NULL, flags=CLONE_CHILD_CLEARTID|CLONE_CHILD_SETTID|SIGCHLD, child_tidptr=0x7) = 12
"#
    );
    // pid 12: a child of the check that clears its environment: still the check (lineage).
    let child = format!(
        r#"execve("/usr/bin/cat", ["cat"], ["PATH=/usr/bin"]) = 0
stat("notes.txt", 0x7ffe) = -1 ENOENT (No such file or directory)
openat(AT_FDCWD<{TARGET}/.ccid/source-v1/tests>, "../docs/guide.md", O_RDONLY) = 3<{TARGET}/.ccid/source-v1/docs/guide.md>
"#
    );
    BTreeMap::from([
        (10, events(&ccid)),
        (11, events(&check)),
        (12, events(&child)),
    ])
}

#[test]
fn only_the_marked_lineage_counts_and_the_stage_maps_back_to_the_checkout() {
    let traces = traces();
    let (observations, stages) = check_observations(&traces, Path::new(REPO));
    assert_eq!(
        stages,
        BTreeSet::from([PathBuf::from(format!("{TARGET}/.ccid/source-v1"))])
    );
    // ccid's own README read and root listing (pid 10, before the marked exec) are not inputs.
    assert!(!observations
        .iter()
        .any(|(path, _)| path == Path::new(&format!("{REPO}/README.md"))));
    let roots = vec![PathBuf::from(REPO), stages.into_iter().next().unwrap()];
    let inputs = generalise(&observations, &roots, 1).unwrap();
    assert_eq!(
        globs(&inputs),
        [
            ".ci/**",
            // readlink's first argument, relative to the check's directory.
            "Cargo.toml",
            // An absent probe at the top: the file or a directory of that name.
            "build.rs",
            "build.rs/**",
            // The child resolved `../docs/guide.md` against its decorated cwd.
            "docs/**",
            // Listed (a test asserting the directory) and read below.
            "inputs/**",
            // The child's relative probe, placed with the cwd it inherited.
            "notes.txt",
            "notes.txt/**",
            "src/**",
        ]
    );
    // pid 12's relative stat("notes.txt") came before any decorated cwd of its own: it is placed
    // with the cwd inherited at the clone, the stage root.
    let notes = observations
        .iter()
        .find(|(path, _)| path.ends_with("notes.txt"))
        .expect("relative probe placed with the inherited cwd");
    assert_eq!(notes.1, Seen::Absent);
}

#[test]
fn the_cwd_of_a_child_comes_from_its_parent_at_the_clone() {
    // pid 12 above: the inherited cwd places stat("notes.txt") at the stage root, so it is the
    // top-level name notes.txt (and possibly a directory of that name).
    let traces = traces();
    let (observations, stages) = check_observations(&traces, Path::new(REPO));
    let mut roots = vec![PathBuf::from(REPO)];
    roots.extend(stages);
    let found: Vec<_> = observations
        .iter()
        .filter(|(path, _)| path.ends_with("notes.txt"))
        .map(|(path, _)| path.clone())
        .collect();
    assert_eq!(
        found,
        [PathBuf::from(format!("{TARGET}/.ccid/source-v1/notes.txt"))]
    );
    assert!(globs(&generalise(&observations, &roots, 1).unwrap()).contains(&"notes.txt"));
}

#[test]
fn listing_the_root_keeps_the_whole_tree() {
    let roots = vec![PathBuf::from(REPO)];
    let observations = vec![
        (PathBuf::from(format!("{REPO}/src/lib.rs")), Seen::Present),
        (PathBuf::from(REPO), Seen::Listed),
    ];
    assert_eq!(
        generalise(&observations, &roots, 1).unwrap(),
        Inputs::Everything
    );
    // Changing into the root, or stat'ing it, is not a listing.
    let observations = vec![
        (PathBuf::from(REPO), Seen::Directory),
        (PathBuf::from(REPO), Seen::Present),
        (PathBuf::from(format!("{REPO}/src/lib.rs")), Seen::Present),
    ];
    assert_eq!(
        globs(&generalise(&observations, &roots, 1).unwrap()),
        [".ci/**", "src/**"]
    );
}

#[test]
fn paths_outside_the_roots_and_tool_state_are_not_inputs() {
    let roots = vec![PathBuf::from(REPO)];
    let observations = vec![
        (PathBuf::from("/usr/lib/libc.so"), Seen::Present),
        (PathBuf::from("/work/repository-next/x"), Seen::Present),
        (PathBuf::from(format!("{REPO}/.git/index")), Seen::Present),
        (PathBuf::from(format!("{REPO}/.moon/cache/x")), Seen::Absent),
        (
            PathBuf::from(format!("{REPO}/.ccid/results/x")),
            Seen::Present,
        ),
    ];
    assert_eq!(
        globs(&generalise(&observations, &roots, 1).unwrap()),
        [".ci/**"]
    );
}

#[test]
fn deeper_generalisation_and_metacharacters() {
    let roots = vec![PathBuf::from(REPO)];
    let observations = vec![
        (
            PathBuf::from(format!("{REPO}/crates/a/src/lib.rs")),
            Seen::Present,
        ),
        (PathBuf::from(format!("{REPO}/crates/b")), Seen::Absent),
        (PathBuf::from(format!("{REPO}/odd[1]*.txt")), Seen::Present),
    ];
    assert_eq!(
        globs(&generalise(&observations, &roots, 2).unwrap()),
        [
            ".ci/**",
            "crates/a/**",
            "crates/b",
            "crates/b/**",
            "odd\\[1\\]\\*.txt"
        ]
    );
    assert_eq!(
        globs(&generalise(&observations, &roots, 1).unwrap()),
        [".ci/**", "crates/**", "odd\\[1\\]\\*.txt"]
    );
}

#[test]
fn strace_strings_are_unescaped() {
    assert_eq!(unescape(r"a\x20b"), b"a b");
    assert_eq!(unescape(r"\303\251t\303\251"), "été".as_bytes());
    assert_eq!(unescape(r#"q\"uote\\d"#), br#"q"uote\d"#);
    assert_eq!(unescape(r"line\nnext\t"), b"line\nnext\t");
    assert_eq!(unescape("plain"), b"plain");
}

#[test]
fn truncated_failed_and_unfinished_lines_are_handled() {
    let parsed = events(&format!(
        "openat(AT_FDCWD<{REPO}>, \"long\"..., O_RDONLY) = 3<{REPO}/longer>\n\
         openat(AT_FDCWD<{REPO}>, \"x\", O_RDONLY <unfinished ...>\n\
         newfstatat(AT_FDCWD<{REPO}>, \"gone\", 0x1, 0) = -1 ENOTDIR (Not a directory)\n\
         openat(AT_FDCWD<{REPO}>, \"locked\", O_RDONLY) = -1 EACCES (Permission denied)\n\
         newfstatat(3<pipe:[123]>, \"\", {{st_mode=S_IFIFO|0600}}, AT_EMPTY_PATH) = 0\n\
         fstat(4<socket:[9]>, \"x\") = 0\n\
         +++ exited with 0 +++\n"
    ));
    let paths: Vec<(String, Seen)> = parsed
        .iter()
        .filter_map(|event| match event {
            Event::Path(path, seen) => Some((path.display().to_string(), *seen)),
            _ => None,
        })
        .collect();
    assert_eq!(
        paths,
        [
            (format!("{REPO}/gone"), Seen::Absent),
            // It exists (an error other than "not found").
            (format!("{REPO}/locked"), Seen::Present),
        ]
    );
}

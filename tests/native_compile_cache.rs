//! Explicit worker acceptance for native compiler caches. No tool installation.
#![forbid(unsafe_code)]
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn trial(root: &Path, shared: &Path, path: &std::ffi::OsStr, success: bool) -> Output {
    let timer = tempfile::NamedTempFile::new_in(shared).unwrap();
    let output = Command::new(std::env::var_os("CCID_TEST_TIME").unwrap())
        .args(["-q", "-f", "{\"user\":%U,\"system\":%S}", "-o"])
        .arg(timer.path())
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_ccid"))
        .args(["check", "--repo"])
        .arg(root)
        .args(["--check", "test"])
        .env("PATH", path)
        .env("CCACHE_DIR", shared.join("ccache"))
        .env("TSC_CACHE_DIR", shared.join("typescript"))
        .env("GOCACHE", shared.join("go"))
        .env("CARGO_TARGET_DIR", shared.join("targets"))
        .env(
            "CI_REPOSITORY_URL",
            "https://example.invalid/test/native-cache-proof",
        )
        .env("CI_JOBS", "2")
        .env("CI_TIMEOUT", "120")
        .env_remove("CCID_RESULT_CACHE")
        .env_remove("CCID_CACHE_CHILD")
        .env_remove("CCID_TARGET_LOCK_HELD")
        .env_remove("CCID_RECEIPT")
        .output()
        .unwrap();
    println!("{}", String::from_utf8_lossy(&output.stdout));
    println!("{}", String::from_utf8_lossy(&output.stderr));
    let cpu: Value = serde_json::from_slice(&fs::read(timer.path()).unwrap()).unwrap();
    println!(
        "{}",
        json!({"event":"native-cache-proof","case":root.file_name().unwrap().to_string_lossy(),"cpu":cpu,"success":output.status.success()})
    );
    assert_eq!(output.status.success(), success);
    output
}

fn fixture(parent: &Path, name: &str, manifest: &str) -> PathBuf {
    let root = parent.join(name);
    fs::create_dir_all(root.join(".ci")).unwrap();
    fs::write(
        root.join(".ci/ccid.toml"),
        format!("schema=1\nproject='native-cache-proof'\n[checks.test]\n{manifest}\n"),
    )
    .unwrap();
    root
}

#[test]
#[ignore = "Worker proof requires CCID_TEST_CCACHE, CCID_TEST_NODE and CCID_TEST_TIME"]
fn native_c_cpp_and_typescript_validate_changed_sources() {
    let parent = tempfile::tempdir().unwrap();
    let shared = parent.path().join("cache");
    for name in ["ccache", "typescript", "targets"] {
        fs::create_dir_all(shared.join(name)).unwrap();
    }
    let mut paths = vec![
        PathBuf::from(std::env::var_os("CCID_TEST_CCACHE").unwrap())
            .parent()
            .unwrap()
            .to_owned(),
        PathBuf::from(std::env::var_os("CCID_TEST_NODE").unwrap())
            .parent()
            .unwrap()
            .to_owned(),
    ];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(paths).unwrap();
    let mut objects = Vec::new();
    for name in ["c-cold", "c-warm"] {
        let root = fixture(parent.path(), name, "kind='commands'\ncommands=[['cc','-g','-c','main.c','-o','c.o'],['c++','-g','-c','main.cpp','-o','cpp.o']]");
        fs::write(
            root.join("main.c"),
            "const char *source = __FILE__; int value(void) { return 7; }\n",
        )
        .unwrap();
        fs::write(root.join("main.cpp"), "const char *source = __FILE__; template<int N> int value() { return N; } int use() { return value<7>(); }\n").unwrap();
        let output = trial(&root, &shared, &path, true);
        if name == "c-warm" {
            let hits: u64 = String::from_utf8_lossy(&output.stderr)
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|v| v["backend"] == "ccache")
                .filter_map(|v| v["hits"].as_u64())
                .sum();
            assert_eq!(
                hits, 2,
                "Both C and C++ must hit native ccache across job paths"
            );
            assert_eq!(
                objects,
                vec![
                    fs::read(root.join("c.o")).unwrap(),
                    fs::read(root.join("cpp.o")).unwrap()
                ]
            );
            fs::write(root.join("main.c"), "this is invalid C;\n").unwrap();
            trial(&root, &shared, &path, false);
        } else {
            objects = vec![
                fs::read(root.join("c.o")).unwrap(),
                fs::read(root.join("cpp.o")).unwrap(),
            ];
        }
    }
    // Locked project dependency installation is the ordinary JavaScript check;
    // it reuses the executor's npm download cache, never installs a global tool.
    let package = json!({"name":"native-cache-proof","version":"1.0.0","private":true,"scripts":{"check":"tsc --noEmit --extendedDiagnostics"},"devDependencies":{"typescript":"5.9.3"}});
    let lock = json!({"name":"native-cache-proof","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"native-cache-proof","version":"1.0.0","devDependencies":{"typescript":"5.9.3"}},"node_modules/typescript":{"version":"5.9.3","resolved":"https://registry.npmjs.org/typescript/-/typescript-5.9.3.tgz","integrity":"sha512-jl1vZzPDinLr9eUt3J/t7V6FgNEw9QjvBPdysz9KfQDD41fQrC2Y4vKQdiaUpFT4bXlb1RHhLpp8wtm6M5TgSw==","dev":true,"bin":{"tsc":"bin/tsc","tsserver":"bin/tsserver"},"engines":{"node":">=14.17"}}}});
    for name in ["ts-cold", "ts-warm"] {
        let root = fixture(
            parent.path(),
            name,
            "kind='javascript'\nmanager='npm'\nscripts=['check']",
        );
        fs::write(
            root.join("package.json"),
            serde_json::to_vec(&package).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("package-lock.json"),
            serde_json::to_vec(&lock).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("tsconfig.json"),
            "{\"compilerOptions\":{\"strict\":true,\"types\":[]},\"files\":[\"main.ts\"]}",
        )
        .unwrap();
        fs::write(root.join("main.ts"), "export const value: number = 7;\n").unwrap();
        let output = trial(&root, &shared, &path, true);
        if name == "ts-warm" {
            assert!(String::from_utf8_lossy(&output.stderr).contains("\"state_restored\":true"));
            fs::write(
                root.join("main.ts"),
                "export const value: number = 'wrong';\n",
            )
            .unwrap();
            let output = trial(&root, &shared, &path, false);
            assert!(String::from_utf8_lossy(&output.stdout).contains("TS2322"));
        }
    }
}

#[test]
#[ignore = "Worker proof requires CCID_TEST_GO and CCID_TEST_TIME"]
fn native_go_reuses_portable_package_objects() {
    let parent = tempfile::tempdir().unwrap();
    let shared = parent.path().join("cache");
    fs::create_dir_all(&shared).unwrap();
    let mut paths = vec![PathBuf::from(std::env::var_os("CCID_TEST_GO").unwrap())
        .parent()
        .unwrap()
        .to_owned()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(paths).unwrap();
    let mut artifact = Vec::new();
    for name in ["go-cold", "go-warm"] {
        let root = fixture(
            parent.path(),
            name,
            "kind='commands'\ncommands=[['go','build','-x','-o','result.a','.']]",
        );
        fs::write(
            root.join("go.mod"),
            "module example.invalid/native-cache-proof\n\ngo 1.22.0\n",
        )
        .unwrap();
        fs::write(
            root.join("main.go"),
            "package proof\nfunc Value() int { return 7 }\n",
        )
        .unwrap();
        let output = trial(&root, &shared, &path, true);
        if name == "go-warm" {
            assert!(
                !String::from_utf8_lossy(&output.stderr).contains("/compile "),
                "Warm Go invocation must restore its native package object"
            );
            assert_eq!(artifact, fs::read(root.join("result.a")).unwrap());
            fs::write(
                root.join("main.go"),
                "package proof\nfunc Value() int { return \"wrong\" }\n",
            )
            .unwrap();
            trial(&root, &shared, &path, false);
        } else {
            assert!(String::from_utf8_lossy(&output.stderr).contains("/compile "));
            artifact = fs::read(root.join("result.a")).unwrap();
        }
    }
}

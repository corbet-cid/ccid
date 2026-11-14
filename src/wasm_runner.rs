//! `ccid coverage wasm-runner`: the browser test runner for instrumented wasm tests.
//!
//! `wasm-bindgen-test-runner` in headless mode stops the browser as soon as the test summary is
//! printed, which is before the page uploads its coverage. This runner starts the upstream runner
//! in its interactive mode (`NO_HEADLESS=1`, a free port), drives headless Chrome through
//! chromedriver, waits for the summary, captures the profile through the repository's
//! `__owned_test_cov_dump` / `__owned_test_module_signature` exports, uploads it to the upstream
//! server (which writes the profraw for `cargo llvm-cov`) and only then exits.

use crate::Result;
use fantoccini::{wd::Capabilities, ClientBuilder};
use regex::Regex;
use serde_json::{json, Value};
use std::{
    ffi::{OsStr, OsString},
    net::TcpListener,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
    sync::mpsc,
    time::{sleep, timeout},
};

/// Time the browser has to finish the tests and upload the profile.
const DEADLINE: Duration = Duration::from_secs(120);

fn failure(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    message.into().into()
}

/// What the page output says about the test run.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    Running,
    Failed,
    Passed,
}

pub(crate) fn progress(text: &str) -> Progress {
    if text.contains("test result: FAILED") {
        return Progress::Failed;
    }
    let passed = Regex::new(r"test result: ok\. [1-9][0-9]* passed").expect("static pattern");
    if passed.is_match(text) {
        Progress::Passed
    } else {
        Progress::Running
    }
}

/// The page address in one line of the upstream runner's output.
pub(crate) fn address(line: &str) -> Option<String> {
    let found = Regex::new(r"http://127\.0\.0\.1:\d+").expect("static pattern");
    found.find(line).map(|m| m.as_str().to_owned())
}

fn forward<R: AsyncRead + Unpin + Send + 'static>(stream: R, lines: mpsc::UnboundedSender<String>) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stream).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// Start the upstream runner and return it with the address of its page.
async fn start_server(program: &OsStr, args: &[String]) -> Result<(Child, String)> {
    let mut runner = Command::new(program)
        .args(args)
        .env("NO_HEADLESS", "1")
        .env("WASM_BINDGEN_TEST_ADDRESS", "127.0.0.1:0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| failure(format!("cannot start wasm-bindgen-test-runner: {error}")))?;
    let (sender, mut lines) = mpsc::unbounded_channel();
    forward(runner.stdout.take().expect("piped"), sender.clone());
    forward(runner.stderr.take().expect("piped"), sender);
    let page = timeout(DEADLINE, async {
        while let Some(line) = lines.recv().await {
            println!("{line}");
            if let Some(found) = address(&line) {
                return Ok(found);
            }
        }
        Err(failure(
            "the upstream test server exited before browser startup",
        ))
    })
    .await
    .map_err(|_| failure("the upstream test server startup timed out"))??;
    // Keep draining the runner's output so it never blocks on a full pipe.
    tokio::spawn(async move {
        while let Some(line) = lines.recv().await {
            println!("{line}");
        }
    });
    Ok((runner, page))
}

async fn run(program: &OsStr, args: Vec<String>) -> Result<()> {
    let (_runner, page) = start_server(program, &args).await?;
    let port = free_port()?;
    let driver = std::env::var_os("CHROMEDRIVER").unwrap_or_else(|| OsString::from("chromedriver"));
    let _driver = Command::new(&driver)
        .arg(format!("--port={port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| failure(format!("cannot start chromedriver: {error}")))?;
    wait_for_port(port).await?;

    let mut chrome = json!({
        "args": ["--headless=new", "--no-sandbox", "--disable-dev-shm-usage"],
    });
    if let Some(binary) = std::env::var_os("CHROME_BIN") {
        chrome["binary"] = Value::String(binary.to_string_lossy().into_owned());
    }
    let mut capabilities = Capabilities::new();
    capabilities.insert("goog:chromeOptions".into(), chrome);
    let client = ClientBuilder::new(hyper_util::client::legacy::connect::HttpConnector::new())
        .capabilities(capabilities)
        .connect(&format!("http://127.0.0.1:{port}"))
        .await
        .map_err(|error| failure(format!("cannot open a browser session: {error}")))?;
    let outcome = drive(&client, &page).await;
    let _ = client.close().await;
    outcome
}

async fn wait_for_port(port: u16) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(20) {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err(failure("chromedriver did not start listening"))
}

const READ_OUTPUT: &str = "return (document.getElementById('output') || {}).textContent || '';";

const UPLOAD: &str = r"
const done = arguments[arguments.length - 1];
(async () => {
    // The module URL upstream's page loaded: do not initialize a second instance.
    const runtime = await import('./wasm-bindgen-test');
    const bytes = runtime.__owned_test_cov_dump();
    if (!bytes.byteLength) throw new Error('Empty actual profiling data');
    const result = await fetch('/__wasm_bindgen/coverage', {
        method: 'POST',
        headers: {'Module-Signature': runtime.__owned_test_module_signature().toString()},
        body: bytes
    });
    if (!result.ok) throw new Error('Upstream profile-file handler refused data');
    done({bytes: bytes.byteLength});
})().catch(error => done({error: String(error.stack || error)}));
";

async fn drive(client: &fantoccini::Client, page: &str) -> Result<()> {
    client
        .goto(page)
        .await
        .map_err(|error| failure(format!("cannot open {page}: {error}")))?;
    let started = Instant::now();
    let mut text = String::new();
    while started.elapsed() < DEADLINE {
        text = client
            .execute(READ_OUTPUT, vec![])
            .await
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        match progress(&text) {
            Progress::Failed => {
                println!("{text}");
                return Err(failure("the browser tests failed"));
            }
            Progress::Passed => {
                let captured = client
                    .execute_async(UPLOAD, vec![])
                    .await
                    .map_err(|error| failure(format!("profile capture failed: {error}")))?;
                if let Some(error) = captured.get("error").and_then(Value::as_str) {
                    return Err(failure(error.to_owned()));
                }
                let bytes = captured.get("bytes").and_then(Value::as_u64).unwrap_or(0);
                if bytes == 0 {
                    return Err(failure("profile capture returned no bytes"));
                }
                println!("{text}");
                println!("Saved actual profiling bytes: {bytes}");
                return Ok(());
            }
            Progress::Running => sleep(Duration::from_millis(100)).await,
        }
    }
    println!("{text}");
    Err(failure(
        "the browser tests did not complete and upload coverage within 120 seconds",
    ))
}

/// Run the upstream runner with `args` in a headless browser; the exit code is 0 only when the
/// tests passed and their profile was uploaded.
pub fn wasm_runner(args: Vec<String>) -> Result<u8> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    match runtime.block_on(run(OsStr::new("wasm-bindgen-test-runner"), args)) {
        Ok(()) => Ok(0),
        Err(error) => {
            eprintln!("ccid coverage wasm-runner: {error}");
            Ok(1)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_runner(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().expect("tempdir");
        let script = directory.path().join("runner");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("write");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        (directory, script)
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn progress_reads_the_summary() {
        assert_eq!(progress("running 1 test"), Progress::Running);
        assert_eq!(
            progress("test result: ok. 0 passed; 0 failed"),
            Progress::Running
        );
        assert_eq!(
            progress("test result: ok. 3 passed; 0 failed"),
            Progress::Passed
        );
        assert_eq!(
            progress("test result: FAILED. 1 passed; 1 failed"),
            Progress::Failed
        );
    }

    #[test]
    fn address_is_taken_from_the_server_banner() {
        assert_eq!(
            address("Interactive browsers tests are now available at http://127.0.0.1:38777"),
            Some("http://127.0.0.1:38777".to_owned())
        );
        assert_eq!(address("Note that interactive mode is enabled"), None);
    }

    #[test]
    fn the_server_is_started_interactively_on_a_free_port() {
        let (_directory, script) = fake_runner(
            "echo \"available at http://127.0.0.1:4321 $NO_HEADLESS $WASM_BINDGEN_TEST_ADDRESS $1\"; sleep 30",
        );
        let (_runner, page) =
            block_on(start_server(script.as_os_str(), &["x.wasm".to_owned()])).expect("started");
        assert_eq!(page, "http://127.0.0.1:4321");
    }

    #[test]
    fn a_runner_that_exits_before_the_address_is_a_failure() {
        let (_directory, script) = fake_runner("echo nothing");
        let error = block_on(start_server(script.as_os_str(), &[])).expect_err("failure");
        assert!(error.to_string().contains("exited before browser startup"));
    }

    #[test]
    fn a_missing_runner_is_a_failure() {
        let error =
            block_on(start_server(OsStr::new("/nonexistent/runner"), &[])).expect_err("failure");
        assert!(error.to_string().contains("cannot start"));
    }
}

//! Bounded command execution and owned process-tree cancellation.
use crate::{event, failure, validate_command, Environment, Result, INTERRUPTED};
#[cfg(unix)]
use process_wrap::std::{ChildWrapper, CommandWrap};
use serde_json::json;
#[cfg(unix)]
use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
};
use std::{
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

pub struct Runner {
    pub root: PathBuf,
    pub environment: Environment,
    pub(crate) deadline: Instant,
    pub(crate) nix_inventory: Option<(String, Vec<String>)>,
    events_to_stderr: bool,
}
#[cfg(unix)]
struct OwnedChild(Box<dyn ChildWrapper>);
#[cfg(unix)]
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}
impl Runner {
    pub fn new(root: PathBuf, environment: Environment, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| failure("Deadline is out of range"))?;
        Self::until(root, environment, deadline)
    }

    pub(crate) fn until(
        root: PathBuf,
        environment: Environment,
        deadline: Instant,
    ) -> Result<Self> {
        Ok(Self {
            root,
            environment,
            deadline,
            nix_inventory: None,
            events_to_stderr: false,
        })
    }
    /// Keep machine-readable stdout separate from process timing diagnostics.
    pub fn with_stderr_events(mut self) -> Self {
        self.events_to_stderr = true;
        self
    }

    pub(crate) fn command_event(&self, value: serde_json::Value) {
        if self.events_to_stderr {
            eprintln!("{value}");
        } else {
            event(value);
        }
    }
    pub fn run(&self, argv: &[String], capture: bool) -> Result<String> {
        validate_command(argv)?;
        if INTERRUPTED.load(Ordering::SeqCst) || Instant::now() >= self.deadline {
            return Err(failure("Check interrupted or total deadline exceeded"));
        }
        #[cfg(all(windows, feature = "windows-experimental"))]
        {
            crate::windows::run(self, argv, capture)
        }
        #[cfg(all(windows, not(feature = "windows-experimental")))]
        {
            Err(failure("Windows check execution requires an experimental native build; process-tree cancellation remains unverified"))
        }
        #[cfg(unix)]
        {
            self.run_unix(argv, capture)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(failure(
                "Owned process cancellation is unsupported on this platform",
            ))
        }
    }
    #[cfg(unix)]
    fn run_unix(&self, argv: &[String], capture: bool) -> Result<String> {
        let started = Instant::now();
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(&self.root)
            .env_clear()
            .envs(&self.environment);
        command.stdin(Stdio::null()).stderr(Stdio::inherit());
        command.stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
        let mut command = CommandWrap::from(command);
        #[cfg(unix)]
        command.wrap(process_wrap::std::ProcessGroup::leader());
        let mut child = OwnedChild(command.spawn()?);
        let reader = if capture {
            let stdout = child
                .0
                .stdout()
                .take()
                .ok_or_else(|| failure("Missing captured stdout"))?;
            let (send, receive) = mpsc::channel();
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = stdout
                    .take(16 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes);
                let _ = send.send(result);
            });
            Some(receive)
        } else {
            None
        };
        let status = loop {
            if INTERRUPTED.load(Ordering::SeqCst) || Instant::now() >= self.deadline {
                #[cfg(unix)]
                {
                    let _ = child.0.signal(15);
                    let grace = Instant::now() + Duration::from_secs(10);
                    while Instant::now() < grace {
                        if child.0.try_wait()?.is_some() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                }
                let _ = child.0.start_kill();
                let _ = child.0.wait();
                return Err(failure(
                    "Check interrupted or timed out; its owned process group/job was terminated",
                ));
            }
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            thread::sleep(Duration::from_millis(20));
        };
        // Do not let detached descendants retain the shared project cache or pipes.
        let _ = child.0.start_kill();
        self.command_event(
            json!({"event":"command", "executable":Path::new(&argv[0]).file_name().map(|s|s.to_string_lossy()), "seconds":started.elapsed().as_secs_f64(), "exit_code":status.code()}),
        );
        if !status.success() {
            return Err(failure(format!(
                "{} failed: {status}",
                Path::new(&argv[0])
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            )));
        }
        if let Some(reader) = reader {
            let bytes = reader.recv_timeout(Duration::from_secs(10)).map_err(|_| {
                failure("Captured output did not close after command termination")
            })??;
            if bytes.len() > 16 * 1024 * 1024 {
                return Err(failure("Captured command output exceeded 16 MiB"));
            }
            return Ok(String::from_utf8(bytes)?.trim().to_owned());
        }
        Ok(String::new())
    }
}

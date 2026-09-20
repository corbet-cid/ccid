//! Keep command cleanup alive when an enclosing executor kills only its child.
use std::{
    io::{self, Read},
    os::unix::process::CommandExt,
    process::{Command, ExitStatus, Stdio},
    sync::atomic::Ordering,
    thread,
    time::Duration,
};

pub fn execute() -> io::Result<ExitStatus> {
    let mut child = Command::new(std::env::current_exe()?)
        .args(std::env::args_os().skip(1))
        .arg("--parent-watch")
        .process_group(0)
        .stdin(Stdio::piped())
        .spawn()?;
    // Only this process retains the writer. The kernel closes it even on
    // SIGKILL; the isolated supervisor then performs its ordinary cleanup.
    let mut liveness = child.stdin.take();
    loop {
        if ccid::INTERRUPTED.load(Ordering::SeqCst) {
            drop(liveness.take());
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                drop(liveness.take());
                return Err(error);
            }
        }
    }
}

pub fn watch_parent() -> io::Result<()> {
    thread::Builder::new()
        .name("ccid-parent".into())
        .spawn(|| {
            let mut input = io::stdin().lock();
            let mut byte = [0];
            loop {
                match input.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            ccid::INTERRUPTED.store(true, Ordering::SeqCst);
        })?;
    Ok(())
}

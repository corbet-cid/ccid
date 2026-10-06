//! Explicit cancellation fixture. This never claims cleanup from an exit code.
use super::*;
fn identity(pid: u32) -> Result<Value> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let end = raw.rfind(')').ok_or("Invalid process stat")?;
    let fields: Vec<_> = raw[end + 1..].split_whitespace().collect();
    Ok(
        json!({"pid":pid,"parent":fields.get(1).ok_or("Missing parent")?.parse::<u32>()?,"group":fields.get(2).ok_or("Missing group")?.parse::<u32>()?,"start_ticks":fields.get(19).ok_or("Missing start ticks")?.parse::<u64>()?}),
    )
}
pub(super) fn run(child: bool) -> Result<()> {
    ctrlc::set_handler(|| {})?;
    if child {
        std::thread::sleep(std::time::Duration::from_secs(120));
        return Ok(());
    }
    let destination = PathBuf::from(std::env::var("CANCEL_PROBE_DIR")?);
    if !destination.is_absolute() {
        return Err("CANCEL_PROBE_DIR must be an explicit absolute new directory".into());
    }
    fs::create_dir(&destination)?;
    let mut child = Command::new(std::env::current_exe()?)
        .args(["crow-ci", "probe-cancellation", "--child"])
        .stdin(Stdio::null())
        .spawn()?;
    let result = (|| -> Result<()> {
        let own = identity(std::process::id())?;
        let supervisor = identity(number(&own, "parent") as u32)?;
        let outer = identity(number(&supervisor, "parent") as u32)?;
        let receipt = json!({"schema":1,"commit":std::env::var("CI_COMMIT_SHA")?,"pipeline":std::env::var("CI_PIPELINE_NUMBER")?,"processes":[outer,supervisor,own,identity(child.id())?],"scratch":std::env::var("TMPDIR")?,"target":std::env::var("CARGO_TARGET_DIR")?,"created":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs_f64()});
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination.join("processes.json"))?;
        writeln!(file, "{}", encode(&receipt)?)?;
        file.sync_all()?;
        let mut event = receipt;
        event["event"] = json!("cancellation-probe-ready");
        emit(&event)?;
        child.wait()?;
        Err("Cancellation probe was not cancelled; no cleanup proof".into())
    })();
    if child.try_wait()?.is_none() {
        child.kill()?;
        child.wait()?;
    }
    result
}

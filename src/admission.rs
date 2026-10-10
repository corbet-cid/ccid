//! A bounded worker admission check, not a scheduler or a swap-occupancy rule.
use super::*;
use std::{path::PathBuf, thread};

/// How often a job held back by resource pressure checks again.
const ADMISSION_POLL: Duration = Duration::from_secs(10);

/// Transient resource pressure, worth waiting for; configuration errors are not.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn refused(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(Refused(message.into()))
}

/// Reclaimable ZFS ARC in MiB: `size - c_min` from a `arcstats` kstat dump. The
/// kernel counts ARC as used, but the shrinker releases it under pressure down
/// to `c_min`. Missing, malformed or inverted values add nothing.
pub(crate) fn reclaimable_arc_mb(arcstats: &str) -> u64 {
    let field = |name: &str| {
        arcstats.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some(name))
                .then(|| fields.nth(1)?.parse::<u64>().ok())
                .flatten()
        })
    };
    match (field("size"), field("c_min")) {
        (Some(size), Some(min)) if min > 0 && size > min => (size - min) / (1024 * 1024),
        _ => 0,
    }
}

/// A `/proc/meminfo` field in MiB.
fn meminfo_mb(meminfo: &str, name: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some(name))
                .then(|| fields.next()?.parse::<u64>().ok())
                .flatten()
        })
        .map(|kib| kib / 1024)
}

const ARCSTATS: &str = "/proc/spl/kstat/zfs/arcstats";

fn available_mb(
    meminfo: &str,
    arcstats: &str,
    limit: Option<u64>,
    current: Option<u64>,
) -> Result<u64> {
    let available = meminfo_mb(meminfo, "MemAvailable:")
        .ok_or_else(|| failure("Cannot read MemAvailable for configured memory admission"))?
        + reclaimable_arc_mb(arcstats);
    Ok(match (limit, current) {
        (Some(limit), Some(current)) => {
            available.min(limit.saturating_sub(current) / (1024 * 1024))
        }
        _ => available,
    })
}

fn apply(env: &mut Environment, available: u64, reserve: u64, ceiling: Option<u64>) -> Result<()> {
    if available <= reserve {
        return Err(refused(format!(
            "Memory admission refused: {available} MiB available, {reserve} MiB reserve required"
        )));
    }
    let usable = available - reserve;
    let requested = value(env, "CI_MEMORY_MB")
        .map(|v| positive(&v, "CI_MEMORY_MB"))
        .transpose()?;
    let minimum = value(env, "CI_MEMORY_PER_JOB_MB")
        .map(|v| positive(&v, "CI_MEMORY_PER_JOB_MB"))
        .transpose()?;
    let bound = ceiling.map_or(usable, |ceiling| usable.min(ceiling));
    let allocation = requested.map_or(bound, |requested| requested.min(bound));
    // A job that cannot get one job's share waits; a request below one job is a
    // configuration error that the budget reports.
    if let Some(minimum) = minimum {
        if allocation < minimum && requested.is_none_or(|requested| requested >= minimum) {
            return Err(refused(format!(
                "Memory admission refused: {allocation} MiB allocatable ({usable} MiB free, {} MiB unreserved), {minimum} MiB per job required",
                ceiling.map_or_else(|| "n/a".to_owned(), |c| c.to_string())
            )));
        }
    }
    set(env, "CI_MEMORY_MB", allocation.to_string());
    event(
        json!({"event":"memory-admission", "available_mb":available, "reserve_mb":reserve, "unreserved_mb":ceiling, "allocation_mb":allocation}),
    );
    Ok(())
}

/// Memory promised to running jobs of one cgroup. Every admitted process holds
/// an exclusively locked `<pid>.job` file with its allocation in MiB for as long
/// as it lives; the kernel drops the lock when it dies, so a crashed job never
/// leaves a reservation behind. A short-lived `.gate` lock serialises the
/// check-and-reserve step so concurrent starts cannot all pass the same check.
const LEDGER_HELD_ENV: &str = "CCID_ADMISSION_HELD";
static HELD: std::sync::Mutex<Option<fs::File>> = std::sync::Mutex::new(None);

fn ledger_dir(env: &Environment) -> PathBuf {
    value(env, "CI_ADMISSION_DIR").map_or_else(
        || std::env::temp_dir().join("ccid-admission"),
        PathBuf::from,
    )
}

fn is_live(path: &Path) -> bool {
    // The holder keeps the file exclusively locked; a lock we can take is stale.
    fs::File::open(path).is_ok_and(|f| matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
}

/// Sum of the allocations whose holders are alive; stale entries are removed.
fn outstanding_mb(dir: &Path) -> io::Result<u64> {
    let mut total = 0;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "job") {
            continue;
        }
        if is_live(&path) {
            total += fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok())
                .unwrap_or(0);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(total)
}

/// An enclosing ccid process (or this one) already reserves memory for the job.
fn holds_reservation(env: &Environment, dir: &Path) -> bool {
    HELD.lock().is_ok_and(|held| held.is_some())
        || value(env, LEDGER_HELD_ENV).is_some_and(|pid| {
            pid.bytes().all(|b| b.is_ascii_digit()) && is_live(&dir.join(format!("{pid}.job")))
        })
}

/// Check and reserve in one step: the entry `<name>.job` stays locked while the
/// returned file lives. Without a usable ledger directory the check degrades to
/// the state-only rule (no entry) instead of failing the job.
fn reserve_in(
    env: &mut Environment,
    dir: &Path,
    name: &str,
    available: u64,
    reserve: u64,
    capacity: u64,
) -> Result<Option<fs::File>> {
    let Ok(gate) = fs::create_dir_all(dir).and_then(|()| {
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(".gate"))
    }) else {
        apply(env, available, reserve, None)?;
        return Ok(None);
    };
    gate.lock()?;
    let Ok(outstanding) = outstanding_mb(dir) else {
        apply(env, available, reserve, None)?;
        return Ok(None);
    };
    let unreserved = capacity.saturating_sub(reserve).saturating_sub(outstanding);
    apply(env, available, reserve, Some(unreserved))?;
    let path = dir.join(format!("{name}.job"));
    let allocation = value(env, "CI_MEMORY_MB").unwrap_or_default();
    let entry = fs::write(&path, allocation)
        .and_then(|()| fs::File::open(&path))
        .and_then(|file| file.lock().map(|()| file));
    drop(gate);
    match entry {
        Ok(file) => Ok(Some(file)),
        Err(error) => {
            event(json!({"event":"admission-ledger-unavailable","error":error.to_string()}));
            Ok(None)
        }
    }
}

fn admit_memory(env: &mut Environment, available: u64, reserve: u64, capacity: u64) -> Result<()> {
    let dir = ledger_dir(env);
    if holds_reservation(env, &dir) {
        return apply(env, available, reserve, None);
    }
    let id = std::process::id();
    if let Some(file) = reserve_in(env, &dir, &id.to_string(), available, reserve, capacity)? {
        if let Ok(mut held) = HELD.lock() {
            *held = Some(file);
        }
        set(env, LEDGER_HELD_ENV, id.to_string());
    }
    Ok(())
}

/// Admit a job, holding it for up to `CI_ADMISSION_WAIT_SECONDS` (default 0)
/// while I/O or memory pressure refuses it, so a busy host delays work
/// instead of failing it.
pub(crate) fn admit(env: &mut Environment) -> Result<()> {
    let wait = value(env, "CI_ADMISSION_WAIT_SECONDS")
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| failure("CI_ADMISSION_WAIT_SECONDS must be a non-negative integer"))
        })
        .transpose()?
        .unwrap_or(0);
    let deadline = Instant::now() + Duration::from_secs(wait);
    loop {
        match admit_once(env) {
            Err(error)
                if error.is::<Refused>()
                    && Instant::now() < deadline
                    && !INTERRUPTED.load(Ordering::SeqCst) =>
            {
                event(json!({"event":"admission-wait","reason":error.to_string()}));
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(ADMISSION_POLL.min(remaining));
            }
            outcome => return outcome,
        }
    }
}

fn admit_once(env: &mut Environment) -> Result<()> {
    if let Some(maximum) = value(env, "CI_MAX_IO_PSI_AVG10") {
        let maximum = pressure_limit(&maximum)?;
        let pressure = fs::read_to_string("/proc/pressure/io").map_err(|_| {
            failure("Configured I/O pressure admission requires readable Linux PSI")
        })?;
        let observed = io_pressure(&pressure, maximum)?;
        event(json!({"event":"io-admission","full_avg10":observed,"maximum":maximum}));
    }
    let Some(reserve) = value(env, "CI_MIN_AVAILABLE_MB") else {
        return Ok(());
    };
    let reserve = positive(&reserve, "CI_MIN_AVAILABLE_MB")?;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = reserve;
        return Err(failure(
            "Configured runtime memory admission currently requires Linux",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        let mount = Path::new("/sys/fs/cgroup");
        let group = fs::read_to_string("/proc/self/cgroup")?
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .map(str::to_owned);
        let candidate = group
            .as_deref()
            .map(|path| mount.join(path.trim_start_matches('/')));
        let group = candidate
            .filter(|path| path.join("memory.max").exists())
            .unwrap_or_else(|| mount.into());
        let limit = match fs::read_to_string(group.join("memory.max")) {
            Ok(text) if text.trim() == "max" => None,
            Ok(text) => Some(text.trim().parse::<u64>()?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let current = if limit.is_some() {
            Some(
                fs::read_to_string(group.join("memory.current"))?
                    .trim()
                    .parse::<u64>()?,
            )
        } else {
            None
        };
        let meminfo = fs::read_to_string("/proc/meminfo")?;
        let available = available_mb(
            &meminfo,
            &fs::read_to_string(ARCSTATS).unwrap_or_default(),
            limit,
            current,
        )?;
        let capacity = limit
            .map(|bytes| bytes / (1024 * 1024))
            .or_else(|| meminfo_mb(&meminfo, "MemTotal:"));
        match capacity {
            Some(capacity) => admit_memory(env, available, reserve, capacity),
            None => apply(env, available, reserve, None),
        }
    }
}

fn pressure_limit(text: &str) -> Result<f64> {
    let maximum: f64 = text.parse()?;
    if !maximum.is_finite() || maximum <= 0.0 || maximum > 100.0 {
        return Err(failure(
            "CI_MAX_IO_PSI_AVG10 must be greater than zero and at most 100",
        ));
    }
    Ok(maximum)
}

fn io_pressure(text: &str, maximum: f64) -> Result<f64> {
    let observed = text
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some("full"))
                .then(|| fields.find_map(|field| field.strip_prefix("avg10=")?.parse::<f64>().ok()))
                .flatten()
        })
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
        .ok_or_else(|| failure("Cannot establish full I/O pressure for configured admission"))?;
    if observed >= maximum {
        return Err(refused(format!("I/O admission refused: full avg10={observed}%, limit={maximum}%; retry when pressure subsides")));
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn io_pressure_is_explicit_bounded_and_fails_closed() {
        for limit in ["0", "-1", "101", "NaN", "inf", ""] {
            assert!(pressure_limit(limit).is_err());
        }
        assert_eq!(pressure_limit("2.5").unwrap(), 2.5);
        assert_eq!(
            io_pressure("some avg10=99.0\nfull avg10=2.4 avg60=1", 2.5).unwrap(),
            2.4
        );
        for text in [
            "full avg10=2.5",
            "full avg10=99",
            "some avg10=0",
            "full avg10=NaN",
            "full avg10=-1",
        ] {
            assert!(io_pressure(text, 2.5).is_err());
        }
        // Only real pressure waits; unreadable or invalid data fails at once.
        assert!(io_pressure("full avg10=99", 2.5)
            .unwrap_err()
            .is::<Refused>());
        assert!(!io_pressure("full avg10=NaN", 2.5)
            .unwrap_err()
            .is::<Refused>());
        assert!(!pressure_limit("0").unwrap_err().is::<Refused>());
    }
    #[test]
    fn cgroup_headroom_caps_host_memory_without_consulting_swap() {
        let mib = 1024 * 1024;
        assert_eq!(
            available_mb(
                "MemAvailable: 65536000 kB\nSwapFree: 0 kB",
                "",
                Some(16384 * mib),
                Some(12288 * mib)
            )
            .unwrap(),
            4096
        );
        assert_eq!(
            available_mb(
                "MemAvailable: 2097152 kB",
                "",
                Some(16384 * mib),
                Some(12288 * mib)
            )
            .unwrap(),
            2048
        );
    }
    const ARC: &str = "13 1 0x01 98 4704 1 2\nname type data\nc_min 4 1073741824\nc_max 4 68719476736\nsize 4 11811160064\n";
    #[test]
    fn reclaimable_arc_is_size_above_c_min_and_degrades_to_zero() {
        assert_eq!(reclaimable_arc_mb(ARC), 10240);
        assert_eq!(reclaimable_arc_mb(""), 0);
        assert_eq!(reclaimable_arc_mb("size 4 100\nc_min 4 200\n"), 0);
        assert_eq!(reclaimable_arc_mb("size 4 9999999999\n"), 0);
        assert_eq!(reclaimable_arc_mb("size 4 x\nc_min 4 1\n"), 0);
        assert_eq!(reclaimable_arc_mb("size 4 5\nc_min 4 0\n"), 0);
    }
    #[test]
    fn arc_adds_to_available_memory_and_cgroup_headroom_still_caps() {
        let mib = 1024 * 1024;
        // 481 MiB MemAvailable plus 10240 MiB reclaimable ARC.
        assert_eq!(
            available_mb("MemAvailable: 492544 kB\n", ARC, None, None).unwrap(),
            10721
        );
        assert_eq!(
            available_mb("MemAvailable: 492544 kB\n", "", None, None).unwrap(),
            481
        );
        assert_eq!(
            available_mb(
                "MemAvailable: 492544 kB\n",
                ARC,
                Some(16384 * mib),
                Some(12288 * mib)
            )
            .unwrap(),
            4096
        );
    }
    #[test]
    fn low_headroom_refuses_and_remaining_memory_bounds_jobs() {
        let mut env = Environment::from([
            ("CI_MEMORY_MB".into(), "16384".into()),
            ("CI_MEMORY_PER_JOB_MB".into(), "2048".into()),
            ("CI_JOBS".into(), "8".into()),
        ]);
        assert!(apply(&mut env, 8192, 8192, None)
            .unwrap_err()
            .is::<Refused>());
        apply(&mut env, 12288, 8192, None).unwrap();
        assert_eq!(budget(&env).unwrap().jobs, 2);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ccid-admission-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }
    fn job_env() -> Environment {
        Environment::from([
            ("CI_MEMORY_MB".into(), "8192".into()),
            ("CI_MEMORY_PER_JOB_MB".into(), "2048".into()),
        ])
    }
    #[test]
    fn concurrent_starts_reserve_instead_of_all_passing_the_same_check() {
        let dir = scratch("reserve");
        // 40 GiB cgroup, 8 GiB reserve, everything free: room for exactly four 8 GiB jobs.
        let mut held = Vec::new();
        for n in 0..4 {
            let mut env = job_env();
            let file = reserve_in(&mut env, &dir, &format!("job{n}"), 40960, 8192, 40960)
                .unwrap()
                .unwrap();
            assert_eq!(value(&env, "CI_MEMORY_MB").unwrap(), "8192");
            held.push(file);
        }
        assert_eq!(outstanding_mb(&dir).unwrap(), 4 * 8192);
        // The fifth waits (Refused), not fails and not a silent overcommit.
        let mut env = job_env();
        let error = reserve_in(&mut env, &dir, "job4", 40960, 8192, 40960).unwrap_err();
        assert!(error.is::<Refused>());
        // A job ending releases its share: the fifth is admitted.
        held.pop();
        let mut env = job_env();
        assert!(reserve_in(&mut env, &dir, "job4", 40960, 8192, 40960)
            .unwrap()
            .is_some());
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn remaining_unreserved_share_bounds_the_allocation_down_to_one_job() {
        let dir = scratch("share");
        let mut first = job_env();
        let _a = reserve_in(&mut first, &dir, "a", 40960, 8192, 40960)
            .unwrap()
            .unwrap();
        // 32768 usable, 8192 held: the next request of 30000 is cut to 24576.
        let mut env = job_env();
        env.insert("CI_MEMORY_MB".into(), "30000".into());
        let _b = reserve_in(&mut env, &dir, "b", 40960, 8192, 40960)
            .unwrap()
            .unwrap();
        assert_eq!(value(&env, "CI_MEMORY_MB").unwrap(), "24576");
        // 0 MiB left: below one job's 2048, so it waits.
        let mut env = job_env();
        assert!(reserve_in(&mut env, &dir, "c", 40960, 8192, 40960)
            .unwrap_err()
            .is::<Refused>());
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn dead_holders_leave_no_reservation() {
        let dir = scratch("stale");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("999999.job"), "30000").unwrap(); // no process holds its lock
        assert_eq!(outstanding_mb(&dir).unwrap(), 0);
        assert!(!dir.join("999999.job").exists());
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn enclosing_reservation_is_not_counted_twice() {
        let dir = scratch("nested");
        let mut parent = job_env();
        let _held = reserve_in(&mut parent, &dir, "4242", 40960, 8192, 40960)
            .unwrap()
            .unwrap();
        let mut child = job_env();
        child.insert(LEDGER_HELD_ENV.into(), "4242".into());
        assert!(holds_reservation(&child, &dir));
        child.insert(LEDGER_HELD_ENV.into(), "../4242".into());
        assert!(!holds_reservation(&child, &dir));
        child.insert(LEDGER_HELD_ENV.into(), "4243".into());
        assert!(!holds_reservation(&child, &dir));
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn unusable_ledger_degrades_to_the_state_check() {
        let blocked = scratch("blocked");
        fs::write(&blocked, "a file, not a directory").unwrap();
        let mut env = job_env();
        assert!(reserve_in(&mut env, &blocked, "x", 40960, 8192, 40960)
            .unwrap()
            .is_none());
        assert_eq!(value(&env, "CI_MEMORY_MB").unwrap(), "8192");
        let _ = fs::remove_file(&blocked);
    }
}

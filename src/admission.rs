//! A bounded worker admission check, not a scheduler or a swap-occupancy rule.
use super::*;

fn available_mb(meminfo: &str, limit: Option<u64>, current: Option<u64>) -> Result<u64> {
    let available = meminfo
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some("MemAvailable:"))
                .then(|| fields.next()?.parse::<u64>().ok())
                .flatten()
        })
        .ok_or_else(|| failure("Cannot read MemAvailable for configured memory admission"))?
        / 1024;
    Ok(match (limit, current) {
        (Some(limit), Some(current)) => {
            available.min(limit.saturating_sub(current) / (1024 * 1024))
        }
        _ => available,
    })
}

fn apply(env: &mut Environment, available: u64, reserve: u64) -> Result<()> {
    if available <= reserve {
        return Err(failure(format!(
            "Memory admission refused: {available} MiB available, {reserve} MiB reserve required"
        )));
    }
    let usable = available - reserve;
    let requested = value(env, "CI_MEMORY_MB")
        .map(|v| positive(&v, "CI_MEMORY_MB"))
        .transpose()?;
    let allocation = requested.map_or(usable, |requested| requested.min(usable));
    set(env, "CI_MEMORY_MB", allocation.to_string());
    event(
        json!({"event":"memory-admission", "available_mb":available, "reserve_mb":reserve, "allocation_mb":allocation}),
    );
    Ok(())
}

pub(crate) fn admit(env: &mut Environment) -> Result<()> {
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
        apply(
            env,
            available_mb(&fs::read_to_string("/proc/meminfo")?, limit, current)?,
            reserve,
        )
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
        return Err(failure(format!("I/O admission refused: full avg10={observed}%, limit={maximum}%; retry when pressure subsides")));
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
    }
    #[test]
    fn cgroup_headroom_caps_host_memory_without_consulting_swap() {
        let mib = 1024 * 1024;
        assert_eq!(
            available_mb(
                "MemAvailable: 65536000 kB\nSwapFree: 0 kB",
                Some(16384 * mib),
                Some(12288 * mib)
            )
            .unwrap(),
            4096
        );
        assert_eq!(
            available_mb(
                "MemAvailable: 2097152 kB",
                Some(16384 * mib),
                Some(12288 * mib)
            )
            .unwrap(),
            2048
        );
    }
    #[test]
    fn low_headroom_refuses_and_remaining_memory_bounds_jobs() {
        let mut env = Environment::from([
            ("CI_MEMORY_MB".into(), "16384".into()),
            ("CI_MEMORY_PER_JOB_MB".into(), "2048".into()),
            ("CI_JOBS".into(), "8".into()),
        ]);
        assert!(apply(&mut env, 8192, 8192).is_err());
        apply(&mut env, 12288, 8192).unwrap();
        assert_eq!(budget(&env).unwrap().jobs, 2);
    }
}

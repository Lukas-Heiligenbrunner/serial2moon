//! Host statistics surfaced as Klipper's `system_stats` object (`sysload`, `cputime`,
//! `memavail`), sampled from procfs the way Klipper's `statistics.py` does. Moonraker
//! clients read it for host load / memory (e.g. Home Assistant's moonraker integration
//! fails to set up its sensors without it).

use std::time::Duration;

use super::StateHandle;

/// Kernel USER_HZ: the unit of `utime`/`stime` in `/proc/<pid>/stat`. Fixed at 100 for
/// userspace on every mainstream Linux architecture.
const CLOCK_TICKS_PER_SEC: f64 = 100.0;

/// Spawn the sampler. Publishes ~1/s (Klipper's cadence). Fields whose procfs source is
/// unreadable (e.g. non-Linux dev hosts) keep their previous value, like Klipper.
pub fn spawn(state: StateHandle) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let sysload = read("/proc/loadavg").and_then(|s| parse_loadavg(&s));
            let cputime = read("/proc/self/stat").and_then(|s| parse_process_cputime(&s));
            let memavail = read("/proc/meminfo").and_then(|s| parse_mem_available(&s));
            state.update(move |s| {
                if let Some(v) = sysload {
                    s.sysload = v;
                }
                if let Some(v) = cputime {
                    s.cputime = v;
                }
                if let Some(v) = memavail {
                    s.memavail = v;
                }
            });
        }
    });
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// 1-minute load average: the first field of `/proc/loadavg`.
fn parse_loadavg(s: &str) -> Option<f64> {
    s.split_whitespace().next()?.parse().ok()
}

/// Process CPU time (user + system) in seconds from `/proc/self/stat` — the equivalent
/// of Python's `time.process_time()` that Klipper reports. The command name (field 2)
/// may contain spaces or parentheses, so fields are counted after its closing `)`.
fn parse_process_cputime(s: &str) -> Option<f64> {
    let rest = &s[s.rfind(')')? + 1..];
    // `rest` starts at field 3 (state); utime and stime are fields 14 and 15.
    let mut fields = rest.split_whitespace().skip(11);
    let utime: u64 = fields.next()?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some((utime + stime) as f64 / CLOCK_TICKS_PER_SEC)
}

/// `MemAvailable` from `/proc/meminfo`, in kB (the unit Klipper reports and Moonraker's
/// `system_info.cpu_info.total_memory` uses).
fn parse_mem_available(s: &str) -> Option<u64> {
    s.lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loadavg() {
        assert_eq!(parse_loadavg("0.52 0.58 0.59 1/389 12345\n"), Some(0.52));
        assert_eq!(parse_loadavg(""), None);
    }

    #[test]
    fn parses_cputime_with_awkward_command_name() {
        // comm "(serial 2) moon)" contains spaces and parentheses; utime=250, stime=75.
        let stat = "4242 (serial 2) moon) S 1 4242 4242 0 -1 4194560 1234 0 0 0 250 75 0 0 20 0 9 0 \
                    100 123456789 2000 18446744073709551615";
        assert_eq!(parse_process_cputime(stat), Some(3.25));
        assert_eq!(parse_process_cputime("4242 (truncated) S 1"), None);
    }

    #[test]
    fn parses_mem_available_in_kb() {
        let meminfo = "MemTotal:         927724 kB\nMemFree:          101000 kB\n\
                       MemAvailable:     512340 kB\nBuffers:           20000 kB\n";
        assert_eq!(parse_mem_available(meminfo), Some(512_340));
        assert_eq!(parse_mem_available("MemTotal: 927724 kB\n"), None);
    }

    #[test]
    fn reads_live_procfs_on_linux() {
        if !cfg!(target_os = "linux") {
            return;
        }
        assert!(
            read("/proc/loadavg")
                .and_then(|s| parse_loadavg(&s))
                .is_some()
        );
        assert!(
            read("/proc/self/stat")
                .and_then(|s| parse_process_cputime(&s))
                .is_some()
        );
        assert!(
            read("/proc/meminfo")
                .and_then(|s| parse_mem_available(&s))
                .is_some_and(|kb| kb > 0)
        );
    }
}

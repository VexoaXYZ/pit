//! Why a task was killed. "Exit code 137" is the usual sign that a CI job
//! ran out of memory, but the log rarely says so: the kernel's OOM killer
//! sends SIGKILL and the job just stops. Pit names the signal, checks whether
//! the kernel recorded an OOM kill while the task ran, and samples the peak
//! memory of the task's processes, so the result can say "out of memory, it
//! was seen using 6.8 GB" instead of "exited with 137".

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How often the task's memory is sampled.
const SAMPLE_EVERY: Duration = Duration::from_millis(250);

/// OOM kills the kernel has made since boot, counting those from cgroup
/// limits (Docker's `--memory`, systemd's `MemoryMax`). Linux only.
pub fn oom_kills() -> Option<u64> {
    let vmstat = std::fs::read_to_string("/proc/vmstat").ok()?;
    vmstat
        .lines()
        .find_map(|l| l.strip_prefix("oom_kill "))
        .and_then(|v| v.trim().parse().ok())
}

/// Resident memory of every process in the process group, in bytes.
#[cfg(target_os = "linux")]
fn group_rss(pgid: u32) -> u64 {
    // SAFETY: plain sysconf query, no memory involved.
    let page = match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        n if n > 0 => n as u64,
        _ => 4096,
    };
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let pgid = pgid.to_string();
    let mut total = 0;
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // The command name is in parentheses and may contain spaces; the
        // fields after it are state, ppid, pgrp, ...
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        if rest.split_whitespace().nth(2) != Some(pgid.as_str()) {
            continue;
        }
        if let Ok(statm) = std::fs::read_to_string(format!("/proc/{pid}/statm")) {
            if let Some(pages) = statm
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
            {
                total += pages * page;
            }
        }
    }
    total
}

#[cfg(not(target_os = "linux"))]
fn group_rss(_: u32) -> u64 {
    0
}

/// Samples the process group's memory until aborted; the counter holds the
/// highest total seen.
pub fn sample(pgid: Option<u32>) -> (Arc<AtomicU64>, tokio::task::JoinHandle<()>) {
    let peak = Arc::new(AtomicU64::new(0));
    let p = peak.clone();
    let handle = tokio::spawn(async move {
        let Some(pgid) = pgid.filter(|_| cfg!(target_os = "linux")) else {
            return;
        };
        loop {
            let rss = tokio::task::spawn_blocking(move || group_rss(pgid))
                .await
                .unwrap_or(0);
            p.fetch_max(rss, Ordering::Relaxed);
            tokio::time::sleep(SAMPLE_EVERY).await;
        }
    });
    (peak, handle)
}

pub fn signal_name(n: i32) -> Option<&'static str> {
    Some(match n {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => return None,
    })
}

pub fn human_bytes(b: u64) -> String {
    let gb = b as f64 / (1u64 << 30) as f64;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{} MB", b >> 20)
    }
}

/// Explains a task that ended by a signal: directly (`signal`), or as a
/// shell or the docker client reports one (exit code 128 + n). `oom` is
/// whether the kernel's OOM killer ran meanwhile, `None` where that cannot
/// be known (no `/proc/vmstat`). A task that failed otherwise while the OOM
/// killer ran (`make` or `cargo` exiting 2 after a child was killed) gets a
/// note too.
pub fn explain(
    exit_code: Option<i32>,
    signal: Option<i32>,
    oom: Option<bool>,
    peak: u64,
) -> Option<String> {
    let peak_note = if peak > 0 {
        format!(
            "; its processes were seen using up to {}",
            human_bytes(peak)
        )
    } else {
        String::new()
    };
    let signalled = match (signal, exit_code) {
        (Some(n), _) => Some((
            n,
            format!("killed by {}", signal_name(n).unwrap_or("a signal")),
        )),
        (None, Some(c)) if (129..=159).contains(&c) => {
            signal_name(c - 128).map(|name| (c - 128, format!("exit {c}: killed by {name}")))
        }
        _ => None,
    };
    let Some((n, how)) = signalled else {
        // `make` or `cargo` exiting 2 after a child was killed for memory.
        return match (exit_code, oom) {
            (Some(c), Some(true)) if c != 0 => Some(format!(
                "exited with {c}, and the kernel's OOM killer ran while it did: one of its \
                 processes may have been killed for memory{peak_note}"
            )),
            _ => None,
        };
    };
    let peak = peak_note;
    Some(match (n, oom) {
        (9, Some(true)) => format!(
            "out of memory: {how}, and the kernel's OOM killer ran while it did{peak}. \
             Give it more memory or less parallelism (`memory` sets a limit for docker tasks)"
        ),
        (9, Some(false)) => {
            format!("{how}; the kernel recorded no OOM kill, so something else sent it{peak}")
        }
        (9, None) => format!("{how} (out of memory, or something else sent it){peak}"),
        _ => format!("{how}{peak}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_kill() {
        let oom = explain(Some(137), None, Some(true), 3 << 30).unwrap();
        assert!(
            oom.starts_with("out of memory: exit 137: killed by SIGKILL"),
            "{oom}"
        );
        assert!(oom.contains("seen using up to 3.0 GB"), "{oom}");
        assert_eq!(
            explain(None, Some(9), Some(false), 0).unwrap(),
            "killed by SIGKILL; the kernel recorded no OOM kill, so something else sent it"
        );
        assert_eq!(
            explain(Some(139), None, Some(false), 200 << 20).unwrap(),
            "exit 139: killed by SIGSEGV; its processes were seen using up to 200 MB"
        );
        // Where the kernel's count cannot be read, it does not guess.
        assert_eq!(
            explain(Some(137), None, None, 0).unwrap(),
            "exit 137: killed by SIGKILL (out of memory, or something else sent it)"
        );
        // A child killed for memory under a parent that exits 2.
        assert!(explain(Some(2), None, Some(true), 0)
            .unwrap()
            .contains("may have been killed for memory"));
        assert_eq!(explain(Some(1), None, Some(false), 0), None);
        assert_eq!(explain(Some(200), None, Some(false), 0), None);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sees_a_process_groups_memory_and_the_kernels_oom_count() {
        assert!(oom_kills().is_some());
        let mut child = tokio::process::Command::new("python3")
            .args([
                "-c",
                "b = bytearray(64 * 1024 * 1024); import time; time.sleep(1.5)",
            ])
            .process_group(0)
            .spawn()
            .unwrap();
        let (peak, h) = sample(child.id());
        let _ = child.wait().await;
        h.abort();
        let seen = peak.load(Ordering::Relaxed);
        assert!(seen >= 64 << 20, "{seen}");
    }
}

/// Runtime metrics for bug reports.
///
/// Captures process-level and per-thread CPU usage, memory
/// footprint, and uptime over a short sample window.  On Linux,
/// these are read from `/proc/self/stat`, `/proc/self/status`, and
/// `/proc/self/task/<tid>/stat`.  On other platforms the struct
/// records that metrics are unavailable so the bug-report ZIP still
/// contains the file with a clear explanation.
use std::time::Duration;

use serde::Serialize;

// ── Public types ───────────────────────────────────────────

/// Per-thread CPU metrics.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadMetrics {
    /// Kernel thread ID.
    pub tid: u64,
    /// Thread name from `/proc/self/task/<tid>/comm`.
    pub name: String,
    /// CPU percentage over the sample window.
    pub cpu_percent: f64,
}

/// Process-level CPU and memory metrics.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessMetrics {
    /// Total process CPU% over the sample window (summed across
    /// all threads; may exceed 100 on multi-core).
    pub cpu_percent: f64,
    /// Resident set size in kB (from VmRSS in /proc/self/status).
    pub rss_kb: u64,
    /// Virtual memory size in kB (VmSize).
    pub vm_size_kb: u64,
    /// Process uptime in seconds (derived from /proc/self/stat
    /// starttime and /proc/uptime).
    pub uptime_secs: f64,
}

/// Full runtime metrics snapshot.
///
/// Uses `#[serde(untagged)]` so the JSON output is either:
/// ```json
/// { "sample_window_ms": 2000, "process": {...}, "threads": [...],
///   "platform": "linux" }
/// ```
/// or:
/// ```json
/// { "platform": "macos", "available": false, "reason": "..." }
/// ```
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum RuntimeMetrics {
    /// Full Linux metrics collected from /proc.
    Linux {
        sample_window_ms: u64,
        process: ProcessMetrics,
        threads: Vec<ThreadMetrics>,
        platform: String,
    },
    /// Metrics unavailable on this platform.
    Unavailable {
        platform: String,
        available: bool,
        reason: String,
    },
}

impl RuntimeMetrics {
    /// Construct an Unavailable variant with the current platform
    /// and the supplied reason string.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        RuntimeMetrics::Unavailable {
            platform: std::env::consts::OS.to_string(),
            available: false,
            reason: reason.into(),
        }
    }
}

// ── Linux implementation ───────────────────────────────────

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::time::Duration;
    use tracing::warn;

    use super::{ProcessMetrics, RuntimeMetrics, ThreadMetrics};

    /// Raw tick counts read from a single /proc stat line.
    #[derive(Debug, Clone)]
    pub(super) struct ProcStat {
        /// Thread name from the parenthesised comm field.
        pub(super) comm: String,
        /// User-mode ticks.
        pub(super) utime: u64,
        /// Kernel-mode ticks.
        pub(super) stime: u64,
        /// Process start time in ticks since boot.
        pub(super) starttime: u64,
    }

    /// Extract comm, utime, stime, and starttime from a
    /// `/proc/<pid>/stat` or `/proc/<pid>/task/<tid>/stat` line.
    ///
    /// The comm field is wrapped in parentheses and may itself
    /// contain spaces and right-parentheses (e.g. process names
    /// like `(Web Content)")`).  We locate the last `)` in the
    /// line and parse fields relative to that position.
    pub(super) fn parse_proc_stat(s: &str) -> Option<ProcStat> {
        // Find the first '(' and the last ')' to extract comm.
        let open = s.find('(')?;
        let close = s.rfind(')')?;
        if close <= open {
            return None;
        }
        let comm = s[open + 1..close].to_string();

        // Fields after the closing ')' are space-separated.
        // Field numbering follows `man 5 proc`:
        //   (1) pid, (2) comm, (3) state, (4) ppid, (5) pgrp,
        //   (6) session, (7) tty_nr, (8) tpgid, (9) flags,
        //   (10) minflt, (11) cminflt, (12) majflt, (13) cmajflt,
        //   (14) utime, (15) stime, (16) cutime, (17) cstime,
        //   ...
        //   (22) starttime
        let after = s[close + 1..].trim_start();
        let fields: Vec<&str> = after.split_whitespace().collect();
        // After ')': state(0), ppid(1), pgrp(2), session(3),
        //   tty_nr(4), tpgid(5), flags(6), minflt(7), cminflt(8),
        //   majflt(9), cmajflt(10), utime(11), stime(12),
        //   cutime(13), cstime(14), priority(15), nice(16),
        //   num_threads(17), itrealvalue(18), starttime(19)
        if fields.len() < 20 {
            return None;
        }
        let utime = fields[11].parse().ok()?;
        let stime = fields[12].parse().ok()?;
        let starttime = fields[19].parse().ok()?;

        Some(ProcStat {
            comm,
            utime,
            stime,
            starttime,
        })
    }

    /// Extract a "Key:   12345 kB" value from /proc/self/status.
    pub(super) fn parse_proc_status_kb(s: &str, key: &str) -> Option<u64> {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix(key) {
                // e.g. "VmRSS:    12345 kB"
                // Strip trailing " kB" if present; take the first token.
                let num_str = rest.split_whitespace().next()?;
                return num_str.parse().ok();
            }
        }
        None
    }

    /// Read a text file from /proc, returning None (with a warning)
    /// on error.
    fn read_proc(path: &str) -> Option<String> {
        match fs::read_to_string(path) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("metrics: failed to read {}: {}", path, e);
                None
            }
        }
    }

    /// Uptime in seconds from /proc/uptime.
    fn proc_uptime_secs() -> Option<f64> {
        let s = read_proc("/proc/uptime")?;
        s.split_whitespace().next()?.parse().ok()
    }

    /// Read the clock-ticks-per-second from sysconf.
    fn clk_tck() -> f64 {
        // SAFETY: sysconf is async-signal-safe and has no unsafe
        // preconditions beyond a valid constant argument.
        let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if v <= 0 {
            100.0
        } else {
            v as f64
        }
    }

    /// Snapshot tick counts for the process and all its threads.
    struct Snapshot {
        proc_stat: ProcStat,
        threads: Vec<(u64, String, u64, u64)>, // (tid, name, utime, stime)
    }

    fn take_snapshot() -> Option<Snapshot> {
        let proc_stat_txt = read_proc("/proc/self/stat")?;
        let proc_stat = parse_proc_stat(&proc_stat_txt)?;

        let task_dir = "/proc/self/task";
        let entries = match fs::read_dir(task_dir) {
            Ok(e) => e,
            Err(err) => {
                warn!("metrics: cannot read {}: {}", task_dir, err);
                return None;
            }
        };

        let mut threads = Vec::new();
        for entry in entries.flatten() {
            let tid_str = entry.file_name();
            let tid: u64 = match tid_str.to_string_lossy().parse() {
                Ok(v) => v,
                Err(_) => continue,
            };

            let stat_path = format!("/proc/self/task/{}/stat", tid);
            let comm_path = format!("/proc/self/task/{}/comm", tid);

            let stat_txt = match read_proc(&stat_path) {
                Some(s) => s,
                None => continue,
            };
            let thread_stat = match parse_proc_stat(&stat_txt) {
                Some(s) => s,
                None => continue,
            };

            // Prefer comm from the dedicated comm file (cleaner, no
            // parentheses, truncated at 15 chars by the kernel).
            let name = match fs::read_to_string(&comm_path) {
                Ok(s) => s.trim().to_string(),
                Err(_) => thread_stat.comm.clone(),
            };

            threads.push((tid, name, thread_stat.utime, thread_stat.stime));
        }

        Some(Snapshot { proc_stat, threads })
    }

    /// Sample process and per-thread metrics over `window`.
    pub(super) fn sample(window: Duration) -> RuntimeMetrics {
        let tck = clk_tck();

        let snap_a = match take_snapshot() {
            Some(s) => s,
            None => {
                return RuntimeMetrics::unavailable(
                    "failed to read /proc/self/stat or /proc/self/task",
                );
            }
        };

        std::thread::sleep(window);

        let snap_b = match take_snapshot() {
            Some(s) => s,
            None => {
                return RuntimeMetrics::unavailable(
                    "failed to read /proc metrics in second sample",
                );
            }
        };

        let window_secs = window.as_secs_f64();

        // Process-level CPU%.
        let proc_utime_delta = snap_b
            .proc_stat
            .utime
            .saturating_sub(snap_a.proc_stat.utime);
        let proc_stime_delta = snap_b
            .proc_stat
            .stime
            .saturating_sub(snap_a.proc_stat.stime);
        let proc_cpu = (proc_utime_delta + proc_stime_delta) as f64 / tck / window_secs * 100.0;

        // Memory from /proc/self/status (use second snapshot for
        // current values).
        let status_txt = read_proc("/proc/self/status").unwrap_or_default();
        let rss_kb = parse_proc_status_kb(&status_txt, "VmRSS:").unwrap_or(0);
        let vm_size_kb = parse_proc_status_kb(&status_txt, "VmSize:").unwrap_or(0);

        // Process uptime: /proc/uptime gives seconds since boot;
        // starttime is ticks since boot, so
        // uptime = sys_uptime - starttime/clk_tck.
        let uptime_secs = if let Some(sys_up) = proc_uptime_secs() {
            let start_secs = snap_b.proc_stat.starttime as f64 / tck;
            (sys_up - start_secs).max(0.0)
        } else {
            0.0
        };

        // Per-thread CPU%.
        // Build a lookup from snap_a tid → (utime, stime).
        let mut a_map: std::collections::HashMap<u64, (u64, u64)> =
            std::collections::HashMap::new();
        for (tid, _, ut, st) in &snap_a.threads {
            a_map.insert(*tid, (*ut, *st));
        }

        let mut threads = Vec::new();
        for (tid, name, ut_b, st_b) in snap_b.threads {
            let (ut_a, st_a) = a_map.get(&tid).copied().unwrap_or((ut_b, st_b));
            let delta = ut_b.saturating_sub(ut_a) + st_b.saturating_sub(st_a);
            let cpu_percent = delta as f64 / tck / window_secs * 100.0;
            threads.push(ThreadMetrics {
                tid,
                name,
                cpu_percent,
            });
        }
        // Sort by descending CPU% for readability.
        threads.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        RuntimeMetrics::Linux {
            sample_window_ms: window.as_millis() as u64,
            process: ProcessMetrics {
                cpu_percent: proc_cpu,
                rss_kb,
                vm_size_kb,
                uptime_secs,
            },
            threads,
            platform: "linux".to_string(),
        }
    }
}

// ── Public entry point ─────────────────────────────────────

/// Sample runtime metrics over the given window duration.
///
/// On Linux, this reads `/proc/self/stat`, `/proc/self/status`, and
/// `/proc/self/task/<tid>/stat`, sleeps for `window`, then re-reads
/// and computes CPU deltas.
///
/// On non-Linux platforms, returns `RuntimeMetrics::Unavailable`
/// immediately without sleeping.
pub fn sample(window: Duration) -> RuntimeMetrics {
    #[cfg(target_os = "linux")]
    {
        linux::sample(window)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = window; // not used on non-Linux
        RuntimeMetrics::unavailable(format!(
            "per-thread metrics not implemented on {}",
            std::env::consts::OS
        ))
    }
}

// ── Unit tests ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_proc_stat ────────────────────────────────────

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_proc_stat_simple() {
        // Minimal synthetic /proc/self/stat line.
        // Field layout (after pid + comm + state):
        //   ppid pgrp session tty_nr tpgid flags
        //   minflt cminflt majflt cmajflt
        //   utime stime cutime cstime priority nice num_threads itrealvalue starttime
        let line = concat!(
            "12345 (ryll) S 1 2 3 4 5 6 7 8 9 10 ",
            "111 222 0 0 0 0 1 0 999 ",
            "0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"
        );
        let stat = linux::parse_proc_stat(line).expect("should parse");
        assert_eq!(stat.comm, "ryll");
        assert_eq!(stat.utime, 111);
        assert_eq!(stat.stime, 222);
        assert_eq!(stat.starttime, 999);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_proc_stat_comm_with_space_and_paren() {
        // comm contains a space and a right-paren; rfind(')') must
        // be used, not find(')').
        let line = concat!(
            "99 (my proc) name) R 1 2 3 4 5 6 7 8 9 10 ",
            "42 17 0 0 0 0 1 0 5432 ",
            "0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"
        );
        let stat = linux::parse_proc_stat(line).expect("should parse");
        assert_eq!(stat.comm, "my proc) name");
        assert_eq!(stat.utime, 42);
        assert_eq!(stat.stime, 17);
        assert_eq!(stat.starttime, 5432);
    }

    // ── parse_proc_status_kb ───────────────────────────────

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_proc_status_kb_vmrss() {
        let status = "\
Name:\tryll\n\
VmPeak:\t  204800 kB\n\
VmSize:\t  196608 kB\n\
VmRSS:\t   98304 kB\n\
VmData:\t   65536 kB\n\
";
        assert_eq!(linux::parse_proc_status_kb(status, "VmRSS:"), Some(98304));
        assert_eq!(linux::parse_proc_status_kb(status, "VmSize:"), Some(196608));
        assert_eq!(linux::parse_proc_status_kb(status, "VmPeak:"), Some(204800));
        // Missing key returns None.
        assert_eq!(linux::parse_proc_status_kb(status, "VmSwap:"), None);
    }

    // ── RuntimeMetrics serialisation ───────────────────────

    #[test]
    fn test_unavailable_serialises() {
        let m = RuntimeMetrics::unavailable("no /proc on this platform");
        let json = serde_json::to_string(&m).unwrap();
        // Must contain platform and available=false and reason.
        assert!(json.contains("\"available\":false"));
        assert!(json.contains("\"reason\":\"no /proc on this platform\""));
        // Must NOT contain a spurious enum tag field.
        assert!(!json.contains("\"Unavailable\""));
    }

    #[test]
    fn test_linux_variant_serialises() {
        let m = RuntimeMetrics::Linux {
            sample_window_ms: 2000,
            process: ProcessMetrics {
                cpu_percent: 12.5,
                rss_kb: 65536,
                vm_size_kb: 131072,
                uptime_secs: 47.2,
            },
            threads: vec![ThreadMetrics {
                tid: 1234,
                name: "ryll".to_string(),
                cpu_percent: 5.0,
            }],
            platform: "linux".to_string(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"sample_window_ms\":2000"));
        assert!(json.contains("\"cpu_percent\":12.5"));
        assert!(json.contains("\"rss_kb\":65536"));
        assert!(json.contains("\"tid\":1234"));
        assert!(json.contains("\"platform\":\"linux\""));
        // Untagged: no enum variant name in the JSON.
        assert!(!json.contains("\"Linux\""));
    }
}

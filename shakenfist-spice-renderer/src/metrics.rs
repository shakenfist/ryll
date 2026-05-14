/// Runtime metrics for bug reports.
///
/// Captures process-level and per-thread CPU usage, memory
/// footprint, and uptime over a short sample window.
///
/// On **Linux**, these are read from `/proc/self/stat`,
/// `/proc/self/status`, and `/proc/self/task/<tid>/stat`.
///
/// On **macOS** (phases 1–3 of `PLAN-macos-runtime-metrics`):
/// process-level metrics via a single
/// `task_info(MACH_TASK_BASIC_INFO)` syscall per snapshot
/// (phase 1) plus per-thread enumeration via `task_threads` +
/// two `thread_info` calls per port (THREAD_BASIC_INFO and
/// THREAD_IDENTIFIER_INFO) plus `pthread_getname_np` for the
/// name (phase 2). The Mach port array from `task_threads` is
/// wrapped in a `MachThreadList` RAII guard so each port
/// reference and the array memory are released on every exit
/// path, including panic. Uptime is baselined by
/// `init_at_startup()`, which the caller invokes at the top
/// of `main()` (phase 3); `uptime_secs` then measures from
/// process start.
///
/// On other platforms the struct records that metrics are
/// unavailable so the bug-report ZIP still contains the file
/// with a clear explanation.
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
/// or, on macOS (phase 1 of PLAN-macos-runtime-metrics fills
/// `process`; `threads` is populated in phase 2):
/// ```json
/// { "sample_window_ms": 2000, "process": {...}, "threads": [],
///   "platform": "macos" }
/// ```
/// or, on platforms without an implementation:
/// ```json
/// { "platform": "freebsd", "available": false, "reason": "..." }
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
    /// macOS metrics collected via Mach `task_info`. Identical
    /// JSON shape to `Linux` — `#[serde(untagged)]` means a
    /// consumer that already handles `Linux` accepts `MacOS`
    /// unchanged. The `platform` field tells them apart.
    MacOS {
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

// ── macOS implementation ───────────────────────────────────

/// macOS metrics implementation per `PLAN-macos-runtime-
/// metrics`: phase 1 added process-level metrics via
/// `task_info(MACH_TASK_BASIC_INFO)`; phase 2 added per-
/// thread enumeration via `task_threads` + per-port
/// `thread_info` (THREAD_BASIC_INFO + THREAD_IDENTIFIER_INFO)
/// + `pthread_getname_np`, with the `MachThreadList` RAII
/// guard handling the Mach port lifecycle.
///
/// **Uptime baseline:** `PROCESS_START` is a
/// `LazyLock<Instant>` forced by the module-level
/// `init_at_startup()` function, which callers invoke at the
/// top of `main()`. As long as that ordering holds,
/// `uptime_secs` measures from process start. If a
/// `sample()` runs before `init_at_startup()`, the baseline
/// is the first-sample moment instead — see phase-3's plan
/// for the ordering-requirement risk note.
#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashMap;
    use std::ffi::CStr;
    use std::sync::LazyLock;
    use std::time::{Duration, Instant};

    use super::{ProcessMetrics, RuntimeMetrics, ThreadMetrics};

    // libc 0.2 does not expose mach_port_deallocate on Apple
    // targets (verified against 0.2.186). The signature is
    // documented and stable Mach ABI; a local extern "C" is
    // simpler than pulling in the `mach2` crate for one call.
    // See PLAN-macos-runtime-metrics-phase-02-threads.md.
    extern "C" {
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    static PROCESS_START: LazyLock<Instant> = LazyLock::new(Instant::now);

    /// Force the `PROCESS_START` LazyLock so subsequent
    /// `uptime_secs` reports measure from this call site
    /// rather than the first `sample()` call. Called from
    /// the module-level `init_at_startup`; see its doc-
    /// comment for the ordering requirement.
    pub(super) fn force_process_start() {
        // Dereferencing the LazyLock initialises it; the
        // result is discarded — the side effect is what we
        // want.
        let _ = *PROCESS_START;
    }

    /// Per-thread CPU + identity captured at a single moment.
    /// `compute_thread_metrics` matches `ThreadSnapshot`
    /// instances between two snapshots by `thread_id` to
    /// compute deltas.
    #[derive(Debug, Clone)]
    pub(super) struct ThreadSnapshot {
        /// 64-bit kernel-assigned thread id from
        /// `thread_identifier_info.thread_id`. Stable for
        /// the thread's lifetime; the linchpin for matching
        /// threads across snapshots since the Mach
        /// thread_act_t port numbers themselves are not
        /// stable across `task_threads` calls.
        pub thread_id: u64,
        pub user_time_us: u64,
        pub system_time_us: u64,
        /// Name from `pthread_getname_np`; empty string if
        /// the thread is unnamed or the lookup fails.
        pub name: String,
    }

    /// Raw `task_basic_info` + per-thread data captured at a
    /// single moment. Constructed by `take_snapshot()` and
    /// consumed by `process_cpu_percent` /
    /// `compute_thread_metrics` so the delta math is
    /// unit-testable without the Mach syscalls.
    #[derive(Debug, Clone)]
    pub(super) struct Snapshot {
        /// Total user CPU time across all threads, microseconds.
        pub user_time_us: u64,
        /// Total system CPU time across all threads, microseconds.
        pub system_time_us: u64,
        /// Resident set size in bytes
        /// (`task_basic_info.resident_size`).
        pub resident_size: u64,
        /// Virtual memory size in bytes
        /// (`task_basic_info.virtual_size`).
        pub virtual_size: u64,
        /// Per-thread snapshot collected via `task_threads` +
        /// per-port `thread_info`. Empty when thread
        /// enumeration fails; the process-level fields are
        /// still populated.
        pub threads: Vec<ThreadSnapshot>,
    }

    /// RAII wrapper around the Mach-allocated thread-port array
    /// returned by `task_threads`. Releases every port via
    /// `mach_port_deallocate` and the array memory via
    /// `vm_deallocate` on Drop, including the panic path. The
    /// only constructor is the struct literal below; wrapping
    /// is enforced by `task_threads` writing directly into the
    /// fields. See PLAN-macos-runtime-metrics-phase-02.
    struct MachThreadList {
        ports: *mut libc::thread_act_t,
        count: libc::mach_msg_type_number_t,
    }

    impl MachThreadList {
        fn as_slice(&self) -> &[libc::thread_act_t] {
            if self.ports.is_null() || self.count == 0 {
                return &[];
            }
            // SAFETY: `task_threads` returned a valid array of
            // `count` thread_act_t entries; we never modify it.
            unsafe { std::slice::from_raw_parts(self.ports, self.count as usize) }
        }
    }

    impl Drop for MachThreadList {
        fn drop(&mut self) {
            if self.ports.is_null() || self.count == 0 {
                return;
            }
            for &port in self.as_slice() {
                // SAFETY: each port is a send-right returned
                // by task_threads; deallocating against
                // mach_task_self() is the documented inverse.
                // Return value ignored: nothing meaningful to
                // do on a cleanup failure from Drop.
                unsafe {
                    let _ = mach_port_deallocate(libc::mach_task_self(), port);
                }
            }
            let bytes = (self.count as usize) * std::mem::size_of::<libc::thread_act_t>();
            // SAFETY: task_threads allocated `bytes` worth of
            // thread_act_t entries via vm_allocate against
            // mach_task_self(); vm_deallocate is the
            // documented inverse. Return value ignored: see
            // above.
            unsafe {
                let _ = libc::vm_deallocate(
                    libc::mach_task_self(),
                    self.ports as libc::vm_address_t,
                    bytes as libc::vm_size_t,
                );
            }
        }
    }

    /// Convert a Mach `time_value_t` (seconds + microseconds)
    /// into a single u64 microsecond count. Saturating because
    /// session-uptime-in-microseconds fits in u64 for half a
    /// million years; the saturating form removes any panic
    /// surface from arithmetic on kernel-controlled values.
    pub(super) fn time_value_to_us(t: libc::time_value_t) -> u64 {
        (t.seconds as u64)
            .saturating_mul(1_000_000)
            .saturating_add(t.microseconds as u64)
    }

    /// Compute total process CPU% from two snapshots taken
    /// `window` apart. Saturating subtraction handles the
    /// theoretical case where the second snapshot's
    /// accumulated CPU is less than the first (clock reset /
    /// thread accounting quirk); `window.as_micros().max(1)`
    /// guards against the zero-window edge case so the result
    /// is always finite.
    pub(super) fn process_cpu_percent(a: &Snapshot, b: &Snapshot, window: Duration) -> f64 {
        let user_delta = b.user_time_us.saturating_sub(a.user_time_us);
        let sys_delta = b.system_time_us.saturating_sub(a.system_time_us);
        let total_us = user_delta.saturating_add(sys_delta);
        let window_us = window.as_micros().max(1) as u64;
        (total_us as f64 / window_us as f64) * 100.0
    }

    /// Compute per-thread CPU% by matching thread_ids across
    /// the two snapshots. Threads present in B but not A are
    /// new arrivals — report 0% (Linux parity); threads in A
    /// but not B are dropped (died mid-window). Output is
    /// sorted by tid for deterministic JSON.
    pub(super) fn compute_thread_metrics(
        a: &[ThreadSnapshot],
        b: &[ThreadSnapshot],
        window: Duration,
    ) -> Vec<ThreadMetrics> {
        let a_by_id: HashMap<u64, &ThreadSnapshot> = a.iter().map(|t| (t.thread_id, t)).collect();
        let window_us = window.as_micros().max(1) as u64;
        let mut out: Vec<ThreadMetrics> = Vec::with_capacity(b.len());
        for tb in b {
            let (user_delta, sys_delta) = match a_by_id.get(&tb.thread_id) {
                Some(ta) => (
                    tb.user_time_us.saturating_sub(ta.user_time_us),
                    tb.system_time_us.saturating_sub(ta.system_time_us),
                ),
                None => (0, 0),
            };
            let total = user_delta.saturating_add(sys_delta);
            out.push(ThreadMetrics {
                tid: tb.thread_id,
                name: tb.name.clone(),
                cpu_percent: (total as f64 / window_us as f64) * 100.0,
            });
        }
        out.sort_by_key(|t| t.tid);
        out
    }

    fn process_uptime_secs() -> f64 {
        PROCESS_START.elapsed().as_secs_f64()
    }

    /// Read the name of the thread identified by `port` using
    /// `pthread_from_mach_thread_np` + `pthread_getname_np`.
    /// Returns an empty string for kernel-internal threads
    /// (NULL pthread) and for any non-zero return from
    /// `pthread_getname_np`. Buffer is `MAXTHREADNAMESIZE = 64`.
    fn read_thread_name(port: libc::thread_act_t) -> String {
        // SAFETY: pthread_from_mach_thread_np maps a Mach port
        // for a thread *in the current process* to its
        // pthread_t. We only ever pass ports returned by
        // task_threads(mach_task_self()), which satisfies that
        // requirement. Returns NULL for unknown ports.
        let pthread = unsafe { libc::pthread_from_mach_thread_np(port) };
        if pthread.is_null() {
            return String::new();
        }
        let mut buf = [0i8; 64];
        // SAFETY: pthread_getname_np writes at most `len` bytes
        // including the nul terminator. The pthread is the one
        // we just looked up; the buffer is a stack-local of
        // exactly `len` bytes.
        let rc = unsafe { libc::pthread_getname_np(pthread, buf.as_mut_ptr(), buf.len()) };
        if rc != 0 {
            return String::new();
        }
        // SAFETY: the buffer was zeroed before the call and
        // pthread_getname_np writes a nul-terminated string,
        // so there is a nul within the buffer bounds.
        let cstr = unsafe { CStr::from_ptr(buf.as_ptr()) };
        cstr.to_string_lossy().into_owned()
    }

    /// Per-thread snapshot via two `thread_info` calls
    /// (THREAD_BASIC_INFO for CPU times, THREAD_IDENTIFIER_INFO
    /// for the stable thread_id) plus a `pthread_getname_np`
    /// for the name. Returns `None` if the thread died between
    /// `task_threads` and the per-thread query — matches the
    /// Linux "tolerate disappearing threads" policy.
    fn take_one_thread_snapshot(port: libc::thread_act_t) -> Option<ThreadSnapshot> {
        let mut basic: libc::thread_basic_info = unsafe { std::mem::zeroed() };
        let mut count: libc::mach_msg_type_number_t = libc::THREAD_BASIC_INFO_COUNT;
        // SAFETY: thread_info has no preconditions beyond a
        // valid thread port and a correctly-sized buffer. We
        // pass a port returned by task_threads (this iteration)
        // and a stack-local of exactly the declared shape.
        let kr = unsafe {
            libc::thread_info(
                port,
                libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                &mut basic as *mut _ as libc::thread_info_t,
                &mut count,
            )
        };
        if kr != libc::KERN_SUCCESS {
            return None;
        }

        let mut ident: libc::thread_identifier_info = unsafe { std::mem::zeroed() };
        let mut count: libc::mach_msg_type_number_t = libc::THREAD_IDENTIFIER_INFO_COUNT;
        // SAFETY: same preconditions as the THREAD_BASIC_INFO
        // call above.
        let kr = unsafe {
            libc::thread_info(
                port,
                libc::THREAD_IDENTIFIER_INFO as libc::thread_flavor_t,
                &mut ident as *mut _ as libc::thread_info_t,
                &mut count,
            )
        };
        if kr != libc::KERN_SUCCESS {
            return None;
        }

        Some(ThreadSnapshot {
            thread_id: ident.thread_id,
            user_time_us: time_value_to_us(basic.user_time),
            system_time_us: time_value_to_us(basic.system_time),
            name: read_thread_name(port),
        })
    }

    /// Enumerate live Mach threads and produce a
    /// `Vec<ThreadSnapshot>`. The Mach port array is wrapped in
    /// `MachThreadList` so port references and the array
    /// allocation are released on every exit path.
    fn take_thread_snapshots() -> Result<Vec<ThreadSnapshot>, &'static str> {
        let mut ports: *mut libc::thread_act_t = std::ptr::null_mut();
        let mut count: libc::mach_msg_type_number_t = 0;
        // SAFETY: task_threads writes the port-array pointer
        // and count by pointer. mach_task_self() is
        // process-lifetime and cannot fail.
        let kr = unsafe { libc::task_threads(libc::mach_task_self(), &mut ports, &mut count) };
        if kr != libc::KERN_SUCCESS {
            return Err("task_threads failed");
        }
        // RAII wrapper: any early return below still cleans up.
        let list = MachThreadList { ports, count };

        let mut snapshots = Vec::with_capacity(list.count as usize);
        for &port in list.as_slice() {
            if let Some(snap) = take_one_thread_snapshot(port) {
                snapshots.push(snap);
            }
            // Else: thread died between task_threads and the
            // per-thread thread_info call; skip silently
            // (Linux parity).
        }
        Ok(snapshots)
    }

    fn take_snapshot() -> Result<Snapshot, &'static str> {
        let mut info: libc::mach_task_basic_info_data_t = unsafe { std::mem::zeroed() };
        let mut count: libc::mach_msg_type_number_t = (std::mem::size_of::<
            libc::mach_task_basic_info_data_t,
        >() / std::mem::size_of::<libc::natural_t>())
            as libc::mach_msg_type_number_t;
        // SAFETY: task_info has no preconditions beyond a live
        // task port and a correctly-sized output buffer. We
        // pass mach_task_self() (the current process's port,
        // process-lifetime, cannot fail) and a stack-local
        // `info` of exactly the shape declared by
        // MACH_TASK_BASIC_INFO. `count` is computed from the
        // same struct and is in/out by pointer. The call does
        // not retain any pointer past return.
        let kr = unsafe {
            libc::task_info(
                libc::mach_task_self(),
                libc::MACH_TASK_BASIC_INFO,
                &mut info as *mut _ as *mut libc::integer_t,
                &mut count,
            )
        };
        if kr != libc::KERN_SUCCESS {
            return Err("task_info(MACH_TASK_BASIC_INFO) failed");
        }
        let threads = take_thread_snapshots()?;
        Ok(Snapshot {
            user_time_us: time_value_to_us(info.user_time),
            system_time_us: time_value_to_us(info.system_time),
            resident_size: info.resident_size,
            virtual_size: info.virtual_size,
            threads,
        })
    }

    pub fn sample(window: Duration) -> RuntimeMetrics {
        let snap_a = match take_snapshot() {
            Ok(s) => s,
            Err(reason) => return RuntimeMetrics::unavailable(reason),
        };
        std::thread::sleep(window);
        let snap_b = match take_snapshot() {
            Ok(s) => s,
            Err(reason) => return RuntimeMetrics::unavailable(reason),
        };
        let cpu_percent = process_cpu_percent(&snap_a, &snap_b, window);
        let threads = compute_thread_metrics(&snap_a.threads, &snap_b.threads, window);
        RuntimeMetrics::MacOS {
            sample_window_ms: window.as_millis() as u64,
            process: ProcessMetrics {
                cpu_percent,
                rss_kb: snap_b.resident_size / 1024,
                vm_size_kb: snap_b.virtual_size / 1024,
                uptime_secs: process_uptime_secs(),
            },
            threads,
            platform: "macos".to_string(),
        }
    }
}

// ── Public entry point ─────────────────────────────────────

/// Sample runtime metrics over the given window duration.
///
/// On Linux, this reads `/proc/self/stat`, `/proc/self/status`,
/// and `/proc/self/task/<tid>/stat`, sleeps for `window`, then
/// re-reads and computes CPU deltas.
///
/// On macOS (phase 1), this calls
/// `task_info(MACH_TASK_BASIC_INFO)` twice with a `sleep(window)`
/// between, computing process-level CPU% from the delta. Threads
/// are not enumerated until phase 2.
///
/// On other platforms, returns `RuntimeMetrics::Unavailable`
/// immediately without sleeping.
pub fn sample(window: Duration) -> RuntimeMetrics {
    #[cfg(target_os = "linux")]
    {
        linux::sample(window)
    }
    #[cfg(target_os = "macos")]
    {
        macos::sample(window)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = window; // not used on unsupported platforms
        RuntimeMetrics::unavailable(format!(
            "per-thread metrics not implemented on {}",
            std::env::consts::OS
        ))
    }
}

/// Initialise platform-specific runtime-metrics state at
/// process start.
///
/// On macOS this forces the `PROCESS_START` LazyLock so
/// subsequent `uptime_secs` values measure from this call
/// site rather than the first `sample()` call. Call once at
/// the top of `main()`, before any tokio runtime init or
/// `sample()` call, so the uptime baseline reflects true
/// process start.
///
/// On other platforms this is a no-op.
///
/// Idempotent and cheap; safe to call more than once. If a
/// `sample()` already ran before `init_at_startup`, the
/// LazyLock is already set and this call is a no-op — see
/// `PLAN-macos-runtime-metrics-phase-03-integration.md`
/// "Risks" for the ordering requirement.
pub fn init_at_startup() {
    #[cfg(target_os = "macos")]
    {
        macos::force_process_start();
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

    #[test]
    fn test_macos_variant_serialises() {
        // Phase-1 contract: same JSON shape as Linux,
        // distinguished by the `platform` field. `threads` is
        // empty in phase 1; phase 2 will populate it.
        let m = RuntimeMetrics::MacOS {
            sample_window_ms: 2000,
            process: ProcessMetrics {
                cpu_percent: 18.75,
                rss_kb: 98_304,
                vm_size_kb: 524_288,
                uptime_secs: 31.4,
            },
            threads: Vec::new(),
            platform: "macos".to_string(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"sample_window_ms\":2000"));
        assert!(json.contains("\"cpu_percent\":18.75"));
        assert!(json.contains("\"rss_kb\":98304"));
        assert!(json.contains("\"vm_size_kb\":524288"));
        assert!(json.contains("\"threads\":[]"));
        assert!(json.contains("\"platform\":\"macos\""));
        // Untagged: no enum variant name in the JSON.
        assert!(!json.contains("\"MacOS\""));
        assert!(!json.contains("\"Linux\""));
    }

    #[test]
    fn test_init_at_startup_runs_without_panic() {
        // Phase-3 contract: init_at_startup() is unconditionally
        // callable on every platform and idempotent. On Linux
        // it is a no-op; on macOS it forces the PROCESS_START
        // LazyLock. Either way, calling it twice in a row from
        // a test must not panic, return an error, or block.
        init_at_startup();
        init_at_startup();
    }

    // ── macOS phase-1 helper tests ─────────────────────────
    //
    // These tests exercise the delta-math and conversion
    // helpers that are platform-independent. They run on every
    // CI matrix entry (including Linux) so a regression in the
    // helper logic is caught before reaching a Mac. The
    // helpers live inside `#[cfg(target_os = "macos")] mod
    // macos`, so the tests are gated to that platform too —
    // delta math correctness is checked there.

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_time_value_to_us_basic() {
        use super::macos::time_value_to_us;
        let tv = libc::time_value_t {
            seconds: 0,
            microseconds: 0,
        };
        assert_eq!(time_value_to_us(tv), 0);
        let tv = libc::time_value_t {
            seconds: 1,
            microseconds: 500_000,
        };
        assert_eq!(time_value_to_us(tv), 1_500_000);
        let tv = libc::time_value_t {
            seconds: 47,
            microseconds: 250,
        };
        assert_eq!(time_value_to_us(tv), 47_000_250);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_process_cpu_percent_delta() {
        use super::macos::{process_cpu_percent, Snapshot};
        // 100 ms window, 50 ms of user CPU + 10 ms of system =
        // 60 ms CPU consumed during a 100 ms wall window.
        // Expected: 60.0 percent.
        let a = Snapshot {
            user_time_us: 1_000_000,
            system_time_us: 0,
            resident_size: 0,
            virtual_size: 0,
            threads: Vec::new(),
        };
        let b = Snapshot {
            user_time_us: 1_050_000,
            system_time_us: 10_000,
            resident_size: 0,
            virtual_size: 0,
            threads: Vec::new(),
        };
        let pct = process_cpu_percent(&a, &b, std::time::Duration::from_millis(100));
        assert!((pct - 60.0).abs() < 0.01, "expected ~60.0%, got {}", pct);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_process_cpu_percent_zero_window() {
        use super::macos::{process_cpu_percent, Snapshot};
        // Zero-duration window must not produce NaN/Inf. The
        // helper bumps the divisor to 1 µs, so the resulting
        // percent is finite (zero when deltas are zero).
        let s = Snapshot {
            user_time_us: 0,
            system_time_us: 0,
            resident_size: 0,
            virtual_size: 0,
            threads: Vec::new(),
        };
        let pct = process_cpu_percent(&s, &s, std::time::Duration::from_millis(0));
        assert!(pct.is_finite(), "percent must be finite: {}", pct);
        assert_eq!(pct, 0.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_process_cpu_percent_clock_reset() {
        use super::macos::{process_cpu_percent, Snapshot};
        // Pathological: second snapshot's CPU is *less* than
        // first's (clock reset / thread-accounting quirk).
        // Saturating subtract yields 0; percent = 0.
        let a = Snapshot {
            user_time_us: 1_000_000,
            system_time_us: 500_000,
            resident_size: 0,
            virtual_size: 0,
            threads: Vec::new(),
        };
        let b = Snapshot {
            user_time_us: 999_000,
            system_time_us: 400_000,
            resident_size: 0,
            virtual_size: 0,
            threads: Vec::new(),
        };
        let pct = process_cpu_percent(&a, &b, std::time::Duration::from_millis(100));
        assert_eq!(pct, 0.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_sample_returns_populated_variant() {
        // End-to-end smoke test on a real Mac: call sample()
        // with a short window and confirm a populated MacOS
        // variant comes back, not Unavailable. After phase 2
        // also asserts the threads list is populated and at
        // least one thread has a name (tokio names its
        // workers).
        let m = sample(std::time::Duration::from_millis(100));
        match m {
            RuntimeMetrics::MacOS {
                sample_window_ms,
                process,
                threads,
                platform,
            } => {
                assert_eq!(platform, "macos");
                assert_eq!(sample_window_ms, 100);
                // Phase 2: every Mac process has at least the
                // main thread. The test binary itself has more.
                assert!(!threads.is_empty(), "expected non-empty threads");
                for t in &threads {
                    assert!(
                        t.cpu_percent.is_finite() && t.cpu_percent >= 0.0,
                        "thread tid={} has invalid cpu_percent={}",
                        t.tid,
                        t.cpu_percent
                    );
                }
                // Sorted by tid for determinism.
                let mut sorted = threads.clone();
                sorted.sort_by_key(|t| t.tid);
                assert_eq!(
                    threads.iter().map(|t| t.tid).collect::<Vec<_>>(),
                    sorted.iter().map(|t| t.tid).collect::<Vec<_>>(),
                    "threads must be sorted by tid"
                );
                // RSS must be positive for any running process.
                assert!(process.rss_kb > 0, "rss_kb={}", process.rss_kb);
                assert!(process.vm_size_kb > 0, "vm_size_kb={}", process.vm_size_kb);
                // CPU% is non-negative and finite.
                assert!(process.cpu_percent >= 0.0 && process.cpu_percent.is_finite());
                // Uptime is non-negative.
                assert!(process.uptime_secs >= 0.0);
            }
            other => panic!("expected MacOS variant, got {:?}", other),
        }
    }

    // ── macOS phase-2 thread-metric tests ──────────────────
    //
    // These exercise `compute_thread_metrics`, the
    // platform-independent helper that produces ThreadMetrics
    // from two ThreadSnapshot lists + a window. They're gated
    // to target_os = "macos" because the types live inside
    // the cfg-gated `mod macos`; the logic itself is pure
    // Rust and could run anywhere.

    #[cfg(target_os = "macos")]
    fn ts(thread_id: u64, user_us: u64, sys_us: u64, name: &str) -> super::macos::ThreadSnapshot {
        super::macos::ThreadSnapshot {
            thread_id,
            user_time_us: user_us,
            system_time_us: sys_us,
            name: name.to_string(),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_compute_thread_metrics_basic() {
        use super::macos::compute_thread_metrics;
        // Two threads, both present in A and B. Window = 100 ms.
        // Thread 1: 50 ms user + 10 ms sys delta = 60% CPU.
        // Thread 2: 25 ms user + 5 ms sys delta = 30% CPU.
        let a = vec![ts(1, 1_000_000, 0, "worker"), ts(2, 500_000, 0, "main")];
        let b = vec![
            ts(1, 1_050_000, 10_000, "worker"),
            ts(2, 525_000, 5_000, "main"),
        ];
        let out = compute_thread_metrics(&a, &b, Duration::from_millis(100));
        assert_eq!(out.len(), 2);
        // Output sorted by tid: thread 1 first, then 2.
        assert_eq!(out[0].tid, 1);
        assert_eq!(out[0].name, "worker");
        assert!(
            (out[0].cpu_percent - 60.0).abs() < 0.01,
            "expected ~60%, got {}",
            out[0].cpu_percent
        );
        assert_eq!(out[1].tid, 2);
        assert!((out[1].cpu_percent - 30.0).abs() < 0.01);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_compute_thread_metrics_new_thread() {
        // Thread in B but not in A is a new arrival mid-window;
        // report 0% CPU (Linux parity), not garbage from
        // attributing B's accumulated CPU to a zero baseline.
        use super::macos::compute_thread_metrics;
        let a = vec![ts(1, 1_000_000, 0, "worker")];
        let b = vec![
            ts(1, 1_050_000, 0, "worker"),
            ts(99, 999_999, 999_999, "newcomer"),
        ];
        let out = compute_thread_metrics(&a, &b, Duration::from_millis(100));
        assert_eq!(out.len(), 2);
        // Find the new thread by tid.
        let newcomer = out.iter().find(|t| t.tid == 99).expect("newcomer present");
        assert_eq!(newcomer.cpu_percent, 0.0);
        assert_eq!(newcomer.name, "newcomer");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_compute_thread_metrics_dropped_thread() {
        // Thread in A but not in B died mid-window; it must not
        // appear in the output at all.
        use super::macos::compute_thread_metrics;
        let a = vec![ts(1, 1_000_000, 0, "worker"), ts(7, 500_000, 0, "doomed")];
        let b = vec![ts(1, 1_050_000, 0, "worker")];
        let out = compute_thread_metrics(&a, &b, Duration::from_millis(100));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tid, 1);
        assert!(out.iter().all(|t| t.tid != 7));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_compute_thread_metrics_sorted_by_tid() {
        // Inputs in arbitrary order produce output sorted
        // ascending by tid.
        use super::macos::compute_thread_metrics;
        let a = vec![ts(42, 0, 0, "a"), ts(7, 0, 0, "b"), ts(101, 0, 0, "c")];
        let b = vec![ts(101, 0, 0, "c"), ts(42, 0, 0, "a"), ts(7, 0, 0, "b")];
        let out = compute_thread_metrics(&a, &b, Duration::from_millis(100));
        let tids: Vec<u64> = out.iter().map(|t| t.tid).collect();
        assert_eq!(tids, vec![7, 42, 101]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_compute_thread_metrics_zero_window() {
        // Zero-window must produce finite percent for every
        // thread (the .max(1) µs guard from phase 1
        // generalises).
        use super::macos::compute_thread_metrics;
        let a = vec![ts(1, 1_000_000, 0, "w")];
        let b = vec![ts(1, 1_050_000, 0, "w")];
        let out = compute_thread_metrics(&a, &b, Duration::from_millis(0));
        assert_eq!(out.len(), 1);
        assert!(out[0].cpu_percent.is_finite());
    }
}

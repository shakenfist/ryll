//! K1 hang watchdog.
//!
//! Monitors main's heartbeat timestamp from a plain `std::thread` -- not a
//! tokio task, by design: if the runtime is itself wedged, a task would never
//! be polled to notice.  When the heartbeat goes silent for more than five
//! seconds the watchdog shells out to gdb for all-thread backtraces, taken at
//! the moment of the freeze rather than after the server-side disconnect has
//! torn everything down.
//!
//! Opt-in via `RYLL_WATCHDOG_GDB=1`.  Requires `gdb` on PATH and either a
//! permissive `kernel.yama.ptrace_scope` (=0) or `cap_sys_ptrace`.
//!
//! Context: the K1 hang investigation, sessions 001b/c/d/f/g.  main's task was
//! observed to silently stop polling some time after T+465 across every
//! reproduction -- neither the read branch nor the keepalive branch fires
//! after that, but the task does not exit either.

// audit-allow-println: this module reports through eprintln! rather than
// tracing, deliberately.  It exists to say something when the main thread has
// stopped responding, and the tracing subscriber is exactly the machinery that
// cannot be relied on in that state -- a wedged thread holding the global
// subscriber lock would swallow the one diagnostic worth having.  Writing
// straight to stderr takes no lock the stuck thread could be holding.
//
// The marker excludes a whole file from wave 1's raw-print check, which is why
// the watchdog lives in its own module: in main_channel.rs it exempted all
// 1900-odd lines, including the message handling that has nothing to do with
// it.  Here it exempts only the code the argument above actually covers.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tracing::info;

/// Silence longer than this triggers a backtrace capture.
const HEARTBEAT_TIMEOUT_MS: u64 = 5_000;

/// How often the watchdog wakes to compare the heartbeat against the clock.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Spawn the watchdog thread if `RYLL_WATCHDOG_GDB=1` is set.
///
/// Does nothing otherwise, so callers can invoke it unconditionally.
pub(crate) fn spawn_if_enabled(last_heartbeat_ms: &Arc<AtomicU64>) {
    if !std::env::var("RYLL_WATCHDOG_GDB")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return;
    }

    let hb = last_heartbeat_ms.clone();
    let pid = std::process::id();
    info!(
        "main: K1 watchdog enabled (pid {}); will dump backtraces if heartbeat silent >5 s",
        pid
    );
    std::thread::Builder::new()
        .name("ryll-watchdog".into())
        .spawn(move || watch(hb, pid))
        .expect("failed to spawn ryll-watchdog thread");
}

fn watch(hb: Arc<AtomicU64>, pid: u32) {
    // Fires once per silence period, so a single hang produces one dump
    // rather than one every poll interval.
    let mut fired = false;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let last = hb.load(Ordering::Relaxed);
        if last == 0 {
            continue;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let gap_ms = now.saturating_sub(last);
        if gap_ms <= HEARTBEAT_TIMEOUT_MS {
            fired = false;
            continue;
        }
        if !fired {
            fired = true;
            capture_backtrace(pid, gap_ms, now);
        }
    }
}

fn capture_backtrace(pid: u32, gap_ms: u64, now: u64) {
    let bt_path = format!("/tmp/ryll-watchdog-bt-{}-{}.txt", pid, now);
    eprintln!(
        "ryll-watchdog: main heartbeat silent for {} ms, \
         capturing all-thread backtrace via gdb -> {}",
        gap_ms, bt_path
    );
    let bt_file = match std::fs::File::create(&bt_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("ryll-watchdog: could not create {}: {}", bt_path, e);
            return;
        }
    };
    let status = std::process::Command::new("gdb")
        .args([
            "--batch",
            "-p",
            &pid.to_string(),
            "-ex",
            "set pagination off",
            "-ex",
            "thread apply all bt",
            "-ex",
            "detach",
            "-ex",
            "quit",
        ])
        .stdout(bt_file)
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {
            eprintln!("ryll-watchdog: backtrace captured to {}", bt_path);
        }
        Ok(s) => {
            eprintln!(
                "ryll-watchdog: gdb exited with status {:?}; \
                 check {} for partial output",
                s.code(),
                bt_path
            );
        }
        Err(e) => {
            eprintln!("ryll-watchdog: failed to spawn gdb: {}", e);
        }
    }
}

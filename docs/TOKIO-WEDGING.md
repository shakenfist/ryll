# K1: tokio waker silently lost on main channel after T+465 s

## TL;DR

After ~7 minutes 45 seconds of an idle SPICE session, ryll's
main-channel tokio task suspends with two valid Wakers
registered (one with the timer wheel, one with the IO driver
for the TLS socket). **Neither Waker is ever called again.**
Other tokio tasks on the same runtime continue to function
normally; the rest of the session disconnects ~45 seconds
later when the SPICE server's rcc 30 s connectivity check
fires and tears every channel down. ryll's main task remains
hung indefinitely (does not exit even on SIGINT).

This document is a checkpoint of the K1 investigation
(PLAN-session-001-feedback Phase 02) so a future session can
pick up where we left off.

## Reproducer

- A built ryll connecting to any SPICE server (we have used
  Kerbside-fronted oVirt VMs and a direct libvirt-managed
  GNOME desktop). Server choice does not appear to matter.
- Connect, leave the session backgrounded / unfocused / idle
  for >7m45s.
- Disconnect modal pops at exactly **session_uptime ≈ 510 s**
  in every reproduction (range 510–540 s, dominated by 510 s).
- main's tokio task does not exit; the run-loop wrapper (added
  in commit `c956ed65`) never logs `exited cleanly` or `exited
  with error` for main even though it does for every other
  channel.

This reproduces against the existing `sf-4` test target from
the dev box at `192.168.1.7` and from a macOS laptop. It
reproduces against a GNOME guest (which also fires K2 large-
frame warnings every 60 s) — we have not yet tested against
`uefi-latency-guest` to see whether the trigger is GNOME-
display-traffic-shaped.

## What we have eliminated

The bug survives all of the following changes:

| Tested change | Outcome |
|---|---|
| **macOS only?** Linux reproduces with identical signature | ❌ not platform-specific |
| **Multi-threaded scheduler?** `--debug-single-thread-runtime` reproduces too | ❌ not scheduler |
| **macOS App Nap / pasteboard?** Linux has neither, reproduces | ❌ not macOS-specific |
| **Clipboard polling?** `RYLL_DISABLE_CLIPBOARD_POLL=1` reproduces with `last_arm=keepalive_idle_done` | ❌ not the clipboard arm |
| **My Phase 02 keepalive code?** K1 was already in session-001b *before* any keepalive/heartbeat additions; the new code just made it diagnosable | ❌ not Phase 02 additions |
| **Server-side stop?** Wire-level tcpdump confirms: **server keeps a healthy connection until T+510 (ack exchanges, then FIN-ACK)**, and FIN delivery is intact at the kernel level | ❌ not server-side after T+465 |

## Diagnostic infrastructure left in tree

The following diagnostic instrumentation is live on the
`session-001-feedback` branch and should stay in until K1 is
closed. Each is gated either by an env var or a Cargo feature
so production builds aren't affected.

| What | Where | Activated by |
|---|---|---|
| Per-channel `run_loop` exit logs | `shakenfist-spice-renderer/src/channels/*.rs` (commit `c956ed65`) | always |
| Heartbeat with `iter_count` and `last_arm` | `main_channel.rs` `run_loop` | always (info!) |
| In-process gdb watchdog (5 s silence threshold, dumps `thread apply all bt` to `/tmp/ryll-watchdog-bt-<pid>-<ts>.txt`) | `main_channel.rs` `run_loop` | env: `RYLL_WATCHDOG_GDB=1` |
| `--debug-single-thread-runtime` flag | `ryll/src/{config,app,main}.rs` | CLI flag |
| `RYLL_DISABLE_CLIPBOARD_POLL` env var | `main_channel.rs` `run_loop` | env: `RYLL_DISABLE_CLIPBOARD_POLL=1` |
| `tokio-console` cargo feature with `console-subscriber` 0.5+ | `ryll/Cargo.toml`, `ryll/src/main.rs` | env: `RYLL_TOKIO_CONSOLE=1` (build with `make build-tokio-console`) |
| `make macos-build` / `make build-tokio-console` Makefile targets | `Makefile` | manual |
| Per-channel pcap capture (post-decryption SPICE) | existing `--capture <DIR>` | CLI flag |

The `RYLL_GIT_SHA` build-time embed (commit `e3179ca5`) means
every test binary identifies itself in `info!("ryll v… (sha)")`
at startup and in the bug-report `metadata.json`.

## Test session evidence directory

`~/ryll-test-sessions/test-session-001*/` holds raw artefacts
from every reproduction. Notable ones:

- `001b/` — three early macOS captures, no Phase 02 code.
- `001c/` — first macOS capture with Phase 02 keepalive plumbing.
- `001d/` — first run with the `client_keepalive_send_count`
  field; established the ~31-keepalive deterministic pattern.
- `001f/` — confirmed main never logs an exit.
- `001g/`, `001h/` — Linux reproductions; established the
  bug is not macOS-specific.
- `001i/` — first Linux disconnect zip.
- `001j/` — Linux with watchdog + run-loop wrapper.
- `001k/` — `--debug-single-thread-runtime` (still hangs).
- `001m/` — `RYLL_DISABLE_CLIPBOARD_POLL=1` (still hangs).
- `001n/`, `001o/` — heartbeat with iter counter showing
  the loop iterated ~3/sec right up to T+465 then stopped.
- `001p/` — per-channel pcap capture confirmed which arms
  fired and when each channel went silent.
- `001q/` — wire-level tcpdump (`wire.pcap`) confirmed the
  server sent FIN at T+510; ryll's main never reacted.
- `001s/` — tokio-console capture (this document's smoking
  gun).

## The smoking-gun snapshot (test-session-001s, 2026-05-10)

Test #10: ryll built with `make build-tokio-console`, run with
`RYLL_WATCHDOG_GDB=1 RYLL_TOKIO_CONSOLE=1`. ryll started at
19:29:12 local. K1 fired at T+465.246. tokio-console connected.
Drilling into main's task (id=23, spawned at
`shakenfist-spice-renderer/src/session.rs:118:23`) yielded:

```
ID: 23 ⏸
Target: tokio::task
Location: shakenfist-spice-renderer/src/session.rs:118:23
Total Time: 13m00s
Busy: 1.17s (0.15%)

Waker:
  Current wakers: 2
  Clones: 12595, Drops: 12593
  Woken: 2179 times
  Last woken: 5m14s ago

Warning: This task occupies a large amount of stack space (1432 bytes)
```

Decoded:

- **Last woken 5m14s ago = session_uptime 7m46s = T+466**: matches every prior reproduction's freeze time exactly.
- **Current wakers: 2** = one clone for the timer wheel
  (covers `heartbeat`, `keepalive_idle`, `keepalive_timeout`),
  one clone for the IO driver (TLS socket via mio).
- **Both wakers stay registered for 5+ minutes without firing**
  — even though heartbeat's deadline (T+467) and the FIN at
  T+510 should both have called them.
- **Other tokio tasks on the same runtime are unaffected.**
  Their wakers fire correctly throughout. Only main's task is
  affected.
- The 1432-byte stack-space warning is a perf hint, not a
  trigger; tokio's auto-box threshold is 2 KiB, we're under
  it. Probably unrelated.

## Conclusion

**Two valid Waker subscriptions are registered for main's
task; both are silently dropped/disconnected by the runtime
after T+465–466 specifically for this task.** This is either
a bug in tokio's runtime bookkeeping or an extremely subtle
interaction in our code that triggers exactly that condition.

## Next steps (in order of cheapness)

1. **Bump tokio to the latest 1.x release.** We are on
   1.51.1. There may be a fix in a newer point release. Same
   goes for `tokio-rustls` (0.26.4 → latest), `mio`, `rustls`.
2. **Test against `uefi-latency-guest`** via `make test-qemu`
   to rule in or out GNOME-display-traffic shape.
3. **Construct a minimal reproducer** suitable for filing a
   tokio issue. The shape is: a single channel reading a
   tokio-rustls TcpStream inside a `tokio::select!` with
   several `sleep_until` and `interval.tick()` arms; under
   sustained server PING/PONG load the read arm has been
   firing reliably; after some specific server-driven event
   the task suspends with non-zero registered Wakers that
   never fire.
4. **Bisect ryll's history** via `make build` checkout-by-
   checkout to find when K1 was first introduced. The pattern
   (always at T+510 disconnect) suggests it's been latent for
   a while and only became visible once the user's dogfooding
   hit the trigger window.
5. **Try replacing `tokio::select!` with `tokio_util::time::FuturesUnordered`
   or hand-rolled future polling** in main_channel — different
   waker subscription shape, might dodge the trigger even if
   it doesn't fix the underlying tokio bug.

## Open questions

- **Does the bug trigger only on main-channel-shaped tasks?**
  Main has 7 select! arms (read + monitors_config_rx +
  debounce_sleep + clipboard_interval + keepalive_timeout +
  keepalive_idle + heartbeat) where most other channels have
  3–4. Could the count of arms or the size of the state
  machine matter?
- **What's special about the 12th server PING?** Server
  consistently sends 12 PINGs at 15 s intervals (T+300 to
  T+465) then stops. This is server-side behaviour we have
  not characterised — but it does NOT cause the freeze on its
  own (server keeps the TCP connection alive cleanly through
  T+510 per `wire.pcap`).
- **Why does the freeze fire after PING #66 specifically?**
  The disconnect-cause snapshots in every reproduction show
  `pong_send_count: 65` at the last visible heartbeat and
  `pong_send_count: 66` after the freeze. The hang is
  triggered after exactly one more PING-PONG cycle past
  whatever server-side state change happens at T+465.

## What to do if the session restarts

- The branch is `session-001-feedback`. Latest commits should
  be on `origin/session-001-feedback`.
- All of the diagnostic instrumentation listed above is in
  the latest commits; nothing needs to be re-added.
- The `~/ryll-test-sessions/test-session-001*/` directories
  are NOT under git — they're scratch evidence. Don't delete.
- The `--cfg tokio_unstable` build is needed for tokio-console
  but the regular `make build` does NOT need it. Don't accidentally
  apply `RUSTFLAGS=--cfg tokio_unstable` to a release build.
- The watchdog environment variable is `RYLL_WATCHDOG_GDB=1`,
  not `RYLL_GDB_WATCHDOG=1`.

## Companion plan

This investigation is part of Phase 02 of
`docs/plans/PLAN-session-001-feedback-phase-02-reconnect.md`.
The K1 fix (whatever it ends up being) should be folded into
Phase 02's "Block C" once we have it. The auto-reconnect UX
work in Block A still applies regardless of K1's outcome —
even if main is fixed, transient network errors will still
benefit from automatic reconnect.

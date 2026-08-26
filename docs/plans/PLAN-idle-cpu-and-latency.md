# Idle CPU reduction and real latency measurement

## Prompt

Before responding to questions or discussion points in this
document, explore the ryll codebase thoroughly. Read relevant
source files, understand existing patterns (SPICE protocol
handling, channel architecture, async task model, image
decompression, egui rendering), and ground your answers in
what the code actually does today. Do not speculate about
the codebase when you could read it instead. Where a question
touches on external concepts (SPICE protocol, QEMU, QXL,
TLS/RSA, LZ/GLZ compression), research as needed to give a
confident answer. Flag any uncertainty explicitly rather than
guessing.

All planning documents should go into `docs/plans/`.

Consult `ARCHITECTURE.md` for the system architecture
overview, channel types, and data flow. Consult `AGENTS.md`
for build commands, project conventions, code organisation,
and a table of protocol reference sources.

## Situation

Two related issues observed during interactive use of ryll
on branch `screenshot`:

### Issue 1 — Ryll consumes ~6 CPU cores at idle

Just sitting connected to a SPICE session with nothing
happening, ryll burns roughly 6 cores. This is unacceptable
for a release. Initial investigation points to two likely
contributors, neither yet confirmed as the dominant cause:

1. **Forced 60 FPS repaint.** [ryll/src/app.rs:2169](https://github.com/shakenfist/ryll/blob/develop/ryll/src/app.rs#L2169)
   calls `ctx.request_repaint_after(Duration::from_millis(16))`
   unconditionally at the end of every `update()`. egui then
   re-renders the entire UI (including all sparklines, the
   surface texture, etc.) 60 times a second whether anything
   changed or not.

2. **INFO-level logging on every protocol message.** Every
   channel logs every message it sends or receives via
   `shakenfist_spice_protocol::logging::log_message`
   ([logging.rs:22-32](https://github.com/shakenfist/ryll/blob/develop/shakenfist-spice-protocol/src/logging.rs#L22-L32))
   at `info!` level. Each line allocates a
   `format_timestamp()` String, takes the global tracing
   subscriber lock, formats the line, and writes to
   `/tmp/ryll.log`. PINGs alone arrive multiple times per
   second on every channel; the inputs/cursor channels add
   per-event traffic. This was the symptom that originally
   prompted PLAN-remaining-issues item 3 ("Verbose logging
   cleanup"), and many but not all sites have already been
   demoted.

3. **Channel read loops.** Each channel has its own tokio
   task with a `tokio::select!` loop. If any of those select
   arms wakes spuriously or yields without backoff, the
   task burns CPU. Worth profiling, not assumed.

The evidence visible to the user is log output like:

```
2026-04-19T20:23:43.871004Z INFO [1776630223.870958] playback received 12 byte opcode 4 ping
2026-04-19T20:23:43.871080Z INFO [1776630223.871078] playback sent 12 byte opcode 3 pong
2026-04-19T20:23:43.871153Z INFO [1776630223.871151] playback received 12 byte opcode 4 ping
2026-04-19T20:23:43.871192Z INFO [1776630223.871192] playback sent 12 byte opcode 3 pong
```

Two timestamps per line — `2026-04-19T20:23:43.871004Z` is
added by `tracing-subscriber`, `[1776630223.870958]` is
added inside `log_message` itself. Redundant.

### Issue 2 — Latency measurement is broken

The current latency code at
[channels/inputs.rs:341-346](https://github.com/shakenfist/ryll/blob/develop/ryll/src/channels/inputs.rs#L341-L346)
sets `last_key_time = Some(Instant::now())` and on the very
next line reads `last_key_time.unwrap().elapsed()`. That
measures essentially zero (microseconds at most). The
`Latency: Xms` label and the latency sparkline added on
this branch consequently always read zero.

The naive fix — measure round-trip from keystroke to next
display update — is unsound: a "screen update" right after
a keystroke might be a clock tick, an MJPEG video stream
frame, a cursor blink, or anything else unrelated. There is
no way to tell whether a given draw_copy was caused by the
keystroke we just sent.

**Better approach found by protocol research:** SPICE has a
built-in PING/PONG mechanism on every channel.

- `SPICE_MSG_PING = 4` (server→client) and
  `SPICE_MSGC_PONG = 3` (client→server) are base-channel
  messages, mandatory since v1, no capability negotiation.
- Payload: `uint32 id` + `uint64 timestamp` (sender's
  monotonic ns). PONG echoes both fields verbatim — perfect
  for unambiguous correlation.
- Direction is asymmetric: there is no `SPICE_MSGC_PING`,
  so ryll cannot originate probes. The server's own
  per-channel ping timer (`red-channel-client.cpp` in
  spice-server) is the source. spice-gtk also only
  responds, never originates.
- Constants and the message struct already exist in this
  codebase: `shakenfist-spice-protocol/src/constants.rs`
  (lines 141, 158, plus per-channel duplicates) and
  `shakenfist-spice-protocol/src/messages.rs:78-108`. The
  main-channel handler at
  `ryll/src/channels/main_channel.rs:378-400` already
  parses PING and emits PONG — it just doesn't measure
  anything.
- Kerbside passes PING/PONG through unchanged
  (kerbside/docs/channel-protocols.md:538-554).

So the latency story is: the server is *already* sending us
the probe data; we just throw it away. We need to record
`Instant::now()` when a PING arrives, optionally compare
against the server timestamp, and emit a real
`ChannelEvent::Latency` event from there. This gives
**network RTT + client receive-loop turnaround**, not the
human-perceived input-to-display latency, but it's a real
number and it's the same metric spice-server itself uses
internally.

True input-to-display latency would require a different
mechanism (e.g. a bug-report flow where the user marks a
known glyph appearing on screen, or an instrumented test
guest). Out of scope for this plan.

## Mission and problem statement

1. **Get idle CPU down to a reasonable level.** Target:
   under 10% of one core when connected to an idle SPICE
   session, with no user input and no display updates. We
   should profile rather than guess: confirm the dominant
   contributor, fix it, measure again.

2. **Fix the latency measurement** by tapping the existing
   PING handlers, recording timestamps, and emitting a real
   `ChannelEvent::Latency` from there. Remove the broken
   keystroke-based code.

3. **Reduce log noise** as a CPU-and-readability win. PING
   and PONG logs in particular should be `debug!`, not
   `info!`. Drop the embedded `[unix_ts]` timestamp from
   `log_message` since `tracing-subscriber` already adds
   one.

## Open questions

1. **Is the 60 FPS forced repaint actually necessary?** The
   `request_repaint_after(16ms)` at app.rs:2169 looks like
   a workaround for some egui interaction where state
   changed but didn't trigger a repaint. We should
   investigate whether replacing it with event-driven
   repaints (call `ctx.request_repaint()` from the channel
   event handler when a frame, cursor move, or status
   change actually arrives) is sufficient. The bandwidth
   sparkline ticks once per second — that needs a repaint
   at 1 Hz, not 60 Hz. Recommendation: try event-driven +
   1 Hz fallback; benchmark.

2. **Should we measure latency on all channels or just
   one?** Server sends PING per-channel. Latency
   characteristics may differ (display channel has bigger
   buffers; inputs is sparser). Recommendation: measure on
   the main channel only for the status bar number, but
   record per-channel latency in the bug-report snapshot
   for diagnostic value. If main is too sparse, fall back
   to display.

3. **Drop the embedded timestamp from `log_message`?**
   Yes — but it changes the format of every protocol log
   line, which may break grep patterns in
   `tools/replay_*.py` or any external log analysis. Need
   to grep for consumers before changing.

4. **Should we add a CLI flag to enable verbose protocol
   logging on demand?** With everything demoted to debug,
   `-v` already enables it via the existing verbose mode
   ([app.rs:settings::is_verbose()] check sites). No new
   flag needed.

5. **Is there a smarter idle strategy than slowing repaint
   cadence?** egui supports `ctx.request_repaint_after`
   with a longer duration when nothing's changed. We could
   track "frames since last meaningful event" and back off
   to 1 Hz after, say, 250 ms of nothing. That gives snappy
   interactive feel and low idle CPU.

## Execution

| Phase | Plan | Status | Merged |
|-------|------|--------|--------|
| 1. Profile idle CPU | [PLAN-idle-cpu-and-latency-phase-01-profile.md](PLAN-idle-cpu-and-latency-phase-01-profile.md) | Complete | 6d52665 |
| 2. Repaint cadence fix | [PLAN-idle-cpu-and-latency-phase-02-repaint.md](PLAN-idle-cpu-and-latency-phase-02-repaint.md) | Complete | 6d52665 |
| 3. Demote protocol logging | [PLAN-idle-cpu-and-latency-phase-03-logging.md](PLAN-idle-cpu-and-latency-phase-03-logging.md) | Complete | 6d52665 |
| 4. Real latency from PING/PONG | [PLAN-idle-cpu-and-latency-phase-04-latency.md](PLAN-idle-cpu-and-latency-phase-04-latency.md) | Complete | 6d52665 |
| 5. Capture runtime metrics in bug reports | [PLAN-idle-cpu-and-latency-phase-05-metrics.md](PLAN-idle-cpu-and-latency-phase-05-metrics.md) | Complete | 6d52665 |
| 6. Push audit | PLAN-idle-cpu-and-latency-phase-06-push-audit.md | Not started | — |

Phase 1 informs phase 2: if profiling shows logging is the
dominant cost, swap their order.  *Profiling result: the
unconditional 60 Hz repaint at app.rs:2169 drives ~6 of
the 6.24 idle cores via llvmpipe rasteriser threads;
logging is a rounding error.  Phase 2 is the only fix that
moves the CPU needle.  Phase 3 is still worth doing for log
readability and a missing is_verbose() guard on the
playback channel.*  Phase 4 is independent of 1-3 but
should land last so its sparkline data is visible once the
other CPU-eating problems are fixed.

Phase 2 was held at `In progress` for four months after its
code landed, because the CPU reduction it is supposed to
deliver had not been confirmed on the reporter's machine.
*Verified 2026-08-27 on that machine: 2.80% of one core
idle, against the 6.24-core baseline, with a synthetic
pointer sweep taking the process to 269.5% and back to
prove egui still wakes on input.*  The numbers and the
method are in the phase 2 plan under *Step 2b result*.

Phase 5 was added after the original incident: had ryll
captured its own per-thread CPU into bug reports, the user
report would have included the llvmpipe breakdown directly
and the profiling phase would have been unnecessary.  This
phase makes future incidents self-debugging.

Phase 6 is the pre-push audit, and it runs last.  It works
`PUSH-AUDIT.md` over the accumulated diff of phases 1-5
against `develop`, not the last phase's diff alone, so it
sees what the repaint, logging, latency and metrics changes
did to each other.  Findings land as their own PR against
`develop`, recorded here under an *Items deferred from the
push audit* heading — the shape `PLAN-web-frontend.md` uses
for its *Items deferred from the post-Phase-N pre-push audit*
sections, minus the phase number, because this phase audits
the whole plan rather than a range of it.  This plan is not
complete until every one is fixed or declined in writing.  If
the audit finds nothing, that gets recorded here in one
sentence.

Phases 1-5 have already merged, so `develop...HEAD` is empty
on the audit branch and the accumulated diff has to be
assembled from where those phases landed.  The `Merged`
column above now records it, reconstructed 2026-08-27, and
the answer is not the one this paragraph assumed: all five
phases landed in a *single* merge, PR #36 (`6d52665`), not
one merge each.  `PUSH-AUDIT.md` cites this plan as its
worked example of a range that cannot be derived after the
fact, and on the per-phase question it is right — but the
plan's commits turn out to run contiguously on the
`screenshot` branch, from `90a954b` (the master plan) to
`1c28d6f`, so the audit range is exactly
`AUDIT_BASE=90a954b^1`, `AUDIT_HEAD=1c28d6f`.

Two things that range does not cover, and phase 6 must
handle explicitly.  `85bc901` ("Address automated reviewer
feedback on PR #36") sits above `AUDIT_HEAD` and may touch
this plan's code, so it has to be inspected separately.
And PR #36 also carried the screenshot-and-latency-HUD
plan's work, which sits *below* `AUDIT_BASE` — excluded by
the bound, but a reminder that the merge diff of `6d52665`
is the wrong patch to hand the judgment agents.

The larger caveat is age.  These commits are from April; the
crate extraction has since moved most of this code into
`shakenfist-spice-renderer`, and the web frontend was built
on the same event path.  Findings drawn from the April diff
have to be re-checked against current `develop` before they
are actionable.  *Two ways this runbook is invoked* in
`PUSH-AUDIT.md` has the rest.

## Agent guidance

### Execution model

All implementation work is done by sub-agents, never
in the management session.

### Planning effort

Phase 1 (profiling) is **medium effort** — the work is
mechanical (run perf/flamegraph, read output) but the
interpretation requires judgment.

Phase 2 (repaint) is **medium-to-high effort** — depends on
egui internals and risks regressing interactivity.

Phase 3 (logging) is **low effort** — search-and-demote.

Phase 4 (latency) is **medium effort** — well-scoped given
the protocol research above, but touches multiple channels
and needs care around PING handler placement.

### Step-level guidance

Per phase. Default model recommendation: **sonnet** for
phases 1, 3, 4; **opus** for phase 2 (egui repaint logic
is subtle).

### Management session review checklist

After each phase:

- [ ] CPU benchmark before/after (for phases 1, 2, 3).
- [ ] `pre-commit run --all-files` passes.
- [ ] `make test` passes.
- [ ] Manual smoke against `make test-qemu`: connect, type,
      mouse, disconnect — nothing regresses.

## Administration and logistics

### Success criteria

Checked 2026-08-27, at the point phase 2 closed.

* **Met.**  Idle CPU under 10% of one core (measured:
  connected, no input, no display activity).  2.80%, with
  the pointer over the window rather than outside it.  See
  *Step 2b result* in the phase 2 plan.
* **Met, but this criterion was mis-stated.**  The
  `Latency: Xms` label and sparkline show real non-zero
  numbers.  They are *not* a PING round-trip, and could
  never have been: `SPICE_MSG_PING` is server→client only,
  so the client has no probe to time a round trip with, a
  point this plan's own *Situation* section makes and its
  success criterion then forgot.  What shipped is the
  interval between consecutive server PINGs
  (`main_channel.rs`, the `main_server::PING` arm), which
  `docs/diagnostics.md` documents accurately.  The
  criterion is corrected here rather than left to read as
  though something else was delivered.
* **Met.**  Protocol logs at `debug!` level by default,
  visible with `-v`; the embedded `[unix_ts]` is gone
  (`shakenfist-spice-protocol/src/logging.rs:251`).
* **Met.**  `pre-commit run --all-files` and `make test`
  both pass.
* **Met, in a different file.**  The criterion names
  `README.md`'s verbose-logging section; the README-as-pitch
  restructure since moved that material out, and `-v` is now
  documented in `docs/configuration.md:79` and `:255`, with
  usage in `docs/troubleshooting.md:343`.  Again the
  intended shape, not a miss.
* **Met, in a different file.**  The criterion names
  `ARCHITECTURE.md`'s "Statistics and Instrumentation"
  section; the doc-discipline restructure since moved that
  material to `docs/diagnostics.md`, where the PING-based
  latency source is described at line 18.  `ARCHITECTURE.md`
  indexes it.  That is the intended shape, not a miss.
* **Met.**  Bug-report ZIPs carry `runtime-metrics.json`
  with process CPU%, per-thread CPU and names, and RSS
  (`bugreport.rs`, `shakenfist_spice_renderer::metrics`;
  covered by `test_bug_report_runtime_metrics_in_zip`).
  Linux-first, with an `unavailable` variant elsewhere.

One observation for phase 6 rather than a criterion: on the
loopback test guest the label read `Latency: 0.1ms`, which
is a gap *within* a server ping burst, not a steady-state
cadence — phase 1 recorded sf-3 sending "a burst of 2 pings
at connect time then going quiet".  A statistic whose value
depends on whether the server happens to be bursting is
worth a look before release.  Recorded, not fixed: it is
phase 4's design, not phase 2's, and changing it is outside
what closing phase 2 authorises.

### Future work

* True keystroke-to-display latency measurement (would
  need an instrumented guest or known-glyph detection).
* Per-channel latency breakdown in the bug-report
  snapshot.
* Adaptive repaint cadence based on observed input/output
  activity.
* Investigate whether the channel select! loops can be
  collapsed to fewer tokio tasks (one task per channel
  may be overkill for sparse channels like cursor/inputs).

### Bugs fixed during this work

(To be filled in as we go.)

### Documentation index maintenance

When created: add to `docs/plans/index.md` Master plans
table and `order.yml`. Mark Complete when all phases land.

### Back brief

Before executing any step of this plan, please back brief
the operator as to your understanding of the plan and how
the work you intend to do aligns with that plan.

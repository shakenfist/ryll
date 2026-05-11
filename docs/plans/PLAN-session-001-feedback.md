# Session-001 dogfooding feedback triage

## Prompt

Triage the feedback gathered during dogfooding test session 001
(see `~/ryll-test-sessions/test-session-001/`). The session was
run on macOS aarch64 against a QEMU VM exposed at `sf-4`. The
artefacts include a free-form `NOTES.md`, six `ryll-bugreport-*.zip`
archives, and one earlier pedantic report. This document is the
master plan for that session's follow-up: each item below either
turns into a phase plan (own commit / own PR), gets folded into
an existing initiative, or is explicitly deferred with a reason.

When working through items, respect the rest of the project's
plan conventions (per-phase plan files named
`PLAN-session-001-feedback-phase-NN-*.md`, one logical change per
commit, master-plan table updated as work lands). Phases in the
execution table below are listed in intended execution order;
no phase has a hard dependency on a phase listed below it, so
sequential execution is always safe.

## Situation

- **Build under test:** ryll 0.1.5, macOS aarch64 client, target
  `sf-4:35569` (QEMU + SPICE).
- **Session window:** ~2026-05-05 08:36Z (pedantic) and
  10:12Z–10:40Z (six manual bug reports).
- **Source material:**
  - `NOTES.md` — four free-form observations.
  - Six bug-report zips with metadata, session/channel state,
    notifications, traffic pcap, and (some) screenshots.
  - One pedantic report for `display:hexdump:126`.
- **Cross-cutting observations from the data itself:**
  - None of the six reports' `notifications.json` contains a
    Connect / Disconnect / Reconnect event. Confirms NOTES item 1
    — those transitions are not currently surfaced.
  - All six reports have
    `runtime-metrics: per-thread metrics not implemented on
    macos`. Mac-originated reports therefore omit that data
    permanently until the gap is closed.
  - Reports B4 and B6 carry a `region` of zero width
    (`{1948,1152,1948,1152}` and `{1940,1152,1940,1152}`). Either
    the user single-clicked instead of dragging, or the
    region-select widget is producing a degenerate rect.

## Catalogue

Sources are tagged `N#` (NOTES.md item), `B#` (bug-report zip
description), `D#` (derived from the report data).

### Confirmed bugs

| ID | Title | Sources | Severity | Disposition |
|----|-------|---------|----------|-------------|
| **K1** | Main channel rcc 30 s unresponsive timeout tears down session (perceived as inputs-channel disconnect) | N3, B3 (10:31:53Z), B5 (10:40:28Z), QEMU log | High — disrupts dogfooding workflow | **Resolved** in `370d8ce5` (root-cause fix) and `cf3d31f5` (regression test). Root cause was an abandoned-receiver deadlock in our own session orchestrator, not a tokio / rustls / kernel bug. See `docs/TOKIO-WEDGING.md` for the chronology. |
| **K2** | Ring-buffer frame builder drops SPICE messages > 64 KB (missing TCP segmentation in `bugreport.rs:317`; live writer already segments via `capture.rs:78`) | N4, B1 (10:12:15Z), B2 (10:15:01Z) | Medium — silently drops large display messages from bug-report pcaps | Phase 08 — own plan; mirror `write_segmented`; pairs with Phase 07 (entry-per-segment fits `Arc<[u8]>`) |
| **K3** | Reconnect resets client audio volume | B6 (10:40:51Z) | Low–Medium — surprises user after every reconnect | Phase 03 — own plan (small) |
| **K4** | Region-select can produce a zero-width rectangle | D2 (B4, B6 metadata) | Low — bad-data path in bug reports | Phase 04 — own plan (small); investigate before deciding fix vs. validation |
| **K5** | Unhandled `SPICE_MSG_DISPLAY_STREAM_DESTROY_ALL` (msg type 126) | pedantic zip 08:36Z (`display:hexdump:126`) | Low — streams are torn down individually elsewhere, but unhandled batch destroy leaves stale stream state on resolution change | Phase 05 — own plan (small); empty payload, one-line equivalent in spice-gtk (`channel-display.c:1880`) |

### Confirmed feature requests

| ID | Title | Sources | Severity | Disposition |
|----|-------|---------|----------|-------------|
| **F1** | Surface (re)connect events in the notifications pane | N1, D1 | Medium — visible UX gap, multiple reports show no transitions | Phase 09 — own plan |
| **F2** | "Turn this notification into a bug report" button | N2 | Low — quality-of-life | Phase 10 — own plan; Q1 resolved (always-fileable with `at_fire` / `post_event_only` marker); depends on Phase 07 landing first |

### Tracked under another plan

| ID | Title | Sources | Tracked at |
|----|-------|---------|------------|
| **U1** | "Video stream appears to not be keeping up" | B4 (10:39:47Z) | `PLAN-video-keeping-up.md` — closes the observability gap first (per-decode wall-time, socket-buffer high-water, ACK-window exhaustion) before attempting a fix, then moves bug-report I/O off the channel read path if instrumentation confirms it's a contributor. |
| **G1** | macOS runtime-metrics not implemented | Every report's `runtime-metrics.json` | `PLAN-macos-runtime-metrics.md` — adds a `MacOS` variant to `RuntimeMetrics` using the Mach `task_info` / `task_threads` / `thread_info` APIs already exposed by `libc` on Apple targets. Independent of session-001 work. |

## Open questions

1. **F2 traffic-buffer question (raised in NOTES item 2): RESOLVED.**
   Investigation showed the ring buffer is byte-capped, not
   time-capped (`ryll/src/bugreport.rs:176`,
   `PER_CHANNEL_BYTES = 50 MB / 6 ≈ 8.33 MB` per channel). At
   session-001 display rates (~2 MB/s typical, 6 MB/s peak from
   B2's `bandwidth_history`), the display ring holds only
   ~1.5–4 seconds of history during active video, while
   low-bandwidth channels (inputs, cursor, main, usbredir,
   playback) effectively retain the whole session.

   Resolution — **always-fileable, varying-quality:**
   - On notification fire, snapshot the ring buffer to a
     bounded side-buffer. Snapshot is retired after 60 s;
     at most 5 active snapshots stored at any time
     (drop oldest when a 6th arrives).
   - The "file as bug report" button is **always present** on
     every notification — never disappears. Clicking always
     produces a report.
   - If a live snapshot exists when the user clicks, the report
     embeds the snapshot and is marked
     `notification-snapshot: at_fire`. This is the gold case —
     report includes the run-up to the notification.
   - If no live snapshot exists (snapshot expired, or fell off
     the max-5 stack), the report embeds the current ring
     state and is marked `notification-snapshot: post_event_only`.
     The button visually indicates this (e.g. dimmed icon /
     hover tooltip "post-event context only — snapshot expired").
   - Cheap to clone thanks to Phase 07 (`Arc<[u8]>` for
     `pcap_frame` makes per-entry clone an atomic refcount bump),
     so the snapshot path stays cheap even on busy channels.
   - Snapshot contents are best-effort: low-bandwidth channels
     get full 60 s of pre-fire context; display under load gets
     whatever the ring held at fire time. Phase 06's rebalance
     improves the display case but doesn't eliminate the cap.
   - Documented limitation: a notification-derived bug report
     for a video-lag complaint may have a shorter pre-fire
     window than the user expects on busy display traffic, but
     the marker tells the report consumer which case they're in.

   Future work (not in scope for this plan): dynamic ring-buffer
   sizing keyed on system memory, so capable hardware gets a
   larger cap (and longer retention) automatically. Static
   per-channel rebalancing in Phase 06 captures most of the
   value without the dynamic-shrink/grow complexity.

2. **K1 root cause: RESOLVED (2026-05-11).** Three days of
   dogfooding-driven investigation across two sub-phases (the
   "diagnostic infrastructure" rollup in `5ba933da` and the
   "fix and regression test" pair `370d8ce5` / `cf3d31f5`)
   eventually localised K1 to our own session orchestrator, not
   anything in tokio / rustls / mio / the kernel.

   The 30 s server-side rcc timeout was a **downstream** symptom:
   what actually happened is that `shakenfist-spice-renderer/src/
   session.rs` created an intermediate `mpsc::channel(64)`,
   drained it only until `SessionInitialized` and
   `ChannelsAvailable` arrived, then dropped it on the floor.
   `MainChannel` kept producing `ChannelEvent::Latency` on every
   PING; after ~65 pings (the exact T+466 s fingerprint we kept
   seeing) the bounded buffer filled and main blocked forever
   inside `event_tx.send().await`. With main's entire `select!`
   suspended, no pongs went out, and the server's rcc timer
   tore the connection down 30 s later. The "tokio waker bug"
   appearance in `docs/TOKIO-WEDGING.md` was real but downstream
   — the waker the runtime was waiting for was the mpsc
   permit-available waker, which would never fire because the
   receiver had been dropped while the buffer was full.

   Fix replaces the temp mpsc with two `oneshot` channels for
   the session id and channels list, so main sends events
   directly into the real caller-owned `event_tx` (capacity
   1024, actually drained by the renderer). All
   `event_tx.send()` sites in `main_channel.rs` are now wrapped
   in a 5 s timeout helper as defense-in-depth.

   Regression test: `make test-k1-idle` (driver in
   `tools/test-k1-idle.sh`) idles a ryll session for 540 s and
   asserts no wedge / no timeout / sufficient pong count.
   Verified passing against spi2eth (TLS) and the local
   `test-qemu` target.

   This also retroactively explains why the previously-landed
   Phase 02 mitigations (mirror keepalive, KEY_MODIFIERS idle
   keepalive on inputs, spurious-PONG main keepalive at 10 s,
   etc.) **never made K1 go away** — they all kept *other*
   channels alive while main itself was suspended on a deadlock
   they couldn't break. Those mitigations remain useful
   defense-in-depth and stay landed, but they're no longer
   load-bearing for K1.

3. **K2 reproduction: RESOLVED — not an MTU issue.** Code walk
   showed the trigger is **missing TCP segmentation in the ring
   buffer's frame builder**, not anything to do with the
   network's MTU. The live pcap writer already segments at
   `MAX_PAYLOAD = 65495` (IPv4 16-bit total-length cap minus
   headers) via `capture.rs:78` `write_segmented`. The ring
   buffer's `build_frame` at `bugreport.rs:317` calls
   `build_tcp_frame` directly and unsegmented; on any SPICE
   message > 64 KB (display image data routinely qualifies),
   the defensive check at `build_tcp_frame:131` warns and
   returns `Vec::new()`, dropping the entry from the bug-report
   pcap. NOTES.md's jumbo-frame hypothesis was a coincidence —
   the limit is not MTU but IPv4's per-packet length field, and
   the fix is segmentation, not a bigger packet. No
   reproduction setup needed.

## Execution

Phases are listed in execution order. No phase has a hard
dependency on a phase listed below it, so working through the
table top-to-bottom is always safe.

| Phase | Plan | Status |
|-------|------|--------|
| 1. Auto-snapshot ring buffer at disconnect moment | PLAN-session-001-feedback-phase-01-disconnect-snapshot.md | Done |
| 2. Main-channel auto-reconnect / keepalive (originally framed as the K1 fix; K1 root cause is now fixed independently in `370d8ce5`. Phase 02 reframed as general-purpose disconnect/reconnect UX — steps 1, 2, 2b, 2c, 2e, 2f, 3 landed during the investigation; steps 4, 5, 6 landed as session-001-feedback follow-ups; step 7 is this doc wrap-up.) | PLAN-session-001-feedback-phase-02-reconnect.md | Done |
| 3. Preserve audio volume across reconnect | PLAN-session-001-feedback-phase-03-audio-volume.md | Not started |
| 4. Region-select zero-width guard | PLAN-session-001-feedback-phase-04-region-select.md | Not started |
| 5. Handle `STREAM_DESTROY_ALL` (display msg 126) | PLAN-session-001-feedback-phase-05-stream-destroy-all.md | Not started |
| 6. Rebalance per-channel ring-buffer split by expected traffic | PLAN-session-001-feedback-phase-06-channel-rebalance.md | Not started |
| 7. `Arc<[u8]>` refactor for `TrafficEntry::pcap_frame` | PLAN-session-001-feedback-phase-07-traffic-arc.md | Not started |
| 8. Segment large messages in ring-buffer frame builder | PLAN-session-001-feedback-phase-08-ring-segmentation.md | Not started |
| 9. Connection events in notifications pane (F1) | PLAN-session-001-feedback-phase-09-connection-notifications.md | Not started |
| 10. Notification → bug-report button (F2) | PLAN-session-001-feedback-phase-10-notification-bug-button.md | Not started |

Hard dependencies the order respects:

- **Phase 02 needs data from Phase 01** — *originally* the K1
  fix needed a real disconnect-moment pcap. With K1 now resolved
  independently of Phase 02's reconnect work (see resolution
  note above), this dependency is historical only. The
  remaining Phase 02 steps (channel-error attribution, reconnect
  state machine, console.vv extensions, docs wrap-up) do not
  depend on Phase 01 data.
- **Phase 08 benefits from Phase 07's `Arc<[u8]>` model** —
  segmenting into multiple entries is cleaner when each segment
  is a cheap `Arc`-shared chunk. Not strictly required, but
  doing 07 first avoids needing to revisit segmentation when 07
  later rewrites the entry shape.
- **Phase 10 needs cheap ring snapshots from Phase 07** — the
  notification-button UX (always-fileable with snapshot at fire)
  relies on cloning ring contents on every notification fire,
  which is only practical with `Arc<[u8]>` payloads.

Phases 3, 4, 5 are small standalone fixes and good warm-ups
between the data-gathering / fix cycle of 01–02 and the
infrastructure work of 06–10.

**Future work (out of scope for this plan):** dynamic ring-buffer
sizing that scales the byte cap to system memory at startup
(`min(total_ram * fraction, ceiling)`, with a 50 MB floor for
parity with today and a `RYLL_TRAFFIC_CAP_MB` override). Static
per-channel rebalancing in Phase 06 captures most of the
diagnostic value without the dynamic-shrink/grow complexity.
Revisit only if real-world reports show display-channel
retention is still too short to be useful after Phase 06.

Out of scope for this plan (tracked elsewhere):
G1 → `PLAN-macos-runtime-metrics.md`;
U1 → `PLAN-video-keeping-up.md`.

# Video stream capability expansion and flap diagnostics

## Prompt

Before responding to questions or discussion points in this
document, explore the ryll codebase thoroughly. Read relevant
source files, understand existing patterns (SPICE protocol
handling, channel architecture, async task model, image
decompression, egui rendering), and ground your answers in
what the code actually does today. Do not speculate about
the codebase when you could read it instead. Where a question
touches on external concepts (SPICE protocol, QEMU, QXL,
H.264 / VP8 codecs, LZ4 compression, vdagent), research as
needed to give a confident answer. Flag any uncertainty
explicitly rather than guessing.

All planning documents should go into `docs/plans/`.

Consult `ARCHITECTURE.md` for the system architecture
overview, channel types, and data flow. Consult `AGENTS.md`
for build commands, project conventions, code organisation,
and a table of protocol reference sources. Key references
include `shakenfist/kerbside` (Python SPICE proxy with
protocol docs and a reference client),
`/srv/src-reference/spice/spice-protocol/` (canonical SPICE
definitions), `/srv/src-reference/spice/spice-gtk/`
(reference C client), `/srv/src-reference/spice/spice-html5/`
(reference JS client), and `/srv/src-reference/spice/spice/`
(server-side SPICE in `server/`).

When we get to detailed planning, I prefer a separate plan
file per detailed phase. These separate files should be named
for the master plan, in the same directory as the master
plan, and simply have `-phase-NN-descriptive` appended before
the `.md` file extension. Tracking of these sub-phases should
be done via a table in this master plan under the Execution
section.

I prefer one commit per logical change, and at minimum one
commit per phase.

## Situation

Test session 002 (and the 002a follow-up that exercised the new
per-stream instrumentation landed on this branch) revealed that
playing video on a guest VM via the SPICE display channel from
macOS produces audible audio but only sporadic display updates.
The earlier sub-investigation, supported by:

- `traffic.pcap` showing long zero-bandwidth gaps from the server,
- the new per-stream counters in `DisplaySnapshot` showing
  `streams_created_total == streams_destroyed_total == 2` over a
  32 s session with `streams_active == []` at snapshot time,
- the `ryll-console-output.txt` log showing six `STREAM_CREATE`
  → `STREAM_DESTROY` cycles (each stream alive ~1–2 s, with
  10–15 s silence between cycles),

pins the symptom on the server's `RED_STREAM_TIMEOUT = 1 s`
(`/srv/src-reference/spice/spice/server/video-stream.h:34`)
combined with the guest producing frames in bursts rather than
continuously. The server creates an MJPEG stream when the
streaming-video heuristic fires, sends a burst, sees no new
frames for 1 s, and destroys the stream
(`server/video-stream.cpp:1031-1044`).

No client behaviour triggers the destruction — the relevant
server code paths are bound only to the timeout, surface
changes, and explicit teardown. The lifecycle is independent
of `STREAM_REPORT` (which only feeds adaptive bitrate in
`server/dcc.cpp:867-880`).

That said, ryll's display-channel capability set in
`shakenfist-spice-protocol/src/constants.rs:116-127` is
notably minimal compared to spice-gtk
(`/srv/src-reference/spice/spice-gtk/src/channel-display.c:975-993`)
and spice-html5 (`spice-html5/src/spiceconn.js:163-169`). We
advertise `SIZED_STREAM | MONITORS_CONFIG | COMPOSITE | A8_SURFACE`
only. We do not advertise:

| Cap | spice-gtk | spice-html5 |
|---|---|---|
| `STREAM_REPORT` (4) | ✓ when adaptive | always |
| `LZ4_COMPRESSION` (5) | ✓ | — |
| `PREF_COMPRESSION` (6) | ✓ | — |
| `MULTI_CODEC` (8) | ✓ | ✓ |
| `CODEC_MJPEG` (9) | ✓ (built-in) | always |
| `CODEC_VP8` (10) | ✓ (gstreamer) | ✓ (WebM) |
| `CODEC_H264` (11) | ✓ (gstreamer) | — |
| `PREF_VIDEO_CODEC_TYPE` (12) | ✓ | — |
| `CODEC_VP9` (13) | ✓ (gstreamer) | — |
| `CODEC_H265` (14) | ✓ (gstreamer) | — |

The most consequential gap is the codec set: without `MULTI_CODEC`
and `CODEC_H264` / `CODEC_VP8`, the server falls through to the
legacy MJPEG-only path
(`server/video-stream.cpp:813-816`). At 1600×1200 each MJPEG
frame is 150–300 KB; H.264 IDR ≈ 30 KB, P-frames a few KB. A
modern codec would also be more efficient on the server side
and may interact better with its stream-detection heuristic.

`openh264` is already a workspace dependency (Cargo.lock — pulled
in by `shakenfist-spice-webrtc`), so the H.264 path is not from
zero.

### On vdagent diagnostics

The spice in-guest agent protocol
(`/srv/src-reference/spice/spice-protocol/spice/vd_agent.h`) is
purely functional. Defined message types cover clipboard
(`VD_AGENT_CLIPBOARD*`), mouse state (`VD_AGENT_MOUSE_STATE`),
monitor configuration (`VD_AGENT_MONITORS_CONFIG`,
`VD_AGENT_DISPLAY_CONFIG`), file transfer
(`VD_AGENT_FILE_XFER_*`), audio volume sync
(`VD_AGENT_AUDIO_VOLUME_SYNC`), and graphics device
identification (`VD_AGENT_GRAPHICS_DEVICE_INFO`). **None
expose guest-side diagnostic information** — there is no opcode
for guest CPU/memory state, render-pipeline health, QXL driver
status, or per-process information. Guest-side troubleshooting
remains an SSH / virsh / host-metrics problem outside ryll's
scope.

What we *can* infer from the agent without protocol extensions:

1. **Connection state** — already surfaced via the `Guest agent
   connected` / `Guest agent disconnected` notifications.
2. **Agent responsiveness** — we could measure the latency of
   `VD_AGENT_REPLY` to `VD_AGENT_MONITORS_CONFIG` as a proxy for
   guest-side stall (a stuck guest typically delays REPLY).
3. **GraphicsDeviceInfo presence** — if the guest is sending
   `VD_AGENT_GRAPHICS_DEVICE_INFO` (cap `GRAPHICS_DEVICE_INFO`,
   bit 17), it has a working enumeration path to the device.

Of these, only the agent-responsiveness probe would add new
diagnostic signal. Worth a small follow-up after this plan;
not in scope here.

### Update from test session 002b (post-phase-1)

The first dogfood session under the new STREAM_REPORT
instrumentation (`test-session-002b`, ryll commit `241ba13f`)
ran against a larger 2048×1152 instance and surfaced a
distinct symptom: drag/resize gestures are unresponsive even
without video playback. The per-stream snapshot data this
phase-1 work added pins the cause: MJPEG decode in the
pure-Rust `jpeg-decoder` crate takes 76–175 ms per frame at
2048×1152, so frames arrive late
(`last_report_last_frame_delay: -142 to -504 ms` across eight
of ten destroyed streams), the spice-server's streaming
heuristic loses confidence, and the stream gets destroyed
within 2 seconds. During the long gaps between streams, the
screen visibly freezes. Network is clean (0 retransmits,
27 KB average packet size), client CPU is idle (process 0%),
and the user's host-side `top` reading on sf-4 agreed —
nothing is CPU-bound at the wire level.

This pushed a new piece of work into the plan: a per-platform
JPEG decoder selector (phase 3, ahead of the H.264 work in
phase 4) so we close the decode-latency gap on every platform
before adding more codecs on top.

## Mission and problem statement

Implement four SPICE display capabilities that ryll should
advertise and use, replace the pure-Rust MJPEG decoder with a
per-platform optimal selector (driven by session 002b's
finding above), and add a UI signal that surfaces the
spice-server's stream-flapping behaviour to operators triaging
"video doesn't play" reports.

In priority order:

1. **`STREAM_REPORT` (cap 4)** — receive `STREAM_ACTIVATE_REPORT`,
   send periodic `SPICE_MSGC_DISPLAY_STREAM_REPORT` (display-client
   opcode 102) so the server's encoder gets adaptive-bitrate
   feedback. Mostly counters we already have in `StreamState`.
2. **`LZ4_COMPRESSION` (cap 5)** — advertise, accept LZ4-encoded
   images from the server. Modest bandwidth/CPU win on the
   static-UI Zlib/GLZ path. `lz4_flex` or similar is the
   obvious decoder choice.
3. **Fast per-platform MJPEG decode** — replace the pure-Rust
   `jpeg-decoder` crate with a runtime-selected best-of-breed
   decoder per platform (ImageIO on macOS, WIC on Windows,
   VA-API on Linux with vendored libjpeg-turbo as the always-
   available baseline, pure-Rust as the universal fallback).
   Inserted ahead of the codec work after session-002b
   evidence that pure-Rust JPEG decode at 2048×1152 is the
   dominant client-side bottleneck.
4. **`MULTI_CODEC` + `CODEC_MJPEG` + `CODEC_H264` (caps 8/9/11)**
   — advertise multi-codec, keep MJPEG as fallback, decode H.264
   stream data via `openh264` (already in tree). Significantly
   reduces bandwidth and may stabilise the server's stream
   lifecycle for video workloads.
5. **`PREF_COMPRESSION` + `PREF_VIDEO_CODEC_TYPE` (caps 6/12)** —
   send the corresponding preference messages once on link-up so
   the server picks the codec/compression we prefer. Cheap once
   the multi-codec path exists.

And one UI feature:

6. **Stream-flap notification** — detect rapid create/destroy
   cycles in the per-stream snapshot data we just landed, and
   raise a one-shot notification (in the existing
   `NotificationStore`) so the operator knows the SPICE server
   is bouncing the video stream. Include the relevant counts and
   a hint that this typically indicates guest/server rather than
   client trouble.

## Open questions

These should be resolved before, or during, the relevant phase.
Items marked **decide now** belong in this master plan; items
marked **decide in phase** belong in the phase plan.

1. **(decide now) Codec order: H.264 first, both H.264 and VP8,
   or VP8 first?** `openh264` is already pulled in; adding it
   adds no new dependencies. VP8 would require `libvpx` or a
   pure-Rust decoder (none mature). **Recommendation: H.264
   only in phase 4; defer VP8/VP9/H265 to follow-up.** Capture
   that explicitly in the plan.

2. **(decide now) Are we OK adding `openh264` as a runtime
   dependency for the client binary?** It already ships through
   the `--web` path but the GUI client doesn't link it today.
   **Recommendation: yes** — it's already an audit-clean
   dependency in the workspace.

3. **(decide in phase 1) STREAM_REPORT timing.** spice-gtk sends
   when `num_frames >= max_window_size` (5), OR
   `timeout_ms` (1000) elapsed since last report, OR three
   consecutive drops. The fields and cadence are well-defined
   in `/srv/src-reference/spice/spice-gtk/src/channel-display.c:1534-1589`
   — we should match. **Decision: mirror spice-gtk semantics
   exactly.**

4. **(decide in phase 2) Where in the image-decode dispatch does
   `LZ4` insert?** Our image-type dispatch lives in
   `shakenfist-spice-renderer/src/channels/display.rs` and the
   `shakenfist-spice-compression` crate. The phase plan needs to
   identify the right hook.

5. **(decide in phase 4) Where does H.264 decode run — inline in
   the display-channel task, or on a dedicated `spawn_blocking`
   task like the encoder?** H.264 decode is meaningfully heavier
   than MJPEG. Inline would simplify the data flow; offloaded
   would protect the channel task from stutter. Investigate in
   the phase plan.

6. **(decide in phase 6) Flap-detection heuristic.** Candidates:
   "N streams destroyed in M seconds with mean lifetime < T"
   (e.g. ≥3 destroys in 30 s, mean lifetime < 3 s). The phase
   plan picks the constants and the cool-down period for the
   notification. **Starting point: ≥3 destroys in 30 s window
   with mean lifetime < 3 s, one-shot per 60 s cool-down.**

7. **(open) Should we cross-validate against virt-viewer before
   shipping H.264?** Yes — see `Future work`. The phase 4 plan
   should call for a manual test against the same VM under both
   ryll and virt-viewer to confirm the flap pattern is or isn't
   shared. Cheap and informative.

## Execution

Eight phases, sequenced so cheap-and-independent work lands
first; per-platform decoder work lands before multi-codec so
the JPEG decode floor is healthy before we add H.264; vdagent
probe is independent and sits late so the documentation phase
covers it. Phase 3 (fast JPEG decode) was inserted after
session 002b showed that MJPEG decode in the pure-Rust
`jpeg-decoder` crate is the dominant bottleneck on macOS at
2048×1152 (76–175 ms per frame).

| Phase | Plan | Status |
|-------|------|--------|
| 1. STREAM_REPORT | PLAN-stream-caps-and-flap-phase-01-stream-report.md | Complete |
| 2. LZ4 compression | PLAN-stream-caps-and-flap-phase-02-lz4.md | Code landed; awaiting smoke test (2C) |
| 3. Fast JPEG decode | PLAN-stream-caps-and-flap-phase-03-jpeg-decoders.md | Planned |
| 4. Multi-codec + H.264 | PLAN-stream-caps-and-flap-phase-04-h264.md | Not started |
| 5. Preference messages | PLAN-stream-caps-and-flap-phase-05-pref-messages.md | Not started |
| 6. Flap notification | PLAN-stream-caps-and-flap-phase-06-flap-notification.md | Not started |
| 7. Vdagent responsiveness probe | PLAN-stream-caps-and-flap-phase-07-vdagent-probe.md | Not started |
| 8. Documentation | PLAN-stream-caps-and-flap-phase-08-docs.md | Not started |

Per-phase intent:

- **Phase 1 — STREAM_REPORT.** Add `display_client::STREAM_REPORT
  = 102`, capability advertisement bit 4, handler for
  `STREAM_ACTIVATE_REPORT` that captures
  `(stream_id, unique_id, max_window_size, timeout_ms)` into the
  matching `StreamState`, and a small ticker that emits
  `SpiceMsgcDisplayStreamReport` on the cadence rules. Reuse the
  per-stream counters already in `StreamState`. Add fields to
  `StreamSnapshot` for `last_report_*` so a bug report shows
  whether we ever sent one. Verify against
  spice-gtk's `display_update_stream_report` for field semantics.
  Recommended planning effort: **high** (the spec needs to be
  read carefully; field semantics matter).

- **Phase 2 — LZ4_COMPRESSION.** Advertise cap 5. Wire LZ4
  decoding into the image-decode dispatch (server may now send
  images with the LZ4 type). Decompressor crate selection:
  `lz4_flex` is pure-Rust and audit-clean. Unit tests with
  vectors from `spice-common` if available, or round-trip
  encoded by the same library. Recommended planning effort:
  **medium** (well-defined; the only judgment call is hook
  placement).

- **Phase 3 — Fast JPEG decode.** Replace the pure-Rust
  `jpeg-decoder` crate (currently called from
  `shakenfist-spice-renderer/src/channels/display.rs::decode_mjpeg_frame`)
  with a platform-optimal selector chain: ImageIO on macOS,
  WIC on Windows, VA-API (dlopen-probed) on Linux with
  vendored libjpeg-turbo (`mozjpeg` crate) as the always-
  available baseline, and pure-Rust as the universal
  fallback. Driven by session 002b's finding that MJPEG
  decode at 2048×1152 takes 76–175 ms in the pure-Rust path,
  causing frames to arrive late, the spice-server's
  streaming heuristic to lose confidence, and the user to
  see frozen displays between streams. New `JpegDecoder`
  trait + `best_for_platform()` selector in
  `shakenfist-spice-compression`; per-stream
  `mjpeg_decoder_backend` and aggregate
  `mjpeg_decode_recent_*` fields in bug reports. Recommended
  planning effort: **high** (cross-platform, four backend
  implementations, COM threading on Windows, dlopen + JPEG
  header parsing for VA-API).

- **Phase 4 — Multi-codec + H.264.** Advertise caps 8 (MULTI_CODEC),
  9 (CODEC_MJPEG), and 11 (CODEC_H264). Hook H.264 decoding into
  the existing `STREAM_DATA` / `STREAM_DATA_SIZED` path keyed on
  `StreamState::codec_type`. Use `openh264` (already in
  Cargo.lock) for the decoder. Reuse all of the per-stream
  instrumentation we already have. Decide on inline vs offload
  during the phase plan (see open question 5). Important: keep
  MJPEG as the fallback so a server that rejects multi-codec
  still works. Recommended planning effort: **high** (decoder
  threading, codec-specific framing, and the first time we add
  a video codec to the GUI binary).

- **Phase 5 — Preference messages.** Add `display_client::PREFERRED_COMPRESSION`
  (opcode 103) and `display_client::PREFERRED_VIDEO_CODEC_TYPE`
  (opcode 104). Advertise caps 6 and 12. Send the preference
  messages once on link establishment. spice-gtk does this in
  `channel-display.c` near the init handler. Recommended
  planning effort: **medium** (mechanical once the cap plumbing
  is in place from earlier phases).

- **Phase 6 — Flap notification.** Add a small per-channel
  watcher (likely a tokio task or a tick inside
  `update_snapshot`) that examines the `streams_recently_destroyed`
  ring. If ≥3 streams destroyed in the last 30 s with mean
  lifetime < 3 s, push a `NotifySeverity::Warn` notification
  via `push_notification` with `NotificationSource::Internal`
  (one-shot, 60 s cool-down) saying something like
  "Server is rapidly creating and tearing down video streams
  ({N} cycles in {Ms}, mean lifetime {Ts}); this usually means
  the guest is producing frames in bursts." Also expose the
  flap state via a transient annotation in the stats panel so
  the user can see it without waiting for the notification
  cool-down. Recommended planning effort: **medium** (the
  heuristic is well-defined; UI integration follows existing
  notification patterns).

- **Phase 7 — Vdagent responsiveness probe.** The spice in-guest
  agent has no diagnostic message types of its own (see the
  *On vdagent diagnostics* note in `Situation`), but two
  client → agent messages are acknowledged by `VD_AGENT_REPLY`:
  `VD_AGENT_MONITORS_CONFIG` (Linux + Windows agents) and
  `VD_AGENT_DISPLAY_CONFIG` (Windows only). `VDAgentReply` is
  `{ uint32 type, uint32 error }` where `type` echoes the
  request opcode, so we can correlate replies to requests.

  Mechanism: instrument the existing send/receive path for
  `VD_AGENT_MONITORS_CONFIG` (we already send this on window
  resize and at session start) to record send-timestamp and
  reply-lag. Add an idle probe that re-sends the current
  monitors config every N seconds when no other monitors
  config has been sent for a while — the guest should treat
  an identical config as a no-op, and if it doesn't, that's
  itself diagnostic. Surface on `MainSnapshot`:

  - `agent_request_count: u32` — outbound MONITORS_CONFIG sends
  - `agent_reply_count: u32` — `VD_AGENT_REPLY` messages received
  - `agent_reply_error_count: u32` — replies with `error != VD_AGENT_SUCCESS`
  - `last_agent_reply_ts_secs: Option<f64>`
  - `last_agent_reply_lag_us: u32`
  - `recent_agent_reply_lag_us: VecDeque<u32>` — bounded ring
    (cap ≈ 16) for min/max/mean
  - `outstanding_agent_request_count: u32` — sends without
    matching reply yet (informational; high values suggest
    a stuck agent)

  Optional UI: raise a `NotifySeverity::Warn` notification if
  `outstanding_agent_request_count > 0` for more than 5 s
  after a probe send. Mirror the cool-down pattern from
  phase 6 to avoid noise. Recommended planning effort:
  **medium** (small surface area; the only judgment call is
  probe cadence and the no-op assumption).

- **Phase 8 — Documentation.** Update `ARCHITECTURE.md`
  capability tables, `AGENTS.md` reference list if a new
  external ref was added, `README.md` if user-visible behaviour
  changed, and add a "video troubleshooting" section to
  `docs/troubleshooting.md` that explains the flap notification,
  the vdagent probe fields, and links to the bug-report fields
  a user should attach. Recommended planning effort: **low**.

## Agent guidance

### Execution model

All implementation work is done by sub-agents, never
in the management session. The management session (this
conversation) is reserved for planning, review, and
decision-making. This keeps the management context lean
and avoids drowning it in implementation diffs.

The workflow is:

1. **Plan** at high effort in the management session.
2. **Spawn a sub-agent** for each implementation step
   with the brief from the plan, at the recommended
   effort level and model.
3. **Review** the sub-agent's output in the management
   session. Check the actual files — the sub-agent's
   summary describes what it intended, not necessarily
   what it did.
4. **Fix or retry** if the output is wrong. Diagnose
   whether the brief was insufficient (improve it) or
   the model was too light (upgrade it), then re-run.
5. **Commit** once the management session is satisfied
   with the result.

This applies to all steps, including high-effort ones.
If a sub-agent can't succeed even with a detailed brief
and the right model, that's a signal the brief needs
improving, not that the management session should do
the implementation itself.

Use `isolation: "worktree"` for sub-agents when the
change is risky or experimental. The worktree is
discarded if the output is unsatisfactory. For safe,
well-understood changes, sub-agents can work directly
in the main tree.

### Planning effort

Phase plans should be created at the effort level recommended
in the phase summary above. Most of this plan's phases are
high or medium effort; phase 8 is low.

### Step-level guidance

Each phase plan should include a step table:

```
| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
| 1a   | medium | sonnet | none     | One-sentence summary of what to do and which files to touch |
| 1b   | high   | opus   | worktree | Why this needs high effort: requires understanding X to do Y |
```

The model choice (opus / sonnet / haiku) should reflect the
quality of the brief and the complexity of the change. Briefs
that front-load research the planner already did allow lighter
models to succeed.

### Management session review checklist

After a sub-agent completes, the management session
should verify:

- [ ] The files that were supposed to change actually
      changed (read them, don't trust the summary).
- [ ] No unrelated files were modified.
- [ ] The code builds (`pre-commit run --all-files` or
      equivalent).
- [ ] Tests pass (`make test` for ryll).
- [ ] The changes match the intent of the brief — not
      just syntactically correct but semantically right.
- [ ] Commit message follows project conventions
      (including the Co-Authored-By line with model,
      context window, effort level, and other settings).

## Administration and logistics

### Success criteria

We will know this plan has been successfully implemented when
all of the following are true:

* `pre-commit run --all-files` is clean (rustfmt, clippy with
  `-D warnings`, shellcheck, secret/unicode scanners).
* `make test` passes; new logic has unit tests.
* `make build` and `make release` both succeed (verifies the
  H.264 dependency is correctly wired in the GUI binary, not
  just the WebRTC crate).
* ryll's display-channel capability advertisement includes,
  at minimum, the four new caps (`STREAM_REPORT` (4),
  `LZ4_COMPRESSION` (5), `MULTI_CODEC` (8), `CODEC_MJPEG` (9),
  `CODEC_H264` (11), `PREF_COMPRESSION` (6),
  `PREF_VIDEO_CODEC_TYPE` (12)) per the priority list above.
* `STREAM_ACTIVATE_REPORT` from the server triggers periodic
  `STREAM_REPORT` replies whose contents match spice-gtk's
  semantics. The reports are visible in `channel-state.json`
  (new per-stream `last_report_*` fields).
* `shakenfist-spice-compression::jpeg::best_for_platform()`
  selects ImageIO on macOS, WIC on Windows, VA-API (when
  available) or libjpeg-turbo on Linux. The active backend is
  visible in `channel-state.json::streams_active[*].mjpeg_decoder_backend`,
  and `mjpeg_decode_recent_mean_us` is well under the prior
  pure-Rust baseline on each platform (target ≤30 ms at
  2048×1152 on macOS Apple Silicon).
* The server can negotiate H.264 stream encoding with ryll;
  H.264 stream_data frames are decoded and painted with
  per-stream counters incrementing in line with frames_received.
* When the spice-server flap pattern (≥3 destroys / 30 s, mean
  lifetime < 3 s) is observed, a `NotifySeverity::Warn`
  notification fires once per 60 s cool-down and includes the
  observed counts.
* `MainSnapshot` carries vdagent reply-lag counters
  (`agent_request_count`, `agent_reply_count`,
  `last_agent_reply_lag_us`, `recent_agent_reply_lag_us`,
  `outstanding_agent_request_count`), populated whenever the
  guest agent is connected, and visible in `channel-state.json`.
* `ARCHITECTURE.md`, `AGENTS.md`, `README.md`, and
  `docs/troubleshooting.md` reflect the new caps, the flap
  notification, and the vdagent probe.
* Lines wrapped at 120 chars; Rust strings use single quotes
  where applicable; trailing whitespace trimmed.

### Future work

Items deliberately deferred from this plan:

* **VP8 / VP9 / H.265 codec support.** Lower expected value
  than H.264 once that is in. Reconsider if the H.264 path is
  consistently chosen by the server but a workload (e.g. an
  H.265-only camera feed) shows up.
* **Stream-flap heuristic tuning.** Phase 6 starts with the
  ≥3-in-30 s rule; we may want to revisit constants once we
  have field experience.
* **Vdagent probe heuristic tuning.** Phase 7 starts with a
  30 s probe cadence and a 5 s outstanding-reply timeout; the
  right values depend on what we see in the field.
* **`GL_SCANOUT` cap.** Only useful if we add a zero-copy GL
  surface path. Not on the roadmap.

### Bugs fixed during this work

(Populated during execution.)

### Documentation index maintenance

When this master plan lands:

* **`docs/plans/index.md`** — add a row to the *Master plans*
  table with the creation date, link, intent summary, status
  (`In progress`), and links to each phase plan as they are
  written.
* **`docs/plans/order.yml`** — add an entry
  `- PLAN-stream-caps-and-flap.md: Stream caps and flap diagnostics`.

When all phases complete, flip the index row's status to
`Complete`.

### Back brief

Before executing any step of this plan, please back brief
the operator as to your understanding of the plan and how
the work you intend to do aligns with that plan.

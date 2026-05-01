# Web Frontend for Ryll (SPICE → Browser Transcoder)

> **Status: concept plan (thought bubble).** This is an exploratory
> design document. Phase files have not been written and no
> implementation work has begun. The execution table below is
> provisional. Treat the open questions as blockers to firming up
> the phase plans.

## Prompt

Before responding to questions or discussion points in this
document, explore the ryll codebase thoroughly. Read relevant
source files, understand existing patterns (SPICE protocol
handling, channel architecture, async task model, image
decompression, the software framebuffer in `src/display/surface.rs`,
egui rendering, audio playback via cpal, headless mode), and
ground your answers in what the code actually does today. Do
not speculate about the codebase when you could read it
instead. Where a question touches on external concepts (SPICE
protocol, QEMU, QXL, TLS/RSA, LZ/GLZ compression, WebRTC,
H.264/VP8/AV1 encoding, browser media APIs, openh264, x264,
webrtc-rs, rav1e, MediaSource Extensions, Web Codecs API),
research as needed to give a confident answer. Flag any
uncertainty explicitly rather than guessing.

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
(the existing JavaScript browser client; useful for what *not*
to do as much as for prior art on the inputs/scancode mapping),
`/srv/src-reference/qemu/qemu/` (server-side SPICE in
`ui/spice-*`), and the existing `--capture` video-encode path
in `ryll/src/capture.rs` (already uses `openh264` and `mp4` and
is the closest in-tree precedent for an encoder pipeline).

When we get to detailed planning, I prefer a separate plan
file per detailed phase. These separate files should be named
for the master plan, in the same directory as the master
plan, and simply have `-phase-NN-descriptive` appended before
the `.md` file extension. Tracking of these sub-phases should
be done via the table in the Execution section of this
document.

I prefer one commit per logical change, and at minimum one
commit per phase. Do not batch unrelated changes into a
single commit. Each commit should be self-contained: it
should build, pass tests, and have a clear commit message
explaining what changed and why.

## Situation

The motivating use case is desktop access from a web browser.
Today the operator runs Kasm Workspaces, which exposes a
Linux desktop session over RDP and uses Apache Guacamole to
transcode RDP into an HTML5 canvas with audio. Kasm works
fine, but it is a third-party stack: every desktop session
goes through RDP (foreign to the shakenfist universe) and
Guacamole (a Java service the operator does not otherwise
use). If ryll could play the same role for SPICE, the
operator could:

1. Run an `xspice` (or QEMU+SPICE) session on the dev desktop.
2. Point a ryll-flavoured transcoder at it.
3. Open the URL the transcoder prints to its console in
   any browser and get the same experience as Kasm —
   keyboard, mouse, audio, over the LAN or VPN.

The result is a fully shakenfist-native VDI loop: SPICE end
to end, with the browser as just another ryll display target.

### What ryll already gives us

The decode side of a SPICE→browser transcoder is essentially
done. Specifically:

- **Software framebuffer.** `DisplaySurface` in
  `ryll/src/display/surface.rs:13` owns an RGBA `Vec<u8>` and
  every SPICE draw op (`blit`, `fill_rect`, `copy_bits`,
  `blit_alpha`, `blit_chroma`, `invert_rect`, `fill_solid`)
  mutates that buffer directly in software. egui's role
  reduces to wrapping the buffer in a `ColorImage` at
  `surface.rs:465` and painting one textured quad per
  surface. The framebuffer is the universal substrate
  today; egui is one consumer of it. Adding a video encoder
  as a *second* consumer is the entire architectural move.
- **Dirty tracking.** `DisplaySurface::is_dirty()` already
  exists at `surface.rs:499`. Per-rectangle dirty regions
  are not tracked yet but are a small extension of the
  existing draw-op call sites; this matters for partial-frame
  encodes.
- **Multi-surface, multi-monitor.** Multiple `DisplaySurface`
  instances are already maintained, indexed by surface id, and
  the `--monitors N` flag drives multi-head configurations.
  Each maps cleanly to a separate video track.
- **Audio pipeline.** `ryll/src/channels/playback.rs` already
  decodes the SPICE playback channel (raw PCM and Opus) and
  feeds a cpal output stream via a lock-free ring buffer. For
  WebRTC the Opus packets can be forwarded with no transcoding;
  for an MSE/Web Codecs path they would need re-containering
  but not re-encoding.
- **Inputs channel.** `ryll/src/channels/inputs.rs` already
  marshals SPICE keyboard scancodes and pointer events. The
  transcoder needs only to translate browser
  `KeyboardEvent.code` / `MouseEvent` into the same
  intermediate form ryll's egui frontend already produces.
  The paste-as-keystrokes US-QWERTY scancode table is the
  closest in-tree precedent.
- **Headless mode.** Already used for cadence/automated
  testing. Proves the SPICE stack runs without an attached
  GUI; the web transcoder is "headless mode plus an encoder
  plus a browser-facing transport". The mere existence of
  headless mode is also evidence that ryll has been
  architected to support more than one frontend, which
  makes the web frontend a natural extension rather than a
  retrofit (see *Design philosophy* below).
- **Reconnect.** The reconnect lifecycle introduced for the
  GUI applies equally to a web session — a transcoder
  process can drop per-session state and re-handshake without
  exiting.
- **`--capture` precedent.** `ryll/src/capture.rs` already
  pulls the dirty framebuffer through `openh264` into an
  `mp4` file. This is the closest existing analogue to the
  encoder half of the web pipeline; lessons (frame pacing,
  encoder lifecycle, Cargo feature gating) carry over.

### What is missing

1. **A video encoder running in real time, driven by
   framebuffer mutation events instead of a wall-clock
   timer.** `--capture` writes a file at fixed cadence;
   live streaming wants something closer to "encode on
   dirty, pace at 30–60 fps, force a keyframe on
   request".
2. **A browser-facing transport.** WebRTC, WebSocket+MSE,
   WebSocket+Web-Codecs, or WebTransport. Each has a
   different latency / complexity / browser-compat profile;
   see open question (3).
3. **A browser shell.** Static HTML+JS that establishes the
   transport, attaches the video to a `<video>` element (or
   `<canvas>` via Web Codecs), captures keyboard/mouse, and
   relays them back. Small but real piece of work; can be
   served by the same Rust binary.
4. **A small HTTP server.** Serves the browser shell and
   the signalling endpoint. Plain HTTP for MVP — the
   browser only *receives* media (no `getUserMedia`), and
   `RTCPeerConnection`, keyboard, and pointer events all
   work in non-secure contexts. HTTPS is deferred until a
   feature that demands a secure context lands (clipboard
   sync, Pointer Lock on Chrome — see open question (9)).
5. **Reusable channel-handling code.** `DisplaySurface`,
   the channel handlers in `ryll/src/channels/`, and the
   per-session orchestration today live inside the ryll
   binary crate. To build a separate `ryll-web` binary
   without dragging in egui/eframe, that code needs to be
   either extracted into a new library crate or hidden
   behind cargo features. See open question (1).

### Design philosophy: ryll is a multi-modal SPICE client

This plan formalises something that has been implicit in the
codebase since headless mode was introduced: **ryll is
intended to be a multi-modal SPICE client, not a
GUI-with-a-test-harness.** The ambition is that every
delivery mode is a first-class citizen and shares as much
functionality as the mode itself can physically support.

After this plan lands the supported modes are:

| Mode | Frontend | Primary use |
|------|----------|-------------|
| GUI | egui / eframe desktop window | Interactive day-to-day VDI access from the operator's own machine |
| Headless | none (stdout + metrics) | Automated testing, CI, cadence latency probing, scripted USB / WebDAV scenarios |
| Web (planned) | Browser via WebRTC | Interactive VDI access from any browser on the LAN, replacing Kasm + Guacamole |

The implication for design and review work going forward
is that **a feature is not "done" when it works in the GUI**;
it should also be reachable from headless and (once it
exists) from the web frontend, modulo features that are
intrinsic to one mode (e.g. egui-specific UI panels, or
browser-only clipboard APIs). When a feature *cannot*
exist in a given mode, the docs should say so explicitly
rather than leaving the gap unstated.

Today's codebase does not actually live up to this rule:
many features grew up GUI-first and have only partial (or
no) headless equivalents. The web frontend cannot meaningfully
be planned without first knowing where the GUI/headless
parity already drifts — otherwise the web frontend will just
inherit those gaps silently. Hence the dedicated audit
phase below (Phase 0). The audit produces a feature × mode
matrix (GUI / headless / web-planned) that:

- becomes input to the web-frontend phase plans (so each
  feature is delivered to the web alongside any catch-up
  work the other modes need);
- doubles as the to-do list for an independent
  **driving-down-the-gaps** workstream that does not need
  to wait for the web frontend to land.

Concrete consequences for this plan:

- **Phase 0 (parity audit) runs first.** Its output is a
  read-only artifact (`docs/multi-mode-parity.md`) that the
  rest of the plan depends on. Gap-closing work spawned by
  the audit is tracked outside this plan as its own
  follow-on plan(s); this plan does not absorb that scope.
- **Phase 1 (renderer extraction) is non-negotiable.** It
  turns "GUI vs headless" from a top-level branch in
  `main.rs` into a thin frontend layered over a shared
  library. Once extracted, the web frontend becomes "third
  consumer of the same library" rather than "second copy
  of the channel handlers".
- **Feature parity is a planning input, not an
  afterthought.** Each phase plan should explicitly call
  out which features it adds to the web frontend, and
  whether the GUI and headless paths need follow-up work
  to retain or regain parity. The Phase 0 matrix is the
  reference.
- **Documentation always names the mode.** Feature lists
  in the README, ARCHITECTURE, and AGENTS files should
  identify which modes a feature is available in, so the
  parity gaps are visible to operators and contributors.

### Why not something else

- **Apache Guacamole does not support SPICE** and never has.
  It supports RDP, VNC, SSH, telnet, and Kubernetes consoles.
  SPICE has been an open feature request on its issue
  tracker for years with no upstream interest. Adding SPICE
  to Guacamole means writing a new protocol module in Java
  for the `guacd` daemon, which is roughly the same scope
  as the work proposed here, in a stack the operator does
  not otherwise touch.
- **`spice-html5`** (the canonical JavaScript SPICE client at
  `/srv/src-reference/spice/spice-html5/`) is essentially
  unmaintained. It implements only a subset of the protocol
  in JavaScript — no audio, no Opus, no LZ4, no QUIC, no
  modern QXL draw ops (`DRAW_COMPOSITE` etc.), marginal
  cursor handling, no USB redirection. Bringing it up to
  parity with ryll means reimplementing in JS most of what
  ryll already does in Rust, with the result still running
  the heavy decode in a browser tab. The decode side has
  been done once; doing it again in a slower language is
  a poor trade.
- **VNC.** Cleanest native browser story (noVNC is mature),
  but VNC drops audio, USB redirection, and shared folders,
  and the operator's desktops already speak SPICE.
- **RDP via xrdp.** Works today via Kasm/Guacamole. Adopting
  SPICE end-to-end is the *whole point*: dogfood ryll, drop
  the Java service, keep one protocol family.

### Operational shape

Initial deployment is single-user, single-session, LAN
(or Tailscale) only:

- One `ryll-web` process runs on the desktop being shared.
- It is launched the same way `ryll` is today — a `.vv`
  file (or CLI flags) points at the SPICE server, so the
  operator-side connection is already authenticated.
- It listens for one browser at a time on plain HTTP, on a
  random ephemeral port chosen at launch. The full URL
  (`http://<host>:<port>/`) is printed to stdout, the same
  way `jupyter notebook` advertises itself.
- There is **no** browser-side authentication and **no**
  TLS in MVP. The threat model is "the operator runs this
  on a trusted LAN and copies the URL into their own
  browser"; this is comparable to leaving an unauthenticated
  X server or a `python -m http.server` running on the LAN.
  HTTPS, per-launch tokens, and a login UI are all
  explicitly future work — see open questions (8) and (9).
- Browsers will refuse a couple of nice-to-have APIs over
  plain HTTP (Pointer Lock on Chrome requires a secure
  context, async clipboard requires a secure context), but
  none of those are MVP features.

Multi-user / multi-session / TURN-relay / hosted-fleet
shapes are explicitly future work — they are interesting
but they are *also* the place Kasm is currently strong, and
chasing them now would distort the MVP.

## Mission and problem statement

Produce a `ryll-web` binary (and accompanying browser shell)
that lets the operator launch ryll-web with a `.vv` file
(exactly like ryll today), copy the printed
`http://<host>:<port>/` URL into a modern browser, and
interact with the SPICE-attached desktop with parity to
the current ryll desktop client for the basics: display,
audio, keyboard, mouse.

MVP scope:

1. New `ryll-web` binary in the workspace, builds on Linux
   x86_64 with the existing toolchain.
2. Connects to a SPICE server using the same `.vv` /
   CLI-flag plumbing as `ryll`. The `.vv` file is consumed
   by `ryll-web` at launch — the browser never sees it.
3. Listens on plain HTTP on an ephemeral port chosen at
   launch and prints the full URL to stdout. Serves a
   static HTML+JS shell from that endpoint.
4. Streams the SPICE display to the browser as a single
   video track (one monitor for MVP), pacing at up to 60
   fps with frame-skip when the framebuffer has not
   changed.
5. Streams SPICE audio to the browser, ideally by
   forwarding Opus packets without re-encoding.
6. Captures keyboard and mouse in the browser and
   forwards them through the SPICE inputs channel with
   correct US-QWERTY scancodes for the ASCII range and
   the common navigation keys (arrows, F-keys, Esc, Tab,
   Backspace, Enter, modifiers).
7. Reconnect-on-disconnect: if the browser tab closes or
   the PeerConnection drops, re-opening the same URL
   resumes against the same SPICE session.
8. Documents how to launch ryll-web from a `.vv` file and
   open the printed URL.

Out of MVP scope (tracked in Future work):

- Multi-monitor (one video track in MVP; multi-track is
  natural extension).
- USB redirection (browser USB story is fragile and
  Chrome-only).
- Folder sharing (the WebDAV channel as ryll uses it
  shares a *local* directory with the guest; "local" in a
  browser is ambiguous).
- Clipboard sync between browser and guest (vdagent
  clipboard).
- Hardware-accelerated encoding (NVENC / QSV / VAAPI).
- Multi-tenant / hosted / multi-session fleet.
- TURN servers / WAN traversal beyond what STUN gets us.
- TLS / HTTPS for the browser-facing endpoint.
- Per-launch URL tokens, login UI, OIDC, mTLS, or any
  other browser-side authentication.
- Mobile-browser UX polish (touch gestures, on-screen
  keyboard).
- Recording / capture from the web side.

## Open questions

Each of these needs to be resolved before the corresponding
phase plan can be written.

1. **Crate layout.** `DisplaySurface` and the channel
   handlers currently live in the `ryll` binary crate. Three
   options:
   - **(a) Extract a new `shakenfist-spice-renderer` library
     crate** containing `DisplaySurface`, the channel
     handler shells, and the per-session orchestration, with
     no UI dependencies. `ryll` and `ryll-web` both depend on
     it. Cleanest long-term, biggest up-front churn. Aligns
     with the existing `shakenfist-spice-{protocol,
     compression, usbredir}` extraction precedent.
   - **(b) Make egui/eframe optional cargo features** on the
     existing `ryll` crate and add `ryll-web` as a second
     binary in the same crate. Less churn, but couples web
     and desktop builds and forces every contributor to
     reason about feature combinatorics.
   - **(c) Copy what we need** into a new `ryll-web` crate
     and accept duplication. Fastest to MVP, worst for
     long-term maintenance.
   **Proposed: (a)**, in a dedicated extraction phase before
   any web-specific work begins. Confirm by trying (a) on
   `DisplaySurface` alone and seeing whether the channel
   handler split is as clean as it looks.

2. **Encoder choice.**
   - **`openh264`** is already a dependency (capture mode);
     pure-Rust bindings, BSD-licensed, software-only, well
     understood, ~5–10 ms / 1080p frame at "ultrafast"
     equivalents. **Proposed default for MVP.**
   - **`x264` via FFI** is faster and tunable but adds a C
     dep we have so far avoided.
   - **VP8** is the lingua franca of WebRTC and is what
     `webrtc-rs` ships with by default; `libvpx` bindings
     exist (`vpx-encode`) but are less tested. Worth
     considering if WebRTC is the chosen transport — it
     keeps the pipeline pure-Rust and matches what
     browsers SDP-negotiate by default.
   - **AV1** (`rav1e`) is pure Rust and produces beautiful
     video but is too slow for live encode at desktop
     resolutions today. Out of MVP.
   - **Hardware encoders** (NVENC / QSV / VAAPI) are out of
     MVP — not portable across the operator's machines.

3. **Transport.** The choice cascades through everything
   downstream. Three serious candidates:
   - **WebRTC (`webrtc-rs`).** Lowest latency (~30–80 ms),
     native browser support, audio+video+datachannel in one
     PeerConnection, Opus passthrough is trivial. But:
     ICE / SDP / DTLS / SRTP machinery is operationally
     heavy, even on a LAN; debugging is harder; the
     `webrtc-rs` crate is real but not as battle-tested as
     `libwebrtc`.
   - **WebSocket + Web Codecs API.** Send H.264 NAL units or
     VP9 frames as binary WS messages; browser decodes via
     `VideoDecoder`. Latency similar to WebRTC, transport
     is trivial, but Web Codecs is Chrome/Edge/Safari only
     (Firefox shipped it in 2025 but coverage is uneven
     for hardware decode). No built-in audio sync — we
     have to stitch that ourselves.
   - **WebSocket + MSE (Media Source Extensions).** Send
     fragmented MP4 chunks; browser plays via `<video>`.
     Universal browser support but ~300–800 ms latency due
     to MSE's buffering model — borderline for desktop
     interaction.
   **Proposed: WebRTC.** It is the only option that gives
   simultaneous low-latency video + low-latency audio +
   low-latency input return path in one well-defined
   primitive, and the LAN-only assumption means we can
   skip TURN entirely and keep STUN simple.

4. **Frame pacing.** The framebuffer is event-driven (only
   changes on draw ops); video encoders prefer a steady
   cadence. Three regimes:
   - **Constant 60 fps, encode every tick.** Wasteful when
     idle; the encoder will produce tiny delta frames so
     bandwidth stays low, but CPU cost is real.
   - **On-dirty only, no pacing.** Lowest CPU; can produce
     wildly variable framerate which interacts badly with
     browser video pipelines.
   - **Hybrid: 30 fps cap, encode-when-dirty within that
     budget, force keyframe on first frame and on
     reconnect.** **Proposed.**
   Per-rectangle dirty tracking would let us encode partial
   frames; nice-to-have but not needed for MVP if we cap
   FPS sensibly.

5. **Audio path.** Forward Opus packets directly into the
   WebRTC audio track (no decode/re-encode), or decode to
   PCM and re-encode? **Proposed: direct Opus passthrough
   when the SPICE server negotiated Opus**, which is the
   common case; fall back to encoding-from-PCM via
   `audiopus` when the server only offers raw PCM. This
   keeps quality and CPU low in the common case.

6. **Input scancode mapping.** Browsers report
   `KeyboardEvent.code` (Atom-style identifiers, e.g.
   `KeyA`, `ArrowLeft`, `F11`); SPICE wants AT/PS/2 set 1
   scancodes. The `paste-as-keystrokes` translator already
   has an ASCII → scancode mapping that can be extended.
   **Proposed: build a `KeyboardEvent.code` → scancode
   table in the browser shell**, send raw scancodes over
   the datachannel, let the Rust side relay to SPICE
   inputs unchanged. Avoids server-side string-key parsing.

7. **Cursor.** SPICE delivers cursor shapes via the cursor
   channel; the desktop client renders them as an egui
   overlay. For the web client we have a choice: composite
   the SPICE cursor into the outgoing video frame (simple,
   adds latency to cursor motion proportional to encode
   pipeline), or send cursor updates over the datachannel
   and let the browser render the cursor as a CSS overlay
   (lower perceived latency, more code). **Proposed:
   composite into the video for MVP**, datachannel-driven
   overlay as future work for "buttery cursor" feel.

8. **HTTPS / TLS.** **Proposed: plain HTTP for MVP.** The
   browser only *receives* media (no `getUserMedia`), so
   secure-context restrictions do not apply to the headline
   features. Listen on an ephemeral port; print the URL to
   stdout. TLS is deferred until a feature that demands a
   secure context lands or until ryll-web is exposed
   beyond a trusted LAN. When TLS does land, the proposed
   shape is "take a cert+key pair on the CLI" with operator
   recipes for `mkcert` / `step-ca` / Let's Encrypt; ACME
   inside ryll-web is further future work.

9. **Authentication.** **Proposed: none in MVP.** The
   threat model is "the operator on a trusted LAN copies
   a URL into their own browser"; the URL contains an
   ephemeral port chosen at launch, and the operator-side
   `.vv` file already authenticated the SPICE connection.
   This is comparable to leaving an unauthenticated X
   server or `python -m http.server` running on the LAN.
   Per-launch URL tokens (`?token=…`, jupyter-style),
   login pages, OIDC, and mTLS are all future work and
   are the natural pairing for whenever TLS lands.

   *Pointer Lock caveat.* Chrome requires a secure context
   for `requestPointerLock()`; Firefox does not. Without
   pointer lock, the browser can only deliver absolute
   pointer coordinates, which is fine for SPICE servers
   that have vdagent (the common case) but degrades
   relative-pointer use cases (games, drawing apps, some
   guest UIs without vdagent). MVP accepts this trade.

10. **Browser shell hosting model.** Static files inside the
    Rust binary via `include_str!` / `include_bytes!`, or
    served from a `static/` directory at runtime?
    **Proposed: `include_bytes!`** so the binary is
    self-contained and the operator does not have to ship a
    sibling `static/` directory.

11. **Multi-monitor in MVP?** WebRTC supports multiple video
    tracks in one PeerConnection trivially. The browser
    shell would need to render each track in a separate
    `<video>` element. **Proposed: single monitor for
    MVP**, multi-monitor as the first post-MVP feature
    because the back end largely already supports it.

12. **`xspice` vs QEMU+SPICE** as the server side. Both
    speak SPICE; the operator will decide based on the
    actual desktop being shared. ryll-web is agnostic. The
    plan does *not* take a position on which the operator
    runs.

13. **Lifecycle and process supervision.** Long-running
    ryll-web processes need to be restarted on crash, on
    desktop reboot, and across SPICE-server restarts.
    **Proposed: ship a systemd unit example in
    `docs/web-frontend.md`** and otherwise stay out of the
    process-supervision business.

14. **CPU budget.** A 1080p60 encode in `openh264`
    "ultrafast" is roughly one core. If the operator's
    desktop is also doing actual work, that may be too
    much. **Proposed: instrument and measure in Phase 5**;
    if it is a real problem, NVENC support (a future-work
    item) is the answer, not "make the encoder smarter".

15. **Where the renderer lives.** Inside `ryll-web`, or as
    a separate `ryll-encode` daemon that ryll-web talks to
    over a Unix socket? The latter would let the operator
    run one encoder per machine and many transports per
    encoder. **Proposed: monolithic `ryll-web` for MVP**;
    revisit if multi-tenancy ever becomes interesting.

## Execution

Phase files are **not yet written**. The breakdown below is
provisional; expect it to change once the open questions
resolve.

| Phase | Plan | Status |
|-------|------|--------|
| 0. Multi-mode parity audit (GUI vs headless today) | PLAN-web-frontend-phase-00-parity-audit.md | Not written |
| 1. Renderer extraction (`shakenfist-spice-renderer` crate) | PLAN-web-frontend-phase-01-extract.md | Not written |
| 2. Encoder pipeline (framebuffer → H.264/VP8 NAL units) | PLAN-web-frontend-phase-02-encoder.md | Not written |
| 3. WebRTC plumbing (`webrtc-rs`, video track, audio track, datachannel) | PLAN-web-frontend-phase-03-webrtc.md | Not written |
| 4. HTTP server + signalling endpoint + browser shell | PLAN-web-frontend-phase-04-server.md | Not written |
| 5. Inputs + cursor + audio plumbing through to SPICE | PLAN-web-frontend-phase-05-iac.md | Not written |
| 6. Reconnect + lifecycle | PLAN-web-frontend-phase-06-lifecycle.md | Not written |
| 7. CI build + packaging + docs | PLAN-web-frontend-phase-07-ci.md | Not written |
| 8. Operator docs + systemd example + cert recipes | PLAN-web-frontend-phase-08-docs.md | Not written |

### Phase 0: Multi-mode parity audit

Survey the existing codebase and produce a single read-only
artifact, `docs/multi-mode-parity.md`, that lists every
user-facing ryll feature in a row and marks for each mode
(GUI / headless / web-planned) one of:

- **available** — feature is fully reachable in this mode;
- **partial** — only some of the feature is reachable
  (e.g. CLI flag exists but no runtime control);
- **missing** — feature is not reachable today;
- **n/a — intrinsic** — the feature physically cannot
  exist in this mode (justification required).

Source material: walk `ryll/src/`, every `--*` CLI flag,
the menu entries in `app.rs`, the side panels (USB,
Folders, Notifications, Traffic), the bug-report and
screenshot hotkeys, the cadence/paste-as-keystrokes
machinery, and the entries in `README.md`'s features list.
Cross-check against the ARCHITECTURE.md mode table added
alongside this plan. For every "missing" or "partial"
cell, link to the relevant source location so a follow-on
plan can be written without rediscovering the gap.

The audit deliberately does **not** propose fixes — it is
a baseline. Closing the gaps is tracked in a separate
follow-on plan (`PLAN-multi-mode-parity-driveup.md`,
written after Phase 0 lands) so the web-frontend phases
do not accidentally absorb headless-feature backlog work.
The artifact is expected to be a living document: when a
feature is added, its row is added; when a mode gains a
feature, the cell is updated; reviewers are expected to
keep it honest. Acceptance: the matrix exists, every
README feature appears in it, and every cell has a value.

### Phase 1: Renderer extraction

Pull `DisplaySurface`, the per-channel handler structs, and
the per-session orchestration code out of the `ryll` binary
crate and into a new `shakenfist-spice-renderer` library
crate. `ryll` becomes a thin egui frontend over the new
crate. No web-facing code yet — this phase is "prove the
existing `ryll` binary still works after the move, with all
existing tests passing on all three platforms". This phase
is also the answer to open question (1); if the extraction
is messier than expected, fall back to feature-flagging
egui inside `ryll`.

### Phase 2: Encoder pipeline

Add a `shakenfist-spice-encoder` crate (or a module inside
the renderer crate) that takes a `&DisplaySurface`, encodes
the dirty framebuffer at a configurable cadence, and emits
NAL units (or VP8 frames). Reuse the `openh264` lessons
from `capture.rs`. Wire keyframe-on-demand, since WebRTC
needs a keyframe whenever a new viewer attaches. No
network code in this phase — feeding the encoder from a
test harness and dumping NAL units to a file is the
acceptance criterion.

### Phase 3: WebRTC plumbing

Bring up `webrtc-rs`. Build a dummy server that, given an
SDP offer over a local TCP socket, negotiates a
PeerConnection with one video track wired to the encoder
from Phase 2 and one audio track wired to a synthetic
Opus stream. Acceptance: a manual `wrtc` test harness or
the `webrtc-rs` examples receive video and play it back.

### Phase 4: HTTP server + signalling + browser shell

Add a tokio HTTP server (`hyper` or `axum`) bound to an
ephemeral port; print the resulting `http://<host>:<port>/`
URL to stdout at startup. Serve a static HTML+JS bundle
(embedded with `include_bytes!`), expose a `POST /offer`
endpoint for SDP exchange, and hand the resulting
PeerConnection off to the WebRTC machinery from Phase 3.
The browser shell is small — `<video>`,
`RTCPeerConnection`, keyboard/mouse capture. Acceptance:
launch ryll-web with a `.vv` file, open the printed URL
in Firefox/Chrome, see the test pattern from Phase 2
playing in the browser. No TLS, no auth.

### Phase 5: Inputs + cursor + audio

Wire the browser-side keyboard/mouse handlers into the
datachannel. Build the `KeyboardEvent.code` → scancode
table on the browser side. Plumb pointer motion through.
On the Rust side, deliver scancodes and pointer events to
the existing `inputs` channel handler. Composite the
SPICE cursor into the encoded video (open question (7)
default). Forward SPICE Opus packets into the WebRTC
audio track. Acceptance: open the URL, the desktop is
visible and audible, and typing/clicking works.

### Phase 6: Reconnect + lifecycle

Reconnect-on-disconnect: when the PeerConnection drops,
hold the SPICE session open for ~30 seconds so the browser
can re-open the same URL and resume. Graceful shutdown on
SIGTERM (the existing `ctrlc` handling carries over). No
auth in this phase — anyone who can reach the printed URL
gets the session. (TLS and per-session tokens are tracked
in Future work.)

### Phase 7: CI + packaging

`cargo build -p ryll-web --release` in the existing
release workflow. Produce a Linux x86_64 binary and ship
it as a release artifact alongside ryll. macOS and
Windows builds are nice-to-have but not blocking — the
operator's deployment target is Linux. `.deb` packaging
to follow the existing pattern.

### Phase 8: Docs

- New `docs/web-frontend.md` covering: what ryll-web is,
  how to launch it from a `.vv` file, where to find the
  printed URL, how to run it as a systemd service,
  troubleshooting WebRTC connectivity, and a clear
  *Security* note that the MVP listens on plain HTTP with
  no authentication and is intended for trusted-LAN use
  only.
- `README.md` — add ryll-web to the supported entry
  points.
- `ARCHITECTURE.md` — add a section explaining the
  renderer-crate split and where the encoder/transport
  sits in the data flow.
- `AGENTS.md` — note `cargo build -p ryll-web` and any
  new linting expectations.
- `docs/portability.md` — record that ryll-web is Linux
  only for MVP, with a note that the encoder code is
  portable.
- `kerbside/docs/` — review whether kerbside's
  documentation should mention ryll-web as a deployment
  pattern.

## Administration and logistics

### Success criteria

We will know the MVP has landed when:

* `cargo build -p ryll-web --release` produces a single
  binary from a clean checkout.
* `ryll-web session.vv` (or the equivalent CLI flags)
  starts, connects to the SPICE server, and prints a
  `http://<host>:<port>/` URL to stdout.
* Opening that URL in Firefox or Chrome on a peer machine
  shows the remote desktop.
* Keyboard input from the browser produces correct
  characters in the guest, including shifted symbols and
  arrow keys.
* Mouse input from the browser produces correct cursor
  motion and clicks in the guest.
* Audio from the guest plays in the browser with
  acceptable sync.
* The browser tab can be closed and re-opened (same URL)
  within ~30 seconds and the SPICE session resumes
  without a server-side reconnect.
* `docs/multi-mode-parity.md` (the Phase 0 artifact)
  exists and every feature listed in `README.md` appears
  in the matrix with a value in every mode column.
* `pre-commit run --all-files` still passes.
* `cargo test --workspace` still passes — the existing
  `ryll` binary continues to work unchanged after the
  Phase 1 extraction.
* `docs/web-frontend.md` exists and is sufficient for the
  operator to bring up a session from scratch.

### Future work

* **Drive down GUI ↔ headless ↔ web parity gaps.** The
  Phase 0 audit is a baseline, not a fix. Each gap that
  the audit surfaces should spawn its own follow-on plan
  (collected under a `PLAN-multi-mode-parity-driveup.md`
  master plan written after Phase 0 lands). This work
  proceeds in parallel with the rest of this plan and is
  *not* a prerequisite for the web frontend MVP — the web
  frontend deliberately ships with a minimal feature set
  in MVP and the parity work catches up incrementally.
* **HTTPS / TLS.** Take a cert+key pair on the CLI; document
  `mkcert` / `step-ca` / Let's Encrypt recipes. Required
  before any feature that wants a secure context (clipboard
  sync, Pointer Lock on Chrome) can land. ACME inside
  ryll-web is further future work.
* **Browser-side authentication.** Per-launch URL token
  (`?token=…`, jupyter-style) as the first step; login UI,
  OIDC, and mTLS as bigger follow-ups. Natural pairing
  with the TLS work above.
* **Multi-monitor.** Add one video track per SPICE display
  surface; arrange them in the browser shell. Most of the
  back-end is already multi-surface.
* **USB redirection** via WebUSB (Chrome/Edge only) or a
  small companion native helper.
* **Clipboard sync** between browser and guest, via the
  async clipboard API and the SPICE vdagent clipboard
  channel.
* **Folder sharing.** Probably via a browser-side
  drag-and-drop area that uploads files into a temporary
  WebDAV mount on the guest. Bigger than it looks.
* **Hardware encoding** (NVENC / QSV / VAAPI) for desktops
  with capable GPUs. Drops encoder CPU to near zero and
  lets the operator run multiple sessions per machine.
* **Per-rect dirty tracking** for partial-frame encodes.
  Meaningful CPU/bandwidth win on mostly-static desktops.
* **Multi-session / multi-tenant** mode — one daemon, many
  desktops, many viewers. Hard-blocked on the
  authentication and TLS items above.
* **TURN support** for WAN access where STUN cannot
  traverse the NAT. Pair with `coturn` deployment notes.
* **AV1 encode** when `rav1e` becomes fast enough for
  real-time desktop resolutions, or via SVT-AV1 FFI.
* **Mobile UX.** Touch gestures, on-screen keyboard
  toggle, pointer-precision indicator.
* **Datachannel-driven cursor overlay** for sub-encoder
  cursor latency.
* **Recording.** Reuse the encoder pipeline to dump a
  session to disk in MP4, paralleling the existing
  `--capture` feature.
* **Replace Kasm in the operator's deployment.**
  Concretely: bring up xspice on the dev desktop, point
  ryll-web at it, retire the Kasm container. Treat as
  the MVP-acceptance milestone *for the operator*, not a
  general-availability claim.

### Bugs fixed during this work

(None yet — no implementation has started.)

### Back brief

Before executing any step of this plan, please back brief
the operator as to your understanding of the plan and how
the work you intend to do aligns with that plan. Because
this is a concept plan, the first "step" is to resolve the
open questions above — especially (1) crate layout, (2)
encoder choice, and (3) transport — before any phase plan
is written.

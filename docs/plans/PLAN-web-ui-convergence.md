# Converge the web frontend toward the eframe GUI's UX

## Prompt

Before responding to questions or discussion points in this
document, explore the ryll codebase thoroughly. Read relevant
source files, understand existing patterns (SPICE protocol
handling, channel architecture, async task model, image
decompression, egui rendering — and especially the eframe
`RyllApp` in `ryll/src/app.rs`, the web frontend in
`ryll/src/web/`, and the wire-format relays in
`ryll/src/web/inputs.rs` / `ryll/src/web/cursor.rs`), and
ground your answers in what the code actually does today.
Do not speculate about the codebase when you could read it
instead. Where a question touches on external concepts (egui
immediate-mode rendering, browser security model around
WebUSB / File System Access API / Clipboard API), research
as needed to give a confident answer. Flag any uncertainty
explicitly rather than guessing.

All planning documents should go into `docs/plans/`.

Consult `ARCHITECTURE.md` for the system architecture
overview. Key references include `ryll/src/app.rs`
(eframe `RyllApp` — the canonical UI), `ryll/src/web/`
(current web frontend), `ryll/src/web/assets/`
(HTML/CSS/JS), and `docs/web-frontend.md`.

When we get to detailed planning, I prefer a separate plan
file per detailed phase, named for the master plan with
`-phase-NN-descriptive` appended. Track sub-phases in a table
in this master plan under the Execution section.

I prefer one commit per logical change, and at minimum one
commit per phase.

## Situation

This plan started as an idea bubble from the ryll
`web-feedback` debugging sessions (008a–008g). Once the
display, input, and resize stories were good enough, the
gap between the web frontend and the eframe GUI as an
operator UX became hard to ignore.

eframe ryll today has: hamburger menu with settings,
notifications panel, channel-state / statistics view,
reconnect dialog, paste-as-keystrokes, bug-report capture,
USB device panel, folder-share panel, clipboard sync,
audio control, modifier-state indicators, Ctrl-Alt-Del
shortcut, and assorted keyboard helpers.

Web ryll today has: a Disconnect button, an Enable-audio
button, a Reconnect button. That is it.

This is not because the web frontend is bad — it is
deliberately minimal because nobody has yet decided what
the parity story should be. The result is that operators
who use both modes context-switch hard between them, and
ones who use only the web mode have no in-band way to
inspect SPICE channel health, see notifications, capture
a bug report, or do most of the things the eframe UI
takes for granted.

Two structural reasons the web frontend lags:

1. **The eframe UI reads renderer state directly.**
   `RyllApp` holds `Arc<Mutex<...>>` references to the
   same `SurfaceMirror`, `NotificationStore`,
   `ChannelSnapshots`, `TrafficBuffers`, etc. that the
   renderer mutates. The web frontend can't do that
   across the browser boundary — any panel that wants
   to display renderer state needs a serialised
   projection over the control DC plus a JS view to
   render it.
2. **No shared UI primitives.** eframe is immediate-mode
   Rust drawing egui widgets; the web frontend is
   HTML/CSS/JS. Every panel currently has to be
   implemented twice unless we commit to an egui-on-wasm
   migration (a real path but a multi-month rewrite).

Some features have genuine ceilings the eframe version
doesn't:

- **USB redirection.** WebUSB exists but is gatekept
  (HTTPS-only, per-origin permission prompts, doesn't
  speak the SPICE usb-redir protocol natively, and isn't
  available on Safari).
- **Folder sharing.** File System Access API is
  Chromium-only in practice and doesn't preserve Unix
  permissions or symlinks.
- **Clipboard sync.** Permission-gated and limited to a
  short list of MIME types.

These probably stay out of reach for the web frontend
indefinitely. The plan should be explicit that the goal
is *operator UX convergence on the operations that make
sense in a browser*, not bit-for-bit feature parity.

## Mission

Grow the web frontend deliberately toward the eframe UX,
starting with the operator-facing affordances that have
the biggest debuggability payoff and the smallest
implementation cost, while accepting that some panels
will always be browser-restricted.

The first concrete deliverable should be a visual
refresh that brings the web frontend's chrome closer to
the eframe GUI's layout idiom — bottom status bar, top
hamburger menu, side notification drawer — even before
all the data those affordances expose is plumbed
through. That way subsequent panels slot into a known
layout rather than each landing as a one-off floating
overlay.

## Open questions

### First-pass UX shape

The user's gut sketch from the conversation that spawned
this plan:

- **Status bar at the bottom.** Mirrors the eframe GUI's
  status bar. Shows: connection state, SPICE channel
  health summary, viewport size, encoder bitrate (once
  PLAN-web-encoder-quality lands), latency.
- **Hamburger menu top-left.** Replaces the current
  scattered top-right buttons. Items: Disconnect, audio
  mute toggle, reconnect, bug-report capture, settings.
- **Audio defaults to muted with a mute icon.** Matches
  the eframe convention; replaces the current "click to
  enable audio" prompt which exists only because of
  browser autoplay policy. The icon makes the muted
  state visible without requiring an action prompt.
- **Optional notification sidebar (drawer).** Slides in
  from the right with the contents of
  `NotificationStore`. Closed by default; the hamburger
  menu has a "Notifications (N)" item that opens it.

The iteration story after that is the open part: each
panel gets evaluated on cost vs. operator pain, not by
how close it gets us to bit-for-bit parity.

### State-projection wire format

Today the control DC carries inputs (browser → server)
and cursor (server → browser). For panels to show
renderer state we need:

- A new message family for state projections.
  Candidates: a single "state snapshot" message at low
  cadence (e.g. 1 Hz), a per-type delta stream, or
  on-demand pull triggered by UI open. Snapshot is the
  simplest; delta is the most bandwidth-efficient.
- A schema for each projected type. `NotificationEntry`
  is straightforward (already serde-friendly). Channel
  state and traffic stats are bigger and need more
  thought about what to omit.
- Decisions about authority for things like
  acknowledging notifications. eframe lets the user
  dismiss a notification; the web sidebar would either
  need to mirror that back to the server or treat its
  view as read-only.

### Convergence path

Three sketched options, in increasing scope:

1. **HTML/JS panels.** Continue with hand-written
   HTML/CSS/JS, growing one panel at a time. Cheapest
   per-panel cost, fastest first results, never truly
   converges with the eframe widget set.
2. **egui-on-wasm overlay.** Compile a subset of the
   eframe UI to wasm, render to a canvas overlaid above
   the `<video>` element, route input through the same
   keyboard / pointer handlers we already have. Real
   parity by construction; significant rewrite; UX
   risks around accessibility and layering an immediate-
   mode canvas above a hardware-accelerated video.
3. **Hybrid.** HTML/JS for high-frequency reactive
   surfaces (notifications, status bar), egui-on-wasm
   for the heavier panels (settings, channel state).
   Best of both, most architectural complexity to keep
   straight.

We can probably start on option 1, defer the option 2 / 3
choice until a few panels have shipped, and revisit once
we have a feel for the per-panel cost of HTML/JS.

### Scope boundary

Explicitly out of scope, but recorded:

- USB redirection. WebUSB doesn't speak SPICE usb-redir
  and the browser permission model is too restrictive to
  give a usable UX.
- Folder sharing. Browser FS APIs can't preserve the
  fidelity SPICE webdav assumes.
- Clipboard sync. Permission-gated, format-limited,
  feasible but not in the first wave.
- Bug-report file output. The browser can't write to the
  operator's filesystem the way the eframe GUI does. May
  end up as a download endpoint instead.

## Execution

Sketch only; phases to be confirmed once phase 0 (decisions)
runs:

| Phase | Plan | Status |
|-------|------|--------|
| 0. Layout decisions and wire-format schema | TBD | Not started |
| 1. Status bar + hamburger menu refactor | TBD | Not started |
| 2. Audio defaults to muted with mute toggle | TBD | Not started |
| 3. Notification sidebar | TBD | Not started |
| 4+. Iterate based on operator feedback | TBD | Not started |

The intent is that phases 1–3 land the layout idiom and
prove the wire-format approach with three small,
operator-facing surfaces. After that, each new panel is
its own miniature plan, not a phase of this one.

## Agent guidance

To be added once the plan is fleshed out. The default
execution model in `PLAN-TEMPLATE.md` applies.

## Administration and logistics

### Success criteria

Placeholder. Likely candidates:

- The web frontend's chrome reads as a smaller cousin of
  the eframe GUI's chrome, not as a separate product.
- Operators using only the web mode can read SPICE-side
  notifications without leaving the browser.
- Audio defaults to muted; the unmute toggle is a
  permanent affordance, not a one-shot button.
- `docs/web-frontend.md` documents the new UI surfaces
  and the convergence intent (and explicit
  out-of-scope-by-design list).

### Future work

- The egui-on-wasm question. Worth revisiting after a
  few HTML/JS panels have shipped — we'll know better
  then whether the per-panel cost justifies the
  rewrite.
- Per-operator preferences (e.g. "open notification
  sidebar by default"). Needs a small local-storage
  layer on the JS side.

### Bugs fixed during this work

(Empty.)

### Documentation index maintenance

When this plan moves past placeholder status, update
`docs/plans/index.md` and `docs/plans/order.yml`.

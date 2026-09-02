# AGENTS.md - Guide for AI Coding Assistants

Conventions and gotchas for working in ryll that you cannot infer by
reading the code. Everything else is documented elsewhere; this file
points you there rather than restating it.

## What ryll is

Ryll is a multi-modal Rust SPICE VDI client. It began as a client for
**performance testing the Kerbside SPICE proxy** (shakenfist/kerbside) and is
now also intended for general-purpose interactive use. Its goals are to:

1. Be a usable day-to-day SPICE client across its GUI, headless, and web modes
2. Generate controlled SPICE traffic as a client
3. Be instrumented to gather performance metrics
4. Measure latency from input events to display updates
5. Run in headless mode for automated benchmarking

Related repositories:

- **shakenfist/kerbside** — the SPICE protocol native proxy being tested
- **shakenfist/kerbside-patches** — OpenStack integration patches for kerbside
- **shakenfist/kerbside/testclient** — the original Python version of ryll

## Where the documentation lives

| Question | Document |
|----------|----------|
| What are the crates, and how do they fit together? | [`ARCHITECTURE.md`](ARCHITECTURE.md) |
| Why is it built this way? | [`docs/design-decisions.md`](docs/design-decisions.md) |
| How do I build, test and debug it? | [`docs/development.md`](docs/development.md) |
| How does CI work, and what gates a PR? | [`docs/ci.md`](docs/ci.md) |
| What does the SPICE wire protocol handling do? | [`docs/spice-protocol.md`](docs/spice-protocol.md) |
| How are surfaces composited and audio played? | [`docs/rendering-pipeline.md`](docs/rendering-pipeline.md) |
| How do USB and folder sharing work? | [`docs/device-redirection.md`](docs/device-redirection.md) |
| What diagnostics does a session record? | [`docs/diagnostics.md`](docs/diagnostics.md) |
| What survives a reconnect? | [`docs/session-lifecycle.md`](docs/session-lifecycle.md) |
| How does `--web` mode work internally? | [`docs/web-mode-internals.md`](docs/web-mode-internals.md) |
| What is the control socket contract, and how is it implemented? | [`docs/control-socket-protocol.md`](docs/control-socket-protocol.md) |
| Which features work in which mode? | [`docs/multi-mode-parity.md`](docs/multi-mode-parity.md) |
| How do I cut a release? | [`docs/releasing.md`](docs/releasing.md) |

`docs/index.md` is the full index.

Links inside `docs/` must resolve within `docs/`. The tree is
synchronised into `shakenfist/shakenfist` under
`docs/components/ryll/` and published on shakenfist.com, where the
repository above `docs/` does not exist, so `../tools/x.sh` and
friends 404 there while rendering perfectly on GitHub. Anything
outside `docs/` — source files, workflows, `README.md` — needs an
absolute `https://github.com/shakenfist/ryll/blob/develop/<path>`
URL. This applies to `docs/plans/` too; those pages are published
as well. The one exception is a fenced block holding a file
destined for somewhere else — a crates.io `README.md` template,
for instance — where the link has to resolve wherever that file
will live.

For the same reason, `docs/` is mkdocs-first: `!!!` admonitions and
mermaid fences are written for the published site, and their
degraded GitHub rendering (a literal `!!!` paragraph above an
indented code block) is knowingly accepted. Do not "fix" them back
to blockquotes or ASCII art.

Do not quote dependency versions in `docs/`. A version copied out of
a manifest goes stale at the next bump and then reads as a claim that
the older version is still supported; the manifests are the only
place it is true. Name the crate and say what it is for. Where a
version *floor* is load-bearing rather than incidental, record the
floor and its reason as a comment in the `Cargo.toml` that declares
it, and have the prose point there. A release number in a worked
example (`X.Y.Z`, or the `0.2.0` in
[`docs/releasing.md`](docs/releasing.md)) is not a pin and is fine.
Saying which upstream release changed a behaviour is a historical
fact, not a pin, and is also fine.

## Protocol reference sources

When working on SPICE protocol implementation details, these
local source trees are available for reference:

| Source | Path | Use for |
|--------|------|---------|
| Kerbside Python proxy | `shakenfist/kerbside/` | Protocol docs in `docs/`, packet parsing in `kerbside/spiceprotocol/packets/`, reference test client in `testclient/ryll/` |
| SPICE protocol headers | `/srv/src-reference/spice/spice-protocol/` | Canonical enum definitions, message structures, capability flags |
| SPICE common library | `/srv/src-reference/spice/spice-common/` | Shared marshalling code used by both server and client |
| SPICE GTK client | `/srv/src-reference/spice/spice-gtk/` | Reference client implementation (C/GTK) |
| spice-html5 | `/srv/src-reference/spice/spice-html5/` | JavaScript SPICE client (useful for LZ/GLZ decompressor reference) |
| virt-viewer | `/srv/src-reference/spice/virt-viewer/` | The standard SPICE client, .vv file handling |
| QEMU | `/srv/src-reference/qemu/qemu/` | Server-side SPICE implementation in `ui/spice-*` |
| Linux kernel | `/srv/src-reference/torvalds/linux/` | QXL driver in `drivers/gpu/drm/qxl/` |

## Multi-mode parity is a requirement, not an aspiration

A feature is not complete when it works in only one of the GUI,
headless and web modes. Every feature should be reachable from every
mode that can physically support it; intrinsic mode-specific features
(egui-only UI panels, browser-only clipboard APIs) must be documented
as such in [`docs/multi-mode-parity.md`](docs/multi-mode-parity.md) so
the gaps stay visible. Adding a feature to one mode without updating
that table is an incomplete change.

## Common tasks

### Adding a new CLI option

1. Add to `Args` struct in `ryll/src/config.rs`
2. Pass through to relevant code in `ryll/src/main.rs` or
   `ryll/src/app.rs`

### Adding a new statistic

1. Add variant to `ChannelEvent` enum in
   `shakenfist-spice-renderer/src/channels/mod.rs`
2. Send from relevant channel handler in
   `shakenfist-spice-renderer/src/channels/`, via
   `self.events.emit(...)` — never a bare `event_tx`. `EventSink`
   couples the queue to the repaint wake-up so a new event cannot
   forget it and leave the UI stale.
3. Handle in `process_events()` in `ryll/src/app.rs`

### Modifying protocol handling

1. Message definitions in
   `shakenfist-spice-protocol/src/messages.rs`
2. Constants/enums in
   `shakenfist-spice-protocol/src/constants.rs`
3. Channel-specific logic in
   `shakenfist-spice-renderer/src/channels/*.rs`
4. Link handshake and auth in
   `shakenfist-spice-protocol/src/link.rs` — both the client
   role (`perform_link`, `perform_auth`, `encrypt_password`)
   and the server/proxy role (`read_link_mess`,
   `send_link_reply`, `read_auth_ticket`, `send_auth_result`,
   `generate_ticket_keypair`, `decrypt_password`). The
   kerbside SPICE proxy rewrite consumes the server role.
5. Parsers of untrusted wire input use the `BoundedReader`
   in `shakenfist-spice-protocol/src/reader.rs` (bounds- and
   overflow-checked, panic-free) rather than ad-hoc slicing.
   New link/message parsers should ship with a fuzz target
   under `shakenfist-spice-protocol/fuzz/` — see
   shakenfist/ryll#135 (broaden coverage) and #136 (retrofit
   existing parsers onto `BoundedReader`).

Helper tooling for these tasks (`tools/pcap-inspect.py`,
`tools/web-smoke.sh`, `examples/control-socket-demo.py`) is documented
in [`docs/development.md`](docs/development.md).

## ChannelEvent versus a trait

Prefer a `ChannelEvent` variant when the concern is event-shaped (a
one-shot notification, a surface lifecycle signal, a latency sample).
Prefer a trait when the concern is a long-lived sink the channel writes
to continuously (traffic recording, capture frames). This keeps the
event channel lightweight and the trait surface minimal. The trait
inventory is in [`ARCHITECTURE.md`](ARCHITECTURE.md).

The renderer crate is **egui-free** — no `eframe` or `egui` types in
its source. Reaching for one is always the wrong answer; the frontend
adapts to the renderer, not the other way round.

## Control socket

[`docs/control-socket-protocol.md`](docs/control-socket-protocol.md) is
the canonical, version-controlled contract for the Unix-domain control
socket. **Any change to the control surface MUST update that spec in
lockstep with the code.** The spec is load-bearing: the latency
loadtest port, the `digest_updated` event and the Sextant scenario test
all implement against it, and changing a verb signature or event shape
without updating it breaks those consumers silently.

New verbs or events **SHOULD** ship with a matching test in
`shakenfist-spice-renderer/tests/control_socket.rs`.

The control socket was designed in kerbside, not here. Per the
cross-repo single-home rule its plan lives at
`shakenfist/kerbside/docs/plans/PLAN-test-harness.md` even though the
implementation commits land in ryll; commits implementing or extending
it carry a `Plan:` trailer pointing back at that plan so the trail
between design and implementation is explicit in `git log`.

## WebRTC conventions

Both of these were learned the hard way and apply to all webrtc-rs work:

- **Handler methods must never block — they run inline in the
  driver event loop.** webrtc-rs 0.20 replaced the per-object
  callback registrations (`on_peer_connection_state_change`,
  `on_track`, `on_data_channel`, `on_message`, ...) with one
  `PeerConnectionEventHandler` supplied to the builder before the
  peer connection exists, and every method on it is awaited
  inline by the driver loop. A slow or blocking handler method
  stalls the whole connection, not just the event it is handling.
  So anything that needs to *loop* — reading a datachannel's
  events or a remote track's RTP — must `tokio::spawn` and return
  immediately, and anything that needs to *hand off from inside a
  handler method* must use `try_send`, never `send().await`, so a
  full channel degrades to a dropped message rather than stalling
  the driver. This is stricter than pre-0.20, where only
  `on_track` firings were serialised on each other.

  **The rule is about the dispatch path, not about the type.**
  Check where a function is actually *called from* before applying
  it. `BridgeEvents` holds both kinds: `on_state_change` is
  dispatched from the handler and uses `try_send`, while
  `on_control_message` is reached only from a spawned
  `run_dc_pump` and therefore awaits — deliberately, because it
  carries keyboard and mouse events, and back-pressure onto SCTP
  is better than a dropped key-up leaving a modifier stuck down in
  the guest. Applying "never block" to the second one cost real
  input events before it was caught. See
  [`docs/web-mode-internals.md`](docs/web-mode-internals.md) for
  where each case bites in `bridge.rs`.

- **One-shot lifecycle events use `StickySignal`, never a bare
  `Notify`.** `Notify::notify_waiters()` wakes only the waiters
  registered at that instant — a waiter that subscribes afterwards
  blocks forever, and `Notified` does not even register interest
  until it is first polled, so the naive "check a flag, then
  await" ordering has a lost-wakeup window. This was a real
  production bug in the bridge reaper.
  `shakenfist_spice_webrtc::StickySignal` packages the correct
  pattern — `Notified::enable()` before the flag check on the wait
  side, `notify_waiters()` (never `notify_one()`, which would leak
  a permit) on the raise side — and is unit-tested against the
  lost-wakeup schedule. Do not hand-roll a fifth copy; that is how
  the original bug got in.

  A *recurring* wake source is the other case, and the rules
  invert. `WebState::bridge_replaced` is a bare `Notify` using
  `notify_one()` on purpose: the stored permit is the feature,
  because it survives the reaper's 500 ms no-bridge sleep and is
  still there when the loop next parks. The cost is that a wake
  carries no information. Any loop that gains a second wake
  source must re-check the condition it actually cares about
  rather than treating the wake as proof — the reaper waking and
  concluding "my bridge died" is a bug that shipped, and the fix
  was to gate the reap on `StickySignal::is_raised`. Sticky for
  a one-shot fact; bare `Notify` plus an explicit re-check for a
  recurring nudge.

## The vendored sfui copy

`ryll/src/web/assets/sfui/` is a verbatim copy of
[sfui](https://github.com/shakenfist/sfui), the shared web UI
design system. **Never edit it in place** — an in-place change
is silently discarded the next time the copy is synced, and the
daily fleet-wide `sfui-vendor` audit fails on the drift. Change
sfui, merge there, then re-vendor with that repository's
`tools/vendor.sh`. Most of the copy is deliberately unserved;
`ryll/src/web/assets/style.css` is where this page's own styles
go, and it overrides sfui by being unlayered rather than by
`!important`. See
[`docs/web-mode-internals.md`](docs/web-mode-internals.md).

## Cargo feature gating

The `ryll` binary ships four features: `gui`, `audio` and `capture`
default-on, `digest-decode` default-off. The kerbside loadtest and
direct-qemu CI build with `--no-default-features`. When adding code
that needs a GUI, audio, capture or digest type, **gate the import at
the use site** — an ungated import breaks the no-default-features
build, which only some CI lanes exercise. The feature list and what
each pulls in is in [`docs/development.md`](docs/development.md).

## Process documents

Four process documents at the repo root capture the
workflows we use repeatedly. Read the relevant document
before starting one of these activities so the resulting
plan/PR follows the established structure. (`-TEMPLATE`
names are true templates that get copied; `PUSH-AUDIT.md`
is a runbook followed in place.)

- **`PLAN-TEMPLATE.md`** — the starting point for
  new plan files in `docs/plans/`. Defines the prompt
  preamble, situation/mission/execution sections, and the
  sub-agent execution model.
- **`PUSH-AUDIT.md`** — pre-push audit for our own
  branches. Two-wave parallel sub-agent review (build /
  style, then code quality / tests / docs / security),
  run as the last phase of every master plan.
- **`MERGE-TEMPLATE.md`** — review and merge process for
  external contributor PRs. Adds deterministic-scanner
  Wave 0, prompt-injection sub-agent, and a mandatory
  follow-up plan that lands as our own PR immediately
  after the contributor's merge.
- **`REVIEW-STATE-TEMPLATE.md`** — skeleton for the local-
  only `REVIEW-STATE.md` file that lives in each
  external-PR review's worktree. Captures findings,
  branch state, contributor history, plan B, and a
  "How to resume" entry point for picking the work back
  up after a pause. Never committed.

## Open questions watch-list

`docs/plans/OPEN-QUESTIONS.md` is a thin, single-page
review surface for symptoms we have seen in bug reports
but cannot yet characterise — entries link out to the
plans that would action them. Check it at the
start of each session closeout (after landing the work
from a new test-session bundle) and ask whether the
bundle moves any open question. If it does, move the
evidence into the linked plan and either close
the entry or sharpen it. Add a new entry when a fresh
symptom doesn't fit anywhere actionable; read the
"When to add a new entry" guidance at the foot of the
file first.

# Plans index

This page summarises every planning document in chronological order. Master
plans decompose work into numbered phases, each with its own detailed plan
file. Standalone plans track issues, follow-ups, or design decisions that
do not require phased execution.

New plans should follow the structure in `PLAN-TEMPLATE.md` at the repo
root. For pre-push audits of our own work see `PUSH-TEMPLATE.md`; for
reviewing and merging external contributor PRs see `MERGE-TEMPLATE.md`.

## Master plans

| Date | Plan | Intent | Status | Phases |
|------|------|--------|--------|--------|
| 2026-04-01 | [Initial porting plan](PLAN-initial.md) | Port the ryll SPICE client from Python to Rust with egui | Complete | (design document) |
| 2026-04-01 | [Capture mode](PLAN-capture.md) | Protocol traffic pcap and display frame video capture for debugging | Complete | [1. Infrastructure](PLAN-capture-phase-01-infra.md), [2. Pcap](PLAN-capture-phase-02-pcap.md), [3. Video](PLAN-capture-phase-03-video.md) |
| 2026-04-01 | [Packaging](PLAN-packaging.md) | Cross-platform packaging for Debian, RPM, macOS, and Windows | Complete | [1. Portability](PLAN-packaging-phase-01-portability.md), [2. CI](PLAN-packaging-phase-02-ci.md), [3. Debian](PLAN-packaging-phase-03-debian.md), [4. RPM](PLAN-packaging-phase-04-rpm.md), [5. macOS](PLAN-packaging-phase-05-macos.md), [6. Windows](PLAN-packaging-phase-06-windows.md), [7. Release](PLAN-packaging-phase-07-release.md) |
| 2026-04-02 | [USB redirection](PLAN-usb-redir.md) | USB device redirection via the SPICE usbredir channel | Complete | [1. VMC channel](PLAN-usb-redir-phase-01-vmc-channel.md), [2. Parser](PLAN-usb-redir-phase-02-usbredir-parser.md), [3. Backend trait](PLAN-usb-redir-phase-03-device-backend.md), [4. Real devices](PLAN-usb-redir-phase-04-real-devices.md), [5. Connect](PLAN-usb-redir-phase-05-device-connect.md), [6. Transfers](PLAN-usb-redir-phase-06-transfers.md), [7. Virtual MSC](PLAN-usb-redir-phase-07-virtual-msc.md), [8. UI](PLAN-usb-redir-phase-08-ui.md), [9. Interrupt](PLAN-usb-redir-phase-09-interrupt.md), [10. Testing](PLAN-usb-redir-phase-10-testing.md) |
| 2026-04-03 | [Cursor rendering](PLAN-cursor-rendering.md) | Render SPICE server-provided cursor as an egui overlay | Complete | [1. Parse](PLAN-cursor-rendering-phase-01-parse.md), [2. Render](PLAN-cursor-rendering-phase-02-render.md) |
| 2026-04-04 | [Bug reports](PLAN-bug-reports.md) | Interactive bug reporting with protocol ring buffers and display region selection | Complete | [1. Ring buffer](PLAN-bug-reports-phase-01-ring-buffer.md), [2. Channel state](PLAN-bug-reports-phase-02-channel-state.md), [3. Zip output](PLAN-bug-reports-phase-03-zip-output.md), [4. GUI button](PLAN-bug-reports-phase-04-gui-button.md), [5. Region select](PLAN-bug-reports-phase-05-region-select.md), [6. Traffic viewer](PLAN-bug-reports-phase-06-traffic-viewer.md), [7. Docs](PLAN-bug-reports-phase-07-docs.md) |
| 2026-04-05 | [USB UI](PLAN-usb-ui.md) | Interactive USB device management panel on the status bar | Complete | [1. Bus fix](PLAN-usb-ui-phase-01-bus-fix.md), [2. Wire tx](PLAN-usb-ui-phase-02-wire-tx.md), [3. Panel](PLAN-usb-ui-phase-03-panel.md), [4. Enumerate](PLAN-usb-ui-phase-04-enumerate.md), [5. Connect](PLAN-usb-ui-phase-05-connect.md), [6. Add disk](PLAN-usb-ui-phase-06-add-disk.md), [7. Polish](PLAN-usb-ui-phase-07-polish.md), [8. Docs](PLAN-usb-ui-phase-08-docs.md) |
| 2026-04-06 | [WebDAV](PLAN-webdav.md) | WebDAV folder sharing via the SPICE port channel | Complete | [1. Port channel](PLAN-webdav-phase-01-port-channel.md), [2. Mux protocol](PLAN-webdav-phase-02-mux-protocol.md), [3. WebDAV server](PLAN-webdav-phase-03-webdav-server.md), [4. Integration](PLAN-webdav-phase-04-integration.md), [5. UI](PLAN-webdav-phase-05-ui.md), [6. Testing](PLAN-webdav-phase-06-testing.md) |
| 2026-04-08 | [Crate extraction](PLAN-crate-extraction.md) | Extract compression, protocol, and usbredir crates for reuse | Complete | [1. Workspace](PLAN-crate-extraction-phase-01-workspace.md), [2. Reserve names](PLAN-crate-extraction-phase-02-reserve-names.md), [3. Compression](PLAN-crate-extraction-phase-03-compression.md), [4. Protocol](PLAN-crate-extraction-phase-04-protocol.md), [5. Usbredir](PLAN-crate-extraction-phase-05-usbredir.md), [6. Client](PLAN-crate-extraction-phase-06-client.md) |
| 2026-04-19 | [Screenshot and latency HUD](PLAN-screenshot-and-latency-hud.md) | Add F8 screenshot capture and a latency sparkline in the stats panel | Complete | [1. Screenshot](PLAN-screenshot-and-latency-hud-phase-01-screenshot.md), [2. Latency sparkline](PLAN-screenshot-and-latency-hud-phase-02-latency-sparkline.md), [3. Docs](PLAN-screenshot-and-latency-hud-phase-03-docs.md) |
| 2026-04-19 | [Idle CPU and latency](PLAN-idle-cpu-and-latency.md) | Investigate 6-core idle CPU usage; replace broken keystroke latency with PING/PONG-based measurement; demote noisy protocol logging; capture runtime metrics in bug reports | Code landed; awaiting user verification | [1. Profile](PLAN-idle-cpu-and-latency-phase-01-profile.md), [2. Repaint](PLAN-idle-cpu-and-latency-phase-02-repaint.md), [3. Logging](PLAN-idle-cpu-and-latency-phase-03-logging.md), [4. Latency](PLAN-idle-cpu-and-latency-phase-04-latency.md), [5. Metrics](PLAN-idle-cpu-and-latency-phase-05-metrics.md) |
| 2026-04-21 | [Display draw-op coverage](PLAN-display-draw-ops.md) | Fill out the SPICE display draw-op set (DRAW_FILL / OPAQUE / BLEND / BLACKNESS / WHITENESS / INVERS / TRANSPARENT / ALPHA_BLEND / COPY_BITS) so BIOS, GRUB, and kernel-console rendering works; add `warn_once!` + `--pedantic` bug-report-per-gap instrumentation on top | Complete | [1. Plumbing](PLAN-display-draw-ops-phase-01-plumbing.md), [2. DRAW_FILL](PLAN-display-draw-ops-phase-02-fill.md), [3. Monochrome](PLAN-display-draw-ops-phase-03-monochrome.md), [4. COPY_BITS](PLAN-display-draw-ops-phase-04-copy-bits.md), [5. Image rop](PLAN-display-draw-ops-phase-05-image-rop.md), [6. Alpha](PLAN-display-draw-ops-phase-06-alpha.md), [7. Invers + warnings](PLAN-display-draw-ops-phase-07-invers-and-warnings.md), [8. Pedantic](PLAN-display-draw-ops-phase-08-pedantic.md), [9. Pedantic handles](PLAN-display-draw-ops-phase-09-pedantic-handles.md), 10. Docs (inline) |
| 2026-04-23 | [Android APK port](PLAN-android-apk.md) | Concept plan for a sideloadable Android APK of ryll, targeting the Google TV Streamer as a thin-client SPICE endpoint | Proposed (concept) | (phases not yet written) |
| 2026-04-23 | [Bug-report trigger snapshot](PLAN-bugreport-trigger-snapshot.md) | Capture the display surface when the bug dialog opens, not at submit, so transient artefacts survive the form-filling delay | Complete | [1. Metadata](PLAN-bugreport-trigger-snapshot-phase-01-metadata.md), [2. Snapshot](PLAN-bugreport-trigger-snapshot-phase-02-snapshot.md), [3. Region image](PLAN-bugreport-trigger-snapshot-phase-03-region.md), [4. Docs](PLAN-bugreport-trigger-snapshot-phase-04-docs.md) |
| 2026-04-25 | [Paste-as-keystrokes fallback](PLAN-paste-as-keystrokes.md) | Synthesise SPICE keystrokes for the contents of a string when no vdagent is available, so guests like uncalibrated-sextant can be driven without an in-guest agent | Complete | [1. Translator](PLAN-paste-as-keystrokes-phase-01-translator.md), [2. Channel + CLI](PLAN-paste-as-keystrokes-phase-02-channel-cli.md), [3. GUI gesture](PLAN-paste-as-keystrokes-phase-03-gui.md), [4. Docs](PLAN-paste-as-keystrokes-phase-04-docs.md) |
| 2026-04-25 | [Hamburger menu](PLAN-hamburger-menu.md) | Replace the status-bar action-button row with a single hamburger menu so the row returns to glanceable state | Complete | [1. Migrate](PLAN-hamburger-menu-phase-01-migrate.md) |
| 2026-04-25 | [Notifications system](PLAN-notifications.md) | In-app notifications surface for protocol gaps, bug-report status, and SPICE_MSG_NOTIFY messages that ryll currently drops on the floor | Complete | [1. Store](PLAN-notifications-phase-01-store.md), [2. SPICE_MSG_NOTIFY](PLAN-notifications-phase-02-spice-notify.md), [3. Existing sources](PLAN-notifications-phase-03-existing-sources.md), [4. GUI](PLAN-notifications-phase-04-gui.md), [5. Docs](PLAN-notifications-phase-05-docs.md) |
| 2026-04-27 | [PR #31 follow-up](PLAN-pr31-followup.md) | Tests, docs, and polish items deferred when landing PR 31 (UI improvements, reconnect, protocol fixes) | Complete | [1. Tests](PLAN-pr31-followup-phase-01-tests.md), [2. Docs](PLAN-pr31-followup-phase-02-docs.md), [3. Polish](PLAN-pr31-followup-phase-03-polish.md) |
| 2026-04-28 | [Connection properties](PLAN-connection-properties.md) | Connection properties dialog showing server endpoint, protocol version, channels, negotiated capabilities, and display surfaces | Not started | 1. Plumbing, 2. Dialog, 3. Cap names, 4. Docs (phase plans pending) |
| 2026-04-30 | [Display window sizing](PLAN-display-window-sizing.md) | Track guest surface size on every SURFACE_CREATE (not just the first), with a hamburger toggle to opt out | Complete | [1. Always-fit](PLAN-display-window-sizing-phase-01-always-fit.md), [2. Toggle](PLAN-display-window-sizing-phase-02-toggle.md), [3. Tests](PLAN-display-window-sizing-phase-03-tests.md), [4. Docs](PLAN-display-window-sizing-phase-04-docs.md), [5. Notify](PLAN-display-window-sizing-phase-05-notify.md) |
| 2026-05-01 | [Web frontend](PLAN-web-frontend.md) | A `--web` mode of ryll that transcodes SPICE to a browser over WebRTC, so a desktop can be reached from any modern browser, with the goal of dogfooding ryll in place of Kasm + Apache Guacamole | In progress | 0. Parity audit (complete; `docs/multi-mode-parity.md`), [1. Renderer extraction](PLAN-web-frontend-phase-01-extract.md) (complete), [2. Encoder](PLAN-web-frontend-phase-02-encoder.md) (complete; H.264 path), [3. WebRTC](PLAN-web-frontend-phase-03-webrtc.md) (complete), [4. HTTP server + token](PLAN-web-frontend-phase-04-server.md) (complete; synthetic source), [5. Inputs/cursor/audio](PLAN-web-frontend-phase-05-iac.md) (complete), 6. Reconnect, 7. CI, 8. Docs (phase plan files not yet written) |

## Standalone plans

These plans track issues, follow-ups, or deferred work without phased
execution.

| Date | Plan | Intent |
|------|------|--------|
| 2026-04-01 | [Remaining issues](PLAN-remaining-issues.md) | Outstanding issues after the initial Rust port bring-up |
| 2026-04-08 | [Display iteration follow-ups](PLAN-display-iteration-followups.md) | Deferred work from display rendering, QUIC decode, and multi-monitor PRs |
| 2026-04-11 | [PR #20 follow-up](PLAN-pr20-followup.md) | Follow-up fixes from clipboard, MJPEG, and disconnect handling |
| 2026-04-11 | [PR #23 follow-up](PLAN-pr23-followup.md) | Follow-up fixes from audio playback channel integration |
| 2026-04-18 | [Supply-chain scanning](PLAN-supply-chain-scanning.md) | Deterministic scanners for dependencies, secrets, and Unicode-based attacks |
| 2026-04-18 | [Supply-chain follow-ups](PLAN-supply-chain-followups.md) | Tracked advisory ignores and unmaintained-crate debt surfaced when landing scanners |
| 2026-04-23 | [Macbook bug-report fixes](PLAN-macbook-bugreport-fixes.md) | MOUSE_MODE wire format, client-mode re-request after guest reboot, and MULTI_MEDIA_TIME handler |

## Consolidation plans

These plans collate deferred work from multiple sources into
a single execution sequence.

| Date | Plan | Intent | Status | Phases |
|------|------|--------|--------|--------|
| 2026-04-16 | [Deferred debt](PLAN-deferred-debt.md) | Pay down correctness bugs, robustness gaps, code quality, tests, and docs across all completed plans | Complete | [1. Display](PLAN-deferred-debt-phase-01-display.md), [2. Audio](PLAN-deferred-debt-phase-02-audio.md), [3. Session](PLAN-deferred-debt-phase-03-session.md), [4. Robustness](PLAN-deferred-debt-phase-04-robustness.md), [5. Cleanup](PLAN-deferred-debt-phase-05-cleanup.md), [6. Tests](PLAN-deferred-debt-phase-06-tests.md), 7. Docs (inline) |

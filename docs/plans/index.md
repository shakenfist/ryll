# Plans index

This page summarises every planning document in chronological order. Master
plans decompose work into numbered phases, each with its own detailed plan
file. Standalone plans track issues, follow-ups, or design decisions that
do not require phased execution.

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
| 2026-04-19 | [Idle CPU and latency](PLAN-idle-cpu-and-latency.md) | Investigate 6-core idle CPU usage; replace broken keystroke latency with PING/PONG-based measurement; demote noisy protocol logging | Not started | 1. Profile, 2. Repaint, 3. Logging, 4. Latency (phase plans pending) |

## Standalone plans

These plans track issues, follow-ups, or deferred work without phased
execution.

| Date | Plan | Intent |
|------|------|--------|
| 2026-04-01 | [Remaining issues](PLAN-remaining-issues.md) | Outstanding issues after the initial Rust port bring-up |
| 2026-04-08 | [Display iteration follow-ups](PLAN-display-iteration-followups.md) | Deferred work from display rendering, QUIC decode, and multi-monitor PRs |
| 2026-04-11 | [PR #20 follow-up](PLAN-pr20-followup.md) | Follow-up fixes from clipboard, MJPEG, and disconnect handling |
| 2026-04-11 | [PR #23 follow-up](PLAN-pr23-followup.md) | Follow-up fixes from audio playback channel integration |

## Consolidation plans

These plans collate deferred work from multiple sources into
a single execution sequence.

| Date | Plan | Intent | Status | Phases |
|------|------|--------|--------|--------|
| 2026-04-16 | [Deferred debt](PLAN-deferred-debt.md) | Pay down correctness bugs, robustness gaps, code quality, tests, and docs across all completed plans | Complete | [1. Display](PLAN-deferred-debt-phase-01-display.md), [2. Audio](PLAN-deferred-debt-phase-02-audio.md), [3. Session](PLAN-deferred-debt-phase-03-session.md), [4. Robustness](PLAN-deferred-debt-phase-04-robustness.md), [5. Cleanup](PLAN-deferred-debt-phase-05-cleanup.md), [6. Tests](PLAN-deferred-debt-phase-06-tests.md), 7. Docs (inline) |

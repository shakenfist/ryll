# ryll --web operator guide

`ryll --web` exposes a SPICE session as an HTTP endpoint
serving a browser shell that talks to the SPICE server via
WebRTC. Single-viewer for MVP; multi-viewer is future work.

## Quick start

    ryll --web session.vv

Optional flags:

- `--web-host 127.0.0.1` — bind address. Defaults to
  loopback; use `0.0.0.0` for LAN access.
- `--web-port 0` — TCP port. Defaults to ephemeral.

The binary prints a URL with a per-launch token:

    ryll: serving web frontend at http://127.0.0.1:34567/?token=abc...

Open the URL in Firefox or Chrome. The browser fetches the
embedded HTML/JS/CSS shell, opens an `RTCPeerConnection`,
exchanges SDP via `POST /offer`, and starts streaming.

## What works

- **Display**: SPICE display channel rendered in the browser
  via H.264 over WebRTC.
- **Inputs**: keyboard and mouse from the browser to SPICE.
- **Cursor**: rendered as a `<img>` overlay above the
  `<video>`; the host browser cursor is hidden.
- **Audio**: Opus passthrough from SPICE (no re-encoding) when
  the server negotiated Opus. PCM-only SPICE servers currently
  produce silent audio (a warning is logged).
- **Resolution**: the SPICE guest resizes to match the browser
  viewport at connect time (via vdagent
  `VDAgentMonitorsConfig`).
- Ctrl-C cleanly stops the binary.

## Reconnect behaviour

Phase 6 makes `--web` mode resilient to browser disconnects.

### Browser tab close → reopen

When the browser tab is closed (or the network between the
browser and ryll drops), the server-side bridge reaper
notices the `RTCPeerConnection` reaching a terminal state
(`Failed`, `Disconnected`, or `Closed`) within ~1 second.
The reaper:

1. Takes the bridge out of the active slot and closes it,
   tearing down the DTLS/SRTP state.
2. Calls `EncoderInfra::stop()` so the H.264 encoder task
   exits and CPU usage drops to idle.
3. Clears the audio pump.

The **SPICE session is left untouched**. Reopening the same
URL at any time establishes a fresh `RTCPeerConnection` via a
new `/offer` round-trip; the encoder restarts, requests a
keyframe, and the guest desktop appears within a few frames.

### Browser-side auto-reconnect

On transient ICE or connection-state failures the browser
retries automatically with exponential backoff:

| Attempt | Delay |
|---------|-------|
| 1 | 1 s |
| 2 | 2 s |
| 3 | 4 s |
| 4 | 8 s |
| 5 | 16 s |

After 5 failed attempts the status overlay shows
"Disconnected. Click to reconnect." and a button lets the
operator trigger a manual retry.

Each attempt constructs a brand-new `RTCPeerConnection` (no
stale SDP cache), resets the backoff counter on a successful
`Connected` transition, and retriggers the viewport-resize
message so the guest resolution re-syncs.

### Graceful shutdown

Ctrl-C or SIGTERM drains the axum HTTP server (existing
graceful-shutdown path) then explicitly closes any active
bridge before the process exits, ensuring DTLS/SRTP state
tears down cleanly.

## Limitations (MVP)

- Single viewer at a time. A second offer replaces the
  existing connection.
- Plain HTTP only. The transport itself (WebRTC's
  DTLS-SRTP) is encrypted by the protocol; the signalling
  page is plain HTTP and intended for trusted-LAN use only.
- No clipboard sync, USB redirection, or folder sharing
  (out of MVP scope).
- No multi-monitor (single video track, single primary
  surface).
- Browser audio autoplay policy: click the volume button on
  the page to enable sound after the page loads.

## Security note

The MVP listens on plain HTTP with a single per-launch
token in the URL. The token is sufficient to defeat casual
port-scanning but is not a substitute for HTTPS. Intended
for trusted-LAN deployment (operator's own machine or a
LAN-connected workstation). For exposure beyond a trusted
LAN, wait for Phase 8's TLS support — or front the server
with an HTTPS reverse proxy.

## Pending phases

Phases 0–6 of the web-frontend plan are complete.
The following phases are still pending:

- **Phase 7 (CI + packaging)**: verify the `--web` dependencies
  build cleanly on Linux, macOS, and Windows in CI.
- **Phase 8 (Operator docs)**: expand this guide with a systemd
  unit example, troubleshooting section, TLS configuration,
  and security hardening notes.

See `docs/plans/PLAN-web-frontend.md` for the master plan and
individual phase plan files for implementation details.

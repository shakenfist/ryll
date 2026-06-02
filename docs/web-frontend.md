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

A Disconnect button is rendered alongside the audio-enable
button in the top-right corner once the connection is
established. Clicking it closes the `RTCPeerConnection`,
suppresses auto-reconnect, and reveals the manual reconnect
button — useful for test harnesses (e.g. `./bin/runtest.sh`)
that need a deterministic exit from the browser session
before tearing down the wider scenario.

### `--web-exit-on-disconnect` (single-shot mode)

By default, the bridge reaper tears down only the WebRTC
layer when the viewer disconnects: the SPICE session stays
live and the HTTP server keeps listening, so a subsequent
browser visit picks up where the previous one left off.
That's the right default for production but unhelpful for
scripted tests that need ryll to exit so their session
logger can do its work.

Pass `--web-exit-on-disconnect` to flip the behaviour:
after the first viewer disconnects, the reaper raises the
process-wide `SHUTDOWN_REQUESTED` flag, the axum watcher
drains the HTTP server, and `run_web`'s post-server
cleanup tears down the bridge, encoder, and SPICE
session before the process exits. Typical use:

```bash
ryll --web --web-exit-on-disconnect session.vv
```

This pairs naturally with the in-page Disconnect button:
the test driver navigates to the URL, performs its
checks, clicks Disconnect (or scripts a click via WebDriver
etc.), and ryll exits without further intervention.

Each attempt constructs a brand-new `RTCPeerConnection` (no
stale SDP cache), resets the backoff counter on a successful
`Connected` transition, and retriggers the viewport-resize
message so the guest resolution re-syncs.

### Graceful shutdown

Ctrl-C or SIGTERM drains the axum HTTP server (existing
graceful-shutdown path) then explicitly closes any active
bridge before the process exits, ensuring DTLS/SRTP state
tears down cleanly.

## Encoder quality

### Defaults

The H.264 encoder is pinned for screen content rather than camera
video. The openh264 library defaults target video conferencing
(usage type `CameraVideoRealTime`); VDI desktops — sharp text,
largely static content with occasional full-screen redraws — sit at
the opposite end of the design space. The explicit configuration
applied by ryll is:

- **Usage type**: `ScreenContentRealTime`. Tells the encoder to
  treat the source as a screen feed and tune its spatial and
  temporal decision-making accordingly.
- **Rate control**: Quality mode with QP range 18–36. Rate control
  stays quality-based so picture quality is consistent across
  frame types; the QP ceiling of 36 prevents the encoder from
  sacrificing fine detail when motion picks up.
- **Profile / level**: High, 4.2. Gives the encoder access to
  the full cabac entropy coder and 8x8 transforms.
- **IDR cadence**: 60 frames (2 s at 30 Hz). Reconnecting viewers
  get a fully self-contained keyframe within 2 s at most.
- **Max frame rate**: 30 Hz.
- **Frame skipping**: disabled. VDI users notice dropped frames
  as cursor stutter; the encoder skips nothing and the decoder
  stays in step.

### `--web-encoder-bitrate-kbps`

```
ryll --web --web-encoder-bitrate-kbps 15000 session.vv
```

Default: 15000 (15 Mbps). Sets the upper bitrate guide-rail for
the Quality rate-control mode. In Quality RC the encoder sends
less data when the scene is static and more on high-motion frames,
up to this ceiling. It is not a constant bitrate target — the
encoder will never produce more than this ceiling per second, but
will typically produce far less on typical desktop content.

When to adjust:

- **Constrained WAN link**: drop to 5000–7000 (5–7 Mbps) to
  avoid filling the link on hard frames.
- **LAN deployment**: the default 15 Mbps is a comfortable
  ceiling; most desktop sessions stay well under it.
- **Diagnostic floor**: if the picture is unexpectedly poor at
  a given bitrate ceiling, check with `--verbose` whether the
  encoder is reporting quality degradation warnings.

### Adaptive bitrate

Phase 2 of the encoder-quality plan
(`docs/plans/PLAN-web-encoder-quality-phase-02-adaptive.md`)
shipped a closed-loop adaptive bitrate controller that drives the
H.264 encoder target to match the available network capacity
between the server and the browser.

#### How the control loop works

The browser samples `RTCPeerConnection.getStats()` at 1 Hz. On
each sample it walks the stats map, finds the nominated,
succeeded candidate-pair entry, and reads
`availableOutgoingBitrate` from it. The raw value is
EMA-smoothed with alpha 0.4. A `{type:'bandwidth', kbps:N}`
message is sent over the control datachannel only when the
smoothed value has moved more than 10 % from the last value
reported, suppressing encoder rebuilds on bandwidth noise.

On the server side, `BrowserMsg::Bandwidth` in
`ryll/src/web/inputs.rs` receives the message and forwards it
as `EncoderControl::SetBitrate(kbps)` to the encoder task
(`shakenfist-spice-renderer/src/encoder/task.rs`). The encoder
task:

1. Clamps the requested value into
   `[500, --web-encoder-bitrate-kbps ceiling]`. The floor of
   500 kbps prevents the estimator from locking the stream into
   an unusable state during transient congestion. The ceiling is
   the operator-supplied value (or the default 15 000 kbps),
   snapshotted at task start.
2. Applies a 10 % hysteresis band on the *currently active*
   bitrate (not on the most-recent request), so a noisy
   estimate oscillating around a threshold does not repeatedly
   rebuild the encoder.
3. On an accepted change, calls `H264Encoder::set_bitrate(bps)`,
   which rebuilds the inner openh264 encoder and sets
   `keyframe_pending = true`. The next encoded frame is an
   implicit IDR.

#### Observability

In-page: a small monospace overlay at the bottom-left of the
browser window reads `<kbps> kbps · <rtt> ms` and updates once
per second while the data channel is open. The bandwidth value
is the smoothed estimate the browser pushes to the server
(updated every tick, not just on band-crossings, so the number
moves more smoothly than the server log). The RTT comes from
`RTCIceCandidatePairStats.currentRoundTripTime` on the same
nominated candidate pair. Either cell shows `—` when the
browser has not populated the field — typical for
`availableOutgoingBitrate` on Firefox / Safari. This HUD is a
down-payment on the bottom status bar planned in
`docs/plans/PLAN-web-ui-convergence.md`; the convergence work
will subsume it when it lands.

Browser console: the sampler logs
`console.debug('[ryll] bandwidth kbps=', N)` on every send to
the server.

Server side: every accepted bandwidth message is logged at
`info!` level:

    web inputs: browser bandwidth estimate N kbps

This appears in ryll's stdout/stderr without `--verbose`. On a
stable link the band-crossing filter typically suppresses most
messages; on a fluctuating link messages appear each time the
estimate moves more than 10 % from the last applied value.

To verify the loop end-to-end, tail the ryll output while the
browser is connected and confirm the log line fires and the
estimate tracks the link.

#### Browser compatibility

`availableOutgoingBitrate` is an optional field. Chrome's GCC
bandwidth estimator populates it reliably. Firefox and Safari
may omit it in some configurations. When the field is absent,
`sampleBandwidth()` returns early without sending a message;
the encoder continues at whatever bitrate was last applied —
typically the operator-set ceiling. Firefox and Safari users
therefore receive the static-bitrate behaviour described in the
`--web-encoder-bitrate-kbps` section above.

#### Validating the loop under constrained bandwidth

To confirm the adaptive loop responds to a real link constraint,
throttle the network interface with `tc`:

    tc qdisc add dev <iface> root tbf rate 5mbit burst 32kbit latency 400ms

With a 5 Mbps cap in place, the browser's bandwidth estimate
should converge below 5 000 kbps within a few seconds and ryll's
log should show the estimate declining. The picture should remain
usable rather than freezing. To remove the throttle:

    tc qdisc del dev <iface> root

Replace `<iface>` with the interface the browser uses to reach
ryll (e.g. `eth0`). These commands require root or `CAP_NET_ADMIN`.
The `tc` tool ships with `iproute2` on most Linux systems.

## Limitations (MVP)

- Single viewer at a time. A second offer replaces the
  existing connection.
- No clipboard sync, USB redirection, or folder sharing
  (out of MVP scope).
- No multi-monitor (single video track, single primary
  surface).
- Browser audio autoplay policy: click the volume button on
  the page to enable sound after the page loads.

## Native TLS

ryll supports HTTPS natively via two flags:

    ryll --web session.vv \
        --web-tls-cert /path/to/cert.pem \
        --web-tls-key  /path/to/key.pem

Both flags must be supplied together; clap rejects one
without the other at parse time. Omitting both keeps the
default plain-HTTP behaviour.

**Accepted formats**: PEM-encoded certificate chain
(`cert.pem`) and PEM-encoded private key (`key.pem`).
A chain file should contain the leaf certificate first,
followed by any intermediate CA certificates.

When TLS is active the startup URL line prints `https://`:

    ryll: serving web frontend at https://0.0.0.0:8443/?token=...

**Cert rotation (MVP)**: ryll does not support inline cert
reload. To rotate a certificate, replace the files on disk
then restart the process (or `systemctl restart ryll-web`).
The URL token changes on each restart.

**Security layers**: WebRTC's media path is always
encrypted by DTLS-SRTP at the protocol level, regardless
of whether the signalling page is over HTTPS. Native TLS
protects the URL token and the signalling page (`GET /`,
`POST /offer`). DTLS-SRTP protects the audio/video media
stream. The two layers are complementary: use native TLS
for the signalling path whenever the traffic crosses any
untrusted network.

## Cert recipes

### mkcert (LAN dev)

[mkcert](https://github.com/FiloSottile/mkcert) installs a
local CA into your machine's trust store so browsers on
that machine accept the generated cert without a warning:

    mkcert -install
    mkcert ryll.lan 192.168.1.10

Pass the resulting `.pem` files directly to
`--web-tls-cert` and `--web-tls-key`.

### certbot (public DNS)

For a host with a public DNS A record and ports 80/443
reachable:

    certbot certonly --standalone -d ryll.example.com

Certs land at
`/etc/letsencrypt/live/ryll.example.com/fullchain.pem`
and `.../privkey.pem`. Auto-renewal:

    # /etc/cron.d/certbot-renew (or use certbot's timer)
    0 3 * * * root certbot renew --quiet \
        --deploy-hook "systemctl restart ryll-web"

### openssl one-off (self-signed)

For a one-afternoon diagnostic session where a browser
warning is acceptable:

    openssl req -x509 -newkey rsa:2048 \
        -keyout key.pem -out cert.pem \
        -days 30 -nodes -subj "/CN=ryll.lan"

The browser will show an untrusted-cert warning. Proceed
by adding a permanent exception, or use mkcert instead.

### Internal CA

For org-managed PKI, request a cert from your internal CA
and follow your org's cert-issuance documentation. The
output should be a PEM chain file and a PEM key file,
which pass directly to `--web-tls-cert`/`--web-tls-key`.

## Reverse-proxy fallback

If you already terminate TLS at a reverse proxy for
unrelated reasons, you can pass the plain-HTTP URL
through to ryll and let the proxy handle HTTPS. Native TLS
is recommended for new deployments — the reverse-proxy
path is documented here as a fallback only.

**Caddy** (autocert handles the cert lifecycle):

```caddy
ryll.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

That is the entire config for a publicly-reachable
deployment with an A record pointing at the host. Caddy
talks to Let's Encrypt automatically.

**nginx** (operator manages the cert separately):

```
proxy_pass http://127.0.0.1:8080;
```

A full nginx server block follows the standard
`proxy_pass` + `ssl_certificate` / `ssl_certificate_key`
pattern.

> **Important — WebRTC media is NOT proxied.**
>
> ICE candidates emitted by ryll point at ryll's host and
> port directly. The browser opens UDP flows to that
> endpoint — they never go through the reverse proxy.
> The proxy carries only the HTTP signalling page and the
> `POST /offer` request.
>
> Consequences:
>
> - **ryll's UDP port range must be reachable from the
>   browser.** Open the relevant firewall ports on ryll's
>   host. The ephemeral RTP port range is chosen by the OS
>   (typically 32768–60999 on Linux).
> - **Bind ryll to the public-facing IP, not loopback,**
>   when the proxy listens on a different IP. Use
>   `--web-host 0.0.0.0` or the specific public IP.
>   If ryll is bound to `127.0.0.1`, the ICE candidates
>   it advertises will be loopback-only and the browser
>   cannot reach them.

## Troubleshooting

### Page loads, video stays black for >10 seconds

**Likely causes:**

- **ICE failure** — UDP between the browser and ryll is
  blocked. Open the browser DevTools console and check the
  `RTCPeerConnection` connection state. If it is stuck on
  `connecting`, ICE negotiation has not completed.
  Fix: ensure ryll's UDP port range is reachable from the
  browser host (firewall / security-group rules). If ryll
  is behind a reverse proxy, see the callout in the
  Reverse-proxy fallback section above.

- **Encoder didn't start** — `RTCPeerConnection` reached
  `connected` but no frames arrived. Check ryll's stderr
  for encoder errors. The encoder requests a keyframe on
  the `Connected` transition; the first frame may take up
  to ~1 second. If no frame arrives after 10 seconds,
  the encoder task is wedged — restart ryll and file a
  bug.

### No audio, video works

**Likely causes:**

- **Browser autoplay policy** — the `<video>` element is
  muted by default to satisfy autoplay rules. Click the
  volume button on the page to enable audio.

- **PCM-only SPICE server** — ryll does Opus passthrough
  only in MVP. If the SPICE server negotiated PCM playback
  (no Opus), ryll logs a warning and audio will be silent
  until a future PCM→Opus encoder lands.

### "Click to reconnect" loop

The browser retries automatically five times with
exponential backoff, then shows a manual button (Phase 6).
If the button appears every time you reconnect, check
ryll's logs for:

    bridge reaper: bridge died, reaping

If this line is absent, the reaper task may not be running
or the bridge is not reaching a terminal state. Restart
ryll and file a bug with the full log.

### High CPU when no browser is connected

This should not happen on Phase 6+ ryll. The bridge reaper
drops the H.264 encoder when the browser disconnects, so
CPU usage returns to near-idle. If you observe sustained
high CPU with no active browser session, check that the
reaper task is reaching the dead-bridge signal in the logs.
If it is absent, file a bug with ryll version and log.

### Cert load errors at startup

ryll prints a clear error chain on cert-load failure, for
example:

    Error: loading --web TLS cert/key from /etc/ryll/tls/cert.pem /
    /etc/ryll/tls/key.pem: ...

Common causes and fixes:

- **File permissions** — the ryll process must be able to
  read both files. Fix:

      chown ryll:ryll cert.pem key.pem
      chmod 0600 key.pem

- **Malformed PEM** — the file is not valid PEM. Re-export
  the cert/key from your CA or regenerate with openssl.

- **Mismatched cert/key** — the public key in the cert
  does not match the private key. Verify they were
  generated together.

### Browser shows cert warning

The certificate is self-signed or the browser does not
trust the issuing CA. Options:

- Use mkcert (see Cert recipes), which installs its CA
  into the system trust store automatically.
- Install your internal CA's root cert into the browser's
  trust store.
- Accept the browser warning for one-off / diagnostic
  access (the media path is still DTLS-SRTP encrypted).

### Ctrl-C ignored (historic, pre-Phase 6)

Phase 6's `with_graceful_shutdown` / `Handle::graceful_shutdown`
path fixed a race where Ctrl-C was delivered before the
axum server was ready to drain. If you see this on a
current ryll build, file a bug. Update to ryll ≥ Phase 6.

## Security note

ryll supports native HTTPS via `--web-tls-cert` /
`--web-tls-key` — this is the recommended deployment for
any traffic that crosses a network you do not fully
control. Plain HTTP is acceptable only for loopback-only
(`--web-host 127.0.0.1`, the default) or fully-trusted-LAN
deployments where the URL token is the only sensitive
material on the wire and you control all endpoints.

In all cases, the WebRTC media path (audio and video) is
encrypted at the protocol level by DTLS-SRTP, independent
of whether the signalling page is served over HTTPS.

## Service mode

For long-lived deployments, run ryll under systemd so it restarts
automatically on failure and logs go to the journal.

A reference unit file is at [`examples/ryll-web.service`](../examples/ryll-web.service).
Copy it to `/etc/systemd/system/ryll-web.service`, then:

    systemctl daemon-reload
    systemctl enable --now ryll-web

### User and group

Create a dedicated unprivileged account:

    useradd -r -s /usr/sbin/nologin ryll

The unit runs as `User=ryll Group=ryll`.

### EnvironmentFile

The unit reads `/etc/ryll/web.env` so you can tune all parameters
without touching the unit file. Create it with owner `root:ryll`,
mode `0640`:

    install -d -o root -g ryll -m 750 /etc/ryll
    install -o root -g ryll -m 640 /dev/null /etc/ryll/web.env

Example `/etc/ryll/web.env`:

    VV_FILE=/etc/ryll/session.vv
    WEB_HOST=0.0.0.0
    WEB_PORT=8443
    WEB_TLS_CERT=/etc/ryll/tls/cert.pem
    WEB_TLS_KEY=/etc/ryll/tls/key.pem

The `.vv` file should be readable only by the ryll user:

    install -o ryll -g ryll -m 600 /dev/null /etc/ryll/session.vv

### Cert file permissions

The TLS key must be readable by the `ryll` user:

    chown ryll:ryll /etc/ryll/tls/cert.pem /etc/ryll/tls/key.pem
    chmod 0600 /etc/ryll/tls/key.pem

### Extracting the per-launch URL

ryll prints its URL with the per-launch token directly to stdout
(not via the tracing pipeline, so the token never reaches journald
or log aggregators). Under systemd, stdout is captured in the
journal only if the unit uses `StandardOutput=journal`. With the
default `StandardOutput=inherit` the URL goes to the terminal where
you launched the service. To read it from the journal when it is
captured there:

    journalctl -u ryll-web -n 50 --no-pager \
        | grep -oE 'https?://[^ ]+token=[^ ]+' | tail -1

The URL includes the token and is valid until the service restarts.

### Graceful shutdown

`KillSignal=SIGTERM` causes `systemctl stop ryll-web` to send SIGTERM.
This engages Phase 6's graceful-shutdown path (`with_graceful_shutdown`
/ `Handle::graceful_shutdown`), which drains in-flight HTTP requests
and tears down any active WebRTC bridge cleanly. `TimeoutStopSec=10s`
is a generous ceiling; normal shutdown completes within ~5 seconds.

### Cert rotation (MVP)

ryll does not support inline cert reload. To rotate a certificate:

    # Install new cert/key into /etc/ryll/tls/, then:
    systemctl restart ryll-web

The URL token changes on each restart. Extract the new URL from the
journal using the recipe above.

### Hardening note

`ProtectSystem=strict` + `ReadOnlyPaths=/etc/ryll` prevents writes
outside the declared paths. If you use `--log-file` to write logs to
disk, add `ReadWritePaths=/var/log/ryll` (or your chosen path) to the
unit's `[Service]` section to relax the restriction.

## CI smoke test

`tools/web-smoke.sh` runs on every Linux PR in CI. It
launches `ryll --web` with a stub `.vv` file (pointing at a
non-existent SPICE server on a local nc listener), waits 3
seconds to verify the process has not exited prematurely,
sends SIGTERM, and asserts that ryll exits cleanly within
5 seconds. This catches regressions in HTTP-server
startup, rustls provider install, and SIGTERM handling
without requiring a real SPICE session.

macOS and Windows CI builds verify the `--web` dependencies
link correctly but do not run the smoke test (runtime smoke
is Linux-only for the MVP; see `docs/portability.md`).

## Project status

All 8 phases of the web-frontend plan are complete. The
`--web` mode ships end-to-end: display, audio, inputs,
cursor, reconnect, CI packaging, native TLS, and operator
documentation. See `docs/plans/PLAN-web-frontend.md` for
the full history.

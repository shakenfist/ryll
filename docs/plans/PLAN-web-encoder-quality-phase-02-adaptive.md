# Phase 2 — Adaptive bitrate via WebRTC bandwidth estimate

Master plan: [PLAN-web-encoder-quality.md](PLAN-web-encoder-quality.md).
Depends on phase 1 having landed (provides the
`EncoderQuality` struct and the `--web-encoder-bitrate-kbps`
ceiling).

## Goal

Make the H264 encoder track the actual browser→server link
capacity. The browser samples
`RTCPeerConnection.getStats()` once per second, picks
`availableOutgoingBitrate` off the active candidate pair,
EMA-smooths it, and signals the server over the existing
control DataChannel. The server forwards the value as a
new `EncoderControl::SetBitrate(u32 kbps)` to the
encoder task, which rebuilds the inner encoder at the new
bitrate.

## Non-goals

- Replacing Quality rate-control with bitrate-target rate
  control (we keep RC=Quality; bitrate is still an upper
  guide rail).
- Server-side `webrtc-rs` stats as a primary signal
  source (deferred — fallback only, future work).
- Live `set_option(ENCODER_OPTION_BITRATE)` via raw_api
  (deferred — rebuild is good enough for v1; see master
  plan Q2).
- ROI / region-quality encoding.

## Design notes

### Browser → server wire format

Add one message type to the existing browser→server
control DC stream (the same channel that already carries
inputs and viewport):

```js
// JS sends this once per second (after EMA passes the
// 10 %-band-crossing filter):
{ type: 'bandwidth', kbps: 7500 }
```

On the server side, the existing `BrowserMsg` enum in
`ryll/src/web/inputs.rs` gets a new variant
`Bandwidth { kbps: u32 }` and the dispatcher forwards it
via the encoder-control mpsc.

### EncoderControl extension

`shakenfist-spice-renderer/src/encoder/task.rs`:

```rust
pub enum EncoderControl {
    RequestKeyframe,
    SetBitrate(u32),  // kbps, clamped at receive site
    Stop,
}
```

Handler in `run()` reads it on the same `try_recv` loop
that already handles `RequestKeyframe`. When
`SetBitrate(kbps)` arrives:

1. Clamp into `[500, ceiling_kbps]` where `ceiling_kbps`
   comes from the `EncoderQuality` set at construction.
2. If the clamped value is within 10 % of the currently
   active bitrate, ignore (avoid rebuild churn).
3. Otherwise: update the encoder's stored `EncoderQuality`
   and call `H264Encoder::set_quality(new_quality)` which
   in this phase is extended to *also* trigger an
   encoder rebuild (or call into `resize()` with the
   current dimensions to reuse the rebuild path).
4. Set `keyframe_pending = true` — the rebuilt encoder
   emits an implicit IDR on its first frame, but we want
   our state machine to know about it for stats.

### Browser-side sampling

In `ryll/src/web/assets/app.js`, after the bridge is
established, set up a 1 Hz `setInterval`:

```js
const BANDWIDTH_SAMPLE_MS = 1000;
const EMA_ALPHA = 0.4;
const BAND_CROSS_PCT = 0.10;
let bandwidthEma = null;
let lastSentKbps = null;

async function sampleBandwidth() {
    const stats = await pc.getStats();
    let bps = null;
    stats.forEach((r) => {
        if (r.type === 'candidate-pair' && r.nominated && r.state === 'succeeded') {
            if (typeof r.availableOutgoingBitrate === 'number') {
                bps = r.availableOutgoingBitrate;
            }
        }
    });
    if (bps === null) return;
    const kbps = Math.round(bps / 1000);
    bandwidthEma = bandwidthEma === null ? kbps : bandwidthEma * (1 - EMA_ALPHA) + kbps * EMA_ALPHA;
    const smoothed = Math.round(bandwidthEma);
    if (lastSentKbps === null
        || Math.abs(smoothed - lastSentKbps) / lastSentKbps > BAND_CROSS_PCT) {
        controlChannel.send(JSON.stringify({ type: 'bandwidth', kbps: smoothed }));
        lastSentKbps = smoothed;
    }
}
```

Wire `setInterval(sampleBandwidth, BANDWIDTH_SAMPLE_MS)`
once the control channel reaches `open` state.

### Notification-friendly observability

Bug reports should capture the bandwidth signal so we can
audit it after the fact. Add the last 60 samples to
whatever the bug-report metadata struct already includes.
Out of scope here is *visualising* the bandwidth in the
HUD — that belongs in PLAN-web-ui-convergence.

## Steps

| Step | Effort | Model  | Isolation | Brief for sub-agent |
|------|--------|--------|-----------|---------------------|
| 2a   | medium | sonnet | none      | Browser-side bandwidth sampling. Edit `ryll/src/web/assets/app.js`. Add a `sampleBandwidth()` async function per the *Browser-side sampling* section of the phase plan. Hook it up via `setInterval` once the control DataChannel hits `'open'` state — find where the existing input-relay setup happens and mirror the lifecycle there. Tear the interval down when the channel closes. Use `pc.getStats()` not the deprecated callback form. Iterate the stats Map via `forEach`. Match candidate pairs with `nominated=true, state='succeeded'`; ignore anything else. The control channel send call should reuse the same helper the disconnect / viewport messages use (read the file to find it). Add a `console.debug('bandwidth kbps=', smoothed)` so we can see the values in the browser devtools when verifying. **Why sonnet:** straight-line JS using a documented W3C API; the existing `app.js` patterns supply the lifecycle scaffolding. |
| 2b   | medium | sonnet | worktree  | Server-side wire-format dispatch. In `ryll/src/web/inputs.rs`, find the `BrowserMsg` enum (or whatever the per-channel-message parser lives in — read the file first) and add a `Bandwidth { kbps: u32 }` variant. Add the matching JSON serde tag (`type: "bandwidth"`). In the dispatcher that converts `BrowserMsg` into renderer-bound actions, route the new variant into an mpsc `Sender<EncoderControl>` — the dispatch site already holds (or can be threaded) the bridge's `encoder_control` sender (read `shakenfist-spice-webrtc/src/bridge.rs::WebrtcBridge` to confirm). Convert kbps→bps and send `EncoderControl::SetBitrate(kbps)`. Add unit tests in `inputs.rs` for the parse path (good JSON; missing field; out-of-range). **Why worktree:** new wire-format message type that touches both browser-side and protocol-adjacent server code; if the design turns out to be wrong we want the worktree discarded clean. **Why sonnet:** the brief is detailed; the change is well-scoped. |
| 2c   | high   | opus   | worktree  | Encoder-side adaptive reconfiguration. In `shakenfist-spice-renderer/src/encoder/task.rs`, extend the `EncoderControl` enum with `SetBitrate(u32 /* kbps */)`. In the `run()` control-message handler, when `SetBitrate(kbps)` arrives: (1) clamp into `[500, encoder.quality().target_bitrate_bps / 1000]` — the ceiling is whatever `EncoderQuality` was constructed with, which acts as the user-set upper rail; (2) if the clamped value is within 10 % of `encoder.quality().target_bitrate_bps / 1000`, ignore (band-crossing filter); (3) otherwise, call a new `H264Encoder::set_bitrate(bps)` method that updates the stored quality *and* rebuilds the inner encoder via the same path `resize()` uses. The first frame after a rebuild is implicitly an IDR — set `keyframe_pending = true` for the stats/state consistency. **Implementer judgment call:** the master plan Q2 proposes the rebuild path over the unsafe SetOption path; if while reading the openh264 0.9.3 crate you decide the rebuild approach has a problem the master plan missed, *stop and flag it* rather than silently switching to SetOption. Add tests for the SetBitrate path in `task.rs`: (a) SetBitrate changes encoder state and the next frame is a keyframe; (b) SetBitrate within the band-crossing tolerance is ignored; (c) SetBitrate above the ceiling clamps. **Why opus:** stateful encoder, runtime reconfiguration, the rebuild-vs-SetOption decision has correctness consequences. **Why worktree:** if the rebuild approach turns out to be wrong we want a clean baseline to restart from. |
| 2d   | medium | sonnet | none      | Bug-report observability. Find where bug-report metadata is assembled (search for the existing channel-state / latency capture). Add a 60-sample ring of the most recent bandwidth values (kbps) received from the browser, plus the currently active encoder bitrate. Wire it through to the bug-report JSON the existing path emits. Add a test that the ring populates correctly across multiple `Bandwidth` messages. **Why sonnet:** follows an existing ring-buffer pattern (look at how latency or channel-state ring buffers are done). |
| 2e   | low    | haiku  | none      | Run `make lint` and `make test`. Report failures verbatim; do not attempt fixes. **Why haiku:** mechanical verification. |
| 2f   | medium | sonnet | none      | Documentation pass. Update `docs/web-frontend.md` with a new "Adaptive bitrate" section: how the loop works (1 Hz sample, EMA-smoothed, 10 % band-crossing filter, clamped to [500, ceiling] kbps), how to read the bandwidth field in bug-report output, and what to expect on a deliberately-throttled link. Update `ARCHITECTURE.md` if it has a wire-format / control-DC section. Update `AGENTS.md` if it has a developer-visible enum list including `EncoderControl`. **Why sonnet:** docs prose with a clear scope. |

## Plan-level effort

Planning this phase was **medium effort**, with one
genuinely opus-worthy step (2c). The W3C `getStats()`
shape and the openh264 reconfigure constraints are
researched in the master plan; the remaining unknowns
are mechanical.

## Commit cadence

One commit per step (six commits, possibly seven if 2c
spawns a follow-up). Co-Authored-By lines must reflect
the sub-agent that produced the work.

## Test plan

Automated:
- `make test` must pass.
- New tests added by steps 2b / 2c / 2d above.

Manual (operator):
1. Build with phase 1 + phase 2. Start
   `ryll --web --web-encoder-bitrate-kbps 15000`.
2. Confirm in browser devtools console that the
   `bandwidth kbps=` debug logs fire once per second
   and that the value tracks the link (e.g. open a few
   tabs streaming video; expect the value to drop).
3. Throttle the link with `tc qdisc add dev eth0 root
   tbf rate 5mbit burst 32kbit latency 400ms` or
   equivalent. Confirm the encoder's emitted bitrate
   visibly settles near 5 Mbps (check via `ip -s link`
   throughput or via Wireshark on the WebRTC SRTP).
4. Remove the throttle; confirm the encoder recovers
   back toward the ceiling within a few seconds.
5. Pull a bug-report capture during a throttled period;
   confirm the bandwidth ring is populated and the
   active encoder bitrate field is updated.

## Risks

- **`availableOutgoingBitrate` is implementation-defined.**
  Chrome's GCC bandwidth estimator (TWCC-based) reports
  reasonable numbers; Firefox / Safari may report less
  reliable ones, or omit the field entirely. The 2a
  brief tolerates a missing field (returns early). If
  Firefox / Safari turn out to systematically report 0,
  we'll need either a TWCC sender-side fallback or to
  use the server-side webrtc-rs stats path — both fall
  into Future work in the master plan.
- **Rebuild-per-band-crossing might be too aggressive.**
  Each rebuild emits an IDR; if the link genuinely
  oscillates around a band-crossing threshold the
  encoder rebuilds repeatedly. The 10 % band filter
  should prevent this, but a hysteresis bug in the
  filter would burn bandwidth. The 2c brief includes a
  unit test for the band-crossing logic — make sure
  that test exercises the oscillation case
  (alternating 5000, 5400, 5000, 5400 kbps inputs
  should result in *one* rebuild, not four).
- **`EncoderControl::SetBitrate` arriving before the
  encoder task is alive.** The bridge already handles
  this for `RequestKeyframe` via the mpsc buffering;
  no new logic needed unless the control mpsc capacity
  is too small to absorb a stack of bandwidth messages
  during connection setup. Pick capacity 16 to be safe.
- **Browser-side interval leak.** If the
  `setInterval` isn't torn down on channel close, it
  keeps trying to send after the channel is dead and
  spams the JS console. Step 2a must teardown on close.

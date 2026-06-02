# Web-mode encoder quality and adaptive bitrate

## Prompt

Before responding to questions or discussion points in this
document, explore the ryll codebase thoroughly. Read relevant
source files, understand existing patterns (SPICE protocol
handling, channel architecture, async task model, image
decompression, egui rendering, and especially the
`shakenfist-spice-renderer/src/encoder/` H264 path and
`shakenfist-spice-webrtc/src/bridge.rs` WebRTC bridge), and
ground your answers in what the code actually does today. Do
not speculate about the codebase when you could read it
instead. Where a question touches on external concepts
(H.264 / openh264 encoder configuration, WebRTC bandwidth
estimation via `RTCPeerConnection.getStats()`, browser
H264 decoder capabilities and 4:4:4 chroma support), research
as needed to give a confident answer. Flag any uncertainty
explicitly rather than guessing.

All planning documents should go into `docs/plans/`.

Consult `ARCHITECTURE.md` for the system architecture
overview. Key references include the openh264 crate docs for
`SEncParamExt`, the WebRTC-rs `stats` module, and the W3C
`getStats()` spec (RTCIceCandidatePairStats /
RTCOutboundRtpStreamStats — `availableOutgoingBitrate` is the
field that matters).

When we get to detailed planning, I prefer a separate plan
file per detailed phase, named for the master plan with
`-phase-NN-descriptive` appended. Track sub-phases in a table
in this master plan under the Execution section.

I prefer one commit per logical change, and at minimum one
commit per phase.

## Situation

This plan started as an idea bubble from the ryll
`web-feedback` debugging sessions (008a–008g). Operators
reported the web-mode display as visibly "fuzzy" compared
to the eframe GUI's MJPEG/raw-pixel path — most obvious on
text and on UI chrome with sharp edges. The eframe path
shows pixel-accurate SPICE content; the web path runs the
same content through H264 → WebRTC → browser decoder.

`shakenfist-spice-renderer/src/encoder/h264.rs:103` calls
`openh264::encoder::Encoder::new()` with no `SEncParamExt`
configuration at all. The encoder inherits openh264's
library defaults, which target video-conferencing use cases
(low bitrate, 4:2:0 chroma, bitrate-controlled rate control).
That is the wrong corner of the design space for a VDI
display where most pixels are text and operators care about
crispness more than minimum bandwidth.

The other half of the picture is bandwidth-awareness. The
encoder currently produces frames at one fixed quality with
no feedback from the network path between ryll and the
browser. That works fine on a LAN but degrades the moment
the operator is on a slow WAN link. The web mode's
*natural* deployment is remote access — the LAN is the
unusual case, not the common one.

## Mission

Improve web-mode display quality across the full range of
expected network conditions:

1. **Quick win:** tune the openh264 encoder defaults for
   VDI use (high bitrate, QP-based RC, low-latency preset,
   ideally 4:4:4 chroma for text crispness). One-file
   change in `h264.rs`, no architecture impact, no new
   wire-format. Substantially closes the gap on LAN
   deployments where bandwidth is not the constraint.
2. **Adaptive bitrate:** wire WebRTC's bandwidth estimate
   back into the encoder via a new `EncoderControl`
   variant so the bitrate tracks the actual link.
   Per-second feedback loop from `RTCPeerConnection.getStats()`
   in the bridge → `EncoderControl::SetBitrate(u32)` →
   openh264 `SetTraceCallbackPrintTrace` / runtime
   reconfigure. The remote-access case is the *normal*
   deployment, not the edge case, so this is not optional
   long-term.

## Open questions

Everything below is a sketch and needs validating against
the codebase / openh264 / browser decoders before phase
planning.

### Encoder parameters to set explicitly (phase 1)

Each of these needs validation against what openh264 actually
exposes via the Rust crate (the binding doesn't cover every
field of `SEncParamExt`):

- **Rate control mode.** Default is bitrate-target. For
  VDI we probably want QP-based (`RC_QUALITY_MODE`) so
  quality stays consistent and bitrate floats — or a
  hybrid where we set both a max bitrate and a min
  quality. Needs testing.
- **Target bitrate.** Default appears to be ~1–2 Mbps.
  LAN deployments easily support 10–20 Mbps and the
  quality difference for text is dramatic. WAN
  deployments will be set lower by the adaptive loop in
  phase 2.
- **Profile / level.** Default is Baseline. Main /
  High give better compression efficiency and most
  browser decoders support them; needs a compatibility
  matrix.
- **Chroma subsampling.** 4:2:0 by default, 4:4:4 for
  text crispness. Browser support is the catch:
  - Chrome / Edge: 4:4:4 is supported but limited to
    certain platforms.
  - Firefox: partial support, recent versions only.
  - Safari: spotty.
  Probably a feature flag with 4:2:0 fallback rather
  than a hard switch.
- **Slice mode.** Single-slice vs multi-slice affects
  error resilience and latency. For WebRTC's reliable
  transport, single-slice is probably fine.
- **GOP structure.** IDR cadence + reference frames.
  Current code's IDR-on-encoder-restart works fine; we
  may want explicit IDR cadence (e.g. every 60 frames)
  for bandwidth-spike recovery.
- **Low-latency mode.** Disable B-frames, minimise
  reordering buffer, encode-on-receive rather than
  encode-on-bunch.

### Bandwidth measurement architecture (phase 2)

- **Where the stats sample lives.** Browser-side
  (`pc.getStats()`) gives the most accurate view of the
  link to the server. Server-side (webrtc-rs's stats)
  knows about the network but not the browser decoder's
  jitter budget. Probably we want both, with the browser
  signalling its observed bitrate over the control DC.
- **Sampling cadence.** 1 s is the W3C-typical interval.
  Faster oscillates; slower lags real link changes.
- **Smoothing.** Naive `availableOutgoingBitrate` jumps
  around; we'd want EMA or a similar low-pass.
- **Encoder reconfigure path.** openh264 supports
  runtime bitrate changes via
  `SetOption(ENCODER_OPTION_BITRATE)`. The Rust crate's
  coverage of that needs checking. If not exposed, we
  rebuild the encoder (same path as `resize()` already
  uses).
- **`EncoderControl` extension.** Add
  `EncoderControl::SetBitrate(u32 kbps)` alongside
  `RequestKeyframe` and `Stop`. The encoder task
  applies it on the next tick; rebuild encoder if
  openh264's runtime path isn't usable.
- **Browser → server signal.** Either piggyback on the
  existing control DC with a new message type
  (`{type:'bandwidth', avg_kbps}`) or read the
  server-side stats and skip the browser entirely. The
  control-DC route is simpler and works without
  webrtc-rs stats support.

### Scope boundary

- **Audio quality** is a separate question (Opus already
  has its own bitrate config; the playback channel
  passes through SPICE's negotiated bitrate). Out of
  scope.
- **HDR / 10-bit colour** is not a SPICE concept; out of
  scope.
- **Video-codec selection** (VP8/VP9/AV1 instead of
  H264) is interesting but would mean reworking the
  webrtc-rs track type. Park for later.

## Execution

To be filled in once phase 1 / phase 2 are scoped. Expected
shape:

| Phase | Plan | Status |
|-------|------|--------|
| 1. Explicit encoder defaults (quick win) | PLAN-web-encoder-quality-phase-01-defaults.md | Not started |
| 2. Adaptive bitrate via WebRTC stats | PLAN-web-encoder-quality-phase-02-adaptive.md | Not started |

Phase 1 should land before phase 2 even starts — phase 1
gives us the static-tuned baseline to measure phase 2 against,
and is also useful on its own for operators on stable links.

## Agent guidance

To be added once the plan is fleshed out. The default
execution model in `PLAN-TEMPLATE.md` applies.

## Administration and logistics

### Success criteria

Placeholder — to be defined when phase planning starts.
Likely candidates:

- The web-mode display visibly matches the eframe GUI's
  text crispness for typical desktop screenshots on a LAN
  link (subjective, but operators should stop calling it
  "fuzzy").
- A WAN link with deliberately constrained bandwidth
  (e.g. 5 Mbps cap) produces a usable display rather than
  freezing or producing severe artefacts.
- Encoder configuration is documented in
  `docs/web-frontend.md` so operators know what knobs
  exist and can override them.

### Future work

- Video codec choice. H264 is the only WebRTC codec ryll
  ships today; VP9 and AV1 are interesting future
  directions for bandwidth-sensitive deployments.
- Per-region quality. Mouse-cursor area at high quality,
  static background at low quality. Significant
  encoder-side work; not worth doing until bigger wins
  land.

### Bugs fixed during this work

(Empty.)

### Documentation index maintenance

When this plan moves past placeholder status, update
`docs/plans/index.md` and `docs/plans/order.yml`.

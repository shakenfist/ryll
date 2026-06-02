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
overview. Key references include the openh264 crate
(`.cargo-cache/registry/.../openh264-0.9.3/src/encoder.rs`)
for the `EncoderConfig` builder API actually available to us,
the WebRTC-rs `stats` module, and the W3C `getStats()` spec
(`RTCOutboundRtpStreamStats` — `availableOutgoingBitrate` is
the field that matters; for the browser→server signal we
read it off the *receiver* via `RTCInboundRtpStreamStats`
and the matching `RTCIceCandidatePairStats`).

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

`shakenfist-spice-renderer/src/encoder/h264.rs:103` and
`:147` (the `resize()` rebuild path) call
`openh264::encoder::Encoder::new()` with no `EncoderConfig`
at all. That means the encoder inherits the crate's defaults:

- `target_bitrate: 120 kbps` — laughably low for a VDI
  desktop; produces visible block artefacts on anything
  beyond a static text screen.
- `usage_type: CameraVideoRealTime` — tells openh264 to
  optimise for camera content (smooth motion, soft edges,
  noise). VDI is the opposite: static frames, sharp text,
  hard edges. There is a `ScreenContentRealTime` variant
  that biases the encoder toward screen content.
- `rate_control_mode: Quality` — this *is* already the
  default in the 0.9.3 crate, despite what the original
  bubble assumed. Keeping it but pinning the QP range
  closer to the high-quality end (e.g. 18–32 instead of
  the default 0–51) is the actual lever.
- `profile: None` (encoder picks Baseline). Main/High give
  better compression efficiency and every browser shipped
  in the last decade decodes them. High444 in particular
  would help text crispness, but see below — we can't
  reach it.
- No explicit `intra_frame_period`, so IDRs are emitted
  only on `force_intra_frame()` / encoder rebuild. Adding
  a periodic IDR cadence (e.g. every 2 s) gives bandwidth
  spikes a recovery point without depending on the
  control DC.

**Crate limitation worth flagging up front:** the openh264
0.9.3 Rust crate does *not* expose a setter for chroma
subsampling. `EncoderConfig::data_format` is a private
field hard-coded to `videoFormatI420` (4:2:0). Reaching
4:4:4 would require either patching the crate or going via
the `raw_api()` `set_option(ENCODER_OPTION_DATAFORMAT)`
path with unsafe. That's a real piece of work — and even
once the encoder produces 4:4:4, browser decoder support
is partial (Chrome: platform-dependent, Firefox: recent
only, Safari: no). Pulled out of phase 1 and into Future
work; a separate plan if we ever want to chase it.

The other half of the picture is bandwidth-awareness. The
encoder currently produces frames at one fixed quality with
no feedback from the network path between ryll and the
browser. That works fine on a LAN but degrades the moment
the operator is on a slow WAN link. The web mode's
*natural* deployment is remote access — the LAN is the
unusual case, not the common one — so adaptive bitrate is
not optional long-term.

## Mission and problem statement

Improve web-mode display quality across the full range of
expected network conditions:

1. **Quick win (phase 1):** wire an explicit `EncoderConfig`
   into `H264Encoder::new` and `H264Encoder::resize`. Pin
   usage type to `ScreenContentRealTime`, lift target
   bitrate into a sensible per-resolution band, narrow the
   QP range, pick Main or High profile, add a periodic IDR
   cadence. One-file change in `h264.rs` plus a small new
   `EncoderQuality` struct so the config can be tweaked
   from the CLI later. No new wire format, no architecture
   impact. Substantially closes the gap on LAN deployments
   where bandwidth is not the constraint.

2. **Adaptive bitrate (phase 2):** wire a per-second
   bandwidth signal from the browser back into the encoder
   via a new `EncoderControl::SetBitrate(u32 kbps)`
   variant. The browser samples
   `RTCPeerConnection.getStats()` once per second, picks
   `availableOutgoingBitrate` off the active
   `RTCIceCandidatePairStats`, EMA-smooths it, and sends
   it over the existing control DataChannel. The bridge
   forwards it as `EncoderControl::SetBitrate`. The
   encoder task either reconfigures the live encoder via
   `raw_api().set_option(ENCODER_OPTION_BITRATE, ...)`
   (unsafe but cheap) or rebuilds it (safe, costs one
   keyframe). The remote-access case is the *normal*
   deployment, not the edge case, so this is not optional
   long-term.

## Open questions

These are the decisions we make *now*, while planning, so
phase plans can be terse. The answers below are proposals;
the management session sanity-checks them before spawning
implementation.

### Q1. Phase 1 numeric targets

Proposed `EncoderConfig` (passed into both `Encoder::new`
and the `resize()` rebuild path):

- `usage_type(UsageType::ScreenContentRealTime)`
- `rate_control_mode(RateControlMode::Quality)` (explicit,
  even though it's the default — defaults can change)
- `qp(QpRange::new(18, 36))` — 0 = lossless, 51 = worst;
  18 is "visually lossless" for most content, 36 caps the
  worst-case to "watchable". Bounds the variance the
  Quality rate control mode is allowed to choose.
- `bitrate(BitRate::from_bps(15_000_000))` — 15 Mbps
  target. Quality RC treats this as an upper guide rail;
  the actual rate floats with content. Phase 2 makes this
  a moving target driven by the browser.
- `max_frame_rate(FrameRate::from_hz(30.0))` — matches
  the current `fps_cap` default in `EncoderTask`.
- `profile(Profile::High)` — better compression
  efficiency than Baseline; universally decoded by
  browsers.
- `level(Level::Level_4_2)` — covers 1080p60. We don't
  go above 1080p in practice today, but Level_4_2 gives
  headroom without bumping into hardware-decoder limits.
- `complexity(Complexity::Low)` — fast encode, real-time
  scenario. We're not trying to win compression
  benchmarks.
- `intra_frame_period(IntraFramePeriod::from_num_frames(60))`
  — 2 s IDR cadence at 30 fps. Bandwidth-spike recovery
  point without depending on the control-DC keyframe path.
- `scene_change_detect(true)` (default — explicit). Helps
  detect when the desktop content changes character.
- `skip_frames(false)` — VDI users notice dropped frames
  more than they notice bitrate spikes. Default is true.

Open: should any of these be CLI-configurable in phase 1?
*Proposal:* expose only `--web-encoder-bitrate-kbps`
(default 15000) so operators on constrained links can
hand-tune without code changes. Everything else stays
hard-coded until we have data showing it should move.

### Q2. Where the runtime reconfiguration happens (phase 2)

Two paths:

a. **Rebuild the encoder.** Same code path as `resize()`.
   Always works, costs one IDR (+ associated bandwidth
   spike) per bitrate change. With 1 Hz update cadence and
   reasonable EMA smoothing, "per bitrate change" should
   resolve to "maybe once every several seconds, when the
   estimate genuinely shifts band". Phase-2-friendly: it
   uses an API we already trust.

b. **Live SetOption.** `unsafe`
   `raw_api().set_option(ENCODER_OPTION_BITRATE, &bps)`.
   Free (no keyframe), but unsafe and unverified for the
   0.9.3 crate.

*Proposal:* phase 2 starts with (a). If we hit visible
"jumpy quality" at the IDR boundaries we revisit (b) in a
follow-on commit. (a) is also the lower-risk thing to put
on the critical path while the bridge → encoder plumbing
is also new.

### Q3. Bandwidth signal source (phase 2)

Three places we could read it:

a. **Browser `pc.getStats()`** → control DC →
   `EncoderControl::SetBitrate`. Most accurate (it's the
   browser's own decoder telling us what it can absorb),
   simplest to wire (control DC already carries inputs;
   adding one message type is a small change to
   `inputs.rs` / `bridge.rs`).
b. **Server-side `webrtc-rs` stats.** The
   `webrtc::peer_connection::stats` module exposes the
   same family of stats objects on the server side. Avoids
   browser changes but the server doesn't see the
   browser's jitter buffer state.
c. **Both, reconciling.** Most robust, most complexity.

*Proposal:* (a) — push the measurement from the browser.
The browser is the canonical authority on what it can
decode. Server-side stats can be added later if we find a
case where the browser estimate is unreliable.

### Q4. EMA window and update cadence (phase 2)

`availableOutgoingBitrate` is noisy at sub-second
sampling. The W3C-typical sample interval is 1 s and
that's what most production WebRTC apps use.

*Proposal:* 1 s sample, 4-sample EMA (alpha ~0.4), only
send to server when the EMA crosses a 10 %-of-current
band so we don't issue a `SetBitrate` per second. Same
band-crossing logic on the server before triggering an
encoder rebuild.

### Q5. Bitrate floor and ceiling (phase 2)

The browser will sometimes report absurdly low values
during startup or after a brief congestion event. We
should clamp.

*Proposal:* floor 500 kbps (below that the picture is
unusable and we'd rather show artefacts than freeze the
estimator), ceiling 15 Mbps (phase 1 default, no point
encoding harder than we did when bandwidth was infinite).
Both configurable via the same CLI flag family added in
phase 1.

### Scope boundary

Explicitly out of scope, recorded so we don't relitigate:

- **4:4:4 chroma.** Crate limitation + browser decoder
  fragmentation. Worth a separate plan if we ever push.
- **Audio quality.** Opus is its own ecosystem and
  already has a sensible bitrate. Not in this plan.
- **HDR / 10-bit colour.** Not a SPICE concept.
- **Video codec selection** (VP8/VP9/AV1). Reworking the
  webrtc-rs track type is its own project. Park.
- **Per-region quality.** Mouse-cursor area at high
  quality, static background at low quality. Interesting
  future work; significant encoder-side complexity.

## Execution

| Phase | Plan | Status |
|-------|------|--------|
| 1. Explicit encoder defaults (quick win) | [PLAN-web-encoder-quality-phase-01-defaults.md](PLAN-web-encoder-quality-phase-01-defaults.md) | Complete |
| 2. Adaptive bitrate via WebRTC stats | [PLAN-web-encoder-quality-phase-02-adaptive.md](PLAN-web-encoder-quality-phase-02-adaptive.md) | Complete |

Phase 1 lands before phase 2 starts — phase 1 gives us
the static-tuned baseline to measure phase 2 against,
and is also useful on its own for operators on stable
links.

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
   with the brief from the phase plan, at the recommended
   effort level and model.
3. **Review** the sub-agent's output in the management
   session. Read the actual files — the sub-agent's
   summary describes what it intended, not necessarily
   what it did.
4. **Fix or retry** if the output is wrong. Diagnose
   whether the brief was insufficient (improve it) or
   the model was too light (upgrade it), then re-run.
5. **Commit** once the management session is satisfied
   with the result.

Use `isolation: "worktree"` for sub-agents when the
change is risky or experimental. Phase 1 is a one-file
config change with existing test scaffolding; no
worktree needed. Phase 2 introduces a new control-DC
message type and an `unsafe`-adjacent runtime
reconfiguration path; use a worktree for at least the
plumbing step so it can be discarded if the approach
turns out to be wrong.

### Planning effort

The master plan itself was created at **high effort** —
it required reading the openh264 crate's actual API to
correct several assumptions from the original bubble
(4:4:4 unreachable, Quality RC already default, etc.).

Each phase plan specifies the recommended effort level
for its own planning. Both phases below are scoped as
medium-effort planning: the protocol research is done in
this master plan, and the phase plans are mostly about
sequencing the code changes.

### Step-level guidance

See the phase plan files for per-step tables. Both
phases use the standard format:

```
| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
```

**Model choice notes for this plan:**

- Phase 1 steps are all sonnet candidates: the brief
  front-loads the openh264 API research, file paths are
  explicit, and the changes are well-scoped.
- Phase 2 step 2a (browser-side stats sampling) is
  sonnet — straightforward JS using a documented W3C
  API. Step 2b (control DC wire format) is sonnet with
  a detailed brief. Step 2c (encoder rebuild on
  SetBitrate) leans opus because it has to choose
  between the rebuild path and the unsafe SetOption
  path, and the consequences of choosing wrong (silent
  miscompiles, undefined behaviour) are real.

**When in doubt, skew to the more capable model.**

### Management session review checklist

After a sub-agent completes, the management session
should verify:

- [ ] The files that were supposed to change actually
      changed (read them, don't trust the summary).
- [ ] No unrelated files were modified.
- [ ] `pre-commit run --all-files` passes.
- [ ] `make test` passes.
- [ ] The changes match the intent of the brief — not
      just syntactically correct but semantically right.
      In particular for this plan: did the sub-agent
      actually pin the `usage_type` to
      `ScreenContentRealTime`, or did it leave it at the
      default and just twiddle the bitrate?
- [ ] Commit message follows project conventions
      (including the Co-Authored-By line with model,
      context window, effort level, and other settings).

## Administration and logistics

### Success criteria

We will know when this plan has been successfully implemented
because the following statements will be true:

- The code passes `pre-commit run --all-files` (rustfmt,
  clippy with `-D warnings`, shellcheck).
- New code follows existing patterns in
  `shakenfist-spice-renderer/src/encoder/` (the
  `H264Encoder` / `EncoderTask` split, mpsc-based
  control plane, async tasks via tokio).
- There are unit tests for new logic, and the existing
  tests still pass (`make test`). The existing
  `encoder_auto_resizes_mid_stream` test must continue
  to pass — phase 1's reconfigure path runs the same
  rebuild it does.
- Lines wrap at 120 characters; single quotes for Rust
  strings where applicable.
- `README.md`, `ARCHITECTURE.md`, and `AGENTS.md` are
  updated if the change adds CLI flags, new message
  types on the control DC, or new public types in the
  renderer crate.
- `docs/web-frontend.md` documents the new encoder
  defaults, the `--web-encoder-bitrate-kbps` flag, and
  (after phase 2) the adaptive-bitrate control loop.
- The web-mode display visibly matches the eframe GUI's
  text crispness on a LAN link (subjective; the
  operator should stop calling it "fuzzy"). Phase 1
  alone should be enough for this on LAN.
- A WAN link with deliberately constrained bandwidth
  (e.g. 5 Mbps cap via `tc qdisc`) produces a usable
  display rather than freezing or showing severe
  artefacts. Phase 2 is the load-bearing change here.

### Future work

- **4:4:4 chroma.** Requires either patching the
  openh264 0.9.3 crate or going via the unsafe
  `set_option(ENCODER_OPTION_DATAFORMAT)` path, plus a
  per-browser decoder-capability negotiation. Probably
  the next big lever once phase 1 + phase 2 are
  bedded in.
- **VP9 / AV1.** webrtc-rs ships VP8 today; VP9 / AV1
  would mean reworking the track type and dragging in
  another encoder dependency. Worth re-evaluating once
  AV1 hardware decoding is universally shipped on
  client devices.
- **Per-region quality / ROI encoding.** Cursor
  neighbourhood and recently-changed tiles at higher
  quality, static background at lower. Significant
  encoder-side work; not worth doing until bigger
  wins land.
- **Server-side bandwidth fallback.** If the browser
  → server bandwidth message ever stops arriving (e.g.
  control DC stalls), the encoder should fall back to
  webrtc-rs's own stats rather than continuing to
  encode at the last-reported rate forever.

### Bugs fixed during this work

(Empty — will be populated as phases land if any
incidental fixes are made along the way.)

### Documentation index maintenance

When this plan first lands a phase:

- Update `docs/plans/index.md` — add a master-plan row
  with creation date 2026-06-02, link to the master plan
  and both phase plans, status `Not started` (then bump
  to `In progress` once phase 1 lands).
- `docs/plans/order.yml` already lists this plan; no
  change needed.

When all phases are complete, set the index.md status to
`Complete`.

### Back brief

Before executing any step of this plan, please back brief
the operator as to your understanding of the plan and how
the work you intend to do aligns with that plan.

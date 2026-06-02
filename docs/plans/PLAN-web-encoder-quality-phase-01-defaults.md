# Phase 1 — Explicit openh264 encoder defaults for VDI

Master plan: [PLAN-web-encoder-quality.md](PLAN-web-encoder-quality.md).

## Goal

Stop inheriting the openh264 `EncoderConfig::default()` —
which targets video-conferencing at 120 kbps — and pass an
explicit VDI-tuned config every time we construct or
rebuild the encoder. Add one CLI flag
(`--web-encoder-bitrate-kbps`) so operators on constrained
links can adjust without recompiling, defaulting to 15 000.

This is the static-tuned baseline phase 2 will measure
against. On its own, it should close most of the "fuzzy
text" gap operators reported on LAN.

## Non-goals

- Adaptive bitrate (phase 2).
- 4:4:4 chroma (crate doesn't expose it; future plan).
- Profile/level operator override (not yet — bake in
  Profile::High / Level_4_2 as constants; only the bitrate
  is exposed).
- Reworking the audio (opus) bitrate path.

## Design notes

### New `EncoderQuality` struct

Add to `shakenfist-spice-renderer/src/encoder/h264.rs`:

```rust
#[derive(Debug, Clone, Copy)]
pub struct EncoderQuality {
    /// Target bitrate in bits per second. Acts as an upper
    /// guide rail for Quality rate control.
    pub target_bitrate_bps: u32,
}

impl Default for EncoderQuality {
    fn default() -> Self {
        Self { target_bitrate_bps: 15_000_000 }
    }
}
```

Keeping it a struct (not a bare u32) so phase 2 can add
`min_bitrate_bps` / `max_bitrate_bps` clamps without
re-signaturing every caller.

### `H264Encoder` constructor changes

- `H264Encoder::new(width, height)` → keep for tests, make
  it call `new_with_quality(width, height, EncoderQuality::default())`.
- New `H264Encoder::new_with_quality(width, height, quality)`
  builds an `EncoderConfig` with the fields listed in the
  master plan Q1, sets `target_bitrate` from `quality`, and
  passes it to `Encoder::with_api_config(OpenH264API::from_source(), cfg)`.
- `H264Encoder::resize(...)` already rebuilds the inner
  encoder. Store the `EncoderQuality` on `H264Encoder` so
  `resize` uses the same config on rebuild.
- Add `H264Encoder::set_quality(EncoderQuality)` for phase
  2 — for phase 1, leave the body as just updating the
  stored quality (no rebuild). Phase 2 will extend.

### Wire the quality through to `EncoderTask` / the bridge

The construction chain:
- `EncoderTask::spawn` already takes a constructed
  `H264Encoder`, so this phase doesn't need to touch it.
- The encoder is constructed in
  `shakenfist-spice-webrtc/src/bridge.rs` (search for
  `H264Encoder::new(`). Replace with
  `H264Encoder::new_with_quality(... , quality)` where
  `quality` is passed into `WebrtcBridge` construction.
- Plumb a new `quality: EncoderQuality` field through
  `BridgeBuilder` (or whatever the existing builder is —
  read the code first).

### CLI flag

In `ryll/src/config.rs`, alongside `web_exit_on_disconnect`:

```rust
#[arg(long, value_name = "KBPS", default_value_t = 15_000)]
pub web_encoder_bitrate_kbps: u32,
```

Pass it into the bridge construction site in
`ryll/src/main.rs::run_web` (multiply by 1000 to get bps,
build `EncoderQuality`, hand it down).

## Steps

| Step | Effort | Model  | Isolation | Brief for sub-agent |
|------|--------|--------|-----------|---------------------|
| 1a   | medium | sonnet | none      | Add `EncoderQuality` struct to `shakenfist-spice-renderer/src/encoder/h264.rs`. Rework `H264Encoder` to store a `quality: EncoderQuality` field; add `new_with_quality(width, height, quality)` constructor that builds an `openh264::encoder::EncoderConfig` per master-plan Q1 (usage_type=ScreenContentRealTime, rate_control_mode=Quality, qp=QpRange::new(18,36), bitrate=BitRate::from_bps(quality.target_bitrate_bps), max_frame_rate=FrameRate::from_hz(30.0), profile=Profile::High, level=Level_4_2, complexity=Complexity::Low, intra_frame_period=IntraFramePeriod::from_num_frames(60), skip_frames=false) and constructs via `Encoder::with_api_config(OpenH264API::from_source(), cfg)`. Keep existing `new(width, height)` as a thin wrapper that calls `new_with_quality(..., EncoderQuality::default())`. Update `resize()` to rebuild the inner encoder *with the stored quality* — currently it calls `Encoder::new()` which would silently drop our config. Add `set_quality(EncoderQuality)` that just updates the stored field for now (phase 2 extends it). Re-export `EncoderQuality` from `encoder/mod.rs` and the crate root. Add a unit test that constructs with a non-default quality and verifies `H264Encoder::quality()` returns it. The existing `encoder_auto_resizes_mid_stream` test must still pass — it exercises the rebuild path. **Why sonnet:** the openh264 API surface is documented in this brief and the change is localised to one file plus its re-exports. |
| 1b   | medium | sonnet | none      | Plumb `EncoderQuality` through to the bridge. Read `shakenfist-spice-webrtc/src/bridge.rs` to find every `H264Encoder::new(` call site. Replace with `H264Encoder::new_with_quality(width, height, quality)` where `quality` is a new field on whatever struct constructs encoders (likely `WebrtcBridge` or a builder near it — read first, don't assume). Default the new field to `EncoderQuality::default()` so existing test call sites don't need updating; only the live-bridge construction path threads a real value. Update existing tests in `bridge.rs` that construct `H264Encoder` to use whichever form is least disruptive. **Why sonnet:** mechanical wiring, the file is large but the change is well-defined. |
| 1c   | medium | sonnet | none      | Add `--web-encoder-bitrate-kbps` CLI flag. In `ryll/src/config.rs`, add `pub web_encoder_bitrate_kbps: u32` with `#[arg(long, value_name = "KBPS", default_value_t = 15_000)]`, alongside the existing `web_exit_on_disconnect` flag — mirror its style exactly. In `ryll/src/main.rs::run_web`, multiply by 1000, construct an `EncoderQuality`, and pass it into the bridge construction call modified in step 1b. Document the flag in `docs/web-frontend.md` (one paragraph in whatever section talks about web mode configuration; the operator-facing help should explain that this is an *upper* bitrate guide-rail for Quality rate control, not a fixed bitrate). **Why sonnet:** clap conventions are already established in `config.rs`; this is paint-by-numbers. |
| 1d   | low    | haiku  | none      | Run `make lint` and `make test`. If anything fails, do not attempt fixes; report the failures verbatim. **Why haiku:** purely mechanical verification step. |
| 1e   | medium | sonnet | none      | Documentation pass: update `README.md`, `ARCHITECTURE.md`, `AGENTS.md`, and `docs/web-frontend.md` to mention (1) the new `EncoderQuality` type and where it's set, (2) the `--web-encoder-bitrate-kbps` CLI flag and its default, (3) the encoder is now pinned to `ScreenContentRealTime` usage / Quality RC / Profile::High. Keep the encoder-specifics tight — operator-facing docs should focus on the CLI flag and the qualitative meaning ("higher = sharper but more bandwidth"); developer-facing docs (`ARCHITECTURE.md`) can list the actual config fields. **Why sonnet:** docs prose with a clear scope. |

## Plan-level effort

Planning this phase was **medium effort**: the openh264
API research is captured in the master plan, and each
step has a paint-by-numbers brief. No protocol decisions
remain open.

## Commit cadence

One commit per step (so five commits if every step
lands cleanly). If 1b and 1c are tiny they can merge, but
1a / 1d / 1e stay separate so the bisect history is
useful. The Co-Authored-By line on each commit must
reflect the sub-agent that did the work (not the
management session's model).

## Test plan

Automated:
- `make test` must pass; in particular
  `encoder_auto_resizes_mid_stream` confirms the rebuild
  path honours the stored quality.
- New unit test in `h264.rs` verifies
  `H264Encoder::quality()` round-trips.

Manual (operator):
1. Build, start `ryll --web --web-encoder-bitrate-kbps 15000`
   against an active SPICE server.
2. Open the browser, render a desktop with small text
   (e.g. a terminal at 11pt). Confirm text is visibly
   sharper than before this phase.
3. Same with `--web-encoder-bitrate-kbps 1500` — should
   degrade gracefully, not freeze.
4. Run a `runtest.sh` capture session for the artefacts.

Subjective sign-off: the operator stops describing the
display as "fuzzy" on a LAN connection.

## Risks

- **Profile / Level mismatch with browser decoder.**
  Profile::High + Level_4_2 is universally decoded by
  every browser shipped in the last decade, but
  *embedded* H264 decoders on some thin clients (older
  Chromecast, certain Android TV builds) can be picky.
  Phase 1 doesn't target those — but if it turns out to
  break Android TV later, falling back to Profile::Main
  / Level_4_1 is one line.
- **15 Mbps is too aggressive for some encoder buffers.**
  openh264's Quality RC mode treats `target_bitrate` as
  an upper rail, so this should manifest as "encoder
  doesn't actually use 15 Mbps" rather than "encoder
  buffer overruns". If symptoms appear, halving the
  default is a one-line change.
- **Configuration silently ignored.** If `with_api_config`
  isn't actually plumbing the QP range through, the
  encoder may quietly fall back to defaults. The new unit
  test (1a) round-trips the quality struct, but verifying
  the encoder *honours* it requires either decoding the
  produced bitstream's SPS or measuring output size. Out
  of scope for phase 1; we rely on operator subjective
  sign-off and on phase 2's adaptive loop being a
  forcing function for getting this right.

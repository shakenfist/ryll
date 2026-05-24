# Phase 13 — Investigate intermittent server-side streaming

Phase 13 of [PLAN-stream-caps-and-flap.md](PLAN-stream-caps-and-flap.md).
Driven by sessions 002e / 002g / 002h / 003 / 004 and **rewritten by
session 005** once the spice-debug surface fix made the qemu log
readable.

## What changed in session 005

For five sessions we believed the server's streaming heuristic
simply did not fire at 1920×1440 with a QXL guest. Client-side
`streams_created_total` was zero across every high-res run.
Once session 005 landed the
`<qemu:env name='G_MESSAGES_DEBUG' value='all'/>` template
edit and the qemu log filled with real spice-server
instrumentation, the picture changed completely:

- **Server creates the right stream.** At 005b's video start
  the server emitted
  `display_channel_create_stream: stream 6 1024x768 (0, 0) (1024, 768) 10 fps`
  — correctly detecting the YouTube video region inside the
  1920×1440 desktop.
- **Client decoded it.** 79 MJPEG frames over 8 seconds — the
  full client-side machinery (cap negotiation, STREAM_CREATE
  handling, MJPEG decode) worked first try.
- **Server destroyed the stream at 06:09:28** (~8 s after
  creation) and then **never recreated it for the remaining
  10 minutes of 005b or the 2 minutes of 005c**, despite the
  user continuing to play the same video in the same region.

So the bug is not "heuristic doesn't fire". The bug is
**single-shot teardown without re-engagement**.

## Likely mechanism (per source-read, to be confirmed)

The qemu log shows `display_channel_debug_oom` firing
**1140 times in 005b** (over 670 s, ~1.7/sec) and **1238 times
in 005c**. Reading the source:

- `display_channel_debug_oom` is called from `red-worker.cpp::handle_dev_oom`
  in two places: `OOM1` before recovery and `OOM2` after.
- `handle_dev_oom` runs `display_channel_free_some()` and
  `red_qxl_flush_resources()` between the two log lines —
  this is an emergency drop of pending drawables to free
  memory.
- The OOM message itself is `RedWorkerMessageOom`, sent by
  `spice_qxl_oom()` at `red-qxl.cpp:328`. Calling
  `spice_qxl_oom` is qemu's QXL device emulation telling
  the spice-server **"the guest QXL driver has run out of
  command-ring memory"**.

So this is **guest-driver memory pressure**, not a
spice-server encoder bandwidth problem. The chain:

```
guest QXL kernel driver out of command-ring slots
  → qemu QXL device emulation sees the out-of-memory notification
    → spice_qxl_oom() sends RedWorkerMessageOom to spice-worker
      → handle_dev_oom drops drawables + flushes (display_channel_free_some)
        → stream-tracking state evicted as a side effect
          → the video-region stream's frame-rate detector loses confidence
            → RED_STREAM_TIMEOUT after the next gap and the stream is destroyed
              → server stays in defensive fallback (ZlibGlzRgb) for that region
```

This is a hypothesis. It is consistent with what we see:

- Continuous OOM pressure throughout 005b/005c.
- Stream creation only at the very start (before the first OOM
  burst), never after.
- Session 004's "more VRAM doesn't help" result is also
  consistent — VRAM matters for streaming creation only via
  this indirect OOM-frequency path; the original
  Did-VRAM-fix-streaming test asked the wrong question.

## What to investigate (in order)

### 13A — Confirm the OOM-evicts-stream-state mechanism

**Effort: medium. Output: a writeup in this file.**

Read the spice-server source carefully:

- `red-worker.cpp::handle_dev_oom` (the OOM handler).
- `display-channel.cpp::display_channel_free_some` (what
  it actually frees — does it touch `Stream` instances /
  `StreamCreateDestroyItem` / stream tracking structures?).
- `red_qxl_flush_resources`.
- `video-stream.cpp` — specifically the per-region frame-rate
  detector (`is_next_stream_frame`,
  `red_stream_input_fps_timeout_callback`) and the conditions
  under which it would *re-engage* after a teardown.

Answer: does an OOM-driven `display_channel_free_some` evict
the per-region frame statistics that the heuristic needs to
re-fire? Or does the stream-create heuristic require some
state that gets cleared on each OOM cycle? Or is it that the
QXL guest driver under memory pressure produces draws of a
different op-type that fail the bitmap-opaque filter (the
original 004 hypothesis, just re-framed as an effect of OOM
rather than of resolution)?

If the mechanism is "OOM evicts stream state, recreation needs
N frames of fresh statistics, OOMs fire faster than N frames
of stats can be gathered" — that's a server-side bug worth
filing upstream against spice-server with a minimal
reproducer.

### 13B — Quantify the OOM-rate dependency on guest VRAM

**Effort: low-medium. Output: a small results table for this file.**

Re-run the 005b workload (1920×1440, ≥3 min) at three guest
VRAM values, all with the spice-debug template edit in place:

| Run | guest VRAM | OOM count over run | 1024×768 stream creates | Stream re-engagements |
|-----|-----------|--------------------|--------------------------|------------------------|
| 005b (already done) | 64 MiB | 1140 over 670 s (1.7/s) | 1 | 0 |
| (next) | 128 MiB | ? | ? | ? |
| (next) | 256 MiB | ? | ? | ? |

If OOM rate scales inversely with VRAM AND stream
re-engagements scale up — the diagnosis is locked. If OOMs
stay high regardless of VRAM, look elsewhere
(`ram`/`vgamem`, qemu's QXL device sizing, guest-driver
allocation pattern). The instructions for this go into a
follow-up `006.md` in `ryll-test-sessions`.

### 13C — Read the guest QXL driver

**Effort: medium. Output: a writeup in this file.**

The relevant guest-side source lives at:

- `/srv/src-reference/torvalds/linux/drivers/gpu/drm/qxl/qxl_release.c`
- `/srv/src-reference/torvalds/linux/drivers/gpu/drm/qxl/qxl_drv.h`
- `/srv/src-reference/torvalds/linux/drivers/gpu/drm/qxl/qxl_cmd.c`

(Confirm paths exist before relying on them; this repo
mirrors several kernel trees and the QXL driver is small.)

What we want to learn: under what conditions does the QXL
guest driver call its `out_of_memory` notification (the thing
that triggers `spice_qxl_oom` on the host)? Is there a
threshold like "<N% command-ring free"? Does it scale with
draw-op size (i.e. would more 4K-tile draws produce more
OOMs than fewer full-screen blits)?

If the trigger is small-and-frequent-draws, the workload
shape matters: a fullscreen video produces large in-place
draws and few OOMs; a windowed-video-on-busy-desktop
produces small partial draws and many OOMs. That would
explain why the same machine streams fine at 1024×768
(everything is windowed-into-a-small-desktop, so the video
takes up *more relative area* in the command ring) and
badly at 1920×1440 (the video shares the ring with all the
desktop chrome behind it).

### 13D — Reduce OOM frequency from the qemu device side

**Effort: out of scope; document only.**

The relevant qemu knob is the QXL device's `ram_size` /
`vram_size` parameters (the `<video><model type='qxl'
ram='65536' vram='65536' ...` block in the libvirt XML).
Larger values give the guest driver more headroom before
it triggers OOM.

Phase 13B above measures the effect; the documentation
follow-up is to update `docs/libvirt-spice-recommendations.md`
with what we actually found, replacing the now-disproven
"VRAM doesn't help" guidance with the more precise truth:
**VRAM doesn't unlock streaming directly, but it reduces
the OOM rate that tears streams down**. Those are not the
same statement.

### 13E — Mitigation candidates if upstream fix isn't in reach

**Effort: medium; depends on 13A's findings.**

Possible mitigations the client could attempt:

1. **More aggressive STREAM_REPORT cadence** — phase 1 of
   the master plan landed STREAM_REPORT. If we send positive
   feedback more often when a stream is healthy, the server's
   stream-detector may resist teardown longer. Read
   `mjpeg_encoder_handle_positive_client_stream_report`
   (we see it in the 005 log) to confirm the report is
   actually feeding the survival decision.

2. **Resolution adaptation hint** — if at high res streams
   always die, ryll could send a `MONITORS_CONFIG` suggesting
   the guest use a resolution we know works (1280×800 in
   the 004 matrix). Heavy-handed; not without operator
   consent. Defer.

3. **Codec preference** — phase 7 (PREF_VIDEO_CODEC_TYPE)
   isn't yet implemented. Once it is, biasing toward H.264
   may reduce per-frame encode time on the server enough
   that the OOM cycle decouples from stream survival. Speculative.

None of these go into code until 13A confirms the mechanism.

## Out of scope

- Patching spice-server. If we find a bug, file upstream
  with the minimal reproducer (013B's data set).
- Patching qemu's QXL emulation. Same.
- Patching the guest kernel QXL driver. Same.
- Rebuilding the spice-server with statistics/recorder enabled
  (`--enable-recorder`). Probably useful eventually but
  unnecessary while spice_debug works.

## Cross-references

- `/srv/src-reference/spice/spice/server/red-worker.cpp:520-548`
  — `handle_dev_oom` (the OOM handler).
- `/srv/src-reference/spice/spice/server/red-qxl.cpp:328`
  — `spice_qxl_oom` (qemu→worker dispatch).
- `/srv/src-reference/spice/spice/server/display-channel.cpp:2411`
  — `display_channel_debug_oom` (the log line we see).
- Session 005 bundles in
  `private:ryll-test-sessions/sessions/test-session-005{a,b,c}.tar.gz`.
- `docs/libvirt-spice-recommendations.md` — VRAM-vs-streaming
  guidance that needs updating per 13D.
- `ryll-test-sessions/manual-test-instructions/005.md` — the
  instructions that produced the 005 data set.

## Success criterion

Phase 13 is complete when the OOM-vs-streaming relationship
is characterised well enough that:

- We can predict (qualitatively) which guest configurations
  will produce stable streaming and which won't.
- Either (a) an upstream issue is filed with a minimal
  reproducer, or (b) operator guidance for VRAM sizing in
  `docs/libvirt-spice-recommendations.md` is precise enough
  to be load-bearing, or (c) a client-side mitigation that
  measurably improves stream lifetime is identified and
  filed as its own follow-up phase.

"We shipped code" is not the success criterion. "We
understand the failure mode" is.

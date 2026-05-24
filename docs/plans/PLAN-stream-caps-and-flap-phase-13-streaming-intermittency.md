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

## 13A findings (source read)

**Verdict: refuted in its strong form, partial in a more interesting
form.** OOM does *not* evict stream-detection state directly; in fact,
the OOM eviction path *populates* the per-region trace buffer that
re-engagement reads. The mechanism that prevents re-engagement is
subtler, and the binding constraint is the trace ring's tiny size
(8 entries) combined with the 200 ms detection window.

### What `handle_dev_oom` actually does

`server/red-worker.cpp:530-549`. Step by step:

1. Asserts the QXL device is running (line 537).
2. Emits `display_channel_debug_oom("OOM1")` (line 539) — the log
   line we count in session 005.
3. Drains pending QXL commands by looping
   `red_process_display()` (line 540) and pushing each batch to
   pipes via `display->push()` (line 541). This *consumes* fresh
   draws from the guest — it does not evict anything.
4. Calls `red_qxl_flush_resources(qxl)` (line 543), a thin
   wrapper at `server/red-qxl.cpp:780-785` that calls into the
   qemu QXL device's `flush_resources` callback. This is the
   qemu-side release-ring drain; no spice-server state is
   touched. If it returns non-zero (released >= 1 resource),
   step 5 is skipped.
5. Fallback only if flush released zero resources:
   `display_channel_free_some(display)` (line 544) followed by a
   second `red_qxl_flush_resources` (line 545).
6. Emits `display_channel_debug_oom("OOM2")` (line 547) and
   clears the worker's pending-OOM bit (line 548).

`display_channel_free_some` (`server/display-channel.cpp:1481-1507`)
does two things: (a) for each DCC, releases GLZ dictionary
drawables held by the encoder (line 1494); (b) walks
`display->priv->current_list` from the tail and calls
`free_one_drawable(display, force_glz_free=TRUE)` up to
`RED_RELEASE_BUNCH_SIZE` times (line 1498).
`free_one_drawable` (line 1451-1471) renders the oldest pending
drawable to the canvas via `drawable_draw`, then calls
`current_remove_drawable` (line 1468).

### Does it touch stream-tracking state?

It touches stream state — but **constructively, not
destructively**. `current_remove_drawable`
(`server/display-channel.cpp:365-374`) calls
`video_stream_trace_add_drawable` (line 368) on every evicted
drawable. That function (`server/video-stream.cpp:1049-1068`)
records the drawable's geometry, `frames_count`,
`first_frame_time`, and `gradual_frames_count` into one slot of
the ring buffer `display->priv->items_trace`, indexed by
`next_item_trace++ & ITEMS_TRACE_MASK`. The eviction filter at
line 1054 skips drawables that are already attached to a stream
(`item->stream`) or are not streamable (`!item->streamable`),
so OOM eviction can only ever *add* candidate frames to the
trace — never overwrite live stream metadata.

The trace ring is `std::array<ItemTrace, NUM_TRACE_ITEMS>` with
`NUM_TRACE_ITEMS = 1 << 3 = 8`
(`server/display-channel-private.h:23-25,115-116`). That is
**the critical constant** — only the eight most recently evicted
streamable drawables are remembered. The trace is reset to
zero only in `stop_streams` (line 226-227), which is itself
only called from `display_channel_surface_unref` when the
primary surface is destroyed (line 230-241). OOM does not
trigger surface destruction. Active `VideoStream` instances
themselves live on `display->priv->streams` and are torn down
by `video_stream_timeout` (`server/video-stream.cpp:1031-1047`)
when their `last_time + RED_STREAM_TIMEOUT` (1 s) has passed —
an inactivity timer, not an OOM-driven path.

### What `display_channel_create_stream` actually requires

Caller chain
(`server/video-stream.cpp:419,585,559-590,628-666,668-707`):

- `display_channel_process_draw` →
  `display_channel_add_drawable` (line 1317-1364) sets
  `drawable->streamable = drawable_can_stream(...)` (line 1353).
  `drawable_can_stream` (line 1044-1080) requires: stream-video
  mode enabled, primary surface, `QXL_EFFECT_OPAQUE`,
  `QXL_DRAW_COPY` with `SPICE_ROPD_OP_PUT`, a
  `SPICE_IMAGE_TYPE_BITMAP` source, and (in FILTER mode)
  area ≥ `RED_STREAM_MIN_SIZE` (96×96).
- `current_add` calls `video_stream_trace_update`
  (`server/display-channel.cpp:1019`).
- `video_stream_trace_update` (`server/video-stream.cpp:628-666`)
  first scans active streams; if none match, it scans the
  eight-slot `items_trace`. For each trace entry,
  `is_next_stream_frame` (line 213-270) checks: same
  src-width/height, identical bbox, and
  `candidate->creation_time - trace.time` ≤
  `RED_STREAM_DETECTION_MAX_DELTA` (NSEC_PER_SEC / 5 = **200 ms**,
  `server/video-stream.h:32`). On match,
  `video_stream_add_frame` increments `frame_drawable->frames_count`
  from `trace.frames_count + 1` (line 568) and tests
  `is_stream_start` (line 182-187), which needs
  `frames_count ≥ RED_STREAM_FRAMES_START_CONDITION = 20` and
  20 % gradual-quality coverage (`server/video-stream.h:35-36`).
- `video_stream_maintenance` (line 668-707) is the other entry
  point, fired when an opaque drawable replaces a previous one
  at the same tree position (`current_add_equal`, line 488).

So re-engagement of a torn-down stream requires twenty
consecutive matching frames at the same bbox, with each
successive draw arriving inside 200 ms of the previous, with
the per-region history threaded through a ring buffer that
holds only eight entries total *across all surfaces and
regions*.

### Verdict

**Partial.** OOM eviction does not directly clobber stream
state — `video_stream_stop` and `display_channel_free_some` are
disjoint paths, and the trace ring is only zeroed on primary
surface teardown. What kills re-engagement is the interaction
between OOM eviction *of unrelated drawables* and the trace
ring's 8-entry capacity. At 1.7 OOMs/sec each releasing up to
`RED_RELEASE_BUNCH_SIZE` (commonly tens of) drawables from the
tail of `current_list`, *every* streamable drawable that gets
evicted writes to the same shared 8-slot ring. With a busy
1920×1440 desktop (chrome, taskbar, cursor blink, every
streamable bitmap blit anywhere) the video region's trace
entries are flushed out of the ring before twenty consecutive
matching draws can accumulate within the 200 ms window — and
the video draws themselves, if they hit `current_list` during
an OOM burst, end up *in the trace* rather than *attached to a
stream* because there is no active stream to attach to. The
heuristic is starved by trace contention, not by state
eviction.

The session 005 evidence is consistent: once the original
1024×768 stream is torn down by `video_stream_timeout` (1 s
gap from the encoder pipeline being throttled by OOM
back-pressure is plausible), every subsequent video frame
arrives into a tree where (a) no active stream matches it and
(b) the trace ring is dominated by recently-evicted desktop
chrome. The video region's own trace entries either never
accumulate enough consecutive within-200 ms hits or are
displaced by other streamable evictions in the same OOM cycle.

**Resolved:** `RED_RELEASE_BUNCH_SIZE = 64`
(`server/image-encoders.h:221`). Each OOM-driven
`display_channel_free_some` evicts up to 64 drawables from
the tail of `current_list`, which is *eight times* the
8-slot `items_trace` ring. A single OOM cycle can therefore
fully overwrite the trace ring multiple times if enough of
the evicted drawables are `streamable`. The trace-contention
argument above is firmly grounded.

**Resolved:** `red_stream_input_fps_timeout_callback` does
not exist in this spice tree — the 13A brief referenced a
function from an older or downstream-patched spice. The FPS
estimate is computed inline in `attach_stream`
(`server/video-stream.cpp:282-292`) using
`RED_STREAM_INPUT_FPS_TIMEOUT = 5 s`. No separate timer
callback path to read.

### Implications for 13B

The hypothesis-as-written ("more VRAM lowers OOM rate, lower
OOM rate lets the heuristic re-fire") is still correct in
direction but the *mechanism* is trace-ring contention rather
than state eviction. The 13B prediction is the same: at
sufficiently high VRAM the guest QXL driver should stop
issuing OOMs, the trace ring stops being repopulated by
unrelated drawables, the video region's trace entries persist
long enough for `video_stream_trace_update` to find a match,
and stream re-engagement should follow within ~1 s of the
first 20 video frames after teardown.

Recommendation for 13B's session: run with 64 MiB / 128 MiB /
256 MiB QXL `vram_size`, all on the same 1920×1440 workload as
005b. Grep qemu logs for: `display_channel_debug_oom` counts
per 60 s, `display_channel_create_stream` events, and
`video_stream_stop`-adjacent destroy lines. The expected
signature of confirmation is OOM count and stream-re-engagement
count being inversely correlated. If 256 MiB still shows
hundreds of OOMs/min, the guest driver is the source and 13C
(read the QXL kernel driver) becomes the next step.

### Implications for 16 / upstream

If 13B confirms, this is a clean upstream bug-report against
spice-server: "On hosts under sustained QXL OOM pressure, the
8-entry `items_trace` ring buffer is fully overwritten faster
than `RED_STREAM_FRAMES_START_CONDITION` consecutive frames
can accumulate for a single region, preventing video stream
re-engagement after `video_stream_timeout` tears the first
stream down. Reproduction: 1920×1440 QXL guest, 64 MiB vram,
windowed YouTube playback for >2 min; observe single
`display_channel_create_stream` event followed by zero
re-engagements over the remainder of the session despite
continuous matching draws." A one-line server-side mitigation
would be to grow `NUM_TRACE_ITEMS` from 8 to (say) 64 — the
trace entries are tiny and the cost is a few hundred bytes per
display channel.

Client-side mitigation (phase 13E candidate): the trace ring
is server-internal; the client cannot directly seed it. The
phase 13E "more aggressive STREAM_REPORT" option only affects
*surviving* streams (it feeds bit-rate / drop accounting in
`mjpeg_encoder_handle_positive_client_stream_report`); it does
nothing for a stream that has already been destroyed. A more
useful client lever, if upstream won't move, is to *avoid the
1 s teardown gap* by ensuring the client doesn't induce
back-pressure — but that is speculation pending 13B's OOM-vs-
VRAM curve.

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

- [docs/troubleshooting.md § Streaming indicator](../troubleshooting.md#streaming-indicator)
  — the live status-bar indicator added in phase 8 is the
  cheapest signal for the OOM-vs-survival investigation here.
  Amber after every short workload run is the visual cue that
  the 005-style "stream lives N seconds then never returns"
  pattern is reproducing; red means the flap heuristic has
  fired and a `Warn` notification with the destroy/lifetime
  numbers is in the bell. Watch it during the 13A reproducer
  rather than scraping snapshots after the fact.
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

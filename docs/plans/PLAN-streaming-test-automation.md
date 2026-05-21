# Streaming test automation

**Status: Proposed (concept). No phases drafted yet.**

## Why this exists

The 002 and 003 dogfood cycles produced rich signal but
needed me at a keyboard each time to provision an instance,
SSH a guest, play a video, file bug reports, and commit
session bundles. That cadence is fine for exploratory bring-
up of new features — the human-in-the-loop interpretation
is half the value — but is wrong for regression coverage
once those features stabilise.

Concretely: once phase 6 (H.264) ships and the streaming
heuristic is understood (phase 13), we want CI to catch
"streaming used to work at 1024×768 with this guest config
and now it doesn't" without anyone having to notice.
Today's manual cadence catches that only when someone
remembers to retest the right scenario.

## What would land in CI

The minimum useful thing: a job that boots a known-good
guest, runs ryll's `--headless` mode against it for a fixed
workload, dumps the same channel-state.json our auto-snapshot
mode produces, and asserts on a small set of structural
properties (`streams_created_total >= 1`,
`mjpeg_decode_failed_count == 0`,
`image_cache_bytes <= image_cache_cap_bytes`, etc.).

What this implies:

- A way to provision a SPICE server in CI. Three plausible
  paths:
  - **(a) Real guest in CI.** Run qemu in the CI runner
    with nested KVM. Expensive (slow boot, needs nested
    virt available on the runner, image storage), but
    fully faithful — same code paths the dogfood
    sessions exercise.
  - **(b) Mock/synthetic SPICE server.** ryll's `--web`
    mode already has a synthetic source path for the
    encoder side; the reverse — a synthetic SPICE
    server that emits predetermined draws on a port — is
    the read-side equivalent. Would need writing.
  - **(c) Pcap replay.** We're collecting real session
    pcaps in the test-sessions repo already. A replayer
    that reads a captured server-side pcap and serves
    its draw messages to a client would let us assert
    deterministic behaviour against frozen wire data.
    Tightly tied to ryll's understanding of the
    protocol but exercises real bytes; doesn't need
    qemu at all.
- A workload driver inside the guest. For (a), an
  in-guest agent or cloud-init runcmd that plays a fixed
  video at session start. For (b)/(c), workload is
  embedded in the synthetic source / pcap respectively.
- A structural-properties assertion harness. The
  channel-state.json schema is stable enough that we can
  write a small Python or Rust script that loads a
  snapshot and asserts on N fields, run as the final CI
  step.
- A way to fail loudly without flaking. The streaming-
  heuristic intermittency observed in 002/003 means
  "stream created" can't be a single-run assertion; need
  several runs and a quorum, or pin the workload to a
  scenario that's deterministic enough not to flap.

## What would NOT land in CI

The exploratory, observe-then-interpret workflow stays
manual. CI is for regression coverage of known-good
behaviour, not for hunting down new bugs. Sessions like
003 (which discovered the resolution sensitivity) are
the kind of work that needs a human reading the bundle.

## Suggested first step (when we get there)

Pick option (c) — pcap replay — as the smallest first
slice. We already have a library of captured server-side
pcaps; adapting one of them into a "replay server" that
ryll connects to needs no qemu, no nested virt, no guest
VM image, no CI-runner-VAAPI questions. The harness would
boot ryll-headless, point it at the replay server, and
assert on the resulting auto-snapshot. Even a single
"this pcap should produce ≥1 stream" assertion would
have caught the phase 6 wiring before it landed.

## When to plan this in detail

After the 002/003 cycle wraps and phase 13 (streaming
intermittency investigation) has produced a clear
characterisation of which scenarios are deterministic
enough to bet a CI test on. Trying to write CI assertions
against a streaming heuristic we don't fully understand
yet would just produce flaky tests.

## Cross-references

- `docs/plans/PLAN-stream-caps-and-flap-phase-13-streaming-intermittency.md`
  — the investigation that has to land before this is
  worth planning in detail.
- `docs/plans/PLAN-ci-platform-matrix.md` — covers
  platform / OS coverage. This plan is orthogonal: it
  covers the *behavioural* test surface, not the
  build/run surface.
- The ryll-test-sessions private repo (`sessions/*.tar.gz`)
  — the corpus that a pcap-replay approach would draw
  from.

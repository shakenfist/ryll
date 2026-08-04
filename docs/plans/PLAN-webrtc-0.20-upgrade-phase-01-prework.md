# webrtc-rs 0.20 upgrade — phase 01: pre-work on 0.17

Parent: [PLAN-webrtc-0.20-upgrade.md](PLAN-webrtc-0.20-upgrade.md)

## Prompt

Every change in this phase compiles, tests, and ships against
webrtc-rs **0.17.1**. Nothing here bumps the dependency. The
purpose is to shrink phase 02 — which is an unavoidably large
atomic commit, because the crate does not build between the
version change and the end of the port — by moving as much work
as possible into commits that can be reviewed and landed
normally.

Two rules for anyone executing this phase:

1. **No behaviour changes.** Every step is a refactor. The answer
   SDP, the RTP output, and the lifecycle signals must be
   identical before and after. Where that is hard to eyeball,
   the step says how to prove it.
2. **If a step cannot be done without touching 0.20 API surface,
   stop and move it to phase 02.** The value of this phase is
   entirely in it being landable today.

Recommended planning effort for this phase: **medium** — the
research is front-loaded into this document. Recommended effort
per step is in the step table.

## Situation

### A correction to the master plan

The master plan states that `connection_state()` at
`bridge.rs:861` is a "state accessor used by the reaper". That is
wrong, and the error matters for how this phase is scoped.

`connection_state()` lives inside a `#[cfg(test)]` impl block
(`bridge.rs:832-866`) and is `pub(crate)`. Production code never
calls it. The reaper in `ryll/src/web/lifecycle.rs` uses
`dead_handle()` / `dead_flag_handle()` instead, which are backed
by the `Arc<Notify>` + `Arc<AtomicBool>` pair and do not touch
the peer connection at all.

So the production bridge does **not** need a connection-state
shadow. What needs one is the *test clients*, which call
`connection_state()` on a raw `RTCPeerConnection`
(`loopback.rs:238`, `:248`, `lifecycle.rs:165`, `:175`,
`bridge.rs:1166-1167`) — and that is exactly the call that
disappears from the trait in 0.20.

The master plan is corrected in the same commit as this file.

### The duplication nobody has had to care about until now

There are **four** near-identical client-side peer connection
setups in the tree:

| Site | Lines | Registers H.264 | Polls state | Has DC handler |
|---|---|---|---|---|
| `bridge.rs` in-file test | 904–948 | yes | yes (`:1166`) | no |
| `tests/loopback.rs` | 100–216 | yes (inline) | yes (`:238`) | yes (`:154`) |
| `tests/lifecycle.rs` | 88–140 | — | yes (`:165`) | no |
| `ryll/src/web/signalling.rs` | 434–492 | — | no | no |

Each one does some subset of: `MediaEngine::default()`,
`register_default_codecs()`, H.264 registration, `Registry::new()`,
`register_default_interceptors()`, `APIBuilder`,
`new_peer_connection()`, two `add_transceiver_from_kind()` calls,
`create_offer()`, `set_local_description()`,
`gathering_complete_promise()`, `local_description()`, then a
poll loop on `connection_state()`.

Every one of those calls changes in 0.20. Left as-is, phase 02
rewrites this boilerplate four times, in two crates, with four
chances to get it subtly different — and the differences would be
invisible until an integration test flakes. Collapsing it to one
implementation first is the single highest-leverage thing this
phase does.

Note that `ryll/src/web/signalling.rs` is in a *different crate*,
so the shared helper has to be reachable across the workspace.

### What phase 02 still has to do afterwards

For calibration, after this phase lands, phase 02's remaining
work in `bridge.rs` is roughly: rewrite ~25 `use` statements,
swap `APIBuilder` for `PeerConnectionBuilder`, add
`.with_udp_addrs()`, add the `streams` field in three places, and
change one `impl` block from inherent methods to
`PeerConnectionEventHandler`. Plus the same treatment once in the
shared test helper instead of four times.

## Steps

### 1a — Capture the 0.17.2 performance baseline

Before touching anything. Phase 04 needs something to compare
against, and it cannot be captured after the bump.

Run a real `--web` session against a real SPICE guest for long
enough to reach steady state (20 minutes is enough), and record:
process RSS, per-thread CPU from the runtime metrics, the latency
HUD's distribution, and the video pump's dropped-packet debug
count. Record the commit SHA, the guest, the resolution, and the
browser alongside the numbers, because a baseline without its
conditions is not a baseline.

Write the numbers into this file under a new "Baseline" heading
rather than a scratch file, so they are still there when phase 04
runs.

### 1b — Promote `rtp` to a direct dependency

`bridge.rs:43-47` imports `H264Payloader`, `OpusPayloader`,
`Header`, `Packet` and `Payloader` through the `webrtc::rtp`
re-export, which does not exist in 0.20.

`shakenfist-spice-renderer` already depends on the standalone
crate directly (`rtp = "0.17"`, with a comment at
`shakenfist-spice-renderer/Cargo.toml:131-136` explaining why),
so this step just brings the webrtc crate into line with an
existing project decision.

Add `rtp = "0.17"` to `shakenfist-spice-webrtc/Cargo.toml`,
change the five imports from `webrtc::rtp::*` to `rtp::*`, and
rewrite the comment at `Cargo.toml:15-17` — it currently explains
that webrtc 0.17.1 is pinned because it re-exports a matching
`rtp`, which stops being the reason once we depend on `rtp`
directly.

Behaviour proof: the types are literally the same types; this is
a path change. `cargo tree -p shakenfist-spice-webrtc -i rtp`
should show one `rtp` version, not two.

### 1c — Extract the shared test-client helper

The big one. Create `shakenfist-spice-webrtc/src/test_client.rs`
behind an optional `test-support` feature, exposing a
`TestPeer` type that covers the union of what the four call sites
need:

- `TestPeer::builder()` with opt-in H.264 registration, opt-in
  seed datachannel, and a choice of transceiver directions —
  the sites differ in these, so the builder has to express the
  differences rather than flatten them.
- `offer_and_gather() -> Result<String>` — create offer, set
  local, wait for gathering, return the resolved SDP.
- `set_remote_answer(sdp)`.
- `wait_until_connected(timeout)`.
- Accessors for the raw `Arc<RTCPeerConnection>` so a site that
  needs something the helper does not cover (loopback's
  `on_track` counters) can still reach through.

`shakenfist-spice-webrtc` gets `test-support = []` in
`[features]`; `ryll` enables it on its dev-dependency. The
existing `#[cfg(test)]` impl block at `bridge.rs:832-866` should
fold into the helper and disappear.

Migrate all four call sites. `loopback.rs` keeps its `on_track`
counter wiring and DC echo handler locally — those are genuinely
test-specific — but gets its PC and its SDP dance from `TestPeer`.

Behaviour proof: `make test` passes, and the SDP each site
produces is unchanged. Capture one offer SDP per site before and
after and diff them; they should differ only in ICE ufrag/pwd,
SSRCs, and fingerprints, which are random per PC.

This step is worth doing carefully. If the helper ends up so
general that every call site passes a different combination of
flags, it has failed — prefer two small helpers over one
parameterised one.

### 1d — Shadow connection state in the helper

With 1c landed there is one place to change. Register
`on_peer_connection_state_change` inside `TestPeer::builder()`,
keep the latest state in an `Arc<Mutex<RTCPeerConnectionState>>`
(or an `AtomicU8` with a conversion, if clippy prefers it), and
have `wait_until_connected` read the shadow.

After this step, `RTCPeerConnection::connection_state()` should
have zero call sites in the workspace. That retires one of the
four "no direct replacement" items in the master plan outright.

Behaviour proof: `grep -rn "\.connection_state()" --include="*.rs"`
returns only the helper's own shadow accessor. Tests still pass,
and — important — still actually *wait*, rather than passing
because the shadow defaults to `Connected`. Assert the shadow
starts at `New`.

### 1e — Collapse the bridge's three callbacks into one struct

`bridge.rs` registers three callbacks at `:258`, `:312` and
`:328`, the last nesting a fourth at `:335`. Introduce a
`BridgeEvents` struct holding what they capture today —
`encoder_control: mpsc::Sender<EncoderControl>`,
`dead: Arc<Notify>`, `dead_flag: Arc<AtomicBool>`,
`incoming_tx: mpsc::Sender<Vec<u8>>` — with three async methods:

```
async fn on_state_change(&self, state: RTCPeerConnectionState)
async fn on_control_message(&self, data: Vec<u8>)
async fn on_remote_data_channel(&self, dc: Arc<RTCDataChannel>)
```

Keep registering them through the 0.17 closure API; each closure
becomes a two-line delegation to an `Arc<BridgeEvents>`. Phase 02
then adds `impl PeerConnectionEventHandler for BridgeEvents` and
deletes the closures — the bodies never move again.

Everything those closures capture is already constructed before
the PC exists (`dead` and `dead_flag` at `:247-248`, `incoming_tx`
at `:309`), so this does not reorder construction. It does mean
`control_dc.on_message` and the nested `remote_dc.on_message`
both delegate to the same `on_control_message`, which is already
the intent — the comment at `:295-308` says so explicitly.

Behaviour proof: the sticky-flag semantics are the subtle part.
`dead_flag.swap(true, ...)` at `:282` guards `notify_waiters()`
so only the first terminal transition fires. Keep that inside
`on_state_change` and keep `tests/lifecycle.rs` green — it
asserts both the first `wait_for_dead` resolving and the second
returning immediately via the fast path.

### 1f — Handler-driven ICE-gathering completion

The riskiest item in phase 02, de-risked here.

`accept_offer` (`:429-430`) calls `gathering_complete_promise()`,
which is gone from the 0.20 trait. 0.17 already has
`on_ice_gathering_state_change`, so the replacement design can be
built and validated *today*:

- Add `gathered: Arc<Notify>` + `gathered_flag: Arc<AtomicBool>`
  to `BridgeEvents`, following the same sticky pattern as
  `dead` / `dead_flag` — the reasoning at `:239-248` applies
  identically, and a late subscriber here would hang `accept_offer`
  forever.
- Raise them from a new `on_ice_gathering_state_change` handler
  when the state reaches `Complete`.
- Rewrite `accept_offer` to await that signal instead of the
  promise.
- Do the same for `TestPeer::offer_and_gather` (which is why 1c
  comes first).

Behaviour proof — and this one deserves real rigour, because a
subtly-early signal produces an SDP that is missing candidates
and fails only on some networks:

Run `accept_offer` 20 times before and after, and assert the
answer SDP contains the same number of `a=candidate:` lines each
time. Additionally assert that the gathering signal fires *after*
`local_description()` returns a description containing at least
one candidate — if `on_ice_gathering_state_change` can fire
before the local description is updated, that ordering bug exists
in 0.17 too and we want to find it now, not in phase 02.

### 1g — Re-measure and confirm no regression

Repeat 1a's measurement on the phase-01 tip. Steps 1b–1f are all
refactors, so the numbers should be within noise of the baseline.
If they are not, something in this phase changed behaviour and
the phase is not done.

## Step table

| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
| 1a | low | haiku | none | Run `ryll --web` against a real SPICE guest for 20 min, record RSS, per-thread CPU, latency HUD distribution, and video-pump drop count into a "Baseline" section of `docs/plans/PLAN-webrtc-0.20-upgrade-phase-01-prework.md`, along with commit SHA, guest, resolution and browser. Do not change any code. |
| 1b | low | sonnet | none | Add `rtp = "0.17"` to `shakenfist-spice-webrtc/Cargo.toml`; change the five imports at `bridge.rs:43-47` from `webrtc::rtp::*` to `rtp::*`; rewrite the now-stale comment at `Cargo.toml:15-17`. Mirror the existing declaration in `shakenfist-spice-renderer/Cargo.toml:131-136`. Verify `cargo tree -p shakenfist-spice-webrtc -i rtp` shows a single version. |
| 1c | high | opus | worktree | Create `shakenfist-spice-webrtc/src/test_client.rs` behind a `test-support` feature exposing a `TestPeer` builder, and migrate all four client-PC setups to it: `bridge.rs:904-948`, `tests/loopback.rs:100-216`, `tests/lifecycle.rs:88-140`, `ryll/src/web/signalling.rs:434-492`. Fold away the `#[cfg(test)]` impl at `bridge.rs:832-866`. `ryll` enables the feature on its dev-dependency. Sites keep their own `on_track` / DC-echo wiring. Prove offer SDPs are unchanged modulo per-PC randomness. Read the "Extract the shared test-client helper" section of this plan first — it explains what must stay configurable and warns against over-generalising. |
| 1d | medium | sonnet | none | Inside `TestPeer`, register `on_peer_connection_state_change` and shadow the latest state; make `wait_until_connected` read the shadow. Afterwards `grep -rn "\.connection_state()" --include="*.rs"` must show no calls on a raw `RTCPeerConnection`. Assert the shadow starts at `New` so the tests still genuinely wait. |
| 1e | high | opus | none | Introduce `BridgeEvents` in `bridge.rs` holding `encoder_control`, `dead`, `dead_flag`, `incoming_tx`, with async methods `on_state_change`, `on_control_message`, `on_remote_data_channel`. Reduce the closures at `:258`, `:312`, `:328` (and the nested one at `:335`) to delegations. Preserve the `dead_flag.swap` guard at `:282` exactly — `tests/lifecycle.rs` asserts both the first `wait_for_dead` resolving and the second taking the sticky fast path. Do not reorder construction. |
| 1f | high | opus | worktree | Add `gathered: Arc<Notify>` + `gathered_flag: Arc<AtomicBool>` to `BridgeEvents` using the same sticky pattern as `dead`/`dead_flag`; raise from a new `on_ice_gathering_state_change` handler on `Complete`; rewrite `accept_offer` (`bridge.rs:429-430`) and `TestPeer::offer_and_gather` to await it instead of `gathering_complete_promise()`. Validate per the "Behaviour proof" in this plan's 1f section — 20 runs, identical `a=candidate:` counts, and confirm the signal cannot fire before the local description carries candidates. |
| 1g | low | haiku | none | Repeat 1a's measurement on the phase-01 tip and append it beside the baseline. Flag any difference outside noise as a regression — every step in this phase is meant to be behaviour-preserving. |

Dependencies: 1a first. 1c before 1d and before 1f. 1e before 1f.
1b is independent and can go any time. 1g last.

## Effort

Two days, up from the one day the master plan estimated. The
increase is step 1c, which the master plan did not account for —
the four-way duplication only became visible on a close read of
the test files.

This is a good trade rather than a slip: 1c moves work *out* of
phase 02's atomic commit, where it would have been four parallel
rewrites reviewed as one diff, into a normal reviewable refactor
that runs against a test suite which still works. Phase 02 should
come down by at least as much, and its risk comes down more.

## Acceptance

- `make test` and `pre-commit run --all-files` pass at every
  commit in the phase, not just the tip.
- `webrtc = "0.17.1"` is unchanged in both manifests. If this
  phase bumped the dependency, it did the wrong thing.
- No call to `RTCPeerConnection::connection_state()` or
  `gathering_complete_promise()` remains anywhere in the
  workspace.
- `bridge.rs` registers its callbacks through `BridgeEvents`
  rather than inline closures, and every closure body is a
  delegation.
- One client-PC construction path exists, not four.
- The 1a and 1g measurements agree within noise.

## Open questions

1. **Does `on_ice_gathering_state_change` fire `Complete` before
   `local_description()` carries the candidates in 0.17?** Step
   1f's proof is designed to answer this. If the ordering is not
   guaranteed, the sticky-signal design needs a second condition
   (poll `local_description()` for a non-empty candidate set) and
   phase 02's estimate goes up.

2. **Should `test-support` be a feature or a separate crate?** A
   feature is lighter and matches the workspace, but it means
   `shakenfist-spice-webrtc` — a published crate — carries test
   scaffolding in its source tree. If that grates, the
   alternative is a `shakenfist-spice-webrtc-testkit` dev-only
   workspace member. Decide during 1c; the migration is the same
   either way.

3. **Does `loopback.rs`'s `on_track` wiring belong in the
   helper?** It is currently the only site with it, so the plan
   leaves it local. If phase 02 or a later plan adds a second
   consumer, revisit rather than generalising speculatively.

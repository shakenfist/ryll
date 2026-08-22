# webrtc-rs 0.20 upgrade — phase 04: soak validation and docs

## Prompt

Close the port. Phases 01–03 landed the code; this phase
establishes that a real browser against a real guest behaves the
way 0.17 did, over minutes rather than seconds, and writes down
what it found.

Before executing any step, read the **Baseline** section of
`docs/plans/PLAN-webrtc-0.20-upgrade-phase-01-prework.md` end to
end — not just its table. The conditions block under it is the
specification for the comparable run, and phase 01 already
recorded two deviations from its own brief (the latency HUD and
the runtime-metrics snapshot loop are GUI-mode-only and do not
exist under `--web`) that this phase inherits. Then read the
browser-session report in
`docs/plans/PLAN-webrtc-0.20-upgrade-phase-02-bump.md:670-736`,
which is the only 0.20 browser evidence that exists so far.

Planning effort: high — not because the work is intricate, but
because most of it cannot be re-run cheaply. A soak measured
under conditions that do not match phase 01's produces a number
with nothing to compare it to, and the guest, the driver cadence
and the sampling method all have to match for the comparison to
mean anything.

This phase is unusual for this project in that its central step
is *operator* work. A browser session cannot be delegated to a
sub-agent, and neither can listening to audio. The step table
marks which steps are which.

## Scope

In:

- Landing #289 (a browser with no video codec gets an
  explanation) and #290 (the video pump stops spinning when
  nothing was negotiated). The master plan already names #289 as
  this phase's gate; see Decision 3 for why #290 comes with it.
- A committed soak harness under `tools/`, reproducing phase
  01's sampling method rather than reinventing it.
- The comparable soak: 20 minutes on the uefi-latency-guest with
  Chromium, under phase 01's exact conditions, compared against
  the 1a/1g numbers.
- A second, deliberately non-comparable session on the XFCE
  desktop guest for the qualitative checks — audio **by ear**,
  input, cursor, viewport resize — which the latency guest
  cannot carry because it has no audio device.
- The Firefox question: why its OpenH264 GMP does not load on
  this host, and a written answer to "is a working OpenH264
  enough, or does ryll need a second codec".
- `RYLL_GATHERING_SOAK=1 make test` on a quiet host.
- Closing out the master plan: results recorded, phase table and
  `docs/plans/index.md` set to Complete, and the two Future-work
  entries this planning session found missing (see survey
  finding 6).

Out:

- **A second video codec.** See Decision 4. If the Firefox
  investigation concludes ryll needs VP8 or VP9, the output of
  this phase is an issue and a paragraph, not an encoder.
- Adopting 0.20's send back-pressure, GSO/GRO batching or SCTP
  receive-window tuning. Already out of scope for the whole
  master plan — "port first, tune later" — and this phase is the
  measurement that a later tuning plan would need as its
  baseline.
- Safari. No Mac is available; the master plan already qualifies
  this with "if a Mac is available".
- Anything about the SPICE side of the stack. A regression that
  reproduces without `--web` is not this phase's.

## What the survey found

The master plan's phase 04 section is accurate in intent and
stale in four specific claims. All four are corrected at source
in the master plan as part of this planning commit, so a later
step does not have to rediscover them.

**1. `run_video_pump` is at `bridge.rs:1576`, not `:644`.** The
master plan's line reference predates phase 02's restructuring.
The claim it supports — that 0.20's three headline changes all
land on that write path — is still true.

**2. "with the latency HUD and runtime metrics captured" cannot
be done.** Both are GUI-mode-only: the auto-snapshot loop is
spawned from `app.rs` and the web shell has no latency HUD.
Phase 01 hit this during 1a and substituted external `/proc`
sampling, recording the substitution under *Deviations from the
step brief*. This phase inherits the substitution, and must,
because sampling the same way is what makes the comparison
valid.

**3. The AGENTS.md / ARCHITECTURE.md item is already done.**
The master plan asks this phase to update both "if the bridge's
task and callback structure changed shape, which phase 02 makes
likely". It did change shape, and phase 02 already wrote it up:
`AGENTS.md:166-221` carries a "WebRTC conventions" section
covering the driver event loop, `BridgeEvents`, `StickySignal`
and the `bridge_replaced` notification. `ARCHITECTURE.md:213-220`
carries the file tree including `bind_addrs.rs`, which phase 03
corrected in place. This phase verifies both still describe the
shipped shape and says so; it should not expect to change them.

**4. Phase 01's "Firefox cannot be the phase-04 viewer on this
host" is superseded.** That conditions block records Firefox 140
ESR failing to establish ICE at all under phase 01's loopback
signalling. Phase 02's session on 0.20 contradicts it directly:
ICE was *fully healthy* — nominated pair, consent refreshing on
schedule — and audio, datachannel, input, cursor and resize all
worked. Only video was missing, for a codec reason
(`PLAN-…-phase-02-bump.md:699-712`, #289). Firefox is a viable
phase-04 viewer; it is video-specific, not transport-specific.

Three further things the survey established that the master plan
does not say:

**5. Phase 01's sampler was never committed.** The Baseline's
conditions describe RSS from `/proc/<pid>/status` and per-thread
CPU from `/proc/<pid>/task/*/stat` every 30 s, with host busy%
per sample, plus a QMP `sendkey` driver every 30 s. None of that
exists in `tools/` — `ls tools/` has no soak or sampling script.
Reproducing the conditions therefore means rewriting the
harness, and any silent difference in *how* it samples changes
the numbers it produces. Decision 2 makes writing it down a step
rather than an accident.

**6. Two deferrals from phase 03 were never recorded anywhere.**
`PLAN-…-phase-03-udp-addrs.md:52` and `:176-182` both say
authenticated TURN (a `--web-ice-server` URL with a username and
credential pair) is "Recorded in Future work". It is not: the
master plan's Future work section does not mention TURN, no
issue exists, and no doc does either. This planning commit adds
it. Phase 03's Definition of done was otherwise met — spot-checks
against the tree confirm the three flags exist with clap help
(`ryll/src/config.rs:196-214`), `host_udp_bind_addrs()` is
literally `UdpBindPolicy::default().resolve()`
(`bind_addrs.rs:371-373`), both docs greps pass and the
"not configurable" sentence is gone.

**7. The lockfile resolves webrtc and rtc at 0.20.3, not the
0.20.2 the manifest declares.** A Renovate patch bump landed
between phase 02's port and its browser session, and phase 02
recorded that. It has not moved since. The soak write-up records
the resolved version, because "0.20" is not a precise enough
statement of what was measured.

**8. The Firefox OpenH264 plugin is present on disk and does not
load.** `~/.mozilla/firefox/lv8it6sq.default-esr/gmp-gmpopenh264/2.6.0/`
contains `libgmpopenh264.so` and the profile prefs record a
successful download. Firefox is 140.14.0esr, one patch newer
than the 140.13.0esr phase 02 tested. So the fresh-profile
hypothesis — that the GMP had simply never been fetched — is
already ruled out; phase 02 checked the same thing. Whatever
stops it loading is a loading failure, not an absence, which is
what step 4d has to characterise.

## Decisions

**1. Two sessions, and only one of them is a measurement.**
The comparable soak reproduces phase 01 exactly: uefi-latency-guest,
Chromium, one QMP `sendkey` every 30 s, `/proc` sampling every
30 s, 20 minutes. The desktop-guest session is qualitative —
audio by ear, input, cursor, resize — and produces no numbers
for the table.

The temptation is to do one richer session and get both. That
would be wrong: phase 01's numbers are a *floor-shape* baseline
on a light workload, and its own conditions block says "Phase 04
must reproduce these conditions to compare against these
numbers". Changing the guest changes the encode load, the repaint
pattern and the audio path all at once, and an RSS difference
would then be unattributable — which is the exact failure the
master plan's "port first, tune later" rule exists to avoid.
Separately, the latency guest has no audio device at all (only
`test-qemu-desktop` adds `intel-hda`), so the audio check could
not happen there even if we wanted it to.

**2. The sampler is committed to `tools/`, not improvised.**
Phase 01's harness was ad hoc and is gone (survey finding 5).
Rewriting it from the conditions prose invites small differences
— a 10 s cadence, RSS from `ps` instead of `/proc/<pid>/status`,
per-process instead of per-thread CPU — each of which quietly
changes the number. Writing it as `tools/web-soak.sh` also makes
the *next* soak cheap, which matters because a tuning plan for
0.20's back-pressure and GSO/GRO work will want exactly this
measurement again. This follows the project rule that anything
longer than a few lines is a script in `tools/` rather than
inline.

**3. #289 and #290 land in this phase, before the soak.**
The master plan already gates the phase on #289 ("Land #289
(tell the viewer) before soaking"). #290 comes with it for a
measurement reason rather than a tidiness one: with no video
codec negotiated, the pump keeps encoding and packetising at
frame rate for output the sender discards, and webrtc-rs logs an
unthrottled `ERROR` per packet from inside the library. A
Firefox session under those conditions produces a CPU number
that is measuring the bug, and a log in which nothing else is
findable. Fixing #290 is a precondition for the Firefox half of
this phase producing usable evidence.

They also share a root cause and a detection point —
`resolve_negotiated_payload_types` already knows there is no
common codec — so fixing one without the other means touching
the same function twice.

**4. The Firefox criterion is "a working OpenH264 gets video",
not "ryll gains a second codec".** This is the decision most
likely to be argued with, and the master plan explicitly leaves
it open: "settle whether a Firefox with a working OpenH264
plugin is enough for this criterion or whether ryll needs a
second codec".

Settling it as *enough*, for three reasons. ryll encodes H.264
only by design, and the encoder is shared with the GUI path — a
second codec is a renderer-side project, not a WebRTC-side one.
The observed failure is a Firefox-side plugin-loading problem on
one host (survey finding 8), not a protocol incompatibility:
Firefox's own H.264 support exists and is shipped, it just is
not loading here. And deciding to add a codec *during* a port's
validation phase reintroduces exactly the attribution problem
the whole master plan is structured to avoid — a video
regression after the soak would then have two candidate causes.

If 4d cannot make the GMP load, the honest output is a
documented finding and an issue proposing VP8, not a codec
written in a hurry at the end of a port. The phase still closes:
its Firefox criterion becomes "Firefox reaches a healthy session
and, when it has no video codec, says so in the page" — which
#289 makes true regardless.

**5. 20 minutes, matching, with one stated escape hatch.**
Phase 01's runs were 20 minutes and both showed RSS climbing
through the run (154→215 MB and 161→197 MB), which its verdict
attributes to ring buffers and caches filling to their caps.
Matching the duration is what makes the endpoint numbers
comparable. If the 0.20 run is still climbing at 20 minutes on a
trajectory the baseline's shape does not predict, extend that
run to 60 minutes and record both — an unbounded leak is worth
more than a tidy comparison, and saying which one happened is
cheap.

**6. A red soak does not silently become a green one.**
If the comparison shows a regression outside noise, this phase
does not fix it inline. It records the numbers, files an issue,
and the master plan's phase table says so. A performance fix
made during the run that measures it is a fix with no
independent measurement.

## Step plan

| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
| 4a | medium | sonnet | none | Fix #289 and #290 together in `shakenfist-spice-webrtc/src/bridge.rs`. `resolve_negotiated_payload_types` (near `:946`, and read #289's body for the full diagnosis) already detects the no-common-video-codec case and only warns. Make that state observable in two ways. First, tell the viewer: the control datachannel already carries server-to-browser messages — find the existing message enum and add a variant naming the condition in operator language ("this browser offered no H.264, so there is no video"), and render it in the web shell under `ryll/src/web/` as visible text over the video area rather than a console log. Follow whatever the shell already does for status text; do not invent a new UI mechanism. Second, stop the waste: `run_video_pump` (`bridge.rs:1576`) must not encode or write when no video payload type resolved. The pumps read the resolved type through an `Arc<AtomicU8>` published after `set_remote_description` (see the phase 02 plan's review follow-up for why it is an atomic and not a plain value), so the pump can check it; pick a sentinel or an `Option`-shaped equivalent that distinguishes "not resolved yet" from "resolved to nothing", because the pump starts before `accept_offer` and must not treat startup as failure. Add a unit test in the style of `loopback_media_flows_when_client_offers_a_narrow_codec_set` (`tests/loopback.rs`) that offers a video section with no H.264 at all and asserts both that the viewer-facing message is sent and that no RTP is written. Do not change the codec registration — `register_default_codecs` is deliberately left alone during the port. |
| 4b | medium | sonnet | none | Write `tools/web-soak.sh`, the harness Decision 2 calls for, and document it in `docs/development.md` beside the existing "Manual verification against a desktop guest" section. It must reproduce phase 01's Baseline conditions exactly — read `docs/plans/PLAN-webrtc-0.20-upgrade-phase-01-prework.md:333-412` first, it is the specification. Arguments: the ryll pid (or a way to find it), a duration defaulting to 20 minutes, a sample interval defaulting to 30 s, and the QMP socket for the keypress driver. Per sample it records: RSS from `/proc/<pid>/status` (`VmRSS`), per-thread CPU from `/proc/<pid>/task/*/stat` (fields 14 and 15, utime and stime, summed across threads), whole-host CPU busy% and load average — phase 01 recorded the host figures per sample precisely so contamination from a shared machine is visible in the record, and this host is shared. Separately it drives one QMP `sendkey` every 30 s; note that phase 01 found a 5 s cadence unusable because the guest's mode-set churn outruns the stream's recovery. Emit CSV plus a summary block matching the Baseline table's rows (RSS start→end, RSS max, CPU as a percentage of one core across the whole run). Have it print a warning at startup that the uefi-latency-guest cycles through eight colours one of which is black, so ~30 s of black every 4 minutes is the guest and not a bug. Must pass `tools/run-shellcheck.sh`. Do not run a soak in this step. |
| 4c | — | operator | — | **Operator step.** The comparable soak. Boot `make test-qemu` (the uefi-latency-guest), start `ryll --web --direct localhost:5900` from a `make build` dev-profile binary with `RUST_LOG=info,shakenfist_spice_webrtc=debug,ryll=debug` — the drop counters only log at debug — and connect Debian Chromium with a fresh profile, `--disable-features=WebRtcHideLocalIpsWithMdns` and `--autoplay-policy=no-user-gesture-required`. Step the guest to teal before starting so the colour cycle begins where phase 01's runs began. Run `tools/web-soak.sh` for 20 minutes. Record the resolved webrtc/rtc version from `Cargo.lock` (0.20.3 today) and the commit SHA alongside the numbers. Also confirm the three log lines `docs/development.md:340-348` names, and that the answer SDP carries at least one candidate and none with an unspecified address. |
| 4d | high | opus | none | **Operator-assisted.** Characterise the Firefox OpenH264 failure and settle Decision 4 in writing. The plugin is present on disk and the profile prefs record a successful download (survey finding 8), so this is a load failure. Start with `about:support` (Media section, GMP plugin state), `about:addons` → Plugins, the browser console with `media.gmp.log.level` turned up, and Firefox's GMP sandbox — Debian's `firefox-esr` packaging and the RDP/Kasm session are both plausible culprits and neither is ryll's. Generate an offer from `about:webrtc` and confirm against the offer SDP rather than against `RTCRtpReceiver.getCapabilities('video')`, which `docs/development.md:361-366` already warns is not evidence. Then either: the GMP loads and Firefox gets video, in which case run the desktop-guest session on Firefox too and record it; or it does not, in which case write down precisely what blocks it, confirm #289's message is what the user sees, and file an issue proposing a second codec with the evidence attached. Either way the outcome is a paragraph in this plan's results section and, if needed, an issue — not an encoder. |
| 4e | — | operator | — | **Operator step.** The qualitative session. `make test-qemu-desktop`, then `ryll --web --direct localhost:5900` and a browser. Check, and record each: video shows the XFCE desktop at the guest's resolution; **audio is audible by ear** — this is the clause inherited from phase 02, where `playback: MODE: 3` proved Opus was negotiated and nobody listened; keyboard and mouse reach the guest; the cursor shape follows; viewport resize propagates. This session produces no numbers and is not compared against the baseline. |
| 4f | low | haiku | none | **Operator-run, agent-recorded.** Run `RYLL_GATHERING_SOAK=1 make test` on a quiet host and record the result. This is the deliberate occasion the 20-iteration invariant-candidate-count check in `accept_offer_answer_carries_all_candidates` (`bridge.rs:2485-2495`) is gated for; `docs/development.md:299-304` explains why it is off by default. If it fails, capture the candidate counts across iterations before concluding anything — host interface churn is the expected false positive and the log distinguishes it. |
| 4g | medium | sonnet | none | Close the plan out. Write a "Results" section into this file carrying the 4c table beside phase 01's 1a/1g columns, the 4d Firefox finding, the 4e qualitative checklist and the 4f result, each with the commit SHA and resolved webrtc version. Verify — do not assume — that `AGENTS.md:166-221` and `ARCHITECTURE.md:213-220` still describe the shipped shape (survey finding 3 says they do; say so explicitly either way, and change them only if they do not). Set this phase to Complete in the master plan's phase table and in `docs/plans/index.md`, and set the master plan's overall status to Complete there too, since this is its last phase. Check the master plan's Success criteria list item by item and note any that this phase could not satisfy, with the reason — Safari has no Mac, and Firefox's outcome depends on 4d. |

Dependencies: 4a and 4b are independent and can run in parallel.
4c depends on both — on 4a because an unfixed #290 would spin
the pump during measurement (Decision 3), on 4b because it is
the harness. 4d depends on 4a for the viewer-facing message.
4e and 4f depend on nothing but the tree building. 4g depends on
all of them.

**Back-brief gate before 4c.** The soak is the one step in this
phase that is expensive to redo — 20 minutes of wall clock, a
booted guest and an attended browser — and a condition that
does not match phase 01 is not discoverable until the numbers
are already wrong. Before starting it, restate the conditions
that will be used and check them against the Baseline's
conditions block line by line.

## Risks and mitigations

- **The measurement is contaminated by the shared host.** This
  machine runs other work; phase 01's 1g run already caught one
  external spike to 29% busy. Mitigation: 4b records host busy%
  and load average per sample, as phase 01 did, so the
  contamination is visible in the record rather than folded into
  the number. If a spike lands inside the window, discard the
  run and repeat it — the harness makes that cheap, which is
  half the point of Decision 2.
- **The conditions drift from phase 01's without anyone
  noticing.** Guest, cadence, browser flags, build profile and
  log level all affect the numbers, and prose is a poor
  specification. Mitigation: the back-brief gate before 4c, and
  4b encoding the sampling half in a script rather than in a
  reader's memory.
- **A regression is found and the phase quietly absorbs it.**
  The pressure at the end of a port is to explain a number away.
  Mitigation: Decision 6 states the rule in advance — record,
  file, and let the phase table say so. The management session
  checks the 4g write-up against the 4c raw CSV rather than
  against 4c's summary.
- **#289's viewer-facing message becomes a second place that
  can be wrong.** A message rendered in the page is a claim
  about negotiation state that can go stale or fire spuriously —
  a viewer that *does* have H.264 must never see it.
  Mitigation: 4a's test asserts both directions, and the
  existing `loopback_media_flows_when_client_offers_a_narrow_codec_set`
  is the negative case — it offers H.264 at browser-chosen
  payload numbers and must stay silent.
- **4a's pump gating misreads startup as failure.** The pumps
  are spawned before `accept_offer`, so "no payload type
  resolved" is the *normal* state for the first moments of every
  session. A naive check would suppress video on every
  connection. Mitigation: the brief calls this out explicitly
  and requires the sentinel to distinguish the two states; the
  management session checks that distinction in the 4a diff, and
  `tests/loopback.rs` passing at all is the regression signal.

## Definition of done

Falsifiable, in the order a reviewer would check them:

- `tools/web-soak.sh` exists, passes `tools/run-shellcheck.sh`,
  and `docs/development.md` explains when to reach for it.
- This file carries a Results section whose table has the same
  rows as phase 01's Baseline table, with the commit SHA and the
  webrtc version resolved in `Cargo.lock` recorded beside it.
- The Results section states, in words, whether the 0.20 numbers
  are within noise of 1a — and if they are not, links the issue
  filed for it.
- A browser session is recorded in which audio was confirmed
  **by ear**, not by `playback: MODE: 3`.
- A browser with no H.264 sees an explanation in the page. A
  unit test asserts it is sent in that case, and
  `loopback_media_flows_when_client_offers_a_narrow_codec_set`
  still passes, asserting it is not sent when a codec did
  negotiate.
- With no video codec negotiated, the video pump writes no RTP —
  covered by the same test, and observable as the absence of
  `Failed to send RTP` in a Firefox session log.
- The Firefox outcome is written down either way: video works,
  or the loading failure is characterised and an issue exists.
- `RYLL_GATHERING_SOAK=1 make test` result is recorded.
- `AGENTS.md` and `ARCHITECTURE.md` have been checked against
  the shipped bridge shape, with the finding stated explicitly
  rather than by silence.
- Every item in the master plan's Success criteria is either
  satisfied or listed as unsatisfied with a reason.
- The master plan's phase table, `docs/plans/index.md` and this
  file all say Complete, and the master plan's overall status in
  `index.md` is Complete.
- `make test`, `make lint` and `pre-commit run --all-files` all
  pass.

## Effort

One day, matching the master plan's estimate, but the shape is
different from the other phases: perhaps two hours of code (4a,
4b), an hour of attended browser time spread across 4c, 4d and
4e, and the rest write-up. The 20-minute soak is wall clock
rather than effort, and 4d is the variance — a GMP that will not
load can absorb an afternoon and still end in "it is Firefox's
packaging, not ours".

The phase is cheap to plan and expensive to redo, which is why
Decision 2 and the back-brief gate exist.

## Status

Planned. Not started.

## Back brief

Before executing any step of this plan, please back brief the
operator as to your understanding of the plan and how the work
you intend to do aligns with that plan.

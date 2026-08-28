# Phase 18: Push audit

Parent plan:
[PLAN-stream-caps-and-flap.md](PLAN-stream-caps-and-flap.md)

## Goal

Run `PUSH-AUDIT.md` over the accumulated diff of every
stream-caps phase that landed, so the audit sees what the
capability, decoder, diagnostics and cache work did to *each
other* rather than what each phase did in isolation.  Findings
land as their own PR against `develop`, recorded in the master
plan under *Items deferred from the push audit*.  The plan is
not complete until every finding is fixed or declined in
writing.

## Planning effort

**Medium.**  The master plan's phase 18 section recommends
**low**, on the grounds that "the runbook does the work; the
phase plan is a wrapper".  That was written before anyone
measured the diff.  It is 64 files and 16 159 insertions —
8 829 of them Rust — which is roughly ten times the accumulated
diff the only previous closing audit in this repository
(`PLAN-idle-cpu-and-latency` phase 6) worked over.  A wrapper
that hands one agent a 19 000-line patch and asks for
"findings" gets a skim.  Splitting the patch, deriving the
range, and deciding what to do about the non-plan commits
inside it is the work below, and it has been done here so the
steps themselves stay light.  The master plan's effort
recommendation has been corrected at source.

## Scope

**In scope:**

- Deriving and recording the audit range for phases 1-12, 14
  and 15, and populating the master plan's `Merged` column so
  the derivation does not have to be repeated.
- Both mechanical waves of `PUSH-AUDIT.md` against that range.
- The wave 2 judgment reviews, split by area.
- Triaging every finding against current `develop`.
- Recording each finding as fixed or declined, in writing, in
  the master plan.

**Out of scope:**

- *Fixing* the findings.  Fixes land as their own PR against
  `develop`, per the master plan.  This phase's PR is the plan
  plus the master-plan corrections.
- Phases 13, 16 and 17 (parked, no code) and phase 15's
  awaiting-reproduction remainder.  Phase 15's landed
  instrumentation *is* in the range; its open investigation is
  not.
- The outstanding operator smoke tests (see *What the survey
  found*).  They gate the master plan going `Complete`; they do
  not gate this audit, which reads landed code.
- Re-auditing the crate extraction.  It predates this plan.

## What the survey found

The master plan's phase 18 section was written before any of
this was checked.  Its central premise — that the accumulated
diff "has to be assembled from their merge commits" and that
pre-convention phases must be "reconstruct[ed]" — turns out to
be true in principle and much cheaper in practice than it
sounds.  Two of its neighbouring claims were wrong and have
been corrected at source in the master plan as part of this
phase's commit; this section records what was found so a later
step does not redo it.

### The audit range is exact

Every phase of this plan landed on one long-lived branch,
`feedback-002`, in exactly **two** pull requests, and those two
merges are **adjacent on `develop`'s first-parent history** —
`cd4c7d9`'s first parent is `f22416a`.  Nothing unrelated
landed between them.

| PR | Merge | Date | Commits | Files | Insertions |
|----|-------|------|---------|-------|-----------|
| #102 | `f22416a` | 2026-05-31 | 92 | 62 | 15 348 |
| #105 | `cd4c7d9` | 2026-06-01 | 22 | 17 | 856 |

So the range is not merely recoverable, it is a plain
two-endpoint range with no filtering needed:

```
AUDIT_BASE=d416338      # f22416a^1, the develop commit before PR #102
AUDIT_HEAD=cd4c7d9      # the PR #105 merge
```

`audit-range.sh` builds `${AUDIT_BASE}...${AUDIT_HEAD}`
(three-dot).  Because `d416338` is an ancestor of `cd4c7d9`,
the merge base is `d416338` itself and the symmetric difference
equals the two-dot range.  Measured both ways: **64 files,
16 159 insertions, 623 deletions.**

Phase-to-PR mapping, for the `Merged` column:

| Phases | Landed in |
|--------|-----------|
| 1-8, 11A, 12, 14, 15 (and the 13/16/17 plan stubs) | `f22416a` (PR #102) |
| 9, 10, 11B | `cd4c7d9` (PR #105) |

This is the third distinct shape `PUSH-AUDIT.md`'s *Two ways
this runbook is invoked* section has now met — one PR
(`idle-cpu-and-latency`), and now two adjacent PRs.  The
runbook already covers it ("look at how the phases landed
before concluding the diff is unrecoverable"), so it needs no
edit this time; step 18f only fills in the table.

### The diff is large, and mostly not code

| Slice | Insertions |
|---|---|
| Rust (`*.rs`) | 8 829 |
| `docs/plans/**` | 5 683 |
| Everything else (docs, CI, Cargo, fixtures) | 1 647 |
| **Total** | **16 159** |

The five largest Rust files account for over half the code:

| File | +/- |
|---|---|
| `shakenfist-spice-compression/src/jpeg.rs` | +1 880 / -0 |
| `shakenfist-spice-compression/src/video.rs` | +979 / -0 |
| `shakenfist-spice-renderer/src/channels/display.rs` | +866 / -383 |
| `ryll/src/bugreport.rs` | +583 / -23 |
| `shakenfist-spice-renderer/src/snapshots.rs` | +530 / -17 |

This is what drives decision 3.

### Five commits in the range are not this plan's

The range is exact for *the branch*; the branch carried a
little else.  In `f22416a`:

| Commit | What | Verdict |
|---|---|---|
| `7115df8` | `ci: workflow_dispatch build for arbitrary branches` (+158, `.github/workflows/manual-build.yml`) | not this plan |
| `d723074` | `Include git sha in --version output` | not this plan |
| `6650b86`, `098bb0a` | `docs/plans/PLAN-streaming-test-automation.md` (+184) | a *different* master plan, spun out of this one |
| `d2dadb7` | `ci: fix cargo-deny failures on PR #102` | caused by this plan's new dependencies — in scope |
| `41f984a` | `docs/libvirt-spice-recommendations.md` (+468) | landed with the phase 9 plan commit — in scope |

Total genuinely-foreign content is under 400 lines across two
files plus one plan document.  Decision 2 keeps them in the
range rather than assembling a hand-filtered patch.

### The audit harness's own defects are fixed

The `idle-cpu-and-latency` audit found that three of the four
range-scoped wave 1 checks were looking at the wrong places
after the crate extraction, and that its fatal `println!` check
did not scan `shakenfist-spice-renderer/` at all — 46% of the
workspace.  Both are fixed on current `develop`:
`tools/audit/wave1-checks.sh` now derives the scan set from the
workspace members in `Cargo.toml`, and
`tools/audit/test-audit-range.sh:36` unsets `AUDIT_BASE` /
`AUDIT_HEAD` before building its scratch repository, so wave 1
no longer fails through pre-commit when the bounds are
exported.

This matters more here than it sounds.  **Most of this plan's
code lives in the two crates that check could not see** —
`shakenfist-spice-renderer` and `shakenfist-spice-compression`
hold roughly 6 700 of the 8 829 Rust insertions.  Had this
audit run before those fixes, wave 1 would have passed
vacuously over the bulk of the diff.

### The code has not moved, but it has drifted

Unlike the `idle-cpu-and-latency` audit — where the crate
extraction had relocated most of the audited code between the
diff and the audit — **all 64 files in this range still exist
at the same paths today**, checked file by file.  There is no
mapping table to maintain.

There is, however, three months of drift *within* those files,
and it is uneven.  Measured `cd4c7d9..develop` on the audited
Rust files:

| File | Since the plan landed |
|---|---|
| `ryll/src/app.rs` | +517 / -332 |
| `shakenfist-spice-renderer/src/channels/main_channel.rs` | +111 / -212 |
| `shakenfist-spice-renderer/src/channels/display.rs` | +152 / -190 |
| `shakenfist-spice-renderer/src/channels/playback.rs` | +32 / -114 |
| `shakenfist-spice-compression/src/jpeg.rs` | +20 / -17 |
| `shakenfist-spice-compression/src/video.rs` | +16 / -19 |

So the compression crate — where the largest and most
security-relevant part of the diff is — is essentially
untouched since it landed, and its findings will be live.  The
renderer channels have moved enough that a triage pass is
mandatory.  That is step 18e.

### One thing found and deliberately not fixed

Fifteen of the Execution table's eighteen `Status` cells carry
prose ("Code landed (5A-5B); 5C operator smoke test pending")
where the `plan-status-vocabulary` shared block
(`PLAN-TEMPLATE.md:144`) asks for exactly one term and nothing
else.  The table predates the block.  Rewriting fifteen rows
would discard status detail that has no other home, so it is
recorded here and handed to step 18g as a decision rather than
folded into this phase.

### One stale claim in the master plan, corrected

The phase 10 row claims the documentation catch-all landed an
"ARCHITECTURE.md capability table + README CLI flag docs".
Neither is where it says today: `ARCHITECTURE.md` mentions no
capability by name (`grep -c 'STREAM_REPORT\|LZ4_COMPRESSION'`
returns 0) and `README.md` mentions none of the CLI flags.  The
content survives — the capability table is in
`docs/spice-protocol.md`, and `--auto-snapshot-interval` is
documented in `docs/features.md:145` and
`docs/diagnostics.md:401` — it was relocated by the later
`llm-doc-structure` (PR #277) and `readme-pitch` (PR #222)
work, which is exactly what those changes were for.  The row
has been corrected to name the current locations, so step 18d's
documentation agent does not report a gap that does not exist.

### What is outstanding, and why it does not block this phase

Five operator smoke tests remain open across the plan: 2C/3H
(per-platform JPEG decode matrix), 5C (auto-snapshot), 6F
(H.264 wire smoke, blocked on an H.264-capable spice-server
build), 9E (deliberate vdagent freeze), and 11C (long-idle
soak).  Phases 13, 16 and 17 are parked and have no code;
phase 15 awaits a reproduction.

None of them changes the diff this phase audits, and none can
be run from a session — they need the operator and a guest.
The master plan's own closeout section directs that phase 18
"closes the plan out once the phases that are going to land
have landed", and all code has landed.  This phase therefore
proceeds, and the master plan stays `In progress` until the
smokes close or are declined.  **Marking the master plan
`Complete` is not part of this phase's definition of done**,
which is the one place this phase deliberately differs from the
`idle-cpu-and-latency` audit it is modelled on.

## Decisions

1. **Audit the May diff as it landed, then triage every
   finding against current `develop` before acting.**  Same
   call the `idle-cpu-and-latency` audit made, and the one a
   reviewer is most likely to argue with: auditing today's
   version of this code would produce more immediately
   actionable findings.  It is rejected because it answers a
   different question — this phase exists to ask what *this
   plan* did to the codebase, and code that someone else has
   since refactored has not thereby been audited.  The cost is
   step 18e, and the survey shows that cost is concentrated in
   the renderer channels; the compression crate barely moved.

2. **Use the range as it stands, foreign commits included,
   rather than assembling a filtered patch.**  Under 400 lines
   of genuinely foreign content across `manual-build.yml` and
   the `--version` change, plus one unrelated plan document.
   Filtering them out would cost a hand-assembled patch and buy
   very little; the alternative failure — an agent auditing the
   wrong plan, which is why the `idle-cpu-and-latency` audit
   needed a patch file — does not arise here, because both
   merges are this plan's.  Every agent brief names the five
   commits and says to skip them.

3. **Split the patch by area for the judgment agents; do not
   hand any one agent the whole 19 000 lines.**  The wave 2
   briefs in `PUSH-AUDIT.md` assume a diff an agent can hold in
   mind.  Code quality and security are split across two area
   halves each — the compression crate (decoders, caches; the
   attacker-facing surface) and the renderer/client
   (diagnostics, snapshots, channel handlers, GUI).  Test and
   documentation review stay whole, because both are
   cross-cutting questions that a split would make harder
   rather than easier.  Six judgment agents, not four and not
   sixteen.

4. **Security review gets the larger share of the budget, and
   it goes to the decoders.**  `jpeg.rs` (+1 880) and `video.rs`
   (+979) parse attacker-controlled bytes from the wire across
   four platform backends, two of which are FFI
   (`ImageIO`, `WIC`) and one of which is `dlopen`-probed
   (VA-API).  `lz4.rs` and `byte_bounded_lru.rs` decompress and
   cache on server-supplied sizes.  That is where a real
   vulnerability would be, and it is the part of the diff that
   has not been touched since it landed.

5. **Populate the `Merged` column as part of this phase, not
   the findings PR.**  It is the artefact that makes a future
   re-audit cheap, it is derived work this phase already did,
   and leaving it for a PR that may find nothing to fix would
   risk losing it.

6. **Findings land as a separate PR from this plan file**, per
   the master plan.  This phase's PR is the plan plus the
   master-plan corrections (the `Merged` column, the phase 10
   documentation locations, and the phase 18 effort level).

7. **This phase does not mark the master plan `Complete`.**
   The outstanding operator smokes are real work, and a closing
   audit that quietly flips the status would erase them.  Step
   18g records the audit outcome; the status stays `In
   progress` with the reason stated in the plan, which is what
   `docs/plans/index.md`'s own vocabulary section asks for.

## Steps

Each step is its own commit where it changes files; 18a-18e
produce findings recorded in this file rather than code.

| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
| 18a | low | haiku | none | Assemble the audit patches. `git diff d416338 cd4c7d9 > /tmp/plan-audit.patch`, then four area sub-patches from the same range, using `git diff d416338 cd4c7d9 -- <paths>`: (i) `compression.patch` — `shakenfist-spice-compression/`; (ii) `renderer.patch` — `shakenfist-spice-renderer/`; (iii) `client.patch` — `ryll/`; (iv) `docs.patch` — `docs/ ARCHITECTURE.md README.md AGENTS.md` and any other `*.md`. Print the diffstat of each and of the whole. Expected shape, and the gate for step 18b: the whole patch is **64 files, 16 159 insertions, 623 deletions**; if it is not, the range broke and everything after this is wasted. Do not interpret anything; this step only assembles. |
| 18b | low | sonnet | none | Run wave 1: `AUDIT_BASE=d416338 AUDIT_HEAD=cd4c7d9 tools/audit/wave1.sh`. Exit codes are tabulated in `PUSH-AUDIT.md`. Two things to know before reading the output. First, wave 1's build, lint and test stages run against the **current tree**, not the audit range, so a failure there means something regressed on `develop` today rather than something wrong with this plan — say which it is. Second, the range-scoped style checks read file content **at `AUDIT_HEAD`**, i.e. at its 2026-06-01 state, so a long-line or unguarded-`log_message` hit may already have been fixed; report them, do not fix them, and mark each as needing the step 18e check. Note in the report whether the `println!`/`eprintln!` check actually scanned `shakenfist-spice-renderer/` and `shakenfist-spice-compression/` — it could not before PR #325, and those two crates hold most of this diff. If wave 1 fails on codes 1-3, stop and report; do not proceed to wave 2. |
| 18c | low | sonnet | none | Run `AUDIT_BASE=d416338 AUDIT_HEAD=cd4c7d9 tools/audit/wave2-mechanical.sh` and report its output verbatim, then add the style-conformance judgment review from `PUSH-AUDIT.md`'s *Style conformance — judgment portion* against `/tmp/plan-audit.patch`. Two areas deserve particular attention. (i) The `repaint_notify.notify_one()` pairing requirement (`docs/design-decisions.md` decision #17): this plan added event sends across the display, playback, usbredir, webdav and main channels — check every `send_event` / `event_tx.send` site in the patch has its pairing. (ii) Channel-prefix log conventions on the new diagnostic logging, which this plan added a lot of. Skip the five non-plan commits listed in this plan's survey section (`7115df8`, `d723074`, `6650b86`, `098bb0a`, and the `PLAN-streaming-test-automation.md` hunks). |
| 18d | medium-to-high | sonnet (2a, 2b, 2c), opus (2d) | none | Six judgment agents from `PUSH-AUDIT.md`, run in parallel, each given a **patch file path** rather than a revision range. Use the runbook's briefs verbatim, with the additions below. All six: the patch is from 2026-06-01; report what the patch shows and do **not** check it against the current tree — that is step 18e's job, and six agents redundantly repeating it is the waste this split exists to avoid. All six: skip the five non-plan commits named in the survey. **2a-1** (code quality, `compression.patch`) — the four JPEG backends in `jpeg.rs` and the MJPEG/H.264 dispatch in `video.rs` are the place a missed abstraction would show; check the backends against the `JpegDecoder` trait for logic that should have been shared. **2a-2** (code quality, `renderer.patch` + `client.patch`) — phase 4 expanded four channel snapshots from the same template; look for the copy-paste that implies. **2b** (test review, whole patch) — `jpeg.rs`, `video.rs`, `lz4.rs` and `byte_bounded_lru.rs` are the new-module cases; note explicitly which of the four platform JPEG backends can be tested on Linux CI and which cannot, since "untested" and "untestable here" are different findings. **2c** (documentation review, `docs.patch` plus the whole patch for context) — note that the capability table now lives in `docs/spice-protocol.md` and the CLI flags in `docs/features.md` / `docs/diagnostics.md`, having been relocated by PRs #222 and #277; do not report their absence from `ARCHITECTURE.md` and `README.md` as a gap. **2d-1** (security, opus, high effort, `compression.patch`) — the highest-value target in this audit. `jpeg.rs` parses attacker-controlled JPEG across ImageIO (macOS FFI), WIC (Windows COM), VA-API (`dlopen`-probed, with hand-rolled JPEG header parsing) and mozjpeg; `video.rs` feeds openh264 wire data; `lz4.rs` decompresses on server-supplied sizes; `byte_bounded_lru.rs` and the GLZ dictionary cap bound memory a malicious server controls. Check unchecked indexing, unbounded or attacker-sized allocation, integer overflow in size arithmetic, `unsafe` invariants and `Send`/`Sync` claims on FFI handles, and COM threading. **2d-2** (security, opus, high effort, `renderer.patch` + `client.patch`) — concurrency and resource exhaustion: the auto-snapshot tokio task and its file rotation cap, the shared `MmClock`, the vdagent probe's reply bookkeeping on the main channel, and whether any new bug-report path can be driven to unbounded disk or memory growth by the server. |
| 18e | high | opus | none | Triage. Take every finding from 18b, 18c and 18d and classify each against **current `develop`** as `still-present`, `already-fixed` or `moved`. No file mapping is needed — all 64 audited files are still at the same paths — but the drift is uneven and the survey table in this plan says where: `app.rs`, `main_channel.rs`, `display.rs` and `playback.rs` have moved substantially since `cd4c7d9`, while `jpeg.rs` and `video.rs` have barely changed, so compression findings should be assumed live until shown otherwise and channel findings should be checked line by line. For each `still-present` finding give the current file and line. Be conservative: a finding you cannot locate is `already-fixed` **only** if you can point at what fixed it; otherwise it stays `still-present` and gets a human look. Output a table: finding, source agent, severity, status, current location. |
| 18f | low | sonnet | none | Record the derivation in the master plan so it never has to be repeated. Fill the `Merged` column of `PLAN-stream-caps-and-flap.md`'s Execution table using the phase-to-PR mapping in this plan's survey section: `f22416a` (PR #102) for phases 1-8, 11, 12, 14 and 15; `cd4c7d9` (PR #105) for phases 9, 10 and 11B; `—` for the parked 13, 16 and 17 and for this phase until it merges. Put commits in that column and nothing else. While in that table, note — do not fix — that fifteen of its `Status` cells carry prose where the `plan-status-vocabulary` shared block (`PLAN-TEMPLATE.md:144`) asks for a single term; that predates the block, it is a consistency finding rather than this phase's work, and step 18g decides whether it becomes one. `PUSH-AUDIT.md` needs no edit: its *Two ways this runbook is invoked* section already tells the reader to look at how the phases landed, which is what worked here. Own commit, subject "Record where each stream-caps phase landed." |
| 18g | medium | opus | none | Management step, not a sub-agent step: review the 18e table, decide fix-or-decline for each finding, and record the outcome in the master plan under a new *Items deferred from the push audit* heading, matching the shape `PLAN-web-frontend.md` uses minus the phase number. Every finding must be fixed or declined **in writing**, with a reason for each declination. If the audit found nothing, that is one sentence and the phase is done. Also fill in the master plan's *Bugs fixed during this work* section if it is still placeholder text — eighteen phases either fixed something or did not, and both are answers. Leave the master plan's status at `In progress` with the outstanding operator smokes named as the reason (decision 7). Fixes land as their own PR against `develop`; this step only decides and records. |

## Risks and mitigations

- **The audit reports "no findings" because it looked at
  nothing.**  The failure this phase exists to prevent, and the
  empty-range guard (exit 6) only catches the degenerate case.
  *Mitigation:* step 18a prints the diffstat and this plan
  states the expected numbers (64 files / 16 159 / 623).  A
  reviewer should check those three numbers first; if they do
  not match, the range broke.
- **An area agent reports on the wrong slice.**  Six agents
  with four patch files is more bookkeeping than the runbook's
  four-with-one.  *Mitigation:* step 18a names each sub-patch
  and prints its diffstat, and each brief in 18d names the
  patch file it gets by the same name.  Step 18e will notice a
  slice nobody reported on, because it enumerates by source
  agent.
- **Stale findings burn the findings PR's credibility.**  Three
  months is long enough for some of this to be gone, and
  `app.rs` alone has turned over 849 lines since.  *Mitigation:*
  step 18e, and its rule that `already-fixed` requires pointing
  at the fix; step 18g checks that every `already-fixed` claim
  carries one.
- **The security review skims the biggest file in the diff.**
  `jpeg.rs` is 1 880 lines of four-backend FFI, which is a lot
  to ask of one agent even at high effort.  *Mitigation:*
  decision 4 gives it a dedicated opus agent with only the
  compression patch, and the brief enumerates the specific
  hazard classes per backend rather than asking for "security
  issues".  If 2d-1 comes back thin relative to that surface,
  re-run it per-backend rather than accepting the result.
- **Phase 18 quietly closes a plan that is not closed.**  Five
  operator smokes are outstanding and a closing audit is
  exactly the moment they would get lost.  *Mitigation:*
  decision 7, and a definition-of-done item that asserts the
  master plan still reads `In progress` with the smokes named.

## Definition of done

Falsifiable items only.

- `git diff --shortstat d416338 cd4c7d9` reports **64 files
  changed, 16 159 insertions(+), 623 deletions(-)**, and step
  18a's assembled patch matches.
- The four area sub-patches exist and their file counts sum to
  64.
- `tools/audit/wave1.sh` has been run with the bounds above and
  its exit code is recorded in this file.
- The wave 1 report states whether the `println!` check scanned
  `shakenfist-spice-renderer/` and `shakenfist-spice-compression/`.
- `tools/audit/wave2-mechanical.sh` output is recorded in this
  file, verbatim.
- All six wave 2 judgment agents have reported, and each report
  is either summarised in this file or its findings appear in
  the 18e table.
- The 18e table exists; every row has a status of
  `still-present`, `already-fixed` or `moved`, and every
  `already-fixed` row names what fixed it.
- The master plan's Execution table has a non-`—` `Merged`
  entry for every phase that landed (1-12, 14, 15), and `—` for
  13, 16 and 17.
- The master plan has an *Items deferred from the push audit*
  section in which every finding is marked fixed or declined
  with a reason — or a single sentence recording that the audit
  found nothing.
- The master plan's *Bugs fixed during this work* section is no
  longer placeholder text.
- The master plan's phase 18 row reads `Complete`; the master
  plan's own status in `docs/plans/index.md` still reads `In
  progress`, and the master plan names the five outstanding
  operator smokes as the reason.
- `pre-commit run --all-files` passes; `make test` passes.

## Back brief

Before executing any step, back brief the operator on the
understanding of this phase and how the intended work aligns
with it.

Three gates where the work is cheap to propose and expensive to
redo, so stop for agreement rather than proceeding:

- **After step 18a**, confirm the assembled patch is the right
  patch — the three headline numbers, and the decision to leave
  the five foreign commits inside the range rather than filter
  them out.  Every later step is wasted if this is wrong.
- **Before step 18d spawns**, confirm the six-agent split and
  which patch each gets.  Respawning six agents over a bad
  split is the most expensive mistake available here.
- **Before step 18g acts on the 18e table**, agree the
  fix-or-decline split.  Declining a finding in writing is a
  judgment the operator owns, not the audit's.

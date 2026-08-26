# Phase 6: Push audit

Parent plan: [PLAN-idle-cpu-and-latency.md](PLAN-idle-cpu-and-latency.md)

## Goal

Run `PUSH-AUDIT.md` over the accumulated diff of phases
1-5, so the audit sees what the repaint, logging, latency
and metrics changes did to *each other* rather than what
each did in isolation.  Findings land as their own PR
against `develop`, recorded in the master plan under
*Items deferred from the push audit*.  The plan is not
complete until every finding is fixed or declined in
writing.

## Planning effort

**High.**  Not because the audit itself is subtle — the
runbook is written — but because this is the first
master-plan closing audit run in this repository, and the
range derivation, the four-month staleness of the diff,
and the interaction with the crate extraction all had to
be worked out before a sub-agent could be briefed.  That
work is below, so the *steps* are mostly medium.

## What the survey found

The master plan's phase 6 section was written before any
of this was checked.  Three of its premises were wrong,
and they have been corrected at source in the master plan
as part of the phase 2 closeout commit — this section
records what was found so a later step does not redo it.

### The audit range is exactly derivable

The master plan assumed five per-phase merge commits
needing reconstruction.  There is **one**: all five phases
landed on the `screenshot` branch, merged as PR #36
(`6d52665`).  The `Merged` column now records it.

Better, the plan's commits run *contiguously* on that
branch, so the range is exact:

```
AUDIT_BASE=90a954b^1     # 8486269, the commit before the master plan
AUDIT_HEAD=1c28d6f       # "Rename last_latency to last_latency_ms"
```

Thirteen commits, and `tools/audit/audit-range.sh` accepts
the bounds as given.  The three candidate ranges, measured:

| Range | Files | Insertions | Verdict |
|---|---|---|---|
| `90a954b^1..1c28d6f` | 25 | 1 957 | **use this** |
| `90a954b^1..85bc901` | 43 | 4 041 | crosses the develop merge |
| `90a954b^1..develop` | 340 | 119 684 | the naive range `PUSH-AUDIT.md` warns about |

Roughly 1 150 of those 1 957 insertions are the plan files
themselves, so the code under audit is about 800 lines.
This is a small audit.

`PUSH-AUDIT.md` cites this very plan as its worked example
of a range that cannot be derived after the fact, and on
the per-phase question it is right.  On the whole-plan
question it is too pessimistic, and the reason is specific
rather than general: this plan's phases happened to land
on one branch, contiguously.  **Do not generalise this to
other plans** — and consider whether `PUSH-AUDIT.md`'s
example paragraph should be softened, which is step 6f.

### Two commits the range does not cover

- **`85bc901`** ("Address automated reviewer feedback on
  PR #36") sits *above* `AUDIT_HEAD`, touches only
  `ryll/src/app.rs` (+41/-21), and is **half in scope**.
  Two of its four items are this plan's (`LatencyTracker`
  history moved to `VecDeque`; the redundant
  `last_latency_ms.is_some()` GUI guards replaced with
  `!self.latency.history.is_empty()`).  The other two are
  the screenshot-HUD plan's `screenshot_paths`.  It cannot
  be folded into the range because `10e7efc` ("Merge
  branch 'develop' into screenshot") sits between, which
  is what inflates the second row of the table above.
- **`6d52665`'s own merge diff** is the wrong patch to
  hand any agent: PR #36 also carried the whole
  screenshot-and-latency-HUD plan.

### The diff is four months stale

Phases 1-5 landed 2026-04-20; this audit runs 2026-08-27.
The crate extraction has since moved most of the audited
code out of `ryll/`:

| In the diff | Today |
|---|---|
| `ryll/src/metrics.rs` | `shakenfist-spice-renderer/src/metrics.rs` |
| `ryll/src/channels/*.rs` | `shakenfist-spice-renderer/src/channels/*.rs` |
| `ryll/src/app.rs` | unchanged |
| `ryll/src/bugreport.rs` | unchanged |
| `shakenfist-spice-protocol/src/logging.rs` | unchanged |

The web frontend was later built on the same event path
this plan introduced, so the repaint-notify contract now
has consumers phases 1-5 never saw.

This is the phase's central design problem and decision 1
addresses it.

### One trap in the tooling

`audit-range.sh`'s content-scanning helpers
(`audit_range_show`) read each file **at `AUDIT_HEAD`** —
that is, at its April content — which is correct for
auditing a historical diff and misleading if read as a
statement about the tree today.  Wave 1's build, lint and
test steps, by contrast, run against the *current* tree.
So a single wave 1 run mixes April-content style findings
with current-tree test results.  Expect it; do not treat
the style findings as live defects without step 6e.

### Two findings already in hand

Surfaced while closing phase 2, so the audit starts from
them rather than rediscovering them:

1. **The latency statistic may be a burst artefact.**  The
   status bar read `Latency: 0.1ms` against a loopback
   guest.  The value is the interval between consecutive
   server PINGs, and phase 1 recorded sf-3 sending "a burst
   of 2 pings at connect time then going quiet" — so the
   number reflects whether the server happens to be
   bursting, not a property of the link.  See the master
   plan's success-criteria section.
2. **The master plan's *Bugs fixed during this work*
   section is still the placeholder** "(To be filled in as
   we go.)"  Either it is genuinely empty, in which case
   say so, or four phases of work fixed something nobody
   recorded.

### What was checked and found correct

Not everything was stale.  Phases 1, 3, 4 and 5 all
survive in current `develop`: the repaint bridge is at
`ryll/src/app.rs:1037` and `:1307` with the 1 Hz fallback
at `:4465`; `log_message` is `debug!` with no embedded
timestamp
(`shakenfist-spice-protocol/src/logging.rs:251`); the
PING latency sample is emitted from
`shakenfist-spice-renderer/src/channels/main_channel.rs:1032`;
and `runtime-metrics.json` reaches the bug-report ZIP via
`ryll/src/bugreport.rs:1296`, covered by
`test_bug_report_runtime_metrics_in_zip`.  `make test`
passes 787 tests and `pre-commit run --all-files` passes
all six hooks as of the phase 2 closeout.

## Decisions

1. **Audit the April diff, then triage every finding
   against current `develop` before acting.**  The
   alternative — auditing today's version of the code
   phases 1-5 introduced — would produce more immediately
   actionable findings, and it is the option a reviewer is
   most likely to argue for.  It is rejected because it
   answers a different question.  This phase exists to ask
   "what did this plan do to the codebase", and a plan
   whose code has since been refactored by someone else
   has not thereby been audited.  Auditing today's code
   would also silently re-audit the crate extraction's
   work, which had its own review.  The cost of the
   choice is a triage pass, which is step 6e, and it is
   cheap because the audit is only ~800 lines of code.

2. **`AUDIT_BASE=90a954b^1`, `AUDIT_HEAD=1c28d6f`, with
   `85bc901` handled as a separate patch.**  Rather than
   widening the range to swallow `85bc901` (43 files, most
   of them unrelated) or dropping it (it contains real
   phase 4 changes).  Step 6a builds a two-part patch file.

3. **Judgment agents get a patch file, not a revision
   range.**  `PUSH-AUDIT.md` requires this, and here it
   matters more than usual: a range would tempt an agent
   into `git show`ing the merge commit and auditing the
   screenshot-HUD plan by accident.

4. **The four wave 2 judgment agents run in parallel, one
   triage agent runs after them.**  They are independent
   by construction; the triage step is not, because it
   needs the full finding list to check against current
   `develop` in one pass.

5. **`PUSH-AUDIT.md` gets a correction as part of this
   phase.**  Its worked example says this plan's range is
   not derivable; the survey shows it is.  Leaving that
   uncorrected would mislead the next plan's audit in the
   direction of not trying.  This is a documentation fix
   to a repo-root runbook, so it is its own step with its
   own commit (6f), not folded into a findings commit.

6. **Findings land as a separate PR from this plan file.**
   Per the master plan.  This phase's PR is the plan plus
   the `PUSH-AUDIT.md` correction; the findings PR follows
   once there are findings to fix.

## Steps

| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
| 6a | low | haiku | none | Build the audit patch. Run `git diff 90a954b^1 1c28d6f > /tmp/plan-audit.patch`, then append the partly-in-scope review commit: `git show 85bc901 >> /tmp/plan-audit.patch`. Verify the result: the patch must contain 25 files from the first diff plus `ryll/src/app.rs` from the second, and must **not** contain `daa4626`-era screenshot work (grep the patch for `screenshot_paths` — it will appear only in the `85bc901` section, which is expected and is called out in step 6c's brief). Print the diffstat of each part. Do not interpret anything; this step only assembles. |
| 6b | low | sonnet | none | Run wave 1: `AUDIT_BASE=90a954b^1 AUDIT_HEAD=1c28d6f tools/audit/wave1.sh`. Exit codes are in `PUSH-AUDIT.md`. Two things to know before reading the output. First, wave 1's build/lint/test steps run against the **current tree**, not the audit range, so they are re-confirming what the phase 2 closeout already measured (787 tests pass, all six pre-commit hooks pass) — a failure here means something regressed on `develop` today, not something wrong with this plan. Second, the range-scoped style checks read file content **at `AUDIT_HEAD`**, i.e. April, so a long-line or unguarded-`log_message` hit may have been fixed since; report them, do not fix them, and mark each as needing the step 6e check. If wave 1 fails on codes 1-3, stop and report — do not proceed to wave 2. |
| 6c | low | sonnet | none | Run `AUDIT_BASE=90a954b^1 AUDIT_HEAD=1c28d6f tools/audit/wave2-mechanical.sh` and report its output verbatim, then add the style-conformance judgment review from `PUSH-AUDIT.md`'s "Style conformance — judgment portion" against `/tmp/plan-audit.patch`. Pay particular attention to the `repaint_notify.notify_one()` pairing requirement (`docs/design-decisions.md` decision #17): phase 2 added a `notify_one()` call after every `event_tx.send()` across seven channel handlers, and a missed pairing is exactly the defect that would make the UI silently stop updating. Check every `send_event`/`event_tx.send` site in the patch has one. Note that the `85bc901` section of the patch contains two hunks about `screenshot_paths` that belong to a different plan — skip those, they are not in scope. |
| 6d | medium-to-high | sonnet (2a/2b/2c), opus (2d) | none | The four wave 2 judgment agents from `PUSH-AUDIT.md`, run in parallel, each against `/tmp/plan-audit.patch` rather than a revision range: **2a code quality**, **2b test review**, **2c documentation review**, **2d security review** (opus, high effort). Use each brief in `PUSH-AUDIT.md` verbatim, with two additions. (i) The patch is from April; report what the patch shows and do not check it against the current tree — that is step 6e's job, and doing it here would have four agents redundantly repeating it. (ii) For 2d specifically, the highest-value target is `ryll/src/metrics.rs` (467 new lines), which parses `/proc/self/stat`, `/proc/self/status` and `/proc/self/task/*` — check the parsing for panics on malformed or truncated `/proc` content, and check the sampling sleep cannot be triggered on a UI thread. For 2c, note that the plan-file criteria naming `README.md` and `ARCHITECTURE.md` were already reconciled during the phase 2 closeout (the content moved to `docs/configuration.md` and `docs/diagnostics.md`); do not re-report that as a gap. |
| 6e | high | opus | none | Triage. Take every finding from 6b, 6c and 6d and classify each against **current `develop`** as `still-present`, `already-fixed`, or `moved` — using the file mapping in this plan's survey section (`ryll/src/metrics.rs` → `shakenfist-spice-renderer/src/metrics.rs`, `ryll/src/channels/*` → `shakenfist-spice-renderer/src/channels/*`; `app.rs`, `bugreport.rs` and `logging.rs` did not move). For each `still-present` finding give the current file and line. This is the step that decides what the findings PR actually contains, so be conservative: a finding you cannot locate in current `develop` is `already-fixed` only if you can point at what fixed it, otherwise it stays `still-present` and gets a human look. Add the two findings already in hand from the survey section (the latency burst-artefact question and the empty *Bugs fixed during this work* section) to the list before triaging. Output a table: finding, source agent, severity, status, current location. |
| 6f | low | sonnet | none | Correct `PUSH-AUDIT.md`'s "Two ways this runbook is invoked" section. It states that a master plan's accumulated diff "is not reliably derivable after the fact" and cites this plan, measured at 338 files, as the example. That is true of the naive range and false of the contiguous-commit range: the real answer here is 25 files, via `90a954b^1..1c28d6f`. Rewrite the passage to keep its warning — the naive range really is 340 files today — while adding that where a plan's phases landed contiguously on one branch, `<first-plan-commit>^1..<last-plan-commit>` gives an exact range, and that the `Merged` column should record the branch and bounding commits, not only a merge SHA. Keep the existing advice to record commits as phases land; that is still the point. Own commit, subject "Record that a contiguous phase range is derivable." |
| 6g | medium | opus | none | Management step, not a sub-agent step: review the 6e table, decide fix-or-decline for each finding, and record the outcome in the master plan under a new *Items deferred from the push audit* heading — matching the shape `PLAN-web-frontend.md` uses, minus the phase number. Every finding must be fixed or declined **in writing**. If the audit found nothing, that is recorded in one sentence and the phase is done. Fixes land as their own PR against `develop`; this step only decides and records. |

## Risks and mitigations

- **The audit reports "no findings" because it looked at
  nothing.**  This is the failure the phase exists to
  prevent, and the empty-range guard (exit 6) only catches
  the degenerate case.  *Mitigation:* step 6a prints the
  diffstat of both patch parts, and step 6b's brief states
  the expected shape (25 files + `app.rs`).  A reviewer
  checking this phase should look at those two numbers
  first; if they are not 25 and 1, the range broke.
- **Stale findings burn the findings PR's credibility.**
  Four months is long enough that some of an April diff's
  problems are already gone.  *Mitigation:* step 6e, and
  its instruction that "already-fixed" requires pointing
  at the fix.  The management session (6g) checks that
  every `already-fixed` claim carries one.
- **An agent audits the wrong plan.**  PR #36 carried two
  plans and the merge diff mixes them.  *Mitigation:*
  patch file rather than revision range (decision 3), and
  an explicit "skip the `screenshot_paths` hunks"
  instruction in 6c and 6d.
- **The `PUSH-AUDIT.md` correction over-corrects.**  The
  contiguous-range trick worked here by luck of how the
  branch was built; presenting it as the general method
  would send the next audit down a wrong path.  *Mitigation:*
  6f's brief says keep the warning and add the exception,
  and the management session reads the resulting wording
  rather than accepting it.

## Definition of done

Falsifiable items only.

- `git diff --stat 90a954b^1 1c28d6f | tail -1` reports
  25 files changed, and the assembled patch also contains
  `ryll/src/app.rs` from `85bc901`.
- `tools/audit/wave1.sh` has been run with the bounds
  above and its exit code is recorded in this file.
- `tools/audit/wave2-mechanical.sh` output is recorded in
  this file, verbatim.
- All four wave 2 judgment agents have reported, and each
  report is either summarised in this file or its findings
  appear in the 6e table.
- The 6e table exists, and every row has a status of
  `still-present`, `already-fixed` or `moved`; every
  `already-fixed` row names what fixed it.
- The master plan has an *Items deferred from the push
  audit* section in which every finding is marked fixed or
  declined, with a reason for each declination — or a
  single sentence recording that the audit found nothing.
- The master plan's *Bugs fixed during this work* section
  is no longer the placeholder text.
- `PUSH-AUDIT.md` no longer claims this plan's range is
  underivable.
- The master plan's phase 6 row reads `Complete`, and
  `docs/plans/index.md` shows the master plan as
  `Complete`.
- `pre-commit run --all-files` passes; `make test` passes.

## Back brief

Before executing any step, back brief the operator on the
understanding of this phase and how the intended work
aligns with it.

Two gates where the work is cheap to propose and expensive
to redo, so stop for agreement rather than proceeding:

- **After step 6a**, confirm the assembled patch is the
  right patch — the diffstat, and the decision to include
  `85bc901` in part rather than widen the range.  Every
  later step is wasted if this is wrong.
- **Before step 6g acts on the 6e table**, agree the
  fix-or-decline split.  Declining a finding in writing is
  a judgment the operator owns, not the audit's.

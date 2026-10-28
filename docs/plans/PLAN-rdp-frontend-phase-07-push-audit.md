# Phase 7: Push audit

Parent plan: [PLAN-rdp-frontend.md](PLAN-rdp-frontend.md)

## Goal

Run `PUSH-AUDIT.md` over the whole of the RDP frontend work, the
eight pull requests that landed phases 1 to 6, and resolve or
decline every finding in writing. This is the plan's closing
phase.

## Planning effort

**Medium.** The runbook is written, and three earlier closing
audits in this repository set the shape
(`PLAN-idle-cpu-and-latency` phase 6, `PLAN-stream-caps-and-flap`
phase 18 and `PLAN-decoder-hardening` phase 2). What is new here
is size: about 18 000 inserted lines across eight merges, most of
it a network-facing server that did not exist a fortnight ago. The
judgment is in how the patch is split for the reviewers and in the
security briefs. The steps carry their own effort levels.

## Scope

In:

- Wave 1 and wave 2 of `PUSH-AUDIT.md` over the eight phase
  merges recorded in the master plan's Execution table.
- The `plan-push-audit-phase` floor checklist, item by item, since
  this plan added a scheduled workflow, a published crate and a
  listener.
- Triage of every finding against current `develop`.
- A fix-or-decline decision for each finding, recorded in the
  master plan.

Out:

- Fixing the findings. Under the `plan-push-audit-phase` shared
  block they land as their own pull request against `develop`;
  this phase decides and records.
- The six issues phase 6 filed: #642 (SIGTERM), #643 (positional
  `.vv`), #644 (help text and comments that overclaim), #645
  (dropped-input counts), #646 (stale parity cells) and #648
  (`--rdp-password-file`). A finding that one of these already
  covers is recorded as tracked by it, not raised again.
- The master plan's *Future work*, which holds the extensions this
  plan chose not to build (EGFX, RD Gateway, multiple monitors and
  the rest). An audit reports defects in what was built.
- `docs/plans/` and `Cargo.lock`. The plan files are reviewed by
  the daily plan-format audit. The lockfile is covered by
  `cargo deny` and `cargo audit` in CI, and the agents review the
  dependency declarations in the `Cargo.toml` files instead.

## What the survey found

### Phase 6 is complete

| Check | Result |
|-------|--------|
| Master plan and `index.md` agree | Both `In progress`; phase 6 is set `Complete` in this branch's close-out commit |
| On `develop` | PR #647 merged 2026-10-27 as `7a5895c`; `git merge-base --is-ancestor 7a5895c origin/develop` succeeds |
| Definition of done | Spot-checked against `develop` at `e49ddce`: both new pages exist; `ARCHITECTURE.md` says 7 crates and names `shakenfist-spice-rdp`; all eight `--rdp*` flags in `ryll/src/config.rs` are in `docs/configuration.md`; "half brightness" and "100 MiB" appear in `docs/rdp-frontend.md` and not in `docs/configuration.md` |
| Leftover branch or worktree | None, local or remote |
| Deferred work | #642 to #646 and #648 are open, and listed in the phase 6 plan's *Results* |

### The audit range is exact

Every phase landed as a pull request, and every cell of the
`Merged` column is a two-parent merge commit:

| Phase | Merge | PR | Diff against first parent |
|-------|-------|----|---------------------------|
| 1. Renderer prep | `4696a95` | #595 | 52 files, +3 834 −696 |
| 2. RDP skeleton | `7b389b1` | #605 | 29 files, +7 423 −127 |
| 2. follow-up | `c314094` | #607 | 5 files, +490 −41 |
| 3. Cursor and resize | `c9a763d` | #612 | 18 files, +2 959 −170 |
| 4. Clipboard and audio | `fe563e1` | #632 | 18 files, +3 583 −64 |
| 5. Lifecycle and CI | `13044d7` | #640 | 28 files, +5 388 −609 |
| 5. review follow-up | `280c047` | #641 | 4 files, +79 −20 |
| 6. Docs and parity | `7a5895c` | #647 | 15 files, +1 949 −211 |

Those sum to 94 distinct files. Of their 25 705 inserted lines,
5 643 are the plan files themselves and 1 405 are `Cargo.lock`.
Both are out of scope (see *Scope*).

Three commits inside the merges are not this plan's work:
`d4345fd` (a tokio bump, in #605), `939dd1a` (a ctutils and
clipboard-win advisory bump, in #612), and each phase's close-out
of the phase before it. The two bumps touch only `Cargo.lock`, and
the close-outs touch only `docs/plans/`, so the scope exclusions
already remove them and nothing else needs filtering.

`#590`, which merged the master plan itself, adds no code and is
not in the range.

### The patch cannot be one net diff

Unrelated work landed on `develop` between the phases. Sixteen of
the audited files were also changed by other pull requests inside
the range, among them `cursor.rs` and `display.rs` (the decoder
hardening findings, #633), `ryll/src/config.rs` and the `Makefile`
(the Kasm workspace, #609), and `docs/ci.md` (two template
resyncs). A net diff from `4696a95^1` to `7a5895c` would hand
their changes to the reviewers as this plan's.

The other 69 files were touched only by this plan's merges, so
for those the net diff is exact, and it is far easier to read than
eight overlapping per-phase hunks of the same new file. So the
patch is built in two parts. The plan's own files go in as one
net diff. The 16 shared files go in as the concatenation of each
phase's diff against its first parent. The script in *Appendix:
assembling the patches* does both and splits the result by area.
Run against `e49ddce`, it produces:

| Patch | Files | Inserted | Deleted |
|-------|-------|----------|---------|
| `rdp` | 13 | 10 936 | 0 |
| `client` | 42 | 3 542 | 793 |
| `tooling` | 11 | 1 777 | 12 |
| `docs` | 19 | 1 714 | 277 |
| **whole** | **85** | **17 969** | **1 082** |

Every in-scope file falls in exactly one area. The areas are:

- `rdp`: the `shakenfist-spice-rdp` crate, plus `ryll/src/rdp.rs`
  and `ryll/src/reconnect_policy.rs`.
- `client`: the renderer, compression, protocol and webrtc crates,
  the rest of `ryll/`, and the workspace `Cargo.toml`.
- `tooling`: `tools/`, `.github/`, the `Makefile`,
  `.pre-commit-config.yaml` and `.gitleaksignore`.
- `docs`: `docs/` and every `*.md`, the crate READMEs included.

### The audited code has barely drifted

Since `7a5895c` only `.pre-commit-config.yaml` (a shellcheck-py
bump) and `docs/ci.md` (a template resync) have changed among the
audited files. Before it, the drift is the 16 shared files above.
The RDP crate, `ryll/src/rdp.rs` and the new renderer modules
(`pointer.rs`, `display/damage.rs`, `event_bus.rs`,
`playback_sink.rs`) are unchanged since their phases landed. So a
finding in the `rdp` area is live until shown otherwise, and only
findings in the shared files need locating line by line. That
makes triage much cheaper than it was for decoder hardening.

### The renderer has outside consumers

kerbside-proxy depends on `shakenfist-spice-renderer`,
`-protocol` and `-compression` at `b33dea4`, a recent `develop`.
So it already builds against phase 1's changes: the slim build,
damage tracking, the shared event bus and pointer translation.
andris pins `-protocol` and `-compression` at `38c51ef`. Floor
item 4 (nothing removed or renamed still has a consumer) has to
be answered by grepping `../kerbside/rust` and `../andris`, not
by assuming.

### One finding already in hand

`tools/publish-crates.sh` has listed `shakenfist-spice-rdp` since
`c515f09` (phase 2), and the crate's `Cargo.toml` has no
`publish = false`. No release has been tagged since: `v0.3.0`
(2026-10-17) predates the crate, and crates.io has no
`shakenfist-spice-rdp`. So the next tagged release will publish
it for the first time, together with a `ryll` crate whose `rdp`
feature depends on it.

The master plan's success criterion says the crate "is not on
crates.io". That is true today, but no phase decided to publish
it. Floor item 5 asks whether a broken change here fails closed.
`make package-check` runs on every pull request; whether it
packages this crate is for step 3 to establish. This goes to
triage like any other finding; it is not decided here.

### The master plan's phase 7 section is accurate

It says to run `PUSH-AUDIT.md` over the merges in the `Merged`
column. Phase 6's cell was the only one empty, and this branch's
close-out fills it. Nothing in the section needed correcting.

## Decisions

1. **The range is the eight merges in the Execution table, and
   nothing else.** They are exact. The two dependency bumps inside
   them touch only the out-of-scope `Cargo.lock`, so no hand
   filtering is needed.

2. **The plan's own files are read as one net diff; shared files
   as per-phase diffs.** A net diff of a file only this plan
   touched is the plan's work and nothing else, and it reads as
   the code that shipped rather than as the seven revisions that
   produced it. For the 16 shared files the net diff would carry
   other plans' work, so the per-phase concatenation is the only
   correct patch. The appendix script does both, and its
   coverage check is step 1's gate.

3. **Six judgment agents, split by area.** This follows the
   stream-caps audit's reasoning: no agent should be handed
   18 000 lines.
   - Code quality and security each get two agents. One reads
     `rdp`, the attacker-facing server. The other reads `client`
     plus `tooling`, the renderer changes, the ryll glue and the
     CI lane.
   - Tests and documentation stay whole, because both are
     cross-cutting.

4. **The RDP security review is the one that matters, and gets
   opus at high effort.** `shakenfist-spice-rdp` accepts TCP
   connections from RDP clients, negotiates TLS and CredSSP, and
   parses client input, clipboard and display-control PDUs. Its
   reviewer must check the claims against the pinned IronRDP
   source in `.cargo-cache/registry/src/`, as the master plan's
   *Agent guidance* requires of any brief that relies on IronRDP
   behaviour. Without that, the review would be checking the
   code against its own comments.

5. **Findings land in a separate pull request, after this one.**
   That is what the shared block says, and it is how the last
   three closing audits here ran. This branch carries the
   close-out, the plan and the execution record. The findings PR
   carries the fixes, records this phase's merge commit and sets
   the plan `Complete`. If nothing needs a code change, this
   branch sets the plan `Complete` itself.

   This is the decision most open to argument. The answer
   differs from decoder hardening, where the diff was 22 files
   and folding the fixes in was a reasonable ask. Here the diff
   is 85 files and the findings will likely span several areas,
   so the case for keeping review of the judgements apart from
   review of the fixes is stronger. The management session does
   not offer a fold-in at step 6.

6. **The publish question is triaged, not settled by the
   audit.** Whether `shakenfist-spice-rdp` should go to crates.io
   is a product decision. The audit's job is to make sure it is
   made deliberately before the next release does it by default.
   Step 6 raises it with the operator, in the same question as the
   other decisions only the operator can make.

## Steps

The runbook's sub-agent briefs are used as written, with the
additions below. `$S` is a directory in the scratchpad of the
session running the phase; write its absolute path into each
brief. Steps 1 to 5 produce findings recorded in this file, not
code.

| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
| 1 | low | sonnet | none | Assemble the patches and run wave 1. Copy the appendix script to `$S/assemble.sh`, then run `bash $S/assemble.sh $S/patches` from the worktree root. Gate: it must print empty `uncovered:` and `duplicated:` lists and a `whole` line of **17969 +, 1082 -**, with the per-area figures in this plan's survey table. If any figure differs, stop and report, because every later step reads these patches. Then run `AUDIT_BASE=4696a95^1 AUDIT_HEAD=7a5895c timeout 3600 tools/audit/wave1.sh` from the worktree root, as a background task. Before reading its output, know that the build, lint and test stages run against the **current tree**: a failure there is a `develop` regression, not a defect in this plan, so say which it is. The range-scoped style checks read files at `7a5895c`, over a range that also holds the foreign pull requests the survey names (#609, #633, #621, #631 and others), so check each hit against `$S/patches/whole.patch` and drop the ones that are not this plan's. On exit 1-3, stop and report. Report the exit code and every non-pass line. |
| 2 | low | sonnet | none | Run `AUDIT_BASE=4696a95^1 AUDIT_HEAD=7a5895c tools/audit/wave2-mechanical.sh` and report its output, filtered to `$S/patches/whole.patch` (same superset caveat as step 1; say how many hits were dropped as foreign). Then do the runbook's *Style conformance — judgment portion* against `$S/patches/whole.patch`. Look in particular at the following. The `rdp:` prefix, or whatever prefix the crate settled on, applied consistently to every new log line in `shakenfist-spice-rdp` and `ryll/src/rdp.rs`. Every `warn!` a connected RDP client or the SPICE server can trigger repeatedly: the convention is now `warn_once!` (see `STYLEGUIDE.md`, *Log levels*). Any password, token or TLS key material reaching a log line at any level. Report a short list, or "Style checks passed." |
| 3 | medium-to-high | sonnet (2a-1, 2a-2, 2b, 2c), opus (2d-1, 2d-2) | none | Six judgment agents from `PUSH-AUDIT.md`, run in parallel, each given patch paths and the step 2 report. Tell each that the patches are as of 2026-10-27, to report what the patches show, and **not** to check current `develop`, which is step 4's job. Each also gets the list of phase 6's open issues (#642-#646, #648) so it can mark a finding as already tracked. **2a-1** (code quality, `rdp.patch`): `server.rs`, `display.rs`, `audio.rs` and `input.rs` are each over 1 000 lines; look for logic the IronRDP handler traits made the crate repeat (per-channel task setup, shutdown, back-pressure), and for comments longer than the code they explain (`comment-proportion` block). **2a-2** (code quality, `client.patch` + `tooling.patch`): phase 1 moved the event bus, mirror tasks and pointer translation out of `ryll` into the renderer for sharing; check that no copy was left behind in `ryll/src/web/` or `ryll/src/app/`, and that `reconnect_policy.rs` is used by every caller it claims. Floor item 4: for every public item phase 1 renamed or removed in the renderer, grep `../kerbside/rust` and `../andris` and state the greps. **2b** (test review, `whole.patch`): `tests/loopback.rs` is the crate's integration test, and `tools/rdp-smoke.py` the CI lane. Map each user-visible behaviour in the master plan's success criteria to a test, and name any that rest only on a manual check recorded in a phase's *Results*. Say which RDP behaviours cannot be tested without a real client, as distinct from untested. **2c** (documentation, `docs.patch`, with `whole.patch` for context): check `docs/rdp-frontend.md`, `docs/rdp-mode-internals.md`, the crate README and `ARCHITECTURE.md` against the code, and the master plan's *Success criteria* status lines against the tree. `AGENTS.md` forbids quoting dependency versions in `docs/`. Do not report what #644 or #646 already track. **2d-1** (security, opus, high effort, `rdp.patch`): the listener, from the side of an unauthenticated peer and of an authenticated but hostile client. Check the pre-authentication slot and connection limits, TLS and CredSSP setup in `tls.rs` and `auth.rs`, and password handling end to end (where it is read, held, compared, logged). Check the bounds on every client-supplied size: clipboard text, display-control layouts, input PDUs and audio formats. Check takeover, whether one client can starve or eject another. Re-check every IronRDP behaviour the code relies on against the pinned source under `.cargo-cache/registry/src/` (run `make fetch` first), naming the version read. **2d-2** (security, opus, high effort, `client.patch` + `tooling.patch`): the renderer changes from the SPICE server's side. Damage rectangle accumulation (can a server make it grow without bound?), the cursor decoder's new formats, and the device-free playback sink's buffering. Then the CI lane: `rdp-smoke.yml` runs on `[self-hosted, vm, debian-13-docker, l]`. Say what a pull request can make it execute, and with what permissions and secrets. Floor items 2 and 5: does the nightly run report its own failure somewhere a human sees, and does the release path fail closed for the new crate (see the survey's in-hand finding; establish whether `make package-check` packages `shakenfist-spice-rdp`). Severity per the runbook. |
| 4 | high | opus | none | Triage. Take every finding from steps 1 to 3, plus the in-hand publish finding, and classify each against **current `develop`** as `still-present`, `already-fixed`, `moved` or `tracked` (by one of #642-#646 or #648). Per the survey, findings in the `rdp` area and the new renderer modules are live until shown otherwise. Findings in the 16 shared files must be located line by line. A finding you cannot locate is `already-fixed` only if you can name the commit that fixed it. Mark each finding that predates this plan, since those become issues rather than findings. Output one table: ID, finding, source agent, severity, status, predates plan, current file and line. |
| 5 | low | sonnet | none | The floor checklist. For each of the five items in the `plan-push-audit-phase` shared block (`PLAN-TEMPLATE.md`, *Execution*), give a verdict over this range with the evidence: the eight merges and their parent counts (item 1); every scheduled or unattended thing the range added, which includes `rdp-smoke.yml`'s cron, and where its failure is reported (item 2); a gitleaks-clean range and no private hostname or address in the patches (item 3, citing `.gitleaksignore`'s new entries and why each is a test fixture); the greps from 2a-2 (item 4); the publish path from 2d-2 (item 5). Where an agent already answered an item, cite it rather than redoing it. |
| 6 | medium | opus | none | Management step, not a sub-agent step. Decide fix-or-decline for each `still-present` finding, and record the outcome in the master plan under a new *Items deferred from the push audit* heading, in the shape `PLAN-decoder-hardening.md` uses. Every declination gives a reason. File issues for findings that predate the plan. Ask the operator, in one `AskUserQuestion` call, the questions only they can answer, including whether `shakenfist-spice-rdp` is published (decision 6). Record the raw output, the step 4 table and the step 5 verdicts in this file under *Execution record*, summarised where long. Update the master plan's *Bugs fixed during this work* if the audit adds any. If nothing needs a code change, say so in one sentence and set the plan `Complete` in the Execution table and `index.md` on this branch. Otherwise leave it `In progress` and open the findings PR after this one merges. Commit subject: "Record the RDP frontend push audit." |

## Risks and mitigations

- **A clean audit for the wrong reason.** A wrong range or a
  dropped area reads as no findings. Step 1 gates on the exact
  totals and on the coverage check, and `wave1.sh` exits 6 on an
  empty explicit range. The management session checks that the
  step 1 report states the 17 969-line figure before starting
  step 2.
- **Foreign hits from the scripts' superset range.** The scripts
  take `4696a95^1..7a5895c`, which also holds a dozen unrelated
  pull requests. Steps 1 and 2 filter each hit against
  `whole.patch`, and say how many they dropped, so a dropped hit is
  visible rather than silent.
- **The security reviewer trusts the code's account of IronRDP.**
  Much of the crate's safety rests on what IronRDP does: when it
  drops a handler future, what it bounds, what the acceptor
  refuses. 2d-1 is told to read the pinned source and name the
  version. The management session spot-checks one IronRDP claim
  in the 2d-1 report against the source before accepting it.
- **Wave 1 fails for reasons that have nothing to do with this
  plan.** The build and tests run on today's tree. Step 1 has to
  say which kind of failure it is. A `develop` regression is
  handled outside this phase, and it does not block wave 2 if the
  range-scoped checks ran.
- **Re-reporting what phase 6 already filed.** Six issues are
  open from the last phase. Every agent gets the list, and triage
  has a `tracked` status for it, so they are not counted twice.

## Definition of done

- `bash $S/assemble.sh $S/patches` prints empty `uncovered:` and
  `duplicated:` lists and `whole` totals of 17 969 inserted and
  1 082 deleted lines. This file records that the audit ran over
  those patches.
- This file's execution record holds the wave 1 exit code, the
  wave 2 mechanical output (or a summary of it), the step 4
  triage table and the step 5 floor verdicts.
- Every row of the step 4 table marked `still-present` appears
  under *Items deferred from the push audit* in the master plan,
  as fixed, to be fixed in the findings PR, or declined with a
  reason. Checkable by counting: the two numbers are equal.
- The master plan's Execution table reads phase 6 `Complete` with
  `Merged` `7a5895c`, and phase 7 linked to this file.
- The publish question has an answer from the operator, recorded
  in the master plan.
- If the audit found nothing needing code, the master plan and
  `index.md` both read `Complete`, and phase 7's `Merged` cell is
  empty. Otherwise both read `In progress`.
- `pre-commit run --all-files` passes on each commit.

## Back brief

Before executing any step of this plan, back brief the operator on
the range and how it is split, the six judgment agents and their
models, and how findings will be landed. Confirm whether this
session may push this branch and open its pull request once the
definition of done passes.

## Appendix: assembling the patches

Run from the root of a ryll checkout whose history holds the eight
merges; the argument is the output directory.

```bash
#!/bin/bash
set -euo pipefail
out=$1
mkdir -p "$out"
merges="4696a95 7b389b1 c314094 c9a763d fe563e1 13044d7 280c047 7a5895c"
base=4696a95^1
head=7a5895c

# Files any phase touched, less the plan files and the lockfile.
for m in $merges; do git diff --name-only "$m^1" "$m"; done | sort -u |
    grep -v -e '^docs/plans/' -e '^Cargo\.lock$' > "$out/files.txt"

# Files a commit outside the plan also touched between base and head.
# Their net diff would carry that commit, so they are read as the
# concatenation of each phase's own diff instead.
: > "$out/shared.txt"
for c in $(git rev-list --first-parent "$base..$head"); do
    short=$(git rev-parse --short=7 "$c")
    case " $merges " in *" $short "*) continue ;; esac
    git diff --name-only "$c^1" "$c" -- $(cat "$out/files.txt") >> "$out/shared.txt"
done
sort -u -o "$out/shared.txt" "$out/shared.txt"
comm -23 "$out/files.txt" "$out/shared.txt" > "$out/own.txt"

# area <name> <pattern...>: the plan's own files matching a pattern
# and not claimed by an earlier area, as one net diff, then each
# phase's diff over the area's shared files.
: > "$out/covered.txt"
pick() {
    local list=$1; shift
    grep -Fvxf "$out/covered.txt" "$list" | while read -r f; do
        for p in "$@"; do case "$f" in $p) echo "$f"; break ;; esac; done
    done
}
area() {
    local name=$1; shift
    local own shared
    own=$(pick "$out/own.txt" "$@")
    shared=$(pick "$out/shared.txt" "$@")
    {
        if [ -n "$own" ]; then git diff "$base" "$head" -- $own; fi
        if [ -n "$shared" ]; then
            for m in $merges; do git diff "$m^1" "$m" -- $shared; done
        fi
    } > "$out/$name.patch"
    printf '%s\n' $own $shared > "$out/$name.files"
    cat "$out/$name.files" >> "$out/covered.txt"
}

# Docs first, so the crate READMEs go to the documentation reviewer.
area docs 'docs/*' '*.md'
area rdp 'shakenfist-spice-rdp/*' 'ryll/src/rdp.rs' 'ryll/src/reconnect_policy.rs'
area client 'shakenfist-spice-renderer/*' 'shakenfist-spice-compression/*' \
    'shakenfist-spice-protocol/*' 'shakenfist-spice-webrtc/*' 'Cargo.toml' 'ryll/*'
area tooling 'tools/*' '.github/*' 'Makefile' '.pre-commit-config.yaml' '.gitleaksignore'

# Every file lands in exactly one area.
sort -o "$out/covered.txt" "$out/covered.txt"
echo "uncovered:"; comm -23 "$out/files.txt" "$out/covered.txt"
echo "duplicated:"; uniq -d "$out/covered.txt"
cat "$out"/{rdp,client,tooling,docs}.patch > "$out/whole.patch"
cp "$out/covered.txt" "$out/whole.files"
for p in rdp client tooling docs whole; do
    printf '%-8s %3d files ' "$p" "$(wc -l < "$out/$p.files")"
    awk '/^\+/ && !/^\+\+\+/ {a++} /^-/ && !/^---/ {d++} END {printf "%6d +, %5d -\n", a, d}' "$out/$p.patch"
done
```

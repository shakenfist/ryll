# PR NN review state

This file is local-only — never staged, never committed,
never pushed. It captures the state of a single external
PR review so that returning to the worktree later does not
require reconstructing context from scratch.

Copy this template into the review's worktree as
`REVIEW-STATE.md` at the start of a review (after Step 0
of `MERGE-TEMPLATE.md`), then keep it updated as you work.
A state file that does not get updated becomes misleading
archaeology — worse than no file at all.

## How to resume

When picking this work back up after time away (operator
prompt to assistant: "we're back in `<worktree>`, read
`REVIEW-STATE.md` first then check what's happened
since"), do these in order:

1. Read this file end-to-end.
2. `git fetch origin` and `git fetch <contributor-remote>`
   to refresh refs.
3. `git log origin/develop..<contributor-remote>/<branch>
   --oneline` to see if the contributor pushed anything
   since our last action.
4. `gh pr view NN --comments` to read any new comments
   from the contributor or other reviewers.
5. `gh pr checks NN` to see CI status. If CI failed,
   investigate against the blocking findings listed
   below.
6. Cross-check that develop has not moved in a way that
   invalidates an earlier rebase or our findings: `git log
   <our-last-action-sha>..origin/develop -- <files we
   touched or care about>`. Anything substantive there
   may mean re-rebasing or reassessing.
7. Decide next action based on what you found:
   - **Contributor engaged and pushed fixes**: re-run
     a Wave 2 review pass on the new commits, look for
     regressions, decide whether to merge.
   - **Contributor commented but not pushed**: respond
     to whatever they said.
   - **Silence + CI passing**: judgment call on whether
     to wait longer or fall back to plan B (see below).
   - **CI failing**: figure out why and decide whether
     to fix on the contributor's branch (preserves
     authorship) or wait for them.
8. Update this file with anything you've learned.

## PR

- **Repo**: <owner>/<repo>
- **Number**: NN
- **Title**: <PR title>
- **Contributor**: <github-handle> (<full name if known>)
- **Head branch**: <contributor-remote>/<branch> (note
  whether `maintainerCanModify` is true — affects whether
  we can push to their branch)
- **Last contributor commit before our actions**: <sha>
- **Our latest action commit on contributor branch**:
  <sha or "none">
- **Local branch tracking our work**: <branch>

## Teachy mode

Set per Step 0 of `MERGE-TEMPLATE.md`. One of:

- **None** — land-and-followup. No PR comments to the
  contributor. All non-blocking findings become our own
  follow-up plan.
- **Light** — post findings as a single PR comment, give
  the contributor a chance. Vague urgency tends to work
  better than hard deadlines.
- **Full** — iterate as long as the contributor engages.

Record the choice and the reason for it. If history with
this contributor changes the choice, note that too.

## What was done

A bullet list of actions we have taken on this review,
in chronological order. Examples:

- Followed `MERGE-TEMPLATE.md` Wave 0 → Wave 1 → Wave 2.
- Rebased onto develop using "develop wins unless PR is
  genuinely better" strategy. Rationale in merge commit
  <sha>.
- Force-pushed rebase to <contributor-remote>/<branch>.
- Posted review comment: <link to PR comment>.
- Other actions as taken.

## What is left

### PR's unique value still pending

After any rebase / our actions, what does this PR still
uniquely contribute that develop does not have? List in
priority order. This is the "is it worth landing?"
calculus made explicit.

- <item> (severity / value)
- <item>
- ...

### Findings posted (or to post)

Findings from Wave 1 and Wave 2 reviews, classified per
Step 5 of `MERGE-TEMPLATE.md`. Summarise here so future-
you can scan without re-reading the PR comment.

**Blocking** (would ship a regression or fail CI):

1. <finding>
2. <finding>

**Should-have** (we'd like to see, happy to discuss):

1. <finding>
2. <finding>

**Nice-to-have** (small things, please if convenient):

1. <finding>
2. <finding>

**Informational** (not requesting changes, just noting):

- <finding>
- <finding>

## Branches in this worktree

- **<branch>** (currently checked out) — <description>
- **<branch>** — <description>

## Other worktrees relevant to this work

If other worktrees were created in service of this review
(e.g. for parallel work on follow-up plans, or for related
PRs), list them here so future-you knows where to look.

- **<worktree>** — <purpose, current state>

## When CI returns

Document the expected outcomes and what to do for each:

- **CI passes**: <action>
- **CI fails**: <expected failure modes and how to debug>
  - The two blocking findings most likely to cause
    failures are <finding 1> and <finding 2>.

## Plan B (if the contributor doesn't engage)

When teachy mode is light or full, plan B is the fallback
if the contributor stops responding. Spell it out
explicitly so future-you can execute without rederiving:

1. Apply the N blocking fixes ourselves on a new branch
   off <base>.
2. <Push location and method>.
3. Write follow-up plan in
   `docs/plans/PLAN-prNN-followup.md` tracking
   should-have, nice-to-have, and informational items.
4. Land the PR with our pre-merge fixes (note any
   Co-Authored-By attribution that should appear).
5. Open follow-up PR using
   `docs/plans/PLAN-prNN-followup.md` as the work list.

If you previously drafted the pre-merge fixes on a branch
and then dropped that branch, note that here so future-you
knows the work needs re-deriving (and where the
instructions live — usually the posted PR comment).

## Don't forget

A grab-bag of things that are easy to forget when
returning to this work:

- New CI gates that landed on develop while we were
  waiting (cargo audit, cargo deny, etc.) and that this
  PR has to pass.
- Cross-repo implications (e.g. kerbside docs that
  reference protocol behaviour this PR changes).
- Any previously-recorded follow-up items in
  `PLAN-supply-chain-followups.md` or similar that this
  PR interacts with.
- Anything else specific to this PR that does not fit
  cleanly elsewhere.

Thanks for your work on this. I appreciate it. Some final
checks before I push.

## How to use this template

The pre-push audit splits into two waves:

**Wave 1 — mechanical.** Build verification, lint,
test suite, and the parts of style conformance that grep
can answer.  Wrapped in a single shell script so it runs
as one tool approval.  Always run wave 1 first; wave 2 is
only worth spending on if wave 1 passes.

**Wave 2 — judgment.** Code-quality, test-coverage,
documentation, and security review.  Some of this is
mechanical (TODO/FIXME/dead-code grep, unsafe block list)
and is wrapped in a second script; the rest needs sub-
agents to read code and apply judgment.  The four
judgment agents are independent and can be spawned in
parallel.

The management session reviews all findings, fixes any
issues, and confirms the push.

## Two ways this runbook is invoked

**As a pre-push gate.** The classic use: an unpushed
branch, about to become a PR. The range audited is
`develop...HEAD`, which is what the briefs below and both
scripts use by default, and findings are fixed on the
branch before the push.

**As a master plan's closing phase.** Every master plan
ends with a push-audit phase. That plan's phases each
landed as their own PR, so by the time the audit runs,
`develop...HEAD` on the audit branch holds nothing but the
audit's own edits. Auditing that reports "no findings" for
the wrong reason, which is the failure the phase exists to
prevent.

What is wanted is the plan's accumulated diff, and it is
not reliably derivable after the fact. Unrelated work
lands on `develop` between phases, so "everything since
the plan file appeared" is far too wide — measured on
`PLAN-idle-cpu-and-latency`, 338 files against the five
phases actually in it — and the phase branches are gone by
then. It has to be *recorded*: as each phase of a master
plan lands, put what landed it in the `Merged` column of
the plan's *Phase order* table. Recording it is what makes
this phase runnable at all. It does not go in the `Status`
column, which the `plan-status-vocabulary` shared block
reserves for a single term.

In this repository that is one SHA per phase, because every
phase lands as its own pull request and a merge commit's
diff against its first parent is the whole of it. That is a
property of how ryll merges, not a general rule: the
`plan-push-audit-phase` shared block asks for every commit
of the phase, or its `first..last` range, where a phase
lands directly on the default branch instead. A single
commit is only ever enough when it is a merge commit.

Given those commits, build the combined patch and hand the
judgment agents its path, rather than a revision range:

```
for m in <the phase merge commits>; do
    git diff "$m^1" "$m"
done > /tmp/plan-audit.patch
```

The two scripts want a range rather than a patch, so give
them the outer bounds:

```
AUDIT_BASE=<first phase merge>^1
AUDIT_HEAD=<last phase merge>
export AUDIT_BASE AUDIT_HEAD
```

Anything unrelated that landed between those two points
falls inside that range, so read the scripts' output as a
superset and check each hit against the patch before
acting on it. Where a plan's phases all landed on one
branch, the range is exact and no filtering is needed.

If the *Phase order* table records no commits — true of
every plan written before this convention — reconstruct
what you can from `gh pr list --state merged`,
`git rev-list --first-parent` and the phase plan filenames,
say in the findings how much of the plan you were actually
able to see, and write the commits into the table so the
next audit does not start from nothing. Do not reconstruct
from a path-filtered `git log` alone: it lists the commits
that touched a path without saying which arrived directly
and which arrived inside a pull request, so it will hand
you a commit that came in under a merge and the audit will
then cover one commit of that pull request rather than the
pull request. It will also hand you commits that merely
touched the same paths as some other plan's work, which is
the wrong question — the range wanted is what this plan
did, not what else was going on.

`tools/audit/wave1.sh` and `tools/audit/wave2-mechanical.sh`
both honour `AUDIT_BASE` and `AUDIT_HEAD`, sharing the
resolution logic in `tools/audit/audit-range.sh`. In the
briefs below, `git diff develop...HEAD` means "the audit
range".

Both scripts fail fast, with exit 6, when an *explicitly
set* `AUDIT_BASE` or `AUDIT_HEAD` does not resolve, or when
the range you asked for has an empty diff. An empty range
is reported at the top of the run and again in the closing
summary, because by then the first one has scrolled away.

The two differ on the range left to default. `wave1.sh`
exits 6 when `develop...HEAD` is empty: build, lint and
tests passing says nothing about a diff nobody looked at,
and a green summary after a ten-minute Docker cycle reads
as though it did. `wave2-mechanical.sh` prints the same
warning and still exits 0 — it reports findings as text
rather than as a status, and that is its documented
contract. Neither fails on a range that will not resolve
at all: a shallow clone with no `develop` stays a NOTE.

Content-scanning checks read each file at `AUDIT_HEAD`
rather than out of the working tree, so uncommitted edits
are not audited — commit first.
`tools/audit/test-audit-range.sh` pins all of that against
a scratch repository, and runs in CI beside shellcheck.

Three items in the management checklist at the end read
differently here. Findings become their own PR against
`develop` rather than fixes on an unpushed branch; the
commit-history and up-to-date items apply to that
follow-up PR; and "ready to push" becomes "every finding
is fixed, or declined in writing in the master plan".

## Wave 1: Mechanical checks

Run the consolidated script (one approval):

```
tools/audit/wave1.sh
```

It performs (and exits non-zero on any failure):

- `pre-commit run --all-files`
- `./scripts/check-rust.sh check` (rustfmt + clippy via
  Docker)
- `cargo test --workspace` via Docker
- mechanical style checks: no raw `println!`/`eprintln!`
  in non-test source, advisory long-line check on Rust
  files in the audit range, advisory check for
  unguarded `logging::log_message` calls.

Exit codes:

| Code | Meaning                                     |
|------|---------------------------------------------|
| 0    | all wave 1 checks passed                    |
| 1    | pre-commit failed                           |
| 2    | rustfmt or clippy failed                    |
| 3    | cargo test failed                           |
| 4    | raw `println!`/`eprintln!` found            |
| 5    | could not `cd` to the repository root       |
| 6    | audit range unusable, or empty by default   |
| 7    | a style check could not find what it scans  |

Code 7 is deliberately not 4.  Four means the code under
audit is wrong; seven means the audit is — the workspace
members would not parse, or the channels directory has
moved again.  A check that cannot find its subject
otherwise reports success, which is how wave 1's only
fatal style check came to miss 46% of the workspace for
months without anyone noticing.

`wave2-mechanical.sh` shares codes 5 and 6 and otherwise
exits 0; it reports findings as text rather than as a
status.

If wave 1 fails, fix the cause and re-run before
spending on wave 2.

### Style conformance — judgment portion

The script covers what grep can prove.  The remaining
style questions need a sub-agent to read code:

| Setting | Value |
|---------|-------|
| Model | sonnet |
| Effort | low |

**Brief for sub-agent (only if wave 1 passes):**

Check `git diff develop...HEAD` for adherence to project
conventions in `AGENTS.md` and `docs/design-decisions.md`:

- Channel handler conventions: message loop structure,
  ACK handling, channel name prefix on log messages
  (e.g. `"display: ..."`, `"playback: ..."`), and the
  `repaint_notify.notify_one()` pairing requirement
  documented in `docs/design-decisions.md` decision #17.
- Protocol message conventions: constants in
  `shakenfist-spice-protocol/src/constants.rs`, message
  parsing in `messages.rs`, name lookups in `logging.rs`.
- Image decompression conventions: header parsing,
  BGRX-to-RGBA conversion, `DecompressedImage` return
  type.
- Field rename / unit-change discipline: did any field
  silently change units (e.g. seconds → ms) without a
  rename or doc comment?

Report a short list of any violations found.  If none,
say "Style checks passed."

## Wave 2: Deeper review

Only run wave 2 after wave 1 passes.

Start with the consolidated mechanical script (one
approval):

```
tools/audit/wave2-mechanical.sh
```

It reports (does not block; never exits non-zero on
findings):

- TODO / FIXME / HACK / XXX in changed source files.
- Newly added `#[allow(dead_code)]` annotations.
- Count of new `#[test]` functions vs Rust files
  changed.
- Documentation files touched (warns if none — the diff
  may have merited doc updates).
- New `unsafe {}` blocks.
- New `.unwrap()` / `.expect()` in changed files (raw
  list — review whether each is in test code or panic-
  safe in production).

Then spawn the judgment agents below.  They can run in
parallel.

### 2a. Code quality

| Setting | Value |
|---------|-------|
| Model | sonnet |
| Effort | medium |

**Brief for sub-agent:**

The mechanical script (`tools/audit/wave2-mechanical.sh`)
already extracted TODO/FIXME comments, new
`#[allow(dead_code)]`, `unsafe{}` blocks, and unwrap/
expect lists.  Take that report as input.

Add the judgment-level review on the diff
(`git diff develop...HEAD`):

- **Duplicated code:** Are there significant blocks of
  duplicated logic that the mechanical scan can't see?
  Look for copy-paste patterns across channel handlers
  or message parsers.
- **Missed abstractions:** Should any new code be
  extracted into a shared module?  Look for logic a
  second channel handler or decompressor would likely
  need.
- **Triage the script's raw findings:** for each
  TODO/unwrap/unsafe the mechanical script flagged, say
  blocking or advisory and why.  Skip ones in
  `#[cfg(test)]` blocks.

<!-- shared-block: comment-proportion v1 -->
Comment proportion (shared block; do not edit -- the canonical
copy lives in shakenfist/development at
`templates/shared-blocks/comment-proportion.md`):

- A comment or docstring earns its length by saying what the code
  cannot: the contract, the units, the failure modes, the reason a
  surprising choice is correct. Restating the code in prose is not
  documentation.
- Treat as candidates any added comment or docstring that is longer
  than the code it documents, and any comment block over roughly
  fifteen lines attached to a body under ten. These are candidates,
  not verdicts -- a subtle algorithm, a public API contract, or a
  hard-won bug explanation can justify the length.
- Where the length is not justified the finding is advisory, and
  the fix is to cut the restatement rather than delete the comment:
  keep the why, drop the line-by-line narration of the what.
- Prose that documents user-visible behaviour rather than the
  implementation usually belongs in `docs/`, with the comment
  reduced to a pointer.
<!-- shared-block-end -->

Report findings as a bullet list.  For each finding,
state the file, line, and whether it's blocking (must
fix before push) or advisory (can address later).

### 2b. Test review

| Setting | Value |
|---------|-------|
| Model | sonnet |
| Effort | medium |

**Brief for sub-agent:**

Review the diff (`git diff develop...HEAD`) for test
coverage:

- Does every new public function or significant code
  path have test coverage?
- Do the tests include adversarial cases (malformed
  input, empty data, overflow values)?
- Are there any assertions that test implementation
  details rather than behaviour (fragile tests)?
- Are there any new modules or functions with zero test
  coverage that should have at least basic tests?

Also verify:
- All existing tests still pass (wave 1 already
  confirmed this, so just check the wave 1 result).
- If practical, note whether `make test-qemu` should
  be run to verify end-to-end SPICE protocol
  interaction.

Report findings as a bullet list grouped by file.

### 2c. Documentation review

| Setting | Value |
|---------|-------|
| Model | sonnet |
| Effort | medium |

**Brief for sub-agent:**

Check that documentation matches the current code state.
Read the diff (`git diff develop...HEAD`) and verify:

<!-- shared-block: readme-discipline v1 -->
README discipline (shared block; do not edit -- the canonical
copy lives in shakenfist/development at
`templates/shared-blocks/readme-discipline.md`):

- New user-visible features are documented in `docs/` (and
  `ARCHITECTURE.md` / `AGENTS.md` where appropriate), not by
  adding bullets to `README.md`.
- `README.md` is a pitch: what the project is, who it is for,
  minimal installation instructions, a small number of usage
  examples, and curated absolute links into `docs/`. It only
  changes when the pitch, the install story, or the
  documentation links change.
- README growth is itself a finding: if the diff adds README
  content that belongs in `docs/`, flag it as blocking and
  move it.
<!-- shared-block-end -->

<!-- shared-block: llm-doc-discipline v1 -->
AGENTS.md and ARCHITECTURE.md discipline (shared block; do not
edit -- the canonical copy lives in shakenfist/development at
`templates/shared-blocks/llm-doc-discipline.md`):

- `AGENTS.md` is a working guide: the conventions, invariants and
  gotchas an agent cannot infer by reading the code, plus curated
  links into `docs/`. It is loaded into every session, so every
  line costs context on every task.
- `ARCHITECTURE.md` is a map: the component inventory, how data
  moves between components, and why the shape is the way it is.
  A deep dive on one subsystem belongs in `docs/`, where humans
  benefit from it too.
- One canonical home per fact. If `docs/` covers it, link to it
  instead of restating it -- and the same rule applies between
  `AGENTS.md` and `ARCHITECTURE.md`.
- Neither file is a reference manual, a runbook, or a changelog.
  CLI flags, configuration keys, wire protocols, step-by-step
  procedures and plan history go to `docs/`.
- Growth in either file is itself a finding: if the diff adds
  content that belongs in `docs/`, flag it as blocking and move
  it.
<!-- shared-block-end -->

<!-- shared-block: plan-phase-references v1 -->
Plan phase references (shared block; do not edit -- the canonical
copy lives in shakenfist/development at
`templates/shared-blocks/plan-phase-references.md`):

- Documentation outside plans directories describes the current
  state of the software, not the history of how it was built. Do
  not write "implemented in phase 5" or "since phase 3 of the
  two-tier CI plan": a reader wants to know whether a feature
  exists, not which phase of which plan delivered it.
- If a documented behaviour is implemented, describe it plainly.
  If it is planned but not yet implemented, link to the master
  plan in `docs/plans/` instead of citing a phase number.
- Reserve the word "phase" for plan documents. A procedural
  document describing a live multi-stage process (a release
  runbook, say) should call its stages "steps" or "stages", so
  that a phase reference in `docs/` is always a plan smell.
- The consistency audit greps `README.md` and `docs/` (excluding
  plans directories) for "phase <number>". Append
  `<!-- audit-ok: phase-reference -->` to a line only when the
  reference is genuinely not about an implementation plan.
<!-- shared-block-end -->

- `ARCHITECTURE.md` reflects any new or modified
  channels, message types, compression algorithms,
  or the connection model.
- `docs/development.md` reflects any new dependencies or
  build commands, and `AGENTS.md` any new convention an
  agent could not infer by reading the code.
- Plan files in `docs/plans/` are up to date — completed
  phases are marked complete, deferred items are listed.
- If SPICE protocol behaviour changed, note whether
  `shakenfist/kerbside/docs/` needs review.

Report findings as a bullet list. "No documentation
gaps found" is a valid answer.

### 2d. Security review

| Setting | Value |
|---------|-------|
| Model | opus |
| Effort | high |

**Brief for sub-agent:**

Security review of the diff (`git diff develop...HEAD`).
This requires careful judgment — read the actual code,
not just the diff summary.

Check for:

- **Input validation:** Could malformed SPICE messages
  cause panics, buffer overflows, or excessive memory
  allocation? Look for unchecked indexing, unbounded
  allocations based on attacker-controlled lengths, and
  arithmetic overflow.
- **Credential handling:** Are user-controlled values
  (connection strings, passwords, certificate paths)
  handled safely? Are passwords logged or stored in
  plaintext?
- **TLS safety:** Is certificate validation correct?
  Are there paths where TLS could be silently
  downgraded? Is the CA cert handling sound?
- **Unsafe code:** Are there any new `unsafe` blocks?
  If so, is the safety invariant documented and sound?
- **Concurrency:** Are there new shared-state patterns
  (Arc, Mutex, atomics)? Could they deadlock or race?
- **Resource exhaustion:** Could a malicious server
  cause unbounded memory growth, file descriptor leaks,
  or CPU spin?

Report findings with severity (critical / high /
medium / low / informational). For each finding, state
the file, line, the vulnerability class, and a
recommended fix.

## Management session checklist

After all agents complete, the management session
should:

- [ ] Wave 1 passed (build, style).
- [ ] Wave 2 findings reviewed.
- [ ] Any blocking findings from 2a/2b/2c have been
      fixed and re-verified.
- [ ] Any security findings from 2d have been assessed
      — critical and high must be fixed before push.
- [ ] The commit history is clean (no fixup commits
      that should be squashed, no accidental files).
- [ ] The branch is up to date with the target branch
      (rebase if needed).
- [ ] Ready to push. (As a master plan's closing phase:
      every finding fixed, or declined in writing in the
      master plan.)

# Title for the plan

## Prompt

Before responding to questions or discussion points in this
document, explore the ryll codebase thoroughly. Read relevant
source files, understand existing patterns (SPICE protocol
handling, channel architecture, async task model, image
decompression, egui rendering), and ground your answers in
what the code actually does today. Do not speculate about
the codebase when you could read it instead. Where a question
touches on external concepts (SPICE protocol, QEMU, QXL,
TLS/RSA, LZ/GLZ compression), research as needed to give a
confident answer. Flag any uncertainty explicitly rather than
guessing.

All planning documents should go into `docs/plans/`.

Consult `ARCHITECTURE.md` for the system architecture
overview, channel types, and data flow. Consult `AGENTS.md`
for build commands, project conventions, code organisation,
and a table of protocol reference sources. Key references
include `shakenfist/kerbside` (Python SPICE proxy with
protocol docs and a reference client),
`/srv/src-reference/spice/spice-protocol/` (canonical SPICE
definitions), `/srv/src-reference/spice/spice-gtk/`
(reference C client), and `/srv/src-reference/qemu/qemu/`
(server-side SPICE in `ui/spice-*`).

When we get to detailed planning, I prefer a separate plan
file per detailed phase. These separate files should be named
for the master plan, in the same directory as the master
plan, and simply have `-phase-NN-descriptive` appended before
the `.md` file extension. Tracking of these sub-phases should
be done via a table like this in this master plan under the
Execution section:

```
| Phase | Plan | Status |
|-------|------|--------|
| 1. Message parsing | PLAN-thing-phase-01-parsing.md | Not started |
| 2. Decompression | PLAN-thing-phase-02-decomp.md | Not started |
| ...   | ...  | ...    |
```

I prefer one commit per logical change, and at minimum one
commit per phase. Do not batch unrelated changes into a
single commit. Each commit should be self-contained: it
should build, pass tests, and have a clear commit message
explaining what changed and why.

## Situation

...

## Mission and problem statement

...

## Open questions

...

## Execution

...

## Agent guidance

When planning phases and steps, assess the effort level
and context that a sub-agent would need to implement each
one. This section helps the operator choose the right
agent configuration (effort level, model, isolation) for
each piece of work without having to re-read the full
plan.

### Master plan effort

The master plan itself should always be created at **high
effort** — it requires broad codebase understanding,
cross-referencing multiple source files, and making
judgment calls about scope and sequencing.

### Phase plan effort

Each phase plan should specify the recommended effort
level for planning that phase. Phases involving deep
protocol research, algorithm understanding, or
architectural decisions should be planned at high effort.
Phases that are mechanical or follow well-established
patterns can be planned at medium effort.

### Step-level guidance

Each phase plan should include a table like this:

```
| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
| 1a   | medium | any   | none      | One-sentence summary of what to do and which files to touch |
| 1b   | high   | any   | worktree  | Why this needs high effort: requires understanding X to do Y |
```

**Effort levels:**
- **high** — Requires reading multiple files, making
  judgment calls, understanding non-obvious invariants,
  or researching external references. The sub-agent
  needs to think carefully about edge cases.
- **medium** — The plan provides enough context that the
  sub-agent can follow a clear brief. May need to read
  a few files but the approach is well-defined.
- **low** — Purely mechanical changes (rename, reformat,
  add a log line). The brief is a complete instruction.

**Model choice:** The planner should recommend which
model is best suited for each step. This is a judgment
call, not a rigid rule — the right model depends on what
the step requires, not on whether it's "planning" or
"implementation".

- **opus** — Best for steps that require deep reasoning,
  cross-file architectural understanding, subtle
  correctness judgment, or complex protocol research.
  Also appropriate for intricate implementation where
  getting it wrong would be costly to debug.
- **sonnet** — Good default for well-briefed
  implementation work. Faster and cheaper than opus.
  Works well when the plan front-loads the research
  and the brief is detailed enough that the agent
  doesn't need to make broad judgment calls.
- **haiku** — Suitable for purely mechanical tasks:
  search-and-replace, adding log lines, running
  commands. The brief must be a near-complete
  instruction.

The model choice interacts with effort level and brief
quality. A detailed brief compensates for a lighter
model — sonnet at medium effort with a thorough brief
often matches opus at medium effort with a vague brief.
The planner's job is to write briefs good enough that
the recommended model can succeed.

Note: the model also determines the context window
(opus has 1M tokens, sonnet and haiku have 200K). Steps
that require holding many files in context simultaneously
may need opus for that reason alone, even if the
reasoning itself is straightforward.

**When in doubt, skew to the more capable model.**
Saving money only matters if the outcome is still
acceptable. A failed or low-quality implementation
wastes more time (and therefore more money) than using
a heavier model would have cost. Only recommend a
lighter model when you are confident the brief is
detailed enough for it to succeed.

**Brief for sub-agent:** This is the key field. Write it
as if briefing a colleague who has never seen the
codebase. Include: what to change, which files to touch,
what patterns to follow, and any non-obvious constraints.
The better the brief, the lower the effort level needed
and the lighter the model that can succeed.

A good brief front-loads the research the planner already
did, so the implementing agent doesn't repeat it. For
example, instead of "add tests for the QUIC decoder",
write "add tests for `quic_decode()` in
`shakenfist-spice-compression/src/quic.rs`. Test vectors:
a 2x2 RGBA image encoded with the reference C encoder at
`/srv/src-reference/spice/spice-common/...`. The function
takes `(data, width, height)` and returns
`Option<Vec<u8>>` of RGBA pixels."

## Administration and logistics

### Success criteria

We will know when this plan has been successfully implemented
because the following statements will be true:

* The code passes `pre-commit run --all-files` (rustfmt,
  clippy with `-D warnings`, shellcheck).
* New code follows existing patterns: channel handler
  structure, message parsing via `byteorder`, async tasks
  via tokio, event communication via mpsc channels.
* There are unit tests for new logic, and the existing tests
  still pass (`make test`).
* Lines are wrapped at 120 characters, single quotes for
  Rust strings where applicable.
* `README.md`, `ARCHITECTURE.md`, and `AGENTS.md` have been
  updated if the change adds or modifies channels, message
  types, or compression algorithms.
* Documentation in `docs/` has been updated to describe any
  new features or configuration options.
* If the changes affect SPICE protocol behaviour, the
  relevant documentation in `shakenfist/kerbside/docs/` has
  also been reviewed and updated if needed.

### Future work

We should list obvious extensions, known issues, unrelated
bugs we encountered, and anything else we should one day do
but have chosen to defer to here so that we don't forget
them.

...

### Bugs fixed during this work

This section should list any bugs we encounter during
development that we fixed.

### Documentation index maintenance

When creating a new master plan from this template, update
the following files in `docs/plans/`:

* **`index.md`** — add a row to the *Master plans* table
  with the creation date, a link to the plan, a one-line
  intent summary, the initial status, and links to each
  phase plan file. Keep the table in chronological order.
* **`order.yml`** — add an entry for the new master plan
  so it appears in the documentation navigation bar. Phase
  files should *not* be added to `order.yml`.

When all phases of a plan are complete, update the status
column in `index.md` to *Complete*.

### Back brief

Before executing any step of this plan, please back brief
the operator as to your understanding of the plan and how
the work you intend to do aligns with that plan.

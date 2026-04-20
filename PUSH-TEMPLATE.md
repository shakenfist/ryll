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
  files in the diff vs `develop`, advisory check for
  unguarded `logging::log_message` calls.

Exit codes:

| Code | Meaning                          |
|------|----------------------------------|
| 0    | all wave 1 checks passed         |
| 1    | pre-commit failed                |
| 2    | rustfmt or clippy failed         |
| 3    | cargo test failed                |
| 4    | raw `println!`/`eprintln!` found |

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
conventions in `AGENTS.md`:

- Channel handler conventions: message loop structure,
  ACK handling, channel name prefix on log messages
  (e.g. `"display: ..."`, `"playback: ..."`), and the
  `repaint_notify.notify_one()` pairing requirement
  documented in AGENTS.md decision #16.
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

- `README.md` reflects any new features, changed usage,
  or updated project structure.
- `ARCHITECTURE.md` reflects any new or modified
  channels, message types, compression algorithms,
  or the connection model.
- `AGENTS.md` reflects any new dependencies, build
  commands, or conventions.
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
- [ ] Ready to push.

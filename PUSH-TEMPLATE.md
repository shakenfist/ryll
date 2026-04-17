Thanks for your work on this. I appreciate it. Some final
checks before I push.

## How to use this template

This pre-push audit is designed to run as parallel
sub-agents at different effort levels. The management
session spawns the agents, collects their findings, and
makes the final push decision.

Run the agents in two waves:

**Wave 1 (parallel, low cost):** Build verification and
style conformance. These are fast and mechanical -- run
them first to catch obvious issues before spending on
deeper review.

**Wave 2 (parallel, after wave 1 passes):** Code quality,
test review, documentation review, and security review.
These are more expensive but independent of each other.

The management session reviews all findings, fixes any
issues, and confirms the push.

## Wave 1: Mechanical checks

### 1a. Build verification

| Setting | Value |
|---------|-------|
| Model | haiku |
| Effort | low |

**Brief for sub-agent:**

Run these commands in order and report pass/fail for
each. Stop at the first failure.

1. `pre-commit run --all-files` — must exit 0.
2. `./scripts/check-rust.sh check` — must exit 0
   (runs rustfmt --check and clippy -D warnings via
   Docker).
3. Run the test suite via Docker:
   ```
   docker run --rm \
     -v "$(pwd)":/workspace \
     -v "$(pwd)/.cargo-cache/registry":/build/.cargo/registry \
     -v "$(pwd)/.cargo-cache/git":/build/.cargo/git \
     -w /workspace \
     -u "$(id -u):$(id -g)" \
     -e HOME=/build \
     ryll-dev cargo test --workspace
   ```
   All tests must pass.

Report the number of tests that passed and any failures.

### 1b. Style conformance

| Setting | Value |
|---------|-------|
| Model | sonnet |
| Effort | low |

**Brief for sub-agent:**

Check the diff (`git diff develop...HEAD`) against the
project conventions in `AGENTS.md`. Specifically verify:

- All source lines are wrapped at 120 characters.
- Channel handler conventions are followed: message loop
  structure, ACK handling, verbose logging via
  `settings::is_verbose()`, channel name prefix on all
  log messages (e.g. `"display: ..."`, `"playback: ..."`).
- Protocol message conventions: constants in
  `shakenfist-spice-protocol/src/constants.rs`, message
  parsing in `messages.rs`, name lookups in `logging.rs`.
- Image decompression conventions: header parsing,
  BGRX-to-RGBA conversion, `DecompressedImage` return
  type.
- No raw `println!` or `eprintln!` — all logging goes
  through `tracing` macros.

Report a short list of any violations found. If none,
say "Style checks passed."

## Wave 2: Deeper review

Only run wave 2 after wave 1 passes. These four agents
can run in parallel.

### 2a. Code quality

| Setting | Value |
|---------|-------|
| Model | sonnet |
| Effort | medium |

**Brief for sub-agent:**

Review the diff (`git diff develop...HEAD`) for:

- **Duplicated code:** Are there any significant blocks
  of duplicated logic? Look for copy-paste patterns
  across channel handlers or message parsers.
- **Missed abstractions:** Should any new code be
  extracted into a shared module? Look for logic that
  a second channel handler or decompressor would
  likely need.
- **TODO/FIXME comments:** List any TODO, FIXME, or
  HACK comments in the changed files. Are any of them
  blocking issues that should be resolved before push?
- **Dead code:** Are there any `#[allow(dead_code)]`
  annotations on new code? Are there unused imports,
  functions, or struct fields?

Report findings as a bullet list. For each finding,
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

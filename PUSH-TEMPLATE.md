Thanks for your work on this. I appreciate it. Some final
checks before I push:

## Code quality

 * Did the changes introduce any significant amount of
   duplicated code? Are there any missed opportunities for
   code reuse or refactoring?
 * Should any new code be extracted into a shared module?
   Look for logic that a second channel handler or
   decompressor would likely need.
 * Are there any TODO comments we should address as part
   of this work?
 * Please ensure all source code is wrapped at 120
   characters.

## Style conformance

 * Does the code follow the project conventions in
   `AGENTS.md`? Check in particular:
   - Rust conventions (rustfmt formatting, clippy clean
     with `-D warnings`).
   - Channel handler conventions (message loop structure,
     ACK handling, verbose logging via `settings::is_verbose`,
     channel name prefix on all log messages).
   - Protocol message conventions (constants in
     `protocol/constants.rs`, message parsing in
     `protocol/messages.rs`, name lookups in
     `protocol/logging.rs`).
   - Image decompression conventions (header parsing,
     BGRX-to-RGBA conversion, `DecompressedImage` return
     type).

## Tests

 * Is there unit test coverage for the changes? This should
   include normal and adversarial cases.
 * All tests should pass. We need to fix any failing tests
   now before we push.
 * Run `pre-commit run --all-files` and confirm all hooks
   pass (rustfmt, clippy, shellcheck).
 * If practical, test against `make test-qemu` to verify
   the SPICE protocol interaction works end-to-end.

## Documentation

 * Has `docs/` been updated to reflect any new or changed
   features?
 * Are all planning files in `docs/plans/`?
 * Has `ARCHITECTURE.md` been updated if this change adds
   or modifies channels, message types, compression
   algorithms, or the connection model?
 * Has `README.md` been updated if usage instructions,
   project structure, or setup steps have changed?
 * Has `AGENTS.md` been updated?
 * Is all deferred work and pre-existing errors listed in
   a plan file?
 * If the changes affect SPICE protocol behaviour or
   documentation, have the relevant docs in
   `shakenfist/kerbside/docs/` been reviewed and updated
   if needed?

## Security review

 * Review these changes as both a security reviewer and an
   experienced developer and correct any errors you find.
 * Are any user-controlled values (connection strings,
   passwords, certificate paths) handled safely?
 * Is TLS certificate validation correct? Are there any
   paths where TLS could be silently downgraded?
 * Could malformed SPICE messages cause panics, buffer
   overflows, or excessive memory allocation?

## Build verification

 * Does `make build` succeed?
 * Does `make test` pass?
 * Does `pre-commit run --all-files` pass?

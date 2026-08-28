#!/usr/bin/env bash
# wave1.sh — pre-push audit, mechanical wave.
#
# Runs the build, lint and test verification that PUSH-AUDIT.md
# wave 1 used to spawn two sub-agents to perform.  Single approval,
# single script, structured exit code.
#
# Exit code:
#   0  all checks passed
#   1  pre-commit failed
#   2  rustfmt/clippy failed
#   3  cargo test failed
#   4  style-conformance grep failed (raw println!/eprintln! found)
#   5  could not cd to the repository root
#   6  the audit range covered nothing: AUDIT_BASE or AUDIT_HEAD was
#      set but does not resolve, an explicitly-set range is empty, or
#      the defaulted develop...HEAD range is empty.  Note the last
#      one -- wave1 hard-fails there, where wave2-mechanical.sh only
#      warns; a build that passed on an empty range proved nothing
#      about the diff.
#   7  a wave 1b check could not locate what it scans: the
#      workspace members would not parse out of Cargo.toml, or the
#      channels directory has moved again.  Kept distinct from 4 on
#      purpose -- 4 means the code under audit is wrong, 7 means the
#      audit is.  A caller that cannot tell those apart will
#      eventually "fix" the wrong one.  Note that 7 makes a
#      previously advisory section fatal: a check that cannot find
#      its subject reports success, which is exactly how the
#      log_message check stayed broken for months.
#
# Style conformance is intentionally kept narrow here — only the
# fully-mechanical checks live in this script.  Anything needing
# judgment (channel-handler conventions, "missed abstractions",
# documentation alignment, security analysis) stays as a wave 2
# sub-agent.
#
# Usage: tools/audit/wave1.sh
#        AUDIT_BASE=<sha> AUDIT_HEAD=develop tools/audit/wave1.sh
# Run from the worktree root.

set -u

# Always run from the repo root the script lives in.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT" || exit 5

red() { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold() { printf '\033[1m%s\033[0m\n' "$*"; }

# Audit range resolution and validation is shared with
# wave2-mechanical.sh; it exits 6 on a range that would make every
# diff-scoped check below report nothing.
# shellcheck source=tools/audit/audit-range.sh
. "$SCRIPT_DIR/audit-range.sh"

# The two wave 1b style checks, in a sourceable file so
# tools/audit/test-wave1-style.sh can exercise them against fixtures.
# shellcheck source=tools/audit/wave1-checks.sh
. "$SCRIPT_DIR/wave1-checks.sh"
audit_range_init

bold "=== wave 1a: pre-commit ==="
if ! pre-commit run --all-files; then
    red "FAIL: pre-commit"
    exit 1
fi
green "PASS: pre-commit"
echo

bold "=== wave 1a: rustfmt + clippy via Docker ==="
if ! ./scripts/check-rust.sh check; then
    red "FAIL: rustfmt/clippy"
    exit 2
fi
green "PASS: rustfmt + clippy"
echo

bold "=== wave 1a: cargo test --workspace via Docker ==="
mkdir -p .cargo-cache/registry .cargo-cache/git
if ! docker run --rm \
    -v "$(pwd)":/workspace \
    -v "$(pwd)/.cargo-cache/registry":/build/.cargo/registry \
    -v "$(pwd)/.cargo-cache/git":/build/.cargo/git \
    -w /workspace \
    -u "$(id -u):$(id -g)" \
    -e HOME=/build \
    ryll-dev cargo test --workspace; then
    red "FAIL: cargo test"
    exit 3
fi
green "PASS: cargo test"
echo

bold "=== wave 1b: mechanical style checks ==="

# 1. No raw println! / eprintln! in non-test source code.
#    Allowlist mechanism: a println!/eprintln! is permitted
#    when the file containing it also contains the marker
#    comment `audit-allow-println`. Any such marker must be
#    reviewed and justified in the same commit that adds it.
#    Files with the marker are entirely excluded from this
#    check — intentional, because a single operator-facing
#    print in a file is the expected pattern and the marker
#    documents the rationale inline.
#
#    The filtering -- both the marker and the exclusion of
#    test-only code -- lives in filter-println-hits.py beside this
#    script, and the scan directories come from the workspace
#    `members` list rather than a list maintained here.  Both moved
#    out of this file so tools/audit/test-wave1-style.sh can call
#    them against fixtures; see wave1-checks.sh for why each one
#    reads the way it does.
mapfile -t MEMBER_SRC_DIRS < <(workspace_member_src_dirs Cargo.toml)
#    A parse failure here must be loud.  An empty list would make
#    the check pass vacuously, which is the exact failure being
#    fixed.
if [[ ${#MEMBER_SRC_DIRS[@]} -eq 0 ]]; then
    red "FAIL: could not read workspace members from Cargo.toml"
    exit 7
fi
PRINTLN_HITS=$(grep -rn --include='*.rs' -E '^[[:space:]]*(println|eprintln)!' \
    "${MEMBER_SRC_DIRS[@]}" 2>/dev/null \
    | grep -v '/tests/' \
    | python3 "$SCRIPT_DIR/filter-println-hits.py" \
    || true)
if [[ -n "$PRINTLN_HITS" ]]; then
    red "FAIL: raw println!/eprintln! found:"
    echo "$PRINTLN_HITS"
    exit 4
fi
green "PASS: no raw println!/eprintln!"

# 2. No log_message calls outside a verbosity guard.  Heuristic:
#    every channel handler that calls logging::log_message should have
#    a verbosity check within the surrounding 5 lines.
#
#    Both halves of this check had gone stale -- it scanned a
#    directory the crate extraction deleted, and keyed on a
#    convention all seven channels had dropped.  wave1-checks.sh
#    carries the detail.
#
#    Content comes from the audit head, the same as the long-line
#    check below, rather than the live working tree: this check used
#    to `grep -r` the checkout directly, so on a historical audit
#    range (AUDIT_HEAD != what's checked out, see PUSH-AUDIT.md, "Two
#    ways this runbook is invoked") it silently answered a different
#    question from every other range-scoped check in this script.
#    audit_range_tree_files finds the file list at AUDIT_HEAD instead
#    of `find`/`-d` on the checkout, so a moved-or-deleted directory
#    is caught there and not masked by whatever happens to be on
#    disk; audit_range_show reads each file's bytes the same way the
#    long-line check does.
CHANNELS_DIR=shakenfist-spice-renderer/src/channels
UNGUARDED=""
mapfile -t CHANNELS_HEAD_FILES < <(audit_range_tree_files "$CHANNELS_DIR")
if [[ ${#CHANNELS_HEAD_FILES[@]} -eq 0 ]]; then
    if [[ -n "$AUDIT_RANGE_USABLE" ]]; then
        red "FAIL: $CHANNELS_DIR does not exist at $AUDIT_HEAD; the log_message check has gone stale again"
        exit 7
    fi
    # Range unusable (audit_range_init already printed a NOTE): every
    # other range-scoped check goes quiet here too, so this one does
    # as well rather than reading a live directory none of its peers
    # would agree is "the audit range".
else
    CHANNELS_SNAPSHOT="$(mktemp -d)"
    for f in "${CHANNELS_HEAD_FILES[@]}"; do
        mkdir -p "$CHANNELS_SNAPSHOT/$(dirname "$f")"
        audit_range_show "$f" > "$CHANNELS_SNAPSHOT/$f"
    done
    #    The awk below reads grep -B5 groups, which are separated by
    #    "--".  For each log_message line it asks whether any of the
    #    context lines *preceding it in the same group* carries a
    #    verbosity guard.  The previous version tested the same two
    #    conditions in the wrong order -- it cleared its flag on a
    #    guard and then re-set it on the log_message line that
    #    followed, so a guard above the call never counted -- and it
    #    printed only whatever hit happened to be last.  Every
    #    guarded site was a candidate to be reported and every site
    #    but one could not be.
    UNGUARDED=$(unguarded_log_messages "$CHANNELS_SNAPSHOT/$CHANNELS_DIR" \
        | sed "s#^$CHANNELS_SNAPSHOT/##")
    rm -rf "$CHANNELS_SNAPSHOT"
fi
# Advisory: this heuristic has known false positives where one guard
# wraps several calls (see unguarded_log_messages).  A precise check
# would have to parse, and is left to wave 2a.
if [[ -n "$UNGUARDED" ]]; then
    echo "ADVISORY: possibly-unguarded logging::log_message:"
    echo "$UNGUARDED"
    echo "(verify manually; this heuristic has false positives)"
fi

# 3. Long-line check: warn on Rust source lines over 120 chars in changed
#    files relative to the audit base.  Non-fatal — purely
#    informational.
#    Content comes from the audit head rather than the checkout, so
#    the check still sees the right bytes when AUDIT_HEAD is not what
#    is checked out.
LONG_LINES=""
while IFS= read -r f; do
    [[ -n "$f" ]] || continue
    hits=$(audit_range_show "$f" \
        | awk -v f="$f" 'length > 120 {print f":"NR": "length" chars"}')
    if [[ -n "$hits" ]]; then
        LONG_LINES+="$hits"$'\n'
    fi
done < <(audit_range_files_matching '*.rs')
if [[ -n "$LONG_LINES" ]]; then
    echo "ADVISORY: lines over 120 chars in changed Rust files:"
    echo "$LONG_LINES" | head -20
fi

green "PASS: wave 1b mechanical"
echo

bold "=== wave 1 complete ==="
# The range only reaches here empty or unusable when it was left to
# default -- an explicit one that selects nothing already exited 6.
# Either way the diff-scoped checks proved nothing, and a green "all
# checks passed" scrolling into view ten minutes after the warning at
# the top would be read as if they had.
if ! audit_range_closing_summary red; then
    # An empty range is fatal here: build, lint and tests passing says
    # nothing about a diff that was never looked at.  A range that
    # would not resolve at all returns 0 -- a shallow clone with no
    # 'develop' has always been a NOTE rather than a failure.
    red "wave 1 is not complete: see above."
    exit 6
fi
green "all mechanical checks passed; proceed to wave 2 (judgment agents)"
exit 0

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
#   6  AUDIT_BASE was set but does not resolve to a commit
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

# Audit range.  Defaults to develop...HEAD -- the pre-push gate,
# where the work under audit is the unpushed branch.  Override with
# AUDIT_BASE / AUDIT_HEAD when auditing a master plan's accumulated
# diff after its phases have already merged; see the "Two ways this
# runbook is invoked" section of PUSH-AUDIT.md for the derivation.
AUDIT_BASE_SET="${AUDIT_BASE+set}"
AUDIT_BASE="${AUDIT_BASE:-develop}"
AUDIT_HEAD="${AUDIT_HEAD:-HEAD}"
AUDIT_RANGE="${AUDIT_BASE}...${AUDIT_HEAD}"

red() { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold() { printf '\033[1m%s\033[0m\n' "$*"; }

# An explicitly-set range that does not resolve, or resolves to nothing,
# is the failure this override exists to prevent -- an audit that reports
# "no findings" because it looked at an empty diff.  Say so loudly.
if ! git rev-parse --verify "$AUDIT_BASE" >/dev/null 2>&1; then
    if [[ -n "$AUDIT_BASE_SET" ]]; then
        red "FAIL: AUDIT_BASE=$AUDIT_BASE does not resolve to a commit"
        exit 6
    fi
    echo "NOTE: '$AUDIT_BASE' not found; diff-scoped checks are skipped."
elif [[ -z "$(git diff --name-only "$AUDIT_RANGE")" ]]; then
    echo "WARNING: $AUDIT_RANGE is an empty diff.  Every diff-scoped"
    echo "check below will report nothing, and that is not a result."
    echo "See PUSH-AUDIT.md, 'Two ways this runbook is invoked'."
fi

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
#    We use a Python one-liner to filter: for each grep hit
#    (format "path:lineno:text"), check whether the source
#    file contains the marker anywhere; if so, skip it.
PRINTLN_HITS=$(grep -rn --include='*.rs' -E '^[[:space:]]*(println|eprintln)!' \
    ryll/src shakenfist-spice-protocol/src shakenfist-spice-compression/src \
    shakenfist-spice-usbredir/src 2>/dev/null \
    | grep -v '#\[cfg(test)\]' \
    | grep -v '/tests/' \
    | python3 -c "
import sys
for line in sys.stdin:
    parts = line.split(':', 2)
    if len(parts) >= 1:
        try:
            content = open(parts[0]).read()
            if 'audit-allow-println' in content:
                continue
        except OSError:
            pass
    print(line, end='')
" \
    || true)
if [[ -n "$PRINTLN_HITS" ]]; then
    red "FAIL: raw println!/eprintln! found:"
    echo "$PRINTLN_HITS"
    exit 4
fi
green "PASS: no raw println!/eprintln!"

# 2. No log_message calls outside an is_verbose() guard.  Heuristic:
#    every channel handler that calls logging::log_message should have
#    a settings::is_verbose() check within the surrounding 5 lines.
UNGUARDED=$(grep -rn -B5 'logging::log_message' ryll/src/channels/ 2>/dev/null \
    | awk '/logging::log_message/ {hit=$0} /is_verbose/ {hit=""} END{if(hit) print hit}' \
    || true)
# The above heuristic is rough; only flag if ALL nearby is_verbose
# checks are missing.  A more precise check is left to wave 2a.
if [[ -n "$UNGUARDED" ]]; then
    echo "ADVISORY: possibly-unguarded logging::log_message:"
    echo "$UNGUARDED"
    echo "(verify manually; this heuristic has false positives)"
fi

# 3. Long-line check: warn on Rust source lines over 120 chars in changed
#    files relative to the audit base.  Non-fatal — purely
#    informational.
if git rev-parse --verify "$AUDIT_BASE" >/dev/null 2>&1; then
    LONG_LINES=$(git diff "$AUDIT_RANGE" --name-only -- '*.rs' \
        | xargs -r awk 'length > 120 {print FILENAME":"NR": "length" chars"}' \
        2>/dev/null || true)
    if [[ -n "$LONG_LINES" ]]; then
        echo "ADVISORY: lines over 120 chars in changed Rust files:"
        echo "$LONG_LINES" | head -20
    fi
fi

green "PASS: wave 1b mechanical"
echo

bold "=== wave 1 complete ==="
green "all mechanical checks passed; proceed to wave 2 (judgment agents)"
exit 0

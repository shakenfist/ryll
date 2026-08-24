#!/usr/bin/env bash
# wave2-mechanical.sh — pre-push audit, scriptable parts of wave 2.
#
# Runs the mechanical subset of wave 2 checks that previously required
# spawning sub-agents.  The judgment-needing parts (missed
# abstractions, doc accuracy, security analysis) still need agents
# (waves 2a-judgment, 2c, 2d).
#
# Reports findings as plain text; never exits non-zero unless the
# script itself failed.  Read the output and decide what to fix.
#
# Usage: tools/audit/wave2-mechanical.sh
#        AUDIT_BASE=<sha> AUDIT_HEAD=develop tools/audit/wave2-mechanical.sh
# Run from the worktree root.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT" || exit 1

# Audit range.  Defaults to develop...HEAD -- the pre-push gate,
# where the work under audit is the unpushed branch.  Override with
# AUDIT_BASE / AUDIT_HEAD when auditing a master plan's accumulated
# diff after its phases have already merged; see the "Two ways this
# runbook is invoked" section of PUSH-AUDIT.md for the derivation.
AUDIT_BASE_SET="${AUDIT_BASE+set}"
AUDIT_BASE="${AUDIT_BASE:-develop}"
AUDIT_HEAD="${AUDIT_HEAD:-HEAD}"
AUDIT_RANGE="${AUDIT_BASE}...${AUDIT_HEAD}"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }

# An explicitly-set range that does not resolve, or resolves to nothing,
# is the failure this override exists to prevent -- an audit that reports
# "no findings" because it looked at an empty diff.  Say so loudly.
if ! git rev-parse --verify "$AUDIT_BASE" >/dev/null 2>&1; then
    if [[ -n "$AUDIT_BASE_SET" ]]; then
        echo "FAIL: AUDIT_BASE=$AUDIT_BASE does not resolve to a commit"
        exit 1
    fi
    echo "NOTE: '$AUDIT_BASE' not found; diff-scoped checks are skipped."
elif [[ -z "$(git diff --name-only "$AUDIT_RANGE")" ]]; then
    echo "WARNING: $AUDIT_RANGE is an empty diff.  Every diff-scoped"
    echo "check below will report nothing, and that is not a result."
    echo "See PUSH-AUDIT.md, 'Two ways this runbook is invoked'."
fi

bold "=== wave 2a: TODO / FIXME / HACK in changed files ==="
if git rev-parse --verify "$AUDIT_BASE" >/dev/null 2>&1; then
    HITS=$(git diff "$AUDIT_RANGE" --name-only \
        | xargs -r grep -nH -E '\b(TODO|FIXME|HACK|XXX)\b' 2>/dev/null \
        | grep -v 'docs/plans/' \
        || true)
    if [[ -n "$HITS" ]]; then
        echo "$HITS"
    else
        echo "(none)"
    fi
fi
echo

bold "=== wave 2a: new #[allow(dead_code)] in changed files ==="
if git rev-parse --verify "$AUDIT_BASE" >/dev/null 2>&1; then
    git diff "$AUDIT_RANGE" -- '*.rs' \
        | grep -E '^\+.*allow\(dead_code\)' \
        | head -20
    echo "(if any of the above were added in this branch, consider whether the dead code can be deleted instead)"
fi
echo

bold "=== wave 2b: new test count in changed files ==="
if git rev-parse --verify "$AUDIT_BASE" >/dev/null 2>&1; then
    NEW_TESTS=$(git diff "$AUDIT_RANGE" -- '*.rs' \
        | grep -cE '^\+\s*#\[test\]' \
        || true)
    echo "new #[test] functions: $NEW_TESTS"
    NEW_RS=$(git diff "$AUDIT_RANGE" --name-only -- '*.rs' | wc -l)
    echo "rust files changed: $NEW_RS"
fi
echo

bold "=== wave 2c: doc files touched in changed set ==="
if git rev-parse --verify "$AUDIT_BASE" >/dev/null 2>&1; then
    DOCS=$(git diff "$AUDIT_RANGE" --name-only \
        | grep -E '^(README\.md|ARCHITECTURE\.md|AGENTS\.md|STYLEGUIDE\.md|docs/)' \
        || true)
    if [[ -n "$DOCS" ]]; then
        echo "$DOCS"
    else
        echo "WARNING: no documentation files touched.  Did the changes merit doc updates?"
    fi
fi
echo

bold "=== wave 2d: security smoke ==="
echo "new unsafe{} blocks in changed files:"
if git rev-parse --verify "$AUDIT_BASE" >/dev/null 2>&1; then
    git diff "$AUDIT_RANGE" -- '*.rs' \
        | grep -nE '^\+.*\bunsafe\b' \
        | head -10 \
        || echo "(none)"
fi
echo

echo "new .unwrap() / .expect() in non-test code:"
if git rev-parse --verify "$AUDIT_BASE" >/dev/null 2>&1; then
    git diff "$AUDIT_RANGE" -- '*.rs' \
        | grep -nE '^\+.*\.(unwrap|expect)\s*\(' \
        | head -20 \
        || echo "(none)"
    echo "(review each: are they panic-safe given the inputs?)"
fi
echo

bold "=== wave 2 mechanical complete ==="
echo "now spawn agents for the judgment-needing parts:"
echo "  2a-judgment: code quality / missed abstractions"
echo "  2c-judgment: doc accuracy vs code intent"
echo "  2d-judgment: security review (input validation, TLS, concurrency)"
exit 0

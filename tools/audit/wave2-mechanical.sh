#!/usr/bin/env bash
# wave2-mechanical.sh — pre-push audit, scriptable parts of wave 2.
#
# Runs the mechanical subset of wave 2 checks that previously required
# spawning sub-agents.  The judgment-needing parts (missed
# abstractions, doc accuracy, security analysis) still need agents
# (waves 2a-judgment, 2c, 2d).
#
# Reports findings as plain text; never exits non-zero unless the
# script itself failed or the audit range is unusable.  Read the
# output and decide what to fix.
#
# Exit code:
#   0  ran to completion
#   5  could not cd to the repository root
#   6  the audit range is unusable: AUDIT_BASE or AUDIT_HEAD was set
#      but does not resolve, or an explicitly-set range is empty
#
# Usage: tools/audit/wave2-mechanical.sh
#        AUDIT_BASE=<sha> AUDIT_HEAD=develop tools/audit/wave2-mechanical.sh
# Run from the worktree root.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT" || exit 5

bold() { printf '\033[1m%s\033[0m\n' "$*"; }

# Audit range resolution and validation is shared with wave1.sh; it
# exits 6 on a range that would make every check below report nothing.
# shellcheck source=tools/audit/audit-range.sh
. "$SCRIPT_DIR/audit-range.sh"
audit_range_init

# Content comes from the audit head rather than the checkout: a file
# deleted within the range is not in the working tree to grep, and a
# checkout that has drifted from AUDIT_HEAD holds the wrong bytes.
bold "=== wave 2a: TODO / FIXME / HACK in changed files ==="
if [[ -n "$AUDIT_RANGE_USABLE" ]]; then
    HITS=""
    while IFS= read -r f; do
        [[ -n "$f" ]] || continue
        case "$f" in docs/plans/*) continue ;; esac
        # -I so a changed binary file does not report itself as a
        # match, and awk rather than sed so a path containing a sed
        # metacharacter cannot rewrite the expression.
        hits=$(audit_range_show "$f" \
            | grep -InE '\b(TODO|FIXME|HACK|XXX)\b' \
            | awk -v f="$f" '{print f":"$0}')
        if [[ -n "$hits" ]]; then
            HITS+="$hits"$'\n'
        fi
    done < <(audit_range_files)
    if [[ -n "$HITS" ]]; then
        printf '%s' "$HITS"
    else
        echo "(none)"
    fi
fi
echo

bold "=== wave 2a: new #[allow(dead_code)] in changed files ==="
if [[ -n "$AUDIT_RANGE_USABLE" ]]; then
    git diff "$AUDIT_RANGE" -- '*.rs' \
        | grep -E '^\+.*allow\(dead_code\)' \
        | head -20
    echo "(if any of the above were added in this branch, consider whether the dead code can be deleted instead)"
fi
echo

bold "=== wave 2b: new test count in changed files ==="
if [[ -n "$AUDIT_RANGE_USABLE" ]]; then
    NEW_TESTS=$(git diff "$AUDIT_RANGE" -- '*.rs' \
        | grep -cE '^\+\s*#\[test\]' \
        || true)
    echo "new #[test] functions: $NEW_TESTS"
    NEW_RS=$(git diff "$AUDIT_RANGE" --name-only -- '*.rs' | wc -l)
    echo "rust files changed: $NEW_RS"
fi
echo

bold "=== wave 2c: doc files touched in changed set ==="
if [[ -n "$AUDIT_RANGE_USABLE" ]]; then
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

# Both checks capture and test rather than piping to '|| echo'; the
# '||' binds to head, which exits 0 on empty input, so the "(none)"
# fallback could never fire and the header was followed by a blank
# line.  A reader cannot tell "checked, found nothing" from "the check
# did not run".  'grep -n' is dropped with them: it numbers lines
# within the diff stream, not within any source file, so the numbers
# meant nothing to the reader they were printed for.
bold "=== wave 2d: security smoke ==="
echo "new unsafe{} blocks in changed files:"
if [[ -n "$AUDIT_RANGE_USABLE" ]]; then
    UNSAFE=$(git diff "$AUDIT_RANGE" -- '*.rs' \
        | grep -E '^\+.*\bunsafe\b' \
        | head -10)
    if [[ -n "$UNSAFE" ]]; then
        echo "$UNSAFE"
    else
        echo "(none)"
    fi
fi
echo

echo "new .unwrap() / .expect() in non-test code:"
if [[ -n "$AUDIT_RANGE_USABLE" ]]; then
    PANICKY=$(git diff "$AUDIT_RANGE" -- '*.rs' \
        | grep -E '^\+.*\.(unwrap|expect)\s*\(' \
        | head -20)
    if [[ -n "$PANICKY" ]]; then
        echo "$PANICKY"
        echo "(review each: are they panic-safe given the inputs?)"
    else
        echo "(none)"
    fi
fi
echo

bold "=== wave 2 mechanical complete ==="
# Shared with wave1.sh, which exits on the code this returns.  This
# script reports findings as text and stays 0 by contract, so the
# verdict is printed and the code deliberately dropped.
audit_range_closing_summary echo || true
echo "now spawn agents for the judgment-needing parts:"
echo "  2a-judgment: code quality / missed abstractions"
echo "  2c-judgment: doc accuracy vs code intent"
echo "  2d-judgment: security review (input validation, TLS, concurrency)"
exit 0

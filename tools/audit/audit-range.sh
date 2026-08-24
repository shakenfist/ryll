# audit-range.sh — audit range resolution, shared by the audit scripts.
#
# Sourced, never executed.  Provides:
#
#   audit_range_init        resolve and validate AUDIT_BASE/AUDIT_HEAD
#   audit_range_files       every file changed in the range
#   audit_range_files_matching PATHSPEC...  the same, filtered
#   audit_range_show FILE   that file's content *at the audit head*
#
# and sets AUDIT_BASE, AUDIT_HEAD, AUDIT_RANGE, AUDIT_RANGE_USABLE and
# AUDIT_RANGE_EMPTY.
#
# The range defaults to develop...HEAD -- the pre-push gate, where the
# work under audit is the unpushed branch.  Override with AUDIT_BASE /
# AUDIT_HEAD when auditing a master plan's accumulated diff after its
# phases have already merged; see the "Two ways this runbook is
# invoked" section of PUSH-AUDIT.md for the derivation.
#
# Exit code on failure: 6, in both callers.  Every failure here is an
# audit that would otherwise report "no findings" because it looked at
# nothing, which is a wrong answer rather than a missing one.

# shellcheck shell=bash
#
# AUDIT_RANGE_USABLE and AUDIT_RANGE_EMPTY are read by the sourcing
# scripts, not here.  When this file is linted on its own -- which the
# pre-commit hook does, being handed only the changed files -- those
# readers are invisible and SC2034 fires on both.  (Mind the wrapping
# below: a comment line whose first word is the linter's own name is
# parsed as a directive, not as prose.)
# shellcheck disable=SC2034

AUDIT_RANGE_EXIT=6

# Resolve the range.  Exits 6 if an explicitly-set endpoint does not
# resolve, or if an explicitly-set range is empty.  An *unset* base
# that does not resolve (a shallow clone with no 'develop') is a NOTE
# and leaves AUDIT_RANGE_USABLE empty, matching the historical
# behaviour of the pre-push gate.
audit_range_init() {
    local base_set head_set
    base_set="${AUDIT_BASE+set}"
    head_set="${AUDIT_HEAD+set}"
    AUDIT_BASE="${AUDIT_BASE:-develop}"
    AUDIT_HEAD="${AUDIT_HEAD:-HEAD}"
    AUDIT_RANGE="${AUDIT_BASE}...${AUDIT_HEAD}"
    AUDIT_RANGE_USABLE=""
    AUDIT_RANGE_EMPTY=""

    if ! git rev-parse --verify --quiet "$AUDIT_BASE^{commit}" >/dev/null; then
        if [[ -n "$base_set" ]]; then
            echo "FAIL: AUDIT_BASE=$AUDIT_BASE does not resolve to a commit" >&2
            exit "$AUDIT_RANGE_EXIT"
        fi
        echo "NOTE: '$AUDIT_BASE' not found; diff-scoped checks are skipped."
        return 0
    fi

    # The head is validated whether or not it was set explicitly.  An
    # unresolvable head makes every 'git diff' below fail to stdout-
    # nothing, which reads exactly like a clean audit.
    if ! git rev-parse --verify --quiet "$AUDIT_HEAD^{commit}" >/dev/null; then
        if [[ -n "$head_set" ]]; then
            echo "FAIL: AUDIT_HEAD=$AUDIT_HEAD does not resolve to a commit" >&2
            exit "$AUDIT_RANGE_EXIT"
        fi
        # Unset and unresolvable means an unborn branch: nothing to audit.
        echo "NOTE: 'HEAD' has no commits yet; diff-scoped checks are skipped."
        return 0
    fi

    AUDIT_RANGE_USABLE=1

    if [[ -z "$(git diff --name-only "$AUDIT_RANGE")" ]]; then
        AUDIT_RANGE_EMPTY=1
        if [[ -n "$base_set" || -n "$head_set" ]]; then
            echo "FAIL: $AUDIT_RANGE is an empty diff.  An explicitly-set" >&2
            echo "range that selects nothing is operator error, not a" >&2
            echo "clean audit.  See PUSH-AUDIT.md, 'Two ways this runbook" >&2
            echo "is invoked'." >&2
            exit "$AUDIT_RANGE_EXIT"
        fi
        echo "WARNING: $AUDIT_RANGE is an empty diff.  Every diff-scoped"
        echo "check below will report nothing, and that is not a result."
        echo "See PUSH-AUDIT.md, 'Two ways this runbook is invoked'."
    fi
}

# Every file changed in the range.  Prints nothing when the range is
# unusable, so callers can pipe it unguarded.
audit_range_files() {
    [[ -n "$AUDIT_RANGE_USABLE" ]] || return 0
    # core.quotePath would C-quote any non-ASCII path, and the quoted
    # form does not resolve as 'git show <head>:<path>' -- a file that
    # silently drops out of every content check.
    git -c core.quotePath=false diff "$AUDIT_RANGE" --name-only
}

# The same, restricted to the given git pathspecs.  Split from
# audit_range_files rather than made variadic so that neither call site
# passes an empty argument list, which shellcheck reads as a mistake.
audit_range_files_matching() {
    [[ -n "$AUDIT_RANGE_USABLE" ]] || return 0
    git -c core.quotePath=false diff "$AUDIT_RANGE" --name-only -- "$@"
}

# One file's content at the audit head.  Deliberately *not* the working
# tree: AUDIT_HEAD is arbitrary, so a checkout that has drifted from it
# would have content checks silently scanning the wrong bytes, and a
# file deleted within the range would not be there to scan at all.
# Files absent at the head are skipped silently.
audit_range_show() {
    git show "$AUDIT_HEAD:$1" 2>/dev/null || true
}

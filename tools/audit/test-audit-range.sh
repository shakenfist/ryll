#!/usr/bin/env bash
# test-audit-range.sh — smoke test for tools/audit/audit-range.sh.
#
# The audit range is the one part of the audit scripts whose failure
# mode is silence: a range that selects nothing produces byte-identical
# output to a clean audit.  That is a wrong answer, not a missing one,
# so each way of getting it wrong is pinned here.
#
# Pure git plumbing against a scratch repository; runs in about a
# second and needs no Docker, unlike the rest of wave 1.
#
# Usage: tools/audit/test-audit-range.sh
# Exit code: 0 all assertions held, 1 otherwise.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPER="$SCRIPT_DIR/audit-range.sh"

# git exports GIT_DIR, GIT_INDEX_FILE and friends to the hooks it runs,
# and pre-commit runs this script as one.  Left set, they override the
# -C below and every git command here operates on the real repository
# instead of the scratch one -- which is exactly as bad as it sounds.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY \
      GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_PREFIX \
      GIT_NAMESPACE GIT_CEILING_DIRECTORIES GIT_CONFIG_GLOBAL

FAILURES=0
red() { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }

# Run audit_range_init in a subshell of the scratch repo with the given
# environment, printing "exit:<code>" after its output so a test can
# assert on both.  audit_range_init exits on failure, hence the subshell.
run_init() {
    (
        cd "$REPO" || exit 99
        # shellcheck source=/dev/null
        . "$HELPER"
        audit_range_init
        echo "usable:${AUDIT_RANGE_USABLE:-}"
        echo "empty:${AUDIT_RANGE_EMPTY:-}"
    ) 2>&1
    echo "exit:$?"
}

assert_contains() {
    local label="$1" needle="$2" haystack="$3"
    if [[ "$haystack" == *"$needle"* ]]; then
        green "PASS: $label"
    else
        red "FAIL: $label -- expected to find '$needle' in:"
        printf '%s\n' "$haystack" | sed 's/^/    /'
        FAILURES=$((FAILURES + 1))
    fi
}

assert_not_contains() {
    local label="$1" needle="$2" haystack="$3"
    if [[ "$haystack" != *"$needle"* ]]; then
        green "PASS: $label"
    else
        red "FAIL: $label -- did not expect '$needle' in:"
        printf '%s\n' "$haystack" | sed 's/^/    /'
        FAILURES=$((FAILURES + 1))
    fi
}

REPO="$(mktemp -d)"
trap 'rm -rf "$REPO"' EXIT

# Belt and braces, and it has to come before the init: 'git init' with
# GIT_DIR still set re-initialises whatever it points at, which is how
# this script once flipped core.bare on a real repository.  So check
# for a leaked variable the unset above did not cover before running
# any git command at all.
for leaked in GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY \
              GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR \
              GIT_NAMESPACE GIT_CEILING_DIRECTORIES; do
    if [[ -n "${!leaked:-}" ]]; then
        red "FAIL: $leaked is still set (=${!leaked})"
        red "Refusing to run: this test mutates whatever repository it finds."
        exit 1
    fi
done

git -C "$REPO" init -q -b develop

# And again afterwards, in case something outside the environment (a
# stray core.worktree, an includeIf) redirected the scratch repo.
ACTUAL_GIT_DIR="$(git -C "$REPO" rev-parse --absolute-git-dir)"
if [[ "$ACTUAL_GIT_DIR" != "$REPO/.git" ]]; then
    red "FAIL: scratch repo resolves to $ACTUAL_GIT_DIR, not $REPO/.git"
    red "Refusing to run: this test mutates whatever repository it finds."
    exit 1
fi
git -C "$REPO" config user.email audit@example.com
git -C "$REPO" config user.name 'Audit Test'

# --- an empty repository, before any commit exists ---------------------
out="$(run_init)"
assert_contains "empty repository is a NOTE, not a failure" "exit:0" "$out"
assert_contains "empty repository skips diff-scoped checks" \
    "diff-scoped checks are skipped" "$out"

echo 'fn main() {}' > "$REPO/base.rs"
git -C "$REPO" add -A
git -C "$REPO" commit -qm 'base'

git -C "$REPO" checkout -q -b phase
LONG="let _x = \"$(printf 'y%.0s' {1..140})\";"
printf 'fn phase() {\n    %s\n    // TODO: something\n}\n' "$LONG" > "$REPO/phase.rs"
git -C "$REPO" rm -q base.rs
git -C "$REPO" add -A
git -C "$REPO" commit -qm 'phase work'
PHASE_SHA="$(git -C "$REPO" rev-parse HEAD)"
# The checkout is deliberately left on develop for the content tests
# below: that is the drift the working-tree read used to miss.
git -C "$REPO" checkout -q develop

# --- an explicitly-set base that does not resolve ----------------------
out="$(AUDIT_BASE=nosuchref run_init)"
assert_contains "unresolvable AUDIT_BASE exits 6" "exit:6" "$out"
assert_contains "unresolvable AUDIT_BASE says which" "AUDIT_BASE=nosuchref" "$out"

# --- an explicitly-set head that does not resolve ----------------------
# This is the one a typo'd merge SHA produces, and the one that used to
# fall through to a green "all mechanical checks passed".
out="$(AUDIT_BASE=develop AUDIT_HEAD=nosuchref run_init)"
assert_contains "unresolvable AUDIT_HEAD exits 6" "exit:6" "$out"
assert_contains "unresolvable AUDIT_HEAD says which" "AUDIT_HEAD=nosuchref" "$out"

# --- an explicit range that resolves but selects nothing ---------------
out="$(AUDIT_BASE=develop AUDIT_HEAD=develop run_init)"
assert_contains "explicit empty range exits 6" "exit:6" "$out"
assert_contains "explicit empty range says so" "empty diff" "$out"

# --- the default range, empty (nothing to push) ------------------------
out="$(run_init)"
assert_contains "default empty range does not exit" "exit:0" "$out"
assert_contains "default empty range warns" "WARNING" "$out"
assert_contains "default empty range sets the flag" "empty:1" "$out"

# --- a shallow clone with no 'develop' ---------------------------------
git -C "$REPO" checkout -q -b other
git -C "$REPO" branch -q -D develop
out="$(run_init)"
assert_contains "missing default branch is a NOTE" "exit:0" "$out"
assert_contains "missing default branch skips checks" "diff-scoped checks are skipped" "$out"
assert_not_contains "missing default branch is not usable" "usable:1" "$out"
git -C "$REPO" branch -q develop "$(git -C "$REPO" rev-parse HEAD~0)"
git -C "$REPO" branch -f develop "$PHASE_SHA^"
git -C "$REPO" checkout -q develop

# --- an unborn HEAD with a resolvable base -----------------------------
# Not operator error, so a NOTE rather than the exit 6 an explicitly-set
# broken head gets.
out="$(cd "$REPO" && git checkout -q --orphan orphan && git rm -rq --cached . && cd - >/dev/null && run_init)"
assert_contains "unborn HEAD is a NOTE, not a failure" "exit:0" "$out"
assert_contains "unborn HEAD says so" "no commits yet" "$out"
assert_not_contains "unborn HEAD is not usable" "usable:1" "$out"
git -C "$REPO" checkout -q develop
git -C "$REPO" branch -q -D orphan 2>/dev/null || true

# --- content checks read the audit head, not the working tree ----------
out="$(
    cd "$REPO" || exit 99
    export AUDIT_BASE=develop AUDIT_HEAD="$PHASE_SHA"
    # shellcheck source=/dev/null
    . "$HELPER"
    audit_range_init >/dev/null
    for f in $(audit_range_files); do
        audit_range_show "$f" | awk -v f="$f" 'length > 120 {print f":"NR}'
        audit_range_show "$f" | grep -q 'TODO' && echo "todo:$f"
    done
    echo "files:$(audit_range_files | tr '\n' ',')"
)"
assert_contains "long line at the head is found off-checkout" "phase.rs:2" "$out"
assert_contains "TODO at the head is found off-checkout" "todo:phase.rs" "$out"
assert_contains "a file deleted in the range is still listed" "base.rs" "$out"
assert_not_contains "a file deleted in the range does not error" "No such file" "$out"

echo
if [[ "$FAILURES" -eq 0 ]]; then
    green "all audit-range assertions held"
    exit 0
fi
red "$FAILURES assertion(s) failed"
exit 1

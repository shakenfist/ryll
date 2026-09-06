#!/usr/bin/env bash
# test-fuzz-targets.sh — smoke test for tools/fuzz-targets.sh.
#
# The extractor decides which targets the nightly fuzz lane builds at
# all. A target it silently drops is never built, never fails and so is
# never reported -- the same silent failure mode that pinned
# tools/report-fuzz-failure.sh and tools/audit/test-audit-range.sh with
# tests, and the same convention.
#
# The guards are the point. The extraction is deliberately not a TOML
# parser, so what matters is that every spelling it cannot read fails
# loudly rather than shortening the list. Each case below is a manifest
# fixture, so the assertions cannot pass by agreeing with the extractor
# about what a manifest looks like.
#
# No network, no Docker, no cargo. Runs in well under a second.
#
# Usage: tools/test-fuzz-targets.sh
# Exit code: 0 all assertions held, 1 otherwise.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EXTRACTOR="$SCRIPT_DIR/fuzz-targets.sh"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILURES=0
red() { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }

OUT=""
STATUS=0

run_extractor() {
    OUT="$("$EXTRACTOR" "$@" 2>&1)"
    STATUS=$?
}

assert_status() {
    local want="$1" what="$2"
    if [ "$STATUS" -eq "$want" ]; then
        green "ok: $what (exit $want)"
    else
        red "FAIL: $what: expected exit $want, got $STATUS"
        FAILURES=$((FAILURES + 1))
    fi
}

assert_equals() {
    local want="$1" what="$2"
    if [ "$OUT" = "$want" ]; then
        green "ok: $what"
    else
        red "FAIL: $what: expected '$want', got '$OUT'"
        FAILURES=$((FAILURES + 1))
    fi
}

assert_contains() {
    local needle="$1" what="$2"
    if [[ "$OUT" == *"$needle"* ]]; then
        green "ok: $what"
    else
        red "FAIL: $what: output does not contain '$needle'"
        FAILURES=$((FAILURES + 1))
    fi
}

# A manifest fixture: a [workspace] table and a preamble, so the
# extractor has to actually track which table it is inside rather than
# grepping for `name`.
manifest() {
    local path="$1"; shift
    {
        printf '[package]\nname = "not-a-fuzz-target"\nversion = "0.0.0"\n\n'
        printf '[workspace]\n\n'
        printf '[dependencies.libfuzzer-sys]\nversion = "0.4"\n\n'
        for target in "$@"; do
            printf '[[bin]]\nname = "%s"\npath = "fuzz_targets/%s.rs"\ntest = false\n\n' \
                "$target" "$target"
        done
    } > "$path"
}

echo "== the happy path =="
manifest "$WORK/four.toml" alpha bravo charlie delta
run_extractor "$WORK/four.toml"
assert_status 0 "a four-target manifest is read"
assert_equals $'alpha\nbravo\ncharlie\ndelta' "all four names, in manifest order"

echo
echo "== the real manifest =="
run_extractor "$REPO_ROOT/shakenfist-spice-protocol/fuzz/Cargo.toml"
assert_status 0 "the repository's own fuzz manifest is read"
# Pinned by count rather than by name: adding a fuzz target should not
# fail this test, but silently dropping to zero or one must.
COUNT="$(printf '%s\n' "$OUT" | grep -c .)"
if [ "$COUNT" -ge 4 ]; then
    green "ok: the real manifest yields $COUNT targets (>= 4)"
else
    red "FAIL: the real manifest yielded only $COUNT target(s)"
    FAILURES=$((FAILURES + 1))
fi

echo
echo "== the mismatch guard =="
# `name="x"` is valid TOML that cargo fuzz accepts and the awk cannot
# see. Without the independent table count this would silently shorten
# the list, which is the whole reason the guard exists.
manifest "$WORK/mismatch.toml" alpha bravo
printf '[[bin]]\nname="charlie"\npath = "fuzz_targets/charlie.rs"\n' \
    >> "$WORK/mismatch.toml"
run_extractor "$WORK/mismatch.toml"
assert_status 1 "an unparseable [[bin]] name fails rather than shortening"
assert_contains "3 [[bin]] table(s) but 2 name(s)" "the guard names both counts"

# Indented `[[bin]]` is the other half of the same guard: the table count
# sees it and the awk does not. It has to be the first table to bite --
# after a column-0 table the awk is still inside a [[bin]] and picks the
# indented name up anyway -- and that is exactly the case where a
# manifest written entirely in the indented style would otherwise yield
# nothing but a passing run.
manifest "$WORK/indented.toml"
printf '  [[bin]]\n  name = "alpha"\n' >> "$WORK/indented.toml"
run_extractor "$WORK/indented.toml"
assert_status 1 "an indented [[bin]] table fails rather than being skipped"

echo
echo "== the zero-target guard =="
manifest "$WORK/empty.toml"
run_extractor "$WORK/empty.toml"
assert_status 1 "a manifest with no [[bin]] tables fails"
assert_contains "no fuzz targets found" "the zero-target guard explains itself"

echo
echo "== a missing manifest =="
run_extractor "$WORK/does-not-exist.toml"
assert_status 1 "a missing manifest fails"
assert_contains "no such file" "the missing manifest is named"

echo
echo "== the target name charset =="
# Repository-controlled and post-merge, so this is not an injection
# guard; it stops a name that would render or search strangely from
# reaching the issue title and the `in:title` qualifier.
manifest "$WORK/oddname.toml" alpha 'has space'
run_extractor "$WORK/oddname.toml"
assert_status 1 "a target name containing a space is rejected"
assert_contains "is not" "the charset guard explains itself"

# The space case is the one that would otherwise pass silently: reading
# the name as awk's third field truncates it to a clean-looking shorter
# name, agreeing with the table count and fuzzing a target that does not
# exist. Assert the mangled form never reaches stdout.
run_extractor "$WORK/oddname.toml"
if [[ "$OUT" == *$'\nhas\n'* || "$OUT" == *$'\nhas' ]]; then
    red "FAIL: a spaced name was truncated to its first word"
    FAILURES=$((FAILURES + 1))
else
    green "ok: a spaced name is not truncated to its first word"
fi

# The `$` is a literal byte in the fixture name, not an expansion --
# which is the whole point of the assertion, and exactly what the
# linter cannot tell from the outside, so it is told. Same disable,
# and same reason, as the fence fixture in test-report-fuzz-failure.sh.
# shellcheck disable=SC2016
manifest "$WORK/dollar.toml" 'na$me'
run_extractor "$WORK/dollar.toml"
assert_status 1 "a target name with a shell metacharacter is rejected"

manifest "$WORK/okname.toml" fuzz_link-mess2
run_extractor "$WORK/okname.toml"
assert_status 0 "underscores, hyphens and digits are accepted"

echo
echo "== usage =="
run_extractor
assert_status 2 "no arguments is a usage error"
run_extractor a b
assert_status 2 "two arguments is a usage error"

echo
if [ "$FAILURES" -eq 0 ]; then
    green "All fuzz-targets assertions passed."
    exit 0
fi
red "$FAILURES assertion(s) failed."
exit 1

#!/usr/bin/env bash
# test-report-fuzz-failure.sh — smoke test for tools/report-fuzz-failure.sh.
#
# The reporter is the nightly fuzz lane's only notification channel:
# the run's colour is a mark on the Actions tab that nobody reads, and
# the issue this script files is what actually reaches a human. Its
# failure mode is therefore silence, and silence is invisible until a
# fuzz target happens to break -- which is the same argument that
# pinned tools/audit/test-audit-range.sh, and the same convention.
#
# Everything here runs through --dry-run, so no network, no GH_TOKEN
# and no `gh`. Runs in well under a second.
#
# Usage: tools/test-report-fuzz-failure.sh
# Exit code: 0 all assertions held, 1 otherwise.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPORTER="$SCRIPT_DIR/report-fuzz-failure.sh"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILURES=0
red() { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }

OUT=""
STATUS=0

# Run the reporter, capturing stdout and stderr together in OUT and
# its exit code in STATUS. The script exits non-zero on bad arguments,
# hence the guard around set -e's absence here.
run_reporter() {
    OUT="$("$REPORTER" "$@" 2>&1)"
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

assert_contains() {
    local needle="$1" what="$2"
    if [[ "$OUT" == *"$needle"* ]]; then
        green "ok: $what"
    else
        red "FAIL: $what: output does not contain '$needle'"
        FAILURES=$((FAILURES + 1))
    fi
}

assert_absent() {
    local needle="$1" what="$2"
    if [[ "$OUT" != *"$needle"* ]]; then
        green "ok: $what"
    else
        red "FAIL: $what: output unexpectedly contains '$needle'"
        FAILURES=$((FAILURES + 1))
    fi
}

echo "== a normal log =="
printf 'compiling\nerror[E0432]: unresolved import\naborting\n' \
    > "$WORK/normal.log"
WORKFLOW_URL="https://example.invalid/run/1" \
    run_reporter fuzz_link_mess_parse "$WORK/normal.log" --dry-run
assert_status 0 "a normal log reports"
assert_contains "Nightly fuzz failure: fuzz_link_mess_parse" \
    "the title names the target"
assert_contains "https://example.invalid/run/1" "the run URL is recorded"
assert_contains "make fuzz-build-fuzz_link_mess_parse" \
    "the reproduce commands name the target"
assert_contains "error[E0432]: unresolved import" \
    "the log tail reaches the body"

echo
echo "== WORKFLOW_URL unset =="
run_reporter fuzz_link_mess_parse "$WORK/normal.log" --dry-run
assert_status 0 "an absent run URL is not fatal"
assert_contains "Run: unknown" "an absent run URL is named as unknown"

echo
echo "== a missing log file =="
run_reporter fuzz_gone "$WORK/does-not-exist.log" --dry-run
assert_status 0 "a missing log still reports"
assert_contains "not found" "a missing log warns"
assert_contains "Nightly fuzz failure: fuzz_gone" \
    "a missing log still names the target"

echo
echo "== an over-long line =="
# 500 bytes on one line, against MAX_LINE_BYTES=200. `cut -b` keeps
# the head of the line, so the tail must not survive.
{ head -c 300 < /dev/zero | tr '\0' 'a'
  head -c 200 < /dev/zero | tr '\0' 'z'
  echo
} > "$WORK/long-line.log"
run_reporter fuzz_long "$WORK/long-line.log" --dry-run
assert_status 0 "an over-long line reports"
assert_absent "zzzzzzzzzz" "bytes past MAX_LINE_BYTES are cut"

echo
echo "== a log over MAX_EXCERPT_BYTES =="
# Forty lines of 200 bytes is 8000 bytes, twice the 4000-byte budget.
# The excerpt is a tail, so the first line must be gone and the last
# must be present.
{ echo "FIRSTLINE"
  for _ in $(seq 1 40); do
      head -c 199 < /dev/zero | tr '\0' 'p'
      echo
  done
  echo "LASTLINE"
} > "$WORK/big.log"
run_reporter fuzz_big "$WORK/big.log" --dry-run
assert_status 0 "an oversized log reports"
assert_contains "LASTLINE" "the tail of an oversized log survives"
assert_absent "FIRSTLINE" "the head of an oversized log is dropped"
BODY_BYTES=$(printf '%s' "$OUT" | wc -c)
if [ "$BODY_BYTES" -lt 6000 ]; then
    green "ok: the whole dry-run body stays under 6000 bytes ($BODY_BYTES)"
else
    red "FAIL: dry-run body is $BODY_BYTES bytes; excerpt bounding leaked"
    FAILURES=$((FAILURES + 1))
fi

echo
echo "== a log containing a markdown fence =="
# A bare ``` in the excerpt would close the fence early and render the
# rest of the log as markdown, so the fence has to grow past it.
#
# The backticks below are the literal bytes the fixture needs, not
# command substitution -- which is exactly what shellcheck cannot tell
# from the outside, so it is told. Same disable, and same reason, as
# the body printfs in report-fuzz-failure.sh.
# shellcheck disable=SC2016
printf 'note: the doc comment reads\n```\nlet x = 1;\n```\ndone\n' \
    > "$WORK/fenced.log"
run_reporter fuzz_fenced "$WORK/fenced.log" --dry-run
assert_status 0 "a fenced log reports"
assert_contains '````' 'the fence grows past a triple backtick in the log'

echo
echo "== invalid UTF-8 and NUL bytes =="
printf 'before\n\xff\xfe\x00bad\nafter\n' > "$WORK/binary.log"
run_reporter fuzz_binary "$WORK/binary.log" --dry-run
assert_status 0 "a log with invalid UTF-8 and NULs reports"
assert_contains "after" "the readable part of a binary log survives"
# NUL stripping is deliberately not asserted here: `OUT="$(...)"`
# drops NUL bytes on capture, so any such assertion would pass
# whether or not the reporter's `tr -d` ran, which is worse than no
# assertion at all.
#
# The reporter degrades to `cat` when iconv is absent, by design, so
# asserting the scrub unconditionally would fail the reporter for
# behaving as documented -- and because this test runs from a
# pre-commit hook, that would block an unrelated commit on a machine
# without iconv. Debian carries it in libc-bin, so CI always takes the
# first branch; a minimal container or a stripped macOS may not. The
# NUL assertion above stays unconditional, since `tr -d` needs no
# iconv.
if command -v iconv >/dev/null 2>&1; then
    if printf '%s' "$OUT" | grep -q $'\xff'; then
        red "FAIL: an invalid UTF-8 byte reached the body"
        FAILURES=$((FAILURES + 1))
    else
        green "ok: invalid UTF-8 is scrubbed from the body"
    fi
else
    green "skip: no iconv, scrubbing degrades to cat by design"
fi

echo
echo "== --run-failure =="
WORKFLOW_URL="https://example.invalid/run/2" \
    run_reporter --run-failure --dry-run
assert_status 0 "--run-failure reports"
assert_contains "Nightly fuzz run failed before reaching the targets" \
    "--run-failure has its own title"
assert_contains "https://example.invalid/run/2" \
    "--run-failure records the run URL"
assert_absent "make fuzz-build-" \
    "--run-failure names no per-target reproduce command"

echo
echo "== --no-artifact =="
WORKFLOW_URL="https://example.invalid/run/3" \
    run_reporter --no-artifact --dry-run
assert_status 0 "--no-artifact reports"
assert_contains "Nightly fuzz run produced no log artifact" \
    "--no-artifact has its own title"
assert_contains "https://example.invalid/run/3" \
    "--no-artifact records the run URL"
assert_absent "make fuzz-build-" \
    "--no-artifact names no per-target reproduce command"
# The two run-level modes exist to be told apart, so neither may
# borrow the other's title -- a shared title would dedup a spell of
# missing artifacts onto a real early failure and bury it.
assert_absent "failed before reaching the targets" \
    "--no-artifact does not claim the run died before the targets"

echo
echo "== dedup and recurrence, against a stubbed gh =="
# Everything above runs through --dry-run, which returns before the
# reporter talks to GitHub at all. That leaves its two most fragile
# pieces untested: the `gh issue list` search qualifier and --json
# field set, and the jq expression that picks an exact title match out
# of the result. Both are things a `gh` or API change breaks silently
# -- a lookup that starts returning nothing does not error, it just
# files a fresh issue every night, which reads as noise rather than as
# this script being broken. So drive them with a stub `gh` instead.
GH_STUB="$WORK/fake-gh"
cat > "$GH_STUB" <<'STUB'
#!/bin/bash
# Records its argv and answers `issue list` from $GH_LIST_JSON.
printf '%s\n' "$*" >> "$GH_CALLS"
if [ "${1:-}" = issue ] && [ "${2:-}" = list ]; then
    cat "$GH_LIST_JSON"
fi
exit 0
STUB
chmod +x "$GH_STUB"

# jq is the reporter's dependency, not this test's, but the lookup
# cannot be exercised without it. Debian and the CI images carry it;
# skip rather than fail where it is absent, on the same reasoning as
# the iconv branch above.
if ! command -v jq >/dev/null 2>&1; then
    green "skip: no jq, the dedup lookup cannot be exercised"
else
    run_stubbed_reporter() {
        local list_json="$1"; shift
        printf '%s' "$list_json" > "$WORK/gh-list.json"
        : > "$WORK/gh-calls"
        GH="$GH_STUB" GH_CALLS="$WORK/gh-calls" \
            GH_LIST_JSON="$WORK/gh-list.json" \
            run_reporter "$@"
        CALLS="$(cat "$WORK/gh-calls")"
    }

    assert_called() {
        local needle="$1" what="$2"
        if [[ "$CALLS" == *"$needle"* ]]; then
            green "ok: $what"
        else
            red "FAIL: $what: gh was not called with '$needle'"
            red "      calls were: $CALLS"
            FAILURES=$((FAILURES + 1))
        fi
    }

    assert_not_called() {
        local needle="$1" what="$2"
        if [[ "$CALLS" != *"$needle"* ]]; then
            green "ok: $what"
        else
            red "FAIL: $what: gh was unexpectedly called with '$needle'"
            FAILURES=$((FAILURES + 1))
        fi
    }

    # An open issue whose title matches exactly: comment, do not file.
    run_stubbed_reporter \
        '[{"number":42,"title":"Nightly fuzz failure: fuzz_dedup"}]' \
        fuzz_dedup "$WORK/normal.log"
    assert_status 0 "a recurrence reports"
    assert_contains "already tracked by issue #42" \
        "an open issue with the same title is found"
    assert_called "issue comment 42" "a recurrence comments on the issue"
    assert_not_called "issue create" "a recurrence files no duplicate"

    # Nothing open: file.
    run_stubbed_reporter '[]' fuzz_new "$WORK/normal.log"
    assert_status 0 "a first failure reports"
    assert_contains "filing a new issue" "an empty lookup falls through to filing"
    assert_called "issue create" "a first failure files an issue"
    assert_not_called "issue comment" "a first failure comments on nothing"

    # A near miss must not be treated as a hit. `in:title` is a
    # substring search, so the qualifier alone would match the issue
    # for a target whose name is a prefix of another target's -- the
    # jq exact-title select is what stops that, and this is the case
    # that fails if the select is ever dropped or loosened.
    run_stubbed_reporter \
        '[{"number":7,"title":"Nightly fuzz failure: fuzz_dedup_extended"}]' \
        fuzz_dedup "$WORK/normal.log"
    assert_status 0 "a near-miss title reports"
    assert_called "issue create" "a title that merely contains ours is not a hit"
    assert_not_called "issue comment" "a near miss does not comment on the wrong issue"

    # A lookup that returns junk must fall through to filing rather
    # than dying: a duplicate issue is a far smaller problem than a
    # failure nobody hears about.
    run_stubbed_reporter 'not json at all' fuzz_junk "$WORK/normal.log"
    assert_status 0 "a broken lookup is not fatal"
    assert_called "issue create" "a broken lookup still files the failure"

    # The run-level modes go through the same lookup, keyed on their
    # own titles.
    run_stubbed_reporter \
        '[{"number":9,"title":"Nightly fuzz run produced no log artifact"}]' \
        --no-artifact
    assert_status 0 "--no-artifact dedups too"
    assert_called "issue comment 9" "--no-artifact comments on its own issue"
fi

echo
echo "== argument contract =="
run_reporter --dry-run
assert_status 2 "no positional arguments is a usage error"
run_reporter fuzz_only_one --dry-run
assert_status 2 "one positional argument is a usage error"
run_reporter a b c --dry-run
assert_status 2 "three positional arguments is a usage error"
run_reporter --run-failure a b --dry-run
assert_status 2 "--run-failure with positionals is a usage error"
run_reporter --no-artifact a b --dry-run
assert_status 2 "--no-artifact with positionals is a usage error"
run_reporter --run-failure --no-artifact --dry-run
assert_status 2 "the two run-level modes together are a usage error"
run_reporter --no-such-flag a b
assert_status 2 "an unknown flag is a usage error"

# --dry-run is position-independent: the workflow appends it, a
# developer types it first.
run_reporter --dry-run fuzz_first "$WORK/normal.log"
assert_status 0 "--dry-run is accepted before the positionals"
assert_contains "Nightly fuzz failure: fuzz_first" \
    "--dry-run first still parses the target"

echo
if [ "$FAILURES" -eq 0 ]; then
    green "test-report-fuzz-failure: all assertions held."
    exit 0
fi
red "test-report-fuzz-failure: $FAILURES assertion(s) failed."
exit 1

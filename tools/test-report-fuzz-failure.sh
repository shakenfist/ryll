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

# Two assertions below depend on tools this test does not itself need --
# iconv for the UTF-8 scrub, jq for the dedup lookup -- and skipping them
# is right on a developer's machine, where this runs from a pre-commit
# hook and must not block an unrelated commit. It is wrong in CI: an
# image change that dropped jq would silently remove the most valuable
# coverage in this file while the check stayed green, which is the same
# silent-coverage-loss failure the fuzz lane itself is built to close.
# So ci.yml sets FUZZ_REPORTER_TEST_STRICT=1 and a missing tool fails.
STRICT="${FUZZ_REPORTER_TEST_STRICT:-}"

# Skip, or fail if we are being strict about it. Returns 0 when the
# caller should go on to skip the block.
skip_or_fail() {
    local what="$1"
    if [ -n "$STRICT" ]; then
        red "FAIL: $what (FUZZ_REPORTER_TEST_STRICT is set)"
        FAILURES=$((FAILURES + 1))
    else
        green "skip: $what"
    fi
}

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
# The reproducer pointer interpolates the target into a path. It is the
# one %s in this body that is not adjacent to its printf argument in
# the source, which is exactly how it came out as `artifacts//` once.
assert_contains "artifacts/fuzz_link_mess_parse/" \
    "the reproducer pointer names the target's artifact directory"

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
# first branch; a minimal container or a stripped macOS may not. Only
# the UTF-8 scrub is conditional: NUL stripping is not asserted at all,
# for the reason above.
if command -v iconv >/dev/null 2>&1; then
    if printf '%s' "$OUT" | grep -q $'\xff'; then
        red "FAIL: an invalid UTF-8 byte reached the body"
        FAILURES=$((FAILURES + 1))
    else
        green "ok: invalid UTF-8 is scrubbed from the body"
    fi
else
    skip_or_fail "no iconv, so the UTF-8 scrub cannot be asserted"
fi

echo
echo "== --run-failure =="
WORKFLOW_URL="https://example.invalid/run/2" \
    run_reporter --run-failure --dry-run
assert_status 0 "--run-failure reports"
assert_contains "Nightly fuzz run failed outside the fuzz targets" \
    "--run-failure has its own title"
assert_contains "https://example.invalid/run/2" \
    "--run-failure records the run URL"
assert_absent "make fuzz-build-" \
    "--run-failure names no per-target reproduce command"
# The body sends a human to a step, so it has to name every step that
# can produce this mode. The extractor is the one most likely to: it
# aborts the target step on a manifest it will not read, and it is the
# one this mode's body used not to mention at all.
assert_contains "tools/fuzz-targets.sh" \
    "--run-failure names the target list extraction as a cause"
assert_contains "fuzz-devcontainer" \
    "--run-failure names the devcontainer build as a cause"
assert_contains "cut short" \
    "--run-failure names a loop cut short as a cause"
assert_contains "names the step that failed" \
    "--run-failure sends the reader to the run log"

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
assert_absent "failed outside the fuzz targets" \
    "--no-artifact does not borrow the run-failure title"

echo
echo "== --fmt-failure =="
printf 'Diff in /workspace/x/parse.rs:12:\n-    let x=1;\n+    let x = 1;\n' \
    > "$WORK/fmt.log"
WORKFLOW_URL="https://example.invalid/run/4" \
    run_reporter --fmt-failure "$WORK/fmt.log" --dry-run
assert_status 0 "--fmt-failure reports"
assert_contains "the fuzz workspace is misformatted" \
    "--fmt-failure has its own title"
assert_contains "https://example.invalid/run/4" \
    "--fmt-failure records the run URL"
# Unlike the other two run-level modes it does excerpt a log, because
# `cargo fmt --check` prints the diff it wants and that diff is the fix.
assert_contains "let x = 1;" "--fmt-failure excerpts the format diff"
assert_contains "make fuzz-fmt" "--fmt-failure names the fixing target"
assert_absent "make fuzz-build-" \
    "--fmt-failure names no per-target reproduce command"
# Each of the three run-level modes must keep its own title: a shared
# one would dedup them onto each other and bury the rarer diagnosis.
assert_absent "failed outside the fuzz targets" \
    "--fmt-failure does not borrow the run-failure title"
assert_absent "no log artifact" \
    "--fmt-failure does not claim the artifact went missing"

# A format check whose log did not survive still has to file: the title
# and the run URL are the load-bearing parts.
run_reporter --fmt-failure "$WORK/does-not-exist.log" --dry-run
assert_status 0 "--fmt-failure survives a missing log"
assert_contains "the fuzz workspace is misformatted" \
    "--fmt-failure still files without a log excerpt"

echo
echo "== the lifecycle footer =="
# Dedup keys on an *open* issue with a matching title, so an issue
# left open after the fix quietly downgrades every later failure of
# the same thing to a comment on a thread people have stopped reading.
# The instruction to close it therefore has to reach the person who
# fixes the failure, which means it has to be in every body.
printf 'diff\n' > "$WORK/fmt-footer.log"
for MODE_ARGS in "fuzz_footer $WORK/normal.log" "--run-failure" \
        "--no-artifact" "--fmt-failure $WORK/fmt-footer.log"; do
    # Deliberately unquoted: each entry is an argv, not one argument.
    # shellcheck disable=SC2086
    run_reporter $MODE_ARGS --dry-run
    assert_contains "Close this issue once the failure is fixed" \
        "$MODE_ARGS carries the close-me instruction"
done

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
# Records its argv and answers `issue list` from $GH_LIST_JSON. A
# non-zero $GH_LIST_STATUS makes the lookup itself fail, which the
# reporter has to tell apart from a lookup that succeeded and found
# nothing: both leave it with no issue number, but only one of them
# means the reporter is broken.
printf '%s\n' "$*" >> "$GH_CALLS"
if [ "${1:-}" = issue ] && [ "${2:-}" = list ]; then
    cat "$GH_LIST_JSON"
    exit "${GH_LIST_STATUS:-0}"
fi
exit 0
STUB
chmod +x "$GH_STUB"

# jq is the reporter's dependency, not this test's, but the lookup
# cannot be exercised without it. Debian and the CI images carry it;
# skip rather than fail where it is absent, on the same reasoning as
# the iconv branch above.
if ! command -v jq >/dev/null 2>&1; then
    skip_or_fail "no jq, so the dedup lookup cannot be exercised"
else
    run_stubbed_reporter() {
        local list_json="$1"; shift
        printf '%s' "$list_json" > "$WORK/gh-list.json"
        : > "$WORK/gh-calls"
        GH="$GH_STUB" GH_CALLS="$WORK/gh-calls" \
            GH_LIST_JSON="$WORK/gh-list.json" \
            GH_LIST_STATUS="${GH_LIST_STATUS:-0}" \
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
    # failure nobody hears about. It must also *say* so. Falling
    # through silently makes a permanently broken lookup look exactly
    # like a run of genuine first failures -- one fresh issue a night,
    # which reads as ordinary nightly noise -- and that is the whole
    # reason this file stubs `gh` at all.
    run_stubbed_reporter 'not json at all' fuzz_junk "$WORK/normal.log"
    assert_status 0 "a broken lookup is not fatal"
    assert_called "issue create" "a broken lookup still files the failure"
    assert_contains "::warning::" "a broken lookup warns rather than passing silently"
    assert_contains "jq could not read" "the warning names what went wrong"

    # The other half of the same silence: `gh` itself failing (rate
    # limit, auth, an API change) rather than returning something
    # unparseable. It produces the same empty answer as a genuine
    # first failure, so only the warning tells them apart.
    GH_LIST_STATUS=1 run_stubbed_reporter '' fuzz_ghfail "$WORK/normal.log"
    assert_status 0 "a failing gh lookup is not fatal"
    assert_called "issue create" "a failing gh lookup still files the failure"
    assert_contains "::warning::" "a failing gh lookup warns"
    assert_contains "dedup lookup failed" "the warning names the lookup"

    # A hit must not warn: a warning on the ordinary path would train
    # the reader to ignore the one that matters.
    run_stubbed_reporter \
        '[{"number":13,"title":"Nightly fuzz failure: fuzz_quiet"}]' \
        fuzz_quiet "$WORK/normal.log"
    assert_status 0 "a working lookup reports"
    assert_absent "::warning::" "a working lookup warns about nothing"

    # The run-level modes go through the same lookup, keyed on their
    # own titles.
    run_stubbed_reporter \
        '[{"number":9,"title":"Nightly fuzz run produced no log artifact"}]' \
        --no-artifact
    assert_status 0 "--no-artifact dedups too"
    assert_called "issue comment 9" "--no-artifact comments on its own issue"

    printf 'diff\n' > "$WORK/fmt-dedup.log"
    run_stubbed_reporter \
        '[{"number":11,"title":"Nightly fuzz: the fuzz workspace is misformatted"}]' \
        --fmt-failure "$WORK/fmt-dedup.log"
    assert_status 0 "--fmt-failure dedups too"
    assert_called "issue comment 11" "--fmt-failure comments on its own issue"
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
run_reporter --fmt-failure --dry-run
assert_status 2 "--fmt-failure with no log is a usage error"
run_reporter --fmt-failure a b --dry-run
assert_status 2 "--fmt-failure with two positionals is a usage error"
run_reporter --run-failure --no-artifact --dry-run
assert_status 2 "two run-level modes together are a usage error"
run_reporter --fmt-failure --run-failure x --dry-run
assert_status 2 "--fmt-failure with another run-level mode is a usage error"
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

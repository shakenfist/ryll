#!/usr/bin/env bash
# test-report-fuzz-run.sh — smoke test for tools/report-fuzz-run.sh.
#
# The marker walk decides what the nightly fuzz lane reports at all.
# Its failure mode is silence: a walk that reports nothing is
# indistinguishable, from the outside, from a nightly with nothing to
# report -- the same argument that pinned tools/report-fuzz-failure.sh
# and tools/fuzz-targets.sh with tests, and the same convention.
#
# The invariant under test is that the report job never finishes
# having reported nothing, because it only runs when the fuzz job did
# not succeed. Every case below is a marker directory fixture, and the
# assertions are about which reporter invocations happened -- so they
# cannot pass by agreeing with the walk about what a marker directory
# looks like.
#
# The reporter itself is stubbed through $REPORTER: what matters here
# is the argv the walk chooses, not the issue body, which
# tools/test-report-fuzz-failure.sh already covers. So no network, no
# GH_TOKEN and no `gh`. Runs in well under a second.
#
# Usage: tools/test-report-fuzz-run.sh
# Exit code: 0 all assertions held, 1 otherwise.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WALKER="$SCRIPT_DIR/report-fuzz-run.sh"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILURES=0
red() { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }

# The stub records one line of argv per invocation, and fails for any
# target named in FAIL_FOR so the "could not be reported" path can be
# driven without breaking the others.
STUB="$WORK/stub-reporter.sh"
cat > "$STUB" <<'STUB_EOF'
#!/bin/bash
echo "$*" >> "${STUB_CALLS}"
for BAD in ${FAIL_FOR:-}; do
    if [ "${1:-}" = "${BAD}" ]; then
        exit 1
    fi
done
exit 0
STUB_EOF
chmod +x "$STUB"

CALLS=""
OUT=""
STATUS=0
SUMMARY=""

# Build a marker directory. Arguments are marker specs:
#   target:NAME   a failed fuzz target
#   fmt           a failed format check
#   ran           the target loop completed
#   info          the run marker (i.e. the artifact arrived)
markers() {
    local dir="$1"; shift
    rm -rf "$dir"
    mkdir -p "$dir"
    local spec
    for spec in "$@"; do
        case "$spec" in
            target:*)
                local name="${spec#target:}"
                echo "failed" > "$dir/$name.failed"
                echo "log for $name" > "$dir/$name.log"
                ;;
            fmt)
                mkdir -p "$dir/fmt"
                echo "misformatted" > "$dir/fmt/check.failed"
                echo "the diff" > "$dir/fmt/check.log"
                ;;
            ran) echo "fuzzed 4 target(s)" > "$dir/targets-ran.txt" ;;
            info) echo "run: https://example.invalid/run/1" > "$dir/run-info.txt" ;;
            *) red "test bug: unknown marker spec '$spec'"; exit 1 ;;
        esac
    done
}

run_walker() {
    CALLS="$WORK/calls.$RANDOM"
    : > "$CALLS"
    SUMMARY="$WORK/summary.$RANDOM"
    : > "$SUMMARY"
    OUT="$(REPORTER="$STUB" STUB_CALLS="$CALLS" \
        GITHUB_STEP_SUMMARY="$SUMMARY" "$WALKER" "$@" 2>&1)"
    STATUS=$?
}

assert_status() {
    local want="$1" what="$2"
    if [ "$STATUS" -eq "$want" ]; then
        green "ok: $what (exit $want)"
    else
        red "FAIL: $what: expected exit $want, got $STATUS"
        red "  output: $OUT"
        FAILURES=$((FAILURES + 1))
    fi
}

# An exact-line match on the recorded argv, not a substring: `--dry-run`
# matching inside `--dry-run-something` is the kind of near miss that
# makes an assertion pass for the wrong reason.
assert_called() {
    local want="$1" what="$2"
    if grep -Fxq -- "$want" "$CALLS"; then
        green "ok: $what"
    else
        red "FAIL: $what: '$want' not among the reporter calls:"
        sed 's/^/    /' "$CALLS" >&2
        FAILURES=$((FAILURES + 1))
    fi
}

assert_not_called() {
    local want="$1" what="$2"
    if grep -Fxq -- "$want" "$CALLS"; then
        red "FAIL: $what: '$want' was among the reporter calls"
        FAILURES=$((FAILURES + 1))
    else
        green "ok: $what"
    fi
}

assert_call_count() {
    local want="$1" what="$2" got
    got="$(grep -c . "$CALLS" || true)"
    if [ "$got" -eq "$want" ]; then
        green "ok: $what ($want call(s))"
    else
        red "FAIL: $what: expected $want reporter call(s), got $got:"
        sed 's/^/    /' "$CALLS" >&2
        FAILURES=$((FAILURES + 1))
    fi
}

assert_summary_contains() {
    local needle="$1" what="$2"
    if grep -Fq -- "$needle" "$SUMMARY"; then
        green "ok: $what"
    else
        red "FAIL: $what: the step summary does not contain '$needle':"
        sed 's/^/    /' "$SUMMARY" >&2
        FAILURES=$((FAILURES + 1))
    fi
}

echo "== failing targets =="
markers "$WORK/targets" ran info target:fuzz_alpha target:fuzz_bravo
run_walker "$WORK/targets"
assert_status 0 "two failing targets report"
assert_called "fuzz_alpha $WORK/targets/fuzz_alpha.log" "the first target is reported with its log"
assert_called "fuzz_bravo $WORK/targets/fuzz_bravo.log" "the second target is reported with its log"
assert_call_count 2 "a completed loop with failures files nothing else"
assert_summary_contains "targets that failed: 2" "the summary counts the targets"

echo
echo "== the format check alone =="
markers "$WORK/fmt" ran info fmt
run_walker "$WORK/fmt"
assert_status 0 "a format failure reports"
assert_called "--fmt-failure $WORK/fmt/fmt/check.log" "the format check gets its own mode"
assert_not_called "--run-failure" "a completed loop is not called a run failure"
assert_call_count 1 "nothing else is filed"
assert_summary_contains "misformatted" "the summary names the format failure"

echo
echo "== a format failure on a night the target loop never ran =="
# The regression this file was written for. Passing targets write no
# marker, so keying the run-level branch on "no other markers" meant a
# format failure suppressed it: the formatting issue got filed and the
# fact that no target was fuzzed at all was never reported. Both have
# to be filed.
markers "$WORK/both" info fmt
run_walker "$WORK/both"
assert_status 0 "a format failure with no completed loop reports"
assert_called "--fmt-failure $WORK/both/fmt/check.log" "the format check is still filed"
assert_called "--run-failure" "the loop that never ran is filed too"
assert_call_count 2 "both, and only both"
assert_summary_contains "the target loop did not complete" \
    "the summary says the loop did not complete"

echo
echo "== a failing target on a night the loop was cut short =="
# Same shape from the other side: markers exist, so the old branch
# would have stayed quiet about the loop never finishing.
markers "$WORK/cutshort" info target:fuzz_alpha
run_walker "$WORK/cutshort"
assert_status 0 "a cut-short loop with a failing target reports"
assert_called "fuzz_alpha $WORK/cutshort/fuzz_alpha.log" "the target is still reported"
assert_called "--run-failure" "the incomplete loop is reported as well"

echo
echo "== the run died before the targets =="
markers "$WORK/early" info
run_walker "$WORK/early"
assert_status 0 "an early death reports"
assert_called "--run-failure" "an early death is a run failure"
assert_not_called "--no-artifact" "an early death is not a missing artifact"
assert_call_count 1 "exactly one issue for the run"

echo
echo "== the artifact never arrived =="
markers "$WORK/gone"
run_walker "$WORK/gone"
assert_status 0 "an empty marker directory reports"
assert_called "--no-artifact" "a missing run marker is a missing artifact"
assert_not_called "--run-failure" "a missing artifact is not called an early death"
assert_summary_contains "artifact did not arrive" "the summary says the artifact is missing"

# The download is continue-on-error, so the directory may not exist at
# all rather than merely being empty. That must report, not crash.
rm -rf "$WORK/never-made"
run_walker "$WORK/never-made"
assert_status 0 "a marker directory that does not exist reports"
assert_called "--no-artifact" "a missing directory is a missing artifact"

echo
echo "== everything passed, and the job still failed =="
# The report job only runs when the fuzz job did not succeed. A
# complete loop, no failures and clean formatting therefore means
# something outside anything the markers describe went wrong -- the
# artifact upload, say. Reporting nothing here would be the same
# silence in a different place.
markers "$WORK/clean" ran info
run_walker "$WORK/clean"
assert_status 0 "a clean marker set still reports"
assert_called "--run-failure" "a failure the markers do not explain is still filed"
assert_summary_contains "account for nothing that failed" \
    "the summary says the markers explain nothing"

echo
echo "== a reporting failure is counted, not thrown =="
markers "$WORK/reportfail" ran info target:fuzz_alpha target:fuzz_bravo
CALLS="$WORK/calls.reportfail"
: > "$CALLS"
SUMMARY="$WORK/summary.reportfail"
: > "$SUMMARY"
OUT="$(REPORTER="$STUB" STUB_CALLS="$CALLS" FAIL_FOR="fuzz_alpha" \
    GITHUB_STEP_SUMMARY="$SUMMARY" "$WALKER" "$WORK/reportfail" 2>&1)"
STATUS=$?
assert_status 1 "a reporting failure fails the job"
assert_called "fuzz_bravo $WORK/reportfail/fuzz_bravo.log" \
    "the target after the failed report is still reported"
assert_summary_contains "could not be reported: 1" "the summary counts it"

echo
echo "== usage =="
run_walker
assert_status 2 "no arguments is a usage error"
run_walker a b
assert_status 2 "two arguments is a usage error"

echo
if [ "$FAILURES" -eq 0 ]; then
    green "test-report-fuzz-run: all assertions held."
    exit 0
fi
red "test-report-fuzz-run: $FAILURES assertion(s) failed."
exit 1

#!/bin/bash
#
# Walk the marker files the nightly fuzz job left behind and report
# every failure they describe, plus any failure they do not.
#
# This is the part of the nightly fuzz lane that decides *what* gets
# reported; tools/report-fuzz-failure.sh decides what an issue says.
# It lives in a script for the same reason its two siblings do:
# nothing in this repository lints or tests a workflow `run:` block --
# tools/run-shellcheck.sh globs scripts/ and tools/, and no job
# invokes actionlint -- and this block's failure mode is silence. A
# report job that walks the markers wrongly is indistinguishable, from
# the outside, from a nightly with nothing to report. See
# tools/test-report-fuzz-run.sh.
#
# The invariant it exists to hold: the report job only runs when the
# fuzz job did not succeed, so it must never finish having reported
# nothing. Three kinds of evidence, in the order they are trusted:
#
#   LOG_DIR/<target>.failed   one fuzz target failed to build or
#                             smoke-run. Reported per target.
#   LOG_DIR/fmt/check.failed  `make fuzz-fmt-check` failed. Its own
#                             mode and its own issue title; it lives
#                             in a subdirectory so the target glob
#                             cannot mistake it for a target.
#   LOG_DIR/targets-ran.txt   written by the fuzz job *after* the
#                             target loop completes.
#
# The last of those is the one that makes the invariant hold. A
# passing target writes no marker, so an empty marker set is ambiguous
# between "every target passed" and "the target loop never ran" -- and
# the loop does abort, with `exit 1`, when tools/fuzz-targets.sh
# rejects the manifest. Keying the run-level branch on the absence of
# a positive completion marker says which of those happened directly,
# rather than inferring it from the absence of other markers, which is
# a signal any second failure on the same night can mask.
#
# Usage:
#   tools/report-fuzz-run.sh LOG_DIR
#
# Inputs (environment):
#   GH_TOKEN     (required) for `gh`, via the reporter.
#   WORKFLOW_URL (optional) run URL, passed through to the reporter.
#   REPORTER     (optional) the reporter command, for tests to stub.
#   GITHUB_STEP_SUMMARY (optional) step summary file to append to.
#
# Exits 0 when everything that needed reporting was reported, 1 when
# any report failed, and 2 on a usage error.

set -euo pipefail

usage() {
    echo "usage: $0 LOG_DIR" >&2
    exit 2
}

if [ "$#" -ne 1 ]; then
    usage
fi

LOG_DIR="$1"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# A seam for the same reason report-fuzz-failure.sh takes `gh` from
# $GH: the interesting behaviour here is which reporter invocations
# happen, and a test that had to reach GitHub to observe that could
# not run in pre-commit.
REPORTER="${REPORTER:-${SCRIPT_DIR}/report-fuzz-failure.sh}"

shopt -s nullglob

FAILURES=0
REPORT_FAILURES=0

# A failure to report must not stop the walk: the other failures still
# have to be reported. They are counted and turned into a job failure
# at the end, so a nightly that could not tell anyone about a broken
# target is never silently green -- but one target nobody could file
# about does not stop the other three being filed.
report() {
    if "${REPORTER}" "$@"; then
        return 0
    fi
    echo "::warning::could not file an issue: $*"
    REPORT_FAILURES=$((REPORT_FAILURES + 1))
}

for MARKER in "${LOG_DIR}"/*.failed; do
    TARGET="$(basename "${MARKER}" .failed)"
    FAILURES=$((FAILURES + 1))
    report "${TARGET}" "${LOG_DIR}/${TARGET}.log"
done

# The format check is not a target and must not be reported as one. It
# does not count towards FAILURES either, because it says nothing
# about whether the targets ran: that is what targets-ran.txt is for.
FMT_FAILED=0
if [ -f "${LOG_DIR}/fmt/check.failed" ]; then
    FMT_FAILED=1
    report --fmt-failure "${LOG_DIR}/fmt/check.log"
fi

# Two ways the markers can fail to account for a job that did not
# succeed, and both file a run-level issue.
#
# The loop did not complete: no targets-ran.txt. The job died before
# the target step (the cargo cache, the devcontainer build, or
# checkout -- though checkout is ahead of the run marker, so that one
# arrives as --no-artifact below),
# the target step aborted on a manifest tools/fuzz-targets.sh would
# not read, or the loop was cut short part way through. Note that this
# is checked whatever else was found -- a format-check failure on the
# same night must not suppress it, which is precisely the hole that
# keying this branch on "no other markers" left open.
#
# Or the loop completed, every target passed, the formatting was
# clean, and the job still failed: something outside everything the
# markers describe went wrong, such as the artifact upload. Reporting
# nothing at all there would be the same silence in a different place.
RUN_LEVEL=0
RUN_LEVEL_WHY=""
if [ ! -f "${LOG_DIR}/targets-ran.txt" ]; then
    RUN_LEVEL=1
    RUN_LEVEL_WHY="the target loop did not complete"
elif [ "${FAILURES}" -eq 0 ] && [ "${FMT_FAILED}" -eq 0 ]; then
    RUN_LEVEL=1
    RUN_LEVEL_WHY="the markers account for nothing that failed"
fi

# run-info.txt tells apart "the job failed and we can see its logs"
# from "the logs never got here". The fuzz job writes it immediately
# after checkout, ahead of everything else that can fail, so its
# presence means the artifact round-tripped; its absence means either
# the download was empty or checkout itself failed, and in both cases
# the fuzz job's own failure is still unread. The two carry different
# issue titles, so a spell of missing artifacts cannot dedup on top of
# a genuine run failure and bury it.
RUN_MODE=""
if [ "${RUN_LEVEL}" -eq 1 ]; then
    if [ -f "${LOG_DIR}/run-info.txt" ]; then
        RUN_MODE=--run-failure
    else
        RUN_MODE=--no-artifact
        RUN_LEVEL_WHY="the fuzz-logs artifact did not arrive"
    fi
    echo "::warning::${RUN_LEVEL_WHY}; filing ${RUN_MODE}"
    report "${RUN_MODE}"
fi

{
    echo "### Nightly fuzz"
    echo ""
    echo "- targets that failed: ${FAILURES}"
    if [ "${FMT_FAILED}" -eq 1 ]; then
        echo "- the fuzz workspace is misformatted"
    fi
    if [ "${RUN_LEVEL}" -eq 1 ]; then
        echo "- ${RUN_LEVEL_WHY}"
    fi
    echo "- could not be reported: ${REPORT_FAILURES}"
} >> "${GITHUB_STEP_SUMMARY:-/dev/null}"

if [ "${REPORT_FAILURES}" -ne 0 ]; then
    echo "::error::${REPORT_FAILURES} failure(s) could not be reported"
    exit 1
fi

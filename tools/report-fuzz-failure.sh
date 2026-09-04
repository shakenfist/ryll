#!/bin/bash
#
# File a GitHub issue describing a fuzz target that failed in the
# nightly run.
#
# The nightly fuzz workflow has no red merge queue to speak through.
# GitHub's only notification for a failed scheduled run is an email to
# whoever pushed last, which at 12:00 UTC is nobody's inbox in
# particular, and the run's result is a mark on the Actions tab that
# nobody is looking at. So the workflow files an issue instead, and
# this is what does it. See docs/ci.md and the criterion this
# implements, shakenfist/development's
# docs/audits/fuzz-nightly-reporting.md.
#
# This is not instar's reporter. instar runs a real coverage-guided
# campaign over forty targets and reports crashes, so it minimizes the
# crashing input and dedups on a normalized panic signature. Ryll's
# fuzz lane is a build-and-doesn't-panic gate: a failure here is
# almost always "the target stopped compiling", the useful evidence is
# the tail of the build log, and the target name alone is a good
# enough dedup key. What is copied deliberately is the shape --
# bounded excerpts, a file rather than argv, comment-don't-duplicate
# on recurrence, and a caller that treats a reporting failure as a
# warning so the remaining targets still run.
#
# Usage:
#   tools/report-fuzz-failure.sh TARGET LOG_FILE [--dry-run]
#
# Inputs (environment):
#   GH_TOKEN     (required unless --dry-run) for `gh`.
#   WORKFLOW_URL (optional) run URL recorded in the issue.

set -euo pipefail

usage() {
    echo "usage: $0 TARGET LOG_FILE [--dry-run]" >&2
    exit 2
}

DRY_RUN=0
POSITIONAL=()
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        -*) usage ;;
        *) POSITIONAL+=("$1") ;;
    esac
    shift
done

if [ "${#POSITIONAL[@]}" -ne 2 ]; then
    usage
fi
TARGET="${POSITIONAL[0]}"
LOG_FILE="${POSITIONAL[1]}"

# Bounded in bytes, and each line bounded too. A fuzz log carries raw
# mutated bytes: one line can be enormous, and a byte-wise slice can
# cut a multi-byte character in half, which the GitHub API rejects.
MAX_EXCERPT_BYTES="${MAX_EXCERPT_BYTES:-4000}"
MAX_LINE_BYTES="${MAX_LINE_BYTES:-200}"

# Decide the scrubber once rather than writing `iconv ... || cat` in
# the pipeline: that form runs cat on whatever is left of the pipe
# when iconv exits partway, splicing raw bytes onto the converted
# prefix and defeating the scrub in the case it exists for.
if command -v iconv >/dev/null 2>&1; then
    SCRUB=(iconv -c -f UTF-8 -t UTF-8)
else
    SCRUB=(cat)
fi

EXCERPT_FILE="$(mktemp)"
BODY_FILE="$(mktemp)"
trap 'rm -f "${EXCERPT_FILE}" "${BODY_FILE}"' EXIT

if [ -f "${LOG_FILE}" ]; then
    cut -b "1-${MAX_LINE_BYTES}" "${LOG_FILE}" 2>/dev/null \
        | tail -n 40 \
        | tail -c "${MAX_EXCERPT_BYTES}" \
        | tr -d '\000' \
        | "${SCRUB[@]}" 2>/dev/null > "${EXCERPT_FILE}" || true
else
    echo "::warning::${LOG_FILE} not found; reporting ${TARGET}" \
        "without a log excerpt" >&2
fi

TITLE="Nightly fuzz failure: ${TARGET}"

# The single quotes are deliberate throughout: every backtick below
# is markdown -- a code span or a fence -- rather than a command
# substitution, and the %s placeholders are printf's, not the
# shell's.
# shellcheck disable=SC2016
{
    printf 'The nightly fuzz run could not build or smoke-run '
    printf '`%s`.\n\n' "${TARGET}"
    printf 'Run: %s\n\n' "${WORKFLOW_URL:-unknown}"
    printf 'Reproduce locally with:\n\n'
    printf '```\nmake fuzz-build-%s\nmake fuzz-smoke-%s\n```\n\n' \
        "${TARGET}" "${TARGET}"
    printf 'Log tail:\n\n'
    printf '```\n'
    cat "${EXCERPT_FILE}"
    printf '\n```\n\n'
    printf 'Filed automatically by `tools/report-fuzz-failure.sh` '
    printf 'from .github/workflows/fuzz.yml.\n'
} > "${BODY_FILE}"

if [ "${DRY_RUN}" -eq 1 ]; then
    echo "--dry-run: would file an issue titled '${TITLE}'"
    cat "${BODY_FILE}"
    exit 0
fi

# Comment on the open issue for this target rather than filing a
# duplicate. A target that stops compiling stays broken until someone
# fixes it, so without this the nightly files one issue per target per
# night. A lookup that fails falls through to filing: a duplicate
# issue is a much smaller problem than a failure nobody hears about.
EXISTING="$(gh issue list \
    --state open \
    --search "in:title \"${TITLE}\"" \
    --json number,title \
    --limit 50 2>/dev/null \
    | jq -r --arg title "${TITLE}" \
        'map(select(.title == $title)) | .[0].number // empty' \
    2>/dev/null || true)"

if [ -n "${EXISTING}" ]; then
    echo "${TARGET} already tracked by issue #${EXISTING}; commenting"
    gh issue comment "${EXISTING}" \
        --body "Failed again in ${WORKFLOW_URL:-this run}."
    exit 0
fi

gh issue create --title "${TITLE}" --body-file "${BODY_FILE}"

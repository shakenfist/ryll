#!/bin/bash
#
# File a GitHub issue describing a fuzz target that failed in the
# nightly run, or a nightly run that failed before it reached any
# target.
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
#   tools/report-fuzz-failure.sh --run-failure [--dry-run]
#
# The second form is for a run that died before the target loop --
# checkout, the cargo cache, the fuzz devcontainer build, or
# `make fuzz-fmt-check`. There is no target to name and no per-target
# log to excerpt, but the run still has to reach a human, so it files
# one issue about the run itself.
#
# Inputs (environment):
#   GH_TOKEN     (required unless --dry-run) for `gh`.
#   WORKFLOW_URL (optional) run URL recorded in the issue.

set -euo pipefail

usage() {
    echo "usage: $0 TARGET LOG_FILE [--dry-run]" >&2
    echo "       $0 --run-failure [--dry-run]" >&2
    exit 2
}

DRY_RUN=0
RUN_FAILURE=0
POSITIONAL=()
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --run-failure) RUN_FAILURE=1 ;;
        -*) usage ;;
        *) POSITIONAL+=("$1") ;;
    esac
    shift
done

TARGET=""
LOG_FILE=""
if [ "${RUN_FAILURE}" -eq 1 ]; then
    # --run-failure names no target and reads no log: the evidence is
    # the run log, which is not a file this script can see.
    if [ "${#POSITIONAL[@]}" -ne 0 ]; then
        usage
    fi
else
    if [ "${#POSITIONAL[@]}" -ne 2 ]; then
        usage
    fi
    TARGET="${POSITIONAL[0]}"
    LOG_FILE="${POSITIONAL[1]}"
fi

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

if [ "${RUN_FAILURE}" -eq 0 ]; then
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
fi

# A markdown fence has to be longer than the longest backtick run
# inside it. The excerpt is a build-log tail, so a rustc diagnostic
# quoting a doc comment, or raw mutated bytes echoed by libFuzzer, can
# put ``` in there -- which closes the fence early and renders the
# rest of the excerpt as markdown. CommonMark allows four or more.
FENCE='```'
if [ -s "${EXCERPT_FILE}" ]; then
    # grep exits 1 on the common case of a log with no backticks in
    # it at all, and `set -o pipefail` would make that kill the
    # script, so the whole substitution is guarded.
    LONGEST_RUN="$( { grep -oE '`+' "${EXCERPT_FILE}" 2>/dev/null || true; } \
        | awk '{ if (length($0) > n) { n = length($0) } } END { print n + 0 }')"
    while [ "${#FENCE}" -le "${LONGEST_RUN:-0}" ]; do
        FENCE="${FENCE}\`"
    done
fi

if [ "${RUN_FAILURE}" -eq 1 ]; then
    TITLE="Nightly fuzz run failed before reaching the targets"
else
    TITLE="Nightly fuzz failure: ${TARGET}"
fi

# The single quotes are deliberate throughout: every backtick below
# is markdown -- a code span or a fence -- rather than a command
# substitution, and the %s placeholders are printf's, not the
# shell's.
# shellcheck disable=SC2016
if [ "${RUN_FAILURE}" -eq 1 ]; then
    {
        printf 'The nightly fuzz run failed before it built any fuzz '
        printf 'target, so there is no per-target issue to file. The '
        printf 'failure is in the run itself -- checkout, the cargo '
        printf 'cache, the `fuzz-devcontainer` build, or `make '
        printf 'fuzz-fmt-check`.\n\n'
        printf 'Run: %s\n\n' "${WORKFLOW_URL:-unknown}"
        printf 'Start from the run log. The `fuzz-logs` artifact holds '
        printf 'only the run marker in this case, because no target '
        printf 'ever wrote one.\n\n'
        printf 'Filed automatically by `tools/report-fuzz-failure.sh` '
        printf 'from .github/workflows/fuzz.yml.\n'
    } > "${BODY_FILE}"
else
    {
        printf 'The nightly fuzz run could not build or smoke-run '
        printf '`%s`.\n\n' "${TARGET}"
        printf 'Run: %s\n\n' "${WORKFLOW_URL:-unknown}"
        printf 'Reproduce locally with:\n\n'
        printf '```\nmake fuzz-build-%s\nmake fuzz-smoke-%s\n```\n\n' \
            "${TARGET}" "${TARGET}"
        printf 'Log tail:\n\n'
        printf '%s\n' "${FENCE}"
        cat "${EXCERPT_FILE}"
        printf '\n%s\n\n' "${FENCE}"
        printf 'Filed automatically by `tools/report-fuzz-failure.sh` '
        printf 'from .github/workflows/fuzz.yml.\n'
    } > "${BODY_FILE}"
fi

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
#
# The lookup matches on title alone and not on the label filed below,
# deliberately: a label someone strips during triage would silently
# turn dedup off and start a nightly issue-per-night again.
EXISTING="$(gh issue list \
    --state open \
    --search "in:title \"${TITLE}\"" \
    --json number,title \
    --limit 50 2>/dev/null \
    | jq -r --arg title "${TITLE}" \
        'map(select(.title == $title)) | .[0].number // empty' \
    2>/dev/null || true)"

if [ -n "${EXISTING}" ]; then
    echo "already tracked by issue #${EXISTING}; commenting"
    gh issue comment "${EXISTING}" \
        --body "Failed again in ${WORKFLOW_URL:-this run}."
    exit 0
fi

# Labelled to match the one other place this repository files an issue
# from CI, release.yml's version-mismatch report, so automated issues
# are filterable as a class.
gh issue create --title "${TITLE}" --label "bug" --body-file "${BODY_FILE}"

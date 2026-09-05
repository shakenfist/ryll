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
#   tools/report-fuzz-failure.sh --no-artifact [--dry-run]
#
# The second form is for a run that died before the target loop --
# checkout, the cargo cache, the fuzz devcontainer build, or
# `make fuzz-fmt-check`. There is no target to name and no per-target
# log to excerpt, but the run still has to reach a human, so it files
# one issue about the run itself.
#
# The third form is for the case where the report job could not read
# the fuzz job's logs at all. These two are separate modes rather than
# one because they are different bugs with different first moves, and
# an issue that names the wrong one sends a human to the wrong place:
# "the fuzz job died early" is a build problem in this repository,
# while "the artifact never arrived" is a problem with the upload,
# download or retention of the artifact itself. They also carry
# different titles, so a spell of missing artifacts cannot dedup on
# top of a genuine early failure and hide it.
#
# Inputs (environment):
#   GH_TOKEN     (required unless --dry-run) for `gh`.
#   WORKFLOW_URL (optional) run URL recorded in the issue.
#   GH           (optional) the `gh` command, for tests to stub.

set -euo pipefail

usage() {
    echo "usage: $0 TARGET LOG_FILE [--dry-run]" >&2
    echo "       $0 --run-failure [--dry-run]" >&2
    echo "       $0 --no-artifact [--dry-run]" >&2
    exit 2
}

# The `gh` invocation is a variable so the smoke test can stub it. The
# dedup lookup and the recurrence comment are the parts of this script
# most likely to break on a `gh` or API change -- the search qualifier,
# the --json field set, the jq expression -- and they are also the
# parts --dry-run cannot reach, because a dry run must not talk to
# GitHub. Without a seam they would be the only untested code in the
# one component whose failure mode is silence.
GH="${GH:-gh}"

DRY_RUN=0
MODE=target
RUN_MODES=0
POSITIONAL=()
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --run-failure) MODE=run-failure; RUN_MODES=$((RUN_MODES + 1)) ;;
        --no-artifact) MODE=no-artifact; RUN_MODES=$((RUN_MODES + 1)) ;;
        -*) usage ;;
        *) POSITIONAL+=("$1") ;;
    esac
    shift
done

# The two run-level modes describe mutually exclusive diagnoses, so
# asking for both is a caller bug rather than something to guess at.
if [ "${RUN_MODES}" -gt 1 ]; then
    usage
fi

TARGET=""
LOG_FILE=""
if [ "${MODE}" != target ]; then
    # Neither run-level mode names a target or reads a log: the
    # evidence is the run log, which is not a file this script can see.
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

if [ "${MODE}" = target ]; then
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

case "${MODE}" in
    run-failure) TITLE="Nightly fuzz run failed before reaching the targets" ;;
    no-artifact) TITLE="Nightly fuzz run produced no log artifact" ;;
    *)           TITLE="Nightly fuzz failure: ${TARGET}" ;;
esac

# The single quotes are deliberate throughout: every backtick below
# is markdown -- a code span or a fence -- rather than a command
# substitution, and the %s placeholders are printf's, not the
# shell's.
# shellcheck disable=SC2016
case "${MODE}" in
    run-failure)
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
        ;;
    no-artifact)
        {
            printf 'The nightly fuzz run failed and the report job could '
            printf 'not read its logs: the `fuzz-logs` artifact did not '
            printf 'arrive, so there is no way to tell from here which '
            printf 'targets failed or why.\n\n'
            printf 'Run: %s\n\n' "${WORKFLOW_URL:-unknown}"
            printf 'This is a problem with the artifact rather than with '
            printf 'the fuzz targets: the fuzz job writes '
            printf '`fuzz-logs/run-info.txt` before the first step that '
            printf 'can fail, so an artifact with no run marker in it is '
            printf 'one that never uploaded, never downloaded, or was '
            printf 'truncated in between. Check the upload and download '
            printf 'steps, and whether the job was cut short by its '
            printf '`timeout-minutes` before the `if: always()` upload '
            printf 'could run.\n\n'
            printf 'Start from the run log.\n\n'
            printf 'Filed automatically by `tools/report-fuzz-failure.sh` '
            printf 'from .github/workflows/fuzz.yml.\n'
        } > "${BODY_FILE}"
        ;;
    *)
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
        ;;
esac

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
echo "dedup lookup: ${TITLE}"
EXISTING="$("${GH}" issue list \
    --state open \
    --search "in:title \"${TITLE}\"" \
    --json number,title \
    --limit 50 2>/dev/null \
    | jq -r --arg title "${TITLE}" \
        'map(select(.title == $title)) | .[0].number // empty' \
    2>/dev/null || true)"

if [ -n "${EXISTING}" ]; then
    echo "already tracked by issue #${EXISTING}; commenting"
    "${GH}" issue comment "${EXISTING}" \
        --body "Failed again in ${WORKFLOW_URL:-this run}."
    exit 0
fi

# Labelled to match the one other place this repository files an issue
# from CI, release.yml's version-mismatch report, so automated issues
# are filterable as a class.
echo "filing a new issue: ${TITLE}"
"${GH}" issue create --title "${TITLE}" --label "bug" --body-file "${BODY_FILE}"

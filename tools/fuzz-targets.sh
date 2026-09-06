#!/bin/bash
#
# Print the cargo-fuzz target names declared by a fuzz crate manifest,
# one per line, or fail loudly if the manifest cannot be read the way
# `cargo fuzz` reads it.
#
# This lives in a script rather than inline in .github/workflows/fuzz.yml
# because it decides which targets the nightly fuzzes at all, and its
# failure mode is silence: a target that quietly falls out of the list is
# never built, never fails, and so is never reported. Nothing lints or
# tests a workflow `run:` block -- tools/run-shellcheck.sh globs scripts/
# and tools/, and no job invokes actionlint -- so the same reasoning that
# put tools/report-fuzz-failure.sh under a smoke test applies here. See
# tools/test-fuzz-targets.sh.
#
# The `[[bin]]` tables in fuzz/Cargo.toml are the source rather than a
# glob over fuzz_targets/, because that manifest is what
# `cargo fuzz build <name>` itself resolves against: a glob would pick up
# a helper module dropped in the directory and would miss a `[[bin]]`
# whose path points elsewhere.
#
# Usage:
#   tools/fuzz-targets.sh MANIFEST
#
# Exits 0 with the target names on stdout, 1 on any condition that would
# make the nightly under-run, and 2 on a usage error.

set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 MANIFEST" >&2
    exit 2
fi

MANIFEST="$1"

if [ ! -f "${MANIFEST}" ]; then
    echo "fuzz-targets: ${MANIFEST}: no such file" >&2
    exit 1
fi

# Not a TOML parser. It wants `[[bin]]` at column 0 and `name` as the
# first field of its line; TOML permits valid spellings it cannot see,
# which is what the table count below is for.
#
# The value is taken as the whole remainder of the line rather than as
# `$3`, so a name containing a space arrives whole and is rejected by the
# charset check below. Reading `$3` would hand back the first word of it
# and look like a clean, shorter name -- a silently wrong target rather
# than a loud failure.
TARGETS="$(awk '/^\[\[bin\]\]/ { in_bin = 1; next }
                /^\[/          { in_bin = 0 }
                in_bin && $1 == "name" {
                    value = $0
                    sub(/^[[:space:]]*name[[:space:]]*=[[:space:]]*/, "", value)
                    sub(/[[:space:]]+$/, "", value)
                    gsub(/"/, "", value)
                    print value
                }' \
    "${MANIFEST}")"

COUNT=0
if [ -n "${TARGETS}" ]; then
    COUNT="$(printf '%s\n' "${TARGETS}" | wc -l | tr -d '[:space:]')"
fi

if [ "${COUNT}" -eq 0 ]; then
    echo "fuzz-targets: no fuzz targets found in ${MANIFEST}" >&2
    exit 1
fi

# `name="x"` without the spaces is the practical spelling the awk above
# misses, and cargo fuzz accepts it. The zero-target guard only catches
# losing *every* target, so count the tables independently and insist the
# two agree; a mismatch means the manifest grew a spelling this
# extraction cannot see, which is a bug to fix rather than a nightly to
# quietly under-run.
#
# The count tolerates leading whitespace that the awk pattern does not,
# deliberately: a guard blind in the same places as the thing it guards
# would agree with it and say nothing.
TABLES="$(grep -cE '^[[:space:]]*\[\[bin\]\]' "${MANIFEST}" || true)"
if [ "${COUNT}" -ne "${TABLES}" ]; then
    echo "fuzz-targets: ${MANIFEST} has ${TABLES} [[bin]] table(s) but" \
        "${COUNT} name(s) parsed: $(printf '%s' "${TARGETS}" | tr '\n' ' ')" >&2
    exit 1
fi

# Target names reach an issue title, a `in:title` search qualifier and
# markdown code spans in tools/report-fuzz-failure.sh. Every use there is
# correctly quoted, so this is not a command-injection guard; it keeps a
# name that would render or search strangely from getting that far, and
# it is the natural place for the check now that one script owns the
# list. cargo-fuzz target names are Rust binary names, so this rejects
# nothing legitimate.
while IFS= read -r TARGET; do
    case "${TARGET}" in
        *[!A-Za-z0-9_-]* | '')
            echo "fuzz-targets: ${MANIFEST}: target name '${TARGET}' is not" \
                "[A-Za-z0-9_-]+" >&2
            exit 1
            ;;
    esac
done <<EOF
${TARGETS}
EOF

printf '%s\n' "${TARGETS}"

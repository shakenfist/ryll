#!/usr/bin/env bash
# wave1-checks.sh — the two wave 1b checks that have to be testable.
#
# Sourced by wave1.sh, the same way audit-range.sh is.  Both functions
# here had rotted silently for months: one scanned a hardcoded crate
# list that the crate extraction left 46% short, the other scanned a
# directory that no longer existed and keyed on a convention every
# channel had dropped.  Neither failure was visible, because a check
# that finds nothing and a check that scans nothing print the same
# thing.
#
# They live in a sourceable file so tools/audit/test-wave1-style.sh can
# call them against fixtures.  That test is the actual guard against
# the next round of rot; the fail-loud checks in wave1.sh only catch
# the specific paths that broke last time.
#
# Defines functions only — sourcing this runs nothing.

# Print the src directory of each workspace member, one per line.
#
# Scoped to the members array, not to the [workspace] table.  An end
# pattern of /^\[[^w]/ was meant to skip [workspace.package] and friends,
# but no table in this Cargo.toml starts with another letter, so the
# range ran to end-of-file and the second sed harvested every
# quoted-string-on-its-own-line in it -- an `exclude = [...]` path, or a
# keyword, would have joined a fatal check it was never meant to gate.
workspace_member_src_dirs() {
    local cargo_toml="${1:-Cargo.toml}"
    sed -n '/^members = \[/,/^\]/p' "$cargo_toml" \
        | sed -n 's/^[[:space:]]*"\([^"]*\)",\?[[:space:]]*$/\1\/src/p'
}

# Print each logging::log_message call in $1 that has no verbosity guard
# within the preceding five lines.
#
# Advisory, and rough by construction.  A call is flagged when no line
# in its 5-line -B context carries a guard.  grep merges groups whose
# contexts overlap, so where one guard wraps several calls the second
# call's buffer starts after the first rather than at the guard above
# both, and it is flagged.  A precise answer needs a parser.
#
# The previous version tested its two conditions in the wrong order --
# it cleared the flag on a guard, then re-set it on the log_message
# line below -- so a guard above a call could never count, and it
# printed only whichever hit came last.
unguarded_log_messages() {
    local dir="$1"
    grep -rn -B5 'logging::log_message' "$dir" 2>/dev/null \
        | awk '
            /^--$/ { n = 0; next }
            /logging::log_message/ {
                guarded = 0
                for (i = 1; i <= n; i++) {
                    if (buf[i] ~ /log_config\.(verbose|intimate)|is_verbose/) {
                        guarded = 1
                    }
                }
                if (!guarded) print $0
                n = 0
                next
            }
            { buf[++n] = $0 }
        ' \
        || true
}

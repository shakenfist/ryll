#!/usr/bin/env bash
# wave1.sh — pre-push audit, mechanical wave.
#
# Runs the build, lint and test verification that PUSH-AUDIT.md
# wave 1 used to spawn two sub-agents to perform.  Single approval,
# single script, structured exit code.
#
# Exit code:
#   0  all checks passed
#   1  pre-commit failed
#   2  rustfmt/clippy failed
#   3  cargo test failed
#   4  style-conformance grep failed (raw println!/eprintln! found)
#   5  could not cd to the repository root
#   6  the audit range covered nothing: AUDIT_BASE or AUDIT_HEAD was
#      set but does not resolve, an explicitly-set range is empty, or
#      the defaulted develop...HEAD range is empty.  Note the last
#      one -- wave1 hard-fails there, where wave2-mechanical.sh only
#      warns; a build that passed on an empty range proved nothing
#      about the diff.
#
# Style conformance is intentionally kept narrow here — only the
# fully-mechanical checks live in this script.  Anything needing
# judgment (channel-handler conventions, "missed abstractions",
# documentation alignment, security analysis) stays as a wave 2
# sub-agent.
#
# Usage: tools/audit/wave1.sh
#        AUDIT_BASE=<sha> AUDIT_HEAD=develop tools/audit/wave1.sh
# Run from the worktree root.

set -u

# Always run from the repo root the script lives in.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT" || exit 5

red() { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold() { printf '\033[1m%s\033[0m\n' "$*"; }

# Audit range resolution and validation is shared with
# wave2-mechanical.sh; it exits 6 on a range that would make every
# diff-scoped check below report nothing.
# shellcheck source=tools/audit/audit-range.sh
. "$SCRIPT_DIR/audit-range.sh"
audit_range_init

bold "=== wave 1a: pre-commit ==="
if ! pre-commit run --all-files; then
    red "FAIL: pre-commit"
    exit 1
fi
green "PASS: pre-commit"
echo

bold "=== wave 1a: rustfmt + clippy via Docker ==="
if ! ./scripts/check-rust.sh check; then
    red "FAIL: rustfmt/clippy"
    exit 2
fi
green "PASS: rustfmt + clippy"
echo

bold "=== wave 1a: cargo test --workspace via Docker ==="
mkdir -p .cargo-cache/registry .cargo-cache/git
if ! docker run --rm \
    -v "$(pwd)":/workspace \
    -v "$(pwd)/.cargo-cache/registry":/build/.cargo/registry \
    -v "$(pwd)/.cargo-cache/git":/build/.cargo/git \
    -w /workspace \
    -u "$(id -u):$(id -g)" \
    -e HOME=/build \
    ryll-dev cargo test --workspace; then
    red "FAIL: cargo test"
    exit 3
fi
green "PASS: cargo test"
echo

bold "=== wave 1b: mechanical style checks ==="

# 1. No raw println! / eprintln! in non-test source code.
#    Allowlist mechanism: a println!/eprintln! is permitted
#    when the file containing it also contains the marker
#    comment `audit-allow-println`. Any such marker must be
#    reviewed and justified in the same commit that adds it.
#    Files with the marker are entirely excluded from this
#    check — intentional, because a single operator-facing
#    print in a file is the expected pattern and the marker
#    documents the rationale inline.
#
#    We use a Python one-liner to filter: for each grep hit
#    (format "path:lineno:text"), check whether the source
#    file contains the marker anywhere; if so, skip it.
#    The directories to scan come from the workspace `members`
#    list in Cargo.toml, not from a list maintained here.  A
#    hardcoded list was silently wrong for months: when the crate
#    extraction moved most of the code into
#    shakenfist-spice-renderer, this check -- the only style check
#    that is fatal rather than advisory -- kept scanning the four
#    crates it was written for and stopped seeing 46% of the
#    workspace, renderer and webrtc included.  Deriving the list
#    means adding a crate to the workspace extends the check.
mapfile -t MEMBER_SRC_DIRS < <(
    sed -n '/^\[workspace\]/,/^\[[^w]/p' Cargo.toml \
        | sed -n 's/^[[:space:]]*"\([^"]*\)",\?[[:space:]]*$/\1\/src/p'
)
#    A parse failure here must be loud.  An empty list would make
#    the check pass vacuously, which is the exact failure being
#    fixed.
if [[ ${#MEMBER_SRC_DIRS[@]} -eq 0 ]]; then
    red "FAIL: could not read workspace members from Cargo.toml"
    exit 4
fi
PRINTLN_HITS=$(grep -rn --include='*.rs' -E '^[[:space:]]*(println|eprintln)!' \
    "${MEMBER_SRC_DIRS[@]}" 2>/dev/null \
    | grep -v '/tests/' \
    | python3 -c "
import re
import sys


def test_region_lines(path):
    '''Line numbers (1-based) that lie in test-only code.

    Two shapes count.  A #[cfg(test)] module, tracked to its closing
    brace, and a #[test] function, tracked the same way.  The old
    filter grepped each *hit line* for #[cfg(test)], which cannot
    match: the attribute is never on the same line as the print, so
    every test diagnostic reached the fatal check as a false
    positive.
    '''
    lines = open(path).read().splitlines()
    marked = set()
    i = 0
    while i < len(lines):
        if re.match(r'\s*#\[(cfg\(test\)|test)\]', lines[i]):
            # Find the opening brace of the item this attribute is on,
            # then walk to its matching close.
            j = i
            while j < len(lines) and '{' not in lines[j]:
                j += 1
            depth = 0
            while j < len(lines):
                depth += lines[j].count('{') - lines[j].count('}')
                marked.add(j + 1)
                if depth <= 0:
                    break
                j += 1
            i = j
        i += 1
    return marked


cache = {}
for line in sys.stdin:
    parts = line.split(':', 2)
    if len(parts) >= 2:
        path = parts[0]
        try:
            lineno = int(parts[1])
        except ValueError:
            lineno = -1
        try:
            if 'audit-allow-println' in open(path).read():
                continue
            if path not in cache:
                cache[path] = test_region_lines(path)
            if lineno in cache[path]:
                continue
        except OSError:
            pass
    print(line, end='')
" \
    || true)
if [[ -n "$PRINTLN_HITS" ]]; then
    red "FAIL: raw println!/eprintln! found:"
    echo "$PRINTLN_HITS"
    exit 4
fi
green "PASS: no raw println!/eprintln!"

# 2. No log_message calls outside an is_verbose() guard.  Heuristic:
#    every channel handler that calls logging::log_message should have
#    a settings::is_verbose() check within the surrounding 5 lines.
UNGUARDED=$(grep -rn -B5 'logging::log_message' ryll/src/channels/ 2>/dev/null \
    | awk '/logging::log_message/ {hit=$0} /is_verbose/ {hit=""} END{if(hit) print hit}' \
    || true)
# The above heuristic is rough; only flag if ALL nearby is_verbose
# checks are missing.  A more precise check is left to wave 2a.
if [[ -n "$UNGUARDED" ]]; then
    echo "ADVISORY: possibly-unguarded logging::log_message:"
    echo "$UNGUARDED"
    echo "(verify manually; this heuristic has false positives)"
fi

# 3. Long-line check: warn on Rust source lines over 120 chars in changed
#    files relative to the audit base.  Non-fatal — purely
#    informational.
#    Content comes from the audit head rather than the checkout, so
#    the check still sees the right bytes when AUDIT_HEAD is not what
#    is checked out.
LONG_LINES=""
while IFS= read -r f; do
    [[ -n "$f" ]] || continue
    hits=$(audit_range_show "$f" \
        | awk -v f="$f" 'length > 120 {print f":"NR": "length" chars"}')
    if [[ -n "$hits" ]]; then
        LONG_LINES+="$hits"$'\n'
    fi
done < <(audit_range_files_matching '*.rs')
if [[ -n "$LONG_LINES" ]]; then
    echo "ADVISORY: lines over 120 chars in changed Rust files:"
    echo "$LONG_LINES" | head -20
fi

green "PASS: wave 1b mechanical"
echo

bold "=== wave 1 complete ==="
# The range only reaches here empty or unusable when it was left to
# default -- an explicit one that selects nothing already exited 6.
# Either way the diff-scoped checks proved nothing, and a green "all
# checks passed" scrolling into view ten minutes after the warning at
# the top would be read as if they had.
if ! audit_range_closing_summary red; then
    # An empty range is fatal here: build, lint and tests passing says
    # nothing about a diff that was never looked at.  A range that
    # would not resolve at all returns 0 -- a shallow clone with no
    # 'develop' has always been a NOTE rather than a failure.
    red "wave 1 is not complete: see above."
    exit 6
fi
green "all mechanical checks passed; proceed to wave 2 (judgment agents)"
exit 0

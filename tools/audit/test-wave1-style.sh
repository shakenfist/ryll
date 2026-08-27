#!/usr/bin/env bash
# test-wave1-style.sh — smoke test for wave 1b's style checks.
#
# Same argument as test-audit-range.sh, applied to the other half of
# the harness: these checks fail silently.  A scan pointed at a
# directory that no longer exists, a filter that suppresses everything,
# and a clean run all print the same thing, so nothing short of a
# fixture with a known answer can tell them apart.  Both checks had in
# fact been broken for months before anyone noticed.
#
# Covers filter-println-hits.py and the two functions in
# wave1-checks.sh.  Pure text processing against fixtures in a
# scratch directory; runs in about a second and needs no Docker,
# unlike the rest of wave 1.
#
# Usage: tools/audit/test-wave1-style.sh
# Exit code: 0 all assertions held, 1 otherwise.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FILTER="$SCRIPT_DIR/filter-println-hits.py"
# shellcheck source=/dev/null
. "$SCRIPT_DIR/wave1-checks.sh"

FAILURES=0
red() { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }

assert_contains() {
    local what="$1" needle="$2" hay="$3"
    if [[ $hay == *"$needle"* ]]; then
        green "PASS: $what"
    else
        red "FAIL: $what"
        red "  expected to find: $needle"
        red "  in: $hay"
        FAILURES=$((FAILURES + 1))
    fi
}

assert_not_contains() {
    local what="$1" needle="$2" hay="$3"
    if [[ $hay != *"$needle"* ]]; then
        green "PASS: $what"
    else
        red "FAIL: $what"
        red "  expected NOT to find: $needle"
        red "  in: $hay"
        FAILURES=$((FAILURES + 1))
    fi
}

assert_equals() {
    local what="$1" want="$2" got="$3"
    if [[ $want == "$got" ]]; then
        green "PASS: $what"
    else
        red "FAIL: $what"
        red "  want: $want"
        red "  got:  $got"
        FAILURES=$((FAILURES + 1))
    fi
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Run the print scan the way wave1.sh runs it, over one fixture dir.
scan() {
    grep -rn --include='*.rs' -E '^[[:space:]]*(println|eprintln)!' "$1" 2>/dev/null \
        | grep -v '/tests/' \
        | python3 "$FILTER" \
        || true
}

# --- the print filter: what it must report ----------------------------
mkdir -p "$WORK/src"

cat > "$WORK/src/plain.rs" <<'RS'
pub fn production() {
    println!("plain production print");
}
RS

# The two false negatives that motivated this test.  Both were
# suppressed by the brace-walking filter as first written: the forward
# search for an opening brace ran past a brace-less item and latched
# onto the next one, and a lone brace inside a string literal skewed
# the depth so the region ran to end-of-file.
cat > "$WORK/src/braceless_attr.rs" <<'RS'
#[cfg(test)]
use foo::Bar;

pub fn production() {
    println!("after an attribute on a brace-less item");
}
RS

cat > "$WORK/src/brace_in_string.rs" <<'RS'
#[test]
fn t() {
    let brace = "{";
}

pub fn production() {
    eprintln!("after an unbalanced brace in a string literal");
}
RS

# Raw strings and lifetimes defeat any line-local rule, so the filter
# tokenises.  A raw string cannot be un-escaped, and 'a must not read
# as an unterminated char literal.
cat > "$WORK/src/raw_and_lifetime.rs" <<'RS'
#[cfg(test)]
mod tests {
    #[test]
    fn raw() {
        let s = r#"unbalanced { inside a raw string"#;
        println!("this one is genuinely in a test");
    }
}

pub fn production<'a>(x: &'a str) {
    println!("after a raw string and a lifetime");
}
RS

cat > "$WORK/src/block_comment.rs" <<'RS'
/* an unbalanced { in a comment
   with /* a nested comment */ inside it */
pub fn production() {
    println!("after a nested block comment");
}
RS

out="$(scan "$WORK/src")"
assert_contains "a print in plain production code is reported" \
    "plain.rs:2" "$out"
assert_contains "an attribute on a brace-less item does not shield the code below" \
    "braceless_attr.rs:5" "$out"
assert_contains "a brace inside a string literal does not shield the code below" \
    "brace_in_string.rs:7" "$out"
assert_contains "a raw string and a lifetime do not shield the code below" \
    "raw_and_lifetime.rs:11" "$out"
assert_contains "a nested block comment does not shield the code below" \
    "block_comment.rs:4" "$out"

# --- the print filter: what it must suppress --------------------------
mkdir -p "$WORK/quiet"

cat > "$WORK/quiet/cfg_test_mod.rs" <<'RS'
pub fn production() {
    let x = 1;
}

#[cfg(test)]
mod tests {
    #[test]
    fn t() {
        println!("diagnostic inside a cfg(test) module");
    }
}
RS

cat > "$WORK/quiet/test_fn.rs" <<'RS'
#[test]
fn solo() {
    eprintln!("diagnostic inside a bare #[test] fn");
}
RS

cat > "$WORK/quiet/marked.rs" <<'RS'
// audit-allow-println: this file is the fixture for the marker.
pub fn production() {
    println!("exempt because the file carries the marker");
}
RS

# A test module declared in another file has no body here, so there is
# no region to mark -- and the code after it is live.
cat > "$WORK/quiet/external_mod.rs" <<'RS'
#[cfg(test)]
mod tests;

pub fn production() {
    println!("after an external test module declaration");
}
RS

out="$(scan "$WORK/quiet")"
assert_not_contains "a print inside a cfg(test) module is not reported" \
    "cfg_test_mod.rs" "$out"
assert_not_contains "a print inside a #[test] fn is not reported" \
    "test_fn.rs" "$out"
assert_not_contains "a file carrying audit-allow-println is exempt" \
    "marked.rs" "$out"
assert_contains "an external test module declaration does not shield the code below" \
    "external_mod.rs:5" "$out"

# --- the workspace member derivation ----------------------------------
# Against the real Cargo.toml: the answer is every workspace member and
# nothing else.  This is the assertion that would have caught the
# hardcoded four-crate list.
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
want="$(sed -n '/^members = \[/,/^\]/p' "$REPO_ROOT/Cargo.toml" | grep -c '"')"
got="$(workspace_member_src_dirs "$REPO_ROOT/Cargo.toml" | wc -l)"
assert_equals "every workspace member is scanned" "$want" "$got"

for crate in ryll shakenfist-spice-renderer shakenfist-spice-webrtc; do
    assert_contains "$crate is in the scan set" "$crate/src" \
        "$(workspace_member_src_dirs "$REPO_ROOT/Cargo.toml")"
done

# A Cargo.toml with an exclude list and a multi-line keyword array.
# The pre-fix parser ran its range to end-of-file and harvested all
# three, pulling a detached fuzz workspace into a fatal check.
cat > "$WORK/contaminated.toml" <<'TOML'
[workspace]
members = [
    "ryll",
]
exclude = [
    "shakenfist-spice-protocol/fuzz",
]
[workspace.package]
keywords = [
    "spice",
    "vdi",
]
TOML
out="$(workspace_member_src_dirs "$WORK/contaminated.toml")"
assert_equals "an exclude list and keywords do not leak into the scan set" \
    "ryll/src" "$out"

# No members array at all: empty output, which wave1.sh turns into a
# loud exit 7 rather than a vacuous pass.
cat > "$WORK/nomembers.toml" <<'TOML'
[package]
name = "solo"
TOML
assert_equals "a Cargo.toml with no members array yields nothing" \
    "" "$(workspace_member_src_dirs "$WORK/nomembers.toml")"

# --- the log_message verbosity guard ----------------------------------
mkdir -p "$WORK/channels"

cat > "$WORK/channels/wrapped.rs" <<'RS'
fn handle(&self) {
    if self.log_config.verbose {
        logging::log_message(&msg);
    }
}
RS

cat > "$WORK/channels/bare.rs" <<'RS'
fn handle(&self) {
    let msg = build();
    logging::log_message(&msg);
}
RS

out="$(unguarded_log_messages "$WORK/channels")"
assert_contains "an unguarded log_message is flagged" "bare.rs" "$out"
assert_not_contains "a guarded log_message is not flagged" "wrapped.rs" "$out"

# The convention this keys on is log_config.verbose.  When it changes
# again, the assertion above fails rather than the check going quiet.

echo
if [[ $FAILURES -eq 0 ]]; then
    green "all wave 1b style assertions held"
    exit 0
fi
red "$FAILURES wave 1b style assertion(s) failed"
exit 1

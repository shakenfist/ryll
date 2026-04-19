#!/bin/bash
# Scan for Unicode bidi and zero-width control characters.
#
# Trojan Source (CVE-2021-42574) and related attacks embed
# bidi or zero-width codepoints in source files to make
# malicious code read as benign. This script refuses to let
# any of those codepoints land in the tree.
#
# Usage:
#   check-bidi.sh                   Scan the whole tree (git ls-files).
#   check-bidi.sh FILE [FILE ...]   Scan explicitly listed files
#                                   (as used by the pre-commit hook).
#
# Exits 0 on clean, 1 if any suspicious codepoints are found.

set -euo pipefail

# Bidi override / isolate controls (CVE-2021-42574):
#   U+202A .. U+202E LRE RLE PDF LRO RLO
#   U+2066 .. U+2069 LRI RLI FSI PDI
# Zero-width and related spoofing chars:
#   U+200B .. U+200F ZWSP ZWNJ ZWJ LRM RLM
#   U+FEFF           BOM / ZWNBSP
#
# We combine both into a single PCRE character class. The files are
# read as bytes; grep -P with LC_ALL=C.UTF-8 interprets the pattern as
# Unicode codepoints in the input.
PATTERN='[\x{202a}-\x{202e}\x{2066}-\x{2069}\x{200b}-\x{200f}\x{feff}]'

if [ $# -eq 0 ]; then
    mapfile -t FILES < <(git ls-files)
else
    FILES=("$@")
fi

if [ ${#FILES[@]} -eq 0 ]; then
    echo "check-bidi: no files to scan."
    exit 0
fi

# Filter to regular files that exist. Pre-commit may pass deleted
# paths on commit-time, and git ls-files may include symlinks.
EXISTING=()
for f in "${FILES[@]}"; do
    if [ -f "$f" ]; then
        EXISTING+=("$f")
    fi
done

if [ ${#EXISTING[@]} -eq 0 ]; then
    exit 0
fi

FOUND=0
# We run grep once per file so the error output names the file and
# line. grep exits 1 on no match, 0 on match, 2 on error — we treat
# match as a failure, no-match as success. -I skips binary files to
# avoid false positives on fixture assets whose bytes happen to
# encode a bidi codepoint.
for f in "${EXISTING[@]}"; do
    if LC_ALL=C.UTF-8 grep -nPI "$PATTERN" -- "$f" 2>/dev/null; then
        echo "check-bidi: suspicious Unicode codepoint in $f" >&2
        FOUND=1
    fi
done

if [ $FOUND -ne 0 ]; then
    echo "" >&2
    echo "check-bidi: one or more files contain bidi or zero-width" >&2
    echo "control characters. These are associated with Trojan Source" >&2
    echo "attacks (CVE-2021-42574). Review carefully and remove, or" >&2
    echo "contact a maintainer if the codepoint is intentional." >&2
    exit 1
fi

exit 0

#!/usr/bin/env python3
"""filter-println-hits.py — drop test-only hits from wave 1's raw print scan.

Reads `grep -rn` output on stdin (``path:lineno:text``) and re-emits only the
hits that are in production code.  A hit is dropped when its file carries an
``audit-allow-println`` marker, or when its line lies inside a ``#[cfg(test)]``
item or a ``#[test]`` function.

This lives in its own file rather than inline in wave1.sh for three reasons:
it was a `python3 -c "..."` double-quoted string, so any future `$` would have
been eaten by bash before python saw it; it is the filter for the only *fatal*
style check, so it needs to be directly testable; and at this length it wants
normal linting.  Its tests are in tools/audit/test-wave1-style.sh.

The bias throughout is to report rather than suppress.  This check exists
because a fatal check had been passing vacuously for months, so when the
analysis cannot make sense of a file, the hit survives and a human looks at it.
"""

import re
import sys

# An attribute introducing test-only code.  Both shapes appear in this tree:
# `#[cfg(test)] mod tests` and a bare `#[test] fn`.
ATTR_RE = re.compile(r'\s*#\[\s*(cfg\(\s*test\s*\)|test)\s*\]')

# The start of a raw string: r"..", r#".."#, br#".."#, cr#".."#.  Raw strings
# are why this file tokenises rather than running a regex per line -- a
# `r"\"` or a brace inside one defeats any line-local rule.
RAW_RE = re.compile(r'(?:b|c)?r(#*)"')

IDENT_CHARS = set('abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_')


def strip_noncode(source):
    """Return source with comment and literal *content* replaced by spaces.

    Newlines are preserved exactly, so line numbers in the result still match
    the file on disk.  The point is that brace counting afterwards sees only
    real code: a `"{"` in a format string no longer skews the depth, which is
    the failure mode that let a test region run to end-of-file and swallow
    every function after it.
    """
    out = []
    i = 0
    n = len(source)
    while i < n:
        c = source[i]

        if c == '/' and source.startswith('//', i):
            while i < n and source[i] != '\n':
                out.append(' ')
                i += 1
            continue

        # Rust block comments nest, so this counts rather than scanning for
        # the first */.
        if c == '/' and source.startswith('/*', i):
            depth = 0
            while i < n:
                if source.startswith('/*', i):
                    depth += 1
                    out.append('  ')
                    i += 2
                    continue
                if source.startswith('*/', i):
                    depth -= 1
                    out.append('  ')
                    i += 2
                    if depth == 0:
                        break
                    continue
                out.append('\n' if source[i] == '\n' else ' ')
                i += 1
            continue

        # A raw string, but only where an identifier could not be continuing:
        # the `r` in `for` must not start one.
        if c in 'rbc' and (i == 0 or source[i - 1] not in IDENT_CHARS):
            m = RAW_RE.match(source, i)
            if m:
                terminator = '"' + m.group(1)
                out.append(' ' * (m.end() - i))
                i = m.end()
                end = source.find(terminator, i)
                end = n if end < 0 else end + len(terminator)
                while i < end:
                    out.append('\n' if source[i] == '\n' else ' ')
                    i += 1
                continue

        if c == '"':
            out.append(' ')
            i += 1
            while i < n:
                if source[i] == '\\':
                    out.append('  ')
                    i += 2
                    continue
                if source[i] == '"':
                    out.append(' ')
                    i += 1
                    break
                out.append('\n' if source[i] == '\n' else ' ')
                i += 1
            continue

        # A quote is a char literal only in the shapes 'x' and '\n'.  Anything
        # else is a lifetime (`'a`, `&'static`) and must be left alone, or the
        # scan would run to the next apostrophe and blank out real code.
        if c == "'":
            if i + 1 < n and source[i + 1] == '\\':
                end = source.find("'", i + 2)
                if end >= 0:
                    out.append(' ' * (end - i + 1))
                    i = end + 1
                    continue
            elif i + 2 < n and source[i + 2] == "'":
                out.append('   ')
                i += 3
                continue

        out.append(c)
        i += 1

    return ''.join(out)


def test_region_lines(path):
    """Return the 1-based line numbers of ``path`` that are test-only code."""
    with open(path, encoding='utf-8', errors='replace') as handle:
        lines = strip_noncode(handle.read()).splitlines()

    marked = set()
    i = 0
    while i < len(lines):
        if not ATTR_RE.match(lines[i]):
            i += 1
            continue

        # Find the opening brace of the item the attribute is on.  A line
        # ending in `;` before any brace means the item has no body at all --
        # `#[cfg(test)] use foo::Bar;` or `#[cfg(test)] mod tests;` -- so
        # there is no region.  Without this bound the search ran on and
        # latched onto the next unrelated item's brace, marking live code as
        # test code.
        j = i
        while j < len(lines) and '{' not in lines[j]:
            if lines[j].rstrip().endswith(';'):
                j = None
                break
            j += 1
        if j is None or j >= len(lines):
            i += 1
            continue

        depth = 0
        k = j
        while k < len(lines):
            depth += lines[k].count('{') - lines[k].count('}')
            if depth <= 0:
                break
            k += 1

        if k >= len(lines):
            # Never balanced.  Rather than mark to end-of-file, mark nothing
            # and let the hits through: an unreadable file should produce
            # noise a human resolves, not silence.
            i += 1
            continue

        marked.update(range(i + 1, k + 2))
        i = k + 1

    return marked


def main():
    regions = {}
    exempt = {}
    for line in sys.stdin:
        parts = line.split(':', 2)
        if len(parts) >= 2:
            path = parts[0]
            try:
                lineno = int(parts[1])
            except ValueError:
                lineno = -1
            try:
                if path not in exempt:
                    with open(path, encoding='utf-8', errors='replace') as handle:
                        exempt[path] = 'audit-allow-println' in handle.read()
                if exempt[path]:
                    continue
                if path not in regions:
                    regions[path] = test_region_lines(path)
                if lineno in regions[path]:
                    continue
            except OSError:
                pass
        print(line, end='')


if __name__ == '__main__':
    main()

#!/usr/bin/env python3
"""Check the feature parity matrix in docs/multi-mode-parity.md.

Every row of the table under "## Parity matrix" must have one cell per column of
its header, and every mode's cell must start with a verdict the page's "How to
read the matrix" table defines: available, partial, missing or "n/a — intrinsic".
Section rows (a bold label and empty cells) are skipped.

The page also asks for a reason in parentheses after "n/a — intrinsic". Some
older cells give none, so that is not checked yet; see shakenfist/ryll#646.

Usage: tools/check-parity-matrix.py [PATH]   (default docs/multi-mode-parity.md)
Exits 0 when the matrix is well formed, 1 otherwise, printing each problem.
"""

import sys

VERDICTS = ('available', 'partial', 'missing', 'n/a — intrinsic')
HEADING = '## Parity matrix'


def cells(line):
    """Split a markdown table row on the pipes that are not inside backticks."""
    out, cur, tick = [], '', False
    for ch in line.strip()[1:-1]:
        if ch == '`':
            tick = not tick
        if ch == '|' and not tick:
            out.append(cur.strip())
            cur = ''
        else:
            cur += ch
    out.append(cur.strip())
    return out


def check(text, path):
    """Return (problems, rows checked) for the matrix in text."""
    lines = text.split('\n')
    if HEADING not in lines:
        return [f'{path}: no "{HEADING}" heading'], 0
    problems, rows, header = [], 0, None
    for n, line in enumerate(lines[lines.index(HEADING):], lines.index(HEADING) + 1):
        if header is None:
            if line.startswith('| Feature |'):
                header = cells(line)
            continue
        if not line.startswith('|'):
            break
        if line.startswith('|---'):
            continue
        row = cells(line)
        if len(row) != len(header):
            problems.append(f'{path}:{n}: {len(row)} cells, the header has {len(header)}')
            continue
        if row[0].startswith('**') and not any(row[1:]):
            continue
        rows += 1
        for mode, cell in zip(header[1:], row[1:]):
            if not cell.startswith(VERDICTS):
                problems.append(f'{path}:{n}: {mode} cell starts {cell[:30]!r}, not a verdict')
    if header is None:
        problems.append(f'{path}: no "| Feature |" table under "{HEADING}"')
    return problems, rows


def main(argv):
    path = argv[1] if len(argv) > 1 else 'docs/multi-mode-parity.md'
    with open(path, encoding='utf-8') as f:
        problems, rows = check(f.read(), path)
    for problem in problems:
        print(problem)
    if not problems:
        print(f'{path}: {rows} feature rows, all well formed')
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main(sys.argv))

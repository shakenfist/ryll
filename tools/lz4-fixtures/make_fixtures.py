#!/usr/bin/env python3
"""Write the SPICE LZ4 test fixtures: captured images plus liblz4-made synthetic streams.

Usage: make_fixtures.py <out dir> [--run-dir <run dir> <name>=<msg>:<shot> ...]

Each captured fixture <name>.bin is the BinaryData body (the bytes after data_size). Its
<name>.rgba.lz4 is the matching screendump region as RGBA (A=255), top row first, compressed
with lz4.block.compress(store_size=True). The script refuses to write a captured fixture whose
liblz4 decode does not match the screendump exactly. <run dir> is the directory lz4_check.py
--dump-dir and qmp_drive.py wrote into; it is needed only when captured fixtures are named.
The synthetic fixtures need no capture and are always written.
"""
import argparse
import json
import os
import struct
import sys

import lz4.block

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lz4_check as c  # noqa: E402

W, H = 37, 16
# For the three-block file only: pixels with x >= REPEAT_X take the formula at row y % 5, so the
# right part of every row in blocks 2 and 3 repeats bytes from an earlier block and liblz4 has
# to reference the dictionary. With the plain formula no row repeats a 4-byte run of an earlier
# row, so liblz4 never used the dictionary and the blocks decoded without it.
REPEAT_X = 18


def formula(x, y, repeat=False):
    if repeat and x >= REPEAT_X:
        y = y % 5
    return ((x * 7 + y) & 0xff, (y * 13) & 0xff, (x ^ y) & 0xff, (x + y * 3) & 0xff)


def pixel_bytes(fmt, x, y, repeat=False):
    r, g, b, a = formula(x, y, repeat)
    if fmt == 8:
        return bytes((b, g, r, 0xaa))
    if fmt == 9:
        return bytes((b, g, r, a))
    if fmt == 7:
        return bytes((b, g, r))
    v = ((r >> 3) << 10) | ((g >> 3) << 5) | (b >> 3)
    return struct.pack('<H', v)


def expected_rgba(fmt, x, y, repeat=False):
    r, g, b, a = formula(x, y, repeat)
    if fmt == 6:
        def ex(v):
            v >>= 3
            return (v << 3) | (v >> 2)
        return bytes((ex(r), ex(g), ex(b), 255))
    return bytes((r, g, b, a if fmt == 9 else 255))


def rows_bytes(fmt, ys, repeat=False):
    return [b''.join(pixel_bytes(fmt, x, y, repeat) for x in range(W)) for y in ys]


def build_stream(top_down, fmt, memory_rows, split):
    """memory_rows: encoded rows in memory order. split: rows per block. Dependent blocks."""
    out = bytearray((top_down, fmt))
    blocks = []
    prior = b''
    i = 0
    for n in split:
        chunk = b''.join(memory_rows[i:i + n])
        i += n
        if prior:
            comp = lz4.block.compress(chunk, store_size=False, dict=prior[-65536:])
        else:
            comp = lz4.block.compress(chunk, store_size=False)
        blocks.append((comp, chunk, prior))
        out += struct.pack('>I', len(comp)) + comp
        prior += chunk
    assert i == len(memory_rows)
    return bytes(out), blocks


def decode_rgba(body, width, height):
    """Independent decode to RGBA rows, top row first, using lz4_check.lz4_decode for the blocks."""
    top_down, fmt, lengths, dec, problems = c.lz4_decode(body, width, height)
    assert not problems, problems
    bpp = c.BPP[fmt]
    assert len(dec) == width * height * bpp, (len(dec), width * height * bpp)
    rows = []
    for r in range(height):
        row = dec[r * width * bpp:(r + 1) * width * bpp]
        out = bytearray()
        for x in range(width):
            p = row[x * bpp:(x + 1) * bpp]
            if fmt == 8:
                out += bytes((p[2], p[1], p[0], 255))
            elif fmt == 9:
                out += bytes((p[2], p[1], p[0], p[3]))
            elif fmt == 7:
                out += bytes((p[2], p[1], p[0], 255))
            else:
                (v,) = struct.unpack('<H', p)
                ch = [(v >> 10) & 31, (v >> 5) & 31, v & 31]
                out += bytes([(k << 3) | (k >> 2) for k in ch] + [255])
        rows.append(bytes(out))
    if not top_down:
        rows.reverse()
    return top_down, fmt, lengths, rows


def synthetic(outdir):
    made = []
    # 32-bit, top-down, three dependent blocks of 5, 5, 6 rows.
    rows = rows_bytes(8, range(H), repeat=True)
    body, blocks = build_stream(1, 8, rows, [5, 5, 6])
    for k, (comp, chunk, prior) in enumerate(blocks[1:], start=2):
        try:
            got = lz4.block.decompress(comp, uncompressed_size=len(chunk))
            dependent = got != chunk
            note = 'decoded without dict but WRONG' if dependent else 'decoded without dict CORRECTLY'
        except lz4.block.LZ4BlockError as e:
            dependent = True
            note = f'fails without dict: {e}'
        with_dict = lz4.block.decompress(comp, uncompressed_size=len(chunk), dict=prior[-65536:])
        print(f'  3blocks block {k}: {len(comp)} bytes; {note}; with dict correct: {with_dict == chunk}')
        assert dependent, 'block does not depend on the dictionary'
    made.append(('synthetic-32-topdown-3blocks.bin', body, 8, True))
    # 32-bit, bottom-up: memory order is bottom row first.
    rows = rows_bytes(8, reversed(range(H)))
    body, _ = build_stream(0, 8, rows, [H])
    made.append(('synthetic-32-bottomup.bin', body, 8, False))
    for name, fmt in (('synthetic-24.bin', 7), ('synthetic-16.bin', 6), ('synthetic-rgba.bin', 9)):
        body, _ = build_stream(1, fmt, rows_bytes(fmt, range(H)), [H])
        made.append((name, body, fmt, False))
    for name, body, fmt, repeat in made:
        top_down, f, lengths, rgba_rows = decode_rgba(body, W, H)
        exp = [b''.join(expected_rgba(fmt, x, y, repeat) for x in range(W)) for y in range(H)]
        ok = rgba_rows == exp
        print(f'  {name}: {len(body)} bytes top_down={top_down} fmt={f} blocks={lengths} '
              f'decode reproduces formula: {ok}')
        assert ok
        if fmt == 8:
            assert all(b == 0xaa for b in c.lz4_decode(body, W, H)[3][3::4]), 'X byte not 0xaa'
        with open(f'{outdir}/{name}', 'wb') as fh:
            fh.write(body)


def captured(run, outdir, specs):
    with open(f'{run}/imgs/results.json') as fh:
        res = {r['msg']: r for r in json.load(fh)}
    for spec in specs:
        name, rest = spec.split('=')
        msg, shot_name = rest.split(':')
        r = res[int(msg)]
        with open(f'{run}/imgs/img-{int(msg):04d}.bin', 'rb') as fh:
            body = fh.read()
        assert len(body) == r['data_size']
        assert r['src'] == (0, 0, r['w'], r['h']) or r['src'] == [0, 0, r['w'], r['h']]
        top_down, fmt, lengths, rows = decode_rgba(body, r['w'], r['h'])
        sw, _sh, pix = c.read_ppm(f'{run}/{shot_name}')
        left, top, right, bottom = r['box']
        assert (right - left, bottom - top) == (r['w'], r['h'])
        shot_rows = []
        for y in range(top, bottom):
            seg = pix[(y * sw + left) * 3:(y * sw + right) * 3]
            shot_rows.append(b''.join(seg[i:i + 3] + b'\xff' for i in range(0, len(seg), 3)))
        match = shot_rows == rows
        print(f'  {name}: msg {msg} {r["w"]}x{r["h"]} top_down={top_down} fmt={fmt} blocks={lengths} '
              f'box={r["box"]} vs {shot_name}: exact={match}')
        assert match, 'decode does not match screendump'
        rgba = b''.join(shot_rows)
        comp = lz4.block.compress(rgba, store_size=True)
        assert lz4.block.decompress(comp) == rgba
        with open(f'{outdir}/{name}.bin', 'wb') as fh:
            fh.write(body)
        with open(f'{outdir}/{name}.rgba.lz4', 'wb') as fh:
            fh.write(comp)
        print(f'    {name}.bin {len(body)} bytes, {name}.rgba.lz4 {len(comp)} bytes (from screendump)')


def main():
    ap = argparse.ArgumentParser(description='Write the SPICE LZ4 test fixtures.')
    ap.add_argument('out_dir')
    ap.add_argument('--run-dir', help='capture run directory; required when captured fixtures are named')
    ap.add_argument('specs', nargs='*', metavar='name=msg:shot', help='captured fixtures to write')
    args = ap.parse_args()
    if args.specs and not args.run_dir:
        ap.error('captured fixtures need --run-dir')
    if args.specs:
        print('captured:')
        captured(args.run_dir, args.out_dir, args.specs)
    print('synthetic:')
    synthetic(args.out_dir)


if __name__ == '__main__':
    main()

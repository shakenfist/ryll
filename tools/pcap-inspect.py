#!/usr/bin/env python3
"""Inspect a ryll --capture pcap file.

Pure-python parser — no tshark/scapy dependency. The pcap files
ryll writes are big-endian (per libpcap format, consumed by whichever
reader) carrying synthetic TCP frames; inside each frame the payload
is the raw SPICE stream (post-link-handshake, mini-header mode).

Subcommands:

    opcodes <path>            Count server-side SPICE message types.
    draw-copy <path>          Break down DRAW_COPY by surface, image
                              type, and scale mode; sample the first
                              occurrence of each image type.
    timeline <path> [--since-last N]
                              Print each server-side message in
                              chronological order with its timestamp,
                              or only those within N seconds of the
                              last message (handy for post-bug-report
                              analysis).

Works on `display.pcap`, `cursor.pcap`, `main.pcap`, etc. Subcommands
that care about draw-op structure only make sense for display.pcap.
"""
from __future__ import annotations

import argparse
import collections
import struct
import sys
from typing import Iterator

# ── SPICE message-type tables ────────────────────────────────

COMMON_OPCODES = {
    1: "MIGRATE",
    2: "MIGRATE_DATA",
    3: "SET_ACK",
    4: "PING",
    5: "WAIT_FOR_CHANNELS",
    6: "DISCONNECTING",
    7: "NOTIFY",
}

DISPLAY_OPCODES = {
    **COMMON_OPCODES,
    101: "MODE",
    102: "MARK",
    103: "RESET",
    104: "COPY_BITS",
    105: "INVALIDATE_LIST",
    108: "INVAL_ALL_PIXMAPS",
    122: "STREAM_CREATE",
    123: "STREAM_DATA",
    124: "STREAM_CLIP",
    125: "STREAM_DESTROY",
    302: "DRAW_FILL",
    303: "DRAW_OPAQUE",
    304: "DRAW_COPY",
    305: "DRAW_BLEND",
    306: "DRAW_BLACKNESS",
    307: "DRAW_WHITENESS",
    308: "DRAW_INVERS",
    309: "DRAW_ROP3",
    310: "DRAW_STROKE",
    311: "DRAW_TEXT",
    312: "DRAW_TRANSPARENT",
    313: "DRAW_ALPHA_BLEND",
    314: "SURFACE_CREATE",
    315: "SURFACE_DESTROY",
    316: "STREAM_DATA_SIZED",
    317: "MONITORS_CONFIG",
    318: "DRAW_COMPOSITE",
    319: "STREAM_ACTIVATE_REPORT",
}

IMAGE_TYPES = {
    0: "BITMAP",
    1: "QUIC",
    100: "LZ_PLT",
    101: "LZ_RGB",
    102: "GLZ_RGB",
    103: "FROM_CACHE",
    104: "SURFACE",
    105: "JPEG",
    106: "FROM_CACHE_LOSSLESS",
    107: "ZLIB_GLZ_RGB",
    108: "JPEG_ALPHA",
}

# ── Pcap reassembly ──────────────────────────────────────────


def _reassemble(path: str) -> dict[tuple[int, int], tuple[bytearray, list[float]]]:
    """Group pcap payloads into per-half-stream byte buffers.

    Returns { (sport, dport): (bytes, [per-byte timestamp segments]) }.
    The timestamps list has one (offset, ts) pair per TCP segment so
    timeline-mode can map a message back to when it arrived.
    """
    with open(path, "rb") as f:
        data = f.read()
    assert data[:4] == b"\xa1\xb2\xc3\xd4", "not a pcap file"
    streams: dict[tuple[int, int], tuple[bytearray, list[tuple[int, float]]]] = (
        collections.defaultdict(lambda: (bytearray(), []))
    )
    o = 24  # global header
    t0: float | None = None
    while o + 16 <= len(data):
        ts_sec, ts_usec, incl, _orig = struct.unpack(">IIII", data[o : o + 16])
        o += 16
        pkt = data[o : o + incl]
        o += incl
        if len(pkt) < 54:
            continue
        ts = ts_sec + ts_usec / 1_000_000.0
        if t0 is None:
            t0 = ts
        sport = struct.unpack(">H", pkt[34:36])[0]
        dport = struct.unpack(">H", pkt[36:38])[0]
        tcp_hdr_len = ((pkt[46] >> 4) & 0xF) * 4
        payload = pkt[14 + 20 + tcp_hdr_len :]
        if not payload:
            continue
        buf, ts_index = streams[(sport, dport)]
        ts_index.append((len(buf), ts - t0))
        buf.extend(payload)
    return streams


def _pick_server_stream(
    streams: dict[tuple[int, int], tuple[bytearray, list[tuple[int, float]]]],
) -> tuple[bytearray, list[tuple[int, float]]]:
    """The server→client half-stream is the one carrying DRAW_*
    messages, which is almost always the larger side."""
    return max(streams.values(), key=lambda pair: len(pair[0]))


def _iter_messages(
    buf: bytearray,
) -> Iterator[tuple[int, int, int, int]]:
    """Yield (offset, msg_type, msg_size, body_end) for each SPICE
    mini-header message in the buffer. Stops at the first truncated
    record."""
    i = 0
    while i + 6 <= len(buf):
        mt = struct.unpack("<H", buf[i : i + 2])[0]
        ms = struct.unpack("<I", buf[i + 2 : i + 6])[0]
        if i + 6 + ms > len(buf):
            return
        yield (i, mt, ms, i + 6 + ms)
        i += 6 + ms


def _ts_at_offset(index: list[tuple[int, float]], off: int) -> float:
    """Lookup the wall-clock timestamp (seconds since capture start)
    for a byte offset in the reassembled stream — the timestamp of the
    TCP segment that delivered that byte."""
    last_ts = 0.0
    for seg_off, ts in index:
        if seg_off > off:
            break
        last_ts = ts
    return last_ts


# ── DrawBase / DrawCopy parsing ──────────────────────────────


def _parse_draw_base(payload: bytes) -> tuple[int, int, int, int, int, int] | None:
    """Returns (surface_id, top, left, bottom, right, end_offset) or
    None on truncation."""
    if len(payload) < 21:
        return None
    sid, top, left, bot, right = struct.unpack("<IIIII", payload[0:20])
    clip_type = payload[20]
    o = 21
    if clip_type == 1:
        if len(payload) < o + 4:
            return None
        n = struct.unpack("<I", payload[o : o + 4])[0]
        o += 4 + 16 * n
    return (sid, top, left, bot, right, o)


def _parse_draw_copy(payload: bytes) -> dict | None:
    b = _parse_draw_base(payload)
    if b is None:
        return None
    sid, top, left, bot, right, o = b
    if len(payload) < o + 4 + 16 + 2 + 1 + 13:
        return None
    src_bitmap_off = struct.unpack("<I", payload[o : o + 4])[0]
    src_top, src_left, src_bot, src_right = struct.unpack(
        "<IIII", payload[o + 4 : o + 20]
    )
    rop = struct.unpack("<H", payload[o + 20 : o + 22])[0]
    scale = payload[o + 22]
    image = None
    if src_bitmap_off and src_bitmap_off + 18 <= len(payload):
        iid = struct.unpack("<Q", payload[src_bitmap_off : src_bitmap_off + 8])[0]
        itype = payload[src_bitmap_off + 8]
        iflags = payload[src_bitmap_off + 9]
        iw, ih = struct.unpack(
            "<II", payload[src_bitmap_off + 10 : src_bitmap_off + 18]
        )
        image = (iid, itype, iflags, iw, ih)
    return {
        "surface_id": sid,
        "rect": (left, top, right, bot),
        "src_rect": (src_left, src_top, src_right, src_bot),
        "rop": rop,
        "scale": scale,
        "image": image,
    }


# ── Subcommands ──────────────────────────────────────────────


def cmd_opcodes(args: argparse.Namespace) -> int:
    streams = _reassemble(args.path)
    combined: collections.Counter[int] = collections.Counter()
    for (sport, dport), (buf, _idx) in streams.items():
        for _off, mt, _ms, _end in _iter_messages(buf):
            combined[mt] += 1
        print(f"stream {sport}→{dport}: {len(buf)} bytes")
    print()
    print("=== Opcodes (both directions combined) ===")
    for mt, n in combined.most_common():
        name = DISPLAY_OPCODES.get(mt, f"?_{mt}")
        print(f"  {mt:5d}  {name:25s}  {n}")
    return 0


def cmd_draw_copy(args: argparse.Namespace) -> int:
    streams = _reassemble(args.path)
    buf, _idx = _pick_server_stream(streams)
    by_surface: collections.Counter[int] = collections.Counter()
    by_image_type: collections.Counter[int] = collections.Counter()
    by_scale: collections.Counter[int] = collections.Counter()
    first_per_type: dict[int, tuple[int, dict]] = {}
    total = 0
    idx = 0
    for _off, mt, _ms, end in _iter_messages(buf):
        body_start = _off + 6
        if mt == 304:
            d = _parse_draw_copy(buf[body_start:end])
            total += 1
            if d is not None:
                by_surface[d["surface_id"]] += 1
                if d["image"] is not None:
                    itype = d["image"][1]
                    by_image_type[itype] += 1
                    first_per_type.setdefault(itype, (idx, d))
                by_scale[d["scale"]] += 1
        idx += 1
    print(f"DRAW_COPY total: {total}\n")
    print("By surface_id:")
    for sid, n in by_surface.most_common():
        print(f"  surface {sid}: {n}")
    print("\nBy image type:")
    for itype, n in by_image_type.most_common():
        name = IMAGE_TYPES.get(itype, f"?_{itype}")
        print(f"  {itype:3d} {name:20s}: {n}")
    print("\nBy scale mode:")
    for sc, n in by_scale.most_common():
        print(f"  {sc}: {n}")
    print("\nFirst example of each image type:")
    for itype, (i, d) in sorted(first_per_type.items()):
        name = IMAGE_TYPES.get(itype, f"?_{itype}")
        print(
            f"  type={itype} {name}: idx={i} surf={d['surface_id']} "
            f"rect={d['rect']} src={d['src_rect']} img={d['image']}"
        )
    return 0


def cmd_timeline(args: argparse.Namespace) -> int:
    streams = _reassemble(args.path)
    buf, index = _pick_server_stream(streams)
    events: list[tuple[float, int, int]] = []
    for off, mt, ms, _end in _iter_messages(buf):
        ts = _ts_at_offset(index, off)
        events.append((ts, mt, ms))
    if not events:
        print("no messages parsed")
        return 1
    last_ts = events[-1][0]
    cutoff = last_ts - args.since_last if args.since_last else 0.0
    shown = 0
    for ts, mt, ms in events:
        if ts < cutoff:
            continue
        name = DISPLAY_OPCODES.get(mt, f"?_{mt}")
        print(f"  {ts - events[0][0]:8.3f}s  {mt:5d} {name:25s} size={ms}")
        shown += 1
    print(f"\n({shown} messages shown of {len(events)} total)")
    return 0


# ── Main ─────────────────────────────────────────────────────


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sp = p.add_subparsers(dest="cmd", required=True)

    po = sp.add_parser("opcodes", help="Count SPICE message types.")
    po.add_argument("path")
    po.set_defaults(func=cmd_opcodes)

    pd = sp.add_parser("draw-copy", help="Break down DRAW_COPY by surface/image type.")
    pd.add_argument("path")
    pd.set_defaults(func=cmd_draw_copy)

    pt = sp.add_parser("timeline", help="Print server-side messages in order.")
    pt.add_argument("path")
    pt.add_argument(
        "--since-last",
        type=float,
        default=0.0,
        metavar="SEC",
        help="Only messages within SEC seconds of the last message.",
    )
    pt.set_defaults(func=cmd_timeline)

    args = p.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())

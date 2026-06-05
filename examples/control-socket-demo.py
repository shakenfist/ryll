#!/usr/bin/env python3
"""Demo client for Ryll's Unix-domain control socket (protocol v1.0).

This script connects to a running ``ryll --headless --control-socket <path>``
instance, completes the hello handshake, queries session status, subscribes to
latency events, sends a spacebar press (scancode 0x39), and pastes the text
"hi".  It reads events for ~3 seconds (or until 5 latency samples arrive) then
exits cleanly.

Intended role: copy-paste starter for the phase-4 latency loadtest port.
The output format (one arrow-prefixed JSON line per message) is structured
enough to appear verbatim in debug logs.

Protocol reference:
    ryll/docs/control-socket-protocol.md

Wire format:
    → {"id": <int>, "method": "<verb>", "params": {...}}     (client → server)
    ← {"id": <int>, "ok": true,  "result": {...}}            (server → client)
    ← {"id": <int>, "ok": false, "error": {"code": "...", ...}}
    ← {"event": "<name>", "data": {...}}                     (unsolicited)

Usage:
    python3 control-socket-demo.py /tmp/ryll.sock

Requirements:
    Python 3.10+; stdlib only (socket, json, argparse, sys, threading, time).
"""

import argparse
import json
import socket
import sys
import threading
import time

# ── Protocol helpers ─────────────────────────────────────────────────────────

_next_id = 0
_id_lock = threading.Lock()


def _new_id() -> int:
    """Return a monotonically-increasing request id (thread-safe)."""
    global _next_id
    with _id_lock:
        _next_id += 1
        return _next_id


def _send(sock: socket.socket, method: str, params: dict) -> int:
    """Serialise and write one NDJSON request; return the request id."""
    req_id = _new_id()
    line = json.dumps({'id': req_id, 'method': method, 'params': params}) + '\n'
    sock.sendall(line.encode('utf-8'))
    print(f'→ {line.rstrip()}')
    return req_id


def _recv_line(buf: bytearray, sock: socket.socket) -> dict:
    """Read bytes until a newline, then parse and return the JSON object."""
    while b'\n' not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            raise EOFError('server closed the connection')
        buf.extend(chunk)
    nl = buf.index(b'\n')
    line = buf[:nl].decode('utf-8')
    del buf[: nl + 1]
    return json.loads(line)


# ── Background reader ─────────────────────────────────────────────────────────

class _Reader(threading.Thread):
    """Background thread that reads NDJSON lines and queues them."""

    def __init__(self, sock: socket.socket):
        super().__init__(daemon=True, name='reader')
        self._sock = sock
        self._buf: bytearray = bytearray()
        self._lock = threading.Lock()
        self._items: list[dict] = []
        self._stop = threading.Event()

    def run(self) -> None:
        try:
            while not self._stop.is_set():
                try:
                    obj = _recv_line(self._buf, self._sock)
                    print(f'← {json.dumps(obj)}')
                    with self._lock:
                        self._items.append(obj)
                except OSError:
                    break
        except EOFError:
            pass

    def stop(self) -> None:
        self._stop.set()

    def drain(self) -> list[dict]:
        """Return and clear all buffered items."""
        with self._lock:
            items = list(self._items)
            self._items.clear()
        return items

    def count_by_event(self, name: str) -> int:
        with self._lock:
            return sum(1 for i in self._items if i.get('event') == name)


# ── Main flow ─────────────────────────────────────────────────────────────────

def main() -> None:
    parser = argparse.ArgumentParser(
        description='Demo client for the Ryll control socket (v1.0).',
    )
    parser.add_argument('socket_path', help='Path to the ryll control socket, e.g. /tmp/ryll.sock')
    parser.add_argument(
        '--latency-samples',
        type=int,
        default=5,
        metavar='N',
        help='Stop after receiving this many latency events (default: 5)',
    )
    parser.add_argument(
        '--duration',
        type=float,
        default=3.0,
        metavar='SEC',
        help='Maximum seconds to collect events (default: 3)',
    )
    args = parser.parse_args()

    # ── Connect ──────────────────────────────────────────────────────────────
    print(f'Connecting to {args.socket_path} …')
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(args.socket_path)
    except (FileNotFoundError, ConnectionRefusedError) as exc:
        print(f'ERROR: could not connect: {exc}', file=sys.stderr)
        sys.exit(1)

    # Start background reader for unsolicited events.
    reader = _Reader(sock)
    reader.start()

    buf: bytearray = bytearray()  # synchronous receive buffer (unused once reader starts)

    def request(method: str, params: dict) -> dict:
        """Send a request and wait for its matching response synchronously."""
        req_id = _send(sock, method, params)
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            items = reader.drain()
            for item in items:
                # Events (no 'id') go back to the reader's queue for the caller
                # to inspect via count_by_event(); responses are returned here.
                if 'id' in item and item['id'] == req_id:
                    return item
                # Non-matching id or pure event: push back.
                with reader._lock:
                    reader._items.append(item)
            time.sleep(0.02)
        raise TimeoutError(f'no response to request {req_id!r} within 5 s')

    try:
        # ── Hello ─────────────────────────────────────────────────────────
        resp = request('hello', {'client_name': 'control-socket-demo', 'protocol_version': '1.0'})
        if not resp.get('ok'):
            print(f'ERROR: hello failed: {resp}', file=sys.stderr)
            sys.exit(1)
        result = resp['result']
        print(
            f'[hello] server={result["server_name"]} proto={result["protocol_version"]} '
            f'methods={result["supported_methods"]} events={result["supported_events"]}'
        )

        # ── Status ────────────────────────────────────────────────────────
        resp = request('status', {})
        if resp.get('ok'):
            r = resp['result']
            print(
                f'[status] spice_connected={r["spice_connected"]} '
                f'agent_connected={r["agent_connected"]} '
                f'surfaces={r["surfaces"]}'
            )

        # ── Subscribe to latency ──────────────────────────────────────────
        resp = request('subscribe', {'events': ['latency']})
        if resp.get('ok'):
            print(f'[subscribe] subscribed={resp["result"]["subscribed"]}')

        # ── Send spacebar (scancode 0x39 = 57) ───────────────────────────
        resp = request('send_key', {'scancode': 0x39, 'state': 'press'})
        print(f'[send_key] spacebar press: ok={resp.get("ok")}')

        # ── Paste "hi" ────────────────────────────────────────────────────
        resp = request('paste', {'text': 'hi'})
        print(f'[paste "hi"] ok={resp.get("ok")}')

        # ── Collect events for up to --duration seconds ───────────────────
        print(f'Collecting events for up to {args.duration:.1f} s '
              f'(or {args.latency_samples} latency samples) …')
        deadline = time.monotonic() + args.duration
        while time.monotonic() < deadline:
            if reader.count_by_event('latency') >= args.latency_samples:
                break
            time.sleep(0.05)

        latency_count = reader.count_by_event('latency')
        print(f'Done. Received {latency_count} latency sample(s).')

    except KeyboardInterrupt:
        print('\nInterrupted.')
    finally:
        reader.stop()
        sock.close()


if __name__ == '__main__':
    main()

#!/usr/bin/env python3
"""Minimal SPICE client that connects and sends a mouse click.

Uses kerbside's SpiceClient for connection/auth, then sends
raw SPICE messages to test mouse input.
"""

import struct
import sys
import time

from kerbside.spiceprotocol import SpiceClient
from kerbside.spiceprotocol.packets import constants


def make_msg(msg_type, payload):
    return struct.pack('<HI', msg_type, len(payload)) + payload


def recv_messages(sock, timeout=2.0):
    """Read all available mini-header messages within timeout."""
    import socket as _socket
    sock.settimeout(timeout)
    msgs = []
    buf = b''
    try:
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                break
            buf += chunk
            while len(buf) >= 6:
                msg_type, msg_size = struct.unpack_from('<HI', buf, 0)
                total = 6 + msg_size
                if len(buf) < total:
                    break
                payload = buf[6:total]
                buf = buf[total:]
                msgs.append((msg_type, payload))
    except _socket.timeout:
        pass
    return msgs


def respond_to_pings(sock, msgs):
    """Respond to any PING messages."""
    for msg_type, payload in msgs:
        if msg_type == 4 and len(payload) >= 12:  # PING
            ping_id, timestamp = struct.unpack_from('<IQ', payload, 0)
            pong = make_msg(3, struct.pack('<IQ', ping_id, timestamp))
            sock.sendall(pong)


def make_client(vv_path):
    """Create a SpiceClient configured from a .vv file, bypassing from_vv_file bugs."""
    import configparser
    vv = configparser.ConfigParser()
    vv.read(vv_path)
    s = vv['virt-viewer']

    client = SpiceClient()
    client.from_static_configuration(
        server=s['host'],
        port=s.get('port', '0'),
        tls_port=s.get('tls-port'),
        secure=bool(s.get('tls-port')),
        password=s.get('password', ''),
        ca_cert=s.get('ca', '').replace('\\n', '\n'),
        host_subject=s.get('host-subject'),
    )
    return client


def main():
    vv_path = sys.argv[1] if len(sys.argv) > 1 else 'console.vv'
    print(f'Reading {vv_path}')

    # Connect main channel using kerbside
    print('Connecting main channel...')
    main_client = make_client(vv_path)
    main_client.connect(connection_id=0, channel=constants.channel_str_to_num['main'])
    print('  Main channel connected')

    # Read session init
    msgs = recv_messages(main_client.sock, timeout=3.0)
    session_id = None
    for msg_type, payload in msgs:
        if msg_type == 103:  # INIT
            session_id = struct.unpack_from('<I', payload, 0)[0]
            mouse_mode = struct.unpack_from('<I', payload, 16)[0]
            print(f'  Session ID: {session_id}, mouse_mode: {mouse_mode}')
        elif msg_type == 104:  # CHANNELS_LIST
            num = struct.unpack_from('<I', payload, 0)[0]
            print(f'  {num} channels available')
    respond_to_pings(main_client.sock, msgs)

    # Request channel list
    main_client.sock.sendall(make_msg(104, b''))
    time.sleep(0.5)
    msgs = recv_messages(main_client.sock, timeout=1.0)
    respond_to_pings(main_client.sock, msgs)
    for msg_type, payload in msgs:
        if msg_type == 104:
            num = struct.unpack_from('<I', payload, 0)[0]
            print(f'  {num} channels available')

    if not session_id:
        print('No session ID received')
        return

    # Connect inputs channel
    print('Connecting inputs channel...')
    inputs_client = make_client(vv_path)
    inputs_client.connect(
        connection_id=session_id,
        channel=constants.channel_str_to_num['inputs']
    )
    print('  Inputs channel connected')

    msgs = recv_messages(inputs_client.sock, timeout=1.0)
    respond_to_pings(inputs_client.sock, msgs)
    for msg_type, payload in msgs:
        print(f'  inputs opcode {msg_type} ({len(payload)} bytes)')

    # Connect display channel
    print('Connecting display channel...')
    display_client = make_client(vv_path)
    display_client.connect(
        connection_id=session_id,
        channel=constants.channel_str_to_num['display']
    )
    print('  Display channel connected')

    # Send display init
    display_init = struct.pack('<BQBI', 1, 20*1024*1024, 1, 3*1024*1024)
    display_client.sock.sendall(make_msg(101, display_init))

    msgs = recv_messages(display_client.sock, timeout=2.0)
    respond_to_pings(display_client.sock, msgs)
    for msg_type, payload in msgs:
        if msg_type == 304:
            print(f'  Initial draw_copy: {len(payload)} bytes')
        elif msg_type == 3:
            gen, window = struct.unpack_from('<II', payload, 0)
            ack = make_msg(1, struct.pack('<I', gen))
            display_client.sock.sendall(ack)
            print(f'  SET_ACK gen={gen} window={window}')
        else:
            print(f'  display opcode {msg_type} ({len(payload)} bytes)')

    # Test 1: Mouse position + click
    print()
    print('=== TEST 1: Mouse click at (500, 400) ===')
    pos = make_msg(112, struct.pack('<IIIB', 500, 400, 0, 0))
    inputs_client.sock.sendall(pos)
    time.sleep(0.2)

    press = make_msg(113, struct.pack('<II', 1, 1))
    inputs_client.sock.sendall(press)
    time.sleep(0.15)

    release = make_msg(114, struct.pack('<II', 1, 0))
    inputs_client.sock.sendall(release)
    time.sleep(1.0)

    msgs = recv_messages(display_client.sock, timeout=2.0)
    respond_to_pings(display_client.sock, msgs)
    draw_count = sum(1 for t, _ in msgs if t == 304)
    print(f'  Display response: {len(msgs)} messages, {draw_count} draw_copy')
    for msg_type, payload in msgs:
        if msg_type not in (4, 3):  # skip ping/set_ack
            print(f'  opcode {msg_type} ({len(payload)} bytes)')

    # Test 2: Key press (space)
    print()
    print('=== TEST 2: Key press (space = 0x39) ===')
    inputs_client.sock.sendall(make_msg(101, struct.pack('<I', 0x39)))
    time.sleep(0.1)
    inputs_client.sock.sendall(make_msg(102, struct.pack('<I', 0xB9)))
    time.sleep(1.0)

    msgs = recv_messages(display_client.sock, timeout=2.0)
    respond_to_pings(display_client.sock, msgs)
    draw_count = sum(1 for t, _ in msgs if t == 304)
    print(f'  Display response: {len(msgs)} messages, {draw_count} draw_copy')
    for msg_type, payload in msgs:
        if msg_type not in (4, 3):
            print(f'  opcode {msg_type} ({len(payload)} bytes)')

    # Test 3: Multiple position updates then click
    print()
    print('=== TEST 3: Mouse move (100,100) -> (500,400) then click ===')
    for x in range(100, 501, 50):
        pos = make_msg(112, struct.pack('<IIIB', x, 400, 0, 0))
        inputs_client.sock.sendall(pos)
        time.sleep(0.02)

    time.sleep(0.3)
    # Position right before click
    pos = make_msg(112, struct.pack('<IIIB', 500, 400, 0, 0))
    inputs_client.sock.sendall(pos)
    time.sleep(0.1)

    press = make_msg(113, struct.pack('<II', 1, 1))
    inputs_client.sock.sendall(press)
    time.sleep(0.15)

    release = make_msg(114, struct.pack('<II', 1, 0))
    inputs_client.sock.sendall(release)
    time.sleep(1.0)

    msgs = recv_messages(display_client.sock, timeout=2.0)
    respond_to_pings(display_client.sock, msgs)
    draw_count = sum(1 for t, _ in msgs if t == 304)
    print(f'  Display response: {len(msgs)} messages, {draw_count} draw_copy')
    for msg_type, payload in msgs:
        if msg_type not in (4, 3):
            print(f'  opcode {msg_type} ({len(payload)} bytes)')

    print()
    print('Done.')


if __name__ == '__main__':
    main()

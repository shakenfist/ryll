#!/usr/bin/env python3
"""Drive the test guest over QMP: send keys, screendump after each settles, log wall-clock times.

Usage: qmp_drive.py <out dir> <key,key,...> [settle seconds] [--qmp SOCKET]

Writes <out dir>/shot-NN.ppm (shot-00 before the first key) and appends each command's
wall-clock time to <out dir>/qmp-log.txt, which lz4_check.py reads to pair draws with
screendumps. The default socket is the one `make test-qemu` creates (QEMU_QMP_SOCKET in the
Makefile).
"""
import argparse
import json
import socket
import time

DEFAULT_SOCK = '/tmp/ryll-test-qemu-qmp.sock'


class Qmp:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        self.f = self.s.makefile('rwb')
        self._read()
        self.cmd('qmp_capabilities')

    def _read(self):
        while True:
            line = self.f.readline()
            msg = json.loads(line)
            if 'event' in msg:
                continue
            return msg

    def cmd(self, name, **args):
        req = {'execute': name}
        if args:
            req['arguments'] = args
        self.f.write(json.dumps(req).encode() + b'\n')
        self.f.flush()
        r = self._read()
        if 'error' in r:
            raise RuntimeError(r)
        return r


def main():
    ap = argparse.ArgumentParser(description='Send keys to the test guest and screendump after each.')
    ap.add_argument('out_dir')
    ap.add_argument('keys', help='comma-separated QEMU qcodes, e.g. 1,2,3')
    ap.add_argument('settle', type=float, nargs='?', default=3.0, help='seconds to wait after each key')
    ap.add_argument('--qmp', default=DEFAULT_SOCK, help=f'QMP socket (default {DEFAULT_SOCK})')
    args = ap.parse_args()

    q = Qmp(args.qmp)
    with open(f'{args.out_dir}/qmp-log.txt', 'a') as log:
        # Initial screendump of whatever is on screen.
        path = f'{args.out_dir}/shot-00.ppm'
        t = time.time()
        q.cmd('screendump', filename=path)
        log.write(f'{t:.6f} screendump {path}\n')
        for i, k in enumerate(args.keys.split(','), start=1):
            t = time.time()
            q.cmd('send-key', keys=[{'type': 'qcode', 'data': k}])
            log.write(f'{t:.6f} send-key {k}\n')
            log.flush()
            time.sleep(args.settle)
            path = f'{args.out_dir}/shot-{i:02d}.ppm'
            t = time.time()
            q.cmd('screendump', filename=path)
            log.write(f'{t:.6f} screendump {path}\n')
            log.flush()


if __name__ == '__main__':
    main()

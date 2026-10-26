#!/usr/bin/env python3
"""Drive ryll --rdp with FreeRDP against the UEFI latency guest.

This is the driver for `.github/workflows/rdp-smoke.yml` and `make
rdp-smoke`. It runs inside the image tools/rdp-smoke/Dockerfile builds,
which carries QEMU, OVMF, Xvfb, FreeRDP, xdotool and xwd; the Makefile
mounts the ryll binary (built with the `rdp` feature), the guest image and
this script:

    tools/rdp-smoke.py --ryll /smoke/ryll --guest /smoke/guest.qcow2 \\
        --workdir /smoke/logs

It starts Xvfb, then QEMU booting the guest with SPICE on a loopback port
it picks, then `ryll --rdp --rdp-port 0 --direct` against that port, then
FreeRDP under Xvfb against ryll. Any keypress in the guest advances a fixed
eight-colour cycle, one step of which is black, so the colour at the
centre of FreeRDP's window is the oracle: it can only change if a key went
all the way from FreeRDP through ryll and SPICE to the guest and the
guest's new frame came all the way back.

Seven checks, strictly in order, each building on the state the one
before left:

  authenticate      FreeRDP connects with the password and ryll logs that
                    it authenticated the client.
  frame             the window's centre turns non-black, and matches the
                    guest's own screen as QEMU dumps it.
  input             a key typed into FreeRDP changes the centre colour to
                    the guest's new colour.
  client-reconnect  FreeRDP is killed and started again; it is served,
                    and input works through it.
  takeover          a second FreeRDP connects; the first is disconnected,
                    and input works through the second.
  spice-reconnect   QEMU is stopped and started again on the same port.
                    The client stays connected and sees its last frame
                    darkened, then the new guest's frame, and input works.
  ctrl-c            SIGINT to ryll with the client connected: ryll exits 0
                    within 5 s and FreeRDP is disconnected.

Once a check fails the rest are skipped, since each needs the state the
failed one should have left.

A failure the environment caused -- Xvfb not starting, QEMU not starting
or its guest not booting, QEMU restarting too slowly to land inside ryll's
retries, a tool missing -- is reported as FAIL (harness),
not as a ryll defect. The guest's own screen (QMP screendump) is checked
before ryll is involved, so a guest that never boots is a harness failure.

Oracles are ryll's stdout and log lines, and pixels. The log lines' exact
text is in the constants below, each naming the source line it must stay
in step with. Every check but `authenticate` is decided by pixels and
process state, so a reworded log line fails safe there.

SECRETS. The RDP password is a fixed test value, but it is still kept out
of everything this script saves: ryll prints it to stdout at launch, so
each process's output is written to a private directory first and only a
copy with the password redacted is saved under --workdir. The script never
prints it, nor a command line that carries it.

Summary lines, one per check, start with "[rdp-smoke] <check>: PASS",
"FAIL", "FAIL (harness)" or "SKIP". Exit status: 0 when every check
passed, 1 otherwise, 2 on a usage error.

Python 3.9+, standard library only.
"""

import argparse
import json
import os
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time


PREFIX = '[rdp-smoke]'

CHECKS = ('authenticate', 'frame', 'input', 'client-reconnect', 'takeover', 'spice-reconnect', 'ctrl-c')

# A fixed password: the lane needs to type it into FreeRDP, and it guards
# a loopback port in a container with no network. Still never printed.
PASSWORD = 'rdp-smoke-test-password'
USERNAME = 'ryll'

OVMF_CODE = '/usr/share/OVMF/OVMF_CODE_4M.fd'
OVMF_VARS = '/usr/share/OVMF/OVMF_VARS_4M.fd'

XVFB_SCREEN = '1920x1200x24'

# ── Timing ──────────────────────────────────────────────────────────────────
#
# Generous: the runners may have no /dev/kvm, and QEMU then falls back to
# TCG, which boots OVMF far more slowly.

XVFB_BOUND = 15.0
# QEMU's SPICE port must accept connections within this long of its start.
QEMU_LISTEN_BOUND = 30.0
# The guest's own screen must show a colour within this long of QEMU's
# start: OVMF, then the guest application.
GUEST_BOOT_BOUND = 180.0
# ryll must print its RDP address within this long of its launch.
RYLL_READY_BOUND = 30.0
# FreeRDP must be authenticated within this long of its start.
AUTH_BOUND = 45.0
# A first frame must reach FreeRDP's window within this long of
# authentication.
FRAME_BOUND = 45.0
# A key's effect must reach FreeRDP's window within this long.
INPUT_BOUND = 20.0
# A taken-over FreeRDP must exit within this long of the new one
# authenticating.
TAKEOVER_EXIT_BOUND = 20.0
# ryll's last SPICE retry comes this long after the session is lost: it
# retries after 1, 4 and 16 s (RECONNECT_BACKOFF_SECS in
# ryll/src/reconnect_policy.rs), then gives up. QEMU must be listening
# again before then, so the outage the spice-reconnect check stages has to
# fit inside it, with a margin for ryll noticing the loss.
RETRY_WINDOW = 1.0 + 4.0 + 16.0
RETRY_MARGIN = 3.0
# The darkened frame must reach the client within this long of QEMU
# stopping. ryll darkens it within a second; the bound is kept short so
# that, with QEMU's restart, the outage stays inside RETRY_WINDOW.
DARK_BOUND = 8.0
# After QEMU restarts, the new guest's frame must reach the client within
# this long: ryll's next retry (see RETRY_WINDOW), then the guest booting
# again.
RECOVER_BOUND = GUEST_BOOT_BOUND + 30.0
# ryll must exit within this long of SIGINT.
STOP_BOUND = 5.0
# FreeRDP must notice that ryll has gone within this long of ryll exiting.
CLIENT_GONE_BOUND = 15.0
# How long a frame must hold still before its colour is believed: a
# colour read mid-update is neither the old one nor the new one.
SETTLE = 0.5
POLL = 0.25

# ── Colours ─────────────────────────────────────────────────────────────────

# Per-channel slack when comparing colours: RDP codecs may round.
COLOUR_TOLERANCE = 24
# A colour with no channel above this is black.
BLACK_MAX = 24

# ── ryll's output ───────────────────────────────────────────────────────────
#
# ryll's tracing output goes to stdout with its println!s: `<timestamp>
# <LEVEL> <message> <fields>`, with no target (ryll/src/main.rs calls
# `with_target(false)`). NO_COLOR is set for ryll, and ANSI escapes are
# stripped anyway.

# Printed by run_rdp (ryll/src/rdp.rs) once the listener is bound:
# "RDP: {local_addr}".
RDP_ADDRESS_RE = re.compile(r'^RDP: (?P<host>[0-9.]+):(?P<port>[0-9]+)\s*$')

# Logged at INFO by ProviderValidator::validate
# (shakenfist-spice-rdp/src/auth.rs) once a client's credentials are
# accepted: "RDP client authenticated", with username and session fields.
AUTHENTICATED_RE = re.compile(r'\bINFO\s+RDP client authenticated\b')

# Logged at INFO by Connection::take_over (shakenfist-spice-rdp/src/
# server.rs) when an authenticated client displaces another. A note only:
# the takeover check is decided by FreeRDP's exit and by input.
TAKEOVER_RE = re.compile(r'\bINFO\s+RDP client took the session over\b')

# Logged at INFO by SpiceAttempts::run (ryll/src/rdp.rs) at the start of
# every SPICE attempt: "rdp: SPICE attempt {}: connecting". A note only.
ATTEMPT_RE = re.compile(r'\bINFO\s+rdp: SPICE attempt (?P<number>[0-9]+): connecting\b')

# Printed by run_rdp (ryll/src/rdp.rs) beside the address: "password:
# {password}", on stdout only, never through the tracing pipeline. The only
# place ryll may print the password.
LAUNCH_PASSWORD_RE = re.compile(r'^password: ')

ANSI_RE = re.compile(r'\x1b\[[0-9;]*[A-Za-z]')


class HarnessError(Exception):
    """The environment, not ryll, could not do what the check needs."""


class CheckFailed(Exception):
    """ryll did not do what the check needs; the message says what."""


def log(message):
    print(f'{PREFIX} {message}', flush=True)


# ── Pure helpers (tested by tools/test-rdp-smoke.py) ────────────────────────

def is_black(colour):
    return max(colour) <= BLACK_MAX


def same_colour(a, b, tolerance=COLOUR_TOLERANCE):
    return all(abs(x - y) <= tolerance for x, y in zip(a, b))


def is_darkened(before, after, tolerance=COLOUR_TOLERANCE):
    """Whether after is before at half brightness.

    The darkened frame halves each channel (darken in
    shakenfist-spice-rdp/src/display.rs). A black frame darkens to black,
    which shows nothing, so callers make sure before is not black.
    """
    return all(abs(x // 2 - y) <= tolerance for x, y in zip(before, after))


def fmt_colour(colour):
    if colour is None:
        return 'none'
    return '#{:02x}{:02x}{:02x}'.format(*colour)


def _mask_value(pixel, mask):
    if not mask:
        return 0
    shift = (mask & -mask).bit_length() - 1
    width = bin(mask).count('1')
    value = (pixel & mask) >> shift
    if width == 8:
        return value
    return (value * 255) // ((1 << width) - 1)


def xwd_centre(data):
    """Return (width, height, (r, g, b)) at the centre of an xwd dump.

    The header is 25 big-endian CARD32s (XWDFileHeader in X11/XWDFile.h),
    then the window name, then ncolors 12-byte XWDColor entries, then the
    pixels in the header's byte order. Only the TrueColor and DirectColor
    ZPixmaps that Xvfb's 24-bit screen produces are handled.
    """
    if len(data) < 100:
        raise ValueError(f'an xwd dump of {len(data)} bytes is shorter than its header')
    fields = struct.unpack('>25I', data[:100])
    header_size, version, pixmap_format = fields[0], fields[1], fields[2]
    width, height = fields[4], fields[5]
    byte_order, bits_per_pixel, bytes_per_line = fields[7], fields[11], fields[12]
    visual_class = fields[13]
    masks = fields[14:17]
    ncolors = fields[19]
    if version != 7 or pixmap_format != 2:
        raise ValueError(f'unsupported xwd dump: version {version}, format {pixmap_format}')
    if visual_class not in (4, 5) or bits_per_pixel not in (24, 32):
        raise ValueError(f'unsupported xwd visual: class {visual_class}, {bits_per_pixel} bits per pixel')
    if width == 0 or height == 0:
        raise ValueError('the xwd dump is empty')
    x, y = width // 2, height // 2
    step = bits_per_pixel // 8
    offset = header_size + ncolors * 12 + y * bytes_per_line + x * step
    raw = data[offset:offset + step]
    if len(raw) != step:
        raise ValueError('the xwd dump is shorter than its header says')
    pixel = int.from_bytes(raw, 'little' if byte_order == 0 else 'big')
    return width, height, tuple(_mask_value(pixel, m) for m in masks)


def ppm_centre(data):
    """Return (width, height, (r, g, b)) at the centre of a binary PPM.

    QMP's screendump writes "P6\\n<w> <h>\\n255\\n" and the pixels.
    """
    tokens = []
    pos = 0
    while len(tokens) < 4:
        while pos < len(data) and data[pos:pos + 1].isspace():
            pos += 1
        if data[pos:pos + 1] == b'#':
            while pos < len(data) and data[pos:pos + 1] != b'\n':
                pos += 1
            continue
        start = pos
        while pos < len(data) and not data[pos:pos + 1].isspace():
            pos += 1
        if start == pos:
            raise ValueError('the PPM header is truncated')
        tokens.append(data[start:pos])
    if tokens[0] != b'P6' or tokens[3] != b'255':
        raise ValueError('not an 8-bit binary PPM')
    width, height = int(tokens[1]), int(tokens[2])
    pixels = pos + 1
    if width == 0 or height == 0:
        raise ValueError('the PPM is empty')
    offset = pixels + ((height // 2) * width + width // 2) * 3
    raw = data[offset:offset + 3]
    if len(raw) != 3:
        raise ValueError('the PPM is shorter than its header says')
    return width, height, tuple(raw)


def rdp_address(lines):
    """The (host, port) ryll printed, or None."""
    for line in lines:
        m = RDP_ADDRESS_RE.match(line)
        if m:
            return m.group('host'), int(m.group('port'))
    return None


def count_matching(lines, regex):
    return sum(1 for line in lines if regex.search(line))


def redact(text, secret):
    """Return (text with secret replaced, how many times it appeared)."""
    count = text.count(secret)
    return text.replace(secret, '<redacted>'), count


def password_leaks(name, text):
    """How many times a process printed the password where it should not.

    ryll prints it once, by design, on its launch line; nothing else
    should print it anywhere.
    """
    total = text.count(PASSWORD)
    if name == 'ryll':
        total -= sum(line.count(PASSWORD) for line in text.splitlines() if LAUNCH_PASSWORD_RE.match(line))
    return total


def summary_line(name, status, elapsed, problems, notes):
    """The summary line for one check; status is PASS, FAIL, HARNESS or SKIP."""
    if status == 'PASS':
        return f'{PREFIX} {name}: PASS in {elapsed:.1f}s -- {"; ".join(notes)}'
    if status == 'SKIP':
        return f'{PREFIX} {name}: SKIP -- an earlier check failed'
    kind = 'FAIL (harness)' if status == 'HARNESS' else 'FAIL'
    detail = '; '.join(problems)
    if notes:
        detail += ' [' + '; '.join(notes) + ']'
    return f'{PREFIX} {name}: {kind} in {elapsed:.1f}s -- {detail}'


# ── Processes ───────────────────────────────────────────────────────────────

class Process:
    """A child process whose output goes to a private log file.

    Saved, with the password redacted, under the workdir by save().
    """

    def __init__(self, harness, name, cmd, env=None):
        self.name = name
        self.raw_log = os.path.join(harness.private, f'{name}.log')
        self.log = os.path.join(harness.workdir, f'{name}.log')
        out = open(self.raw_log, 'wb')
        try:
            self.proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT,
                                         env=env or harness.env, start_new_session=True)
        except FileNotFoundError as e:
            raise HarnessError(f'cannot run {cmd[0]}: {e}')
        finally:
            out.close()
        self.started = time.monotonic()

    @property
    def pid(self):
        return self.proc.pid

    def running(self):
        return self.proc.poll() is None

    def lines(self):
        try:
            with open(self.raw_log, 'rb') as f:
                text = f.read().decode('utf-8', errors='replace')
        except FileNotFoundError:
            return []
        return [ANSI_RE.sub('', line) for line in text.splitlines()]

    def wait(self, bound):
        try:
            return self.proc.wait(timeout=bound)
        except subprocess.TimeoutExpired:
            return None

    def kill(self):
        """Kill the process's whole session, and reap it."""
        if self.proc.poll() is None:
            try:
                os.killpg(self.proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            self.proc.wait()

    def save(self):
        """Copy the log, password redacted, to the workdir; return a problem or None."""
        try:
            with open(self.raw_log, 'rb') as f:
                text = f.read().decode('utf-8', errors='replace')
        except FileNotFoundError:
            return None
        leaks = password_leaks(self.name, text)
        text, _ = redact(text, PASSWORD)
        with open(self.log, 'w', encoding='utf-8') as f:
            f.write(ANSI_RE.sub('', text))
        if leaks:
            return f'{self.name} printed the password {leaks} time(s) where it should not; redacted from the saved log'
        return None

    def tail(self, count=15):
        lines = [line for line in self.lines() if line.strip()]
        out = []
        for line in lines[-count:]:
            out.append(redact(line, PASSWORD)[0])
        return out


class Qmp:
    """A minimal QMP client: greeting, capabilities, one command at a time."""

    def __init__(self, path, timeout=10.0):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self.buf = bytearray()
        self._read()
        self.execute('qmp_capabilities')

    def close(self):
        self.sock.close()

    def _read(self):
        while b'\n' not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError('QEMU closed its QMP socket')
            self.buf.extend(chunk)
        nl = self.buf.index(b'\n')
        line = bytes(self.buf[:nl])
        del self.buf[:nl + 1]
        return json.loads(line.decode('utf-8'))

    def execute(self, command, arguments=None):
        msg = {'execute': command}
        if arguments:
            msg['arguments'] = arguments
        self.sock.sendall(json.dumps(msg).encode('utf-8') + b'\n')
        while True:
            reply = self._read()
            if 'event' in reply:
                continue
            if 'error' in reply:
                raise RuntimeError(f'QMP {command}: {reply["error"].get("desc")}')
            return reply.get('return')


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def port_open(port):
    try:
        with socket.create_connection(('127.0.0.1', port), timeout=1.0):
            return True
    except OSError:
        return False


def tool_version(cmd):
    try:
        out = subprocess.run(cmd, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.TimeoutExpired):
        return 'unknown'
    text = (out.stdout or out.stderr).strip().splitlines()
    return text[0] if text else 'unknown'


class Harness:
    """Xvfb, QEMU, ryll and the FreeRDP clients, and their cleanup."""

    def __init__(self, args):
        self.args = args
        self.workdir = args.workdir
        # Sockets and raw logs: short paths (a Unix socket path must be
        # under 108 bytes) and never uploaded.
        self.private = tempfile.mkdtemp(prefix='rdps-')
        self.env = dict(os.environ, NO_COLOR='1', HOME=self.private)
        self.display = None
        self.xvfb = None
        self.qemu = None
        self.qemu_starts = 0
        self.qmp_path = os.path.join(self.private, 'qmp.sock')
        self.spice_port = free_port()
        self.accelerator = 'unknown'
        self.ryll = None
        self.rdp = None
        self.clients = {}
        self.client_starts = 0

    # Xvfb.

    def start_xvfb(self):
        read_fd, write_fd = os.pipe()
        try:
            out = open(os.path.join(self.private, 'xvfb.log'), 'wb')
            try:
                proc = subprocess.Popen(
                    ['Xvfb', '-displayfd', str(write_fd), '-screen', '0', XVFB_SCREEN, '-nolisten', 'tcp'],
                    stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT, pass_fds=(write_fd,),
                    start_new_session=True)
            except FileNotFoundError as e:
                raise HarnessError(f'cannot run Xvfb: {e}')
            finally:
                out.close()
            os.close(write_fd)
            write_fd = None
            self.xvfb = proc
            deadline = time.monotonic() + XVFB_BOUND
            number = b''
            while time.monotonic() < deadline:
                ready = _read_ready(read_fd, deadline - time.monotonic())
                if not ready:
                    break
                chunk = os.read(read_fd, 64)
                if not chunk:
                    break
                number += chunk
                if number.endswith(b'\n'):
                    break
            if not number.strip().isdigit():
                raise HarnessError(f'Xvfb did not report a display within {XVFB_BOUND:.0f} s')
            self.display = ':' + number.strip().decode()
            self.env['DISPLAY'] = self.display
        finally:
            if write_fd is not None:
                os.close(write_fd)
            os.close(read_fd)

    # QEMU.

    def start_qemu(self):
        """Start QEMU on the lane's SPICE port; return once the port listens."""
        self.qemu_starts += 1
        n = self.qemu_starts
        vars_copy = os.path.join(self.private, f'ovmf-vars-{n}.fd')
        shutil.copyfile(OVMF_VARS, vars_copy)
        if os.path.exists(self.qmp_path):
            os.unlink(self.qmp_path)
        # make test-qemu's guest (Makefile), less -daemonize, with its own
        # port and paths, and an overlay so the mounted image is never
        # written.
        cmd = [
            'qemu-system-x86_64',
            '-accel', 'kvm', '-accel', 'tcg',
            '-display', 'none',
            '-machine', 'q35',
            '-m', '128',
            '-drive', f'if=pflash,format=raw,readonly=on,file={OVMF_CODE}',
            '-drive', f'if=pflash,format=raw,file={vars_copy}',
            '-drive', f'file={self.args.guest},format=qcow2,if=virtio,snapshot=on',
            '-vga', 'qxl',
            '-spice', f'port={self.spice_port},addr=127.0.0.1,disable-ticketing=on',
            '-qmp', f'unix:{self.qmp_path},server=on,wait=off',
        ]
        self.qemu = Process(self, f'qemu-{n}', cmd)
        deadline = time.monotonic() + QEMU_LISTEN_BOUND
        while time.monotonic() < deadline:
            if not self.qemu.running():
                tail = ' | '.join(self.qemu.tail(5))
                raise HarnessError(f'QEMU exited with status {self.qemu.proc.returncode} at start: {tail}')
            if port_open(self.spice_port) and os.path.exists(self.qmp_path):
                break
            time.sleep(POLL)
        else:
            raise HarnessError(f'QEMU did not listen on its SPICE port within {QEMU_LISTEN_BOUND:.0f} s')
        try:
            qmp = Qmp(self.qmp_path)
            try:
                kvm = qmp.execute('query-kvm') or {}
            finally:
                qmp.close()
            self.accelerator = 'kvm' if kvm.get('enabled') else 'tcg'
        except (OSError, EOFError, RuntimeError, ValueError) as e:
            raise HarnessError(f'QEMU\'s QMP socket did not answer: {e}')

    def stop_qemu(self):
        """Stop QEMU as an outage would: abruptly."""
        if self.qemu is not None:
            self.qemu.kill()
            self.qemu.save()

    def guest_colour(self):
        """The colour at the centre of the guest's own screen, or None."""
        path = os.path.join(self.private, 'screendump.ppm')
        try:
            qmp = Qmp(self.qmp_path)
            try:
                qmp.execute('screendump', {'filename': path})
            finally:
                qmp.close()
            with open(path, 'rb') as f:
                return ppm_centre(f.read())[2]
        except (OSError, EOFError, RuntimeError, ValueError):
            return None

    def wait_guest_booted(self):
        """Wait for the guest's own screen to show a colour; return it."""
        deadline = time.monotonic() + GUEST_BOOT_BOUND
        colour = None
        while time.monotonic() < deadline:
            if not self.qemu.running():
                raise HarnessError(f'QEMU exited with status {self.qemu.proc.returncode} while booting')
            colour = self.guest_colour()
            if colour is not None and not is_black(colour):
                return colour
            time.sleep(1.0)
        raise HarnessError(f'the guest showed no colour within {GUEST_BOOT_BOUND:.0f} s of QEMU starting '
                           f'(last {fmt_colour(colour)}, accelerator {self.accelerator})')

    # ryll.

    def start_ryll(self):
        cmd = [self.args.ryll, '--rdp', '--rdp-port', '0', '--rdp-user', USERNAME, '--rdp-password', PASSWORD,
               '--direct', f'127.0.0.1:{self.spice_port}']
        self.ryll = Process(self, 'ryll', cmd)
        deadline = time.monotonic() + RYLL_READY_BOUND
        while time.monotonic() < deadline:
            self.rdp = rdp_address(self.ryll.lines())
            if self.rdp:
                return
            if not self.ryll.running():
                raise CheckFailed(f'ryll exited with status {self.ryll.proc.returncode} before printing '
                                  'its RDP address')
            time.sleep(POLL)
        raise CheckFailed(f'ryll printed no "RDP: <address>" line within {RYLL_READY_BOUND:.0f} s')

    def require_ryll(self):
        if self.ryll is None or not self.ryll.running():
            rc = self.ryll.proc.returncode if self.ryll else None
            raise CheckFailed(f'ryll is not running (exit status {rc})')

    def authenticated(self):
        return count_matching(self.ryll.lines(), AUTHENTICATED_RE)

    # FreeRDP.

    def start_client(self):
        """Start a FreeRDP client; return its name, which is also its window title."""
        self.client_starts += 1
        name = f'freerdp-{self.client_starts}'
        host, port = self.rdp
        cmd = ['xfreerdp3', f'/v:{host}:{port}', f'/u:{USERNAME}', f'/p:{PASSWORD}', '/cert:ignore',
               f'/t:rdp-smoke-{name}', '/log-level:INFO', '-auto-reconnect']
        self.clients[name] = Process(self, name, cmd)
        return name

    def client(self, name):
        return self.clients[name]

    def window(self, name):
        """FreeRDP's window id, or None while it has none."""
        try:
            out = subprocess.run(['xdotool', 'search', '--name', f'^rdp-smoke-{name}$'], env=self.env,
                                 stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)
        except FileNotFoundError as e:
            raise HarnessError(f'cannot run xdotool: {e}')
        except subprocess.TimeoutExpired:
            return None
        ids = out.stdout.split()
        return ids[-1] if ids else None

    def client_colour(self, name):
        """The colour at the centre of a client's window, or None."""
        wid = self.window(name)
        if wid is None:
            return None
        try:
            out = subprocess.run(['xwd', '-silent', '-id', wid], env=self.env, stdin=subprocess.DEVNULL,
                                 capture_output=True, timeout=10)
        except FileNotFoundError as e:
            raise HarnessError(f'cannot run xwd: {e}')
        except subprocess.TimeoutExpired:
            return None
        if out.returncode != 0:
            return None
        try:
            return xwd_centre(out.stdout)[2]
        except ValueError:
            return None

    def settled_colour(self, name, bound):
        """A colour that held for SETTLE seconds, or None within bound."""
        deadline = time.monotonic() + bound
        last = None
        since = time.monotonic()
        while time.monotonic() < deadline:
            colour = self.client_colour(name)
            if colour is None or last is None or not same_colour(colour, last, 0):
                last = colour
                since = time.monotonic()
            elif time.monotonic() - since >= SETTLE:
                return colour
            time.sleep(POLL / 2)
        return None

    def send_key(self, name):
        """Type a space into a client's window through XTEST, as a user would."""
        wid = self.window(name)
        if wid is None:
            raise CheckFailed(f'{name} has no window to type into')
        try:
            subprocess.run(['xdotool', 'windowfocus', wid, 'key', 'space'], env=self.env,
                           stdin=subprocess.DEVNULL, capture_output=True, timeout=10, check=True)
        except FileNotFoundError as e:
            raise HarnessError(f'cannot run xdotool: {e}')
        except (subprocess.TimeoutExpired, subprocess.CalledProcessError) as e:
            raise HarnessError(f'xdotool could not type into {name}\'s window: {e}')

    # Cleanup.

    def cleanup(self):
        """Stop everything this harness started, and save the logs."""
        problems = []
        procs = list(self.clients.values()) + [p for p in (self.ryll, self.qemu) if p is not None]
        for proc in procs:
            proc.kill()
            problem = proc.save()
            if problem:
                problems.append(problem)
        if self.xvfb is not None and self.xvfb.poll() is None:
            try:
                os.killpg(self.xvfb.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            self.xvfb.wait()
        xvfb_log = os.path.join(self.private, 'xvfb.log')
        if os.path.exists(xvfb_log):
            shutil.copyfile(xvfb_log, os.path.join(self.workdir, 'xvfb.log'))
        shutil.rmtree(self.private, ignore_errors=True)
        return problems


def _read_ready(fd, timeout):
    ready, _, _ = select.select([fd], [], [], max(0.0, timeout))
    return bool(ready)


# ── The checks ──────────────────────────────────────────────────────────────

class Check:
    def __init__(self, name):
        self.name = name
        self.notes = []
        self.start = time.monotonic()

    def note(self, message):
        self.notes.append(message)


def wait_authenticated(h, client, before):
    """Wait for ryll to log one more authentication than before."""
    deadline = time.monotonic() + AUTH_BOUND
    while time.monotonic() < deadline:
        h.require_ryll()
        if h.authenticated() > before:
            return
        if not h.client(client).running():
            tail = ' | '.join(h.client(client).tail(5))
            raise CheckFailed(f'{client} exited with status {h.client(client).proc.returncode} before ryll '
                              f'authenticated it: {tail}')
        time.sleep(POLL)
    raise CheckFailed(f'ryll logged no "RDP client authenticated" for {client} within {AUTH_BOUND:.0f} s')


def wait_frame(h, client, check, bound=FRAME_BOUND):
    """Wait for the client's centre to show the guest's colour, not black."""
    deadline = time.monotonic() + bound
    colour = guest = None
    while time.monotonic() < deadline:
        h.require_ryll()
        if not h.client(client).running():
            raise CheckFailed(f'{client} exited with status {h.client(client).proc.returncode} while waiting '
                              'for a frame')
        colour = h.settled_colour(client, max(SETTLE * 4, deadline - time.monotonic()))
        guest = h.guest_colour()
        if colour is not None and guest is not None and not is_black(colour) and same_colour(colour, guest):
            check.note(f'{client} shows {fmt_colour(colour)}, as the guest does')
            return colour
        time.sleep(POLL)
    raise CheckFailed(f'{client}\'s centre did not show the guest\'s colour within {bound:.0f} s '
                      f'(client {fmt_colour(colour)}, guest {fmt_colour(guest)})')


def type_and_see(h, client, check):
    """Type a key into the client and wait for the guest's next colour to come back."""
    before = h.settled_colour(client, INPUT_BOUND)
    if before is None:
        raise CheckFailed(f'{client}\'s window did not hold a colour long enough to read')
    h.send_key(client)
    deadline = time.monotonic() + INPUT_BOUND
    colour = guest = None
    while time.monotonic() < deadline:
        h.require_ryll()
        if not h.client(client).running():
            raise CheckFailed(f'{client} exited with status {h.client(client).proc.returncode} after a key')
        colour = h.settled_colour(client, max(SETTLE * 4, deadline - time.monotonic()))
        if colour is not None and not same_colour(colour, before):
            guest = h.guest_colour()
            if guest is not None and same_colour(colour, guest):
                check.note(f'key moved {client} from {fmt_colour(before)} to {fmt_colour(colour)}')
                return colour
        time.sleep(POLL)
    guest = h.guest_colour()
    raise CheckFailed(f'a key typed into {client} did not change its centre from {fmt_colour(before)} '
                      f'within {INPUT_BOUND:.0f} s (now {fmt_colour(colour)}, guest {fmt_colour(guest)})')


def wait_exit(proc, bound):
    deadline = time.monotonic() + bound
    while time.monotonic() < deadline:
        if not proc.running():
            return proc.proc.returncode
        time.sleep(POLL)
    return None


class Smoke:
    def __init__(self, harness):
        self.h = harness
        self.current = None

    def check_authenticate(self, check):
        h = self.h
        h.start_ryll()
        check.note(f'ryll listening on {h.rdp[0]}:{h.rdp[1]}')
        self.current = h.start_client()
        wait_authenticated(h, self.current, 0)
        check.note(f'{self.current} authenticated')

    def check_frame(self, check):
        wait_frame(self.h, self.current, check)

    def check_input(self, check):
        type_and_see(self.h, self.current, check)

    def check_client_reconnect(self, check):
        h = self.h
        old = self.current
        h.client(old).kill()
        h.client(old).save()
        check.note(f'{old} killed')
        before = h.authenticated()
        self.current = h.start_client()
        wait_authenticated(h, self.current, before)
        wait_frame(h, self.current, check)
        type_and_see(h, self.current, check)

    def check_takeover(self, check):
        h = self.h
        old = self.current
        before = h.authenticated()
        self.current = h.start_client()
        wait_authenticated(h, self.current, before)
        rc = wait_exit(h.client(old), TAKEOVER_EXIT_BOUND)
        if rc is None:
            raise CheckFailed(f'{old} was still connected {TAKEOVER_EXIT_BOUND:.0f} s after {self.current} '
                              'authenticated')
        h.client(old).save()
        check.note(f'{old} exited ({rc})')
        if count_matching(h.ryll.lines(), TAKEOVER_RE):
            check.note('ryll logged the takeover')
        wait_frame(h, self.current, check)
        type_and_see(h, self.current, check)

    def check_spice_reconnect(self, check):
        h = self.h
        client = self.current
        # A black frame darkens to black, which proves nothing.
        last = h.settled_colour(client, INPUT_BOUND)
        if last is None or is_black(last):
            last = type_and_see(h, client, check)
        attempts = count_matching(h.ryll.lines(), ATTEMPT_RE)
        h.stop_qemu()
        stopped = time.monotonic()
        deadline = stopped + DARK_BOUND
        colour = None
        while time.monotonic() < deadline:
            h.require_ryll()
            if not h.client(client).running():
                raise CheckFailed(f'{client} was disconnected when SPICE went down '
                                  f'(exit status {h.client(client).proc.returncode})')
            colour = h.settled_colour(client, max(SETTLE * 4, deadline - time.monotonic()))
            if colour is not None and is_darkened(last, colour):
                break
            time.sleep(POLL)
        else:
            raise CheckFailed(f'{client}\'s centre did not darken from {fmt_colour(last)} within '
                              f'{DARK_BOUND:.0f} s of QEMU stopping (now {fmt_colour(colour)})')
        check.note(f'{fmt_colour(last)} darkened to {fmt_colour(colour)} {time.monotonic() - stopped:.1f}s '
                   'after QEMU stopped')
        h.start_qemu()
        restarted = time.monotonic()
        # A QEMU that came back after ryll's last retry says nothing about
        # ryll: it gave up, as it should. That is the runner being slow.
        outage = restarted - stopped
        if outage > RETRY_WINDOW - RETRY_MARGIN:
            raise HarnessError(f'QEMU took {outage:.1f}s to listen again after it was stopped, too close to '
                               f'ryll\'s last retry {RETRY_WINDOW:.0f} s after the loss to judge the reconnect')
        check.note(f'QEMU listened again {outage:.1f}s after it was stopped')
        wait_frame(h, client, check, RECOVER_BOUND)
        check.note(f'new guest frame {time.monotonic() - restarted:.1f}s after QEMU restarted')
        more = count_matching(h.ryll.lines(), ATTEMPT_RE) - attempts
        check.note(f'{more} SPICE attempt(s) after the outage')
        type_and_see(h, client, check)

    def check_ctrl_c(self, check):
        h = self.h
        client = self.current
        h.require_ryll()
        if not h.client(client).running():
            raise CheckFailed(f'{client} was not connected when the check began')
        start = time.monotonic()
        h.ryll.proc.send_signal(signal.SIGINT)
        rc = h.ryll.wait(STOP_BOUND)
        took = time.monotonic() - start
        if rc is None:
            raise CheckFailed(f'ryll did not exit within {STOP_BOUND:.0f} s of SIGINT')
        if rc != 0:
            raise CheckFailed(f'ryll exited with status {rc} {took:.1f}s after SIGINT')
        check.note(f'ryll exited 0 {took:.2f}s after SIGINT')
        crc = wait_exit(h.client(client), CLIENT_GONE_BOUND)
        if crc is None:
            raise CheckFailed(f'{client} was still running {CLIENT_GONE_BOUND:.0f} s after ryll exited')
        check.note(f'{client} exited ({crc})')

    def run(self, name, check):
        getattr(self, 'check_' + name.replace('-', '_'))(check)


def run_checks(args):
    os.makedirs(args.workdir, exist_ok=True)
    started = time.monotonic()
    h = Harness(args)
    smoke = Smoke(h)
    lines = []
    failed = False
    try:
        log(f'ryll: {tool_version([args.ryll, "--version"])}')
        log(f'FreeRDP: {tool_version(["xfreerdp3", "--version"])}')
        log(f'QEMU: {tool_version(["qemu-system-x86_64", "--version"])}')
        log(f'/dev/kvm: {"present" if os.path.exists("/dev/kvm") else "absent"}')
        setup = Check('setup')
        try:
            h.start_xvfb()
            h.start_qemu()
            colour = h.wait_guest_booted()
            log(f'setup: Xvfb on {h.display}; guest booted to {fmt_colour(colour)} in '
                f'{time.monotonic() - setup.start:.1f}s with {h.accelerator}; SPICE on port {h.spice_port}')
        except (HarnessError, OSError) as e:
            failed = True
            line = summary_line('setup', 'HARNESS', time.monotonic() - setup.start, [f'HARNESS: {e}'], [])
            print(line, flush=True)
            lines.append(line)
        for name in args.checks:
            if failed:
                line = summary_line(name, 'SKIP', 0, [], [])
            else:
                check = Check(name)
                log(f'{name}: starting')
                status, problems = 'PASS', []
                try:
                    smoke.run(name, check)
                except HarnessError as e:
                    status, problems = 'HARNESS', [f'HARNESS: {e}']
                except CheckFailed as e:
                    status, problems = 'FAIL', [str(e)]
                except Exception as e:
                    status, problems = 'FAIL', [f'the driver raised {type(e).__name__}: {e}']
                if status != 'PASS':
                    failed = True
                    if h.ryll is not None:
                        log('  last lines of ryll\'s output:')
                        for out in h.ryll.tail():
                            print(f'    {out}', flush=True)
                line = summary_line(name, status, time.monotonic() - check.start, problems, check.notes)
            print(line, flush=True)
            lines.append(line)
    finally:
        for problem in h.cleanup():
            failed = True
            log(problem)
    passed = sum(1 for line in lines if ': PASS ' in line)
    log(f'{passed}/{len(args.checks)} checks passed; the driver ran for {time.monotonic() - started:.1f}s')
    for line in lines:
        print(line, flush=True)
    return 1 if failed else 0


def parse_args(argv):
    parser = argparse.ArgumentParser(
        description='Run the RDP smoke lane: FreeRDP against ryll --rdp against the UEFI latency guest.')
    parser.add_argument('--ryll', required=True, help='the ryll binary under test, built with the rdp feature')
    parser.add_argument('--guest', required=True, help='the UEFI latency guest qcow2; never written')
    parser.add_argument('--workdir', required=True, help='where the (redacted) logs are saved')
    parser.add_argument('--checks', default=','.join(CHECKS),
                        help=f'run the checks up to the last of these named (from {",".join(CHECKS)})')
    args = parser.parse_args(argv)
    names = [c.strip() for c in args.checks.split(',') if c.strip()]
    if not names or any(c not in CHECKS for c in names):
        parser.error(f'--checks must name some of {", ".join(CHECKS)}')
    # Each check needs the state the ones before it leave, so the
    # selection runs everything up to the last one named.
    args.checks = list(CHECKS[:max(CHECKS.index(c) for c in names) + 1])
    if not os.access(args.ryll, os.X_OK):
        parser.error(f'--ryll {args.ryll} is not executable')
    if not os.path.exists(args.guest):
        parser.error(f'--guest {args.guest} does not exist')
    return args


def _terminate(signum, frame):
    # Turn a SIGTERM (docker stop, a CI cancel) into an exception, so the
    # cleanup in run_checks still stops QEMU and the rest.
    raise SystemExit(f'{PREFIX} terminated by signal {signum}')


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    signal.signal(signal.SIGTERM, _terminate)
    return run_checks(args)


if __name__ == '__main__':
    sys.exit(main())

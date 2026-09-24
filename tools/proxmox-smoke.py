#!/usr/bin/env python3
"""Drive ryll against a real Proxmox VE node's spiceproxy.

This is the driver for `.github/workflows/proxmox-functional.yml`. It runs
after shakenfist/actions' `deploy-proxmox-on-shakenfist` has stood up a
single-node PVE 9 with a SPICE guest, and takes that action's outputs:

    tools/proxmox-smoke.py --ryll target/release/ryll \\
        --mint-script "$MINT_SCRIPT" --api-url "$API_URL" --node "$NODE" \\
        --vmid "$VMID" --token-id "$TOKEN_ID" --token-file "$TOKEN_FILE" \\
        --ca-file "$CA_FILE" --workdir "$RUNNER_TEMP/proxmox-smoke"

It runs four checks, strictly one after another, because minting a ticket
resets qemu's SPICE password and so supersedes any session still up:

  positive     mint, launch ryll within 10 s, and require every channel to
               authenticate through the tunnel; then hold the session to
               mint + 120 s and require it to still be alive.
  expired      mint, wait 50 s, launch; require the spiceproxy's 401 and
               ryll's ticket-expiry hint, and no TLS or link error.
  wrong-pin    mint, alter the CN in host-subject, launch; require a TLS
               failure whose warning names both the pinned and the
               presented subject.
  missing-pin  mint, delete host-subject, launch; require ryll's refusal to
               tunnel without a pin, and no dial of the proxy at all.

Every check mints its own ticket through the action's mint script and
launches ryll straight away. Mint-to-launch is measured on the runner; if it
is over 10 s (beyond any deliberate wait) the check fails as a HARNESS
failure, so a slow runner is never reported as a ryll defect.

Each negative check asserts a specific message, never just an exit status:
an exit status alone passes for the wrong reason.

Oracles are ryll's own log lines. Their exact text is in the constants
below, each naming the source line it must stay in step with. If one of
those messages is reworded, this script fails in the safe direction (a
required line goes missing) -- except for the two absence checks, which is
why the positive check first proves the dial-line oracle can match at all.

SECRETS. A minted .vv carries a live SPICE password and a pseudo-hostname
holding a proxy ticket. This script never prints either, nor the .vv. Each
.vv is written 0600 by the mint script, edited in place at 0600, and deleted
when its check ends. ryll's output is captured to a per-check log under
--workdir; if it contains the password or any long part of the
pseudo-hostname the check fails and those strings are redacted from the
saved log. Never upload a .vv: this script deletes them, but a crash
between mint and cleanup can leave one behind.

ryll --verbose also appends to /tmp/ryll.log (see ryll/src/main.rs). That
file is not this script's and is not redacted; do not upload it.

Summary lines, one per check, start with "[proxmox-smoke] <check>: PASS" or
"FAIL". Exit status: 0 when every selected check passed, 1 otherwise, 2 on
a usage error.

Python 3.9+, standard library only.
"""

import argparse
import base64
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time


PREFIX = '[proxmox-smoke]'

CHECKS = ('positive', 'expired', 'wrong-pin', 'missing-pin')

# ── Timing ──────────────────────────────────────────────────────────────────

# Longest allowed gap between the mint and ryll's launch, beyond any
# deliberate wait. Proxmox tickets last about 30 s (the proxy refuses at
# 41 s and later, and SPICE auth already fails at 35-36 s), so a harness
# that takes longer than this to launch is testing the clock, not ryll.
LAUNCH_BOUND = 10.0

# The expired check's wait after the mint. Not 45: that is only four
# seconds clear of the first 401 measured, and the mint time is taken just
# before the API request, so the real ticket age at the CONNECT is higher
# still.
EXPIRED_DELAY = 50.0

# The positive session must still be alive this long after its mint: long
# past the ticket's validity, which proves an established tunnel does not
# depend on the ticket staying valid.
HOLD = 120.0

# From launch: the control socket must appear within SOCKET_BOUND, and
# every channel must have authenticated and a surface must exist within
# SESSION_BOUND.
SOCKET_BOUND = 20.0
SESSION_BOUND = 30.0

# A negative check's ryll must exit within this long of its launch. Every
# refusal here is decided on the first channel's first round trip.
NEGATIVE_EXIT_BOUND = 30.0

# How long ryll gets to exit after SIGINT before it is killed.
STOP_BOUND = 15.0

# The mint script talks to the API with a 20 s curl timeout of its own.
MINT_TIMEOUT = 60.0

# ── ryll's log lines ────────────────────────────────────────────────────────
#
# Asserted against ryll's stdout and stderr, which carry its tracing output:
# `<timestamp> <LEVEL> <message>`, with no target (ryll/src/main.rs calls
# `with_target(false)`). NO_COLOR is set for ryll, and ANSI escapes are
# stripped anyway.

# The channels a Proxmox SPICE guest offers that ryll must bring up.
CHANNELS = ('main', 'display', 'inputs', 'cursor')

# Logged at INFO by SpiceClient::connect_channel
# (shakenfist-spice-protocol/src/client.rs) once perform_auth has returned,
# that is, once the server accepted the ticket: "{}: connected
# successfully", with the channel's ChannelType::name(). The control socket
# has no per-channel status, so for the cursor channel this is the only
# oracle there is.
CHANNEL_CONNECTED_RE = r'\bINFO\s+{channel}: connected successfully\s*$'

# Logged at DEBUG by SpiceClient::open_transport (client.rs) before every
# dial, whether of the proxy or of the server: "Connecting to {} (TLS:
# {})", with ConnectionConfig::display_target(). ryll/src/main.rs logs the
# same text once at INFO before any connection is attempted, which is why
# the level is part of the match. Absence of this line is the missing-pin
# check's evidence that nothing was dialled.
DIAL_RE = re.compile(r'\bDEBUG\s+Connecting to (?P<target>.*) \(TLS: (?:true|false)\)\s*$')

# display_target()'s suffix under a proxy (shakenfist-spice-protocol/src/
# lib.rs). Every dial in this lane must carry it, or ryll went direct.
TUNNELLED = '(tunnelled, target redacted)'

# Logged at INFO by connect_channel after the transport (and so any
# CONNECT and TLS) succeeded, before the SPICE link. Its absence shows a
# failure happened before the link.
LINK_STARTED = 'performing link handshake'

# How ryll's headless mode reports the connection task's error: at ERROR,
# "Connection task failed: {:#}" (shakenfist-spice-renderer/src/session.rs,
# run_headless).
CONNECT_FAILED = 'Connection task failed: '

# ConnectError::Unauthorized's message (shakenfist-spice-protocol/src/
# proxy.rs), which quotes the proxy's status line with {:?}.
EXPIRED_RE = re.compile(
    re.escape(CONNECT_FAILED)
    + r'HTTP proxy refused the CONNECT: "HTTP/1\.[01] 401\b[^"]*"\. '
    + re.escape(
        "If this is a Proxmox VE spiceproxy, the ticket in the connection's host has probably expired: "
        'Proxmox tickets are valid for about 30 seconds'))

# rustls' Display for Error::InvalidCertificate, which is what
# SpiceCaVerifier's rejection reaches the connection task as.
TLS_FAILURE = CONNECT_FAILED + 'invalid peer certificate'

# SpiceCaVerifier::check_subject's warning (client.rs), whose second half is
# HostSubjectError::Mismatch (shakenfist-spice-protocol/src/host_subject.rs):
#   TLS: rejecting certificate: pinned host_subject <pin>: certificate
#   subject does not match expected "<pin>": attribute <i> (CN) value
#   "<presented>" does not match
# It names the pinned subject in full and the presented CN, which is what
# "names both subjects" means here.
PIN_REJECTED = 'TLS: rejecting certificate: pinned host_subject {pin}: '
PIN_PRESENTED = '(CN) value "{presented}" does not match'

# SpiceClient::new's refusal (client.rs), which runs before any dial.
MISSING_PIN_RE = re.compile(
    re.escape(CONNECT_FAILED)
    + r'refusing to tunnel through HTTP proxy \S+ without a host_subject: '
    + re.escape("a tunnelled connection must pin the server's certificate subject"))

# Errors that mean the connection got further than the check intends.
LATE_ERRORS = ('invalid peer certificate', 'Link error', 'Authentication failed')

ANSI_RE = re.compile(r'\x1b\[[0-9;]*[A-Za-z]')

# Pseudo-hostname fields at least this long are treated as secret: the
# ticket and its signature are, "pvespiceproxy", the vmid and the node
# name are not.
SECRET_FIELD_MIN = 16

PNG_MAGIC = b'\x89PNG\r\n\x1a\n'

# Left shift: accepted by any guest, types nothing.
SEND_KEY_SCANCODE = 0x2A


class HarnessError(Exception):
    """The harness, not ryll, could not do what the check needs."""


def log(message):
    print(f'{PREFIX} {message}', flush=True)


# ── .vv handling ────────────────────────────────────────────────────────────

def read_vv(path):
    """Return a .vv's lines and its key=value fields. Never print either."""
    with open(path, encoding='utf-8') as f:
        lines = f.read().splitlines()
    if not lines or lines[0].strip() != '[virt-viewer]':
        raise HarnessError('the minted .vv does not start with [virt-viewer]')
    fields = {}
    for line in lines[1:]:
        if '=' in line:
            key, value = line.split('=', 1)
            fields[key.strip()] = value
    return lines, fields


def write_vv(path, lines):
    tmp = path + '.tmp'
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, 'w', encoding='utf-8') as f:
        f.write('\n'.join(lines) + '\n')
    os.replace(tmp, path)


def drop_key(lines, key):
    kept = [line for line in lines if line.split('=', 1)[0].strip() != key]
    if len(kept) == len(lines):
        raise HarnessError(f'the minted .vv has no {key} field to remove')
    return kept


def alter_cn(subject):
    """Return (altered subject, original CN) with the CN value changed.

    PVE's host-subject is a plain comma-separated list such as
    "OU=PVE Cluster Node,O=Proxmox Virtual Environment,CN=<fqdn>", with no
    escaped commas.
    """
    parts = subject.split(',')
    for i, part in enumerate(parts):
        key, sep, value = part.partition('=')
        if sep and key.strip().upper() == 'CN':
            parts[i] = f'{key}=wrong-pin-{value}'
            return ','.join(parts), value
    raise HarnessError('the minted host-subject has no CN to alter')


def replace_key(lines, key, value):
    out = []
    for line in lines:
        if line.split('=', 1)[0].strip() == key:
            out.append(f'{key}={value}')
        else:
            out.append(line)
    return out


class Redaction:
    """A string ryll must never print, and a description safe to report.

    The description and the value are separate attributes rather than a
    tuple, so nothing built from the description (a problem message, the
    summary line) is derived from the value.
    """

    __slots__ = ('description', 'value')

    def __init__(self, description, value):
        self.description = description
        self.value = value


def redactions_of(fields):
    """The strings ryll must never print: the password and the ticket."""
    redactions = []
    password = fields.get('password', '')
    if password:
        redactions.append(Redaction('the SPICE password', password))
    host = fields.get('host', '')
    if host:
        redactions.append(Redaction('the pseudo-hostname', host))
        for part in host.split(':'):
            if len(part) >= SECRET_FIELD_MIN:
                redactions.append(Redaction('part of the pseudo-hostname', part))
    return redactions


# ── ryll's log ──────────────────────────────────────────────────────────────

def read_log(path):
    try:
        with open(path, 'rb') as f:
            text = f.read().decode('utf-8', errors='replace')
    except FileNotFoundError:
        return []
    return [ANSI_RE.sub('', line) for line in text.splitlines()]


def any_line(lines, pattern):
    regex = re.compile(pattern) if isinstance(pattern, str) else pattern
    return any(regex.search(line) for line in lines)


def contains(lines, needle):
    return any(needle in line for line in lines)


def channels_up(lines):
    return [c for c in CHANNELS if any_line(lines, CHANNEL_CONNECTED_RE.format(channel=re.escape(c)))]


def dial_targets(lines):
    return [m.group('target') for m in (DIAL_RE.search(line) for line in lines) if m]


def scrub_log(path, redactions):
    """Redact secrets from a saved log; return a problem per secret found."""
    try:
        with open(path, 'rb') as f:
            text = f.read().decode('utf-8', errors='replace')
    except FileNotFoundError:
        return []
    problems = []
    # Longest first, so the whole pseudo-hostname is counted before its parts.
    for redaction in sorted(redactions, key=lambda r: -len(r.value)):
        count = text.count(redaction.value)
        if count:
            text = text.replace(redaction.value, '<redacted>')
            problems.append(f'ryll printed {redaction.description} {count} time(s); '
                            'redacted from the saved log')
    if problems:
        fd = os.open(path, os.O_WRONLY | os.O_TRUNC)
        with os.fdopen(fd, 'w', encoding='utf-8') as f:
            f.write(text)
    return problems


def show_tail(path, count=25):
    """Print the end of a (scrubbed) ryll log, without its DEBUG noise."""
    lines = [line for line in read_log(path) if not re.search(r'\b(DEBUG|TRACE)\b', line)]
    if not lines:
        log(f'  ryll printed nothing above DEBUG (full log: {path})')
        return
    log(f'  last {min(count, len(lines))} non-debug lines of {path}:')
    for line in lines[-count:]:
        print(f'    {line}', flush=True)


# ── Control socket ──────────────────────────────────────────────────────────

class Control:
    """A minimal client for ryll's control socket.

    The framing is examples/control-socket-demo.py's: NDJSON requests with
    an integer id, and responses matched by id, with unsolicited events
    (which carry no id) skipped. See docs/control-socket-protocol.md.
    """

    def __init__(self, path, timeout=10.0):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self.buf = bytearray()
        self.next_id = 0

    def close(self):
        self.sock.close()

    def _recv_line(self):
        while b'\n' not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError('ryll closed the control socket')
            self.buf.extend(chunk)
        nl = self.buf.index(b'\n')
        line = bytes(self.buf[:nl])
        del self.buf[:nl + 1]
        return json.loads(line.decode('utf-8'))

    def request(self, method, params=None):
        self.next_id += 1
        req_id = self.next_id
        line = json.dumps({'id': req_id, 'method': method, 'params': params or {}}) + '\n'
        self.sock.sendall(line.encode('utf-8'))
        while True:
            msg = self._recv_line()
            if msg.get('id') == req_id:
                return msg

    def hello(self):
        resp = self.request('hello', {'client_name': 'proxmox-smoke', 'protocol_version': '1.2'})
        if not resp.get('ok'):
            raise RuntimeError(f'hello refused: {resp.get("error")}')


def screenshot_png(control):
    """Return (ok, description) for one PNG screenshot of surface 0."""
    resp = control.request('screenshot', {'format': 'png'})
    if not resp.get('ok'):
        return False, f'screenshot failed: {resp.get("error")}'
    data = base64.b64decode(resp['result'].get('data_base64', ''))
    if not data.startswith(PNG_MAGIC):
        return False, f'screenshot returned {len(data)} bytes that are not a PNG'
    return True, f'{resp["result"].get("width")}x{resp["result"].get("height")} PNG, {len(data)} bytes'


# ── One check ───────────────────────────────────────────────────────────────

class Check:
    def __init__(self, smoke, name):
        self.smoke = smoke
        self.name = name
        self.dir = os.path.join(smoke.workdir, name)
        self.vv = os.path.join(self.dir, 'console.vv')
        self.ryll_log = os.path.join(self.dir, 'ryll.log')
        self.sockdir = None
        self.sock = None
        self.proc = None
        self.redactions = []
        self.notes = []
        self.problems = []
        self.harness = False
        self.mint_time = None
        self.launch_time = None
        self.start = time.monotonic()

    def fail(self, message):
        self.problems.append(message)

    def note(self, message):
        self.notes.append(message)

    # The mint.

    def mint(self):
        os.makedirs(self.dir, mode=0o700, exist_ok=True)
        for stale in (self.vv, self.ryll_log):
            if os.path.exists(stale):
                os.unlink(stale)
        s = self.smoke.args
        cmd = [
            s.mint_script,
            '--api-url', s.api_url, '--node', s.node, '--vmid', s.vmid,
            '--token-id', s.token_id, '--token-file', s.token_file,
            '--ca-file', s.ca_file, '--out', self.vv,
        ]
        try:
            result = subprocess.run(cmd, stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                    timeout=MINT_TIMEOUT)
        except subprocess.TimeoutExpired:
            raise HarnessError(f'the mint script did not finish within {MINT_TIMEOUT:.0f} s')
        if result.returncode != 0:
            # The mint script's own messages carry no credential by design
            # (tools/proxmox-mint-vv.sh in shakenfist/actions).
            for line in result.stderr.strip().splitlines()[-5:]:
                log(f'  mint: {line}')
            raise HarnessError(f'the mint script failed with exit status {result.returncode}')
        out = result.stdout.strip().splitlines()
        try:
            self.mint_time = float(out[-1].strip().replace(',', '.'))
        except (IndexError, ValueError):
            raise HarnessError('the mint script did not print a mint time')
        if not os.path.exists(self.vv):
            raise HarnessError('the mint script succeeded but wrote no .vv')
        lines, fields = read_vv(self.vv)
        self.redactions = redactions_of(fields)
        for key in ('proxy', 'host', 'tls-port', 'password', 'host-subject'):
            if not fields.get(key):
                raise HarnessError(f'the minted .vv has no {key} field')
        return lines, fields

    # The launch.

    def launch(self, deliberate_wait=0.0):
        self.sockdir = tempfile.mkdtemp(prefix='pxs-')
        self.sock = os.path.join(self.sockdir, 'ryll.sock')
        now = time.time()
        overhead = now - self.mint_time - deliberate_wait
        if overhead > LAUNCH_BOUND:
            raise HarnessError(
                f'{overhead:.1f} s passed between the mint and the launch (bound {LAUNCH_BOUND:.0f} s'
                + (f', beyond the deliberate {deliberate_wait:.0f} s wait' if deliberate_wait else '')
                + '); the runner is too slow to test a ticket that lasts about 30 s. This is not a ryll '
                'defect')
        env = dict(os.environ, NO_COLOR='1')
        cmd = [self.smoke.args.ryll, '--headless', '--verbose', '--file', self.vv,
               '--control-socket', self.sock]
        logf = open(self.ryll_log, 'wb')
        try:
            self.launch_time = time.time()
            self.proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=logf,
                                         stderr=subprocess.STDOUT, env=env)
        finally:
            logf.close()
        since_mint = self.launch_time - self.mint_time
        self.note(f'mint->launch {since_mint:.2f}s'
                  + (f' ({since_mint - deliberate_wait:.2f}s past the {deliberate_wait:.0f}s wait)'
                     if deliberate_wait else ''))

    def wait_exit(self, bound):
        try:
            return self.proc.wait(timeout=bound)
        except subprocess.TimeoutExpired:
            return None

    def stop(self):
        """SIGINT ryll (its Ctrl+C path), then kill it if it hangs."""
        if self.proc is None or self.proc.poll() is not None:
            return self.proc.returncode if self.proc else None, 0.0
        start = time.monotonic()
        self.proc.send_signal(signal.SIGINT)
        rc = self.wait_exit(STOP_BOUND)
        took = time.monotonic() - start
        if rc is None:
            self.proc.kill()
            self.proc.wait()
            return None, took
        return rc, took

    def cleanup(self):
        if self.proc is not None and self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        for path in (self.vv, self.vv + '.tmp'):
            if os.path.exists(path):
                os.unlink(path)
        if self.sockdir:
            shutil.rmtree(self.sockdir, ignore_errors=True)
        if os.path.exists(self.ryll_log):
            for problem in scrub_log(self.ryll_log, self.redactions):
                self.fail(problem)

    def summary(self):
        elapsed = time.monotonic() - self.start
        if self.problems:
            kind = 'FAIL (harness)' if self.harness else 'FAIL'
            detail = '; '.join(self.problems)
            if self.notes:
                detail += ' [' + '; '.join(self.notes) + ']'
        else:
            kind = 'PASS'
            detail = '; '.join(self.notes)
        return f'{PREFIX} {self.name}: {kind} in {elapsed:.1f}s -- {detail}'


# ── The four checks ─────────────────────────────────────────────────────────

def wait_for_socket(check):
    deadline = check.launch_time + SOCKET_BOUND
    while time.time() < deadline:
        if check.proc.poll() is not None:
            check.fail(f'ryll exited with status {check.proc.returncode} before its control socket appeared')
            return None
        if os.path.exists(check.sock):
            try:
                control = Control(check.sock)
                control.hello()
                return control
            except (OSError, EOFError, RuntimeError, ValueError):
                pass
        time.sleep(0.2)
    check.fail(f'the control socket did not answer within {SOCKET_BOUND:.0f} s of the launch')
    return None


def run_positive(check):
    check.mint()
    check.launch()
    control = wait_for_socket(check)
    if control is None:
        # Say how far the channels got, which is usually why.
        if check.proc.poll() is None:
            check.stop()
        missing = [c for c in CHANNELS if c not in channels_up(read_log(check.ryll_log))]
        if missing:
            check.fail(f'no "<channel>: connected successfully" line for {", ".join(missing)}')
        return
    try:
        # Every channel authenticated, and a surface exists.
        deadline = check.launch_time + SESSION_BOUND
        status = {}
        up = []
        while time.time() < deadline and check.proc.poll() is None:
            up = channels_up(read_log(check.ryll_log))
            resp = control.request('status')
            status = resp.get('result', {}) if resp.get('ok') else {}
            if len(up) == len(CHANNELS) and status.get('surfaces'):
                break
            time.sleep(0.5)
        missing = [c for c in CHANNELS if c not in up]
        if missing:
            check.fail(f'no "<channel>: connected successfully" line for {", ".join(missing)} within '
                       f'{SESSION_BOUND:.0f} s of the launch')
        else:
            check.note(f'{", ".join(CHANNELS)} authenticated '
                       f'{time.time() - check.launch_time:.1f}s after launch')
        if not status.get('spice_connected'):
            check.fail('status did not report spice_connected')
        if not status.get('surfaces'):
            check.fail('status reported no display surface')
        else:
            check.note(f'{len(status["surfaces"])} surface(s)')

        ok, what = screenshot_png(control)
        if ok:
            check.note(f'screenshot {what}')
        else:
            check.fail(what)

        resp = control.request('send_key', {'scancode': SEND_KEY_SCANCODE, 'state': 'press'})
        if resp.get('ok'):
            check.note('send_key accepted')
        else:
            check.fail(f'send_key refused: {resp.get("error")}')
    finally:
        control.close()

    # Every dial went through the proxy. This also proves DIAL_RE still
    # matches ryll's dial line, which the missing-pin check relies on
    # matching nothing.
    targets = dial_targets(read_log(check.ryll_log))
    if len(targets) < len(CHANNELS):
        check.fail(f'only {len(targets)} dial line(s) matched; the missing-pin check\'s "no dial" '
                   'oracle is not trustworthy')
    direct = [t for t in targets if not t.endswith(TUNNELLED)]
    if direct:
        check.fail(f'{len(direct)} dial(s) did not go through the proxy')

    # Hold to mint + HOLD.
    hold_until = check.mint_time + check.smoke.args.hold
    while time.time() < hold_until:
        if check.proc.poll() is not None:
            break
        time.sleep(min(1.0, max(0.0, hold_until - time.time())))
    at = time.time() - check.mint_time
    if check.proc.poll() is not None:
        check.fail(f'ryll exited with status {check.proc.returncode} at mint+{at:.1f}s, before '
                   f'mint+{check.smoke.args.hold:.0f}s')
        return
    try:
        control = Control(check.sock)
        control.hello()
    except (OSError, EOFError, RuntimeError, ValueError) as e:
        check.fail(f'the control socket stopped answering by mint+{at:.1f}s: {e}')
        return
    try:
        resp = control.request('status')
        status = resp.get('result', {}) if resp.get('ok') else {}
        if status.get('spice_connected') and status.get('surfaces'):
            check.note(f'alive at mint+{at:.1f}s')
        else:
            check.fail(f'at mint+{at:.1f}s status reported spice_connected={status.get("spice_connected")} '
                       f'with {len(status.get("surfaces") or [])} surface(s)')
        ok, what = screenshot_png(control)
        if ok:
            check.note(f'second screenshot {what}')
        else:
            check.fail(f'second {what}')
    finally:
        control.close()
    if contains(read_log(check.ryll_log), CONNECT_FAILED):
        check.fail('ryll logged "Connection task failed" during the session')

    rc, took = check.stop()
    if rc is None:
        check.fail(f'ryll did not exit within {STOP_BOUND:.0f} s of SIGINT and was killed')
    elif rc != 0:
        check.fail(f'ryll exited with status {rc} after SIGINT')
    else:
        check.note(f'exited 0 {took:.1f}s after SIGINT')


def run_negative(check, prepare, deliberate_wait, verify):
    lines, fields = check.mint()
    context = prepare(check, lines, fields)
    if deliberate_wait:
        time.sleep(max(0.0, check.mint_time + deliberate_wait - time.time()))
    check.launch(deliberate_wait)
    rc = check.wait_exit(NEGATIVE_EXIT_BOUND)
    took = time.time() - check.launch_time
    if rc is None:
        check.fail(f'ryll was still running {NEGATIVE_EXIT_BOUND:.0f} s after the launch')
        check.stop()
    elif rc == 0:
        check.fail(f'ryll exited with status 0, {took:.1f}s after the launch')
    else:
        check.note(f'exited {rc} {took:.1f}s after launch')
    verify(check, read_log(check.ryll_log), context)


def expired_prepare(check, lines, fields):
    return None


def expired_verify(check, lines, context):
    if any_line(lines, EXPIRED_RE):
        check.note('401 with the ticket hint')
    else:
        check.fail('no "HTTP proxy refused the CONNECT: ...401..." error with the Proxmox ticket hint')
    late = [e for e in LATE_ERRORS if contains(lines, e)]
    if late:
        check.fail(f'got past the CONNECT: {", ".join(repr(e) for e in late)}')
    if contains(lines, LINK_STARTED):
        check.fail('a SPICE link handshake started')


def wrong_pin_prepare(check, lines, fields):
    pin, presented = alter_cn(fields['host-subject'])
    write_vv(check.vv, replace_key(lines, 'host-subject', pin))
    return pin, presented


def wrong_pin_verify(check, lines, context):
    pin, presented = context
    rejected = re.compile(re.escape(PIN_REJECTED.format(pin=pin)) + '.*'
                          + re.escape(PIN_PRESENTED.format(presented=presented)))
    if any_line(lines, rejected):
        check.note('pin warning names the pinned and the presented CN')
    else:
        check.fail('no "TLS: rejecting certificate: pinned host_subject ..." warning naming both the '
                   'altered pin and the presented CN')
    if contains(lines, TLS_FAILURE):
        check.note('TLS handshake refused')
    else:
        check.fail(f'no "{TLS_FAILURE}" error')
    targets = dial_targets(lines)
    if not targets:
        check.fail('ryll never dialled the proxy')
    elif not all(t.endswith(TUNNELLED) for t in targets):
        check.fail('a dial did not go through the proxy')
    if any_line(lines, EXPIRED_RE):
        check.fail('the proxy refused the ticket, so the pin was never tested')
    if contains(lines, LINK_STARTED):
        check.fail('a SPICE link handshake started despite the wrong pin')


def missing_pin_prepare(check, lines, fields):
    write_vv(check.vv, drop_key(lines, 'host-subject'))
    return None


def missing_pin_verify(check, lines, context):
    if any_line(lines, MISSING_PIN_RE):
        check.note('refused to tunnel without a host_subject')
    else:
        check.fail('no "refusing to tunnel through HTTP proxy ... without a host_subject" error')
    targets = dial_targets(lines)
    if targets:
        check.fail(f'ryll dialled {len(targets)} time(s) despite having no pin')
    else:
        check.note('no dial')
    if contains(lines, LINK_STARTED):
        check.fail('a SPICE link handshake started')


# ── Driver ──────────────────────────────────────────────────────────────────

class Smoke:
    def __init__(self, args):
        self.args = args
        self.workdir = args.workdir

    def run(self, name):
        check = Check(self, name)
        log(f'{name}: starting')
        try:
            if name == 'positive':
                run_positive(check)
            elif name == 'expired':
                run_negative(check, expired_prepare, self.args.expired_delay, expired_verify)
            elif name == 'wrong-pin':
                run_negative(check, wrong_pin_prepare, 0.0, wrong_pin_verify)
            elif name == 'missing-pin':
                run_negative(check, missing_pin_prepare, 0.0, missing_pin_verify)
        except HarnessError as e:
            check.harness = True
            check.fail(f'HARNESS: {e}')
        except Exception as e:
            # Report it, clean up, and go on to the next check.
            check.fail(f'the driver raised {type(e).__name__}: {e}')
        finally:
            check.cleanup()
        if check.problems and os.path.exists(check.ryll_log):
            show_tail(check.ryll_log)
        line = check.summary()
        print(line, flush=True)
        return not check.problems, line


def parse_args(argv):
    parser = argparse.ArgumentParser(
        description='Run the Proxmox lane checks: ryll against a real PVE spiceproxy.')
    parser.add_argument('--ryll', required=True, help='the ryll binary under test')
    parser.add_argument('--mint-script', required=True, help="shakenfist/actions' proxmox-mint-vv.sh")
    parser.add_argument('--api-url', required=True)
    parser.add_argument('--node', required=True)
    parser.add_argument('--vmid', required=True)
    parser.add_argument('--token-id', required=True)
    parser.add_argument('--token-file', required=True)
    parser.add_argument('--ca-file', required=True)
    parser.add_argument('--workdir', required=True, help='where per-check ryll logs are kept')
    parser.add_argument('--checks', default=','.join(CHECKS),
                        help=f'comma-separated subset of {",".join(CHECKS)}, run in that order')
    # These two exist for the harness's own tests and for a deliberate-
    # failure run; the lane uses the defaults.
    parser.add_argument('--expired-delay', type=float, default=EXPIRED_DELAY, help=argparse.SUPPRESS)
    parser.add_argument('--hold', type=float, default=HOLD, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    names = [c.strip() for c in args.checks.split(',') if c.strip()]
    unknown = [c for c in names if c not in CHECKS]
    if unknown or not names:
        parser.error(f'--checks must name some of {", ".join(CHECKS)}')
    args.checks = [c for c in CHECKS if c in names]
    for label, path in (('--ryll', args.ryll), ('--mint-script', args.mint_script)):
        if not os.access(path, os.X_OK):
            parser.error(f'{label} {path} is not executable')
    return args


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    os.makedirs(args.workdir, mode=0o700, exist_ok=True)
    smoke = Smoke(args)
    results = [smoke.run(name) for name in args.checks]
    passed = sum(1 for ok, _ in results if ok)
    log(f'{passed}/{len(results)} checks passed')
    for _, line in results:
        print(line, flush=True)
    return 0 if passed == len(results) else 1


if __name__ == '__main__':
    sys.exit(main())

#!/usr/bin/env bash
# test-proxmox-smoke.sh — smoke test for tools/proxmox-smoke.py.
#
# The Proxmox lane's driver decides pass and fail from ryll's log lines,
# and its negative checks pass when ryll *fails* to connect. A driver bug
# therefore tends to read as a green lane, and the lane is expensive to run
# (it books a nested Proxmox node), so its logic is pinned here instead --
# the same convention as tools/test-report-fuzz-failure.sh.
#
# A fake mint script stands in for shakenfist/actions'
# tools/proxmox-mint-vv.sh and a fake ryll stands in for the real one. The
# fake ryll prints the log lines real ryll prints, copied from the driver's
# oracle constants' source sites, and serves a minimal control socket. Each
# case below injects one fault and asserts that the driver catches it, and
# every case asserts that the driver never printed the ticket or password.
#
# The fake ryll behaves as ryll should, not as it does: it exits non-zero
# when a connection fails. Real headless ryll currently exits 0 then, which
# the driver reports; the "exit-zero" case pins that.
#
# No network, no Docker, no cargo, no Proxmox. Python 3 only. Runs in about
# fifteen seconds.
#
# Usage: tools/test-proxmox-smoke.sh
# Exit code: 0 all assertions held, 1 otherwise.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DRIVER="$SCRIPT_DIR/proxmox-smoke.py"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILURES=0
red() { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }

# ── The fake mint script ─────────────────────────────────────────────────────
#
# Same arguments as proxmox-mint-vv.sh. Writes a 0600 .vv whose pseudo-
# hostname carries the mint time (so the fake ryll can play the proxy's
# ticket-age check), prints the mint time, and leaves the secrets it minted
# in $FAKE_STATE so the test can look for them in the driver's output.
cat > "$WORK/fake-mint" <<'EOF'
#!/usr/bin/env python3
import argparse, os, secrets, sys, time
p = argparse.ArgumentParser()
for a in ('api-url', 'node', 'vmid', 'token-id', 'token-file', 'ca-file', 'out'):
    p.add_argument('--' + a, required=True)
args = p.parse_args()
fault = os.environ.get('FAKE_MINT_FAULT', '')
if fault == 'fail':
    sys.stderr.write('proxmox-mint-vv.sh: the Proxmox API refused the console request with HTTP 403\n')
    sys.exit(1)
minted = time.time()
password = secrets.token_hex(8)
host = 'pvespiceproxy:%.6f-%s:%s:%s::%s' % (minted, secrets.token_hex(20), args.vmid, args.node,
                                            secrets.token_hex(24))
fields = [
    ('type', 'spice'), ('proxy', 'http://pve1.test:3128'), ('host', host), ('tls-port', '61000'),
    ('password', password), ('delete-this-file', '1'),
    ('host-subject', 'OU=PVE Cluster Node,O=Proxmox Virtual Environment,CN=pve1.test'),
    ('ca', '-----BEGIN CERTIFICATE-----\\nMIIB\\n-----END CERTIFICATE-----\\n'),
]
fd = os.open(args.out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, 'w') as f:
    f.write('[virt-viewer]\n' + ''.join('%s=%s\n' % kv for kv in fields))
state = os.environ['FAKE_STATE']
with open(os.path.join(state, 'secrets'), 'a') as f:
    f.write(password + '\n' + host + '\n')
print('%.6f' % (minted - 20 if fault == 'slow' else minted))
EOF

# ── The fake ryll ────────────────────────────────────────────────────────────
cat > "$WORK/fake-ryll" <<'EOF'
#!/usr/bin/env python3
import base64, datetime, json, os, signal, socket, sys, threading, time
args = sys.argv[1:]
vv = args[args.index('--file') + 1]
sock_path = args[args.index('--control-socket') + 1]
fault = os.environ.get('FAKE_RYLL_FAULT', '')
ttl = float(os.environ.get('FAKE_TTL', '1.0'))
open(os.path.join(os.environ['FAKE_STATE'], 'launched'), 'a').write('x\n')

fields = {}
for line in open(vv).read().splitlines()[1:]:
    k, _, v = line.partition('=')
    fields[k] = v

def log(level, msg):
    ts = datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%S.%fZ')
    print('%s %5s %s' % (ts, level, msg), flush=True)

target = 'pve1.test:3128 (tunnelled, target redacted)'
if fault == 'direct':
    target = 'pve1.test:61000'
failed_rc = 0 if fault == 'exit-zero' else 1

def dial():
    log('DEBUG', 'Connecting to %s (TLS: true)' % target)

log('INFO', 'ryll v0.0.0 (fake)')
log('INFO', 'Connecting to %s (TLS: true)' % target)
if fault == 'leak':
    log('DEBUG', 'link password %s' % fields['password'])

subject = fields.get('host-subject')
if subject is None:
    if fault == 'dial-anyway':
        dial()
    log('ERROR', 'Connection task failed: refusing to tunnel through HTTP proxy http://pve1.test:3128 '
        "without a host_subject: a tunnelled connection must pin the server's certificate subject")
    sys.exit(failed_rc)

dial()
minted = float(fields['host'].split(':')[1].split('-')[0])
if time.time() - minted > ttl:
    log('ERROR', 'Connection task failed: HTTP proxy refused the CONNECT: "HTTP/1.0 401 invalid ticket". '
        "If this is a Proxmox VE spiceproxy, the ticket in the connection's host has probably expired: "
        'Proxmox tickets are valid for about 30 seconds, so fetch a fresh .vv file and open it promptly')
    sys.exit(failed_rc)

cn = subject.rsplit('CN=', 1)[1]
if cn != 'pve1.test':
    presented = 'someone-else.test' if fault == 'wrong-presented' else 'pve1.test'
    log('WARN', 'TLS: rejecting certificate: pinned host_subject %s: certificate subject does not match '
        'expected "%s": attribute 2 (CN) value "%s" does not match' % (subject, subject, presented))
    log('ERROR', 'Connection task failed: invalid peer certificate: NotValidForName')
    sys.exit(failed_rc)

stop = threading.Event()
signal.signal(signal.SIGINT, lambda *_: stop.set())
PNG = base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==')

def serve(conn):
    f = conn.makefile('rwb')
    for raw in f:
        req = json.loads(raw)
        m = req['method']
        if m == 'hello':
            res = {'server_name': 'ryll', 'protocol_version': '1.2', 'supported_methods': [],
                   'supported_events': []}
        elif m == 'status':
            res = {'spice_connected': True, 'agent_connected': False,
                   'surfaces': [{'channel_id': 0, 'surface_id': 0, 'width': 720, 'height': 400}]}
        elif m == 'screenshot':
            res = {'width': 1, 'height': 1, 'format': 'png', 'data_base64': base64.b64encode(PNG).decode()}
        else:
            res = {}
        f.write((json.dumps({'event': 'surface_drawn', 'data': {}}) + '\n').encode())
        f.write((json.dumps({'id': req['id'], 'ok': True, 'result': res}) + '\n').encode())
        f.flush()

srv = socket.socket(socket.AF_UNIX)
srv.bind(sock_path)
srv.listen()
threading.Thread(target=lambda: [threading.Thread(target=serve, args=(srv.accept()[0],), daemon=True).start()
                                 for _ in iter(int, 1)], daemon=True).start()
for channel in ('main', 'display', 'inputs', 'cursor'):
    if channel != 'main':
        dial()
    log('INFO', '%s: performing link handshake (id=0)' % channel)
    if fault == 'no-cursor' and channel == 'cursor':
        log('ERROR', 'Connection task failed: Authentication failed: PermissionDenied')
        sys.exit(1)
    log('INFO', '%s: connected successfully' % channel)
deadline = time.time() + (1.0 if fault == 'die' else 3600)
while not stop.is_set() and time.time() < deadline:
    time.sleep(0.05)
os.unlink(sock_path)
log('INFO', 'Headless mode finished')
sys.exit(3 if fault == 'die' else 0)
EOF
chmod +x "$WORK/fake-mint" "$WORK/fake-ryll"

OUT=""
STATUS=0
CASE=0

# Run the driver with any extra arguments given, capturing stdout and
# stderr together in OUT and the exit code in STATUS. Faults are injected by
# prefixing the call with FAKE_RYLL_FAULT=... or FAKE_MINT_FAULT=..., which
# bash exports to the function's children for the duration of the call.
run_driver() {
    CASE=$((CASE + 1))
    export FAKE_STATE="$WORK/state-$CASE"
    RUN_WORKDIR="$WORK/run-$CASE"
    mkdir -p "$FAKE_STATE"
    OUT="$(FAKE_TTL=1.0 python3 "$DRIVER" \
        --ryll "$WORK/fake-ryll" --mint-script "$WORK/fake-mint" \
        --api-url https://pve1.test:8006 --node pve1 --vmid 100 \
        --token-id 'console@pve!smoke' --token-file /dev/null --ca-file /dev/null \
        --workdir "$RUN_WORKDIR" --hold 2 --expired-delay 1.5 "$@" 2>&1)"
    STATUS=$?
}

assert_status() {
    local want="$1" what="$2"
    if [ "$STATUS" -eq "$want" ]; then
        green "ok: $what (exit $want)"
    else
        red "FAIL: $what: expected exit $want, got $STATUS"
        printf '%s\n' "$OUT"
        FAILURES=$((FAILURES + 1))
    fi
}

assert_contains() {
    local needle="$1" what="$2"
    if [[ "$OUT" == *"$needle"* ]]; then
        green "ok: $what"
    else
        red "FAIL: $what: output does not contain '$needle'"
        printf '%s\n' "$OUT"
        FAILURES=$((FAILURES + 1))
    fi
}

# Every case: nothing the mint produced reached the driver's output, and no
# .vv outlived its check.
assert_hygiene() {
    local secret leaked=0 left
    if [ -f "$FAKE_STATE/secrets" ]; then
        while IFS= read -r secret; do
            if [ -n "$secret" ] && [[ "$OUT" == *"$secret"* ]]; then
                leaked=1
            fi
        done < "$FAKE_STATE/secrets"
    fi
    if [ "$leaked" -eq 0 ]; then
        green "ok: no password or pseudo-hostname in the driver's output"
    else
        red "FAIL: the driver printed a password or pseudo-hostname"
        FAILURES=$((FAILURES + 1))
    fi
    left="$(find "$RUN_WORKDIR" -name '*.vv*' 2>/dev/null)"
    if [ -z "$left" ]; then
        green "ok: no .vv left in the workdir"
    else
        red "FAIL: .vv files left behind: $left"
        FAILURES=$((FAILURES + 1))
    fi
}

echo "== a ryll that behaves =="
run_driver
assert_status 0 "every check passes"
assert_contains "positive: PASS" "positive passes"
assert_contains "main, display, inputs, cursor authenticated" "all four channels are named"
assert_contains "alive at mint+2." "the session is held to mint + hold"
assert_contains "exited 0" "ryll stops cleanly on SIGINT"
assert_contains "expired: PASS" "expired passes"
assert_contains "401 with the ticket hint" "expired saw the 401 and the hint"
assert_contains "(0." "expired reports its overhead past the deliberate wait"
assert_contains "wrong-pin: PASS" "wrong pin passes"
assert_contains "pin warning names the pinned and the presented CN" "wrong pin saw both subjects"
assert_contains "missing-pin: PASS" "missing pin passes"
assert_contains "refused to tunnel without a host_subject; no dial" "missing pin refused before dialling"
assert_contains "4/4 checks passed" "the tally is right"
assert_hygiene

echo
echo "== ryll exits 0 after a failed connect =="
FAKE_RYLL_FAULT=exit-zero run_driver
assert_status 1 "an exit status of 0 fails the negative checks"
assert_contains "expired: FAIL" "expired fails"
assert_contains "ryll exited with status 0" "the reason is the exit status"
assert_contains "401 with the ticket hint" "the message assertion still reports"
assert_contains "positive: PASS" "positive is unaffected"
assert_hygiene

echo
echo "== ryll prints the password =="
FAKE_RYLL_FAULT=leak run_driver --checks positive
assert_status 1 "a leak fails the check"
assert_contains "ryll printed the SPICE password 1 time(s)" "the leak is named"
if grep -rqF -f "$FAKE_STATE/secrets" "$RUN_WORKDIR"; then
    red "FAIL: the saved ryll log still holds the password"
    FAILURES=$((FAILURES + 1))
else
    green "ok: the saved ryll log was redacted"
fi
assert_hygiene

echo
echo "== the cursor channel never authenticates =="
FAKE_RYLL_FAULT=no-cursor run_driver --checks positive
assert_status 1 "a missing channel fails positive"
assert_contains 'connected successfully" line for cursor' "the missing channel is named"
assert_hygiene

echo
echo "== the session dies before mint + hold =="
FAKE_RYLL_FAULT=die run_driver --checks positive
assert_status 1 "an early exit fails positive"
assert_contains "ryll exited with status 3 at mint+" "the early exit is reported"
assert_hygiene

echo
echo "== ryll dials without a pin =="
FAKE_RYLL_FAULT=dial-anyway run_driver --checks missing-pin
assert_status 1 "a dial fails missing-pin even with the refusal present"
assert_contains "ryll dialled 1 time(s) despite having no pin" "the dial is reported"
assert_hygiene

echo
echo "== ryll goes direct instead of through the proxy =="
FAKE_RYLL_FAULT=direct run_driver --checks positive,wrong-pin
assert_status 1 "a direct dial fails"
assert_contains "dial(s) did not go through the proxy" "positive catches it"
assert_contains "a dial did not go through the proxy" "wrong pin catches it"
assert_hygiene

echo
echo "== the warning names some other presented subject =="
FAKE_RYLL_FAULT=wrong-presented run_driver --checks wrong-pin
assert_status 1 "the wrong presented CN fails wrong-pin"
assert_contains "naming both the altered pin and the presented CN" "the missing subject is reported"
assert_hygiene

echo
echo "== the mint fails =="
FAKE_MINT_FAULT=fail run_driver --checks positive,missing-pin
assert_status 1 "a mint failure fails"
assert_contains "positive: FAIL (harness)" "it is a harness failure"
assert_contains "mint: proxmox-mint-vv.sh: the Proxmox API refused" "the mint's own reason is shown"
assert_contains "missing-pin: FAIL (harness)" "later checks still run"
assert_hygiene

echo
echo "== the runner is slow between mint and launch =="
FAKE_MINT_FAULT=slow run_driver --checks positive
assert_status 1 "a slow launch fails"
assert_contains "positive: FAIL (harness)" "it is a harness failure"
assert_contains "This is not a ryll defect" "it says whose problem it is"
if [ -e "$FAKE_STATE/launched" ]; then
    red "FAIL: ryll was launched against a ticket the harness knew was stale"
    FAILURES=$((FAILURES + 1))
else
    green "ok: ryll was not launched"
fi
assert_hygiene

echo
echo "== usage =="
run_driver --checks positive,bogus
assert_status 2 "an unknown check is a usage error"

echo
if [ "$FAILURES" -eq 0 ]; then
    green "All proxmox-smoke assertions held."
    exit 0
fi
red "$FAILURES proxmox-smoke assertion(s) failed."
exit 1

#!/usr/bin/env bash
# Smoke-test that ryll --web starts, binds, and shuts down cleanly.
#
# Usage: tools/web-smoke.sh [RYLL_BINARY]
#
# Does NOT verify a full WebRTC handshake — that is what
# tests/loopback.rs covers. This test verifies only that the HTTP
# entry point starts up without crashing and exits cleanly on SIGTERM.
#
# The SPICE side will fail to connect (127.0.0.1:1 is not a real
# server) but the HTTP server binds independently (run_connection is
# spawned concurrently via tokio::spawn before web::run is called),
# so the process stays alive.

set -euo pipefail

BIN="${1:-target/release/ryll}"
PORT="${WEB_PORT:-18080}"

if [ ! -x "$BIN" ]; then
    echo "ERROR: ryll binary not found or not executable: $BIN"
    echo "Build with: make build"
    exit 1
fi

TMPDIR_WORK="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_WORK"' EXIT

# Synthetic .vv file pointing at a non-existent SPICE server.
# ryll --web still starts the HTTP server even though the SPICE side
# cannot connect, because run_connection is spawned concurrently.
cat > "$TMPDIR_WORK/test.vv" <<'VV'
[virt-viewer]
type=spice
host=127.0.0.1
port=1
VV

echo "Starting ryll --web on port $PORT with stub .vv ..."
"$BIN" --web --web-port "$PORT" --file "$TMPDIR_WORK/test.vv" \
    >"$TMPDIR_WORK/ryll.stdout" 2>"$TMPDIR_WORK/ryll.stderr" &
PID=$!

# Give the HTTP server a few seconds to bind.
sleep 3

if ! kill -0 "$PID" 2>/dev/null; then
    echo "FAIL: ryll --web exited prematurely"
    echo "--- stdout ---"
    cat "$TMPDIR_WORK/ryll.stdout" || true
    echo "--- stderr ---"
    cat "$TMPDIR_WORK/ryll.stderr" || true
    exit 1
fi

echo "Process $PID is alive — sending SIGTERM ..."
kill -TERM "$PID"

WAIT_START=$SECONDS
while kill -0 "$PID" 2>/dev/null; do
    if (( SECONDS - WAIT_START > 5 )); then
        echo "FAIL: ryll --web did not exit within 5 s of SIGTERM"
        echo "--- stdout ---"
        cat "$TMPDIR_WORK/ryll.stdout" || true
        echo "--- stderr ---"
        cat "$TMPDIR_WORK/ryll.stderr" || true
        kill -9 "$PID" 2>/dev/null || true
        exit 1
    fi
    sleep 0.2
done

# Reap the child to avoid a leftover zombie and collect exit code.
# The process may have exited non-zero (SPICE connect failed) — that
# is acceptable; we only care that it exited promptly after SIGTERM.
wait "$PID" || true

echo "smoke test passed"

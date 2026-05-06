#!/usr/bin/env bash
# Smoke-test that ryll --web starts, binds, and shuts down cleanly.
#
# Usage: tools/web-smoke.sh [--tls] [RYLL_BINARY]
#
# Without --tls: verifies the plain-HTTP path. Hits http://localhost:PORT/
# is NOT performed because the token is per-launch and reading it from
# stdout adds flakiness — the bind+SIGTERM round-trip is the contract.
#
# With --tls: generates a throwaway self-signed cert via openssl into
# the temp dir, launches ryll --web with --web-tls-cert / --web-tls-key,
# verifies https://localhost:PORT/ responds at all (curl -sk -o /dev/null
# returns 0 even on 401, which is what we expect without the token), then
# SIGTERMs and verifies clean exit.
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

USE_TLS=0
BIN=""
for arg in "$@"; do
    case "$arg" in
        --tls)
            USE_TLS=1
            ;;
        *)
            BIN="$arg"
            ;;
    esac
done
BIN="${BIN:-target/release/ryll}"
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

EXTRA_ARGS=()
if [ "$USE_TLS" -eq 1 ]; then
    if ! command -v openssl >/dev/null 2>&1; then
        echo "ERROR: --tls requires openssl on PATH"
        exit 1
    fi
    echo "Generating throwaway self-signed cert ..."
    openssl req -x509 -newkey rsa:2048 \
        -keyout "$TMPDIR_WORK/key.pem" \
        -out "$TMPDIR_WORK/cert.pem" \
        -days 1 -nodes \
        -subj "/CN=localhost" 2>/dev/null
    EXTRA_ARGS+=(--web-tls-cert "$TMPDIR_WORK/cert.pem"
                 --web-tls-key  "$TMPDIR_WORK/key.pem")
    SCHEME="https"
    CURL_FLAGS="-sk"
    MODE_LABEL="TLS"
else
    SCHEME="http"
    CURL_FLAGS="-s"
    MODE_LABEL="plain-HTTP"
fi

echo "Starting ryll --web ($MODE_LABEL) on port $PORT with stub .vv ..."
"$BIN" --web --web-port "$PORT" --file "$TMPDIR_WORK/test.vv" "${EXTRA_ARGS[@]}" \
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

# Connectivity probe: in TLS mode, prove the listener actually
# responds. Without the token we expect 401 from the auth
# middleware, which is fine — we only care that the TLS handshake
# completes and an HTTP status comes back.
if [ "$USE_TLS" -eq 1 ]; then
    if ! curl $CURL_FLAGS -o /dev/null \
            "$SCHEME://localhost:$PORT/" --max-time 5; then
        echo "FAIL: TLS connectivity probe to $SCHEME://localhost:$PORT/ failed"
        echo "--- stdout ---"
        cat "$TMPDIR_WORK/ryll.stdout" || true
        echo "--- stderr ---"
        cat "$TMPDIR_WORK/ryll.stderr" || true
        kill -TERM "$PID" 2>/dev/null || true
        exit 1
    fi
    echo "TLS connectivity probe OK"

    # Also verify ryll emitted an https:// URL line on stdout.
    # The URL is printed directly to stdout (not via tracing)
    # so the token never leaks into journald or log aggregators.
    if ! grep -q "https://" "$TMPDIR_WORK/ryll.stdout"; then
        echo "FAIL: ryll did not emit https:// URL line on stdout"
        echo "--- stdout ---"
        cat "$TMPDIR_WORK/ryll.stdout" || true
        kill -TERM "$PID" 2>/dev/null || true
        exit 1
    fi
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

echo "smoke test passed ($MODE_LABEL)"

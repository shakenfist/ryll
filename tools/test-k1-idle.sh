#!/usr/bin/env bash
#
# Long-idle integration test for K1 (main-channel-wedge) regression.
#
# K1 was an abandoned-receiver deadlock in main_channel.rs where main
# blocked forever on `event_tx.send().await` after the temp event
# channel in session.rs filled up (~65 Latency events, ~T+466s).
# Fixed by removing the temp channel; this test guards against the
# fix regressing.
#
# What it does:
#   1. Launches ryll headless against a SPICE server (assumes
#      `make test-qemu` is already running on $HOST_PORT).
#   2. Sets RYLL_K1_MAIN_ONLY=1 to skip secondary channels — the
#      historical K1 fingerprint shows up there regardless, and
#      isolating main makes the test cheaper.
#   3. Idles for IDLE_SECS (default 540 s, ~T+466 + 75 s headroom).
#   4. Fails on any of:
#        - ryll exited early
#        - "event_tx.send() timed out" warning in the log (defensive
#          timeout fired — channel backpressure regression)
#        - "channel error" / "peer closed" / "read error" lines
#        - Fewer pongs than we'd expect for IDLE_SECS / 30 s
#
# Environment overrides:
#   IDLE_SECS    — seconds to idle (default 540)
#   HOST_PORT    — SPICE server (default localhost:5900)
#   RYLL         — ryll binary path (default ./target/debug/ryll)

set -euo pipefail

IDLE_SECS="${IDLE_SECS:-540}"
HOST_PORT="${HOST_PORT:-localhost:5900}"
RYLL="${RYLL:-./target/debug/ryll}"
LOG_DIR="$(mktemp -d -t ryll-k1-XXXXXX)"
LOG="$LOG_DIR/ryll.log"

if [ ! -x "$RYLL" ]; then
    echo "ryll not found at $RYLL — run 'make build' first" >&2
    exit 1
fi

echo "K1 long-idle test: idling for ${IDLE_SECS}s against $HOST_PORT"
echo "log: $LOG"

# Launch ryll headless+main-only. PID captured for cleanup.
RYLL_K1_MAIN_ONLY=1 nohup "$RYLL" \
    --direct "$HOST_PORT" \
    --headless \
    --verbose \
    > "$LOG" 2>&1 &
RYLL_PID=$!

cleanup() {
    kill "$RYLL_PID" 2>/dev/null || true
    wait "$RYLL_PID" 2>/dev/null || true
}
trap cleanup EXIT

# Wait the idle window. We don't poll — the test's whole point is to
# leave main alone and see if it stays alive.
sleep "$IDLE_SECS"

if ! kill -0 "$RYLL_PID" 2>/dev/null; then
    echo "FAIL: ryll exited before ${IDLE_SECS}s elapsed" >&2
    tail -40 "$LOG" >&2
    exit 1
fi

if grep -q "event_tx.send() timed out" "$LOG"; then
    echo "FAIL: defensive event_tx send timeout fired — channel backpressure regression" >&2
    grep "event_tx.send" "$LOG" | head -5 >&2
    exit 1
fi

if grep -qiE "channel error|peer closed|read error" "$LOG"; then
    echo "FAIL: log contains disconnect / read-error signature" >&2
    grep -iE "channel error|peer closed|read error" "$LOG" | head -5 >&2
    exit 1
fi

# Server sends a ping ~every 15 s; client pong rate should track. We
# require at least IDLE_SECS / 30 pongs as a generous lower bound that
# still catches a multi-minute wedge.
pong_count=$(grep -c "main sent .* opcode 3 pong" "$LOG" || true)
expected_min=$((IDLE_SECS / 30))
if [ "$pong_count" -lt "$expected_min" ]; then
    echo "FAIL: only $pong_count main pongs in ${IDLE_SECS}s (expected >= $expected_min)" >&2
    echo "  this is the K1 wedge signature — main stopped responding to pings" >&2
    tail -20 "$LOG" >&2
    exit 1
fi

echo "PASS: ${IDLE_SECS}s elapsed, ryll alive, $pong_count main pongs sent"

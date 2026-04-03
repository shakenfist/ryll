#!/bin/bash
# End-to-end smoke test for virtual USB mass storage passthrough.
#
# Usage: tools/test-usb-disk.sh [SPICE_PORT]
#
# Starts QEMU with USB redirection, connects ryll in headless mode
# with a test RAW image, and verifies the protocol exchange works.
#
# Requirements:
#   - qemu-system-x86_64 installed
#   - ryll built (./target/debug/ryll or set RYLL env var)
#   - OVMF firmware installed

set -euo pipefail

PORT="${1:-5900}"
IMAGE="testdata/usb-test.raw"
RYLL="${RYLL:-./target/debug/ryll}"
LOG="/tmp/ryll-usb-test.log"
PASS=0
FAIL=0

echo "=== USB disk passthrough smoke test ==="
echo ""

# Check prerequisites
if ! command -v qemu-system-x86_64 >/dev/null 2>&1; then
    echo "SKIP: qemu-system-x86_64 not found"
    exit 0
fi

if [ ! -x "$RYLL" ]; then
    echo "SKIP: ryll binary not found at $RYLL"
    echo "Build with: make build"
    exit 0
fi

# Create test image if needed
if [ ! -f "$IMAGE" ]; then
    echo "Creating ${IMAGE}..."
    dd if=/dev/zero of="$IMAGE" bs=1M count=64 2>/dev/null
fi

# Start QEMU with USB redirection
echo "Starting QEMU with USB redirection on port ${PORT}..."
make test-qemu-usb

# Give QEMU time to boot
echo "Waiting for QEMU to start..."
sleep 3

# Connect ryll with the virtual disk (headless, short run)
echo "Connecting ryll with --usb-disk ${IMAGE}..."
echo ""
timeout 10 "$RYLL" \
    --direct "localhost:${PORT}" \
    --headless \
    --verbose \
    --usb-disk "$IMAGE" \
    2>&1 | tee "$LOG" || true

echo ""
echo "=== Checking results ==="

check() {
    local pattern="$1"
    local description="$2"
    if grep -q "$pattern" "$LOG"; then
        echo "  PASS: $description"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $description"
        FAIL=$((FAIL + 1))
    fi
}

check "sending hello" "Client sent usbredir hello"
check "server hello" "Server hello received"
check "auto-connected" "Virtual disk auto-connected"
check "USB device connected" "UsbDeviceConnected event received"
check "usb-disk: opened" "RAW image opened successfully"

echo ""

# Cleanup
make test-qemu-stop

echo ""
echo "=== Results: ${PASS} passed, ${FAIL} failed ==="

if [ "$FAIL" -gt 0 ]; then
    echo "Log saved to: $LOG"
    exit 1
fi

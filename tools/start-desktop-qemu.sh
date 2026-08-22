#!/bin/bash
# Launch the XFCE desktop test guest used for manual `--web`
# verification.
#
# Usage: start-desktop-qemu.sh \
#   --qcow2 PATH \
#   --seed PATH \
#   --ovmf-code PATH \
#   --ovmf-vars PATH \
#   --spice-port N \
#   --pid-file PATH \
#   [--memory MB] [--cpus N] [--spice-addr ADDR]
#
# --spice-addr defaults to 127.0.0.1. Ticketing is disabled and this
# guest has a desktop, a known password and passwordless sudo, so
# binding it to every interface would hand a session to anyone on the
# same network. The documented workflow points ryll at localhost.
#
# Unlike tools/start-qemu.sh in kerbside, this guest is a full desktop
# and the point of it is to exercise every SPICE channel a browser
# viewer touches. Three devices matter and are easy to leave out:
#
#   * the vdagent virtserialport, without which the SPICE server never
#     offers client (absolute) mouse mode and never resizes the guest
#     to the viewer's viewport. A guest missing this looks like a
#     broken pointer, not like a missing device.
#   * an audio device, without which the playback channel carries
#     nothing and "no audio" is indistinguishable from a bug.
#   * user-mode networking, so cloud-init and apt work in the guest.
#
# Backgrounds qemu and writes the PID to --pid-file.

set -euo pipefail

QCOW2=''
SEED=''
OVMF_CODE=''
OVMF_VARS=''
SPICE_PORT=''
PID_FILE=''
# Optional QMP control socket. Lets a script drive the guest --
# sendkey, screendump, quit -- without a human at the viewer, which
# is what tools/web-soak.sh needs and what makes an unattended
# keyboard or audio check possible at all.
QMP_SOCKET=''
MEMORY='2048'
CPUS='2'
SPICE_ADDR='127.0.0.1'

while [ $# -gt 0 ]; do
    case "$1" in
        --qcow2)      QCOW2="$2";      shift 2 ;;
        --seed)       SEED="$2";       shift 2 ;;
        --ovmf-code)  OVMF_CODE="$2";  shift 2 ;;
        --ovmf-vars)  OVMF_VARS="$2";  shift 2 ;;
        --spice-port) SPICE_PORT="$2"; shift 2 ;;
        --spice-addr) SPICE_ADDR="$2"; shift 2 ;;
        --pid-file)   PID_FILE="$2";   shift 2 ;;
        --qmp)        QMP_SOCKET="$2"; shift 2 ;;
        --memory)     MEMORY="$2";     shift 2 ;;
        --cpus)       CPUS="$2";       shift 2 ;;
        *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

for arg in QCOW2 SEED OVMF_CODE OVMF_VARS SPICE_PORT PID_FILE; do
    if [ -z "${!arg}" ]; then
        # Variables are FOO_BAR; flags are --foo-bar. Lowercasing
        # alone printed `--ovmf_code`, which is not a flag this
        # script accepts, so the error told you to pass something
        # that would then be rejected as unknown.
        flag="${arg,,}"
        echo "ERROR: --${flag//_/-} is required" >&2
        exit 1
    fi
done

for f in "${QCOW2}" "${SEED}" "${OVMF_CODE}" "${OVMF_VARS}"; do
    if [ ! -f "${f}" ]; then
        echo "ERROR: file not found: ${f}" >&2
        exit 1
    fi
done

# qemu writes to the vars file at runtime, so work on a copy. Put it
# beside the disk overlay rather than beside the pid file: the overlay
# lives in testdata/, which `make clean-testdata` already sweeps,
# whereas the pid file is in /tmp and nothing removed the copy.
VARS_COPY="${QCOW2%.qcow2}-ovmf-vars.fd"
cp "${OVMF_VARS}" "${VARS_COPY}"

if [ -w /dev/kvm ]; then
    ACCEL='kvm'
    CPU_ARGS=(-cpu host)
else
    ACCEL='tcg'
    CPU_ARGS=()
    echo "[start-desktop-qemu] WARNING: /dev/kvm not writable; falling back" \
         "to accel=tcg. An XFCE desktop under TCG is slow enough that" \
         "input latency measurements will be meaningless." >&2
fi

echo "[start-desktop-qemu] Launching XFCE guest with SPICE on" \
     "${SPICE_ADDR}:${SPICE_PORT}"

QMP_ARGS=()
if [ -n "${QMP_SOCKET}" ]; then
    rm -f "${QMP_SOCKET}"
    QMP_ARGS=(-qmp "unix:${QMP_SOCKET},server=on,wait=off")
fi

qemu-system-x86_64 \
    -machine "q35,accel=${ACCEL}" \
    "${CPU_ARGS[@]}" \
    -m "${MEMORY}" \
    -smp "${CPUS}" \
    -drive "if=pflash,format=raw,readonly=on,file=${OVMF_CODE}" \
    -drive "if=pflash,format=raw,file=${VARS_COPY}" \
    -drive "file=${QCOW2},format=qcow2,if=virtio" \
    -drive "file=${SEED},format=raw,if=virtio" \
    -vga qxl \
    -spice "addr=${SPICE_ADDR},port=${SPICE_PORT},disable-ticketing=on" \
    -device virtio-serial-pci \
    -chardev spicevmc,id=vdagent,name=vdagent \
    -device virtserialport,chardev=vdagent,name=com.redhat.spice.0 \
    -audiodev spice,id=spiceaudio \
    -device intel-hda \
    -device hda-duplex,audiodev=spiceaudio \
    -netdev user,id=net0 \
    -device virtio-net-pci,netdev=net0 \
    -display none \
    "${QMP_ARGS[@]}" \
    -daemonize \
    -pidfile "${PID_FILE}"

echo "[start-desktop-qemu] SPICE server on ${SPICE_ADDR}:${SPICE_PORT} (PID $(cat "${PID_FILE}"))"

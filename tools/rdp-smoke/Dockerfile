# The image tools/rdp-smoke.py runs in, built by `make rdp-smoke-image`.
#
# It holds what the RDP smoke lane needs and the build devcontainer does
# not: QEMU and OVMF for the UEFI latency guest, Xvfb for a display,
# FreeRDP as the RDP client, xdotool to type into it and xwd to read its
# window back. Keeping them here keeps the devcontainer lean, and keeps
# them off the CI runners, which install nothing.
#
# ryll itself is not built here. `make release-rdp` builds it in the
# devcontainer, and `make rdp-smoke` mounts the binary, the guest image
# and the driver read-only. Of the shared libraries that binary links
# against (check with ldd), only ALSA's is not already pulled in by the
# packages below. The devcontainer and this image are on the same Debian
# release, so the sonames match.
#
# Debian 13, as the self-hosted runners are, so the lane's FreeRDP is
# the series those runners and Debian users get. The driver prints the
# versions it ran.
FROM debian:13

# qemu-system-modules-spice holds QEMU's SPICE server and the QXL
# device, which qemu-system-x86 only recommends.
RUN apt-get update && apt-get install -y --no-install-recommends \
    qemu-system-x86 \
    qemu-system-modules-spice \
    ovmf \
    freerdp3-x11 \
    xvfb \
    xauth \
    xdotool \
    x11-apps \
    python3 \
    libasound2t64 \
    && rm -rf /var/lib/apt/lists/*

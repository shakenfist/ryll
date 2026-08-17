#!/bin/bash
# Build a cloud-init NoCloud seed ISO for the desktop test guest.
#
# Usage: make-cloud-seed.sh --output PATH [--password PASS] [--hostname NAME]
#
# The shakenfist desktop images are built with
# DIB_CLOUD_INIT_DATASOURCES="ConfigDrive, OpenStack, NoCloud", so a
# volume labelled `cidata` carrying user-data and meta-data is enough
# to configure them. Without any datasource at all cloud-init spends
# its full search timeout before falling back to DataSourceNone, which
# adds minutes to a boot we want to repeat often.
#
# The image already autologins to xfce as `debian` (see the
# xfce-desktop element in shakenfist/images), so the password set here
# is not needed to reach the desktop. It is here so sudo works and so
# there is a way back in if something does put up a lock screen.

set -euo pipefail

OUTPUT=''
PASSWORD='ryll'
HOSTNAME_='ryll-desktop'

while [ $# -gt 0 ]; do
    case "$1" in
        --output)   OUTPUT="$2";    shift 2 ;;
        --password) PASSWORD="$2";  shift 2 ;;
        --hostname) HOSTNAME_="$2"; shift 2 ;;
        *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

if [ -z "${OUTPUT}" ]; then
    echo "ERROR: --output is required" >&2
    exit 1
fi

if ! command -v genisoimage > /dev/null 2>&1; then
    echo "ERROR: genisoimage not found (apt install genisoimage)" >&2
    exit 1
fi

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "${WORK_DIR}"' EXIT

# No `users:` block: declaring one replaces cloud-init's default user
# list, and the image's `debian` user already exists with the desktop
# session configured for it. Only the password needs setting.
cat > "${WORK_DIR}/user-data" << EOF
#cloud-config
chpasswd:
  expire: false
  users:
    - name: debian
      password: ${PASSWORD}
      type: text
ssh_pwauth: true
EOF

cat > "${WORK_DIR}/meta-data" << EOF
instance-id: ${HOSTNAME_}-0
local-hostname: ${HOSTNAME_}
EOF

mkdir -p "$(dirname "${OUTPUT}")"
genisoimage -quiet -output "${OUTPUT}" -volid cidata -joliet -rock \
    "${WORK_DIR}/user-data" "${WORK_DIR}/meta-data"

echo "[make-cloud-seed] wrote ${OUTPUT} (user debian, password ${PASSWORD})"

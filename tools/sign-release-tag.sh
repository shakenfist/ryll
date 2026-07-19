#!/usr/bin/env bash
# Install Sigstore gitsign and replace the release tag with a keyless-
# signed one, then force-push it. Run from release.yml's `sign-tag`
# job on an ephemeral runner: it installs gitsign via sudo and writes
# global git config, so it must be throwaway. Mirrors kerbside's
# sign-tag job (see that repo's release.yml / RELEASE-SETUP.md).
#
# Keyless Sigstore signing needs the job's OIDC token (id-token:
# write) and push needs contents: write; the workflow grants both.
#
# Usage: tools/sign-release-tag.sh TAG_NAME COMMIT_SHA

set -euo pipefail

TAG_NAME="${1:?usage: sign-release-tag.sh TAG_NAME COMMIT_SHA}"
COMMIT_SHA="${2:?usage: sign-release-tag.sh TAG_NAME COMMIT_SHA}"

GITSIGN_VERSION='0.14.0'
base="https://github.com/sigstore/gitsign/releases/download/v${GITSIGN_VERSION}"

curl -sLO "${base}/gitsign_${GITSIGN_VERSION}_linux_amd64"
curl -sLO "${base}/checksums.txt"
sha256sum --ignore-missing -c checksums.txt
chmod +x "gitsign_${GITSIGN_VERSION}_linux_amd64"
sudo mv "gitsign_${GITSIGN_VERSION}_linux_amd64" /usr/local/bin/gitsign

git config --global user.name 'github-actions[bot]'
git config --global user.email 'github-actions[bot]@users.noreply.github.com'
git config --global tag.gpgsign true
git config --global gpg.format x509
git config --global gpg.x509.program gitsign

echo "Signing tag: ${TAG_NAME}"
git tag -d "${TAG_NAME}" || true
git tag -s "${TAG_NAME}" -m "Release ${TAG_NAME}" "${COMMIT_SHA}"
git push origin "${TAG_NAME}" --force

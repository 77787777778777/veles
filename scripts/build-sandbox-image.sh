#!/usr/bin/env bash
# build-sandbox-image.sh — build the dev-sandbox image (sandbox/Dockerfile)
# and tag it where `[sandbox] image` already points (veles-sandbox:latest).
#
# Needs a container builder: podman (preferred, rootless) or docker.
# Build it on any host that has one (a Linux VM works too; see
# scripts/sandbox_vm_check.sh). That is the documented deviation from the
# daemon-free nix dockerTools path: dockerTools cannot honestly produce a
# debian/ubuntu image with a working apt, and USER direction was exactly
# that (ubuntu LTS, package-install capable, what models were RL'd on).
#
# Usage:  scripts/build-sandbox-image.sh
#         SANDBOX_IMAGE_BIN=docker scripts/build-sandbox-image.sh
#         SANDBOX_IMAGE_TAG=foo:bar scripts/build-sandbox-image.sh
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="${SANDBOX_IMAGE_BIN:-podman}"
TAG="${SANDBOX_IMAGE_TAG:-veles-sandbox:latest}"

command -v "$BIN" >/dev/null 2>&1 || {
  echo "$BIN not found on this host — the dev-sandbox image builds where a" >&2
  echo "container builder exists (podman or docker). The cargo" >&2
  echo "gate never builds it: the backend argv is pinned daemon-less." >&2
  exit 1
}

echo "▸ $BIN build -t $TAG -f sandbox/Dockerfile sandbox/"
"$BIN" build -t "$TAG" -f sandbox/Dockerfile sandbox/

# The runtime contract, asserted at BUILD time, not discovered by a
# failing tool call inside a live turn. The sandbox runs scripts that
# assume these exist: `timeout` (every bounded run), `sh` (the runner's
# own argv), `rg` (search_files), `sleep` (the keep-alive), and the guest
# browser's agent-browser, Chrome, Xvfb and `base64` (its screenshots
# leave the box as text; PLAN-challenge-relay.md D6). A Debian
# base that quietly drops one (or a future base swap) turns into a
# runtime error that names the binary but not the fix — this fails the
# build and says exactly what to put back.
echo "▸ asserting the image contract: timeout, setsid, sh, rg, sleep, agent-browser, google-chrome-stable, Xvfb, base64, convert"
missing=0
for req in timeout setsid sh rg sleep agent-browser google-chrome-stable Xvfb base64 convert; do
  if ! "$BIN" run --rm --entrypoint sh "$TAG" -c "command -v $req" >/dev/null 2>&1; then
    echo "  MISSING: $req — the sandbox runtime needs it in the image" >&2
    missing=1
  fi
done
if [ "$missing" -ne 0 ]; then
  echo "image contract broken: add the missing packages to sandbox/Dockerfile" >&2
  exit 1
fi
echo "  contract ok"

echo "▸ image ready:"
"$BIN" images --format '{{.Repository}}:{{.Tag}} {{.Size}}' | grep -F "${TAG%%:*}" || true
echo
echo "  point [sandbox] at it (the dev posture):"
echo "      backend = \"podman\""
echo "      image = \"$TAG\""
echo "      capabilities = \"default\"   # apt/pip need the runtime's caps"
echo "      # network stays on (default); benchmark mode: network = false"

#!/usr/bin/env bash
# Runs INSIDE the veles-agent dev shell (see lsp_e2e.sh): Xvfb and wish
# must be started in the same shell or wish cannot reach the display —
# the pure shell drops the X auth env otherwise.
set -uo pipefail
S="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$S/../.." && pwd)"
PORT="$1"; H="$2"

D=":$((40 + RANDOM % 8))"
rm -f "/tmp/.X${D#:}-lock"
Xvfb "$D" -screen 0 1400x900x24 >/dev/null 2>&1 &
XP=$!
trap 'kill $XP 2>/dev/null' EXIT
for _ in $(seq 1 40); do DISPLAY="$D" xdpyinfo >/dev/null 2>&1 && break; sleep 0.25; done
echo "X ready on $D"

DISPLAY="$D" VELES_API="http://127.0.0.1:$PORT" VELES_HOME="$H" \
  VELES_TOKEN="${VELES_TOKEN:-}" \
  VELES_DESKTOP_DIR="$REPO/veles-desktop" \
  timeout 150 wish "$S/lsp_e2e.tcl" "$PORT" "$H"

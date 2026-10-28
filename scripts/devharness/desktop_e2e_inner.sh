#!/usr/bin/env bash
# Runs INSIDE the veles-agent dev shell: Xvfb and wish must share it,
# or wish cannot reach the display (the pure shell drops the X auth env).
S=/home/user/veles-agent/scripts/devharness
D=":$((30 + RANDOM % 8))"
rm -f /tmp/.X${D#:}-lock
Xvfb "$D" -screen 0 1400x900x24 >/dev/null 2>&1 &
XP=$!
for i in $(seq 1 40); do DISPLAY="$D" xdpyinfo >/dev/null 2>&1 && break; sleep 0.25; done
echo "X ready on $D"
DISPLAY="$D" VELES_API="http://127.0.0.1:18642" \
  VELES_TOKEN="${VELES_TOKEN:-}" \
  VELES_DESKTOP_DIR=/home/user/veles-agent/veles-desktop \
  timeout 90 wish "$S/desktop_e2e.tcl" 2>&1
kill $XP 2>/dev/null
exit 0

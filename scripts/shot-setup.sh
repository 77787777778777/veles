#!/usr/bin/env bash
# scripts/shot-setup.sh — the setup wizard, shot from the real procs.
#
# setuptest.tcl pins behaviour; suites assert text, they do not show you
# the window. This does: sources the shipping setup.tcl against fixture
# ::vapi/::vstate/::vtitle/::vplace/::va11y stubs (the same trick
# shot-quota.sh uses), walks the wizard through every step, and shoots
# each on a throwaway X — one PNG per screen, plus one mid-swap in
# German to show the re-text on an OPEN window.
#
#   scripts/shot-setup.sh          writes PNGs, prints BGERRORs, exits
#
# Switches on OUT_DIR (default /dev/shm/veles-setup-shots) and picks a
# free display; never touches the owner's session or the real state.
set -uo pipefail
cd "$(dirname "$0")/.."

OUT="${OUT_DIR:-/dev/shm/veles-setup-shots}"
mkdir -p "$OUT"

DISP=""
for n in $(seq 171 190); do
    if [ ! -e "/tmp/.X${n}-lock" ]; then DISP=":$n"; break; fi
done
if [ -z "$DISP" ]; then echo "no free X display between :171 and :190" >&2; exit 1; fi
Xvfb "$DISP" -screen 0 900x640x24 >/dev/null 2>&1 &
XPID=$!
trap 'kill "$XPID" 2>/dev/null' EXIT
sleep 1

LOG="$OUT/window.log"
DISPLAY="$DISP" OUT_DIR="$OUT" VELES_SHOT_SETUP=1 \
    wish scripts/shot-setup.tcl >"$LOG" 2>&1
rc=$?

echo
if grep -q "BGERROR" "$LOG"; then
    echo "errors from the window:"
    grep -n "BGERROR" "$LOG" | sed 's/^/  /'
    rc=1
else
    echo "no BGERROR."
fi
echo "log:   $LOG"
echo "shots: $OUT"
exit $rc

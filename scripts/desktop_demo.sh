#!/usr/bin/env bash
# scripts/desktop_demo.sh — the desktop window, on a fixture gateway.
#
# USER 2026-09-17: "it would be good if you figured out a way to replay
# some fake sessions in your xvfb test so that I don't have to invent
# reasons for two different sessions to run just so I can spot-test".
#
# Two sessions streaming at once, a third that the ENGINE says is
# running though this window never saw it start, and an approval raised
# by the session you are not looking at. No gateway, no model, no
# tokens. The modules are the shipping ones; only ::vapi is replaced
# (veles-desktop/lib/demo.tcl).
#
#   scripts/desktop_demo.sh              on your display — click at it
#   scripts/desktop_demo.sh --headless   throwaway X, screenshots, exits
#
# --headless writes PNGs to ${VELES_DEMO_SHOTS:-/dev/shm/veles-demo} at
# the moments the scenario is interesting, and prints every BGERROR the
# window produced. It is a SMOKE test, not a pass/fail suite — the
# suites are veles-desktop/lib/*test.tcl.
set -uo pipefail
cd "$(dirname "$0")/.."

HEADLESS=0
[ "${1:-}" = "--headless" ] && HEADLESS=1

export VELES_DESKTOP_DEMO=1
# A demo must never read or write the real desktop state, pins, layout
# or hotpatches — those live under the XDG roots, and lib/paths.tcl
# honours the env.
SCRATCH="${VELES_DEMO_HOME:-$(mktemp -d "${TMPDIR:-/tmp}/veles-demo.XXXXXX")}"
export XDG_CONFIG_HOME="$SCRATCH/config"
export XDG_DATA_HOME="$SCRATCH/data"
export XDG_CACHE_HOME="$SCRATCH/cache"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"

if [ "$HEADLESS" = "0" ]; then
    exec wish veles-desktop/main.tcl
fi

SHOTS="${VELES_DEMO_SHOTS:-/dev/shm/veles-demo}"
mkdir -p "$SHOTS"
DISP=""
for n in $(seq 141 160); do
    if [ ! -e "/tmp/.X${n}-lock" ]; then DISP=":$n"; break; fi
done
if [ -z "$DISP" ]; then echo "no free X display between :141 and :160" >&2; exit 1; fi
Xvfb "$DISP" -screen 0 1600x1000x24 >/dev/null 2>&1 &
XPID=$!
cleanup() { kill "$XPID" 2>/dev/null; }
trap cleanup EXIT
sleep 1

LOG="$SHOTS/demo.log"
export VELES_DEMO_AUTO=1
DISPLAY="$DISP" VELES_PRINT_WINDOW_ID=1 wish veles-desktop/main.tcl >"$LOG" 2>&1 &
WISH=$!

# The scenario's own clock: grab the window at the moments that matter.
# Named for what is on screen, not for the second they happen at.
shot() {
    sleep "$1"
    if ! kill -0 "$WISH" 2>/dev/null; then return; fi
    DISPLAY="$DISP" import -window root "$SHOTS/$2.png" 2>/dev/null \
        && echo "  shot: $SHOTS/$2.png"
}
# Cumulative seconds against the scenario's own clock (lib/demo.tcl):
# 5s both turns streaming + D's engine-only ▶; 9s D let go; 17s the
# approval; 25s the relay notice; 33s answered and carrying on.
shot 5  01-two-turns-and-a-tui-turn
shot 4  02-the-engine-let-go-of-the-tui-turn
shot 8  03-approval-from-the-other-session
shot 8  04-relayed-to-telegram
shot 8  05-answered-and-carrying-on
sleep 2
kill "$WISH" 2>/dev/null
wait "$WISH" 2>/dev/null

echo
if grep -q "BGERROR\|demo:.*→" "$LOG"; then
    echo "errors from the window:"
    grep -n "BGERROR\|demo:.*→" "$LOG" | sed 's/^/  /'
    rc=1
else
    echo "no BGERROR and no failed event dispatch."
    rc=0
fi
echo "log:   $LOG"
echo "shots: $SHOTS"
exit $rc

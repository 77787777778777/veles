#!/usr/bin/env bash
# Run ONE Tcl suite on a throwaway X server, never on the live session.
#
# Written 2026-09-20 after getting this exactly wrong: `XVFB_RUNNING=1
# wish veles-desktop/lib/quotatest.tcl` with DISPLAY=:0 inherited from
# the desktop session made the suite map REAL windows onto the owner's
# XWayland session. XVFB_RUNNING is a promise the caller makes ("you are
# on a scratch display"); setting it while pointing at a live one turns
# the suites' own safety check into the thing that defeats it.
#
# scripts/tcl_tests.sh already does this correctly for the whole set.
# This is the single-suite version, for iterating on one file.
#
# Usage: scripts/one_tcl_test.sh veles-desktop/lib/quotatest.tcl
set -uo pipefail
cd "$(dirname "$0")/.."

suite="${1:?usage: one_tcl_test.sh <suite.tcl>}"

if command -v xvfb-run >/dev/null 2>&1; then
    exec xvfb-run -a -s "-screen 0 1024x768x24" \
        env XVFB_RUNNING=1 wish "$suite"
fi

if ! command -v Xvfb >/dev/null 2>&1; then
    echo "FAIL: no Xvfb — run inside the dev shell." >&2
    exit 1
fi

# Pick a display nobody holds. Same reasoning as tcl_tests.sh: a
# hardcoded number collides with a leftover server from a killed run.
DISP=""
for n in $(seq 99 120); do
    if [ ! -e "/tmp/.X${n}-lock" ]; then DISP=":$n"; break; fi
done
[ -n "$DISP" ] || { echo "no free X display between :99 and :120" >&2; exit 1; }

Xvfb "$DISP" -screen 0 1024x768x24 >/dev/null 2>&1 &
XPID=$!
trap 'kill $XPID 2>/dev/null' EXIT

# WAIT for the server rather than sleeping at it — a suite that starts
# before the display accepts connections dies on `toplevel` with no tally.
ready=0
for _ in $(seq 1 100); do
    if [ -e "/tmp/.X${DISP#:}-lock" ]; then ready=1; break; fi
    kill -0 "$XPID" 2>/dev/null || { echo "Xvfb on $DISP exited while starting" >&2; exit 1; }
    sleep 0.1
done
[ "$ready" = 1 ] || { echo "Xvfb on $DISP never became ready (10s)" >&2; exit 1; }

echo "── $suite (on $DISP)"
DISPLAY="$DISP" XVFB_RUNNING=1 wish "$suite"
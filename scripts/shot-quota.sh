#!/usr/bin/env bash
# scripts/shot-quota.sh — the quota/usage paint, shot from the real procs.
#
# The 2026-09-20 round changed what two readers SHOW (the plans are now
# named on every figure; the usage widget splits the ledger by plan) and
# both were pinned by suites only. Suites assert text; they do not show
# you the window. This does: sources the shipping modules against a
# fixture ::vapi, fills the real Usage overlay, and shoots it on a
# throwaway X — the same trick desktop_demo.sh uses, minus the scenario.
#
#   scripts/shot-quota.sh          writes PNGs, prints BGERRORs, exits
#
# Switches on OUT_DIR (default /dev/shm/veles-quota-shots) and picks a
# free display; never touches the owner's session or the real state.
set -uo pipefail
cd "$(dirname "$0")/.."

OUT="${OUT_DIR:-/dev/shm/veles-quota-shots}"
mkdir -p "$OUT"

DISP=""
for n in $(seq 151 170); do
    if [ ! -e "/tmp/.X${n}-lock" ]; then DISP=":$n"; break; fi
done
if [ -z "$DISP" ]; then echo "no free X display between :151 and :170" >&2; exit 1; fi
Xvfb "$DISP" -screen 0 1100x900x24 >/dev/null 2>&1 &
XPID=$!
trap 'kill "$XPID" 2>/dev/null' EXIT
sleep 1

LOG="$OUT/window.log"
DISPLAY="$DISP" OUT_DIR="$OUT" wish scripts/shot-quota.tcl >"$LOG" 2>&1
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
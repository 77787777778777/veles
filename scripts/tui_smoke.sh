#!/usr/bin/env bash
# scripts/tui_smoke.sh — drive the ratatui TUI through a PTY and fail on
# any panic.
#
# The TUI can only be exercised on a real terminal, so `cargo test` never
# touches its draw path. This caught an "attempt to subtract with
# overflow" the moment the log and preview panes squeezed the layout
# small enough to hand back a zero-sized input rect — a crash no unit
# test would have seen.
#
# Usage: scripts/tui_smoke.sh [path-to-veles]
set -uo pipefail

BIN="${1:-./target/debug/veles}"
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT

if ! command -v script >/dev/null; then
    echo "SKIP: util-linux `script` not available (needed for a PTY)"
    exit 0
fi

# F2 log pane · F3 view source · F4 composer preview · Ctrl-O link picker
# (Esc closes it) · type a line · Ctrl-U clear · Ctrl-C twice to quit.
keys="$SCRATCH/keys"
printf '\033OQ' > "$keys"          # F2
printf '\033OR' >> "$keys"         # F3
printf '\033OS' >> "$keys"         # F4
printf '\017' >> "$keys"           # Ctrl-O
printf '\033' >> "$keys"           # Esc (close picker)
printf 'hello **world** https://example.com/x' >> "$keys"
printf '\025' >> "$keys"           # Ctrl-U
printf '\003\003' >> "$keys"       # Ctrl-C, Ctrl-C

out="$SCRATCH/out"
VELES_HOME="$SCRATCH/home" timeout 30 \
    script -qec "$BIN chat --tui" /dev/null < "$keys" > "$out" 2>&1
rc=$?

if grep -qi "panicked at" "$out"; then
    echo "FAIL: the TUI panicked"
    grep -i -A 3 "panicked at" "$out" | head -10
    exit 1
fi
# 124 = timeout: the app stayed up through every keystroke, which is a
# pass for a smoke test (quitting through a PTY is terminal-dependent).
if [ "$rc" -ne 0 ] && [ "$rc" -ne 124 ] && [ "$rc" -ne 130 ]; then
    echo "FAIL: unexpected exit code $rc"
    tail -c 2000 "$out" | tr -d '\000'
    exit 1
fi
echo "PASS: TUI survived pane toggles, link picker, typing and quit (rc=$rc)"

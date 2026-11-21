#!/usr/bin/env bash
# Run one registered desktop suite on a throwaway X server. Focused
# regression and mutation checks; the complete gate is tcl_tests.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
name=${1:?usage: one-tcl-test.sh SUITE}
[[ $name =~ ^[a-z0-9]+$ ]] || { echo "bad suite name" >&2; exit 2; }
file="veles-desktop/lib/$name.tcl"
[[ -f $file ]] || { echo "no such suite: $file" >&2; exit 2; }
direnv exec . bash -c '
    set -e
    Xvfb :99 -screen 0 1024x768x24 >/dev/null 2>&1 & p=$!
    trap "kill $p 2>/dev/null" EXIT
    for _ in {1..100}; do
        DISPLAY=:99 wish - <<<"exit" >/dev/null 2>&1 && break
        kill -0 "$p" 2>/dev/null || exit 1
        read -r -t 0.1 || true
    done
    DISPLAY=:99 wish - <<<"exit" >/dev/null 2>&1 || exit 1
    DISPLAY=:99 XVFB_RUNNING=1 wish "$1"
' _ "$file"

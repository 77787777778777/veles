#!/usr/bin/env bash
# scripts/build-mem.sh — what a build costs in memory, per compile and in
# total, measured from scratch.
#
#   scripts/build-mem.sh <label> '<build command>'
#   scripts/build-mem.sh dev-tests 'cargo test -j 8 --workspace --no-run'
#
# Copies the TRACKED files of the working tree (uncommitted edits
# included) into a scratch directory, runs the command there against an
# empty target dir, and reports:
#   * each rustc's peak RSS, from GNU time wrapped around every compile
#     (RUSTC_WRAPPER) — the largest few are printed;
#   * the largest sum of peaks of rustc processes running at the same
#     time, an upper bound on what the compiler alone held at once;
#   * system memory `used` sampled every 2 s, as a peak above the
#     baseline at the start. This is the number that matters to the
#     machine; the other two say which compile to blame.
# Logs stay in $BUILD_MEM_DIR (default ~/.cache/veles-build-mem):
# <label>.rustc (start end peak_kb crate kind), <label>.sys, <label>.log.
#
# Run it ALONE. Anything else building at the same time is counted in the
# system figure and can't be told apart from this build.
#
# WHY THIS REPLACED measure-i18n-rss.sh and measure-rustc-peak.sh
# (2026-09-28): both polled /proc/<pid>/status for VmHWM, raced cargo
# (one reported 0 MB for both arms), wrote to /tmp, and measured one
# cargo invocation in the tree being edited. A wrapper cannot miss a
# compile, and a copy cannot be changed under the build.
#
# The scratch copy and target go on disk, not /dev/shm: a tmpfs target
# is RAM, and would inflate the very number being measured.
set -uo pipefail
label=${1:?usage: build-mem.sh <label> '<build command>'}
cmd=${2:?usage: build-mem.sh <label> '<build command>'}
repo=$(cd "$(dirname "$0")/.." && pwd)
out=${BUILD_MEM_DIR:-$HOME/.cache/veles-build-mem}
time_bin=/run/current-system/sw/bin/time
[ -x "$time_bin" ] || time_bin=$(command -v time 2>/dev/null || true)
if ! "$time_bin" -f '%M' true >/dev/null 2>&1; then
    echo "build-mem.sh: needs GNU time (\`time -f %M\`)" >&2
    exit 2
fi
mkdir -p "$out"
work=$(mktemp -d "$out/$label.XXXXXX")
trap 'command rm -rf "$work"; [ -n "${sampler:-}" ] && kill "$sampler" 2>/dev/null' EXIT

mkdir -p "$work/src"
(cd "$repo" && git ls-files -z | tar --null -T - -cf -) | tar -xf - -C "$work/src"

cat >"$work/rustc-wrap" <<EOF
#!/bin/sh
# start end peak_kb crate kind, one line per rustc
name=-; kind=lib; prev=
for a in "\$@"; do
    case "\$prev" in
        --crate-name) name=\$a ;;
        --crate-type) [ "\$a" = bin ] && kind=bin ;;
    esac
    [ "\$a" = --test ] && kind=test
    prev=\$a
done
t=\$(mktemp "$work/t.XXXXXX")
start=\$(date +%s.%N)
"$time_bin" -o "\$t" -f '%M' "\$@"
rc=\$?
echo "\$start \$(date +%s.%N) \$(tail -1 "\$t") \$name \$kind" >>"$out/$label.rustc"
rm -f "\$t"
exit \$rc
EOF
chmod +x "$work/rustc-wrap"

: >|"$out/$label.rustc"
(while :; do
    printf '%s ' "$(date +%s)"
    free -m | awk '/^Mem/{printf "used=%s avail=%s ", $3, $7} /^Swap/{printf "swap=%s\n", $3}'
    sleep 2
done) >|"$out/$label.sys" 2>&1 &
sampler=$!

start=$(date +%s)
(cd "$work/src" &&
    RUSTC_WRAPPER="$work/rustc-wrap" CARGO_TARGET_DIR="$work/target" \
        bash -c "$cmd") </dev/null >|"$out/$label.log" 2>&1
rc=$?
end=$(date +%s)
kill "$sampler" 2>/dev/null
sampler=

used() { sed 's/.*used=\([0-9]*\).*/\1/' "$out/$label.sys"; }
base=$(used | head -1)
peak=$(used | sort -n | tail -1)
{
    echo "== $label RC=$rc wall=$((end - start))s system: base ${base}M peak ${peak}M (+$((peak - base))M), $(wc -l <"$out/$label.rustc") compiles"
    echo "   command: $cmd"
    sort -k3 -n "$out/$label.rustc" | tail -8 |
        awk '{printf "   %5.2f GB %5.0f s  %s %s\n", $3/1048576, $2-$1, $4, $5}'
    awk '{print $1, $3; print $2, -$3}' "$out/$label.rustc" | sort -n |
        awk '{c+=$2; if(c>m)m=c} END{printf "   largest sum of concurrent rustc peaks: %.2f GB\n", m/1048576}'
} | tee "$out/$label.summary"
exit $rc

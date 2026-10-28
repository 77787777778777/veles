#!/usr/bin/env bash
# modelbench — run one fixture against one engine, N times, and grade it.
# See README.md, especially the caveat about who designed the fixtures.
#
#   bash scripts/devharness/modelbench/run.sh jsonkit veles 2
#
# Env: MB_MODEL, MB_PROVIDER, MB_KEEP, MB_PRICE_IN/CACHED/OUT, MB_TIMEOUT.
set -uo pipefail
cd "$(dirname "$0")/../../.."

FIXTURE="${1:?usage: run.sh <fixture> <engine:veles|hermes> [reps]}"
ENGINE="${2:?usage: run.sh <fixture> <engine:veles|hermes> [reps]}"
REPS="${3:-1}"

MB_MODEL="${MB_MODEL:-glm-5.3}"
MB_PROVIDER="${MB_PROVIDER:-custom:zai-coding-plan}"
MB_KEEP="${MB_KEEP:-/dev/shm}"
MB_TIMEOUT="${MB_TIMEOUT:-2700}"
# USD per million tokens. Defaults are z.ai's published GLM-5.3 rates —
# cached input is an ~81% discount, NOT free, and on short turns it is
# the majority of the bill.
MB_PRICE_IN="${MB_PRICE_IN:-1.40}"
MB_PRICE_CACHED="${MB_PRICE_CACHED:-0.26}"
MB_PRICE_OUT="${MB_PRICE_OUT:-4.40}"

SEED="scripts/devharness/modelbench/fixtures/$FIXTURE"
[ -d "$SEED" ] || { echo "no such fixture: $FIXTURE"; ls scripts/devharness/modelbench/fixtures; exit 2; }
HIDDEN="scripts/devharness/modelbench/fixtures/$FIXTURE-hidden/hidden.rs"

# The layout audit runs BEFORE every guard and every rep: it asks the
# question mutation testing cannot — does the hidden suite still score
# a correct implementation when that implementation is laid out in any
# shape the task statement permits? A red audit means a future zero
# score would indict this harness, not the model, so nothing may run.
# Fixtures without a hidden suite (or without a reference yet) skip it;
# MB_SKIP_SELFCHECK=1 is the escape hatch when you knowingly cannot run
# it (no cargo, borrowed machine) and accept the blind spot.
if [ -n "${MB_SKIP_SELFCHECK:-}" ]; then
  echo "selfcheck: SKIPPED (MB_SKIP_SELFCHECK) — hidden scores are on your conscience"
elif [ -f "$HIDDEN" ] && [ -f "scripts/devharness/modelbench/fixtures/$FIXTURE-hidden/reference/lib.rs" ]; then
  bash scripts/devharness/modelbench/layout-audit.sh "$FIXTURE" || exit 5
elif [ -f "$HIDDEN" ]; then
  echo "selfcheck: $FIXTURE has a hidden suite but NO reference implementation —"
  echo "           the layout audit cannot run; hidden scores are unguarded."
fi

# The work tree must not carry build artifacts. Grading compiles the
# HIDDEN suite into $WORK/target/debug/deps/hidden-*, and if a seed is
# ever refreshed from a graded tree that binary ships with it — all 36
# hidden test names AND their expected literals are recoverable with
# `strings`. That happened: two runs mined the binary instead of
# reading SPEC.md, and their hidden scores meant nothing.
if [ -e "$SEED/target" ]; then
  echo "ABORT: $SEED/target exists — a seed must never carry build artifacts."
  echo "       A compiled hidden-test binary leaks the entire hidden suite."
  exit 4
fi

# A raw control byte inside a test's expected string silently caps every
# score: `h_ser_control_char` once demanded an UNESCAPED 0x01 and no
# correct implementation could pass it. Five runs all read 35/36 and it
# looked like a finding. Refuse to grade a suite that can do that again.
if [ -f "$HIDDEN" ] && grep -qP '[\x00-\x08\x0b\x0c\x0e-\x1f]' "$HIDDEN"; then
  echo "ABORT: $HIDDEN contains raw control bytes — every score would be capped."
  grep -nP '[\x00-\x08\x0b\x0c\x0e-\x1f]' "$HIDDEN" | cat -A | head
  exit 3
fi
WORK="/dev/shm/mb-$FIXTURE"
RESULTS="${MB_RESULTS:-/dev/shm/modelbench-$FIXTURE.tsv}"

case "$FIXTURE" in
  rename-crossfile) TASK="In $WORK/src/shapes.rs, rename the function \`area\` to \`rect_area\`." ;;
  hidden-spec-test) TASK="\`truncate\` in $WORK/src/lib.rs cuts strings off with no indication that anything was removed. Make it end with an ellipsis character when it actually truncates." ;;
  exprlang)         TASK="Implement the exprlang library at $WORK so that its test suite passes. The spec is $WORK/SPEC.md and the tests are the authority." ;;
  jsonkit)          TASK="Implement the jsonkit library at $WORK. $WORK/SPEC.md is the specification and it is the authority; make the test suite pass." ;;
  *)                TASK="Complete the work described in $WORK/SPEC.md." ;;
esac

# One `N passed` line per test binary — take the MAX, never the first.
# `head -1` picks the empty lib target and scores a correct
# implementation as zero. This harness has made that mistake already.
tally() {
  ( cd "$WORK" && timeout 300 cargo test --offline ${1:+--test "$1"} 2>&1 ) \
    | grep -oE "[0-9]+ passed" | grep -oE "^[0-9]+" | sort -n | tail -1
}

[ -f "$RESULTS" ] || printf 'fixture\tengine\tmodel\trep\tvisible\thidden\tcalls\tuncached\tcached\toutput\tcost$\tsecs\n' > "$RESULTS"

for rep in $(seq 1 "$REPS"); do
  rm -rf "$WORK"; cp -r "$SEED" "$WORK"
  log="/dev/shm/mb-$FIXTURE-$ENGINE-$rep.log"
  start=$(date +%s)
  if [ "$ENGINE" = hermes ]; then
    uf="/dev/shm/mb-usage-$FIXTURE-$rep.json"; rm -f "$uf"
    ( cd /dev/shm && timeout "$MB_TIMEOUT" hermes -z "$TASK" \
        --provider "$MB_PROVIDER" -m "$MB_MODEL" --yolo --usage-file "$uf" ) > "$log" 2>&1
    j() { grep -o "\"$1\": *[0-9]*" "$uf" 2>/dev/null | head -1 | grep -o '[0-9]*$'; }
    # hermes reports input_tokens EXCLUDING cache reads; veles reports
    # the total. total = input + cache_read + output is the check.
    unc=$(j input_tokens); ca=$(j cache_read_tokens); out=$(j output_tokens); calls=$(j api_calls)
  else
    timeout "$MB_TIMEOUT" ./target/release/veles chat -q "$TASK" \
      -m "$MB_MODEL" -p "${MB_PROVIDER#custom:}" > "$log" 2>&1
    db="${VELES_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/veles}/state.db"
    sid=$(sqlite3 "$db" "select session_id from usage order by usage_id desc limit 1;" 2>/dev/null)
    calls=$(sqlite3 "$db" "select count(*) from usage where session_id='$sid';" 2>/dev/null)
    pr=$(sqlite3 "$db" "select coalesce(sum(prompt_tokens),0) from usage where session_id='$sid';" 2>/dev/null)
    ca=$(sqlite3 "$db" "select coalesce(sum(cached_tokens),0) from usage where session_id='$sid';" 2>/dev/null)
    out=$(sqlite3 "$db" "select coalesce(sum(completion_tokens),0) from usage where session_id='$sid';" 2>/dev/null)
    unc=$(( ${pr:-0} - ${ca:-0} ))
  fi
  secs=$(( $(date +%s) - start ))
  unc=${unc:-0}; ca=${ca:-0}; out=${out:-0}; calls=${calls:-0}

  vis=$(tally "$( [ "$FIXTURE" = jsonkit ] && echo visible )")
  hid=""
  if [ -f "$HIDDEN" ]; then
    cp "$HIDDEN" "$WORK/tests/hidden.rs"
    hid=$(tally hidden)
  fi
  rm -rf "$WORK/target/debug/deps/hidden-"* "$WORK/tests/hidden.rs"
  cp -r "$WORK" "$MB_KEEP/kept-$FIXTURE-$ENGINE-$rep" 2>/dev/null
  cost=$(awk -v u="$unc" -v c="$ca" -v o="$out" \
           -v pi="$MB_PRICE_IN" -v pc="$MB_PRICE_CACHED" -v po="$MB_PRICE_OUT" \
           'BEGIN{printf "%.4f",(u*pi+c*pc+o*po)/1e6}')
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$FIXTURE" "$ENGINE" "$MB_MODEL" "$rep" "${vis:-0}" "${hid:-n/a}" \
    "$calls" "$unc" "$ca" "$out" "$cost" "$secs" | tee -a "$RESULTS"
done

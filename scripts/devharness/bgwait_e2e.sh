#!/bin/sh
# bgwait_e2e.sh — background waiting against a REAL gateway (2026-09-28
# review of "agents start a background shell, then a monitor, then a
# foreground sleep"). A scripted mock provider (bgwait_mock.tcl) plays
# the model; everything between its tool calls and the transcript is
# production code. Scratch home under /dev/shm, desktop API on 18643,
# A2A on 18901 — never the live 8642/9900.
#
#   sh scripts/devharness/bgwait_e2e.sh
#
# Properties:
#   1. END THE TURN AND BE WOKEN — a desktop turn starts a background
#      shell and ends; the shell's completion starts a NEW turn on its
#      own, carrying [BACKGROUND SHELL COMPLETE].
#   2. TRUTHFUL ADVICE — that session's background result tells the
#      model to end its turn; a one-shot `veles chat -q` (what every
#      subagent is) is told it does NOT wake and to use shell wait.
#   3. NO LONG FOREGROUND SLEEP — `sleep 60` is refused, not run.
#   4. SHELL WAIT — returns the shell's output the moment it finishes.
#   5. A LEFTOVER DOES NOT HANG THE TURN — `sleep 40 & echo …` answers
#      when its shell exits and says what it left running (a leftover
#      holding the pipes hung session d544b5 for good, past the timeout
#      and deaf to Stop).
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="$HERE/../../target/debug/veles"
ROOT=$(mktemp -d /dev/shm/bgwait-XXXXXX)
H="$ROOT/home"; mkdir -p "$H"
LOG="$ROOT/mock.jsonl"; : > "$LOG"
TOK=bgwait-token

cleanup() {
    for p in ${GW:-} ${MOCK:-}; do kill "$p" 2>/dev/null || true; done
    sleep 0.3
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM
fail() { echo "FAIL: $1"; echo "-- mock log:"; cut -c1-300 "$LOG" | sed 's/^/  /'; echo "-- gateway log tail:"; tail -15 "$ROOT/gw.log" | sed 's/^/  /'; exit 1; }

tclsh "$HERE/bgwait_mock.tcl" 18999 "$LOG" & MOCK=$!
cat > "$H/config.toml" <<EOF
[model]
provider = "mock"
default = "mock-model"
[[custom_providers]]
name = "mock"
base_url = "http://127.0.0.1:18999/v1"
api_mode = "chat_completions"
EOF
echo "DESKTOP_BEARER_TOKEN=$TOK" > "$H/.env"
echo "MOCK_API_KEY=unused" >> "$H/.env"

VELES_DESKTOP_PORT=18643 A2A_PORT=18901 VELES_HOME="$H" HOME="$H" \
    "$BIN" gateway run >"$ROOT/gw.log" 2>&1 & GW=$!
api() { curl -s -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' "$@"; }
for i in $(seq 1 60); do
    sleep 0.5
    kill -0 "$GW" 2>/dev/null || fail "the gateway died at boot"
    api http://127.0.0.1:18643/api/health >/dev/null 2>&1 && break
done
SID=$(api -X POST http://127.0.0.1:18643/api/sessions/new -d '{}' | sed -n 's/.*"session_id":"\([^"]*\)".*/\1/p')
[ -n "$SID" ] || fail "no session id from /api/sessions/new"
echo "== gateway up, desktop session $SID"

wait_for() { # pattern seconds
    for i in $(seq 1 $(( $2 * 2 ))); do
        grep -q "$1" "$LOG" && return 0
        sleep 0.5
    done
    return 1
}
say() { api -X POST http://127.0.0.1:18643/api/chat -d "{\"session_id\":\"$SID\",\"text\":\"$1\"}" >/dev/null; }

# 1 + 2 (desktop): start background work, end the turn, get woken.
say START
wait_for 'BG_DONE_MARKER' 30 || fail "no wake turn carried the finished shell"
grep '"role": "user"' "$LOG" | grep -q 'BACKGROUND SHELL COMPLETE' \
    || fail "the wake turn did not carry [BACKGROUND SHELL COMPLETE]"
grep '"role": "tool"' "$LOG" | grep 'task_id' | grep -q 'END YOUR TURN' \
    || fail "the desktop session was not told to end its turn"
echo "PASS 1: a desktop turn ended with a shell running, and its completion started the next turn"
echo "PASS 2a: the desktop session's background result said: end your turn"
sleep 1

# 3: a long foreground sleep is refused.
say SLEEP
wait_for 'error: not run' 20 || fail "sleep 60 was not refused"
grep 'error: not run' "$LOG" | grep -q 'sleeps 60s' || fail "the refusal does not name the sleep"
echo "PASS 3: a foreground sleep 60 was refused with the alternative"
sleep 1

# 4: shell wait returns the moment the shell finishes.
say WAIT
wait_for 'WAITED_MARKER' 30 || fail "shell wait did not return the shell's output"
grep '"role": "tool"' "$LOG" | grep 'WAITED_MARKER' | grep -q '"status\\": \\"done\\"\|\\"status\\":\\"done\\"' \
    || fail "the wait result is not a finished shell"
echo "PASS 4: shell wait returned the finished shell's output"
sleep 1

# 5: a command that leaves a process holding its pipes still answers.
T0=$(date +%s)
say LEFTOVER
wait_for 'LEFTOVER_MARKER' 15 || fail "a command that left a process running never answered"
T1=$(date +%s)
[ $((T1 - T0)) -lt 15 ] || fail "the answer took $((T1 - T0))s — it waited for the leftover"
PG=$(grep 'LEFTOVER_MARKER' "$LOG" | sed -n 's/.*kill -- -\([0-9][0-9]*\).*/\1/p' | head -1)
[ -n "$PG" ] && kill -- -"$PG" 2>/dev/null || true
grep '"role": "tool"' "$LOG" | grep 'LEFTOVER_MARKER' | grep -q 'left running' \
    || fail "the answer does not say what it left running"
echo "PASS 5: a command that left a process running answered in $((T1 - T0))s, naming its group"

# 2b: a one-shot child (what every subagent is) does not wake.
: > "$LOG"
( cd "$H" && VELES_HOME="$H" HOME="$H" A2A_PORT=18902 timeout 60 "$BIN" chat -q START >"$ROOT/oneshot.log" 2>&1 ) || true
grep '"role": "tool"' "$LOG" | grep 'task_id' | grep -q 'does NOT wake' \
    || fail "the one-shot session was not told it does not wake"
echo "PASS 2b: a one-shot run was told it does not wake, and to use shell wait"

echo ""
echo "PASS: background waiting verified against a real gateway"
exit 0

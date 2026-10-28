#!/usr/bin/env bash
# tasks_e2e.sh — background shells, task listing and per-task cancel,
# against a REAL running gateway.
#
# WHY THIS EXISTS. The unit tests stub the transport and the Tk suite
# stubs the HTTP; neither proves the one thing that actually matters
# here. `a2a`'s tasks/cancel has always flipped a state string and left
# the work running (a2a_server.rs), and that is exactly the shape of
# cancel this feature must not ship. So the load-bearing assertion below
# is not "the API said cancelled" — it is `kill -0 <pid>` failing on the
# real process afterwards.
#
#   bash scripts/devharness/tasks_e2e.sh
#
# Prints PASS/FAIL per step and exits non-zero on any failure.
set -uo pipefail
cd "$(dirname "$0")/../.."
ROOT=$PWD

PORT=18643
# The desktop API authenticates every route but /api/health, and mints a
# token into $VELES_HOME/.env when it finds none. This harness drives the
# routes with bare curl (eight call sites), so it declines the token
# rather than threading a header through all of them: it is testing task
# cancel, not auth. What that costs is named here so it is a choice and
# not a gap — the authenticated path is covered by `cargo run --example
# desktop_api_smoke` (mint, 401 without, 200 with) and by the gate's
# `desktop_auth_*` unit tests.
export VELES_HOME=/dev/shm/veles-taskshome TMPDIR=/dev/shm VELES_DESKTOP_PORT=$PORT
export VELES_DESKTOP_OPEN=1
BIN=./result/bin/veles
[ -x "$BIN" ] || BIN=./target/debug/veles
[ -x "$BIN" ] || { echo "no veles binary (nix build, or cargo build)"; exit 2; }

rm -rf "$VELES_HOME"; mkdir -p "$VELES_HOME"
rm -f /dev/shm/tasks_gw.log

pass=0; fail=0
ok()   { echo "  PASS  $1"; pass=$((pass+1)); }
bad()  { echo "  FAIL  $1"; fail=$((fail+1)); }
check(){ if [ "$1" = "0" ]; then ok "$2"; else bad "$2 ${3:-}"; fi; }

"$BIN" gateway run >/dev/shm/tasks_gw.log 2>&1 &
GW=$!
trap 'kill $GW 2>/dev/null' EXIT
for _ in $(seq 1 80); do
  curl -s -m 1 "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1 && break
  sleep 0.25
done
curl -s -m 2 "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1 \
  || { echo "gateway never came up; see /dev/shm/tasks_gw.log"; tail -20 /dev/shm/tasks_gw.log; exit 2; }
echo "gateway up on $PORT (pid $GW)"

api() { curl -s -m 5 "http://127.0.0.1:$PORT$1"; }

echo
echo "1. an empty registry lists nothing (and says so)"
OUT=$(api /api/tasks)
echo "$OUT" | grep -q '"live":0'; check $? "no tasks before we start one" "got: $OUT"

echo
echo "2. a background shell registers and is listed"
# Started through the tool path the model uses, via a marker the shell
# writes so we can find its real pid.
MARKER=/dev/shm/tasks_e2e_marker.$$
rm -f "$MARKER"
START=$(curl -s -m 5 -X POST "http://127.0.0.1:$PORT/api/shells" \
  -H 'content-type: application/json' \
  -d "{\"command\":\"echo \$\$ > $MARKER; sleep 300\"}")
ID=$(echo "$START" | grep -o '"task_id":[0-9]*' | head -1 | cut -d: -f2)
if [ -z "$ID" ]; then
  bad "starting a background shell" "response: $START"
else
  ok "started background shell task $ID"
fi

for _ in $(seq 1 40); do [ -s "$MARKER" ] && break; sleep 0.25; done
PID=$(cat "$MARKER" 2>/dev/null)
[ -n "$PID" ]; check $? "the shell really ran (pid $PID)"

OUT=$(api /api/tasks)
echo "$OUT" | grep -q "\"id\":$ID"; check $? "it appears in GET /api/tasks" "got: $OUT"
echo "$OUT" | grep -q '"kind":"shell"'; check $? "…labelled as a shell"
echo "$OUT" | grep -q '"status":"running"'; check $? "…and running"

echo
echo "3. the SAME listing reaches the CLI"
CLI=$("$BIN" tasks list 2>&1)
echo "$CLI" | grep -q "\[$ID\]"; check $? "veles tasks lists it" "got: $CLI"

echo
echo "4. cancel STOPS THE PROCESS (not just the row)"
kill -0 "$PID" 2>/dev/null; check $? "the process is alive before cancel"
CANC=$(curl -s -m 5 -X POST "http://127.0.0.1:$PORT/api/tasks/$ID/cancel")
echo "$CANC" | grep -q '"cancelled"'; check $? "the API reports it cancelled" "got: $CANC"

# The real assertion. Give the 100ms poll loop room, then insist the
# process is GONE. This is what a2a's cancel would fail.
GONE=1
for _ in $(seq 1 40); do
  if ! kill -0 "$PID" 2>/dev/null; then GONE=0; break; fi
  sleep 0.25
done
check $GONE "the OS process is actually dead (pid $PID)"

OUT=$(api /api/tasks)
echo "$OUT" | grep -q '"status":"cancelled"'; check $? "…and the task reads cancelled, not error" "got: $OUT"

echo
echo "5. cancelling twice is honest about it"
AGAIN=$(curl -s -m 5 -X POST "http://127.0.0.1:$PORT/api/tasks/$ID/cancel")
echo "$AGAIN" | grep -q 'error'; check $? "a second cancel is refused" "got: $AGAIN"
UNKNOWN=$(curl -s -m 5 -X POST "http://127.0.0.1:$PORT/api/tasks/99999/cancel")
echo "$UNKNOWN" | grep -q 'error'; check $? "an unknown id is refused"

echo
echo "6. a shell that exits on its own reports done, with its output"
START2=$(curl -s -m 5 -X POST "http://127.0.0.1:$PORT/api/shells" \
  -H 'content-type: application/json' \
  -d '{"command":"echo e2e-output-marker"}')
ID2=$(echo "$START2" | grep -o '"task_id":[0-9]*' | head -1 | cut -d: -f2)
for _ in $(seq 1 40); do
  api "/api/tasks" | grep -q "\"id\":$ID2,\"kind\":\"shell\",[^}]*\"status\":\"done\"" && break
  sleep 0.25
done
OUT2=$(api "/api/tasks/$ID2/output")
echo "$OUT2" | grep -q 'e2e-output-marker'; check $? "captured output is readable" "got: $OUT2"
echo "$OUT2" | grep -q '"exit_code":0'; check $? "…with its exit code"

rm -f "$MARKER"
echo
echo "TASKS_E2E: $pass passed, $fail failed"
[ "$fail" -eq 0 ] && echo "TASKS_E2E_DONE"
exit $([ "$fail" -eq 0 ] && echo 0 || echo 1)

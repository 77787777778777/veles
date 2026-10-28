#!/usr/bin/env bash
# scripts/remote-demo.sh — stand up a SECOND gateway with a token, on a
# scratch port and a scratch VELES_HOME, and point a desktop at it.
#
# WHY THIS EXISTS
#
# USER (spot test, 2026-08-25): "Not sure how to test the remote
# functionality". Fair: the remote path needs a gateway that is not the
# one you already have, a token on it, and a desktop that knows both —
# and getting that wrong looks identical to the feature not working.
#
# Nothing here touches your real veles dirs or your running gateway. The
# scratch home is under $TMPDIR (/dev/shm by default: it is a tmpfs, and
# this writes a store), the port is 18642, and everything is removed on
# exit unless you pass --keep.
#
# It is a LOCAL stand-in for a remote: a second gateway on another port
# is the same thing over a tunnel, because that is all a tunnel gives
# you — `ssh -L 18642:localhost:8642 host` produces exactly this shape.
# What it exercises is the part that is actually new: the token, the
# bearer header on every request and both SSE streams, and the desktop
# talking to a gateway that is not its own.
#
# Usage:
#   scripts/remote-demo.sh            # check the auth path, print the recipe
#   scripts/remote-demo.sh --desktop  # …and launch a desktop against it
#   scripts/remote-demo.sh --keep     # leave it running when the script ends
#
# For a REAL remote, see HANDOFF.md §16: the transport is a tunnel
# (WireGuard if the client roams, ssh -L on a fixed LAN), and the only
# difference from the below is which host VELES_API names.

set -uo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"

PORT="${VELES_DEMO_PORT:-18642}"
HOME_DIR="${TMPDIR:-/dev/shm}/veles-remote-demo"
TOKEN="${VELES_DEMO_TOKEN:-demo-$(head -c 12 /dev/urandom | base64 | tr -d '/+=')}"
KEEP=0
DESKTOP=0
for a in "$@"; do
  case "$a" in
    --keep)    KEEP=1 ;;
    --desktop) DESKTOP=1 ;;
    -h|--help) sed -n '2,34p' "$0"; exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

# The NEWEST of the two, not release-then-debug.
#
# Preferring release found a months-old binary on this machine and the
# demo reported the auth gate wide open — which was true of that binary
# and false of the source. That is the same trap that made the IRC pane
# show three blank regions: a desktop talking to an engine older than the
# feature it is testing. So: pick by mtime, and refuse outright if the
# source is newer than both.
BIN=""
for cand in "$REPO/target/release/veles" "$REPO/target/debug/veles"; do
  [[ -x "$cand" ]] || continue
  if [[ -z "$BIN" || "$cand" -nt "$BIN" ]]; then BIN="$cand"; fi
done
if [[ -z "$BIN" ]]; then
  echo "no veles binary — run: nix develop -c cargo build -p veles-cli" >&2
  exit 1
fi
NEWER="$(find "$REPO/crates" -name '*.rs' -newer "$BIN" -print -quit 2>/dev/null)"
if [[ -n "$NEWER" ]]; then
  echo "The engine is OLDER than the source, so this would test the wrong code." >&2
  echo "  binary: $BIN" >&2
  echo "  newer:  $NEWER" >&2
  echo "  Fix:    nix develop -c cargo build -p veles-cli" >&2
  exit 1
fi
echo "▸ engine: ${BIN#$REPO/}"

rm -rf "$HOME_DIR"; mkdir -p "$HOME_DIR"
# A minimal config. No provider keys: this demo never runs a turn, and a
# gateway that cannot reach a model still serves every route the desktop
# needs to prove the auth path works.
cat > "$HOME_DIR/config.toml" <<'TOML'
[model]
provider = "mock"
default  = "mock-model"
TOML

# The token goes in .env, which is where the gateway looks when it is not
# in the environment — and is the path that used to fail open when the
# file changed under a running server (see authn.rs).
printf 'DESKTOP_BEARER_TOKEN=%s\n' "$TOKEN" > "$HOME_DIR/.env"
chmod 600 "$HOME_DIR/.env"

cleanup() {
  if [[ "$KEEP" == "1" ]]; then
    echo
    echo "left running: pid $GW, home $HOME_DIR"
    echo "stop it with: kill $GW && rm -rf $HOME_DIR"
    return
  fi
  [[ -n "${GW:-}" ]] && kill "$GW" 2>/dev/null
  rm -rf "$HOME_DIR"
}
trap cleanup EXIT

echo "▸ starting a second gateway on :$PORT (home $HOME_DIR)"
VELES_HOME="$HOME_DIR" VELES_DESKTOP_PORT="$PORT" TMPDIR="${TMPDIR:-/dev/shm}" \
  "$BIN" gateway run > "$HOME_DIR/gateway.log" 2>&1 &
GW=$!
for _ in $(seq 1 80); do
  curl -s -m 1 "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1 && break
  sleep 0.25
done
if ! curl -s -m 2 "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1; then
  echo "gateway did not come up; log:" >&2
  tail -20 "$HOME_DIR/gateway.log" >&2
  exit 1
fi

code() { curl -s -o /dev/null -w '%{http_code}' -m 5 "$@"; }
pass=0; fail=0
check() {
  if [[ "$2" == "$3" ]]; then printf '  ok   %-46s %s\n' "$1" "$3"; pass=$((pass+1))
  else printf '  FAIL %-46s got %s, wanted %s\n' "$1" "$2" "$3"; fail=$((fail+1)); fi
}

echo
echo "▸ the auth gate"
check "health is open (it is the liveness probe)" \
      "$(code "http://127.0.0.1:$PORT/api/health")" 200
check "everything else needs the token" \
      "$(code "http://127.0.0.1:$PORT/api/status")" 401
check "a wrong token is refused" \
      "$(code -H 'Authorization: Bearer wrong' "http://127.0.0.1:$PORT/api/status")" 401
check "the right token is accepted" \
      "$(code -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT/api/status")" 200
check "tcl/eval is behind it too" \
      "$(code -X POST -H 'Content-Type: application/json' -d '{"code":"expr 1"}' \
         "http://127.0.0.1:$PORT/api/tcl/eval")" 401

echo
echo "▸ the config cannot be pulled out from under it"
: > "$HOME_DIR/.env"
check "still refused with the token file emptied" \
      "$(code "http://127.0.0.1:$PORT/api/status")" 401
check "and the token it started with still works" \
      "$(code -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT/api/status")" 200
printf 'DESKTOP_BEARER_TOKEN=%s\n' "$TOKEN" > "$HOME_DIR/.env"

echo
echo "▸ the bind stayed loopback"
check "no VELES_DESKTOP_HOST means 127.0.0.1 only" \
      "$(ss -ltn 2>/dev/null | grep -c "127.0.0.1:$PORT")" 1

echo
if [[ "$fail" -gt 0 ]]; then
  echo "remote-demo: $pass passed, $fail FAILED"
  exit 1
fi
echo "remote-demo: $pass passed, 0 failed"

cat <<EOF

▸ Point a desktop at it:

    VELES_API=http://127.0.0.1:$PORT VELES_TOKEN='$TOKEN' \\
      scripts/veles-desktop.sh

  The launcher notices the URL is not its own gateway and skips both the
  local autostart and the staleness check.

  Things worth trying, because each one is a different code path:

    - drop the token   → an error dialog naming the gateway, not a window
                         full of empty panes
    - wrong token      → same, within a second
    - the IRC tab      → it will say there is no IRC on THAT gateway,
                         which is the empty state this pass added
    - the Dev pane     → the file tree is the REMOTE machine's, and the
                         language server now runs there too (HANDOFF §17)

▸ Save it as a named instance instead, so it is one variable next time:

    mkdir -p ~/.config/veles-desktop
    cat > ~/.config/veles-desktop/instances.json <<'JSON'
    {"demo": {"url": "http://127.0.0.1:$PORT", "token": "$TOKEN"}}
    JSON
    chmod 600 ~/.config/veles-desktop/instances.json
    VELES_INSTANCE=demo scripts/veles-desktop.sh

  That file lives on THIS machine, at 0600 — never in a gateway's state
  blob, so connecting to one gateway does not hand it the tokens for the
  others.

EOF

if [[ "$DESKTOP" == "1" ]]; then
  echo "▸ launching the desktop"
  VELES_API="http://127.0.0.1:$PORT" VELES_TOKEN="$TOKEN" \
    "$REPO/scripts/veles-desktop.sh"
elif [[ "$KEEP" != "1" ]]; then
  echo "(pass --keep to leave the gateway up, or --desktop to launch one now)"
fi

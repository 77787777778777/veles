#!/bin/sh
# b3a-two-gateways.sh — the two-gateway real-binary check for the B3a
# fence, as the 2026-09-28 review left it: gossip both ways, a
# non-arbiter HOLDING a lease through renewals, failover when its
# holder dies, and a refused singleton that skips itself instead of
# taking the gateway down. Scratch homes under /dev/shm; nothing
# touches the live config or the live :9900.
#
#   sh scripts/b3a-two-gateways.sh        (~5 minutes: real lease TTLs)
#
# Exit 0 only if every property held.
#
# PORTS. Neither gateway binds 9900 — the LIVE gateway owns it. The
# per-process A2A_PORT beats both config and dotenv.
#
# IDENTITIES. `arbiter = <id>` must match a registry row, and rows come
# from node.toml, so each home gets a PRE-SEEDED node.toml.
#
# LEASES, PRE-SEEDED in the arbiter's store before it boots (the only
# way to hand the cron lease to the NON-arbiter in a two-node pool —
# the arbiter otherwise grabs it at its own boot):
#   singleton:cron      held by b  (far expiry; b's first renewal
#                                   replaces it with a real 90 s one)
#   singleton:telegram  held by a  (a runs no telegram; b is configured
#                                   for it and must be refused)
#
# Properties:
#   1. GOSSIP CONVERGENCE — b's registry learns a's row over the wire.
#   2. PEER-OBSERVED ECHO — b's copy of a carries `seen=<addr>`.
#   3. FENCE ARMED on a (the arbiter).
#   4. CRON STANDBY on a — the lease is b's, so a's scheduler waits.
#   5. B HOLDS CRON THROUGH RENEWALS WITH GOSSIP RUNNING BOTH WAYS —
#      after b's first grant, ≥ 4 more asks are served and none is
#      refused. (B3a bound the peer to the arbiter's OWN row whenever
#      the echo of the arbiter's gossip was newer: "bound to another
#      node", and a lost lease never came back.) b's FIRST boot cannot
#      arm the fence (it has not heard of the arbiter yet): cron keeps
#      trying and arms once gossip lands. (B3a refused once, for good.)
#   6. A FRESH NODE STARTS NO TELEGRAM — on b's first boot the fence is
#      refused, so the boot-gated telegram poller does not start.
#      (B3a read a refused fence as "single node, ungated": it logged
#      "no telegram will start" and started telegram.)
#   6b. A REFUSED TELEGRAM LEASE STANDS BY — restart b (the fence arms
#      now); a holds the telegram lease, so b's supervisor never starts
#      the poller, and b keeps running. (B3a `bail!`ed the whole
#      gateway; C1/C2 then skipped the duty for good, so nobody could
#      take it over.)
#   7. FAILOVER — kill b; a's standby takes cron once b's lease runs
#      out, and never before.
set -e
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/debug/veles"
ROOT=$(mktemp -d /dev/shm/b3a-XXXXXX)
A="$ROOT/a"; B="$ROOT/b"
mkdir -p "$A" "$B"
A_ID="aaaaaaaa-0000-4000-8000-000000000001"
B_ID="bbbbbbbb-0000-4000-8000-000000000002"

cleanup() {
    for p in ${GA:-} ${GB:-}; do kill "$p" 2>/dev/null || true; done
    sleep 0.3
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

fail() {
    echo "FAIL: $1"
    echo "-- a log (cluster/lease/cron):"; grep -iE "cluster|lease|cron|fence|telegram" "$ROOT/a.log" | tail -12 | sed 's/^/  a> /'
    echo "-- b log (cluster/lease/cron):"; grep -iE "cluster|lease|cron|fence|telegram" "$ROOT/b.log" | tail -12 | sed 's/^/  b> /'
    exit 1
}

cat > "$A/node.toml" <<EOF
id = "$A_ID"
name = "b3a-node-a"
EOF
cat > "$B/node.toml" <<EOF
id = "$B_ID"
name = "b3a-node-b"
EOF

# Both sides gossip to each other (the natural configuration, and the
# one that exposed the binding flap).
cat > "$A/config.toml" <<EOF
[platforms.a2a]
enabled = true
[node]
endpoint = "http://127.0.0.1:19899"
[a2a_agents.b]
url = "http://127.0.0.1:19901"
# OUTBOUND identity: a presents tok-a; b's A2A_PEER_TOKENS=a:tok-a maps
# it to peer name "a".
auth = { type = "bearer", token = "tok-a" }
[cluster]
memsync_peers = ["b"]
gossip_peers = ["b"]
arbiter = "aaaaaaaa"
gossip_interval_secs = 3
stale_after_secs = 60
EOF
cat > "$B/config.toml" <<EOF
[platforms.a2a]
enabled = true
[platforms.telegram]
enabled = true
[node]
endpoint = "http://127.0.0.1:19901"
[a2a_agents.a]
url = "http://127.0.0.1:19899"
auth = { type = "bearer", token = "tok-b" }
[cluster]
memsync_peers = ["a"]
gossip_peers = ["a"]
arbiter = "aaaaaaaa"
gossip_interval_secs = 3
stale_after_secs = 60
EOF

echo 'A2A_PEER_TOKENS=b:tok-b' > "$A/.env"
echo 'A2A_BEARER_TOKEN=tok-a' >> "$A/.env"
echo 'A2A_PEER_TOKENS=a:tok-a' > "$B/.env"
echo 'A2A_BEARER_TOKEN=tok-b' >> "$B/.env"
# A FAKE token: b must be refused the lease before it would ever use it.
echo 'TELEGRAM_BOT_TOKEN=0000000000:fake-token-for-the-fence-check' >> "$B/.env"

# Pre-seed the leases in a's store (the schema comes from any CLI open).
VELES_HOME="$A" HOME="$A" "$BIN" nodes list >/dev/null 2>&1 || true
NOW=$(date -u +%Y-%m-%dT%H:%M:%S+00:00)
sqlite3 "$A/state.db" "INSERT INTO singleton_leases (key, holder, granted_at, expires_at) VALUES
  ('singleton:cron', '$B_ID', '$NOW', '2099-01-01T00:00:00+00:00'),
  ('singleton:telegram', '$A_ID', '$NOW', '2099-01-01T00:00:00+00:00');"

echo "== homes under $ROOT"
run_gw() { # home port log
    env -u TELEGRAM_BOT_TOKEN -u TELEGRAM_ALLOWED_USERS A2A_PORT="$2" \
        VELES_HOME="$1" HOME="$1" "$BIN" gateway run >"$3" 2>&1 &
}
run_gw "$A" 19899 "$ROOT/a.log"; GA=$!
run_gw "$B" 19901 "$ROOT/b.log"; GB=$!
echo "== gateways started (a=$GA b=$GB)"

# 1 + 2: convergence and the echo, over the wire.
ok=""
for i in $(seq 1 40); do
    sleep 0.5
    kill -0 "$GA" 2>/dev/null || fail "gateway a died"
    kill -0 "$GB" 2>/dev/null || fail "gateway b died"
    BNODES="$(VELES_HOME="$B" HOME="$B" "$BIN" nodes list 2>/dev/null | grep -v "^20" || true)"
    if echo "$BNODES" | grep -q "seen="; then ok=1; break; fi
done
[ -n "$ok" ] || fail "no peer-observed address on b after 20s"
echo "PASS 1+2: b learned a over gossip and observed its address"

grep -q "singleton fence ARMED" "$ROOT/a.log" || fail "fence not ARMED on a"
echo "PASS 3: a (arbiter) armed the fence"

grep -q "cron scheduler on STANDBY" "$ROOT/a.log" || fail "a's cron is not on standby"
echo "PASS 4: a's cron waits on standby (the lease is b's)"

grep -q "telegram adapter not started: the singleton fence is refused" "$ROOT/b.log" \
    || fail "b's first boot did not refuse telegram on the unarmed fence"
grep -q "starting telegram adapter" "$ROOT/b.log" && fail "b STARTED telegram on a refused fence"
kill -0 "$GB" 2>/dev/null || fail "b died after the telegram refusal"
echo "PASS 6: a fresh b (fence not armed yet) started no telegram and kept running"

# 5: b gets cron once its gossip binding exists, then holds it.
echo "== waiting for b's grant and 4 renewals (~2.5 min)"
ok=""
for i in $(seq 1 90); do
    sleep 1
    if grep -q "lease singleton:cron held by $B_ID" "$ROOT/b.log"; then ok=1; break; fi
done
[ -n "$ok" ] || fail "b was never granted the cron lease"
served_after_grant() {
    # a's audit: lease asks from peer b for singleton:cron, from the
    # first COMPLETED one on. Prints "<completed> <rejected>".
    grep '"event":"lease_ask"' "$A/a2a_audit.jsonl" 2>/dev/null \
        | grep '"peer":"b"' | grep '"key":"singleton:cron"' \
        | awk '/TASK_STATE_COMPLETED/{on=1} on{ if (/TASK_STATE_COMPLETED/) c++; else r++ } END{print c+0, r+0}'
}
for i in $(seq 1 170); do
    sleep 1
    set -- $(served_after_grant)
    [ "$1" -ge 5 ] && break
done
set -- $(served_after_grant)
[ "$1" -ge 5 ] || fail "only $1 of b's cron asks served after its grant"
[ "$2" -eq 0 ] || fail "$2 of b's cron asks REFUSED after its grant (binding flap?)"
if awk "/lease singleton:cron held by $B_ID/{h=1} h && /lease singleton:cron not granted/{bad=1} END{exit !bad}" "$ROOT/b.log"; then
    fail "b lost the cron lease after holding it"
fi
grep -q "lease singleton:cron held by $A_ID" "$ROOT/a.log" && fail "a took cron while b held it — two holders"
echo "PASS 5: b held cron through $1 served asks, 0 refused, gossip both ways"

# 6b: restart b — now it knows the arbiter, the fence arms, and the
# telegram lease (a's) is refused by name. b stays up.
kill "$GB"; GB=""
sleep 1
run_gw "$B" 19901 "$ROOT/b2.log"; GB=$!
ok=""
for i in $(seq 1 40); do
    sleep 0.5
    kill -0 "$GB" 2>/dev/null || { cp "$ROOT/b2.log" "$ROOT/b.log"; fail "restarted b died"; }
    # 2026-09-29 review: a refused node stands BY (its supervisor starts
    # the poller if the lease is ever granted to it) instead of giving
    # up for good — so the line is "on standby", and the thing that must
    # never appear is the supervisor STARTING the duty.
    if grep -q "telegram adapter on standby — lease singleton:telegram not granted" "$ROOT/b2.log"; then ok=1; break; fi
done
[ -n "$ok" ] || { cp "$ROOT/b2.log" "$ROOT/b.log"; fail "restarted b did not refuse the telegram lease by name"; }
grep -q "singleton fence ARMED" "$ROOT/b2.log" || { cp "$ROOT/b2.log" "$ROOT/b.log"; fail "restarted b's fence did not arm"; }
sleep 2
grep -q "telegram adapter: lease singleton:telegram held — starting" "$ROOT/b2.log" && { cp "$ROOT/b2.log" "$ROOT/b.log"; fail "restarted b STARTED telegram on a refused lease"; }
grep -q "starting telegram adapter" "$ROOT/b2.log" && { cp "$ROOT/b2.log" "$ROOT/b.log"; fail "restarted b claims to start telegram on a refused lease"; }
kill -0 "$GB" 2>/dev/null || { cp "$ROOT/b2.log" "$ROOT/b.log"; fail "restarted b died after the refusal"; }
echo "PASS 6b: restarted b armed its fence, stands by for the telegram lease, and kept running"

# 7: failover.
kill "$GB"; GB=""
echo "== b killed; waiting for its lease to run out and a's standby to take cron (≤ 2.5 min)"
ok=""
for i in $(seq 1 160); do
    sleep 1
    if grep -q "lease singleton:cron held by $A_ID" "$ROOT/a.log"; then ok=1; break; fi
done
[ -n "$ok" ] || fail "a never took over cron after b died"
echo "PASS 7: a's standby took cron after b's lease ran out (${i}s after the kill)"

echo ""
echo "PASS: two-gateway B3a verification complete"
exit 0

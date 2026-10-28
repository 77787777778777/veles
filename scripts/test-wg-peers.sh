#!/usr/bin/env bash
# scripts/test-wg-peers.sh — deploy/vps/wg/veles-wg-peers.sh against a
# fake `wg` and a scratch config: what it adds, what it refuses (a bad
# key, an address outside the subnet or already held, a planted
# symlink), and what a removal leaves of the config.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
t=$(mktemp -d "${TMPDIR:-/tmp}/wg-peers.XXXXXX")
trap 'rm -rf "$t"' EXIT
mkdir -p "$t/bin" "$t/queue"
cat > "$t/bin/wg" <<'W'
#!/bin/sh
echo "wg $*" >> "$WG_LOG"
if [ "$1" = show ]; then printf 'HUBPEERKEY\t10.0.30.2/32\n'; fi
W
chmod +x "$t/bin/wg"
export PATH="$t/bin:$PATH" WG_LOG="$t/wg.log" VELES_WG_CONF="$t/wg0.conf"
cat > "$t/wg0.conf" <<'C'
[Interface]
Address = 10.0.30.1/24
PrivateKey = hubsecret
ListenPort = 51820

# the laptop
[Peer]
PublicKey = LAPTOPKEYLAPTOPKEYLAPTOPKEYLAPTOPKEYLAPTOPK=
AllowedIPs = 10.0.30.2/32
C
key=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
printf '# veles pool: phone2\n[Peer]\nPublicKey = %s\nAllowedIPs = 10.0.30.4/32\n' "$key" > "$t/queue/phone2.conf"
printf '[Peer]\nPublicKey = notakey\nAllowedIPs = 10.0.30.5/32\n' > "$t/queue/badkey.conf"
printf '[Peer]\nPublicKey = %s\nAllowedIPs = 10.0.31.5/32\n' "$key" > "$t/queue/outside.conf"
printf '[Peer]\nPublicKey = %s\nAllowedIPs = 10.0.30.2/32\n' "$key" > "$t/queue/thief.conf"
ln -s /etc/hostname "$t/queue/planted.conf"
fail=0
check() { if eval "$2"; then echo "  ✓ $1"; else echo "  ✗ $1"; fail=1; fi; }

bash "$here/deploy/vps/wg/veles-wg-peers.sh" wg0 10.0.30.0/24 "$t/queue" > "$t/out" 2>&1
check "the good peer is added to the interface" "grep -qx 'wg set wg0 peer $key allowed-ips 10.0.30.4/32' $t/wg.log"
check "and written to the config" "grep -q 'AllowedIPs = 10.0.30.4/32' $t/wg0.conf"
check "a bad key is refused" "grep -q 'skip badkey: not a WireGuard public key' $t/out"
check "an address outside the subnet is refused" "grep -q 'skip outside' $t/out"
check "the laptop's address cannot be taken" "grep -q 'skip thief: 10.0.30.2 is already held' $t/out"
check "a planted symlink is not read" "! grep -q planted $t/out && [ -L $t/queue/planted.conf ]"
check "only one peer was set" "[ \$(grep -c 'wg set' $t/wg.log) = 1 ]"
check "the done file moved aside" "[ -f $t/queue/done/phone2.conf ] && [ ! -f $t/queue/phone2.conf ]"

printf '%s\n' "$key" > "$t/queue/phone2.remove"
bash "$here/deploy/vps/wg/veles-wg-peers.sh" wg0 10.0.30.0/24 "$t/queue" > "$t/out2" 2>&1
check "a removal takes the peer off the interface" "grep -qx 'wg set wg0 peer $key remove' $t/wg.log"
check "and out of the config" "! grep -q '10.0.30.4' $t/wg0.conf && ! grep -q 'veles pool: phone2' $t/wg0.conf"
check "the laptop's peer and the interface stay" "grep -q 'LAPTOPKEY' $t/wg0.conf && grep -q 'PrivateKey = hubsecret' $t/wg0.conf && grep -q '# the laptop' $t/wg0.conf"
check "the config stays owner-only" "[ \$(stat -c %a $t/wg0.conf) = 600 ]"
# A removal naming one of the hub's OWN peers (not added through this
# helper) is skipped: the laptop keeps its tunnel and its address.
sets_before=$(grep -c 'wg set' "$t/wg.log")
printf '%s\n' LAPTOPKEYLAPTOPKEYLAPTOPKEYLAPTOPKEYLAPTOPK= > "$t/queue/laptop.remove"
bash "$here/deploy/vps/wg/veles-wg-peers.sh" wg0 10.0.30.0/24 "$t/queue" > "$t/out3" 2>&1
check "a removal of a peer the helper never added is skipped" "grep -q 'skip laptop: .* is not a peer this helper added' $t/out3"
check "and the laptop's peer stays on the interface and in the config" "[ \$(grep -c 'wg set' $t/wg.log) = $sets_before ] && grep -q 'LAPTOPKEY' $t/wg0.conf"
check "the skipped removal is filed, not retried" "[ -f $t/queue/done/laptop.remove ]"
# `done/` pointed elsewhere by the queue's owner: nothing is applied and
# nothing is written where the link points.
mkdir -p "$t/q2" "$t/elsewhere"
ln -s "$t/elsewhere" "$t/q2/done"
printf '# veles pool: phone3\n[Peer]\nPublicKey = %s\nAllowedIPs = 10.0.30.6/32\n' "$key" > "$t/q2/phone3.conf"
rc=0; bash "$here/deploy/vps/wg/veles-wg-peers.sh" wg0 10.0.30.0/24 "$t/q2" > "$t/out4" 2>&1 || rc=$?
check "a symlinked done/ stops the run" "[ $rc = 2 ] && grep -q 'not a plain directory' $t/out4"
check "and nothing was applied or moved" "[ \$(grep -c 'wg set' $t/wg.log) = $sets_before ] && [ -f $t/q2/phone3.conf ] && [ ! -e $t/elsewhere/phone3.conf ]"
if [ $fail != 0 ]; then echo "--- out"; cat "$t/out" "$t/out2" "$t/out3" "$t/out4"; echo "--- conf"; cat "$t/wg0.conf"; fi
echo "test-wg-peers: $([ $fail = 0 ] && echo ok || echo FAILED)"
exit $fail

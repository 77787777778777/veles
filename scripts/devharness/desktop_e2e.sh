#!/usr/bin/env bash
# Full stack: mock provider -> gateway -> real Tk desktop under Xvfb.
cd /home/user/veles-agent
S=/home/user/veles-agent/scripts/devharness
rm -f /dev/shm/mock.log /dev/shm/gw.log
bash $S/mock_home.sh >/dev/null
python3 $S/mock_provider.py 18999 /dev/shm/mock.log >/dev/null 2>&1 &
MP=$!
for i in $(seq 1 25); do curl -s -m 1 http://127.0.0.1:18999/v1/models >/dev/null 2>&1 && break; sleep 0.2; done

export VELES_HOME=/dev/shm/veles-mockhome TMPDIR=/dev/shm VELES_DESKTOP_PORT=18642

# The desktop API authenticates every route but /api/health, and mints a
# token into $VELES_HOME/.env when it finds none. Seed one instead of
# scraping the minted file: deterministic, no race with the gateway
# writing it, and the same path an operator's own token takes. Written
# AFTER mock_home.sh, which rm -rf's the home.
export VELES_TOKEN=devharness-desktop-token
printf 'DESKTOP_BEARER_TOKEN=%s\n' "$VELES_TOKEN" > "$VELES_HOME/.env"
chmod 600 "$VELES_HOME/.env"

./result/bin/veles gateway run >/dev/shm/gw.log 2>&1 &
GW=$!
for i in $(seq 1 60); do curl -s -m 1 http://127.0.0.1:18642/api/health >/dev/null 2>&1 && break; sleep 0.25; done
echo "gateway: $(curl -s -m 2 http://127.0.0.1:18642/api/health)"

S2=/home/user/veles-agent/scripts/devharness
direnv exec /home/user/veles-agent bash "$S2/desktop_e2e_inner.sh"
kill $GW $MP 2>/dev/null
exit 0

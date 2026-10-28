#!/usr/bin/env bash
# push-image.sh — ship the images build-image.sh made to the VPS and load
# each into ITS user's rootless podman (deploy/vps): the gateway image for
# `veles`, the signer image for `veles-signer`. Each is tagged :current,
# which the quadlets run; restarting the service is a separate step
# (deploy/README.md, "Updating").
#
# The gateway image is NOT tagged :current while a module on the VPS was
# built for another core: the next start, planned or not, would refuse
# every one of them ("core changed"; 2026-10-10, IRC, slopdrop, telegram
# and the script hosts were down for 2–4 minutes). Install the modules
# built with this image first (scripts/push-plugin.sh --no-reload <name>,
# each), then push. ALLOW_STALE_MODULES=1 tags it anyway.
#
#   scripts/push-image.sh            the newest of both in result-images/
#   scripts/push-image.sh gateway    one of them
#   SSH_TARGET=host scripts/push-image.sh      (default user@vps.example.org)
set -euo pipefail
cd "$(dirname "$0")/.."

SSH_TARGET="${SSH_TARGET:-user@vps.example.org}"
which="${1:-all}"
case "$which" in
    all) kinds="agent signer" ;;
    gateway) kinds="agent" ;;
    signer) kinds="signer" ;;
    *) echo "usage: $0 [all|gateway|signer]" >&2; exit 2 ;;
esac

for kind in $kinds; do
    archive=$(ls -t result-images/veles-"$kind"-*.tar.gz 2>/dev/null | head -1 || true)
    [ -n "$archive" ] || { echo "no veles-$kind image in result-images/ (scripts/build-image.sh)" >&2; exit 1; }
    [ -f "$archive.sha256" ] || { echo "no $archive.sha256" >&2; exit 1; }
    base=$(basename "$archive")
    tag=${base#veles-"$kind"-}
    tag=${tag%.tar.gz}
    case "$kind" in agent) user=veles ;; signer) user=veles-signer ;; esac

    tag_current=1
    if [ "$kind" = agent ]; then
        keyfile="${archive%.tar.gz}.key"
        [ -f "$keyfile" ] || { echo "no $keyfile (an image from before the key; scripts/build-image.sh gateway)" >&2; exit 1; }
        key=$(tr -dc '0-9a-f' <"$keyfile" | head -c 64)
        [ ${#key} = 64 ] || { echo "$keyfile: not a module key" >&2; exit 1; }
        echo "▸ the VPS modules against this image's key $key"
        # A module carries its core's key as text in a section of its
        # own: grep finds this key in a module built for it, and in no
        # other.
        stale=$(ssh "$SSH_TARGET" "sudo sh -c '
            for d in /srv/pool/veles/data/modules/*/; do
                [ -e \"\$d/current\" ] || continue
                n=\$(basename \"\$d\")
                if LC_ALL=C grep -q -a -F $key \"\$d/current\"; then echo \"  ok    \$n\" >&2
                else echo \"\$n\"; fi
            done'")
        if [ -n "$stale" ]; then
            echo "  built for ANOTHER core: $(echo $stale)"
            if [ "${ALLOW_STALE_MODULES:-}" = 1 ]; then
                echo "  ALLOW_STALE_MODULES=1: tagging :current anyway — the next start refuses them"
            else
                tag_current=0
            fi
        fi
    fi

    echo "▸ $base → $SSH_TARGET (user $user)"
    ssh "$SSH_TARGET" 'mkdir -p ~/veles-images'
    scp -q "$archive" "$archive.sha256" "$SSH_TARGET:veles-images/"
    # The user's podman reads the archive from a directory only it can
    # read; `cd /` because podman refuses a working directory its user
    # cannot read (wrath's home).
    ssh "$SSH_TARGET" "set -e
        cd ~/veles-images
        sha256sum -c '$base.sha256'
        sudo install -d -o $user -g $user -m 0700 /srv/pool/$user/images
        sudo install -o $user -g $user -m 0600 '$base' /srv/pool/$user/images/
        rm -f '$base' '$base.sha256'
        uid=\$(id -u $user)
        sudo sh -c \"cd / && runuser -u $user -- env XDG_RUNTIME_DIR=/run/user/\$uid \\
            podman load -q -i /srv/pool/$user/images/$base\"
        if [ $tag_current = 1 ]; then
            sudo sh -c \"cd / && runuser -u $user -- env XDG_RUNTIME_DIR=/run/user/\$uid \\
                podman tag localhost/veles-$kind:$tag localhost/veles-$kind:current\"
        fi
        sudo rm -f /srv/pool/$user/images/$base
        # A container of a new image under the quadlets' keep-id mapping
        # first copies every layer, chowned (126 s for the gateway,
        # measured 2026-10-02) — longer than the unit's 90 s start
        # timeout, so a plain restart would time out mid-copy. Make that
        # copy here, once, with nothing waiting on it.
        echo '  warming the keep-id layer copy (minutes)…'
        sudo sh -c \"cd / && runuser -u $user -- env XDG_RUNTIME_DIR=/run/user/\$uid \\
            podman create -q --replace --name veles-warm-$kind \\
                --userns=keep-id:uid=1000,gid=1000 localhost/veles-$kind:$tag >/dev/null \\
            && runuser -u $user -- env XDG_RUNTIME_DIR=/run/user/\$uid \\
            podman rm -f veles-warm-$kind >/dev/null\"
        sudo sh -c \"cd / && runuser -u $user -- env XDG_RUNTIME_DIR=/run/user/\$uid \\
            podman images --format '{{.Repository}}:{{.Tag}} {{.Size}}'\" | grep veles-$kind
    "
    if [ $tag_current = 0 ]; then
        echo "▸ loaded as localhost/veles-$kind:$tag, NOT tagged :current. Install the modules first:" >&2
        for n in $stale; do echo "    scripts/push-plugin.sh --no-reload $n" >&2; done
        echo "  then run this again (the load is repeated; it is cheap next to an outage)" >&2
        exit 1
    fi
done
echo "▸ loaded. Restart with: ssh $SSH_TARGET sudo systemctl --user -M veles@ restart veles-gateway"
echo "  (or veles-signer@ / veles-signer); then scripts/vps-test.sh"

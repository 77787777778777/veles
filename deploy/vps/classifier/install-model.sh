#!/bin/sh
# deploy/vps/classifier/install-model.sh — stage the int8 model onto the VPS.
#
# Usage: install-model.sh <local-model-dir>
#   <local-model-dir> holds model.int8.onnx, tokenizer.json, labels.json,
#   PROVENANCE.txt (the /dev/shm/clf-export output of the export run).
#
# Copies them to /srv/pool/veles/classifier/model on vps.example.org, owned by
# veles, dir 0750 files 0640, and verifies the sha256 on the far side against
# the local files so a truncated copy is never mounted. Idempotent.
set -eu

SRC=${1:?usage: install-model.sh <local-model-dir>}
HOST=user@vps.example.org
DEST=/srv/pool/veles/classifier/model
for f in model.int8.onnx tokenizer.json labels.json PROVENANCE.txt; do
    [ -f "$SRC/$f" ] || { echo "missing $SRC/$f" >&2; exit 1; }
done

ssh "$HOST" 'sudo -n -u veles mkdir -p '"$DEST"' && sudo -n -u veles chmod 750 '"$DEST"
# scp as veles keeps ownership; a staging dir + move is not needed for 0750/0640
# because scp -p preserves the local mode. Set the local modes first.
chmod 640 "$SRC"/model.int8.onnx "$SRC"/tokenizer.json "$SRC"/labels.json "$SRC"/PROVENANCE.txt
scp -p "$SRC"/model.int8.onnx "$SRC"/tokenizer.json "$SRC"/labels.json "$SRC"/PROVENANCE.txt \
    "$HOST:$DEST/" >/dev/null 2>&1 || {
    # /srv/pool/veles is 0700 veles: fall back to a sudo-tee copy.
    for f in model.int8.onnx tokenizer.json labels.json PROVENANCE.txt; do
        cat "$SRC/$f" | ssh "$HOST" 'sudo -n -u veles sh -c "cat > '"$DEST"'/'"$f"'"' \
            || { echo "copy failed for $f" >&2; exit 1; }
    done
}

# Verify sha256 on the far side against the local digests.
local_sums=$(cd "$SRC" && sha256sum model.int8.onnx tokenizer.json labels.json)
remote_sums=$(ssh "$HOST" 'sudo -n -u veles sh -c "cd '"$DEST"' && sha256sum model.int8.onnx tokenizer.json labels.json"')
if [ "$local_sums" != "$remote_sums" ]; then
    echo "sha256 MISMATCH:" >&2
    echo "local:  $local_sums" >&2
    echo "remote: $remote_sums" >&2
    exit 1
fi
echo "installed + verified:"
echo "$remote_sums"

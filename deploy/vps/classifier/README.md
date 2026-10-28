# deploy/vps/classifier — the /tcl output scorer

A sidecar on the VPS CPU that scores the exact text of a public `/tcl`
reply, so the gateway's `slopdrop_check` never needs a third-party
endpoint. Detoxify `unitary/unbiased-toxic-roberta` (Apache-2.0), exported
to ONNX (torchscript, opset 17) and quantized to int8. ~12 ms/inference on
one EPYC-Milan vCPU; no GPU.

It is a DUMB scorer, and the gateway holds the thresholds and the
fail-closed rule.
- `POST /score {"text"}` returns every sigmoid head:
  `{"model","scores":{…},"tokens","truncated","ms"}`.
  - It reads up to 512 tokens (RoBERTa's ceiling).
  - `truncated: true` says the tail went unscored. The gateway withholds
    that text, and any reply that is not a 2xx with all 16 heads.
- `GET /health`.
- `--self-test` (identd-style): two probes plus the truncation flag.

## Files

- `veles-classifier.py` — the service (stdlib http.server + onnxruntime +
  tokenizers + numpy).
- `Containerfile` — python:3.12-slim, pinned wheels, no USER (the quadlet
  runs it keep-id:uid=1000 = the host `veles` user, which owns the model).
- `veles-classifier.container` — the rootless quadlet. It goes in
  `~veles/.config/containers/systemd/`.
  - On `veles-net`, with NO published port: the gateway reaches it by name.
  - The model is mounted read-only, and the root is read-only too.
  - Limited to `--cpus=1 --memory=1g`.
- `export-model.py` — the export + int8 quantization + provenance run.
- `install-model.sh <local-model-dir>` — stage the export to
  `/srv/pool/veles/classifier/model` as veles (0750 dir / 0640 files) and
  verify the sha256 on the far side.

## Deploy / update the model

The model is NOT in the image — mount-swapped, so a re-export needs no
rebuild, only a restart:

```sh
# export + quantize (throwaway container, writes /dev/shm/clf-export):
#   see the export script (torch.onnx + onnxruntime.quantize_dynamic)
sh deploy/vps/classifier/install-model.sh /dev/shm/clf-export
# build the image as veles from the sources in /srv/pool/veles/classifier/src
# (keep the old one as :prev for a rollback), self-test it, then restart:
#   podman tag localhost/veles-classifier:current localhost/veles-classifier:prev
#   podman build -t localhost/veles-classifier:current -f Containerfile .
#   podman run --rm --userns keep-id:uid=1000,gid=1000 --read-only \
#     -v /srv/pool/veles/classifier/model:/model:ro \
#     localhost/veles-classifier:current --self-test
ssh user@vps.example.org sudo systemctl --user -M veles@ restart veles-classifier
# probe from INSIDE the gateway (the real call path):
ssh user@vps.example.org 'sudo -u veles env XDG_RUNTIME_DIR=/run/user/$(id -u veles) sh -c \
  "cd /; podman exec veles-gateway curl -sf http://veles-classifier:8800/health"'
```

Provenance of the 10-06 model (`fd991de9…` model.int8.onnx): source
https://huggingface.co/unitary/unbiased-toxic-roberta @ 36295dd8 (Apache-2.0);
max |onnx − torch| on the sigmoid scores 0.0247 across 6 probes; benign
toxicity 0.0004, abusive 0.9973. Recorded in the export's PROVENANCE.txt.

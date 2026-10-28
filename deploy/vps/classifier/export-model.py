"""Export Detoxify's unbiased RoBERTa to ONNX, quantize it to int8, and
check both against the original model. Runs in a throwaway container;
writes into /out: model.int8.onnx, tokenizer.json, labels.json,
PROVENANCE.txt.

    mkdir -p /dev/shm/clf-export
    podman run --rm -v /dev/shm/clf-export:/out \
      -v "$PWD/deploy/vps/classifier/export-model.py:/export.py:ro" \
      docker.io/library/python:3.12-slim sh -c '
        pip install -q --no-cache-dir torch --index-url https://download.pytorch.org/whl/cpu &&
        pip install -q --no-cache-dir transformers onnx onnxruntime &&
        python /export.py' </dev/null

then `install-model.sh /dev/shm/clf-export`. The 2026-10-06 run gave
model.int8.onnx sha256 fd991de9…, max |onnx - torch| 0.0247."""
import hashlib
import json
import subprocess
import sys

import numpy as np
import onnxruntime as ort
import torch
from onnxruntime.quantization import QuantType, quantize_dynamic
from transformers import AutoModelForSequenceClassification, AutoTokenizer

REPO = "unitary/unbiased-toxic-roberta"
REV = "36295dd80b422dc49f40052021430dae76241adc"
OUT = "/out"

tok = AutoTokenizer.from_pretrained(REPO, revision=REV, use_fast=True)
model = AutoModelForSequenceClassification.from_pretrained(REPO, revision=REV).eval()
labels = [model.config.id2label[i] for i in range(model.config.num_labels)]

enc = tok(["hello there", "a longer second sentence to pad the batch"], return_tensors="pt", padding=True)
fp32 = f"{OUT}/model.fp32.onnx"
args = (enc["input_ids"], enc["attention_mask"])
kw = dict(
    input_names=["input_ids", "attention_mask"],
    output_names=["logits"],
    dynamic_axes={"input_ids": {0: "b", 1: "t"}, "attention_mask": {0: "b", 1: "t"}, "logits": {0: "b"}},
    opset_version=17,
)
try:
    torch.onnx.export(model, args, fp32, dynamo=False, **kw)
    how = "torchscript exporter"
except TypeError:
    torch.onnx.export(model, args, fp32, **kw)
    how = "default exporter"

int8 = f"{OUT}/model.int8.onnx"
quantize_dynamic(fp32, int8, weight_type=QuantType.QInt8)
tok.backend_tokenizer.save(f"{OUT}/tokenizer.json")
with open(f"{OUT}/labels.json", "w") as f:
    json.dump(labels, f)

probe = [
    "have a nice day",
    "you are a complete idiot and everyone hates you",
    "the weather in paris is mild in spring",
    "i will find where you live",
    "fuck this shit",
    "muslims and jews and christians walked into a bar",
]
with torch.no_grad():
    e = tok(probe, return_tensors="pt", padding=True, truncation=True, max_length=256)
    ref = torch.sigmoid(model(**e).logits).numpy()
worst = {}
for name, path in (("fp32", fp32), ("int8", int8)):
    s = ort.InferenceSession(path, providers=["CPUExecutionProvider"])
    e = tok(probe, return_tensors="np", padding=True, truncation=True, max_length=256)
    got = s.run(None, {"input_ids": e["input_ids"].astype(np.int64),
                       "attention_mask": e["attention_mask"].astype(np.int64)})[0]
    got = 1 / (1 + np.exp(-got))
    worst[name] = float(np.abs(got - ref).max())
    if name == "int8":
        for t, row, r in zip(probe, got, ref):
            top = sorted(zip(labels, row, r), key=lambda x: -x[2])[:3]
            print(f"{t[:40]:40s} " + "  ".join(f"{l}={g:.3f}/{q:.3f}" for l, g, q in top))
print("max |onnx - torch| sigmoid:", worst, "via", how)

import os
os.remove(fp32)
def sha(p):
    return hashlib.sha256(open(p, "rb").read()).hexdigest()
freeze = subprocess.run([sys.executable, "-m", "pip", "freeze"], capture_output=True, text=True).stdout
with open(f"{OUT}/PROVENANCE.txt", "w") as f:
    f.write(f"source: https://huggingface.co/{REPO} @ {REV} (Apache-2.0)\n")
    f.write(f"export: torch.onnx ({how}), opset 17, then onnxruntime quantize_dynamic QInt8\n")
    f.write(f"max |onnx - torch| on sigmoid scores, 6 probes: {worst}\n")
    for n in ("model.int8.onnx", "tokenizer.json", "labels.json"):
        f.write(f"sha256 {sha(f'{OUT}/{n}')}  {n}\n")
    f.write("\npip freeze:\n" + freeze)
print(open(f"{OUT}/PROVENANCE.txt").read().split("\npip freeze")[0])

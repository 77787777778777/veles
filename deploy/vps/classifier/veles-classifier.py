#!/usr/bin/env python3
"""veles-classifier: a dumb scorer for /tcl public-output checks.

POST /score {"text": "..."} -> {"model", "scores": {label: p}, "tokens",
                                 "truncated", "ms"}
GET  /health                -> {"ok": true, "model", "labels"}

It wraps Detoxify's unitary/unbiased-toxic-roberta, exported to ONNX and
quantized to int8 (see PROVENANCE.txt). It makes NO decision: it scores
the exact text it is given and returns every sigmoid head. The caller
(veles slopdrop_check.rs) holds the thresholds and the fail-closed rule.

Stdlib http.server + onnxruntime + tokenizers only. One inference per
request, single-threaded; the VPS gives it one vCPU.
"""
import argparse
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL_DIR = os.environ.get("VELES_CLASSIFIER_MODEL", "/model")
ONNX = os.path.join(MODEL_DIR, "model.int8.onnx")
TOK = os.path.join(MODEL_DIR, "tokenizer.json")
LABELS = os.path.join(MODEL_DIR, "labels.json")
MAX_LEN = int(os.environ.get("VELES_CLASSIFIER_MAX_LEN", "512"))
# RoBERTa's own ceiling (514 positions, 2 reserved). The public post
# carries at most 560 chars, which is usually far under it, but spaced or
# symbol-heavy text can spend a token per character. Longer input is still
# scored (truncated by the tokenizer), and the answer SAYS so:
# `truncated: true` — the caller withholds a text whose tail was never
# read, instead of mistaking an unscored tail for a clean one.

_sess = None
_tok = None
_labels = None
_model_id = None


def _load():
    global _sess, _tok, _labels, _model_id
    if _sess is not None:
        return
    import onnxruntime as ort
    from tokenizers import Tokenizer

    _tok = Tokenizer.from_file(TOK)
    _tok.enable_truncation(max_length=MAX_LEN)
    with open(LABELS, encoding="utf-8") as f:
        _labels = json.load(f)
    so = ort.SessionOptions()
    so.intra_op_num_threads = 1
    so.inter_op_num_threads = 1
    _sess = ort.InferenceSession(ONNX, sess_options=so, providers=["CPUExecutionProvider"])
    with open(os.path.join(MODEL_DIR, "PROVENANCE.txt"), encoding="utf-8") as f:
        _model_id = f.readline().strip() or "unbiased-toxic-roberta-int8"


def score(text):
    _load()
    t0 = time.perf_counter()
    enc = _tok.encode(text)
    ids = [enc.ids]
    mask = [enc.attention_mask]
    import numpy as np

    inputs = {
        "input_ids": np.asarray(ids, dtype=np.int64),
        "attention_mask": np.asarray(mask, dtype=np.int64),
    }
    logits = _sess.run(None, inputs)[0][0]
    probs = 1.0 / (1.0 + np.exp(-logits))
    ms = (time.perf_counter() - t0) * 1000.0
    return {
        "model": _model_id,
        "scores": {label: round(float(p), 6) for label, p in zip(_labels, probs)},
        "tokens": len(enc.ids),
        "truncated": bool(enc.overflowing),
        "ms": round(ms, 2),
    }


class H(BaseHTTPRequestHandler):
    server_version = "veles-classifier/1.0"
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # quiet; the gateway logs its own outcome
        pass

    def _json(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/health":
            try:
                _load()
                self._json(200, {"ok": True, "model": _model_id, "labels": _labels})
            except Exception as e:  # a broken mount must not look healthy
                self._json(503, {"ok": False, "error": str(e)})
        else:
            self._json(404, {"error": "not found"})

    def do_POST(self):
        if self.path != "/score":
            self._json(404, {"error": "not found"})
            return
        try:
            n = int(self.headers.get("Content-Length") or 0)
            if n <= 0 or n > 1 << 20:
                self._json(400, {"error": "bad length"})
                return
            req = json.loads(self.rfile.read(n))
            text = req["text"]
            if not isinstance(text, str):
                raise ValueError("text must be a string")
            self._json(200, score(text))
        except (ValueError, KeyError, json.JSONDecodeError) as e:
            self._json(400, {"error": f"bad request: {e}"})
        except Exception as e:
            self._json(500, {"error": str(e)})


def self_test():
    # identd-style: load the model, score two probes, print, exit.
    good = score("have a nice day, the build is green")
    bad = score("you are a complete idiot and everyone hates you")
    print("model:", good["model"])
    print("labels:", len(_labels))
    print("benign  toxicity=%.4f ms=%.1f" % (good["scores"]["toxicity"], good["ms"]))
    print("abusive toxicity=%.4f insult=%.4f ms=%.1f"
          % (bad["scores"]["toxicity"], bad["scores"]["insult"], bad["ms"]))
    long = score("a " * (MAX_LEN + 50))
    print("truncated: short=%s long=%s (tokens %d)"
          % (good["truncated"], long["truncated"], long["tokens"]))
    ok = (good["scores"]["toxicity"] < 0.05 and bad["scores"]["toxicity"] > 0.9
          and good["truncated"] is False and long["truncated"] is True)
    print("self-test:", "OK" if ok else "FAIL")
    sys.exit(0 if ok else 1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default=os.environ.get("VELES_CLASSIFIER_HOST", "0.0.0.0"))
    ap.add_argument("--port", type=int, default=int(os.environ.get("VELES_CLASSIFIER_PORT", "8800")))
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args()
    if a.self_test:
        self_test()
    _load()
    print(f"veles-classifier on {a.host}:{a.port}, model {_model_id}", flush=True)
    ThreadingHTTPServer((a.host, a.port), H).serve_forever()


if __name__ == "__main__":
    main()

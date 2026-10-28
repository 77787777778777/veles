#!/usr/bin/env python3
"""Minimal OpenAI-compatible streaming provider, so the whole turn loop
can be driven without a real model. Streams reasoning_content first,
then content, exactly like DeepSeek/Kimi-style reasoning models.

    mock_provider.py [PORT [LOG]]

MOCK_SCENARIO picks the turn's one tool call, so each kind of ask can be
put in front of a surface: `terminal` (the default, `echo hi`),
`clarify` (a question with three choices), `approval` (a recursive
delete, which the approval prefilter puts to the user when
`approvals.mode = "manual"`), `rg` (`rg --version` through the terminal,
which finds ripgrep the way the agent's own commands do). A turn whose
message carries `[mock:<scenario>]` gets that one instead."""
import json, os, re, sys, time
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18999
LOG = open(sys.argv[2], "a") if len(sys.argv) > 2 else sys.stderr
SCENARIOS = {
    "terminal": ("terminal", {"command": "echo hi"}),
    "clarify": ("clarify", {"question": "Which colour should the banner be?",
                            "choices": ["pink", "cyan", "gold"]}),
    "approval": ("terminal", {"command": "rm -rf /tmp/veles-mock-build"}),
    "rg": ("terminal", {"command": "rg --version"}),
}
SCENARIO = os.environ.get("MOCK_SCENARIO", "terminal")
if SCENARIO not in SCENARIOS:
    sys.exit(f"MOCK_SCENARIO must be one of {', '.join(SCENARIOS)}, not {SCENARIO!r}")

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass

    def do_GET(self):
        if self.path.endswith("/models"):
            body = json.dumps({"data": [{"id": "mock-model"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(404); self.end_headers()

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(n)
        try:
            req = json.loads(raw)
        except Exception:
            req = {}
        msgs = req.get("messages", [])
        print(f"--- REQUEST: {len(msgs)} messages", file=LOG)
        for m in msgs:
            c = (m.get("content") or "")[:70].replace("\n", "\\n")
            rc = (m.get("reasoning_content") or "")
            print(f"    {m.get('role'):9} content={c!r} reasoning={len(rc)}ch", file=LOG)
        LOG.flush()

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        def chunk(delta, finish=None):
            d = {"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
            self.wfile.write(b"data: " + json.dumps(d).encode() + b"\n\n")
            self.wfile.flush()

        # TOOL_ROUND: on the FIRST request of a turn, answer with a tool
        # call; on the follow-up (which carries a role=tool message) give
        # the final answer. That is the shape a real agent loop takes.
        # Only THIS turn's results count: a session's history keeps every
        # earlier turn's, and those made every later turn skip its call.
        last_user = max((i for i, m in enumerate(msgs) if m.get("role") == "user"), default=-1)
        has_tool_result = any(m.get("role") == "tool" for m in msgs[last_user + 1:])
        if not has_tool_result:
            said = msgs[last_user].get("content") if last_user >= 0 else ""
            asked = re.search(r"\[mock:(\w+)\]", said if isinstance(said, str) else json.dumps(said))
            name, args = SCENARIOS.get(asked.group(1) if asked else SCENARIO, SCENARIOS[SCENARIO])
            chunk({"role": "assistant"})
            chunk({"tool_calls": [{"index": 0, "id": "call_1",
                                   "function": {"name": name, "arguments": ""}}]})
            chunk({"tool_calls": [{"index": 0,
                                   "function": {"arguments": json.dumps(args)}}]})
            chunk({}, "tool_calls")
            self.wfile.write(b'data: [DONE]\n\n')
            self.wfile.flush()
            return

        chunk({"role": "assistant"})
        # Reasoning first — long enough to trip the 15-line
        # reasoning-summary plugin threshold.
        for i in range(18):
            chunk({"reasoning_content": f"reasoning line {i}\n"})
            time.sleep(0.005)
        # Then the actual answer.
        answer = "\n".join([
            "L1 inline [the docs](https://example.com/docs) here.",
            "[at line start](https://example.com/start) then text.",
            "two [first](https://a.example) and [second](https://b.example) links.",
            "- bullet with [a link](https://example.com/bullet)",
            "1. numbered with [a link](https://example.com/num)",
            "> quoted [a link](https://example.com/quote)",
            "bare https://example.com/bare url",
            "**bold [inside bold](https://example.com/bold) text**",
            "under_score [snake_link](https://example.com/a_b_c) end",
            "`code` then [after code](https://example.com/aftercode)",
            "",
            "```tcl",
            "set x [list a b]",
            "```",
            "after the fence [post fence](https://example.com/postfence).",
            "# Heading with [head link](https://example.com/head)",
        ]) + "\n"
        for w in answer.split(" "):
            chunk({"content": w + " "})
            time.sleep(0.002)
        chunk({}, "stop")
        self.wfile.write(b'data: {"usage":{"prompt_tokens":11,"completion_tokens":22}}\n\n')
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

HTTPServer(("127.0.0.1", PORT), H).serve_forever()

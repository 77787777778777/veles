#!/usr/bin/env python3
"""slopdrop-prebake: verdicts for slopdrop's base library (PLAN-slopdrop-art.md P5).

A public /tcl reply on Bluesky is judged partly by the bad list: the
per-proc verdicts in the slopdrop module's safety.tsv on the VPS
(`<data>/module-state/slopdrop/safety.tsv`, `hash<TAB>verdict`). Until
2026-10-12 the list covered only the TRACKED procs (477 rows), and the
base library (state/stolen-treasure.tcl, about 8,400 procs) was invisible:
`GET /api/procs?base=1` lists it since slopdrop fd19d6f. This script
fills the gap, in four stages:

  names    a hand list of the worst procs → unsafe, no model asked;
  random   the procs that pick a random stamp or art → unsafe;
  stamps   every ST* stamp: evaluated, drawn as the reply would draw it,
           shown to the image check; what it withholds → unsafe;
  procs    every base proc still without a verdict, in ~10 KB batches,
           rated by a cheap text model (no tools: a plain chat completion,
           because the bodies are untrusted text).

The models are the USER's choice for this (2026-10-12; the text model moved
to the dated 1017 snapshot 2026-10-23 — the undated release is no longer the
default anywhere): deepseek-v4.1-flash-1017
(falling back to deepseek-v4-flash-0731) for text, qwen3.8-flash for the
pictures, both on the `ali-token-plan-personal` route of the laptop's
config. The key is read from the environment or the veles env files and
never printed.

Only `unsafe` puts a proc on the bad list. What is only adult (sexual,
nude or gory) gets NO row: the USER's rule (2026-10-12) is that such a
picture posts with a self-label, which the image check decides per reply,
while a `borderline` row would withhold it as link-only. It is listed in
the report. `safe` grants nothing; the row only spares a re-rating.

Rows are MERGED into the VPS file, never appended: an existing verdict
(the owner's `approved` or `blocked` above all) always wins, and the file
is backed up first. Nothing is written with --dry.

  python3 -I scripts/slopdrop-prebake.py [--dry] [--stage names,random,stamps,procs]
                                          [--limit N] [--jobs N]
  python3 -I scripts/slopdrop-prebake.py --recheck REPORT [--dry]   (a second opinion
                                          on the text model's unsafe rows; see recheck())
  python3 -I scripts/slopdrop-prebake.py --fixups [--dry]   (slur names, stamp
                                          wrappers; see fixups())
"""

import argparse
import base64
import concurrent.futures as cf
import json
import os
import re
import shlex
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

SSH = os.environ.get("SSH_TARGET", "user@vps.example.org")
VPS_TSV = "/srv/pool/veles/data/module-state/slopdrop/safety.tsv"
SLOPDROP = "http://10.0.30.1:8080"
ROUTE_URL = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"
KEY_ENV = "ALI_TOKEN_PLAN_PERSONAL_API_KEY"
TEXT_MODELS = ["deepseek-v4.1-flash-1017", "deepseek-v4-flash-0731"]
VISION_MODEL = "qwen3.8-flash"

# Stage `names`: procs whose purpose is the harm (the 2026-10-12 inventory).
HAND_BLOCKED = ["kunwon1", "img-incog", "bkabait", "ctcbait", "harass_celeb", "doxdrop", "get_ip"]
# Stage `random`: a random stamp or art piece can be any of the bad ones.
RANDOM_PICKERS = ["randart", "slot_pull", "youtubesay", "rSTAMP"]

# The image check's categories that withhold anyone's picture (art.rs
# PUBLIC_WITHHOLD); the prompt is art.rs PROMPT, read from the source.
PUBLIC_WITHHOLD = {"hate_symbol", "slur", "violence_threat", "minor", "personal_info",
                   "harassment", "self_harm"}

TEXT_PROMPT = """You rate Tcl procs from an old IRC bot's library before a bot runs them in PUBLIC Bluesky replies for strangers.
For EACH proc below, judge what calling it can make the bot post, and answer one line per proc:
<name><TAB><verdict>
with verdict one of:
- unsafe: it can post slurs or hate, sexual content involving minors, threats, harassment or doxxing of a real person, someone's private details or IP address, or it is a bait or abuse tool, or it picks its output at random from a set that may include such items;
- adult: none of that, but it can post sexual, nude or graphic (gore) content;
- safe: anything else (jokes, crude humour, swearing, games, utilities, formatting helpers).
Judge by what the code does and the text it contains. Each proc starts with a line `### <name>`; copy the name exactly. Answer only the lines, no commentary, every proc exactly once.

PROCS:
"""

# Stems that keep a proc on the bad list by its NAME alone, whatever a
# model says (folded: lower case, leetspeak undone, letters and digits
# only). The name is what a line types to call a proc, and a stamp is
# called as <name>say; the second opinion passed `STnegroosama` reading
# its drawing. Deliberately broad: a name is not prose, and a false hit
# costs a withheld reply the owner can approve.
NAME_STEMS = ["nigg", "nigr", "negro", "chink", "gook", "spic", "wetback", "kike", "jew",
              "fag", "dyke", "tranny", "trann", "tard", "sperg", "nazi", "swasti", "hitler",
              "heil", "1488", "kkk", "coon", "raghead", "towelhead", "sandnig", "porchmonk"]
# Innocent words a stem sits inside, masked before matching.
NAME_STEM_OK = ["stardefense", "mustard", "custard", "bastard", "standard", "jewel",
                "raccoon", "cocoon", "tycoon", "spice", "spicy"]


def folded(s):
    """check.rs `fold`: lower case, leetspeak undone, letters and digits only."""
    leet = {"0": "o", "1": "i", "3": "e", "4": "a", "5": "s", "7": "t", "@": "a", "$": "s"}
    out = []
    for c in s.lower():
        c = leet.get(c, c)
        if c.isalnum():
            out.append(c)
    return "".join(out)


def name_flagged(name):
    f, raw = folded(name), name.lower()
    for ok in NAME_STEM_OK:
        f, raw = f.replace(ok, ""), raw.replace(ok, "")
    # "1488" folds to "ia88"; the raw lower case keeps the digits.
    return any(st in f or st in raw for st in NAME_STEMS)


# The second opinion (`--recheck`): what the first reading got wrong both
# ways is spelled out. It flagged `fedoragoonsay` (a stamp saying the
# caller's words); a reader of the body alone passed `STnegroosama` (a
# drawing whose slur is its name, the name a line calls it by).
RECHECK_PROMPT = """You rate Tcl procs from an old IRC bot's library before the bot runs them in PUBLIC Bluesky replies for strangers. A first reader flagged each of these as unsafe; give an independent second opinion.
For EACH proc answer one line:
<name><TAB><verdict>
with verdict one of:
- unsafe: its NAME or its output carries a slur or a hate term (the name counts: people type it to call the proc, and a stamp is called as <name>say), hate symbols or nazi references, sexual content involving minors, threats, harassment or doxxing of a real person, someone's private details or IP address; or it is a bait or abuse tool; or it picks its output at random from a set that may include such items;
- adult: none of that, but it can post sexual, nude or graphic (gore) content;
- safe: anything else: jokes, crude humour, swearing, games, utilities, formatting helpers. A proc that draws a picture saying whatever text the caller gives it (a *say proc) is judged by its picture and its name, not by what a caller might type.
Each proc starts with a line `### <name>`; copy the name exactly. Answer only the lines, no commentary, every proc exactly once.

PROCS:
"""


def die(msg):
    print(f"prebake: {msg}", file=sys.stderr)
    sys.exit(1)


# Every call rides ONE ssh connection: the stamps stage makes ~300 calls,
# and a burst of short logins gets the VPS resetting new connections for
# minutes (fail2ban or sshd's MaxStartups).
SSH_OPTS = ["-o", "ControlMaster=auto", "-o", "ControlPersist=120",
            "-o", "ControlPath=" + os.path.join(os.environ.get("TMPDIR", "/tmp"), "prebake-ssh-%C")]


def ssh(cmd, data=None):
    r = subprocess.run(["ssh", *SSH_OPTS, SSH, cmd], input=data, capture_output=True, timeout=600)
    if r.returncode != 0:
        die(f"ssh failed ({r.returncode}): {r.stderr.decode(errors='replace')[-400:]}")
    return r.stdout


def remote_token_curl(path, body=None):
    """curl slopdrop on the VPS with the non-admin token, read there and never echoed."""
    tok = ('T=$(sudo grep "^SLOPDROP_TOKEN=" /srv/pool/veles/data/.env | cut -d= -f2- | tr -d \'"\'); ')
    if body is None:
        cmd = f'cd / && {tok}curl -s -H "Authorization: Bearer $T" {shlex.quote(SLOPDROP + path)}'
        return json.loads(ssh(cmd))
    cmd = (f'cd / && {tok}curl -s -H "Authorization: Bearer $T" -H "Content-Type: application/json" '
           f'-d @- {shlex.quote(SLOPDROP + path)}')
    return json.loads(ssh(cmd, data=json.dumps(body).encode()))


def api_key():
    if os.environ.get(KEY_ENV):
        return os.environ[KEY_ENV]
    for f in [Path.home() / ".config/veles/secrets.env", Path.home() / ".config/veles/.env"]:
        try:
            for line in f.read_text().splitlines():
                k, _, v = line.partition("=")
                if k.strip() == KEY_ENV:
                    return v.strip().strip('"').strip("'")
        except OSError:
            pass
    die(f"{KEY_ENV} is not set and not in the veles env files")


def complete(messages, models, key, max_tokens=4000, extra=None):
    """One chat completion, no tools. A cut stream or a 429 is retried."""
    last = None
    for model in models:
        for attempt in range(4):
            body = json.dumps({"model": model, "messages": messages, "stream": False,
                               "max_tokens": max_tokens, **(extra or {})}).encode()
            req = urllib.request.Request(ROUTE_URL + "/chat/completions", data=body, headers={
                "Authorization": f"Bearer {key}", "Content-Type": "application/json"})
            try:
                with urllib.request.urlopen(req, timeout=180) as r:
                    out = json.loads(r.read())
                text = (out.get("choices") or [{}])[0].get("message", {}).get("content") or ""
                if text.strip():
                    return text
                last = f"{model}: an empty answer"
            except urllib.error.HTTPError as e:
                last = f"{model}: HTTP {e.code}"
                if e.code not in (429, 500, 502, 503, 504):
                    break
            except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as e:
                last = f"{model}: {e}"
            time.sleep(5 * (attempt + 1))
    raise RuntimeError(last or "no answer")


def vision_prompt(repo):
    src = (repo / "crates/veles-slopdrop/src/art.rs").read_text()
    m = re.search(r'pub const PROMPT: &str = "(.*?)";\n', src, re.S)
    if not m:
        die("art.rs PROMPT not found")
    raw = re.sub(r"\\\n\s*", "", m.group(1))
    return raw.replace("\\n", "\n").replace('\\"', '"')


def parse_categories(answer):
    """art.rs parse_answer, in short: the last JSON object with a categories list."""
    answer = answer.split("</think>")[-1]
    for i in reversed([m.start() for m in re.finditer(r"\{", answer)]):
        try:
            v, _ = json.JSONDecoder().raw_decode(answer[i:])
        except json.JSONDecodeError:
            continue
        if isinstance(v, dict) and isinstance(v.get("categories"), list):
            return {str(c).strip().lower() for c in v["categories"]} - {"", "none"}, v.get("reason", "")
    raise ValueError("no verdict in the answer")


def rate_batch(batch, key, prompt=TEXT_PROMPT):
    """One batch of `(name, source)` rated by the text model: {name: verdict}."""
    body = prompt + "\n\n".join(src for _, src in batch)
    # Thinking off: a batch is then ~3 s instead of ~150 s and 15k
    # tokens, and the rating is a lookup, not a puzzle.
    ans = complete([{"role": "user", "content": body}], TEXT_MODELS, key, 8000,
                   {"enable_thinking": False})
    # Names as the model copies them; one that two procs share once
    # trimmed is ambiguous and stays unrated (fails closed).
    names = {}
    for n, _ in batch:
        names.setdefault(n.strip(), []).append(n)
    got = {}
    verdicts = ("safe", "adult", "unsafe")
    prev = None  # a `### name` line with no verdict on it
    for line in ans.split("</think>")[-1].splitlines():
        line = line.strip().strip("`").strip("|").strip()
        if line.startswith("###"):
            line = line[3:]
        # A model that put each verdict on the line after the name.
        bare = line.strip().lower().rstrip(".").strip("*`|: ")
        if bare in verdicts and prev is not None:
            line = f"{prev}\t{bare}"
        prev = None
        # `<name><TAB><verdict>` as asked; a model that wrote
        # spaces, a colon or a bar instead still ends with the word.
        parts = line.rsplit("\t", 1) if "\t" in line else line.rsplit(None, 1)
        if len(parts) != 2 or parts[1].strip().lower().rstrip(".").strip("*`|") not in verdicts:
            prev = line.strip() or None
            if len(parts) != 2:
                continue
        name, v = parts
        v = v.strip().lower().rstrip(".").strip("*`")
        name = name.strip().rstrip(":|-").strip().strip("`*")
        hit = names.get(name, [])
        if len(hit) == 1 and v in ("safe", "adult", "unsafe"):
            got[hit[0]] = v
    if not got:
        sample = " | ".join(ans.split("</think>")[-1].splitlines()[:3])
        print(f"prebake: a batch parsed to nothing; it began: {sample[:200]}",
              file=sys.stderr)
    return got, len(batch)


def source_of(name, p):
    """A proc as the text model reads it."""
    return f"### {name}\nargs: {p.get('args', '')}\n{p.get('body', '')}"[:4000]


def batched(items, cap):
    """`items` ((name, source) pairs) in batches of about `cap` characters."""
    out, cur, size = [], [], 0
    for n, src in items:
        if cur and size + len(src) > cap:
            out.append(cur)
            cur, size = [], 0
        cur.append((n, src))
        size += len(src)
    if cur:
        out.append(cur)
    return out


def recheck(a, by_name, have):
    """A second opinion on the text model's own `unsafe` rows (a report of
    an earlier run, `--recheck REPORT`): each is rated again in a small
    batch (about 2,500 characters, so its neighbours sway it less), and the
    row stays `unsafe` only when this rating agrees. A bad-list row is a
    backstop: the word checks and the image check still read what a proc
    posts, while a wrong row withholds a stranger's reply and asks the
    owner. The first run (2026-10-12) put `fedoragoonsay`, whose body is
    `stampsay -l -s fedoragoon [join $args]`, on the bad list. Rows the
    hand list, the random pickers or the image check decided, and any row
    the owner changed since (it is no longer `unsafe`), are left alone.
    A second `safe` becomes `safe`; a second `adult` removes the row."""
    key = api_key()
    rows = [l.split("\t") for l in Path(a.recheck).read_text().splitlines()]
    cand = [(r[0], r[1]) for r in rows
            if len(r) >= 4 and r[2] == "unsafe" and r[3] == "rated"
            and have.get(r[1]) == "unsafe"
            and r[0] in by_name and by_name[r[0]]["hash"] == r[1]
            and r[0] not in HAND_BLOCKED and r[0] not in RANDOM_PICKERS
            and not name_flagged(r[0])]
    batches = batched([(n, source_of(n, by_name[n])) for n, _ in cand], 2500)
    if a.limit:
        batches = batches[: a.limit]
    print(f"prebake: {len(cand)} unsafe rows to recheck, {len(batches)} batches")
    second = {}
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        for fut in cf.as_completed([ex.submit(rate_batch, b, key, RECHECK_PROMPT)
                                    for b in batches]):
            try:
                got, _ = fut.result()
            except Exception as e:  # a failed batch keeps its rows unsafe
                print(f"prebake: a batch failed: {e}", file=sys.stderr)
                continue
            second.update(got)
    change = {by_name[n]["hash"]: (v, n) for n, v in second.items() if v in ("safe", "adult")}
    counts = {}
    for v in second.values():
        counts[v] = counts.get(v, 0) + 1
    print(f"prebake: rechecked {len(second)}: {counts}; {len(change)} rows come off the bad list")
    with open(a.report, "w") as f:
        for h, (v, n) in sorted(change.items(), key=lambda kv: kv[1][1]):
            f.write(f"{n}\t{h}\tunsafe->{v}\n")
    print(f"prebake: report in {a.report}")
    if a.dry or not change:
        return
    apply_changes({h: ("safe" if v == "safe" else "-") for h, (v, _) in change.items()},
                  "recheck")


def apply_changes(changes, label):
    """Rewrite the VPS store with `changes` ({hash: verdict}, `-` = remove the
    row), after a backup. Only a row the prebake writes (`safe`, `unsafe`)
    changes; the owner's `approved` and `blocked`, and `borderline`, are
    never touched. A hash with no row gets one (not for `-`)."""
    rows = "".join(f"{h}\t{v}\n" for h, v in sorted(changes.items()))
    stamp = time.strftime("%Y%m%d%H%M%S")
    q = shlex.quote
    prog = ('NR==FNR {m[$1]=$2; next} '
            '($1 in m) {seen[$1]=1; if (($2=="safe" || $2=="unsafe") && m[$1]!="-") '
            '{print $1 "\\t" m[$1]; next} if ($2=="safe" || $2=="unsafe") next} '
            '{print} '
            'END {for (h in m) if (!(h in seen) && m[h]!="-") print h "\\t" m[h]}')
    rewrite = (f"awk -F'\\t' {q(prog)} /dev/stdin {q(VPS_TSV)} | sort > {q(VPS_TSV + '.new')} && "
               f"chown --reference={q(VPS_TSV)} {q(VPS_TSV + '.new')} && "
               f"chmod --reference={q(VPS_TSV)} {q(VPS_TSV + '.new')} && "
               f"mv {q(VPS_TSV + '.new')} {q(VPS_TSV)} && wc -l < {q(VPS_TSV)}")
    out = ssh(f"cd / && sudo cp -p {q(VPS_TSV)} {q(VPS_TSV + '.bak-pre-' + label + '-' + stamp)} && "
              f"sudo sh -c {q(rewrite)}", data=rows.encode())
    print(f"prebake: {label}: the file has {out.decode().strip()} verdicts "
          f"(backup {VPS_TSV}.bak-pre-{label}-{stamp})")


WRAPPER = re.compile(r"^\s*stamp(?:say|think)\s+(?:-l\s+)?-s\s+(\S+)\s+\[join \$args\]\s*$")


def fixups(a, by_name, have):
    """Deterministic corrections over the store (`--fixups`):
    - a proc whose name carries a slur or hate stem ([`NAME_STEMS`]) is
      `unsafe`, whatever a model said;
    - a pure stamp wrapper, `<x>say` or `<x>think` whose whole body is
      `stampsay ?-l? -s <x> [join $args]`, takes its stamp's verdict: the
      words are the caller's, the picture is the stamp's, and the stamp
      was rated by its drawing and its source (`fedoragoonsay` was put on
      the bad list by a model that judged what a caller might type).
    Only rows the prebake writes change (see [`apply_changes`])."""
    changes = {}
    why = {}
    for n, p in by_name.items():
        if p.get("origin") != "base":
            continue
        h = p["hash"]
        cur = have.get(h)
        if cur not in (None, "safe", "unsafe"):
            continue
        if name_flagged(n):
            if cur != "unsafe":
                changes[h], why[h] = "unsafe", f"{n}: its name"
            continue
        m = WRAPPER.match(p.get("body", ""))
        if m and (n == m.group(1) + "say" or n == m.group(1) + "think"):
            st = by_name.get("ST" + m.group(1))
            sv = have.get(st["hash"]) if st else None
            if sv in ("safe", "unsafe") and sv != cur:
                changes[h], why[h] = sv, f"{n}: its stamp ST{m.group(1)} is {sv}"
    for h in sorted(changes, key=lambda h: why[h]):
        print(f"  {changes[h]:7} {why[h]}")
    print(f"prebake: {len(changes)} fixups")
    with open(a.report, "w") as f:
        for h in sorted(changes, key=lambda h: why[h]):
            f.write(f"{why[h]}\t{h}\t{changes[h]}\n")
    if a.dry or not changes:
        return
    apply_changes(changes, "fixups")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry", action="store_true")
    ap.add_argument("--stage", default="names,random,stamps,procs")
    ap.add_argument("--limit", type=int, default=0, help="at most N model calls per stage")
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--render", help="the veles-textart render example binary")
    ap.add_argument("--recheck", help="a report of an earlier run: re-rate its text-model unsafe rows")
    ap.add_argument("--fixups", action="store_true",
                    help="the deterministic corrections: slur names, stamp wrappers")
    ap.add_argument("--report", default=os.environ.get("TMPDIR", "/tmp") + "/slopdrop-prebake.tsv")
    a = ap.parse_args()
    repo = Path(__file__).resolve().parent.parent
    stages = set(a.stage.split(","))

    procs = remote_token_curl("/api/procs?base=1")
    if not isinstance(procs, list):
        die("the procs listing is not a list")
    by_name = {p["name"].lstrip(":"): p for p in procs if p.get("name") and p.get("hash")}
    tsv = ssh(f"cd / && sudo cat {VPS_TSV}").decode()
    have = dict(l.split("\t", 1) for l in tsv.splitlines() if "\t" in l)
    print(f"prebake: {len(by_name)} procs ({sum(1 for p in procs if p.get('origin') == 'base')} base), "
          f"{len(have)} verdicts")
    if a.recheck:
        return recheck(a, by_name, have)
    if a.fixups:
        return fixups(a, by_name, have)
    new = {}      # hash -> (verdict, name, why)
    # The worse verdict wins: a stamp is rated twice, by the image check
    # on its drawing and by the text model on its source, because what
    # the picture shows is not all it says (`ST_tard1` draws a face with
    # no words; its slur is its name, the name a line calls it by).
    worse = {"safe": 0, "adult": 1, "borderline": 2, "unsafe": 3}

    def put(name, verdict, why):
        p = by_name.get(name)
        if not p or p["hash"] in have:
            return
        had = new.get(p["hash"])
        if had is None or worse[verdict] > worse[had[0]]:
            new[p["hash"]] = (verdict, name, why)

    if "names" in stages:
        for n in HAND_BLOCKED:
            put(n, "unsafe", "hand list")
    if "random" in stages:
        for n in RANDOM_PICKERS:
            put(n, "unsafe", "picks at random")

    if "stamps" in stages:
        if not a.render:
            die("--render <veles-textart render binary> is needed for the stamps stage")
        key = api_key()
        prompt = vision_prompt(repo)
        stamps = sorted(n for n, p in by_name.items()
                        if n.startswith("ST") and p["hash"] not in have and p["hash"] not in new)
        if a.limit:
            stamps = stamps[: a.limit]
        out_dir = Path(os.environ.get("TMPDIR", "/tmp")) / "slopdrop-prebake-stamps"
        out_dir.mkdir(parents=True, exist_ok=True)

        def one(name):
            r = remote_token_curl("/api/eval", {"code": name, "user": "prebake"})
            if r.get("is_error"):
                return name, "borderline", "the stamp errors"
            text = "\n".join(r.get("output") or [])
            png = out_dir / f"{name}.png"
            rr = subprocess.run([a.render, str(png)], input=text.encode(), capture_output=True)
            if rr.returncode != 0:
                return name, "borderline", "it could not be drawn"
            data = "data:image/png;base64," + base64.b64encode(png.read_bytes()).decode()
            ans = complete([{"role": "user", "content": [
                {"type": "text", "text": prompt},
                {"type": "image_url", "image_url": {"url": data}}]}], [VISION_MODEL], key, 2000)
            cats, why = parse_categories(ans)
            bad = sorted(cats & PUBLIC_WITHHOLD)
            if bad:
                return name, "unsafe", ",".join(bad) + ": " + why
            # Labelled per reply by the image check: no row.
            return name, ("adult" if cats else "safe"), ",".join(sorted(cats))

        with cf.ThreadPoolExecutor(a.jobs) as ex:
            for fut in cf.as_completed([ex.submit(one, n) for n in stamps]):
                try:
                    name, verdict, why = fut.result()
                except Exception as e:  # a stamp that fails stays without a verdict: fails closed
                    print(f"prebake: a stamp failed: {e}", file=sys.stderr)
                    continue
                put(name, verdict, why)
                print(f"  {verdict:10} {name}  {why[:100]}")

    if "procs" in stages:
        key = api_key()
        # Every base proc without a verdict, and the stamps the image check
        # rated short of unsafe (their source is read as well).
        todo = sorted(n for n, p in by_name.items()
                      if p.get("origin") == "base" and p["hash"] not in have
                      and new.get(p["hash"], ("safe",))[0] != "unsafe"
                      and not (p["hash"] in new and not n.startswith("ST")))
        batches = batched([(n, source_of(n, by_name[n])) for n in todo], 10_000)
        if a.limit:
            batches = batches[: a.limit]
        print(f"prebake: {len(todo)} base procs without a verdict, {len(batches)} batches")

        text_rated = set()

        def run(batches, label):
            done = 0
            with cf.ThreadPoolExecutor(a.jobs) as ex:
                for fut in cf.as_completed([ex.submit(rate_batch, b, key) for b in batches]):
                    try:
                        got, n = fut.result()
                    except Exception as e:  # a failed batch stays unrated
                        print(f"prebake: a batch failed: {e}", file=sys.stderr)
                        continue
                    for name, v in got.items():
                        put(name, v, "rated")
                        text_rated.add(name)
                    done += 1
                    print(f"  {label} {done}/{len(batches)}: {len(got)}/{n} rated")

        run(batches, "batch")
        # What a batch left unrated (a refusal, a garbled answer, a failed
        # call) goes again in small batches, once; what is still unrated
        # after that keeps no verdict and the image check decides alone.
        left = [(n, src) for b in batches for (n, src) in b if n not in text_rated]
        if left:
            small = batched(left, 2500)
            print(f"prebake: {len(left)} procs unrated, again in {len(small)} small batches")
            run(small, "retry")

    counts = {}
    for v, _, _ in new.values():
        counts[v] = counts.get(v, 0) + 1
    print(f"prebake: {len(new)} new verdicts {counts}")
    with open(a.report, "w") as f:
        for h, (v, n, why) in sorted(new.items(), key=lambda kv: kv[1][1]):
            f.write(f"{n}\t{h}\t{v}\t{why}\n")
    print(f"prebake: report in {a.report}")
    if a.dry or not any(v != "adult" for v, _, _ in new.values()):
        return
    # Only the new rows travel, and the merge happens on the VPS at write
    # time with the file's rows FIRST, so a verdict the owner gave while
    # this ran (an approve, a delete) wins over ours.
    rows = "".join(f"{h}\t{v}\n" for h, (v, _, _) in sorted(new.items()) if v != "adult")
    stamp = time.strftime("%Y%m%d%H%M%S")
    q = shlex.quote
    merge = (f"cat {q(VPS_TSV)} - | awk -F'\\t' 'NF==2 && !seen[$1]++' | sort > {q(VPS_TSV + '.new')} && "
             f"chown --reference={q(VPS_TSV)} {q(VPS_TSV + '.new')} && "
             f"chmod --reference={q(VPS_TSV)} {q(VPS_TSV + '.new')} && "
             f"mv {q(VPS_TSV + '.new')} {q(VPS_TSV)} && wc -l < {q(VPS_TSV)}")
    out = ssh(f"cd / && sudo cp -p {q(VPS_TSV)} {q(VPS_TSV + '.bak-pre-prebake-' + stamp)} && "
              f"sudo sh -c {q(merge)}", data=rows.encode())
    print(f"prebake: merged; the file has {out.decode().strip()} verdicts "
          f"(backup {VPS_TSV}.bak-pre-prebake-{stamp})")


if __name__ == "__main__":
    main()

# devharness — drive the whole stack without a real model

`mock_provider.py` is a minimal OpenAI-compatible **streaming** provider:
it streams `reasoning_content` first, then `content`, exactly like the
reasoning models this agent talks to, and logs the message list it was
sent. That makes the full loop reproducible offline:

    mock provider  ->  veles gateway  ->  the real Tk desktop under Xvfb

## Why it exists

Three of the worst bugs in this project were invisible to unit tests
because they lived in the seams:

* Tk silently drops `insert` on a `-state disabled` text widget, so a
  whole session's answers vanished with no error anywhere.
* `StreamEvent::Route` carried only the provider, so the status bar
  could not name the model — visible only by looking at the real bar.
* A link inside `**bold**` rendered as raw markdown — visible only by
  rendering a real reply.

Each was found in minutes here after a long time hiding.

## Run it

    bash scripts/devharness/desktop_e2e.sh

It starts the mock on :18999, a gateway on :18642 with a scratch
`VELES_HOME` under /dev/shm, boots `veles-desktop/main.tcl` on a
throwaway X display, sends a prompt and dumps the transcript, its tag
ranges and the status bar.

Edit the `answer` in `mock_provider.py` to render whatever reply you
need to inspect.

## `lsp_e2e.sh` — the LSP path, end to end

    bash scripts/devharness/lsp_e2e.sh

No mock model (the language-server path never calls one). It builds a
scratch `VELES_HOME` with a project (its own git root) and a plugins
tree, starts a gateway on :18646, boots the real desktop under Xvfb, and
drives `::vlsp` / `::vdev`: hover and go-to-definition on a project proc,
then on `::veles::call_llm` in a plugin (resolved through the compiled-in
`plugins/veles_api.tcl` stub that `sync_api_stub` drops next to the live
plugins), a tab switch that must re-point the server, a root ping-pong
that must not thrash or crash it, and Shift-hold on a live tooltip. Prints
one line per check, then `lsp_e2e: N passed, M failed`, `BGERRORS: 0`,
`LSP_E2E_DONE`, and the tcl-lsp spawn count (one per root switch — more
means a start is thrashing the session). The Tk suites stub `::vlsp`
(`devtest`) or the event stream (`lspstarttest`); this is the only thing
that drives a real handshake against the real `tcl-lsp` binary.

## cache_e2e — is the prefix actually stable, and is the reporting honest?

    bash scripts/devharness/cache_e2e.sh

Drives `cache_mock.tcl` (a cache-aware provider) behind a real gateway on
a scratch home, then asserts on what the provider ACTUALLY received —
which is the one thing the unit tests cannot see. The headline number it
produces: a 6-round turn in each of two independent sessions sends **one
distinct prefix**, not twelve.

`cache_mock.tcl` is tclsh, not python — `python3` is not in this dev
shell (see Caveats), and this repo has Tcl everywhere anyway. It
sha256-hashes the cacheable prefix (system message plus tool array) per
request, keeps a census on `GET /__stats`, reports
`prompt_tokens_details.cached_tokens` like a caching provider, drives an
N-round tool loop, and can arm one 429 via `GET /__fail429`.

It counts TURN rounds separately from AUX calls: a request with no tools
is the auto-titler or the compaction summarizer, which has its own small
prompt by design. Counting those as prefix misses reports a cache defect
that is not there — the first version did, and the false alarm is how an
instrument gets ignored.

Assertions: one prefix per turn (C3), the same prefix across two sessions
(C2), the cache hit rate reaching `/api/sessions/:id/context` (C4),
`doctor --prefix` agreeing with the wire (C8), tool tiering delivering
and `tier = "full"` rolling it back (C5), no `reasoning_content` replayed
by default (Step 3.3), and no `cache_control` on the wire under the
default `auto` mode (Step 1.5 — the one that keeps a strict endpoint from
400-ing).

## Caveats

* `python3` is NOT in the pure dev shell, and `wish` is not outside it —
  so the harness runs the mock and gateway outside `direnv` and only
  `wish` inside, and Xvfb has to be started in the SAME shell as `wish`
  (the pure shell drops the X auth env otherwise).
* This is a DEVELOPMENT harness, not part of `nix flake check`. It needs
  a network-free localhost socket and a real X server; the Tcl suites in
  `scripts/tcl_tests.sh` are the gate.

## `tasks_e2e.sh` — background shells, task listing, and a cancel with teeth

```sh
bash scripts/devharness/tasks_e2e.sh
```

Boots a real gateway on a scratch `VELES_HOME`, starts a real background
shell through `POST /api/shells`, and checks the whole surface end to
end: the task appears in `GET /api/tasks`, the *separate* `veles tasks`
CLI process sees it over loopback, output is captured and readable, and
a finished shell reports `done` with its exit code.

**The load-bearing assertion is `kill -0`.** The shell writes its own pid
to a marker file; after `POST /api/tasks/:id/cancel` the harness insists
that pid is gone. Reporting "cancelled" is easy and worthless on its own
— `a2a`'s `tasks/cancel` (`a2a_server.rs`) flips a state string and
leaves the work running, and that is precisely the shape of cancel this
feature must not have shipped. Nothing else in the tree tests that
distinction, because until now nothing else could be cancelled.

No mock provider: the task path never calls a model.

# Writing veles plugins

Eight tiers, and what each can actually reach. The short version:

| tier | where it lives | can it drive the engine? | rebuild veles? |
| --- | --- | --- | --- |
| **Tcl** | `~/.config/veles/plugins/<name>/plugin.tcl` | yes, all of it | no — hot reload |
| **JavaScript / TypeScript** | `~/.config/veles/plugins/<name>/plugin.ts` + `kind = "js"` | yes, the same operations as Tcl (`veles.*`) | no — hot reload, a fresh context per load (R9, the lang-js module) |
| **Racket** | `~/.config/veles/plugins/<name>/plugin.rkt` + `kind = "racket"` | yes, the same operations as Tcl (the `veles` collection) | no — hot reload, a fresh namespace and custodian per load (R9, the lang-racket module; Racket CS stays booted) |
| **Common Lisp** | `~/.config/veles/plugins/<name>/plugin.lisp` + `kind = "lisp"` | yes, the same operations as Tcl (the `VELES` package) | no — hot reload, a fresh package per load (R9, the lang-lisp module; ECL stays booted) |
| **Rust, out-of-tree** | a `cdylib` + `kind = "rust"` manifest | yes, all of it (ABI 4: ops, platforms, timers, sockets, async) | no — `unload`/`reload` in-process, by owner; ABI 4 only (an ABI 3 `.so` is refused by name, before any of its code runs) |
| **Module** | a Rust `dylib` on `libveles_runtime.so` + `kind = "module"` | yes, core's own API directly | no — a module reloads in-process (the gateway PID stays), but only against the SAME build of core (the build key) |
| **Process** | an executable + `kind = "process"` manifest | through host methods only (protocol v1) | no — reload restarts only that process |
| **Rust, in-tree** | a crate linked into the binary | yes, it *is* the engine | yes |

The module tier is for the platforms
and hosts that need core in-process (IRC's ask/ACL/lockdown verbs). A
module is built together with the gateway (`cargo build -p veles-cli -p
<module>`), declares itself with `veles_core::export!`, and the host
refuses any module whose key, runtime link or core imports do not match
this process — from the file, before any of its code runs. Teardown is
host-driven: the gate closes, `stop(reason)` gets a deadline, every
registration the instance made is purged by owner, its runtime is shut
down, and the library is unmapped — or kept mapped, with the reason
(a pinned library, a module's own tracing callsite), never unmapping
live code.

Until 2026-09-04 the middle row was "no". ABI version 1 gave an
out-of-tree plugin three function pointers, all of them registration: it
could be called BY veles and could never call veles. That is closed;
`rustplugins.rs` records what the claim used to be and why it was wrong.
ABI 4 (2026-10-09, R8) REPLACED ABI 3 rather than extending it: every
resource a `.so` registers is host-owned and handle-addressed, so the
host can unload a native plugin as completely as a module, and a plugin
can be a platform, hold timers and TCP/TLS sockets, and answer a tool
asynchronously. The version is read from its own export before init;
an ABI 3 plugin is refused with the rebuild instruction, never run.

The in-tree tier is a real option but it is not really a plugin — you
are editing the program. Use it when you want the type checker and the
engine's own crate graph. Use the other two for everything else.

---

## What a plugin can register

| surface | Tcl | out-of-tree Rust |
| --- | --- | --- |
| lifecycle hook | `::veles::hook <hook> <proc>` | `register_hook` |
| tool (model-callable) | `veles::register_tool` | `register_tool` |
| slash command | `veles::register_command` | `register_command` |
| gateway filter | `pre_gateway_dispatch` hook | `register_gateway_filter` |
| **provider driver (a wire)** | `veles::register_provider` | `register_provider` |
| **service** (for other plugins, by name) | `veles::register_service` | — (a module: `services::register`; see [Services](#services--one-plugin-using-anothers)) |
| async tool (a future, no parked thread) | — (Tcl handlers are sync) | `register_tool_async` |
| **chat platform** (outbound route + inbound publish) | — | `register_platform` / `platform_inbound` |
| timer | Tcl's own `after` | `timer_after` / `timer_cancel` |
| TCP / TLS socket | Tcl's own `socket` + `tls` | `tcp_connect` / `tcp_send` / `tcp_close` |

Every out-of-tree registration answers a handle the host owns;
`unregister(handle)` drops one early and an unload drops them all.

`veles plugins hooks` lists every hook point, which the engine actually
emits, and which have a handler attached right now.

## What a plugin can call

One table, `bridgeops::OP_NAMES`, reached from Tcl as `veles::<op>` and
from a `.so` as `host_call("<op>", "[…args…]")`:

```
call_llm          config_get        config_set        history           http_get
http_post         http_stream_close http_stream_next  http_stream_open  json_get
limits            nodes             note              principal         provider_emit
routes            sandbox_where     send_outbound     service_call      service_next
services          sessions          task_cancel       tasks             tool_call
usage_totals
```

`http_stream_open` / `http_stream_next` / `http_stream_close` are the
streaming transport, and they are three ops rather than one because a driver
runs on a blocking thread and has to PULL: `open` answers as soon as the
response status is known, `next` yields one event at a time, `close` frees the
handle. `http_post` buffers a whole body and carries a 30 second total
timeout unless given a longer one (`?timeout_secs?`), so a driver built on it
cannot show incremental output. See "Writing a provider driver".

A cargo test cross-checks that list against the live interpreter, against
`plugins/veles_api.tcl`, and against what `host_call` accepts. An op
cannot reach one language and not the other; if it could, the gate fails.

### Who the call is for

`principal`, `sandbox_where` and `limits` read the identity model the
plugin lives in. The bridge predated it entirely — sixteen ops, none of
which could answer "who is this call for" — so a hook firing on an
inbound message could not tell a guest from the owner, which is the one
distinction the access model draws.

All three are read-only. They take an optional session id and answer
honestly when there is none (an owner surface has no principal, and
saying so beats inventing a guest).

### What a plugin is NOT trusted with

Plugins are owner-installed code, so these are not stranger-reachable
holes. They were undeclared capabilities that announced nothing.

- **`tool_call` is gated, as the caller.** It dispatches through a
  session's registry and applies `acl::gate_denial` — the same function
  the model-driven tool loop calls, judging the resolved target rather
  than the alias. Before, the check lived only in the tool loop, so this
  op reached every tool with no capability check and no approval.

  WHOSE registry is the part that took a second round. The engine
  publishes one per session (`tools::publish_session_registry`, a bounded
  map — 256 live sessions, oldest evicted) and resolves a call in this
  order: the `session_id` argument, then the session whose hook is
  running, then the last-published one. The middle step is what a plugin
  normally relies on: every hook payload carries `session_id`, and the
  engine publishes it as the ambient identity for the duration of the
  handler call, in both languages. So a handler that calls
  `veles::tool_call` with no arguments is judged as its own caller — a
  guest's plugin cannot borrow the owner's capabilities, and the owner's
  is not denied a guest's.

  The first step exists because the ambient is same-thread by
  construction: a handler that defers its call to a thread of its own
  must name the session. The last is the pre-existing behaviour, now a
  fallback for a call with no session at all rather than the only answer.
  A named session with no published registry is an error, not a silent
  fall-through to somebody else's.

  Both dispatch paths are gated. A plugin-registered Tcl tool runs
  directly on the interpreter thread (an async dispatch from there would
  wedge it), and that path used to skip the check entirely.

- **`config_set` cannot rewrite the access model.** `[acl]`,
  `[tool_caps]` and the keys that ARE the sandbox fence
  (`user_env_network`, `mounts`, `extra_args`, `capabilities`,
  `backend`, the default-zone knobs, `envs`/`assign`) are refused unless
  `[plugins] allow_acl_writes = true`. `[acl]` is live-reloaded, so an
  unrestricted write was a way to grant yourself owner with immediate
  effect. Ordinary keys — `sandbox.memory_mb`, `model.default` — are
  untouched: a guard broad enough to be annoying is a guard people
  disable.

- **Both announce themselves.** `tool_call` and `config_set` were the
  only two ops that did not park a `⚙ plugin` note;
  `send_outbound`, `note`, `task_cancel` and `call_llm` all did. The two
  most powerful things a plugin can do were the two invisible ones.

`plugins/veles_api.tcl` is the signature reference. It is copied next to
your plugins at boot so a language server can resolve `::veles::*`, and
a test keeps it honest in both directions — nothing missing, nothing
invented.

---

## Tcl

Each plugin is sourced under its own owner (`tcl:<name>`, R7): the
tools, commands and providers it registers while being sourced, and
the hook handlers it adds with `veles::hook`, are attributed to it.
`veles plugins unload <name>` (or `/plugins unload <name>`) drops all
of them and keeps the plugin out of every reload — the file watcher's
included — until `veles plugins start <name>` sources it again;
`reload <name>` re-sources one plugin, purging its earlier load first.
A handler registered later, from a callback, is core's: the owner is
scoped to the sourcing. `[plugins] disabled` in config.toml is the
durable switch; an unload lasts until the gateway restarts.

`~/.config/veles/plugins/<name>/` with a `plugin.toml`:

```toml
name = "my-plugin"
kind = "tcl"                    # or "rust" (entry = "plugin.so")
entry = "plugin.tcl"
provides_hooks = ["post_llm_call"]
description = "..."
```

`veles plugins new <name>` scaffolds a working one. The directory is a
git repo the runtime snapshots on every change, so `veles plugins
rollback <commit>` is a real undo — and it commits the working tree
first, so uncommitted edits survive the rollback that overwrites them.

### Bundled plugins

A release also carries a few Tcl plugins of its own, in a second,
**read-only** root: `<prefix>/share/veles/plugins/<name>/` beside the
binary (`bin/../share/veles/plugins` in the Linux archive, the AppImage,
the nix package and so the OCI image, and the Windows zip), or the
directories named in `VELES_BUNDLED_PLUGINS` (a search path: `:`
between entries, `;` on Windows — the APK sets it to the plugins it
unpacks from `runtime.zip`). 0.1.2 ships one: `tcl-anthropic-driver`,
the Anthropic Messages driver the setup wizard's Anthropic choice uses
(see [Writing a provider driver](#writing-a-provider-driver)).

The two roots merge by manifest name, and **your copy wins**: a
`~/.config/veles/plugins/tcl-anthropic-driver/` replaces the bundled one
entirely, which is how you edit or pin a bundled plugin. A bundled
plugin obeys `[plugins] enabled` / `disabled` like any other —
`disabled = ["tcl-anthropic-driver"]` switches it off. veles never
writes to the bundled root: it is not part of the plugins git repo, the
watcher does not watch it, and `veles plugins remove` refuses a bundled
plugin by name (disable it, or shadow it, instead). `veles plugins list`
names the bundled roots it found and marks each row `src=bundled` or
`src=user`; `/api/plugins` carries `bundled: true|false`.

Reloading is automatic (a watcher on the directory) and manual
(`veles tcl reload`, which asks the RUNNING gateway over loopback — it
is the live interpreter you want, not a fresh one).
**A plugin file is re-sourced on every reload, so it must be
idempotent**: redefining a proc is fine, appending to a global list
twice is not. Registration is replace-by-name everywhere — tools, slash
commands, provider drivers, rust plugins — so re-registering the same
name replaces rather than stacks.

Hook handlers read their arguments from `::veles::hook_args` (a JSON
string, set as a VARIABLE so braces survive) and return `""` for "no
opinion" or a JSON verdict to decide. First non-empty verdict wins.

Every payload carries `session_id`, and `::veles::hook_session` holds the
same value while your handler runs (`""` outside one). Pass it to
`veles::tool_call` only if you call from a thread of your own; on your
own thread the engine has already published it as the ambient identity.

## JavaScript / TypeScript

A `kind = "js"` plugin is run by the **lang-js** module (R9): one
QuickJS-ng runtime for the gateway, a context of its own per plugin,
TypeScript stripped by oxc before the engine sees it (`.ts`, `.mts`:
types, enums, namespaces, parameter properties; nothing is lowered to
an older ECMAScript, the engine speaks the current one). The entry is
evaluated as an ES module, so top-level `await` works and the file
starts in strict mode; there is no module loader, so `import` of other
files is not available yet.

```toml
name = "my-plugin"
kind = "js"
entry = "plugin.ts"            # or plugin.js / plugin.mjs
description = "..."
```

`veles plugins new <name> --kind js` scaffolds one. The lifecycle is
the Tcl tier's: the plugin is loaded when the host starts, re-loaded
in a FRESH context when a file under the plugins directory changes or
on `veles plugins reload <name>`, and `veles plugins unload <name>`
takes it out (its tools, commands and hooks purged by its owner
`js:<name>`) until `start` names it again. A load that throws leaves
nothing registered and the listing says `error` with the message.

The API is the `veles` object, bound before the plugin's own code; every
call answers a Promise:

```ts
await veles.registerTool(
  { name: "weather", description: "...", schema: { type: "object", properties: { city: { type: "string" } } } },
  async (args: { city: string }, sessionId: string) => {
    const body = await veles.httpGet(`https://example.net/${args.city}`);
    return `it is ${JSON.parse(body).temp}°`;        // a string, or any value (JSON for the model)
  },
);
await veles.registerCommand("hello", "a prompt", (text: string) => `Say hello to ${text}`);
veles.hook("post_llm_call", (args: { content?: string }) =>
  args.content?.includes("secret") ? { decision: "redact" } : undefined);   // undefined = no opinion
```

`veles.toolCall(name, args, sessionId?)`, `callLLM`, `httpGet`,
`httpPost`, `configGet`, `configSet`, `note`, `sendOutbound`,
`principal`, `sessions`, `history`, `usageTotals`, `limits`, `nodes`,
`routes`, `tasks`, `taskCancel`, `sandboxWhere`, `providerEmit` and
`jsonGet` are the same operations `veles::*` gives Tcl, by the same
names in camelCase (`veles.call("tool_call", …)` reaches any of them by
its Tcl name). A tool handler gets the calling session as its second
argument: pass it to `toolCall` so the call is judged as that
principal. `console.log/warn/error` go to the gateway's journal under
the plugin's owner; `setTimeout`, `setInterval` and their clears work.

What is enforced: a callback that runs for more than 30 s without
pausing (an `await` pauses) is interrupted and answers an error, and
the other plugins carry on; a tool has 300 s to answer, a command 60 s,
a hook 30 s; the heap is capped at 64 MB per runtime. What is not: a
plugin is trusted code on the gateway's machine, like a Tcl plugin.

The tiers after this one (Racket, Common Lisp) are the same seam with
another engine, each a module of its own: `kind = "racket"` and
`kind = "lisp"`.


## Racket

A `kind = "racket"` plugin is run by the **lang-racket** module (R9):
Racket CS 9.3 embedded in the gateway, booted once per process, a
namespace and a custodian of its own per plugin. The entry is a
`#lang racket/base` module (any `#lang` the install's collects provide
works) that requires the `veles` collection the host supplies; it is
instantiated on load, and again in a fresh namespace on every reload,
so its top level runs once per load.

```toml
name = "my-plugin"
kind = "racket"
entry = "plugin.rkt"
description = "..."
```

`veles plugins new <name> --kind racket` scaffolds one. The lifecycle is
the Tcl tier's: loaded when the host starts, re-loaded when a file
under the plugins directory changes or on `veles plugins reload
<name>`, and `veles plugins unload <name>` shuts the plugin's custodian
down (its threads die, its tools, commands and hooks are purged by its
owner `racket:<name>`) until `start` names it again. A load that
raises leaves nothing registered and the listing says `error` with the
message; `(exit)` is refused. Each plugin's custodian is limited to 64
MB; a handler that runs 300 s without answering is killed.

```racket
#lang racket/base
(require veles json)
(register-tool "weather" "..." (hash 'type "object" 'properties (hash 'city (hash 'type "string")))
  (lambda (args session-id)
    (define body (http-get (format "https://example.net/~a" (hash-ref args 'city))))
    (format "it is ~a°" (hash-ref (string->jsexpr body) 'temp))))   ; a string, or a jsexpr (JSON for the model)
(register-command "hello" "a prompt" (lambda (text) (format "Say hello to ~a" text)))
(hook "post_llm_call"
  (lambda (args)
    (if (regexp-match? #rx"secret" (hash-ref args 'content ""))
        (hash 'decision "redact")
        (void))))                                                     ; (void) = no opinion
```

Handlers run as Racket threads, so a slow one blocks nothing else; a
host call from a handler (`http-get`, `tool-call`, …) waits on a
channel while the engine answers. `tool-call`, `call-llm`, `http-get`,
`http-post`, `config-get`, `config-set`, `note`, `send-outbound`,
`principal`, `sessions`, `history`, `usage-totals`, `limits`, `nodes`,
`routes`, `tasks`, `task-cancel`, `sandbox-where`, `provider-emit` and
`json-get` are the same operations `veles::*` gives Tcl, by the same
table; `(veles-call op arg ...)` reaches any of them by name; a plugin
offers a service with `(register-service name description handler
#:cap cap)` and calls another's with `(service-call name params)`
([Services](#services--one-plugin-using-anothers)); and
`(veles-log text [level])` writes under the plugin's owner.

Two things a Racket plugin must not do: `(exit)`, which is refused, and
subprocesses (`system`, `process`, `subprocess`): the gateway keeps its
own SIGCHLD handling, so a child Racket spawns is never reaped by
Racket. Run commands through the engine's tools (`tool-call`) instead.

What the host is, so the `/plugins` listing reads right: Racket CS is
process-global, so the lang-racket module stays mapped for the
gateway's life — a stop or unload of the MODULE waits out the host's
10 s shutdown deadline on its engine thread and reports it `kept
mapped`, a `start` after a `stop` reuses the booted engine (its
scripts load again), and a replaced module file is a gateway restart
(a second copy refuses to boot Racket again). Plugins, by contrast, reload freely. The engine
costs about 95 MB of memory once up. An error that escapes the runtime
at boot (a collection in the state dir that does not even read) ends
the engine with that message and the module's start fails with it; the
gateway stays up, where Racket's own escape handler would have exited
it. The gateway's own SIGTERM, SIGINT and SIGCHLD handlers go in
before any module starts and the boot puts back what it found, so a
stop reaches the gateway with Racket up. It is not built for Android.

## Common Lisp

A `kind = "lisp"` plugin is run by the **lang-lisp** module (R9): ECL
26.5.5 embedded in the gateway, booted once per process. The entry is a
Lisp file the host loads into a package of its own (`VELES-SCRIPT/<NAME>`)
that uses COMMON-LISP and VELES, as source, compiled to ECL's bytecode:
there is no C compiler in the image. It is loaded again into a fresh
package on every reload, so its top level runs once per load.

```toml
name = "my-plugin"
kind = "lisp"
entry = "plugin.lisp"
description = "..."
```

`veles plugins new <name> --kind lisp` scaffolds one. The lifecycle is
the Tcl tier's: loaded when the host starts, re-loaded when a file
under the plugins directory changes or on `veles plugins reload
<name>`, and `veles plugins unload <name>` kills the plugin's threads
and deletes its packages (its tools, commands and hooks are purged by
its owner `lisp:<name>`) until `start` names it again. A load that
signals leaves nothing registered and the listing says `error` with the
message; `(ext:exit)` and `(ext:quit)` are refused. A handler that runs
300 s without answering is killed.

```lisp
(register-tool "weather" "..."
  (obj "type" "object" "properties" (obj "city" (obj "type" "string")))
  (lambda (args session-id)
    (declare (ignore session-id))
    (let ((body (json-parse (http-get (format nil "https://example.net/~a" (gethash "city" args))))))
      (format nil "it is ~a°" (gethash "temp" body)))))   ; a string, or JSON (a hash table) for the model
(register-command "hello" "a prompt" (lambda (text) (format nil "Say hello to ~a" text)))
(hook "post_llm_call"
  (lambda (args)
    (when (search "secret" (gethash "content" args ""))
      (obj "decision" "redact"))))                          ; NIL = no opinion
```

JSON comes in as EQUAL hash tables with string keys and vectors; true is
T, false is NIL, null is `:null`. A handler answers a string or anything
`json-string` writes: a hash table (`(obj "k" v ...)` makes one), a
vector or list, a number, T, `:false`; NIL is null, which a hook means
as "no opinion". Every string crosses between Lisp and the gateway as
ASCII JSON with `\u` escapes, so non-ASCII text arrives intact both
ways.

Handlers run in threads of their own (ECL's threads are real threads),
so a slow one blocks nothing else; a host call from a handler
(`http-get`, `tool-call`, …) waits on a condition variable while the
engine answers. `tool-call`, `call-llm`, `http-get`, `http-post`,
`config-get`, `config-set`, `note`, `send-outbound`, `principal`,
`sessions`, `history`, `usage-totals`, `limits`, `nodes`, `routes`,
`tasks`, `task-cancel`, `sandbox-where`, `provider-emit` and `json-get`
are the same operations `veles::*` gives Tcl, by the same table; `(call
op arg ...)` reaches any of them by name; a plugin offers a service with
`(register-service name description handler :cap cap)` and calls
another's with `(service-call name params)`
([Services](#services--one-plugin-using-anothers)); `(log-line text
[level])` writes under the plugin's owner; and `(spawn name function)`
starts a thread that belongs to the plugin and dies with its unload. A
thread made with `mp:process-run-function` directly is the plugin's own
to end. An error nothing handles in such a thread is logged and ends
that thread; it never reaches ECL's debugger.

What a runaway plugin costs: a recursion without end is a
`stack-overflow` condition in its own thread (threads get 32 MB stacks,
ECL checks against 8 MB), a float divided by zero is infinity (float
traps are off), and the whole Lisp heap is capped at 512 MB (past it,
an allocation signals a storage condition in the script). Plugins share
that heap; there is no per-plugin memory limit.

What the host is, so the `/plugins` listing reads right: ECL is
process-global, so the lang-lisp module stays mapped for the gateway's
life — a stop or unload of the MODULE waits out the host's 10 s
shutdown deadline on its engine thread and reports it `kept mapped`, a
`start` after a `stop` reuses the booted engine (its scripts load
again), and a replaced module file is a gateway restart (a second copy
refuses to boot ECL again). Plugins, by contrast, reload freely. The
engine boots in about 50 ms and costs about 40 MB of memory once up.
ECL's own signal handlers are off (a fault on a Rust thread stays a
Rust crash), the gateway's SIGTERM, SIGINT and SIGCHLD handlers are put
back after the boot, and ECL adds only its collector's SIGPWR/SIGXCPU
and its thread-interrupt signal (36). The default stack of threads
created without one is raised to 32 MB for the whole process; Rust's
threads name their own. It is not built for Android.

## Out-of-tree Rust

A `cdylib` linking `veles-plugin-abi-sdk` and nothing else of veles.
Linking veles-core would make it in-tree by definition — same crate
graph, same allocator, same version lock. The SDK is safe Rust over the
raw contract in `veles-plugin-abi` (ABI 4); a plugin in another
language implements that C contract directly.

```rust
use veles_plugin_abi_sdk::{veles_plugin, Host};

fn init(host: &Host) -> Result<(), String> {
    host.register_tool("hello", "says hello", r#"{"type":"object"}"#, |args| {
        Ok(format!("hello: {args}"))
    });
    Ok(())
}

veles_plugin!(init);
```

`veles_plugin!` emits the three exports — `veles_plugin_abi_version`,
`veles_plugin_init` and `veles_plugin_free_string` — and `Host` is a
`Copy` handle (the host table plus THIS plugin's id) you may keep in a
static and use from any thread, for as long as the plugin is loaded.

### What the host owns

Everything. Each `register_*`, `timer_after` and `tcp_connect` answers
a `Handle` (0 = refused; the journal says why). The host attributes
every registration to the plugin id, not to the thread that made it, so
a hook registered from inside a tool call or a timer callback is the
plugin's just the same. `/plugins unload <name>` (or `veles plugins
unload`, or the `/api/plugins/runtime/<name>/unload` route) purges
every tool, command, hook, filter, provider, platform route, timer and
socket the plugin owns, waits a bounded time for callbacks still
running its code, then unmaps the library — or reports "code kept
mapped: N callback(s) still running" and keeps it. `start` loads it
again; a loaded plugin's `reload` is an unload and a load from the file
as it is now, so a rebuilt `.so` goes live without a gateway restart —
once the old code was unmapped. A kept-mapped copy is what the same
path maps again (`dlopen` returns the mapping it already has), so a
rebuild that must replace a pinned copy ships under a new file name
(the manifest's `entry`) or waits for the callbacks to end.
`Host::unregister(handle)` drops one resource early. The closures behind
a registration are leaked on purpose (one small allocation each): the
host may drop a registration without telling the plugin, so nothing on
the plugin side can safely free them; a timer's closure is freed when it
fires.

### What a plugin can do (ABI 4)

| | SDK | note |
| --- | --- | --- |
| hooks, tools, commands, filters, providers | `register_hook` … `register_provider` | as before; sync handlers on the host's blocking threads |
| an async tool | `register_tool_async(name, desc, schema, Fn(&str) -> ToolFuture)` | the host polls the future natively through a `VelesWaker` bridge; no thread is parked. The future must not block: it waits on a timer, a socket event or a thread of the plugin's own, which wakes it |
| a chat platform | `register_platform(key, public_feed, Fn(outbound_json) -> Result)` | the host claims the key's outbound route and hands every outbound message to `deliver` as JSON; the plugin publishes traffic with `platform_inbound(inbound_json)`. A key another tier already serves is refused. `public_feed = true` makes the platform a public feed for the `/tcl` policy |
| timers | `timer_after(ms, FnOnce)` / `timer_cancel` | a cancelled timer never fires, even if it was due |
| sockets | `tcp_connect(host, port, tls, Fn(SocketEvent))`, `tcp_send`, `tcp_close` | the host's runtime and TLS stack (rustls, webpki roots); events — `Connected`, `Data`, `Closed`, `Error` — arrive one at a time, in order |
| the engine | `call(op, &[args])`, `note(summary, detail)` | the same `bridgeops::OP_NAMES` table Tcl has; HTTP stays an op |

### Three rules

1. **Refuse an ABI version you do not recognise — and the host does it
   for you.** The host reads `veles_plugin_abi_version()` before
   anything else and loads ABI 4 only. A plugin without that export is
   ABI 3 or older; it is refused by name with "rebuild against
   veles-plugin-abi 4.x", and no code of it runs.
   `crates/veles-plugin-badabi-so` is a plugin that claims ABI 3 and
   writes a file if its init ever runs, so the refusal path has a test
   that proves the file is never written.
2. **Whoever allocated it frees it.** Strings you return go back to your
   `veles_plugin_free_string`; strings `host_call` returns go back to
   `host_free_string`. The SDK does both; a raw-ABI plugin must.
3. **Never unwind across the boundary.** A Rust `extern "C"` function
   that unwinds ABORTS the process; the gateway's panic hook cannot
   catch it. The SDK wraps every export and every trampoline in
   `catch_unwind`, so a handler that panics answers an error (and files
   a note) and the plugin's other registrations keep working — the
   conformance suite's "a panicking tool answers an error and the host
   lives" is this. A raw-ABI plugin gets none of that: no `unwrap`, no
   `expect`, no indexing untrusted input.

Handlers are SYNC and run where blocking is allowed, so `call` works
inside any of them. From `init` it is not dangerous, just too early to
be useful: there is no tool registry yet, no gateway, and no session for
anything to be read out of. Registration is what init is for.

Your handler's payload carries `session_id`, and the engine publishes it
as the ambient identity for the duration of the call — so
`call("tool_call", &["name", "{}"])` from inside a handler is judged
as that session, with nothing extra to pass. The ambient is a
thread-local: from a timer, a socket callback or a thread of your own,
name the session yourself (`["tool_call", "name", "{}", "<session_id>"]`).

Worked example: `crates/veles-plugin-sample-so` — one of each surface,
a tool that calls back into the engine, an async tool woken by a timer,
a platform, timers, sockets, a tool that panics on purpose and one that
registers a hook late. `crates/veles-plugin-anthropic-so` is a real
provider driver on the SDK.

### The conformance suite

`crates/veles-core/tests/support/conformance.rs` runs the same scenarios
— loads and registers; unload purges everything; reload does not double;
a panicking tool answers an error and the host lives — against every
tier that holds tools, commands and hooks: the native tier in
`tests/plugin_so.rs` and the module tier through `veles
module-host-test conformance`. A new tier implements `Tier` (load,
unload, what its fixture registers) and gets the suite for free.

## Modules — the author guide

A module is a Rust `dylib` loaded into the gateway's process. It calls
core directly (same statics, same tokio, same tracing subscriber) and is
unloaded by the host, not by its own goodwill. IRC, slopdrop and
Telegram are modules; read `crates/veles-telegram/src/module.rs` for
the shortest complete one.

### The two crates

| crate | type | holds |
| --- | --- | --- |
| `crates/veles-<name>` | rlib | the code, its tests, the `module` submodule (`manifest()` + the `VelesPlugin` impl) |
| `crates/veles-module-<name>` | `crate-type = ["dylib"]`, one line of `src/lib.rs` | `veles_core::export!(veles_<name>::module::Module, veles_<name>::module::manifest());` |

The split is the R4 lesson (`dylib-form-links-dynamically`): a crate
that is BOTH an rlib and a dylib is linked dynamically by everything that
depends on it, so the gateway would map the module at startup and every
reload would run that old copy. The dylib crate exists to be dlopened and
nothing links it (`the_gateway_links_no_module` in
`crates/veles-cli/tests/module_load.rs` pins that).

Dependencies come through `veles_runtime` (`veles_runtime::tokio`,
`::tracing`, `::anyhow`, `::veles_core`, `::veles_types`), never a
second copy of tokio or tracing: the shared crates live once, in
`libveles_runtime.so`.

### The manifest

`veles_types::plugin::Manifest`, the same one process plugins send:

- `instances`: `Single`, or `PerEntry { section }` for one instance per
  `[[platforms.<p>.networks]]`-style entry (IRC);
- `lease`: the pool lease each instance holds (`singleton:telegram`,
  `singleton:irc:{instance}`); an instance without one on this node
  stands by (`standby`);
- `platforms`: the bus routes the module claims while on duty;
- `config_sections`, `secrets`: what it reads;
- `enabled_env` / `required_env` (R6): the per-node switch. The host
  reads them from VELES_HOME's `.env` only, BEFORE any lease: a false
  `enabled_env` or an unset `required_env` lists the module `off` with the
  reason and spawns nothing.

### start / stop

```rust
fn start(&self, cx: ModuleContext) -> BoxFuture<'_, anyhow::Result<()>>;
fn stop(&self, reason: &str) -> BoxFuture<'_, ()>;
```

`start` runs on the instance's own runtime (one worker, threads tagged
with the owner `module:<name>[:<instance>]`). Spawn long-running work
with `cx.handle.spawn` and return; everything spawned there is dropped
by the host's `shutdown_timeout` at teardown. Blocking code goes on
`cx.handle.spawn_blocking` (Telegram's dispatcher: `spawn_blocking` +
`Handle::block_on`). Never `std::thread::spawn`, never a runtime of your
own, never a ctrl-c handler: those outlive the instance and keep the
library mapped. The host pins the gateway's SIGTERM, SIGINT and SIGCHLD
handlers before the first module starts; a module that embeds a
runtime which installs handlers of its own puts back what it found
once that boot is done, as lang-racket does.

`stop(reason)` is a courtesy with a deadline (IRC sends QUIT, Telegram
ends its long poll through the dispatcher's `ShutdownToken`). Teardown
does not depend on it: after `stop`, the host purges every registration
the instance made, shuts the runtime down, drains calls in flight and
unmaps the library when nothing of the instance is left. A library kept
mapped is reported (`kept-mapped`, with the reason). A module's own
`tracing` callsites pin it (reported as `pinned`, no fault): expect that
when you log through `tracing::info!` in module code.

### Registering things

Call core's ordinary API from the instance thread; the owner is read
from the thread tag, so a purge removes exactly your entries:

- tools: `tools::register_global_tool` (sessions created after pick it
  up; a session holds a tool late-bound, so a reload is seamless);
- commands: `commands::register_command` (`CommandBody::Native` or
  `Direct` for a command that answers without a prompt);
- hooks: `rustplugins::register_plugin` / the `VelesPlugin::hooks`
  list;
- HTTP routes: `moduleroutes::register_routes` →
  `/api/plugins/<name>/<route>` (503 while not loaded);
- ACL: `acl::declare_tool_cap(tool, cap)`;
- secrets: `secrets::declare_platform_secret`;
- the bus: `cx.bus` (inbound/outbound), `bus::register_delivery_observer`
  for receipts;
- seams other modules fill: `tcleval::set_backend`,
  `tcleval::set_public_reply`, `triggers::set_forwarder`,
  `syncask::register_source`, `rooms::register_thread_room`,
  `ask::set_owner_fallback`.

Do it in `start`, every start — a reload purges and starts again, so a
`Once` guard would leave the second start with nothing registered.

### Layers (R7)

Four core subsystems are owner-tracked stacks a module can replace or
wrap (`veles_core::layers`): the turn runner, the system prompt builder,
tool dispatch and inbound routing. A layer gets the input and a `next`;
calling `next` runs what was installed below (core's default at the
bottom), not calling it replaces the subsystem. Installs stack in order;
a purge (reload, stop) removes the module's layer and the previous
implementation is in effect again.

```rust
struct Shout;
impl veles_core::layers::PromptLayer for Shout {
    fn build(&self, cx: &PromptCx<'_>, next: NextPrompt<'_>) -> String {
        let base = next.build(cx);          // wrap: core's prompt…
        format!("{base}\n\nANSWER IN CAPITALS.") // …plus a line
    }
}
veles_core::layers::install_prompt(Arc::new(Shout));   // in start()
```

### State

`<data_dir>/module-state/<name>/` is the module's own directory
(`veles backup` carries it). Settle it in `start`; slopdrop's
`data::migrate_once` is the pattern for moving an older layout in with a
backup left behind.

### Building, installing, reloading

The host refuses a module whose build key differs from the gateway's
(`core changed: rebuild the module with this core, or restart the
gateway on the new core`). The key covers the shared crates' sources,
`Cargo.lock`, the toolchain and RUSTFLAGS, so:

- **the dev gateway** (`veles-gateway-dev`, runs `target/debug/veles`):
  `cargo build --workspace --exclude veles-runtime`, then
  `scripts/install-module-dev.sh <name>`, then restart the gateway (a
  module built in a separate cargo invocation links a different runtime
  variant and is refused);
- **the VPS**: `scripts/push-plugin.sh <name>` builds the nix package
  the image carries, installs content-addressed under
  `<data>/modules/<name>/<sha256>.so` + `current`, and reloads — no
  restart, the PID stays. With `--no-reload` for a deploy that changes
  core: push the module first, then the image, then restart;
- **the APK**: the engine's cargo invocation builds the modules; they
  ship as `lib/<abi>/libveles_module_<name>.so` and `Engine.java` writes
  `files/modules/<name>/plugin.toml` per start;
- **Windows**: the file is `veles_module_<name>.dll`, installed as
  `%LOCALAPPDATA%\veles\modules\<name>\<sha256>.dll` with `current` a
  one-line **pointer file** naming the hash (no symlink) — the host reads
  both forms on every platform. `veles plugins install-module <name>
  <built file>` is the portable installer (`scripts/install/windows.ps1`
  uses it; it works on Linux too; `<name>` must be the name the module
  declares, which the host checks against the manifest at load). The key section is `.vmodkey` there
  (a PE section name is at most 8 bytes), the runtime
  `veles_runtime.dll`; `module::inspect` reads PE imports and exports
  where it reads ELF dynamic symbols. Racket and Lisp modules are not
  built on Windows yet;
- **your own repository**: an overlay module. `veles plugins new <name>
  --kind overlay` writes the crate and a flake whose
  `velesWithModules` builds veles with the module in it, so the
  result's gateway and modules share one key (extending.md, "Rust
  overlay modules"; not part of the stable API).

`veles plugins status | reload | stop | start | unload <name>` and
`/plugins …` in chat act on a module by name (or `name:instance`).

### Testing

Unit tests live in the rlib. The host test
(`veles module-host-test <case>`, `crates/veles-cli/src/modulehosttest.rs`)
loads the REAL dylib into a scratch gateway: a cargo test binary links
core statically and cannot load modules, so the cargo test in
`crates/veles-cli/tests/module_host.rs` runs the case as a child
process. Prove: the registrations appear on start, vanish on stop, the
route is claimed, a reload re-claims, a stop ends inside its deadline,
and the library is unmapped (or pinned only by your callsites).

---

## Process plugins

A child process the gateway spawns, supervises and talks JSON-RPC 2.0
to, one message per line on its stdin/stdout. Its stderr goes to the
gateway's log (target `veles_core::plugin`, at the level the plugin
logged it), and the last 200 lines are kept for `veles plugins logs`. A
crash or a rebuild restarts that process and nothing else — the
gateway, its turns and the other platforms keep running.

This is the tier for a plugin that needs isolation, or is written in
any language, and only needs what the protocol carries: the atproto
(Bluesky) platform runs here. It is NOT the tier for a plugin that has
to register real functionality inside core — IRC needs about sixty
core items, and its ask/ACL verbs cannot cross a process boundary into
a dispatcher that holds the chat lock. Those become in-process
**modules** on a shared core library, unloaded and reloaded by the host. An earlier
version of this page said a `.so` "cannot be safely unloaded"; that was
wrong — it can, when the host owns its runtime and its registrations,
which is what the module tier is for.

Write one with `crates/veles-plugin-sdk`. It links `veles-types` only,
never veles-core — so a plugin cannot read the gateway's per-process
state by accident (in a second process that state would silently be
empty), and builds as a static binary. The worked example is
`crates/veles-plugin-echo`, a toy platform whose integration tests run
the real host against the real binary.

```rust
use veles_plugin_sdk::*;

struct Mine;

#[async_trait]
impl Plugin for Mine {
    async fn start(&self, host: Host, init: InitParams) -> Result<(), String> {
        // init.config holds the sections the manifest declared;
        // init.dirs.state is this instance's own directory.
        // Spawn the connection on a task of your own and return.
        Ok(())
    }
    async fn deliver(&self, msg: OutboundMessage) -> Result<Receipt, String> {
        // send msg.text to msg.chat_id on the platform
        Ok(Receipt { ids: vec![] })
    }
}

fn main() -> std::process::ExitCode {
    run(manifest(), Mine)
}
```

### The manifest

`veles-plugin-X --describe` prints it with no network and no gateway;
the gateway reads it before it spawns anything.

| field | what |
| --- | --- |
| `name`, `version`, `protocol` | `protocol` must equal the gateway's (1), or the plugin is refused by name |
| `instances` | `single`, or `per_entry` over a config array (`platforms.echo.rooms`, keyed by `name`): one process per entry |
| `lease` | a cluster singleton key, `singleton:...`, `{instance}` filled in: the process runs only on the node holding it |
| `platforms` | the platform keys it speaks for — the only ones it may deliver or vouch for |
| `config_sections` | the dotted config sections it is handed at `initialize` |
| `secrets` | the secret names it may ask for |
| `services` | the services it provides while it runs (see [Services](#services--one-plugin-using-anothers)), answered by its `request` handler under the service's name |
| `uses` | the services it calls (`name`, `optional`, `description`): the dependency view |

Installed is a directory with a `plugin.toml` — `name`, `kind =
"process"`, `entry` (the executable, relative or absolute) — under the
first of: `$VELES_PLUGIN_PATH` (colon-separated roots),
`<data_dir>/process-plugins/`, `<prefix>/lib/veles/plugins/`. **Not**
`~/.config/veles/plugins`: that directory is the Tcl plugins' git repo,
snapshotted on every change. `[plugins] enabled`/`disabled` apply as for
the other tiers.

### What the gateway decides, not the plugin

- **Who a sender is.** A plugin may set `sender_id` only for a platform
  it declares; anything else arrives unverified. The message's route is
  the instance it came from and `sender_ctx.platform` is the message's
  platform, whatever the plugin wrote. `acl.resolve` answers from the
  gateway's live ACL, for the plugin's own platforms only.
- **Secrets.** `secret.get` answers only names the manifest declares.
  The process starts with an EMPTY environment plus a short allowlist
  (`pluginhost::CHILD_ENV`) — the gateway's own environment carries
  every provider key.
- **Lifecycle.** A process that ends is restarted with doubling back-off
  (reset after it has run a while). A reload asks it to `shutdown`,
  waits, then kills. Outbound messages queue per (platform, instance)
  while it is down — in order, at most 1000, at most 10 minutes old —
  and go out when it answers again. A delivery that was in flight when
  it died is not retried (no duplicate posts).
- **Updates.** Every start runs the binary installed NOW, and asks it
  `--describe` first, so a reload after an update takes the new
  version's config sections and secrets on its first run. A version
  that changes its lease or its platforms is not run under the old
  ones: it fails with that reason, and a gateway restart takes it.
- **Leases.** Under a cluster arbiter each instance runs only while
  this node holds its lease, and its platform is on this node's bus
  only then: a standby node does not claim it.
- **Exit.** When the gateway stops, every plugin gets `shutdown`
  (`reason: "the gateway is stopping"`). When the gateway dies without
  a word, the plugin's stdin closes and the SDK shuts it down itself,
  within `SHUTDOWN_LIMIT` — no orphan keeps a connection open.

Every start, crash, reload and stop parks a plugin note.

### Protocol v1

| direction | method | |
| --- | --- | --- |
| gateway → plugin | `initialize` | `InitParams` → the manifest |
| gateway → plugin | `outbound.deliver` | an `OutboundMessage` → `Receipt { ids }` |
| gateway → plugin | `shutdown` | `{ reason }` |
| plugin → gateway | `bus.inbound` (notification) | an `InboundMessage` |
| plugin → gateway | `note` | `{ summary, detail }` |
| plugin → gateway | `secret.get` | `{ name }` → `{ value }` |
| plugin → gateway | `acl.resolve` | `{ sender_id, sender_ctx }` → the principal |

Any other method either way is the plugin's own (`echo.say`), reachable
from the gateway with `PluginHost::call`. The types are
`veles_types::plugin`; the framing is `veles_types::jsonrpc`.

### Operating them

| | |
| --- | --- |
| `veles plugins status` | every instance: state, pid, version, restarts, last error, lease |
| `veles plugins reload <name>[:<instance>]` | restart it (waits for the answer) |
| `veles plugins stop` / `start <target>` | stop until started; messages wait meanwhile |
| `veles plugins logs <target>` | the last lines it wrote |
| `/plugins` | the same listing in any chat; `/plugins reload\|stop\|start <target>` is admin, filed and answered at once |

The CLI talks to the running gateway's desktop API
(`/api/process-plugins`), with the same token as `veles tcl`.

---

## Services — one plugin using another's

A plugin can offer a feature to other plugins by NAME, and use another
plugin's feature without knowing which plugin provides it. The IRC
adapter's trigger rows are the first users: a Slopdrop row calls
`triggers.forward` (the slopdrop module provides it), a Bluesky row calls
`appview.query` (the atproto plugin provides it, reading as the bot).
Neither side names the other; the registry (`veles_core::services`)
connects them, and either can be replaced by any plugin that provides the
same name.

A **service** is a dotted lowercase name (`group.verb`), JSON params and
a JSON answer. Its contract is the provider's to document: write it down
where its users will look (the shared ones are typed in
`veles_types::plugin`: `TriggersForwardParams`, `AppviewQueryParams`, …).

| tier | provides | calls | the layer below (`next`) |
| --- | --- | --- | --- |
| module | `veles_core::services::register(decl, handler)` in `start` (purged on unload) | `veles_core::services::call(name, params).await` | `cx.next(params).await` |
| process plugin | its manifest `services`, answered by its `request` handler while it runs | `host.call_service(name, params, session)` (the `service.call` request) | an override point's request is `{input, next, caller, session_id}`: `host.service_next(next, input)` (the `service.next` request) |
| Tcl | `veles::register_service <name> <proc> ?description? ?cap?`; the proc reads `::veles::service_request` (`{params, caller, session_id, next}` — read it first) and returns JSON or text | `veles::service_call <name> <params_json> ?session_id?` | `veles::service_next [veles::json_get $::veles::service_request next] $params_json` |
| JavaScript | `veles.registerService({name, description, cap}, (params, {caller, sessionId, next}) => answer)` | `await veles.serviceCall(name, params, sessionId?)` | `await next(params)` |
| Racket | `(register-service name description handler #:cap cap)`; `(handler params caller session-id)` answers a string or a jsexpr | `(service-call name params [session-id])`, a jsexpr; `(services)` lists them | `(service-next params)` inside the handler |
| Common Lisp | `(register-service name description handler :cap cap)`; `(handler params caller session-id)` answers a string or JSON | `(service-call name params &optional session-id)`, read as JSON; `(services)` lists them | `(service-next params)` inside the handler |
| `.so` (ABI 4) | `host.register_service(name, description, cap, f)` (SDK), `register_json` with kind `service` (raw); `f` gets `{params, caller, session_id, next}` | `host_call("service_call", …)` | `host.call("service_next", &[next, params_json])` |

**Who may call** is the provider's decision, per service (`cap`):

- unset — **internal**: core and in-process modules only. They are
  compiled into the gateway and share its trust. A process plugin or a
  script is refused, by name. The bot's write services (`post.create`,
  `repo.delete_record`, `hosted.delete`) and `triggers.forward` are
  internal: no third-party code posts or deletes as the bot.
- `any` — every caller (`appview.query`, a read).
- an ACL capability (`web`, `shell`, …) — a script's or process plugin's
  call is judged as the session it is made for (the one it names, else
  the hook's), like `veles::tool_call`: that session's principal must
  hold the capability. No session, no call.

Every call from a script or process plugin is announced on the
transcript, like a `tool_call`.

**Two providers of one name:** the later registration answers; when it
goes (unload, reload, a process plugin exiting) the earlier one answers
again. That is how a plugin overrides another's service, and how a
script can stand in for a module's.

**Wrapping: `next`.** A provider can also ask what the layer below it
would answer, and change that instead of replacing it: every call hands
the provider a `next` token (the column above says where each tier finds
it), and `service_next` with that token runs the provider underneath. A
token is issued for one call and dies with it, so it can be neither
guessed nor kept; the layer below runs for the same caller and session.

**Override points** are services core (or a module) declares with its
OWN answer at the bottom of the stack (`services::declare_point`): a
decision veles asks by name, which any plugin can replace or wrap
without touching this tree. A point's answer is held to its output
schema, and an override that errors, times out or answers outside it is
skipped (the layer below answers, and a plugin note says why) unless the
point says `fail`. `/plugins overrides`, `veles plugins overrides` and
`GET /api/plugins/overrides` list each point and who answers it, top
first. Which points exist, and what each decides, is in
[extending.md](extending.md).

**Dependencies are declared.** A module or process plugin lists what it
provides in `services` and what it calls in `uses` (`optional = true`
when it works without it); a script says the same in `plugin.toml`:

```toml
provides_services = ["weather.now"]
uses_services     = ["appview.query"]     # needs it
optional_services = ["triggers.forward"]  # works without it
```

`/plugins services`, `veles plugins services` and
`GET /api/plugins/services` join those declarations with what is
registered right now: who answers each service (and whom it covers), who
declares providing it, who uses it, and which required use has no
provider here. A call that finds nothing names the plugin that declares
the service ("declared by atproto: not running here, or not started
yet") — the usual answer on a node that does not hold that plugin's
lease.

The services in the tree today:

| service | provider | access | what |
| --- | --- | --- | --- |
| `triggers.forward` | slopdrop module (`[slopdrop]` enabled) | internal | one channel event into slopdrop's trigger engine; the replies |
| `triggers.masks` | slopdrop module | internal | which events each channel still has a live binding for |
| `appview.query` | atproto plugin | any | one read-only `app.bsky.*.get*` query as the bot |
| `repo.get_record` | atproto plugin | any | one public record by AT URI, raw, with no viewer (what a block hides reads); `found: false` when deleted |
| `post.create` | atproto plugin | internal | post text as the bot |
| `repo.delete_record` | atproto plugin | internal | delete one record of the bot's repo |
| `hosted.delete` | atproto plugin | internal | take a hosted copy down |

## Writing a provider driver

The biggest thing a plugin can do: teach veles a wire it does not speak.

```toml
[[custom_providers]]
name     = "my-anthropic"
api_mode = "plugin"
driver   = "anthropic-messages"
base_url = "https://api.anthropic.com"
key_env  = "ANTHROPIC_API_KEY"
```

That entry works on a fresh install: `anthropic-messages` is registered
by `tcl-anthropic-driver`, a [bundled plugin](#bundled-plugins), and it
is exactly what `veles setup`'s Anthropic choice writes (as `name =
"anthropic"`). The wizard wrote Anthropic's OpenAI-compatible layer
(`api_mode = "chat_completions"` at `https://api.anthropic.com/v1`)
before 0.1.2; Anthropic documents that layer as having no prompt
caching, and it ignores `reasoning_effort` and returns no thinking. An
entry of that older shape keeps working, and a wizard re-run recognises
it as Anthropic and keeps its URL and wire as they are; to move it to
the native wire, change its `base_url` and `api_mode` and add the
`driver` line as above.

A driver **translates**; it does not assemble and it does not parse. It
receives the OpenAI-shaped request body the engine already built —
attachment parts, cache breakpoints, tool serialization,
`reasoning_effort`, `extra_body` all applied — and answers an
OpenAI-shaped response object:

```json
{ "choices": [ { "message": { "role": "assistant", "content": "…" } } ],
  "usage":   { "prompt_tokens": 1, "completion_tokens": 2 } }
```

The engine parses that with the same code a native route uses, so
tool-call reassembly, the reasoning/content split and all four spellings
of prompt-cache accounting come for free. To fail a turn, answer
`{"error": "…", "status": 502}` — the message reaches the user instead
of an empty assistant reply.

Two things the engine hands you that are not part of the wire:

- **`veles` (reserved key on the request)** — `base_url`, `api_key`,
  `provider` and `driver` for THIS route, plus the endpoint knobs below.
  Read it; do not forward it. It exists so one driver can serve several
  provider entries with different credentials and different gateway shapes,
  which a global config lookup could never do.
- **Streaming** — call `provider_emit` with an OpenAI-shaped
  `chat.completion.chunk` as many times as you like before returning.
  It is an op, not a callback, which is why the Tcl and C sides have the
  same shape.

Your driver owns its transport, but it does not have to write one:
`http_get` / `http_post` are the engine's client, with its timeout and
its 1 MB cap, and both take a headers object for the endpoints that are
not bearer-auth.

`http_post` BUFFERS, and that is a ceiling as much as a convenience: a
driver built on it cannot show incremental output at all, and its total
timeout is 30 seconds unless the call passes `?timeout_secs?` (up to 900),
which a non-streaming call must: a long answer on a frontier model takes
longer than that.
For a wire that streams, use the three-op transport instead:

```tcl
set r [veles::http_stream_open $url $body application/json $headers]
# {"id":7,"status":200} — the STATUS arrives before any body, which is
# what lets you classify 429/5xx and let the engine fail over
while 1 {
    set e [veles::http_stream_next 7]
    if {[veles::json_get $e done] ne ""}  { break }   ;# clean end
    if {[veles::json_get $e error] ne ""} { break }   ;# transport died
    set data [veles::json_get $e event]               ;# one data: payload
}
veles::http_stream_close 7
```

Both terminal answers free the handle, so returning early on an error leaks
nothing, and `close` is idempotent either way. A non-2xx status still
delivers its body — as one event, then `done` — so the error path and the
success path are the same loop.

### Endpoint knobs, for the compatible gateways

One driver can serve a provider's own API and every proxy that speaks its
wire, because the parts that differ per gateway are typed fields on
`[[custom_providers]]` and arrive on the `veles` key:

| field | means | driver default |
| --- | --- | --- |
| `endpoint_path` | where to POST, relative to `base_url` | the wire's own (`/v1/messages`) |
| `auth_style` | `"x-api-key"` or `"bearer"` | `"x-api-key"` |
| `api_version` | the value of the wire-version header | the driver's pinned version |
| `max_output_tokens` | the output cap to send | discovered, else the driver's fallback |
| `cache_ttl` | `"5m"` or `"1h"`: how long the endpoint keeps a cached prefix | the driver's (the Anthropic one: `"1h"`) |

The engine also sends `timeout_secs` on the same key: the total time a
NON-streaming call may take (600, what its own provider client allows).
Pass it to `http_post` as the fifth argument. A non-streaming answer arrives
only once the model has written all of it, and a compaction summary takes
longer than `http_post`'s 30 second default.

An unknown `auth_style` is refused at config load rather than silently
defaulted, because the failure it prevents reads as a rejected API key: the
driver would authenticate with the wrong header and the operator would go
rotate a credential that was fine.

**Two worked examples, deliberately the same driver twice:**

- `plugins/examples/tcl-anthropic-driver/` (Tcl, ~1200 lines)
- `crates/veles-plugin-anthropic-so/` (out-of-tree cdylib)

The Anthropic one is the fullest driver in the tree, and worth reading before
writing your own: it reads its endpoint's own capability catalog rather than
carrying a table of model families, streams over the transport above, and
translates attachments, prompt-cache breakpoints, thinking, tool calls and
server tools both ways.

`crates/veles-core/tests/driver_parity.rs` feeds one canned Anthropic
response to both and asserts the two `Completion` values are **equal**, and
then does the same for each feature — an attachment reaching the wire as an
image block, the capability catalog deciding the thinking shape, a streamed
answer matching the same answer unstreamed, a 429 becoming a route that fails
over. That is the test behind the claim at the top of this page.

Prove yours the same way the engine does:

```
$ veles doctor --providers
registered drivers (1):
  anthropic-messages       from tcl:anthropic::drive
route chain (1):
  my-anthropic   claude-x   plugin:anthropic-messages (from tcl:anthropic::drive)
asking each route for one word (stops at the first answer):
  my-anthropic   OK ("ok")
```

That command boots the Tcl runtime (which is what loads plugins, which
is what registers drivers), so it is the only surface that can tell you
whether a `driver = "..."` line names something real — config validation
runs before plugins load and cannot check it.

---

## Visibility

Plugin automation announces itself. `veles::note` parks a plugin-action
note on the transcript, and the engine parks one for you when a plugin
sends gateway outbound, registers a tool, registers a command or
registers a driver. This is not optional politeness: a mechanism acting
without the user asking must be visible, or the agent is doing things
nobody can account for.

## Where things live

| path | what |
| --- | --- |
| `~/.config/veles/plugins/` | your plugins (a git repo) |
| `<prefix>/share/veles/plugins/`, `$VELES_BUNDLED_PLUGINS` | the bundled Tcl plugins, read-only; yours shadow them by name |
| `plugins/veles_api.tcl` | the signature reference, copied there at boot |
| `crates/veles-plugin-abi/` | the C ABI (version 4), what a non-Rust `.so` implements |
| `crates/veles-plugin-abi-sdk/` | the safe Rust SDK over it, what a Rust `.so` links; catches panics at the boundary |
| `crates/veles-core/src/soservices.rs` | the native tier's host-owned resources: handles, platforms on the bus, timers, sockets |
| `crates/veles-core/tests/support/conformance.rs` | the tier conformance suite (native + module) |
| `crates/veles-core/src/bridgeops.rs` | the op table both languages share |
| `crates/veles-core/src/rustplugins.rs` | the `.so` loader (version check, refusal, unload) and the host function table |
| `crates/veles-core/src/pluginhost.rs` | the process-plugin host |
| `crates/veles-core/src/modulehost.rs` | the module host: discovery, the build key, per-instance runtimes, teardown |
| `crates/veles-core/src/layers.rs` | the layer slots: turn runner, prompt builder, tool dispatch, inbound routing |
| `crates/veles-core/src/owned.rs` | owner-tracked registries and slots; what a purge removes |
| `crates/veles-plugin-sdk/` | what a process plugin links |
| `crates/veles-types/src/plugin.rs` | protocol v1 |
| `<data_dir>/plugin-state/<name>/<instance>/` | a process plugin's own data; `veles backup` carries it |
| `<data_dir>/module-state/<name>/` | a module's own data; `veles backup` carries it |
| `<data_dir>/modules/<name>/` | installed module code (content-addressed `.so`/`.dll` + `current`, a symlink or a pointer file); not in a backup, the installer puts it back |

# Extension API changes

Newest first. Each heading is `## <version> sha256:<the first 16 hex
digits of docs/extension-api.json's sha256>`, and the gate checks the
newest one against the spec (`crates/veles-cli/tests/ext_api.rs`).
`veles plugins api --stamp` prints the heading for the spec as it is.
Every entry says what changed and what a plugin author has to do. See
[extending.md](extending.md), "How a change to the API is published".

## 0.9 sha256:e2e0cf1484ec608f (2026-10-26)

A plugin can declare the configuration options an installer renders.

Added:
- `knobs` in the process and module manifest. Each entry carries a
  dotted `path`, an optional TOML-literal `default`, a `shape`, an
  optional `note`, and an optional `env` override name. The existing
  `Knob` type and constructors are shared with the process-plugin SDK.

The installer writes only options declared by the selected manifest and
permitted by its `config_sections`. Secret values stay in the setup/vault
flow; a credential-shaped knob is not a generic component answer.

What a plugin author has to do: nothing. Older manifests without `knobs`
deserialize with an empty list and have no generic installer form. To
offer options, declare them in the manifest and return the same manifest
from `--describe` and `initialize`.

## 0.8 sha256:cbc74970eb98dc04 (2026-10-21)

A plugin can ask the atproto plugin which account it acts as.

Added:
- `repo.whoami` — a service name the atproto plugin provides (gateway →
  plugin, `{}` → `{did, handle}`). `did` is the SESSION's DID, checked
  against the configured one when the adapter starts, so it is the one
  authoritative answer to "whose repo is this"; `handle` is for display.
  The `/atproto delete` command reads it before it trusts a record's
  ownership (crates/veles-atproto-cmd), so a delete can never be aimed at
  a repo the bot does not own.

Why a service and not a config read: the configured DID is what the
operator typed, the session DID is what the PDS signed in; the two can
disagree after a handle move or a mistyped config, and only the second
proves anything. A caller about to delete must read the proof, not the
intent.

What a plugin author has to do: nothing. A plugin that calls
`repo.delete_record` or `hosted.delete` should call `repo.whoami` first
and compare the record's repo to `did`; the service answers from the
plugin's own session, no network round trip.

## 0.7 sha256:0b69a9b571a1044c (2026-10-16)

A provider driver can stream, which it could not.

Added:
- `http_stream_open`, `http_stream_next`, `http_stream_close` — a
  server-sent-event transport a driver PULLS from, in all four script
  languages (`veles::http_stream_open` / `httpStreamOpen` /
  `http-stream-open`). `open` takes the same optional content-type and
  headers arguments `http_post` does, and answers
  `{"id":N,"status":200}` as soon as the response STATUS is known.
  `next` answers `{"event":"…"}` per event, `{"done":true}` at a clean
  end, or `{"error":"…"}` if the transport died. `close` frees the
  handle and is idempotent.

Why three ops and not one: `http_post` buffers a whole body and carries a
30 second TOTAL timeout, so a driver built on it can neither show
incremental output nor survive a long turn on a frontier model. And why a
pull loop rather than a callback: the socket tier's callback shape cannot
be shared by both plugin languages, because a Tcl callback is a proc name
and has no meaning across the C ABI (see `TCL_ONLY_COMMANDS`). A pull is
one op call in either, the same answer `service_next` gave for service
layers.

A driver does not have to use these — `http_post` still works, and a
non-streaming driver that emits its finished text once through
`provider_emit` is still honest about what it does.

What a plugin author has to do: nothing. A driver that wants to stream
reads the status from `open` (so 429 and 5xx still reach the engine as
what they are, and the route can fail over), then pulls events until
`done` or `error`. Both terminal answers free the handle, so returning
early on an error leaks nothing.

## 0.6 sha256:fafd5cc24fd45747 (2026-10-12)

A phone's events reach plugins (docs/phone.md "Reacting to the phone").

Added:
- the hook `device_event`, fired by the desktop API's
  `POST /api/device/event` for each event the phone's gateway service
  sends. Its arguments: `device` (the phone's name), `kind`
  (`notification` or `power`), `source` (a notification's app package;
  `connected` or `disconnected` for power), `app`, `title`, `text`, `at`
  (epoch milliseconds), and `session_id`, which is `device:<device>`.
  The reply is ignored.
- That session has a User principal and no tool registry, so a
  `veles::tool_call` from the handler is refused. A notification's text
  is other people's words: act on it with the plugin's own code.

What a plugin author has to do: nothing. A plugin that wants a phone's
notifications registers for `device_event`. While the phone's engine is
stopped, an event reaches it only when a trigger listens to that app.

## 0.5 sha256:c660c355a440929a (2026-10-11)

The bundled platform modules declare points of their own.

Added:
- `irc.addressed` (the IRC module) and `telegram.addressed` (the
  Telegram module): whether an IRC channel line or a Telegram group
  message is for the bot, and what the bot reads of it. Core's verdict
  (the mention gate, IRC's bare `tcl` convention) is handed in as
  `core_address`; answer `{address, text}`. Everything after the point
  still applies (authorization, the ACL, lockdown), and an IRC command in
  a channel still needs a real mention.
- the manifest key `points` (`PointDecl`: `name`, `summary`, `input`,
  `output`, `on_error` = `fall_through` or `fail`, `deadline_ms`): the
  points a module or process plugin declares. The published spec reads
  the in-tree modules' points from their manifests.
- a point declared once per instance (one per IRC network) counts its
  declarers' defaults as defaults, not as overrides.

What a plugin author has to do: nothing. A process plugin's `points` are
read, not yet declared with the plugin's own answer at the bottom.

## 0.4 sha256:b2bef3f29187a2db (2026-10-11)

Three of core's subsystems become override points:
what the R7 layers give a Rust module, now
for every tier. Each answer sits above any module layer.

Added to `override_points`:
- `prompt.build`: the system prompt a session starts with. Input
  `{session_id, principal, disabled_skills, prompt}` (`prompt` is what
  core and the module layers built); answer `{prompt}`.
- `tool.dispatch`: every tool call. Input `{session_id, name, args,
  call_id}`; answer `{result}`. Core's default (what `next` reaches) runs
  the call through the session's gated dispatch; a wrapper passes the
  input on whole, its name and args changed or not.
- `inbound.route`: every inbound message before the ACL. Input the
  message; answer `{action: "route", message}` or `{action: "drop"}`. A
  `message` whose `platform`, `chat_id`, `sender`, `sender_id` or `route`
  differs from what came in is refused and the original is routed.

The turn runner stays a Rust module's layer only.

What a plugin author has to do: nothing.

## 0.3 sha256:c22a9aa732727a45 (2026-10-11)

The first core override point, `failover.decide`:
what happens after a call to a model route failed.

Added:
- `failover.decide` in `override_points`, with its input (`session_id`,
  `interactive`, `attempt`, `hops`, `route`, `error` with `kind` and
  `status`, `core_would_fall_back`, `remaining_routes`, `config`) and its
  answers (`advance`, `retry`, `wait` with `seconds`, `goto` with
  `provider` and `model`, `stop`, `ask`, `policy`). Core's own answer is
  `policy`: the configured `[[failover]]` rules and `/failover`, exactly
  as before. An override owns the hops it decides (it fails over with no
  rule at all), is asked about every failure, and one chain makes at
  most 32 calls whatever it answers. The non-streaming chain asks the
  same point.
- `examples/plugins/failover-backoff/`: a TypeScript override (wait and
  retry on a rate limit, then move on; advance on a server error; the
  rest to `next`).

What a plugin author has to do: nothing. A plugin that registered a
service named `failover.decide` before (none is known) now answers the
decision.

## 0.2 sha256:67a519b222282420 (2026-10-11)

Override points and `next`.

Added:
- the op `service_next` (Tcl `veles::service_next token params_json`,
  JS `veles.serviceNext`, Racket and Common Lisp `service-next`): what
  the layer below a provider answers. Every service call now carries a
  `next` token: in a script's payload (`::veles::service_request`'s
  `next`; JS's second handler argument has `next(params)`; Racket and
  Lisp read it implicitly inside the handler), in a `.so` service's
  input, and in a process plugin's envelope for an override point;
- the process protocol request `service.next` (`{token, params}`), and an
  override point's request envelope `{input, next, caller, session_id}`
  (plain services keep their bare params);
- C ABI 4: `struct_size` right after `abi_version` (the table only grows
  at its end from here; check it before using a newer field),
  `register_json` (kind `JSON_KIND_SERVICE` = `"service"`: a `.so` can
  PROVIDE a service, and so override a point), and `VelesJsonFn`;
- override points themselves: `services::declare_point` in core, the
  fall-through on a failed override, `/plugins overrides`,
  `veles plugins overrides`, `GET /api/plugins/overrides`. No core point
  is declared yet; `failover.decide` comes with 0.3.

Removed: the process protocol names `irc.send`, `irc.op`, `irc.typing`,
`irc_tap.publish`, `irc.history` and `session.principal`. They were
declared for the abandoned IRC process plugin and nothing ever answered
them.

What a plugin author has to do: a `.so` must be rebuilt against this
`veles-plugin-abi` (`struct_size` moved every later field of the host
table). Scripts and process plugins: nothing; a service handler that
ignores `next` behaves as before.

## 0.1 sha256:dbabea248cc04463 (2026-10-11)

The first published spec: the API exactly as it stood on 2026-10-11,
nothing added or removed. It covers the 22 bridge operations and their
bindings in Tcl, JavaScript, Racket and Common Lisp, the script
registrars, the fired and reserved hook names, the `plugin.toml` keys and
kinds, the process and module manifest, process protocol v1's method and
shared service names, and C ABI 4's header (symbols, constants, type
aliases and `#[repr(C)]` structs, in field order).

New in this version: the `api` key in `plugin.toml`, an optional
statement of the version a plugin was written against.

What a plugin author has to do: nothing.

# Extending and overriding veles without forking it

<!-- extension-api: 0.9 sha256:e2e0cf1484ec608f -->

You change how veles behaves from your own plugin directory, not from a
patch to this repository. An update replaces the program and leaves
`~/.config/veles/plugins/` alone, so a change you made there survives the
update without a re-merge. This page says what a plugin may rely on
across updates (the **extension API**), how that API is versioned, how a
change to it is published, and every name in it.

The plugin author's guide, with the walkthroughs per language, is
[plugins.md](plugins.md).

## Which tier, and what it promises

| tier | where your code lives | stable across updates? |
| --- | --- | --- |
| Tcl script | `~/.config/veles/plugins/<name>/plugin.tcl` | yes: the extension API |
| JavaScript / TypeScript, Racket, Common Lisp script | the same directory, `kind = "js"`, `"racket"` or `"lisp"` | yes: the extension API |
| out-of-tree Rust (`kind = "rust"`, a cdylib on C ABI 4) | the same directory | yes: the extension API (the ABI section below) |
| process plugin (`kind = "process"`, JSON-RPC protocol v1) | `<data>/process-plugins/<name>/` | yes: the extension API (the protocol section below) |
| module (`kind = "module"`, a Rust dylib on `libveles_runtime.so`) | built in the same cargo invocation as the gateway | **no**: it calls veles' internals directly and loads only into the build it came from |
| overlay module (a module from your own repository) | your repository; veles' flake builds it into a veles of your own | **no**: it is a module ("Rust overlay modules" below) |

A module can replace whole subsystems (the layer slots in plugins.md,
"Layers (R7)"). Its source can live in your own repository: veles' flake
builds it into a veles package of your own ("Rust overlay modules"
below). It still calls veles' internals, so its Rust API changes
whenever core does, and an update can break its build.

## What you can override today (API 0.7)

- **A tool.** A plugin tool registered under a built-in tool's name
  replaces it in every session created afterwards; unloading the plugin
  brings the built-in back. The plugin's tool is listed in the deferred
  tier unless `[tools.overrides]` promotes it.
- **A slash command, partly.** A plugin command wins over a same-named
  `.md` command. Built-in slash commands are answered first and cannot be
  replaced.
- **A provider's wire.** Any `[[custom_providers]]` entry can speak
  through a plugin's driver: `api_mode = "plugin"` and
  `driver = "<name>"` (Tcl `register_provider`, ABI `register_provider`).
- **A service.** When two plugins provide one service name, the later
  registration answers until it unloads, and then the earlier one does
  again. So a script can replace a feature another plugin offers
  (`appview.query`, `triggers.forward`, …). Since 0.2 it can also WRAP
  it: every call hands the provider a `next` token, and `service_next`
  with it answers what the layer below would (plugins.md, "Services",
  has each tier's spelling).
- **The failover decision.** Since 0.3 a plugin overrides
  `failover.decide`: retry, wait, move to any configured route, stop or
  ask, per failure, with or without `[[failover]]` rules ("Override
  points" below).
- **The system prompt, tool dispatch and inbound routing.** Since 0.4,
  `prompt.build`, `tool.dispatch` and `inbound.route`, in any tier.
- **Which IRC and Telegram messages reach the bot.** Since 0.5,
  `irc.addressed` and `telegram.addressed`.
- **A hook.** Hooks mostly observe. A few honour a verdict:
  `pre_tool_call` can deny a tool call, `pre_gateway_dispatch` can skip
  or rewrite an inbound message, and `failover` can veto one hop when the
  failover rule says `mode = "script"` (it cannot retry, wait or pick
  the next route). The semantics per hook are in plugins.md.

## Override points

An **override point** is a decision veles asks by name: a service that
core (or a module) registers with its OWN answer at the bottom of the
stack. Your plugin registers a service of the same name, in any tier,
and answers instead; or it calls `next`, gets the answer below, and
changes it. Unloading your plugin puts the layer below back. Nothing in
this repository changes, so an update never asks you to re-merge.

- **The contract is the point's schemas.** Each point publishes the JSON
  Schema of what a provider is handed and of what it must answer
  (reference below, and the spec's `override_points`). The host checks an
  answer's `type`, `required` keys and each property's `enum`.
- **A broken override does not break the decision.** An override that
  errors, takes longer than the point's deadline, or answers outside the
  schema is skipped: the layer below answers and a plugin note on the
  transcript says what failed. A point that must not be skipped says
  `on_error = fail`, and its caller gets the error instead.
- **Load order does not matter.** A point's default always sits at the
  bottom, even when your plugin registered the name before the point was
  declared.
- **Every change is announced.** An override taking a point, and the
  point's answer coming back when it goes, each park a plugin note.
- **Who answers what:** `/plugins overrides`, `veles plugins overrides`
  and `GET /api/plugins/overrides` list each point, its summary and its
  stack, top first.
- **A process plugin** gets a point's request as an envelope,
  `{input, next, caller, session_id}`, and asks `service.next` with that
  `next`. Its plain services keep their bare params.
- **A `.so`** provides one with `register_json` (kind `service`; the SDK's
  `register_service`) and reaches `next` through
  `host_call("service_next", …)`.

The points core declares, and the points the bundled platform modules
declare, are listed in the reference below.

### `irc.addressed`, `telegram.addressed`: whether a message is for the bot

Since 0.5 the IRC and Telegram modules each declare a point for the
decision they make before anything reaches the gateway: is this channel
line (IRC) or group message (Telegram) for the bot, and what does the
bot read of it? Core's verdict is handed in as `core_address`: the
mention gate (`require_mention`), and on IRC the bare `tcl …`
convention. Answer `{address, text}`; `next` gives core's verdict.
Private messages are always for the bot and are not asked about.

| point | you get | default |
| --- | --- | --- |
| `irc.addressed` | `network`, `channel`, `sender`, `nick` (the bot's), `text`, `mentioned`, `require_mention`, `core_address`, `body` (the line without its address) | `{address: core_address, text: body}` |
| `telegram.addressed` | `chat_id`, `sender`, `sender_id`, `bot_username`, `text`, `mentioned`, `require_mention`, `core_address` | `{address: core_address, text}` |

The point only decides whether the message goes on. Everything after it
still applies: authorization, the ACL, lockdown, the eval budget. On
IRC a slash command in a channel still needs a real mention (the rule
that keeps a second bot from acting on a command meant for another
nick), and channel context is still added only to a mentioned line.
Each module declares its point under its own instance, so it exists
while the module runs on that node.

A module or process plugin lists the points it declares in its
manifest's `points`, each a `PointDecl` (`name`, `summary`, `input`,
`output`, `on_error`, `deadline_ms`); `on_error` is `fall_through` or
`fail`. A process plugin's points are read for the listing and the spec;
declaring one from a process plugin, with its own answer at the bottom,
is not built yet.

### `prompt.build`, `tool.dispatch`, `inbound.route`: three subsystems

Since 0.4 three of the subsystems a Rust module can wrap with a layer
are override points too, so a script, a `.so` or a process plugin can
change them. Your answer sits above any module layer.

- **`prompt.build`**: the system prompt a session starts with. You get
  `{session_id, principal, disabled_skills, prompt}`, where `prompt` is
  what core (and any module layer) built; answer `{prompt}`. Wrapping is
  `next(input)` and editing its `prompt`. The prompt is built once per
  session, so the cost is one call per session.
- **`tool.dispatch`**: every tool call a session makes. You get
  `{session_id, name, args, call_id}` and answer `{result}`, the text the
  model sees. `next` runs core's dispatch, gated by the session's ACL as
  always; pass it the input whole, its `name` and `args` changed if you
  like (core needs `call_id`). Answering without `next` means no tool
  ran; to run another tool, use `tool_call`, which is gated as the
  caller. This point is asked on every tool call while it is
  overridden, so keep the handler quick.
- **`inbound.route`**: every message a platform delivers, before the
  ACL, the commands and the turn. You get the message
  (`veles_types::bus::InboundMessage` as JSON) and answer
  `{action: "route", message}` or `{action: "drop"}`. You may change the
  `text`; you may not change `platform`, `chat_id`, `sender`, `sender_id`
  or `route`, because the ACL resolves the sender after this point. An
  answer that changes one of them is refused, with a note, and the
  original message is routed.

The turn runner stays a Rust module's layer only: it streams events and
changes the history in place, which a JSON round trip per event cannot
do.

### `failover.decide`: what happens after a model call fails

veles asks `failover.decide` every time a call to a model route fails.
Core's own answer is `{"action": "policy"}`: apply the configured policy
(a `/failover` session override, the `[[failover]]` rules, and with
neither, stop). With no override the point costs nothing and failover
behaves exactly as before 0.3.

Your provider is handed:

| field | what |
| --- | --- |
| `session_id` | the session the turn runs in |
| `interactive` | a person can answer an ask in this session |
| `attempt` | retries of this route so far: 0 on its first failure |
| `hops` | routes this chain has left behind so far |
| `route` | `{provider, model}` of the route that failed |
| `error` | `{kind, status, message}`; `kind` is `rate_limited`, `server`, `network`, `client` or `truncated` |
| `core_would_fall_back` | the error is one core falls back on (429, 5xx, network) |
| `remaining_routes` | the routes after this one, in chain order, each `{provider, model}` |
| `config` | what the configured policy says for this route: `{mode, source, max_hops, script}` |

It answers `{"action": …}`, with a `reason` the transcript shows:

| action | what veles does |
| --- | --- |
| `advance` | tries the next route in the chain |
| `retry` | calls the same route again |
| `wait` | waits `seconds` (at most 3600, cancellable), then calls the same route again |
| `goto` | moves to the configured route `provider` (and `model`, when given); a route that is not configured is refused, with a note, and the policy decides |
| `stop` | ends the turn with the failure and the reason |
| `ask` | asks the person, as the `ask` mode does |
| `policy` | applies the configured policy: core's own answer, and what `next` gives a wrapper |

An override owns the hops it decides: it can fail over with no
`[[failover]]` rule at all (the configured policy's entry check is for
the policy's own hops). Every failure is asked about, including an error
core does not fall back on (a 4xx), and one chain makes at most 32 calls,
retries and waits included, whatever an override answers. The
non-streaming chain (the summarizer, aux calls) asks the same point with
`interactive` false; there `ask` means the configured policy.

`examples/plugins/failover-backoff/` is a working override in
TypeScript: on a rate limit it waits and retries twice and then moves on,
on a server error it advances, and everything else goes to `next`.
Copy the directory into `~/.config/veles/plugins/`; unloading it gives
the decision back.

## Rust overlay modules (not stable)

An overlay module is a Rust module that lives in your own repository
and is compiled inside veles' build. A module loads only into the
gateway of the build that made it, so veles' flake builds your crate and
veles in one cargo invocation. The result is a whole veles package:
`bin/veles` and every module in `lib/`, yours among them.

Start one with:

    veles plugins new greeter --kind overlay

It writes `./greeter/`: a `dylib` crate on `veles-runtime`, a module
with one tool, a README, and a `flake.nix` that calls veles'
`lib.<system>.velesWithModules`:

    veles.lib.x86_64-linux.velesWithModules {
      modules = [ { name = "greeter"; src = ./.; } ];
    }

`examples/overlay-module/` is that scaffold for `greeter`, and a test
keeps the two equal. veles' own flake builds it as
`packages.veles-overlay-example`.

What the flake does, and what it asks of your crate:

- It copies each `src` to `crates/overlay-<name>` in veles' tree, adds
  it to the workspace, and builds veles and the overlays together.
- `name` is lowercase letters, digits and `-`, starting with a letter.
  The evaluation stops on any other name.
- The crate names `veles-runtime = { workspace = true }`. It may use
  veles-runtime's re-exports and the crates veles' `Cargo.lock` already
  has, nothing else: the build is offline, and the lock gains only the
  overlay's own entry. A crate the lock lacks stops the build before
  anything compiles, with
  `veles overlay: a module depends on a crate this Cargo.lock does not have.`
- `flake.lock` pins the veles you build against. `nix flake update
  veles` moves to a newer one, and the next `nix build` rebuilds the
  module against it with no step of yours.

Run the gateway from that build, and install every module you use from
it, the stock ones too: the gateway refuses a module from any other
build ("core changed: rebuild the module with this core"). The
scaffold's README has the install commands. `veles plugins reload
<name>` swaps the module in place, as for any module.

This tier is not part of the extension API and is not in the spec. The
module calls veles' Rust internals (`veles_core::*`), so an update can
break its build, and the changelog will not warn you. When an override
point, a script, a `.so` or a process plugin can do the job, use that.
An overlay is for what needs Rust inside the process: a layer, a
platform, a module seam.

## Versioning

- The version is `EXT_API_VERSION` (`crates/veles-types/src/plugin.rs`).
  `veles plugins api` prints it with the counts of each surface.
- Until the first release it is `0.x`, and every change bumps the minor
  number. Anything may still change in 0.x; the changelog says what did.
- From 1.0 it follows semver: adding a name is a minor bump; removing or
  reshaping one is a major bump.
- A plugin may say which version it was written against with
  `api = "0.1"` in its `plugin.toml`. The loader notes when a plugin
  targets a newer API than the running veles has. After 1.0 it refuses a
  plugin whose major version differs.

## How a change to the API is published

The published spec is `docs/extension-api.json`. It is generated from the
code by `veles plugins api --write`, never edited by hand: the bridge op
table, the hook names, the plugin.toml and manifest keys, the four script
bindings, the process protocol's methods and the C ABI's header are read
from the source a plugin author reads.

Three tests in the gate (`crates/veles-cli/tests/ext_api.rs`) fail when
the code, the spec, the changelog and this page drift apart:

| test | fails when |
| --- | --- |
| `the_published_spec_matches_the_code` | the code's surface differs from `docs/extension-api.json` |
| `every_spec_change_has_a_changelog_entry` | the newest entry of `docs/extension-api-changes.md` does not name this version and this spec's hash, or a version repeats or goes up the file |
| `the_docs_match_the_spec` | this page's stamp line differs from the spec's, it never names an item of the spec, or its reference tables name something the spec no longer has |

The precommit gate also runs `scripts/check-ext-api-site.sh`: the docs
site's copy of the spec and its page's stamp must match the repo's.

So a change to the surface means, in order:

1. change the code;
2. bump `EXT_API_VERSION`;
3. `veles plugins api --write` in the repo root;
4. add an entry at the top of `docs/extension-api-changes.md` headed
   `## <version> sha256:<hash>`, with what changed and what a plugin
   author has to do (`veles plugins api --stamp` prints the heading);
5. update this page: the text the change touches, the reference tables,
   then the stamp line;
6. update the site page (`~/veles-docs-work/src/en/extending.md`), then
   run `build/refresh-ext-api.sh` there (it copies the spec, regenerates
   the reference page and prints the stamp the page must carry).

## Reference

Every name below is in the spec. The tables are checked in both
directions: a name the spec has and this page lacks fails the gate, and
so does a first-column name the spec no longer has.

<!-- extension-api:reference:begin -->

### Operations every script tier can call

| op | Tcl | JavaScript | Racket | Common Lisp |
| --- | --- | --- | --- | --- |
| `call_llm` | `::veles::call_llm system user ?provider? ?model?` | `callLLM` | `call-llm` | `call-llm` |
| `config_get` | `::veles::config_get path` | `configGet` | `config-get` | `config-get` |
| `config_set` | `::veles::config_set path value` | `configSet` | `config-set` | `config-set` |
| `history` | `::veles::history session_id ?limit?` | `history` | `history` | `history` |
| `http_get` | `::veles::http_get url ?headers_json?` | `httpGet` | `http-get` | `http-get` |
| `http_post` | `::veles::http_post url body ?content_type? ?headers_json?` | `httpPost` | `http-post` | `http-post` |
| `http_stream_close` | `::veles::http_stream_close id` | `httpStreamClose` | `http-stream-close` | `http-stream-close` |
| `http_stream_next` | `::veles::http_stream_next id` | `httpStreamNext` | `http-stream-next` | `http-stream-next` |
| `http_stream_open` | `::veles::http_stream_open url body ?content_type? ?headers_json?` | `httpStreamOpen` | `http-stream-open` | `http-stream-open` |
| `json_get` | `::veles::json_get json key ?arg …?` | `jsonGet` | `json-get` | `json-get` |
| `limits` | `::veles::limits` | `limits` | `limits` | `limits` |
| `note` | `::veles::note summary ?detail?` | `note` | `note` | `note` |
| `nodes` | `::veles::nodes` | `nodes` | `nodes` | `nodes` |
| `principal` | `::veles::principal ?session_id?` | `principal` | `principal` | `principal` |
| `provider_emit` | `::veles::provider_emit chunk_json` | `providerEmit` | `provider-emit` | `provider-emit` |
| `routes` | `::veles::routes` | `routes` | `routes` | `routes` |
| `sandbox_where` | `::veles::sandbox_where ?session_id?` | `sandboxWhere` | `sandbox-where` | `sandbox-where` |
| `send_outbound` | `::veles::send_outbound platform chat_id text` | `sendOutbound` | `send-outbound` | `send-outbound` |
| `service_call` | `::veles::service_call name params_json ?session_id?` | `serviceCall` | `service-call` | `service-call` |
| `service_next` | `::veles::service_next token params_json` | `serviceNext` | `service-next` | `service-next` |
| `services` | `::veles::services` | `services` | `services` | `services` |
| `sessions` | `::veles::sessions` | `sessions` | `sessions` | `sessions` |
| `task_cancel` | `::veles::task_cancel id` | `taskCancel` | `task-cancel` | `task-cancel` |
| `tasks` | `::veles::tasks` | `tasks` | `tasks` | `tasks` |
| `tool_call` | `::veles::tool_call name args_json ?session_id?` | `toolCall` | `tool-call` | `tool-call` |
| `usage_totals` | `::veles::usage_totals ?session_id?` | `usageTotals` | `usage-totals` | `usage-totals` |

An out-of-tree Rust plugin reaches the same table through
`host_call("<op>", "[…args…]")`.

### Registering things from a script

| registrar | Tcl | JavaScript | Racket | Common Lisp |
| --- | --- | --- | --- | --- |
| `register_tool` | `::veles::register_tool name description schema_json procname` | `registerTool` | `register-tool` | `register-tool` |
| `register_command` | `::veles::register_command name description procname` | `registerCommand` | `register-command` | `register-command` |
| `register_service` | `::veles::register_service name procname ?description? ?cap?` | `registerService` | `register-service` | `register-service` |
| `register_provider` | `::veles::register_provider name procname` | none | none | none |
| `hook` | `::veles::hook hookname procname` | `hook` | `hook` | `hook` |

### The rest of each script binding

| name | language | what it is |
| --- | --- | --- |
| `plugin_unload` | Tcl | `::veles::plugin_unload name`, the per-plugin unload |
| `hook_disable` | Tcl | `::veles::hook_disable hookname procname` |
| `hook_enable` | Tcl | `::veles::hook_enable hookname procname` |
| `hook_is_off` | Tcl | `::veles::hook_is_off hookname procname` |
| `hooks_list` | Tcl | `::veles::hooks_list` |
| `hooks_json` | Tcl | `::veles::hooks_json` |
| `hooks_off_clear` | Tcl | `::veles::hooks_off_clear` |
| `dispatch_hook` | Tcl | `::veles::dispatch_hook hookname` |
| `call` | JavaScript, Common Lisp | any op by name: `veles.call(op, …args)`, `(veles:call op …)` |
| `log` | JavaScript | `veles.log(text, level)` |
| `veles-call` | Racket | any op by name |
| `veles-log` | Racket | a log line |
| `log-line` | Common Lisp | a log line |
| `json-parse` | Common Lisp | JSON text to Lisp data |
| `json-string` | Common Lisp | Lisp data to JSON text |
| `obj` | Common Lisp | build a JSON object |
| `spawn` | Common Lisp | run a function in a script thread of its own |

### Hooks

| hook | fired? |
| --- | --- |
| `pre_tool_call` | fired |
| `post_tool_call` | fired |
| `pre_llm_call` | fired |
| `post_llm_call` | fired |
| `api_request_error` | fired |
| `on_session_start` | fired |
| `pre_gateway_dispatch` | fired |
| `pre_approval_request` | fired |
| `post_approval_response` | fired |
| `compact_start` | fired |
| `compact_end` | fired |
| `failover` | fired |
| `subagent_start` | fired |
| `subagent_stop` | fired |
| `background_child_complete` | fired |
| `device_event` | fired: a phone's notification or power change (since 0.6; its `session_id` is `device:<name>`, which may not call tools) |
| `transform_terminal_output` | reserved (accepted for hermes plugins, never fired) |
| `transform_tool_result` | reserved |
| `transform_llm_output` | reserved |
| `pre_verify` | reserved |
| `pre_api_request` | reserved |
| `post_api_request` | reserved |
| `on_session_end` | reserved |
| `on_session_finalize` | reserved |
| `on_session_reset` | reserved |
| `kanban_task_claimed` | reserved |
| `kanban_task_completed` | reserved |
| `kanban_task_blocked` | reserved |

### plugin.toml

| key | meaning |
| --- | --- |
| `name` | the plugin's name |
| `kind` | the tier: `tcl` (the default), `rust`, `js`, `racket`, `lisp`, `module` or `process` |
| `entry` | the file to load (default `plugin.tcl`) |
| `description` | one line for listings |
| `provides_hooks` | the hooks it answers |
| `provides_services` | the services it registers |
| `uses_services` | the services it needs |
| `optional_services` | the services it works without |
| `api` | the extension API version it was written against |

| kind | tier |
| --- | --- |
| `tcl` | Tcl script, in the gateway's interpreter |
| `rust` | out-of-tree cdylib, C ABI 4 |
| `js` | JavaScript or TypeScript, the lang-js host |
| `racket` | Racket, the lang-racket host |
| `lisp` | Common Lisp, the lang-lisp host |
| `module` | a Rust dylib built with the gateway (not stable) |
| `process` | an executable speaking protocol v1 |

### The process and module manifest

A process plugin prints it for `--describe` and answers it to
`initialize`; a module exports it.

| key | meaning |
| --- | --- |
| `name` | the plugin's name |
| `version` | its own version |
| `protocol` | the protocol it speaks (1) |
| `description` | one line |
| `instances` | `single`, or `per_entry` (one instance per entry of a config array; `array`, `name`) |
| `lease` | the pool lease each instance holds (`singleton:…`) |
| `platforms` | the bus routes it claims: `key`, `public_feed` |
| `config_sections` | the config.toml sections it may read |
| `knobs` | declared installer options: `path`, `default`, `shape`, `note`, `env`. Older manifests may omit it. Secret values stay in the setup/vault flow. |
| `secrets` | the secret names it may read: `name`, `description`, `required` |
| `services` | the services it provides: `name`, `description`, `cap` (unset = internal, `any`, or an ACL capability) |
| `uses` | the services it uses: `name`, `optional`, `description` |
| `hooks` | the hooks it answers: `name`, `description` |
| `enabled_env` | a `.env` switch; false turns the plugin off on this node |
| `required_env` | a `.env` name; unset turns the plugin off on this node |
| `points` | the override points it declares, each a `PointDecl`: `name`, `summary`, `input`, `output`, `on_error`, `deadline_ms` |
| `key` | a platform's bus key |
| `public_feed` | a platform whose every line is a public post |
| `cap` | who may call a service |
| `optional` | a use the plugin works without |
| `required` | a secret the plugin says it cannot work without; the gateway does not act on it yet |
| `single` | one instance |
| `per_entry` | one instance per config entry |
| `summary` | a point's one line |
| `input` | the JSON Schema of what a point's provider is handed |
| `output` | the JSON Schema of what it must answer |
| `on_error` | `fall_through` (the layer below answers) or `fail` |
| `deadline_ms` | how long one layer may take |
| `fall_through` | an `on_error` |
| `fail` | an `on_error` |

### Process protocol v1

| method | direction | answered today? |
| --- | --- | --- |
| `initialize` | gateway → plugin | yes |
| `outbound.deliver` | gateway → plugin | yes |
| `shutdown` | gateway → plugin | yes |
| `bus.inbound` | plugin → gateway | yes |
| `note` | plugin → gateway | yes |
| `secret.get` | plugin → gateway | yes |
| `acl.resolve` | plugin → gateway | yes |
| `pairing.grant_scoped` | plugin → gateway | yes |
| `thread_room.register` | plugin → gateway | yes |
| `config.get` | plugin → gateway | yes |
| `ledger.record_posts` | plugin → gateway | yes |
| `service.call` | plugin → gateway | yes |
| `service.next` | plugin → gateway (since 0.2: the layer below an override, by its `next` token) | yes |
| `post.create` | a service name (atproto provides it) | yes |
| `repo.delete_record` | a service name (atproto) | yes |
| `repo.whoami` | a service name (atproto; since 0.8: the session DID and handle the plugin acts as — the only proof of ownership before a delete) | yes |
| `hosted.delete` | a service name (atproto) | yes |
| `appview.query` | a service name (atproto) | yes |
| `repo.get_record` | a service name (atproto) | yes |
| `vision.describe` | a service name (core provides it) | yes |
| `bsky.event` | a service name (slopdrop provides it) | yes |
| `triggers.forward` | a service name (slopdrop) | yes |
| `triggers.masks` | a service name (slopdrop) | yes |
The names `irc.send`, `irc.op`, `irc.typing`, `irc_tap.publish`,
`irc.history` and `session.principal` were declared for the abandoned IRC
process plugin and never answered; 0.2 removed them.

### C ABI 4

A plugin exports three symbols:

| symbol | what |
| --- | --- |
| `veles_plugin_abi_version` | answers 4, read before anything else runs |
| `veles_plugin_init` | gets the host table and the plugin id |
| `veles_plugin_free_string` | frees a string the plugin handed the host |

The host table, `VelesHostApi`, in order:

| field | what |
| --- | --- |
| `abi_version` | 4 |
| `struct_size` | the size of the table as the host that filled it was built (since 0.2): a field past it does not exist on that host |
| `register_hook` | a hook handler; answers a handle |
| `register_tool` | a tool (sync or async); answers a handle |
| `register_command` | a slash command |
| `register_provider` | a provider wire |
| `register_gateway_filter` | a gateway filter |
| `register_platform` | a chat platform: outbound route and inbound publish |
| `unregister` | drop one handle |
| `platform_inbound` | publish an inbound message for a platform this plugin registered |
| `timer_after` | a one-shot timer |
| `timer_cancel` | cancel it |
| `tcp_connect` | a TCP or TLS socket |
| `tcp_send` | send on it |
| `tcp_close` | close it |
| `host_call` | any op from the table above |
| `host_free_string` | free a string the host handed the plugin |
| `register_json` | since 0.2: a JSON callback of a kind (`service` today, `JSON_KIND_SERVICE`) under a name; check `struct_size` first |

The structs the registrars take:

| struct | what |
| --- | --- |
| `VelesToolSpec` | a tool's name, description, schema and callback |
| `VelesCommandSpec` | a slash command |
| `VelesProviderSpec` | a provider wire |
| `VelesPlatformSpec` | a chat platform |
| `VelesFuture` | an async tool's answer, polled by the host |
| `VelesWaker` | the waker the host hands each poll |
| `VelesHostApi` | the host table above |

The full types, constants and field order are in the spec's `abi`
section; `crates/veles-plugin-abi/src/lib.rs` is the header.

### Override points

| point | decides | when a provider fails | deadline |
| --- | --- | --- | --- |
| `failover.decide` | what happens after a call to a model route failed ("`failover.decide`" above has its input and answers) | the layer below answers | 30 s |
| `prompt.build` | the system prompt a session starts with | the layer below answers | 10 s |
| `tool.dispatch` | every tool call a session makes | the layer below answers | 600 s |
| `inbound.route` | every inbound message: route it (its text changed or not) or drop it | the layer below answers | 10 s |
| `irc.addressed` | whether an IRC channel line is for the bot, and what it reads (the IRC module) | the layer below answers | 5 s |
| `telegram.addressed` | whether a Telegram group message is for the bot, and what it reads (the Telegram module) | the layer below answers | 5 s |

<!-- extension-api:reference:end -->

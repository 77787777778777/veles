# veles plugin API — declaration stubs for tooling, never sourced.
#
# The commands below are registered in the LIVE interpreter at startup:
# the `veles::*` natives are Rust `Tcl_CreateObjCommand` (see
# crates/veles-core/src/bridge.rs), and the hook registry lives in the
# embedded prelude (crates/veles-core/src/tclrt.rs). None of them
# exists as a `proc` in any file a language server can see — so before
# this file, hovering `::veles::call_llm` in a plugin returned nothing
# while `set`/`puts` worked: the only commands a static index can find
# are the ones written down.
#
# This file IS the write-down. It is copied next to the live plugins at
# boot (sync_api_stub) so the tcl-lsp workspace index resolves the API
# for every plugin project, and it is kept honest by a cargo test that
# cross-checks these names against a real interpreter. Bodies are stubs;
# sourcing this file would SHADOW the real commands — do not.
#
# Keep signatures in sync with bridge.rs and the tclrt.rs prelude.

namespace eval ::veles {}

# Summarize/complete one prompt with the configured model fleet.
# Blocks the calling thread until the answer lands. With `provider`
# (optionally `model`) the call pins that route; otherwise the core
# picks by the model default and its failover rules. Returns the
# assistant text, or raises on provider failure.
proc ::veles::call_llm {system user {provider ""} {model ""}} { return "" }

# Read one (or a chain of) keys out of a JSON document:
#   veles::json_get $doc display_reasoning
# Follows nested objects; ARRAYS take numeric indices
# (veles::json_get $doc messages 0 role). Returns "" for a missing key.
proc ::veles::json_get {json key args} { return "" }

# One "provider -> model" line per configured route, for diagnostics.
proc ::veles::routes {} { return "" }

# Session introspection (read-only store access):
#   sessions     JSON array of {id,title,created_at,message_count},
#                newest first (up to 50).
#   history      the LAST $limit messages of one session, chronological:
#                {"session_id","total","messages":[{msg_id,role,content,
#                tool_name,created_at}...]}. $limit defaults to 20.
#   usage_totals token accounting (calls, prompt/completion/cached/
#                cache_write tokens), global or for one session.
proc ::veles::sessions {} { return "" }
proc ::veles::history {session_id {limit 20}} { return "" }
proc ::veles::usage_totals {{session_id ""}} { return "" }
#   nodes — the cluster registry (Plan B): every registered compute
#           node as a JSON array (id, name, host, version, uptime_class,
#           gpu, endpoint, caps, registered_at, heartbeat_at, self,
#           stale — by [cluster] stale_after_secs). Read-only.
proc ::veles::nodes {} { return "" }

# Read the live config as JSON and walk a dotted path:
#   veles::config_get agent.guards
# Missing path → "" (json_get convention).
proc ::veles::config_get {path} { return "" }

# Write one config key to disk (configedit::edit under the hood, same as
# the desktop's POST /api/config/set): comments preserved, .bak backup
# first, and an unreadable result is refused. The value is JSON (so
# numbers/bools/arrays keep their type) with a plain-string fallback:
#   veles::config_set sandbox.backend podman
#   veles::config_set sandbox.mounts {\[{"host":"/data","container":"/data","mode":"ro"}\]}
# "" (or null) UNSETS the key. Returns
# {"saved","backup","effect","note"} — effect is the honest advisory
# (live / new_sessions / restart_gateway), so a plugin knows whether the
# change needs a restart instead of guessing.
proc ::veles::config_set {path value} { return "" }

# HTTP from the bridge runtime: 30s timeout, 1MB body cap. The plugins
# interpreter is the TRUSTED owner surface — untrusted callers never
# reach these. Raises on transport errors and non-2xx status.
#
# $headers_json is a JSON OBJECT of name -> value, for the endpoints
# that do not use bearer auth:
#   veles::http_post $url $body application/json \
#       {{"x-api-key":"...","anthropic-version":"2023-06-01"}}
# A driver that cannot set headers cannot authenticate, which is why
# this exists (see plugins/examples/tcl-anthropic-driver).
proc ::veles::http_get {url {headers_json ""}} { return "" }
proc ::veles::http_post {url body {content_type "application/json"} {headers_json ""}} { return "" }

# Streaming HTTP: the transport a provider driver needs, because the two
# above BUFFER. http_post awaits the whole body, so a driver using it
# cannot show incremental output at all, and it carries a 30s TOTAL
# timeout that a long frontier-model turn blows through. These three pull
# one server-sent event at a time and have no ceiling on the call itself.
#
#   set r [veles::http_stream_open $url $body application/json $headers]
#   # r = {"id":7,"status":200}  — the STATUS arrives before any body,
#   #                              which is what lets a driver classify
#   #                              429/5xx and take part in failover
#   while 1 {
#       set e [veles::http_stream_next 7]
#       if {[veles::json_get $e done] ne ""}  { break }   ;# clean end
#       if {[veles::json_get $e error] ne ""} { break }   ;# transport died
#       set data [veles::json_get $e event]              ;# one data: payload
#   }
#   veles::http_stream_close 7
#
# Both terminal answers free the handle, so returning early on an error
# leaks nothing; close is idempotent either way. A non-2xx status still
# delivers its body — as one event, then done — so the error path and the
# success path are the SAME loop.
#
# $headers_json is the same optional object http_post takes, for the
# endpoints that do not use bearer auth.
proc ::veles::http_stream_open {url body {content_type "application/json"} {headers_json ""}} { return "" }
proc ::veles::http_stream_next {id} { return "" }
proc ::veles::http_stream_close {id} { return "" }

# Push a message to a live gateway adapter (telegram/irc/...). Returns
# "sent"; raises when no gateway is running or the platform has no
# adapter. Every send parks a plugin-action note — silent outbound
# automation is prohibited.
proc ::veles::send_outbound {platform chat_id text} { return "" }

# Park a plugin-action note for the transcript (kind is always
# "plugin" — a plugin cannot masquerade as another mechanism). This is
# how plugin automation stays visible; use it when your plugin does
# something the user did not ask for in this turn.
proc ::veles::note {summary {detail ""}} { return "" }

# Background tasks (the /tasks machinery): listing text and cancel.
proc ::veles::tasks {} { return "" }
proc ::veles::task_cancel {id} { return "" }

# Dispatch a real tool exactly as a turn would (aliases + deferred
# tools resolve the same way). $args_json is the JSON argument object;
# the tool's string output is returned, including error/suggestion text
# and an ACL denial — a denied call is a RESULT, never a raised error.
#
# WHOSE call it is, which is what the ACL judges it as:
#   $session_id given  -> that session's registry and principal
#   otherwise          -> the session whose hook is now running
#                         (::veles::hook_session, set by the engine
#                         around every handler call)
#   neither            -> the last-published session's registry
# So a handler that calls a tool needs no arguments at all and is still
# judged as its own caller — a guest's plugin cannot borrow the owner's
# capabilities, and the owner's cannot be denied a guest's. Pass
# $session_id explicitly only when your handler defers the call to a
# thread of its own, where the engine's ambient session does not follow.
proc ::veles::tool_call {name args_json {session_id ""}} { return "" }

# Register a plugin TOOL: the model can call it in sessions created
# AFTER registration. $procname reads ::veles::tool_args (the JSON
# arguments) and returns the tool's output string. $schema_json is the
# JSON-schema object for the arguments. Raises when $procname is not
# defined or the name is invalid.
proc ::veles::register_tool {name description schema_json procname} { return "" }

# Plugin SERVICES: one plugin using another's feature by name, whoever
# provides it (docs/plugins.md, "Services"). Declare what you provide and
# use in plugin.toml (provides_services, uses_services, optional_services).
#
# Call one. $params_json is the provider's contract; the answer is its
# text when it is a string, else its JSON. Refused when nothing provides
# it here, when its provider keeps it internal (core and in-process
# modules only), or when it needs a capability the session's principal
# (the named one, else the hook's) lacks.
proc ::veles::service_call {name params_json {session_id ""}} { return "" }
# Who provides and who uses which service here, as a JSON array (name,
# provider, access, declared_by, used_by, missing_required).
proc ::veles::services {} { return "" }
# Provide one. $procname reads ::veles::service_request — JSON
# {"params": …, "caller": …, "session_id": …, "next": …}, read it FIRST —
# and returns the answer (JSON, or text). $cap is who may call: "" = core
# and in-process modules only, "any" = everyone, or an ACL capability name.
# Withdrawn when this plugin unloads.
proc ::veles::register_service {name procname {description ""} {cap ""}} { return "" }
# What the layer BELOW yours answers, when your proc provides a service
# (or an override point) someone else also provides: wrap it, or answer
# your own way. $token is the "next" field of ::veles::service_request,
# valid while that request runs. Your ::veles::service_request is put
# back when it returns.
proc ::veles::service_next {token params_json} { return "" }

# Register a PROVIDER DRIVER: a wire veles does not speak natively.
#
# Reach it from config with a provider entry that says
#   api_mode = "plugin"
#   driver   = "<name>"
#
# $procname is called for every turn on that route. It reads
# ::veles::provider_request — the OpenAI-shaped request body the engine
# already built, with attachment parts, cache breakpoints, tool
# serialization, reasoning_effort and extra_body all applied — and
# returns an OpenAI-shaped RESPONSE object:
#
#   {"choices":[{"message":{"role":"assistant","content":"..."}}],
#    "usage":{"prompt_tokens":N,"completion_tokens":N}}
#
# The engine parses that with the same code a native route uses, so
# tool-call reassembly, the reasoning/content split and all four
# spellings of prompt-cache accounting come for free. A driver
# TRANSLATES; it does not assemble and it does not parse.
#
# To fail a turn, answer {"error":"...", "status":N} — the message
# reaches the user instead of an empty assistant reply.
proc ::veles::register_provider {name procname} { return "" }

# Stream from inside a driver proc. $chunk_json is one OpenAI-shaped
# chat.completion.chunk:
#   {"choices":[{"delta":{"content":"partial text"}}]}
# `reasoning_content` (or `reasoning`) works the same way. Call it as
# many times as you like before returning the final response object.
# Outside a driver call it RAISES — emitting into nothing is a
# misunderstanding worth hearing about, not a silent no-op.
proc ::veles::provider_emit {chunk_json} { return "" }

# Register a slash command whose render is a proc call: /$name runs
# $procname with ::veles::command_args set to the raw text after the
# command name; the proc returns the prompt text for the turn. Wins
# over a same-named .md file. Works on REPL/TUI/chat/gateway/desktop.
proc ::veles::register_command {name description procname} { return "" }

# Register $procname as a handler for lifecycle hook $hookname.
# Handlers run in registration order; see below for the hook points.
proc ::veles::hook {hookname procname} { return "" }

# Drop every hook handler plugin $name registered while it was being
# sourced (the host purges its tools, commands and other registrations
# by owner at the same time). Returns the number of handlers dropped.
# The host calls this on `unload`; a plugin has no reason to.
proc ::veles::plugin_unload {name} { return 0 }

# Switch one handler off (and back on) without editing plugin sources.
# The off-list survives reloads; the core re-applies it after each load.
proc ::veles::hook_disable {hookname procname} { return "" }
proc ::veles::hook_enable {hookname procname} { return "" }
proc ::veles::hook_is_off {hookname procname} { return 0 }

# The registry, for /status and debugging.
proc ::veles::hooks_list {} { return "" }
proc ::veles::hooks_json {} { return "" }
proc ::veles::hooks_off_clear {} { return "" }

# Core-internal: fires every handler registered for $hookname.
proc ::veles::dispatch_hook {hookname} { return "" }

# Hook points a plugin may register for. Every handler reads its payload
# from ::veles::hook_args (a JSON object) and answers with a JSON object
# or "" for no opinion. EVERY payload carries "session_id" — the session
# whose hook this is — and ::veles::hook_session holds the same value for
# the duration of the dispatch ("" outside one), which is what makes a
# bare `veles::tool_call` from a handler be judged as that session.
#
# The engine's list is data, not prose: hooks::KNOWN_HOOKS, split into
# FIRED_HOOKS (these really fire) and RESERVED_HOOKS (accepted for hermes
# plugin portability, nothing emits them). Registering for a reserved one
# is legal and does nothing.
#
#   pre_tool_call         {"tool","args"} — a real gate: answer
#                         {"action":"deny","reason":...} to stop the call
#                         ("allow"/"approve" and "block"/"reject" are
#                         accepted spellings). The model is told.
#   post_tool_call        {"tool","result_len"} — observe only
#   pre_llm_call          {} — the prompt is about to go out
#   post_llm_call         {"role","content","reasoning",
#                          "display_reasoning","provider","model",
#                          "surface"} — answer {"display_reasoning":...}
#                          and/or {"content":...} to rewrite the DISPLAY
#                          copies (never the replay field)
#   api_request_error     {"provider","error"} — a route failed
#   failover              {"from_provider","from_model","error"} —
#                         {"action":"deny","reason":...} vetoes advancing
#                         to the next route; silence advances
#   on_session_start      {"text"} — a turn is starting
#   compact_start         {"history_chars","threshold_chars","cut"} —
#                         {"action":"cancel"} stops this compaction,
#                         {"extra_context":"..."} re-injects text
#   compact_end           {"chars_before","chars_after","cut","archived"}
#   pre_approval_request  {"tool","rule","reason","detail"} — first
#                         refusal before a human is asked
#   post_approval_response {"tool","rule","answer"} — observe the answer
#   pre_gateway_dispatch  {"platform","chat_id","sender","text"} —
#                         {"action":"skip"} drops the message,
#                         {"action":"rewrite","text":...} replaces it
#   subagent_start        {"child_id","goal"}
#   subagent_stop         {"child_id","goal","ok"}
#   background_child_complete {"child_id","goal","result"}

# ── the identity model (read-only) ───────────────────────────────────
#
# Who a session acts for, where its tool calls land, and what the
# budgets are. The bridge predates the ACL entirely, so a hook firing
# on an inbound message could not tell a guest from the owner — which
# is the one distinction the whole access-control model draws.
#
# `session_id` is optional and defaults to "" (no session), which
# answers honestly rather than inventing a caller:
#   veles::principal $sid     -> {"session_id","principal":{subject,id,mask,
#                                 role,via,label,caps,max_context_tokens,
#                                 max_tokens_per_day,max_turns_per_hour}}
#   veles::sandbox_where $sid -> {"zone":"host"|"sandbox","sandbox","key","fenced"}
#   veles::limits             -> {"by_role":[...],"max_concurrent_turns"}
proc ::veles::principal {{session_id ""}} { return "" }
proc ::veles::sandbox_where {{session_id ""}} { return "" }
proc ::veles::limits {} { return "" }

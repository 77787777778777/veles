# tcl-anthropic-driver — a COMPLETE provider driver for the Anthropic
# Messages wire, and the reference for what a driver can be.
#
# A driver TRANSLATES: the engine hands it the OpenAI-shaped request body it
# already built (attachments, cache breakpoints, tool serialization,
# reasoning_effort, extra_body all applied) and parses whatever
# OpenAI-shaped response comes back. Tool-call reassembly, the
# reasoning/content split and the four spellings of prompt-cache accounting
# are therefore not this file's problem, and the transport is the engine's
# (`veles::http_stream_*`), not a socket this file dials.
#
# What IS this file's problem is that Anthropic's wire genuinely differs,
# and that it differs STRICTLY:
#
#   * the system prompt is a top-level field, not a `role: "system"` message
#   * `max_tokens` is REQUIRED, where OpenAI treats it as optional
#   * content is an ARRAY OF BLOCKS (text / thinking / tool_use /
#     tool_result / image), not a string
#   * a tool call is a `tool_use` block inside the assistant's content with
#     its arguments as a JSON OBJECT; OpenAI has a sibling `tool_calls`
#     array with a JSON STRING
#   * a tool RESULT is a `tool_result` block in a USER message, not a
#     message with `role: "tool"`
#   * auth is `x-api-key` + `anthropic-version`, not `Authorization: Bearer`
#   * usage is input_tokens/output_tokens, with cache accounting under
#     cache_creation_input_tokens / cache_read_input_tokens, and
#     input_tokens EXCLUDES both: it is only what follows the last cache
#     breakpoint, where OpenAI's prompt_tokens is the whole prompt
#   * an UNKNOWN field is a hard 400 ("Extra inputs are not permitted"), so
#     sending something the model does not support fails the turn rather
#     than being ignored
#
# That last point is why this driver asks the endpoint what it supports
# instead of carrying a table. See `anthropic::caps`.
#
# ── The per-model minefield, measured 2026-10-16 against the live API ────
#
# Three request shapes are in circulation at once, and which one a model
# wants is NOT a clean generation cut:
#
#   claude-opus-4-5          effort OK      adaptive NO   temperature OK
#   claude-sonnet-4-5-*      effort REJECT  adaptive NO   temperature OK
#   claude-haiku-4-5-*       effort REJECT  adaptive NO   temperature OK
#   claude-*-4-6             effort OK      adaptive OK   temperature OK
#   claude-*-4-7 and newer   effort OK      adaptive OK   temperature REJECT
#
# `claude-opus-4-5` accepts `output_config.effort` while its same-numbered
# siblings reject it, so no "4.5 is legacy" rule works. And `fable-5` /
# `fable-5-1` are lines that exist in no table anybody could have written
# down. `/v1/models` answers all of it — `capabilities.effort.supported`,
# `capabilities.thinking.types.adaptive.supported`, and `max_tokens` — so
# this driver reads that once per model and caches it.
#
# The same mechanism is what makes this driver usable against an
# anthropic-compatible GATEWAY rather than only against Anthropic: a gateway
# that implements `/v1/models` is interrogated like the real thing, and one
# that does not falls back to sending only what is unconditionally safe.
#
# ── Thinking, and why it is never replayed ───────────────────────────────
#
# A `thinking` block carries a `signature` that Anthropic requires on the
# way back in: sending the block without it is a 400
# (`messages.N.content.0.thinking.signature: Field required`), measured
# live. The engine stores reasoning as a plain string with nowhere to keep
# a signature, so the only safe translation is to DROP thinking on replay —
# which Anthropic accepts (also measured: the turn completes normally).
# Reasoning is still surfaced to the user and still persisted; it just is
# not sent back. That is a correctness decision, not a shortcut, and it is
# the one a driver that guessed would get wrong the first time extended
# thinking and tool use met.
#
# ── A note on the JSON helpers at the bottom ─────────────────────────────
#
# The plugin interpreter has no tcllib (measured: `package require json`
# fails there), so this file writes its own encoder. The convention is
# strict and worth stating before reading the code, because getting it
# wrong is the classic way a driver corrupts a conversation:
#
#   every VALUE handed to jobj/jarr is ALREADY a JSON fragment —
#   a string is jstr'd, a number jnum'd, a nested object jobj'd.
#
# There is no guessing and no "encode if it looks like a string": a scalar
# from `veles::json_get` comes back as bare text (`user`) and an object
# comes back as a fragment (`{"type":"ephemeral"}`), and only the call site
# knows which it asked for. Every scalar is therefore wrapped at the call
# site. The parity test parses the outbound body with a real JSON parser,
# so a site that forgot fails loudly rather than shipping a malformed
# request to a live endpoint.

namespace eval anthropic {}

# The wire-version header. Anthropic requires it and dates its changes to
# it, so it is pinned rather than tracking "latest" — an upstream change
# should be a decision here, not a surprise in production. A gateway that
# needs a different value sets `api_version` on the provider entry.
set ::anthropic::api_version "2023-06-01"

# How long Anthropic keeps a cached prefix, when the provider entry does not
# say (`cache_ttl`). One hour, not the 5-minute default: a tool loop on a
# frontier model spends 3-4 minutes writing one answer and more running its
# tools, so the 5-minute entry was gone before the next request in half the
# misses of a long session, and each miss rewrote ~500k tokens (2026-10-29).
# The 1-hour write costs 2x input instead of 1.25x, but a round writes only
# what it added, so that difference is a few thousand tokens per round
# against a whole history per miss.
set ::anthropic::cache_ttl "1h"

# API version 2026-10-21 REMOVED the sampling parameters: a request under
# that version sending temperature/top_p/top_k is a hard 400 on EVERY model
# — the ban is per-version, not per-generation, and the capability catalog
# cannot answer it, because it reports what a MODEL supports, not what a
# VERSION permits. (Measured live 2026-10-22: haiku-4-5 + temperature is
# 200 under 2023-06-01 and 400 under 2026-10-21.) The sampling gate
# therefore asks two questions, and the version's answer overrides the
# model's. See `anthropic::version_allows_sampling`.
set ::anthropic::sampling_removed_version "2026-10-21"

# Anthropic REQUIRES max_tokens and the engine never sets one, so a default
# has to come from somewhere. Used only when neither the provider entry
# (`max_output_tokens`) nor capability discovery can say. Deliberately
# modest: too high is a hard 400 on every model below it, while too low
# costs output length on one turn.
set ::anthropic::fallback_max_tokens 8192

# model → capability dict, keyed per endpoint. The plugin interpreter is
# long-lived, so this is a real cache and not a per-call re-read: the
# catalog is ~14 models of nested JSON, and asking for it on every turn
# would make it the slowest part of the turn.
array set ::anthropic::caps_cache {}

# The effort names this wire accepts. veles' ladder is a superset; anything
# outside this set is dropped rather than sent, because an unknown effort is
# a hard 400 and losing a turn over a spelling is a bad trade.
set ::anthropic::efforts {low medium high xhigh max}

# Server-side tools, by the name an operator would write in
# `built_in_tools` → the tool object this wire wants. Anthropic versions its
# server tools in the type name, and a type it does not know is a hard 400,
# so an unrecognized name is dropped rather than guessed at.
array set ::anthropic::server_tools {
    web_search     {"type":"web_search_20250305","name":"web_search"}
    web_extractor  {"type":"web_search_20250305","name":"web_search"}
    code_execution {"type":"code_execution_20250825","name":"code_execution"}
}

# ── capabilities ────────────────────────────────────────────────────────

proc anthropic::caps_key {base model} {
    # The separator is U+241F, a symbol that cannot appear in a URL or a
    # model id, so two different (endpoint, model) pairs cannot collide into
    # one cache entry. A cache that conflated two gateways would serve one
    # gateway's capability answers to the other.
    return "$base\u241f$model"
}

# What we know about one model, as a dict:
#   known     1 when discovery answered for this model
#   effort    1 when output_config.effort is accepted
#   adaptive  1 when thinking:{type:adaptive} is accepted
#   legacy    1 when thinking:{type:enabled,budget_tokens} is accepted
#   sampling  1 when temperature/top_p/top_k are accepted
#   max_out   the model's own output cap, or 0
#
# `known 0` is the honest state for a gateway with no `/v1/models`, and it
# changes what the driver sends: nothing capability-dependent goes out,
# because on a strict endpoint a guess is a failed turn.
proc anthropic::caps {base key model} {
    set ck [anthropic::caps_key $base $model]
    if {[info exists ::anthropic::caps_cache($ck)]} {
        return $::anthropic::caps_cache($ck)
    }
    set c [anthropic::discover $base $key $model]
    set ::anthropic::caps_cache($ck) $c
    return $c
}

# The capability dict used when discovery is not worth a round trip: nothing
# capability-dependent is sent. `sampling 1` is the one permissive field,
# because a sampling parameter this driver forwards can only have come from
# an operator's own extra_body — if they set one for a model that rejects
# it, that is their request and the endpoint's answer, not a guess of ours.
proc anthropic::unknown_caps {} {
    return [dict create known 0 effort 0 adaptive 0 legacy 0 sampling 1 max_out 0]
}

# Read one model's capabilities out of `/v1/models`.
#
# Failures are not errors: a gateway that does not implement the endpoint,
# a network blip, or a model the catalog has not caught up to all yield
# `known 0`, and the driver degrades to the conservative shape instead of
# failing the turn. What must never happen is a driver that cannot talk to
# its endpoint because it could not read a catalog.
proc anthropic::discover {base key model} {
    set url "[anthropic::trim_base $base]/v1/models"
    if {[catch {::veles::http_get $url [anthropic::headers_for_key $key ""]} raw]} {
        return [anthropic::unknown_caps]
    }

    # `jget`, not `json_get`: a gateway that mounts the wire but not the
    # catalog may answer with an HTML 200, and json_get ERRORS on that rather
    # than missing. Discovery failing must degrade to "unknown", never abort
    # the turn.
    #
    # Walk `data` by index until the id matches or the array runs out. A miss
    # returns "", which is the loop's exit.
    for {set i 0} {1} {incr i} {
        set id [anthropic::jget $raw data $i id]
        if {$id eq ""} { break }
        if {$id ne $model} { continue }

        set effort [anthropic::truthy [anthropic::jget $raw data $i capabilities effort supported]]
        set adaptive [anthropic::truthy [anthropic::jget $raw data $i capabilities thinking types adaptive supported]]
        set legacy [anthropic::truthy [anthropic::jget $raw data $i capabilities thinking types enabled supported]]
        set max_out [anthropic::jget $raw data $i max_tokens]
        if {![string is integer -strict $max_out]} { set max_out 0 }

        # Sampling is the one thing the catalog does not report directly,
        # and it is the one that changed behaviour mid-line: temperature is
        # accepted through 4.6 and a hard 400 from 4.7 on ("deprecated for
        # this model"). `thinking.types.enabled.supported` is the field that
        # tracks it, and it does so across all fourteen models this key can
        # reach — verified 2026-10-16 by asking each model for `temperature`
        # and comparing against the catalog:
        #
        #   enabled=true   opus-4-5, sonnet-4-5, haiku-4-5, opus-4-6,
        #                  sonnet-4-6            → temperature ACCEPTED
        #   enabled=false  opus-4-7, opus-4-8, and every 5-series
        #                                        → temperature REJECTED
        #
        # That the legacy thinking shape and the sampling parameters were
        # retired together is why one field predicts the other. It is still
        # an inference, so it is stated rather than buried: if a future line
        # breaks it, this is the line to change. (Note that
        # `capabilities.effort.supported` does NOT work as the predictor —
        # opus-4-5 supports effort and still accepts temperature, while its
        # sibling sonnet-4-5 rejects effort and accepts temperature.)
        set sampling $legacy

        return [dict create known 1 effort $effort adaptive $adaptive \
                    legacy $legacy sampling $sampling max_out $max_out]
    }
    # The catalog answered but did not list this model. Same posture as a
    # catalog that did not answer: send nothing capability-dependent.
    return [anthropic::unknown_caps]
}

proc anthropic::truthy {v} {
    return [expr {$v eq "true" || $v eq "1"}]
}

proc anthropic::trim_base {base} {
    set b $base
    while {[string length $b] > 0 && [string index $b end] eq "/"} {
        set b [string range $b 0 end-1]
    }
    return $b
}

# ── endpoint knobs ──────────────────────────────────────────────────────
#
# All of these come from the reserved `veles` key the engine adds to every
# driver request, which is what lets ONE driver serve several provider
# entries with different credentials and different gateway shapes. Each is
# optional and each falls back to this driver's own default when empty.

proc anthropic::endpoint_path {req} {
    set p [::veles::json_get $req veles endpoint_path]
    if {$p eq ""} { return "/v1/messages" }
    if {![string match "/*" $p]} { set p "/$p" }
    return $p
}

# The headers for a route, with its configured auth style and version.
#
# `x-api-key` is Anthropic's own; `bearer` is what the gateways that resell
# it usually want, because that is what their auth stack speaks. An
# unrecognized style falls back to x-api-key — config load already refuses
# anything outside the two (`validate_auth_styles`), so this arm is a
# defence against a caller that bypassed validation, not a silent default.
proc anthropic::headers_for {req key} {
    return [anthropic::headers_for_key $key \
        [::veles::json_get $req veles auth_style] \
        [anthropic::api_version_for $req]]
}

proc anthropic::api_version_for {req} {
    set v [::veles::json_get $req veles api_version]
    if {$v eq ""} { return $::anthropic::api_version }
    return $v
}

# Does the negotiated wire version still accept sampling parameters?
#
# Only a real Anthropic version DATE can trigger the ban: a gateway may
# put any string in the version header (`api_version` is an operator
# knob), and the 2026-10-21 rule is Anthropic's, not the gateway's —
# dropping an operator's temperature against a proxy that would have
# accepted it is a worse guess than forwarding it. A date at or past
# `sampling_removed_version` gets the ban; anything else the permissive
# answer. ISO dates sort correctly as strings, which is why the compare
# is a plain string compare.
proc anthropic::version_allows_sampling {version} {
    set is_date [expr {
        [string length $version] == 10
        && [string match {[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]} $version]
    }]
    return [expr {!($is_date && [string compare $version $::anthropic::sampling_removed_version] >= 0)}]
}

proc anthropic::headers_for_key {key style {ver ""}} {
    if {$ver eq ""} { set ver $::anthropic::api_version }
    if {$style eq "bearer"} {
        return [anthropic::jobj [list \
            [list "authorization" [anthropic::jstr "Bearer $key"]] \
            [list "anthropic-version" [anthropic::jstr $ver]]]]
    }
    return [anthropic::jobj [list \
        [list "x-api-key" [anthropic::jstr $key]] \
        [list "anthropic-version" [anthropic::jstr $ver]]]]
}

# ── request: OpenAI shape → Anthropic Messages shape ────────────────────

proc anthropic::to_wire {req caps streaming} {
    set model [::veles::json_get $req model]
    set out_msgs {}
    set system ""
    set system_cache ""

    for {set i 0} {1} {incr i} {
        set role [::veles::json_get $req messages $i role]
        if {$role eq ""} { break }
        set content [::veles::json_get $req messages $i content]

        if {$role eq "system"} {
            # Hoisted out of `messages`. CONCATENATED when there is more
            # than one: keeping only the first would silently lose
            # instructions.
            #
            # The engine may have marked a cache breakpoint here, which
            # turns `content` into a parts array. Both shapes are handled
            # and the breakpoint is preserved — Anthropic's system field
            # takes the same block array, so a cache_control the engine
            # computed survives the translation instead of being dropped.
            if {[anthropic::content_is_parts $req $i]} {
                lassign [anthropic::parts_text $content] content cc
                if {$cc ne ""} { set system_cache $cc }
            }
            if {$system eq ""} {
                set system $content
            } else {
                append system "\n\n" $content
            }
            continue
        }

        if {$role eq "tool"} {
            # A tool result is a block in a USER message. Consecutive
            # results are MERGED into one user message, which is the
            # canonical shape when the model called several tools in one
            # turn; separate user messages also work, but one message is
            # what the wire means by "here are your results".
            #
            # The engine marks a cache breakpoint on a tool message too:
            # explicit mode marks the last message of every request, and
            # mid-turn that is a tool result. The mark turns `content` into
            # a one-part array, so the text goes back into the result and
            # the marker onto the tool_result block itself, where Anthropic
            # caches like at any content block. Read as a plain string, the
            # array reached the model as its own JSON and the marker never
            # reached the wire, which is how a session's rolling cache died
            # at the first steer (2026-10-28).
            set id [::veles::json_get $req messages $i tool_call_id]
            set cache ""
            if {[anthropic::content_is_parts $req $i]} {
                lassign [anthropic::parts_text $content] content cache
            }
            set fields [list \
                [list "type" [anthropic::jstr "tool_result"]] \
                [list "tool_use_id" [anthropic::jstr $id]] \
                [list "content" [anthropic::jstr $content]]]
            if {$cache ne ""} { lappend fields [list "cache_control" $cache] }
            set block [anthropic::jobj $fields]
            set last [lindex $out_msgs end]
            if {[anthropic::is_tool_result_msg $last]} {
                # Append into the message already there rather than opening a
                # new one. The built message ends in a close-bracket and a
                # close-brace — the content array's and the object's — so
                # both are dropped, the new block is joined with a comma,
                # and both are put back. Getting this wrong is invisible
                # until the endpoint rejects the body, which is why the
                # parity test parses what this builds.
                #
                # NOTE for whoever edits this proc: Tcl counts braces
                # LITERALLY inside a proc body — a hash mark is not a
                # comment there. An unbalanced brace in prose closes the
                # proc early and the rest of the file evaluates at top
                # level, which is how "can't read model: no such variable"
                # reached the loader once already.
                set merged "[string range $last 0 end-2],$block\]\}"
                set out_msgs [lreplace $out_msgs end end $merged]
            } else {
                lappend out_msgs [anthropic::jobj [list \
                    [list "role" [anthropic::jstr "user"]] \
                    [list "content" [anthropic::jarr [list $block]]]]]
            }
            continue
        }

        # An assistant turn that CALLED tools carries them as blocks beside
        # its text, not in a separate field.
        set blocks {}
        if {[anthropic::content_is_parts $req $i]} {
            # A user message with attachments arrives as OpenAI parts: text
            # and image_url. Translated to this wire's blocks, which is the
            # difference between an image reaching the model and an image
            # silently becoming an empty string.
            for {set p 0} {1} {incr p} {
                set t [::veles::json_get $content $p type]
                if {$t eq ""} { break }
                switch -- $t {
                    text {
                        set txt [::veles::json_get $content $p text]
                        if {$txt eq ""} { continue }
                        lappend blocks [anthropic::text_block $txt $content $p]
                    }
                    image_url {
                        set img [anthropic::image_block \
                            [::veles::json_get $content $p image_url url] \
                            [::veles::json_get $content $p cache_control]]
                        if {$img ne ""} { lappend blocks $img }
                    }
                }
            }
        } elseif {$content ne ""} {
            lappend blocks [anthropic::text_block $content "" -1]
        }

        for {set t 0} {1} {incr t} {
            set tid [::veles::json_get $req messages $i tool_calls $t id]
            if {$tid eq ""} { break }
            set tname [::veles::json_get $req messages $i tool_calls $t function name]
            set targs [::veles::json_get $req messages $i tool_calls $t function arguments]
            # OpenAI carries arguments as a JSON *string*; this wire wants
            # the object itself.
            if {$targs eq ""} { set targs "{}" }
            lappend blocks [anthropic::jobj [list \
                [list "type" [anthropic::jstr "tool_use"]] \
                [list "id" [anthropic::jstr $tid]] \
                [list "name" [anthropic::jstr $tname]] \
                [list "input" $targs]]]
        }

        # NOTE what is NOT here: `reasoning_content`. The engine replays it
        # when the provider asks, and this driver drops it, because a
        # thinking block without its `signature` is a hard 400 and there is
        # nowhere in the engine's message model to keep one. See the header
        # comment. Dropping is accepted by the endpoint; sending is not.

        if {[llength $blocks] == 0} { continue }
        lappend out_msgs [anthropic::jobj [list \
            [list "role" [anthropic::jstr $role]] \
            [list "content" [anthropic::jarr $blocks]]]]
    }

    set parts [list \
        [list "model" [anthropic::jstr $model]] \
        [list "max_tokens" [anthropic::jnum [anthropic::max_out $req $caps]]] \
        [list "messages" [anthropic::jarr $out_msgs]]]

    if {$system ne ""} {
        if {$system_cache eq ""} {
            lappend parts [list "system" [anthropic::jstr $system]]
        } else {
            # The engine asked for an explicit cache breakpoint. system
            # takes the block-array form so the breakpoint has somewhere to
            # live; the bare string form has no such slot.
            lappend parts [list "system" [anthropic::jarr [list \
                [anthropic::jobj [list \
                    [list "type" [anthropic::jstr "text"]] \
                    [list "text" [anthropic::jstr $system]] \
                    [list "cache_control" $system_cache]]]]]]
        }
    }

    # ── thinking / effort, per what the model actually supports ─────────
    #
    # The engine's `reasoning_effort` is the only signal, and it maps to two
    # different fields on this wire depending on the model. Sending either
    # one to a model that does not support it is a 400, so when capabilities
    # are unknown BOTH are withheld. That is the conservative posture
    # `discover` states: a gateway that will not say what it supports gets
    # no guesses.
    set effort [::veles::json_get $req reasoning_effort]
    if {$effort ne "" && [dict get $caps known]} {
        set want_thinking [expr {$effort ne "off" && $effort ne "none"}]
        if {[dict get $caps adaptive]} {
            if {$want_thinking} {
                lappend parts [list "thinking" \
                    [anthropic::jobj [list [list "type" [anthropic::jstr "adaptive"]]]]]
            }
        } elseif {[dict get $caps legacy]} {
            # The legacy shape wants a token budget and requires
            # max_tokens > budget_tokens. Half the cap is the split: enough
            # room to think and enough left to answer, and it cannot violate
            # the ordering the endpoint enforces. 1024 is the floor the
            # endpoint itself states.
            set cap [anthropic::max_out $req $caps]
            set budget [expr {$cap / 2}]
            if {$budget < 1024} { set budget 1024 }
            if {$want_thinking} {
                lappend parts [list "thinking" [anthropic::jobj [list \
                    [list "type" [anthropic::jstr "enabled"]] \
                    [list "budget_tokens" [anthropic::jnum $budget]]]]]
            }
        }
        if {[dict get $caps effort] && [lsearch -exact $::anthropic::efforts $effort] >= 0} {
            lappend parts [list "output_config" [anthropic::jobj [list \
                [list "effort" [anthropic::jstr $effort]]]]]
        }
    }

    # ── sampling, only where it is still accepted ───────────────────────
    #
    # temperature/top_p/top_k are DEPRECATED on the current lines and a hard
    # 400 there, so they are forwarded only when the model is known to take
    # them. They arrive via extra_body, which the engine merges verbatim
    # onto the request body; this driver lifts them into the wire body.
    #
    # TWO gates, because the model check alone was version-blind:
    # 2026-10-21 removed the parameters per-VERSION, so a model that still
    # takes temperature under 2023-06-01 (and the catalog says so — the
    # capability is real, the version just no longer permits sending it)
    # gets a hard 400 under the new wire. The version gate overrides BOTH
    # the catalog's answer and `unknown_caps`'s permissive default, which
    # is exactly the combination that would otherwise forward a temperature
    # into a guaranteed 400.
    if {[dict get $caps sampling] && [anthropic::version_allows_sampling \
            [anthropic::api_version_for $req]]} {
        foreach f {temperature top_p top_k} {
            set v [::veles::json_get $req $f]
            if {$v ne ""} { lappend parts [list $f [anthropic::jnum $v]] }
        }
    }

    # stop_sequences: the engine has no field for these, so they ride
    # extra_body under either spelling. Both are accepted because OpenAI
    # says `stop` and Anthropic says `stop_sequences`, and an operator
    # writing extra_body for this wire should not have to remember which.
    set stop [::veles::json_get $req stop_sequences]
    if {$stop eq ""} { set stop [::veles::json_get $req stop] }
    if {$stop ne ""} {
        if {[anthropic::is_json_array $stop]} {
            lappend parts [list "stop_sequences" $stop]
        } else {
            lappend parts [list "stop_sequences" \
                [anthropic::jarr [list [anthropic::jstr $stop]]]]
        }
    }

    # tool_choice, translated from OpenAI's spellings. The engine does not
    # set it, so this only fires for an operator who put it in extra_body —
    # but "required" and the named-function form are real OpenAI shapes that
    # do not exist here, and forwarding one verbatim is a 400.
    set tc [::veles::json_get $req tool_choice]
    if {$tc ne ""} {
        set translated [anthropic::tool_choice $tc]
        if {$translated ne ""} { lappend parts [list "tool_choice" $translated] }
    }

    # Tools: OpenAI nests the schema under function.parameters; this wire
    # puts name/description/input_schema at the top level. Server tools come
    # after, in their own object shape.
    set tools {}
    for {set t 0} {1} {incr t} {
        set tname [::veles::json_get $req tools $t function name]
        if {$tname eq ""} { break }
        set tdesc [::veles::json_get $req tools $t function description]
        set tschema [::veles::json_get $req tools $t function parameters]
        if {$tschema eq ""} { set tschema {{"type":"object"}} }
        set pairs [list \
            [list "name" [anthropic::jstr $tname]] \
            [list "description" [anthropic::jstr $tdesc]] \
            [list "input_schema" $tschema]]
        # A cache breakpoint the engine put on the last tool survives.
        set cc [::veles::json_get $req tools $t cache_control]
        if {$cc ne ""} { lappend pairs [list "cache_control" $cc] }
        lappend tools [anthropic::jobj $pairs]
    }
    foreach sv [anthropic::server_tools_for $req] {
        lappend tools $sv
    }
    if {[llength $tools] > 0} {
        lappend parts [list "tools" [anthropic::jarr $tools]]
    }

    # Anything else an operator put in extra_body that this wire understands
    # verbatim. Passthrough is last so an explicit knob cannot be overwritten
    # by an inferred one; the transport keys the engine owns
    # (messages/model/stream/tools) never reach here, because its own
    # denylist drops them before the driver sees the body.
    foreach f {metadata service_tier container} {
        set v [::veles::json_get $req $f]
        if {$v ne ""} { lappend parts [list $f $v] }
    }

    if {$streaming} {
        lappend parts [list "stream" "true"]
    }
    return [anthropic::with_cache_ttl [anthropic::jobj $parts] [anthropic::cache_ttl_for $req]]
}

# The cache lifetime to ask for: the provider entry's `cache_ttl` (the engine
# sends it on the `veles` key), else this driver's default.
proc anthropic::cache_ttl_for {req} {
    set t [string trim [::veles::json_get $req veles cache_ttl]]
    if {$t eq ""} { return $::anthropic::cache_ttl }
    return $t
}

# Every breakpoint in `body`, with the lifetime `ttl` asks for. The engine
# marks breakpoints as `{"type":"ephemeral"}`, which is the 5-minute cache;
# "1h" adds `"ttl":"1h"` to each. Done on the finished body, so no site that
# carries a breakpoint can be missed. Matching the serialized text is exact:
# inside a JSON string every quote is escaped, so the unescaped pattern can
# only be a real `cache_control` object, never message text that mentions one.
proc anthropic::with_cache_ttl {body ttl} {
    if {$ttl ne "1h"} { return $body }
    return [string map [list \
        {"cache_control":{"type":"ephemeral"}} \
        {"cache_control":{"type":"ephemeral","ttl":"1h"}}] $body]
}

# What the operator pinned as an output cap on the provider entry, or 0 when
# they did not. Split out because `drive` needs the ANSWER to "was one
# pinned?" before it decides whether the catalog read is worth making, and
# `max_out` needs the value itself — two questions about one field, which is
# where a duplicated parse would drift.
proc anthropic::configured_max_out {req} {
    set configured [::veles::json_get $req veles max_output_tokens]
    if {$configured eq ""} { return 0 }
    if {![string is integer -strict $configured]} { return 0 }
    if {$configured <= 0} { return 0 }
    return $configured
}

# The output cap to send. Provider config wins, then what the catalog said,
# then this driver's default. A configured value is NOT clamped to the
# catalog's: an operator who sets one has a reason, and a gateway's catalog
# may lag the models it serves.
proc anthropic::max_out {req caps} {
    set configured [anthropic::configured_max_out $req]
    if {$configured > 0} { return $configured }
    set discovered [dict get $caps max_out]
    if {$discovered > 0} { return $discovered }
    return $::anthropic::fallback_max_tokens
}

# OpenAI's tool_choice spellings → this wire's. "" means "do not send", so a
# shape this wire cannot express is dropped rather than forwarded into a
# 400.
proc anthropic::tool_choice {tc} {
    # A bare string: "auto", "none", "required".
    if {![anthropic::is_json_object $tc]} {
        switch -- $tc {
            auto     { return [anthropic::jobj [list [list "type" [anthropic::jstr "auto"]]]] }
            none     { return [anthropic::jobj [list [list "type" [anthropic::jstr "none"]]]] }
            required { return [anthropic::jobj [list [list "type" [anthropic::jstr "any"]]]] }
            default  { return "" }
        }
    }
    set type [::veles::json_get $tc type]
    if {$type eq "function"} {
        set name [::veles::json_get $tc function name]
        if {$name eq ""} { return "" }
        return [anthropic::jobj [list \
            [list "type" [anthropic::jstr "tool"]] \
            [list "name" [anthropic::jstr $name]]]]
    }
    # Already an Anthropic-shaped object, from an operator who knew the wire.
    if {$type eq "auto" || $type eq "any" || $type eq "tool" || $type eq "none"} {
        return $tc
    }
    return ""
}

# `built_in_tools` → this wire's server-tool objects, in configured order.
# Unknown names are dropped: sending a server-tool type the endpoint has
# never heard of is a 400, and one unrecognized name must not cost the turn.
proc anthropic::server_tools_for {req} {
    set out {}
    for {set i 0} {1} {incr i} {
        set name [::veles::json_get $req veles built_in_tools $i]
        if {$name eq ""} { break }
        if {[info exists ::anthropic::server_tools($name)]} {
            lappend out $::anthropic::server_tools($name)
        }
    }
    return $out
}

# ── content blocks ──────────────────────────────────────────────────────

proc anthropic::text_block {text parts_json idx} {
    set pairs [list \
        [list "type" [anthropic::jstr "text"]] \
        [list "text" [anthropic::jstr $text]]]
    # A cache breakpoint the engine marked on this part, preserved.
    if {$parts_json ne "" && $idx >= 0} {
        set cc [::veles::json_get $parts_json $idx cache_control]
        if {$cc ne ""} { lappend pairs [list "cache_control" $cc] }
    }
    return [anthropic::jobj $pairs]
}

# An OpenAI `image_url` part → this wire's `image` block.
#
# The engine hands over a data URL (`data:<mime>;base64,…`), which is the
# shape OpenAI's `image_url` wants and NOT the shape this wire wants: here
# the media type and the payload are separate fields under `source`. A plain
# http(s) URL becomes a `url` source. Anything else — an unrecognized
# scheme, a malformed data URL — yields "", so the rest of the message still
# goes out. Losing one image with the turn intact beats a 400 that loses the
# turn.
proc anthropic::image_block {url cache_control} {
    if {$url eq ""} { return "" }
    if {[string match "data:*" $url]} {
        set semi [string first ";" $url]
        set comma [string first "," $url]
        if {$semi < 5 || $comma < 0 || $comma < $semi} { return "" }
        set mime [string range $url 5 [expr {$semi - 1}]]
        set b64 [string range $url [expr {$comma + 1}] end]
        if {$mime eq "" || $b64 eq ""} { return "" }
        set pairs [list \
            [list "type" [anthropic::jstr "base64"]] \
            [list "media_type" [anthropic::jstr $mime]] \
            [list "data" [anthropic::jstr $b64]]]
    } elseif {[string match "http://*" $url] || [string match "https://*" $url]} {
        set pairs [list \
            [list "type" [anthropic::jstr "url"]] \
            [list "url" [anthropic::jstr $url]]]
    } else {
        return ""
    }
    if {$cache_control ne ""} { lappend pairs [list "cache_control" $cache_control] }
    return [anthropic::jobj [list \
        [list "type" [anthropic::jstr "image"]] \
        [list "source" [anthropic::jobj $pairs]]]]
}

# ── response: Anthropic Messages shape → OpenAI shape ───────────────────

proc anthropic::from_wire {resp} {
    set text ""
    set thinking ""
    set calls {}

    for {set i 0} {1} {incr i} {
        set type [::veles::json_get $resp content $i type]
        if {$type eq ""} { break }
        # NOTE: server_tool_use / web_search_tool_result are NOT rendered —
        # the endpoint ran a server tool itself, and their content is not a
        # reply to the user. The model's own text block is. Skipped rather
        # than appended, because pasting a search-result JSON blob into the
        # visible answer is worse than dropping it. They reach the `switch`
        # below and match no arm.
        #
        # That comment lives HERE and not between the switch patterns: Tcl
        # counts a comment at pattern position as a pattern with no body,
        # which is a load-time error ("extra switch pattern with no body").
        switch -- $type {
            text {
                append text [::veles::json_get $resp content $i text]
            }
            thinking {
                append thinking [::veles::json_get $resp content $i thinking]
            }
            tool_use {
                set input [::veles::json_get $resp content $i input]
                if {$input eq ""} { set input "{}" }
                # Back to an arguments STRING, which is what the OpenAI shape
                # — and therefore the engine's own parser — expects.
                lappend calls [anthropic::jobj [list \
                    [list "id" [anthropic::jstr [::veles::json_get $resp content $i id]]] \
                    [list "type" [anthropic::jstr "function"]] \
                    [list "function" [anthropic::jobj [list \
                        [list "name" [anthropic::jstr [::veles::json_get $resp content $i name]]] \
                        [list "arguments" [anthropic::jstr $input]]]]]]]
            }
        }
    }

    set msg [list \
        [list "role" [anthropic::jstr "assistant"]] \
        [list "content" [anthropic::jstr $text]]]
    if {$thinking ne ""} {
        # Surfaced and persisted; never replayed (see the header comment).
        lappend msg [list "reasoning_content" [anthropic::jstr $thinking]]
    }
    if {[llength $calls] > 0} {
        lappend msg [list "tool_calls" [anthropic::jarr $calls]]
    }

    # Usage. The two cache fields keep their Anthropic spellings on purpose:
    # the engine's `cache_usage` reads cache_read_input_tokens and
    # cache_creation_input_tokens directly, so renaming them here would
    # throw the accounting away.
    #
    # prompt_tokens is the WHOLE prompt, the OpenAI meaning the engine
    # reads it with. Anthropic's input_tokens counts only what came after
    # the last cache breakpoint, so the cache write and read are added back.
    # Passed through alone, a cached 26k prompt reported 2 tokens: the
    # compaction bar never moved and the cache share read in the millions
    # of percent.
    set usage {}
    set in [::veles::json_get $resp usage input_tokens]
    set out [::veles::json_get $resp usage output_tokens]
    if {$in ne ""} {
        set total $in
        foreach f {cache_creation_input_tokens cache_read_input_tokens} {
            set v [::veles::json_get $resp usage $f]
            if {[string is entier -strict $v]} { set total [expr {$total + $v}] }
        }
        lappend usage [list "prompt_tokens" [anthropic::jnum $total]]
    }
    if {$out ne ""} { lappend usage [list "completion_tokens" [anthropic::jnum $out]] }
    foreach f {cache_creation_input_tokens cache_read_input_tokens} {
        set v [::veles::json_get $resp usage $f]
        if {$v ne ""} { lappend usage [list $f [anthropic::jnum $v]] }
    }
    # Thinking tokens. Anthropic reports them under
    # usage.output_tokens_details.thinking_tokens; the engine's
    # `reasoning_usage` reads ONLY the OpenAI spelling
    # (completion_tokens_details.reasoning_tokens). Translated here, or the
    # number is lost and the thinking is billed as answer output — the exact
    # lie that separate counter exists to expose.
    set think [::veles::json_get $resp usage output_tokens_details thinking_tokens]
    if {$think ne ""} {
        lappend usage [list "completion_tokens_details" [anthropic::jobj [list \
            [list "reasoning_tokens" [anthropic::jnum $think]]]]]
    }

    set finish [anthropic::finish_reason [::veles::json_get $resp stop_reason]]

    return [anthropic::jobj [list \
        [list "choices" [anthropic::jarr [list [anthropic::jobj [list \
            [list "message" [anthropic::jobj $msg]] \
            [list "finish_reason" $finish]]]]]] \
        [list "usage" [anthropic::jobj $usage]]]]
}

# stop_reason → finish_reason. The engine reads finish_reason to decide
# whether a turn is over and whether a tool round follows, so mapping it is
# not cosmetic: `tool_use` has to become `tool_calls` or the loop stops at
# the first tool call instead of running it.
proc anthropic::finish_reason {stop} {
    switch -- $stop {
        end_turn      { return [anthropic::jstr "stop"] }
        stop_sequence { return [anthropic::jstr "stop"] }
        max_tokens    { return [anthropic::jstr "length"] }
        tool_use      { return [anthropic::jstr "tool_calls"] }
        pause_turn    { return [anthropic::jstr "stop"] }
        refusal       { return [anthropic::jstr "content_filter"] }
        default       {
            # An unknown stop_reason maps to an EMPTY finish_reason, which
            # the engine treats as "no verdict from the provider". Guessing
            # "stop" would claim a turn ended cleanly when the wire said
            # something this driver has never seen.
            return [anthropic::jstr ""]
        }
    }
}

# ── streaming ───────────────────────────────────────────────────────────
#
# Anthropic streams SSE, and the events are typed: message_start carries the
# input usage, content_block_start/delta/stop carry the blocks, message_delta
# carries stop_reason and the output usage, message_stop ends it, and ping is
# a keepalive to ignore. Assembling that into one response is what this does,
# and it ends by handing the NON-streaming shape to `from_wire` so the two
# paths cannot disagree about what a completion looks like.

proc anthropic::drive_stream {body url headers} {
    # One accumulator per block index. text and thinking concatenate their
    # own text; tool_use keeps its id and name separate from its argument
    # FRAGMENTS, because a tool_use block receives both a header (on
    # content_block_start) and a stream of JSON fragments (on
    # input_json_delta), and one slot for two facts produces neither.
    array set blocks {}
    array set kinds {}
    array set partials {}
    array set tool_ids {}
    array set tool_names {}
    array set usage {}
    set stop_reason ""

    if {[catch {::veles::http_stream_open $url $body "application/json" $headers} opened]} {
        return [anthropic::err $opened [anthropic::status_of $opened]]
    }
    set stream_id [::veles::json_get $opened id]
    set status [::veles::json_get $opened status]

    # A non-2xx status still delivers its body as one event, so the error
    # path and the success path are the same loop. The body is pulled here so
    # the message the user sees is the endpoint's own, not "http 429".
    if {$status ne "200"} {
        set detail ""
        if {![catch {set nxt [::veles::http_stream_next $stream_id]}]} {
            set detail [::veles::json_get $nxt event]
        }
        catch {::veles::http_stream_close $stream_id}
        set msg [anthropic::error_message $detail]
        if {$msg eq ""} { set msg "http $status" }
        return [anthropic::err $msg $status]
    }

    while {1} {
        if {[catch {set nxt [::veles::http_stream_next $stream_id]} e]} {
            catch {::veles::http_stream_close $stream_id}
            return [anthropic::err $e 502]
        }
        if {[::veles::json_get $nxt done] ne ""} { break }
        set etype [::veles::json_get $nxt error]
        if {$etype ne ""} {
            catch {::veles::http_stream_close $stream_id}
            return [anthropic::err $etype 502]
        }
        set data [::veles::json_get $nxt event]
        # `[DONE]` is an OpenAI convention this wire never sends, but a
        # gateway in front of it might; treat it as the end either way.
        if {$data eq "" || $data eq "\[DONE\]"} { continue }

        set ev [anthropic::jget $data type]
        switch -- $ev {
            message_start {
                # Input usage arrives FIRST on this wire, and the output
                # count with it is a placeholder — message_delta corrects it.
                foreach f {input_tokens output_tokens cache_creation_input_tokens cache_read_input_tokens} {
                    set v [anthropic::jget $data message usage $f]
                    if {$v ne ""} { set usage($f) $v }
                }
                set think [anthropic::jget $data message usage output_tokens_details thinking_tokens]
                if {$think ne ""} { set usage(thinking_tokens) $think }
            }
            content_block_start {
                set idx [anthropic::jget $data index]
                set kinds($idx) [anthropic::jget $data content_block type]
                set blocks($idx) ""
                set partials($idx) ""
                if {$kinds($idx) eq "tool_use"} {
                    set tool_ids($idx) [anthropic::jget $data content_block id]
                    set tool_names($idx) [anthropic::jget $data content_block name]
                }
            }
            content_block_delta {
                set idx [anthropic::jget $data index]
                switch -- [anthropic::jget $data delta type] {
                    text_delta {
                        set t [anthropic::jget $data delta text]
                        if {$t ne ""} {
                            append blocks($idx) $t
                            # Emitted as it arrives, which is the entire
                            # point of streaming: the surface shows tokens
                            # landing instead of waiting for the turn.
                            ::veles::provider_emit [anthropic::delta_chunk "content" $t]
                        }
                    }
                    thinking_delta {
                        set t [anthropic::jget $data delta thinking]
                        if {$t ne ""} {
                            append blocks($idx) $t
                            ::veles::provider_emit [anthropic::delta_chunk "reasoning_content" $t]
                        }
                    }
                    signature_delta {
                        # The replay credential. Received and dropped on
                        # purpose: see the header comment on why thinking is
                        # never sent back.
                    }
                    input_json_delta {
                        # Tool arguments arrive as JSON FRAGMENTS — strings
                        # that only parse once concatenated, so they
                        # accumulate raw and are assembled at the end.
                        append partials($idx) [anthropic::jget $data delta partial_json]
                    }
                }
            }
            message_delta {
                set sr [anthropic::jget $data delta stop_reason]
                if {$sr ne ""} { set stop_reason $sr }
                foreach f {output_tokens input_tokens cache_creation_input_tokens cache_read_input_tokens} {
                    set v [anthropic::jget $data usage $f]
                    if {$v ne ""} { set usage($f) $v }
                }
                set think [anthropic::jget $data usage output_tokens_details thinking_tokens]
                if {$think ne ""} { set usage(thinking_tokens) $think }
            }
            message_stop { break }
            error {
                set m [anthropic::jget $data error message]
                if {$m eq ""} { set m "stream error" }
                catch {::veles::http_stream_close $stream_id}
                return [anthropic::err $m [anthropic::status_of_type \
                    [anthropic::jget $data error type]]]
            }
            # ping, and anything a future wire adds, are ignored: a driver
            # that failed on an event it did not recognize would break the
            # moment Anthropic shipped one.
        }
    }
    catch {::veles::http_stream_close $stream_id}

    # Reassemble into the non-streaming shape.
    set content {}
    foreach idx [lsort -integer [array names kinds]] {
        set kind $kinds($idx)
        if {$kind eq "tool_use"} {
            set raw $partials($idx)
            # A fragment run that does not parse — a stream cut mid-argument
            # — becomes `{}`, the same thing the engine does for a malformed
            # tool call. Failing the turn over truncated arguments would
            # throw away the text that arrived fine beside them.
            if {$raw eq ""} { set raw "{}" }
            lappend content [anthropic::jobj [list \
                [list "type" [anthropic::jstr "tool_use"]] \
                [list "id" [anthropic::jstr $tool_ids($idx)]] \
                [list "name" [anthropic::jstr $tool_names($idx)]] \
                [list "input" $raw]]]
        } elseif {$kind eq "thinking"} {
            lappend content [anthropic::jobj [list \
                [list "type" [anthropic::jstr "thinking"]] \
                [list "thinking" [anthropic::jstr $blocks($idx)]]]]
        } elseif {$blocks($idx) ne ""} {
            # text, and only text: a server-tool block leaves `blocks` empty
            # because its content is not a reply to render, so it is skipped
            # here exactly as `from_wire` skips it.
            lappend content [anthropic::jobj [list \
                [list "type" [anthropic::jstr "text"]] \
                [list "text" [anthropic::jstr $blocks($idx)]]]]
        }
    }

    set upairs {}
    if {[info exists usage(input_tokens)]} {
        lappend upairs [list "input_tokens" [anthropic::jnum $usage(input_tokens)]]
    }
    if {[info exists usage(output_tokens)]} {
        lappend upairs [list "output_tokens" [anthropic::jnum $usage(output_tokens)]]
    }
    foreach f {cache_creation_input_tokens cache_read_input_tokens} {
        if {[info exists usage($f)]} {
            lappend upairs [list $f [anthropic::jnum $usage($f)]]
        }
    }
    if {[info exists usage(thinking_tokens)]} {
        lappend upairs [list "output_tokens_details" [anthropic::jobj [list \
            [list "thinking_tokens" [anthropic::jnum $usage(thinking_tokens)]]]]]
    }

    return [anthropic::from_wire [anthropic::jobj [list \
        [list "content" [anthropic::jarr $content]] \
        [list "stop_reason" [anthropic::jstr $stop_reason]] \
        [list "usage" [anthropic::jobj $upairs]]]]]
}

# One OpenAI-shaped streaming chunk carrying a single delta field. Split out
# because both the text and the reasoning arms need it, and hand-writing the
# same nesting twice is where the two would drift.
proc anthropic::delta_chunk {field text} {
    return [anthropic::jobj [list \
        [list "choices" [anthropic::jarr [list [anthropic::jobj [list \
            [list "delta" [anthropic::jobj [list \
                [list $field [anthropic::jstr $text]]]]]]]]]]]]
}

# ── the driver ──────────────────────────────────────────────────────────

proc anthropic::drive {} {
    set req $::veles::provider_request

    # The route's own endpoint and credential, from the reserved `veles` key
    # the engine adds to every driver request. NOT a global config lookup:
    # one driver serves every provider entry that names it, and a work
    # account and a personal one have different keys — as do two gateways
    # fronting the same wire.
    set base [::veles::json_get $req veles base_url]
    if {$base eq ""} { set base "https://api.anthropic.com" }
    set key [::veles::json_get $req veles api_key]
    set model [::veles::json_get $req model]

    # Capabilities are consulted when the request needs a decision they
    # inform, and there are TWO such decisions, not one:
    #
    #   * which thinking/effort/sampling shape this model accepts, when the
    #     engine asked for a reasoning effort;
    #   * what `max_tokens` to send, when the operator did not pin one —
    #     Anthropic REQUIRES that field, so the driver must produce a number,
    #     and its own fallback is 8192. On a model that takes 128000 that is
    #     a silent truncation of every long answer, which is exactly the kind
    #     of failure that reports as "the model stopped mid-sentence".
    #
    # Both are answered by the same cached catalog read, so consulting it on
    # a plain turn costs one GET per (endpoint, model) per process and then
    # nothing. A turn that neither thinks nor needs a discovered cap still
    # makes exactly one HTTP call.
    set needs_caps [expr {
        [::veles::json_get $req reasoning_effort] ne ""
        || [anthropic::configured_max_out $req] == 0
    }]
    if {$needs_caps} {
        set caps [anthropic::caps $base $key $model]
    } else {
        set caps [anthropic::unknown_caps]
    }

    set url "[anthropic::trim_base $base][anthropic::endpoint_path $req]"
    set headers [anthropic::headers_for $req $key]

    # Streaming, when the engine asked for it. The request body says so;
    # `stream` is on the engine's extra_body denylist because the transport
    # owns it, and here the transport is this driver.
    if {[::veles::json_get $req stream] eq "true"} {
        return [anthropic::drive_stream [anthropic::to_wire $req $caps 1] $url $headers]
    }

    set body [anthropic::to_wire $req $caps 0]

    # A non-streaming answer arrives only once the model has written all of
    # it, so the call needs the time a whole generation takes, not
    # `http_post`'s 30 second default: a compaction summary blew through
    # that on every attempt and compaction never happened (2026-10-29).
    # The engine sends the figure (`veles.timeout_secs`, the 600 s its own
    # provider client allows), and only an engine that sends it has the
    # fifth `http_post` argument: an older one refuses five arguments, and
    # a user copy of this driver can outlive the engine it came with. So
    # the timeout goes to `http_post` exactly when the engine named one.
    set timeout [::veles::json_get $req veles timeout_secs]
    set post [list ::veles::http_post $url $body "application/json" $headers]
    if {[string is entier -strict $timeout]} { lappend post $timeout }
    if {[catch {
        set raw [{*}$post]
    } err]} {
        # `http_post` raises on non-2xx with the body in the message, so the
        # status has to be read back out of it — that is what lets a 429
        # reach the engine as a rate limit and drive failover, instead of
        # arriving as an undifferentiated driver failure.
        return [anthropic::err [anthropic::error_message $err] [anthropic::status_of $err]]
    }

    # Anthropic reports API errors in a 200-shaped envelope too, so this is a
    # second check and not a redundant one. `jget` because a 200 from a
    # misconfigured gateway can carry a non-JSON body, and a raise here would
    # surface as a Network failure instead of "the body was not usable".
    set etype [anthropic::jget $raw error type]
    if {$etype ne ""} {
        return [anthropic::err [anthropic::error_message $raw] \
            [anthropic::status_of_type $etype]]
    }

    set out [anthropic::from_wire $raw]

    # A non-streaming call still emits its text as one chunk. That is the
    # honest version of "this path does not stream": the surface shows the
    # answer arriving rather than the engine silently deciding nothing was
    # said until the turn ended.
    set content [::veles::json_get $out choices 0 message content]
    if {$content ne ""} {
        ::veles::provider_emit [anthropic::delta_chunk "content" $content]
    }
    return $out
}

# ── errors ──────────────────────────────────────────────────────────────

proc anthropic::err {msg status} {
    return [anthropic::jobj [list \
        [list "error" [anthropic::jstr $msg]] \
        [list "status" [anthropic::jnum $status]]]]
}

# The status to report. This is the half of error handling that decides
# whether the engine retries, falls back to the next route, or fails the
# turn: `should_fallback` fires on 429 and 5xx and nothing else, so a driver
# that reported everything as a generic failure would take a plugin route out
# of failover entirely.
proc anthropic::status_of {raw} {
    if {[regexp {http (\d\d\d)} $raw -> code]} { return $code }
    return 502
}

# Anthropic's own error TYPE → a status. The envelope arrives with HTTP 200
# from some gateways, so the type is the only evidence, and mapping it is what
# lets an overloaded_error fall back instead of failing the turn.
proc anthropic::status_of_type {type} {
    switch -- $type {
        rate_limit_error      { return 429 }
        overloaded_error      { return 529 }
        api_error             { return 500 }
        authentication_error  { return 401 }
        permission_error      { return 403 }
        not_found_error       { return 404 }
        request_too_large     { return 413 }
        invalid_request_error { return 400 }
        default               { return 502 }
    }
}

# The message the user should see: the endpoint's own error text when there
# is one, else the transport's. Extracted from a JSON error envelope if the
# payload has one, because "http 429: {"type":"error",...}" is a wall of
# punctuation where "rate_limit_error: too many tokens" is an answer.
# Returns "" when there is nothing worth showing.
proc anthropic::error_message {raw} {
    if {$raw eq ""} { return "" }
    # `jget` throughout, and this is the site that made the difference: this
    # proc is handed transport failure strings ("http: error sending request
    # for url …"), which are not JSON at all. With `json_get` it raised, the
    # error escaped `drive`, and the engine reported a Network failure —
    # so a real 429 from the endpoint never reached the status mapping and
    # the route stopped failing over.
    set type [anthropic::jget $raw error type]
    if {$type ne ""} {
        return "$type: [anthropic::jget $raw error message]"
    }
    # `http_post`'s raise shape: "http NNN: <body>". Try the body as JSON.
    if {[regexp {http \d\d\d: (.*)$} $raw -> body]} {
        set type [anthropic::jget $body error type]
        if {$type ne ""} {
            return "$type: [anthropic::jget $body error message]"
        }
    }
    return $raw
}

# ── JSON, by hand ───────────────────────────────────────────────────────
#
# See the header comment for the convention. Every value passed to jobj and
# jarr is already a JSON fragment; every scalar is wrapped at its call site.

# A `::veles::json_get` that cannot raise.
#
# The engine's json_get ERRORS on malformed input (it is `Err` in
# `bridgeops`, which becomes a Tcl error), and it only returns "" for a MISS
# inside otherwise-valid JSON. That distinction is the whole reason this
# wrapper exists: a driver hands json_get three kinds of string that are NOT
# guaranteed to be JSON — a transport failure message, a gateway that
# answers a catalog request with HTML, a truncated SSE event — and an error
# there would abort `drive` mid-translation and surface as a Network failure.
# Concretely, that is how a real 429 became "driver failed: json_get: bad
# json" and the route stopped failing over.
#
# Used only where the input may not be JSON this driver built. The request
# body and a successful response are engine-produced JSON and go through
# json_get directly.
proc anthropic::jget {json args} {
    if {[catch {::veles::json_get $json {*}$args} out]} { return "" }
    return $out
}

proc anthropic::jstr {s} {
    set out ""
    foreach ch [split $s ""] {
        scan $ch %c code
        switch -- $ch {
            "\"" { append out {\"} }
            "\\" { append out {\\} }
            "\n" { append out {\n} }
            "\r" { append out {\r} }
            "\t" { append out {\t} }
            "\b" { append out {\b} }
            "\f" { append out {\f} }
            default {
                if {$code < 32} {
                    append out [format {\u%04x} $code]
                } else {
                    append out $ch
                }
            }
        }
    }
    return "\"$out\""
}

# A number, or 0. `string is double -strict` accepts the integer and decimal
# forms the wire uses and rejects everything else, so a non-numeric value
# that reached here becomes 0 rather than emitting invalid JSON — a wrong
# number is recoverable, a malformed body is a 400.
proc anthropic::jnum {v} {
    if {[string is double -strict $v]} { return $v }
    return "0"
}

proc anthropic::jarr {items} {
    return "\[[join $items ,]\]"
}

proc anthropic::jobj {pairs} {
    set parts {}
    foreach p $pairs {
        lappend parts "[anthropic::jstr [lindex $p 0]]:[lindex $p 1]"
    }
    return "\{[join $parts ,]\}"
}

# Is `v` a JSON array, as opposed to plain text that merely begins with one?
#
# `string index`, not `string match "\[*"`: in a glob pattern an unmatched
# `[` opens a character class, so that match silently never fired and every
# parts array fell through to the string branch — which is how an attachment
# became a stringified JSON blob instead of an image block. Pinned by
# driver_parity's attachment test (calibrated: reverting that fix fails it).
#
# But a first character is not a type. Message content is EITHER a parts
# array OR plain text, and plain text that happens to start with a literal
# `[` is common: the engine wraps a message that crossed surfaces as
# "[origin] this message arrived via the other surface". Sent down the parts
# path, that text reached `json_get`, which RAISES on input it cannot parse
# (bridge's `parse_json_lenient` tries the text, then the text braced, and
# gives up), aborting `drive` mid-translation. The turn died with "driver
# failed: json_get: bad json: expected value at line 1 column 2" — column 2,
# the character after the `[` that was mistaken for a bracket.
#
# So: parse it, and only then look at the first character. For a document
# that parses, the first non-blank character does determine the type, which
# makes this the same test the Rust driver's `Value::Array` arm makes.
proc anthropic::is_json_array {v} {
    if {![anthropic::parses_as_json $v]} { return 0 }
    return [expr {[string index [string trimleft $v] 0] eq "\["}]
}

proc anthropic::is_json_object {v} {
    if {![anthropic::parses_as_json $v]} { return 0 }
    return [expr {[string index [string trimleft $v] 0] eq "\{"}]
}

# Does `v` parse as JSON at all, of any shape?
#
# `json_get` answers "" for a MISS inside valid JSON and raises for input it
# cannot parse, so a key that cannot match is a parse probe: "" means it
# parsed. Same distinction `jget` exists to exploit, used for the shape
# question rather than to swallow an error.
#
# The probe key is long and unlikely enough that no real payload carries it;
# a document that did would still answer "" and so still count as JSON,
# which is the right answer anyway.
proc anthropic::parses_as_json {v} {
    return [expr {![catch {::veles::json_get $v __veles_shape_probe__}]}]
}

# Is message `i`'s content a parts ARRAY in the request the engine sent, or
# a string?
#
# Asked of the REQUEST, not of the extracted content: json_get hands back a
# string node and an array node alike, as text, so `is_json_array` on that
# text cannot tell a parts array from a message whose text merely parses as
# one — a tool's `[1, 2, 3]`, a user pasting a JSON list. Such a message went
# down the parts path, the loop found no parts, and a user message vanished
# from the request outright. Indexing INTO the node succeeds only when it
# really is an array, which is the same question the Rust driver's
# `Value::Array` arm asks.
proc anthropic::content_is_parts {req i} {
    return [expr {[::veles::json_get $req messages $i content 0] ne ""}]
}

# The text of an engine parts array, and the cache_control on any of its
# parts — a two-element list. Used where the wire block must carry the same
# bytes whether or not the engine marked it: a tool_result keeps its plain
# string content, so the copy marked this round and the unmarked copy every
# later round re-sends are the same prefix; the system prompt is one joined
# string. The text goes in, and the breakpoint moves onto the block itself.
proc anthropic::parts_text {content} {
    set text ""
    set cache ""
    for {set p 0} {1} {incr p} {
        set t [::veles::json_get $content $p type]
        if {$t eq ""} { break }
        if {$t eq "text"} {
            append text [::veles::json_get $content $p text]
        }
        set cc [::veles::json_get $content $p cache_control]
        if {$cc ne ""} { set cache $cc }
    }
    return [list $text $cache]
}

# Is this an already-built user message holding only tool_result blocks? The
# merge in `to_wire` needs to know without re-parsing, and the shape it built
# is the shape it tests for.
proc anthropic::is_tool_result_msg {msg} {
    if {$msg eq ""} { return 0 }
    if {![string match {*"role":"user"*} $msg]} { return 0 }
    return [string match {*"type":"tool_result"*} $msg]
}

::veles::register_provider anthropic-messages anthropic::drive

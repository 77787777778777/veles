# tcl-alias-filter — example: a pre_gateway_dispatch filter.
#
# Hook contract (frontends/gateway.rs hooks_fire_gateway):
#   core sets ::veles::hook_args = JSON{platform, chat_id, sender, text}
#   handler returns "" (no opinion) or one of
#     {"action":"skip"}                  — drop the message entirely
#     {"action":"rewrite","text":"..."}  — replace the turn text
#     {"action":"allow"}                 — explicit pass-through
#
# First non-empty return wins (hermes transform-hook semantics), here
# and in the Rust filter that runs after this hook.
#
# What this example does: `!ping` becomes a canned prompt, so the model
# answers without the user typing more than the alias. Everything else
# passes through untouched.

namespace eval alias_filter {}

proc alias_filter::handle {} {
    set text [string trim [::veles::json_get $::veles::hook_args text]]
    if {$text eq "!ping"} {
        # Escaped-JSON return (reasoning-summary's spelling): the
        # gateway parses this as the rewrite decision.
        return "{\"action\":\"rewrite\",\"text\":\"Reply with exactly one word: pong\"}"
    }
    return ""
}

::veles::hook pre_gateway_dispatch alias_filter::handle

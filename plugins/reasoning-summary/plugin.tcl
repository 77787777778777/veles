# reasoning-summary — first real veles Tcl plugin.
#
# Direct port of hermes' reasoning-summary plugin (~40 lines of Python →
# Tcl). Summarizes long reasoning blocks with a cheap aux model before
# they are displayed. DISPLAY-SIDE ONLY: the replay reasoning field is
# never touched — only display_reasoning is set (hermes invariant:
# msg["reasoning"] display vs reasoning_content replay).
#
# Hook contract:
#   core sets ::veles::hook_args = JSON{role, content, reasoning,
#     display_reasoning, provider, model}
#   handler returns "" (leave alone) or JSON{"display_reasoning": "..."}

namespace eval reasoning_summary {}

# Threshold: same 15-line cutoff as hermes' gateway truncation.
set ::reasoning_summary::min_lines 15

set ::reasoning_summary::system_prompt \
  "You are a reasoning summarizer. The user's AI assistant produced the \
following chain-of-thought / reasoning text while answering. Summarize \
it into a concise paragraph (2-4 sentences, at most ~120 words) that \
captures the key conclusions, the important steps, and any caveats. Do \
NOT include meta-commentary, self-correction noise, or filler. Output \
ONLY the summary text."

proc reasoning_summary::handle {} {
    variable min_lines
    variable system_prompt

    # Only assistant turns carry reasoning.
    set role [::veles::json_get $::veles::hook_args role]
    if {$role ne "assistant"} { return "" }

    set reasoning [::veles::json_get $::veles::hook_args reasoning]
    if {$reasoning eq ""} { return "" }

    # Line count threshold (hermes parity).
    set nlines [llength [split [string trimright $reasoning "\n"] "\n"]]
    if {$nlines < $min_lines} { return "" }

    # Fail-open: any error leaves the original reasoning displayed.
    if {[catch {
        ::veles::call_llm $system_prompt $reasoning
    } summary]} {
        # call_llm raised — keep the raw reasoning (fail-open).
        return ""
    }
    if {[string trim $summary] eq ""} { return "" }

    # Escape for JSON embedding.
    set summary [string map {"\\" "\\\\" "\"" "\\\"" "\n" "\\n" "\r" "\\r" "\t" "\\t"} $summary]
    return "{\"display_reasoning\": \"$summary\"}"
}

::veles::hook post_llm_call reasoning_summary::handle

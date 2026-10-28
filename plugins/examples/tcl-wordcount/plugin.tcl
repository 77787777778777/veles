# tcl-wordcount — example: register a real tool from Tcl.
#
# Demonstrates `veles::register_tool`:
#   - the JSON schema is passed inline (double-braced so the bridge
#     receives intact JSON after Tcl strips the quoting braces),
#   - the handler proc reads its arguments from `::veles::tool_args`
#     (a JSON object), and returns the tool's output string.
#
# Sessions created AFTER this plugin loads can call the tool; that is
# the documented reach of `register_global_tool` (same as adapter
# tools). Run it from a session with:
#   veles::tool_call wordcount {"text": "one two three"}

namespace eval wordcount {}

proc wordcount::run {} {
    # Arguments arrive as a JSON object in ::veles::tool_args.
    set text [::veles::json_get $::veles::tool_args text]
    if {$text eq ""} {
        return "error: wordcount needs a `text` argument"
    }
    # Split on whitespace; count the resulting words and characters.
    set words [llength [split [string trim $text]]]
    set chars [string length $text]
    return "words=$words chars=$chars"
}

# Schema: a JSON object with one string property. Double-braced so the
# outer Tcl quoting braces are stripped and the bridge parses real JSON.
set ::wordcount::schema {{"type":"object","properties":{"text":{"type":"string","description":"Text to count words and characters in"}},"required":["text"]}}

::veles::register_tool \
    wordcount \
    "Count the words and characters in a piece of text." \
    $::wordcount::schema \
    wordcount::run

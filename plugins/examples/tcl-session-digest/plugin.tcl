# tcl-session-digest — example: bridge reads + plugin-action notes.
#
# On session start, build a tiny digest of the store's newest session
# (its last three messages) and park it as a plugin-action note, where
# the next turn surfaces it. Pure reads + one note: the example works
# with NO provider configured, which is the point — the bridge is not
# just an LLM facade.
#
# Bridge commands demonstrated:
#   veles::sessions                JSON array, newest first
#   veles::history <sid> <limit>   {"session_id","total","messages":[...]}
#   veles::json_get ...            nested keys AND numeric array indices
#   veles::note <summary> ?detail? park a plugin-action note
#
# Hook contract: on_session_start args = JSON{"text": <user text>};
# returning "" leaves the turn untouched (this hook is informational).

namespace eval session_digest {}

# Sessions shorter than this are not worth digesting.
set ::session_digest::min_messages 3

proc session_digest::handle {} {
    variable min_messages

    # Newest session in the store (the one this turn belongs to, or the
    # one just before it on the very first turn — either way, relevant).
    set sessions [::veles::sessions]
    set sid [::veles::json_get $sessions 0 id]
    if {$sid eq ""} { return "" }

    set hist [::veles::history $sid 3]
    set total [::veles::json_get $hist total]
    if {$total eq "" || $total < $min_messages} { return "" }

    # Walk the messages array by numeric index.
    set lines {}
    for {set i 0} {$i < 3} {incr i} {
        set role [::veles::json_get $hist messages $i role]
        if {$role eq ""} { break }
        set content [string trim [::veles::json_get $hist messages $i content]]
        if {[string length $content] > 80} {
            set content "[string range $content 0 79]…"
        }
        lappend lines "$role: $content"
    }

    ::veles::note \
        "session digest for $sid ($total messages)" \
        [join $lines "\n"]

    # Informational hook: never touch the turn.
    return ""
}

::veles::hook on_session_start session_digest::handle

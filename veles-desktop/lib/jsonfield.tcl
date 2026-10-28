# veles-desktop/lib/jsonfield.tcl — one field out of a parsed JSON
# object, with the engine's `null` rendered as absence.
#
# WHY THIS FILE EXISTS
#
# tcllib's `::json::json2dict` maps JSON null to the four-character
# STRING "null". There is no second signal: a key whose value was null
# and a key whose value was the text "null" parse identically. Every
# `Option<T>` the engine serialises therefore arrives here as "null",
# and the natural Tcl idiom
#
#     catch { set v [dict get $d k] }
#     if {$v ne ""} { … }
#
# is TRUE for a field the engine did not set. Two live consequences,
# both reported by the user on 2026-09-18:
#
#   * `/api/sessions` sends `"parent": null` for every session that is
#     not a child (desktop_api.rs). The sidebar read it straight and
#     put the ↳ child glyph on all 563 rows — "All of the sessions in
#     the session list on desktop are showing the child icon".
#   * `/api/tasks` sends `"child_session": null` for every shell and
#     monitor (tasks.rs). The tasks overlay read it straight and gave
#     each one an [attach] link to a session named "null".
#
# Neither suite could see it, because both fixtures OMIT the key rather
# than sending null — a fixture that cannot produce the wire's shape
# tests the fixture. Both now build their rows through a real parse.
#
# THE TRADE IS DELIBERATE. A genuine string "null" is indistinguishable
# from a JSON null and comes back as $default. No field this program
# reads can legitimately hold it: ids, titles, session ids, model
# names, reasons. A field that could must read the dict directly and
# say in a comment why.
#
# Sourced by main.tcl immediately before api.tcl, and by the suites
# that exercise a reader. It is in ::vapi because it is about API
# responses; it lives in its own file because api.tcl drags in http,
# tls and the instance registry, and a suite that only renders a list
# should not have to.
#
# There are eleven older hand-rolled `ne "null"` guards in overlays.tcl
# and one in api.tcl. They are correct; they are also twelve copies of
# the rule this proc now states once. Converting them is a separate
# mechanical commit (PENDING).

namespace eval ::vapi {}

proc ::vapi::field {d key {default ""}} {
    if {![catch { dict get $d $key } v] && $v ne "null"} { return $v }
    return $default
}

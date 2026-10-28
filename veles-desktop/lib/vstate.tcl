# veles-desktop/lib/vstate.tcl — one read/merge/write for the durable
# desktop state blob (/api/desktop/state, backed by desktop_state.json).
#
# THE BUG THIS EXISTS TO END
#
# This file's own docstring used to claim it had replaced four modules'
# copies of the read-merge-write dance. It had not: sidebar pins, dock
# layout, editline history and the theme each still had their own, and
# between them they used THREE different Tcl→JSON serialisers that
# disagreed about what a list is. One caller — the titlebar — used this
# module. So which encoder ran depended on which setting you happened to
# change last, and picking a theme re-encoded everybody else's data.
#
# From the live state file on this machine:
#
#   "input_history": {"/mechanisms": {"Use":"zai","an":"xwayland", …},
#                     "/model": {"/model":"ox"}, …}
#
# `to_json` treats any even-length list as a dict — `dict size` succeeds
# on every even-length list — so a list of prompts became an OBJECT with
# each prompt's own words paired into keys and values. Half the composer
# history was destroyed, and JSON objects do not preserve order, so the
# shredded half is not recoverable. (`preview.label` went the same way:
# the string "X.Org logo" is stored as {"X.Org": "logo"}.)
#
# So: stop guessing. The blob has known keys with known types, declared
# once here, and one writer that encodes by the declaration.
#
#   ::vstate::all              the blob, repaired
#   ::vstate::get key ?default?
#   ::vstate::put key value    merges one key, never replaces the blob
#   ::vstate::merge dict       merges several
#   ::vstate::repairs          what the last read had to fix
#   ::vstate::error            why the last write failed
#
# `put`, not `set`: a proc named `set` in this namespace shadows the
# builtin for every other proc in it, which is a trap with no upside.
#
# Failure to WRITE is no longer soft. The desktop must still start when
# the backend is down — that is a read — but a preference that silently
# fails to save is a preference that comes back wrong on the next start
# with no explanation, which is exactly the class of bug this app keeps
# finding. `put` and `merge` return 0 and leave the reason in
# ::vstate::error; every caller says so.

namespace eval ::vstate {
    # key -> string | number | strings (JSON array) | counts (JSON
    #        object of string -> number) | pairs (JSON object of
    #        string -> string)
    # Anything absent falls through to to_json, so a key written by
    # newer code — or by the server, like override:<sid> — survives a
    # round trip rather than being dropped.
    variable schema {
        theme            string
        keymap           string
        wm_geometry      string
        native_titlebar  number
        sash0            number
        sash1            number
        composer_height  number
        pins             strings
        input_history    strings
        coach_seen       counts
        tab_current      string
        tab_layout       pairs
        irc_read_marker  counts
        irc_highlight_words strings
        app_grants       pairs
    }
    variable repaired {}
    variable error ""
}

# Serialize a Tcl value to JSON: nested dicts → objects, multi-element
# lists → arrays, the literal `null` → JSON null (so a round-tripped
# cleared override stays cleared), everything else an escaped string.
#
# Only for keys the schema does not declare. It is a guesser, and the
# guessing is why the history is gone.
proc ::vstate::to_json {v} {
    if {$v eq "null"} { return "null" }
    if {![catch {dict size $v} n] && $n > 0} {
        set parts {}
        dict for {k val} $v {
            lappend parts "\"[::vapi::json_escape $k]\": [to_json $val]"
        }
        return "\{[join $parts ", "]\}"
    }
    if {[llength $v] > 1} {
        set parts {}
        foreach item $v { lappend parts [to_json $item] }
        return "\[[join $parts ", "]\]"
    }
    return "\"[::vapi::json_escape $v]\""
}

proc ::vstate::encode {key value} {
    variable schema
    set type ""
    if {[dict exists $schema $key]} { set type [dict get $schema $key] }
    switch -- $type {
        number {
            if {[string is double -strict $value]} { return $value }
            # Never an array: a height is a number or, if something
            # mangled it, a string — but not a list of two numbers
            # because it happened to contain a space.
            return "\"[::vapi::json_escape $value]\""
        }
        strings {
            set parts {}
            foreach item $value { lappend parts "\"[::vapi::json_escape $item]\"" }
            return "\[[join $parts ", "]\]"
        }
        string { return "\"[::vapi::json_escape $value]\"" }
        pairs {
            # A JSON object of string -> string. The VALUES are opaque
            # here: tab_layout's are a compact payload owned entirely
            # by tabs.tcl. Opaque is the point — one JSON string per
            # entry is unambiguous coming back, where a nested object
            # would hand the shape problem to whoever parses it next.
            set parts {}
            foreach {k v} $value {
                lappend parts "\"[::vapi::json_escape $k]\": \"[::vapi::json_escape $v]\""
            }
            return "\{[join $parts ", "]\}"
        }
        counts {
            # A JSON object, always — the coach's per-command tally.
            # Non-numeric entries are dropped rather than written out,
            # so one bad value cannot make the whole key unreadable on
            # the next start.
            set parts {}
            foreach {k n} $value {
                if {![string is integer -strict $n]} { continue }
                lappend parts "\"[::vapi::json_escape $k]\": $n"
            }
            return "\{[join $parts ", "]\}"
        }
    }
    return [to_json $value]
}

# Was KEY written as a JSON object in this document? Exact, because JSON
# says so: an array opens with "[", an object with "{". Decided from the
# RAW text and not from the parsed value, because after parsing a
# four-string array and a two-pair object are both a four-element Tcl
# list — every shape heuristic that tries to tell them apart eats real
# data (two multi-word prompts look exactly like a mangled object).
proc ::vstate::_was_object {raw key} {
    return [regexp "\"[_re_escape $key]\"\\s*:\\s*\\\{" $raw]
}

proc ::vstate::_re_escape {s} {
    return [string map {\\ \\\\ ^ \\^ $ \\$ . \\. { [} { \[} ] \\] ( \\( ) \\) | \\| * \\* + \\+ ? \\? \{ \\\{ \} \\\}} $s]
}

# Coerce a value back to the shape its key is declared to have.
#
# For `strings`, the corrupt shape's KEYS are intact entries; its VALUES
# are the next entry with its words shredded into pairs, and the order
# is gone. Keep what survived, drop what did not, say how much.
proc ::vstate::_repair {key value was_object} {
    variable schema
    variable repaired
    if {![dict exists $schema $key]} { return $value }
    switch -- [dict get $schema $key] {
        strings {
            if {[catch { llength $value }]} { return {} }
            if {$was_object} {
                set out {}
                foreach {k v} $value { lappend out $k }
                lappend repaired \
                    "$key: recovered [llength $out] entries, [llength $out] lost to an older encoder"
                return $out
            }
            return $value
        }
        number {
            if {[string is double -strict $value]} { return $value }
            return ""
        }
        pairs {
            # Same shape rule as `counts`: an object is correct, an
            # array is corruption, and only the raw text can tell them
            # apart. Values need no validation — they are strings by
            # declaration.
            if {!$was_object} { return {} }
            if {[catch { dict size $value }]} { return {} }
            return $value
        }
        counts {
            # The inverse of `strings`: this one is SUPPOSED to be an
            # object, so an array is the corrupt shape. Same rule
            # either way — decide from the raw JSON, never from the
            # parsed value, because both parse to a flat Tcl list.
            if {!$was_object} { return {} }
            if {[catch { dict size $value }]} { return {} }
            set out {}
            dict for {k n} $value {
                if {[string is integer -strict $n]} { dict set out $k $n }
            }
            return $out
        }
    }
    return $value
}

# The blob, repaired. Never throws: a missing state file, an unreachable
# backend or an unparseable document is a first run, not an error.
proc ::vstate::all {} {
    variable repaired {}
    variable schema
    set st {}
    # Unattached: no request leaves the process; the local blob is
    # simply empty until an attach. (::vapi::unattached is 1 whenever
    # the module is at its default, which includes every legacy test
    # stub that never binds an instance — those return {} as before.)
    if {[llength [info procs ::vapi::unattached]] && [::vapi::unattached]} {
        return {}
    }
    if {[catch { ::vapi::get /api/desktop/state } raw]} { return {} }
    if {[catch { ::json::json2dict $raw } parsed]} { return {} }
    if {[catch { dict size $parsed }]} { return {} }
    dict for {k v} $parsed {
        # Ask the RAW document what shape this key was written in, for
        # every key whose declared type cares. `strings` is corrupt
        # when it is an object; `counts` is corrupt when it is not.
        set was_obj 0
        if {[dict exists $schema $k]
            && [dict get $schema $k] in {strings counts pairs}} {
            set was_obj [_was_object $raw $k]
        }
        dict set st $k [_repair $k $v $was_obj]
    }
    return $st
}

# What the last read had to fix, for a caller that wants to say so.
proc ::vstate::repairs {} { variable repaired ; return $repaired }

proc ::vstate::get {key {default ""}} {
    set st [all]
    if {[dict exists $st $key]} { return [dict get $st $key] }
    return $default
}

# Merge UPDATES into the stored blob and write it back. 1 on success;
# 0 with the reason in ::vstate::error.
#
# A read that FAILED is not an empty blob (2026-09-20: "something
# simply reset my keybindings to normie"). `all` answers {} both for a
# FIRST RUN (no file yet — writing the first keys is correct) and for a
# gateway that was too busy to answer — and merging into the second {}
# shipped a blob containing ONLY the new keys, erasing keymap, theme,
# layout, everything, on any put during a stall. Distinguish them by
# asking the GET directly: a FAILED read refuses the write and reports
# it, because the alternative is destroying every other module's state
# to save one key. A genuinely empty document reads as "" (present,
# zero-length) and merges as before.
proc ::vstate::merge {updates} {
    variable error ""
    # Unattached: no request leaves the process. Report the reason so a
    # preference that cannot be saved says so (a silent failure is what
    # this module's whole docstring is against).
    if {[llength [info procs ::vapi::unattached]] && [::vapi::unattached]} {
        set error "not attached"
        return 0
    }
    if {[catch { ::vapi::get /api/desktop/state } raw]} {
        set error $raw
        return 0
    }
    # Read-modify-write against the WHOLE blob, through the SAME
    # repair pass `all` runs — so a merge does not re-encode the old
    # encoders' mangled shapes back into the file.
    variable repaired
    set repaired {}
    set st {}
    if {![catch { ::json::json2dict $raw } parsed] && ![catch { dict size $parsed }]} {
        variable schema
        dict for {k v} $parsed {
            set was_obj 0
            if {[dict exists $schema $k]
                && [dict get $schema $k] in {strings counts pairs}} {
                set was_obj [_was_object $raw $k]
            }
            dict set st $k [_repair $k $v $was_obj]
        }
    }
    dict for {k v} $updates { dict set st $k $v }
    set parts {}
    dict for {k v} $st {
        lappend parts "\"[::vapi::json_escape $k]\": [encode $k $v]"
    }
    if {[catch { ::vapi::post /api/desktop/state "\{[join $parts ", "]\}" } e]} {
        set error $e
        return 0
    }
    return 1
}

proc ::vstate::put {key value} { return [merge [dict create $key $value]] }

# veles-desktop/lib/instances.tcl — the saved-gateway registry.
#
# A name → {url token} map of gateways this desktop can point at, kept in
# a plain JSON file on THIS machine:
#
#   $XDG_CONFIG_HOME/veles-desktop/instances.json   (mode 0600)
#
#   {"local": {"url": "http://127.0.0.1:8642"},
#    "vps":   {"url": "http://10.0.20.7:8642", "token": "…"}}
#
# WHY NOT ::vstate
#
# The first version of this rode ::vstate, which means it rode
# POST /api/desktop/state, which means `std::fs::write` inside whichever
# GATEWAY happened to be selected. Save the registry while connected to
# the VPS and the VPS's disk ends up holding the bearer tokens for every
# other gateway you use — at desktop_state.json's default permissions,
# next to a `/api/tcl/eval` route. Credentials for host B do not belong
# on host A, and a list of which machines you connect to is desktop-local
# state by nature: it is not something any one gateway should know.
#
# It also retires the encoding that arrangement forced. ::vstate's
# `pairs` type stores string → string, so two fields had to be packed
# into one value as "url<TAB>token" and split apart on the way back,
# resting on the claim that no URL or token can contain a tab.
#
# HAND-EDITABLE, deliberately: plain JSON, one object per instance, no
# packed fields. Fixing a typo'd URL should be `$EDITOR`, not a UI.
#
# THE WRITE IS ATOMIC AND NEVER WORLD-READABLE. Writing in place would
# leave the file 0644 for the moment between `open` and `file
# attributes`, with the token already in it. So: write a temp file in the
# same directory, chmod it, then rename over the target. Same directory
# because rename is only atomic within a filesystem.

package require json

namespace eval ::vinst {
    variable instances {}     ;# name -> {url <u> token <t> [host…] [unit…] [group…]}
    variable error ""
    variable last_attached ""
    variable remote_join {}   ;# target-issued join id + local return view
}

# The registry path. XDG first, then the usual default. Never $VELES_HOME
# — that is the GATEWAY's directory, and on a remote connection it is not
# even on this machine.
proc ::vinst::path {} {
    # The config BASE, by the same rule as every other path here
    # ($XDG_CONFIG_HOME, %APPDATA% on Windows, else ~/.config — see
    # ::vpaths::_xdg; config.rs's `desktop_registry_path` mirrors it for
    # `veles gui`). This used to read HOME itself, which Windows does not
    # set: the registry was never written there, every boot came up
    # unattached, and the device tool found no window (2026-10-13).
    if {[set p [::vpaths::profile]] ne ""} {
        return [file join [::vpaths::data_dir] instances.json]
    }
    set veles [::vpaths::_xdg XDG_CONFIG_HOME .config]
    if {$veles eq ""} { return "" }
    return [file join [file dirname $veles] veles-desktop instances.json]
}

proc ::vinst::error_text {} { variable error ; return $error }

# Read the registry. A missing file is a fresh install, not an error; an
# unreadable or malformed one IS reported, because a registry that
# silently comes back empty looks exactly like one that was never saved.
proc ::vinst::load {} {
    variable instances
    variable last_attached
    variable remote_join
    variable error ""
    set p [path]
    if {$p eq "" || ![file exists $p]} { set instances {} ; return 0 }
    if {[catch { set fh [open $p r] } e]} { set error $e ; return 0 }
    fconfigure $fh -encoding utf-8
    set raw [read $fh]
    close $fh
    if {[catch { ::json::json2dict $raw } parsed]} {
        set error "instances.json is not valid JSON: $parsed"
        return 0
    }
    if {[catch { dict size $parsed }]} {
        set error "instances.json is not a JSON object"
        return 0
    }
    set out {}
    dict for {name spec} $parsed {
        # The last-attached sidecar is not an instance; skip it in the
        # entry loop (it is read explicitly below).
        if {$name eq "__last_attached__" || $name eq "__remote_join__"} { continue }
        set url "" ; set tok ""
        catch { set url [dict get $spec url] }
        catch { set tok [dict get $spec token] }
        # A nameless or URL-less entry cannot be selected, so it is not an
        # entry. Dropping it quietly would hide a hand-edit typo.
        if {$url eq ""} {
            # …unless it is a bare string (someone flattened the
            # sidecar). Not an error either way.
            if {[catch { dict get $spec last } side]} {
                set error "instance \"$name\" has no url — ignored"
            }
            continue
        }
        set entry [list url $url token $tok]
        # Optional keys (Instances pane, PLAN Part 1). Absent means
        # absent: no host ⇒ no start/stop, no group ⇒ group by URL host.
        foreach k {host unit group} {
            set v ""
            catch { set v [dict get $spec $k] }
            if {$v ne ""} { dict set entry $k $v }
        }
        dict set out $name $entry
    }
    set instances $out
    set remote_join {}
    if {[dict exists $parsed __remote_join__]} {
        catch { set remote_join [dict get $parsed __remote_join__] }
    }
    # The sidecar key survives a load; a missing one is a first run.
    if {[dict exists $parsed __last_attached__]} {
        catch { set last_attached [dict get $parsed __last_attached__ last] }
    }
    return [dict size $out]
}

# Write the registry. 1 on success; 0 with the reason in ::vinst::error.
proc ::vinst::save {} {
    variable instances
    variable last_attached
    variable remote_join
    variable error ""
    set p [path]
    if {$p eq ""} { set error "no HOME or XDG_CONFIG_HOME" ; return 0 }
    set dir [file dirname $p]
    if {[catch { file mkdir $dir }] } {
        set error "cannot create $dir"
        return 0
    }
    set parts {}
    dict for {name spec} $instances {
        set url [dict get $spec url]
        set tok ""
        catch { set tok [dict get $spec token] }
        set fields "\"url\": \"[::vapi::json_escape $url]\""
        if {$tok ne ""} {
            append fields ", \"token\": \"[::vapi::json_escape $tok]\""
        }
        # Optional keys ride along when set, so a hand-edited file keeps
        # loading after a round trip through the pane's editor.
        foreach k {host unit group} {
            set v ""
            catch { set v [dict get $spec $k] }
            if {$v ne ""} {
                append fields ", \"$k\": \"[::vapi::json_escape $v]\""
            }
        }
        lappend parts "  \"[::vapi::json_escape $name]\": {$fields}"
    }
    # The last-attached sidecar, written only when set: restarting the
    # desktop reattaches to the last instance unless asked otherwise.
    if {$last_attached ne ""} {
        lappend parts "  \"__last_attached__\": {\"last\": \"[::vapi::json_escape $last_attached]\"}"
    }
    if {[dict size $remote_join]} {
        set fields {}
        foreach {k v} $remote_join {
            lappend fields "\"[::vapi::json_escape $k]\": \"[::vapi::json_escape $v]\""
        }
        lappend parts "  \"__remote_join__\": \{[join $fields {, }]\}"
    }
    set doc "{\n[join $parts ",\n"]\n}\n"

    # Temp file in the SAME directory: rename is atomic only within one
    # filesystem, and /tmp is frequently a different one.
    set tmp "$p.new[pid]"
    if {[catch {
        set fh [open $tmp w]
        fconfigure $fh -encoding utf-8
        puts -nonewline $fh $doc
        close $fh
        # Before the rename, so the token is never readable by anyone
        # else for even an instant.
        file attributes $tmp -permissions 0600
        file rename -force $tmp $p
    } e]} {
        catch { file delete -force $tmp }
        set error $e
        return 0
    }
    return 1
}

proc ::vinst::names {} {
    variable instances
    return [dict keys $instances]
}

# Bind EVERY saved instance into ::vapi, so `with_instance <row>` can
# reach any registry row — the Instances pane polls per-row, and a row
# that is not bound silently resolves to the DEFAULT gateway (api.tcl's
# `_conn` fallback), which is how the VPS row came to render the local
# gateway's version, uptime and session count, all green (2026-08-28).
# Called at boot (main.tcl) right after ::vinst::load, and by test
# suites that drive the pane without a real boot. Never clobbers a name
# already bound deliberately — env bindings win, and a remembered
# attach binding is already in place when this runs.
proc ::vinst::bind_all {} {
    variable instances
    foreach n [dict keys $instances] {
        if {[::vapi::instance_bound $n]} { continue }
        set spec [dict get $instances $n]
        set u "" ; set t ""
        catch { set u [dict get $spec url] }
        catch { set t [dict get $spec token] }
        if {$u ne ""} { ::vapi::bind_instance_inheriting $n $u $t }
    }
    return [dict size $instances]
}

proc ::vinst::get {name} {
    variable instances
    if {![dict exists $instances $name]} { return {} }
    return [dict get $instances $name]
}

# Add or replace one entry. Does NOT save — the caller decides when the
# registry is written, and does not have to want a write to want an
# in-memory entry.
proc ::vinst::add {name url {token ""}} {
    variable instances
    dict set instances $name [list url $url token $token]
    return $name
}

# Update ONE field of one entry, save, return 1/0. Not named `set`:
# a proc named `set` in this namespace shadows the builtin for every
# other proc in it — the same trap ::vstate documents for its own
# `put`.
proc ::vinst::update {name key value} {
    variable instances
    if {![dict exists $instances $name]} { return 0 }
    set spec [dict get $instances $name]
    if {$value eq ""} {
        dict unset spec $key
    } else {
        dict set spec $key $value
    }
    dict set instances $name $spec
    return [save]
}

# The host part of a URL, for grouping. Lives HERE rather than in the
# pane because ::vinst::groups needs it and the registry must not
# depend on the pane sourcing first.
proc ::vinst::host_of {url} {
    if {![regexp {^https?://([^/:]+)} $url -> h]} { return $url }
    return $h
}

# {group -> [names]}, groups sorted, names sorted — the shape the tree
# wants. An entry with no `group` groups by its URL's hostname.
proc ::vinst::groups {} {
    variable instances
    set out [dict create]
    dict for {name spec} $instances {
        set g ""
        catch { set g [dict get $spec group] }
        if {$g eq ""} {
            set url ""
            catch { set url [dict get $spec url] }
            set g [host_of $url]
            if {$g eq ""} { set g "?" }
        }
        dict lappend out $g $name
    }
    set sorted [dict create]
    foreach g [lsort [dict keys $out]] {
        dict set sorted $g [lsort [dict get $out $g]]
    }
    return $sorted
}

# A URL the client may actually dial. The registry is a file the
# operator edits by hand; a `file://` or a shell-ish string in there
# must not reach anything that opens a socket (or, worse, an ssh).
proc ::vinst::validate {url} {
    return [regexp {^https?://[A-Za-z0-9._:\[\]-]+} $url]
}

# The last instance the client was ATTACHED to, and its setter. This is
# desktop-local state like the window geometry; it lives beside the
# registry rather than in the gateway's blob. Persisted as a second key
# in the same file (kept out of the per-instance specs so it never
# round-trips as one).
proc ::vinst::last_attached {} {
    variable last_attached
    return $last_attached
}

proc ::vinst::set_last_attached {name} {
    variable last_attached
    set last_attached $name
    # Persist via the registry file's sidecar key. The in-memory value
    # is authoritative for the session even when the write fails.
    save
    return $name
}

proc ::vinst::remote_join {} { variable remote_join ; return $remote_join }

# Revalidate the saved target view after a desktop restart. This is a
# read-only target call: it does not create a join or leave one. Return
# {ok 1} only for the exact live binding; a refusal is a stale sidecar,
# not a licence to turn the target session into ordinary desktop chat.
proc ::vinst::validate_remote_join {} {
    variable remote_join
    if {![dict size $remote_join]} { return [dict create ok 1] }
    foreach k {join_id instance target_session from_instance from_session} {
        if {![dict exists $remote_join $k] || [dict get $remote_join $k] eq ""} {
            return [dict create ok 0 error "saved join is incomplete"]
        }
    }
    set inst [dict get $remote_join instance]
    if {$inst ne [::vapi::attached]} {
        return [dict create ok 0 error "saved join target is not attached"]
    }
    set path "/api/join/status?[::http::formatQuery \
        join_id [dict get $remote_join join_id] \
        session_id [dict get $remote_join target_session]]"
    if {[catch {::vapi::with_instance $inst [list ::vapi::get_json $path]} response]} {
        return [dict create ok 0 error "cannot validate saved join on $inst: $response"]
    }
    if {[dict exists $response error]} {
        return [dict create ok 0 error [dict get $response error]]
    }
    if {![dict exists $response join]} {
        return [dict create ok 0 error "saved join was not accepted by $inst"]
    }
    set live [dict get $response join]
    if {[dict get $live join_id] ne [dict get $remote_join join_id]
        || [dict get $live target_session] ne [dict get $remote_join target_session]} {
        return [dict create ok 0 error "saved join identity does not match the target"]
    }
    return [dict create ok 1]
}

# Restart restoration has a visible failure path. A bad binding is
# forgotten locally, then the saved origin is reattached and its session
# reopened. A failed local save is reported and never treated as success.
proc ::vinst::restore_remote_join {} {
    variable remote_join
    if {![dict size $remote_join]} { return 0 }
    set spec $remote_join
    set verdict [validate_remote_join]
    if {[dict get $verdict ok]} {
        ::vchat::open_session [dict get $spec target_session]
        return 1
    }
    set origin [dict get $spec from_instance]
    set sid [dict get $spec from_session]
    set why [dict get $verdict error]
    set saved [set_remote_join {}]
    if {$origin ne [::vapi::attached]} {
        if {![::vapp::attach $origin]} {
            ::vstatus::set_msg [format [::vi18n::mc {saved join refused: %s; restoring %s failed}] $why $origin]
            return -1
        }
    }
    ::vchat::open_session $sid
    set note [format [::vi18n::mc {saved join refused and left: %s}] $why]
    if {!$saved} { append note [format [::vi18n::mc {; could not save the cleared return view: %s}] [error_text]] }
    ::vchat::emit_message system $note
    ::vstatus::set_msg $note
    return -1
}

proc ::vinst::set_remote_join {spec} {
    variable remote_join
    set previous $remote_join
    set remote_join $spec
    if {[save]} { return 1 }
    # A failed disk write is not a new in-memory attachment. Keep the
    # caller and the next restart on the same last-good return view.
    set remote_join $previous
    return 0
}

proc ::vinst::remove {name} {
    variable instances
    if {![dict exists $instances $name]} { return 0 }
    dict unset instances $name
    return 1
}

# Point ::vapi at a saved instance. `keep_url` / `keep_token` let an
# explicit VELES_API / VELES_TOKEN win over the saved values — naming an
# instance should not silently override pointing at one.
#
# ONLY SAFE AT BOOT. Every pane, both SSE streams and the tab layouts
# hold data fetched from whichever gateway was current; changing the base
# URL underneath them leaves the previous gateway's sessions, files and
# scrollback on screen, labelled as the new one's. Live switching needs
# that invalidation pass and is its own piece of work — see PENDING.
proc ::vinst::select {name {keep_url 0} {keep_token 0}} {
    variable instances
    if {![dict exists $instances $name]} {
        load
    }
    if {![dict exists $instances $name]} {
        error "no saved instance named \"$name\" in [path]"
    }
    set spec [dict get $instances $name]
    if {!$keep_url}   { ::vapi::set_base  [dict get $spec url] }
    if {!$keep_token} { ::vapi::set_token [dict get $spec token] }
    return $name
}

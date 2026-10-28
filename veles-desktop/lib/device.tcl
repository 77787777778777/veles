# veles-desktop/lib/device.tcl — what this window can do for the agent
# (PLAN-sessions-and-phone.md A0/A1, USER decision D3).
#
# The window says what it can do — each capability a name, a line the
# agent reads, a JSON schema for its arguments — and listens for calls.
# The agent's `device` tool lists what every connected window offers and
# calls one; the gateway pushes the call HERE, to this window and no
# other, the provider runs, and the answer is posted back.
#
# On a phone (AndroWish) the providers reach Android through `borg`:
# notifications, speech, location, intents, sensors. On a laptop they do
# what a desktop can (the clipboard, a notification, opening a link). A
# provider is one proc: `::vdevice::provide name description schema proc
# ?-foreground 1? ?-timeout secs?`. The proc takes the call's arguments
# as a dict and returns the answer — plain text, or JSON wrapped with
# ::vapi::json_raw — or throws, and the agent reads the error.
#
# Shape: /api/desktop/eval's (dev.tcl), targeted. The stream is
# /api/device/events?client_id=…; the answer goes to /api/device/result.

namespace eval ::vdevice {
    # name -> dict {description schema proc foreground timeout}
    variable caps [dict create]
    variable tok ""
    variable client_id ""
    variable calls 0
    # The next reconnect's delay: a gateway that stopped (a phone's idles
    # out after five minutes) comes back, and the window must find it.
    variable retry_ms 2000
    variable retry_id ""
}

# Register (or replace) one capability. Safe before or after start: a
# registration while connected re-announces the catalog.
proc ::vdevice::provide {name description schema proc args} {
    variable caps
    set opts [dict merge {-foreground 0 -timeout 0} $args]
    dict set caps $name [dict create \
        description $description schema $schema proc $proc \
        foreground [dict get $opts -foreground] \
        timeout [dict get $opts -timeout]]
    variable tok
    if {$tok ne ""} { catch { _hello } }
}

proc ::vdevice::withdraw {name} {
    variable caps
    dict unset caps $name
    variable tok
    if {$tok ne ""} { catch { _hello } }
}

# The name the agent calls this device by. A phone is "phone" — the one
# the person carries; a laptop is its host name.
proc ::vdevice::device_name {} {
    if {[::vdevice::on_android]} { return phone }
    return [info hostname]
}

proc ::vdevice::on_android {} {
    return [expr {[info commands ::borg] ne ""}]
}

proc ::vdevice::platform {} {
    if {[on_android]} { return android }
    switch -- $::tcl_platform(os) {
        Darwin { return macos }
        "Windows NT" { return windows }
        default { return linux }
    }
}

# Stable for this window's life: a reconnect is the same client.
proc ::vdevice::client_id {} {
    variable client_id
    if {$client_id eq ""} {
        set client_id "[device_name]-[pid]-[clock milliseconds]"
    }
    return $client_id
}

# Open the request stream (boot, and after attaching to another gateway).
proc ::vdevice::start {} {
    variable tok
    stop
    if {[::vapi::unattached]} { return "" }
    set path "/api/device/events?client_id=[::vapi::urlencode [client_id]]"
    # PASSIVE: the window keeping its own line open, not a person using
    # the gateway — a phone's engine must still idle out
    # (veles_core::idle) with veles open and untouched on the screen.
    set ::vapi::passive 1
    set failed [catch { ::vapi::sse_open $path ::vdevice::_event } t]
    set ::vapi::passive 0
    if {$failed} {
        _retry
        return ""
    }
    set tok $t
    return $tok
}

proc ::vdevice::stop {} {
    variable tok
    variable retry_id
    if {$retry_id ne ""} {
        after cancel $retry_id
        set retry_id ""
    }
    if {$tok ne ""} {
        catch { ::vapi::sse_close $tok }
        set tok ""
    }
}

# Try again later, each time a little later (2 s, doubling to a minute).
proc ::vdevice::_retry {} {
    variable retry_ms
    variable retry_id
    if {$retry_id ne ""} { return }
    set retry_id [after $retry_ms {
        set ::vdevice::retry_id ""
        ::vdevice::start
    }]
    set retry_ms [expr {min($retry_ms * 2, 60000)}]
}

# The catalog, as the gateway takes it.
proc ::vdevice::_hello {} {
    variable caps
    set list {}
    foreach name [lsort [dict keys $caps]] {
        set c [dict get $caps $name]
        set schema [dict get $c schema]
        if {[string trim $schema] eq ""} { set schema "{}" }
        lappend list [::vapi::json_raw [format \
            {{"name": %s, "description": %s, "schema": %s, "foreground": %s, "timeout_secs": %d}} \
            "\"[::vapi::json_escape $name]\"" \
            "\"[::vapi::json_escape [dict get $c description]]\"" \
            $schema \
            [expr {[dict get $c foreground] ? "true" : "false"}] \
            [dict get $c timeout]]]
    }
    # Passive, like the stream: announcing is not using.
    set ::vapi::passive 1
    set failed [catch {
        ::vapi::post_json /api/device/hello [dict create \
            client_id [::vapi::json_string [client_id]] \
            name [::vapi::json_string [device_name]] \
            platform [::vapi::json_string [platform]] \
            capabilities [::vapi::json_array $list]]
    } e]
    set ::vapi::passive 0
    if {$failed} { error $e }
}

# One event off the stream.
proc ::vdevice::_event {event data} {
    switch -- $event {
        device-live {
            # The stream is open: say what we can do. Every reconnect
            # says it again — the gateway may have restarted. A failure
            # here is retried by the next reconnect; nothing to show.
            variable retry_ms
            set retry_ms 2000
            catch { _hello }
        }
        closed {
            # The gateway went away (a phone's engine idles out) or the
            # connection dropped: find it again when it is back.
            variable tok
            set tok ""
            _retry
        }
        request {
            set id ""
            set action ""
            set a {}
            catch { set id [dict get $data id] }
            catch { set action [dict get $data action] }
            catch { set a [dict get $data args] }
            if {$id eq ""} { return }
            # Off the event handler: a provider may wait (speech, a
            # location fix), and the stream must keep reading.
            after idle [list ::vdevice::_run $id $action $a]
        }
    }
}

# `a`, never `args`: a proc's last parameter named args collects the
# remaining arguments as a LIST, and every provider would get its dict
# wrapped in one.
proc ::vdevice::_run {id action a} {
    variable caps
    variable calls
    incr calls
    if {![dict exists $caps $action]} {
        _answer $id 0 "this device has no action $action"
        return
    }
    set proc [dict get $caps $action proc]
    if {[catch { {*}$proc $a } result]} {
        _answer $id 0 $result
    } else {
        _answer $id 1 $result
    }
}

proc ::vdevice::_answer {id ok result} {
    if {[llength $result] == 2 && [lindex $result 0] eq "::vapi::__json__"} {
        set value $result
    } else {
        set value [::vapi::json_string $result]
    }
    # A lost answer leaves the agent waiting out its timeout: say why,
    # where a person debugging the window will look.
    if {[catch {
        ::vapi::post_json /api/device/result [dict create \
            id [::vapi::json_string $id] ok [::vapi::json_bool $ok] result $value]
    } e]} {
        catch { puts stderr "veles-desktop: the answer to device call $id was not delivered: $e" }
    }
}

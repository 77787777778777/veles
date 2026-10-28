# veles-desktop/lib/api.tcl — REST + SSE client for the desktop API.
#
# Sync helpers (api_get/api_post/api_delete) block the CALLER but not the
# event loop; used for list/status calls that return in milliseconds.
# Chat uses the async SSE path (api_chat_sse) built on Tcl core http's
# -handler/-command mode — the proven M5 pattern: one blocking gets per
# fileevent invocation, handler returns the byte count.
#
# NOTE (checked 2026-08-23, http 2.9.8): calling `http::geturl` WITHOUT
# -command is not a UI freeze. Its synchronous path ends in `http::wait`,
# whose body is `vwait ${token}(status)` — a live event loop — so Tk keeps
# redrawing for the whole request. An audit of this file claimed these
# calls froze the window for the 10s timeout; they do not. Verified by
# driving the real gateway with a self-rescheduling `after 1` timer and
# counting ticks during 20 requests: 48 ticks, i.e. the loop kept running.
# Do not "fix" this with a -command/vwait rewrite; it buys nothing and
# adds a nested-vwait re-entrancy hazard.
#
# ENCODING (fixed 2026-08-10): every HTTP read is forced to UTF-8.
# Without `-binary`, Tcl's http package decodes the body with the
# SYSTEM encoding, which falls back to iso8859-1 when the launched wish
# has a non-UTF-8 locale (LANG=C or unset) — producing the classic
# mojibake (em-dash → "â€”", curly quotes → "â€™"). The gateway always
# sends UTF-8 JSON, so we read raw bytes and decode explicitly.

package require http
package require json

# MULTI-INSTANCE RULE (read before adding any async path):
#
# `with_instance` is DYNAMIC scope — it restores `current` on the way
# out, including on error. Any request that RETURNS LATER (sse_open,
# post_async's callback) has already resolved its url and token at call
# time, which is correct; but anything the callback derives from the
# CURRENT instance afterwards is wrong, because `current` is back to
# whatever it was. That is why the SSE layer captures `url` and
# `instance` into `_sse(...)` per token at OPEN time: `_sse_finished`
# names the gateway the stream actually talked to, not whichever one is
# current when the socket dies. A new async path must do the same.
#
# The same trap has a second half at the APP level: an in-flight reply
# from gateway A must not paint into gateway B's panes after an attach.
# So every async path here also captures the ATTACH EPOCH
# (::vapp::epoch) at call time and refuses to dispatch its result if the
# epoch moved while it was out — see `_epoch_now` / `_epoch_fresh`,
# `_sse_finished` and `_post_async_done`. Closing the stream is not
# enough on its own: the teardown closes it, but the socket's own
# -command still fires afterwards with whatever the kernel had already
# buffered, and that callback lands in a world that has moved on.
#
# ::vapp is OPTIONAL here on purpose — api.tcl loads in suites that
# never source attach.tcl, and it must keep working there. No ::vapp
# means one epoch forever, which is exactly right for a process that
# cannot attach.

namespace eval ::vapi {
    variable base "http://127.0.0.1:8642"
    if {[info exists ::env(VELES_DESKTOP_PORT)] && $::env(VELES_DESKTOP_PORT) ne ""} {
        set base "http://127.0.0.1:$::env(VELES_DESKTOP_PORT)"
    }
    variable timeout 10000
    # Multi-instance addressing. `base` stays the URL of the DEFAULT
    # gateway (the one `set_base`/`set_token` point at — today, the
    # boot-time or env-provided one), so every legacy `_url` call is
    # unchanged. The REGISTRY of saved gateways lives in
    # lib/instances.tcl, on this machine — not here, and emphatically
    # not in the gateway's own state blob.
    variable token ""
    # The ATTACHED instance: a name bound below, or "" when the client
    # is unattached. When set, `_conn` resolves through it and `base`/
    # `token` are ignored; the Instances pane reaches instances OTHER
    # than the attached one through `with_instance`, which layers a
    # dynamic override on top without disturbing the attachment.
    variable current ""
    variable instances        ;# name -> {url <u> token <t>}
    array set instances {}
}

# Register an instance's connection details under a name. Binding is
# local to the process; the durable copy is ::vinst's registry file.
proc ::vapi::bind_instance {name url token} {
    variable instances
    set instances($name) [list url $url token $token]
    return $name
}

# Bind a registry row, inheriting the DEFAULT token when the row has
# none of its own and points at the same origin.
#
# THE BUG THIS EXISTS FOR (2026-09-11). The gateway mints a bearer into
# <config_dir>/.env on first boot and the launcher hands it to the
# desktop as VELES_TOKEN (veles-cli/src/lib.rs:417). `main.tcl` puts it
# in ::vapi::token. But `_conn` resolves an ATTACHED instance through
# the instances table and ignores base/token entirely — so a registry
# row written before token minting existed, which has no `token` key,
# bound as token "" and every request went out with no Authorization
# header. The boot probe got 401 and the app exited telling the operator
# to "check VELES_TOKEN", which was correct all along.
#
# SAME ORIGIN IS THE WHOLE POINT. A saved REMOTE gateway must never be
# handed the local gateway's credential just because it has none of its
# own — that would post the operator's bearer to whatever host the row
# names. The comparison is deliberately strict: a false negative is a
# 401 with a message that now names the instance, while a false positive
# mails a credential to a third party.
proc ::vapi::_origin {url} {
    set u [string trim $url]
    if {[regexp {^([a-zA-Z][a-zA-Z0-9+.-]*://[^/]+)} $u -> o]} {
        return [string tolower $o]
    }
    return [string tolower [string trimright $u /]]
}

proc ::vapi::bind_instance_inheriting {name url tok} {
    variable base
    variable token
    if {$tok eq "" && $token ne "" && [_origin $url] eq [_origin $base]} {
        set tok $token
    }
    return [bind_instance $name $url $tok]
}

# Does this bound instance have a token? The Instances pane shows it, so
# a pre-minting row is visible as such instead of being inferred from a
# 401 three steps later.
proc ::vapi::instance_has_token {name} {
    variable instances
    if {![info exists instances($name)]} { return 0 }
    return [expr {[dict get $instances($name) token] ne "" ? 1 : 0}]
}

# Run `script` with `name` as the current instance. Restores the
# previous one on the way out, INCLUDING ON ERROR — a failed request
# must not leave the app pointed somewhere the user did not choose.
#
# DYNAMIC SCOPE: useless around anything asynchronous (see the rule at
# the top of this file). The Instances pane uses it around synchronous
# per-row polls only.
#
# UNBOUND NAMES ARE AN ERROR. `_conn` resolves `current` through the
# bound-instance table and FALLS BACK TO THE DEFAULT BASE for anything
# else — so a with_instance on a name nothing bound would silently probe
# the DEFAULT gateway while the caller believes it reached the named
# one. Hit live: boot bound only the attached row, the pane polled the
# VPS row unbound, and the VPS showed the LOCAL gateway's version,
# uptime and session count, all of them green. Fail here instead.
proc ::vapi::with_instance {name script} {
    variable current
    variable instances
    if {$name ne "" && ![info exists instances($name)]} {
        error "no such instance: $name"
    }
    set prev $current
    set current $name
    set rc [catch { uplevel 1 $script } res opts]
    set current $prev
    return -options $opts $res
}

# Attach the WHOLE CLIENT to `name`; "" detaches. Every pane talks to
# that gateway until the next attach. Must be called only AFTER
# ::vapp::teardown_for_attach has run against the outgoing instance —
# flipping the target underneath live panes is the cross-gateway leak
# the teardown checklist exists to prevent. Refuses an unbound name.
proc ::vapi::use_instance {name} {
    variable instances
    variable current
    if {$name ne "" && ![info exists instances($name)]} {
        error "no such instance: $name"
    }
    set current $name
    return $current
}

# The attached instance name, or "" when unattached.
proc ::vapi::attached {} {
    variable current
    return $current
}

# 1 when `name` has connection details bound (used by ::vapp::attach to
# avoid clobbering a deliberate boot-time binding with registry values).
proc ::vapi::instance_bound {name} {
    variable instances
    return [expr {[info exists instances($name)] ? 1 : 0}]
}

# The url a bound instance would be talked to at, or "".
proc ::vapi::instance_url {name} {
    variable instances
    if {![info exists instances($name)]} { return "" }
    return [dict get $instances($name) url]
}

# 1 when no gateway is attached. Modules that must not send requests
# while unattached test THIS, not `attached`, so a legacy stub without
# either proc keeps working in the older test suites.
proc ::vapi::unattached {} {
    variable current
    return [expr {$current eq ""}]
}

# The connection the next request will use: the attached/bound
# instance, or the default. An unknown name falls back to the default
# rather than producing a hostless URL — a typo'd instance name must
# not silently build "/api/status" with no origin.
proc ::vapi::_conn {} {
    variable current
    variable instances
    variable base
    variable token
    if {$current ne "" && [info exists instances($current)]} {
        return $instances($current)
    }
    return [list url $base token $token]
}

# The Authorization header for an ALREADY-RESOLVED connection, or
# nothing when its token is empty.
#
# Async paths must not call _auth_headers: that re-resolves through
# `current`, which is the whole trap. They resolve `_conn` once, at call
# time, and pass the dict here. An empty token yields NO header at all —
# the same as the synchronous path. The first version of the async
# capture sent a bare `Bearer ` on loopback, which this gateway's open
# mode ignores but a proxy in front of it need not.
proc ::vapi::_conn_headers {conn} {
    variable passive
    set h [::vapi::_locale_header]
    set t [dict get $conn token]
    if {$t ne ""} { lappend h Authorization "Bearer $t" }
    # A TIMER talking to the gateway, not a person: the engine's idle
    # clock (--exit-when-idle) must not count it as use, or an open-but-
    # untouched desktop would keep a phone's gateway up for ever. The
    # caller sets `passive` around the WHOLE passive operation (the 30 s
    # layout save does a GET and a POST, both passive) and clears it
    # itself — it is read here, never consumed, so a gesture made with
    # the flag unset is always a use. 2026-10-04: the layout save was
    # the one keeping the engine alive.
    if {[info exists passive] && $passive} {
        lappend h X-Veles-Passive 1
    }
    if {![llength $h]} { return {} }
    return [list -headers $h]
}

# The window's UI language, on EVERY request: the engine renders the text
# this window shows (command output, the help table, a turn's failover
# prompt) in the language of the request that asked — per request, so the
# gateway's IRC and Telegram users are not switched along with this window
# (crates/veles-core/src/i18n.rs, "per-request locale"). A session given
# its own language with /locale still wins on the engine side.
#
# One -headers list with the bearer: http::geturl does not merge two
# -headers options, the later one simply replaces the earlier.
#
# Empty when ::vi18n is not loaded, so suites that source api.tcl alone
# see exactly the headers they always did.
proc ::vapi::_locale_header {} {
    if {![llength [info commands ::vi18n::current]]} { return {} }
    return [list X-Veles-Locale [::vi18n::current]]
}

# ── the attach epoch, as api.tcl sees it ────────────────────────────
#
# ::vapp owns the counter (lib/attach.tcl); this is the optional-import
# side of it, so api.tcl keeps loading in suites that never source
# attach.tcl. Without ::vapp the epoch is a constant, every callback is
# fresh, and behaviour is exactly what it was before attach existed.
proc ::vapi::_epoch_now {} {
    if {![llength [info commands ::vapp::epoch]]} { return 0 }
    return [::vapp::epoch]
}

proc ::vapi::_epoch_fresh {e} {
    if {![llength [info commands ::vapp::fresh]]} { return 1 }
    return [::vapp::fresh $e]
}


proc ::vapi::set_base {url} {
    variable base
    set base $url
}

proc ::vapi::base {} {
    variable base
    return $base
}

proc ::vapi::token {} {
    variable token
    return $token
}

proc ::vapi::set_token {t} {
    variable token
    set token $t
}

# The bearer header for the current instance, or nothing on loopback.
proc ::vapi::_auth_headers {} {
    return [::vapi::_conn_headers [_conn]]
}

proc ::vapi::_url {path} {
    return "[dict get [_conn] url]$path"
}

# Decode raw HTTP bytes as UTF-8 (never the system/iso8859-1 default).
# THE OTHER HALF OF THE 2026-08-10 ENCODING FIX, missing until now.
#
# That fix added `-binary 1` so the RESPONSE is read as bytes and decoded
# explicitly — correct, and it is why the mojibake went away. But
# `-binary 1` also makes the socket binary in the OUTBOUND direction, and
# Tcl writes a string to a binary channel one low byte per character. So
# every POST body was sent with its non-ASCII characters truncated:
#
#   "—" U+2014 → 0x14        (a control byte; silently wrong)
#   "·" U+00B7 → 0xB7        (a lone continuation byte: INVALID UTF-8)
#
# The gateway rejects the second kind outright — "Request body didn't
# contain valid UTF-8" — so /api/fs/write could not save a file
# containing a middle dot or an accented letter, and /api/chat could not
# send a message with one. Found by driving the real Dev-pane LSP path
# against a live gateway: `didOpen` of this very file 400'd, because
# api.tcl contains a "·".
#
# The body is therefore encoded to UTF-8 bytes HERE, before Tcl sees it.
# Do NOT "fix" this instead by putting charset=utf-8 in -type: the http
# package would then encode the query itself, on top of this, and every
# non-ASCII character would go out doubly encoded.
proc ::vapi::_utf8_out {s} {
    if {[catch {encoding convertto utf-8 $s} bytes]} { return $s }
    return $bytes
}

proc ::vapi::_utf8 {bytes} {
    if {[catch {encoding convertfrom utf-8 $bytes} out]} {
        return $bytes
    }
    return $out
}

# `ms` overrides the default timeout for one call — the Instances pane
# polls potentially-dead hosts and must not let them hold the timer for
# the full 10s.
# A TRANSFER THAT DID NOT COMPLETE IS AN ERROR, not an empty body.
#
# `http::geturl` only THROWS when it cannot open the socket at all —
# connection refused, DNS failure. A transfer that starts and then times
# out, resets or hits EOF comes back as a perfectly ordinary token whose
# `status` is "timeout"/"reset"/"eof", whose `data` is "" and whose
# `ncode` is 0. Without this check `get` returned "" and the caller's
# `catch` never fired.
#
# Found by driving the Instances pane at a BLACK-HOLED host
# (10.255.255.1): its dot went GREEN and its detail said "reachable",
# because the 2s health poll timed out and the timeout read as success.
# Killing a gateway on loopback does not reproduce it — that is a
# refused connection, which geturl does throw on — which is why the
# earlier live check passed while the case the pane exists for did not.
proc ::vapi::_finish {tok} {
    set st [http::status $tok]
    set data [_utf8 [http::data $tok]]
    set code [http::ncode $tok]
    set why ""
    catch { set why [http::error $tok] }
    http::cleanup $tok
    if {$st ne "ok"} {
        error "transport: $st[expr {$why eq "" ? "" : " ($why)"}]"
    }
    if {$code >= 400} { error "HTTP $code: $data" }
    return $data
}

proc ::vapi::get {path {ms 0}} {
    variable timeout
    if {$ms <= 0} { set ms $timeout }
    return [_finish [http::geturl [_url $path] -binary 1 -timeout $ms \
        {*}[_auth_headers]]]
}

proc ::vapi::get_json {path {ms 0}} {
    return [::json::json2dict [get $path $ms]]
}

# Async GET with a completion callback: `$cmd $ok $value` — on success
# VALUE is the parsed JSON, on failure it is the error text. The
# connection is CAPTURED at call time, exactly like chat_sse, so a
# re-attach between fire and finish cannot hand the reply to the wrong
# gateway. The caller paints only when the epoch is still fresh.
#
# Exists for the session-switch render cache: verifying a cached
# transcript must not block the UI thread on the very operation the
# cache makes instant.
proc ::vapi::get_json_async {path cmd {ms 0}} {
    variable timeout
    variable current
    # The namespace's table: without this line the entry below was a
    # local that died with the call, and no reply was ever handed on.
    variable _async_done
    if {$ms <= 0} { set ms $timeout }
    set conn [_conn]
    set epoch [_epoch_now]
    set tok [http::geturl [dict get $conn url]$path -binary 1 -timeout $ms \
        -command ::vapi::_get_json_async_done {*}[::vapi::_conn_headers $conn]]
    set _async_done($tok) [list $cmd $epoch]
    return $tok
}

proc ::vapi::_get_json_async_done {tok} {
    variable _async_done
    if {![info exists _async_done($tok)]} { return }
    lassign $_async_done($tok) cmd epoch
    unset _async_done($tok)
    set ok 1
    set val ""
    if {[catch {_finish $tok} raw]} {
        set ok 0
        set val $raw
    } elseif {[catch {::json::json2dict $raw} d]} {
        set ok 0
        set val $d
    } else {
        set val $d
    }
    # A re-attach made this reply stale — never hand it to the painter.
    if {![_epoch_fresh $epoch]} { return }
    # `cmd` is a command PREFIX (`::vchat::_redraw_asks $sid`): called as
    # one word it named no command, and the catch hid that.
    catch { uplevel #0 [list {*}$cmd $ok $val] }
}

proc ::vapi::post {path body {ms 0}} {
    variable timeout
    if {$ms <= 0} { set ms $timeout }
    set tok [http::geturl [_url $path] -method POST -type application/json \
        -binary 1 -query [_utf8_out $body] -timeout $ms {*}[_auth_headers]]
    set st [http::status $tok]
    set data [_utf8 [http::data $tok]]
    set code [http::ncode $tok]
    set why ""
    catch { set why [http::error $tok] }
    http::cleanup $tok
    # Same rule as `get`: a transfer that timed out or reset is a
    # failure, not an empty reply. A POST is worse than a GET here — a
    # reload that timed out must never report "reloaded".
    if {$st ne "ok"} {
        error "transport: $st[expr {$why eq "" ? "" : " ($why)"}]"
    }
    if {$code >= 400} { error "HTTP $code: $data" }
    return $data
}

proc ::vapi::delete {path} {
    variable timeout
    return [_finish [http::geturl [_url $path] -method DELETE -binary 1 \
        -timeout $timeout {*}[_auth_headers]]]
}

# JSON string escaping.
#
# THE BUG THIS HAD, for four months: the first pair of the map read
#
#     string map {"\\\\" "\\\\"  …}
#
# and `string map` list-parses its argument, so BOTH elements came out as
# two backslashes — a rule that maps `\\` to `\\` and does nothing at
# all. A LONE backslash was therefore emitted raw, and JSON reads what
# follows it as an escape. `a\b` came back from the gateway as `a`: not a
# parse error, just a shorter string. Every JSON body the desktop writes
# went through here — /api/fs/write (so saving any file containing a
# backslash through the Dev pane corrupted it), the ::vstate blob, chat
# messages. lspclient.tcl's _jstr had the same map written correctly
# (`"\\"`, one backslash), which is what makes it a typo rather than a
# theory.
#
# Built with [list] and literal characters now, so the escaping of this
# file's own source cannot change what the map means. Backslash goes
# FIRST; `string map` scans left to right and never re-reads what it has
# already emitted, so a replacement containing backslashes is safe.
#
# The \u00XX tail covers the rest: JSON forbids raw control characters in
# strings, and a stray \x01 in a filename or a paste would otherwise
# produce a document the gateway cannot parse.
proc ::vapi::json_escape {s} {
    variable _esc_map
    return [string map $_esc_map $s]
}

namespace eval ::vapi {
    variable _esc_map [list \\ "\\\\" \" "\\\"" \n "\\n" \r "\\r" \t "\\t" \b "\\b" \f "\\f"]
    for {set _i 0} {$_i < 32} {incr _i} {
        set _c [format %c $_i]
        if {[lsearch -exact $_esc_map $_c] >= 0} { continue }
        lappend _esc_map $_c [format {\u%04x} $_i]
    }
    unset -nocomplain _i _c
}

# Serialize any Tcl value as a JSON value (string, list→array, dict-
# shaped even-lists→object, numbers pass through). Enough for the
# desktop's POST bodies; the gateway is the only consumer.
#
# NOTE the n==1 base case is load-bearing: without it a single non-
# numeric word (e.g. "6*7") recursed into itself forever — the REPL
# probe caught the hang. Arbitrary user text must NOT be re-read as a
# structure; callers that carry opaque code/content build the JSON
# themselves with json_escape (see lib/dev.tcl).
# ── saying what you mean ────────────────────────────────────────────
#
# `_json_value` INFERS a type from an untyped Tcl value, which is the
# only thing it can do and is wrong in two ways that matter:
#
#   * There is no boolean. `true` is a one-word list, so it encodes as
#     the STRING "true" — and `body["enabled"].as_bool()` on the engine
#     side answers None and falls back to its default. A hook toggle
#     built this way always sent "enable", whichever way you clicked.
#   * A ONE-ELEMENT list is indistinguishable from a scalar, so a list
#     of one entry encodes as a string. Saving a single memory entry was
#     refused with "send `entries` (a list)".
#
# Rather than deepen the guessing, these let a caller state the type.
# `json_raw` wraps pre-rendered JSON in a tagged pair that `_json_value`
# emits verbatim; the tag is namespaced so no real payload collides.
#
# `json_string` is the one to reach for when a VALUE is free text: the
# dict-shaped heuristic below infers from the flattened list, so a
# value like "steer me somewhere else" flattens to an even-length,
# identifier-keyed list and serializes as a nested OBJECT — which is
# how every steer with an odd number of words hit
# `{"error":"message required"}` (HTTP 400) while "no-spaces" worked.
# USER: "steer doesn't work at all".

proc ::vapi::json_raw {text} {
    return [list ::vapi::__json__ $text]
}

proc ::vapi::json_bool {v} {
    return [json_raw [expr {$v ? "true" : "false"}]]
}

# Force an ARRAY, whatever its length. Elements still go through the
# inference, so use this for structured ones.
proc ::vapi::json_array {items} {
    set parts {}
    foreach i $items { lappend parts [_json_value $i] }
    return [json_raw "\[[join $parts ", "]\]"]
}

# An array of STRINGS, with no inference on the elements.
#
# The distinction is not academic: a memory entry like "one entry" is a
# two-word Tcl list whose first word is a clean identifier, so the
# inference reads it as the object {"one": "entry"}. Prose is the
# common case for these arrays, so it gets its own constructor.
proc ::vapi::json_strings {items} {
    set parts {}
    foreach i $items { lappend parts "\"[json_escape $i]\"" }
    return [json_raw "\[[join $parts ", "]\]"]
}

# A JSON object whose keys may be dotted config paths or arbitrary text.
# Values remain typed at their call site; an empty object stays `{}`.
proc ::vapi::json_object {pairs} {
    return [list ::vapi::__json_dict__ $pairs]
}

# Force a STRING, even for something that looks like a number or a list
# (a model name, a schedule pattern, a version).
proc ::vapi::json_string {v} {
    return [json_raw "\"[json_escape $v]\""]
}

proc ::vapi::_json_value {v} {
    # A known container retains its structure even when a key contains
    # dots, dashes or spaces (manifest option paths do). Explicit scalar
    # tags above this branch still win at every value position.
    if {[llength $v] == 2 && [lindex $v 0] eq "::vapi::__json_dict__"} {
        set parts {}
        foreach {k val} [lindex $v 1] {
            lappend parts "\"[json_escape $k]\": [_json_value $val]"
        }
        return "\{[join $parts ", "]\}"
    }
    # An explicitly typed value wins over every inference below.
    if {[llength $v] == 2 && [lindex $v 0] eq "::vapi::__json__"} {
        return [lindex $v 1]
    }
    if {[string is integer -strict $v] || [string is double -strict $v]} {
        return $v
    }
    set n [llength $v]
    if {$n == 0} { return "\[\]" }
    if {$n == 1} {
        # Single word → JSON string. Stops the infinite recursion.
        return "\"[json_escape $v]\""
    }
    # Dict-shaped: even-length list whose elements alternate key/value
    # and every key is a clean identifier.
    if {$n % 2 == 0} {
        set is_dict 1
        foreach {k val} $v {
            if {![regexp {^[A-Za-z_][A-Za-z0-9_]*$} $k]} { set is_dict 0; break }
        }
        if {$is_dict} {
            set parts {}
            foreach {k val} $v {
                lappend parts "\"[json_escape $k]\": [_json_value $val]"
            }
            return "\{[join $parts ", "]\}"
        }
    }
    set parts {}
    foreach item $v { lappend parts [_json_value $item] }
    return "\[[join $parts ", "]\]"
}

# POST without waiting for the reply.
#
# `::vapi::post` blocks the event loop until the gateway answers, which
# is fine for a button but not for the LSP: a `didChange` goes out on
# every keystroke, and a blocking round trip per keystroke is a visibly
# stuttering editor — worse over a tunnel, where it is a whole RTT.
#
# The reply is dropped by design: LSP notifications have no reply, and
# LSP responses come back on the event stream, not on this request. A
# TRANSPORT failure is still worth hearing about, so `on_err` gets it.
#
# `on_ok`, when given, runs once the gateway has ACCEPTED the body
# (status ok, code < 400). That is how the LSP client keeps dependent
# messages in order — didOpen acknowledged BEFORE semanticTokens/full
# goes out — without blocking the editor on either one. It runs at
# most once per request, and only while the attach epoch is fresh,
# exactly like `on_err`.
proc ::vapi::post_async {path body {on_err ""} {on_ok ""}} {
    variable timeout
    # CAPTURED AT CALL TIME (multi-instance rule, top of file): the
    # connection AND the attach epoch. `current` may be anywhere by the
    # time the callback fires, and the world may have attached
    # elsewhere entirely.
    set conn [_conn]
    set ep [_epoch_now]
    set tok [http::geturl "[dict get $conn url]$path" -method POST \
        -type application/json \
        -binary 1 -query [_utf8_out $body] -timeout $timeout \
        -command [list ::vapi::_post_async_done $on_err $on_ok [dict get $conn url] $ep] \
        {*}[_conn_headers $conn]]
    return $tok
}

proc ::vapi::_post_async_done {on_err on_ok at ep token} {
    set status "error"
    set code 0
    set data ""
    catch { set status [http::status $token] }
    catch { set code [http::ncode $token] }
    catch { set data [_utf8 [http::data $token]] }
    catch { http::cleanup $token }
    if {$status eq "ok" && $code < 400} {
        if {$on_ok ne "" && [_epoch_fresh $ep]} {
            if {[catch { uplevel #0 [list {*}$on_ok] } report_err]} {
                catch { puts stderr "veles-desktop: post_async: $report_err" }
            }
        }
        return
    }
    if {$on_err eq ""} { return }
    # The client attached elsewhere while this was out. Reporting now
    # would put gateway A's failure in front of someone looking at
    # gateway B — the exact cross-gateway leak the attach epoch exists
    # to stop. The request is over; drop its result on the floor.
    if {![_epoch_fresh $ep]} { return }
    set why [expr {$code >= 400 ? [http_reason $code $at] : "transport: $status"}]
    if {[catch { uplevel #0 [list {*}$on_err $why] } report_err]} {
        catch { puts stderr "veles-desktop: $why ($report_err)" }
    }
}

# POST a Tcl value as a JSON body, parse the JSON reply. (Previously
# chat.tcl called this but it never existed — /steer /branch /undo
# /retry were silently dead behind their `catch`.)
proc ::vapi::post_json {path body_val} {
    return [::json::json2dict [post $path [_json_value $body_val]]]
}

# POST with a parsed JSON completion, preserving the ordinary post_async
# callback contract: on_err receives text; on_ok receives the parsed
# response. post_async's no-argument acknowledgement is unchanged.
# Generation lets a caller reject a late reply for
# a destroyed widget, a different component, or a changed answer set.
proc ::vapi::post_json_async {path body on_err on_ok} {
    variable timeout
    set conn [_conn]
    set ep [_epoch_now]
    set tok [http::geturl "[dict get $conn url]$path" -method POST \
        -type application/json -binary 1 -query [_utf8_out [_json_value $body]] \
        -timeout $timeout \
        -command [list ::vapi::_post_json_async_done $on_err $on_ok [dict get $conn url] $ep] \
        {*}[_conn_headers $conn]]
    return $tok
}

proc ::vapi::_post_json_async_done {on_err on_ok at ep token} {
    if {![_epoch_fresh $ep]} { catch {http::cleanup $token}; return }
    set status "error"
    set code 0
    set data ""
    catch {set status [http::status $token]}
    catch {set code [http::ncode $token]}
    catch {set data [_utf8 [http::data $token]]}
    catch {http::cleanup $token}
    if {$status ne "ok" || $code >= 400} {
        if {$on_err ne ""} {
            set why [expr {$code >= 400 ? "HTTP $code: $data" : "transport: $status"}]
            uplevel #0 [list {*}$on_err $why]
        }
        return
    }
    if {[catch {::json::json2dict $data} value]} {
        if {$on_err ne ""} { uplevel #0 [list {*}$on_err "invalid JSON response: $value"] }
        return
    }
    if {$on_ok ne ""} { uplevel #0 [list {*}$on_ok $value] }
}

# ── SSE chat ─────────────────────────────────────────────────────────
# Callback contract (namespace ::vchat):
#   on_route provider · on_reasoning text · on_delta text ·
#   on_tool state name payload · on_done dict · on_error msg ·
#   on_finished status
# Returns the http token (for ::vapi::chat_stop).

# STREAM STATE IS PER TOKEN, not global.
#
# `_sse_event` (the last `event:` line, which the following `data:` line
# belongs to) and `_sse_tok` used to be single variables. That allowed
# exactly one turn at a time, which is why switching sessions mid-turn
# was refused outright rather than supported — a second stream would
# have interleaved its event names into the first one's data lines.
#
# Keyed by http token, each stream also remembers WHICH SESSION it
# belongs to, so ::vchat can tell whether an event is for the transcript
# currently on screen.
namespace eval ::vapi {
    variable _sse       ;# (event,$tok) (session,$tok)
    array set _sse {}
    ;# Pending async GETs: tok -> {callback epoch}. get_json_async keys
    ;# them; _get_json_async_done consumes them exactly once.
    variable _async_done
    array set _async_done {}
}

# Start a turn. FIRE-AND-FORGET: POST /api/chat returns 202 at once and
# the turn runs detached on the gateway. Its events ride the session bus
# and are watched on the persistent `session_events` stream that
# open_session already holds — the SAME stream a second window or a
# reconnect uses, so starting a turn and viewing it are decoupled.
#
# This used to be `chat_sse`, which POSTed and returned the SSE stream as
# the response body. That made the request that started a turn the only
# thing that could ever observe it (mpsc is single-consumer): no second
# window, no rejoin after a reconnect, and a detached turn ran blind. The
# body is built by hand (json_escape, not the dict heuristic) because
# `text` is free-form and a multi-word value would otherwise be mangled —
# the same reason send_current uses json_string for steer.
proc ::vapi::chat_send {session_id text {attachments {}}} {
    set join ""
    if {[llength [info commands ::vinst::remote_join]]} {
        set spec [::vinst::remote_join]
        if {[dict size $spec]} {
            foreach k {instance target_session join_id} {
                if {![dict exists $spec $k] || [dict get $spec $k] eq ""} { return }
            }
            if {[dict get $spec instance] eq [attached]
                && [dict get $spec target_session] eq $session_id} {
                set join [dict get $spec join_id]
            }
        }
    }
    set join_field [expr {$join eq "" ? "null" : "\"[json_escape $join]\""}]
    set att {}
    foreach a $attachments { lappend att "\"[json_escape $a]\"" }
    set body "{\"session_id\":\"[json_escape $session_id]\",\"text\":\"[json_escape $text]\"\
,\"attachments\":\[[join $att ,]\],\"join_id\":$join_field}"
    return [post /api/chat $body]
}

# Open the persistent event stream for a session — the ONE way this client
# watches a turn now. Same resumable-stream contract as IRC's
# `/api/irc/events`: the engine replays persisted rows after `cursor`,
# hands over the in-flight turn's snapshot, marks the live boundary, then
# streams; on a drop the caller reconnects from the last durable cursor.
#
# TWO cursors: messages AND notes. A note's anchor is where it RENDERS
# (`after_msg_id`), not when it was created — so a note anchored at or
# before the message cursor but persisted after it would never replay
# off the message cursor alone. `note_cursor` is the highest
# `engine_notes.id` this client has rendered; rows it misses get sent as
# `history` frames with `note_id` set.
#
# The handler is bound to the session id (a list), so an event is
# attributed to the right transcript even after `current` moves on — the
# multi-instance rule at the top of this file. The session is ALSO
# recorded per token so `stream_session`/`streaming_sessions`/`chat_stop`
# keep working for it exactly as they did for the old chat stream.
proc ::vapi::session_events {session_id cursor note_cursor handler} {
    variable _sse
    variable current
    set path "/api/sessions/$session_id/events"
    # Resume from the last DURABLE msg_id rendered. 0 means "replay the
    # whole transcript" — used only when there is no cursor yet; open_session
    # paints history windowed first and hands the resulting last_msg_id
    # here, so the stream carries the live tail, not a second full copy.
    set qs {}
    if {$cursor > 0} { lappend qs "since=$cursor" }
    if {$note_cursor > 0} { lappend qs "since_note=$note_cursor" }
    if {[llength $qs]} { append path "?[join $qs &]" }
    set conn [_conn]
    set tok [http::geturl "[dict get $conn url]$path" -method GET \
        -binary 1 -handler ::vapi::_sse_handler -command ::vapi::_sse_finished \
        -timeout 0 {*}[_conn_headers $conn]]
    set _sse(event,$tok) ""
    set _sse(handler,$tok) $handler
    set _sse(session,$tok) $session_id
    set _sse(instance,$tok) $current
    set _sse(url,$tok) [dict get $conn url]
    set _sse(epoch,$tok) [_epoch_now]
    return $tok
}

# The session a live stream belongs to, or "" if the token is unknown.
proc ::vapi::stream_session {tok} {
    variable _sse
    if {[info exists _sse(session,$tok)]} { return $_sse(session,$tok) }
    return ""
}

# Sessions with a stream currently open.
proc ::vapi::streaming_sessions {} {
    variable _sse
    set out {}
    foreach k [array names _sse session,*] {
        if {[lsearch -exact $out $_sse($k)] < 0} { lappend out $_sse($k) }
    }
    return $out
}

# The url an open stream belongs to (for reasons that must name the
# gateway the stream was actually talking to), or "".
proc ::vapi::stream_url {tok} {
    variable _sse
    if {[info exists _sse(url,$tok)]} { return $_sse(url,$tok) }
    return ""
}

# Stop one session's turn, or every open one when called with no
# argument. Stopping "the" turn was unambiguous only while a single
# stream could exist.
proc ::vapi::chat_stop {{session_id ""}} {
    variable _sse
    foreach k [array names _sse session,*] {
        if {$session_id ne "" && $_sse($k) ne $session_id} { continue }
        set tok [string range $k [string length session,] end]
        catch {http::reset $tok}
    }
}

proc ::vapi::_sse_handler {sock token} {
    # Force UTF-8 on the SSE socket (the comment at the top explains why).
    if {[catch {fconfigure $sock -encoding utf-8}]} {}
    if {[catch {gets $sock line} n]} { return 0 }
    if {$n >= 0} { _sse_line $token $line }
    if {$n < 0} { set n 0 }
    return $n
}

# Open a long-lived SSE stream whose events go to HANDLER rather than
# to ::vchat. `handler` is called as `$handler $event $dict`.
#
# The chat stream predates this and is still dispatched inline below,
# because its events are bound up with which transcript is on screen.
# Anything else — the IRC event stream — gets its own handler, so a
# second consumer does not mean a second copy of the SSE plumbing and
# does not risk disturbing the chat path.
#
# The connection is CAPTURED at open time (multi-instance rule, top of
# file): `current` will have moved on by the time the socket dies, but
# the reason a rejected token produces must still name the gateway the
# stream was actually talking to.
proc ::vapi::sse_open {path handler} {
    variable _sse
    variable current
    set conn [_conn]
    set tok [http::geturl "[dict get $conn url]$path" -method GET \
        -binary 1 -handler ::vapi::_sse_handler -command ::vapi::_sse_finished \
        -timeout 0 {*}[_conn_headers $conn]]
    set _sse(event,$tok) ""
    set _sse(handler,$tok) $handler
    set _sse(instance,$tok) $current
    set _sse(url,$tok) [dict get $conn url]
    set _sse(epoch,$tok) [_epoch_now]
    return $tok
}

# The gateway URL a reason should name: an explicit `at` (a stream or
# async call that captured its connection), else whatever is current.
proc ::vapi::_reason_origin {{at ""}} {
    if {$at ne ""} { return $at }
    return [dict get [_conn] url]
}

# The engine's own words out of a thrown ::vapi error.
#
# A refusal arrives as HTTP 4xx with {"ok":false,"error":"…"}, and
# `post`/`get` throw it as "HTTP 400: {json}" — so a caller that reads
# `ok`/`error` off a RETURNED dict never sees one, and a caller that
# shows the thrown text shows raw JSON. This takes the `error` field out
# when there is one, the body when there is not, and the text as it came
# otherwise (a transport failure has no JSON at all). Moved here from
# manage.tcl so the setup wizard decodes a refusal the same way.
proc ::vapi::reason {err} {
    if {[regexp {^HTTP \d+: (.*)$} $err -> body]} {
        if {![catch { ::json::json2dict $body } d] && [dict exists $d error]} {
            return [dict get $d error]
        }
        if {[string trim $body] ne ""} { return $body }
    }
    return $err
}

# One phrasing for an HTTP failure, so the composer, the IRC pane and
# the boot probe do not each invent their own words for a 401. `at` is
# the url the failing request was actually sent to — without it, a
# stream opened against instance X that dies while Y is current would
# name Y as the one refusing the token.
proc ::vapi::http_reason {code {at ""}} {
    set origin [_reason_origin $at]
    switch -- $code {
        401 - 403 {
            # "check VELES_TOKEN" was the whole message, and on
            # 2026-09-11 it pointed away from the cause for a full
            # debugging round: VELES_TOKEN was correct, and the ATTACHED
            # INSTANCE was overriding it with a token it did not have.
            # A registry row written before the gateway minted bearers
            # has no `token` key, `_conn` resolves through the row, and
            # `_auth_headers` sends nothing. Name which of the two it is.
            set who [attached]
            if {$who eq "" || ![instance_bound $who]} {
                return "$origin rejected the token (HTTP $code) — check VELES_TOKEN"
            }
            if {[instance_has_token $who]} {
                return "$origin rejected the token (HTTP $code) — instance\
                        \"$who\" sent a token and the gateway refused it; check\
                        VELES_TOKEN against DESKTOP_BEARER_TOKEN in the\
                        gateway's .env"
            }
            return "$origin rejected the token (HTTP $code) — instance \"$who\"\
                    has NO token saved, so none was sent. Registry rows written\
                    before the gateway minted bearers have no token; re-save it\
                    from the Instances pane, or set VELES_API so the launcher's\
                    VELES_TOKEN is used directly."
        }
        404 { return "$origin has no such endpoint (HTTP 404) — engine too old?" }
    }
    return "$origin returned HTTP $code"
}

# A code the gateway will keep returning however long we wait. Retrying
# an authentication failure is just a slower way of failing.
proc ::vapi::fatal_code {code} {
    return [expr {$code == 401 || $code == 403 || $code == 404}]
}

proc ::vapi::sse_close {tok} {
    catch { http::reset $tok }
}

proc ::vapi::_sse_line {tok line} {
    variable _sse
    if {[regexp {^event: (.+)$} $line -> ev]} {
        set _sse(event,$tok) [string trim $ev]
        return
    }
    if {![regexp {^data: (.+)$} $line -> payload]} return
    set ev ""
    if {[info exists _sse(event,$tok)]} { set ev $_sse(event,$tok) }
    # A stream with its own handler never touches the chat path.
    if {[info exists _sse(handler,$tok)]} {
        if {[catch {::json::json2dict $payload} d]} { set d {} }
        # The handler's errors are ITS problem to report, but they must
        # not kill the reader — a malformed line would otherwise take
        # the whole stream down and look like a dropped connection.
        if {[catch { uplevel #0 [list {*}$_sse(handler,$tok) $ev $d] } err]} {
            catch { ::vstatus::set_msg [format [::vi18n::mc {stream handler: %s}] $err] }
        }
        return
    }
    # Tell ::vchat which transcript this event belongs to BEFORE
    # dispatching. Tk's event loop is single-threaded and dispatch is
    # synchronous, so one variable is enough — and every handler can
    # decide whether it is looking at the session on screen.
    set ::vchat::stream_session [stream_session $tok]
    if {[catch {::json::json2dict $payload} d]} return
    switch -- $ev {
        route     {
            set _m ""
            catch { set _m [dict get $d model] }
            set _r ""
            catch { set _r [dict get $d reasoning_effort] }
            if {$_r eq "null"} { set _r "" }
            ::vchat::on_route [dict get $d provider] $_m $_r
        }
        reasoning { ::vchat::on_reasoning [dict get $d text] }
        delta     { ::vchat::on_delta [dict get $d text] }
        tool {
            ::vchat::on_tool [dict get $d state] [dict get $d name] $d
        }
        note      {
            # A live engine note. Its `id` is the persisted
            # engine_notes.id (stamped by the engine when it saved the
            # note): advance this session's note cursor by it so the
            # note is never replayed after a reconnect.
            set nsid [stream_session $tok]
            if {$nsid ne ""} {
                variable ::vchat::ev_note_cursor
                set nid 0
                catch { set nid [dict get $d id] }
                if {$nid > 0} {
                    if {![info exists ::vchat::ev_note_cursor($nsid)] \
                        || $nid > $::vchat::ev_note_cursor($nsid)} {
                        set ::vchat::ev_note_cursor($nsid) $nid
                    }
                }
            }
            ::vchat::on_note $d
        }
        ask       { ::vask::show $d }
        ask-resolved { ::vask::resolved $d }
        ask-relayed  { ::vask::relayed $d }
        done      { ::vchat::on_done $d }
        error     { ::vchat::on_error [dict get $d error] }
    }
}

# Answer an open question (approval / clarify). `choice` is a choice key
# ("once"/"deny"/…); `text` is free-form for clarify. The turn is parked
# on this call, so it must go out even while the SSE stream is live —
# ::vapi::post is a separate short request, which is fine.
# The one request that carries a secret VALUE (the secret dialog). The
# gateway stores it in its vault and refuses it from any machine but its
# own; the response names what was stored, never the value.
proc ::vapi::secret_set {name value {ask_id ""}} {
    set body "{\"name\":\"[json_escape $name]\",\"value\":\"[json_escape $value]\",\"ask_id\":\"[json_escape $ask_id]\"}"
    return [post /api/secrets/set $body]
}

# May this desktop send secret VALUES to its gateway? The GATEWAY decides
# (its own machine, or a network in its [vault] trusted_networks — a
# WireGuard subnet), asked before a masked field is offered so nobody
# types a key for nothing. No answer, or an older gateway, is "no".
proc ::vapi::secrets_accepted {} {
    if {[catch { set d [get_json /api/secrets 5000] } _err]} { return 0 }
    return [expr {[dict exists $d accepts_values] && [dict get $d accepts_values] eq "true"}]
}

proc ::vapi::ask_decide {id choice {text ""}} {
    set body "{\"choice\":\"[json_escape $choice]\",\"text\":\"[json_escape $text]\"}"
    return [post /api/approvals/$id/decide $body]
}

# The multi answer (2026-09-26 parity): `answers` is a Tcl dict
#   qk -> {selected {c1 c2} text "..."}
# sent as the request's `answers` object.
proc ::vapi::ask_decide_answers {id answers} {
    set parts {}
    foreach {qk a} $answers {
        set sel {}
        catch { set sel [dict get $a selected] }
        set txt ""
        catch { set txt [dict get $a text] }
        set sitems {}
        foreach k $sel {
            lappend sitems "\"[json_escape $k]\""
        }
        lappend parts "\"[json_escape $qk]\":{\"selected\":\[[join $sitems ,]\],\"text\":\"[json_escape $txt]\"}"
    }
    set body "{\"answers\":\{[join $parts ,]\}}"
    return [post /api/approvals/$id/decide $body]
}

# A single multi-select question: `sel` is the plain list of choice
# keys → {"answers":["c1","c2"]}.
proc ::vapi::ask_decide_multi {id sel} {
    set items {}
    foreach k $sel {
        lappend items "\"[json_escape $k]\""
    }
    set body "{\"answers\":\[[join $items ,]\]}"
    return [post /api/approvals/$id/decide $body]
}

proc ::vapi::asks {} {
    return [get_json /api/asks]
}

# WHY THE HTTP CODE TRAVELS WITH `closed`
#
# `http::geturl -handler` does not throw on an HTTP error: a 401 arrives
# as a perfectly successful transfer whose BODY happens to be
# {"error":"unauthorized"}. That body carries no `data:` prefix, so
# `_sse_line` ignores every byte of it, the stream ends, and the consumer
# sees an ordinary disconnect. ::virc then reconnects on a backoff that
# doubles to a minute — forever, silently. A desktop pointed at a gateway
# with the wrong VELES_TOKEN showed an empty IRC pane and said nothing.
#
# So the terminal event carries `code`, and a consumer can tell "the
# gateway went away" (retry) from "the gateway refused me" (do not).
proc ::vapi::_sse_finished {token} {
    variable _sse
    set status [http::status $token]
    set code 0
    catch { set code [http::ncode $token] }
    # The url this stream actually talked to, captured at open time.
    set at ""
    if {[info exists _sse(url,$token)]} { set at $_sse(url,$token) }
    # …and the attach epoch it was opened in. A stream belonging to a
    # gateway the client has since DETACHED FROM must not report into
    # the panes that now belong to a different one. The teardown closes
    # both streams, but closing a socket does not un-queue its own
    # -command: this callback still runs, one event loop later, in a
    # world that has moved on. Clean up, say nothing.
    set fresh 1
    if {[info exists _sse(epoch,$token)]} {
        set fresh [_epoch_fresh $_sse(epoch,$token)]
    }
    if {!$fresh} {
        _forget_stream $token
        return
    }
    # A stream with its own handler (IRC, and the session event stream)
    # is told it ended and skips the chat callbacks ENTIRELY — its handler
    # owns reconnect and reporting. This branch used to sit BELOW the
    # `status eq "error"` on_error call, so a dropped event stream painted
    # "✖ connection: …" into the transcript and cleared the running flag
    # (declaring the turn interrupted) before the handler ever saw the
    # close. That is exactly the "stream lost ⇒ turn died" lie the
    # reconnect supervisor exists to prevent: the socket dropping must
    # reach the handler (which resumes from the cursor), not on_error.
    if {[info exists _sse(handler,$token)]} {
        set h $_sse(handler,$token)
        _forget_stream $token
        # A `reset` end is one WE started: sse_close/chat_stop reset
        # tokens, and http::reset runs the -command SYNCHRONOUSLY — from
        # inside the very teardown that is closing the stream. Reporting
        # that as "closed" made every consumer treat its own cleanup as
        # a drop and arm a reconnect; the reconnect closed the LIVE
        # stream, whose reset armed the next, and the client storm-looped
        # — reconnecting once a SECOND forever, each cycle repainting the
        # in-flight snapshot and rebuilding the sidebar (USER: "the
        # session list and the output flicker while a turn runs, a
        # cursor rides top to bottom"). A stream that ends on its own
        # ends eof or error; those still report.
        if {$status eq "reset"} { return }
        catch { uplevel #0 [list {*}$h "closed" \
            [dict create status $status code $code url $at token $token]] }
        return
    }
    # No handler: the legacy chat path (POST /api/chat used to return the
    # SSE stream itself). Fire-and-forget chat removed the only stream that
    # reached here, so this is defensive — report to ::vchat and clean up.
    # The owning session survives until the callbacks have run.
    set ::vchat::stream_session [stream_session $token]
    if {$status eq "error"} {
        catch { ::vchat::on_error "connection: [http::error $token]" }
    }
    catch {http::cleanup $token}
    # The reason is computed OUTSIDE the catch, so the only thing bound
    # and dropped is the dispatch — and even that goes to stderr rather
    # than nowhere. A catch around the reporting of a failure is how a
    # failure stops being reported at all.
    if {$code >= 400} {
        set why [http_reason $code $at]
        if {[catch { ::vchat::on_error $why } dispatch_err]} {
            catch { puts stderr "veles-desktop: $why (on_error: $dispatch_err)" }
        }
    }
    _forget_stream $token
    set ::vchat::stream_session ""
}

# Drop every per-token record of a stream. _sse_finished used to unset
# exactly the keys it knew about, one `catch {array unset}` each — fine
# while the record was two keys, wrong now that streams also capture
# url/instance/epoch. One helper, called from every exit.
proc ::vapi::_forget_stream {token} {
    variable _sse
    foreach k [array names _sse *,$token] {
        array unset _sse $k
    }
    catch {http::cleanup $token}
}

# Percent-encode one query-string value.
#
# Needed the moment a value can carry a space or a `*` — a cron schedule
# ("every 1h", "cron:0 9 * * 1") is both. `http::formatQuery` would do it
# but builds a whole key=value string; this encodes the value alone so
# call sites keep composing their own paths, which is how every other
# request in this file is written.
proc ::vapi::urlencode {s} {
    set out ""
    foreach ch [split $s ""] {
        if {[string match {[A-Za-z0-9._~-]} $ch]} {
            append out $ch
        } else {
            foreach byte [split [encoding convertto utf-8 $ch] ""] {
                append out [format %%%02X [scan $byte %c]]
            }
        }
    }
    return $out
}

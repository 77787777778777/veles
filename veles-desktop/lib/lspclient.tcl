# veles-desktop/lib/lspclient.tcl — a real LSP client for the Dev pane,
# talking JSON-RPC to tcl-lsp (the user's own tree-sitter-tcl language
# server) THROUGH THE GATEWAY. lsp-mode feel in Tk:
#
#   semantic tokens → syntax highlighting (tree-sitter accurate)
#   publishDiagnostics → underlines + status count
#   hover → signature + doc tooltip (lsp-ui-doc style)
#   completion → candidate list fed to the editor popup
#
# ── WHERE THE SERVER RUNS (changed 2026-08-25) ───────────────────────
#
# It used to be a subprocess of `wish`: `open "|tcl-lsp" r+`. That was
# fine while the desktop and the files were on one machine. They are not
# any more — the Dev pane reads its buffer over /api/fs/read, from the
# GATEWAY's filesystem — so the server now runs there, next to the files,
# and this file talks to it over /api/lsp/ start, send, events, stop.
#
# It half-worked before, which is why it was easy to misread. Sync is by
# full document TEXT, not by path, so semantic tokens, diagnostics, hover
# and completion were already correct against a remote buffer; only the
# `file://` URI named a path on the wrong machine. What genuinely did not
# work was anything needing the PROJECT — and that did not work locally
# either, because `rootUri` was `null` and the server had nothing to
# index. It is now the gateway's derived project root, which is a new
# capability on a local connection as much as a remote one.
#
# USER: "it says lsp can only work locally which seems stupid? Just run
# the lsp on the remote".
#
# NOTHING ABOVE THE TRANSPORT CHANGED. `_dispatch`, the pending map, the
# capability probe and every result normaliser (60 assertions in
# lsptest.tcl) are untouched: the gateway is a framed pipe that parses
# no JSON-RPC field of its own.
#
# One server process per dev-pane session, full-document sync (the
# buffer is small; a contentChange with no range is a full replace and
# is valid under the server's INCREMENTAL sync mode too).
#
# Protocol notes (probed live 2026-08-22):
#   - server honors capabilities.general.positionEncodings ["utf-8"]
#     (Tk text indices are codepoint-based — matches)
#   - semantic legend: 0 comment · 1 function · 2 namespace · 3 class
#     4 method · 5 variable · 6 parameter · 7 keyword; mod bit0 = decl
#   - hover returns markdown (```tcl fence + docstring)
#
# ── EVERY EXTENSION LANGUAGE (0.2.0) ────────────────────────────────
#
# The gateway picks the server by file type (lsp_servers.rs): tcl-lsp,
# typescript-language-server, rust-analyzer, clangd, racket-langserver,
# a Common Lisp server. Each language family is its own gateway SESSION
# (`?session=` on send, events and stop), so a tab switch between a Tcl
# plugin and a Rust plugin crate reuses both servers instead of killing
# one. The start reply carries the document URI and the root URI, built
# for the GATEWAY's platform, and the client keeps them verbatim.
#
# Positions are where languages differ. Tk counts the characters of a
# Tcl string (UTF-16 units under Tcl 8.6), the servers count what they
# negotiated: tcl-lsp says utf-8 and means BYTES (`proc` after "éé€" is
# at 17, not 13 — the Tcl buffers were mis-highlighted on every
# non-ASCII line), clangd and rust-analyzer take the utf-32 offered,
# tsserver negotiates nothing and counts UTF-16. `_to_server` and
# `_from_server` convert one line at a time against the server's view of
# the document; an ASCII line is the identity.
#
# A server may ASK the client things (workspace/configuration,
# client/registerCapability, window/workDoneProgress/create). Those were
# dropped as notifications, which leaves a server waiting on an answer
# that never comes; they are answered now.
#
# SOURCE DISCIPLINE: Tcl counts literal curly braces even inside
# comments and double-quoted strings when they sit in a braced proc
# body. This file therefore writes EVERY JSON brace as \173 / \175
# (octal; hex \x7b would swallow the following hex-looking quote as
# part of the escape) and keeps comments brace-free. Do not
# reintroduce raw braces in strings or comments here.

namespace eval ::vlsp {
    variable ch ""            ;# SSE stream token ("" = not attached)
    variable root ""          ;# project root, as the GATEWAY resolved it
    variable buf ""           ;# unused since the gateway does the framing
    variable next_id 2        ;# request ids; 1 is reserved for `initialize`
    variable session_ready 0  ;# this client owns a fresh initialize handshake
    variable pending {}       ;# id -> callback command
    variable version 1
    variable uri ""
    variable doc_path ""      ;# the path `start` was called with (client-side spelling)
    variable ready 0
    variable on_diag ""       ;# callback: rows list
    variable apply_w ""       ;# the widget semantic tokens paint into
    variable legend_types {}  ;# from initialize result (in order)
    variable _pending_text ""
    # ── 0.2.0: every extension language (owner ask 287976.2) ──
    variable session ""       ;# the gateway session key (tcl, typescript, rust, c, racket, lisp)
    variable language tcl     ;# the LSP languageId of the open document
    variable server ""        ;# the server command, for status lines
    variable root_uri ""      ;# the root as a URI, built by the gateway
    variable gateway_os ""    ;# the gateway's platform (paths are ITS paths)
    variable encoding utf-16  ;# the negotiated position encoding
    variable doc_lines {}     ;# the server's view of the document, by line
    variable last_error ""    ;# why the last start did not give a server
    variable note ""          ;# a known limit the gateway reported
}

# ── framing ──────────────────────────────────────────────────────────

# What the server advertised at initialize (see ::vlsp::supports).
namespace eval ::vlsp { variable server_caps {} }

# Send one JSON-RPC message. The Content-Length framing moved to the
# gateway, which is also where the byte-vs-character trap moved to (it is
# pinned there by lsp_proxy::tests::content_length_counts_bytes_not_characters).
#
# ASYNC, deliberately: this runs on every keystroke via didChange, and a
# blocking round trip per keystroke is a stuttering editor — over a
# tunnel, a whole RTT of it.
#
# `on_ok` is the ordering seam: each message is its own HTTP POST, so
# two back-to-back sends can arrive at the server's stdin in EITHER
# order. A message that only makes sense after a previous one (a
# semanticTokens/full that needs the didOpen to have landed) is queued
# from the earlier POST's on_ok instead of being fired alongside it.
proc ::vlsp::_send {obj {on_ok ""}} {
    variable ch
    if {$ch eq ""} { return }
    ::vapi::post_async [_session_path /api/lsp/send] $obj ::vlsp::_send_failed $on_ok
}

# An /api/lsp path for this client's gateway session. Without a key the
# gateway means its most recent session, which is what a 0.1.x gateway
# (one session, no keys) always answers.
proc ::vlsp::_session_path {path} {
    variable session
    if {![regexp {^[A-Za-z0-9_-]+$} $session]} { return $path }
    return "$path?session=$session"
}

# The server's name for status lines: what the gateway ran, as its
# program name. The gateway reports the file it found, and it may be a
# Windows machine while this desktop is not: `file tail` does not split
# on a backslash here, so the path is cut on both separators, and the
# suffix Windows adds (.exe, .cmd, .bat) is not part of the name.
proc ::vlsp::server_label {} {
    variable server
    variable language
    if {$server ne ""} {
        set name [lindex [split $server "/\\"] end]
        regsub -nocase {\.(exe|cmd|bat)$} $name {} name
        if {$name ne ""} { return $name }
        return $server
    }
    if {$language eq "tcl"} { return tcl-lsp }
    return [::vi18n::mc {language server}]
}

# A status line about the server: its name, then `text` (already
# translated by the caller, or the server's own words).
proc ::vlsp::_say {text} {
    catch { ::vstatus::set_msg [format [::vi18n::mc {%1$s: %2$s}] [server_label] $text] }
}

proc ::vlsp::_send_failed {why} {
    # One report, then stop: a dead gateway would otherwise produce one
    # of these per keystroke.
    variable ch
    _stop "send failed — $why"
}

proc ::vlsp::_jstr {s} {
    return "\"[string map {"\\" "\\\\" "\"" "\\\"" "\n" "\\n" "\r" "\\r" "\t" "\\t"} $s]\""
}

# The gateway's event stream. `lsp` carries one complete frame, already
# de-framed there; the body rides inside a one-key wrapper because a raw
# frame containing a newline would become two `data:` lines and only the
# first would be read.
proc ::vlsp::_on_stream {ev d} {
    variable ch
    variable session_ready
    switch -- $ev {
        lsp {
            if {![dict exists $d frame]} { return }
            _dispatch [dict get $d frame]
        }
        lsp-live {
            # A reused, already initialized server must not receive a
            # second initialize. Its request loop answers null for that
            # method, which used to wipe this client's capability map.
            if {!$session_ready} { _handshake }
        }
        lsp-initialized {
            # The gateway replays only the bounded initialize result for a
            # reused process. A fresh client can therefore learn its caps
            # without asking the running server to initialize a second time.
            if {[dict exists $d frame]} {
                _dispatch [dict get $d frame]
            }
        }
        lsp-lagged {
            # A dropped response leaves a pending id waiting forever, so
            # say it rather than hanging quietly.
            _stop "event stream fell behind — reopen the buffer"
        }
        lsp-exit {
            # The server died on its own; the gateway passes on its last
            # words. Before, the stream simply went quiet.
            set why ""
            catch { set why [dict get $d reason] }
            variable last_error $why
            _stop [format [::vi18n::mc {the server stopped: %s}] $why]
        }
        closed {
            set code 0
            catch { set code [dict get $d code] }
            set ch ""
            if {[::vapi::fatal_code $code]} {
                _stop [::vapi::http_reason $code]
            } else {
                _stop "event stream closed"
            }
        }
    }
}

proc ::vlsp::_dispatch {body} {
    variable pending
    variable on_diag
    if {[catch {::json::json2dict $body} msg]} {
        # The server's JSON is always valid; a parse failure means the
        # json pkg choked on escapes — fall back to a crude method scan.
        if {[regexp {"method"\s*:\s*"([^"]+)"} $body _ method]} {
            if {$method eq "textDocument/publishDiagnostics" && $on_diag ne ""} {
                if {[catch { uplevel #0 [list $on_diag {}] } diag_err]} {
                    puts stderr "veles-desktop: lsp diag callback failed: $diag_err"
                }
            }
        }
        return
    }
    # A request FROM the server (method and id): answer it, or the server
    # waits on this client for good.
    if {[dict exists $msg method] && [dict exists $msg id]} {
        _answer_server [dict get $msg id] [dict get $msg method] \
            [expr {[dict exists $msg params] ? [dict get $msg params] : {}}]
        return
    }
    # Notification?
    if {[dict exists $msg method]} {
        set method [dict get $msg method]
        if {$method eq "textDocument/publishDiagnostics" && $on_diag ne ""} {
            # Only the open document's. rust-analyzer and tsserver publish
            # for every file in the project; those rows used to be painted
            # onto whatever buffer was open.
            set for_uri ""
            catch { set for_uri [dict get $msg params uri] }
            if {$for_uri ne "" && ![same_document $for_uri]} { return }
            set rows {}
            catch {
                foreach d [dict get $msg params diagnostics] {
                    set line [dict get $d range start line]
                    set col [_from_server $line [dict get $d range start character]]
                    set sev "ERROR"
                    catch {
                        set s [dict get $d severity]
                        set sev [lindex {"" ERROR WARNING INFO HINT} $s]
                    }
                    set m ""
                    catch { set m [dict get $d message] }
                    lappend rows [list [expr {$line + 1}] $col $sev $m]
                }
            }
            uplevel #0 [list $on_diag $rows]
        } elseif {$method eq "\$/progress"} {
            _progress [expr {[dict exists $msg params] ? [dict get $msg params] : {}}]
        } elseif {$method eq "window/showMessage"} {
            set type 3
            set text ""
            catch { set type [dict get $msg params type] }
            catch { set text [dict get $msg params message] }
            # Errors and warnings only; info chatter is the log's.
            if {$type <= 2 && $text ne ""} { _say $text }
        }
        return
    }
    # Response.
    if {![dict exists $msg id]} { return }
    set id [dict get $msg id]
    if {![dict exists $pending $id]} { return }
    set cb [dict get $pending $id]
    dict unset pending $id
    set result ""
    catch { set result [dict get $msg result] }
    set err ""
    catch { set err [dict get $msg error] }
    variable asked
    if {$err ne "" && [_retryable $id $err]} {
        lassign [dict get $asked $id] method params attempt
        dict unset asked $id
        after 250 [list ::vlsp::_ask_again $method $params $cb [expr {$attempt + 1}]]
        return
    }
    catch { dict unset asked $id }
    uplevel #0 [list {*}$cb $result $err]
}

# The retry, unless the client stopped or moved on meanwhile (a request
# for a document that is no longer open is noise).
proc ::vlsp::_ask_again {method params cb attempt} {
    variable ch
    variable ready
    if {$ch eq "" || !$ready} { return }
    _request $method $params $cb $attempt
}

# A JSON-RPC id as it goes back on the wire: a number bare, else a string.
proc ::vlsp::_json_id {id} {
    if {[string is integer -strict $id]} { return $id }
    return [_jstr $id]
}

# Answer a server-to-client request. The answers are the ones a client
# with no workspace settings, no file watching and its own edit engine
# gives: defaults for every configuration item, acknowledgement for a
# registration or a progress token, a refusal for anything else.
proc ::vlsp::_answer_server {id method params} {
    set idj [_json_id $id]
    switch -- $method {
        workspace/configuration {
            set n 0
            catch { set n [llength [dict get $params items]] }
            set result "\[[join [lrepeat $n null] ,]\]"
        }
        client/registerCapability - client/unregisterCapability -
        window/workDoneProgress/create - window/showMessageRequest -
        workspace/codeLens/refresh - workspace/inlayHint/refresh -
        workspace/diagnostic/refresh {
            set result null
        }
        workspace/semanticTokens/refresh {
            set result null
            after idle ::vlsp::_request_semantic
        }
        workspace/applyEdit {
            set result "\173\"applied\":false,\"failureReason\":[_jstr {the Dev pane applies edits through its own rename and code actions}]\175"
        }
        default {
            _send "\173\"jsonrpc\":\"2.0\",\"id\":$idj,\"error\":\173\"code\":-32601,\"message\":[_jstr "not supported by the Dev pane: $method"]\175\175"
            return
        }
    }
    _send "\173\"jsonrpc\":\"2.0\",\"id\":$idj,\"result\":$result\175"
}

namespace eval ::vlsp { variable progress_titles {} }

# Work-done progress (rust-analyzer's indexing, clangd's background
# index) on the status line, so a hover that answers nothing yet reads
# as "still loading" rather than "broken".
proc ::vlsp::_progress {params} {
    variable progress_titles
    set token ""
    set v {}
    catch { set token [dict get $params token] }
    catch { set v [dict get $params value] }
    set kind ""
    set title ""
    set message ""
    set pct ""
    catch { set kind [dict get $v kind] }
    catch { set title [dict get $v title] }
    catch { set message [dict get $v message] }
    catch { set pct [dict get $v percentage] }
    if {$kind eq "begin"} { dict set progress_titles $token $title }
    if {$title eq "" && [dict exists $progress_titles $token]} {
        set title [dict get $progress_titles $token]
    }
    if {$kind eq "end"} {
        catch { dict unset progress_titles $token }
        if {$title ne ""} { _say [format [::vi18n::mc {%s done}] $title] }
        return
    }
    set line $title
    if {$message ne ""} { append line " " $message }
    if {$pct ne ""} { append line " $pct%" }
    if {[string trim $line] ne ""} { _say [string trim $line] }
}

# ── paths and URIs (they are the GATEWAY's) ──────────────────────────

# The gateway's platform, from the start reply; this machine's when an
# older gateway did not say (it was then the same machine in practice).
proc ::vlsp::_gateway_os {} {
    variable gateway_os
    if {$gateway_os ne ""} { return $gateway_os }
    if {$::tcl_platform(platform) eq "windows"} { return windows }
    return unix
}

proc ::vlsp::_uri_encode {s} {
    set out ""
    foreach b [split [encoding convertto utf-8 $s] ""] {
        if {[regexp {^[A-Za-z0-9/._~:@!$&'()*+,;=-]$} $b]} {
            append out $b
        } else {
            scan $b %c c
            append out [format %%%02X $c]
        }
    }
    return $out
}

proc ::vlsp::_uri_decode {s} {
    set out ""
    set n [string length $s]
    for {set i 0} {$i < $n} {incr i} {
        set c [string index $s $i]
        set h [string range $s [expr {$i + 1}] [expr {$i + 2}]]
        if {$c eq "%" && [string length $h] == 2 && [string is xdigit -strict $h]} {
            append out [binary format H2 $h]
            incr i 2
        } else {
            append out [encoding convertto utf-8 $c]
        }
    }
    return [encoding convertfrom utf-8 $out]
}

# A gateway path as a file URI, for a gateway on `os` (unix or windows).
# The gateway sends the URI itself now; this is the fallback for one that
# does not, and it must not be "file://" glued to a Windows path
# (dev-ext-13).
proc ::vlsp::path_to_uri {path {os ""}} {
    if {$os eq ""} { set os [_gateway_os] }
    if {$os eq "windows"} {
        set p [string map [list \\ /] $path]
        if {[string match {//\?/UNC/*} $p]} { set p "//[string range $p 8 end]" }
        if {[regexp {^//\?/([A-Za-z]:.*)$} $p -> drive]} { set p $drive }
        if {[string match //* $p]} {
            set rest [string range $p 2 end]
            set i [string first / $rest]
            if {$i < 0} { return "file://$rest/" }
            return "file://[string range $rest 0 [expr {$i - 1}]][_uri_encode [string range $rest $i end]]"
        }
        return "file:///[_uri_encode $p]"
    }
    return "file://[_uri_encode $path]"
}

# The gateway path a file URI names; "" for any other scheme. Lenient
# about what servers send back (an encoded or lower-case drive letter, a
# localhost authority).
proc ::vlsp::uri_to_path {uri {os ""}} {
    if {$os eq ""} { set os [_gateway_os] }
    if {![string match file://* $uri]} { return "" }
    set rest [string range $uri 7 end]
    set host ""
    if {[string index $rest 0] ne "/"} {
        set i [string first / $rest]
        if {$i < 0} {
            set host $rest
            set rest /
        } else {
            set host [string range $rest 0 [expr {$i - 1}]]
            set rest [string range $rest $i end]
        }
    }
    if {[string equal -nocase $host localhost]} { set host "" }
    set p [_uri_decode $rest]
    if {$os eq "windows"} {
        if {$host eq "" && [regexp {^/([A-Za-z]):(.*)$} $p -> drive tail]} {
            return "[string toupper $drive]:[string map [list / \\] $tail]"
        }
        if {$host ne ""} { return "\\\\$host[string map [list / \\] $p]" }
        return [string map [list / \\] $p]
    }
    if {$host ne ""} { return "//$host$p" }
    return $p
}

# Two gateway paths name the same file: case and separators do not
# matter on a Windows gateway; on any other the desktop's normalisation
# is the one it always used.
proc ::vlsp::same_path {a b {os ""}} {
    if {$os eq ""} { set os [_gateway_os] }
    if {$os eq "windows"} {
        return [string equal -nocase [string map [list \\ /] $a] [string map [list \\ /] $b]]
    }
    return [expr {[file normalize $a] eq [file normalize $b]}]
}

# Whether a URI a server sent is the document this client has open.
proc ::vlsp::same_document {other} {
    variable uri
    if {$other eq $uri} { return 1 }
    set a [uri_to_path $other]
    set b [uri_to_path $uri]
    if {$a eq "" || $b eq ""} { return 0 }
    return [same_path $a $b]
}

# ── positions: Tk characters <-> the server's units ──────────────────

# How many of the server's units one character takes.
proc ::vlsp::_char_units {ch enc} {
    scan $ch %c cp
    switch -- $enc {
        utf-8 {
            if {$cp < 0x80} { return 1 }
            if {$cp < 0x800} { return 2 }
            if {$cp < 0x10000} { return 3 }
            return 4
        }
        utf-32 { return 1 }
        default { return [expr {$cp > 0xFFFF ? 2 : 1}] }
    }
}

# A Tk column on `line` (0-based) as the server's column. `split` hands
# an astral character over whole while Tk (Tcl 8.6) counts it as two, so
# the Tk side advances by the element's own length.
proc ::vlsp::_to_server {line char {enc ""}} {
    variable doc_lines
    variable encoding
    if {$enc eq ""} { set enc $encoding }
    set text [lindex $doc_lines $line]
    if {[string is ascii $text]} { return $char }
    set tk 0
    set units 0
    foreach ch [split $text ""] {
        if {$tk >= $char} { break }
        incr tk [string length $ch]
        incr units [_char_units $ch $enc]
    }
    if {$tk < $char} { incr units [expr {$char - $tk}] }
    return $units
}

# The server's column on `line` (0-based) as a Tk column.
proc ::vlsp::_from_server {line units {enc ""}} {
    variable doc_lines
    variable encoding
    if {$enc eq ""} { set enc $encoding }
    set text [lindex $doc_lines $line]
    if {[string is ascii $text]} { return $units }
    set tk 0
    set u 0
    foreach ch [split $text ""] {
        set w [_char_units $ch $enc]
        if {$u + $w > $units} { return $tk }
        incr u $w
        incr tk [string length $ch]
    }
    return [expr {$tk + $units - $u}]
}

# The document text the server holds, by line, for the conversions.
proc ::vlsp::_remember_text {text} {
    variable doc_lines
    set doc_lines [split $text \n]
}

# ── lifecycle ────────────────────────────────────────────────────────

# Does the server advertise this capability? `name` is the LSP
# capability key, e.g. referencesProvider / signatureHelpProvider.
#
# A provider may be `true`, or an options OBJECT (which is also "yes"),
# or `false`/absent. Anything present and not false counts.
proc ::vlsp::supports {name} {
    variable server_caps
    if {![info exists server_caps]} { return 0 }
    if {![dict exists $server_caps $name]} { return 0 }
    set v [dict get $server_caps $name]
    if {$v eq "false" || $v eq "0" || $v eq ""} { return 0 }
    return 1
}

proc ::vlsp::running {} {
    variable ch
    return [expr {$ch ne ""}]
}

# The path of the synced document as `start` was given it (the
# client-side spelling, not the gateway-canonical URI). The Dev pane
# compares it against its own buffer path before syncing, so a buffer
# that is NOT the open document never didChanges over someone else's.
proc ::vlsp::doc_path {} {
    variable doc_path
    return $doc_path
}

# $tell_gateway 0 skips the POST /api/lsp/stop. The re-root path passes
# it: its probe POST /api/lsp/start has ALREADY replaced the gateway's
# server at the new root, and the fresh start that follows is idempotent
# — an async stop fired here can otherwise land AFTER that fresh start
# and kill the server it just spawned, leaving the handshake talking to
# nothing. Every other caller (stream closed, init failed, lagged) is a
# genuine "server is gone" and still tells the gateway.
proc ::vlsp::_stop {why {tell_gateway 1}} {
    variable ch
    variable ready
    variable session_ready
    variable _stopping
    # Re-entrancy guard. `sse_close` below is `http::reset`, whose
    # -command callback runs SYNCHRONOUSLY and re-enters here through
    # _on_stream's `closed` branch — which would otherwise run with the
    # DEFAULT tell_gateway=1 and POST /api/lsp/stop even when the outer
    # call deliberately passed 0 (the re-root path). One _stop wins; the
    # nested one is the same teardown, already in progress.
    if {[info exists _stopping] && $_stopping} { return }
    set _stopping 1
    set keep_session_ready [expr {!$tell_gateway && $session_ready}]
    try {
        variable buf ""
        variable doc_path ""
        variable pending {}
        variable asked {}
        set ready 0
        set session_ready 0
        # Clear `ch` BEFORE closing, not after — the re-entrant call must
        # not see a live token and try to close it again.
        if {$ch ne ""} {
            set tok $ch
            set ch ""
            catch { ::vapi::sse_close $tok } _already_closed
        }
        # Let the gateway reap the server. Best-effort: if the gateway is
        # the thing that just went away, there is nobody to tell, and its
        # own kill_on_drop handles the process either way.
        if {$tell_gateway} {
            catch { ::vapi::post_async [_session_path /api/lsp/stop] {{}} } _gateway_gone
        }
        if {$why ne ""} { _say $why }
    } finally {
        set session_ready $keep_session_ready
        set _stopping 0
    }
}

# Start (or reuse) the server and open $path as the synced document.
# `text` is the buffer content. `diag_cb` receives the diagnostics rows.
#
# $path is a path on the GATEWAY (it is what /api/fs/file was given), so
# it is resolved there, not here — `file normalize` would resolve a
# relative one against the wrong machine's cwd, and the project root walk
# needs a filesystem this process cannot see.
#
# The gateway also picks the SERVER, by the file's type, and says which
# session it runs in. A switch to a file of another language family is a
# switch of session, handled like a switch of root: step this client off
# the old one (the gateway keeps it for the next tab of that language)
# and attach to the new.
proc ::vlsp::start {path text diag_cb} {
    variable ch
    variable ready
    variable uri
    variable doc_path
    variable root
    variable root_uri
    variable on_diag
    variable apply_w
    variable session_ready
    variable pending
    variable next_id
    variable _pending_text
    variable session
    variable language
    variable server
    variable gateway_os
    variable last_error
    variable note
    set on_diag $diag_cb
    set _pending_text $text
    set apply_w ""
    set last_error ""
    # A new document belongs to whichever editor buffer opened it; the
    # previous widget target is meaningless for it. Cleared, not set —
    # apply_tokens resolves through ::vdev::dev_editor (the current
    # buffer) and paint-target pinning is addable later via
    # set_apply_widget when a caller knows better.

    # Set when the re-root probe below spawned the server for the new root:
    # the fresh-start POST then finds that live process and answers
    # reused=true, but nobody has initialized it yet (dev-ext-6).
    set probe_spawned 0
    if {$ch ne ""} {
        # Already up: the reuse path. But reuse is only correct when the
        # NEW file belongs to the project the server was rooted at. The
        # gateway derives a root per path (dev.tcl → the repo, an edited
        # plugin → plugins/ under the config dir), and a session kept across
        # roots
        # answers from the OLD index — where a stale shadow copy of the
        # file can shadow the live one. So ask the gateway what it would
        # derive; a different root means stop + restart with the right
        # project instead of a silent wrong-project open. The JSON body is
        # built by hand (this file's brace discipline), not via [list] —
        # a Tcl list stringifies with braces the gateway's JSON parser
        # would choke on.
        set probe "\173\"path\": [_jstr $path]\175"
        set newroot_resp ""
        if {[catch {
            set newroot_resp [::json::json2dict [::vapi::post /api/lsp/start $probe]]
        } probe_err]} {
            # The probe failed — we cannot know whether the running
            # server is rooted right. Reuse is unsafe (a stale index
            # answers from the wrong project); stop and fall through
            # to the fresh-start path.
            puts stderr "veles-desktop: lsp re-root probe failed: $probe_err — restarting"
            _stop "" 0
        } else {
            set newroot ""
            set newsession ""
            set probe_ok 1
            catch { set newroot [dict get $newroot_resp root] }
            catch { set newsession [dict get $newroot_resp session] }
            catch { set probe_ok [expr {[dict get $newroot_resp ok] ? 1 : 0}] }
            set moved [expr {$newroot ne "" && $root ne "" && $newroot ne $root}]
            # Another language family: another server, even at one root.
            if {$newsession ne "" && $session ne "" && $newsession ne $session} {
                set moved 1
            }
            if {!$probe_ok} { set moved 1 }
            if {$moved} {
                set probe_reused 1
                catch { set probe_reused [expr {[dict get $newroot_resp reused] ? 1 : 0}] }
                if {!$probe_reused} { set probe_spawned 1 }
                # Client-side teardown only: the probe above already
                # re-rooted the gateway, so a POST /api/lsp/stop here
                # would race the fresh start below and could kill it.
                _stop "" 0
                # Fall through to the fresh-start path below with the
                # gateway already re-rooted by the POST we just made.
            }
        }
        if {$ch ne ""} {
            # Same project: didClose the previous document — the server
            # keeps every document it was ever told about until closed,
            # and opening file after file used to accumulate stale ones
            # (and two URIs for the same file when the first open
            # resolved a canonical path this one does not).
            set newuri [path_to_uri $path]
            catch { set newuri [dict get $newroot_resp uri] }
            if {$newuri eq "" || $newuri eq "null"} { set newuri [path_to_uri $path] }
            if {$uri ne "" && $uri ne $newuri} {
                _send "\173\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didClose\",\"params\":\173\"textDocument\":\173\"uri\":[_jstr $uri]\175\175\175"
            }
            set uri $newuri
            set doc_path $path
            catch {
                set l [dict get $newroot_resp language]
                if {$l ne "" && $l ne "null"} { set language $l }
            }
            # The semantic request rides didOpen's ACK, not a parallel
            # POST: when they raced, the small request could reach the
            # server before the large didOpen and be answered null for a
            # document it did not know yet — silently dropped, and the
            # buffer stayed unhighlighted until a later reopen happened
            # to win the race. That was "highlighting needs 2-3 reopens".
            _open_doc $text ::vlsp::_request_semantic
            return
        }
    }

    set body "\173\"path\": [_jstr $path]\175"
    if {[catch { ::json::json2dict [::vapi::post /api/lsp/start $body] } resp]} {
        set last_error $resp
        ::vstatus::set_msg [format [::vi18n::mc {language server: the gateway would not start one: %s}] $resp]
        return
    }
    set started 0
    catch { set started [expr {[dict get $resp ok] ? 1 : 0}] }
    if {!$started} {
        set why "unknown error"
        catch { set why [dict get $resp error] }
        set last_error $why
        # The server lives on the GATEWAY's PATH now, so say which
        # machine has to have it installed (the gateway's message names
        # the program and how to install it).
        ::vstatus::set_msg [format [::vi18n::mc {language server: not started on the gateway — %s}] $why]
        return
    }
    set root ""
    catch { set root [dict get $resp root] }
    set gateway_os ""
    catch { set gateway_os [dict get $resp platform] }
    set session ""
    catch { set session [dict get $resp session] }
    set server ""
    catch { set server [dict get $resp cmd] }
    set note ""
    catch {
        set note [dict get $resp note]
        if {$note eq "null"} { set note "" }
    }
    set language [language_for $path]
    catch {
        set l [dict get $resp language]
        if {$l ne "" && $l ne "null"} { set language $l }
    }
    set root_uri ""
    catch { set root_uri [dict get $resp root_uri] }
    if {$root_uri eq "" && $root ne ""} { set root_uri [path_to_uri $root] }
    set reused 0
    catch { set reused [dict get $resp reused] }
    if {$probe_spawned} { set reused 0 }
    # A fresh process needs this client's initialize. A reused process has
    # already had one; preserve an existing handshake/capability snapshot.
    if {!$reused} {
        set session_ready 0
        set pending {}
        set ::vlsp::asked {}
        set next_id 2
        set ready 0
    } else {
        # This client has not initialized the reused process itself. Reserve
        # the replayed response before subscribing to the event stream.
        set session_ready 1
        set ready 0
        set pending [dict create 1 ::vlsp::_on_init]
        set ::vlsp::asked {}
        set next_id 2
    }
    set resolved $path
    catch { set resolved [dict get $resp path] }
    set uri ""
    catch { set uri [dict get $resp uri] }
    if {$uri eq "" || $uri eq "null"} { set uri [path_to_uri $resolved] }
    # The client-side spelling, kept separately: the URI is the
    # GATEWAY's canonical path, but the Dev pane compares its own path
    # against the document to decide what a keystroke belongs to.
    set doc_path $path

    # Attach BEFORE the handshake: initialize's reply comes back on this
    # stream, and a reply that arrives before anyone is listening is a
    # pending id that never resolves. _on_stream fires _handshake on the
    # `lsp-live` marker.
    if {[catch { ::vapi::sse_open [_session_path /api/lsp/events] ::vlsp::_on_stream } tok]} {
        set last_error $tok
        ::vstatus::set_msg [format [::vi18n::mc {language server: event stream unavailable: %s}] $tok]
        return
    }
    set ch $tok
}

# The LSP languageId of a file, by extension — the same table as the
# gateway's (lsp_servers.rs Language::from_path), for an older gateway
# that does not say.
proc ::vlsp::language_for {path} {
    switch -- [string tolower [file extension $path]] {
        .tcl - .tm - .test - .tk { return tcl }
        .ts - .mts - .cts - .tsx { return typescript }
        .js - .mjs - .cjs - .jsx { return javascript }
        .rs { return rust }
        .c - .h { return c }
        .cc - .cpp - .cxx - .hh - .hpp - .hxx { return cpp }
        .rkt { return racket }
        .lisp - .lsp - .cl - .asd { return commonlisp }
    }
    return ""
}

# The client capabilities for this document's language. Tcl keeps the
# exact handshake tcl-lsp was probed with (utf-8, nothing else); the
# other servers are offered utf-32 (Tk-friendly, one unit per character)
# and utf-16 (the protocol default), hover in either format, and
# work-done progress so indexing shows on the status line.
proc ::vlsp::client_capabilities {lang} {
    if {$lang eq "tcl" || $lang eq ""} {
        return "\173\"general\":\173\"positionEncodings\":\[[_jstr utf-8]\]\175\175"
    }
    # publishDiagnostics is not optional politeness: typescript-language-
    # server publishes nothing to a client that leaves it out.
    return "\173\"general\":\173\"positionEncodings\":\[[_jstr utf-32],[_jstr utf-16]\]\175,\"textDocument\":\173\"hover\":\173\"contentFormat\":\[[_jstr markdown],[_jstr plaintext]\]\175,\"synchronization\":\173\"didSave\":false\175,\"publishDiagnostics\":\173\"relatedInformation\":false\175\175,\"window\":\173\"workDoneProgress\":true\175\175"
}

# initialize — the language's position encodings, and the project root
# the gateway derived. `rootUri` was `null` here for the whole life of
# this file until 2026-08-25, which is why nothing project-wide worked.
proc ::vlsp::_handshake {} {
    variable root
    variable root_uri
    variable session_ready
    variable language
    if {$session_ready} { return }
    set session_ready 1
    set ru "null"
    if {$root_uri ne ""} {
        set ru [_jstr $root_uri]
    } elseif {$root ne ""} {
        set ru [_jstr [path_to_uri $root]]
    }
    # Workspace folders for the servers that read them instead of
    # rootUri; tcl-lsp's handshake stays the one it was probed with.
    set folders ""
    if {$ru ne "null" && $language ne "tcl" && $language ne ""} {
        set folders ",\"workspaceFolders\":\[\173\"uri\":$ru,\"name\":[_jstr [file tail $root]]\175\]"
    }
    set init "\173\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":\173\"processId\":null,\"rootUri\":$ru$folders,\"capabilities\":[client_capabilities $language]\175\175"
    _send $init
    dict set ::vlsp::pending 1 ::vlsp::_on_init
}

proc ::vlsp::_on_init {result err} {
    variable ready
    variable legend_types
    variable _pending_text
    variable encoding
    if {$err ne ""} {
        if {[llength $result] && ![catch {dict exists $result capabilities} has_caps] && $has_caps} {
            variable server_caps
            set server_caps [dict get $result capabilities]
        }
        _stop "initialize failed"
        return
    }
    # Record the semantic legend order (probe says 0..7 fixed, but read
    # what the server tells us anyway).
    catch {
        set legend_types [dict get $result capabilities semanticTokensProvider legend tokenTypes]
    }
    # What the positions on the wire count. Absent means utf-16 (the
    # protocol's default; tsserver negotiates nothing); clangd also says
    # it as the older `offsetEncoding`.
    set encoding utf-16
    catch { set encoding [dict get $result capabilities positionEncoding] }
    catch { set encoding [dict get $result offsetEncoding] }
    # Remember what the server says it can do, so a feature it does NOT
    # support reports that instead of appearing to do nothing. A request
    # for an unadvertised method usually returns null, which is
    # indistinguishable from "no results" at the UI.
    variable server_caps
    catch { set server_caps [dict get $result capabilities] }
    _send "\173\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":\173\175\175" \
        [list ::vlsp::_open_doc $_pending_text ::vlsp::_request_semantic]
    set ready 1
    # didOpen and the semantic request are CHAINED through the gateway's
    # ACKs (initialized -> didOpen -> semanticTokens/full), not fired as
    # parallel POSTs: separate connections could land them on the
    # server's stdin in any order, and a semantic request that arrived
    # before its didOpen was answered null and dropped. This was the
    # first-open half of "highlighting needs 2-3 reopens".
}

proc ::vlsp::_open_doc {text {on_ok ""}} {
    variable uri
    variable version
    variable language
    _remember_text $text
    set lang [expr {$language eq "" ? "tcl" : $language}]
    set didopen "\173\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didOpen\",\"params\":\173\"textDocument\":\173\"uri\":[_jstr $uri],\"languageId\":[_jstr $lang],\"version\":$version,\"text\":[_jstr $text]\175\175\175"
    _send $didopen $on_ok
}

proc ::vlsp::_request {method params_json cb {attempt 0}} {
    variable next_id
    variable pending
    variable asked
    set id $next_id
    incr next_id
    dict set pending $id $cb
    dict set asked $id [list $method $params_json $attempt]
    _send "\173\"jsonrpc\":\"2.0\",\"id\":$id,\"method\":[_jstr $method],\"params\":$params_json\175"
    return $id
}

# What each pending request asked, for asking again: id -> {method
# params attempt}. ContentModified (-32801) and ServerCancelled (-32802)
# mean "not now", not "no" — rust-analyzer answers them while it reloads
# its workspace — and the protocol's remedy is the same request again.
# Three tries, a quarter second apart; then the caller gets the error.
namespace eval ::vlsp { variable asked {} }

proc ::vlsp::_retryable {id err} {
    variable asked
    if {![dict exists $asked $id]} { return 0 }
    set code ""
    catch { set code [dict get $err code] }
    if {$code ne "-32801" && $code ne "-32802"} { return 0 }
    return [expr {[lindex [dict get $asked $id] 2] < 3}]
}

# Full-content sync (contentChange with no range = full replace).
proc ::vlsp::sync {text} {
    variable ch
    variable ready
    variable uri
    variable version
    if {$ch eq "" || !$ready} { return }
    incr version
    _remember_text $text
    set change "\173\"text\":[_jstr $text]\175"
    set msg "\173\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didChange\",\"params\":\173\"textDocument\":\173\"uri\":[_jstr $uri],\"version\":$version\175,\"contentChanges\":\[$change\]\175\175"
    # Re-highlight + re-diagnose on every sync — queued behind the
    # didChange's ACK like the open path, so the server never answers a
    # semantic request from the version BEFORE the one it just got
    # (debouncing is the caller's job).
    _send $msg ::vlsp::_request_semantic
}

# The last start's reason for not having a server ("" after a good one),
# and the known limit the gateway reported for this document's setup.
proc ::vlsp::last_error {} { variable last_error ; return $last_error }
proc ::vlsp::note {} { variable note ; return $note }
proc ::vlsp::language {} { variable language ; return $language }

# ── semantic tokens → highlight ─────────────────────────────────────

proc ::vlsp::_request_semantic {} {
    variable uri
    variable ready
    variable server_caps
    if {!$ready} { return }
    # A server without semantic tokens (racket-langserver, some Lisp
    # servers) would leave an edited buffer unhighlighted: the Dev pane
    # syncs to the server INSTEAD of re-running the regex highlighter.
    if {[info exists server_caps] && [llength $server_caps] && ![supports semanticTokensProvider]} {
        _fallback_highlight
        return
    }
    set params "\173\"textDocument\":\173\"uri\":[_jstr $uri]\175\175"
    _request "textDocument/semanticTokens/full" $params ::vlsp::_on_semantic
}

proc ::vlsp::_fallback_highlight {} {
    set w [_paint_widget]
    if {$w ne "" && [llength [info commands ::vsyn::highlight]]} {
        catch { ::vsyn::highlight $w }
    }
}

# Token-type name → editor tag. The names are the legend's: tcl-lsp's
# eight, and the standard set rust-analyzer, clangd and tsserver use.
proc ::vlsp::_tag_for {tt decl} {
    variable legend_types
    set name [expr {$tt < [llength $legend_types]
        ? [lindex $legend_types $tt] : ""}]
    switch -- $name {
        comment   { return syn_comment }
        function  { return [expr {$decl ? "syn_proc" : "syn_func"}] }
        namespace - class - struct - enum - interface - type -
        typeParameter - concept - builtinType - typeAlias - union {
            return syn_ns
        }
        method - macro - decorator - attribute - builtinAttribute {
            return syn_func
        }
        variable - parameter - property - enumMember - member -
        constParameter - label {
            return syn_var
        }
        keyword - modifier - boolean - selfKeyword - selfTypeKeyword {
            return syn_keyword
        }
        string - regexp - character - escapeSequence { return syn_string }
        number { return syn_number }
    }
    return ""
}

proc ::vlsp::_on_semantic {result err} {
    if {$err ne "" || $result eq "" || $result eq "null"} { return }
    set data {}
    catch { set data [dict get $result data] }
    if {$data eq ""} { return }
    uplevel #0 [list ::vlsp::apply_tokens $data]
}

# Pin the paint target explicitly (the Dev pane calls this when it
# opens a buffer, so a detached editor keeps getting ITS tokens rather
# than whatever buffer happens to be current at reply time).
proc ::vlsp::set_apply_widget {w} {
    variable apply_w
    set apply_w $w
}

# Apply delta-encoded tokens to the dev editor.
#
# THE TARGET WIDGET CHANGED WITH THE TAB STRIP (98647b4, 2026-08-24):
# this used to be a constant — the Dev pane held exactly one editor at
# `$::vpanes::R.dev.edit.body`. That widget no longer EXISTS; buffers
# live at `[::vdev::buf_stack].bN` (or a toplevel when detached). For
# weeks `apply_tokens` resolved a dead path, `winfo exists` failed, and
# the tokens were discarded — semantic highlighting stopped working
# while diagnostics (painted via `editor_widget` on the dev side)
# carried on. Never hardcode the buffer widget again: resolve through
# the editor apparatus, with the legacy path only as the LAST fallback
# for harnesses that build a single Text by hand.
proc ::vlsp::_paint_widget {} {
    variable apply_w
    if {$apply_w eq "" || ![winfo exists $apply_w]} {
        set apply_w ""
        if {[llength [info commands ::vdev::dev_editor]]} {
            if {![catch { set apply_w [::vdev::dev_editor] }]} {
                if {![winfo exists $apply_w]} { set apply_w "" }
            }
        }
    }
    if {$apply_w eq ""} { catch { set apply_w $::vpanes::R.dev.edit.body } }
    if {$apply_w eq "" || ![winfo exists $apply_w]} { return "" }
    return $apply_w
}

proc ::vlsp::apply_tokens {data} {
    variable doc_lines
    set w [_paint_widget]
    if {$w eq ""} { return }
    ::vsyn::_tags $w

    # One pass over the token stream, bucketing index pairs per tag. Then
    # ONE remove plus ONE multi-range add per tag. The old shape — a
    # per-token `tag add` — is ~one tag-table update PER TOKEN (tens of
    # thousands on a big file) and that churn is what made sync repaints
    # freeze the editor while the wheel kept queueing events.
    #
    # Starts and lengths are in the server's units; a non-ASCII line is
    # converted (tcl-lsp's utf-8 bytes painted `proc` four characters
    # late after "éé€"). The ASCII test is made once per line.
    array set ranges {}
    set line 0
    set start 0
    set ascii_line -1
    set ascii 1
    foreach {dl ds len tt mods} $data {
        if {$dl > 0} {
            incr line $dl
            set start $ds
        } else {
            incr start $ds
        }
        set decl [expr {$mods & 1}]
        set tag [_tag_for $tt $decl]
        if {$tag eq ""} continue
        if {$line != $ascii_line} {
            set ascii_line $line
            set ascii [string is ascii [lindex $doc_lines $line]]
        }
        if {$ascii} {
            set sc $start
            set ec [expr {$start + $len}]
        } else {
            set sc [_from_server $line $start]
            set ec [_from_server $line [expr {$start + $len}]]
        }
        # LSP lines are 0-based; Tk text lines are 1-based.
        set s "[expr {$line + 1}].$sc"
        set e "[expr {$line + 1}].$ec"
        lappend ranges($tag) [list $s $e]
    }
    foreach t {syn_comment syn_string syn_keyword syn_proc syn_func syn_ns syn_var syn_number} {
        catch { $w tag remove $t 1.0 end }
        if {![info exists ranges($t)] || [llength $ranges($t)] == 0} continue
        set pairs {}
        foreach pair $ranges($t) { lappend pairs {*}$pair }
        catch { $w tag add $t {*}$pairs }
    }
    # The legend has no string/number types — fill those in.
    ::vsyn::extra_passes $w
}

# ── hover ────────────────────────────────────────────────────────────
# Debounced by dev.tcl (Motion → after 300ms → hover_at).

proc ::vlsp::hover_at {line char cb} {
    variable uri
    variable ready
    if {!$ready} { return }
    set char [_to_server $line $char]
    set params "\173\"textDocument\":\173\"uri\":[_jstr $uri]\175,\"position\":\173\"line\":$line,\"character\":$char\175\175"
    _request "textDocument/hover" $params $cb
}

# Render a hover result (markdown) into the tooltip. Minimal md: strip
# ```tcl fences, keep the text (Tk has no markdown in tooltips).
#
# `contents` comes in three shapes: MarkupContent (kind + value; tcl-lsp,
# clangd, rust-analyzer), a MarkedString (a plain string, or language +
# value) and an ARRAY of MarkedStrings (older tsserver bridges). The
# array used to be shown as its Tcl list form.
proc ::vlsp::render_hover {result} {
    if {$result eq "" || $result eq "null"} { return "" }
    set md ""
    catch { set md [dict get $result contents value] }
    if {$md eq ""} {
        set contents ""
        catch { set contents [dict get $result contents] }
        set md $contents
        # A MarkedString with a language: the value is the text.
        if {![catch {dict exists $contents value} one] && $one &&
            [dict exists $contents language]} {
            set md [dict get $contents value]
        } elseif {[llength $contents] > 1 &&
                  ![catch {dict exists [lindex $contents 0] value} arr] && $arr} {
            # An array whose first entry is a language + value pair; the
            # rest are such pairs or plain strings.
            set parts {}
            foreach item $contents {
                if {![catch {dict exists $item value} has] && $has} {
                    lappend parts [dict get $item value]
                } else {
                    lappend parts $item
                }
            }
            set md [join $parts "\n\n"]
        }
    }
    # Strip code fences but keep their content.
    set lines {}
    foreach ln [split $md "\n"] {
        if {[string match "```*" $ln]} continue
        lappend lines $ln
    }
    return [string trim [join $lines "\n"]]
}

# ── completion ───────────────────────────────────────────────────────

proc ::vlsp::complete_at {line char cb} {
    variable uri
    variable ready
    if {!$ready} { return }
    set char [_to_server $line $char]
    set params "\173\"textDocument\":\173\"uri\":[_jstr $uri]\175,\"position\":\173\"line\":$line,\"character\":$char\175\175"
    _request "textDocument/completion" $params $cb
}

# Normalize a completion result into label/kind pairs.
proc ::vlsp::completion_items {result} {
    set items {}
    if {$result eq "" || $result eq "null"} { return $items }
    set list $result
    catch { set list [dict get $result items] }
    foreach c $list {
        set label ""
        catch { set label [dict get $c label] }
        set kind ""
        catch {
            set k [dict get $c kind]
            set kind [lindex {"" text method function constructor field
                variable class interface module property unit value
                keyword snippet color file reference folder enum
                enum_member constant struct event operator type_parameter} $k]
        }
        if {$label ne ""} { lappend items [list $label $kind] }
    }
    return $items
}

# ── definition / go-to (M-.) ────────────────────────────────────────

proc ::vlsp::definition_at {line char cb} {
    variable uri
    variable ready
    if {!$ready} { return }
    set char [_to_server $line $char]
    set params "\173\"textDocument\":\173\"uri\":[_jstr $uri]\175,\"position\":\173\"line\":$line,\"character\":$char\175\175"
    _request "textDocument/definition" $params $cb
}

# A server column on `line` of document `u` as a Tk column — converted
# when `u` is the open document (the only text this client holds).
proc ::vlsp::_from_server_in {u line char} {
    if {[catch { same_document $u } same] || !$same} { return $char }
    return [_from_server $line $char]
}

# Normalize a definition result to a 3-element list: uri, 0-based line,
# 0-based char. Accepts a single Location or LocationLink, or an array of
# either (take the first). Empty when the server found nothing.
proc ::vlsp::definition_target {result} {
    if {$result eq "" || $result eq "null"} { return {} }
    set loc $result
    if {[llength $result] >= 1 &&
        ([dict exists [lindex $result 0] uri] || [dict exists [lindex $result 0] targetUri])} {
        set loc [lindex $result 0]
    }
    set line 0
    set char 0
    if {[dict exists $loc targetUri]} {
        set u [dict get $loc targetUri]
        catch { set line [dict get $loc targetSelectionRange start line] }
        catch { set char [dict get $loc targetSelectionRange start character] }
    } elseif {[dict exists $loc uri]} {
        set u [dict get $loc uri]
        catch { set line [dict get $loc range start line] }
        catch { set char [dict get $loc range start character] }
    } else {
        return {}
    }
    return [list $u $line [_from_server_in $u $line $char]]
}

# ── references ──────────────────────────────────────────────────────

# Every use of the symbol at (line, char), including its declaration.
proc ::vlsp::references_at {line char cb {include_decl 1}} {
    variable uri
    variable ready
    if {!$ready} { return }
    set char [_to_server $line $char]
    set inc [expr {$include_decl ? "true" : "false"}]
    set params "\173\"textDocument\":\173\"uri\":[_jstr $uri]\175,\
\"position\":\173\"line\":$line,\"character\":$char\175,\
\"context\":\173\"includeDeclaration\":$inc\175\175"
    _request "textDocument/references" $params $cb
}

# Normalize a references result to a list of {uri line char} triples,
# in document order. A server may answer null (no references) or a bare
# Location array; both are handled.
proc ::vlsp::reference_list {result} {
    set out {}
    if {$result eq "" || $result eq "null"} { return $out }
    foreach loc $result {
        if {![dict exists $loc uri]} { continue }
        set line 0
        set char 0
        catch { set line [dict get $loc range start line] }
        catch { set char [dict get $loc range start character] }
        set u [dict get $loc uri]
        lappend out [list $u $line [_from_server_in $u $line $char]]
    }
    return $out
}

# ── document highlight ──────────────────────────────────────────────

# Every occurrence of the symbol at (line, char), for highlight-on-demand.
proc ::vlsp::document_highlight_at {line char cb} {
    variable uri
    variable ready
    if {!$ready} { return }
    set char [_to_server $line $char]
    set params "\173\"textDocument\":\173\"uri\":[_jstr $uri]\175,\"position\":\173\"line\":$line,\"character\":$char\175\175"
    _request "textDocument/documentHighlight" $params $cb
}

# Normalize a DocumentHighlight array to a list of
# {kind sline schar eline echar} (0-based). kind: 1 Text, 2 Read, 3 Write.
# Null/empty → empty, so the caller can keep its decorations clear.
proc ::vlsp::highlight_list {result} {
    set out {}
    if {$result eq "" || $result eq "null"} { return $out }
    foreach h $result {
        set kind 0
        set sl 0; set sc 0; set el 0; set ec 0
        catch { set kind [dict get $h kind] }
        catch { set sl [dict get $h range start line] }
        catch { set sc [dict get $h range start character] }
        catch { set el [dict get $h range end line] }
        catch { set ec [dict get $h range end character] }
        # Always the open document's occurrences.
        lappend out [list $kind $sl [_from_server $sl $sc] $el [_from_server $el $ec]]
    }
    return $out
}

# ── rename ───────────────────────────────────────────────────────────

proc ::vlsp::rename_at {line char new_name cb} {
    variable uri
    variable ready
    if {!$ready} { return }
    set char [_to_server $line $char]
    set params "\173\"textDocument\":\173\"uri\":[_jstr $uri]\175,\"position\":\173\"line\":$line,\"character\":$char\175,\"newName\":[_jstr $new_name]\175"
    _request "textDocument/rename" $params $cb
}

# Normalize a WorkspaceEdit.changes to a flat list of
# {uri sline schar eline echar newtext} (0-based). A rename may touch
# several files; every edit is kept so the caller can at least report the
# files it did not apply rather than silently half-renaming.
proc ::vlsp::rename_edit {result} {
    set out {}
    if {$result eq "" || $result eq "null"} { return $out }
    set changes {}
    catch { set changes [dict get $result changes] }
    dict for {uri edits} $changes {
        foreach e $edits {
            set sl 0; set sc 0; set el 0; set ec 0
            catch { set sl [dict get $e range start line] }
            catch { set sc [dict get $e range start character] }
            catch { set el [dict get $e range end line] }
            catch { set ec [dict get $e range end character] }
            # A TextEdit without newText is not an empty replacement —
            # it is a malformed one. Skip it rather than delete the range.
            if {[catch { set nt [dict get $e newText] }]} {
                puts stderr "veles-desktop: lsp rename: TextEdit missing newText, skipped"
                continue
            }
            set sc [_from_server_in $uri $sl $sc]
            set ec [_from_server_in $uri $el $ec]
            lappend out [list $uri $sl $sc $el $ec $nt]
        }
    }
    return $out
}

# ── signature help ──────────────────────────────────────────────────

proc ::vlsp::signature_at {line char cb} {
    variable uri
    variable ready
    if {!$ready} { return }
    set char [_to_server $line $char]
    set params "\173\"textDocument\":\173\"uri\":[_jstr $uri]\175,\"position\":\173\"line\":$line,\"character\":$char\175\175"
    _request "textDocument/signatureHelp" $params $cb
}

# Render a SignatureHelp to one line: the active signature, with the
# active parameter marked. Returns "" when the server has nothing —
# callers use that to keep the popup hidden rather than flashing an
# empty one.
proc ::vlsp::render_signature {result} {
    if {$result eq "" || $result eq "null"} { return "" }
    set sigs {}
    catch { set sigs [dict get $result signatures] }
    if {![llength $sigs]} { return "" }
    set active 0
    catch { set active [dict get $result activeSignature] }
    if {$active >= [llength $sigs]} { set active 0 }
    set sig [lindex $sigs $active]
    set label ""
    catch { set label [dict get $sig label] }
    if {$label eq ""} { return "" }
    # Mark the parameter the cursor is in, when the server says which.
    set ap -1
    catch { set ap [dict get $result activeParameter] }
    catch { set ap [dict get $sig activeParameter] }
    set params {}
    catch { set params [dict get $sig parameters] }
    if {$ap >= 0 && $ap < [llength $params]} {
        set plabel ""
        catch { set plabel [dict get [lindex $params $ap] label] }
        # A parameter label may be a string OR a [start end] offset pair
        # into the signature label; both are in the LSP spec.
        if {[llength $plabel] == 2 && [string is integer -strict [lindex $plabel 0]]} {
            lassign $plabel a b
            set plabel [string range $label $a [expr {$b - 1}]]
        }
        if {$plabel ne "" && [string first $plabel $label] >= 0} {
            set label [string map [list $plabel "\u2039$plabel\u203a"] $label]
        }
    }
    return $label
}

# ── document symbols / outline ──────────────────────────────────────

proc ::vlsp::document_symbols {cb} {
    variable uri
    variable ready
    if {!$ready} { return }
    set params "\173\"textDocument\":\173\"uri\":[_jstr $uri]\175\175"
    _request "textDocument/documentSymbol" $params $cb
}

# Normalize to a flat list of 3-element lists: name, kind label, 0-based
# line. Accepts SymbolInformation (location key) or DocumentSymbol
# (range key) shapes from the server.
proc ::vlsp::symbol_list {result} {
    set items {}
    if {$result eq "" || $result eq "null"} { return $items }
    set list $result
    foreach s $list {
        set name ""
        set kind ""
        set line 0
        catch { set name [dict get $s name] }
        catch {
            set k [dict get $s kind]
            set kind [lindex {"" file module namespace package class method
                property field constructor enum interface function variable
                constant string number boolean array object key null
                enum_member struct event operator type_parameter} $k]
        }
        if {[dict exists $s range]} {
            catch { set line [dict get $s range start line] }
        } elseif {[dict exists $s location]} {
            catch { set line [dict get $s location range start line] }
        }
        if {$name ne ""} { lappend items [list $name $kind $line] }
    }
    return $items
}

# ── folding ranges ───────────────────────────────────────────────────
#
# The server knows where a proc, a namespace or a comment block starts
# and ends; without asking it, an editor either guesses from indentation
# or cannot fold at all.

proc ::vlsp::folding_ranges {cb} {
    variable uri
    variable ready
    if {!$ready} { return }
    set params "\173\"textDocument\":\173\"uri\":[_jstr $uri]\175\175"
    _request "textDocument/foldingRange" $params $cb
}

# Normalize a FoldingRange array to {startLine endLine kind} triples,
# 0-based, longest first — so a nested fold is found before the outer one
# when the caller looks for "the fold at this line".
proc ::vlsp::folding_list {result} {
    set out {}
    if {$result eq "" || $result eq "null"} { return $out }
    foreach f $result {
        set sl -1; set el -1; set kind ""
        catch { set sl [dict get $f startLine] }
        catch { set el [dict get $f endLine] }
        catch { set kind [dict get $f kind] }
        # A range that does not span a line hides nothing; dropping it
        # here keeps "toggle the fold here" from finding an empty one.
        if {$sl < 0 || $el <= $sl} { continue }
        lappend out [list $sl $el $kind]
    }
    return [lsort -integer -index 0 $out]
}

# ── code actions ─────────────────────────────────────────────────────

proc ::vlsp::code_action_at {sline schar eline echar cb {diags {}}} {
    variable uri
    variable ready
    if {!$ready} { return }
    set schar [_to_server $sline $schar]
    set echar [_to_server $eline $echar]
    set range "\173\"start\":\173\"line\":$sline,\"character\":$schar\175,\"end\":\173\"line\":$eline,\"character\":$echar\175\175"
    set params "\173\"textDocument\":\173\"uri\":[_jstr $uri]\175,\"range\":$range,\"context\":\173\"diagnostics\":\133\135\175\175"
    _request "textDocument/codeAction" $params $cb
}

# Normalize a CodeAction[] | Command[] result to {title kind edit command}
# quadruples. `edit` is a WorkspaceEdit (or ""), `command` a Command dict
# (or ""). Both shapes are on the wire: a server may answer with bare
# Commands, in which case the title is the only thing they share.
proc ::vlsp::code_action_list {result} {
    set out {}
    if {$result eq "" || $result eq "null"} { return $out }
    foreach a $result {
        set title ""
        set kind ""
        set edit ""
        set command ""
        catch { set title [dict get $a title] }
        catch { set kind [dict get $a kind] }
        catch { set edit [dict get $a edit] }
        if {[dict exists $a command]} {
            set c [dict get $a command]
            # A CodeAction carries a Command in `command`; a bare Command
            # IS the object, and then `command` is the string id.
            if {[catch { dict get $c command }]} {
                set command $a
            } else {
                set command $c
            }
        } elseif {![dict exists $a edit] && [dict exists $a title]} {
            # Neither edit nor command: nothing to run.
            set command ""
        }
        if {$title eq ""} { continue }
        lappend out [list $title $kind $edit $command]
    }
    return $out
}

# A WorkspaceEdit's changes, flattened the same way rename_edit does —
# code actions and renames both answer with one, so they share the shape
# the editor applies.
proc ::vlsp::workspace_edit {edit} { return [rename_edit $edit] }

proc ::vlsp::execute_command {command cb} {
    variable ready
    if {!$ready} { return }
    set name ""
    catch { set name [dict get $command command] }
    if {$name eq ""} { return }
    set args "\133\135"
    if {[dict exists $command arguments]} {
        if {[catch {
            set list {}
            foreach a [dict get $command arguments] { lappend list $a }
            # Arguments are opaque, server-defined JSON. We only ever hand
            # back what the server gave us, so re-encoding strings is enough
            # for the servers that use them at all.
            set parts {}
            foreach a $list { lappend parts [_jstr $a] }
            set args "\133[join $parts ,]\135"
        } arg_err]} {
            puts stderr "veles-desktop: lsp execute_command: arguments dropped: $arg_err"
        }
    }
    set params "\173\"command\":[_jstr $name],\"arguments\":$args\175"
    _request "workspace/executeCommand" $params $cb
}

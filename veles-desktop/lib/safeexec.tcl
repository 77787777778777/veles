# veles-desktop/lib/safeexec.tcl — hand a remote string to a program
# without letting it become a command.
#
# WHY THIS EXISTS
#
# Tcl's `exec` parses redirection operators OUT OF THE ARGUMENT LIST
# ITSELF, before it spawns anything. A word beginning with `>` is an
# output redirection; a word that is exactly `|` starts a pipeline. That
# is not shell quoting — there is no shell — it is exec's own grammar,
# and it applies to the CONTENTS of a variable just as much as to a
# literal you typed.
#
# irc.tcl:869 did this, which looks completely ordinary:
#
#     exec $notify_path $title $body &
#
# `$title` is the sender's NICK and `$body` is the MESSAGE TEXT. Both
# come off the wire from a stranger; a private message notifies
# unconditionally, so no channel membership is needed. Demonstrated on
# this machine, against tclsh 8.6.16:
#
#   text = ">/home/user/.ssh/authorized_keys"
#       → the file is TRUNCATED. It happens even when the command
#         cannot be found and even with `&`, because exec opens the
#         redirection target before it tries to spawn.
#   nick = "|", text = "id"
#       → `id` RUNS as the desktop user. `|` is a legal nick character
#         (RFC 2812 special = "[" / "]" / "\" / "`" / "_" / "^" / "{" /
#         "|" / "}"), so the attacker supplies both words of a pipeline.
#
# So: arbitrary program execution and arbitrary file truncation, from
# one IRC message, with no authentication.
#
# This is not a novel bug, which is the point of writing it down. It is
# the class behind XChat's CVE-2010-2192 (a remote `ircs://` URI reached
# the handler's `--command` argument) — a remote string that stopped
# being data and became an argument. Every client in the survey has one:
# WeeChat WSA-2026-11 and WSA-2026-6 are the filesystem-path version,
# HexChat's CVE-2016-2233 is `..` in a server name.
#
# WHAT THE FIX IS
#
# The dangerous forms are all decided by a word's FIRST characters, so
# a single leading space defeats every one of them — verified, not
# assumed: exec sees " >file" as a plain argument. A leading space in a
# notification body or a window title is invisible to the reader, which
# is why this is preferable to dropping or escaping the character and
# lying to the user about what was said.
#
# `-` gets the same treatment for a different reason: it is not exec
# redirection, it is option injection into whatever program is being
# spawned. `notify-send -t 1` is not a security boundary, but a message
# that silently becomes a flag is still a message the reader never saw.

namespace eval ::vsafe {
    # Schemes a URL may carry before we hand it to a system opener.
    # Everything else — file:, data:, javascript:, and the ircs: family
    # that produced the XChat CVE — is refused. slopdrop's URL validator
    # makes the same call for the same reason (PUBLIC_DEPLOYMENT_SECURITY
    # .md, "SSRF Prevention"): an allow-list is the only kind of scheme
    # check that stays correct as handlers get added to the desktop.
    variable url_schemes {http https mailto}
}

# True if `exec` would read this word as redirection or a pipeline
# rather than as an argument.
#
# The grammar, from the exec(n) manual: `|` and `|&` are pipelines;
# `<`, `<<` and `<@` are input; `>`, `>>`, `>&`, `>>&` and `>@` are
# output; and any of the output forms may carry a leading file
# descriptor number (`2>`, `2>>`, `2>@`).
proc ::vsafe::is_exec_operator {s} {
    if {$s eq "|" || $s eq "|&"} { return 1 }
    if {[string match {<*} $s]} { return 1 }
    if {[regexp {^[0-9]*[>]} $s]} { return 1 }
    return 0
}

# Make one string safe to pass to `exec` as a single argument word.
# Returns the string unchanged when it was already safe, so the common
# case costs nothing and the diff at each call site stays readable.
proc ::vsafe::arg {s} {
    if {[is_exec_operator $s] || [string match {-*} $s]} {
        return " $s"
    }
    return $s
}

# Percent-encode one string so it can be a URL path segment or query
# value rather than URL SYNTAX.
#
# Same family as everything else in this file — a value that stops being
# a value and becomes structure — which is why it lives here rather than
# in ::vapi or a pane: safeexec.tcl is sourced before both, so ::virc
# and ::vinstpane can share ONE implementation without depending on each
# other. The Instances pane's first version escaped cron job ids with
# ::vapi::json_escape, a JSON *string* escaper, so an id containing `/`
# or `?` addressed a different endpoint than the one intended.
#
# Unreserved set is RFC 3986's: everything else goes to %XX over the
# UTF-8 bytes.
#
# THE DASH GOES FIRST in the bracket expression, and that is not style.
# Tcl's `string match` does NOT treat a trailing `-` as literal the way
# a POSIX bracket expression does: with `[A-Za-z0-9._~-]` the hyphen
# falls out of the set entirely, so every `-` was percent-encoded as
# %2D. Harmless (a server decodes it identically) but wrong, and it was
# in this code for as long as ::virc::_urlenc has existed. Verified both
# forms against tclsh 8.6.16 rather than reasoned about.
proc ::vsafe::urlenc {s} {
    set out ""
    foreach ch [split $s ""] {
        if {[string match {[-A-Za-z0-9._~]} $ch]} {
            append out $ch
        } else {
            foreach b [split [encoding convertto utf-8 $ch] ""] {
                append out [format "%%%02X" [scan $b %c]]
            }
        }
    }
    return $out
}

# A URL fit to hand to xdg-open, or "" if it is not one.
#
# Returns "" rather than throwing: every caller is a click handler or a
# notification, where the right answer to a hostile URL is to do nothing
# visible, not to raise a dialog that tells the attacker their string
# arrived.
proc ::vsafe::url {u} {
    variable url_schemes
    set u [string trim $u]
    if {![regexp {^([A-Za-z][A-Za-z0-9+.-]*):} $u -> scheme]} { return "" }
    if {[string tolower $scheme] ni $url_schemes} { return "" }
    # A scheme-bearing URL cannot begin with an exec operator, but run it
    # through `arg` anyway: the check above is the thing most likely to
    # be loosened later, and this way loosening it stays safe.
    return [arg $u]
}

# Fire-and-forget launch of a helper program with remote-supplied
# arguments. This is the shape every risky call site in the desktop
# actually has — a notifier, a browser, a file opener — and routing them
# through one proc is what lets uitest.tcl assert that no module grew a
# tenth one.
#
# EVERY argument is treated as untrusted, so a literal flag must NOT be
# passed here: `spawn $term --directory $dir` would send " --directory"
# and the flag would arrive as a positional argument instead. Written
# down because the first draft of this very fix did exactly that. A call
# that needs literal flags uses `exec` directly and wraps only its data
# words in `arg`; the uitest check accepts both shapes.
#
# Errors are swallowed deliberately and reported by the return value:
# the callers are notifications and click handlers, where "the opener is
# not installed" must not propagate out of an event binding.
proc ::vsafe::spawn {cmd args} {
    if {$cmd eq ""} { return 0 }
    set safe {}
    foreach a $args { lappend safe [arg $a] }
    if {[catch { exec $cmd {*}$safe & }]} { return 0 }
    return 1
}

# veles-desktop/lib/markdown.tcl — markdown → Tk text-widget renderer.
#
# Renders agent output the way hermes-desktop does (rendered markdown,
# not raw asterisks). Supported: headings (h1-h3), **bold**, *italic*,
# `inline code`, fenced code blocks (with language label), [links](url)
# (clickable), bullet/numbered lists with nesting, > blockquotes,
# horizontal rules. GFM tables have aligned, independently wrapping
# columns; their cells retain inline formatting and links.
#
# Usage:
#   ::vmd::render $w $text        append rendered markdown at end
#   ::vmd::plain  $w $text        append unrendered (user messages)
#
# Streaming strategy (matches hermes-desktop): deltas stream as raw
# text; on turn completion the region is re-rendered through here.
# Incremental markdown parsing mid-stream is brittle by nature.

namespace eval ::vmd {
    variable fonts_done 0
    variable tags_done
}

# Links and headers take their clicks through ::vtext::click (a tap's
# release on a phone, the press elsewhere). Where lib/vtext.tcl is not
# loaded (a renderer on its own, as some suites use it), a click is the
# press; vtext.tcl's own definition replaces this one wherever it is.
if {![llength [info commands ::vtext::click]]} {
    namespace eval ::vtext {}
    proc ::vtext::click {w tag script} { $w tag bind $tag <Button-1> $script }
}

# ── fonts (created once, derived from theme fonts) ───────────────────
proc ::vmd::_fonts {} {
    variable fonts_done
    if {$fonts_done} return
    set fonts_done 1
    set fam [font actual TkDefaultFont -family]
    set size [font actual TkDefaultFont -size]
    set ffam [font actual TkFixedFont -family]
    foreach name [list md_h1 md_h2 md_h3 md_bold md_italic md_bolditalic \
        md_code md_codeblock md_fencelang md_bullet md_quote md_link md_hr] {
        catch { font delete $name }
    }
    font create md_h1 -family $fam -size [expr {$size + 4}] -weight bold
    font create md_h2 -family $fam -size [expr {$size + 2}] -weight bold
    font create md_h3 -family $fam -size [expr {$size + 1}] -weight bold
    font create md_bold -family $fam -size $size -weight bold
    font create md_italic -family $fam -size $size -slant italic
    font create md_bolditalic -family $fam -size $size -weight bold -slant italic
    font create md_code -family $ffam -size $size
    font create md_codeblock -family $ffam -size $size
    font create md_fencelang -family $ffam -size [expr {$size - 1}] -slant italic
    font create md_bullet -family $fam -size $size
    font create md_quote -family $fam -size $size -slant italic
    font create md_link -family $fam -size $size -underline 1
    font create md_hr -family $fam -size $size
}

# ── tags (per widget, idempotent) ────────────────────────────────────
proc ::vmd::_tags {w} {
    variable tags_done
    if {[info exists tags_done($w)]} return
    _fonts
    $w tag configure md_h1 -font md_h1 -foreground $::vt::T(accent) \
        -spacing1 8 -spacing3 4
    $w tag configure md_h2 -font md_h2 -foreground $::vt::T(cyan) \
        -spacing1 6 -spacing3 3
    $w tag configure md_h3 -font md_h3 -foreground $::vt::T(purple) \
        -spacing1 5 -spacing3 2
    $w tag configure md_bold -font md_bold -foreground $::vt::T(fg)
    $w tag configure md_italic -font md_italic -foreground $::vt::T(fg_dim)
    $w tag configure md_bolditalic -font md_bolditalic -foreground $::vt::T(fg)
    $w tag configure md_code -font md_code -foreground $::vt::T(gold) \
        -background $::vt::T(bg_panel)
    $w tag configure md_codeblock -font md_codeblock -background $::vt::T(bg_input) \
        -foreground $::vt::T(cyan) \
        -lmargin1 12 -lmargin2 12 -rmargin 12 \
        -spacing1 2 -spacing3 2 -relief flat -borderwidth 0
    $w tag configure md_fencelang -font md_fencelang -foreground $::vt::T(gold) \
        -background $::vt::T(bg_input) -lmargin1 12 -lmargin2 12
    $w tag configure md_bullet -lmargin1 8 -lmargin2 24 \
        -foreground $::vt::T(accent)
    $w tag configure md_list -lmargin1 8 -lmargin2 24 \
        -foreground $::vt::T(fg)
    $w tag configure md_quote -font md_quote -foreground $::vt::T(fg_dim) \
        -lmargin1 14 -lmargin2 14 -spacing1 2 -spacing3 2
    $w tag configure md_link -font md_link -foreground $::vt::T(cyan)
    $w tag configure md_hr -foreground $::vt::T(hairline)
    # Column heading reused as a field label in the stacked table
    # layout: dim, so the VALUE is what the eye lands on.
    $w tag configure md_tablelabel -foreground $::vt::T(fg_dim) \
        -font md_fencelang
    ::vtext::click $w md_link {::vmd::_link_click %W @%x,%y}
    $w tag bind md_link <Enter> {%W configure -cursor hand2}
    $w tag bind md_link <Leave> {%W configure -cursor xterm}
    # Double-click a code block to copy it — the common reason anyone
    # reaches for the mouse in a chat transcript.
    $w tag bind md_codeblock <Double-Button-1> {::vmd::_copy_block %W @%x,%y}
    $w tag bind md_codeblock <Enter> {%W configure -cursor hand2}
    $w tag bind md_codeblock <Leave> {%W configure -cursor xterm}
    # Done-flag set LAST: if any tag configure above throws, the next
    # _tags call retries instead of leaving the widget half-tagged
    # forever (the Aug 11 -rmargin1 bug hid behind an early flag + a
    # catch in theme.tcl, so code blocks silently rendered unstyled).
    set tags_done($w) 1
}

# Copy the whole fenced block under the pointer.
proc ::vmd::_copy_block {w coord} {
    if {[catch { $w index $coord } idx]} return
    set range [$w tag prevrange md_codeblock "$idx +1c"]
    if {[llength $range] != 2} {
        set range [$w tag nextrange md_codeblock $idx]
    }
    if {[llength $range] != 2} return
    lassign $range s e
    set code [$w get $s $e]
    clipboard clear
    clipboard append $code
    ::vstatus::set_msg [format [::vi18n::mc {copied code block (%s chars)}] [string length $code]]
}

# Insert a clickable link. The URL is carried by a UNIQUE PER-LINK TAG,
# not by line number: tags ride along with the text through every
# insert/delete, whereas line numbers go stale the moment anything above
# changes (and a line with two links could only remember one of them).
proc ::vmd::_emit_link {w label url} {
    variable _link_seq
    variable _link_url
    variable _link_pool
    if {![info exists _link_seq]} { set _link_seq 0 }
    if {![info exists _link_pool]} { set _link_pool {} }
    incr _link_seq
    # Recycle from a bounded pool so a link-heavy session never grows
    # the widget's tag table without bound (tags are the slow-scroll
    # killer in Tk text widgets). Reusing a tag deletes it from the
    # widget first — old text loses its link, new text gains it.
    if {[llength $_link_pool] >= 96} {
        set tag [lindex $_link_pool 0]
        set _link_pool [lrange $_link_pool 1 end]
        catch { $w tag delete $tag }
        unset -nocomplain _link_url($tag)
    } else {
        set tag "mdlink_$_link_seq"
    }
    set _link_url($tag) $url
    lappend _link_pool $tag
    _emit $w $label [list md_link $tag]
}

# Render a markdown image.
#
# A LOCAL file goes to ::vmedia, which is the same path the preview pane
# and MEDIA: markers already use — it draws the picture when Tk (or a
# converter on PATH) can decode it, and an honest artifact card naming
# the format when it cannot. So an SVG or a webp becomes a card here
# rather than nothing, which is what ::vmedia exists to guarantee.
#
# A REMOTE url stays a link. Fetching it would mean the transcript makes
# network requests to whatever a model happened to write, which is a
# different decision from "render markdown" and not one to take silently.
#
# `file://` counts as local — it is how the agent refers to a path it
# just wrote.
proc ::vmd::_emit_image {w label url} {
    set path $url
    if {[string match "file://*" $path]} { set path [string range $path 7 end] }
    if {[regexp {^[a-zA-Z][a-zA-Z0-9+.-]*://} $path]} {
        # Remote: a link, labelled so it is obviously an image.
        _emit_link $w [expr {$label eq "" ? $url : $label}] $url
        return
    }
    if {![file isfile $path]} {
        # Say the path. A broken image that renders as its alt text alone
        # is indistinguishable from prose, and the alt text is exactly
        # what does NOT tell you which file is missing.
        _emit $w [::vi18n::mc {\[missing image: %s\]} $path] [list md_code]
        return
    }
    if {$label ne ""} { catch { ::va11y::name $w $label } } ;# i18n-exempt: alt text from the document
    if {[catch { ::vmedia::insert $w $path } e]} {
        _emit $w [::vi18n::mc {\[image: %1$s — %2$s\]} $path $e] [list md_code]
    }
}

# Forget every link target for this widget (session switch / clear).
proc ::vmd::forget_links {w} {
    variable _link_url
    variable _link_pool
    foreach tag [array names _link_url] {
        catch { $w tag delete $tag }
    }
    array unset _link_url
    set _link_pool {}
    # Table tags are minted per table (each carries its own -tabs), so
    # they accumulate with the transcript and have to go with it.
    foreach tag [$w tag names] {
        if {[string match "md_table*" $tag]} { catch { $w tag delete $tag } }
    }
}

# `%x,%y` from the binding is a COORDINATE, not a text index — it has to
# be converted with `$w index` first. It never was, so the lookup key was
# the literal string "@123,45", `info exists` was always false, and every
# click silently did nothing. That was the whole bug.
proc ::vmd::_link_click {w coord} {
    variable _link_url
    if {$coord eq ""} return
    if {[catch { $w index $coord } idx]} return
    foreach tag [$w tag names $idx] {
        if {[info exists _link_url($tag)]} {
            open_url $_link_url($tag)
            return
        }
    }
}

# Hand a URL to the desktop browser. Reports what actually happened —
# the old version claimed "opened $url" unconditionally, even when the
# exec had failed, which is a large part of why this looked mysterious.
proc ::vmd::open_url {url} {
    # A markdown link target is MODEL-emitted text, so the scheme is
    # allow-listed before anything is spawned. A system opener will act
    # on file:, data: and the ircs: family just as happily as on http —
    # handing it whichever one arrives is XChat's CVE-2010-2192.
    set safe [::vsafe::url $url]
    if {$safe eq ""} {
        ::vstatus::set_msg [format [::vi18n::mc {refused to open %s (only http, https, mailto)}] $url]
        return 0
    }
    # On a phone (AndroWish) there is no xdg-open or $BROWSER: Android
    # opens a link with an intent, in the browser the person chose. Every
    # link the agent wrote did nothing there until 2026-10-11.
    if {[info commands ::borg] ne ""} {
        if {[catch { borg activity android.intent.action.VIEW $safe {} }]} {
            ::vstatus::set_msg [format [::vi18n::mc {could not open %s (no working browser opener)}] $url]
            return 0
        }
        ::vstatus::set_msg [format [::vi18n::mc {opened %s}] $url]
        return 1
    }
    if {$::tcl_platform(platform) eq "windows"} {
        # `start` is a cmd builtin; auto_execok resolves it to `cmd /c
        # start`. The empty "" fills start's window-title slot, so the
        # URL is never read as one. The URL went through ::vsafe::url
        # above and goes through ::vsafe::arg here as well.
        set start [auto_execok start]
        if {$start ne "" && ![catch { exec {*}$start "" [::vsafe::arg $safe] & }]} {
            ::vstatus::set_msg [format [::vi18n::mc {opened %s}] $url]
            return 1
        }
    }
    set openers {}
    if {[info exists ::env(BROWSER)] && $::env(BROWSER) ne ""} {
        lappend openers $::env(BROWSER)
    }
    lappend openers xdg-open open sensible-browser
    foreach cmd $openers {
        if {[auto_execok $cmd] eq ""} continue
        if {[::vsafe::spawn [lindex [auto_execok $cmd] 0] $safe]} {
            ::vstatus::set_msg [format [::vi18n::mc {opened %s}] $url]
            return 1
        }
    }
    ::vstatus::set_msg [format [::vi18n::mc {could not open %s (no working browser opener)}] $url]
    return 0
}

# ── inline tokenizer ─────────────────────────────────────────────────
# Scans for the leftmost of: `code`, **bold**, [link](url), *italic*.
# Protected constructs win by priority; plain runs get the base tag.
proc ::vmd::_inline {w text basetag} {
    while {[string length $text] > 0} {
        set best_start -1
        set best_type ""
        set best_match {}

        # `inline code` — CM 6.1 runs: a span opens on a BACKTICK RUN and
        # closes on the NEXT run of the same length, with one
        # leading/trailing space inside stripped when both sides carry
        # one. The old matcher {`[^`\n]+`} could only strip ONE tick
        # per side, so on a triple run it claimed the middle tick and
        # left the outer pair as literal text (xenia 2026-09-12:
        # `look: ```block``` done` → ``block``).
        set code_n 0
        if {[set span [_code_span_indices $text]] ne ""} {
            lassign $span s e code_n
            if {$best_start < 0 || $s < $best_start} {
                set best_start $s; set best_end $e; set best_type code
            }
        }
        # **bold** — non-greedy and asterisk-tolerant, so a nested
        # *italic* (or a link) inside the span does not break the match.
        # The old class was [^*\n]+, which cannot cross a single `*`, so
        # `**a *b* c**` never matched as bold at all: the italic branch
        # claimed `*b*` and the `**` markers were left in the text as
        # literal asterisks. _inline_bold — the proc written to handle
        # exactly this case — was unreachable.
        if {[regexp -indices {\*\*[^\n]+?\*\*} $text m]} {
            lassign $m s e
            if {$best_start < 0 || $s < $best_start} {
                set best_start $s; set best_end $e; set best_type bold
            }
        }
        # ![alt](path) — AN IMAGE, and it has to be tested before the
        # link branch below. `![x](/a.png)` matches the link pattern from
        # the `[`, one character in, so the link branch wins the
        # earliest-match race by starting later only if this one is
        # missing entirely. It was: markdown images had NO handling here
        # at all, so the `!` came out as literal text and the picture
        # became a clickable link labelled with its alt text. USER: "The
        # xorg logo seems to only render in the preview but not in the
        # transcript" — the preview pane renders the file, the transcript
        # rendered the alt text.
        if {[regexp -indices {!\[[^\]\n]*\]\([^)\n]+\)} $text m]} {
            lassign $m s e
            if {$best_start < 0 || $s < $best_start} {
                set best_start $s; set best_end $e; set best_type image
            }
        }
        # [link](url)
        if {[regexp -indices {\[[^\]\n]+\]\([^)\n]+\)} $text m]} {
            lassign $m s e
            if {$best_start < 0 || $s < $best_start} {
                set best_start $s; set best_end $e; set best_type link
            }
        }
        # <https://example.com> — CommonMark's angle autolink. Needs its
        # own branch BEFORE the bare-URL one: the bare-URL class stops
        # at `>`, so the brackets were left as literal text on either
        # side of the link. Models emit this constantly inside tables.
        if {[regexp -indices {<https?://[^>\s]+>} $text m]} {
            lassign $m s e
            if {$best_start < 0 || $s < $best_start} {
                set best_start $s; set best_end $e; set best_type anglelink
            }
        }
        # bare URL — http(s)://… not already inside [text](url), which
        # the link branch above claims first because it starts earlier.
        # Without this a plain pasted URL was dead text.
        if {[regexp -indices {https?://[^\s<>()\[\]"']+} $text m]} {
            lassign $m s e
            if {$best_start < 0 || $s < $best_start} {
                set best_start $s; set best_end $e; set best_type autolink
            }
        }
        # *italic* / _italic_ — only if not part of **
        if {[regexp -indices {(^|[^*])\*[^*\n]+\*} $text m]} {
            lassign $m s e
            # skip the leading non-* char captured for disambiguation
            set s2 $s
            set ch [string index $text $s]
            if {$ch ne "*"} { incr s2 }
            if {$best_start < 0 || $s2 < $best_start} {
                set best_start $s2; set best_end $e; set best_type italic
            }
        }

        if {$best_start < 0} {
            # no more constructs — emit the rest as plain
            _emit $w $text [list $basetag]
            set text ""
            break
        }

        # plain run before the construct
        if {$best_start > 0} {
            _emit $w [string range $text 0 [expr {$best_start - 1}]] [list $basetag]
        }
        set chunk [string range $text $best_start $best_end]
        switch -- $best_type {
            code {
                # Both delimiters are still ON the chunk — strip the
                # equal run on each side (CM 6.1), then the one-space
                # rule when both sides carry one.
                set n $code_n
                set inner [string range $chunk $n end-${n}]
                if {[string index $inner 0] eq " " \
                    && [string index $inner end] eq " "} {
                    set inner [string range $inner 1 end-1]
                }
                _emit $w $inner {md_code}
            }
            bold {
                set inner [string range $chunk 2 end-2]
                # A LINK OR INLINE CODE INSIDE BOLD used to be emitted
                # literally. The bold span starts before the link, so it
                # wins the earliest-match race above, and this branch
                # then inserted its inner text as one plain md_bold run —
                # brackets, parens, URL and all. `**see [docs](url)**` is
                # an extremely common shape in model output, and it
                # rendered as the raw markdown.
                #
                # Recurse instead, so the inner constructs are parsed
                # with md_bold as the base tag for the plain runs.
                if {[_has_nested $inner]} {
                    _inline $w $inner md_bold
                } elseif {[regexp {\*[^*\n]+\*} $inner]} {
                    # nested italic inside bold: **a *b* c**
                    _inline_bold $w $inner
                } else {
                    _emit $w $inner {md_bold}
                }
            }
            italic {
                set inner [string range $chunk 1 end-1]
                if {[_has_nested $inner]} {
                    _inline $w $inner md_italic
                } else {
                    _emit $w $inner {md_italic}
                }
            }
            image {
                regexp {^!\[([^\]]*)\]\(([^)]+)\)$} $chunk -> label url
                _emit_image $w $label $url
            }
            link {
                regexp {^\[([^\]]+)\]\(([^)]+)\)$} $chunk -> label url
                _emit_link $w $label $url
            }
            autolink {
                _emit_link $w $chunk $chunk
            }
            anglelink {
                set url [string range $chunk 1 end-1]
                _emit_link $w $url $url
            }
        }
        set text [string range $text [expr {$best_end + 1}] end]
    }
}

# Pull the next `code` span out of $text as {start end ticks} or {}.
#
# CM 6.1's tick-run rule: an opening run of N backticks closes at the
# next run of at LEAST N backticks; the closer is the first N of that
# run, and the span's content is whatever falls between, newlines
# included. There is no shortest-run preference — the FIRST run of any
# length past the opener's own is the closer, even if a shorter one
# would have closed sooner (the flourish # in CM 6.1's examples).
#
# This is why quote-mounting a backtick run in prose needs odd counts:
# ``` in the middle of a line finds no equal second run and passes
# through as text, exactly like CM.
proc ::vmd::_code_span_indices {text} {
    # ONLY THE FIRST tick run may open: -indices (not -all) returns
    # one pair or nothing. (A failed regexp LEAVES the capture
    # variable from a previous call — hence the fenced locals, which
    # is the stale-m bug that made every no-match look like a hit.)
    if {![regexp -indices {`+} $text full]} { return {} }
    set os "" ; set oe ""
    lassign $full os oe
    set n [expr {$oe - $os + 1}]
    # NO closer with n ticks → -indices yields {}. A shorter run
    # (e.g. opener ```, closer ``) is NOT a closer per CM 6.1.
    if {![regexp -indices -start [expr {$oe + 1}] "`{$n}" $text closer]} { return {} }
    set cs "" ; set ce ""
    lassign $closer cs ce
    return [list $os $ce $n]
}

# Does this span contain a construct that must still be parsed inside
# an emphasis run? Bare URLs count: `**see https://x.example**` has the
# same problem as a bracketed link.
proc ::vmd::_has_nested {text} {
    if {[regexp {\[[^\]\n]+\]\([^)\n]+\)} $text]} { return 1 }
    if {[regexp {`[^`\n]+`} $text]}                     { return 1 }
    if {[regexp {https?://[^\s<>()\[\]"']+} $text]}    { return 1 }
    return 0
}

# bold containing italic: split on *...* inside
proc ::vmd::_inline_bold {w text} {
    while {[regexp -indices {\*[^*\n]+\*} $text m]} {
        lassign $m s e
        if {$s > 0} {
            _emit $w [string range $text 0 [expr {$s - 1}]] {md_bold}
        }
        _emit $w [string range $text [expr {$s + 1}] [expr {$e - 1}]] {md_bolditalic}
        set text [string range $text [expr {$e + 1}] end]
    }
    if {[string length $text]} {
        _emit $w $text {md_bold}
    }
}

# The single run-insert primitive for markdown. Every byte ::vmd writes
# into a text widget goes through here, so "how a markdown run is
# written" is decided in ONE place (the transcript's ::vchat::_run and
# media's ::vmedia::_emit are the sibling chokepoints). Callers hold the
# widget writable; this is a pure pass-through, so it adds no state
# toggling. tags defaults to {} so a bare newline run needs no empty
# list at the call site.
proc ::vmd::_emit {w text {tags {}}} {
    $w insert end $text $tags
}

# ── block-level renderer ─────────────────────────────────────────────
# ── where a growing document has stopped changing ────────────────────
#
# A live streaming region used to be re-rendered WHOLE on every tick, so
# a turn paid O(answer x turn-length) and the Tk event loop — the same
# thread that reads the socket — starved while it ran. The fix needs one
# fact the renderer already knows and never exposed: how much of the text
# so far can never be affected by what arrives next.
#
# That boundary is a blank line at fence depth zero. Everything before
# one renders identically whether it is rendered now or with the rest of
# the document later, because a blank line closes every block `render`
# tracks across lines — a paragraph, a list, a quote, and a table (the
# table branch flushes on the first non-table line, and a blank line is
# one). A blank line INSIDE a fenced block closes nothing, which is
# exactly why this shares `render`'s fence state machine rather than
# scanning for "\n\n": settling mid-fence would render half a code block
# as prose and leave its closer dangling.
#
# Returns the character index just past the last such boundary, or 0 when
# nothing has settled yet.
proc ::vmd::settle_point {text} {
    set fence 0
    set fence_len 0
    set pos 0
    set settle 0
    set lines [split $text "\n"]
    set last [expr {[llength $lines] - 1}]
    for {set i 0} {$i <= $last} {incr i} {
        set line [lindex $lines $i]
        # +1 for the newline `split` consumed.
        set next [expr {$pos + [string length $line] + 1}]
        if {$fence} {
            # CM 4.5, the same closer rule render uses: the opening run's
            # tick count, optional trailing whitespace.
            if {[regexp "^ {0,3}`{${fence_len}}\[ \t\]*\$" $line]} {
                set fence 0
                set fence_len 0
            }
        } elseif {[regexp {^ {0,3}(`{3,})} $line -> ticks]} {
            set fence 1
            set fence_len [string length $ticks]
        } elseif {$i < $last && [string trim $line] eq ""} {
            # $i < $last because the element after the text's FINAL
            # newline is a phantom — the absence of more text, not a
            # blank line in the document. Settling on it would close a
            # block the next delta may still be writing, and two table
            # rows split across that cut render as two tables, not one.
            set settle $next
        }
        set pos $next
    }
    return $settle
}

proc ::vmd::render {w text} {
    _tags $w
    variable _current_w
    set _current_w $w
    set fence 0
    set fence_lang ""
    set fence_len 0
    set table_buf {}
    set in_table 0

    foreach line [split $text "\n"] {
        # ── fenced code block state ──
        if {$fence} {
            # CM 4.5: the closer is the OPENING run's tick count (with
            # optional trailing whitespace). A fixed 3-tick closer used
            # to end a 4-tick fence early — its body swallowed the real
            # closer, which then printed as a fake language label.
            if {[regexp -indices "^ {0,3}`{${fence_len}}\[ \t\]*\$" $line]} {
                set fence 0
                set fence_len 0
                _emit $w "\n" {}
                continue
            }
            # A longer tick run than the opener's is CONTENT (CM 4.5) —
            # inside a 3-tick fence, a 4-tick line is code, not a
            # nested opener.
            _emit $w "$line\n" {md_codeblock}
            continue
        }

        # ── table buffering (real aligned columns, not a code block) ──
        if {$in_table} {
            if {[regexp {^\s*\|} $line]} {
                lappend table_buf $line
                continue
            }
            _flush_table $w $table_buf
            set table_buf {}
            set in_table 0
        }
        if {[regexp {^\s*\|.*\|\s*$} $line]} {
            set in_table 1
            lappend table_buf $line
            continue
        }

        # ── fence open (CM 4.5: up to 3 leading spaces, a run of 3 or
        # more backticks, then an info string; a run of backticks in
        # the PROSE position — column > 3 — is not a fence) ──
        if {[regexp {^ {0,3}(`{3,})(.*)$} $line -> ticks lang]} {
            set fence 1
            set fence_len [string length $ticks]
            set fence_lang [string trim $lang]
            if {$fence_lang ne ""} {
                _emit $w "· $fence_lang\n" {md_fencelang}
            }
            continue
        }

        # ── headings ──
        if {[regexp {^(#{1,3})\s+(.+)$} $line -> hashes rest]} {
            set lvl [string length $hashes]
            _inline $w $rest md_h$lvl
            _emit $w "\n" md_h$lvl
            continue
        }

        # ── blockquote (also renders reasoning blocks) ──
        if {[regexp {^>\s?(.*)$} $line -> rest]} {
            _inline $w $rest md_quote
            _emit $w "\n" md_quote
            continue
        }

        # ── horizontal rule ──
        if {[regexp {^\s*(-{3,}|\*{3,}|_{3,})\s*$} $line]} {
            _emit $w "────────────────────────────\n" md_hr
            continue
        }

        # ── task list items (checkboxes) ──
        #
        # MUST come before the bullet branch: `^(\s*)[-*+]\s+(.+)$`
        # matches `- [ ] todo` as an ordinary bullet whose content is
        # "[ ] todo", so with the bullet test first this branch could
        # never run and checkboxes rendered as literal brackets.
        if {[regexp {^(\s*)-\s+\[([ xX])\]\s+(.+)$} $line -> indent mark content]} {
            set glyph [expr {$mark eq " " ? "☐" : "☑"}]
            _emit $w "[string repeat {    } [expr {[string length $indent] / 2}]]$glyph " md_bullet
            _inline $w $content md_list
            _emit $w "\n" md_list
            continue
        }

        # ── bullet list ──
        if {[regexp {^(\s*)[-*+]\s+(.+)$} $line -> indent content]} {
            set depth [expr {[string length $indent] / 2}]
            _emit $w "[string repeat {    } $depth]• " md_bullet
            _inline $w $content md_list
            _emit $w "\n" md_list
            continue
        }

        # ── numbered list ──
        if {[regexp {^(\s*)(\d+)\.\s+(.+)$} $line -> indent num content]} {
            set depth [expr {[string length $indent] / 3}]
            _emit $w "[string repeat {    } $depth]$num. " md_bullet
            _inline $w $content md_list
            _emit $w "\n" md_list
            continue
        }

        # ── blank line ──
        if {[string trim $line] eq ""} {
            _emit $w "\n" {}
            continue
        }

        # ── ordinary paragraph ──
        # Chunk very long lines (compaction summaries, pasted walls of
        # text) so `-wrap word` stays cheap: a single 3-5k-char line
        # makes Tk recompute the wrap on every scroll/redraw — the
        # classic slow-scroll cause. Chunks break at the last space so
        # words aren't split.
        if {[string length $line] > 300} {
            set start 0
            set n [string length $line]
            while {$start < $n} {
                set end [expr {$start + 300}]
                if {$end >= $n} {
                    set end $n
                } else {
                    set sp [string last " " $line $end]
                    if {$sp > $start} { set end $sp }
                }
                _inline $w [string range $line $start [expr {$end - 1}]] {}
                set start $end
            }
        } else {
            _inline $w $line {}
        }
        _emit $w "\n" {}
    }

    # unclosed fence at EOF
    if {$fence} {
        _emit $w "\n" {}
    }
    if {$in_table} {
        _flush_table $w $table_buf
    }
}

# GFM tables
#
# USER: "the markdown seems to have regressed and is presented as a
# quoted block". It had not regressed: tables had never been rendered.
# Every row went into `md_codeblock` verbatim -- tinted background, 12px
# indent, monospace, and NO inline pass -- so `**bold**`, `[a](b)`,
# angle autolinks and `<br>` all showed as raw punctuation. A set of
# search results, the single most common table a chat agent produces,
# came out as an unreadable grey slab.
#
# Every table keeps its columns. Wide cells and explicit <br> breaks
# wrap inside their own column, so the next field stays beside them.
# Tk's tab stops align cells in proportional fonts without padding
# spaces; every cell goes through _inline for emphasis and links.

# Split a GFM row without treating an escaped pipe as a delimiter.
# Outer pipes are syntax, but empty cells between pipes are data.
proc ::vmd::_table_cells {row} {
    set row [string trim $row]
    if {[string index $row 0] eq "|"} { set row [string range $row 1 end] }
    if {[string index $row end] eq "|" && [string index $row end-1] ne "\\"} {
        set row [string range $row 0 end-1]
    }
    set cells {}
    set cur ""
    set n [string length $row]
    for {set i 0} {$i < $n} {incr i} {
        set ch [string index $row $i]
        if {$ch eq "\\" && $i + 1 < $n} {
            set next [string index $row [expr {$i + 1}]]
            if {$next eq "|" || $next eq "\\"} {
                append cur $next
                incr i
                continue
            }
        }
        if {$ch eq "|"} {
            lappend cells [string trim $cur]
            set cur ""
        } else {
            append cur $ch
        }
    }
    lappend cells [string trim $cur]
    return $cells
}

proc ::vmd::_is_table_sep {row} {
    set cells [_table_cells $row]
    if {![llength $cells]} { return 0 }
    foreach c $cells {
        if {![regexp {^:?-+:?$} $c]} { return 0 }
    }
    return 1
}

proc ::vmd::_table_aligns {row} {
    set out {}
    foreach c [_table_cells $row] {
        set l [expr {[string index $c 0] eq ":"}]
        set r [expr {[string index $c end] eq ":"}]
        if {$l && $r} { lappend out center } elseif {$r} { lappend out right } else { lappend out left }
    }
    return $out
}

proc ::vmd::_cell_lines {cell} {
    set mark "\u0001"
    set cell [regsub -all -nocase {<br\s*/?>} $cell $mark]
    set out {}
    foreach l [split $cell $mark] { lappend out [string trim $l] }
    return $out
}

# Text for measurement, not for display. Links and emphasis occupy the
# width of their labels, not of their markdown delimiters.
proc ::vmd::_plain_text {s} {
    regsub -all {!?\[([^\]\n]*)\]\([^)\n]*\)} $s {\1} s
    regsub -all {<(https?://[^>\s]+)>} $s {\1} s
    regsub -all {\*\*} $s "" s
    regsub -all "`" $s "" s
    regsub -all {\*} $s "" s
    return $s
}

# Words stay whole where possible. A link with spaces in its label, or a
# code span, remains one token so wrapping never breaks its syntax.
proc ::vmd::_table_words {text} {
    set words {}
    set cur ""
    set square 0
    set paren 0
    set code 0
    set escaped 0
    foreach ch [split $text ""] {
        if {$escaped} { append cur $ch ; set escaped 0 ; continue }
        if {$ch eq "\\"} { append cur $ch ; set escaped 1 ; continue }
        if {$ch eq "`"} { set code [expr {!$code}] }
        if {!$code} {
            if {$ch eq {[}} { incr square }
            if {$ch eq {]} && $square} { incr square -1 }
            if {$ch eq {(} && !$square} { incr paren }
            if {$ch eq {)} && $paren} { incr paren -1 }
        }
        if {$ch eq " " && !$square && !$paren && !$code} {
            if {$cur ne ""} { lappend words $cur ; set cur "" }
        } else {
            append cur $ch
        }
    }
    if {$cur ne ""} { lappend words $cur }
    return $words
}

proc ::vmd::_wrap_table_line {text px} {
    if {$text eq ""} { return [list ""] }
    set out {}
    set cur ""
    foreach word [_table_words $text] {
        set candidate [expr {$cur eq "" ? $word : "$cur $word"}]
        if {$cur ne "" && [font measure md_bullet [_plain_text $candidate]] > $px} {
            lappend out $cur
            set cur $word
        } else {
            set cur $candidate
        }
        # A plain long word is split on characters. Inline constructs are
        # kept intact rather than emitting broken markdown.
        while {[font measure md_bullet [_plain_text $cur]] > $px
               && ![regexp {[\[`]} $cur]} {
            set cut [string length $cur]
            while {$cut > 1 && [font measure md_bullet [_plain_text [string range $cur 0 $cut-1]]] > $px} { incr cut -1 }
            lappend out [string range $cur 0 $cut-1]
            set cur [string range $cur $cut end]
        }
    }
    if {$cur ne ""} { lappend out $cur }
    if {![llength $out]} { return [list ""] }
    return $out
}

proc ::vmd::_table_chunks {cell px} {
    set out {}
    foreach line [_cell_lines $cell] {
        foreach part [_wrap_table_line $line $px] { lappend out $part }
    }
    if {![llength $out]} { return [list ""] }
    return $out
}

proc ::vmd::_measure_table_cells {rows ncol} {
    set out {}
    for {set c 0} {$c < $ncol} {incr c} {
        set mx 0
        foreach r $rows {
            foreach line [_cell_lines [lindex $r $c]] {
                # Measure the font the body will use. Bold and ordinary
                # glyphs have different advance widths in a proportional
                # font; using one font for both shifts wrapped cells.
                foreach font {md_bullet md_bold} {
                    set px [font measure $font [_plain_text $line]]
                    if {$px > $mx} { set mx $px }
                }
            }
        }
        lappend out $mx
    }
    return $out
}

# Natural widths, shrunk fairly to the available pixel width. Narrow
# columns do not force the prose columns to become one-character strips.
proc ::vmd::_table_widths {rows avail ncol} {
    set widths [_measure_table_cells $rows $ncol]
    set natural_chars {}
    for {set c 0} {$c < $ncol} {incr c} {
        set mc 0
        foreach r $rows {
            foreach line [_cell_lines [lindex $r $c]] {
                set mc [expr {max($mc, [string length [_plain_text $line]])}]
            }
        }
        lappend natural_chars $mc
    }
    set minw [font measure md_bullet "n"]
    set total 0
    foreach width $widths { incr total $width }
    while {$total > $avail} {
        set widest 0
        for {set c 1} {$c < $ncol} {incr c} {
            if {[lindex $widths $c] > [lindex $widths $widest]} { set widest $c }
        }
        set current [lindex $widths $widest]
        if {$current <= $minw} break
        set next $minw
        for {set c 0} {$c < $ncol} {incr c} {
            if {$c != $widest && [lindex $widths $c] > $next} { set next [lindex $widths $c] }
        }
        set shrink [expr {min($current - $next, $total - $avail)}]
        if {$shrink < 1} { set shrink 1 }
        lset widths $widest [expr {$current - $shrink}]
        set total [expr {$total - $shrink}]
    }
    return [list $widths $natural_chars]
}

proc ::vmd::_flush_table {w buf} {
    set header {}
    set aligns {}
    set rows {}
    set seen_sep 0
    foreach row $buf {
        if {[_is_table_sep $row]} {
            if {!$seen_sep && [llength $rows] == 1} {
                set header [lindex $rows 0]
                set rows {}
                set aligns [_table_aligns $row]
            }
            set seen_sep 1
            continue
        }
        lappend rows [_table_cells $row]
    }
    if {![llength $header] && ![llength $rows]} { return }
    set all $rows
    if {[llength $header]} { set all [linsert $rows 0 $header] }
    _render_table_grid $w $header $rows $aligns $all
    _emit $w "\n" {}
}

# Every table remains a table. There is no stacked-record fallback:
# physical lines within a logical row share the same tab stops, so a long
# or <br>-containing cell wraps without pushing the next column away.
proc ::vmd::_render_table_grid {w header rows aligns all} {
    variable table_seq
    if {![info exists table_seq]} { set table_seq 0 }
    incr table_seq
    set ttag md_table$table_seq
    set ncol 0
    foreach r $all { if {[llength $r] > $ncol} { set ncol [llength $r] } }
    if {!$ncol} { return }
    set avail [winfo width $w]
    if {$avail <= 1} { set avail [expr {[$w cget -width] * [font measure md_bullet "n"]}] }
    set gap [font measure md_bullet "  "]
    set avail [expr {max($ncol * [font measure md_bullet "n"], $avail - 24 - $gap * ($ncol - 1))}]
    lassign [_table_widths $all $avail $ncol] widths chars

    set tabs {}
    set x 0
    set rulen 0
    for {set c 0} {$c < $ncol} {incr c} {
        incr rulen [lindex $chars $c]
        if {$c < $ncol - 1} { incr rulen 3 }
        # The first cell starts at the line's origin. A tab stop at zero
        # is invalid in Tk and no tab precedes that cell anyway.
        if {$c == 0} {
            set x [expr {[lindex $widths $c] + $gap}]
            continue
        }
        set just [lindex $aligns $c]
        if {$just eq "right"} {
            set stop [expr {$x + [lindex $widths $c]}]
        } elseif {$just eq "center"} {
            set stop [expr {$x + [lindex $widths $c] / 2}]
        } else {
            set just left
            set stop $x
        }
        lappend tabs $stop $just
        set x [expr {$x + [lindex $widths $c] + $gap}]
    }
    $w tag configure $ttag -tabs $tabs -lmargin1 10 -lmargin2 10 -spacing1 1 -spacing3 1
    $w tag configure ${ttag}_head -foreground $::vt::T(accent)

    if {[llength $header]} {
        set start [$w index "end-1c"]
        _emit_row $w $header md_bold $widths $aligns
        $w tag add $ttag $start "end-1c"
        $w tag add ${ttag}_head $start "end-1c"
        set n [expr {max(4, min(200, $rulen))}]
        _emit $w "[string repeat "─" $n]\n" md_hr
    }
    foreach r $rows {
        set start [$w index "end-1c"]
        _emit_row $w $r {} $widths $aligns
        $w tag add $ttag $start "end-1c"
    }
}

proc ::vmd::_emit_row {w cells basetag widths aligns} {
    set ncol [llength $widths]
    set chunks {}
    set height 1
    for {set c 0} {$c < $ncol} {incr c} {
        set parts [_table_chunks [lindex $cells $c] [lindex $widths $c]]
        lappend chunks $parts
        set height [expr {max($height, [llength $parts])}]
    }
    for {set line 0} {$line < $height} {incr line} {
        for {set c 0} {$c < $ncol} {incr c} {
            if {$c} { _emit $w "\t" {} }
            set parts [lindex $chunks $c]
            set piece [expr {$line < [llength $parts] ? [lindex $parts $line] : ""}]
            _inline $w $piece $basetag
        }
        _emit $w "\n" {}
    }
}
proc ::vmd::plain {w text} {
    _tags $w
    _emit $w "$text\n" {}
}

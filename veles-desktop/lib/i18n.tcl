# veles-desktop/lib/i18n.tcl — msgcat-backed UI strings (l10n phase 2).
#
# Mirrors the Rust side's locale resolution (config tui.locale >
# VELES_LOCALE > LANG/LC_ALL > en) so both surfaces switch together,
# and mirrors its NORMALIZATION too: catalog ids are canonical
# lowercase (`ru`, `pl`, `zh_hans`, `sr_cyrl`), region codes fold onto
# the language (`de_DE` -> `de`) and the two script-split languages
# map region -> script (`zh_TW` -> zh_hant, `sr_RS` -> sr_cyrl).
# The live bug that rule exists for: rust-i18n used to answer English
# for `de_DE` while msgcat answered German — one stored value, two
# languages. Both sides now normalize the same way.
#
# Catalogs: veles-desktop/msgs/<id>.msg, msgcat format. en.msg is the
# GENERATED identity catalog of every key used in the tree
# (scripts/i18n-sync.sh), so a key with no entry in the active locale
# falls back to the English text rather than to a bare identifier —
# msgcat's `mc` returns the key when the lookup misses, and the keys
# ARE the English strings.
#
# Usage in widgets:  ::vi18n::mc {Hello}
# Widgets whose -text is a STORED string:
#   ::vi18n::label $w {Hello}                    — created once
#   ::vi18n::label $w {failed: %s} $err          — RUNTIME writes with args
# Both register the widget so ::vi18n::relabel re-texts it live when the
# language is swapped (a Tk widget's -text is a stored string; only
# widgets that ask again get the new locale for free).
#
# WHICH SHAPE TO USE, and it is not a style preference:
#   * `-text [::vi18n::mc {Hello}]` — fine. The rendered string IS the
#     catalog value, so the swap-time reverse sweep recognises it.
#   * `-text [::vi18n::mc {failed: %s} $e]` — USE ::vi18n::label INSTEAD.
#     The rendered string ("failed: timeout") appears in no catalog, so
#     the sweep cannot map it back and the label would stay in the old
#     language for the rest of the session. Same for [format …] around a
#     no-arg mc. i18ntest fails on a new one of these.
#
# Swap: ::vi18n::use <locale> sets the active locale for the running
# app (msgcat lazy-loads the new catalog on mclocale, no restart);
# ::vi18n::swap toggles primary <-> secondary.
#
# NOTE for sources: Tcl counts braces inside comments too. Catalog keys
# are brace-balanced by construction (the extractor refuses others), so
# this file spells out every literal brace in prose.

namespace eval ::vi18n {
    # Captured at SOURCE time (info script == this file); inside procs
    # info script would return the CALLER's path instead.
    variable lib_dir [file dirname [info script]]
    variable dir ""          ;# catalog dir, resolved by init
    variable active ""       ;# canonical id in force
    variable primary ""      ;# canonical id of the configured/boot default
    variable secondary ""    ;# canonical id the swap toggles to ("" = none)
    # Widget -> {key args...} for widgets whose -text is a stored string
    # that must follow a live language swap.
    variable wkeys
    array set wkeys {}
    # Widget -> the exact text ::vi18n::label last wrote there. relabel()
    # re-renders only while the widget still holds it (see relabel).
    variable wrender
    array set wrender {}
    # Same pair for ACCESSIBLE NAMES that carry format args: w -> {key
    # args...} and w -> the text ::vi18n::aname last wrote.
    variable akeys
    array set akeys {}
    variable arender
    array set arender {}
}

# Canonical form of one locale spelling: lowercase, `-` folded to `_`,
# codeset/modifier stripped. Mirrors crates/veles-core/src/i18n.rs.
proc ::vi18n::canon {raw} {
    set s [string tolower [string trim $raw]]
    set s [regsub {[.@].*$} $s ""]
    return [string map {- _} $s]
}

# Does this build ship a catalog for $id (a canonical id)?
proc ::vi18n::have {id} {
    if {$::vi18n::dir eq ""} { return 0 }
    return [file exists [file join $::vi18n::dir $id.msg]]
}

# Map any spelling onto a canonical id we actually ship.
#
# Same rules as the Rust normalize(): exact match, then the two
# script-split languages by region, then the bare language, then the
# input itself (which msgcat answers from en.msg's fallback).
proc ::vi18n::normalize {raw} {
    set c [::vi18n::canon $raw]
    if {$c eq ""} { return en }
    # "C"/"POSIX" (and their .UTF-8 spellings) mean "no preference",
    # not a language named C. Without this the app's active id was "c"
    # on any box whose LANG is C.UTF-8 — which is this laptop's own
    # default, and not a locale any catalog can exist for.
    if {$c in {c posix}} { return en }
    if {[::vi18n::have $c]} { return $c }
    set parts [split $c _]
    set lang [lindex $parts 0]
    if {$lang eq "zh"} {
        set want zh_hans
        foreach p $parts {
            if {$p in {tw hk mo hant}} { set want zh_hant ; break }
        }
        return $want
    }
    if {$lang eq "sr"} {
        set want sr_cyrl
        foreach p $parts {
            if {$p in {latn latin}} { set want sr_latn ; break }
        }
        return $want
    }
    if {[llength $parts] > 1} { return $lang }
    return $c
}

# Every locale we ship, canonical ids, sorted (the picker's list).
proc ::vi18n::available {} {
    if {$::vi18n::dir eq ""} { return {} }
    set out {}
    foreach f [lsort [glob -nocomplain [file join $::vi18n::dir *.msg]]] {
        lappend out [file rootname [file tail $f]]
    }
    return $out
}

# Localized DISPLAY NAME of a locale id, for pickers: the endonym, so a
# speaker recognizes their own language in the list regardless of the
# language the list is currently rendered in.
proc ::vi18n::locale_name {id} {
    set names {
        en       English
        de       Deutsch
        ru       Русский
        uk       Українська
        pl       Polski
        cs       Čeština
        sk       Slovenčina
        bg       Български
        sr_cyrl  { Српски (ћирилица) }
        sr_latn  { Srpski (latinica) }
        hr       Hrvatski
        zh_hans  简体中文
        zh_hant  繁體中文
        vi       { Tiếng Việt }
        id       { Bahasa Indonesia }
        th       ไทย
        ms       { Bahasa Melayu }
        hi       हिन्दी
        mr       मराठी
        ne       नेपाली
        bn       বাংলা
        as       অসমীয়া
        te       తెలుగు
        ta       தமிழ்
        gu       ગુજરાતી
        kn       ಕನ್ನಡ
        ml       മലയാളം
        or       ଓଡ଼ିଆ
        pa       ਪੰਜਾਬੀ
        si       සිංහල
        ha       Hausa
        ig       Igbo
        yo       Yorùbá
        pcm      { Nigerian Pidgin }
        sw       Kiswahili
        am       አማርኛ
        so       Soomaali
        zu       isiZulu
        af       Afrikaans
        xh       isiXhosa
        sn       chiShona
        rw       Kinyarwanda
        wo       Wolof
        mg       Malagasy
        fil      Filipino
        tl       Tagalog
        ja       日本語
    }
    # Paired walk, not lsearch+lindex: the table holds {id name} pairs.
    # The name is trimmed because the braced entries are written as
    # `{ Bahasa Indonesia }` — the braces are Tcl's grouping punctuation
    # and are not part of the name, but they leave a leading space.
    foreach {lid lname} $names {
        if {$lid eq $id} { return [string trim $lname] }
    }
    return $id
}

# The value of `key` in config.toml's [tui] table, or "" when unset.
#
# Section-aware on purpose. The first version matched `^\s*locale\s*=`
# ANYWHERE in the file, so a `locale = "…"` in any other table (a plugin's,
# a provider's) silently became the UI language. Both spellings TOML
# allows are read: `locale = "x"` under a `[tui]` header, and a top-level
# dotted `tui.locale = "x"`.
proc ::vi18n::tui_value {data key} {
    set section ""
    foreach line [split $data \n] {
        # `[table]` and `[[array.of.tables]]` both end the previous table.
        if {[regexp {^\s*\[\[?\s*([^\]]+?)\s*\]\]?\s*(#.*)?$} $line -> sec]} {
            set section $sec
            continue
        }
        if {$section eq "tui"
            && [regexp "^\\s*${key}\\s*=\\s*\"(\[^\"\]*)\"" $line -> v]} {
            return $v
        }
        if {$section eq ""
            && [regexp "^\\s*tui\\.${key}\\s*=\\s*\"(\[^\"\]*)\"" $line -> v]} {
            return $v
        }
    }
    return ""
}

# Resolve + install the locale. Called at app boot (main.tcl).
# Returns the canonical id in force.
proc ::vi18n::init {} {
    package require msgcat

    set ::vi18n::dir [file join $::vi18n::lib_dir .. msgs]

    set loc ""
    set sec ""
    # 1. config tui.locale — read the same config.toml the gateway uses
    #    (cheap grep; the gateway is the source of truth and usually
    #    already running). "The same" is a claim about a path, so the path
    #    comes from ::vpaths and not from a remembered layout: this used to
    #    be ~/.veles/config.toml, which the 2026-09-04 XDG split moved, and
    #    a locale nobody could set is indistinguishable from no locale
    #    support.
    set cfg [::vpaths::config_file]
    if {[file exists $cfg]} {
        if {![catch {
            set fh [open $cfg r]; set data [read $fh]; close $fh
        }]} {
            set loc [::vi18n::tui_value $data locale]
            set sec [::vi18n::tui_value $data locale_secondary]
        }
    }
    # 2. VELES_LOCALE env
    if {$loc eq ""} { catch { set loc $::env(VELES_LOCALE) } }
    # 3. system LANG/LC_ALL
    if {$loc eq ""} {
        foreach v {LC_ALL LANG} {
            if {[info exists ::env($v)] && $::env($v) ne ""
                && $::env($v) ne "C" && $::env($v) ne "POSIX"} {
                set loc $::env($v)
                break
            }
        }
    }
    if {$loc eq ""} { set loc en }

    # Register the catalog folder BEFORE switching: mclocale loads the
    # new locale's file from every registered folder, which is what makes
    # a later swap need no reload call (verified live: one mcload, then
    # a switch to de answers German with no second load).
    set canonical [::vi18n::normalize $loc]
    ::msgcat::mcload $::vi18n::dir
    ::msgcat::mclocale $canonical
    set ::vi18n::active $canonical
    set ::vi18n::primary $canonical

    # Secondary: the language ::vi18n::swap toggles to. Configured
    # `tui.locale_secondary` wins. Unconfigured: "en" is the honest
    # default for a non-English primary (swap back to English) and ""
    # for an English primary — a swap with nowhere to go says so rather
    # than picking a language on the user's behalf.
    set sec_id [::vi18n::normalize $sec]
    if {$sec eq ""} {
        set sec_id [expr {$canonical eq "en" ? "" : "en"}]
    }
    if {$sec_id eq $canonical} { set sec_id "" }
    set ::vi18n::secondary $sec_id
    # Prime the reverse index for the locale now on screen, so the FIRST
    # swap has something to reverse against. Without this the sweep ran
    # with an empty map and did nothing until a second swap — the same
    # silent-no-op shape as building the map for the wrong locale.
    ::vi18n::build_revmap $canonical
    return $canonical
}

# Switch the running app's language. msgcat re-resolves on every mc
# call, so only widgets that STORED a string need re-texting —
# ::vi18n::relabel does that for the ones registered via ::vi18n::label.
# Returns the canonical id installed; refuses (by name, listing what we
# have) a locale we ship no catalog for, because that would show
# English while the picker claimed a switch.
proc ::vi18n::use {raw} {
    package require msgcat
    set wanted [::vi18n::normalize $raw]
    if {![::vi18n::have $wanted]} {
        return -code error "no catalog for '[string trim $raw]' (have: [join [::vi18n::available] {, }])"
    }
    # ORDER MATTERS, and it is the whole trick. Three rules:
    #   * the index must describe the locale being LEFT, because that is
    #     the text on screen when the sweep runs;
    #   * nothing may rebuild it between the sweep and the sweep (the
    #     first version built the NEW locale's map immediately before
    #     sweeping, which matches nothing rendered yet, so the sweep
    #     considered 0 widgets and silently did nothing — the original
    #     bug, reintroduced by line order);
    #   * afterwards it is rebuilt for the locale now on screen, so
    #     key_for() answers correctly between swaps.
    set leaving $::vi18n::active
    ::msgcat::mclocale $wanted
    if {$leaving ne "" && $leaving ne $wanted} {
        ::vi18n::build_revmap $leaving
    }
    set ::vi18n::active $wanted
    ::vi18n::relabel
    # Order against ::va11y::relabel_names (a few lines down) is NOT
    # load-bearing, and the reason is the fact this whole round rests on: a
    # name registered here carries FORMAT ARGS, so its rendered text
    # ("field: Model") is in no catalog and appears in NEITHER the old nor
    # the new index. relabel_names therefore skips it whichever order the
    # two run in. Verified by running both orders and watching the name
    # follow the swap either way.
    ::vi18n::relabel_anames
    set stats [::vi18n::resweep]
    set titles [::vi18n::resweep_titles]
    # Accessible names are STORED strings too (85 sites pass
    # [::vi18n::mc {…}] to ::va11y::name), so the widget sweep cannot see
    # them — they live in a11y's own array, not in a widget option.
    if {[llength [info commands ::va11y::relabel_names]]} {
        catch { ::va11y::relabel_names }
    }
    set ::vi18n::sweep_stats [list [lindex $stats 0] [lindex $stats 1] $titles]
    # Finally the callbacks, for text that is computed rather than
    # catalogued (the status bar's "en ⇄ de" chip). After the sweep, so a
    # handler sees the new locale fully in place.
    ::vi18n::run_locale_hooks
    # Now that every widget holds the new locale's text, the index must
    # describe the NEW locale — ready for the next swap, which will treat
    # it as the one being left.
    ::vi18n::build_revmap $wanted
    return $wanted
}

# Toggle between the configured primary and secondary languages.
# Returns the id now in force.
proc ::vi18n::swap {} {
    if {$::vi18n::secondary eq ""} {
        # Point at the GUI that fixes it, not at a config key: the first
        # version said "set tui.locale_secondary", i.e. go and edit a file
        # by hand (USER report, 2026-09-25). The status-bar chip does not
        # even get this far — with no target it opens the picker.
        #
        # Plain English on purpose: this file is outside the key extractor
        # (see the NOTE at the top), so an ::vi18n::mc here could never be
        # translated. The chip never reaches this line with no target — it
        # opens the picker — so only programmatic callers read it.
        return -code error "no swap target yet — pick one in the language picker (Set as swap target)"
    }
    # The pair flips BEFORE the switch, not after: ::vi18n::use runs the
    # locale hooks, and the status-bar chip paints "<current> ⇄
    # <secondary>" from them. Flipping afterwards left the chip reading
    # "de ⇄ de" after every swap (seen on the real desktop).
    set was $::vi18n::active
    set target $::vi18n::secondary
    set ::vi18n::secondary $was
    if {[catch { ::vi18n::use $target } now]} {
        set ::vi18n::secondary $target
        return -code error $now
    }
    return $now
}

# Make $raw the language the swap toggles to, for the running window.
# Refuses a language with no catalog, and the language already in force
# (a swap target equal to the active language is a toggle that does
# nothing). Returns the canonical id. Persisting it is the caller's
# explicit act (langpick's Set as swap target writes tui.locale_secondary).
proc ::vi18n::set_secondary {raw} {
    set id [::vi18n::normalize $raw]
    # Plain English (this file is outside the key extractor); the picker
    # checks the in-use case itself and says it translated.
    if {![::vi18n::have $id]} {
        return -code error "no catalog for '[string trim $raw]'"
    }
    if {$id eq [::vi18n::current]} {
        return -code error "$id is the language in use — pick another one to swap with"
    }
    set ::vi18n::secondary $id
    ::vi18n::run_locale_hooks
    return $id
}

# The id in force.
proc ::vi18n::current {} {
    if {$::vi18n::active eq ""} { return en }
    return $::vi18n::active
}

# Translate with the package's locale. Falls back to the literal string
# when no catalog entry exists (loud-missing policy: for an
# English-as-key catalog the fallback IS the correct English text).
#
# Optional args format %s / %1$s placeholders through [format], so a
# caller can write one call instead of format-wrapping:
#   ::vi18n::mc {copied message (%s chars)} $n
proc ::vi18n::mc {s args} {
    if {[catch {set r [::msgcat::mc $s]} ]} { set r $s }
    if {$r eq ""} { set r $s }
    if {[llength $args]} {
        if {[catch {set r [format $r {*}$args]} ]} {
            # A translation whose placeholders do not match the call is
            # a translator bug, not a reason to lose the message: fall
            # back to the English source formatted the same way.
            catch {set r [format $s {*}$args]}
        }
    }
    return $r
}

# Register a widget whose -text is a stored string, and set that text
# from the catalog now. relabel() re-applies after a swap.
#
# The key is the English source string (the catalog convention); extra
# args are format arguments, kept so relabel can re-format.
#
# USE THIS FOR ANY -text THAT GOES THROUGH [format] OR CARRIES ARGS.
# A widget given `-text [::vi18n::mc {failed: %s} $e]` holds a string no
# catalog contains ("failed: timeout"), so the swap-time reverse sweep
# cannot recognise it and the label stays in the old language for the
# rest of the session. Registering it is what makes it follow — that is
# the whole reason this proc takes format args.
#
# SEE ALSO the wrender note below: this records the text it WROTE, so a
# widget that is later overwritten with runtime state is left alone by a
# swap rather than resurrected into a stale message.
proc ::vi18n::label {w key args} {
    if {![winfo exists $w]} { return 0 }
    # A widget with no -text option (a text widget, a treeview, a canvas)
    # cannot be re-texted by relabel(), so registering it would promise
    # something the swap cannot deliver — and worse, would make resweep
    # skip it as "owned" while nothing ever updates it. Refuse instead.
    # Treeview ITEM text (`$t insert ... -text`) has no registration
    # channel at all and is a documented limitation; see docs/i18n-runtime.md.
    if {[catch { $w cget -text }]} { return 0 }
    set text [::vi18n::mc $key {*}$args]
    set ::vi18n::wkeys($w) [list $key {*}$args]
    set ::vi18n::wrender($w) $text
    catch { $w configure -text $text }
    return 1
}

# The {key args...} a registered widget renders, or "" when it is not
# registered (used by tests and by relabel).
proc ::vi18n::widget_key {w} {
    if {![info exists ::vi18n::wkeys($w)]} { return "" }
    return $::vi18n::wkeys($w)
}

# Register a callback to run AFTER a locale change completes.
#
# For text the reverse sweep cannot fix because it is not catalog text:
# the status bar's language chip renders "en ⇄ de" from the active locale
# plus the configured secondary, which no value->key map can recognise.
# Callbacks run after the sweep, so a handler sees the new locale fully in
# place and can compute whatever it needs.
#
# A proc NAME (or command prefix), not a script body, so a handler can be
# re-registered on every widget rebuild without accumulating duplicate
# closures — registering the same name twice is a no-op.
proc ::vi18n::on_locale_change {cmd} {
    variable locale_hooks
    if {![info exists locale_hooks]} { set locale_hooks {} }
    if {[lsearch -exact $locale_hooks $cmd] < 0} {
        lappend locale_hooks $cmd
    }
}

# Drop a previously registered callback.
proc ::vi18n::off_locale_change {cmd} {
    variable locale_hooks
    if {![info exists locale_hooks]} { return }
    set i [lsearch -exact $locale_hooks $cmd]
    if {$i >= 0} { set locale_hooks [lreplace $locale_hooks $i $i] }
}

# Run every registered callback. One that throws must not abort the others
# or the swap itself: a stale chip is a blemish, a half-switched app is a
# bug.
proc ::vi18n::run_locale_hooks {} {
    variable locale_hooks
    if {![info exists locale_hooks]} { return }
    foreach cmd $locale_hooks {
        if {[llength $cmd]} { catch { {*}$cmd } }
    }
}

# An accessible name whose text carries format args, registered so it
# follows a live swap. THE NAME ANALOGUE OF ::vi18n::label, and needed
# for the same reason: `::va11y::name $w [::vi18n::mc {field: %s} $l]`
# stores "field: Model", a string in no catalog, so relabel_names()
# cannot reverse-map it and the tooltip stays in the old language.
#
# Call AFTER the widget exists. Not a drop-in for va11y::name on a
# widget that is not created yet.
proc ::vi18n::aname {w key args} {
    if {![winfo exists $w]} { return 0 }
    set text [::vi18n::mc $key {*}$args]
    set ::vi18n::akeys($w) [list $key {*}$args]
    set ::vi18n::arender($w) $text
    if {[llength [info commands ::va11y::name]]} {
        catch { ::va11y::name $w $text }
    }
    return 1
}

# Re-render every registered accessible name. Mirrors relabel(): a name
# the app has since overwritten is dropped rather than resurrected.
proc ::vi18n::relabel_anames {} {
    variable akeys
    variable arender
    if {![llength [info commands ::va11y::describe]]} { return 0 }
    set changed 0
    foreach w [array names akeys] {
        if {![winfo exists $w]} {
            unset -nocomplain akeys($w) arender($w)
            continue
        }
        set cur ""
        catch { set cur [::va11y::describe $w] }
        if {![info exists arender($w)] || $cur ne $arender($w)} {
            unset -nocomplain akeys($w) arender($w)
            continue
        }
        set spec $akeys($w)
        set text [::vi18n::mc [lindex $spec 0] {*}[lrange $spec 1 end]]
        set arender($w) $text
        catch { ::va11y::name $w $text }
        incr changed
    }
    return $changed
}

# Re-apply every registered widget's text in the (new) active locale.
# Widgets that died since registration are dropped as we go.
#
# A REGISTERED WIDGET MAY HAVE BEEN OVERWRITTEN. That is the normal life
# of a status label: registered once with `refused: %s`, then a later
# turn writes plain runtime state into the same label. Re-rendering
# unconditionally would resurrect a stale translated message over what
# the user is actually reading — the same failure the runtime-text guard
# in resweep exists to prevent, arriving by a different road. So a
# widget is re-texted only while it still holds the text label() wrote;
# once something else has claimed it, the registration is dropped.
proc ::vi18n::relabel {} {
    variable wkeys
    variable wrender
    foreach w [array names wkeys] {
        if {![winfo exists $w]} {
            unset -nocomplain wkeys($w) wrender($w)
            continue
        }
        set spec $wkeys($w)
        # Claimed by someone else? Then it is no longer ours to re-text.
        set cur ""
        catch { set cur [$w cget -text] }
        if {![info exists wrender($w)] || $cur ne $wrender($w)} {
            unset -nocomplain wkeys($w) wrender($w)
            continue
        }
        set text [::vi18n::mc [lindex $spec 0] {*}[lrange $spec 1 end]]
        set wrender($w) $text
        catch { $w configure -text $text }
    }
}

# ── LIVE SWAP: re-text every widget, not only the registered ones ────
#
# USER REPORT (2026-09-21): "the actual language-switching only seems to
# work within the language-switch window". True, and the shape of the tree
# is why: of the sites that put catalog text into a widget, ZERO used
# ::vi18n::label — the registering call — while 255 did
# `-text [::vi18n::mc {Key}]`, which resolves ONCE at creation, and 16
# wrapped it in [format ...]. So a swap moved msgcat's answers and left
# every existing widget holding the text it captured at build time. The
# persistent chrome (status bar, sidebar, panes, findbar) stayed in the
# old language until restart.
#
# WHY NOT REWRITE THE 255 CALL SITES: a Tcl widget option is a command
# argument, so ::vi18n::mc cannot learn which widget it is evaluating
# -text for — registering from inside the expression is impossible. A
# rewrite would also have to solve the 16 format-wrapped sites and any
# text the user can change at runtime, which `::vi18n::label` cannot
# express either (it re-renders from a fixed key+args list).
#
# So the sweep runs the other way: walk the widget tree at swap time and
# REVERSE-MAP each widget's current text back to a catalog key, then
# re-render in the new locale. Text that is not catalog text (a session
# name, a count, something typed) is simply absent from the index and is
# left alone — which is what makes a whole-tree sweep safe to run.
#
# The index is built from the locale being LEFT, because that is what is
# on screen, and from msgcat's own loaded catalog (::msgcat::Msgs) so
# there is no second source of truth to drift from the .msg files.
namespace eval ::vi18n {
    variable revmap          ;# value -> key, for the locale on screen
    # `array unset` for the same reason as in build_revmap: `array set x {}`
    # does not clear a populated array.
    array unset revmap
    array set revmap {}
    variable revmap_locale "" ;# which locale revmap describes
    variable sweep_stats {0 0 0} ;# {changed considered titles}
}

# (Re)build value -> key for $loc. Returns the number of entries.
#
# Duplicate VALUES collapse to one key: two keys with identical text are
# indistinguishable from a rendered widget. The first wins; the other
# re-renders to the same string, so nothing changes on screen.
#
# A locale's entries only appear in ::msgcat::Msgs once that locale has
# been ASKED for something — mcload reads no files, it just records the
# directory. Building the index for a locale nobody has looked up yet
# therefore returned 0 entries, which made the whole sweep a no-op (the
# first version did exactly that, silently). So this forces the load by
# switching to the locale and resolving one key before reading. Once
# loaded, a locale stays in Msgs even after the current locale changes,
# so the index for the locale being LEFT remains readable.
proc ::vi18n::build_revmap {loc} {
    variable revmap
    variable revmap_locale
    # `array unset`, NOT `array set revmap {}` — the latter is a NO-OP on
    # a populated array (verified: 2 entries in, 2 entries out), so the
    # index ACCUMULATED across calls and mixed locales. Building it for
    # de after en produced 1905 entries from a 976-entry catalog, with
    # stale English values still mapping to keys — a wrong translation
    # waiting to happen, not merely a wrong count.
    array unset revmap
    array set revmap {}
    set revmap_locale $loc
    if {![info exists ::msgcat::Msgs]} { return 0 }

    # Force the locale into Msgs (see above). mclocale is cheap and the
    # caller restores its own choice immediately afterwards.
    set prev [::msgcat::mclocale]
    catch { ::msgcat::mclocale $loc }
    catch { ::msgcat::mc {} }
    catch { ::msgcat::mclocale $prev }

    set M [set ::msgcat::Msgs]
    # Find the namespace bucket that carries THIS locale, and take the
    # one with the most entries when several match.
    #
    # Two wrong versions came before this, both silently returning an
    # index too small to be useful:
    #   * a literal "::" lookup — msgcat files entries under the CALLING
    #     namespace, and the catalogs are sourced from ::vi18n, so "::"
    #     is not where they live;
    #   * `foreach ns [dict keys $M]` taking the FIRST bucket whose
    #     `dict get $bucket $loc` does not throw — ::tk matched first and
    #     contributed 89 entries instead of 976. The catalogs under
    #     ::vi18n were never read.
    # "Largest" is the honest selector: ::tk holds Tk's own handful of
    # strings, ours hold the catalog.
    set best ""
    set bestn -1
    foreach ns [dict keys $M] {
        set bucket [dict get $M $ns]
        if {[catch { set d [dict get $bucket $loc] }] || $d eq ""} { continue }
        set n [dict size $d]
        if {$n > $bestn} { set best $d ; set bestn $n }
    }
    if {$best eq ""} { return 0 }
    dict for {k v} $best {
        if {$v eq ""} { continue }
        if {![info exists revmap($v)]} { set revmap($v) $k }
    }
    return [array size revmap]
}

# Is this widget's -text RUNTIME STATE rather than a label?
#
# A small, explicit list, because the distinction cannot be inferred: a
# status line looks exactly like a label to a walker. The cost of getting
# it wrong is asymmetric — re-texting a label is the point, re-texting a
# live status message destroys information the user was reading.
proc ::vi18n::is_runtime_text_widget {w} {
    switch -- $w {
        .statusbar.msg { return 1 }
    }
    return 0
}

# The key whose text is $text in the locale on screen, or "" when the
# text is not catalog text.
proc ::vi18n::key_for {text} {
    variable revmap
    if {[info exists revmap($text)]} { return $revmap($text) }
    return ""
}

# Re-text every widget whose current text maps back to a key. Returns
# {changed considered}.
#
# Only widgets WITH the option are considered: `cget -text` throws on a
# frame, a text widget or a canvas, and the catch is what makes this one
# walk work across every widget class without a type switch.
proc ::vi18n::resweep {{w .} {acc ""}} {
    variable revmap
    if {$acc eq ""} { set acc [list 0 0] }
    # Widgets whose -text is RUNTIME STATE, not a label, must be skipped.
    # `.statusbar.msg` carries whatever the last action said ("gateway up
    # 3s", "redrawn", a copied-message count). Those strings are not
    # labels — they change every few seconds and many are built by
    # interpolation — and a few of them DO collide with catalog keys (the
    # string `redrawn` is an entry), so a sweep in the middle of a swap
    # would replace a live status message with a translation of it. The
    # list is a contract with the widgets that own runtime text; extend it
    # when another appears.
    if {[::vi18n::is_runtime_text_widget $w]} {
        # Still recurse: the container's children are ordinary widgets.
        foreach c [winfo children $w] { set acc [::vi18n::resweep $c $acc] }
        return $acc
    }
    # A REGISTERED widget belongs to relabel(), which ran just before this
    # walk and has already re-rendered it from its key. Sweeping it here
    # would re-map its NEW-locale text through the index of the locale we
    # just left — usually a no-op, but a locale pair that shares a word
    # ("Start") maps back to the other key and the widget ends up running
    # two owners. One owner per widget: registered here means skipped
    # there, and relabel's bookkeeping stays the single source of truth.
    if {[info exists ::vi18n::wkeys($w)]} {
        foreach c [winfo children $w] { set acc [::vi18n::resweep $c $acc] }
        return $acc
    }
    # The counters must be written BACK into $acc before recursing, and
    # the recursion must be threaded through the SAME accumulator. The
    # first version did `incr changed` on a local and then rebuilt each
    # child's acc from the un-updated list argument, so every increment
    # was discarded and the sweep reported "0 changed, 0 considered"
    # while visibly re-texting the tree. The count is what makes this
    # verifiable, so it has to be right.
    set changed [lindex $acc 0]
    set considered [lindex $acc 1]

    if {[winfo exists $w]} {
        # `-text` is the only option the tree actually populates from the
        # catalog (255 sites): buttons, labels, checkbuttons, radiobuttons
        # and ttk::labelframe all spell their caption that way. A `-label`
        # pass existed here briefly and was removed — no call site uses it,
        # so it was a branch that could rot unnoticed.
        if {![catch { set cur [$w cget -text] }] && $cur ne ""} {
            incr considered
            if {[info exists revmap($cur)]} {
                set want [::vi18n::mc $revmap($cur)]
                if {$want ne $cur && ![catch { $w configure -text $want }]} {
                    incr changed
                }
            }
        }
    }
    set acc [list $changed $considered]
    foreach c [winfo children $w] {
        set acc [::vi18n::resweep $c $acc]
    }
    return $acc
}

# Window titles are not widgets, so they need their own pass: 13 call
# sites set a title through `wm title ... [::vi18n::mc {…}]`, and a title
# set once would otherwise stay in the old language too.
proc ::vi18n::resweep_titles {} {
    variable revmap
    set changed 0
    foreach w [winfo children .] {
        if {[winfo toplevel $w] ne $w} { continue }
        if {[catch { set cur [wm title $w] }] || $cur eq ""} { continue }
        if {![info exists revmap($cur)]} { continue }
        set want [::vi18n::mc $revmap($cur)]
        if {$want ne $cur && ![catch { wm title $w $want }]} { incr changed }
    }
    return $changed
}
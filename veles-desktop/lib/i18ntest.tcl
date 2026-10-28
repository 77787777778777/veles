# veles-desktop/lib/i18ntest.tcl — the l10n contract for the desktop.
#
# What this suite pins (each case fails without its fix):
#
#   1. LOCALE IDS. Catalog files are canonical lowercase ids; every
#      spelling a user can put in config.toml/VELES_LOCALE/LANG maps
#      onto one of them. The live bug: `de_DE` answered German in
#      msgcat while rust-i18n answered English — one stored value, two
#      languages. Also `C.UTF-8` (this laptop's own LANG) must mean
#      "no preference", not a language named "c".
#   2. THE SHIPPED SET. Every language the round promised has a
#      catalog that parses as Tcl.
#   3. NO GAPS. Every key in en.msg exists in every shipped locale —
#      the loud-missing fallback hides a gap otherwise, and a shipped-
#      but-incomplete catalog is the same silent failure one level up.
#   4. THE SWAP. ::vi18n::use switches the RUNNING app (no restart), a
#      registered widget re-texts, an unknown locale refuses by name.
#   5. NOTHING LEFT UNWRAPPED. The static scan: user-visible literals
#      in the desktop must go through ::vi18n::mc. This is the case
#      that keeps the next literal from being added. A literal that is
#      genuinely not prose (a brand glyph, a key name) says so on the
#      line with `i18n-exempt: <reason>` — an exemption is a written
#      decision, like every other rule in this tree.
#
# Run: scripts/one_tcl_test.sh veles-desktop/lib/i18ntest.tcl

set _dir [file dirname [file normalize [info script]]]
source [file join $_dir paths.tcl]
source [file join $_dir i18n.tcl]
# The picker is part of this contract: it is where the swap is reached
# from, and its rows must agree with ::vi18n::available.
source [file join $_dir langpick.tcl]
# The status bar too: the swap CHIP's handler (::vlang::swap_click) ends
# every path in ::vstatus::set_msg, and until 2026-09-21 no test called
# it — which is how an unbound `$err` in the refusal path reached a
# shipped surface. set_msg guards its widget write with catch, so this
# needs no display.
source [file join $_dir statusbar.tcl]
# And a11y: accessible names are STORED rendered strings (85 call sites),
# so they need their own re-resolve pass on a swap and this suite now
# checks that they follow one.
source [file join $_dir a11y.tcl]

# The picker (langpick.tcl) paints with theme colours; this suite does not
# load the theme, so it gets neutral stand-ins — the picker is exercised
# for behaviour, not looks.
if {![array exists ::vt::T]} {
    namespace eval ::vt {}
    foreach _k {fg fg_dim bg_panel bg_input select_bg} { set ::vt::T($_k) grey }
}

set ::T 0
set ::F 0
# Same shape as pathtest.tcl's ok: `cond` is an UNEVALUATED expression
# evaluated in the caller's scope, so a case can be written
#   ok "name" {$now eq "de"}
# and the variables live where the case does.
proc ok {name cond} {
    incr ::T
    if {[uplevel 1 [list expr $cond]]} {
        puts "  ok   $name"
    } else {
        incr ::F
        puts "  FAIL $name"
    }
}

# Deterministic environment: the suite asserts resolution, so it must
# not inherit the developer's LANG or the live config.toml. paths.tcl
# resolves the config path through this proc, so overriding it is the
# seam (the env var would be read too late — source time).
set _tdir [expr {[info exists ::env(TMPDIR)] ? $::env(TMPDIR) : "/tmp"}]
set _tmp [file join $_tdir "i18ntest-[pid]"]
file mkdir $_tmp
set _cfg [file join $_tmp config.toml]
set _fh [open $_cfg w]
puts $_fh {[tui]}
puts $_fh {locale = "en"}
close $_fh
proc ::vpaths::config_file {} {
    if {[info exists ::vi18n_test_cfg]} { return $::vi18n_test_cfg }
    return [::vpaths::config_dir]/config.toml
}
set ::vi18n_test_cfg $_cfg

puts "── locale ids (the one-stored-value-two-languages bug)"
::vi18n::init

set have [::vi18n::available]
ok "the catalogue set is present" {[llength $have] >= 17}

# The shipped set is the CONTRACT, and it is checked as one list rather
# than one assertion per locale: with ~47 locales, a per-locale shape
# buries a single missing catalog in a wall of identical lines, and the
# failure this has to communicate is "these are missing", not "one of
# these forty-seven is missing".
#
# The list grew 2026-09-21 (USER ask): Indian sub-continent, Nigerian,
# the large African languages, the Philippine pair and Japanese — minus
# ur and sd, which are Arabic-script and deliberately absent because the
# desktop's Tk 8.6 has no shaping or bidi (measured live: a ZWNJ-blocked
# conjunct renders byte-identical to the ligature), so they would arrive
# mirrored and disconnected.
set want {
    en de ru uk pl cs sk bg sr_cyrl sr_latn hr zh_hans zh_hant vi id th ms
    hi mr ne bn as te ta gu kn ml or pa si
    ha ig yo pcm sw am so zu af xh sn rw wo mg
    fil tl ja
}
set missing {}
foreach w $want {
    if {[lsearch -exact $have $w] < 0} { lappend missing $w }
}
if {[llength $missing]} {
    puts "      not shipped yet: [join $missing { }]"
}
ok "every promised language has a catalog ([llength $want] wanted, [llength $missing] missing)" {
    [llength $missing] == 0
}
# And the picker must not advertise a language that has no catalog: a
# row that switches to nothing is worse than an absent row.
set excess {}
foreach h $have {
    if {[lsearch -exact $want $h] < 0} { lappend excess $h }
}
if {[llength $excess]} { puts "      shipped but unlisted: [join $excess { }]" }
ok "no catalog is shipped that the contract does not name" {[llength $excess] == 0}

ok "de_DE folds onto de (not English)" {[::vi18n::normalize de_DE] eq "de"}
ok "de-DE folds onto de" {[::vi18n::normalize de-DE] eq "de"}
ok "ru_RU folds onto ru" {[::vi18n::normalize ru_RU] eq "ru"}
ok "zh_CN folds onto zh_hans" {[::vi18n::normalize zh_CN] eq "zh_hans"}
ok "zh_TW folds onto zh_hant" {[::vi18n::normalize zh_TW] eq "zh_hant"}
ok "zh_Hans stays zh_hans" {[::vi18n::normalize zh_Hans] eq "zh_hans"}
ok "sr_RS folds onto sr_cyrl" {[::vi18n::normalize sr_RS] eq "sr_cyrl"}
ok "sr_Latn folds onto sr_latn" {[::vi18n::normalize sr_Latn] eq "sr_latn"}
ok "C.UTF-8 means no preference, not a language named c" {
    [::vi18n::normalize C.UTF-8] eq "en"
}
ok "an unknown language still resolves (to itself; en per key)" {
    [::vi18n::normalize qq_ZZ] eq "qq"
}

puts "── the catalogues parse and carry no orphan keys"
set enfile [file join $_dir .. msgs en.msg]
set fh [open $enfile r]
set enkeys {}
foreach line [split [read $fh] \n] {
    set line [string trim $line]
    if {![string match "::msgcat::mcset en *" $line]} { continue }
    if {[catch { llength $line } n] || $n != 4} { continue }
    lappend enkeys [lindex $line 2]
}
close $fh
set enkeys [lsort -unique $enkeys]
ok "en.msg is non-trivial ([llength $enkeys] keys)" {[llength $enkeys] > 400}

foreach id $have {
    set path [file join $_dir .. msgs $id.msg]
    set probe [interp create]
    set code [catch { $probe eval [list source -encoding utf-8 $path] } err]
    interp delete $probe
    # msgcat may be absent in the probe; only a PARSE failure counts.
    set parse_ok 1
    if {$code && ([string match "*brace*" $err] || [string match "*quote*" $err])} {
        set parse_ok 0
        puts "      $id.msg does not parse: $err"
    }
    ok "$id.msg is sourceable Tcl" {$parse_ok}
}

# Completeness: every en key resolvable in every locale. A missing key
# falls back to English silently, so an incomplete catalog LOOKS fine
# until someone reads a paragraph in the wrong language.
foreach id $have {
    if {$id eq "en"} { continue }
    set fh [open [file join $_dir .. msgs $id.msg] r]
    set keys {}
    foreach line [split [read $fh] \n] {
        set line [string trim $line]
        if {![string match "::msgcat::mcset $id *" $line]} { continue }
        if {[catch { llength $line } n] || $n != 4} { continue }
        lappend keys [lindex $line 2]
    }
    close $fh
    set keys [lsort -unique $keys]
    set missing {}
    foreach k $enkeys {
        if {[lsearch -exact $keys $k] < 0} { lappend missing $k }
    }
    set orphan {}
    foreach k $keys {
        if {[lsearch -exact $enkeys $k] < 0} { lappend orphan $k }
    }
    if {[llength $missing]} {
        puts "      $id: [llength $missing] missing, e.g. [lrange $missing 0 2]"
    }
    if {[llength $orphan]} {
        puts "      $id: orphan keys: [lrange $orphan 0 2]"
    }
    ok "$id.msg is complete ([llength $keys] keys)" {[llength $missing] == 0}
    ok "$id.msg has no orphan keys" {[llength $orphan] == 0}
}

# ── what the catalogs SAY, not only that the keys exist ─────────────
#
# "Complete" above counts keys. The 2026-09-21 round passed it at 47/47
# while ~125 engine rows were translations of a key's NAME (the %{id} in
# "cancelled task %{id}" simply gone) and ~128 desktop rows had lost the
# edge space that makes " · next: %s" a suffix instead of a weld. The
# shipped-catalog check compares every row with its English source —
# en.yml's text for a symbolic key — for placeholders, edge spaces and
# brackets. One ok per locale, naming the first bad row when there is one.
puts "── every row says what its English source says (placeholders, edges, brackets)"
source [file join $_dir .. .. scripts i18n-catalog-check.tcl]
set _cc [::i18ncheck::run [file normalize [file join $_dir .. ..]] [file join $_dir .. msgs]]
dict for {_loc _info} $_cc {
    set _p [dict get $_info problems]
    if {[llength $_p]} { puts "      $_loc: [lindex $_p 0]" }
    ok "$_loc: no row breaks its source" {[llength $_p] == 0}
}
ok "the check read every shipped locale" {[dict size $_cc] == [llength [::vi18n::available]] - 1}

puts "── the swap switches the running app"
set now [::vi18n::use de]
ok "::vi18n::use installs the locale" {$now eq "de" && [::vi18n::current] eq "de"}
ok "and the message catalog follows" {[::vi18n::mc {Send}] ne {Send}}
ok "switching back restores English" {
    [::vi18n::use en] eq "en" && [::vi18n::mc {Send}] eq {Send}
}
set _code [catch {::vi18n::use xx} _err]
ok "an unshipped locale refuses BY NAME, listing what exists" {
    $_code == 1 && [string match "*no catalog for 'xx'*" $_err] \
        && [string match "*de*" $_err]
}

# ── the CHIP handler, which nothing exercised until 2026-09-21 ───────
#
# The user hit this live: clicking the status-bar chip reported
# "can't read \"err\": no such variable" instead of saying why the swap
# did not happen. swap_click caught into `now` but read `$err` — copied
# from the sibling procs, which catch into `err` — so the REFUSAL path
# (no secondary configured, the common case) raised an unbound-variable
# error and the real message never appeared. The suite above tested
# ::vi18n::swap and relabel directly and never called the handler, which
# is exactly how a one-line typo reached a shipped surface.
#
# The assertion is on the REPORTED MESSAGE, not on "it did not throw":
# an unbound var inside a Tk callback is swallowed by the event loop
# (that is why the user saw BGERROR rather than a crash), so a test
# that only checks for an exception passes on the broken code.
puts "── the status-bar chip reports refusals, not an unbound variable"
# set_msg appends to ::vstatus::history (there is no `last` variable —
# the bar holds one message and the history keeps the rest), so the
# assertion reads the newest entry.
set ::vstatus::history {}
set _sec [set ::vi18n::secondary]
set ::vi18n::secondary ""
set _rc [catch {::vlang::swap_click} _e]
set _msg [lindex [lindex [set ::vstatus::history] end] 1]
ok "swap_click with no secondary does not raise an unbound variable" {
    $_rc == 0 && ![string match "*no such variable*" $_e] \
        && ![string match "*no such variable*" $_msg]
}
set ::vi18n::secondary $_sec

# ── the swap target is SET IN THE GUI (USER report, 2026-09-25) ──────
#
# "it asks me to manually tui.secondary_language or something in the
# config file instead of this being an option in the desktop gui". The
# pin above used to assert exactly that message — the refusal named
# `locale_secondary` as the fix. Now the chip, with nothing to swap to,
# OPENS the picker and says why; the picker has "Set as swap target";
# and no path tells the user to edit config.toml.
puts "── the swap target is set in the GUI, never by editing config.toml"
# Record config writes instead of making them — at the WIRE, with the
# real encoder in front of it. The first version of this stub replaced
# post_json itself and accepted any argument, so it could not see that
# the picker handed post_json a hand-built JSON string it then re-encoded
# into a body with no `path`: every "Save as default" had answered 400.
# The recorded body is what the gateway would receive, decoded.
source [file join $_dir jsonfield.tcl]
source [file join $_dir api.tcl]
set ::_posts {}
proc ::vapi::post {path body {ms 0}} {
    lappend ::_posts [list $path [::json::json2dict $body]]
    return "{}"
}
proc _posted {i field} {
    set d [lindex $::_posts $i 1]
    if {![dict exists $d $field]} { return "<no $field>" }
    return [dict get $d $field]
}

::vi18n::use en
set ::vi18n::primary en
set ::vi18n::secondary ""
set ::vstatus::history {}
catch { destroy .lang_picker }
set _rc [catch {::vlang::swap_click} _e]
ok "the chip with no swap target OPENS the picker instead of refusing" {
    $_rc == 0 && [winfo exists .lang_picker]
}
ok "…and the picker says why it opened" {
    [string match "*Set as swap target*" [.lang_picker.state cget -text]]
}
ok "…and nothing anywhere says to edit tui.locale_secondary by hand" {
    ![string match "*locale_secondary*" [lindex [lindex $::vstatus::history end] 1]]
    && ![string match "*locale_secondary*" [.lang_picker.state cget -text]]
}
set _swap_err ""
catch { ::vi18n::swap } _swap_err
ok "::vi18n::swap's own refusal points at the picker, not at a config key" {
    ![string match "*tui.locale*" $_swap_err] && [string match "*swap target*" $_swap_err]
}

# Select Deutsch and press "Set as swap target".
proc _select {id} {
    set i 0
    foreach r [::vlang::rows] {
        if {[string match "*·  $id" [lindex $r 1]]} {
            .lang_picker.list selection clear 0 end
            .lang_picker.list selection set $i
            return
        }
        incr i
    }
}
_select de
set ::_posts {}
::vlang::_set_target .lang_picker
ok "Set as swap target makes it the live target" { $::vi18n::secondary eq "de" }
ok "…persists it as tui.locale_secondary, and only that — in a body the gateway can read" {
    [llength $::_posts] == 1
    && [lindex $::_posts 0 0] eq "/api/config/set"
    && [_posted 0 path] eq "tui.locale_secondary"
    && [_posted 0 value] eq "de"
}
ok "…and the chip now shows the pair" {
    ![winfo exists .statusbar.lang] || [.statusbar.lang cget -text] eq "en ⇄ de"
}
ok "…and the picker's state line names it" {
    [string match "*Deutsch*" [.lang_picker.state cget -text]]
}
_select en
set _rc [catch { ::vi18n::set_secondary en } _e]
ok "the language in use cannot be its own swap target" {
    $_rc == 1 && $::vi18n::secondary eq "de"
}

# "Make default" writes tui.locale and KEEPS a configured target (the first
# version overwrote it with whatever was on screen, and could set
# target == default).
set ::vi18n::secondary ru
_select pl
set ::_posts {}
::vlang::_make_default .lang_picker
ok "Make default switches the window and writes tui.locale" {
    [::vi18n::current] eq "pl" && $::vi18n::primary eq "pl"
    && [llength $::_posts] == 1
    && [_posted 0 path] eq "tui.locale" && [_posted 0 value] eq "pl"
}
ok "…and keeps the configured swap target" { $::vi18n::secondary eq "ru" }
# …but a target EQUAL to the new default hands its place to the old one.
_select ru
set ::_posts {}
::vlang::_make_default .lang_picker
ok "a target equal to the new default becomes the old default" {
    $::vi18n::primary eq "ru" && $::vi18n::secondary eq "pl"
}
# The chip after a SWAP names the language it would swap back to. The pair
# used to flip only after ::vi18n::use had run the hooks, so the chip
# painted "de ⇄ de" on the real desktop.
set _chip ""
proc ::_chip_probe {} {
    set ::_chip "[::vi18n::current] ⇄ $::vi18n::secondary"
}
# The chip registers the same kind of hook when the real bar is built.
::vi18n::on_locale_change ::_chip_probe
::vi18n::swap
ok "after a swap the chip reads current ⇄ the language it came from" {
    $::_chip eq "pl ⇄ ru" && [::vi18n::current] eq "pl" && $::vi18n::secondary eq "ru"
}
::vi18n::off_locale_change ::_chip_probe
catch { destroy .lang_picker }
::vi18n::use en
set ::vi18n::primary en
set ::vi18n::secondary $_sec

# ── the config reader reads [tui] and nothing else ──────────────────
puts "── init reads tui.locale from the \[tui\] table only"
set _cfg "\[plugins.x\]\nlocale = \"ru\"\n\n\[tui\]\ntheme = \"dark\"\nlocale = \"de\"\n\n\[\[platforms.irc.networks\]\]\nlocale_secondary = \"ja\"\n"
ok "a locale key in another table is not the UI language" {
    [::vi18n::tui_value $_cfg locale] eq "de"
}
ok "an array-of-tables header ends \[tui\] too" {
    [::vi18n::tui_value $_cfg locale_secondary] eq ""
}
ok "the top-level dotted spelling is read" {
    [::vi18n::tui_value "tui.locale = \"pl\"\n\[x\]\nlocale = \"ru\"\n" locale] eq "pl"
}

puts "── a swap re-texts widgets that never registered (the reverse sweep)"
# USER REPORT (2026-09-21): "the actual language-switching only seems to
# work within the language-switch window". The cause was that 255 sites
# put catalog text into a widget with `-text [::vi18n::mc {…}]`, which
# resolves ONCE at creation, while ZERO used ::vi18n::label — the only
# shape relabel() could re-text. So the swap moved msgcat's answers and
# left every existing widget holding its captured string.
#
# The fix reverse-maps each widget's current text back to a catalog key
# (built from msgcat's own loaded catalog) and re-renders it. These cases
# pin the three things that make it work, each of which was a separate
# bug while it was being written:
#   * the index must come from the locale being LEFT;
#   * it must be read from the right msgcat namespace;
#   * the sweep must recurse and actually count.
if {[catch {package require Tk}] == 0 || [winfo exists .]} {
    set _b [ttk::button .i18n_sweep_btn -text [::vi18n::mc {Send}]]
    set _l [ttk::label .i18n_sweep_lbl -text [::vi18n::mc {Set}]]
    frame .i18n_sweep_outer
    set _n [ttk::button .i18n_sweep_outer.deep -text [::vi18n::mc {Save}]]
    # Text that is NOT catalog text: must be left alone.
    set _u [ttk::label .i18n_sweep_user -text "session-abc123"]
    set _en_b1 [$_b cget -text]

    ::vi18n::use de
    set _de_send [::vi18n::mc {Send}]
    set _de_set  [::vi18n::mc {Set}]
    set _de_save [::vi18n::mc {Save}]

    ok "a -text button the old way now follows the swap" \
        {[$_b cget -text] eq $_de_send}
    ok "a -text label the old way now follows the swap" \
        {[$_l cget -text] eq $_de_set}
    ok "the sweep recurses into nested containers" \
        {[$_n cget -text] eq $_de_save}
    ok "non-catalog text is NOT touched" \
        {[$_u cget -text] eq {session-abc123}}
    ok "the sweep reports what it did (changed considered)" {
        [lindex $::vi18n::sweep_stats 0] >= 3 && [lindex $::vi18n::sweep_stats 1] >= 3
    }
    ::vi18n::use en
    ok "swapping back restores the English text" {[$_b cget -text] eq $_en_b1}

    destroy $_b $_l $_u
    destroy .i18n_sweep_outer
} else {
    puts "  skip (no Tk display)"
}

puts "── the index is built from the locale being LEFT, not the new one"
# The delicate half of the fix. Building the index AFTER mclocale would
# describe the new locale, match nothing rendered yet, and silently make
# the sweep a no-op — the original bug wearing a different hat. Pin it by
# asking directly: with de active, the map must resolve de's values.
#
# The COUNT is asserted against the catalog's own size rather than a
# loose floor, because a count that is wrong is how two real bugs here
# were caught: an index built from the wrong msgcat namespace (89
# entries instead of 976) and one that ACCUMULATED across calls because
# `array set x {}` does not clear an array (1905 entries from a 976-entry
# catalog, with stale values from the previous locale still mapping to
# keys — a wrong translation waiting to happen).
set _n_de [::vi18n::build_revmap de]
set _de_cat [dict size [dict get [dict get [set ::msgcat::Msgs] ::vi18n] de]]
ok "the index covers the catalog ($_n_de of $_de_cat entries, deduped by value)" {
    $_n_de > 500 && $_n_de <= $_de_cat
}
ok "…and cannot exceed the catalog it was built from" {$_n_de <= $_de_cat}
# The lookup must use a VALUE of the locale the index was built for.
# Asking for the ENGLISH text here would be asking the German index about
# a string it does not contain — "Send" is a de value only because de
# leaves some strings untranslated, which is a coincidence, not the rule.
# (The first version of this case made exactly that mistake and failed;
# the map was right and the assertion was wrong.)
set _de_val [::vi18n::key_for Senden]
ok "…and maps a German value back to its English key" {$_de_val eq {Send}}
ok "unknown text has no key" {[::vi18n::key_for "session-abc123"] eq ""}
# Rebuilding must not grow it: this is the accumulation bug directly.
set _n_de2 [::vi18n::build_revmap de]
ok "rebuilding the index does not accumulate ($_n_de -> $_n_de2)" {
    $_n_de2 == $_n_de
}

puts "── computed text follows a swap via the locale-change hook"
# The reverse sweep only handles text that IS catalog text. The status
# bar's language chip renders "en ⇄ de" from the active locale plus the
# configured secondary, which no value->key map can recognise, so it
# needs a different mechanism: a callback registered with
# ::vi18n::on_locale_change and run after the sweep. Without the hook the
# chip kept showing whatever pair was current when the bar was built —
# and the comment above refresh_lang claimed it was "called at build and
# from poll" while grep found only the build.
set ::hook_ran 0
proc ::i18ntest_hook {} { set ::hook_ran [expr {$::hook_ran + 1}] }
::vi18n::on_locale_change ::i18ntest_hook
::vi18n::use de
set _after_one $::hook_ran
ok "a registered hook runs on a locale change" {$_after_one >= 1}
# Registering the same command twice must not duplicate the call.
::vi18n::on_locale_change ::i18ntest_hook
set _before $::hook_ran
::vi18n::use en
ok "re-registering the same hook does not double it" \
    {[expr {$::hook_ran - $_before}] == 1}
::vi18n::off_locale_change ::i18ntest_hook
set _before2 $::hook_ran
::vi18n::use de
ok "an unregistered hook stops running" {[expr {$::hook_ran - $_before2}] == 0}
::vi18n::use en

puts "── a registered widget re-texts on swap"
if {[catch {package require Tk} ] == 0 || [winfo exists .]} {
    set b [ttk::button .i18ntest_btn]
    ::vi18n::label $b {Send}
    set en_text [$b cget -text]
    ::vi18n::use de
    set de_text [$b cget -text]
    # Capture what the catalog says WHILE de is active: asking after
    # switching back would compare the German text with the English mc
    # and always fail.
    set de_mc [::vi18n::mc {Send}]
    ::vi18n::use en
    ok "label() sets the English text" {$en_text eq "Send"}
    ok "relabel() follows the swap" {$de_text ne "Send" && $de_text eq $de_mc}
    destroy $b
} else {
    puts "  skip (no Tk display)"
}

puts "── nothing visible is left unwrapped"
# The static scan. A user-visible literal that skips ::vi18n::mc shows
# up here; this is the case that keeps the next one from being added.
set _bad {}
foreach f [lsort [glob -nocomplain [file join $_dir *.tcl]]] {
    if {[string match "*test.tcl" $f]} { continue }
    if {[file tail $f] eq "i18n.tcl"} { continue }
    set fh [open $f r]
    set n 0
    while {[gets $fh line] >= 0} {
        incr n
        if {[string match "*i18n-exempt*" $line]} { continue }
        # Literal -text "..." with real words and no $-interpolation:
        # a visible label that cannot translate. `--` before the
        # pattern: it starts with a literal `-`, which regexp would
        # otherwise try to read as an option ("bad option -text…").
        if {![regexp -- {-text "([^"$]*)"} $line -> lit]} { continue }
        if {![regexp {[A-Za-z]{2,}} $lit]} { continue }
        if {[string match "*vi18n*" $line]} { continue }
        set cut [string first "-text" $line]
        if {[string match "*#*" [string range $line 0 $cut]]} { continue }
        lappend _bad "[file tail $f]:$n"
    }
    close $fh
}
if {[llength $_bad]} { puts "      unwrapped: $_bad" }
ok "no user-visible -text literal bypasses the catalog" {[llength $_bad] == 0}

puts "── every visible string IS a key in the catalog"
# THE STRONGER HALF, and the one the scan above cannot see. That scan
# looks for a LITERAL at a -text; this one asks the question that
# actually decides whether a user sees English:
#
#     is the string this site renders a key in en.msg?
#
# A site can be wrapped and still be untranslated — `-text [::vi18n::mc
# $label]` passes the scan above (it calls vi18n) while `$label` is a
# table cell that no extractor rule ever collected. THAT is the bug USER
# reported on 2026-09-26, five times, in five shapes: the steer button
# (a quoted ternary), the pane names and tooltips (foreach data), the
# help text (a registry column), the M-x palette (a label list), the
# system messages (composed). Every one was "wrapped" and every one
# rendered English in all 47 locales.
#
# The measurement comes from scripts/i18n-audit.tcl — SOURCED, not
# re-implemented, so this contract and that report cannot drift about
# what "missing" means. It is the same single-source rule that makes
# uitest source scripts/i18n-keys.tcl for the generator's key set.
source [file join $_dir .. .. scripts i18n-audit.tcl]
lassign [::i18naudit::run [file normalize [file join $_dir .. ..]] \
             [::i18naudit::label_lists]] \
    _au_incat _au_exempt _au_missing _au_total
if {[llength $_au_missing]} {
    puts "      $_au_total visible strings scanned, $_au_incat in the catalog"
    foreach _m [lrange $_au_missing 0 9] {
        lassign $_m _f _ln _kind _cls _txt
        set _txt [string map {\n { } \t { }} $_txt]
        if {[string length $_txt] > 60} { set _txt "[string range $_txt 0 57]…" }
        puts "      NOT A KEY: $_f:$_ln \[$_kind] $_txt"
    }
    if {[llength $_au_missing] > 10} {
        puts "      … and [expr {[llength $_au_missing] - 10}] more (run scripts/i18n-audit.tcl --list)"
    }
}
ok "every visible string is a catalog key (${_au_incat}/$_au_total accounted)" {
    [llength $_au_missing] == 0
}

puts "── nothing stores text the reverse sweep cannot recognise"
# THE OTHER HALF OF THE SWEEP CONTRACT, and the reason ::vi18n::label
# takes format args at all.
#
# The sweep (and relabel_names for a11y strings) works by reverse-
# mapping a widget's current text back to a catalog key. That only
# works when the rendered string IS a catalog value. A call that passes
# format args renders something the catalog does not contain —
# "failed: timeout" is not a key — so the widget would keep the old
# language for the rest of the session, silently, with no failing
# assertion anywhere. `::vi18n::label $w {failed: %s} $err` records the
# key and args instead, which is why it exists.
#
# This scan is what keeps the next such site from being added. It
# checks -text assignments (creation or configure) and accessible names,
# and requires every one that carries format args to go through
# label()/aname(). A site that genuinely cannot (a treeview ITEM is not
# a widget option and has no registration channel) states its reason
# with the i18n-exempt marker.
set _blind {}
# Does this line render catalog text WITH format args, in a way the
# sweep cannot reverse-map? String operations, not a regexp: the pattern
# needs a literal `}`, `[` and `$`, and every one of those collides with
# Tcl's own parsing rules in a quoted or braced literal (braces must
# balance, brackets command-substitute) — three failed attempts' worth of
# evidence that the regex is the wrong tool here.
#
# The test: find `vi18n::mc`, find its braced key, then look at what
# follows before the closing bracket. A `$` means format args. A
# trailing `\` means the call continues on the next line, which is how
# the multi-line sites are written. A `format [` in front means the same
# thing by another spelling.
proc ::i18ntest_fa_blind {line} {
    set i [string first "vi18n::mc" $line]
    if {$i < 0} { return 0 }
    set before [string range $line 0 [expr {$i - 1}]]
    set b [string first "\{" $line $i]
    set e [string first "\}" $line $b]
    if {$b < 0 || $e < 0} { return 0 }
    # Only the text BETWEEN the key's closing brace and the bracket that
    # ends the mc call. Scanning the whole rest of the line was the first
    # version's bug: `-text [::vi18n::mc {…}] -fill $pink` has a `$` in
    # it, but that `$pink` belongs to a different option and the mc call
    # carries no args at all — 25 false positives from that.
    set rest [string range $line [expr {$e + 1}] end]
    set close [string first "\]" $rest]
    if {$close < 0} { set close [string length $rest] }
    set after [string range $rest 0 [expr {$close - 1}]]
    if {[string first "\$" $after] >= 0} { return 1 }
    # A call continued onto the next line: `… [::vi18n::mc {Key} \`.
    if {[string trim $after] eq "" && [string match "*\\" [string trimright $line]]} {
        return 1
    }
    if {[string match "*format *" $before]} { return 1 }
    return 0
}
foreach f [lsort [glob -nocomplain [file join $_dir *.tcl]]] {
    if {[string match "*test.tcl" $f]} { continue }
    if {[file tail $f] eq "i18n.tcl"} { continue }
    set all [read [set fh [open $f r]]]
    close $fh
    set n 0
    foreach line [split $all \n] {
        incr n
        if {[string match "*i18n-exempt*" $line]} { continue }
        if {[string match "#*" [string trim $line]]} { continue }
        # Is this line putting catalog text with format args into a
        # widget or an accessible name?
        set is_target 0
        if {[string match "*-text *" $line] || [string match "*va11y::name*" $line]} {
            set is_target 1
        }
        if {!$is_target} { continue }
        if {![::i18ntest_fa_blind $line]} { continue }
        # Registered shapes are the fix, not the bug.
        if {[string match "*vi18n::label*" $line] || [string match "*vi18n::aname*" $line]} { continue }
        lappend _blind "[file tail $f]:$n"
    }
}
if {[llength $_blind]} { puts "      sweep-blind: $_blind" }
ok "no -text or a11y name hides from the swap sweep (use label/aname)" \
    {[llength $_blind] == 0}

puts "── a placeholder key is always formatted"
# `"[::vi18n::mc {  ⟶ answered: %s}] $shown"` looks wrapped, passes
# every check above, and puts a literal "%s" in the transcript: mc with
# no args returns the format string untouched, and the value lands AFTER
# it. Three sites shipped that way in the 2026-09-26 l10n round (every
# answered question read "⟶ answered: %s once"). A key carrying a
# placeholder must reach format: args on the mc call, `[format [mc …]`,
# or a `[format \` line continued into it. String operations, for the
# reason the sweep-blind scan above gives.
proc ::i18ntest_unformatted {line prev} {
    set hits {}
    set from 0
    while {[set i [string first "vi18n::mc \{" $line $from]] >= 0} {
        set b [expr {$i + [string length "vi18n::mc "]}]
        set e [string first "\}" $line $b]
        if {$e < 0} { break }
        set from $e
        set key [string range $line [expr {$b + 1}] [expr {$e - 1}]]
        if {![regexp {%([0-9]+\$)?[-0-9.]*[sdxfc]} [string map {%% {}} $key]]} { continue }
        set before [string range $line 0 [expr {$i - 1}]]
        set rest [string range $line [expr {$e + 1}] end]
        set close [string first "\]" $rest]
        if {$close < 0} { set close [string length $rest] }
        # Args on the mc call itself: mc formats.
        if {[string trim [string range $rest 0 [expr {$close - 1}]]] ne ""} { continue }
        # The call continues on the next line (its args are there).
        if {$close == [string length $rest]
            && [string match "*\\" [string trimright $line]]} { continue }
        # `[format [::vi18n::mc {…}] …]`, on this line or continued.
        if {[regexp {format\s+\[(::)?$} $before]} { continue }
        if {[regexp {^\s*\[(::)?$} $before]
            && [regexp {format\s*\\$} [string trimright $prev]]} { continue }
        lappend hits $key
    }
    return $hits
}
set _unfmt {}
foreach f [lsort [glob -nocomplain [file join $_dir *.tcl]]] {
    if {[string match "*test.tcl" $f]} { continue }
    set fh [open $f r]
    fconfigure $fh -encoding utf-8
    set prev ""
    set n 0
    foreach line [split [read $fh] \n] {
        incr n
        if {![string match "#*" [string trim $line]]
            && ![string match "*i18n-exempt*" $line]} {
            foreach k [::i18ntest_unformatted $line $prev] {
                lappend _unfmt "[file tail $f]:$n {$k}"
            }
        }
        set prev $line
    }
    close $fh
}
foreach _u $_unfmt { puts "      unformatted: $_u" }
ok "no placeholder key reaches the screen unformatted" {[llength $_unfmt] == 0}

puts "── a registered widget that was overwritten is not resurrected"
# THE FAILURE MODE label() INTRODUCES. A status label is registered once
# with a catalog message and then overwritten with plain runtime state by
# a later turn. Re-rendering it unconditionally on a swap would put a
# stale translated message back over whatever the user is reading — the
# same hazard the runtime-text guard exists for, arriving by a new road.
# relabel() therefore re-texts only while the widget still holds the
# text label() wrote, and drops the registration once something else
# has claimed it.
if {[catch {package require Tk}] == 0 || [winfo exists .]} {
    set _rw [ttk::label .i18n_overwrite_lbl]
    ::vi18n::label $_rw {saved}
    set _before [$_rw cget -text]
    # Something else claims the widget, exactly as a status line does.
    $_rw configure -text "gateway up 3s"
    ::vi18n::use de
    ok "an overwritten label keeps its runtime text" \
        {[$_rw cget -text] eq "gateway up 3s"}
    ::vi18n::use en
    ok "…and the stale catalog string does not come back" \
        {[$_rw cget -text] eq "gateway up 3s"}
    ok "…and it is no longer registered" \
        {[::vi18n::widget_key $_rw] eq ""}
    destroy $_rw

    # The positive case, so the guard cannot pass by never re-texting.
    set _k [ttk::label .i18n_keep_lbl]
    ::vi18n::label $_k {failed: %s} "timeout"
    set _en [$_k cget -text]
    ::vi18n::use de
    set _de [$_k cget -text]
    ::vi18n::use en
    ok "a registered formatted label follows the swap" \
        {$_en ne $_de && $_en ne ""}
    ok "…and comes back" {[$_k cget -text] eq $_en}
    destroy $_k
} else {
    puts "  skip (no Tk display)"
}

puts "── a registered accessible name follows the swap"
if {[catch {package require Tk}] == 0 || [winfo exists .]} {
    set _an [ttk::entry .i18n_aname_e]
    ::vi18n::aname $_an {field: %s} "Model"
    set _en_an [::va11y::describe $_an]
    ::vi18n::use de
    set _de_an [::va11y::describe $_an]
    ::vi18n::use en
    ok "an accessible name with format args follows the swap" \
        {$_en_an ne $_de_an && $_en_an ne ""}
    ok "…and comes back" {[::va11y::describe $_an] eq $_en_an}
    destroy $_an
} else {
    puts "  skip (no Tk display)"
}

puts "── accessible names are translatable"
set _ab {}
foreach f [lsort [glob -nocomplain [file join $_dir *.tcl]]] {
    if {[string match "*test.tcl" $f]} { continue }
    if {[file tail $f] eq "a11y.tcl"} { continue }
    # i18n.tcl is where the translation machinery itself lives: its
    # ::va11y::name calls pass an already-resolved `$text` from
    # ::vi18n::mc, so they are the fix, not a gap. The other scans in
    # this file skip it for the same reason.
    if {[file tail $f] eq "i18n.tcl"} { continue }
    set all [read [set fh [open $f r]]]
    close $fh
    set n 0
    foreach line [split $all \n] {
        incr n
        if {![string match "*va11y::name*" $line]} { continue }
        if {[string match "#*" [string trim $line]]} { continue }
        if {[string match "*proc ::va11y::name*" $line]} { continue }
        if {[string match "*i18n-exempt*" $line]} { continue }
        lappend _ab "[file tail $f]:$n"
    }
}
# Names are often split across lines ("...bar.send \\" then the text), so
# re-check with a two-line window before believing a hit.
set _ab2 {}
foreach hit $_ab {
    lassign [split $hit :] file ln
    set fh [open [file join $_dir $file] r]
    set all [split [read $fh] \n]
    close $fh
    set win [join [lrange $all [expr {$ln - 1}] [expr {$ln}]] ""]
    if {[string match "*vi18n*" $win]} { continue }
    lappend _ab2 $hit
}
if {[llength $_ab2]} { puts "      unwrapped: $_ab2" }
ok "every accessible name goes through the catalog" {[llength $_ab2] == 0}

puts "── accessible names follow a swap too"
# ::va11y::name STORES the rendered string (85 call sites pass it
# [::vi18n::mc {…}]), so a swap left every accessible name and tooltip
# describing the previous language. Same class of staleness as widget
# -text, but invisible to the widget sweep because it lives in a11y's own
# array rather than in a widget option.
if {[catch {package require Tk}] == 0 || [winfo exists .]} {
    set _aw [ttk::button .i18n_a11y_btn]
    ::va11y::name $_aw [::vi18n::mc {Send}]
    set _en_name [::va11y::describe $_aw]
    ::vi18n::use de
    set _de_name [::va11y::describe $_aw]
    ok "an accessible name re-resolves on a locale change" {
        $_de_name ne $_en_name && $_de_name ne ""
    }
    ::vi18n::use en
    ok "…and comes back on the way home" {[::va11y::describe $_aw] eq $_en_name}
    destroy $_aw
} else {
    puts "  skip (no Tk display)"
}

puts "── runtime status text is NOT re-texted by the sweep"
# A status line looks exactly like a label to a tree walker, and its
# content is runtime state ("gateway up 3s", the last action's message).
# A few of those strings DO collide with catalog keys — `redrawn` is an
# entry — so without a guard a swap would replace a live status message
# with a translation of it, destroying what the user was reading.
ok "the status message widget is declared runtime text" {
    [::vi18n::is_runtime_text_widget .statusbar.msg] == 1
}
ok "an ordinary label is not" {
    [::vi18n::is_runtime_text_widget .some.label] == 0
}

puts "── the picker's rows follow the swap"
set _rows [::vlang::rows]
set _marked 0
set _ids_ok 1
foreach _row $_rows {
    lassign $_row _marks _name
    # Containment, not equality: a row can carry several marks now
    # (◆ active, ★ default, ⇄ swap target — the active language is
    # usually the default too).
    if {[string first "◆" $_marks] >= 0} { incr _marked }
    set _idx [string last "  ·  " $_name]
    if {$_idx < 0} { set _ids_ok 0 ; continue }
    set _id [string trim [string range $_name [expr {$_idx + 5}] end]]
    if {[lsearch -exact [::vi18n::available] $_id] < 0} { set _ids_ok 0 }
}
ok "a row per shipped catalog" {[llength $_rows] == [llength [::vi18n::available]]}
ok "the active locale is marked exactly once" {$_marked == 1}
ok "every row carries its canonical id as the last field" {$_ids_ok}

puts "── endonyms survive their own spaces"
# The bug this pins: the table was a flat whitespace-separated list read
# with `lsearch` + `lindex+1`, so any name with a SPACE was truncated to
# its first word - `id` and `ms` both answered "Bahasa", `vi` answered
# "Tiếng". A truncated endonym is worse than an untranslated one: it
# names a language that does not exist, in the row a speaker scans for
# their own. Multi-word names must come back whole and trimmed.
ok "vi is the full endonym" {[::vi18n::locale_name vi] eq "Tiếng Việt"}
ok "id is the full endonym" {[::vi18n::locale_name id] eq "Bahasa Indonesia"}
ok "ms is its own name, not id's" {
    [::vi18n::locale_name ms] eq "Bahasa Melayu"
}
ok "two multi-word names stay distinct" {
    [::vi18n::locale_name id] ne [::vi18n::locale_name ms]
}
ok "a braced name has no stray padding" {
    [::vi18n::locale_name id] eq [string trim [::vi18n::locale_name id]]
}
# Every shipped locale must have a name that is NOT its own id: an id
# leaking into the picker means the row is unreadable for the person it
# is for. (en's endonym is "English", so no id is its own name.)
set _noname {}
foreach _l [::vi18n::available] {
    set _n [::vi18n::locale_name $_l]
    if {$_n eq $_l || [string trim $_n] eq ""} { lappend _noname $_l }
}
if {[llength $_noname]} { puts "      no endonym: [lrange $_noname 0 5]" }
ok "every shipped locale has an endonym" {[llength $_noname] == 0}

file delete -force $_tmp
puts ""
# ── a 4-byte character on a 4 KiB boundary ─────────────────────────
# Tcl 8.6 reads a file in 4 KiB buffers and mis-decodes a 4-byte UTF-8
# character (U+10000 and up) whose lead byte sits at 4093 mod 4096; a
# catalog line carrying one loaded under a garbled key and fell back to
# English (ru's 📎 hint, 2026-10-04). The writers pad around it
# (scripts/i18n-unsplit.tcl); this names a catalog that has one anyway,
# which the completeness checks above would only call "incomplete".
set _split {}
foreach _f [lsort [glob -nocomplain [file join $_dir .. msgs *.msg]]] {
    set _fh [open $_f rb]; set _bytes [read $_fh]; close $_fh
    foreach _pair [regexp -all -indices -inline {[\xf0-\xf4]} $_bytes] {
        if {[lindex $_pair 0] % 4096 == 4093} {
            lappend _split "[file tail $_f] byte [lindex $_pair 0]"
        }
    }
}
foreach _u $_split { puts "      split 4-byte character: $_u — run scripts/i18n-sync.sh" }
ok "no catalog puts a 4-byte character where Tcl 8.6 splits it" {[llength $_split] == 0}

# The tally is `i18ntest:`, matching the FILE name, because that is what
# scripts/precommit.sh and flake.nix derive from `TESTS=` and grep for
# (`^<basename-of-file>: N passed, M failed`). This printed `i18n:` — a
# label nobody greps — so the suite's own failure count was INVISIBLE to
# the gate: all that ever fired was the "no tally" check, which only
# reports an abort. A suite can be red here for a real reason and the
# gate still name it "aborted", which is a misleading report on top of
# an unchecked number.
if {$::F} {
    puts "i18ntest: $::T passed, $::F failed"
    exit 1
}
puts "i18ntest: $::T passed, 0 failed"
exit 0
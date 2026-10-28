# veles-desktop/lib/theme.tcl — design tokens + ttk styling + palettes.
#
# Multiple named palettes; `synthwave` is the default. Switch live with
# `::vt::use_theme <name>` (the palette overlay calls this). User themes
# live in themes/*.tcl under the config dir and are auto-discovered — each
# defines a `::vt::palette_<name>` array with the same keys as the built-ins.

namespace eval ::vt {
    # Monospace font specs across the UI are written {monospace <size> …},
    # NEVER {TkFixedFont <size>}. Tk reads a font LIST's first element as a
    # FAMILY NAME, and "TkFixedFont" is a NAMED font, not a family — so
    # {TkFixedFont 10} finds no such family and silently falls back to the
    # default SANS, rendering every "monospace" surface (terminal tool
    # output, the code editor, diffs, IRC) in a proportional font with the
    # wrong glyph coverage. "monospace" IS a real family token: fontconfig
    # resolves the generic alias to the user's preferred mono — here
    # CaskaydiaCove Nerd Font, which carries the powerline/icon/box-drawing
    # glyphs terminal output is full of (see ~/.config/fontconfig). irc.tcl
    # already knew this for {TkFixedFont bold}, which errors outright; the
    # numeric-size cases failed SILENTLY until terminal output came back.
}

# ── built-in palettes ───────────────────────────────────────────────
# Each is an array with the same keys; ::vt::T is the ACTIVE one.

proc ::vt::palette_synthwave {a} {
    upvar 1 $a T
    array set T {
        bg         #0d0613
        bg_panel   #1f1235
        bg_input   #120820
        bg_hover   #2e1d4a
        fg         #f5edff
        fg_dim     #b8a6d8
        hairline   #46325e
        accent     #ff3d8a
        accent_fg  #1a0510
        user       #2de2e6
        assistant  #f5edff
        reasoning  #d4a0ff
        tool       #ffd24d
        system     #ff5c70
        ok         #5dff9b
        warn       #ffd24d
        select_bg  #5c3a8f
        pink       #ff3d8a
        cyan       #2de2e6
        purple     #d4a0ff
        gold       #ffd24d
        green      #5dff9b
        red        #ff5c70
    }
}

proc ::vt::palette_classic {a} {
    upvar 1 $a T
    array set T {
        bg         #121417
        bg_panel   #1b1e23
        bg_input   #0f1115
        bg_hover   #2a3040
        fg         #e6e6e6
        fg_dim     #8a8f98
        hairline   #3a414d
        accent     #6ea8fe
        accent_fg  #0a0e14
        user       #74e0c9
        assistant  #e6e6e6
        reasoning  #b08cff
        tool       #e5c07b
        system     #ff6b6b
        ok         #81c784
        warn       #e5c07b
        select_bg  #2d3644
        pink       #6ea8fe
        cyan       #74e0c9
        purple     #b08cff
        gold       #e5c07b
        green      #81c784
        red        #ff6b6b
    }
}

proc ::vt::palette_amber {a} {
    upvar 1 $a T
    array set T {
        bg         #120b00
        bg_panel   #1c1404
        bg_input   #0a0600
        bg_hover   #2e2208
        fg         #ffd966
        fg_dim     #9a7b2a
        hairline   #4a3a10
        accent     #ffb000
        accent_fg  #1c1404
        user       #ffb000
        assistant  #ffd966
        reasoning  #cc8800
        tool       #ffb000
        system     #ff6a00
        ok         #ffb000
        warn       #ff6a00
        select_bg  #33260a
        pink       #ffb000
        cyan       #ffb000
        purple     #ffb000
        gold       #ffb000
        green      #ffb000
        red        #ff6a00
    }
}

proc ::vt::palette_mono {a} {
    upvar 1 $a T
    array set T {
        bg         #0a0a0a
        bg_panel   #141414
        bg_input   #070707
        bg_hover   #222222
        fg         #e8e8e8
        fg_dim     #777777
        hairline   #333333
        accent     #c8c8c8
        accent_fg  #0a0a0a
        user       #c8c8c8
        assistant  #e8e8e8
        reasoning  #888888
        tool       #c8c8c8
        system     #ffffff
        ok         #c8c8c8
        warn       #c8c8c8
        select_bg  #2a2a2a
        pink       #c8c8c8
        cyan       #c8c8c8
        purple     #c8c8c8
        gold       #c8c8c8
        green      #c8c8c8
        red        #ffffff
    }
}

# The active palette — synthwave by default.
array set ::vt::T {}
set ::vt::current_theme "synthwave"

# Discover all theme names: built-ins + the user's themes/*.tcl.
# ── themes from the engine ───────────────────────────────────────────
#
# The Rust side owns theme resolution (built-ins + themes/*.toml under the
# config dir) and serves the result at /api/themes. This used to be
# duplicated as
# parallel `palette_*` procs here, hand-synced with theme.rs — same
# preset names, different key names, two files per user theme. Those
# procs remain as the OFFLINE FALLBACK for when the gateway is not up.
#
# Rust key → Tcl key:
#   text→fg  dim→fg_dim  panel→bg_panel  panel_hi→bg_hover  hair→hairline
# The role colours (user/assistant/reasoning/tool/...) are derived from
# the same six accents, so a TOML theme needs no extra keys.
# `apply_theme` only pushes ttk styles, so the long-lived core-Tk
# widgets (transcript, composer, sidebar, preview) kept their old colours
# until restart. Re-colour them explicitly on every switch.
proc ::vt::retheme_widgets {} {
    foreach {w opts} {
        .mid.chatwrap.chat.input   {-background bg_panel -foreground fg -insertbackground pink}
        .mid.chatwrap.chat.preview {-background bg_panel -foreground fg}
        $::vsidebar::R.list {-background bg_panel -foreground fg}
    } {
        if {![winfo exists $w]} continue
        foreach {opt key} $opts {
            catch { $w configure $opt $::vt::T($key) }
        }
    }
    # Composer focus ring follows the theme too.
    catch {
        .mid.chatwrap.chat.input configure -highlightbackground $::vt::T(hairline) \
            -highlightcolor $::vt::T(accent)
    }
    # EVERY live transcript, not just the one on screen. Sessions keep
    # their own widget now and are hidden rather than wiped, so a theme
    # switch that re-coloured only the visible one left every other
    # session carrying the old palette until it was repainted — which,
    # since it never is, means until the app restarts.
    set _logs .mid.chatwrap.chat.log
    catch { set _logs [::vchat::logs_all] }
    foreach _w $_logs {
        if {![winfo exists $_w]} { continue }
        catch {
            $_w configure -background $::vt::T(bg_input) -foreground $::vt::T(fg) \
                -insertbackground $::vt::T(pink)
        }
        # Tags carry their own colours; rebuild the ones we own.
        catch {
            $_w tag configure user -foreground $::vt::T(user)
            $_w tag configure userhdr -foreground $::vt::T(cyan)
            $_w tag configure assistant -foreground $::vt::T(assistant)
            $_w tag configure reasoning -foreground $::vt::T(reasoning)
            $_w tag configure tool -foreground $::vt::T(tool)
            $_w tag configure system -foreground $::vt::T(system)
            $_w tag configure status -foreground $::vt::T(fg_dim)
            $_w tag configure note -foreground $::vt::T(fg_dim)
            $_w tag configure note_more -foreground $::vt::T(accent)
            $_w tag configure stamp -foreground $::vt::T(fg_dim)
            $_w tag configure stampday -foreground $::vt::T(fg_dim)
            $_w tag configure mdsource -foreground $::vt::T(fg_dim)
        }
        catch { ::vmd::_tags $_w }
    }
    # The Dev pane's tab strip is a canvas, so it carries its own
    # background rather than inheriting a ttk style — a theme switch
    # would otherwise leave one bright bar across the pane.
    catch {
        $::vpanes::R.dev.edit.tabs configure -background $::vt::T(bg)
    }
    # Every Text widget, not the three that happened to be listed above:
    # selection and caret colours are theme-derived and there are 22 of
    # them.
    catch { ::vtext::restyle_all }
    # The markdown tags are created per widget on first use; drop the
    # "already tagged" flag so they are rebuilt with the new palette.
    catch { array unset ::vmd::tags_done }
    catch { ::vmd::_tags .mid.chatwrap.chat.preview }
    # Re-render the transcript so md tags pick up the new colours.
    catch { ::vchat::rebuild }
}

proc ::vt::from_engine {name target} {
    upvar 1 $target T
    if {[catch {::vapi::get_json /api/themes} d]} { return 0 }
    foreach t [dict get $d themes] {
        if {[dict get $t name] ne $name} continue
        array set T [list \
            bg          [dict get $t bg] \
            bg_panel    [dict get $t panel] \
            bg_input    [dict get $t bg] \
            bg_hover    [dict get $t panel_hi] \
            fg          [dict get $t text] \
            fg_dim      [dict get $t dim] \
            hairline    [dict get $t hair] \
            accent      [dict get $t pink] \
            accent_fg   [dict get $t bg] \
            user        [dict get $t cyan] \
            assistant   [dict get $t text] \
            reasoning   [dict get $t purple] \
            tool        [dict get $t gold] \
            system      [dict get $t red] \
            ok          [dict get $t green] \
            warn        [dict get $t gold] \
            select_bg   [dict get $t panel_hi] \
            pink        [dict get $t pink] \
            cyan        [dict get $t cyan] \
            purple      [dict get $t purple] \
            gold        [dict get $t gold] \
            green       [dict get $t green] \
            red         [dict get $t red]]
        return 1
    }
    return 0
}

proc ::vt::theme_names {} {
    # Engine list first (it knows about *.toml user themes); fall back
    # to the built-in names plus any local *.tcl themes.
    if {![catch {::vapi::get_json /api/themes} d]} {
        set names {}
        foreach t [dict get $d themes] { lappend names [dict get $t name] }
        if {[llength $names] > 0} { return $names }
    }
    set names {synthwave classic amber mono}
    set dir [::vpaths::theme_dir]
    if {[file isdir $dir]} {
        foreach f [glob -nocomplain -directory $dir *.tcl] {
            set n [file rootname [file tail $f]]
            if {$n ni $names} { lappend names $n }
        }
    }
    return $names
}

# Load a user theme from the config dir's themes/<name>.tcl (defines
# ::vt::palette_<name>). Safe to call for built-ins (no-op).
proc ::vt::load_user_theme {name} {
    if {$name in {synthwave classic amber mono}} { return }
    set f [file join [::vpaths::theme_dir] "$name.tcl"]
    if {[file isfile $f]} {
        # isolate: source in a sandbox interp, then grab the array
        set sid [interp create]
        interp transfer {} {} $sid
        catch {interp eval $sid [list source $f]}
        catch {
            interp eval $sid [list uplevel "#0" [list ::vt::palette_$name ::vt::T]]
        }
        interp delete $sid
    }
}

# Apply a theme by name: loads it (if user), populates ::vt::T, styles.
proc ::vt::use_theme {name} {
    # Engine first (single source of truth), local procs as fallback.
    if {![::vt::from_engine $name ::vt::T]} {
        ::vt::load_user_theme $name
        if {[catch {::vt::palette_$name ::vt::T} e]} {
            # unknown theme — keep the current one
            return -code error "unknown theme: $name"
        }
    }
    set ::vt::current_theme $name
    ::vt::apply_theme
    ::vt::retheme_widgets
    # The banner is theme-aware — re-skin it with the new palette.
    catch { ::vbrand::redraw }
    # Persist the choice. This block used to re-encode EVERY key in the
    # blob as a quoted string on its way past, so changing a theme
    # rewrote the pins and the composer history into strings.
    if {![::vstate::put theme $name]} {
        catch { ::vstatus::set_msg [format [::vi18n::mc {theme not saved: %s}] $::vstate::error] }
    }
}

proc ::vt::apply_theme {} {
    variable T
    option add *background $T(bg)
    option add *foreground $T(fg)
    option add *selectBackground $T(select_bg)
    option add *selectForeground $T(fg)
    option add *highlightThickness 0
    # Text widgets need three more that the wildcards above do not
    # reach — inactiveSelectBackground in particular, which is why an
    # unfocused selection was invisible. See lib/vtext.tcl.
    catch { ::vtext::defaults }

    ttk::style theme use clam
    ttk::style configure . -background $T(bg) -foreground $T(fg) \
        -fieldbackground $T(bg_input) -bordercolor $T(hairline) \
        -lightcolor $T(bg_panel) -darkcolor $T(bg_panel)
    ttk::style configure TFrame -background $T(bg)
    ttk::style configure TLabelframe -background $T(bg) -foreground $T(fg_dim) \
        -bordercolor $T(hairline)
    ttk::style configure TLabelframe.Label -background $T(bg) -foreground $T(fg_dim)
    ttk::style configure TLabel -background $T(bg) -foreground $T(fg)
    ttk::style configure TButton -background $T(bg_panel) -foreground $T(fg) \
        -bordercolor $T(hairline) -focuscolor $T(accent) -padding {10 4}
    ttk::style map TButton -background [list active $T(bg_hover) \
        pressed $T(accent)] -foreground [list pressed $T(accent_fg)]
    ttk::style configure Accent.TButton -background $T(accent) \
        -foreground $T(accent_fg) -bordercolor $T(accent) -font {TkDefaultFont 10 bold}
    ttk::style map Accent.TButton -background [list active $T(pink) \
        disabled $T(hairline)] -foreground [list disabled $T(fg_dim) \
        active $T(accent_fg)]
    # Dev-pane buffer tabs. Tight padding because a strip of them has to
    # fit across the pane, and the active one is marked by the accent
    # rather than by a border — the strip already reads as a row.
    ttk::style configure DevTab.TButton -background $T(bg_panel) \
        -foreground $T(fg_dim) -bordercolor $T(hairline) \
        -padding {6 2} -font {TkDefaultFont 9}
    ttk::style map DevTab.TButton -background [list active $T(bg_hover)] \
        -foreground [list active $T(fg)]
    ttk::style configure DevTabOn.TButton -background $T(bg_hover) \
        -foreground $T(accent) -bordercolor $T(accent) \
        -padding {6 2} -font {TkDefaultFont 9 bold}
    ttk::style map DevTabOn.TButton -background [list active $T(bg_hover)] \
        -foreground [list active $T(accent)]
    # Check and radio marks in the palette. Clam's own are a white disc
    # when off and a white ring round a black dot when on: on a dark
    # theme the choice NOT made was the one that stood out (emulator,
    # 2026-09-30; the laptop drew it the same).
    foreach s {TCheckbutton TRadiobutton} {
        ttk::style configure $s -indicatorbackground $T(bg_input) \
            -indicatorforeground $T(accent) \
            -upperbordercolor $T(fg_dim) -lowerbordercolor $T(fg_dim)
        ttk::style map $s -indicatorbackground [list pressed $T(bg_hover) disabled $T(bg)] \
            -indicatorforeground [list disabled $T(fg_dim)]
    }
    ttk::style configure TEntry -fieldbackground $T(bg_input) -foreground $T(fg) \
        -insertcolor $T(pink) -bordercolor $T(hairline)
    # A readonly combobox is clam's own light-grey field, its text
    # selected on focus: the All settings section picker read white on
    # pale grey on the real phone (2026-10-11).
    ttk::style configure TCombobox -fieldbackground $T(bg_input) -foreground $T(fg) \
        -arrowcolor $T(fg) -bordercolor $T(hairline)
    ttk::style map TCombobox \
        -fieldbackground [list readonly $T(bg_input) disabled $T(bg)] \
        -foreground [list readonly $T(fg) disabled $T(fg_dim)] \
        -selectbackground [list readonly $T(bg_input)] \
        -selectforeground [list readonly $T(fg)]
    ttk::style configure TScrollbar -background $T(bg_panel) -troughcolor $T(bg) \
        -bordercolor $T(bg) -arrowcolor $T(fg_dim)
    # A scrollbar is `disabled` whenever everything fits, and clam maps
    # a disabled background to its own light grey: an empty or short
    # transcript had a pale bar down its side (emulator, 2026-09-30).
    ttk::style map TScrollbar -background [list disabled $T(bg_panel) active $T(bg_hover)] \
        -arrowcolor [list disabled $T(hairline)]
    # 24 desktop pixels; a phone's are dp (lib/android.tcl), or the rows
    # overlap their own text.
    set rowh 24
    if {[llength [info commands ::vandroid::dp]]} { set rowh [::vandroid::dp 24] }
    ttk::style configure Treeview -background $T(bg_panel) -foreground $T(fg) \
        -fieldbackground $T(bg_panel) -bordercolor $T(hairline) -rowheight $rowh
    ttk::style configure Treeview.Heading -background $T(bg_panel) -foreground $T(fg_dim)
    ttk::style map Treeview -background [list selected $T(select_bg)] \
        -foreground [list selected $T(fg)]
    ttk::style configure Horizontal.TProgressbar -background $T(accent) \
        -troughcolor $T(bg_panel) -bordercolor $T(bg_panel)
    # A phone's paddings and indicators in dp (lib/android.tcl).
    if {[llength [info commands ::vandroid::touch_theme]]} { ::vandroid::touch_theme }
}
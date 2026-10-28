#!/usr/bin/env wish
# veles-desktop — Tcl/Tk window onto the running veles gateway.
# hermes-desktop replacement; feature status lives in COVERAGE.md.
#
# Composition root: sources lib/ modules, builds the layout, binds
# shortcuts. All behavior lives in the modules — which makes the whole
# UI hot-patchable (see lib/hotpatch.tcl, the differentiator).
#
# Env: VELES_API (default http://127.0.0.1:8642),
#      VELES_TOKEN (bearer for a gateway with DESKTOP_BEARER_TOKEN /
#        DESKTOP_PEER_TOKENS set; unset for the loopback default),
#      VELES_INSTANCE (name from instances.json — supplies both of the
#        above, so a saved remote is one variable instead of two),
#      VELES_DESKTOP_SELFTEST=<msg> (headless smoke hook),
#      VELES_PRINT_WINDOW_ID=1 (print the X window id after mapping —
#      used by the visual-capture harness).

package require Tk

# UTF-8, unconditionally. Pasting UTF-8 text with the system encoding
# left at the locale's default (C/POSIX → Latin-1) is how an "…" or a
# "—" becomes "â€¦" / "â€"" — USER: "ascii/unicode conversion artefacts
# when copying from unicode sources … â characters everywhere". This must
# run BEFORE any clipboard/selection read.
encoding system utf-8

set _script_dir [file dirname [info script]]

proc bgerror {msg} {
    puts stderr "BGERROR: $msg"
    catch { puts stderr $::errorInfo }
    catch { ::vstatus::set_msg "bgerror: $msg" }
}

# ── modules ──────────────────────────────────────────────────────────
# paths before everything: the three XDG roots, mirroring the engine's
# config_dir/data_dir/cache_dir. i18n reads config.toml, so it has to be
# able to ask where that is.
source [file join $_script_dir lib paths.tcl]
# i18n FIRST (l10n): resolves the locale (config > env > system > en)
# so every later module can translate at source time.
source [file join $_script_dir lib i18n.tcl]
::vi18n::init
# Android (AndroWish): the phone's size, before any font is measured.
# A no-op everywhere else.
source [file join $_script_dir lib android.tcl]
::vandroid::scale
# safeexec: Tcl's exec parses redirection out of the ARGUMENT LIST, so a
# remote nick or message can become a command. One module knows that, and
# everything that spawns a helper goes through it. No dependencies.
source [file join $_script_dir lib safeexec.tcl]
# a11y: ui.font_scale scales every stock Tk font before widgets exist.
source [file join $_script_dir lib a11y.tcl]
# vlist owns list-picker behaviour (cursor, filter, wrap) — sourced
# before every picker that uses it.
source [file join $_script_dir lib vlist.tcl]
# One popup-placement rule (centred vs. anchored, both clamped).
source [file join $_script_dir lib place.tcl]
# Right-click menus, one implementation.
source [file join $_script_dir lib ctxmenu.tcl]
# Real emacs editing for text widgets (kill ring, mark, undo stack).
source [file join $_script_dir lib emacs.tcl]
# In-app window controls, so the native title bar can be turned off.
source [file join $_script_dir lib vstate.tcl]
source [file join $_script_dir lib titlebar.tcl]
::va11y::init
source [file join $_script_dir lib vtext.tcl]
source [file join $_script_dir lib theme.tcl]
source [file join $_script_dir lib brand.tcl]
source [file join $_script_dir lib bindings.tcl]
source [file join $_script_dir lib keymap.tcl]
# The language picker + status-bar chip (i18n swap). After i18n (it
# reads ::vi18n) and before statusbar (which packs its chip).
source [file join $_script_dir lib langpick.tcl]
source [file join $_script_dir lib coach.tcl]
source [file join $_script_dir lib help.tcl]
source [file join $_script_dir lib jsonfield.tcl]
source [file join $_script_dir lib api.tcl]
# THE DEMO GATEWAY (VELES_DESKTOP_DEMO=1): fixture sessions and a
# scripted timeline in place of the transport, so the cross-session
# behaviour — two turns at once, a background approval, a ▶ this window
# never saw start — can be looked at without inventing two errands for a
# real model. Sourced HERE, immediately after api.tcl, so that no real
# request is ever attempted: boot, the sidebar's first refresh and the
# status poll all land on the fixture. Everything above ::vapi is the
# shipping code. See lib/demo.tcl and scripts/desktop_demo.sh.
if {[info exists env(VELES_DESKTOP_DEMO)] && $env(VELES_DESKTOP_DEMO) eq "1"} {
    source [file join $_script_dir lib demo.tcl]
}
source [file join $_script_dir lib instances.tcl]
# attach.tcl: the app-wide attach switch (PLAN Part 6). Sourced before
# the panes so ::vapi::unattached guards resolve at call time, not at
# source time; instpane sources after it (its ssh path reads
# ::vapp::epoch).
source [file join $_script_dir lib attach.tcl]
source [file join $_script_dir lib instpane.tcl]
source [file join $_script_dir lib statusbar.tcl]
source [file join $_script_dir lib media.tcl]
source [file join $_script_dir lib markdown.tcl]
source [file join $_script_dir lib dock.tcl]
source [file join $_script_dir lib tabs.tcl]
source [file join $_script_dir lib whichkey.tcl]
source [file join $_script_dir lib syntax.tcl]
source [file join $_script_dir lib lspclient.tcl]
source [file join $_script_dir lib cmds.tcl]
source [file join $_script_dir lib complete.tcl]
source [file join $_script_dir lib editline.tcl]
source [file join $_script_dir lib dev.tcl]
# The device bridge: what this window can do for the agent (a phone's
# borg, a laptop's clipboard), announced to the gateway and answered here.
source [file join $_script_dir lib device.tcl]
source [file join $_script_dir lib device_android.tcl]
source [file join $_script_dir lib device_desktop.tcl]
source [file join $_script_dir lib chat.tcl]
source [file join $_script_dir lib sidebar.tcl]
source [file join $_script_dir lib panes.tcl]
source [file join $_script_dir lib irc.tcl]
source [file join $_script_dir lib findbar.tcl]
# manage.tcl: the shared manager-overlay shape (toolbar, tree, the API
# error unwrap, the honest config-effect line). Sourced before the
# overlays that use it — and after ctxmenu/place/a11y, which it calls.
source [file join $_script_dir lib manage.tcl]
source [file join $_script_dir lib overlays.tcl]
source [file join $_script_dir lib acl.tcl]
# sessions.tcl: the Sessions window (search, paging, every session
# action). After manage/overlays (its chrome) and sidebar/chat (it calls
# their open/pin/export procs at run time).
source [file join $_script_dir lib sessions.tcl]
# setup.tcl: the first-run wizard (PLAN-setup-wizard-tk.md). After acl
# (same overlay idioms), api/vstate/place/titlebar (it calls all four).
source [file join $_script_dir lib install.tcl]
source [file join $_script_dir lib setup.tcl]
source [file join $_script_dir lib sandbox.tcl]
source [file join $_script_dir lib phone.tcl]
source [file join $_script_dir lib settingsform.tcl]
source [file join $_script_dir lib apps.tcl]
source [file join $_script_dir lib ask.tcl]
source [file join $_script_dir lib failgraph.tcl]
source [file join $_script_dir lib palette.tcl]
source [file join $_script_dir lib cmdtree.tcl]
source [file join $_script_dir lib hotpatch.tcl]

if {[info exists env(VELES_API)] && $env(VELES_API) ne ""} {
    ::vapi::set_base $env(VELES_API)
}
# A remote instance needs both its URL (VELES_API) and its bearer token
# (VELES_TOKEN). The token alone is harmless on loopback — the gateway
# ignores an Authorization header when no token is configured.
if {[info exists env(VELES_TOKEN)] && $env(VELES_TOKEN) ne ""} {
    ::vapi::set_token $env(VELES_TOKEN)
}
# VELES_INSTANCE names a saved gateway and supplies both of the above.
# Explicit VELES_API/VELES_TOKEN win — naming a thing should not override
# pointing at one.
if {[info exists env(VELES_INSTANCE)] && $env(VELES_INSTANCE) ne ""} {
    if {[catch { ::vinst::select $env(VELES_INSTANCE) \
        [info exists env(VELES_API)] [info exists env(VELES_TOKEN)] } _e]} {
        catch { puts stderr "veles-desktop: $_e" }
    } else {
        # Bind under the instance's NAME with the EFFECTIVE url/token
        # (select already honoured explicit env overrides), so the boot
        # attach can name it.
        ::vapi::bind_instance $env(VELES_INSTANCE) \
            [::vapi::base] [::vapi::token]
    }
}
# THE REGISTRY, and the boot attach decision (PLAN Part 6).
#
# An explicit VELES_API is a deliberate point-at: bind it as a synthetic
# instance named `env` so the app is ATTACHED (not unattached-with-a-
# base-url, which every request guard would refuse). ::vapp::boot_decide
# then applies the precedence chain: env > VELES_INSTANCE > the last
# instance attached in a previous run > unattached.
::vinst::load
# A phone's first run attaches to the gateway in the same app.
::vandroid::seed_registry
# Bind EVERY saved instance into ::vapi, not just the one this boot
# attaches to. See ::vinst::bind_all for the live bug this exists for:
# an unbound registry row silently polled the DEFAULT gateway, so the
# VPS row rendered the local gateway's version, uptime, platforms and
# session count, all green. Env bindings (the `env`/`VELES_INSTANCE`
# paths above) win — bind_all never clobbers a name already bound.
::vinst::bind_all
if {[info exists env(VELES_API)] && $env(VELES_API) ne ""} {
    set _t ""
    if {[info exists env(VELES_TOKEN)]} { set _t $env(VELES_TOKEN) }
    ::vapi::bind_instance env $env(VELES_API) $_t
    ::vapi::use_instance env
}
::vapp::boot_decide

# ── say it now if the gateway will not talk to us ────────────────────
#
# /api/health is deliberately unauthenticated (it is the container
# liveness probe), so it CANNOT tell us the token is wrong. Probe a real
# route instead.
#
# A refused token is fatal in a way a down gateway is not: the desktop is
# designed to start against a dead backend and show it, but against a
# LIVE backend that rejects every request it would open with a full set
# of empty panes and no explanation. Say so and stop.
#
# UNATTACHED (PLAN Part 6): the boot attach happens in ::vapp::boot,
# AFTER the layout exists. When the app boots unattached there is
# nothing to probe — no request leaves the process. When ::vinst has
# already pointed base/token at a gateway (legacy env boot), probe THAT.
if {![::vapi::unattached]} {
    if {[catch { ::vapi::get /api/status } _probe]} {
        if {[regexp {HTTP (401|403)} $_probe -> _code]} {
            catch { wm withdraw . }
            catch {
                tk_messageBox -type ok -icon error -title "veles-desktop" \
                    -message "Cannot connect to the gateway" \
                    -detail [::vapi::http_reason $_code]
            }
            catch { puts stderr "veles-desktop: [::vapi::http_reason $_code]" }
            exit 1
        }
        # Anything else — refused, timed out, mid-restart — is the existing
        # "start anyway and show it in the status bar" case.
    }
}

::vt::use_theme synthwave
# Override with the persisted choice if there is one. Unattached: no
# request leaves the process — the default theme stands until an attach.
if {![::vapi::unattached]} {
    if {![catch {::vapi::get_json /api/desktop/state} st]} {
        if {[dict exists $st theme]} {
            set saved [dict get $st theme]
            if {$saved ne "synthwave"} {
                catch {::vt::use_theme $saved}
            }
        }
    }
}

# ── layout ───────────────────────────────────────────────────────────
# Pack order is load-bearing (pitfall 18): bottom bar first, then top
# banner; the middle is ONE panedwindow (.mid) whose children are the
# three dockable regions — sidebar (left), chatwrap (findbar + chat),
# panes (right, docked on demand). Sashes = mouse-resize; C-x s / C-x p
# undock sidebar/panes; Alt-←/→ walks focus between regions.
wm title . "veles"
if {![::vandroid::window]} {
    wm geometry . 1080x720
    wm minsize . 760 480
}

::vstatus::build            ;# .statusbar claims the bottom strip first
# Our own window controls, ABOVE the banner. Packed unconditionally so
# the native title bar can be switched off at any time (palette →
# Toggle native title bars) without the window becoming unmovable.
# Not on a phone, where the window is the screen.
if {![::vandroid::on]} {
    ::vtitle::attach . "veles" -close { ::vdock::save_layout ; destroy . }
}
::vbrand::build             ;# .brand — synthwave banner across the top
pack .brand -side top -fill x

::vdock::build              ;# .mid panedwindow — the resizable middle
ttk::frame .mid.chatwrap    ;# container for findbar + chat (a pane child)
::vsidebar::build           ;# .sidebar (root frame; the pane manages it)
::vfind::build              ;# .mid.chatwrap.findbar (hidden until Ctrl-F)
::vchat::build              ;# .mid.chatwrap.chat
::vpanes::build             ;# .panes — right shell (Files/Review/Preview/Term), revealed on demand

::virc::boot                ;# .ircbufs/.ircview/.ircroster — the IRC tab

::vinstpane::boot           ;# .instlist/.instdetail/.instctl — Instances

::vdock::dock .sidebar
::vdock::dock .mid.chatwrap

# Tabs are saved LAYOUTS (emacs tab-bar), so they take over managing
# .mid from here — which is why they boot after the regions exist and
# after the first dock, not before.
::vtabs::boot

# The app-wide attach decision (PLAN Part 6). Runs AFTER tabs: with a
# remembered instance it re-attaches (teardown is a no-op at boot) and
# the last active tab stands; unattached it selects the Instances tab.
# This is the ONLY place ::vapp::boot is called.
::vapp::boot

# ── shortcuts ────────────────────────────────────────────────────────
# Key binding modes (normie/emacs/vi) are owned by lib/keymap.tcl —
# switching mode re-binds everything. M-x (Alt-x) opens the palette in
# every mode. The `break` in the bind scripts stops the Text class
# bindings from also acting (Ctrl-K deletes to EOL in a text widget).
# The saved native-title-bar choice, applied now that every window that
# carries its own strip exists (set_native skips the ones that do not).
catch { ::vtitle::restore_native }

::vkeymap::boot
# The coach reads its tally from the state blob, so it boots AFTER the
# keymap (it needs a mode to resolve chords in) and after ::vapi is up.
catch { ::vcoach::init }
::vedit::install          ;# readline editing, history, completion, C-g/C-h
::vedit::hist_load

proc ::vsidebar::select_rel {delta} {
    set R $::vsidebar::R
    set n [$R.list size]
    if {$n == 0} return
    set sel [$R.list curselection]
    set cur [expr {$sel eq "" ? 0 : [lindex $sel 0]}]
    set next [expr {($cur + $delta + $n) % $n}]
    $R.list selection clear 0 end
    $R.list selection set $next
    $R.list see $next
    ::vsidebar::on_select
}

# ── boot ─────────────────────────────────────────────────────────────
::vhotpatch::init
::vhotpatch::load
# First-run setup wizard (PLAN-setup-wizard-tk.md): fires ONLY when the
# engine says config.toml is absent (/api/setup/state first_run) and this
# desktop has no setup_done marker. An operator with a hand-rolled config
# never sees it. Runs after the map so the wizard centres on a real
# window.
#
# NOT gated on being attached. A fresh `veles gui` on a first-run box
# boots UNATTACHED (no registry entry, no VELES_API) yet has the loopback
# gateway it just spawned — ::vapi falls back to base 127.0.0.1:8642, and
# /api/setup/state answers from it, which is exactly when the wizard must
# open. maybe_first_run catches a failed fetch and returns 0, so a
# desktop with no gateway anywhere stays quiet rather than erroring; the
# wizard never fires against a gateway it cannot reach.
after idle {
    catch { ::vsetup::boot }
}
# Coach every affordance that exists by now — the composer's
# Send/Stop/Attach, the title-bar controls, the IRC strip, the
# instance pane. Overlays coach themselves through
# ::voverlay::_header; panes do it in ::vpanes::show.
after idle { catch { ::vcoach::attach_all . } }
# Subscribe to the agent-eval queue. Listening is NOT consent: the
# arm switch in the Dev pane is, and ::vdev::_eval_event checks it
# on every request. Being subscribed while disarmed is what lets
# the gateway answer "switch it on in the Dev pane" instead of
# leaving an agent waiting for a window it cannot see.
after idle { catch { ::vdev::eval_listen } }
# Offer this window's capabilities to the agent's `device` tool: a phone
# registers its borg providers, a laptop its desktop ones, and the
# stream reconnects by itself when the gateway comes and goes.
after idle {
    catch { ::vdevice::android::register }
    catch { ::vdevice::desktop::register }
    catch { ::vapps::register }
    catch { ::vdevice::start }
}
::vsidebar::load_pins
::vsidebar::refresh
::vstatus::poll
::vpanes::poll_start
::vdock::load_layout
# A phone: the composer on its own row, rows of buttons that wrap, the
# regions stacked in portrait with the session list a drawer; then what
# a finger, the Back key and Android's app life cycle need.
::vandroid::layout
::vandroid::install

# Persist layout on exit and whenever a sash moves.
wm protocol . WM_DELETE_WINDOW {
    ::vdock::save_layout
    destroy .
}
# Save periodically too (lib/dock.tcl: a crash shouldn't lose the layout).
::vdock::_layout_save_loop

# hermes-desktop remembers the active session across restarts; ours
# opens the most recent one (or the remembered one when the session
# list is still there). Unattached: the empty-state stands; the attach
# path's rebuild opens the new gateway's freshest session.
after 600 {
    if {[::vapi::unattached]} { focus .mid.chatwrap.chat.input ; return }
    if {[catch {::vapi::get_json /api/sessions} resp]} {
        focus .mid.chatwrap.chat.input
    } else {
        set join [::vinst::remote_join]
        if {[dict size $join] && [dict exists $join instance] && [dict exists $join target_session]
            && [dict get $join instance] eq [::vapi::attached]} {
            # Revalidate the exact target binding and its current ACL;
            # reopening a session id alone is not a restored join.
            ::vinst::restore_remote_join
        } elseif {[llength [dict get $resp sessions]]} {
            # session_meta is ordered updated_at DESC → first is freshest
            set latest [dict get [lindex [dict get $resp sessions] 0] id]
            catch {::vchat::open_session $latest}
        }
        focus .mid.chatwrap.chat.input
    }
}

# Opt-in smoke test: VELES_DESKTOP_SELFTEST=<message> opens a fresh
# session and sends that message once (full loop without a human).
if {[info exists env(VELES_LAYOUT_PROBE)] && $env(VELES_LAYOUT_PROBE) eq "1"} {
    source [file join $_script_dir lib layout_probe.tcl]
}
if {[info exists env(VELES_PRINT_WINDOW_ID)] && $env(VELES_PRINT_WINDOW_ID) eq "1"} {
    after 1500 { puts "WINDOW_ID=[winfo id .]" }
}
if {[info exists env(VELES_DESKTOP_SELFTEST)] && $env(VELES_DESKTOP_SELFTEST) ne ""} {
    set selftest_msg $env(VELES_DESKTOP_SELFTEST)
    after 800 {
        ::vsidebar::new_session
        after 400 {
            .mid.chatwrap.chat.input insert insert $selftest_msg
            ::vchat::send_current
        }
    }
}

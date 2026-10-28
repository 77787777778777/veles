# veles-desktop/lib/attach.tcl — attach: point the WHOLE client at one
# gateway (Part 6 of PLAN-instances-pane.md).
#
# THE MODEL, settled with the user 2026-08-26:
#
#   The client is attached to exactly zero or one instances. Every
#   fetch outside the Instances pane goes to the attached instance, or
#   nowhere if unattached. A region's data always belongs to the
#   current attach epoch; anything older is discarded on arrival.
#
# Attaching to B while on A is the SAME operation as attaching from
# unattached, with a teardown phase in between. There is deliberately
# no fast path — a "quick switch" that skips steps is how cross-gateway
# data leaks in.
#
# THE TEARDOWN CHECKLIST (one proc, one place to audit):
#
#   1. close BOTH SSE streams (chat + IRC) — they capture url+token at
#      open time, so a stream left open keeps talking to the OLD
#      gateway silently;
#   2. reset ::virc::cursor to 0 — line ids are per-store; the old
#      cursor silently skips or re-renders lines against a different
#      gateway;
#   3. drop the chat session list and clear the transcript;
#   4. clear the Files tree, git panes and preview;
#   5. close Dev buffers and POST /api/lsp/stop on the OUTGOING
#      gateway — inside with_instance, because `current` has not
#      flipped yet and nothing else guarantees where that POST lands;
#   6. bump the epoch (in-flight pane callbacks discard themselves);
#   7. only now flip ::vapi::use_instance, persist last_attached, and
#      rebuild every visible region.
#
# WHAT DOES NOT CHANGE ON ATTACH: ::vstate (theme, sashes, tab
# layouts) stays local to the desktop and is NOT re-read from the new
# gateway's blob. Changing the window's appearance because attach
# changed is astonishing. If per-instance UI state is ever wanted, key
# it by instance name explicitly and document it.
#
# SOURCING ORDER: after api.tcl, before the panes that must no-op when
# unattached (they test ::vapi::unattached, which needs no sourcing
# order).

namespace eval ::vapp {
    variable epoch 0        ;# attach epoch; async callbacks capture it
                            ;# and discard themselves if it moved
    variable booted 0
}

proc ::vapp::epoch {} {
    variable epoch
    return $epoch
}

proc ::vapp::fresh {e} {
    variable epoch
    return [expr {$e == $epoch}]
}

# ── the teardown checklist ──────────────────────────────────────────
#
# Runs BEFORE use_instance flips the target, against the OUTGOING
# instance. ONE proc so there is exactly one place to audit; each step
# is numbered in a comment matching the plan. `steps` (optional,
# caller-side) receives each step name as it fires, so the test suite
# can assert the ORDER — LSP stop before use_instance, streams before
# cursor, and so on.

proc ::vapp::teardown_for_attach {{steps ""}} {
    # 1. Close BOTH SSE streams. vlsp's stream too — its _stop does the
    #    LSP POST as well, so it is invoked here rather than duplicated.
    if {[llength [info commands ::vapi::chat_stop]]} {
        catch { ::vapi::chat_stop } stopped_streams
    }
    if {[llength [info commands ::virc::disconnect]]} {
        catch { ::virc::disconnect } irc_closed
    }
    # The LSP stop POSTs to /api/lsp/stop, and it MUST land on the
    # OUTGOING gateway — otherwise two language servers run and the new
    # one's diagnostics arrive for files the old one indexed.
    #
    # STEP ORDER IS THE ONLY THING THAT GUARANTEES THAT, and it is
    # enough: this runs before use_instance flips `current`. The plan
    # asked for a belt-and-braces `with_instance [::vapi::attached]`
    # around it; that was written and then removed, because
    # ::vapi::attached RETURNS `current` — so `with_instance
    # [attached]` sets current to current. A literal no-op, untestable
    # by construction, and worse than nothing: it reads like a
    # guarantee that is really just the ordering restated.
    #
    # What pins it instead is an assertion in insttest that the stop
    # resolved to the OUTGOING gateway's url. Move this step below
    # use_instance and that goes red.
    if {[llength [info commands ::vlsp::_stop]]} {
        catch { ::vlsp::_stop "" } lsp_stopped
    }
    if {$steps ne ""} { uplevel 1 [list $steps streams] }

    # 2. Reset the IRC cursor. Line ids are per-store.
    if {[llength [info commands ::virc::reset_cursor]]} {
        catch { ::virc::reset_cursor } cursor_reset
    }
    if {$steps ne ""} { uplevel 1 [list $steps cursor] }

    # 3. Drop the chat session list and clear the transcript.
    if {[llength [info commands ::vchat::detach_clear]]} {
        catch { ::vchat::detach_clear } chat_cleared
    }
    if {[llength [info commands ::vsidebar::detach_clear]]} {
        catch { ::vsidebar::detach_clear } sb_cleared
    }
    if {$steps ne ""} { uplevel 1 [list $steps chat] }

    # 4. Clear the Files tree, git panes and preview.
    if {[llength [info commands ::vpanes::detach_clear]]} {
        catch { ::vpanes::detach_clear } panes_cleared
    }
    if {$steps ne ""} { uplevel 1 [list $steps panes] }

    # 5. Dev buffers closed; the LSP stop POST went to the OUTGOING
    #    gateway in step 1 (vlsp::_stop posts it synchronously-ish via
    #    post_async). Buffers here.
    if {[llength [info commands ::vstatus::detach_async]]} {
        catch { ::vstatus::detach_async } status_cleared
    }
    if {[llength [info commands ::voverlay::detach_async]]} {
        catch { ::voverlay::detach_async } overlays_cleared
    }
    if {[llength [info commands ::vdev::close_all]]} {
        catch { ::vdev::close_all } devs_closed
    }
    if {$steps ne ""} { uplevel 1 [list $steps dev] }

    # 6. The epoch bump. Everything async still in flight for the old
    #    world captures an epoch that no longer matches and drops
    #    itself on arrival.
    _bump_epoch
    if {$steps ne ""} { uplevel 1 [list $steps epoch] }
}

proc ::vapp::_bump_epoch {} {
    variable epoch
    incr epoch
    return $epoch
}

# ── attach / detach ─────────────────────────────────────────────────

# Attach the whole client to `name`. Returns 1 on success.
proc ::vapp::attach {name} {
    if {[catch {
        # Bind from the registry ONLY when not already bound: the boot
        # path binds deliberately (an explicit VELES_API/VELES_TOKEN
        # overrides a saved instance's url/token, and that override
        # must survive the boot attach).
        if {$name ne "" && ![::vapi::instance_bound $name]} {
            set spec [::vinst::get $name]
            if {$spec eq ""} {
                error "no saved instance named \"$name\""
            }
            set url "" ; set tok ""
            catch { set url [dict get $spec url] }
            catch { set tok [dict get $spec token] }
            ::vapi::bind_instance_inheriting $name $url $tok
        }
    } bind_err]} {
        ::vstatus::set_msg [format [::vi18n::mc {attach: %s}] $bind_err]
        return 0
    }
    teardown_for_attach
    # 7. Only now flip the target.
    if {[catch { ::vapi::use_instance $name } flip_err]} {
        ::vstatus::set_msg [format [::vi18n::mc {attach: %s}] $flip_err]
        return 0
    }
    if {$name ne ""} {
        ::vinst::set_last_attached $name
        set url [::vapi::instance_url $name]
        if {$url eq ""} {
            catch { set url [dict get [::vinst::get $name] url] } url_miss
        }
        ::vstatus::set_attachment [dict create name $name url $url]
        rebuild_regions
        # The transcript's empty state is attach-state-dependent; the
        # teardown above painted the "not attached" wording, so repaint
        # it NOW that the flip has happened.
        if {[llength [info commands ::vchat::paint_empty_state]]} {
            catch { ::vchat::paint_empty_state }
        }
        ::vstatus::set_msg [format [::vi18n::mc {attached to %s}] $name]
    } else {
        ::vinst::set_last_attached ""
        ::vstatus::set_attachment ""
        ::vstatus::set_msg [::vi18n::mc {detached}]
    }
    return 1
}

# Detach: full checklist, flip to "", select the Instances tab. The
# other tabs keep their frames and show their unattached empty-state.
proc ::vapp::detach {} {
    return [attach ""]
}

# Rebuild every visible region against the (new) current instance:
# first fetches for the panes that need them. ::vstate is deliberately
# NOT re-read — the window's appearance does not change on attach.
proc ::vapp::rebuild_regions {} {
    if {[::vapi::unattached]} { return }
    if {[llength [info commands ::virc::reconnect]]} {
        catch { ::virc::reconnect }
    }
    if {[llength [info commands ::vsidebar::refresh]]} {
        catch { ::vsidebar::refresh }
    }
    if {[llength [info commands ::vpanes::refresh_files]]} {
        catch { ::vpanes::refresh_files }
    }
    if {[llength [info commands ::vpanes::refresh_review]]} {
        catch { ::vpanes::refresh_review }
    }
    if {[llength [info commands ::vstatus::poll_now]]} {
        catch { ::vstatus::poll_now }
    }
}

# ── boot ────────────────────────────────────────────────────────────
#
# SPLIT, because the boot attach must happen in two places:
#
#   boot_decide  runs EARLY (main.tcl, before the layout): decides
#                which instance this process is attached to and flips
#                ::vapi. No UI exists yet, so the teardown half of the
#                checklist is genuinely a no-op — nothing has been
#                fetched, no stream is open. The one thing it must do
#                even now is bump the epoch, so anything that slips in
#                a request before the layout is up is discarded.
#
#   boot         runs LATE (main.tcl, after tabs): selects the tab an
#                unattached boot lands on and paints the attachment in
#                the status bar. A boot WITH an instance keeps
#                today's behaviour — the last active tab stands.
#
# Precedence: a boot-time env binding (VELES_INSTANCE without an
# explicit VELES_API) > last_attached > unattached.

proc ::vapp::boot_decide {} {
    variable booted
    # An env path above may already have attached this process
    # (VELES_API → synthetic `env`; VELES_INSTANCE → its name). That is
    # the most deliberate choice available; honour it and stop.
    if {[::vapi::attached] ne ""} {
        _bump_epoch
        return [::vapi::attached]
    }
    set want ""
    if {[info exists ::env(VELES_INSTANCE)] && $::env(VELES_INSTANCE) ne ""
        && [::vapi::instance_bound $::env(VELES_INSTANCE)]} {
        set want $::env(VELES_INSTANCE)
    }
    if {$want eq "" && ![info exists ::env(VELES_INSTANCE)]} {
        set join [::vinst::remote_join]
        if {[dict size $join] && [dict exists $join instance]
            && [::vapi::instance_bound [dict get $join instance]]} {
            # A saved join is a target attachment, not the origin's last
            # tab. An explicit VELES_INSTANCE above still takes priority.
            set want [dict get $join instance]
        }
    }
    if {$want eq ""} {
        set want [::vinst::last_attached]
        # last_attached is a NAME in the registry file; nothing binds
        # registry names into ::vapi at boot except the env paths, so
        # bind it here — found by the live boot probe: the remembered
        # instance was silently discarded and every boot came up
        # unattached.
        if {$want ne "" && ![::vapi::instance_bound $want]} {
            set spec [::vinst::get $want]
            if {$spec ne ""} {
                set u "" ; set t ""
                catch { set u [dict get $spec url] }
                catch { set t [dict get $spec token] }
                ::vapi::bind_instance_inheriting $want $u $t
            }
        }
        if {$want ne "" && ![::vapi::instance_bound $want]} { set want "" }
    }
    _bump_epoch
    if {$want eq ""} {
        # Nothing bound and no env pointer: a genuinely unattached
        # boot. `base` still holds the loopback default, but
        # `unattached` is what every request guard tests.
        ::vapi::use_instance ""
        return ""
    }
    ::vapi::use_instance $want
    return $want
}

proc ::vapp::boot {} {
    variable booted
    set booted 1
    if {[::vapi::attached] eq ""} {
        # Unattached boot: the Instances tab is the default surface.
        catch { ::vtabs::select instances }
    }
    # Paint the attachment (or the unattached marker) now that the bar
    # exists; ::vstatus::set_attachment is what ::vapp::attach calls
    # too, so the two paths cannot drift apart in wording.
    set n [::vapi::attached]
    if {$n eq ""} {
        ::vstatus::set_attachment ""
        if {[llength [info commands ::vchat::paint_empty_state]]} {
            catch { ::vchat::paint_empty_state }
        }
    } else {
        ::vstatus::set_attachment [dict create name $n \
            url [::vapi::instance_url $n]]
        if {[llength [info commands ::vchat::paint_empty_state]]} {
            catch { ::vchat::paint_empty_state }
        }
    }
    return $n
}

# The rule, for anyone reading the code instead of the plan:
# every fetch outside the Instances pane goes to the attached instance,
# or nowhere if unattached.
proc ::vapp::attached_pane_guard {who} {
    if {![::vapi::unattached]} { return 0 }
    ::vstatus::set_msg [format \
        [::vi18n::mc {%s — not attached; press Attach in the Instances tab}] $who]
    return 1
}

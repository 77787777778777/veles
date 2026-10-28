# Boot the REAL desktop against the REAL gateway, send a prompt, and
# report what the transcript actually contains. [info script] stays
# correct for main.tcl when sourced from here.
set ::bgerrors {}
proc bgerror {msg} { lappend ::bgerrors $msg }
source /home/user/veles-agent/veles-desktop/main.tcl

set W .mid.chatwrap.chat.log

proc dump {} {
    set w $::W
    puts "----- TRANSCRIPT -----"
    puts [$w get 1.0 end-1c]
    puts "----- TAGS OF INTEREST -----"
    foreach t {md_link md_bold md_code mdsource assistant user tool} {
        set r ""
        catch { set r [$w tag ranges $t] }
        puts [format "  %-10s ranges=%s" $t $r]
    }
    puts "  md_link -font:       [catch {$w tag cget md_link -font} v; set v]"
    puts "  md_link -foreground: [catch {$w tag cget md_link -foreground} v; set v]"
    set n 0
    foreach t [$w tag names] { if {[string match "mdlink_*" $t]} { incr n } }
    puts "  per-link tags:       $n"
    puts "----- unparsed markdown left in the text? -----"
    set ln 0
    foreach line [split [$w get 1.0 end-1c] "\n"] {
        incr ln
        if {[string match {*](http*} $line]} {
            puts "  UNPARSED line $ln: $line"
        }
    }
    puts "----- md_link covered text -----"
    foreach {a b} [$w tag ranges md_link] { puts "  '[$w get $a $b]'" }
    puts "  source_mode:         [expr {[info exists ::vchat::source_mode] ? $::vchat::source_mode : "unset"}]"
    puts "----- STATUS BAR -----"
    catch { puts "  msg: '[$::vstatus::W.msg cget -text]'" }
    foreach child [winfo children .statusbar] {
        catch { puts "  [winfo name $child]: '[$child cget -text]'" }
    }
}

after 3000 {
    puts "SESSION: $::vchat::session"
    ::vsidebar::new_session
    after 1200 {
        puts "SESSION AFTER NEW: $::vchat::session"
        catch { ::vchat::refresh_route }
        update
        puts "STATUS BEFORE PROMPT:"
        foreach child [winfo children .statusbar] {
            catch { puts "  [winfo name $child]: '[$child cget -text]'" }
        }
        .mid.chatwrap.chat.input insert insert "give me a link"
        ::vchat::send_current
        after 6000 {
            dump
            set w $::W
            puts "----- READ-ONLY TRANSCRIPT -----"
            puts "  -state:            [$w cget -state]"
            puts "  readonly bindtag:  [::vtext::is_readonly $w]"
            puts "  -insertbackground: [$w cget -insertbackground]"
            puts "  -insertwidth:      [$w cget -insertwidth]"
            puts "  takefocus:         [$w cget -takefocus]"
            set before [$w get 1.0 end-1c]
            focus -force $w
            update
            event generate $w <KeyPress-x> -state 0
            update
            puts "  typing 'x' changed the text: [expr {[$w get 1.0 end-1c] ne $before}]"
            puts "  keymap mode:       $::vkeymap::current"
            catch { ::vkeymap::use emacs }
            update
            puts "  after switch:      $::vkeymap::current"
            $w mark set insert 5.0
            focus -force $w
            update
            event generate $w <Control-space>
            update
            event generate $w <Control-f>
            event generate $w <Control-f>
            event generate $w <Control-f>
            update
            puts "  C-space then C-f x3 -> sel: '[$w tag ranges sel]'"
            puts "  sel -background:   [$w tag cget sel -background]"
            puts "  sel -foreground:   [$w tag cget sel -foreground]"
            puts "  widget -selectbackground: [$w cget -selectbackground]"
            puts "  widget -inactiveselectbackground: [$w cget -inactiveselectbackground]"
            puts "  widget has focus:  [expr {[focus] eq \"$w\"}]  (focus=[focus])"
            puts "  theme select_bg:   $::vt::T(select_bg)"
            puts "  theme bg_input:    $::vt::T(bg_input)"
            puts "  theme name:        [expr {[info exists ::vt::current] ? $::vt::current : "?"}]"
            puts "  sel tag priority:  [lsearch [$w tag names] sel] of [llength [$w tag names]] tags"
            puts "  tags above sel:    [lrange [$w tag names] [expr {[lsearch [$w tag names] sel]+1}] end]"
            catch { puts "  region text: '[$w get sel.first sel.last]'" }
            # Reopen the session: engine notes persisted during the
            # turn must come back, at the point they happened.
            puts "----- RELOAD -----"
            set sid $::vchat::session
            ::vchat::open_session $sid
            update idletasks
            after 900 {
                set txt [$::W get 1.0 end-1c]
                puts "  --- full reloaded transcript ---"
                foreach line [split $txt "\n"] {
                    puts "    | [string range $line 0 88]"
                }
                puts "  --- note tag ranges: [$::W tag ranges note] ---"
                puts "BGERRORS: [llength $::bgerrors]"
                foreach e $::bgerrors { puts "  ! $e" }
                puts "E2E_DONE"
                exit 0
            }
            return
        }
    }
}

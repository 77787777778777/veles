# Extension-first development: exact previews, supported scaffolds and
# the gateway's live API as the reference. Nothing is created until the
# author confirms the preview; publishing is a separate explicit action.
#
# AI assistance works on four levels (dev-ext-10, owner ask 287976.2):
# a design question with no buffer, a change to the open file, a change
# set across the files of its extension, and an explanation of the
# current diagnostics; the New extension dialog adds a whole extension
# generated from a description. Every level is one direct, tool-free
# request the gateway scans for credentials first; the two that change
# files return a reviewed proposal that only an explicit apply writes.
# Model calls take tens of seconds, so they are asynchronous here, with
# a model's timeout rather than the 10 s gateway default.
namespace eval ::vdev {
    variable extension_preview {}
    variable extension_created {}
    variable extension_generated {}
    variable assist_preview {}
    variable assist_widget ""
    variable assist_path ""
    variable assist_instruction ""
    variable assist_apply_allowed 0
    variable assist_mode file
    variable assist_seq 0
    variable generate_seq 0
    # How long a model may take to answer, in ms.
    variable assist_timeout 300000
}

proc ::vdev::_assist_invalidate {w} {
    if {![winfo exists $w]} { return }
    set ::vdev::assist_preview {}
    set ::vdev::assist_instruction ""
    set ::vdev::assist_apply_allowed 0
    catch { $w.bar.apply state disabled }
}

proc ::vdev::_assist_request_edited {} {
    if {![winfo exists .vdev_extension_assist] ||
        ![.vdev_extension_assist.request edit modified]} { return }
    .vdev_extension_assist.request edit modified 0
    _assist_invalidate .vdev_extension_assist
}

proc ::vdev::extension_assist {} {
    set path [editor_path]
    set have_buffer [expr {$path ne "" && [winfo exists [editor_widget]]}]
    set w .vdev_extension_assist
    if {[winfo exists $w]} { destroy $w }
    ::vmanage::window $w [::vi18n::mc {AI extension assistance}] 880x680 {560 440}
    set ::vdev::assist_preview {}
    set ::vdev::assist_apply_allowed 0
    set ::vdev::assist_widget [expr {$have_buffer ? [editor_widget] : ""}]
    set ::vdev::assist_path [expr {$have_buffer ? $path : ""}]
    set ::vdev::assist_mode [expr {$have_buffer ? "file" : "ask"}]
    if {$have_buffer} {
        set where "[::vi18n::mc {Extension source}]: $path"
    } else {
        set where [::vi18n::mc {No file is open. Ask a design question, or open an extension file for help with its code.}]
    }
    ttk::label $w.path -wraplength 820 -justify left -text $where
    pack $w.path -fill x -padx 10 -pady 5
    ttk::frame $w.modes
    pack $w.modes -fill x -padx 10
    foreach {mode label} [list \
        file [::vi18n::mc {Change this file}] \
        extension [::vi18n::mc {Change this extension's files}] \
        explain [::vi18n::mc {Explain the diagnostics}] \
        ask [::vi18n::mc {Ask a design question}]] {
        ttk::radiobutton $w.modes.$mode -text $label -value $mode \
            -variable ::vdev::assist_mode -command ::vdev::_assist_mode_changed
        pack $w.modes.$mode -side left -padx {0 10}
        if {!$have_buffer && $mode ne "ask"} { $w.modes.$mode state disabled }
    }
    ttk::label $w.instructions -wraplength 820 -justify left
    pack $w.instructions -anchor w -padx 10 -pady {6 0}
    text $w.request -height 5 -wrap word -undo 1 \
        -background $::vt::T(bg_input) -foreground $::vt::T(fg)
    pack $w.request -fill x -padx 10 -pady 6
    $w.request edit modified 0
    bind $w.request <<Modified>> {+::vdev::_assist_request_edited}
    ::vmanage::button $w.bar preview [::vi18n::mc {Preview proposal}] \
        ::vdev::_extension_assist_preview
    set apply [::vmanage::button $w.bar apply [::vi18n::mc {Apply proposal…}] \
        ::vdev::_extension_assist_apply]
    $apply state disabled
    ttk::label $w.bar.route -text [::vi18n::mc {No proposal has been requested. Nothing will be written automatically.}]
    pack $w.bar.route -side left -fill x -expand 1
    text $w.diff -wrap none -background $::vt::T(bg_input) -foreground $::vt::T(fg)
    pack $w.diff -fill both -expand 1 -padx 10 -pady 8
    ::vtext::readonly $w.diff
    _assist_mode_changed
    focus $w.request
    bind $w <Escape> [list destroy $w]
}

# What each level says it will do, and what its button is called.
proc ::vdev::_assist_mode_changed {} {
    set w .vdev_extension_assist
    if {![winfo exists $w]} { return }
    _assist_invalidate $w
    switch -- $::vdev::assist_mode {
        extension {
            set text [::vi18n::mc {Describe the change. The AI may change or add several files of this extension. Every file is shown as a diff, and nothing is written before you confirm.}]
            set label [::vi18n::mc {Preview change set}]
        }
        explain {
            set text [::vi18n::mc {The current diagnostics (from the language server or Check) are sent with this file. Optionally add a question. The answer is text only; nothing can be written.}]
            set label [::vi18n::mc {Explain}]
        }
        ask {
            set text [::vi18n::mc {Ask how to build an extension: which boundary, which language, which API. No source is sent, and nothing can be written.}]
            set label [::vi18n::mc {Ask}]
        }
        default {
            set text [::vi18n::mc {Describe the change. The AI may propose a replacement, but cannot write it before you inspect and confirm the diff.}]
            set label [::vi18n::mc {Preview proposal}]
        }
    }
    $w.instructions configure -text $text
    $w.bar.preview configure -text $label
}

# The diagnostics of the buffer the dialog was opened on, as
# `line:col SEVERITY message` lines; "" when there are none.
proc ::vdev::_assist_diagnostics {} {
    set origin $::vdev::assist_widget
    if {$origin eq "" || ![info exists ::vdev::editor_diagnostics($origin)]} { return "" }
    set lines {}
    foreach row $::vdev::editor_diagnostics($origin) {
        lassign $row line col severity message
        lappend lines "$line:$col $severity $message"
    }
    return [join $lines "\n"]
}

proc ::vdev::_extension_assist_preview {} {
    variable assist_seq
    variable assist_timeout
    set w .vdev_extension_assist
    if {![winfo exists $w]} { return }
    set instruction [string trim [$w.request get 1.0 end-1c]]
    set mode $::vdev::assist_mode
    if {$instruction eq "" && $mode ne "explain"} {
        ::vmanage::say $w [expr {$mode eq "ask"
            ? [::vi18n::mc {Write the design question first.}]
            : [::vi18n::mc {Describe the requested extension change first.}]}] warn
        return
    }
    set path $::vdev::assist_path
    set content ""
    if {$::vdev::assist_widget ne "" && [winfo exists $::vdev::assist_widget]} {
        set content [$::vdev::assist_widget get 1.0 end-1c]
    }
    set ::vdev::assist_preview {}
    set ::vdev::assist_apply_allowed 0
    $w.bar.apply state disabled
    set diagnostics [_assist_diagnostics]
    switch -- $mode {
        ask {
            set endpoint /api/extensions/assist/ask
            set pairs [list question [::vapi::json_string $instruction]]
            if {$path ne ""} { lappend pairs path [::vapi::json_string $path] }
        }
        explain {
            if {$diagnostics eq ""} {
                ::vmanage::say $w [::vi18n::mc {This buffer has no diagnostics to explain. Run Check, or wait for the language server.}] warn
                return
            }
            if {$instruction ne ""} {
                append diagnostics "\n\n[::vi18n::mc {The author asks}]: $instruction"
            }
            set endpoint /api/extensions/assist/explain
            set pairs [list path [::vapi::json_string $path] \
                content [::vapi::json_string $content] \
                diagnostics [::vapi::json_string $diagnostics]]
        }
        extension {
            set endpoint /api/extensions/assist/propose
            set pairs [list path [::vapi::json_string $path] \
                instruction [::vapi::json_string $instruction] \
                diagnostics [::vapi::json_string $diagnostics]]
        }
        default {
            if {$diagnostics eq ""} {
                set diagnostics [::vi18n::mc {No diagnostics reported by the current LSP document.}]
            }
            set endpoint /api/extensions/assist/preview
            set pairs [list path [::vapi::json_string $path] \
                content [::vapi::json_string $content] \
                diagnostics [::vapi::json_string $diagnostics] \
                instruction [::vapi::json_string $instruction]]
        }
    }
    set seq [incr assist_seq]
    $w.bar.route configure -text [::vi18n::mc {Waiting for the model…}]
    ::vapi::post_json_async $endpoint [::vapi::json_object $pairs] \
        [list ::vdev::_assist_failed $seq $instruction] \
        [list ::vdev::_assist_answered $seq $mode $instruction] $assist_timeout
}

proc ::vdev::_assist_failed {seq instruction why} {
    set w .vdev_extension_assist
    if {$seq != $::vdev::assist_seq || ![winfo exists $w]} { return }
    $w.bar.route configure -text [::vi18n::mc {No proposal. Nothing was written.}]
    if {[_assist_offer_extension $w $why $instruction]} { return }
    set shown $why
    if {[regexp {^HTTP \d+: (.*)$} $why -> body] &&
        ![catch { ::json::json2dict $body } reply] && [dict exists $reply error]} {
        set shown [dict get $reply error]
    }
    ::vmanage::say $w [::vmanage::_reason $shown] warn
}

proc ::vdev::_assist_answered {seq mode instruction response} {
    set w .vdev_extension_assist
    if {$seq != $::vdev::assist_seq || ![winfo exists $w]} { return }
    if {[dict exists $response error]} {
        ::vmanage::say $w [dict get $response error] warn
        return
    }
    set route "[dict get $response model] @ [dict get $response provider]"
    if {$mode in {ask explain}} {
        # An answer: there is nothing to apply.
        $w.bar.route configure -text "$route - [::vi18n::mc {answer only, nothing to apply}]"
        ::vtext::set_text $w.diff [dict get $response answer]
        return
    }
    set ::vdev::assist_preview $response
    set ::vdev::assist_instruction $instruction
    set ::vdev::assist_apply_allowed 1
    $w.bar.apply state !disabled
    $w.bar.route configure -text "$route - [::vi18n::mc {proposal only, not applied}]"
    set text "[::vi18n::mc {Extension API}]: [dict get $response api]\n"
    append text "[::vi18n::mc {Checks run}]: [dict get $response checks_run]\n"
    if {$mode eq "extension"} {
        set files {}
        foreach f [dict get $response files] {
            lappend files "[dict get $f path][expr {[dict get $f created] ? " ([::vi18n::mc {new}])" : ""}]"
        }
        append text "[::vi18n::mc {Files}]: [join $files {, }]\n"
        if {[dict exists $response summary] && [dict get $response summary] ne ""} {
            append text "[::vi18n::mc {Summary}]: [dict get $response summary]\n"
        }
    } else {
        append text "[::vi18n::mc {Recommended checks}]: [dict get $response recommended_checks]\n"
    }
    append text "\n" [dict get $response diff]
    ::vtext::set_text $w.diff $text
}

# Extension first (dev-ext-10). AI assistance edits user extensions only;
# for a core or repository file the gateway's refusal carries
# draft_as_extension, and the change is offered as a new extension (with
# the request as its purpose) instead of a bare refusal. Returns 1 when
# the refusal was handled here.
proc ::vdev::_assist_offer_extension {w err instruction} {
    if {![regexp {^HTTP \d+: (.*)$} $err -> body]} { return 0 }
    if {[catch { ::json::json2dict $body } reply] ||
        ![dict exists $reply draft_as_extension] ||
        ![string is true -strict [dict get $reply draft_as_extension]]} { return 0 }
    if {![::vmanage::confirm $w [::vi18n::mc {Draft this as an extension?}] \
        [::vi18n::mc {AI assistance edits user extensions only, and this file is not part of one. Most changes fit an extension (a hook, tool, service override or provider driver) that survives upgrades; a core patch is for what the extension API cannot express. Create a new extension for this change instead? Its files are previewed before anything is written.}]]} {
        ::vmanage::say $w [::vmanage::_reason $err] warn
        return 1
    }
    destroy $w
    ::vdev::new_extension
    set ::vdev_new_purpose [string range [lindex [split $instruction \n] 0] 0 159]
    ::vdev::_extension_preview
    return 1
}

proc ::vdev::_extension_assist_apply {} {
    set w .vdev_extension_assist
    if {![winfo exists $w] || ![llength $::vdev::assist_preview] ||
        !$::vdev::assist_apply_allowed} { return }
    set proposal $::vdev::assist_preview
    if {[dict exists $proposal operation] && [dict get $proposal operation] eq "changeset"} {
        _extension_assist_apply_set $w $proposal
        return
    }
    set path [dict get $proposal path]
    if {![::vmanage::confirm $w [::vi18n::mc {Apply this AI proposal?}] \
        [::vi18n::mc {Only the reviewed extension buffer will be replaced. The file is still protected against changes made after this preview. No commit, push or PR happens.}]]} {
        return
    }
    set origin $::vdev::assist_widget
    if {[string trim [$w.request get 1.0 end-1c]] ne $::vdev::assist_instruction} {
        _assist_invalidate $w
        return
    }
    if {![winfo exists $origin] || [editor_widget] ne $origin ||
        [editor_path] ne $path ||
        [$origin get 1.0 end-1c] ne [dict get $proposal before]} {
        set ::vdev::assist_preview {}
        set ::vdev::assist_apply_allowed 0
        $w.bar.apply state disabled
        ::vmanage::say $w [::vi18n::mc {The source buffer changed. Request a new proposal before applying.}] warn
        return
    }
    set body [::vapi::json_object [list \
        proposal_id [::vapi::json_string [dict get $proposal proposal_id]]]]
    if {[catch {::vapi::post_json /api/extensions/assist/apply $body} response]} {
        ::vmanage::say $w [::vmanage::_reason $response] warn
        return
    }
    if {[dict exists $response error]} {
        set ::vdev::assist_apply_allowed 0
        $w.bar.apply state disabled
        ::vmanage::say $w "[dict get $response error] [::vi18n::mc {Request a new preview before trying again.}]" warn
        return
    }
    if {[editor_path] eq $path && [winfo exists [editor_widget]]} {
        ::vdev::_editor_fill [editor_widget] [dict get $proposal after]
        if {[dict exists $response sha256]} {
            set ::vdev::editor_sha([editor_widget]) [dict get $response sha256]
        }
    }
    set ::vdev::assist_preview {}
    set ::vdev::assist_apply_allowed 0
    $w.bar.apply state disabled
    ::vmanage::say $w [::vi18n::mc {The reviewed proposal was applied locally. Review and test it before publishing.}] ok
}

# A reviewed change set: every file or none, revalidated by the gateway.
# An open buffer on one of the files must be unchanged since the preview
# (its unsaved edits would otherwise be overwritten), and is refreshed
# from what was written.
proc ::vdev::_extension_assist_apply_set {w proposal} {
    set files [dict get $proposal files]
    if {[string trim [$w.request get 1.0 end-1c]] ne $::vdev::assist_instruction} {
        _assist_invalidate $w
        return
    }
    set origin $::vdev::assist_widget
    if {$origin ne "" && [winfo exists $origin] && [info exists ::vdev::editor_dirty($origin)] &&
        $::vdev::editor_dirty($origin)} {
        ::vmanage::say $w [::vi18n::mc {Save or revert the open buffer before applying a change set.}] warn
        return
    }
    if {![::vmanage::confirm $w [::vi18n::mc {Apply this change set?}] \
        [format [::vi18n::mc {%d file(s) of the extension will be written exactly as reviewed. If any of them changed since the preview, nothing is written. No commit, push or PR happens.}] [llength $files]]]} {
        return
    }
    set body [::vapi::json_object [list \
        proposal_id [::vapi::json_string [dict get $proposal proposal_id]]]]
    if {[catch {::vapi::post_json /api/extensions/assist/apply $body} response]} {
        set shown $response
        if {[regexp {^HTTP \d+: (.*)$} $response -> raw] &&
            ![catch { ::json::json2dict $raw } reply] && [dict exists $reply error]} {
            set shown [dict get $reply error]
        }
        set ::vdev::assist_apply_allowed 0
        $w.bar.apply state disabled
        ::vmanage::say $w "[::vmanage::_reason $shown] [::vi18n::mc {Request a new preview before trying again.}]" warn
        return
    }
    if {[dict exists $response error]} {
        set ::vdev::assist_apply_allowed 0
        $w.bar.apply state disabled
        ::vmanage::say $w "[dict get $response error] [::vi18n::mc {Request a new preview before trying again.}]" warn
        return
    }
    set here [editor_path]
    if {$here ne "" && [winfo exists [editor_widget]] && [dict exists $response files]} {
        foreach f [dict get $response files] {
            set same [expr {[dict get $f path] eq $here}]
            if {[llength [info commands ::vdev::_same_path]]} {
                set same [::vdev::_same_path [dict get $f path] $here]
            }
            if {$same} {
                ::vdev::_editor_fill [editor_widget] [dict get $f content]
                set ::vdev::editor_sha([editor_widget]) [dict get $f sha256]
            }
        }
    }
    set ::vdev::assist_preview {}
    set ::vdev::assist_apply_allowed 0
    $w.bar.apply state disabled
    ::vmanage::say $w [format [::vi18n::mc {The reviewed change set was applied (%d files). Review and test it before publishing.}] [llength $files]] ok
}

proc ::vdev::new_extension {} {
    set w .vdev_new_extension
    if {[winfo exists $w]} { destroy $w }
    ::vmanage::window $w [::vi18n::mc {New extension}] 780x650 {560 480}
    set ::vdev_new_name ""
    set ::vdev_new_purpose ""
    set ::vdev_new_kind tcl
    set ::vdev_new_api 0.11
    if {![catch {::vapi::get_json /api/plugins/api} response]} {
        catch { set ::vdev_new_api [dict get $response version] }
    }
    ttk::frame $w.form
    pack $w.form -fill x -padx 12 -pady 6
    ttk::label $w.form.nl -text [::vi18n::mc {Name}]
    ttk::entry $w.form.name -textvariable ::vdev_new_name
    ttk::label $w.form.pl -text [::vi18n::mc {Purpose}]
    ttk::entry $w.form.purpose -textvariable ::vdev_new_purpose
    ttk::label $w.form.kl -text [::vi18n::mc {Language}]
    ttk::combobox $w.form.kind -state readonly -textvariable ::vdev_new_kind \
        -values {tcl js racket lisp}
    ttk::label $w.form.al -text [::vi18n::mc {Extension API}]
    ttk::label $w.form.api -textvariable ::vdev_new_api
    grid $w.form.nl $w.form.name -sticky ew -padx 4 -pady 4
    grid $w.form.pl $w.form.purpose -sticky ew -padx 4 -pady 4
    grid $w.form.kl $w.form.kind -sticky ew -padx 4 -pady 4
    grid $w.form.al $w.form.api -sticky ew -padx 4 -pady 4
    grid columnconfigure $w.form 1 -weight 1
    set ::vdev::extension_generated {}
    ::vmanage::button $w.bar preview [::vi18n::mc {Preview files}] ::vdev::_extension_preview
    ::vmanage::button $w.bar generate [::vi18n::mc {Generate with AI…}] ::vdev::_extension_generate \
        [::vi18n::mc {write the whole extension from its purpose, as a reviewed preview}]
    ::vmanage::button $w.bar create [::vi18n::mc {Create extension}] ::vdev::_extension_create
    ::vmanage::button $w.bar guide [::vi18n::mc {Extension guide}] ::vdev::extension_guide
    ::vmanage::button $w.bar plugins [::vi18n::mc {Manage extensions}] {
        ::voverlay::open plugins
    }
    text $w.preview -wrap word -background $::vt::T(bg_input) -foreground $::vt::T(fg)
    pack $w.preview -fill both -expand 1 -padx 12 -pady 8
    ::vtext::readonly $w.preview
    bind $w <Return> {+::vdev::_extension_preview}
    focus $w.form.name
    _extension_preview
}

proc ::vdev::_extension_form {} {
    return [dict create \
        name [string trim $::vdev_new_name] \
        description [string trim $::vdev_new_purpose] \
        kind $::vdev_new_kind]
}

proc ::vdev::_extension_preview {} {
    set w .vdev_new_extension
    if {![winfo exists $w]} { return }
    set form [_extension_form]
    if {[dict get $form name] eq ""} {
        ::vtext::set_text $w.preview [::vi18n::mc {Choose a name, purpose and language, then preview the exact files. Nothing has been written.}]
        return
    }
    if {[catch {::vapi::post_json /api/plugins/preview $form} response]} {
        ::vtext::set_text $w.preview [::vmanage::_reason $response]
        return
    }
    if {[dict exists $response error]} {
        ::vtext::set_text $w.preview [dict get $response error]
        return
    }
    set ::vdev::extension_preview $response
    set text "[::vi18n::mc {No files have been written. Review the exact manifest and source, then confirm creation.}]\n\n"
    foreach file [dict get $response files] {
        append text "── [dict get $file name] ──\n[dict get $file text]\n"
    }
    ::vtext::set_text $w.preview $text
}

# Generate with AI: the scaffold for this name and language, completed
# by the model from the purpose, shown as the exact files. Create then
# writes THOSE files (one explicit, single-use apply), or nothing.
proc ::vdev::_extension_generate {} {
    variable generate_seq
    variable assist_timeout
    set w .vdev_new_extension
    if {![winfo exists $w]} { return }
    set form [_extension_form]
    if {[dict get $form name] eq "" || [dict get $form description] eq ""} {
        ::vmanage::say $w [::vi18n::mc {Give the extension a name and describe what it should do (the purpose), then generate.}] warn
        return
    }
    set ::vdev::extension_generated {}
    set seq [incr generate_seq]
    ::vtext::set_text $w.preview [::vi18n::mc {Waiting for the model… Nothing has been written.}]
    ::vapi::post_json_async /api/extensions/assist/generate [::vapi::json_object [list \
        name [::vapi::json_string [dict get $form name]] \
        kind [::vapi::json_string [dict get $form kind]] \
        description [::vapi::json_string [dict get $form description]]]] \
        [list ::vdev::_extension_generate_failed $seq] \
        [list ::vdev::_extension_generated $seq $form] $assist_timeout
}

proc ::vdev::_extension_generate_failed {seq why} {
    set w .vdev_new_extension
    if {$seq != $::vdev::generate_seq || ![winfo exists $w]} { return }
    set shown $why
    if {[regexp {^HTTP \d+: (.*)$} $why -> body] &&
        ![catch { ::json::json2dict $body } reply] && [dict exists $reply error]} {
        set shown [dict get $reply error]
    }
    ::vtext::set_text $w.preview "[::vi18n::mc {Nothing was generated or written.}]\n\n$shown"
}

proc ::vdev::_extension_generated {seq form response} {
    set w .vdev_new_extension
    if {$seq != $::vdev::generate_seq || ![winfo exists $w]} { return }
    if {[dict exists $response error]} {
        _extension_generate_failed $seq [dict get $response error]
        return
    }
    dict set response form $form
    set ::vdev::extension_generated $response
    set text "[format [::vi18n::mc {Generated by %s - nothing has been written. Review every file, then Create extension writes exactly these.}] "[dict get $response model] @ [dict get $response provider]"]\n"
    if {[dict exists $response summary] && [dict get $response summary] ne ""} {
        append text "\n[dict get $response summary]\n"
    }
    foreach file [dict get $response files] {
        append text "\n── [dict get $file path] ──\n[dict get $file after]\n"
    }
    ::vtext::set_text $w.preview $text
}

proc ::vdev::_extension_create {} {
    set w .vdev_new_extension
    if {![winfo exists $w]} { return }
    set form [_extension_form]
    set generated $::vdev::extension_generated
    if {[llength $generated]} {
        if {[dict get $generated form] ne $form} {
            # The name, language or purpose changed after generating.
            set ::vdev::extension_generated {}
            ::vmanage::say $w [::vi18n::mc {The form changed since the extension was generated. Generate again, or preview the plain scaffold.}] warn
            return
        }
        _extension_create_generated $w $generated
        return
    }
    if {![llength $::vdev::extension_preview]} {
        _extension_preview
        return
    }
    set expected [expr {[dict get $form description] eq "" ? "a new veles plugin" : [dict get $form description]}]
    if {[dict get $::vdev::extension_preview name] ne [dict get $form name] ||
        [dict get $::vdev::extension_preview kind] ne [dict get $form kind] ||
        ![string match "*$expected*" [.vdev_new_extension.preview get 1.0 end-1c]]} {
        ::vmanage::say $w [::vi18n::mc {Preview the changed name or language before creating it.}] warn
        return
    }
    if {![::vmanage::confirm $w [::vi18n::mc {Create this extension?}] \
        [::vi18n::mc {The reviewed files will be written to your user plugin directory. Veles saves a local recovery snapshot and attempts to reload the extension; publishing is not part of this action.}]]} {
        return
    }
    set response [::vmanage::post $w /api/plugins/new $form]
    if {![llength $response]} { return }
    set ::vdev::extension_created $response
    if {[dict exists $response path]} {
        ::voverlay::_open_in_dev [dict get $response path]
    }
    if {[dict exists $response reload_error]} {
        ::vmanage::say $w "[::vi18n::mc {Created, but reload failed}]: [dict get $response reload_error]" warn
    } else {
        ::vmanage::say $w [::vi18n::mc {Created and reloaded. Review and test the extension before publishing.}] ok
    }
}

proc ::vdev::_extension_create_generated {w generated} {
    set files [dict get $generated files]
    if {![::vmanage::confirm $w [::vi18n::mc {Create this generated extension?}] \
        [format [::vi18n::mc {The %d reviewed file(s) will be written to a new directory in your user plugin directory, and veles attempts to load the extension. If a directory of that name appeared since the preview, nothing is written. Publishing is not part of this action.}] [llength $files]]]} {
        return
    }
    set body [::vapi::json_object [list \
        proposal_id [::vapi::json_string [dict get $generated proposal_id]]]]
    if {[catch {::vapi::post_json /api/extensions/assist/apply $body} response]} {
        set ::vdev::extension_generated {}
        ::vmanage::say $w [::vmanage::_reason $response] warn
        return
    }
    set ::vdev::extension_generated {}
    if {[dict exists $response error]} {
        ::vmanage::say $w [dict get $response error] warn
        return
    }
    set ::vdev::extension_created $response
    # Open the manifest's entry, else the first file written.
    set entry ""
    foreach f [dict get $generated files] {
        if {[dict get $f path] eq "plugin.toml"} {
            regexp {entry\s*=\s*"([^"]+)"} [dict get $f after] -> entry
        }
    }
    set open ""
    foreach f [dict get $response files] {
        if {$open eq "" || [dict get $f relative] eq $entry} { set open [dict get $f path] }
    }
    if {$open ne ""} { catch { ::voverlay::_open_in_dev $open } }
    if {[dict exists $response reload_error]} {
        ::vmanage::say $w "[::vi18n::mc {Created, but reload failed}]: [dict get $response reload_error]" warn
    } else {
        ::vmanage::say $w [::vi18n::mc {Created and reloaded. Review and test the extension before publishing.}] ok
    }
}

proc ::vdev::extension_doc {which} {
    set root [file normalize [file join $::vdev::lib_dir .. .. docs]]
    set file [expr {$which eq "api" ? "extending.md" : "plugins.md"}]
    set path [file join $root $file]
    if {[file exists $path]} {
        ::voverlay::_open_in_dev $path
    } else {
        ::vstatus::set_msg [format [::vi18n::mc {documentation is not in this source checkout: %s}] $path]
    }
}

proc ::vdev::_guide_paragraph {w text} {
    $w insert end "$text\n\n"
}

# Example code, tagged `snippet` so it reads as code and devtest can source
# every example exactly as shown. Each one creates its namespace first:
# `proc ::my_plugin::x` before `namespace eval ::my_plugin` fails with
# "unknown namespace" in the gateway's interpreter (dev-ext-7).
proc ::vdev::_guide_snippet {w code} {
    $w insert end $code snippet "\n\n"
}

proc ::vdev::extension_guide {} {
    set w .vdev_extension_guide
    if {[winfo exists $w]} { destroy $w }
    ::vmanage::window $w [::vi18n::mc {Extension development}] 850x620 {540 380}
    ::vmanage::button $w.bar new [::vi18n::mc {New extension}] {
        destroy .vdev_extension_guide
        ::vdev::new_extension
    }
    ::vmanage::button $w.bar plugins [::vi18n::mc {Manage extensions}] {::voverlay::open plugins}
    ::vmanage::button $w.bar author [::vi18n::mc {Author guide}] {::vdev::extension_doc author}
    ::vmanage::button $w.bar api [::vi18n::mc {API reference}] {::vdev::extension_doc api}
    text $w.body -wrap word -background $::vt::T(bg_input) -foreground $::vt::T(fg)
    pack $w.body -fill both -expand 1 -padx 10 -pady 8
    $w.body tag configure snippet -font TkFixedFont
    _guide_paragraph $w.body [::vi18n::mc {Start with an extension in your user plugin directory. It survives upgrades and does not require a gateway rebuild. New extension previews the exact manifest and source before writing. Choose the supported language and API version, review the files, then test the extension in isolation.}]
    _guide_paragraph $w.body [::vi18n::mc {Choose the narrowest boundary: a Tool adds a model-callable capability; a Hook observes or amends a lifecycle event; a Service or override point wraps a named decision; a Provider driver adds a provider wire; a Core patch is appropriate only when the published extension API cannot express the change.}]
    _guide_paragraph $w.body [::vi18n::mc {The editor opens gateway files, including remote instances, and runs the language server for the file's language on the gateway: tcl-lsp, typescript-language-server (TypeScript and JavaScript), rust-analyzer (a Rust plugin crate or module), clangd (C), racket-langserver and a Common Lisp server. Hover, go to definition, completion and live diagnostics work for every language the gateway has a server for; when it has none, the editor says which program to install there and keeps syntax highlighting. Check runs the matching checker on the gateway: tcl-lsp, tsc --noEmit, node --check, cargo check, clang -fsyntax-only, or the Racket and Lisp readers. Save, reload in Manage extensions, and test the extension without publishing local state.}]
    _guide_paragraph $w.body [::vi18n::mc {Use the Author guide and API reference buttons for the actual contracts. The Plugins overlay lists live hooks; services and override points come from the gateway's own registry, not a copied list in this guide.}]
    _guide_paragraph $w.body [::vi18n::mc {Example: a hook}]
    _guide_snippet $w.body {namespace eval ::my_plugin {}
proc ::my_plugin::after_call {} {
    # set args $::veles::hook_args
    return ""
}
::veles::hook post_llm_call ::my_plugin::after_call}
    _guide_paragraph $w.body [::vi18n::mc {Example: a tool}]
    _guide_snippet $w.body {namespace eval ::my_plugin {}
proc ::my_plugin::hello {} {
    set who [::veles::json_get $::veles::tool_args who]
    return "hello, $who"
}
::veles::register_tool my_plugin_hello {Say hello} \
    {{"type":"object","properties":{"who":{"type":"string"}}}} \
    ::my_plugin::hello}
    _guide_paragraph $w.body [::vi18n::mc {Example: a service override wrapping core}]
    _guide_snippet $w.body {namespace eval ::my_plugin {}
proc ::my_plugin::service {} {
    # Read this request before any nested service call.
    set request $::veles::service_request
    set next [::veles::json_get $request next]
    set params [::veles::json_get $request params]
    # Delegate unchanged input; keep the returned decision's schema.
    return [::veles::service_next $next $params]
}
::veles::register_service failover.decide ::my_plugin::service}
    _guide_paragraph $w.body [::vi18n::mc {Example: a provider driver}]
    _guide_snippet $w.body {namespace eval ::my_plugin {}
proc ::my_plugin::provider {} {
    # Translate this OpenAI-shaped request to your provider's wire.
    # Return an OpenAI-shaped response or {error, status}.
    set request $::veles::provider_request
    return $request
}
::veles::register_provider my_provider ::my_plugin::provider}
    _guide_paragraph $w.body [::vi18n::mc {AI assist works on four levels: ask a design question (no file needed), change this file, change several files of this extension, or explain the current diagnostics. New extension can also generate a whole extension from its purpose. Every request is sent without tools and is refused when it carries a registered secret or a credential-shaped value; changes are shown as diffs and written only when you apply them. Ask for an extension design first; request a core patch only after identifying the missing API.}]
    _guide_paragraph $w.body [::vi18n::mc {Upstream contribution is an explicit publishing action, not an automatic upload. In Manage extensions, Contribute lists every user extension: select the extensions and files, inspect the diff and secrets scan, run checks, then create one branch and one PR (to veles by default) with your chosen git identity. Each extension goes under its own directory in the repository, and an existing file there is replaced only after a separate confirmation. Never publish node.toml, local credentials, tokens, private endpoints or unrelated plugin state. Local recovery snapshots are not an upstream commit or a PR.}]
    ::vtext::readonly $w.body
    catch { ::va11y::name $w.body [::vi18n::mc {extension development guide}] }
    bind $w <Escape> [list destroy $w]
}

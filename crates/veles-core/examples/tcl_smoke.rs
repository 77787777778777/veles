//! Smoke test: bidirectional Rust↔Tcl bridge.
//! Hook contract under test (current): handlers take NO args; the core
//! delivers the JSON payload in ::veles::hook_args (set via interp.set,
//! so JSON braces survive Tcl word-quoting intact).
//! Run: cargo run --example tcl_smoke

fn main() {
    let home = std::env::temp_dir().join("veles-smoke");
    let _ = std::fs::create_dir_all(&home);

    let h = veles_core::tclrt::spawn(home).expect("tcl spawn");

    // 1. engine facts synced into Tcl
    let v = h.eval("set ::veles_version").expect("channel");
    println!("1) Tcl sees veles_version = {v:?}");
    assert_eq!(v, veles_core::VERSION);

    // 2. Tcl→Rust direction: register a hook handler from Tcl...
    h.eval(
        r#"
        proc my_on_session_start {} {
            return "handled: $::veles::hook_args"
        }
        ::veles::hook on_session_start my_on_session_start
        "#,
    )
    .expect("channel");

    // 3. Rust→Tcl direction: the real transport — TclRequest::Hook.
    // The core sets ::veles::hook_args via interp.set (a Tcl object, no
    // parser pass), so JSON braces survive; then dispatches by name.
    let (tx, rx) = tokio::sync::oneshot::channel();
    h.send(veles_core::tclrt::TclRequest::Hook {
        name: "on_session_start".into(),
        args_json: r#"{"text":"hello"}"#.into(),
        reply: tx,
    })
    .expect("send");
    let r = rx.blocking_recv().expect("recv").expect("hook ok");
    println!("2) hook dispatch result    = {r:?}");
    assert_eq!(r, Some(r#"handled: {"text":"hello"}"#.to_string()));

    // 4. unregistered hook returns empty
    let (tx, rx) = tokio::sync::oneshot::channel();
    h.send(veles_core::tclrt::TclRequest::Hook {
        name: "no_such_hook".into(),
        args_json: "{}".into(),
        reply: tx,
    })
    .expect("send");
    let r = rx.blocking_recv().expect("recv").expect("hook ok");
    println!("3) unregistered hook      = {r:?}");
    assert_eq!(r, None);

    // 5. introspection
    let r = h.eval("::veles::hooks_list").expect("channel");
    println!("4) hooks_list               = {r:?}");

    println!("\nALL SMOKE TESTS PASSED");
}

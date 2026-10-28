//! Plugin loop smoke test — M3 proof, run against the REAL home
//! (`config_dir()`, ~/.config/veles) so the installed reasoning-summary plugin loads and its
//! native bridge (veles::call_llm) works.
//!
//! Requires: plugins/reasoning-summary installed under ~/.config/veles/plugins.
//! Run: cargo run --example plugin_loop

use veles_core::tclrt::{self, TclRequest};

fn main() {
    let home = veles_core::config::veles_home();
    println!("home: {}", home.display());
    let h = tclrt::spawn(home).expect("tcl spawn");

    // 1. native bridge commands registered
    let cmds = h.eval("info commands ::veles::*").expect("channel");
    println!("1) native commands: {cmds}");
    for c in ["::veles::call_llm", "::veles::routes", "::veles::json_get"] {
        assert!(cmds.contains(c), "missing native command {c}");
    }

    // 2. plugin loaded and hooked
    let hooks = h.eval("::veles::hooks_list").expect("channel");
    println!("2) hooks: {hooks}");
    assert!(
        hooks.contains("post_llm_call"),
        "reasoning-summary did not register post_llm_call"
    );

    // 3. json_get works (the plugin depends on it).
    // NOTE: JSON travels in double quotes here — a bare brace-word would
    // have Tcl strip the outer braces. The real hook path avoids quoting
    // entirely via the ::veles::hook_args variable transport.
    let v = h
        .eval(r#"::veles::json_get "{\"a\":{\"b\":\"hello {world}\"}}" a b"#)
        .expect("channel");
    println!("3) json_get nested: {v:?}");
    assert_eq!(v, "hello {world}");

    // 4. THRESHOLD: short reasoning → plugin returns "" (leave alone)
    let (tx, rx) = tokio::sync::oneshot::channel();
    h.send(TclRequest::Hook {
        name: "post_llm_call".into(),
        args_json: serde_json::json!({
            "role": "assistant",
            "content": "hi",
            "reasoning": "just one line of thought",
            "provider": "ali-token-plan-personal",
            "model": "deepseek-v4-flash-0731"
        })
        .to_string(),
        reply: tx,
    })
    .expect("send");
    let r = rx.blocking_recv().expect("recv").expect("hook ok");
    println!("4) short reasoning → {r:?}");
    assert_eq!(r, None, "short reasoning must NOT be summarized");

    // 5. LIVE: long reasoning → real aux call → display_reasoning JSON
    let long_reasoning: String = (1..=20)
        .map(|i| format!("step {i}: considering option {i} and its tradeoffs"))
        .collect::<Vec<_>>()
        .join("\n");
    let (tx, rx) = tokio::sync::oneshot::channel();
    h.send(TclRequest::Hook {
        name: "post_llm_call".into(),
        args_json: serde_json::json!({
            "role": "assistant",
            "content": "the answer",
            "reasoning": long_reasoning,
            "provider": "ali-token-plan-personal",
            "model": "deepseek-v4-flash-0731"
        })
        .to_string(),
        reply: tx,
    })
    .expect("send");
    let r = rx.blocking_recv().expect("recv").expect("hook ok");
    println!("5) long reasoning → {r:?}");
    let r = r.expect("long reasoning must be summarized");
    let v: serde_json::Value = serde_json::from_str(&r).expect("handler must return valid JSON");
    let summary = v["display_reasoning"]
        .as_str()
        .expect("display_reasoning field");
    assert!(!summary.is_empty());
    assert!(
        summary.len() < long_reasoning.len(),
        "summary should be shorter"
    );
    println!("   summary: {summary}");

    println!("\nPLUGIN LOOP PASSED (native bridge + real aux LLM call)");
}

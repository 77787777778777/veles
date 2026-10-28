// A test/example driver: it spawns onto a scratch runtime it owns;
// the M3 hop (clippy.toml) is for production core/frontends code.
#![allow(clippy::disallowed_methods)]

//! Desktop API durable smoke test — offline REST-layer verification.
//!
//! Spins up the real DesktopServer (axum) against a scratch store on a
//! test port and drives every endpoint except the streaming LLM turn:
//!   0. the API mints a bearer token when none is configured, enforces it
//!      (401 without), and the minted one is accepted
//!   1. /api/health
//!   2. POST /api/sessions/new → session row exists in the store
//!   3. GET /api/sessions lists it
//!   4. GET /api/sessions/{id}/history returns persisted messages
//!   5. POST /api/chat with a bogus session → HTTP 400, no turn runs
//!   6. DELETE /api/sessions/{id} removes it
//!   7. /api/status · /api/config · /api/usage
//!   8. /api/search?q= (FTS snippet path)
//!   9. /api/sessions/:id/title rename
//!  10. /api/sessions/:id/override deferred → store row
//!  11. /api/desktop/state round trip
//!  12. /api/skills · /api/memory · /api/plugins · /api/cron shapes
//!
//! The streaming chat path needs a live model; it is covered by the
//! ad-hoc live verification (curl SSE + Tk selftest) rather than here.
//!
//! Run: cargo run --example desktop_api_smoke

use std::sync::{Arc, Mutex};
use veles_core::config::VelesConfig;
use veles_core::hooks::HookBus;
use veles_core::providers::ChatMessage;
use veles_core::store::Store;
use veles_frontends::desktop_api::{default_port, desktop_enabled, DesktopServer};

#[tokio::main]
async fn main() {
    // Free-ish test port, resolved through the same env path the server uses.
    std::env::set_var("VELES_DESKTOP_PORT", "18642");

    // Scratch VELES_HOME: skills/memory/plugins/discovery must not touch
    // the user's real veles dirs, and the state blob test needs isolation.
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("VELES_HOME", dir.path());

    let store = Arc::new(Mutex::new(
        Store::open(&dir.path().join("state.db")).expect("open"),
    ));
    let cfg = Arc::new(VelesConfig::default());
    assert!(desktop_enabled(&cfg), "desktop on by default");
    assert_eq!(default_port(&cfg), 18642, "env port override wins");
    let hooks = HookBus::new(None);

    // Seed one session with a user+assistant pair so history/search have rows.
    let seeded = {
        let s = store.lock().unwrap();
        s.create_session("seed-001").unwrap();
        s.append_message("seed-001", &ChatMessage::user("hello seed"))
            .unwrap();
        s.append_message("seed-001", &ChatMessage::assistant("hi from seed", None))
            .unwrap();
        assert!(s.session_exists("seed-001").unwrap());
        assert!(!s.session_exists("ghost").unwrap());
        "seed-001".to_string()
    };

    let srv = Arc::new(DesktopServer::new(cfg, store.clone(), hooks));
    tokio::spawn({
        let srv = srv.clone();
        async move {
            if let Err(e) = veles_frontends::desktop_api::serve(srv).await {
                eprintln!("serve error: {e:#}");
            }
        }
    });
    // Let the listener bind.
    let base = "http://127.0.0.1:18642";
    let probe = reqwest::Client::new();
    let mut up = false;
    for _ in 0..50 {
        if probe.get(format!("{base}/api/health")).send().await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(up, "desktop API never came up");

    // Nothing was configured, so `serve` MINTED a token into the scratch
    // home's `.env` on the way up — every route below is behind it. Read
    // through the same helper a client uses, which also proves the minted
    // credential is the one the server resolved.
    let token = veles_core::authn::desktop_bearer_token();
    assert!(
        !token.is_empty(),
        "serve() must mint a token when none is configured"
    );
    let unauth = probe
        .get(format!("{base}/api/sessions"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unauth.status(),
        401,
        "a token that is minted but not enforced is worse than none"
    );
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();
    println!("server up on {base} (bearer, {} hex chars)", token.len());

    // 1) health
    let h: serde_json::Value = client
        .get(format!("{base}/api/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(h["status"], "ok");
    assert!(h["uptime_s"].is_u64());
    println!("1) /api/health ✓");

    // 2) new session
    let r = client
        .post(format!("{base}/api/sessions/new"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: serde_json::Value = r.json().await.unwrap();
    let sid = body["session_id"].as_str().unwrap().to_string();
    assert!(
        store.lock().unwrap().session_exists(&sid).unwrap(),
        "row persisted"
    );
    println!("2) POST /api/sessions/new ✓ ({sid})");

    // 3) list
    let l: serde_json::Value = client
        .get(format!("{base}/api/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<String> = l["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().into())
        .collect();
    assert!(ids.contains(&sid));
    assert!(ids.contains(&seeded));
    println!("3) GET /api/sessions ✓ ({} sessions)", ids.len());

    // 4) history of the seeded session
    let hist: serde_json::Value = client
        .get(format!("{base}/api/sessions/{seeded}/history"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let msgs = hist["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "hello seed");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"], "hi from seed");
    println!("4) GET /api/sessions/{{id}}/history ✓");

    // 5) chat against a bogus session → 400, nothing runs
    let r = client
        .post(format!("{base}/api/chat"))
        .json(&serde_json::json!({"session_id": "ghost", "text": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let e: serde_json::Value = r.json().await.unwrap();
    assert!(e["error"].as_str().unwrap().contains("not found"));
    println!("5) POST /api/chat bogus session → 400 ✓");

    // 6) delete
    let r = client
        .delete(format!("{base}/api/sessions/{sid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        !store.lock().unwrap().session_exists(&sid).unwrap(),
        "row gone"
    );
    println!("6) DELETE /api/sessions/{{id}} ✓");

    // 7) status/config/usage shapes
    let st: serde_json::Value = client
        .get(format!("{base}/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st["status"], "ok");
    assert!(st["session_count"].is_u64());
    assert!(st["platforms"].is_array());
    let cf: serde_json::Value = client
        .get(format!("{base}/api/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(cf.get("default_model").is_some());
    assert!(cf["aliases"].is_array());
    assert!(cf["providers"].is_array());
    assert!(cf["memory_limits"]["memory"].as_u64().unwrap() > 0);
    let us: serde_json::Value = client
        .get(format!("{base}/api/usage"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(us["prompt_tokens"].is_u64());
    println!("7) /api/status + /api/config + /api/usage ✓");

    // 8) search finds the seeded assistant reply (FTS)
    let sr: serde_json::Value = client
        .get(format!("{base}/api/search?q=seed&limit=5"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let hits = sr["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "FTS should find 'seed'");
    assert_eq!(hits[0]["session_id"], seeded);
    println!("8) GET /api/search ✓ ({} hits)", hits.len());

    // 9) title rename
    let r = client
        .post(format!("{base}/api/sessions/{seeded}/title"))
        .json(&serde_json::json!({"title": "renamed seed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        store.lock().unwrap().title(&seeded).unwrap().as_deref(),
        Some("renamed seed")
    );
    println!("9) POST /api/sessions/{{id}}/title ✓");

    // 10) override: deferred (no cached agent) → state blob
    let r = client
        .post(format!("{base}/api/sessions/{seeded}/override"))
        .json(&serde_json::json!({"model": "test-model"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["applied"], "deferred");
    // The override lands in the STORE row, not in desktop_state.json: the
    // `override:<sid>` blob key is a one-time MIGRATION source now (R4.5),
    // read once when a session is opened and left in place. Asserting the
    // blob here passed while the write went somewhere else, and then
    // failed for good when the desktop stopped writing it — the assertion
    // is on the row the server actually reads.
    let row = store
        .lock()
        .unwrap()
        .override_get(&seeded)
        .unwrap()
        .expect("the deferred override must land in the session row");
    assert_eq!(row.model.as_deref(), Some("test-model"));
    println!("10) POST /api/sessions/{{id}}/override deferred ✓");

    // 11) desktop state round trip (blob merge-safe write)
    let r = client
        .post(format!("{base}/api/desktop/state"))
        .json(&serde_json::json!({"pins": ["a", "b"], "theme": "dark"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let blob2: serde_json::Value = client
        .get(format!("{base}/api/desktop/state"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(blob2["pins"], serde_json::json!(["a", "b"]));
    assert_eq!(blob2["theme"], "dark");
    println!("11) /api/desktop/state round trip ✓");

    // 12) skills/memory/plugins/cron shapes (scratch home → empty but valid)
    let sk: serde_json::Value = client
        .get(format!("{base}/api/skills"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(sk["skills"].is_array());
    let mm: serde_json::Value = client
        .get(format!("{base}/api/memory"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(mm["memory"].is_array());
    assert!(mm["user"].is_array());
    assert!(mm["limits"]["memory"].as_u64().unwrap() > 0);
    let pl: serde_json::Value = client
        .get(format!("{base}/api/plugins"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(pl["plugins"].is_array());
    let cr: serde_json::Value = client
        .get(format!("{base}/api/cron"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(cr["jobs"].is_array());
    println!("12) /api/skills + /api/memory + /api/plugins + /api/cron ✓");

    println!("desktop_api_smoke: all offline REST checks passed");
}

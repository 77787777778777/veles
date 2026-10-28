//! approval_smoke — the M11 gate, end to end, WITHOUT spending tokens.
//!
//! Covers the paths a live test can't reach cheaply: every mode, both
//! standing-rule tiers, the non-interactive policy, and the shape of the
//! message a denied call hands back to the model. `smart` is exercised
//! with the aux screen deliberately unreachable (no API key), which is
//! the "screen unavailable → ask the human" fallback.
//!
//! Run: cargo run -p veles-core --example approval_smoke

use serde_json::json;
use std::sync::Arc;
use veles_core::agent::Agent;
use veles_core::approval::Mode;
use veles_core::ask::{
    AskAnswer, AskKind, AskRequest, Asker, CHOICE_ALWAYS, CHOICE_DENY, CHOICE_ONCE, CHOICE_SESSION,
};
use veles_core::config::{ModelSection, ProviderEntry, VelesConfig};
use veles_core::hooks::HookBus;
use veles_core::store::Store;
use veles_core::tools;

/// Answers with a canned decision and records what it was asked.
struct ScriptedAsker {
    answer: String,
    seen: Arc<std::sync::Mutex<Vec<AskRequest>>>,
}

#[async_trait::async_trait]
impl Asker for ScriptedAsker {
    async fn ask(&self, req: AskRequest) -> AskAnswer {
        self.seen.lock().unwrap().push(req);
        AskAnswer::choice(&self.answer)
    }
}

fn cfg(mode: &str, non_interactive: &str) -> Arc<VelesConfig> {
    let mut c = VelesConfig {
        model: ModelSection {
            provider: Some("custom:scratch".into()),
            default: Some("test-model".into()),
            api_mode: None,
            binds: Vec::new(),
        },
        ..Default::default()
    };
    // A route must exist for Agent::new; nothing here ever calls it in
    // manual/off mode, and in smart mode the failure IS the test.
    c.custom_providers.push(ProviderEntry {
        name: "scratch".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        key_env: Some("VELES_APPROVAL_SMOKE_KEY".into()),
        api_mode: "chat_completions".into(),
        model: Some("test-model".into()),
        ..Default::default()
    });
    c.approvals.mode = mode.into();
    c.approvals.non_interactive = non_interactive.into();
    c.approvals.timeout_secs = 5;
    Arc::new(c)
}

fn agent(
    cfg: Arc<VelesConfig>,
    store: Arc<std::sync::Mutex<Store>>,
    asker: Option<Arc<dyn Asker>>,
) -> Agent {
    let sid = format!("approval-smoke-{}", std::process::id());
    store.lock().unwrap().create_session(&sid).unwrap();
    let registry = Arc::new(tools::base_registry(store.clone(), &sid));
    let a = Agent::new(cfg, HookBus::new(None), registry, store, sid).unwrap();
    match asker {
        Some(k) => a.with_asker(k),
        None => a,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("veles-approval-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let store = Arc::new(std::sync::Mutex::new(Store::open(&dir.join("state.db"))?));

    let risky = json!({"command": "rm -rf /tmp/veles-scratch"});
    let boring = json!({"command": "cargo build --release"});
    let mut passed = 0;

    // 1. mode=off: nothing is gated, not even the risky call.
    {
        let a = agent(cfg("off", "deny"), store.clone(), None);
        assert!(a.approve_call("terminal", &risky, None).await.is_none());
        assert_eq!(Mode::parse("off"), Mode::Off);
        println!("1. off  → risky call runs ungated ✓");
        passed += 1;
    }

    // 2. mode=manual + deny: the model gets an explicit refusal it is
    //    told not to route around.
    {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let a = agent(
            cfg("manual", "deny"),
            store.clone(),
            Some(Arc::new(ScriptedAsker {
                answer: CHOICE_DENY.into(),
                seen: seen.clone(),
            })),
        );
        let out = a
            .approve_call("terminal", &risky, None)
            .await
            .expect("denied");
        assert!(out.starts_with("error:"), "denial is a tool error: {out}");
        assert!(out.contains("Do not retry"));
        {
            let asked = seen.lock().unwrap();
            assert_eq!(asked.len(), 1);
            assert_eq!(asked[0].kind, AskKind::Approval);
            assert!(asked[0].detail.as_deref().unwrap().contains("rm -rf"));
        }
        // Ordinary work is never even asked about.
        assert!(a.approve_call("terminal", &boring, None).await.is_none());
        assert_eq!(seen.lock().unwrap().len(), 1, "boring call was not gated");
        println!("2. manual + deny → tool error, prompt carries the command ✓");
        passed += 1;
    }

    // 3. "allow once" runs it, and asks again next time.
    {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let a = agent(
            cfg("manual", "deny"),
            store.clone(),
            Some(Arc::new(ScriptedAsker {
                answer: CHOICE_ONCE.into(),
                seen: seen.clone(),
            })),
        );
        assert!(a.approve_call("terminal", &risky, None).await.is_none());
        assert!(a.approve_call("terminal", &risky, None).await.is_none());
        assert_eq!(seen.lock().unwrap().len(), 2, "once does not stick");
        println!("3. once → allowed, asked again ✓");
        passed += 1;
    }

    // 4. "session" sticks for this agent only.
    {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let a = agent(
            cfg("manual", "deny"),
            store.clone(),
            Some(Arc::new(ScriptedAsker {
                answer: CHOICE_SESSION.into(),
                seen: seen.clone(),
            })),
        );
        assert!(a.approve_call("terminal", &risky, None).await.is_none());
        assert!(a.approve_call("terminal", &risky, None).await.is_none());
        assert_eq!(seen.lock().unwrap().len(), 1, "session rule reused");
        // A different session starts clean.
        let seen2 = Arc::new(std::sync::Mutex::new(Vec::new()));
        let b = agent(
            cfg("manual", "deny"),
            store.clone(),
            Some(Arc::new(ScriptedAsker {
                answer: CHOICE_DENY.into(),
                seen: seen2.clone(),
            })),
        );
        assert!(b.approve_call("terminal", &risky, None).await.is_some());
        assert_eq!(seen2.lock().unwrap().len(), 1, "other session asks again");
        println!("4. session → sticky per session, not across ✓");
        passed += 1;
    }

    // 5. "always" persists to the DB and survives a new agent.
    {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let a = agent(
            cfg("manual", "deny"),
            store.clone(),
            Some(Arc::new(ScriptedAsker {
                answer: CHOICE_ALWAYS.into(),
                seen: seen.clone(),
            })),
        );
        assert!(a.approve_call("terminal", &risky, None).await.is_none());
        assert!(store
            .lock()
            .unwrap()
            .approval_rule_exists("terminal:recursive-delete")?);
        let seen2 = Arc::new(std::sync::Mutex::new(Vec::new()));
        let b = agent(
            cfg("manual", "deny"),
            store.clone(),
            Some(Arc::new(ScriptedAsker {
                answer: CHOICE_DENY.into(),
                seen: seen2.clone(),
            })),
        );
        assert!(b.approve_call("terminal", &risky, None).await.is_none());
        assert!(
            seen2.lock().unwrap().is_empty(),
            "always rule short-circuits"
        );
        store
            .lock()
            .unwrap()
            .approval_rule_remove("terminal:recursive-delete")?;
        println!("5. always → persisted rule, honoured by a fresh agent ✓");
        passed += 1;
    }

    // 6. Config allowlist skips the prompt entirely.
    {
        let mut c = (*cfg("manual", "deny")).clone();
        c.approvals.allowlist = vec!["rm -rf /tmp/*".into()];
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let a = agent(
            Arc::new(c),
            store.clone(),
            Some(Arc::new(ScriptedAsker {
                answer: CHOICE_DENY.into(),
                seen: seen.clone(),
            })),
        );
        assert!(a.approve_call("terminal", &risky, None).await.is_none());
        assert!(seen.lock().unwrap().is_empty());
        println!("6. allowlist glob → never asked ✓");
        passed += 1;
    }

    // 7. No asker (cron / A2A / -q): policy decides, both ways.
    {
        let a = agent(cfg("manual", "deny"), store.clone(), None);
        assert!(a.approve_call("terminal", &risky, None).await.is_some());
        let b = agent(cfg("manual", "allow"), store.clone(), None);
        assert!(b.approve_call("terminal", &risky, None).await.is_none());
        println!("7. non-interactive → approvals.non_interactive decides ✓");
        passed += 1;
    }

    // 8. smart with an unreachable screen falls through to the human
    //    (fail-closed, not fail-open) and says so in the prompt.
    {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let a = agent(
            cfg("smart", "deny"),
            store.clone(),
            Some(Arc::new(ScriptedAsker {
                answer: CHOICE_ONCE.into(),
                seen: seen.clone(),
            })),
        );
        assert!(a.approve_call("terminal", &risky, None).await.is_none());
        let asked = seen.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert!(
            asked[0].prompt.contains("screen unavailable"),
            "prompt says the screen failed: {}",
            asked[0].prompt
        );
        println!("8. smart + dead screen → human is asked, prompt is honest ✓");
        passed += 1;
    }

    // 9. Sensitive paths gate the file tools too.
    {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let a = agent(
            cfg("manual", "deny"),
            store.clone(),
            Some(Arc::new(ScriptedAsker {
                answer: CHOICE_DENY.into(),
                seen: seen.clone(),
            })),
        );
        assert!(a
            .approve_call(
                "write_file",
                &json!({"path": "~/.ssh/authorized_keys"}),
                None
            )
            .await
            .is_some());
        assert!(a
            .approve_call("write_file", &json!({"path": "src/main.rs"}), None)
            .await
            .is_none());
        println!("9. file tools → sensitive paths only ✓");
        passed += 1;
    }

    // 10. Every decision is audited.
    {
        let rows = store.lock().unwrap().approval_history(100)?;
        assert!(rows.len() >= 10, "audit rows: {}", rows.len());
        assert!(rows.iter().any(|r| r.decided_by == "human"));
        assert!(rows.iter().any(|r| r.decided_by == "policy"));
        assert!(rows.iter().any(|r| r.decided_by == "rule"));
        assert!(rows.iter().any(|r| r.verdict == "deny"));
        println!(
            "10. audit trail → {} rows, all deciders present ✓",
            rows.len()
        );
        passed += 1;
    }

    println!("\napproval_smoke: {passed}/10 PASS");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

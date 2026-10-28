//! End-to-end smoke: load the real hermes config, resolve the primary
//! provider, make ONE live chat completion. Proves config compat +
//! provider client + streaming-free round trip.
//!
//! Run: cargo run --example live_llm

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = veles_core::config::load().expect("config");
        let entry = cfg
            .provider(cfg.model.provider.as_deref().unwrap_or(""))
            .expect("primary provider");
        let home = veles_core::config::veles_home();
        let key = veles_core::providers::resolve_api_key(entry, &home);
        println!(
            "provider: {} @ {}",
            entry.name,
            &entry.base_url[..60.min(entry.base_url.len())]
        );
        println!("key: {}…", &key[..12.min(key.len())]);

        let home = veles_core::config::veles_home();
        let ua = veles_core::providers::resolve_user_agent(cfg.agent.user_agent.as_deref(), &home);
        // build_client, not ProviderClient::new: a responses-mode entry
        // must speak the responses wire, and hardcoding the chat client
        // here would silently do the wrong thing on one.
        let client = veles_core::providers::build_client(entry, &key, &ua).expect("client");
        let req = veles_core::providers::ChatRequest {
            model: cfg
                .model
                .default
                .clone()
                .unwrap_or_else(|| "deepseek-v4-flash-0731".into()),
            messages: vec![veles_core::providers::ChatMessage::user(
                "Reply with exactly: VELES_OK",
            )],
            tools: vec![],
            reasoning_effort: None,
            stream: false,
            replay_reasoning: false,
            cache_mode: veles_core::providers::CacheMode::Auto,
            extra_body: None,
        };
        match client.chat(&req, &entry.name).await {
            Ok(completion) => {
                let content = &completion.message.content;
                let reasoning = completion.message.reasoning.as_deref();
                println!("LLM reply: {content}");
                if let Some(r) = reasoning {
                    println!("(reasoning passthrough present, {} chars)", r.len());
                }
                assert!(content.contains("VELES_OK"), "unexpected reply: {content}");
                println!("\nLIVE LLM ROUND TRIP OK");
            }
            Err(e) => {
                eprintln!("LLM call failed: {e:#}");
                std::process::exit(1);
            }
        }
    });
}

//! Explicitly invoked, isolated Anthropic Message Threads live measurement.
//!
//! This is NOT a test-gate example: it makes billed requests. It requires
//! `--live`, uses a fresh XDG home/store, never changes the operator's
//! configuration, and starts with six rounds (override with --rounds=N).
//!
//! ANTHROPIC_API_KEY=... direnv exec . cargo run -p veles-core \
//!   --example message_threads_live -- --live --rounds=6

use std::sync::{Arc, Mutex};
use std::time::Instant;
use veles_core::config::{ModelSection, ModelsSpec, ProviderEntry, VelesConfig};
use veles_core::providers::{build_routes, ChatRequest, StreamEvent};
use veles_core::{agent::Agent, hooks::HookBus, store::Store, tools::ToolRegistry};

struct Scratch(std::path::PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: Vec::new(),
        tools: Vec::new(),
        reasoning_effort: None,
        stream: true,
        replay_reasoning: false,
        cache_mode: veles_core::providers::CacheMode::Explicit,
        extra_body: None,
        thread: None,
    }
}

fn measure_request(
    req: &ChatRequest,
    route: &veles_core::providers::PluginRoute,
    state: Option<&veles_core::message_threads::State>,
) -> (usize, &'static str) {
    let prepared = veles_core::message_threads::prepare(state, req, route, false);
    let kind = if prepared.continuing {
        "continue"
    } else {
        "create"
    };
    let mut projected = prepared.req.clone();
    projected.messages = prepared.messages;
    projected.thread = Some(veles_core::providers::ThreadRequest {
        previous_message_id: prepared.previous_response_id,
    });
    (
        veles_core::providers::wire_body(&projected)
            .to_string()
            .len(),
        kind,
    )
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--live") {
        anyhow::bail!("refusing billed requests without --live");
    }
    if std::env::var_os("ANTHROPIC_API_KEY").is_none() {
        anyhow::bail!("ANTHROPIC_API_KEY must be supplied in the environment");
    }
    let rounds = args
        .iter()
        .find_map(|a| a.strip_prefix("--rounds="))
        .map(str::parse::<usize>)
        .transpose()?
        .unwrap_or(6);
    if !(2..=50).contains(&rounds) {
        anyhow::bail!("--rounds must be between 2 and 50");
    }
    let model = args
        .iter()
        .find_map(|a| a.strip_prefix("--model="))
        .unwrap_or("claude-haiku-4-5")
        .to_string();
    let unique = format!(
        "veles-thread-live-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    let _scratch = Scratch(root.clone());
    let config = root.join("config");
    let data = root.join("data");
    let cache = root.join("cache");
    std::fs::create_dir_all(&config)?;
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&cache)?;
    std::env::set_var("XDG_CONFIG_HOME", &config);
    std::env::set_var("XDG_DATA_HOME", &data);
    std::env::set_var("XDG_CACHE_HOME", &cache);
    std::env::set_var("VELES_HOME", data.join("veles"));
    std::env::remove_var("VELES_PROFILE");
    std::env::remove_var("VELES_ANTHROPIC_MESSAGE_THREADS");
    let native_so = args.iter().any(|a| a == "--native-so");
    if native_so {
        std::env::remove_var("VELES_PLUGIN_DIRS");
    } else {
        std::env::set_var(
            "VELES_PLUGIN_DIRS",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../plugins/examples/tcl-anthropic-driver"
            ),
        );
    }
    std::fs::create_dir_all(config.join("veles"))?;
    std::fs::create_dir_all(data.join("veles"))?;
    // A fixed, tiny system prompt keeps the comparison about the growing
    // authoritative conversation, not a machine-specific agent prompt.
    let provider = ProviderEntry {
        name: "anthropic-live".into(),
        base_url: "https://api.anthropic.com".into(),
        key_env: Some("ANTHROPIC_API_KEY".into()),
        api_mode: "plugin".into(),
        driver: Some("anthropic-messages".into()),
        models: ModelsSpec::List(vec![model.clone()]),
        api_version: Some("2026-10-21".into()),
        max_output_tokens: Some(64),
        cache_ttl: Some("5m".into()),
        message_threads: Some(true),
        ..Default::default()
    };
    let cfg = Arc::new(VelesConfig {
        model: ModelSection {
            provider: Some("custom:anthropic-live".into()),
            default: Some(model.clone()),
            ..Default::default()
        },
        custom_providers: vec![provider],
        ..Default::default()
    });
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let tcl = veles_core::tclrt::spawn(veles_core::config::config_dir())?;
        // Hooks are deliberately empty, but Tcl owns the native driver.
        let hooks = HookBus::new(Some(tcl));
        hooks
            .eval_tcl("if {$::veles_plugin_load_error ne \"\"} {error $::veles_plugin_load_error}")
            .await
            .map_err(|e| anyhow::anyhow!("Tcl plugin initialization: {e}"))?;
        if native_so {
            let so = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../target/debug/libveles_plugin_anthropic_so.so"
            );
            veles_core::rustplugins::load_so(std::path::Path::new(so), "anthropic-so", "live")?;
        }
        let expected = if native_so {
            "so:anthropic-so:anthropic-messages"
        } else {
            "tcl:tcl-anthropic-driver:anthropic-messages"
        };
        if !veles_core::providers::drivers()
            .iter()
            .any(|d| d.qualified_name() == expected)
        {
            anyhow::bail!("the bundled native Anthropic Tcl driver did not register");
        }
        let store = Arc::new(Mutex::new(Store::open(&data.join("veles/state.db"))?));
        let seeded =
            "Veles live thread measurement uses only synthetic short facts. Reply briefly.";
        println!("mode=threaded+unthreaded rounds={rounds} model={model} cache=explicit");
        println!("request_bytes=local wire projection; ttft=first visible text or reasoning delta");
        for threaded in [true, false] {
            let mut mode_cfg = (*cfg).clone();
            mode_cfg.custom_providers[0].message_threads = Some(threaded);
            let cfg = Arc::new(mode_cfg);
            let session = if threaded {
                "live-threaded"
            } else {
                "live-unthreaded"
            };
            let agent = Agent::new(
                cfg.clone(),
                hooks.clone(),
                Arc::new(ToolRegistry::new()),
                store.clone(),
                session.into(),
            )?;
            *agent.system_prompt.write().unwrap() = seeded.into();
            let mut history = store.lock().unwrap().load_history(session)?;
            let mut totals = (0usize, 0u128);
            let routes = build_routes(&cfg)?;
            let veles_core::providers::RouteClient::Plugin(route) = &routes[0].client else {
                anyhow::bail!("live measurement requires a native Anthropic plugin route");
            };
            println!(
                "route_driver={} resolved_driver={} threads_enabled={}",
                route.driver,
                veles_core::providers::driver(&route.driver)
                    .map(|d| d.qualified_name())
                    .unwrap_or_else(|| "missing".into()),
                veles_core::providers::message_threads_enabled(route)
            );
            for round in 0..rounds {
                let prompt = format!("Synthetic fact {round}: the number is {round}. Reply OK only.");
                let projected = ChatRequest {
                    messages: {
                        let mut m = vec![veles_core::providers::ChatMessage {
                            role: "system".into(),
                            ..veles_core::providers::ChatMessage::user(seeded)
                        }];
                        m.extend(history.clone());
                        m.push(veles_core::providers::ChatMessage::user(&prompt));
                        m
                    },
                    ..request(&model)
                };
                let (bytes, kind) = if threaded {
                    measure_request(&projected, route, agent.message_thread_state().as_ref())
                } else {
                    (
                        veles_core::providers::wire_body(&projected)
                            .to_string()
                            .len(),
                        "full",
                    )
                };
                if let Some(state) = agent.message_thread_state() {
                    println!(
                        "before_round={} cursor_suffix={} sent_messages={}",
                        round,
                        state.previous_message_id.len().saturating_sub(8),
                        state.sent
                    );
                }
                let started = Instant::now();
                let first = Mutex::new(None);
                let answer = agent
                    .run_turn(
                        &mut history,
                        &prompt,
                        None,
                        |ev| {
                            if matches!(ev, StreamEvent::Delta(_) | StreamEvent::ReasoningDelta(_))
                            {
                                let mut slot = first.lock().unwrap();
                                if slot.is_none() {
                                    *slot = Some(started.elapsed().as_millis());
                                }
                            }
                            if let StreamEvent::Note(n) = ev {
                                eprintln!("engine-note={}", n.line());
                            }
                        },
                    )
                    .await?;
                let elapsed = started.elapsed().as_millis();
                let ttft = first
                    .into_inner()
                    .unwrap()
                    .map_or_else(|| "none".into(), |v| v.to_string());
                println!(
                    "mode={} round={} request_kind={} local_wire_bytes={} ttft_ms={} total_ms={} reply_chars={}",
                    if threaded { "threaded" } else { "unthreaded" },
                    round,
                    kind,
                    bytes,
                    ttft,
                    elapsed,
                    answer.reply.len()
                );
                println!(
                    "after_round={} response_id_present={} cursor={:?}",
                    round,
                    agent.message_thread_state().is_some(),
                    agent
                        .message_thread_state()
                        .map(|s| (s.previous_message_id.len(), s.sent))
                );
                totals.0 += bytes;
                totals.1 += elapsed;
            }
            println!(
                "mode={} mean_local_wire_bytes={} mean_total_ms={}",
                if threaded { "threaded" } else { "unthreaded" },
                totals.0 / rounds,
                totals.1 / rounds as u128
            );
        }
        // Exercise the same public route resolution used by Agent::new.
        let _ = build_routes(&cfg)?;
        Ok(())
    })
}

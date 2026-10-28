//! Live STREAMING smoke — proves the M1 SSE path end-to-end against the
//! real token-plan route: deltas arrive incrementally, reasoning_delta is
//! captured, and the assembled message is complete.
//!
//! Run: cargo run --example live_stream

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = veles_core::config::load().expect("config");
        let routes = veles_core::providers::build_routes(&cfg).expect("routes");
        let route = &routes[0];

        let req = veles_core::providers::ChatRequest {
            model: route.model.clone(),
            messages: vec![veles_core::providers::ChatMessage::user(
                "Count from 1 to 5, one per line.",
            )],
            tools: vec![],
            reasoning_effort: None,
            stream: true,
            replay_reasoning: false,
            cache_mode: veles_core::providers::CacheMode::Auto,
            extra_body: None,
        };

        let mut deltas = 0usize;
        let mut reasoning = 0usize;
        let completion = route
            .client
            .chat_stream(&req, &route.provider, None, |ev| match ev {
                veles_core::providers::StreamEvent::Delta(t) => {
                    deltas += t.len();
                    print!("{t}");
                }
                veles_core::providers::StreamEvent::ReasoningDelta(_) => reasoning += 1,
                veles_core::providers::StreamEvent::Route { provider: p, .. } => {
                    eprintln!("\n[streaming via {p}]")
                }
                veles_core::providers::StreamEvent::ToolStart(_, _) => {}
                veles_core::providers::StreamEvent::ToolEnd(_, _) => {}
                veles_core::providers::StreamEvent::Done(_) => {}
                veles_core::providers::StreamEvent::Cancelled => {}
                veles_core::providers::StreamEvent::Ask(_) => {}
                veles_core::providers::StreamEvent::Note(n) => {
                    eprintln!("\n[{}]", n.line())
                }
            })
            .await
            .expect("stream");

        println!();
        println!("deltas: {deltas} chars, reasoning chunks: {reasoning}");
        println!(
            "final content ({} chars): {}",
            completion.message.content.len(),
            completion.message.content
        );
        assert!(
            completion.message.content.contains('5'),
            "expected 1..5 in reply"
        );
        println!("\nLIVE STREAMING OK");
    });
}

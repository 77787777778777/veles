//! Source-side streaming PACING probe.
//!
//! `live_stream.rs` proves the deltas arrive. This one asks the question
//! that matters when a frontend looks stalled: *at what rate, and in what
//! sizes, does the PROVIDER actually hand them over?*
//!
//! USER, on deepseek-v4.1-flash: "I get huge chunks of thinking with huge
//! delays". A reasoner that flushes big reasoning chunks at once and a
//! renderer too slow to keep up look identical from the outside, so this
//! has to be measured at the wire before any render work is believed. Run
//! it, then compare against the desktop-side trace (VELES_DESKTOP_TRACE):
//! if the gaps are here, the frontend is innocent.
//!
//! Run: cargo run --example stream_pacing -- [model-substring]
//!      cargo run --example stream_pacing -- v4.1-flash-1017

use std::time::Instant;

/// One arrival, as the client saw it.
struct Arrival {
    /// Microseconds since the request was issued.
    at_us: u128,
    /// "reasoning" or "content".
    kind: &'static str,
    /// Bytes in THIS chunk — the thing that distinguishes "streaming
    /// smoothly" from "flushing a paragraph at a time".
    len: usize,
}

/// p50/p95/max over a sorted-in-place copy. Empty input answers zeros
/// rather than panicking on an index, so a route that emits no reasoning
/// at all still prints a row.
fn percentiles(v: &mut [u128]) -> (u128, u128, u128) {
    if v.is_empty() {
        return (0, 0, 0);
    }
    v.sort_unstable();
    let at = |f: f64| v[((v.len() - 1) as f64 * f).round() as usize];
    (at(0.5), at(0.95), v[v.len() - 1])
}

fn report(label: &str, arrivals: &[Arrival], kind: &str) {
    let mine: Vec<&Arrival> = arrivals.iter().filter(|a| a.kind == kind).collect();
    if mine.is_empty() {
        println!("{label:>10}: none");
        return;
    }
    let bytes: usize = mine.iter().map(|a| a.len).sum();
    // Gaps BETWEEN arrivals of this kind — the inter-arrival time a
    // frontend's event loop actually sees.
    let mut gaps: Vec<u128> = mine.windows(2).map(|w| w[1].at_us - w[0].at_us).collect();
    let (g50, g95, gmax) = percentiles(&mut gaps);
    let mut lens: Vec<u128> = mine.iter().map(|a| a.len as u128).collect();
    let (l50, l95, lmax) = percentiles(&mut lens);
    let span_us = mine
        .last()
        .unwrap()
        .at_us
        .saturating_sub(mine[0].at_us)
        .max(1);
    println!(
        "{label:>10}: {} chunks, {} bytes over {:.2}s  |  gap p50 {:.1}ms p95 {:.1}ms max {:.1}ms  |  chunk p50 {l50}B p95 {l95}B max {lmax}B  |  {:.0} B/s, {:.0} chunks/s",
        mine.len(),
        bytes,
        span_us as f64 / 1e6,
        g50 as f64 / 1000.0,
        g95 as f64 / 1000.0,
        gmax as f64 / 1000.0,
        bytes as f64 / (span_us as f64 / 1e6),
        mine.len() as f64 / (span_us as f64 / 1e6),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let raw = args.iter().any(|a| a == "--raw");
    let want = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_default();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let cfg = veles_core::config::load().expect("config");
        let routes = veles_core::providers::build_routes(&cfg).expect("routes");
        // `provider/model` builds that exact pair the way a session
        // `/model` override does (agent.rs `effective_routes`): the
        // model the user is asking about need not be a configured
        // ROUTE — v4.1-flash is a models-map entry, not one of the
        // three routes build_routes returns. A bare string matches an
        // existing route by model or provider; no argument takes the
        // primary, exactly as live_stream.rs does.
        let route = match want.split_once('/') {
            Some((pkey, model)) => {
                let entry = cfg
                    .provider(pkey)
                    .unwrap_or_else(|| panic!("unknown provider: {pkey}"));
                let home = veles_core::config::config_dir();
                let key = veles_core::providers::resolve_api_key(entry, &home);
                let ua = veles_core::providers::resolve_user_agent(
                    cfg.agent.user_agent.as_deref(),
                    &home,
                );
                let mut r = routes
                    .first()
                    .cloned()
                    .expect("at least one route to clone the posture from");
                r.provider = entry.name.clone();
                r.model = model.to_string();
                r.client = veles_core::providers::build_client(entry, &key, &ua)
                    .expect("client for the named provider");
                r.reasoning_effort = entry.reasoning_effort.clone();
                r.extra_body = entry.extra_body.clone();
                r.replay_reasoning = entry.replay_reasoning.unwrap_or(false);
                r
            }
            None => routes
                .iter()
                .find(|r| want.is_empty() || r.model.contains(&want) || r.provider.contains(&want))
                .cloned()
                .unwrap_or_else(|| {
                    panic!(
                        "no route matching {want:?}; have: {}\n\
                         (or name one as provider/model, e.g. \
                         ali-token-plan-personal/deepseek-v4.1-flash-1017)",
                        routes
                            .iter()
                            .map(|r| format!("{}/{}", r.provider, r.model))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }),
        };
        eprintln!("[probing {} / {}]", route.provider, route.model);

        // Long enough to stream for a while, and shaped to produce BOTH
        // a reasoning pass and a substantial prose answer with markdown
        // block structure — the exact input the desktop renderer chokes
        // on.
        let prompt = "Explain, in about 900 words of markdown with several \
                      headings, bullet lists and one fenced code block, how a \
                      single-threaded event loop starves when a render \
                      callback runs longer than the interval between input \
                      events. Think it through carefully first.";
        // --raw answers a different question than the event trace, and
        // has to run against the SAME prompt to be comparable.
        if raw {
            let pkey = want
                .split_once('/')
                .map(|(p, _)| p)
                .unwrap_or(&route.provider);
            let entry = cfg.provider(pkey).expect("provider for --raw");
            let home = veles_core::config::config_dir();
            let key = veles_core::providers::resolve_api_key(entry, &home);
            raw_probe(entry, &route.model, prompt, &key).await;
            return;
        }

        let req = veles_core::providers::ChatRequest {
            model: route.model.clone(),
            messages: vec![veles_core::providers::ChatMessage::user(prompt)],
            tools: vec![],
            reasoning_effort: None,
            stream: true,
            replay_reasoning: false,
            cache_mode: veles_core::providers::CacheMode::Auto,
            extra_body: None,
        };

        let t0 = Instant::now();
        let mut arrivals: Vec<Arrival> = Vec::new();
        let completion = route
            .client
            .chat_stream(&req, &route.provider, None, |ev| {
                let at_us = t0.elapsed().as_micros();
                match ev {
                    veles_core::providers::StreamEvent::Delta(t) => arrivals.push(Arrival {
                        at_us,
                        kind: "content",
                        len: t.len(),
                    }),
                    veles_core::providers::StreamEvent::ReasoningDelta(t) => {
                        arrivals.push(Arrival {
                            at_us,
                            kind: "reasoning",
                            len: t.len(),
                        })
                    }
                    _ => {}
                }
            })
            .await
            .expect("stream");
        let total_us = t0.elapsed().as_micros();

        // The raw trace first, so a burst pattern is visible by eye and
        // not only through the percentiles. One line per chunk.
        println!("# at_ms\tkind\tbytes");
        for a in &arrivals {
            println!("{:.1}\t{}\t{}", a.at_us as f64 / 1000.0, a.kind, a.len);
        }
        println!();
        println!("route: {} / {}", route.provider, route.model);
        println!("wall:  {:.2}s to last chunk", total_us as f64 / 1e6);
        report("reasoning", &arrivals, "reasoning");
        report("content", &arrivals, "content");
        report_bursts(&arrivals, "reasoning");
        report_bursts(&arrivals, "content");
        println!(
            "final: {} content chars, {} reasoning chars",
            completion.message.content.len(),
            completion
                .message
                .reasoning
                .as_deref()
                .map(str::len)
                .unwrap_or(0),
        );
    });
}

/// Raw-wire mode (`--raw`): time the BYTE CHUNKS, not the parsed events.
///
/// `chat_stream` calls `on_event` synchronously inside its own
/// `stream.next().await` loop, so if one large TCP read carries forty SSE
/// frames, all forty fire back-to-back with a ~0 gap. That looks exactly
/// like "the provider batched forty tokens", and it is not the same claim.
/// This mode settles it: it POSTs the same request and reports when raw
/// bytes landed and how many `data:` frames each read carried.
///
/// Deliberately its own request rather than a hook in `chat_stream` —
/// production code does not grow a measurement seam for a diagnostic.
async fn raw_probe(
    entry: &veles_core::config::ProviderEntry,
    model: &str,
    prompt: &str,
    key: &str,
) {
    use futures::StreamExt;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .expect("client");
    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    let t0 = Instant::now();
    let resp = http
        .post(format!(
            "{}/chat/completions",
            entry.base_url.trim_end_matches('/')
        ))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .expect("send");
    eprintln!(
        "[raw] status {} after {:.0}ms; transfer-encoding={:?} content-encoding={:?}",
        resp.status(),
        t0.elapsed().as_millis(),
        resp.headers()
            .get("transfer-encoding")
            .and_then(|v| v.to_str().ok()),
        resp.headers()
            .get("content-encoding")
            .and_then(|v| v.to_str().ok()),
    );
    let mut stream = resp.bytes_stream();
    let mut reads: Vec<(u128, usize, usize)> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(b) = chunk else { break };
        let frames = b.windows(6).filter(|w| w == b"data: ").count();
        reads.push((t0.elapsed().as_micros(), b.len(), frames));
    }
    println!("# raw_at_ms\tbytes\tsse_frames");
    for (at, len, frames) in &reads {
        println!("{:.1}\t{}\t{}", *at as f64 / 1000.0, len, frames);
    }
    let total: usize = reads.iter().map(|r| r.1).sum();
    let mut gaps: Vec<u128> = reads.windows(2).map(|w| w[1].0 - w[0].0).collect();
    let (g50, g95, gmax) = percentiles(&mut gaps);
    println!();
    println!(
        "raw: {} socket reads, {} bytes, {} frames over {:.2}s | read gap p50 {:.1}ms p95 {:.1}ms max {:.1}ms | {:.0} frames/read",
        reads.len(),
        total,
        reads.iter().map(|r| r.2).sum::<usize>(),
        t0.elapsed().as_secs_f64(),
        g50 as f64 / 1000.0,
        g95 as f64 / 1000.0,
        gmax as f64 / 1000.0,
        reads.iter().map(|r| r.2).sum::<usize>() as f64 / reads.len().max(1) as f64,
    );
}

/// Bursts, which is the shape the percentiles hide.
///
/// A gap longer than `BURST_GAP_MS` starts a new burst. This is the whole
/// diagnosis in one line: "2139 chunks at a p50 gap of 0.0ms" and "107
/// bursts averaging 450ms apart" are the same data, and only the second
/// one tells you what the frontend's event loop is being handed.
const BURST_GAP_US: u128 = 50_000;

fn report_bursts(arrivals: &[Arrival], kind: &str) {
    let mine: Vec<&Arrival> = arrivals.iter().filter(|a| a.kind == kind).collect();
    if mine.len() < 2 {
        return;
    }
    let mut bursts: Vec<(u128, usize, usize)> = Vec::new(); // (start_us, chunks, bytes)
    bursts.push((mine[0].at_us, 1, mine[0].len));
    for w in mine.windows(2) {
        if w[1].at_us - w[0].at_us > BURST_GAP_US {
            bursts.push((w[1].at_us, 1, w[1].len));
        } else {
            let last = bursts.last_mut().unwrap();
            last.1 += 1;
            last.2 += w[1].len;
        }
    }
    let mut ibg: Vec<u128> = bursts.windows(2).map(|w| w[1].0 - w[0].0).collect();
    let (b50, b95, bmax) = percentiles(&mut ibg);
    let mut bb: Vec<u128> = bursts.iter().map(|b| b.2 as u128).collect();
    let (y50, _y95, ymax) = percentiles(&mut bb);
    println!(
        "{:>10}: {} bursts (>{}ms apart) | inter-burst p50 {:.0}ms p95 {:.0}ms max {:.0}ms | bytes/burst p50 {y50} max {ymax}",
        format!("{kind} bursts"),
        bursts.len(),
        BURST_GAP_US / 1000,
        b50 as f64 / 1000.0,
        b95 as f64 / 1000.0,
        bmax as f64 / 1000.0,
    );
}

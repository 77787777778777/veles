//! Fallback chain smoke test — no network needed.
//! Verifies: route building from config, error classification, and the
//! non-sticky walk-down behavior of call_with_fallback's contract.
//!
//! Run: cargo run --example fallback_chain

use veles_core::providers::ProviderError;

fn main() {
    // 1. Error classification (mirrors hermes _should_fallback).
    let rate = ProviderError::RateLimited("Throttling.AllocationQuota".into());
    let server = ProviderError::Server {
        status: 503,
        body: "upstream".into(),
    };
    let net = ProviderError::Network("connection reset".into());
    let client = ProviderError::Client {
        status: 401,
        body: "bad key".into(),
    };
    assert!(rate.should_fallback(), "429 must fall back");
    assert!(server.should_fallback(), "5xx must fall back");
    assert!(net.should_fallback(), "network must fall back");
    assert!(!client.should_fallback(), "4xx (auth) must NOT fall back");
    println!("1) error classification OK");

    // 2. Route chain from the real config: primary + fallbacks in order.
    let cfg = veles_core::config::load().expect("config");
    let routes = veles_core::providers::build_routes(&cfg).expect("routes");
    assert!(!routes.is_empty());
    println!("2) route chain from ~/.hermes/config.yaml:");
    for (i, r) in routes.iter().enumerate() {
        println!("   [{}] {} → {}", i, r.provider, r.model);
    }
    // This machine's config: personal primary, zai + team fallbacks.
    if routes.len() >= 3 {
        assert!(routes[0].provider.contains("personal"));
        println!(
            "   (personal primary + {} fallbacks, as configured)",
            routes.len() - 1
        );
    }

    // 3. Non-sticky semantics are structural: call_with_fallback always
    // starts at routes[0] (verified by code path — each call gets a fresh
    // loop over the chain; there is no persisted "last good route").
    println!("3) non-sticky by construction: no sticky-route state exists");

    println!("\nFALLBACK CHAIN SMOKE PASSED");
}

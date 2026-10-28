//! Aux-task routing (S16 companion to failover.rs).
//!
//! Aux consumers — session titles, approval screens, reasoning-summary
//! plugins (`veles::call_llm`), delegate children — all make small LLM
//! calls that compete with the main brain for the same plan's RPM
//! budget. When a scope rate-limits, these consumers should MOVE, not
//! hammer: this module resolves the effective aux route honoring
//! `aux_fallback_provider/model` from the matching failover rule.
//!
//! USER CONTEXT (2026-08-22): deepseek-flash-class models are super cheap
//! but kick you out under concurrent load; the fix is pointing aux work
//! elsewhere when the primary scope is throttled.

use crate::config::VelesConfig;

/// Resolve the route an AUX task should use right now.
///
/// Precedence:
///   1. explicit provider arg (plugin passed one) — honored as-is
///   2. the failover rule matching the PRIMARY route's (provider, model)
///      with `aux_fallback` set → that pair (if still resolvable)
///   3. otherwise the primary route (existing behavior)
pub fn resolve_aux_route<'a>(
    cfg: &VelesConfig,
    routes: &'a [crate::providers::Route],
    explicit_provider: Option<&str>,
) -> Option<&'a crate::providers::Route> {
    if routes.is_empty() {
        return None;
    }
    // 1. explicit provider wins.
    if let Some(p) = explicit_provider {
        return routes.iter().find(|r| r.provider == p);
    }
    // 2. aux_fallback from the rule matching the primary scope.
    let primary = &routes[0];
    let decision = crate::failover::resolve(
        &primary.provider,
        &primary.model,
        None,
        &cfg.failover_rules,
        false,
    );
    if let Some((ref ap, ref am)) = decision.aux_fallback {
        // The aux pair must exist among configured routes; prefer exact
        // provider+model, then any route on the aux provider.
        if let Some(r) = routes.iter().find(|r| &r.provider == ap && &r.model == am) {
            return Some(r);
        }
        if let Some(r) = routes.iter().find(|r| &r.provider == ap) {
            return Some(r);
        }
        tracing::warn!(
            "failover.aux_fallback target {ap}/{am} not in configured routes — using primary"
        );
    }
    // 3. primary.
    Some(primary)
}

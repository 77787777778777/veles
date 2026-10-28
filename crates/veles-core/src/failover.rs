//! Failover policy (S16) — replaces the silent hermes-style chain walk
//! with an explicit, scoped decision process.
//!
//! USER DIRECTIVE (2026-08-22): "I really hate the hermes default
//! failover and the failover we've patched into it. Each failover
//! decision should be a multi-step interactive process with the user by
//! default and scriptable as an option, settable per provider+model pair
//! AND per session."
//!
//! USER DIRECTIVE (2026-09-05): failover is DISABLED by default now
//! (implicit = Stop). The interactive `ask` and silent `auto` remain
//! usable ONLY behind an explicit `[[failover]]` rule or a `/failover`
//! session override. See `Decision::implicit` for the note.
//!
//! Modes (per rule):
//!   - `ask`    — the failure becomes an interactive multi-step
//!     ask: route failed (which, why) → [retry] [next route] [stop];
//!     choosing next shows where the chain would go before it goes.
//!   - `auto`   — walk the chain silently.
//!   - `stop`   — never fall back; error surfaces immediately.
//!   - `script` — auto, but every decision fires the `failover` Tcl hook
//!     ({from_provider, from_model, to_provider, to_model, error}); a
//!     handler returning {"action":"deny"} vetoes that hop.
//!
//! Resolution order for a failing call on (provider P, model M) in
//! session S:
//!   1. session override (`failover_mode` in the session's policy row,
//!      set via /failover or the desktop picker)
//!   2. first matching config rule: explicit-model rules beat
//!      provider-only rules beat wildcard/default; within the same
//!      specificity, config order decides
//!   3. implicit default: `stop` (failover disabled — for human AND
//!      headless sessions, so a transient failure is a visible error,
//!      never a silent re-route)

use serde::{Deserialize, Serialize};

/// One failover mode value, parsed from config/session overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Interactive multi-step decision (the default for humans).
    #[default]
    Ask,
    /// Silent chain walk (hermes parity; headless default).
    Auto,
    /// Never fall back.
    Stop,
    /// Auto + vetoable `failover` hooks per hop.
    Script,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "ask" | "interactive" => Mode::Ask,
            "auto" | "silent" | "chain" => Mode::Auto,
            "stop" | "never" | "fail" => Mode::Stop,
            "script" | "hook" => Mode::Script,
            _ => return None,
        })
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::Auto => "auto",
            Mode::Stop => "stop",
            Mode::Script => "script",
        }
    }
}

/// What a resolved policy says to do on failure of one route.
#[derive(Debug, Clone)]
pub struct Decision {
    pub mode: Mode,
    /// Remaining allowed hops (None = unlimited).
    pub max_hops: Option<u32>,
    /// Where the rule came from (session / config:<idx> / implicit).
    pub source: String,
    /// Default wait duration for the "wait" action (usage-window reset).
    pub wait_default_secs: u64,
    /// RPM/concurrency pressure handling: when set, aux-task consumers
    /// (titles, screens, reasoning-summary plugins, subagents) that hit
    /// 429 on this scope get re-pointed to (provider, model) instead of
    /// hammering the same route.
    pub aux_fallback: Option<(String, String)>,
    /// For `script` mode: which named script to run (None = the historic
    /// single `failover` hook).
    pub script: Option<String>,
}

impl Decision {
    /// Implicit default when no rule and no session override names a
    /// mode. This was `Ask` (interactive) / `Auto` (headless), which
    /// made every transient provider failure pop a modal or silently
    /// re-route. USER DIRECTIVE (2026-09-05): *"disable the failover
    /// policy for now — it was already disabled in hermes and it's
    /// very annoying. I'll think of a better one later but for now
    /// let's just keep it simple."* Disabled means: a failed route
    /// errors immediately (Stop); no auto-proceed, no ask. Explicit
    /// `[[failover]]` rules and `/failover` session overrides still
    /// take precedence if the user later opts back in per-scope.
    pub fn implicit(_interactive: bool) -> Self {
        Self {
            mode: Mode::Stop,
            max_hops: None,
            source: "implicit".into(),
            wait_default_secs: 300,
            aux_fallback: None,
            script: None,
        }
    }
}

/// Resolve the effective decision for a failing (provider, model) call
/// in a session with optional per-session mode override.
///
/// `rules` = cfg.failover_rules (config order). Specificity: rules that
/// name BOTH provider and model come first, then provider-only, then
/// catch-alls; ties break by config order (stable partition).
pub fn resolve(
    provider: &str,
    model: &str,
    session_override: Option<&str>,
    rules: &[crate::config::FailoverRule],
    interactive_asker: bool,
) -> Decision {
    // 1. Session override wins outright.
    if let Some(m) = session_override.and_then(Mode::parse) {
        return Decision {
            mode: m,
            max_hops: None,
            source: "session".into(),
            wait_default_secs: 300,
            aux_fallback: None,
            script: None,
        };
    }
    // 2. Config rules by specificity then order.
    fn matches(r: &crate::config::FailoverRule, p: &str, m: &str) -> bool {
        let p_ok = r
            .provider
            .as_deref()
            .map(|x| x == "*" || x == p)
            .unwrap_or(true);
        let m_ok = r
            .model
            .as_deref()
            .map(|x| x == "*" || x == m)
            .unwrap_or(true);
        p_ok && m_ok
    }
    let specificity = |r: &crate::config::FailoverRule| -> u8 {
        match (&r.provider, &r.model) {
            (Some(p), Some(m)) if p != "*" || m != "*" => 0,
            (Some(_), None) => 1,
            (None, Some(_)) => 1,
            _ => 2,
        }
    };
    let mut indexed: Vec<(usize, &crate::config::FailoverRule)> =
        rules.iter().enumerate().collect();
    indexed.sort_by_key(|(i, r)| (specificity(r), *i));
    for (_, r) in indexed {
        if matches(r, provider, model) {
            return Decision {
                mode: Mode::parse(r.mode()).unwrap_or(Mode::Ask),
                max_hops: r.max_hops,
                source: format!("config[provider={:?},model={:?}]", r.provider, r.model),
                wait_default_secs: r.wait_default_secs.unwrap_or(300),
                aux_fallback: match (&r.aux_fallback_provider, &r.aux_fallback_model) {
                    (Some(p), Some(m)) => Some((p.clone(), m.clone())),
                    _ => None,
                },
                script: r.script.clone(),
            };
        }
    }
    // 3. Implicit default.
    Decision::implicit(interactive_asker)
}

// ── named scripts (multiple failover scripts) ──────────────────────

/// Where named failover scripts live. Each `<name>.tcl` defines
/// `proc failover::handle {}` and returns "" (advance) or
/// `{"action":"deny"}` (veto), exactly like the historic single hook.
pub fn scripts_dir() -> std::path::PathBuf {
    crate::config::config_dir().join("failover-scripts")
}

/// A script name → a safe path component. Letters, digits, `-` and `_`
/// only; anything else is refused, because the name becomes both a file
/// path and, effectively, an eval target. Bounded so a hostile name
/// cannot drop a huge file on disk.
pub fn is_valid_script_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The path for a named script (validated by the caller).
pub fn script_path(name: &str) -> std::path::PathBuf {
    scripts_dir().join(format!("{name}.tcl"))
}

// ── the interactive ask (multi-step) ─────────────────────────────────

/// Step 1 question payload: what failed and what the options do.
pub fn ask_prompt(provider: &str, model: &str, err: &str, next: Option<(&str, &str)>) -> String {
    let mut s = crate::i18n::lookup(
        "failover_needed",
        &[
            ("provider", provider.to_string()),
            ("model", model.to_string()),
        ],
    );
    s.push('\n');
    s.push_str(&crate::i18n::lookup(
        "failover_error_line",
        &[("error", err.to_string())],
    ));
    s.push('\n');
    s.push_str(&crate::i18n::lookup("failover_what_next", &[]));
    s.push('\n');
    match next {
        Some((p, m)) => s.push_str(&crate::i18n::lookup(
            "failover_next_route",
            &[("provider", p.to_string()), ("model", m.to_string())],
        )),
        None => s.push_str(&crate::i18n::lookup("failover_no_more_routes", &[])),
    }
    s
}

/// Parse the user's reply to the failover ask.
/// Returns RetrySame | AdvanceOne | Wait(secs) | Stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskVerdict {
    RetrySame,
    AdvanceOne,
    /// Pause for N seconds (usage-window reset / RPM cooldown), then
    /// resume the SAME route. Duration comes from the reply ("wait",
    /// "wait 20m") or the rule default when bare.
    Wait(u64),
    Stop,
}

/// Choices rendered into the ask (order = the numbers users type).
/// `wait_label` carries the effective default duration.
pub fn choices(wait_default_secs: u64) -> Vec<(&'static str, String)> {
    vec![
        ("retry", crate::i18n::lookup("choice_retry", &[])),
        ("next", crate::i18n::lookup("choice_next", &[])),
        (
            "wait",
            crate::i18n::lookup("choice_wait", &[("secs", wait_default_secs.to_string())]),
        ),
        ("stop", crate::i18n::lookup("choice_stop", &[])),
    ]
}

/// Parse a duration suffix: "20m" → 1200, "90s"/"90" → 90, "2h" → 7200.
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix('m') {
        (n, 60u64)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else {
        (s, 1)
    };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

pub fn parse_verdict(reply: &str) -> Option<AskVerdict> {
    let t = reply.trim();
    let low = t.to_ascii_lowercase();
    // "wait", "wait 20m", "w 5m", "4".
    let mut tokens = low.split_whitespace();
    let first = tokens.next().unwrap_or("");
    match first {
        "r" | "retry" | "again" => return Some(AskVerdict::RetrySame),
        "1" => return Some(AskVerdict::RetrySame),
        "n" | "next" | "fallback" => return Some(AskVerdict::AdvanceOne),
        "2" => return Some(AskVerdict::AdvanceOne),
        "s" | "stop" | "cancel" | "abort" | "no" => return Some(AskVerdict::Stop),
        "3" => return Some(AskVerdict::Stop),
        "w" | "wait" | "4" => {
            let dur = tokens
                .next()
                .and_then(parse_duration)
                // Bare "4"/"w"/"wait": sensible default 5 min.
                .unwrap_or(300);
            return Some(AskVerdict::Wait(dur.clamp(10, 3600)));
        }
        _ => {}
    }
    None
}

// ── persistence (per-session override) ───────────────────────────────

/// Session-scoped failover override, persisted in the meta table so
/// --resume keeps it. Keyed `failover_mode:<session_id>`.
pub const META_PREFIX: &str = "failover_mode:";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionOverride {
    pub mode: String,
}

// ── the override point `failover.decide` (PLAN-extension-api.md E2) ──
//
// USER 2026-10-11: with hermes, changing the failover strategy meant a
// patch to re-merge on every update. Here it is a decision veles asks by
// name: core registers the configured policy (above) as the point's
// default, and a plugin in any tier answers instead, or wraps it.
//
// Core's default answers `{"action": "policy"}`: "apply the configured
// policy" (the session override, the `[[failover]]` rules, the implicit
// stop). So a wrapper decides what it cares about and hands the rest to
// `next`; with no override the seam never builds the JSON at all
// (`services::overridden`), and failover behaves exactly as before.

/// The point's name.
pub const POINT: &str = "failover.decide";

/// How many calls one chain makes at most, retries and waits included,
/// before it gives up: an override that keeps answering `retry` must not
/// hold a turn forever.
pub const MAX_TRIES: u32 = 32;

/// The longest `wait` an override may ask for, in seconds.
pub const MAX_WAIT_SECS: u64 = 3600;

/// The point as declared: its summary, what a provider is handed and
/// what it must answer. Part of the published extension API.
pub fn point_decl() -> crate::services::PointDecl {
    let route = serde_json::json!({
        "type": "object",
        "required": ["provider", "model"],
        "properties": {
            "provider": {"type": "string"},
            "model": {"type": "string"}
        }
    });
    crate::services::PointDecl {
        name: POINT.into(),
        summary: "what to do after a call to a model route failed: retry it, wait and \
                  retry, try the next route, go to a named route, stop, ask the user, \
                  or apply the configured policy"
            .into(),
        input: serde_json::json!({
            "type": "object",
            "required": ["session_id", "interactive", "attempt", "hops", "route", "error",
                         "core_would_fall_back", "remaining_routes", "config"],
            "properties": {
                "session_id": {"type": "string"},
                "interactive": {"type": "boolean",
                    "description": "a person can answer an ask in this session"},
                "attempt": {"type": "integer",
                    "description": "retries of this route so far (0 on its first failure)"},
                "hops": {"type": "integer",
                    "description": "routes left behind so far in this chain"},
                "route": route,
                "error": {
                    "type": "object",
                    "required": ["kind", "message"],
                    "properties": {
                        "kind": {"enum": ["rate_limited", "server", "network", "client", "truncated"]},
                        "status": {"type": ["integer", "null"]},
                        "message": {"type": "string"}
                    }
                },
                "core_would_fall_back": {"type": "boolean",
                    "description": "the error is one core falls back on (429, 5xx, network)"},
                "remaining_routes": {"type": "array", "items": route,
                    "description": "the routes after this one, in chain order"},
                "config": {
                    "type": "object",
                    "description": "what the configured policy says for this route",
                    "properties": {
                        "mode": {"enum": ["ask", "auto", "stop", "script"]},
                        "source": {"type": "string"},
                        "max_hops": {"type": ["integer", "null"]},
                        "script": {"type": ["string", "null"]}
                    }
                }
            }
        }),
        output: serde_json::json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {"enum": ["advance", "retry", "wait", "goto", "stop", "ask", "policy"]},
                "seconds": {"type": "integer",
                    "description": "for `wait`: how long, at most 3600"},
                "provider": {"type": "string", "description": "for `goto`: a configured route"},
                "model": {"type": "string",
                    "description": "for `goto`: its model (empty: the provider's first route)"},
                "reason": {"type": "string", "description": "shown with the decision"}
            }
        }),
        on_error: crate::services::OnError::FallThrough,
        deadline_ms: 30_000,
    }
}

/// Declare the point once per process, with core's default. Called before
/// anything asks whether it is overridden, and before any plugin's
/// registration of the name lands (`services`), so neither load order nor
/// a missed boot step can hide an override.
pub fn ensure_point() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Core's own: never purged, whatever thread happens to be first
        // (a module registering the name can be what triggers this).
        let declared = crate::services::declare_core_point(
            point_decl(),
            crate::services::handler(|_input, _cx| async {
                Ok(serde_json::json!({"action": "policy"}))
            }),
        );
        if let Err(e) = declared {
            tracing::error!("failover: the `{POINT}` point could not be declared: {e}");
        }
    });
}

/// The error class the point's input names.
pub fn error_kind(e: &crate::providers::ProviderError) -> &'static str {
    use crate::providers::ProviderError as E;
    match e {
        E::RateLimited(_) => "rate_limited",
        E::Server { .. } => "server",
        E::Network(_) => "network",
        E::Client { .. } => "client",
        E::Truncated(_) => "truncated",
    }
}

/// The HTTP status behind an error, when there is one.
pub fn error_status(e: &crate::providers::ProviderError) -> Option<u16> {
    use crate::providers::ProviderError as E;
    match e {
        E::RateLimited(_) => Some(429),
        E::Server { status, .. } | E::Client { status, .. } => Some(*status),
        E::Network(_) | E::Truncated(_) => None,
    }
}

/// One answer of the point, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Advance,
    Retry,
    Wait(u64),
    Goto {
        provider: String,
        model: String,
    },
    Stop(String),
    Ask,
    /// Apply the configured policy (core's default answer).
    Policy,
}

/// Parse an answer. The schema check already held `action` to its enum;
/// this reads the arguments each action needs.
pub fn parse_verdict_json(v: &serde_json::Value) -> Result<Verdict, String> {
    let reason = v["reason"].as_str().unwrap_or("").to_string();
    Ok(match v["action"].as_str().unwrap_or("") {
        "advance" => Verdict::Advance,
        "retry" => Verdict::Retry,
        "wait" => {
            let secs = v["seconds"]
                .as_u64()
                .ok_or_else(|| format!("`wait` needs `seconds`: {v}"))?;
            Verdict::Wait(secs.min(MAX_WAIT_SECS))
        }
        "goto" => Verdict::Goto {
            provider: v["provider"]
                .as_str()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| format!("`goto` needs `provider`: {v}"))?
                .to_string(),
            model: v["model"].as_str().unwrap_or("").to_string(),
        },
        "stop" => Verdict::Stop(reason),
        "ask" => Verdict::Ask,
        "policy" => Verdict::Policy,
        other => return Err(format!("unknown action `{other}`")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FailoverRule;

    fn rule(p: Option<&str>, m: Option<&str>, mode: &str) -> FailoverRule {
        FailoverRule {
            provider: p.map(String::from),
            model: m.map(String::from),
            mode: Some(mode.into()),
            script: None,
            max_hops: None,
            wait_default_secs: None,
            aux_fallback_provider: None,
            aux_fallback_model: None,
        }
    }

    #[test]
    fn specificity_order_and_config_order() {
        let rules = vec![
            rule(Some("ali-personal"), None, "ask"),    // provider-only
            rule(Some("zai"), Some("glm-5.3"), "auto"), // exact pair
            rule(None, None, "stop"),                   // catch-all
        ];
        // Exact pair beats earlier provider-only rule.
        let d = resolve("zai", "glm-5.3", None, &rules, true);
        assert_eq!(d.mode, Mode::Auto);
        assert_eq!(
            d.source,
            "config[provider=Some(\"zai\"),model=Some(\"glm-5.3\")]"
        );
        // Provider-only applies to other models of that provider.
        assert_eq!(
            resolve("ali-personal", "other", None, &rules, true).mode,
            Mode::Ask
        );
        // Catch-all takes everything else.
        assert_eq!(
            resolve("openrouter", "x", None, &rules, false).mode,
            Mode::Stop
        );
    }

    #[test]
    fn session_override_beats_config() {
        let rules = vec![rule(None, None, "stop")];
        let d = resolve("p", "m", Some("auto"), &rules, true);
        assert_eq!(d.mode, Mode::Auto);
        assert_eq!(d.source, "session");
    }

    #[test]
    fn implicit_is_stop_when_no_rule_or_override_names_a_mode() {
        // Failover is DISABLED by default (USER 2026-09-05): with no
        // [[failover]] rule and no session override, a failed route
        // errors immediately — for human AND headless alike.
        assert_eq!(resolve("p", "m", None, &[], true).mode, Mode::Stop);
        assert_eq!(resolve("p", "m", None, &[], false).mode, Mode::Stop);
    }

    #[test]
    fn verdict_parsing() {
        assert_eq!(parse_verdict("retry"), Some(AskVerdict::RetrySame));
        assert_eq!(parse_verdict("1"), Some(AskVerdict::RetrySame));
        assert_eq!(parse_verdict("n"), Some(AskVerdict::AdvanceOne));
        assert_eq!(parse_verdict("2"), Some(AskVerdict::AdvanceOne));
        assert_eq!(parse_verdict("stop"), Some(AskVerdict::Stop));
        assert_eq!(parse_verdict("3"), Some(AskVerdict::Stop));
        assert_eq!(parse_verdict("what?"), None);
    }

    #[test]
    fn prompt_names_next_route_when_present() {
        let p = ask_prompt("a", "m1", "429", Some(("b", "m2")));
        assert!(p.contains("b (m2)"));
        let p = ask_prompt("a", "m1", "429", None);
        assert!(p.contains("no further routes"));
    }

    #[test]
    fn a_script_rule_carries_its_script_name() {
        let mut r = rule(Some("a"), Some("m"), "script");
        r.script = Some("retry-401".into());
        let d = resolve("a", "m", None, &[r], false);
        assert_eq!(d.mode, Mode::Script);
        assert_eq!(d.script.as_deref(), Some("retry-401"));

        // No script on the rule → the decision is None (the default hook).
        let d = resolve(
            "a",
            "m",
            None,
            &[rule(Some("a"), Some("m"), "script")],
            false,
        );
        assert_eq!(d.script, None);
    }

    #[test]
    fn script_names_are_validated() {
        assert!(is_valid_script_name("retry-401"));
        assert!(is_valid_script_name("my_script"));
        assert!(is_valid_script_name("A-1_b"));
        assert!(!is_valid_script_name(""));
        assert!(!is_valid_script_name("a/b"));
        assert!(!is_valid_script_name("x y"));
        assert!(!is_valid_script_name("../../etc"));
        assert!(!is_valid_script_name(&"x".repeat(65)));
    }
    /// Every action the point's output schema allows parses, with the
    /// arguments it needs; a missing argument is an error, and a wait is
    /// capped.
    #[test]
    fn every_action_the_point_allows_parses() {
        let decl = point_decl();
        let actions = decl.output["properties"]["action"]["enum"]
            .as_array()
            .expect("the action enum")
            .clone();
        assert_eq!(actions.len(), 7, "{actions:?}");
        for a in actions {
            let mut v = serde_json::json!({ "action": a });
            if a == "wait" {
                v["seconds"] = serde_json::json!(5);
            }
            if a == "goto" {
                v["provider"] = serde_json::json!("p");
            }
            assert!(parse_verdict_json(&v).is_ok(), "{v}");
        }
        assert_eq!(
            parse_verdict_json(&serde_json::json!({"action": "wait", "seconds": 999_999})),
            Ok(Verdict::Wait(MAX_WAIT_SECS))
        );
        assert!(parse_verdict_json(&serde_json::json!({"action": "wait"})).is_err());
        assert!(parse_verdict_json(&serde_json::json!({"action": "goto"})).is_err());
        assert_eq!(
            parse_verdict_json(&serde_json::json!({"action": "stop", "reason": "r"})),
            Ok(Verdict::Stop("r".into()))
        );
    }

    /// The point is declared once, as core's (never purged), and core's
    /// own answer is "apply the configured policy".
    #[tokio::test]
    async fn core_answers_its_own_point_with_the_configured_policy() {
        use crate::providers::ProviderError;
        ensure_point();
        ensure_point();
        assert!(crate::services::is_point(POINT));
        assert_eq!(
            crate::services::purge_owner(None),
            0,
            "core is never purged"
        );
        // Other tests' overrides pass a session-less call through.
        let v = crate::services::ask(POINT, serde_json::json!({}), None)
            .await
            .expect("core answers");
        assert_eq!(v["action"], "policy", "{v}");
        let errs = [
            ProviderError::RateLimited("x".into()),
            ProviderError::Server {
                status: 502,
                body: String::new(),
            },
            ProviderError::Network("x".into()),
            ProviderError::Client {
                status: 400,
                body: String::new(),
            },
            ProviderError::Truncated("x".into()),
        ];
        let kinds: Vec<&str> = errs.iter().map(error_kind).collect();
        let allowed =
            point_decl().input["properties"]["error"]["properties"]["kind"]["enum"].clone();
        for k in &kinds {
            assert!(
                allowed.as_array().unwrap().contains(&serde_json::json!(k)),
                "{k}"
            );
        }
        assert_eq!(error_status(&errs[0]), Some(429));
        assert_eq!(error_status(&errs[1]), Some(502));
        assert_eq!(error_status(&errs[2]), None);
    }
}

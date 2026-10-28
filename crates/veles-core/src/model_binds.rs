//! Which route a NEW session starts on, by who and where.
//!
//! `[model] default` is the route for everybody, and `/model` is the
//! route for one session that somebody chose by hand. Between them sat
//! the case the operator actually has and could not express: *strangers
//! and regulars in a channel get the cheap model; my own sessions do
//! not* — without anybody having to remember to type `/model`.
//!
//! ```toml
//! [[model.binds]]
//! platform = "irc"
//! role     = "<=trusted"      # everyone at or below trusted
//! provider = "ali-token-plan-team"
//! model    = "qwen3.8-flash"
//! ```
//!
//! ## Three things that are easy to get wrong here
//!
//! **It applies at session CREATION, not per turn.** "The default for
//! new sessions" is what was asked for, and it is also the only place it
//! can go without fighting `/model`: a per-turn application would
//! silently undo an explicit choice on the next message.
//!
//! **It must run after the fork.** `frontends::fork` carries the
//! previous session's overrides forward, so a bind applied before it is
//! overwritten by the session it was supposed to replace.
//!
//! **It never overrides an explicit choice.** A session that already has
//! a model override keeps it — a bind FILLS an empty slot. Otherwise
//! `/model` would last until the next `/new` and no further, which is
//! the opposite of what a session-scoped override means.
//!
//! ## It announces itself
//!
//! A route nobody chose, that changes what the turn costs, is exactly
//! the kind of implicit action the notes doctrine exists to surface —
//! so [`note_for`] builds the `Note` the first turn carries. USER, on
//! the general rule: *"any guards and other implicit mechanisms should
//! actually always be explicitly shown in the transcript"*.

use crate::acl::{Principal, SenderContext};
use crate::config::{ModelBind, VelesConfig};

/// Storage prefix for runtime binds, so `/defaults bind` works without
/// editing a file. Mirrors `sandboxes::BIND_PREFIX`.
pub const BIND_PREFIX: &str = "modelbind:";

fn selector(b: &ModelBind) -> crate::selector::Selector<'_> {
    crate::selector::Selector {
        identity: b.identity.as_deref(),
        network: b.network.as_deref(),
        channel: b.channel.as_deref(),
        platform: b.platform.as_deref(),
        role: b.role.as_deref(),
    }
}

/// Does this rule actually name a route?
///
/// A bind with neither provider nor model matches and then changes
/// nothing, which reads from the outside as the bind not working.
fn names_a_route(b: &ModelBind) -> bool {
    b.provider.as_deref().is_some_and(|s| !s.trim().is_empty())
        || b.model.as_deref().is_some_and(|s| !s.trim().is_empty())
}

/// Every bind: config first, then runtime rows.
///
/// Order is load-bearing — `selector::most_specific` keeps the FIRST on
/// a tie, so an equally specific runtime bind does not displace a
/// declared one without saying so.
pub fn binds(
    cfg: &VelesConfig,
    store: Option<&std::sync::Arc<std::sync::Mutex<crate::store::Store>>>,
) -> Vec<ModelBind> {
    let mut out: Vec<ModelBind> = cfg
        .model
        .binds
        .iter()
        .filter(|b| names_a_route(b))
        .cloned()
        .collect();
    out.extend(runtime_binds(store));
    out
}

fn runtime_binds(
    store: Option<&std::sync::Arc<std::sync::Mutex<crate::store::Store>>>,
) -> Vec<ModelBind> {
    let Some(store) = store else {
        return Vec::new();
    };
    let Ok(g) = store.lock() else {
        return Vec::new();
    };
    g.meta_scan_prefix(BIND_PREFIX)
        .into_iter()
        .filter_map(|k| serde_json::from_str::<ModelBind>(&g.meta_get(&k)?).ok())
        .filter(names_a_route)
        .collect()
}

/// The key a runtime bind is stored under — its selectors, so re-binding
/// the same selectors REPLACES rather than accumulating.
pub fn bind_key(b: &ModelBind) -> String {
    let part = |o: &Option<String>| o.as_deref().unwrap_or("*").to_ascii_lowercase();
    format!(
        "{}|{}|{}|{}|{}",
        part(&b.identity),
        part(&b.network),
        part(&b.channel),
        part(&b.platform),
        part(&b.role)
    )
}

/// The bind in force for this caller, here — or `None`.
pub fn resolve(
    cfg: &VelesConfig,
    principal: &Principal,
    ctx: &SenderContext,
    store: Option<&std::sync::Arc<std::sync::Mutex<crate::store::Store>>>,
) -> Option<ModelBind> {
    crate::selector::most_specific(binds(cfg, store), principal, ctx, selector)
}

/// The `/model` argument a bind resolves to, in the UNAMBIGUOUS
/// spelling.
///
/// `model@provider` and not `provider/model`, because this string goes
/// to `set_model` unparsed and `@` cannot occur in a model id — see
/// `Agent::set_model_inner` step 3.5 for why the other spelling would be
/// a guess. A bind naming only one half hands over just that half, which
/// `set_model` already understands (`custom:<provider>` switches the
/// provider and keeps its default model; a bare name is a model on the
/// current one).
pub fn model_arg(b: &ModelBind) -> Option<String> {
    let p = b
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let m = b.model.as_deref().map(str::trim).filter(|s| !s.is_empty());
    match (p, m) {
        (Some(p), Some(m)) => Some(format!("{m}@{p}")),
        (Some(p), None) => Some(format!("custom:{p}")),
        (None, Some(m)) => Some(m.to_string()),
        (None, None) => None,
    }
}

/// One line naming which rule chose this route, for the transcript.
pub fn note_for(b: &ModelBind, resolved: &str) -> crate::notes::EngineNote {
    let mut scope: Vec<String> = Vec::new();
    for (label, v) in [
        ("identity", &b.identity),
        ("network", &b.network),
        ("channel", &b.channel),
        ("platform", &b.platform),
        ("role", &b.role),
    ] {
        if let Some(x) = v.as_deref().filter(|s| !s.trim().is_empty()) {
            scope.push(format!("{label}={x}"));
        }
    }
    let where_ = if scope.is_empty() {
        "every session".to_string()
    } else {
        scope.join(" ")
    };
    crate::notes::EngineNote::new(
        crate::notes::NoteKind::Route,
        format!("route from a model bind ({where_}): {resolved}"),
    )
    .with_detail(format!(
        "This session started on {resolved} because a [[model.binds]] rule \
         matching {where_} names it. `/model <name>` overrides it for this \
         session; `/defaults binds` lists the rules."
    ))
}

/// Everything wrong with the bind configuration, one line each.
///
/// Returned rather than logged, the way `aliases::problems` is: a rule
/// that silently never fires is the failure mode this whole area keeps
/// producing, and the old sandbox matcher's exact-role comparison is the
/// worked example — `role = "<=trusted"` parsed as nothing and matched
/// nobody, in a file, forever.
pub fn problems(cfg: &VelesConfig) -> Vec<String> {
    let mut out = Vec::new();
    for (i, b) in cfg.model.binds.iter().enumerate() {
        let n = i + 1;
        if !names_a_route(b) {
            out.push(format!(
                "[[model.binds]] #{n} names no provider and no model — it would \
                 match and change nothing"
            ));
        }
        if let Some(r) = b.role.as_deref().filter(|s| !s.trim().is_empty()) {
            if !crate::selector::role_spec_is_valid(r) {
                out.push(format!(
                    "[[model.binds]] #{n}: '{r}' is not a role — use banned, guest, \
                     user, trusted, admin or owner, optionally with <=, >=, < or >"
                ));
            }
        }
        if let Some(p) = b.provider.as_deref().filter(|s| !s.trim().is_empty()) {
            if cfg.provider(p).is_none() {
                let known: Vec<&str> = cfg
                    .custom_providers
                    .iter()
                    .map(|x| x.name.as_str())
                    .collect();
                out.push(format!(
                    "[[model.binds]] #{n}: provider '{p}' is not configured — known: {}",
                    known.join(", ")
                ));
            }
        }
    }
    out
}

/// Render the binds for `/defaults binds`.
pub fn listing(
    cfg: &VelesConfig,
    store: Option<&std::sync::Arc<std::sync::Mutex<crate::store::Store>>>,
) -> String {
    let all = binds(cfg, store);
    if all.is_empty() {
        return "no model binds — every new session starts on [model] default\n\
                add one with /defaults bind platform=irc role=<=trusted model=… provider=…"
            .into();
    }
    let mut out = String::from("model binds (most specific wins):\n");
    for b in &all {
        let mut sel: Vec<String> = Vec::new();
        for (label, v) in [
            ("identity", &b.identity),
            ("network", &b.network),
            ("channel", &b.channel),
            ("platform", &b.platform),
            ("role", &b.role),
        ] {
            if let Some(x) = v.as_deref().filter(|s| !s.trim().is_empty()) {
                sel.push(format!("{label}={x}"));
            }
        }
        let where_ = if sel.is_empty() {
            "(any)".to_string()
        } else {
            sel.join(" ")
        };
        out.push_str(&format!(
            "  {where_}  →  {}\n",
            model_arg(b).unwrap_or_else(|| "(no route)".into())
        ));
    }
    for p in problems(cfg) {
        out.push_str(&format!("  ! {p}\n"));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::Role;
    use crate::config::{ModelSection, ProviderEntry};

    fn cfg_with(binds: Vec<ModelBind>) -> VelesConfig {
        VelesConfig {
            model: ModelSection {
                provider: Some("ali-team".into()),
                default: Some("flash".into()),
                binds,
                ..Default::default()
            },
            custom_providers: vec![
                ProviderEntry {
                    name: "ali-team".into(),
                    base_url: "http://127.0.0.1:0/v1".into(),
                    ..Default::default()
                },
                ProviderEntry {
                    name: "cheap".into(),
                    base_url: "http://127.0.0.1:2/v1".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn bind(role: Option<&str>, platform: Option<&str>, provider: &str, model: &str) -> ModelBind {
        ModelBind {
            role: role.map(str::to_string),
            platform: platform.map(str::to_string),
            provider: Some(provider.into()),
            model: Some(model.into()),
            ..Default::default()
        }
    }

    fn ctx() -> SenderContext {
        SenderContext {
            mask: Some("someone!~s@example.net".into()),
            network: Some("irc.synirc.net".into()),
            channel: Some("#flashsupport".into()),
            platform: Some("irc".into()),
            chat_id: Some("#flashsupport".into()),
            direct: false,
            handle: None,
            post: None,
        }
    }

    /// The case the feature exists for: everyone at or below trusted on
    /// the cheap route, the operator untouched.
    #[test]
    fn a_role_range_bind_catches_the_channel_and_not_the_operator() {
        let cfg = cfg_with(vec![bind(
            Some("<=trusted"),
            Some("irc"),
            "cheap",
            "cheap-model",
        )]);
        for r in [Role::Guest, Role::User, Role::Trusted] {
            let b = resolve(&cfg, &Principal::known("irc:x", r), &ctx(), None)
                .unwrap_or_else(|| panic!("{r:?} must match"));
            assert_eq!(model_arg(&b).as_deref(), Some("cheap-model@cheap"));
        }
        for r in [Role::Admin, Role::Owner] {
            assert!(
                resolve(&cfg, &Principal::known("irc:x", r), &ctx(), None).is_none(),
                "{r:?} is above the range and keeps [model] default"
            );
        }
    }

    /// The route goes out in the spelling that cannot be misread — see
    /// `Agent::set_model_inner` step 3.5, and the model ids with slashes
    /// in them that make `provider/model` a guess.
    #[test]
    fn a_bind_hands_over_the_unambiguous_spelling() {
        let both = ModelBind {
            provider: Some("openrouter".into()),
            model: Some("openrouter/free".into()),
            ..Default::default()
        };
        assert_eq!(
            model_arg(&both).as_deref(),
            Some("openrouter/free@openrouter"),
            "a slashed model id must survive, which only the @ form does"
        );
        let only_provider = ModelBind {
            provider: Some("cheap".into()),
            ..Default::default()
        };
        assert_eq!(model_arg(&only_provider).as_deref(), Some("custom:cheap"));
        let only_model = ModelBind {
            model: Some("m".into()),
            ..Default::default()
        };
        assert_eq!(model_arg(&only_model).as_deref(), Some("m"));
        assert!(model_arg(&ModelBind::default()).is_none());
    }

    /// A rule that matches and changes nothing is worse than no rule: it
    /// looks configured. Dropped from the table AND reported.
    #[test]
    fn a_bind_naming_no_route_is_dropped_and_reported() {
        let empty = ModelBind {
            platform: Some("irc".into()),
            ..Default::default()
        };
        let cfg = cfg_with(vec![empty]);
        assert!(
            resolve(&cfg, &Principal::known("irc:x", Role::User), &ctx(), None).is_none(),
            "it must not match"
        );
        let p = problems(&cfg);
        assert!(
            p.iter()
                .any(|x| x.contains("names no provider and no model")),
            "{p:?}"
        );
    }

    /// The silent no-op the shared matcher was extracted to kill — now
    /// loud on this side too.
    #[test]
    fn an_unparseable_role_is_reported_rather_than_matching_nobody_quietly() {
        let cfg = cfg_with(vec![bind(Some("<=nonsense"), None, "cheap", "m")]);
        assert!(resolve(&cfg, &Principal::known("irc:x", Role::User), &ctx(), None).is_none());
        let p = problems(&cfg);
        assert!(p.iter().any(|x| x.contains("is not a role")), "{p:?}");
        // …and the valid range spelling is NOT reported.
        let ok = cfg_with(vec![bind(Some("<=trusted"), None, "cheap", "m")]);
        assert!(problems(&ok).is_empty(), "{:?}", problems(&ok));
    }

    #[test]
    fn an_unconfigured_provider_in_a_bind_is_reported() {
        let cfg = cfg_with(vec![bind(None, None, "nosuch", "m")]);
        let p = problems(&cfg);
        assert!(p.iter().any(|x| x.contains("not configured")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("ali-team")), "{p:?}");
    }

    #[test]
    fn the_most_specific_bind_wins() {
        let cfg = cfg_with(vec![
            bind(None, None, "cheap", "wide"),
            bind(None, Some("irc"), "cheap", "on-irc"),
            ModelBind {
                channel: Some("#flashsupport".into()),
                provider: Some("cheap".into()),
                model: Some("in-that-room".into()),
                ..Default::default()
            },
        ]);
        let b = resolve(&cfg, &Principal::known("irc:x", Role::User), &ctx(), None).expect("match");
        assert_eq!(b.model.as_deref(), Some("in-that-room"));
    }

    #[test]
    fn the_note_names_the_scope_that_chose_the_route() {
        let b = bind(Some("<=trusted"), Some("irc"), "cheap", "m");
        let n = note_for(&b, "cheap/m");
        assert!(n.summary.contains("cheap/m"), "{}", n.summary);
        assert!(n.summary.contains("role=<=trusted"), "{}", n.summary);
        assert!(n.summary.contains("platform=irc"), "{}", n.summary);
        assert!(
            n.detail.contains("/model"),
            "the note must name the way out"
        );
    }

    #[test]
    fn listing_reads_back_and_an_empty_table_says_how_to_add_one() {
        let cfg = cfg_with(vec![bind(Some("<=trusted"), Some("irc"), "cheap", "m")]);
        let l = listing(&cfg, None);
        assert!(l.contains("role=<=trusted"), "{l}");
        assert!(l.contains("m@cheap"), "{l}");
        assert!(listing(&cfg_with(vec![]), None).contains("/defaults bind"));
    }
}

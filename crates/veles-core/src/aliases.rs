//! Command aliases — one config word that is itself a command.
//!
//! `[model_aliases]` has always named a (provider, model) pair, but as an
//! ARGUMENT: `/new`, then `/model team-code`, then the thing you actually
//! wanted to say. Three steps, and the first one is easy to forget, so
//! the prompt lands on whatever route the previous session was using.
//!
//! An alias here IS the command:
//!
//! ```text
//! /qwen-flash summarise this backlog     → a NEW session on that route,
//!                                          running that prompt; the next
//!                                          mention continues it
//! /qwen-image-3-pro a beautiful unicorn  → an image on that route
//! ```
//!
//! ## Where the names come from
//!
//! Two sources, and the split is deliberate:
//!
//! 1. **`[[command_aliases]]`**, declared. The only source that can name
//!    a media kind, because nothing in a provider entry says which of its
//!    models draw and which talk — `[imagegen]`/`[videogen]`/`[asr]` and
//!    `agent.tts_*` express that by POINTING at a route, not by a
//!    capability flag, so there is nothing to derive a media alias from.
//! 2. **Every `[model_aliases]` key**, promoted to a `chat` alias unless
//!    a declared alias already claims the name. Those pairs are already
//!    exactly what a chat alias needs, and restating them would be a
//!    second place to edit.
//!
//! ## Why collisions are an error and not a shadow
//!
//! There are 83 command words today. An alias that quietly shadows one
//! takes a command away with no message, and the person who typed it
//! reads the result as the command misbehaving. `collisions` names both
//! sides; the caller refuses to serve the alias rather than the builtin.

use crate::config::VelesConfig;

/// What an alias makes. `Chat` starts a turn; the rest spend a media
/// budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasKind {
    Chat,
    Image,
    Video,
    Audio,
}

impl AliasKind {
    /// Parse the config spelling. `None` is `Chat` — the common case, and
    /// the one a `[model_aliases]` promotion always is.
    pub fn parse(s: Option<&str>) -> Result<Self, String> {
        match s.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            None | Some("") | Some("chat") | Some("text") => Ok(Self::Chat),
            Some("image") | Some("img") => Ok(Self::Image),
            Some("video") | Some("vid") => Ok(Self::Video),
            Some("audio") | Some("tts") | Some("speech") => Ok(Self::Audio),
            Some(other) => Err(format!(
                "unknown alias kind '{other}' — chat, image, video or audio"
            )),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

/// Where a name came from, so `/commands` can say so and a collision
/// message can point at the right block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasSource {
    /// `[[command_aliases]]`.
    Declared,
    /// Promoted from a `[model_aliases]` key.
    ModelAlias,
}

/// One resolved alias.
#[derive(Debug, Clone)]
pub struct Alias {
    pub name: String,
    pub kind: AliasKind,
    pub provider: String,
    pub model: String,
    pub reasoning: Option<String>,
    pub description: Option<String>,
    pub source: AliasSource,
}

impl Alias {
    /// The route as `/model` would take it. `model@provider` and not
    /// `provider/model`, because `@` cannot occur in a model id and this
    /// string is handed to `set_model` unparsed — see
    /// `Agent::set_model_inner` step 3.5 for why the other spelling would
    /// be a guess here.
    pub fn model_arg(&self) -> String {
        format!("{}@{}", self.model, self.provider)
    }

    /// One line for a listing.
    pub fn summary(&self) -> String {
        let route = format!("{}/{}", self.provider, self.model);
        match &self.description {
            Some(d) => format!("/{} — {} ({}, {})", self.name, d, route, self.kind.label()),
            None => format!("/{} — {} ({})", self.name, route, self.kind.label()),
        }
    }
}

/// Strip a leading `/` and lowercase, so `/QWEN-Flash`, `qwen-flash` and
/// `/qwen-flash` are one name. Command words are ASCII-lowercase by
/// convention everywhere else in the dispatcher.
fn norm(name: &str) -> String {
    name.trim().trim_start_matches('/').to_ascii_lowercase()
}

/// Is this word already a builtin command (or a builtin's bare name)?
///
/// Checks the whole taxonomy — shared, surface-specific and the
/// duplicated session-lifecycle four — because shadowing any of them is
/// the same defect.
pub fn is_builtin(name: &str) -> bool {
    use crate::slash_names::{DUPLICATED_ACROSS_SURFACES, SHARED_COMMANDS, SURFACE_SPECIFIC};
    let n = norm(name);
    SHARED_COMMANDS
        .iter()
        .chain(SURFACE_SPECIFIC)
        .chain(DUPLICATED_ACROSS_SURFACES)
        .any(|c| c.trim_start_matches('/').eq_ignore_ascii_case(&n))
}

/// Every alias this config defines, declared first and promotions after.
///
/// Malformed entries are DROPPED rather than silently coerced, and the
/// reason is reported by [`problems`] — a `kind = "picture"` that quietly
/// became a chat alias would spend a text budget on a drawing prompt.
pub fn table(cfg: &VelesConfig) -> Vec<Alias> {
    let mut out: Vec<Alias> = Vec::new();

    for a in &cfg.command_aliases {
        let name = norm(&a.name);
        if name.is_empty() || a.provider.trim().is_empty() || a.model.trim().is_empty() {
            continue;
        }
        let Ok(kind) = AliasKind::parse(a.kind.as_deref()) else {
            continue;
        };
        if out.iter().any(|x| x.name == name) {
            continue; // first declaration wins; `problems` names the rest
        }
        out.push(Alias {
            name,
            kind,
            provider: a.provider.trim().to_string(),
            model: a.model.trim().to_string(),
            reasoning: a.reasoning.clone(),
            description: a.description.clone(),
            source: AliasSource::Declared,
        });
    }

    // Promotions. Sorted, because a HashMap's order is not stable and a
    // listing that shuffles between runs reads as something changing.
    let mut keys: Vec<&String> = cfg.model_aliases.keys().collect();
    keys.sort();
    for k in keys {
        let name = norm(k);
        if name.is_empty() || out.iter().any(|x| x.name == name) {
            continue;
        }
        let m = &cfg.model_aliases[k];
        if m.provider.trim().is_empty() || m.model.trim().is_empty() {
            continue;
        }
        out.push(Alias {
            name,
            kind: AliasKind::Chat,
            provider: m.provider.trim().to_string(),
            model: m.model.trim().to_string(),
            reasoning: m.reasoning.clone(),
            description: None,
            source: AliasSource::ModelAlias,
        });
    }

    out
}

/// Everything wrong with the alias configuration, each as one line.
///
/// Returned rather than logged so the caller decides how loud to be: the
/// gateway prints these once at boot, `/commands` shows them beside the
/// list, and a test asserts the live config produces none.
pub fn problems(cfg: &VelesConfig) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for a in &cfg.command_aliases {
        let name = norm(&a.name);
        if name.is_empty() {
            out.push("[[command_aliases]] entry with no name".to_string());
            continue;
        }
        if let Err(e) = AliasKind::parse(a.kind.as_deref()) {
            out.push(format!("alias /{name}: {e}"));
        }
        if a.provider.trim().is_empty() {
            out.push(format!("alias /{name}: no provider"));
        }
        if a.model.trim().is_empty() {
            out.push(format!("alias /{name}: no model"));
        }
        if seen.contains(&name) {
            out.push(format!(
                "alias /{name}: declared twice — the first one wins"
            ));
        }
        seen.push(name);
    }

    // Collisions are checked against the FINAL table, so a promotion that
    // shadows a builtin is caught too. `[model_aliases].model` promoting
    // to `/model` is the live example of why.
    for a in table(cfg) {
        if is_builtin(&a.name) {
            let where_ = match a.source {
                AliasSource::Declared => "[[command_aliases]]",
                AliasSource::ModelAlias => "[model_aliases] (promoted)",
            };
            out.push(format!(
                "alias /{} from {where_} collides with the builtin command /{} — \
                 the builtin wins; rename the alias",
                a.name, a.name
            ));
        }
    }

    // A route that names no configured provider is a 404 waiting for the
    // first person who types it. Cheap to check here, impossible to
    // explain later.
    for a in table(cfg) {
        if cfg.provider(&a.provider).is_none() {
            let known: Vec<&str> = cfg
                .custom_providers
                .iter()
                .map(|p| p.name.as_str())
                .collect();
            out.push(format!(
                "alias /{}: provider '{}' is not configured — known: {}",
                a.name,
                a.provider,
                known.join(", ")
            ));
        }
    }

    out
}

/// Resolve a command word to an alias, or `None`.
///
/// A word that names a builtin is never an alias, whatever the config
/// says — the builtin wins, and `problems` is where the operator is told
/// their alias is inert.
pub fn resolve(cfg: &VelesConfig, head: &str) -> Option<Alias> {
    let n = norm(head);
    if n.is_empty() || is_builtin(&n) {
        return None;
    }
    table(cfg).into_iter().find(|a| a.name == n)
}

/// Resolve a word that arrived as a COMMAND — it must carry the leading
/// slash.
///
/// [`resolve`] is deliberately lenient about the slash, because the
/// places that ask "is this a command name?" (IRC's `known_command`, the
/// listings) hold a bare name. The DISPATCH paths are the opposite case
/// and the lenient answer is dangerous there: two of the three command
/// blocks in this tree run over every inbound line, not only over slash
/// lines, so a lenient lookup lets an alias named `fastly` hijack any
/// message whose first word is `fastly` — mint a session, change the
/// route, and run the remainder as a prompt.
///
/// Making that a separate function rather than a rule to remember at
/// each call site is the point: it was already forgotten twice while
/// this was being written.
pub fn resolve_command(cfg: &VelesConfig, head: &str) -> Option<Alias> {
    if !head.starts_with('/') {
        return None;
    }
    resolve(cfg, head)
}

/// Is this word an alias? Cheaper than `resolve` for a gate that only
/// needs the yes/no — IRC's `known_command` asks exactly this.
pub fn is_alias(cfg: &VelesConfig, head: &str) -> bool {
    resolve(cfg, head).is_some()
}

/// The listing `/commands` appends, or `None` when nothing is configured.
pub fn listing(cfg: &VelesConfig) -> Option<String> {
    let t = table(cfg);
    if t.is_empty() {
        return None;
    }
    let mut out = String::from("aliases (one word, one route):\n");
    for a in &t {
        out.push_str("  ");
        out.push_str(&a.summary());
        out.push('\n');
    }
    let probs = problems(cfg);
    if !probs.is_empty() {
        out.push_str("problems:\n");
        for p in &probs {
            out.push_str("  ! ");
            out.push_str(p);
            out.push('\n');
        }
    }
    Some(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CommandAlias, ModelAlias, ProviderEntry};

    fn cfg_with(aliases: Vec<CommandAlias>, models: &[(&str, &str, &str)]) -> VelesConfig {
        let mut model_aliases = std::collections::HashMap::new();
        for (k, p, m) in models {
            model_aliases.insert(
                (*k).to_string(),
                ModelAlias {
                    provider: (*p).to_string(),
                    model: (*m).to_string(),
                    ..Default::default()
                },
            );
        }
        VelesConfig {
            command_aliases: aliases,
            model_aliases,
            custom_providers: vec![
                ProviderEntry {
                    name: "ali-team".into(),
                    base_url: "http://127.0.0.1:0/v1".into(),
                    ..Default::default()
                },
                ProviderEntry {
                    name: "zai".into(),
                    base_url: "http://127.0.0.1:2/v1".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn alias(name: &str, kind: Option<&str>, provider: &str, model: &str) -> CommandAlias {
        CommandAlias {
            name: name.into(),
            kind: kind.map(str::to_string),
            provider: provider.into(),
            model: model.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_declared_alias_resolves_and_carries_its_route() {
        let cfg = cfg_with(
            vec![alias(
                "qwen-flash",
                Some("chat"),
                "ali-team",
                "qwen3.8-flash",
            )],
            &[],
        );
        let a = resolve(&cfg, "/qwen-flash").expect("resolves with the slash");
        assert_eq!(a.kind, AliasKind::Chat);
        assert_eq!(a.provider, "ali-team");
        assert_eq!(a.model, "qwen3.8-flash");
        assert_eq!(
            a.model_arg(),
            "qwen3.8-flash@ali-team",
            "the route must go to set_model in the UNAMBIGUOUS spelling"
        );
        assert!(resolve(&cfg, "qwen-flash").is_some(), "and without it");
        assert!(resolve(&cfg, "/QWEN-FLASH").is_some(), "and in any case");
        assert!(resolve(&cfg, "/nope").is_none());
    }

    #[test]
    fn every_model_alias_becomes_a_chat_command() {
        let cfg = cfg_with(
            vec![],
            &[
                ("team-code", "ali-team", "kimi-k2.7-code"),
                ("zai", "zai", "glm-5.3"),
            ],
        );
        let a = resolve(&cfg, "/team-code").expect("promoted");
        assert_eq!(a.kind, AliasKind::Chat);
        assert_eq!(a.source, AliasSource::ModelAlias);
        assert_eq!(a.model, "kimi-k2.7-code");
        assert!(resolve(&cfg, "/zai").is_some());
    }

    #[test]
    fn a_declaration_beats_the_promotion_of_the_same_name() {
        // Same word in both sources: the explicit one wins, and it is the
        // one that can name a media kind.
        let cfg = cfg_with(
            vec![alias("zai", Some("image"), "zai", "some-image-model")],
            &[("zai", "zai", "glm-5.3")],
        );
        let a = resolve(&cfg, "/zai").expect("declared");
        assert_eq!(a.kind, AliasKind::Image);
        assert_eq!(a.source, AliasSource::Declared);
        assert_eq!(a.model, "some-image-model");
        assert_eq!(
            table(&cfg).iter().filter(|x| x.name == "zai").count(),
            1,
            "one name is one alias"
        );
    }

    /// The defect this check exists to stop: an alias silently taking a
    /// command away, so the person who typed `/model` gets a turn on some
    /// route instead of the model listing, and reads it as a bug in
    /// `/model`.
    #[test]
    fn an_alias_never_shadows_a_builtin_and_says_so() {
        let cfg = cfg_with(
            vec![alias("model", Some("chat"), "ali-team", "qwen3.8-flash")],
            &[("status", "zai", "glm-5.3")],
        );
        assert!(
            resolve(&cfg, "/model").is_none(),
            "a builtin word is never an alias"
        );
        assert!(
            resolve(&cfg, "/status").is_none(),
            "not even a promoted one"
        );
        let p = problems(&cfg);
        assert!(
            p.iter()
                .any(|x| x.contains("/model") && x.contains("collides")),
            "the declared collision must be reported: {p:?}"
        );
        assert!(
            p.iter()
                .any(|x| x.contains("/status") && x.contains("collides")),
            "and the promoted one: {p:?}"
        );
    }

    #[test]
    fn a_route_naming_no_configured_provider_is_a_problem_not_a_surprise() {
        let cfg = cfg_with(vec![alias("ghost", None, "nosuch", "m")], &[]);
        let p = problems(&cfg);
        assert!(
            p.iter()
                .any(|x| x.contains("/ghost") && x.contains("not configured")),
            "{p:?}"
        );
        assert!(
            p.iter().any(|x| x.contains("ali-team")),
            "and it must name what IS configured: {p:?}"
        );
    }

    #[test]
    fn a_bad_kind_is_dropped_and_reported_never_coerced_to_chat() {
        let cfg = cfg_with(vec![alias("draw", Some("picture"), "ali-team", "m")], &[]);
        assert!(
            resolve(&cfg, "/draw").is_none(),
            "an unparseable kind must not become a chat alias that bills text"
        );
        let p = problems(&cfg);
        assert!(p.iter().any(|x| x.contains("unknown alias kind")), "{p:?}");
    }

    #[test]
    fn the_media_kinds_parse_by_every_spelling() {
        assert_eq!(AliasKind::parse(None).unwrap(), AliasKind::Chat);
        assert_eq!(AliasKind::parse(Some("")).unwrap(), AliasKind::Chat);
        assert_eq!(AliasKind::parse(Some("IMAGE")).unwrap(), AliasKind::Image);
        assert_eq!(AliasKind::parse(Some("img")).unwrap(), AliasKind::Image);
        assert_eq!(AliasKind::parse(Some("vid")).unwrap(), AliasKind::Video);
        assert_eq!(AliasKind::parse(Some("tts")).unwrap(), AliasKind::Audio);
        assert!(AliasKind::parse(Some("nope")).is_err());
    }

    #[test]
    fn a_clean_config_has_no_problems_and_an_empty_one_has_no_listing() {
        let cfg = cfg_with(
            vec![alias(
                "qwen-image",
                Some("image"),
                "ali-team",
                "qwen-image-3.0-pro",
            )],
            &[("team", "ali-team", "deepseek-v4-flash-0731")],
        );
        assert!(problems(&cfg).is_empty(), "{:?}", problems(&cfg));
        let l = listing(&cfg).expect("two aliases");
        assert!(l.contains("/qwen-image"), "{l}");
        assert!(l.contains("/team"), "{l}");
        assert!(!l.contains("problems:"), "a clean config lists none: {l}");
        assert!(listing(&VelesConfig::default()).is_none());
    }

    /// The hole `resolve_command` exists to close.
    ///
    /// Two of the three command blocks in this tree run over EVERY
    /// inbound line, not only over slash lines — the gateway's and the
    /// TUI's. With the lenient lookup, an alias named `fastly` turned
    /// the ordinary sentence "fastly is a CDN" into: mint a session,
    /// change the route, and run "is a CDN" as the prompt. The person
    /// who typed it gets a non-sequitur answer billed to a provider they
    /// did not choose, and nothing in the transcript explains it.
    #[test]
    fn a_bare_word_is_never_a_command_even_when_it_names_an_alias() {
        let cfg = cfg_with(
            vec![alias("fastly", Some("chat"), "ali-team", "m")],
            &[("team", "ali-team", "m2")],
        );
        assert!(
            resolve(&cfg, "fastly").is_some(),
            "the lenient lookup still answers for name questions"
        );
        assert!(
            resolve_command(&cfg, "fastly").is_none(),
            "but a DISPATCH lookup must refuse a word with no slash"
        );
        assert!(
            resolve_command(&cfg, "/fastly").is_some(),
            "and accept the same word with one"
        );
        // The promoted ones too — they reach the same dispatch paths.
        assert!(resolve_command(&cfg, "team").is_none());
        assert!(resolve_command(&cfg, "/team").is_some());
        assert!(resolve_command(&cfg, "").is_none());
        assert!(resolve_command(&cfg, "/").is_none());
    }
}

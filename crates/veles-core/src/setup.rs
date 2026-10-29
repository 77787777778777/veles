//! First-run setup — the ONE implementation every wizard drives.
//!
//! `veles setup` (the headless CLI) and the Tk desktop's Setup… window
//! used to be two writers of the same two files, and they drifted: the
//! 2026-09 round found the CLI emitting `api_key_env` for `key_env`,
//! `bot_token_env` for a token that is env-only by design, and a
//! `[platforms.ircx]` block whose `tls`/`nick`/array-shaped `channels`
//! mapped to nothing (`60786dd`). A fresh install therefore had a
//! provider with no credentials and an IRC block that never turned the
//! adapter on, and no dead-key audit could see it because the keys were
//! in nobody's file yet.
//!
//! The fix is structural, not another review: the block builders, the
//! config emitter and the write all live HERE, in core. The CLI's
//! `run_setup` collects answers on stdin and calls [`setup_plan`] →
//! [`apply`]; the desktop POSTs the same answers to `/api/setup/*`,
//! which calls the same two functions. There is one shape for the TOML,
//! so the two frontends cannot disagree about it.
//!
//! Properties, each load-bearing and each pinned by a test below:
//!
//! 1. **The result loads.** [`setup_plan`] parses its own output and runs
//!    it through the same `finish_loaded` step `config::load` does, so a
//!    field nobody reads (the `60786dd` class) or a value the loader
//!    refuses fails HERE, in a pure function, rather than at the next
//!    gateway boot. The adapter-facing half — do the emitted platform
//!    keys match what the live `veles_irc` / `veles_telegram` connectors
//!    declare they read — cannot live here (core cannot depend on the
//!    adapters), so it stays in `veles-cli`'s `setup_wizard_tests`.
//! 2. **`[model]` is a table, and it routes.** The CLI this replaced
//!    wrote top-level `model = "x"` strings, which do not deserialize.
//!    And a `[model] provider` naming no `[[custom_providers]]` entry
//!    parses fine and then fails every turn with "unknown provider", so
//!    that is refused by name too.
//! 3. **Secrets never land in `config.toml`.** The telegram token is
//!    env-only; the provider key is named by `key_env`, which must look
//!    like a variable NAME — a pasted key is refused, without echoing it.
//! 4. **An existing config is EDITED, not replaced** (USER 2026-09-27,
//!    and PLAN-setup-wizard-tk.md's own "the fresh-file case is the one
//!    place a whole-file emit is fine"). Only the sections the operator
//!    answered change, key by key, through `toml_edit`: comments, key
//!    order and every section the wizard does not own survive, and a
//!    walk through the wizard that changes no answer changes no byte.
//!    The version this replaced swapped the live file for the wizard's
//!    minimal one, and its only backup was the single `config.toml.bak`
//!    every later config edit overwrites.
//! 5. **Skipped is not "no".** A step in [`SetupAnswers::skipped`] is left
//!    out of a fresh config and left exactly as it is in an existing one;
//!    an unticked Telegram box that was NOT skipped turns the adapter off.
//! 6. **The preview is the write.** A plan carries the signature of the
//!    config.toml it was computed from, and [`apply`] refuses
//!    ([`StaleBase`]) when the file has changed since, so the bytes the
//!    operator read are the bytes that land.
//!
//! Notes and refusals go through `crate::i18n`, so the desktop (which
//! sends `X-Veles-Locale`) shows them in the wizard's language.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

/// The Alibaba Model Studio Token Plan's OpenAI-compatible base. The
/// default for [`ProviderChoice::AliTokenPlan`]; an operator on a
/// different region or a self-hosted gateway overrides `base_url`.
pub const ALI_TOKEN_PLAN_BASE_URL: &str =
    "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1";

/// The wizard's steps that own part of config.toml, by the ids both
/// frontends put in [`SetupAnswers::skipped`].
pub const SECTIONS: &[&str] = &["provider", "model", "telegram", "irc", "acl", "locale"];

// ── the answers ──────────────────────────────────────────────────────

/// Which LLM route the wizard wires up. Every choice but the last two
/// is a row of [`PROVIDER_PRESETS`]; the serialized names are what the
/// desktop API and `veles install profile` answers carry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderChoice {
    /// OpenRouter — one key, every major model. The default choice (a
    /// `{}` body from the API means this) because a first-run user with
    /// any one key there gets a working route.
    #[default]
    #[serde(rename = "openrouter")]
    OpenRouter,
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "gemini")]
    Gemini,
    #[serde(rename = "deepseek")]
    DeepSeek,
    /// A local Ollama server: no key, the models are whatever was pulled.
    #[serde(rename = "ollama")]
    Ollama,
    /// Alibaba Model Studio's Token Plan.
    #[serde(rename = "ali_token_plan")]
    AliTokenPlan,
    /// Any other OpenAI-compatible endpoint (vLLM, llama.cpp, a gateway).
    /// Explicit rename: derive's snake_case would say `open_ai_compatible`.
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
    /// Write no provider entry. In an existing config, leave the
    /// `[[custom_providers]]` it has exactly as they are.
    #[serde(rename = "skip")]
    Skip,
}

/// One row of the provider step: everything a first run needs to wire a
/// provider without knowing its base URL or what its key variable is
/// usually called. Both wizards (and `/api/setup/state`) render from
/// this table, so a new provider is one row here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProviderPreset {
    pub choice: ProviderChoice,
    /// What the step shows. A product name, not translated.
    pub label: &'static str,
    /// The `[[custom_providers]] name` written by default.
    pub entry_name: &'static str,
    pub base_url: &'static str,
    /// Empty: the endpoint takes no key (a local server).
    pub key_env: &'static str,
    pub api_mode: &'static str,
    /// The provider DRIVER a plugin registers, for `api_mode = "plugin"`;
    /// "" for the native wires. The Anthropic row names the bundled Tcl
    /// driver (`plugins::bundled_roots`), so a fresh install reaches the
    /// Messages API with no plugin copied by hand.
    pub driver: &'static str,
    /// A model id the provider keeps stable (an alias where it has one),
    /// or "" when there is none to suggest and the user picks from the
    /// provider's own catalog ([`list_models`]). Catalogs move every
    /// month; this is a starting point, not a pin.
    pub model: &'static str,
    /// Where to get a key ("" for none).
    pub key_url: &'static str,
}

/// The Anthropic preset's base URL: the Messages API's host, which the
/// driver appends `/v1/messages` to.
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

/// The base URL the Anthropic preset wrote before 0.1.2: Anthropic's
/// OpenAI-compatible surface, reached with `api_mode =
/// "chat_completions"`. Still recognised as the Anthropic choice
/// ([`choice_for_base_url`]), so a re-run over such a config shows
/// Anthropic rather than "another OpenAI-compatible server" and keeps
/// the entry's own URL and wire.
pub const ANTHROPIC_OPENAI_COMPAT_BASE_URL: &str = "https://api.anthropic.com/v1";

/// The provider step's choices, in the order the wizards show them.
pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        choice: ProviderChoice::OpenRouter,
        label: "OpenRouter",
        entry_name: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        key_env: "OPENROUTER_API_KEY",
        api_mode: "chat_completions",
        driver: "",
        model: "openrouter/auto",
        key_url: "https://openrouter.ai/keys",
    },
    ProviderPreset {
        choice: ProviderChoice::OpenAi,
        label: "OpenAI",
        entry_name: "openai",
        base_url: "https://api.openai.com/v1",
        key_env: "OPENAI_API_KEY",
        api_mode: "chat_completions",
        driver: "",
        model: "",
        key_url: "https://platform.openai.com/api-keys",
    },
    ProviderPreset {
        choice: ProviderChoice::Anthropic,
        label: "Anthropic",
        entry_name: "anthropic",
        // The NATIVE Messages wire, through the bundled Tcl driver
        // (plugins/examples/tcl-anthropic-driver, shipped under
        // share/veles/plugins). Not Anthropic's OpenAI-compatible
        // surface (`ANTHROPIC_OPENAI_COMPAT_BASE_URL`, what this row
        // wrote until 0.1.2): Anthropic documents that layer as having
        // no prompt caching, and it ignores `reasoning_effort` and never
        // returns thinking. The driver appends `/v1/messages` itself,
        // so the base URL has no path.
        base_url: ANTHROPIC_BASE_URL,
        key_env: "ANTHROPIC_API_KEY",
        api_mode: "plugin",
        driver: "anthropic-messages",
        model: "",
        key_url: "https://console.anthropic.com/settings/keys",
    },
    ProviderPreset {
        choice: ProviderChoice::Gemini,
        label: "Google Gemini",
        entry_name: "gemini",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        key_env: "GEMINI_API_KEY",
        api_mode: "chat_completions",
        driver: "",
        model: "gemini-flash-latest",
        key_url: "https://aistudio.google.com/apikey",
    },
    ProviderPreset {
        choice: ProviderChoice::DeepSeek,
        label: "DeepSeek",
        entry_name: "deepseek",
        base_url: "https://api.deepseek.com/v1",
        key_env: "DEEPSEEK_API_KEY",
        api_mode: "chat_completions",
        driver: "",
        model: "deepseek-chat",
        key_url: "https://platform.deepseek.com/api_keys",
    },
    ProviderPreset {
        choice: ProviderChoice::Ollama,
        label: "Ollama (local)",
        entry_name: "ollama",
        base_url: "http://localhost:11434/v1",
        key_env: "",
        api_mode: "chat_completions",
        driver: "",
        model: "",
        key_url: "",
    },
    ProviderPreset {
        choice: ProviderChoice::AliTokenPlan,
        label: "Alibaba Model Studio (Token Plan)",
        entry_name: "ali-token-plan",
        base_url: ALI_TOKEN_PLAN_BASE_URL,
        key_env: "ALI_TOKEN_PLAN_API_KEY",
        api_mode: "chat_completions",
        driver: "",
        model: "qwen3.8-flash",
        key_url: "https://modelstudio.console.alibabacloud.com/",
    },
    ProviderPreset {
        choice: ProviderChoice::OpenAiCompatible,
        label: "Other OpenAI-compatible server",
        entry_name: "local",
        base_url: "http://localhost:8000/v1",
        key_env: "",
        api_mode: "chat_completions",
        driver: "",
        model: "",
        key_url: "",
    },
];

/// The preset row for a choice (`None` for [`ProviderChoice::Skip`]).
pub fn preset(choice: ProviderChoice) -> Option<&'static ProviderPreset> {
    PROVIDER_PRESETS.iter().find(|p| p.choice == choice)
}

/// The preset an existing provider entry most likely came from, by its
/// base URL; anything unrecognised is "another OpenAI-compatible server".
pub fn choice_for_base_url(base_url: &str) -> ProviderChoice {
    let norm = |s: &str| s.trim_end_matches('/').to_ascii_lowercase();
    if norm(base_url) == norm(ANTHROPIC_OPENAI_COMPAT_BASE_URL) {
        return ProviderChoice::Anthropic;
    }
    PROVIDER_PRESETS
        .iter()
        .find(|p| {
            p.choice != ProviderChoice::OpenAiCompatible && norm(p.base_url) == norm(base_url)
        })
        .map_or(ProviderChoice::OpenAiCompatible, |p| p.choice)
}

/// Where a provider's model catalog is, from its base URL.
///
/// The OpenAI wire's base URLs end in their version (`…/v1`,
/// `…/v1beta/openai`), and the catalog is `{base}/models`. A base URL
/// with NO path is a native wire whose driver appends the version
/// itself — the Anthropic preset's `https://api.anthropic.com`, which
/// the driver turns into `/v1/messages` — so its catalog is
/// `{base}/v1/models`, with `limit=1000`: Anthropic pages that list at
/// 20 by default, and a wizard showing the first page would hide most
/// of the catalog.
pub fn models_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let path = base
        .split_once("://")
        .map_or("", |(_, rest)| rest.find('/').map_or("", |i| &rest[i..]));
    if path.is_empty() {
        format!("{base}/v1/models?limit=1000")
    } else {
        format!("{base}/models")
    }
}

/// The model ids a provider's `/models` lists, for the wizard's model
/// step: sorted, de-duplicated, empty on any failure (the wizard then
/// says so and the user types a name). `key` is the value itself — the
/// wizard has it before anything is stored.
pub async fn list_models(base_url: &str, key: &str) -> Result<Vec<String>> {
    let base = base_url.trim_end_matches('/');
    anyhow::ensure!(
        base.starts_with("https://") || base.starts_with("http://"),
        "the base URL must start with https:// or http://"
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let mut req = client.get(models_url(base));
    if !key.is_empty() {
        // Bearer for the OpenAI wire; Anthropic's /models reads
        // x-api-key, and the extra header is harmless elsewhere.
        req = req
            .bearer_auth(key)
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01");
    }
    let resp = req.send().await.context("the provider did not answer")?;
    let status = resp.status();
    anyhow::ensure!(
        status.is_success(),
        "the provider answered HTTP {} to its model list",
        status.as_u16()
    );
    let body = resp.text().await?;
    let mut ids: Vec<String> = crate::discovery::parse_catalog(&body)
        .into_iter()
        .map(|m| m.id)
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// The ACL posture the wizard leaves the box in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AclChoice {
    /// Leave `[acl]` dormant: the adapter allowlist
    /// (`IRC_ALLOWED_USERS` / `TELEGRAM_ALLOWED_USERS`) IS the trust
    /// boundary, and every sender resolves as owner. Pre-ACL behaviour,
    /// and the default — curated mode is an explicit opt-in. In an
    /// existing config with an `[acl]`, this removes nothing.
    #[default]
    LegacyOpen,
    /// Write the first `[acl]` owner entry. This FLIPS the box into
    /// curated mode (`AclConfig::is_configured` becomes true, the
    /// default role becomes `guest`, fail-closed) — which is the point,
    /// and why the wizard must say so before writing it.
    Curated,
}

/// Everything both wizards collect. `#[serde(default)]` on the struct
/// means the desktop API can accept a partial JSON body and fill the
/// rest from [`Default`], so a screen the operator skipped arrives as
/// its default rather than a deserialization error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SetupAnswers {
    pub provider: ProviderChoice,
    /// OpenAI-compatible endpoint (or an override for the token plan).
    pub base_url: String,
    /// NAME of the env var holding the provider key (`key_env`).
    pub key_env: String,
    /// Wire protocol; empty → the choice's own (`plugin` for Anthropic,
    /// `chat_completions` for the rest).
    pub api_mode: String,
    /// The provider driver for `api_mode = "plugin"`; empty → the
    /// choice's own (`anthropic-messages` for Anthropic). Ignored, and
    /// not written, for any other wire.
    pub driver: String,
    /// `[[custom_providers]] name`. Empty → the choice's default.
    pub provider_name: String,
    /// `[model] default`. Empty → the choice's default.
    pub model: String,
    /// `[model] provider`. Empty → the entry this wizard writes.
    pub default_provider: String,
    pub telegram: bool,
    pub irc_server: String,
    pub irc_port: u16,
    pub irc_use_tls: bool,
    pub irc_nickname: String,
    /// A STRING, space or comma separated (`apply_mapping` reads it with
    /// `as_str`; an array is ignored as silently as a missing key).
    pub irc_channels: String,
    /// SASL account for the bot's nick (`sasl_username`, PLAIN). Empty →
    /// no SASL. Its password is a secret: `IRC_SASL_PASSWORD`, never
    /// config.toml.
    pub irc_sasl_username: String,
    pub acl: AclChoice,
    /// The first owner when [`AclChoice::Curated`]: `telegram:<numeric
    /// id>`, `irc:<services account>`, or a `nick!user@host` mask (which
    /// becomes an owner `[[acl.entries]]` row — `[acl] owner` compares
    /// identities, and a mask there never matches anyone).
    pub acl_owner: String,
    /// `[tui] locale`, the default language. Empty → not written.
    pub locale: String,
    /// Steps the operator SKIPPED (ids from [`SECTIONS`]): left out of a
    /// fresh config, left as they are in an existing one.
    pub skipped: Vec<String>,
}

impl Default for SetupAnswers {
    /// The happy-path first run: OpenRouter, its auto-routing model, no
    /// platform adapters, dormant ACL. Every field a screen would have
    /// set is a sane value, so a `{}` body from the API is a working
    /// config rather than an empty one.
    fn default() -> Self {
        Self {
            provider: ProviderChoice::OpenRouter,
            base_url: String::new(),
            key_env: String::new(),
            api_mode: String::new(),
            driver: String::new(),
            provider_name: String::new(),
            model: String::new(),
            default_provider: String::new(),
            telegram: false,
            irc_server: String::new(),
            irc_port: 6697,
            irc_use_tls: true,
            irc_nickname: String::new(),
            irc_channels: String::new(),
            irc_sasl_username: String::new(),
            acl: AclChoice::LegacyOpen,
            acl_owner: String::new(),
            locale: String::new(),
            skipped: Vec::new(),
        }
    }
}

impl SetupAnswers {
    fn skips(&self, section: &str) -> bool {
        self.skipped.iter().any(|s| s == section)
    }
}

// ── the plan (pure) ──────────────────────────────────────────────────

/// Whether a plan writes a whole new config.toml or edits one in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupMode {
    Fresh,
    Edit,
}

/// The would-be result of a setup run: the two file bodies and what to
/// say about them. Touches NO disk — that is [`apply`]. The desktop's
/// `/api/setup/preview` returns this verbatim (only env-var NAMES appear,
/// never values), so the operator reads the exact bytes that will be
/// written before anything is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupPlan {
    /// The whole `config.toml` body, as it will be after the write.
    pub config_toml: String,
    /// The `.env` body (names the secrets the answers imply, every line
    /// commented). Written only when no `.env` exists yet.
    pub env_text: String,
    /// Env-var NAMES the answers require (provider key, telegram token).
    /// Never values.
    pub secrets: Vec<String>,
    /// Honest notes for the summary screen, already in the request's
    /// language. Each is a sentence the UI shows verbatim.
    pub notes: Vec<String>,
    /// Allowlist var NAMES the `.env` should mention (an enabled chat
    /// adapter with no `[acl]`) — not secrets, but the door.
    pub allowlists: Vec<String>,
    pub mode: SetupMode,
    /// [`base_sig`] of the config.toml text this plan was computed from
    /// ("" when there was none). [`apply`] refuses when the file on disk
    /// no longer matches it.
    pub base_sig: String,
}

/// config.toml changed between the plan and the write.
#[derive(Debug)]
pub struct StaleBase;

impl std::fmt::Display for StaleBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::i18n::lookup(
            "config.toml changed since the preview — refresh the preview and check it again",
            &[],
        ))
    }
}

impl std::error::Error for StaleBase {}

/// A signature of a config.toml text (`None` = no file): what a plan
/// was computed from, compared again at write time.
pub fn base_sig(text: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    match text {
        None => String::new(),
        Some(t) => format!("{:x}", Sha256::digest(t.as_bytes())),
    }
}

/// The provider step, resolved against its choice's defaults.
struct ProviderAnswer {
    name: String,
    base_url: String,
    key_env: String,
    api_mode: String,
    /// Non-empty exactly when `api_mode` is `plugin`.
    driver: String,
    default_model: &'static str,
}

/// The first ACL owner, in the shape the ACL can actually match.
enum Owner {
    /// `telegram:<id>` / `irc:<account>` → `[acl] owner`.
    Id(String),
    /// `nick!user@host` → an owner `[[acl.entries]]` row.
    Mask(String),
}

/// Turn answers into a plan. `existing` is the current config.toml text
/// (`None`, or only whitespace, means a fresh install). Pure: it reads
/// nothing and writes nothing.
pub fn setup_plan(a: &SetupAnswers, existing: Option<&str>) -> Result<SetupPlan> {
    for s in &a.skipped {
        if !SECTIONS.contains(&s.as_str()) {
            bail!(
                "unknown setup step {s:?} in skipped — expected one of {}",
                SECTIONS.join(", ")
            );
        }
    }
    let edit_base = existing.filter(|t| !t.trim().is_empty());
    let mut notes: Vec<String> = Vec::new();
    let mut secrets: Vec<String> = Vec::new();

    let provider = if a.skips("provider") {
        None
    } else {
        provider_answer(a)?
    };
    if let Some(p) = &provider {
        if !p.key_env.is_empty() {
            secrets.push(p.key_env.clone());
        }
    }

    // `[model]`: which entry routes the default model, and the model.
    // Unset, the provider is the entry just written (the ordinary case).
    let pair = if a.skips("model") {
        None
    } else {
        let (prov, model) = match &provider {
            Some(p) => {
                let model = or_default(&a.model, p.default_model);
                if model.is_empty() {
                    // A preset with no stable alias to suggest: writing
                    // the entry without a model leaves a box that cannot
                    // answer, which is exactly what a first run must not do.
                    bail!(crate::i18n::lookup(
                        "choose a default model for %{provider} — the model step can list the provider's models",
                        &[("provider", p.name.clone())]
                    ));
                }
                (or_default(&a.default_provider, &p.name), model)
            }
            None => (
                a.default_provider.trim().to_string(),
                a.model.trim().to_string(),
            ),
        };
        if prov.is_empty() != model.is_empty() {
            // `build_routes` bails with "no model.provider"/"no
            // model.default" on half a pair. Say so rather than write it.
            notes.push(crate::i18n::lookup(
                "the model/provider pair is incomplete — the gateway will not route until both are present",
                &[],
            ));
            None
        } else if prov.is_empty() {
            None
        } else {
            Some((prov, model))
        }
    };

    let owner = if a.skips("acl") || a.acl != AclChoice::Curated {
        None
    } else {
        owner_answer(&a.acl_owner)?
    };
    if !a.skips("irc") && a.irc_port == 0 {
        bail!(crate::i18n::lookup(
            "the IRC port must be a number from 1 to 65535",
            &[]
        ));
    }
    if !a.skips("telegram") && a.telegram {
        // Never config.toml — the vault, the environment or `.env`.
        secrets.push("TELEGRAM_BOT_TOKEN".to_string());
    }
    if !a.skips("irc") && irc_wanted(a) && !a.irc_sasl_username.trim().is_empty() {
        secrets.push(IRC_SASL_PASSWORD.to_string());
    }

    let (cfg, mode) = match edit_base {
        None => (fresh_text(a, &provider, &pair, &owner), SetupMode::Fresh),
        Some(base) => (
            edit_text(base, a, &provider, &pair, &owner)?,
            SetupMode::Edit,
        ),
    };

    // ── the result must load, and route ─────────────────────────────
    // The `60786dd` lesson: anything that GENERATES config is part of the
    // config surface. Parse it back AND run the loader's own validation,
    // so a shape the gateway cannot read is refused here, not at boot.
    let mut parsed: crate::config::VelesConfig = toml::from_str(&cfg).map_err(|e| {
        anyhow::anyhow!("the setup wizard built a config.toml that does not parse: {e}\n---\n{cfg}")
    })?;
    crate::config::finish_loaded(&mut parsed)?;
    if let Some((prov, _)) = &pair {
        if parsed.provider(prov).is_none() {
            bail!(crate::i18n::lookup(
                "[model] provider %{name} names no [[custom_providers]] entry — name the entry this wizard writes, or one config.toml already has",
                &[("name", format!("{prov:?}"))],
            ));
        }
    }

    // ── what the operator must hear ─────────────────────────────────
    if let Some(p) = &provider {
        if p.key_env.is_empty() {
            notes.push(crate::i18n::lookup(
                "no key env var is named for the provider %{name} — it is called without a key",
                &[("name", p.name.clone())],
            ));
        }
    }
    if parsed.custom_providers.is_empty() {
        notes.push(crate::i18n::lookup(
            "no provider is configured — add a [[custom_providers]] entry and set the [model] pair before running a turn",
            &[],
        ));
    }
    let was_curated = edit_base
        .and_then(|b| toml::from_str::<crate::config::VelesConfig>(b).ok())
        .is_some_and(|c| c.acl.is_configured());
    let curated = parsed.acl.is_configured();
    if curated && !was_curated {
        notes.push(crate::i18n::lookup(
            "writing the first [acl] owner switches this gateway into CURATED mode: the default role becomes guest (fail-closed) and the adapter allowlist is no longer the trust boundary",
            &[],
        ));
    }
    if !a.skips("acl") && a.acl == AclChoice::Curated && owner.is_none() && !curated {
        notes.push(crate::i18n::lookup(
            "curated access was chosen with no owner identity, so [acl] stays dormant — give one like telegram:123456 or irc:account to enable it",
            &[],
        ));
    }
    if !a.skips("acl") && a.acl == AclChoice::LegacyOpen && was_curated {
        notes.push(crate::i18n::lookup(
            "config.toml already has an [acl] section, and the wizard leaves it as it is — it never removes access rules",
            &[],
        ));
    }
    // The open door: an enabled chat adapter with no [acl] and (as far as
    // this file can say) no allowlist answers ANYONE who reaches the bot,
    // and a dormant ACL resolves every sender as owner — every tool,
    // shell included. `boot_trust_line` says it at each boot; the wizard
    // says it before the write.
    let mut allowlists: Vec<String> = Vec::new();
    for (platform, var) in [
        ("Telegram", "TELEGRAM_ALLOWED_USERS"),
        ("IRC", "IRC_ALLOWED_USERS"),
    ] {
        let key = platform.to_ascii_lowercase();
        let on = parsed
            .platforms
            .get(key.as_str())
            .and_then(|t| t.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if on && !curated {
            notes.push(crate::i18n::lookup(
                "%{platform} is enabled with no [acl] section: until %{var} in .env names who may talk to the bot, anyone who reaches it is treated as owner, with every tool",
                &[("platform", platform.to_string()), ("var", var.to_string())],
            ));
            allowlists.push(var.to_string());
        }
    }

    secrets.dedup();
    let env_text = env_document(&secrets, &allowlists);
    Ok(SetupPlan {
        config_toml: cfg,
        env_text,
        secrets,
        notes,
        allowlists,
        mode,
        base_sig: base_sig(existing),
    })
}

/// The provider step against its choice's defaults. `None` for
/// [`ProviderChoice::Skip`].
fn provider_answer(a: &SetupAnswers) -> Result<Option<ProviderAnswer>> {
    let Some(p) = preset(a.provider) else {
        return Ok(None);
    };
    let (name, base, key, model) = (p.entry_name, p.base_url, p.key_env, p.model);
    let key_env = or_default(&a.key_env, key);
    if !key_env.is_empty() && !is_env_name(&key_env) {
        // Never echo it: what is in this field is, most likely, the key.
        bail!(crate::i18n::lookup(
            "the API key field takes the NAME of an environment variable (like %{example}), not the key itself — the key goes into the hidden field",
            &[("example", "OPENROUTER_API_KEY".to_string())]
        ));
    }
    let base_url = or_default(&a.base_url, base);
    // The choice's own wire when none was given — but only at the
    // choice's own URL. An Anthropic entry at the pre-0.1.2 OpenAI-
    // compatible URL with no api_mode is a chat_completions entry; the
    // plugin wire there would post to `/v1/v1/messages`.
    let norm = |s: &str| s.trim_end_matches('/').to_ascii_lowercase();
    let wire_default = if norm(&base_url) == norm(base) {
        p.api_mode
    } else {
        "chat_completions"
    };
    let api_mode = or_default(&a.api_mode, wire_default);
    if !crate::config::SUPPORTED_API_MODES.contains(&api_mode.as_str()) {
        bail!(crate::i18n::lookup(
            "unknown api_mode %{mode} — expected one of %{modes}",
            &[
                ("mode", format!("{api_mode:?}")),
                ("modes", crate::config::SUPPORTED_API_MODES.join(", ")),
            ],
        ));
    }
    let driver = if api_mode == "plugin" {
        let d = or_default(&a.driver, p.driver);
        if d.is_empty() {
            // The same refusal build_client would make at boot, made
            // before anything is written. (English, like build_client's:
            // it names config keys, and no wizard row reaches it — every
            // preset with the plugin wire names its driver.)
            bail!(
                "api_mode = \"plugin\" needs `driver = \"<name>\"` naming a driver some plugin \
                 registers. `veles doctor --providers` lists the ones that are loaded."
            );
        }
        d
    } else {
        String::new()
    };
    Ok(Some(ProviderAnswer {
        name: or_default(&a.provider_name, name),
        base_url,
        key_env,
        api_mode,
        driver,
        default_model: model,
    }))
}

/// The owner answer in a shape the ACL can match, or a refusal by name.
fn owner_answer(raw: &str) -> Result<Option<Owner>> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let bare = t.strip_prefix("irc:").unwrap_or(t);
    let spaced = |s: &str| s.chars().any(char::is_whitespace);
    if bare.contains('!') && bare.contains('@') && !spaced(bare) {
        return Ok(Some(Owner::Mask(bare.to_string())));
    }
    let ok = match t.split_once(':') {
        Some(("telegram", id)) => !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()),
        Some(("irc", acct)) => !acct.is_empty() && !spaced(acct) && !acct.contains(['!', '@', ',']),
        Some((platform, id)) => {
            !platform.is_empty()
                && platform
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && !id.is_empty()
                && !spaced(id)
        }
        None => false,
    };
    if !ok {
        bail!(crate::i18n::lookup(
            "owner identity %{id} is not one the ACL can match — use telegram:<numeric id>, irc:<services account>, or a nick!user@host mask",
            &[("id", format!("{t:?}"))],
        ));
    }
    Ok(Some(Owner::Id(t.to_string())))
}

/// A fresh install's whole config.toml.
fn fresh_text(
    a: &SetupAnswers,
    provider: &Option<ProviderAnswer>,
    pair: &Option<(String, String)>,
    owner: &Option<Owner>,
) -> String {
    let mut cfg = String::new();
    cfg.push_str("# veles configuration — generated by the setup wizard.\n");
    cfg.push_str(
        "# Re-run `veles setup` or the desktop's Setup… to change it: an existing\n\
         # file is edited in place, and only the sections you answer change.\n\n",
    );
    if let Some((prov, model)) = pair {
        // A TABLE, not top-level `model = "…"` strings: `model` is a
        // `ModelSection`, and a string there does not deserialize.
        cfg.push_str(&format!(
            "[model]\nprovider = {}\ndefault = {}\n\n",
            q(prov),
            q(model)
        ));
    }
    let locale = a.locale.trim();
    if !a.skips("locale") && !locale.is_empty() {
        cfg.push_str(&format!("[tui]\nlocale = {}\n\n", q(locale)));
    }
    if let Some(p) = provider {
        cfg.push_str(&provider_setup_block(
            &p.name,
            &p.base_url,
            &p.key_env,
            &p.api_mode,
            &p.driver,
        ));
        cfg.push('\n');
    }
    if !a.skips("telegram") && a.telegram {
        cfg.push_str(&telegram_setup_block());
        cfg.push('\n');
    }
    if !a.skips("irc") && irc_wanted(a) {
        cfg.push_str(&irc_block_for(a));
        cfg.push('\n');
    }
    match owner {
        Some(Owner::Id(id)) => {
            cfg.push_str(&acl_setup_block(id));
            cfg.push('\n');
        }
        Some(Owner::Mask(mask)) => {
            cfg.push_str(&acl_owner_mask_block(mask));
            cfg.push('\n');
        }
        None => {}
    }
    // The desktop surface is on by default anyway; written so the file
    // says what the machine does. A2A is NOT: `a2a_enabled` defaults to
    // off, and a wizard that switched on an agent-to-agent endpoint the
    // operator never asked about (the version this replaced did) opened
    // a door in the name of "being explicit".
    cfg.push_str("[platforms.desktop]\nenabled = true\n");
    cfg
}

/// An existing config.toml with only the answered sections changed.
///
/// Every value comes from the SAME block builder the fresh path emits,
/// parsed and merged key by key ([`merge_block`]), so the two paths
/// cannot spell a key differently. A value that already says the same
/// thing is not touched at all — its comments and formatting survive,
/// and a walk that changes no answer changes no byte.
fn edit_text(
    base: &str,
    a: &SetupAnswers,
    provider: &Option<ProviderAnswer>,
    pair: &Option<(String, String)>,
    owner: &Option<Owner>,
) -> Result<String> {
    let mut doc: DocumentMut = base
        .parse()
        .context("config.toml is not valid TOML, so the wizard cannot edit it in place")?;

    if let Some((prov, model)) = pair {
        set_leaf(&mut doc, &["model"], "provider", Value::from(prov.as_str()))?;
        set_leaf(&mut doc, &["model"], "default", Value::from(model.as_str()))?;
    }
    let locale = a.locale.trim();
    if !a.skips("locale") && !locale.is_empty() {
        set_leaf(&mut doc, &["tui"], "locale", Value::from(locale))?;
    }
    if let Some(p) = provider {
        upsert_provider(&mut doc, p)?;
    }
    if !a.skips("telegram") {
        if a.telegram {
            merge_block(
                &mut doc,
                &telegram_setup_block(),
                &["platforms", "telegram"],
            )?;
        } else if has_table(&doc, &["platforms", "telegram"]) {
            // Answered "no" (not skipped): switch it off, keep its settings.
            set_leaf(
                &mut doc,
                &["platforms", "telegram"],
                "enabled",
                Value::from(false),
            )?;
        }
    }
    if !a.skips("irc") {
        if irc_wanted(a) {
            merge_block(&mut doc, &irc_block_for(a), &["platforms", "irc"])?;
            if a.irc_channels.trim().is_empty() {
                // The builder writes no key for "no channels" (only a
                // commented hint), so an emptied field removes the key.
                if let Some(t) = table_at_mut(&mut doc, &["platforms", "irc"]) {
                    t.remove("channels");
                }
            }
        } else if has_table(&doc, &["platforms", "irc"]) {
            set_leaf(
                &mut doc,
                &["platforms", "irc"],
                "enabled",
                Value::from(false),
            )?;
        }
    }
    match owner {
        Some(Owner::Id(id)) => crate::configedit::array_add(&mut doc, &["acl", "owner"], id)?,
        Some(Owner::Mask(mask)) => add_owner_mask(&mut doc, mask)?,
        None => {}
    }
    Ok(doc.to_string())
}

fn irc_wanted(a: &SetupAnswers) -> bool {
    let s = a.irc_server.trim();
    !s.is_empty() && !s.eq_ignore_ascii_case("skip")
}

/// The env var the IRC adapter reads the bot's SASL password from.
pub const IRC_SASL_PASSWORD: &str = "IRC_SASL_PASSWORD";

fn irc_block_for(a: &SetupAnswers) -> String {
    let mut b = irc_setup_block_full(
        a.irc_server.trim(),
        a.irc_port,
        a.irc_use_tls,
        &a.irc_nickname,
        &a.irc_channels,
    );
    let account = a.irc_sasl_username.trim();
    if !account.is_empty() {
        // The keys `apply_mapping` reads; the password is never here.
        b.push_str(&format!(
            "sasl_mechanism = \"PLAIN\"\nsasl_username = {}\n",
            q(account)
        ));
    }
    b
}

// ── toml_edit surgery (edit mode) ────────────────────────────────────

/// Merge the table at `path` in a builder's `block` into `doc`, key by
/// key, through [`set_leaf`].
fn merge_block(doc: &mut DocumentMut, block: &str, path: &[&str]) -> Result<()> {
    let frag: DocumentMut = block.parse().context("a setup block did not parse")?;
    let mut src = frag.as_table();
    for k in path {
        src = src
            .get(k)
            .and_then(Item::as_table)
            .with_context(|| format!("a setup block has no [{}]", path.join(".")))?;
    }
    for (k, item) in src.iter() {
        if let Some(v) = item.as_value() {
            set_leaf(doc, path, k, v.clone())?;
        }
    }
    Ok(())
}

/// Set `key` in the table at `path`, creating the table (and implicit
/// parents) when missing. An equal value is left exactly as it is; a
/// different one takes over the old one's decor, so a trailing comment
/// on the line survives the change.
fn set_leaf(doc: &mut DocumentMut, path: &[&str], key: &str, mut v: Value) -> Result<()> {
    let t = ensure_table(doc, path)?;
    if let Some(old) = t.get(key).and_then(Item::as_value) {
        if same_value(old, &v) {
            return Ok(());
        }
        *v.decor_mut() = old.decor().clone();
    } else {
        v.decor_mut().clear();
    }
    t.insert(key, Item::Value(v));
    Ok(())
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.value() == y.value(),
        (Value::Integer(x), Value::Integer(y)) => x.value() == y.value(),
        (Value::Boolean(x), Value::Boolean(y)) => x.value() == y.value(),
        _ => false,
    }
}

fn ensure_table<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Result<&'a mut Table> {
    let mut t = doc.as_table_mut();
    for (i, k) in path.iter().enumerate() {
        if !t.contains_key(k) {
            let mut nt = Table::new();
            // `[platforms.irc]` without an empty `[platforms]` above it.
            nt.set_implicit(i + 1 < path.len());
            t.insert(k, Item::Table(nt));
        }
        t = t.get_mut(k).and_then(Item::as_table_mut).with_context(|| {
            format!(
                "`{}` in config.toml is not a table the wizard can edit — edit it by hand",
                path[..=i].join(".")
            )
        })?;
    }
    Ok(t)
}

fn table_at_mut<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Option<&'a mut Table> {
    let mut t = doc.as_table_mut();
    for k in path {
        t = t.get_mut(k)?.as_table_mut()?;
    }
    Some(t)
}

fn has_table(doc: &DocumentMut, path: &[&str]) -> bool {
    let mut t = doc.as_table();
    for k in path {
        match t.get(k).and_then(Item::as_table) {
            Some(n) => t = n,
            None => return false,
        }
    }
    true
}

/// Update the `[[custom_providers]]` entry named `p.name` in place, or
/// append it. Only the four keys the wizard asks about are touched —
/// `models`, `context_length`, `extra_body` and anything else the
/// operator wrote into that entry stay.
fn upsert_provider(doc: &mut DocumentMut, p: &ProviderAnswer) -> Result<()> {
    let block = provider_setup_block(&p.name, &p.base_url, &p.key_env, &p.api_mode, &p.driver);
    let frag: DocumentMut = block.parse().context("the provider block did not parse")?;
    let new_entry = frag
        .get("custom_providers")
        .and_then(Item::as_array_of_tables)
        .and_then(|a| a.get(0))
        .cloned()
        .context("the provider block has no [[custom_providers]] entry")?;

    if !doc.contains_key("custom_providers") {
        doc.insert(
            "custom_providers",
            Item::ArrayOfTables(ArrayOfTables::new()),
        );
    }
    let arr = doc
        .get_mut("custom_providers")
        .and_then(Item::as_array_of_tables_mut)
        .context(
            "custom_providers in config.toml is not a list of [[custom_providers]] tables, \
             so the wizard cannot edit it — edit it by hand",
        )?;
    let at = arr
        .iter()
        .position(|t| t.get("name").and_then(Item::as_str) == Some(p.name.as_str()));
    let Some(i) = at else {
        arr.push(new_entry);
        return Ok(());
    };
    let entry = arr.get_mut(i).context("the entry vanished mid-edit")?;
    for (k, item) in new_entry.iter() {
        let Some(v) = item.as_value() else { continue };
        // An empty key_env over an entry that has none stays absent: the
        // entry already resolves its key some other way (VELES_API_KEY),
        // and writing `key_env = ""` would switch that off.
        if k == "key_env" && v.as_str() == Some("") && !entry.contains_key("key_env") {
            continue;
        }
        let mut v = v.clone();
        if let Some(old) = entry.get(k).and_then(Item::as_value) {
            if same_value(old, &v) {
                continue;
            }
            *v.decor_mut() = old.decor().clone();
        } else {
            v.decor_mut().clear();
        }
        entry.insert(k, Item::Value(v));
    }
    Ok(())
}

/// Append an owner `[[acl.entries]]` row for `mask`, unless one exists.
fn add_owner_mask(doc: &mut DocumentMut, mask: &str) -> Result<()> {
    let frag: DocumentMut = acl_owner_mask_block(mask)
        .parse()
        .context("the owner-mask block did not parse")?;
    let row = frag
        .get("acl")
        .and_then(|a| a.get("entries"))
        .and_then(Item::as_array_of_tables)
        .and_then(|a| a.get(0))
        .cloned()
        .context("the owner-mask block has no [[acl.entries]] row")?;
    let acl = ensure_table(doc, &["acl"])?;
    if !acl.contains_key("entries") {
        acl.insert("entries", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let rows = acl
        .get_mut("entries")
        .and_then(Item::as_array_of_tables_mut)
        .context(
            "acl.entries in config.toml is not a list of [[acl.entries]] tables — edit it by hand",
        )?;
    if rows
        .iter()
        .any(|t| t.get("identity").and_then(Item::as_str) == Some(mask))
    {
        return Ok(());
    }
    rows.push(row);
    Ok(())
}

// ── prefill ──────────────────────────────────────────────────────────

/// The answers an existing config.toml already gives, so a re-run
/// starts from what is there (and a walk that changes nothing writes
/// nothing). The provider step shows the entry `[model] provider` names.
pub fn current_answers(existing: &str) -> Result<SetupAnswers> {
    let cfg: crate::config::VelesConfig = toml::from_str(existing)?;
    let mut a = SetupAnswers {
        provider: ProviderChoice::Skip,
        ..SetupAnswers::default()
    };
    a.model = cfg.model.default.clone().unwrap_or_default();
    a.default_provider = cfg.model.provider.clone().unwrap_or_default();
    if let Some(p) = cfg.model.provider.as_deref().and_then(|n| cfg.provider(n)) {
        a.provider = choice_for_base_url(&p.base_url);
        a.provider_name = p.name.clone();
        a.base_url = p.base_url.clone();
        a.key_env = p.key_env.clone().unwrap_or_default();
        // The entry's EFFECTIVE wire: an absent api_mode is
        // chat_completions, and saying so here keeps a wizard from
        // filling the empty field with its choice's own wire — which,
        // for an Anthropic entry at the old OpenAI-compatible URL, is
        // the plugin wire at a URL it cannot use.
        a.api_mode = if p.api_mode.trim().is_empty() {
            "chat_completions".to_string()
        } else {
            p.api_mode.clone()
        };
        a.driver = p.driver.clone().unwrap_or_default();
    }
    let plat = |name: &str, key: &str| cfg.platforms.get(name).and_then(|t| t.get(key)).cloned();
    a.telegram = plat("telegram", "enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if plat("irc", "enabled").and_then(|v| v.as_bool()) == Some(true) {
        let s = |k: &str| {
            plat("irc", k)
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        };
        a.irc_server = s("server");
        a.irc_nickname = s("nickname");
        a.irc_channels = s("channels");
        a.irc_sasl_username = s("sasl_username");
        if let Some(port) = plat("irc", "port").and_then(|v| v.as_u64()) {
            a.irc_port = u16::try_from(port).unwrap_or(6697);
        }
        if let Some(tls) = plat("irc", "use_tls").and_then(|v| v.as_bool()) {
            a.irc_use_tls = tls;
        }
    }
    if cfg.acl.is_configured() {
        a.acl = AclChoice::Curated;
        a.acl_owner = cfg.acl.owner.first().cloned().unwrap_or_default();
    }
    a.locale = cfg.tui.locale.clone().unwrap_or_default();
    Ok(a)
}

/// What an edit changes, as a unified diff ("" when nothing does) — the
/// readable form of an edit-mode preview, where the whole file would bury
/// the three lines that move.
pub fn preview_diff(before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(2)
        .header("config.toml (now)", "config.toml (after)")
        .to_string()
}

// ── apply (the only disk-touching half) ──────────────────────────────

/// What [`apply`] wrote, and what it deliberately did not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SetupReport {
    pub config_path: PathBuf,
    pub mode: SetupMode,
    /// The previous `config.toml`, kept by the same rule every config
    /// edit uses (`configedit`), when there was one.
    pub config_backup: Option<PathBuf>,
    pub env_path: PathBuf,
    /// Whether `.env` was created or had lines added. It is NEVER
    /// rewritten: an existing file only gains commented lines for the
    /// names it does not mention yet, so a live token survives a re-run.
    pub env_written: bool,
    /// The var names those added lines carry.
    pub env_added: Vec<String>,
    /// The secrets the config expects that are STILL unset anywhere
    /// (the vault, the environment, the env files) after this run.
    pub secrets_needed: Vec<String>,
    /// Secret NAMES this run stored in the vault (values it was given).
    pub secrets_stored: Vec<String>,
}

/// config.toml's current text, `None` when there is no file.
pub fn read_existing(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    }
}

/// Write the plan's config.toml — refusing with [`StaleBase`] when the
/// file is no longer the one the plan was computed from — and the `.env`
/// (0600) only when none exists. `dir` is the config directory
/// (`config::config_dir()` in production; a temp dir in tests).
pub fn apply(plan: &SetupPlan, dir: &Path) -> Result<SetupReport> {
    apply_with(plan, dir, &BTreeMap::new())
}

/// [`apply`], storing the secret VALUES this run was given in the vault.
///
/// Order is the contract: the names are checked first (a name the plan
/// does not imply is refused before anything is written), then the
/// config is written (refused on a stale base, before any secret is
/// stored), and only then do the values go into the vault. The `.env`
/// template names only what is still unset afterwards — a commented
/// `# KEY=` beside a key that lives in the vault would read as "missing".
pub fn apply_with(
    plan: &SetupPlan,
    dir: &Path,
    values: &BTreeMap<String, String>,
) -> Result<SetupReport> {
    check_secret_names(plan, values)?;
    std::fs::create_dir_all(dir)?;
    let cfg_path = dir.join("config.toml");
    let env_path = dir.join(".env");

    let current = read_existing(&cfg_path)?;
    if base_sig(current.as_deref()) != plan.base_sig {
        return Err(StaleBase.into());
    }
    let config_backup = crate::configedit::write_with_backup(
        &cfg_path,
        current.as_deref().unwrap_or(""),
        &plan.config_toml,
    )?;

    // The `.env` holds live credentials: created when absent, and
    // otherwise only APPENDED to — commented lines for the names it does
    // not mention yet. "Only when absent" was the first cut, and on a
    // real first run the file is never absent: the gateway mints
    // DESKTOP_BEARER_TOKEN into it at boot, before the wizard runs, so the
    // template (and its open-door line) never landed anywhere.
    let secrets_stored = store_secrets(plan, values)?;
    // The template leaves out what lives in the vault (stored now or
    // before); an existing `.env` is checked line by line below. Not the
    // whole resolver: a process environment is not a file the operator
    // edits, and a template line is a hint, never a value.
    let in_vault = crate::vault::local_values();
    let unset: Vec<String> = plan
        .secrets
        .iter()
        .filter(|n| !in_vault.contains_key(*n))
        .cloned()
        .collect();
    let mut env_added: Vec<String> = Vec::new();
    if !env_path.exists() {
        write_private(&env_path, &env_document(&unset, &plan.allowlists))?;
        env_added.extend(unset.iter().cloned());
        env_added.extend(plan.allowlists.iter().cloned());
    } else {
        let have = std::fs::read_to_string(&env_path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", env_path.display()))?;
        let missing = |vars: &[String]| -> Vec<String> {
            vars.iter()
                .filter(|v| !env_mentions(&have, v))
                .cloned()
                .collect()
        };
        let (s, al) = (missing(&unset), missing(&plan.allowlists));
        if !s.is_empty() || !al.is_empty() {
            use std::io::Write;
            let mut block = String::new();
            if !have.is_empty() && !have.ends_with('\n') {
                block.push('\n');
            }
            block.push_str(
                "\n# added by the setup wizard — uncomment a line and put the value after the `=`.\n",
            );
            block.push_str(&env_lines(&s, &al));
            std::fs::OpenOptions::new()
                .append(true)
                .open(&env_path)
                .and_then(|mut f| f.write_all(block.as_bytes()))
                .map_err(|e| anyhow::anyhow!("appending to {}: {e}", env_path.display()))?;
            env_added.extend(s);
            env_added.extend(al);
        }
    }
    let env_written = !env_added.is_empty();

    Ok(SetupReport {
        config_path: cfg_path,
        mode: plan.mode,
        config_backup,
        env_path,
        env_written,
        env_added,
        secrets_needed: unset
            .into_iter()
            .filter(|n| crate::secrets::resolve_in(n, dir).is_none())
            .collect(),
        secrets_stored,
    })
}

/// Every name in `values` must be one the plan implies (the provider's
/// key, the bot token, the SASL password). The wizard is not a general
/// secret store; `veles secret set` is.
pub fn check_secret_names(plan: &SetupPlan, values: &BTreeMap<String, String>) -> Result<()> {
    for name in values.keys() {
        if !plan.secrets.contains(name) {
            bail!(crate::i18n::lookup(
                "%{name} is not a secret these answers use — only the provider key, the bot token and the SASL password are stored here",
                &[("name", name.clone())],
            ));
        }
    }
    Ok(())
}

/// Store the non-empty values in the vault, each under the policy its
/// name gets from the config just written (a provider key may go to
/// that provider's host; the bot token and the SASL password are
/// engine-only). An empty value keeps whatever is stored already.
fn store_secrets(plan: &SetupPlan, values: &BTreeMap<String, String>) -> Result<Vec<String>> {
    check_secret_names(plan, values)?;
    let cfg: crate::config::VelesConfig = toml::from_str(&plan.config_toml).unwrap_or_default();
    let mut stored = Vec::new();
    for (name, value) in values {
        let v = value.trim();
        if v.is_empty() {
            continue;
        }
        crate::vault::set_local(name, v, crate::secrets::inferred_policy(name, &cfg))?;
        stored.push(name.clone());
    }
    Ok(stored)
}

/// Write a file that holds secrets, readable by its owner only —
/// including when it already existed with looser bits. The mode is set
/// BEFORE the text goes in, so there is no moment when a new file holds
/// a secret at the umask's bits.
pub fn write_private(path: &Path, text: &str) -> Result<()> {
    crate::platform::write_private(path, text.as_bytes())
        .map_err(|e| anyhow::anyhow!("writing {} (owner-only): {e}", path.display()))
}

// ── block builders ───────────────────────────────────────────────────
//
// Each replaced a silent misconfiguration the wizard used to write. The
// comments are the record of what broke; keep them.

/// The `[[custom_providers]]` entry for an OpenAI-compatible endpoint.
///
/// `key_env` is the field `resolve_api_key` reads. The wizard wrote
/// `api_key_env`, which no struct has, so a provider configured through
/// the first-run wizard had a base URL and no key — and what the operator
/// saw was a 401 from their own server rather than a config error naming
/// the field.
///
/// `driver` is written only when non-empty: it is the `api_mode =
/// "plugin"` wire's other half, and the Anthropic preset has had no
/// usable entry without it since that preset moved to the native wire
/// (0.1.2) — `build_client` refuses a plugin entry that names none.
pub fn provider_setup_block(
    name: &str,
    base_url: &str,
    key_env: &str,
    api_mode: &str,
    driver: &str,
) -> String {
    // `key_env` is emitted even when empty: `key_env = ""` means "call
    // this endpoint with no key" (an unauthenticated local server).
    // Leaving the key OUT would mean something else — `resolve_api_key`
    // then falls back to `VELES_API_KEY`.
    let mut block = format!(
        "[[custom_providers]]\nname = {name}\nbase_url = {base_url}\nkey_env = {key_env}\napi_mode = {api_mode}\n",
        name = q(name),
        base_url = q(base_url),
        key_env = q(key_env),
        api_mode = q(api_mode),
    );
    if !driver.is_empty() {
        block.push_str(&format!("driver = {}\n", q(driver)));
    }
    block
}

/// The `[platforms.telegram]` block the wizard writes.
///
/// Just the switch. The token is env-only — `TELEGRAM_BOT_TOKEN` from the
/// environment or `.env`, never a config key — and the block this replaced
/// invented `bot_token_env` to hold the name of a var, so a "configured"
/// telegram platform had no token and no path to one. The wizard now puts
/// the var in the `.env` it writes and says so.
pub fn telegram_setup_block() -> String {
    "[platforms.telegram]\nenabled = true\n".to_string()
}

/// The `[platforms.irc]` block, default port/TLS/nick and a commented
/// channels hint. Kept as the simple shape the CLI's block test drives;
/// the wizard collects the full set and calls [`irc_setup_block_full`].
pub fn irc_setup_block(server: &str) -> String {
    irc_setup_block_full(server, 6697, true, "veles", "")
}

/// The `[platforms.irc]` block with every field the wizard collects.
///
/// Three mistakes, all silent, in the version this replaces: the table was
/// `[platforms.ircx]` — hermes' name for the platform, which nothing in
/// this build reads, since `irc_enabled` asks for `platforms.irc.enabled`
/// — and inside it `tls` and `nick`, where the adapter maps `use_tls` and
/// `nickname`. It also wrote `channels = []`, and `apply_mapping` reads
/// that key with `as_str`, so an array was ignored as quietly as a key
/// that does not exist. A wizard that answers the IRC question has to
/// produce a bot that connects.
pub fn irc_setup_block_full(
    server: &str,
    port: u16,
    use_tls: bool,
    nickname: &str,
    channels: &str,
) -> String {
    let nick = if nickname.trim().is_empty() {
        "veles"
    } else {
        nickname.trim()
    };
    let mut s = format!(
        "[platforms.irc]\nenabled = true\nserver = {server}\nport = {port}\nuse_tls = {use_tls}\nnickname = {nick}\n",
        server = q(server),
        nick = q(nick),
    );
    let ch = channels.trim();
    if ch.is_empty() {
        // A commented hint, not a key: an empty `channels` would be read
        // as "join nothing", which is a valid but surprising answer.
        s.push_str(
            "# channels = \"#chan\"    # a STRING, space or comma separated; IRC_CHANNELS beats it\n",
        );
    } else {
        s.push_str(&format!("channels = {}\n", q(ch)));
    }
    s
}

/// The first `[acl]` block. Writing ANY `[acl]` key flips the box from
/// dormant into curated mode (`AclConfig::is_configured`), where the
/// default role becomes `guest` and the adapter allowlist stops being the
/// trust boundary. `setup_plan` records that as a note so the wizard can
/// say it out loud before the operator commits.
pub fn acl_setup_block(owner: &str) -> String {
    format!("[acl]\nowner = [{owner}]\n", owner = q(owner.trim()))
}

/// The owner as a `nick!user@host` mask: an `[[acl.entries]]` row with
/// role owner — the shape `owner_via_mask_entry` (acl.rs) resolves. In
/// `[acl] owner`, which compares identities like `irc:<account>`, a mask
/// matches nobody; the wizard used to suggest exactly that, so choosing
/// Curated with its own example locked the operator out on IRC.
pub fn acl_owner_mask_block(mask: &str) -> String {
    format!(
        "[[acl.entries]]\nidentity = {mask}\nrole = \"owner\"\n",
        mask = q(mask.trim())
    )
}

// ── small helpers ────────────────────────────────────────────────────

/// A TOML basic string, quoted and escaped by the `toml` crate's own
/// serializer rather than a hand-rolled escaper — a base URL or nickname
/// with a `"` or `\` in it must not break the document. The parse-back in
/// [`setup_plan`] is the backstop if this ever produces something odd.
fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// `s` trimmed, or `default` when it is empty/whitespace.
fn or_default(s: &str, default: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        default.to_string()
    } else {
        t.to_string()
    }
}

/// An environment variable NAME: what `key_env` must hold.
fn is_env_name(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// The `.env` body: a header and one COMMENTED assignment per variable
/// the answers imply. Commented on purpose: `config::load` reads this
/// file into the process environment, and an uncommented `NAME=` sets
/// NAME to the empty string there — after which a value filled in later
/// is ignored (`resolve_api_key` takes the environment first) until the
/// gateway restarts. A commented line is skipped, so a provider key the
/// operator fills in is read from the file by the next session.
fn env_document(secrets: &[String], allowlists: &[String]) -> String {
    let mut env = String::from(
        "# veles secrets — uncomment a line and put the value after the `=`.\n\
         # Read at gateway start; a provider key is also read by each new session.\n",
    );
    env.push_str(&env_lines(secrets, allowlists));
    env
}

/// One commented block per variable (see [`env_document`] for why they
/// are commented).
fn env_lines(secrets: &[String], allowlists: &[String]) -> String {
    let mut env = String::new();
    for var in secrets {
        env.push_str(&format!(
            "\n# read by the provider/platform enabled in config.toml\n# {var}=\n"
        ));
    }
    for var in allowlists {
        env.push_str(&format!(
            "\n# who may talk to the bot (comma separated). Left unset, ANYONE who\n\
             # reaches it may — and with no [acl] section, as owner.\n# {var}=\n"
        ));
    }
    env
}

/// Does a `.env` text already name `var` — set (`VAR=`, `export VAR=`)
/// or as a commented template line (`# VAR=`)? Either way the operator
/// has been told about it, and adding it again would only be noise.
fn env_mentions(text: &str, var: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim().trim_start_matches('#').trim();
        let t = t.strip_prefix("export ").unwrap_or(t);
        t.split_once('=').is_some_and(|(k, _)| k.trim() == var)
    })
}

// ── tests ────────────────────────────────────────────────────────────
//
// The adapter-facing shape checks (does `platforms.irc.use_tls` match what
// `veles_irc::config_surface()` declares?) live in `veles-cli`'s
// `setup_wizard_tests`, because core cannot depend on the adapter crates.
// These cover what core CAN prove alone.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VelesConfig;

    fn parsed(cfg: &str) -> VelesConfig {
        toml::from_str(cfg).expect("the wizard's config must deserialize")
    }

    fn fresh(a: &SetupAnswers) -> SetupPlan {
        setup_plan(a, None).expect("plan")
    }

    /// The notes and refusals are translated, and the process locale is
    /// shared by every test in the binary; a test that reads their English
    /// pins its own thread to it.
    fn en() -> crate::i18n::ScopeGuard {
        crate::i18n::ScopeGuard::enter(Some("en".into()))
    }

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The wizard's secret fields land in the vault, each under its
    /// kind's policy: the provider key may go to its provider's host,
    /// the bot token is engine-only. The `.env` template no longer names
    /// what is stored; nothing is in config.toml or `.env` in the clear.
    #[test]
    fn secrets_given_to_the_wizard_go_into_the_vault_by_kind() {
        let _en = en();
        let (home, _env) = crate::testenv::scratch_home();
        let dir = home.path();
        let a = SetupAnswers {
            provider: ProviderChoice::OpenAiCompatible,
            base_url: "https://api.example.com/v1".into(),
            key_env: "VELES_T_WIZ_KEY".into(),
            provider_name: "ex".into(),
            model: "m".into(),
            telegram: true,
            ..SetupAnswers::default()
        };
        let plan = setup_plan(&a, None).unwrap();
        let report = apply_with(
            &plan,
            dir,
            &values(&[
                ("VELES_T_WIZ_KEY", "sk-wizard-typed-0000111"),
                ("TELEGRAM_BOT_TOKEN", ""),
            ]),
        )
        .unwrap();
        assert_eq!(report.secrets_stored, vec!["VELES_T_WIZ_KEY".to_string()]);
        assert_eq!(
            report.secrets_needed,
            vec!["TELEGRAM_BOT_TOKEN".to_string()]
        );
        let listed = crate::secrets::listing();
        let key = listed.iter().find(|l| l.name == "VELES_T_WIZ_KEY").unwrap();
        assert_eq!(key.source, "vault");
        assert_eq!(key.policy.kind, crate::vault::Kind::Provider);
        assert_eq!(key.policy.hosts, vec!["api.example.com".to_string()]);
        let env = std::fs::read_to_string(dir.join(".env")).unwrap();
        assert!(
            !env.contains("VELES_T_WIZ_KEY"),
            "stored, so not templated: {env}"
        );
        assert!(env.contains("# TELEGRAM_BOT_TOKEN="), "still unset: {env}");
        for f in ["config.toml", ".env"] {
            let t = std::fs::read_to_string(dir.join(f)).unwrap();
            assert!(!t.contains("sk-wizard-typed"), "{f} holds the value");
        }
        // The bot token, given later, is a platform credential.
        let plan = setup_plan(
            &a,
            std::fs::read_to_string(dir.join("config.toml"))
                .ok()
                .as_deref(),
        )
        .unwrap();
        apply_with(
            &plan,
            dir,
            &values(&[("TELEGRAM_BOT_TOKEN", "123:bot-token-typed")]),
        )
        .unwrap();
        let listed = crate::secrets::listing();
        let bot = listed
            .iter()
            .find(|l| l.name == "TELEGRAM_BOT_TOKEN")
            .unwrap();
        assert_eq!(bot.policy.kind, crate::vault::Kind::Platform);
        assert!(!bot.policy.shell && bot.policy.hosts.is_empty());
    }

    /// The wizard is not a general secret store: a name its answers do
    /// not use is refused before config.toml is touched or anything is
    /// stored.
    #[test]
    fn a_secret_name_the_answers_do_not_use_is_refused_before_any_write() {
        let _en = en();
        let (home, _env) = crate::testenv::scratch_home();
        let dir = home.path();
        let plan = setup_plan(&SetupAnswers::default(), None).unwrap();
        let err = apply_with(&plan, dir, &values(&[("SOMETHING_ELSE", "v-v-v-v-v")]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("SOMETHING_ELSE is not a secret these answers use"),
            "{err}"
        );
        assert!(
            !dir.join("config.toml").exists(),
            "config written despite the refusal"
        );
        assert!(crate::secrets::resolve("SOMETHING_ELSE").is_none());
    }

    /// An IRC SASL account writes the keys the adapter reads, and makes
    /// the password a secret the wizard asks for — never a config key.
    #[test]
    fn a_sasl_account_writes_the_adapters_keys_and_asks_for_the_password() {
        let a = SetupAnswers {
            irc_server: "irc.libera.chat".into(),
            irc_sasl_username: "velesbot".into(),
            ..SetupAnswers::default()
        };
        let plan = setup_plan(&a, None).unwrap();
        assert!(
            plan.config_toml.contains("sasl_username = \"velesbot\""),
            "{}",
            plan.config_toml
        );
        assert!(
            plan.config_toml.contains("sasl_mechanism = \"PLAIN\""),
            "{}",
            plan.config_toml
        );
        assert!(
            !plan.config_toml.contains("sasl_password"),
            "{}",
            plan.config_toml
        );
        assert!(plan.secrets.contains(&IRC_SASL_PASSWORD.to_string()));
        // And a re-run reads the account back.
        let back = current_answers(&plan.config_toml).unwrap();
        assert_eq!(back.irc_sasl_username, "velesbot");
    }

    #[test]
    fn the_default_plan_emits_a_model_table_not_top_level_strings() {
        let plan = fresh(&SetupAnswers::default());
        let c = parsed(&plan.config_toml);
        // The bug this pins: top-level `model = "x"` does NOT deserialize
        // (model is a ModelSection). A table does, and lands in the struct.
        assert_eq!(c.model.provider.as_deref(), Some("openrouter"));
        assert_eq!(c.model.default.as_deref(), Some("openrouter/auto"));
        assert!(
            !plan.config_toml.contains("\nmodel = \""),
            "top-level model string is the unparseable shape:\n{}",
            plan.config_toml
        );
        assert_eq!(plan.mode, SetupMode::Fresh);
    }

    #[test]
    fn the_provider_block_lands_as_a_custom_provider_with_key_env() {
        let plan = fresh(&SetupAnswers::default());
        let c = parsed(&plan.config_toml);
        assert_eq!(c.custom_providers.len(), 1, "{}", plan.config_toml);
        let p = &c.custom_providers[0];
        assert_eq!(p.name, "openrouter");
        assert_eq!(p.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(p.api_mode, "chat_completions");
        assert_eq!(
            p.key_env.as_deref(),
            Some("OPENROUTER_API_KEY"),
            "key_env is the field resolve_api_key reads; api_key_env is not"
        );
        assert!(plan.secrets.contains(&"OPENROUTER_API_KEY".to_string()));
    }

    #[test]
    fn an_openai_compatible_endpoint_uses_its_own_answers() {
        let a = SetupAnswers {
            provider: ProviderChoice::OpenAiCompatible,
            base_url: "http://127.0.0.1:11434/v1".into(),
            key_env: "OLLAMA_KEY".into(),
            api_mode: "chat_completions".into(),
            provider_name: "ollama".into(),
            model: "llama3".into(),
            ..Default::default()
        };
        let plan = fresh(&a);
        let c = parsed(&plan.config_toml);
        assert_eq!(c.model.provider.as_deref(), Some("ollama"));
        assert_eq!(c.model.default.as_deref(), Some("llama3"));
        let p = &c.custom_providers[0];
        assert_eq!(p.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(p.key_env.as_deref(), Some("OLLAMA_KEY"));
        assert!(plan.secrets.contains(&"OLLAMA_KEY".to_string()));
    }

    #[test]
    fn skip_writes_no_provider_and_says_so() {
        let _en = en();
        let a = SetupAnswers {
            provider: ProviderChoice::Skip,
            ..Default::default()
        };
        let plan = fresh(&a);
        let c = parsed(&plan.config_toml);
        assert!(c.custom_providers.is_empty(), "{}", plan.config_toml);
        assert!(c.model.provider.is_none() && c.model.default.is_none());
        assert!(
            plan.notes.iter().any(|n| n.contains("no provider")),
            "a skipped provider must be announced: {:?}",
            plan.notes
        );
    }

    #[test]
    fn telegram_writes_a_switch_and_no_secret_in_config() {
        let plan = fresh(&SetupAnswers {
            telegram: true,
            ..Default::default()
        });
        let c = parsed(&plan.config_toml);
        let tg = c.platforms.get("telegram").expect("telegram table");
        assert_eq!(tg.get("enabled").and_then(|v| v.as_bool()), Some(true));
        // The block is JUST the switch — no token key. Asserted on the
        // block itself (the full config's base_url contains "token-plan",
        // so a whole-document substring check would be a false positive).
        assert!(
            !telegram_setup_block().to_lowercase().contains("token"),
            "the telegram token is env-only; a config key for it is a secret \
             in a file people copy around"
        );
        assert!(
            !plan.config_toml.to_lowercase().contains("token ="),
            "no config key holds a token value:\n{}",
            plan.config_toml
        );
        assert!(plan.secrets.contains(&"TELEGRAM_BOT_TOKEN".to_string()));
    }

    #[test]
    fn irc_uses_the_table_and_keys_the_adapter_reads() {
        let plan = fresh(&SetupAnswers {
            irc_server: "irc.libera.chat".into(),
            irc_port: 6697,
            irc_use_tls: true,
            irc_nickname: "mybot".into(),
            irc_channels: "#veles #dev".into(),
            ..Default::default()
        });
        let c = parsed(&plan.config_toml);
        // `irc`, not `ircx`: `irc_enabled` reads platforms.irc.enabled.
        let irc = c
            .platforms
            .get("irc")
            .expect("the table irc_enabled and apply_mapping look up");
        assert!(
            c.platforms.get("ircx").is_none(),
            "ircx is hermes' name, not read here"
        );
        assert_eq!(irc.get("enabled").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            irc.get("server").and_then(|v| v.as_str()),
            Some("irc.libera.chat")
        );
        assert_eq!(irc.get("port").and_then(|v| v.as_u64()), Some(6697));
        assert_eq!(
            irc.get("use_tls").and_then(|v| v.as_bool()),
            Some(true),
            "`tls` is not the key the adapter maps"
        );
        assert_eq!(
            irc.get("nickname").and_then(|v| v.as_str()),
            Some("mybot"),
            "`nick` is not the key the adapter maps"
        );
        // channels is a STRING; an array is read with as_str and ignored.
        assert_eq!(
            irc.get("channels").and_then(|v| v.as_str()),
            Some("#veles #dev"),
            "channels must be a string, not an array"
        );
    }

    #[test]
    fn irc_with_no_channels_comments_the_hint_not_an_empty_key() {
        let plan = fresh(&SetupAnswers {
            irc_server: "irc.oftc.net".into(),
            ..Default::default()
        });
        assert!(
            plan.config_toml.contains("# channels ="),
            "an empty channels key reads as 'join nothing'; the hint stays a comment:\n{}",
            plan.config_toml
        );
        let c = parsed(&plan.config_toml);
        assert!(c.platforms.get("irc").unwrap().get("channels").is_none());
    }

    #[test]
    fn curated_acl_writes_an_owner_and_warns_about_the_flip() {
        let _en = en();
        let plan = fresh(&SetupAnswers {
            acl: AclChoice::Curated,
            acl_owner: "telegram:123456".into(),
            ..Default::default()
        });
        let c = parsed(&plan.config_toml);
        assert_eq!(c.acl.owner, vec!["telegram:123456".to_string()]);
        assert!(c.acl.is_configured(), "an owner makes the ACL curated");
        assert!(
            plan.notes.iter().any(|n| n.contains("CURATED")),
            "the wizard must say the flip out loud: {:?}",
            plan.notes
        );
    }

    #[test]
    fn curated_acl_with_no_owner_stays_dormant_rather_than_locking_out() {
        let _en = en();
        let plan = fresh(&SetupAnswers {
            acl: AclChoice::Curated,
            acl_owner: "  ".into(),
            ..Default::default()
        });
        let c = parsed(&plan.config_toml);
        assert!(
            !c.acl.is_configured(),
            "an ownerless [acl] would lock the operator out"
        );
        assert!(!plan.config_toml.contains("[acl]"), "{}", plan.config_toml);
        assert!(plan.notes.iter().any(|n| n.contains("dormant")));
    }

    #[test]
    fn legacy_open_writes_no_acl_block() {
        let plan = fresh(&SetupAnswers::default());
        assert!(!plan.config_toml.contains("[acl]"), "{}", plan.config_toml);
    }

    #[test]
    fn a_skipped_section_writes_nothing_for_it() {
        // Default answers: no telegram, no IRC server, legacy ACL — the
        // config must carry no trace of any of them.
        let plan = fresh(&SetupAnswers::default());
        for table in ["[platforms.telegram]", "[platforms.irc]", "[acl]"] {
            assert!(
                !plan.config_toml.contains(table),
                "{table} was written for a section the answers skipped:\n{}",
                plan.config_toml
            );
        }
        // "skip" is the CLI's sentinel for the IRC server.
        let plan = fresh(&SetupAnswers {
            irc_server: "skip".into(),
            ..Default::default()
        });
        assert!(
            !plan.config_toml.contains("[platforms.irc]"),
            "{}",
            plan.config_toml
        );
    }

    #[test]
    fn skipped_means_left_out_whatever_the_fields_say() {
        // The Tk Skip button used to be Next: typed answers on a skipped
        // step were written anyway.
        let plan = fresh(&SetupAnswers {
            telegram: true,
            irc_server: "irc.libera.chat".into(),
            skipped: vec!["telegram".into(), "irc".into(), "provider".into()],
            ..Default::default()
        });
        for table in [
            "[platforms.telegram]",
            "[platforms.irc]",
            "[[custom_providers]]",
        ] {
            assert!(
                !plan.config_toml.contains(table),
                "{table}:\n{}",
                plan.config_toml
            );
        }
        assert!(!plan.secrets.contains(&"TELEGRAM_BOT_TOKEN".to_string()));
    }

    #[test]
    fn an_unknown_skipped_step_is_refused_by_name() {
        let err = setup_plan(
            &SetupAnswers {
                skipped: vec!["telegarm".into()],
                ..Default::default()
            },
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("telegarm"), "{err}");
    }

    #[test]
    fn every_preset_writes_a_routable_config_and_reads_back_as_itself() {
        for p in PROVIDER_PRESETS {
            let a = SetupAnswers {
                provider: p.choice,
                // A preset with no alias to suggest needs the user's pick.
                model: if p.model.is_empty() {
                    "picked-model".into()
                } else {
                    String::new()
                },
                ..Default::default()
            };
            let plan = fresh(&a);
            let c = parsed(&plan.config_toml);
            let entry = &c.custom_providers[0];
            assert_eq!(entry.name, p.entry_name, "{}", p.label);
            assert_eq!(entry.base_url, p.base_url, "{}", p.label);
            assert_eq!(entry.api_mode, p.api_mode, "{}", p.label);
            assert_eq!(
                entry.driver.as_deref().unwrap_or(""),
                p.driver,
                "{}",
                p.label
            );
            // "Routable" means a route builds from it, not only that it
            // parses: a plugin entry with no driver parses fine.
            let ua = crate::providers::UserAgentProfile::veles();
            crate::providers::build_client(entry, "k", &ua)
                .unwrap_or_else(|e| panic!("{}: {e:#}", p.label));
            assert_eq!(
                c.model.provider.as_deref(),
                Some(p.entry_name),
                "{}",
                p.label
            );
            assert!(
                c.model.default.as_deref().is_some_and(|m| !m.is_empty()),
                "{}",
                p.label
            );
            assert_eq!(plan.secrets.is_empty(), p.key_env.is_empty(), "{}", p.label);
            let back = current_answers(&plan.config_toml).unwrap();
            assert_eq!(back.provider, p.choice, "{} prefills as itself", p.label);
        }
    }

    /// The Anthropic preset (0.1.2) writes the NATIVE Messages wire: the
    /// plugin api_mode, the driver the bundled Tcl plugin registers, and
    /// the host-only base URL that driver appends `/v1/messages` to. It
    /// used to write `chat_completions` at `/v1`, Anthropic's OpenAI-
    /// compatible layer, which has no prompt caching. The entry it writes
    /// builds a plugin route — the check config load cannot make, made
    /// here — and the driver it names is the one the bundled plugin
    /// registers, read from the plugin itself.
    #[test]
    fn the_anthropic_preset_writes_the_native_wire_through_the_bundled_driver() {
        let plan = fresh(&SetupAnswers {
            provider: ProviderChoice::Anthropic,
            model: "claude-x".into(),
            ..Default::default()
        });
        let c = parsed(&plan.config_toml);
        let e = &c.custom_providers[0];
        assert_eq!(e.name, "anthropic");
        assert_eq!(
            e.base_url, "https://api.anthropic.com",
            "{}",
            plan.config_toml
        );
        assert_eq!(e.api_mode, "plugin", "{}", plan.config_toml);
        assert_eq!(e.driver.as_deref(), Some("anthropic-messages"));
        assert_eq!(e.key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        let ua = crate::providers::UserAgentProfile::veles();
        match crate::providers::build_client(e, "k", &ua).expect("a routable entry") {
            crate::providers::RouteClient::Plugin(r) => {
                assert_eq!(r.driver, "anthropic-messages")
            }
            _ => panic!("the Anthropic preset must build a plugin route"),
        }
        let shipped = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../plugins/examples/tcl-anthropic-driver/plugin.tcl"),
        )
        .expect("the bundled driver's source");
        assert!(
            shipped.contains(&format!(
                "register_provider {}",
                preset(ProviderChoice::Anthropic).unwrap().driver
            )),
            "the preset names a driver the bundled plugin does not register"
        );
        // No other row grows a driver line.
        let plain = fresh(&SetupAnswers::default()).config_toml;
        assert!(!plain.contains("driver"), "{plain}");
    }

    /// Configs the pre-0.1.2 wizard wrote — Anthropic at its OpenAI-
    /// compatible `/v1` — are still recognised as Anthropic, and a re-run
    /// keeps their URL and their wire: the plugin wire at `/v1` would
    /// post to `/v1/v1/messages`. Pinned both ways an empty api_mode can
    /// arrive: prefilled from a config that has none, and in answers
    /// that name the old URL with no wire.
    #[test]
    fn an_anthropic_entry_at_the_old_compat_url_stays_anthropic_on_its_own_wire() {
        assert_eq!(
            choice_for_base_url(ANTHROPIC_OPENAI_COMPAT_BASE_URL),
            ProviderChoice::Anthropic
        );
        assert_eq!(
            choice_for_base_url("https://api.anthropic.com/v1/"),
            ProviderChoice::Anthropic
        );
        assert_eq!(
            choice_for_base_url(ANTHROPIC_BASE_URL),
            ProviderChoice::Anthropic
        );

        let old = "[model]\nprovider = \"anthropic\"\ndefault = \"claude-x\"\n\n\
                   [[custom_providers]]\nname = \"anthropic\"\n\
                   base_url = \"https://api.anthropic.com/v1\"\nkey_env = \"ANTHROPIC_API_KEY\"\n";
        let back = current_answers(old).expect("prefill");
        assert_eq!(back.provider, ProviderChoice::Anthropic);
        assert_eq!(
            back.api_mode, "chat_completions",
            "the entry's effective wire"
        );
        let t = setup_plan(&back, Some(old)).expect("plan").config_toml;
        let e = parsed(&t).custom_providers[0].clone();
        assert_eq!(e.base_url, ANTHROPIC_OPENAI_COMPAT_BASE_URL, "{t}");
        assert_eq!(e.api_mode, "chat_completions", "{t}");
        assert!(e.driver.is_none(), "{t}");

        let t = fresh(&SetupAnswers {
            provider: ProviderChoice::Anthropic,
            base_url: ANTHROPIC_OPENAI_COMPAT_BASE_URL.into(),
            model: "claude-x".into(),
            ..Default::default()
        })
        .config_toml;
        let e = parsed(&t).custom_providers[0].clone();
        assert_eq!(e.api_mode, "chat_completions", "{t}");
        assert!(e.driver.is_none(), "{t}");
    }

    /// A plugin wire with no driver anywhere is refused before anything
    /// is written, naming where to look — the same words build_client
    /// would use at boot.
    #[test]
    fn a_plugin_wire_without_a_driver_is_refused() {
        let err = setup_plan(
            &SetupAnswers {
                provider: ProviderChoice::OpenAiCompatible,
                api_mode: "plugin".into(),
                model: "m".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("driver"), "{err}");
        assert!(err.contains("doctor --providers"), "{err}");
    }

    /// Where the model step looks. The OpenAI wire's base URLs carry
    /// their version and list at `{base}/models`; a host-only base URL
    /// (the Anthropic native preset) lists at `/v1/models`, unpaged.
    #[test]
    fn the_model_list_url_follows_the_base_url_shape() {
        assert_eq!(
            models_url("https://openrouter.ai/api/v1"),
            "https://openrouter.ai/api/v1/models"
        );
        assert_eq!(
            models_url(ANTHROPIC_OPENAI_COMPAT_BASE_URL),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            models_url(ANTHROPIC_BASE_URL),
            "https://api.anthropic.com/v1/models?limit=1000"
        );
        assert_eq!(
            models_url("https://api.anthropic.com/"),
            "https://api.anthropic.com/v1/models?limit=1000"
        );
        assert_eq!(
            models_url("http://localhost:11434/v1"),
            "http://localhost:11434/v1/models"
        );
    }

    /// `list_models` asks the URL `models_url` names, with the headers
    /// Anthropic's catalog reads: a server that answers ONLY
    /// `/v1/models?limit=1000` with an `x-api-key` lists for the Anthropic
    /// preset's host-only base URL.
    #[tokio::test]
    async fn the_model_step_lists_a_host_only_base_url_at_v1_models() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).expect("read");
            let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
            let ok = req.starts_with("get /v1/models?limit=1000 ")
                && req.contains("x-api-key: k")
                && req.contains("anthropic-version: ");
            let (status, body) = if ok {
                (
                    "200 OK",
                    r#"{"data":[{"id":"claude-b","type":"model"},{"id":"claude-a","type":"model"}],"has_more":false}"#,
                )
            } else {
                ("404 Not Found", "{}")
            };
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        let got = list_models(&format!("http://127.0.0.1:{port}"), "k").await;
        server.join().expect("server");
        assert_eq!(got.expect("listed"), ["claude-a", "claude-b"]);
    }

    #[test]
    fn a_preset_without_a_suggested_model_refuses_an_empty_pick() {
        let _en = en();
        let err = setup_plan(
            &SetupAnswers {
                provider: ProviderChoice::OpenAi,
                ..Default::default()
            },
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("choose a default model for openai"), "{err}");
    }

    #[test]
    fn no_preset_is_named_for_one_person() {
        // The first-run wizard once defaulted to the owner's own
        // "ali-token-plan-personal" route and key name.
        for p in PROVIDER_PRESETS {
            assert!(
                !p.entry_name.contains("personal") && !p.key_env.contains("PERSONAL"),
                "{p:?}"
            );
        }
        assert_eq!(SetupAnswers::default().provider, ProviderChoice::OpenRouter);
    }

    #[test]
    fn the_default_provider_defaults_to_the_entry_written() {
        let c = parsed(&fresh(&SetupAnswers::default()).config_toml);
        assert_eq!(c.model.provider.as_deref(), Some("openrouter"));
    }

    #[test]
    fn an_unknown_api_mode_is_refused_by_name() {
        let _en = en();
        let err = setup_plan(
            &SetupAnswers {
                provider: ProviderChoice::OpenAiCompatible,
                api_mode: "carrier_pigeon".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("carrier_pigeon"), "{err}");
        assert!(
            err.contains("chat_completions"),
            "names the valid set: {err}"
        );
    }

    #[test]
    fn a_value_with_a_quote_does_not_break_the_document() {
        let plan = fresh(&SetupAnswers {
            provider: ProviderChoice::OpenAiCompatible,
            provider_name: r#"we"ird"#.into(),
            base_url: "http://x/v1".into(),
            key_env: "K".into(),
            model: "m".into(),
            ..Default::default()
        });
        assert_eq!(
            parsed(&plan.config_toml).custom_providers[0].name,
            r#"we"ird"#
        );
    }

    #[test]
    fn a_locale_answer_lands_in_tui() {
        let plan = fresh(&SetupAnswers {
            locale: "de".into(),
            ..Default::default()
        });
        assert_eq!(parsed(&plan.config_toml).tui.locale.as_deref(), Some("de"));
    }

    // ── REVIEW 2026-09-27: each of these was red on the merged branch ─

    #[test]
    fn a_key_value_in_key_env_is_refused_and_not_echoed() {
        let _en = en();
        let err = setup_plan(
            &SetupAnswers {
                provider: ProviderChoice::OpenAiCompatible,
                key_env: "sk-live-0123456789abcdef".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(!err.contains("sk-live-0123456789abcdef"), "echoed: {err}");
        assert!(err.contains("NAME"), "says what the field wants: {err}");
    }

    #[test]
    fn an_irc_mask_owner_is_an_owner_entry_not_an_owner_id() {
        let plan = fresh(&SetupAnswers {
            acl: AclChoice::Curated,
            acl_owner: "irc:wrath!~w@home.example".into(),
            ..Default::default()
        });
        let c = parsed(&plan.config_toml);
        assert!(
            c.acl.owner.is_empty(),
            "a mask never matches in owner: {:?}",
            c.acl.owner
        );
        assert_eq!(c.acl.entries.len(), 1, "{}", plan.config_toml);
        assert_eq!(c.acl.entries[0].identity, "wrath!~w@home.example");
        assert_eq!(c.acl.entries[0].role, "owner");
        // And the ACL resolves it: the mask IS the owner on that host.
        let ctx = crate::acl::SenderContext {
            mask: Some("wrath!~w@home.example".into()),
            ..Default::default()
        };
        assert_eq!(
            crate::acl::Principal::resolve_ctx(None, &ctx, &c.acl).role,
            crate::acl::Role::Owner
        );
    }

    #[test]
    fn an_owner_the_acl_cannot_match_is_refused_by_name() {
        for bad in ["telegram:@wrath", "wrath", "irc:", "telegram:12 34"] {
            let r = setup_plan(
                &SetupAnswers {
                    acl: AclChoice::Curated,
                    acl_owner: bad.into(),
                    ..Default::default()
                },
                None,
            );
            assert!(r.is_err(), "{bad} was accepted");
        }
        for good in ["telegram:123456", "irc:wrath", "matrix:@me:example.org"] {
            setup_plan(
                &SetupAnswers {
                    acl: AclChoice::Curated,
                    acl_owner: good.into(),
                    ..Default::default()
                },
                None,
            )
            .unwrap_or_else(|e| panic!("{good}: {e}"));
        }
    }

    #[test]
    fn a_model_provider_naming_no_entry_is_refused() {
        let _en = en();
        let err = setup_plan(
            &SetupAnswers {
                provider_name: "mine".into(),
                default_provider: "openrouter".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("openrouter"), "by name: {err}");
    }

    #[test]
    fn a2a_is_not_switched_on_unasked() {
        let plan = fresh(&SetupAnswers::default());
        assert!(!plan.config_toml.contains("a2a"), "{}", plan.config_toml);
    }

    #[test]
    fn the_env_template_does_not_pin_empty_values() {
        let plan = fresh(&SetupAnswers {
            telegram: true,
            ..Default::default()
        });
        for l in plan.env_text.lines() {
            assert!(
                l.trim().is_empty() || l.starts_with('#'),
                "an uncommented `{l}` is SET to empty on load"
            );
        }
        assert!(plan.env_text.contains("# TELEGRAM_BOT_TOKEN="));
    }

    #[test]
    fn an_open_door_is_said_out_loud() {
        let _en = en();
        let plan = fresh(&SetupAnswers {
            telegram: true,
            ..Default::default()
        });
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("TELEGRAM_ALLOWED_USERS")),
            "{:?}",
            plan.notes
        );
        assert!(
            plan.env_text.contains("# TELEGRAM_ALLOWED_USERS="),
            "{}",
            plan.env_text
        );
        // Curated closes the door, so the warning does not fire.
        let plan = fresh(&SetupAnswers {
            telegram: true,
            acl: AclChoice::Curated,
            acl_owner: "telegram:1".into(),
            ..Default::default()
        });
        assert!(
            !plan
                .notes
                .iter()
                .any(|n| n.contains("TELEGRAM_ALLOWED_USERS")),
            "{:?}",
            plan.notes
        );
    }

    // ── edit in place ───────────────────────────────────────────────

    /// A config an operator wrote by hand: comments, sections the wizard
    /// does not own, a provider with keys it does not ask about.
    const HAND_ROLLED: &str = r##"# my box — do not lose this comment
[model]
provider = "ali-token-plan-team"   # the team key routes by default
default = 'qwen3-coder-plus'   # a literal string: a rewrite would re-quote it

[agent.guards]
max_iterations = 42

[[custom_providers]]
name = "ali-token-plan-team"
base_url = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"
key_env = "ALI_TOKEN_PLAN_TEAM_API_KEY"
api_mode = "chat_completions"
context_length = 131072

[[custom_providers]]
name = "local"
base_url = "http://127.0.0.1:8000/v1"
key_env = ""
api_mode = "chat_completions"

[platforms.irc]
enabled = true
server = "irc.libera.chat"
port = 6_697
use_tls = true
nickname = "veles"
channels = "#veles"
sasl_mechanism = "PLAIN"

[acl]
owner = ["irc:wrath"]
"##;

    #[test]
    fn a_walk_that_changes_nothing_writes_nothing() {
        let a = current_answers(HAND_ROLLED).expect("prefill");
        let plan = setup_plan(&a, Some(HAND_ROLLED)).expect("plan");
        assert_eq!(plan.mode, SetupMode::Edit);
        assert_eq!(
            plan.config_toml, HAND_ROLLED,
            "prefilled answers, unchanged, must reproduce the file byte for byte"
        );
    }

    #[test]
    fn an_edit_changes_only_what_was_answered() {
        let mut a = current_answers(HAND_ROLLED).expect("prefill");
        a.model = "qwen3-max".into();
        a.telegram = true;
        let plan = setup_plan(&a, Some(HAND_ROLLED)).expect("plan");
        let t = &plan.config_toml;
        for kept in [
            "# my box — do not lose this comment",
            "# the team key routes by default",
            "max_iterations = 42",
            "context_length = 131072",
            "sasl_mechanism = \"PLAIN\"",
            "owner = [\"irc:wrath\"]",
            "name = \"local\"",
        ] {
            assert!(t.contains(kept), "lost `{kept}`:\n{t}");
        }
        let c = parsed(t);
        assert_eq!(c.model.default.as_deref(), Some("qwen3-max"));
        assert_eq!(c.model.provider.as_deref(), Some("ali-token-plan-team"));
        assert_eq!(c.custom_providers.len(), 2, "upserted, not appended:\n{t}");
        assert_eq!(
            c.platforms
                .get("telegram")
                .and_then(|p| p.get("enabled"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        // Not a fresh file: no generated header, no desktop block added.
        assert!(!t.contains("generated by the setup wizard"), "{t}");
    }

    #[test]
    fn skipped_steps_leave_an_existing_section_exactly_as_it_is() {
        let a = SetupAnswers {
            // Answers that WOULD change things, all on skipped steps.
            irc_server: String::new(),
            acl: AclChoice::LegacyOpen,
            provider: ProviderChoice::OpenAiCompatible,
            model: "other".into(),
            skipped: vec![
                "provider".into(),
                "model".into(),
                "irc".into(),
                "acl".into(),
                "telegram".into(),
                "locale".into(),
            ],
            ..Default::default()
        };
        let plan = setup_plan(&a, Some(HAND_ROLLED)).expect("plan");
        assert_eq!(plan.config_toml, HAND_ROLLED);
    }

    #[test]
    fn an_unticked_adapter_is_switched_off_not_deleted() {
        let mut a = current_answers(HAND_ROLLED).expect("prefill");
        a.irc_server = String::new();
        let t = setup_plan(&a, Some(HAND_ROLLED)).expect("plan").config_toml;
        let c = parsed(&t);
        let irc = c.platforms.get("irc").expect("the table stays");
        assert_eq!(irc.get("enabled").and_then(|v| v.as_bool()), Some(false));
        assert!(t.contains("sasl_mechanism"), "{t}");
    }

    #[test]
    fn legacy_open_over_a_curated_config_removes_nothing_and_says_so() {
        let _en = en();
        let mut a = current_answers(HAND_ROLLED).expect("prefill");
        a.acl = AclChoice::LegacyOpen;
        let plan = setup_plan(&a, Some(HAND_ROLLED)).expect("plan");
        assert!(plan.config_toml.contains("owner = [\"irc:wrath\"]"));
        assert!(
            plan.notes.iter().any(|n| n.contains("never removes")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn the_model_provider_may_name_another_existing_entry() {
        let mut a = current_answers(HAND_ROLLED).expect("prefill");
        a.default_provider = "local".into();
        let c = parsed(&setup_plan(&a, Some(HAND_ROLLED)).expect("plan").config_toml);
        assert_eq!(c.model.provider.as_deref(), Some("local"));
    }

    #[test]
    fn prefill_reads_the_file() {
        let a = current_answers(HAND_ROLLED).expect("prefill");
        assert_eq!(a.provider, ProviderChoice::AliTokenPlan);
        assert_eq!(a.provider_name, "ali-token-plan-team");
        assert_eq!(a.key_env, "ALI_TOKEN_PLAN_TEAM_API_KEY");
        assert_eq!(a.model, "qwen3-coder-plus");
        assert_eq!(a.irc_server, "irc.libera.chat");
        assert_eq!(a.irc_channels, "#veles");
        assert_eq!(a.acl, AclChoice::Curated);
        assert_eq!(a.acl_owner, "irc:wrath");
        assert!(!a.telegram);
    }

    // ── apply: the disk half ────────────────────────────────────────

    #[test]
    fn apply_writes_config_and_a_private_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = fresh(&SetupAnswers {
            telegram: true,
            ..Default::default()
        });
        let report = apply(&plan, dir.path()).expect("apply");

        assert!(report.config_path.exists());
        assert_eq!(
            report.config_backup, None,
            "a fresh dir has nothing to back up"
        );
        assert!(report.env_written, "no .env existed, so one is written");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&report.env_path), 0o600, "the .env holds secrets");
            // configedit's rule: a config.toml written from nothing is
            // owner-only (the wizard's own copy of the write skipped it).
            assert_eq!(
                mode(&report.config_path),
                0o600,
                "config.toml is born private"
            );
        }
        let env = std::fs::read_to_string(&report.env_path).unwrap();
        assert!(env.contains("# OPENROUTER_API_KEY="));
        assert!(env.contains("# TELEGRAM_BOT_TOKEN="));
    }

    #[test]
    fn apply_edits_in_place_keeps_a_backup_and_leaves_the_env_alone() {
        // `resolve_in` consults the process env first, and a developer
        // shell on this box EXPORTS the real ALI_TOKEN_PLAN_TEAM_API_KEY
        // (the secrets the gateway runs with). Unsandboxed, the test
        // read it as "already set" and asserted a different
        // secrets_needed than the fixture means — green under `env -i`,
        // red from the operator's own terminal. Scrub it for this test.
        let _lock = crate::testenv::lock();
        let prev = std::env::var_os("ALI_TOKEN_PLAN_TEAM_API_KEY");
        std::env::remove_var("ALI_TOKEN_PLAN_TEAM_API_KEY");
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("config.toml");
        let env = dir.path().join(".env");
        std::fs::write(&cfg, HAND_ROLLED).unwrap();
        std::fs::write(&env, "TELEGRAM_BOT_TOKEN=a-live-token\n").unwrap();

        let mut a = current_answers(HAND_ROLLED).expect("prefill");
        a.telegram = true;
        let plan = setup_plan(&a, Some(HAND_ROLLED)).expect("plan");
        let report = apply(&plan, dir.path()).expect("apply");
        // clippy::single_match (surfaced by the -D warnings flake gate):
        // one meaningful arm is an if let.
        if let Some(v) = prev {
            std::env::set_var("ALI_TOKEN_PLAN_TEAM_API_KEY", v);
        }

        let bak = report.config_backup.expect("the previous text is kept");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), HAND_ROLLED);
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(after.contains("max_iterations = 42"), "{after}");
        assert!(after.contains("[platforms.telegram]"), "{after}");
        // The live token line is untouched; only the key the edited
        // config names and the file did not mention is added, commented.
        let env_text = std::fs::read_to_string(&env).unwrap();
        assert!(
            env_text.starts_with("TELEGRAM_BOT_TOKEN=a-live-token\n"),
            "{env_text}"
        );
        assert_eq!(
            report.env_added,
            vec!["ALI_TOKEN_PLAN_TEAM_API_KEY".to_string()]
        );
        assert!(
            env_text.contains("# ALI_TOKEN_PLAN_TEAM_API_KEY="),
            "{env_text}"
        );
        // "Needed" is what is still unset anywhere — the finish screen's
        // to-do list. The live token counts as set; the commented team
        // key does not.
        assert_eq!(
            report.secrets_needed,
            vec!["ALI_TOKEN_PLAN_TEAM_API_KEY".to_string()]
        );
    }

    /// The REAL first run, seen through the gateway binary: the gateway
    /// mints DESKTOP_BEARER_TOKEN into `.env` at boot, before the wizard
    /// has run — so "write the template only when there is no .env"
    /// meant the template never landed at all. The names the config now
    /// needs are appended, commented, to the file that is there; every
    /// line already in it is left exactly as it was.
    #[test]
    fn apply_adds_the_missing_names_to_an_env_the_gateway_already_made() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = dir.path().join(".env");
        std::fs::write(
            &env,
            "DESKTOP_BEARER_TOKEN=minted\nTELEGRAM_BOT_TOKEN=live\n",
        )
        .unwrap();
        let plan = fresh(&SetupAnswers {
            telegram: true,
            ..Default::default()
        });
        let r = apply(&plan, dir.path()).expect("apply");
        let text = std::fs::read_to_string(&env).unwrap();
        assert!(
            text.starts_with("DESKTOP_BEARER_TOKEN=minted\nTELEGRAM_BOT_TOKEN=live\n"),
            "existing lines untouched:\n{text}"
        );
        assert!(text.contains("# OPENROUTER_API_KEY="), "{text}");
        assert!(text.contains("# TELEGRAM_ALLOWED_USERS="), "{text}");
        assert_eq!(
            text.matches("TELEGRAM_BOT_TOKEN").count(),
            1,
            "a name the file already has is not added again:\n{text}"
        );
        assert!(r.env_written, "the file was written to");
        assert_eq!(
            r.env_added,
            vec![
                "OPENROUTER_API_KEY".to_string(),
                "TELEGRAM_ALLOWED_USERS".to_string()
            ]
        );
        // Idempotent: a second apply of the same plan adds nothing.
        let plan = setup_plan(
            &SetupAnswers {
                telegram: true,
                ..Default::default()
            },
            std::fs::read_to_string(dir.path().join("config.toml"))
                .ok()
                .as_deref(),
        )
        .expect("plan");
        let r = apply(&plan, dir.path()).expect("apply again");
        assert_eq!(std::fs::read_to_string(&env).unwrap(), text);
        assert!(!r.env_written && r.env_added.is_empty());
    }

    #[test]
    fn apply_refuses_a_plan_whose_config_changed_underneath_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("config.toml");
        std::fs::write(&cfg, HAND_ROLLED).unwrap();
        let plan = setup_plan(&current_answers(HAND_ROLLED).unwrap(), Some(HAND_ROLLED)).unwrap();
        // A hand edit (or another writer) lands between preview and write.
        std::fs::write(&cfg, format!("{HAND_ROLLED}\n[memory]\nx = 1\n")).unwrap();
        let err = apply(&plan, dir.path()).unwrap_err();
        assert!(err.downcast_ref::<StaleBase>().is_some(), "{err}");
        assert!(
            std::fs::read_to_string(&cfg).unwrap().contains("[memory]"),
            "nothing was written"
        );
        // And a plan made for a fresh dir is stale once a file appears.
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = fresh(&SetupAnswers::default());
        std::fs::write(dir.path().join("config.toml"), "# appeared\n").unwrap();
        assert!(apply(&plan, dir.path())
            .unwrap_err()
            .downcast_ref::<StaleBase>()
            .is_some());
    }

    #[test]
    fn write_private_tightens_loose_bits_on_an_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join(".env");
        std::fs::write(&p, "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_private(&p, "secret").expect("write_private");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "loose bits are tightened: {mode:o}");
        }
    }

    #[test]
    fn the_preview_carries_no_secret_values_only_names() {
        let plan = fresh(&SetupAnswers {
            provider: ProviderChoice::OpenAiCompatible,
            key_env: "MY_KEY".into(),
            model: "m".into(),
            telegram: true,
            ..Default::default()
        });
        assert!(plan.config_toml.contains("MY_KEY"), "key_env names the var");
        assert!(!plan.config_toml.to_lowercase().contains("api_key ="));
        for s in &plan.secrets {
            assert!(is_env_name(s), "a secret entry is a VAR NAME: {s}");
        }
    }
}

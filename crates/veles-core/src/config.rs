//! Configuration layer.
//!
//! Two layers:
//! 1. Native `veles.toml` — the going-forward format.
//! 2. Compat reader for hermes `config.yaml` (subset) so migration is
//!    zero-friction: same ~/.hermes layout, same provider entries,
//!    same custom_providers blocks.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// hermes `models` field is polymorphic:
///   - map: { "model-name": { context_length: N } }  (ali token plans)
///   - list: [ "glm-4.5", ... ]                      (zai coding plan)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum ModelsSpec {
    #[default]
    Empty,
    List(Vec<String>),
    Map(std::collections::HashMap<String, serde_yaml::Value>),
}

/// A YAML null, which has no TOML spelling.
fn is_yaml_null(v: &serde_yaml::Value) -> bool {
    v.is_null()
}

impl ModelsSpec {
    /// context_length override for a model, if one is declared.
    pub fn context_length(&self, model: &str) -> Option<u64> {
        match self {
            ModelsSpec::Map(m) => m
                .get(model)
                .and_then(|v| v.get("context_length"))
                .and_then(|v| v.as_u64()),
            _ => None,
        }
    }

    /// All model names in this spec (keys of map or items of list).
    pub fn model_names(&self) -> Vec<String> {
        match self {
            ModelsSpec::Empty => Vec::new(),
            ModelsSpec::List(l) => l.clone(),
            ModelsSpec::Map(m) => m.keys().cloned().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            ModelsSpec::Empty => true,
            ModelsSpec::List(l) => l.is_empty(),
            ModelsSpec::Map(m) => m.is_empty(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub base_url: String,
    pub key_env: Option<String>,
    /// Wire protocol. "chat_completions", "responses" and "plugin" (or
    /// empty = unset → chat_completions) are accepted; `config::load`
    /// rejects anything else by name. "responses" speaks the OpenAI
    /// Responses API at `{base_url}/responses` (see responses.rs) — the
    /// wire the Model Studio Token Plan's server-side tools live on.
    /// "plugin" hands the route to a driver a plugin registered, named
    /// by `driver` below.
    #[serde(default)]
    pub api_mode: String,
    /// Which registered provider driver speaks this route's wire.
    /// Required when `api_mode = "plugin"` and meaningless otherwise.
    ///
    /// Existence cannot be checked at config load: drivers register
    /// when PLUGINS load, which happens after config is read. So load
    /// validates that the field is present and `build_client` names
    /// every registered driver when the lookup misses. `veles doctor
    /// --providers` is where you go to see what is actually there.
    #[serde(default)]
    pub driver: Option<String>,
    /// Where the driver posts, relative to `base_url`. Empty lets the
    /// driver use its own default (`/v1/messages` for Anthropic).
    ///
    /// This exists because "anthropic-compatible" is not one shape. A
    /// proxy that terminates Anthropic's wire but mounts it at
    /// `/api/v1/messages`, or that serves several providers under one
    /// host with the model in the path, cannot be reached by a driver
    /// that hardcodes the suffix — and the alternative is a driver per
    /// gateway, which is the thing a driver registry exists to avoid.
    ///
    /// Driver-owned like `base_url` itself: only meaningful with
    /// `api_mode = "plugin"`, ignored by the native wires (they compose
    /// their own paths).
    #[serde(default)]
    pub endpoint_path: Option<String>,
    /// How the credential is carried: `"x-api-key"` (default) or
    /// `"bearer"`.
    ///
    /// Anthropic's own API takes `x-api-key`; the gateways that resell it
    /// very often take `Authorization: Bearer` instead, because that is
    /// what their own auth stack speaks. Validated at config load
    /// (`validate_auth_styles`), because an unknown value silently
    /// falling back to the default would authenticate with the wrong
    /// header and look like a bad key.
    #[serde(default)]
    pub auth_style: Option<String>,
    /// The wire-version header's VALUE. Empty lets the driver use its
    /// pinned default (`2023-06-01` for Anthropic).
    ///
    /// Which HEADER carries it is the driver's decision — Anthropic names
    /// it `anthropic-version`, a gateway may name it something else — so
    /// this is the value only. Set it when a gateway requires a version
    /// the driver's pin does not match.
    #[serde(default)]
    pub api_version: Option<String>,
    /// The output-token cap the driver must send.
    ///
    /// Anthropic REQUIRES `max_tokens` where OpenAI treats it as optional,
    /// and the engine never sets one — so a driver has to invent a number,
    /// and one hardcoded number is wrong for every model it did not know
    /// about. This is where the config says it, per provider, with the
    /// driver's own default as the fallback. It is a typed field rather
    /// than an `extra_body` key because a driver has to be able to trust
    /// it exists and is a number.
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// How long the provider keeps a cached prompt prefix, on a wire that
    /// lets the client choose: `"5m"` or `"1h"` (`validate_cache_ttls`). A
    /// plugin driver receives it on the `veles` key; absent means the
    /// driver's own default — the Anthropic driver's is `"1h"`, because
    /// the 5-minute entry expired inside long tool loops and every expiry
    /// rewrote the whole history into the cache.
    #[serde(default)]
    pub cache_ttl: Option<String>,
    /// Server-side ("built-in") tools to declare on every request this
    /// provider's routes send, e.g. `["web_search", "web_extractor",
    /// "code_interpreter"]`. The endpoint runs them itself and bills per
    /// successful invocation. They cannot ride `extra_body` — `tools` is
    /// on that merge's denylist because the client owns the tool array,
    /// and that is correct; this field is the sanctioned door.
    ///
    /// Two wires honor it, and how each does differs:
    ///   * `api_mode = "responses"` — mapped to `{"type": name}` server
    ///     tools directly (see `responses::tools_for_responses`).
    ///   * `api_mode = "plugin"` — carried verbatim to the driver under
    ///     the reserved `veles` key, and it is the DRIVER's job to
    ///     translate the names into whatever its wire wants (Anthropic
    ///     spells web search `{"type":"web_search_20250305","name":...}`
    ///     and code execution differently again). A driver that does not
    ///     recognize a name drops it, since sending an unknown server-tool
    ///     type is a hard 400 on a strict endpoint.
    ///
    /// chat_completions has no such concept, so there `build_routes`
    /// warns and drops them.
    #[serde(default)]
    pub built_in_tools: Vec<String>,
    pub model: Option<String>,
    /// Skipped when empty for the same reason as `platforms`:
    /// `ModelsSpec::Empty` is a unit variant.
    #[serde(default, skip_serializing_if = "ModelsSpec::is_empty")]
    pub models: ModelsSpec,
    pub context_length: Option<u64>,
    /// Per-provider reasoning-effort override. When set, this provider's
    /// routes use it instead of the global agent.reasoning_effort —
    /// gateways that reject `reasoning_effort` (mistral, codestral)
    /// set "none" here so their routes still work.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Fetch `{base_url}/models` and MERGE the answer into `models`.
    ///
    /// A PAYG key against a hyperscaler or router serves hundreds of
    /// models on a catalog that changes weekly; hand-enumerating is both
    /// hopeless and stale. See `discovery::wanted` for the three-way
    /// rule this participates in.
    #[serde(default)]
    pub discover: Option<bool>,
    /// An arbitrary JSON blob merged into the request body for this
    /// provider's routes (PENDING 2026-08-29, extra_body passthrough).
    /// The endpoint's own tools/options land here verbatim —
    /// enable_search, code_interpreter, qwen server-side tools. The
    /// endpoint is the validator (same verdict reasoning_effort gets);
    /// only transport-contract keys are refused (see
    /// providers::merge_extra_body).
    #[serde(default)]
    pub extra_body: Option<serde_json::Value>,
    /// Per-provider prompt-cache mode, overriding the global
    /// `[cache].mode`: "auto" (what the wire needs: cache_control
    /// breakpoints on the Anthropic Messages driver and on OpenRouter's
    /// Anthropic models, which cache nothing without them; nothing on
    /// every other wire, which caches on its own), "explicit" (always
    /// emit breakpoints), "off" (never emit). Sending unknown fields to a
    /// strict endpoint is how you get a 400, so `auto` stays inert
    /// wherever the provider does not need the markers.
    #[serde(default)]
    pub cache_mode: Option<String>,
    /// Whether assistant messages carry their reasoning back to this
    /// provider on replay (`reasoning_content`). DeepSeek's reasoner
    /// rejects it on input; most others simply bill for it. Default:
    /// false — the reasoning stays in the store either way, this is
    /// only what goes on the wire.
    #[serde(default)]
    pub replay_reasoning: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelAlias {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub base_url: String,
    pub reasoning: Option<String>,
}

/// `[[command_aliases]]` — one config word that becomes a slash command.
///
/// `[model_aliases]` already names a (provider, model) pair, but it is an
/// argument TO `/model`: two steps and a session you did not want. An
/// alias here IS the command — `/qwen-flash <prompt>` starts a fresh
/// session on that route and runs the prompt in it, and
/// `/qwen-image-3-pro <prompt>` makes the image on that route.
///
/// Every `[model_aliases]` key is promoted to a `chat` alias
/// automatically (see `crate::aliases::table`), so this block is only for
/// the media kinds and for chat aliases that want a pinned reasoning
/// effort or a description.
///
/// There is deliberately NO `api` field. The wire shape is already
/// decided by `[imagegen]`/`[videogen]`'s own `api`, or inferred from the
/// provider's `base_url` (`EndpointApi::resolve`) — and a second place to
/// say it that only sometimes wins is worse than one place that always
/// does.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommandAlias {
    /// The command word, without the leading `/`.
    pub name: String,
    /// `chat` (default), `image`, `video`, or `audio`.
    #[serde(default)]
    pub kind: Option<String>,
    pub provider: String,
    pub model: String,
    /// Chat only: pin a reasoning effort, the way `[model_aliases]` can.
    pub reasoning: Option<String>,
    /// One line for `/commands`, so an alias is not a word with no
    /// discoverable meaning.
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VelesConfig {
    #[serde(default)]
    pub model: ModelSection,
    /// Vision-capable model (for the image tools). See `VisionSection`.
    #[serde(default)]
    pub vision: VisionSection,
    /// Default image-generation route (for `generate_image` and the
    /// `/gen image` command). See `GenSection`.
    #[serde(default)]
    pub imagegen: GenSection,
    /// Default video-generation route (for `generate_video` and the
    /// `/gen video` command). See `GenSection`. Video is a DashScope
    /// async task there (submit, poll, expiring result URL).
    #[serde(default)]
    pub videogen: GenSection,
    /// Channel-event triggers (plan item 1, 2026-09-14): reactions
    /// that run without a turn — link titles and timtom through the
    /// slopdrop pass-through, image descriptions through `[vision]`.
    #[serde(default)]
    pub triggers: TriggersSection,
    /// `[tokenplan]` — upstream Alibaba plan quota, read by `/quota`.
    #[serde(default)]
    pub tokenplan: TokenPlanSection,
    /// `[notify]` — where a question goes when nobody answers it on the
    /// surface it was born on. See `NotifySection`.
    #[serde(default)]
    pub notify: NotifySection,
    /// `[web]` — where `web_search` sends its queries. See `WebSection`.
    #[serde(default)]
    pub web: WebSection,
    /// Default speech-recognition route (for the `transcribe_audio`
    /// tool). See `GenSection`; there is no local fallback, so an unset
    /// provider is an error that names the config.
    #[serde(default)]
    pub asr: GenSection,
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub fallback_providers: Vec<FallbackEntry>,
    /// Failover policy rules (S16). First match wins; absent = implicit
    /// `stop` (failover disabled — USER 2026-09-05). Explicit rules or
    /// a `/failover` session override opt back in per scope.
    #[serde(default, rename = "failover")]
    pub failover_rules: Vec<FailoverRule>,
    /// Train-shaped tool names → registry targets (`apply_patch =
    /// "patch"`). Merged OVER the code-default table, which means both
    /// can win or lose individually.
    #[serde(default)]
    pub tool_aliases: std::collections::HashMap<String, String>,
    /// hermes shape: a LIST of provider entries with `name` keys.
    #[serde(default)]
    pub custom_providers: Vec<ProviderEntry>,
    #[serde(default)]
    pub model_aliases: std::collections::HashMap<String, ModelAlias>,
    /// `[[command_aliases]]` — config words that become slash commands.
    /// See `CommandAlias` and `crate::aliases`.
    #[serde(default)]
    pub command_aliases: Vec<CommandAlias>,
    /// Skipped when null: `serde_yaml::Value::Null` serializes as a
    /// UNIT, and TOML has no unit type — so a config without this
    /// section could not be written at all. See the round-trip test
    /// at the bottom of this file.
    #[serde(default, skip_serializing_if = "is_yaml_null")]
    pub platforms: serde_yaml::Value,
    /// hermes `display` block (display.platforms.<name>: streaming,
    /// show_reasoning, tool_progress, cleanup_progress, reasoning_style,
    /// show_route, ...). Kept as a
    /// raw value — adapters pick out their own platform's keys.
    #[serde(default, skip_serializing_if = "is_yaml_null")]
    pub display: serde_yaml::Value,
    #[serde(default)]
    pub plugins: PluginsSection,
    /// Skill library visibility. Skills are discovered from disk, so
    /// "installed" and "in the prompt" used to be the same thing; this
    /// is how one is switched off without deleting it.
    #[serde(default)]
    pub skills: SkillsSection,
    /// Individual hook HANDLERS switched off, one level finer than
    /// disabling the plugin that registered them.
    #[serde(default)]
    pub hooks: HooksSection,
    /// `[node]` — this machine's identity and capabilities as a compute
    /// backend in the pool (Plan B). See `NodeSection`.
    #[serde(default)]
    pub node: NodeSection,
    /// `[cluster]` — how this node joins and sees the rest of the pool.
    /// See `ClusterSection`.
    #[serde(default)]
    pub cluster: ClusterSection,
    /// `[vault]` — this node's rules for the secrets vault. Node-local:
    /// configsync never replicates a section it does not name.
    #[serde(default)]
    pub vault: VaultSection,
    /// `[pool]` — how a new machine joins the pool (`crate::pool`, QR
    /// pairing). Node-local: only the hub that mints WireGuard peers
    /// sets `[pool.wireguard]`.
    #[serde(default)]
    pub pool: PoolSection,
    /// Which automatic mechanisms announce themselves in the transcript.
    #[serde(default)]
    pub mechanisms: MechanismsSection,
    #[serde(default)]
    pub mcp_servers: std::collections::HashMap<String, serde_yaml::Value>,
    #[serde(default)]
    pub memory: MemorySection,
    /// Prompt-cache accounting and prefix-stability knobs. The providers
    /// veles is actually configured against cache automatically, so the
    /// dominant lever is not a header — it is keeping the prefix stable
    /// and being able to SEE whether it stayed stable.
    #[serde(default)]
    pub cache: CacheSection,
    /// Which builtin tools are resident in every prompt vs discoverable
    /// on demand (deferred). See `ToolsSection`.
    #[serde(default)]
    pub tools: ToolsSection,
    /// Tool-call approvals (M11). hermes' `approvals:` block shares the
    /// `mode` key, so an inherited hermes config keeps its posture.
    #[serde(default)]
    pub approvals: ApprovalsSection,
    /// Sandboxed script execution (unprivileged callers, cron no_agent
    /// jobs, execute_code's sandbox flag). Backend selection lives here
    /// since 2026-09-03: native (default) or podman; see
    /// `sandbox::backend_from`.
    #[serde(default)]
    pub sandbox: SandboxSection,
    /// Multi-user capability ACL (S15). Absent ⇒ legacy single-user
    /// posture (adapter allowlists remain the only gate).
    #[serde(default)]
    pub acl: crate::acl::AclConfig,
    /// Pre-mutation file snapshots (M13).
    #[serde(default)]
    pub checkpoints: CheckpointsSection,
    /// Outbound A2A peers (hermes `a2a_agents` section — same shape).
    #[serde(default)]
    pub a2a_agents: std::collections::HashMap<String, A2APeer>,
    /// TUI surface settings (theme name, keymap, …).
    #[serde(default)]
    pub tui: TuiSection,
    /// Capability classification for tools veles does not ship —
    /// plugin tools (`veles::register_tool`) and adapter tools. A name
    /// with no builtin classification is OWNER-ONLY by default
    /// (fail-closed, `acl::tool_gate`); this is how the owner opens one
    /// up: `[tool_caps] wordcount = "web"`. Values are capability names,
    /// or `"self"` for a tool that checks its own caps per action, or
    /// `"owner"` to say the default out loud. Builtin names are ignored
    /// here — their caps are not config-editable.
    #[serde(default)]
    pub tool_caps: std::collections::HashMap<String, String>,
}

/// TUI surface settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TuiSection {
    /// Theme name — a built-in (`synthwave`, `classic`, `amber`,
    /// `mono`) or a user theme from the config home's
    /// `themes/<name>.toml` (`theme::user_theme_dir()`).
    #[serde(default)]
    pub theme: Option<String>,
    /// Keyboard binding scheme: `normie` (default), `emacs`, `vi`.
    #[serde(default)]
    pub keymap: Option<String>,
    /// UI locale for user-facing strings (i18n). Unset → VELES_LOCALE
    /// → system LANG/LC_ALL → "en".
    #[serde(default)]
    pub locale: Option<String>,
    /// The SECOND language, for the swap (USER ask 2026-09-20: "a
    /// dedicated button to swap"). The picker's button toggles between
    /// `locale` and this; unset means the swap has nowhere to go and
    /// says so rather than picking a language on the user's behalf.
    #[serde(default)]
    pub locale_secondary: Option<String>,
}

/// `[node]` — this machine as a compute backend in the pool (Plan B,
/// 2026-09-20). Everything here is node-local by definition: it is the
/// one config that must NOT be replicated, because it describes the
/// machine reading it. The stable identity (`id`) is persisted in
/// `<config_dir>/node.toml` on first boot, not here — a config you can
/// copy between machines must not carry the identity that tells them
/// apart.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeSection {
    /// Display name. Unset → the machine's hostname, which is usually
    /// already the right name ("laptop", "vps").
    #[serde(default)]
    pub name: Option<String>,
    /// Availability class: `"always-on"` (the VPS — never sleeps, what
    /// cron and the telegram poller are pinned to) or `"ephemeral"`
    /// (a laptop — comes and goes). Drives placement once B4 lands.
    #[serde(default)]
    pub uptime_class: Option<String>,
    /// How the rest of the pool reaches this node (its A2A / WG URL).
    /// Advertised in the registry so peers can route work here.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// The node's static WAN address, when its uplink has one (B1.5).
    /// Registered as `asserted`; unset, the registry records nothing.
    /// No local probe stands in for it: the WAN address is what peers
    /// see a connection arrive from, and whether it is static is a
    /// property of the uplink, not of this host's configuration (a
    /// peer-observed source is B3a's). Placement (IRC CertFP/SASL wants
    /// a stable egress) reads this field; `endpoint` says reachable,
    /// this says STABLE.
    #[serde(default)]
    pub static_ip: Option<String>,
    /// Free-form capability overrides, merged OVER the probe
    /// (`caps.gpu` from nvidia-smi, etc). The door for capabilities
    /// no probe can see: "has the good microphone", "behind the fast
    /// uplink". JSON object.
    #[serde(default)]
    pub caps: std::collections::HashMap<String, serde_json::Value>,
    /// Node-local SHADOWS of replicated config keys (B2): the value
    /// THIS node reads for a key the pool otherwise shares. Dotted
    /// key → TOML value, e.g. `model.default = "deepseek-v4-flash-0731"`
    /// on the VPS while the laptop reads the merged default. The
    /// shadow applies at config LOAD, before any reader — the rest of
    /// the process cannot tell a shadowed key from a written one.
    /// Overridable keys are allowlisted ([`OVERRIDABLE_KEYS`]) to
    /// keep the file honest: overriding `cluster.seeds` would fork
    /// the pool's own wiring under a local key.
    #[serde(default)]
    pub overrides: std::collections::HashMap<String, toml::Value>,
}

/// `[cluster]` — how this node sees the pool. Plan B is gossip with a
/// static seed (the always-on node's WireGuard address earns its static
/// IP exactly here); the `bootstrap` field is deliberately a NAME, not
/// a mechanism, so a DHT backend (Plan C) slots in behind the same key
/// without a config rewrite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterSection {
    /// Bootstrap strategy: `"static"` (dial the seeds below; the only
    /// one B1 implements) — later `"dht"` (Plan C). Unknown values load
    /// but `node::bootstrap` refuses them by name, the same verdict
    /// shape as `validate_api_modes`.
    #[serde(default = "default_cluster_bootstrap")]
    pub bootstrap: String,
    /// Static seed addresses (`bootstrap = "static"`): the always-on
    /// node(s) a fresh worker dials first to learn the rest of the pool.
    #[serde(default)]
    pub seeds: Vec<String>,
    /// Gossip/heartbeat cadence, seconds. The same interval drives the
    /// registry heartbeat (B1) now and the peer exchange (B3) later.
    #[serde(default = "default_cluster_gossip_secs")]
    pub gossip_interval_secs: u64,
    /// A node whose heartbeat is older than this many seconds renders
    /// "stale" in `veles nodes` — not deleted (an ephemeral laptop that
    /// went to sleep is still a pool member), just marked.
    #[serde(default = "default_cluster_stale_secs")]
    pub stale_after_secs: u64,
    /// A2A peer identities (as the A2A server authenticates them — the
    /// names in `A2A_PEER_TOKENS`) whose memory sync offers this node
    /// STORES in its inbox. Empty (the default) refuses every offer by
    /// name: memory arriving from a peer the operator never named is not
    /// something to keep on disk, and storing is still not applying —
    /// `veles memory apply --inbox` is the explicit step.
    #[serde(default)]
    pub memsync_peers: Vec<String>,
    /// Names in `[a2a_agents]` this node EXCHANGES REGISTRY GOSSIP with
    /// on the heartbeat cadence (B3a). Empty (the default) = gossip
    /// off: nothing dials out. The peers listed here should also be in
    /// `memsync_peers` on the receiving side — the allowlist that gates
    /// inbound gossip is the same list that gates the sync inboxes.
    #[serde(default)]
    pub gossip_peers: Vec<String>,
    /// Names in `[a2a_agents]` (their A2A peer identities) allowed to run
    /// WORK on this node — `delegate_task backend = <this node>` from
    /// theirs (B4). Empty (the default) = remote work refused. Kept
    /// apart from `memsync_peers` on purpose (USER, 2026-09-28): a sync
    /// peer may drop offers into an inbox the operator applies by hand;
    /// a task peer runs a subagent here, with a shell, files and keys.
    #[serde(default)]
    pub task_peers: Vec<String>,
    /// Names in `[a2a_agents]` whose STORE sync offers (B5 replicas)
    /// this node stores in its storesync inbox. Empty (the default)
    /// refuses every offer. Kept separate from `memsync_peers` by the
    /// same reasoning as `task_peers`: memory is identity data the
    /// operator merges by hand; a store offer carries a peer's whole
    /// session history into a replica file — its own trust decision.
    #[serde(default)]
    pub storesync_peers: Vec<String>,
    /// Who may push a SESSION MIGRATION at this node (B6). Its own
    /// list, the task_peers precedent: a migration carries history
    /// that will be fed to an LLM here, so a sync peer or a task peer
    /// is not automatically a migration peer. An allowlisted push
    /// AUTO-ADOPTS (USER, 2026-09-28: the allowlist IS the target's
    /// consent; the operator already commanded the migration at the
    /// source). Empty default = this node adopts nothing.
    #[serde(default)]
    pub migrate_peers: Vec<String>,
    /// Public keys trusted to sign TICKETS — the authorization for a call
    /// another node asks this one to run beyond what the ACL's User role
    /// may (PLAN-pool-round.md Part D3, `crate::ticket`). Name → key. A
    /// gateway holds only these public halves. `[cluster]` is local to
    /// each node — configsync never carries it — so a peer cannot add a
    /// signer here.
    #[serde(default)]
    pub ticket_signers: std::collections::BTreeMap<String, TicketSignerEntry>,
    /// Headless signers this node asks for a ticket (Part S): the URLs of
    /// `veles signer` processes on OTHER devices, which put the call to
    /// the owner on their own Telegram bot or IRC nick and sign on an
    /// explicit yes. The bearer token is the secret `VELES_SIGNER_TOKEN`.
    /// Asked alongside whoever is attached to the session; the first
    /// ticket wins, and the owner's no on a signer is a no.
    #[serde(default)]
    pub signers: Vec<String>,
    /// AUTO store sync (B5 tail): push this node's log to every
    /// `storesync_peers` peer on the gossip cadence, and apply stored
    /// offers at receive time — continuous convergence instead of the
    /// hand-apply posture. Default OFF: the operator posture (stored
    /// offers, applied by `veles replicas apply --inbox`) stays the
    /// behavior unless this is set, and nothing dials out unasked.
    #[serde(default)]
    pub storesync_auto: bool,
    /// How far back a BACKFILL reaches, in days of session activity: the
    /// store log only sees writes made after it is installed, so a fresh
    /// install (and a peer that joins after pruning) re-sends the rows of
    /// every session active this recently — enough for a replica to take
    /// the session over. 0 = no backfill. `veles replicas backfill` runs
    /// one by hand.
    #[serde(default = "default_storesync_backfill_days")]
    pub storesync_backfill_days: u32,
    /// FAILOVER (PLAN-pool-round.md C2): hold a node lease at the
    /// arbiter (`node:<id>`), so that when this node stops answering the
    /// arbiter can let a survivor take its sessions over from a replica.
    /// Off (the default): nobody may take this node's sessions over — the
    /// arbiter refuses, since a node that never held the lease might just
    /// be one that does not take part.
    #[serde(default)]
    pub failover: bool,
    /// With `failover`: stop running turns in this node's own sessions
    /// while its node lease is lost, so a partition can never leave two
    /// writers. Off by default — a laptop that goes offline would stop
    /// working; without it, what each side writes during a partition is
    /// kept, as a fork, when they meet again.
    #[serde(default)]
    pub failover_fence: bool,
    /// Memory paths that are THIS NODE's own facts and must never be
    /// pushed to a peer (B1.5). Globs against the memory path
    /// (`system/core.md`); the `reference/nodes/` subtree is always local, whoever's
    /// it is. Without this, replication is a superset of the truth: a
    /// line like "laptop: ~/.hermes/notes/peer-demon.md" lands on the
    /// VPS, where it is false. Shared identity memory stays the default
    /// — only what a glob names stays put.
    #[serde(default)]
    pub local_memory_globs: Vec<String>,
    /// The node that arbitrates the singletons (cron scheduler,
    /// telegram poller, IRC connections). ONE node id (or unique
    /// prefix) — the lease B3a grants is fail-closed: the arbiter
    /// unreachable ⇒ nobody polls ⇒ no doubles, which at two nodes is
    /// the only safe answer. B3b replaces it with a Raft lease behind
    /// the same config key once ≥3 always-on nodes exist.
    #[serde(default)]
    pub arbiter: Option<String>,
    /// Default `delegate_task backend` for THIS node's delegates (B4):
    /// the same spellings the tool argument takes — `local`, a node
    /// name/id, `gpu`, `any`. Unset (the default) = local, the
    /// historical behavior; an explicit per-call `backend` always wins.
    /// This is the pool's "scheduling rules become config" knob — set
    /// `any` on the laptop and delegates spread to the least-loaded
    /// node without the model having to ask.
    #[serde(default)]
    pub delegate_backend: Option<String>,
    /// Where THIS node's sandboxed calls run when it has no sandbox of
    /// its own — the phone, which has no container runtime. A node id,
    /// unique prefix or name. When set, terminal, the file tools and
    /// execute_code whose zone is a sandbox go to that node (through
    /// [`crate::remotetool`], as `[tools.home]` calls do), where a peer's
    /// call lands in a box of its own and never on that machine's host.
    /// Unset (the default): a node that advertises no container runtime
    /// sends them to the least loaded task peer that does
    /// (`remotetool::auto_sandbox_home`, `[tools] auto_home`); with none,
    /// they run here and fail and say so. Node-local, like all of `[cluster]`: a
    /// replicated one would send the laptop's own sandbox calls away.
    #[serde(default)]
    pub sandbox_home: Option<String>,
    /// Memory replicates on its own (the memory half of
    /// `storesync_auto`): this node pushes its new memory revisions to
    /// every `memsync_peers` entry on the gossip cadence, and applies an
    /// offer it receives at once — fresh paths and fast-forwards land, a
    /// path both sides changed (a CONFLICT) stays in the inbox for
    /// `veles memory apply --inbox`, never silently resolved. Off by
    /// default: `veles memory push` / `apply --inbox` by hand.
    #[serde(default)]
    pub memsync_auto: bool,
    /// Config replicates on its own: this node logs its own config edits
    /// and pushes them to every `memsync_peers` entry on the gossip
    /// cadence, and applies an offer it receives at once — except a
    /// change to a security section (`acl`, `approvals`, `sandbox`,
    /// `hooks`, `plugins`, `mcp_servers`), which waits in the inbox for
    /// the owner (`/configsync apply`). On by default (USER 2026-10-04:
    /// "Config sync is manual push? Not what I expected"); it only does
    /// anything with `memsync_peers` set.
    ///
    /// It carries the vault too: a vault write goes to every peer that
    /// lacks it, and an offer merges on arrival into a vault this node
    /// already has (rows count only when a member signed them). Adopting
    /// a vault on the first sync (`--trust`) and admitting a node stay
    /// the operator's.
    #[serde(default = "default_true")]
    pub configsync_auto: bool,
}

/// `[pool]` — joining the pool by QR (`crate::pool`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PoolSection {
    /// The hub's WireGuard, when veles mints a tunnel for every node it
    /// invites (USER decision D10, 2026-10-11). Off while `subnet` is
    /// empty: the invite then carries the URL the owner names.
    #[serde(default)]
    pub wireguard: PoolWireguard,
}

/// `[pool.wireguard]` — the hub's tunnel, as a new peer needs to see it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PoolWireguard {
    /// The interface the root helper adds peers to.
    #[serde(default = "default_wg_interface")]
    pub interface: String,
    /// The tunnel's network, e.g. `10.0.30.0/24`. Empty = off.
    #[serde(default)]
    pub subnet: String,
    /// The hub's own address in it, e.g. `10.0.30.1`.
    #[serde(default)]
    pub address: String,
    /// Where peers dial the hub, `host:port`.
    #[serde(default)]
    pub endpoint: String,
    /// The hub's public key (base64, as `wg show` prints it).
    #[serde(default)]
    pub public_key: String,
    /// PersistentKeepalive for the peer, seconds (a phone behind NAT).
    #[serde(default = "default_wg_keepalive")]
    pub keepalive: u16,
}

fn default_wg_interface() -> String {
    "wg0".to_string()
}

fn default_wg_keepalive() -> u16 {
    25
}

impl Default for PoolWireguard {
    fn default() -> Self {
        Self {
            interface: default_wg_interface(),
            subnet: String::new(),
            address: String::new(),
            endpoint: String::new(),
            public_key: String::new(),
            keepalive: default_wg_keepalive(),
        }
    }
}

impl PoolWireguard {
    pub fn enabled(&self) -> bool {
        !self.subnet.trim().is_empty()
    }
}

/// One `[cluster.ticket_signers.<name>]` entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TicketSignerEntry {
    /// `ed25519:<base64url public key>` — what `veles ticket keygen`
    /// prints.
    pub key: String,
}

impl Default for ClusterSection {
    fn default() -> Self {
        Self {
            bootstrap: default_cluster_bootstrap(),
            seeds: Vec::new(),
            gossip_interval_secs: default_cluster_gossip_secs(),
            stale_after_secs: default_cluster_stale_secs(),
            memsync_peers: Vec::new(),
            gossip_peers: Vec::new(),
            task_peers: Vec::new(),
            storesync_peers: Vec::new(),
            storesync_auto: false,
            storesync_backfill_days: default_storesync_backfill_days(),
            failover: false,
            failover_fence: false,
            migrate_peers: Vec::new(),
            ticket_signers: Default::default(),
            signers: Vec::new(),
            local_memory_globs: Vec::new(),
            arbiter: None,
            delegate_backend: None,
            sandbox_home: None,
            memsync_auto: false,
            configsync_auto: true,
        }
    }
}

fn default_cluster_bootstrap() -> String {
    "static".to_string()
}
fn default_cluster_gossip_secs() -> u64 {
    30
}
fn default_cluster_stale_secs() -> u64 {
    300
}
fn default_storesync_backfill_days() -> u32 {
    14
}

/// `[vault]` — see `crate::vault`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VaultSection {
    /// Networks whose callers may send secret VALUES to this gateway's
    /// desktop API (the setup wizard's key fields, the secret dialog),
    /// besides loopback, which always may: CIDRs or single addresses,
    /// e.g. `["10.0.30.0/24"]` for a WireGuard subnet. The API is plain
    /// HTTP, so list only networks that are encrypted underneath — a
    /// WireGuard tunnel is; a LAN is not. Empty: loopback only.
    #[serde(default)]
    pub trusted_networks: Vec<String>,
}

/// One configured A2A peer (outbound client target).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct A2APeer {
    pub url: String,
    #[serde(default)]
    pub auth: A2APeerAuth,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct A2APeerAuth {
    #[serde(default, rename = "type")]
    pub kind: String, // "bearer"
    #[serde(default)]
    pub token: String,
}

impl VelesConfig {
    /// Find a custom provider by name ("custom:<name>" or bare name).
    pub fn provider(&self, key: &str) -> Option<&ProviderEntry> {
        let name = key.strip_prefix("custom:").unwrap_or(key);
        self.custom_providers.iter().find(|p| p.name == name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelSection {
    pub provider: Option<String>,
    pub default: Option<String>,
    /// Wire default for every provider entry that does not set its own
    /// `api_mode` (applied in `load` before validation). Empty/unset
    /// means chat_completions, the way it always was.
    pub api_mode: Option<String>,
    /// `[[model.binds]]` — which route a NEW session starts on, by who
    /// and where. See `ModelBind` and `crate::model_binds`.
    #[serde(default)]
    pub binds: Vec<ModelBind>,
}

/// One `[[model.binds]]` rule: a route for a class of caller.
///
/// `[model] default` is the route for everybody. This is the answer to
/// "everybody EXCEPT" — the case the operator actually has: strangers
/// and regulars in a channel on something cheap, the operator's own
/// sessions on something good, without either of them having to
/// remember `/model`.
///
/// Selectors are the same five `[sandbox.assign]` uses and are matched
/// by the same code (`crate::selector`), so "which rule is in force"
/// has one answer in this tree rather than two that drift. Most
/// specific wins; an absent selector is a wildcard.
///
/// `role` COMPARES: `"<=trusted"` is everyone at or below trusted,
/// which is how the common case is spelled. A bare `"trusted"` is still
/// exact.
///
/// Applied when a session is CREATED, and only to a session that has no
/// explicit choice of its own — `/model` always wins, and keeps winning
/// for the life of that session.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelBind {
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    /// `trusted`, `<=trusted`, `>=admin`, `<owner`, `>guest`.
    #[serde(default)]
    pub role: Option<String>,
    /// The route. At least one of the two must be set, or the rule says
    /// nothing and is reported as a problem rather than matching and
    /// changing nothing.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
}

/// The vision route — a model that can actually see images. The default
/// text models (deepseek-flash, glm-5.3) reject `image_url` parts, so the
/// `vision_analyze` tool needs a model from somewhere else. Unset is an
/// error at call time, not a silent fallback to a model that will 400.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VisionSection {
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// One generation route — which provider a media call spends its
/// prompt on. Unset means the LOCAL ComfyUI instance for image and
/// video (what those tools were born as; `COMFYUI_URL` names it), and
/// there is no local fallback for ASR. Set `provider` to a
/// `[providers]` entry and the call goes to that endpoint instead;
/// `api` names the wire shape ("openai" or "dashscope") — unset infers
/// it from the base_url, since a DashScope-compatible base cannot serve
/// the OpenAI media routes (it answers them with a bare, bodyless 404)
/// and its native paths are the only ones that work. Per-call routing
/// (the tool's arguments, the command's flags) overrides this, so one
/// default and one-off detours coexist.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GenSection {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Wire shape override: "openai" is `{base_url}/images/generations`
    /// (image) or `{base_url}/audio/speech` (tts); "dashscope" is the
    /// native `/api/v1/services/...` route — a different JSON shape, and
    /// video and ASR are ASYNC there (submit, poll, fetch the expiring
    /// result URL immediately). Unset infers dashscope from a
    /// `/compatible-mode`, `maas.` or `dashscope` base_url, else openai.
    #[serde(default)]
    pub api: Option<String>,
}

/// `[triggers]` — reactions to channel events that run without a turn
/// (plan item 1, 2026-09-14).
///
/// OFF by default, on purpose: forwarding unaddressed channel text to
/// another process is a real widening, and it is per-channel opt-in
/// through the trigger rows themselves (no row names the channel ⇒
/// nothing forwards) on top of this switch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggersSection {
    /// Master switch. Nothing observes, forwards or describes unless
    /// this is true.
    #[serde(default)]
    pub enabled: bool,
    /// How often the gateway polls slopdrop's timers, seconds. 0
    /// disables the tick.
    #[serde(default = "default_timers_poll_secs")]
    pub timers_poll_secs: u64,
    /// Brake on the bridge: forwards per channel per minute. The
    /// slopdrop interpreter is single-threaded; a busy channel must
    /// not starve `tcl` from the same channel.
    #[serde(default = "default_max_per_minute")]
    pub max_per_minute: u64,
}

impl Default for TriggersSection {
    fn default() -> Self {
        Self {
            enabled: false,
            timers_poll_secs: default_timers_poll_secs(),
            max_per_minute: default_max_per_minute(),
        }
    }
}

/// `[tokenplan]` — reading the UPSTREAM Alibaba plan's quota (`/quota`).
///
/// `[web]` — the search backends `web_search` queries, in order. The
/// default is a local SearXNG (podman quadlet on this laptop, see
/// `~/.config/containers/systemd/searxng.container`); a metasearch
/// daemon is the only keyless backend that does not live or die by one
/// engine's bot wall. Each entry is tried in turn and the first that
/// answers wins, so an offline searxng falls through to brave, then
/// DuckDuckGo. (DDG scraping alone proved 1-of-5 under bot-challenge in
/// a 2026-09-15 measurement; brave answered 5-of-5; searxng 5-of-5.)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSection {
    /// Ordered list of search backends: "searxng", "brave", "ddg".
    /// An empty list falls back to the built-in default order.
    pub backends: Vec<String>,
    /// Base URL of the local SearXNG JSON API.
    pub searxng_url: String,
    /// Per-request timeout for the loopback searxng call, seconds.
    pub searxng_timeout_secs: u64,
    /// Per-request timeout for the internet backends, seconds.
    pub remote_timeout_secs: u64,
    /// Command that STARTS a stopped SearXNG, run once when the first
    /// query hits a connection refused (the quadlet idle-watcher stops
    /// the container after SEARXNG_IDLE_SECS of quiet; waking costs ~5s
    /// and the brave fallback covers the cold window anyway). None
    /// disables the wake attempt. The default names this laptop's
    /// user unit; it fails harmlessly (fast non-zero, swallowed)
    /// anywhere the unit does not exist.
    pub searxng_wake_cmd: Option<String>,
    /// Launch the agent's browser and the guest browser WITH Chrome's
    /// automation flag, so pages see `navigator.webdriver = true`. Off by
    /// default: a person solving a bot challenge through the relay needs
    /// a browser that does not say it is automated, and Cloudflare turned
    /// down every solve in one that did (2026-10-11). On for a site that
    /// serves a declared automaton better. Read at each browser call;
    /// agent-browser relaunches a browser whose launch flags change, and
    /// the page it had open is gone (0.38.1, checked 2026-10-11).
    pub browser_webdriver: bool,
    /// Run the browsers headless, as they were before 2026-10-12. Off by
    /// default: where Xvfb is installed they draw headed on a private
    /// virtual display that nobody sees. Headless Chrome names itself
    /// (`HeadlessChrome` in its user agent and client hints), renders
    /// WebGL in software and reports an 800x600 screen under a 1280x720
    /// window. Cloudflare held it at "Just a moment…" while the same page
    /// let a headed one through on its own within 5 seconds (Coventry's
    /// Pure portal, 2026-10-12). Where Xvfb is missing they run headless
    /// whatever this says.
    pub browser_headless: bool,
    /// How this node carries web traffic for a session that is not the
    /// owner's (`egress.rs`, PLAN-challenge-relay.md D5/D6): `"direct"`
    /// (the default) like the owner's, `"proxy"` through
    /// [`Self::others_proxy`] and [`Self::others_searxng_url`] and never
    /// re-homed, or `"refuse"`. The pool's VPS says "proxy" (Tor), and
    /// the laptop "refuse": the USER does not want other people's
    /// searches and fetches to leave from their home address.
    pub others: String,
    /// The proxy a non-owner session's `web_extract` goes through when
    /// `others = "proxy"`: `socks5h://host:port`. Each session gets its
    /// own SOCKS user name, so a Tor `IsolateSOCKSAuth` port gives each
    /// its own circuit.
    pub others_proxy: Option<String>,
    /// The SearXNG a non-owner session's `web_search` asks when
    /// `others = "proxy"`. Nothing else is asked for them: the direct
    /// backends would leave from this node's own address.
    pub others_searxng_url: Option<String>,
    /// `others = "refuse"`: the node (its id) to send other people's
    /// searches, fetches and browser calls to instead of refusing them,
    /// one that carries them through its proxy. The laptop names the
    /// VPS, so the owner's `/egress tor` from the desktop reaches Tor.
    pub others_via: Option<String>,
}

fn default_searxng_wake_cmd() -> Option<String> {
    Some("systemctl --user start searxng.service".into())
}

impl Default for WebSection {
    fn default() -> Self {
        Self {
            backends: vec!["searxng".into(), "brave".into(), "ddg".into()],
            searxng_url: "http://127.0.0.1:8888".into(),
            searxng_timeout_secs: 10,
            remote_timeout_secs: 15,
            searxng_wake_cmd: default_searxng_wake_cmd(),
            browser_webdriver: false,
            browser_headless: false,
            others: "direct".into(),
            others_proxy: None,
            others_searxng_url: None,
            others_via: None,
        }
    }
}

/// Off by default. Turning it on needs an AccessKey pair, which is a
/// different credential from the `sk-sp-*` inference keys in
/// `[[custom_providers]]`: those cannot read quota at all (their gateway
/// 404s every billing path), and reading quota is account management
/// under a separate key. See `crate::tokenplan`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPlanSection {
    /// Master switch. Nothing is fetched, cached or polled unless true.
    #[serde(default)]
    pub enabled: bool,
    /// Env var (or `.env` key) holding the AccessKey ID. Path B — the
    /// team plan's credits, seats and spend.
    ///
    /// The ID is an identifier, not a secret, and deliberately does NOT
    /// match `secrets::name_is_secret_shaped`: Alibaba AKIDs begin
    /// `LTAI`, which is not in `value_is_credential_shaped`'s prefix
    /// list either, so it is not scrubbed from tool output. The SECRET
    /// below is, by name. That split is a decision, not an oversight.
    #[serde(default = "default_ak_id_env")]
    pub access_key_id_env: String,
    /// Env var holding the AccessKey SECRET. The name ends in `_SECRET`
    /// so `secrets::name_is_secret_shaped` catches it and the scrubber
    /// redacts it out of any tool output that prints it.
    #[serde(default = "default_ak_secret_env")]
    pub access_key_secret_env: String,
    /// Which ModelStudio host answers. `cn-beijing` serves the same
    /// data; the account's own region is the sane choice.
    #[serde(default = "default_tokenplan_region")]
    pub region: String,
    /// How long a fetched snapshot stays good, seconds. Every surface
    /// reads the cache; only an explicit `/quota refresh` (or the
    /// poller) goes to Singapore.
    #[serde(default = "default_quota_ttl_secs")]
    pub cache_ttl_secs: u64,
    /// How often the gateway warms the cache in the background,
    /// seconds. 0 disables the poll entirely — the cache is then filled
    /// only by whoever asks first, and the desktop's status field shows
    /// nothing until they do.
    #[serde(default = "default_quota_poll_secs")]
    pub poll_secs: u64,
    /// Path A — the personal plan's rolling windows and free tier,
    /// which answer to a console session a browser mints
    /// (`bl auth login --console`). This is where that CLI writes it.
    /// `~` is expanded. Set `ALIBABA_CONSOLE_TOKEN` in `.env` to
    /// override the file entirely.
    #[serde(default = "default_console_config")]
    pub console_config: String,
    /// The z.ai coding plan's quota, read with the plan's own `sk-…`
    /// key (2026-09-16). Unlike the two Alibaba paths there is no
    /// console dance: `GET {base}/api/monitor/usage/quota/limit`
    /// answers to a plain Bearer of the inference key, so a key in
    /// `.env` is the whole story. The base is configurable because the
    /// CN mirror of the same plan answers at `open.bigmodel.cn` with
    /// the same path.
    #[serde(default = "default_zai_base")]
    pub zai_base_url: String,
    /// Env var holding that key. The name matches the secrets block so
    /// the scrubber redacts it from tool output.
    #[serde(default = "default_zai_key_env")]
    pub zai_key_env: String,
    /// The Kimi Code plan's quota, read with the plan's own `sk-kimi-…`
    /// key (2026-09-20). Same story as z.ai — `GET {base}/usages`
    /// answers a plain Bearer, so a key in `.env` is the whole tale —
    /// and the same reason for a configurable base: the CN hostname
    /// (`api.kimi.com`) and the overseas one (`api.kimi.ai`) serve the
    /// identical payload.
    #[serde(default = "default_kimi_base")]
    pub kimi_base_url: String,
    /// Env var holding that key.
    #[serde(default = "default_kimi_key_env")]
    pub kimi_key_env: String,
}

/// `[notify]` — the ask that follows you off the surface it was born on.
///
/// PENDING §"Cross-session clarify relay", USER verbatim: "in that case
/// I should be asked on telegram? why is nothing asking the clarify at
/// all?" — a `clarify` from a desktop session timed out unanswered
/// while the owner was on Telegram the whole time.
///
/// The ask-relay that already exists fans a question out to the other
/// live legs of the SAME session id, which is correct as designed and
/// reaches nobody who has walked away: a desktop session and a Telegram
/// DM are different session ids. This is the cross-session half, and it
/// is deliberately the light version of "push notifications" — Telegram
/// already has push, on a device the owner already carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifySection {
    /// Master switch for the relay. Off means the ask behaves exactly
    /// as it did: head leg, same-session fan-out, timeout.
    #[serde(default)]
    pub ask_relay: bool,
    /// How long the ask gets on the surface it was born on before the
    /// owner's phone is involved, seconds.
    ///
    /// NOT zero and NOT the ask's own timeout. Zero buzzes the phone
    /// for every approval you answer at the keyboard in two seconds;
    /// waiting for the 300s timeout means the turn has usually already
    /// taken the timeout answer, which makes the message a notification
    /// of something that is over. A minute is "you are not at the
    /// machine".
    #[serde(default = "default_ask_relay_delay")]
    pub ask_relay_delay_secs: u64,
    /// Which kinds of question may follow you: `approval`, `clarify`,
    /// `failover`. Anything not listed stays where it was asked.
    #[serde(default = "default_ask_relay_kinds")]
    pub ask_relay_kinds: Vec<String>,
}

fn default_ask_relay_delay() -> u64 {
    60
}

fn default_ask_relay_kinds() -> Vec<String> {
    vec!["approval".into(), "clarify".into(), "failover".into()]
}

impl Default for NotifySection {
    fn default() -> Self {
        Self {
            ask_relay: false,
            ask_relay_delay_secs: default_ask_relay_delay(),
            ask_relay_kinds: default_ask_relay_kinds(),
        }
    }
}

impl Default for TokenPlanSection {
    fn default() -> Self {
        Self {
            enabled: false,
            access_key_id_env: default_ak_id_env(),
            access_key_secret_env: default_ak_secret_env(),
            region: default_tokenplan_region(),
            cache_ttl_secs: default_quota_ttl_secs(),
            poll_secs: default_quota_poll_secs(),
            console_config: default_console_config(),
            zai_base_url: default_zai_base(),
            zai_key_env: default_zai_key_env(),
            kimi_base_url: default_kimi_base(),
            kimi_key_env: default_kimi_key_env(),
        }
    }
}

fn default_ak_id_env() -> String {
    "ALIBABA_ACCESS_KEY_ID".to_string()
}
fn default_ak_secret_env() -> String {
    "ALIBABA_ACCESS_KEY_SECRET".to_string()
}
fn default_tokenplan_region() -> String {
    crate::tokenplan::DEFAULT_REGION.to_string()
}
/// Five minutes. Credits move on the scale of a turn, not a second, and
/// a UI that repaints every 15s must not mean a Singapore round trip
/// every 15s.
fn default_quota_ttl_secs() -> u64 {
    300
}
/// Fifteen minutes — long enough that the background warm is invisible,
/// short enough that the status field is never an hour stale.
fn default_quota_poll_secs() -> u64 {
    900
}
fn default_console_config() -> String {
    "~/.bailian/config.json".to_string()
}

fn default_zai_base() -> String {
    "https://api.z.ai".to_string()
}
fn default_zai_key_env() -> String {
    "ZAI_CODING_PLAN_API_KEY".to_string()
}
/// The CN hostname Kimi's own docs and hermes' kimi-coding profile use.
/// The overseas twin `api.kimi.ai` serves the same payload — probed
/// 2026-09-20, both fleet keys, both hostnames — so either works and the
/// one the inference route already talks to is the default.
fn default_kimi_base() -> String {
    "https://api.kimi.com/coding/v1".to_string()
}
fn default_kimi_key_env() -> String {
    "KIMI_API_KEY".to_string()
}

fn default_timers_poll_secs() -> u64 {
    10
}
fn default_max_per_minute() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSection {
    /// hermes: agent.reasoning_effort (e.g. "xhigh").
    pub reasoning_effort: Option<String>,
    /// Outbound inference User-Agent policy. Some providers gate features
    /// behind "approved" harnesses; this lets veles present an approved
    /// wire profile. Values: "hermes" | "claw" | a literal custom string.
    /// Unset → honest `veles-agent/<version>`. Overridden by
    /// VELES_USER_AGENT in the config home's `.env`.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Context compression (M10). Defaults keep it conservative: trigger
    /// only when history is genuinely large.
    #[serde(default)]
    pub compaction: CompactionSection,
    /// TTS provider name override (default: use the inference provider).
    #[serde(default)]
    pub tts_provider: Option<String>,
    /// TTS voice name (provider-specific, e.g. "alloy", "nova").
    #[serde(default)]
    pub tts_voice: Option<String>,
    /// TTS model name (default: "tts-1").
    #[serde(default)]
    pub tts_model: Option<String>,
    /// May veles `pip install --user edge-tts` on the host when the
    /// fallback voice is missing? Default FALSE.
    ///
    /// The fallback is reachable from a chat message — any caller with
    /// `tts` whose provider route fails lands on it — and installing a
    /// package runs code from an index on this machine. That is not a
    /// thing a stranger's "say hello" should be able to trigger, so the
    /// tool refuses and names the command instead.
    #[serde(default)]
    pub tts_autoinstall: bool,
    /// TTS wire shape override: "openai" (`/audio/speech`, raw bytes)
    /// or "dashscope" (`/api/v1/services/audio/tts/SpeechSynthesizer`,
    /// `input.{text,voice,format}` body, expiring `audio.url` fetched
    /// immediately). Unset INFERRES it from the provider's base_url —
    /// the shape belongs to the provider, so a `/gen --provider` switch
    /// cannot strand the call on the wrong door.
    #[serde(default)]
    pub tts_api: Option<String>,
    /// Harness guards (ported from hermes' guard family).
    #[serde(default)]
    pub guards: GuardsSection,
    /// Which engine injections enter the model's context (RM3-S4/A4).
    /// Every injection is VISIBLE in the transcript as a labelled
    /// block regardless; these knobs only decide whether the model
    /// READS it. Compaction is structural (always sent) and steer is
    /// the user's own words (always sent); the nudge keeps its own
    /// guards.verification_nudge knob.
    #[serde(default)]
    pub injections: InjectionsSection,
    /// Head+tail truncation applied to EVERY tool result before it
    /// enters history (0 = no cap). The terminal is the most-called
    /// tool and used to cap nothing — one `cargo build` could write
    /// megabytes into history that every later round re-sends.
    #[serde(default = "default_tool_output_cap")]
    pub tool_output_cap: usize,
    /// Within-turn failover stickiness.
    #[serde(default)]
    pub failover: FailoverBehaviorSection,
}

/// Failover BEHAVIOR knobs (distinct from the `failover_rules` policy
/// table, which decides WHETHER a failure may advance at all).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailoverBehaviorSection {
    /// Once a round lands on route N>0, later rounds of the SAME turn
    /// start there instead of re-trying route[0] every call — three
    /// cold prefixes for one turn is the failure this prevents. Resets
    /// at the turn boundary (preserving the documented "reverts when
    /// the usage window resets" behavior).
    #[serde(default = "default_true")]
    pub sticky_within_turn: bool,
}

impl Default for FailoverBehaviorSection {
    fn default() -> Self {
        Self {
            sticky_within_turn: true,
        }
    }
}

/// The defaults serde fills in, and nothing else — one source.
///
/// This was `#[derive(Default)]`, which gave every field its TYPE's
/// default while serde gave it the field's: `tool_output_cap` was 30000
/// in a config with an `[agent]` table and 0 (no cap at all) in one
/// without, and `veles config defaults` documented the 0 (found
/// 2026-10-04, when fill stopped writing values and an all-comment file
/// stopped meaning the defaults).
impl Default for AgentSection {
    fn default() -> Self {
        toml::from_str("").expect("every [agent] field has a serde default")
    }
}

fn default_tool_output_cap() -> usize {
    30_000
}

/// Harness-guard knobs. All defaults are ON — the whole point is that
/// veles stops claiming "done" without evidence and stops looping on
/// identical failing calls.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GuardsSection {
    /// Hard cap on tool rounds per turn (hermes max_iterations=500).
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Identical-call repeat count that produces a warning.
    #[serde(default = "default_guardrail_warn")]
    pub guardrail_warn_at: u32,
    /// Identical-call repeat count that halts the turn.
    #[serde(default = "default_guardrail_halt")]
    pub guardrail_halt_at: u32,
    /// Verification nudge (the "checker"): at turn end, when code was
    /// edited without fresh passing evidence, inject a synthetic USER
    /// message telling the model to go and verify.
    ///
    /// DEFAULT OFF. It is the most expensive mechanism in the harness
    /// per unit of proven value: each firing costs a full extra round —
    /// the whole prefix plus the whole history — up to
    /// `verification_max_attempts` times per turn, and it fires on the
    /// ordinary edit-then-report shape, not just on sloppy turns. It
    /// also puts words in the USER's mouth: the transcript grows a
    /// message the user never wrote, which is the hermes habit this
    /// project exists to refuse.
    ///
    /// Kept, not deleted, and one line turns it back on:
    /// `[agent.guards] verification_nudge = true`.
    #[serde(default)]
    pub verification_nudge: bool,
    /// Max nudges per turn (bounded; hermes uses 2).
    #[serde(default = "default_nudge_attempts")]
    pub verification_max_attempts: u32,
    /// Verification HINT — the cheap replacement for the nudge above.
    ///
    /// USER: "most of this was pilfered from hermes or letta and they're
    /// not the sharpest minds, but if you can think of a better way of
    /// doing it then please try it".
    ///
    /// Same signal, three things fixed:
    ///
    /// 1. **It costs no round.** One line is appended to the edit tool's
    ///    OWN result — a message the model was already going to receive
    ///    — instead of provoking another full prefix + history.
    /// 2. **It arrives when acting is cheap.** The nudge fires after the
    ///    model has composed a final answer, so obeying means throwing
    ///    that answer away. The hint lands at the moment the model is
    ///    picking its next tool call.
    /// 3. **It puts no words in the user's mouth.** The nudge writes a
    ///    synthetic USER message into the transcript forever; this is
    ///    tool output, which is what it actually is.
    /// 4. **It only speaks when it has something to say.** Until
    ///    2026-09-03 it fired on every code edit with a line that was
    ///    true by construction — "no passing check has run since this
    ///    edit", and none can have. It now waits for the SECOND
    ///    unchecked edit and carries the count.
    ///
    /// The two are independent and can both be on; `verification_nudge`
    /// remains for anyone who wants the hermes behaviour.
    #[serde(default = "default_true")]
    pub verification_hint: bool,
    /// Refuse a FOREGROUND `terminal` command that sleeps this many
    /// seconds or more (0 = off). A long foreground sleep holds the
    /// whole turn, and everything that finishes meanwhile — background
    /// shells, monitors, subagents — waits behind it: the live store
    /// held ~300 `sleep ≥ 20` calls in ten days, up to `sleep 1500`,
    /// usually right after starting background work that reports by
    /// itself. Claude Code blocks foreground sleeps the same way; the
    /// refusal names what to do instead (end the turn, `shell wait`,
    /// `monitor`). Short sleeps and poll loops still run.
    #[serde(default = "default_foreground_sleep_max")]
    pub foreground_sleep_max_secs: u64,
}

fn default_foreground_sleep_max() -> u64 {
    10
}

/// Which engine injections the model READS (RM3-S4/A4). Visibility is
/// not negotiable — every injection renders in the transcript as a
/// labelled block whatever these say. Off means "log it, show it, but
/// do not spend the model's context on it".
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InjectionsSection {
    /// Background subagent/shell/monitor completions enter the context
    /// as user-role events. Off: they stay in the transcript (and the
    /// status count) and the model is not told.
    #[serde(default = "default_true")]
    pub background: bool,
    /// The open TODO LIST, re-injected after compaction. Named for what
    /// it carries: this knob and its caption both used to say "plan",
    /// which is a different thing that lives under `plan` below and was
    /// not preserved at all (USER 2026-09-18).
    #[serde(default = "default_true")]
    pub todo: bool,
    /// The plan this session PRESENTED (plan mode's `present_plan`),
    /// re-injected after compaction. The plan lives in session meta and
    /// in no history row — the ask that carried it is answered — so a
    /// compaction that did not re-inject it lost it outright, and the
    /// model's only route back was reading state.db.
    #[serde(default = "default_true")]
    pub plan: bool,
    /// A compact_start hook's extra context.
    #[serde(default = "default_true")]
    pub hook: bool,
    /// IRC channel context attached to addressed channel turns (an
    /// adapter sends it with `InboundMessage.context` when
    /// `platforms.irc.inject_channel_context` or observe mode is on).
    /// Off: the transcript still shows the ⇒ labelled block — the model
    /// is just not spending context on it.
    #[serde(default = "default_true")]
    pub irc_context: bool,
    /// The atproto surface framing + thread context an atproto turn
    /// carries (the persona/capability note and, for a re-mention, the
    /// thread's posts). Same injection mechanism as `irc_context`; a
    /// separate knob so one surface's context can be read without the
    /// other's. Default on — the framing is what makes the bot behave on
    /// a public feed.
    #[serde(default = "default_true")]
    pub atproto_context: bool,
}

impl InjectionsSection {
    /// Is the rolling/per-turn CONTEXT injection on for this platform?
    /// The mechanism is generic (`InboundMessage.context`); the knob is
    /// per-surface so each can be read independently. A platform with no
    /// named knob reads its context by default (the visible transcript
    /// row is never gated — only whether the model spends context on it).
    pub fn rolling_context(&self, platform: &str) -> bool {
        match platform {
            "irc" => self.irc_context,
            "atproto" => self.atproto_context,
            _ => true,
        }
    }
}

impl Default for InjectionsSection {
    fn default() -> Self {
        Self {
            background: true,
            todo: true,
            plan: true,
            hook: true,
            irc_context: true,
            atproto_context: true,
        }
    }
}

fn default_max_iterations() -> u32 {
    500
}
fn default_guardrail_warn() -> u32 {
    3
}
fn default_guardrail_halt() -> u32 {
    5
}
fn default_nudge_attempts() -> u32 {
    2
}

impl Default for GuardsSection {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            guardrail_warn_at: default_guardrail_warn(),
            guardrail_halt_at: default_guardrail_halt(),
            verification_nudge: false,
            verification_max_attempts: default_nudge_attempts(),
            verification_hint: true,
            foreground_sleep_max_secs: default_foreground_sleep_max(),
        }
    }
}

/// Context compression knobs (M10). Trigger is a PERCENTAGE of the
/// resolved context window (user directive 2026-08-10 — a fixed char
/// budget is wrong because windows differ wildly per model).
/// Resolution order for the window: per-model context_length in the
/// provider's models map → provider-level context_length →
/// fallback_context_tokens. Originals are ALWAYS archived, never
/// deleted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSection {
    /// Compact when history reaches this percent (0–100) of the
    /// context window. 0 disables compaction.
    #[serde(default = "default_trigger_percent")]
    pub trigger_percent: f64,
    /// Context window to assume when neither the model nor the
    /// provider declares context_length (tokens).
    #[serde(default = "default_fallback_context_tokens")]
    pub fallback_context_tokens: u64,
    /// Hard override: if > 0, this char threshold wins over the
    /// percent-of-window calculation (ops/testing escape hatch).
    #[serde(default)]
    pub threshold_chars: usize,
    /// Keep at most this many recent messages uncompressed.
    #[serde(default = "default_keep_recent")]
    pub keep_recent: usize,
    /// Don't compress unless at least this many head messages get
    /// summarized (avoids pointless tiny compressions).
    #[serde(default = "default_min_head")]
    pub min_head: usize,
    /// Per-message cap in the summarizer transcript (tool output noise).
    #[serde(default = "default_per_msg_cap")]
    pub per_msg_cap: usize,
    /// Mid-turn compaction threshold (percent of window). Routine
    /// compaction runs once at TURN START; mid-turn, only a window
    /// about to overflow (this percent) triggers it — compacting
    /// mid-turn invalidates the cached suffix at the exact moment the
    /// turn is deepest into it. 0 = never compact mid-turn.
    #[serde(default = "default_mid_turn_percent")]
    pub mid_turn_percent: f64,
    /// Effective-budget CAP (tokens). The compaction bar is
    /// min(context_window × trigger_percent, this): a route that
    /// declares a huge context window must not make compaction
    /// unreachable in practice — "window declared as 1M tokens, so
    /// 85% never fires" was the actual failure. 0 = no cap (escape
    /// hatch; `threshold_chars` remains the explicit hard override).
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: u64,
    /// The soft WARN line (tokens). History crossing it fires ONE
    /// EngineNote per session naming /compress — nothing implicit;
    /// the user hears about a long context before the engine rewrites
    /// it. 0 = never warn.
    #[serde(default = "default_warn_context_tokens")]
    pub warn_context_tokens: u64,
    /// What compaction AIMS at, as a percent of the warn line. Compacting
    /// down TO the warn line is pointless — the next round re-crosses it
    /// and the note fires again, so the session sits at the bar forever.
    /// Aiming well below it leaves room to actually work: 33% of a 200k
    /// warn line means a compaction lands near 66k (USER, 2026-09-13:
    /// "the budget should definitely aim at <1/3 the warning threshold,
    /// so 66k for us. not 200k because then we're immediately back at
    /// the warning").
    ///
    /// The target is applied to the WHOLE prompt: the fixed per-round
    /// prefix (system prompt + tool schemas) is subtracted before the
    /// budget is handed to history, so what lands near the target is
    /// what the model is actually sent, not just the history slice.
    /// 0 = aim at the compaction bar itself (the old behaviour).
    #[serde(default = "default_target_percent")]
    pub target_percent: f64,
}

fn default_trigger_percent() -> f64 {
    85.0
}
fn default_fallback_context_tokens() -> u64 {
    128_000
}
fn default_keep_recent() -> usize {
    12
}
fn default_min_head() -> usize {
    6
}
fn default_per_msg_cap() -> usize {
    2_000
}
fn default_mid_turn_percent() -> f64 {
    97.0
}
fn default_max_context_tokens() -> u64 {
    450_000
}
fn default_warn_context_tokens() -> u64 {
    200_000
}
fn default_target_percent() -> f64 {
    33.0
}

impl Default for CompactionSection {
    fn default() -> Self {
        Self {
            trigger_percent: default_trigger_percent(),
            fallback_context_tokens: default_fallback_context_tokens(),
            threshold_chars: 0,
            keep_recent: default_keep_recent(),
            min_head: default_min_head(),
            per_msg_cap: default_per_msg_cap(),
            mid_turn_percent: default_mid_turn_percent(),
            max_context_tokens: default_max_context_tokens(),
            warn_context_tokens: default_warn_context_tokens(),
            target_percent: default_target_percent(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FallbackEntry {
    pub provider: String,
    pub model: Option<String>,
}

/// Failover policy for one scope (S16). `mode` decides what happens when
/// a route fails with a fallback-class error:
///   - "ask"    (default): the failure becomes a multi-step ask — the
///     user sees the failing route + error and chooses retry / next
///     route / stop. Interactive surfaces only.
///   - "auto":  walk the chain silently (hermes behavior).
///   - "stop":  never fall back; surface the error immediately.
///   - "script": like auto but fires `failover` hooks at each decision
///     so a Tcl plugin can drive it headlessly.
///
/// Scope specificity: a rule with an explicit model matches only that
/// (provider, model) pair; provider-only rules match every call on that
/// provider; `default` covers everything else. FIRST match wins; the
/// implicit default is `stop` (failover disabled) on every surface,
/// unless an explicit rule or session override opts back in here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FailoverRule {
    /// Provider name as configured in custom_providers ("*" or omitted
    /// = any).
    #[serde(default)]
    pub provider: Option<String>,
    /// Model name ("*" or omitted = any model on the provider).
    #[serde(default)]
    pub model: Option<String>,
    /// ask | auto | stop | script
    #[serde(default)]
    pub mode: Option<String>,
    /// For `script` mode: which script in the config home's
    /// `failover-scripts/` (`failover::scripts_dir()`) runs for this
    /// scope. Omitted = the historic single `failover` hook (the
    /// `plugins/failover/plugin.tcl` handler, under the config home's
    /// `plugins/`).
    #[serde(default)]
    pub script: Option<String>,
    /// Cap how many consecutive asks/auto-advances happen before giving
    /// up (protects against walking a long chain while asleep at 3am).
    #[serde(default)]
    pub max_hops: Option<u32>,
    /// Default duration (seconds) for a "wait for usage reset" action.
    /// 5-hour rolling windows want ~1800; RPM cooldowns want 60-300.
    #[serde(default)]
    pub wait_default_secs: Option<u64>,
    /// Aux-model reconfiguration: when this scope rate-limits, aux-task
    /// consumers (titles, approval screens, reasoning-summary plugins,
    /// subagents) get pointed at this (provider, model) pair instead of
    /// competing with the main brain on the throttled route. Cheap-model
    /// discipline: deepseek-flash-class routes need this when several
    /// aux consumers share one plan's RPM budget.
    #[serde(default)]
    pub aux_fallback_provider: Option<String>,
    #[serde(default)]
    pub aux_fallback_model: Option<String>,
}

impl FailoverRule {
    pub fn mode(&self) -> &str {
        self.mode.as_deref().unwrap_or("stop")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginsSection {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default)]
    pub disabled: Vec<String>,
    /// May a plugin's `veles::config_set` write `[acl]` or `[sandbox]`?
    ///
    /// Default NO. Those two sections decide what every other caller
    /// may do, and `[acl]` is live-reloaded, so an unrestricted
    /// `config_set` was a way for plugin code to grant itself owner
    /// with immediate effect. Plugins are owner-installed, so this was
    /// never reachable by a stranger — but it was an undeclared
    /// capability, and the point of a knob is that somebody chose.
    #[serde(default)]
    pub allow_acl_writes: bool,
}

/// `native` everywhere; `auto` on an Android build, where it comes to the
/// userland package's boxes when the userland answers and to `native`
/// when it does not (`sandbox::resolve_backend`). Spelled in the config
/// so `veles config defaults` on the phone says what the default is.
fn default_sandbox_backend() -> String {
    if cfg!(target_os = "android") {
        "auto".into()
    } else {
        "native".into()
    }
}
fn default_sandbox_image() -> String {
    "veles-sandbox:latest".into()
}
fn default_sandbox_memory_mb() -> u64 {
    256
}
fn default_sandbox_network() -> bool {
    true
}
fn default_user_env_network() -> String {
    "isolated".into()
}
fn default_sandbox_capabilities() -> String {
    "drop".into()
}
fn default_user_env_idle_stop_secs() -> u64 {
    3600
}

/// `[sandbox]` — how unprivileged script execution is isolated. The
/// execution half (safe-interp driver, scratch workspace, timeout) is
/// shared by every backend; this section picks the FENCE around it.
/// See `sandbox.rs` for the backend contract and the documented next
/// backends (nspawn / firecracker / jail).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSection {
    /// `native` (a.k.a. `host` — a tclsh subprocess ON THE HOST +
    /// RLIMIT_AS; the historical behavior, still the default: some jobs
    /// only make sense with real host access) or `podman` (rootless
    /// container). Unknown names fail loudly at run time. On Android the
    /// default is `auto`: the userland package's boxes where it answers
    /// (`userland`), else `native`; naming a backend, `native` to opt out,
    /// always wins. Node-local (configsync never replicates it).
    #[serde(default = "default_sandbox_backend")]
    pub backend: String,
    /// Container image for container backends. Contract: tclsh on PATH
    /// inside the image. `--pull=never` applies — the image must exist
    /// locally; veles never fetches code at sandbox time.
    #[serde(default = "default_sandbox_image")]
    pub image: String,
    /// Memory cap in MB. native enforces it best-effort via RLIMIT_AS;
    /// podman enforces it via cgroups. Default matches
    /// `sandbox::SANDBOX_MEM_LIMIT_BYTES`.
    #[serde(default = "default_sandbox_memory_mb")]
    pub memory_mb: u64,
    /// Network access inside container backends. Default TRUE (the dev
    /// sandbox posture: models install packages, fetch, browse). Set
    /// `network = false` for the benchmark / untrusted posture —
    /// podman maps it to `--network none`, nspawn to
    /// `--private-network`. native cannot police this and ignores it —
    /// the safe interp simply has no socket commands.
    ///
    /// For the per-run podman fence `true` passes no `--network` flag:
    /// the container's own namespace (pasta), the same isolated posture
    /// as `user_env_network` below. It was `--network host` until
    /// 2026-10-02 — the host's loopback, and through the VPS's sandbox
    /// socket the VPS's whole network.
    #[serde(default = "default_sandbox_network")]
    pub network: bool,
    /// Network POSTURE for user-env containers (`user_envs.rs`) — the
    /// per-principal boxes every below-Admin caller is routed into.
    ///
    /// This exists because `network = true` used to mean `--network
    /// host` here, which shares the HOST's network namespace: a
    /// channel member's "sandbox" sat on the host's loopback, where
    /// `desktop_api` served the operator API with no auth at all,
    /// precisely because "nobody else can reach loopback". That was a
    /// full escalation from `Role::User` to owner, and it made the claim
    /// in `acl.rs` that sandbox-scoped caps never buy host access false.
    ///
    /// The desktop API has since grown a bearer token it mints by
    /// default, so that ONE route is no longer the escalation it was.
    /// This knob is not relaxed for it: the host's loopback also carries
    /// the A2A server, whatever database or hermes instance is running,
    /// and every other daemon that trusts being unreachable — and a
    /// fence whose safety depends on enumerating them is not a fence.
    ///
    ///   `isolated` (default) — no `--network` flag at all, so podman
    ///       rootless uses its own default (pasta): outbound works, and
    ///       the host's 127.0.0.1 is NOT the container's. That outbound
    ///       has a precondition: a rootless podman needs pasta or
    ///       slirp4netns installed, and with neither it still creates the
    ///       box — with no network at all, and nothing to publish into
    ///       it. `ensure` warns once when this host is that host, and
    ///       `veles doctor --sandbox` reports it as a problem.
    ///   `none`     — `--network none`; no network at all.
    ///   `host`     — the old behaviour. Reachable only by naming it,
    ///       because it hands every routed caller the host's loopback.
    ///
    /// `network = false` still wins outright: that knob is the
    /// untrusted posture and it means no network, whatever this says.
    /// Unknown values fall back to `isolated` — the tighter answer, not
    /// the looser one.
    #[serde(default = "default_user_env_network")]
    pub user_env_network: String,
    /// Capability posture for container backends: `drop` (default —
    /// `--cap-drop ALL`, the hardened pure-compute fence) or `default`
    /// (the runtime's rootless default set, needed for package
    /// installation INSIDE the image: dpkg/pip want CAP_CHOWN & co
    /// within the userns). Host safety never comes from this knob — it
    /// comes from rootless isolation itself. Unknown values fail loudly.
    #[serde(default = "default_sandbox_capabilities")]
    pub capabilities: String,
    /// Kernel image for the firecracker backend (the only consumer;
    /// every other backend ignores it). No default — a firecracker
    /// config without a kernel fails loudly at backend resolution.
    #[serde(default)]
    pub kernel_path: Option<String>,
    /// Explicit host access, default DENY: nothing is mounted unless
    /// listed here. Each entry is a bind into the container with an
    /// explicit mode — `ro` or `rw`; anything else fails loudly. The
    /// trust boundary is whoever edits this file. Ignored by native
    /// (which is the host already).
    #[serde(default)]
    pub mounts: Vec<SandboxMount>,
    /// Backend escape hatch: extra argv appended to the container
    /// command (e.g. `--cpus`, `1`) — also the extension point for
    /// restrictions this section does not have a knob for yet. Ignored
    /// by native.
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// User envs (persistent per-principal containers, `user_envs.rs`):
    /// seconds of idleness after which a running env container is
    /// stopped. The WORKSPACE survives (it is a host directory); the
    /// next request starts the container again. 0 disables idle-stop.
    /// The env containers also inherit `image`, `memory_mb`,
    /// `network` and `capabilities` from this section.
    #[serde(default = "default_user_env_idle_stop_secs")]
    pub user_env_idle_stop_secs: u64,
    /// How many container `exec`s ONE sandbox runs at a time. 0 is
    /// unbounded (the historical behavior).
    ///
    /// This only became load-bearing when a sandbox stopped belonging
    /// to one principal: a channel box is SHARED, so everyone talking
    /// to the bot in `#chan` fires into one container, and podman will
    /// start as many `exec`s against it as it is asked to until the
    /// box's own memory cap kills whichever process is unlucky. The
    /// bound is per BOX, not per caller, because the resource being
    /// protected is the container. A call that cannot get a slot
    /// within half a minute is REFUSED and says why — a silent queue
    /// would be exactly the kind of invisible mechanism the transcript
    /// rules exist to prevent.
    #[serde(default = "default_sandbox_max_concurrent_exec")]
    pub max_concurrent_exec: u32,
    /// Where an ADMIN or OWNER principal's tool calls run when the call
    /// says nothing: `"sandbox"` (the default) or `"host"`.
    ///
    /// USER, 2026-09-09: *"Admins/owners should have an option to act
    /// outside of the sandbox, but use the sandbox by default."* This
    /// governs principals arriving over a connector; the local owner
    /// surfaces (desktop, TUI, CLI) carry no principal at all and stay
    /// on the host, because the human at that keyboard IS the machine's
    /// owner and a sandbox there would fence them out of their own
    /// laptop.
    #[serde(default = "default_admin_default_zone")]
    pub admin_default_zone: String,
    /// Where a call with NO principal runs when it says nothing: the
    /// desktop, the TUI, the REPL and the bare CLI, plus cron jobs and
    /// A2A peers. `"host"` (the default) or `"sandbox"`.
    ///
    /// USER, 2026-09-09, on inverting the flag: *"we should fail
    /// closed and default to sandbox"* — which this knob is the
    /// deliberate exception to. Reaching a local surface already means
    /// being at this machine's keyboard, and an agent that cannot
    /// touch the checkout it was asked to edit is not sandboxed, it is
    /// broken. Set `"sandbox"` to fence those too; every call then
    /// needs `host: true` to leave.
    #[serde(default = "default_local_default_zone")]
    pub local_default_zone: String,
    /// Named sandboxes declared in config (`[[sandbox.envs]]`). The
    /// implicit ones — one per channel, one per DM — need no
    /// declaration; this is for the ones an operator names, and for
    /// giving one a different image or memory budget.
    #[serde(default)]
    pub envs: Vec<SandboxDef>,
    /// Who lands in which sandbox (`[[sandbox.assign]]`). Resolved by
    /// the same specificity rules as the ACL, so the two read alike.
    #[serde(default)]
    pub assign: Vec<SandboxBind>,
}

/// A named sandbox.
///
/// The name is the identity: the container is `veles-user-env-<key>`
/// where the key hashes the NAME (plus the veles home and its
/// incarnation), so everyone routed to one name shares one container
/// and one workspace. `image`, `memory_mb` and `network` default to the
/// section's.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SandboxDef {
    pub name: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub memory_mb: Option<u64>,
    #[serde(default)]
    pub network: Option<bool>,
    /// Free-text note for the listing — why this box exists.
    #[serde(default)]
    pub note: Option<String>,
    /// Who created it, for a runtime definition. Config entries leave
    /// it empty.
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub created_at: Option<u64>,
}

/// "These callers, in this place, use that sandbox."
///
/// Every selector is optional and every present one must match. An
/// `identity` may be a hostmask pattern, an exact `platform:principal`,
/// or `user:<name>` for an `[[acl.users]]` group. `role` selects by
/// resolved role (`guest`, `user`, …) — the one selector the ACL's own
/// entries do not have, because an ACL entry GRANTS a role and cannot
/// also select on it.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SandboxBind {
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    pub sandbox: String,
}

/// One explicit bind mount into a container sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxMount {
    /// Absolute host path.
    pub host: String,
    /// Where it appears inside the container.
    pub container: String,
    /// `ro` or `rw` — nothing else is accepted.
    pub mode: String,
}

impl Default for SandboxSection {
    fn default() -> Self {
        Self {
            backend: default_sandbox_backend(),
            image: default_sandbox_image(),
            memory_mb: default_sandbox_memory_mb(),
            network: default_sandbox_network(),
            user_env_network: default_user_env_network(),
            capabilities: default_sandbox_capabilities(),
            kernel_path: None,
            mounts: Vec::new(),
            extra_args: Vec::new(),
            user_env_idle_stop_secs: default_user_env_idle_stop_secs(),
            max_concurrent_exec: default_sandbox_max_concurrent_exec(),
            admin_default_zone: default_admin_default_zone(),
            local_default_zone: default_local_default_zone(),
            envs: Vec::new(),
            assign: Vec::new(),
        }
    }
}

/// Four at once per box: enough that a shared channel box does not
/// feel serialized to the people using it, low enough that a burst
/// cannot flatten one container's memory cap.
fn default_sandbox_max_concurrent_exec() -> u32 {
    4
}

fn default_admin_default_zone() -> String {
    "sandbox".to_string()
}

fn default_local_default_zone() -> String {
    "host".to_string()
}

impl SandboxSection {
    /// Process-cached resolution of the live `[sandbox]` section.
    /// `VELES_SANDBOX_BACKEND` overrides only the backend name (tests
    /// and one-shot runs); everything else comes from config. A config
    /// that fails to load falls back to defaults — a broken config must
    /// not take the sandbox down, and native is the safe default.
    pub fn current() -> SandboxSection {
        static CACHED: std::sync::OnceLock<SandboxSection> = std::sync::OnceLock::new();
        CACHED
            .get_or_init(|| {
                let mut s = crate::config::load().map(|c| c.sandbox).unwrap_or_default();
                if let Ok(b) = std::env::var("VELES_SANDBOX_BACKEND") {
                    let b = b.trim().to_string();
                    if !b.is_empty() {
                        s.backend = b;
                    }
                }
                s
            })
            .clone()
    }
}

/// Skills excluded from the prompt index everywhere.
///
/// A per-session override (store meta, `skill:<id>:<name>`) beats this,
/// so "off by default, on for this one session" is expressible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsSection {
    #[serde(default)]
    pub disabled: Vec<String>,
    /// What the prompt's SKILLS block contains:
    ///   "full"     — every enabled skill (the historical behavior).
    ///   "recent"   — enabled skills touched in the last
    ///                `index_recent_sessions` sessions, capped at
    ///                `index_max_entries`, plus a pointer to skills_list.
    ///   "deferred" — the pointer line only.
    /// skills_list/skill_view are always resident, so nothing becomes
    /// unreachable — only what pays rent in every prompt changes.
    #[serde(default = "default_skills_index_mode")]
    pub index_mode: String,
    #[serde(default = "default_skills_index_max")]
    pub index_max_entries: usize,
    #[serde(default = "default_skills_index_recent")]
    pub index_recent_sessions: u32,
}

impl Default for SkillsSection {
    fn default() -> Self {
        Self {
            disabled: Vec::new(),
            index_mode: default_skills_index_mode(),
            index_max_entries: default_skills_index_max(),
            index_recent_sessions: default_skills_index_recent(),
        }
    }
}

fn default_skills_index_mode() -> String {
    "recent".into()
}
fn default_skills_index_max() -> usize {
    // Was 30 while `recent` had no signal: skill_uses recorded ~2.5% of
    // real loads, so the seats went alphabetical and 107 of 137 skills
    // were nameless in every prompt. With the recording fixed, recency
    // actually decides priority, and a description is ~200 chars — 60
    // entries is ~12k chars (~3k tokens) of index that now earns its
    // place. `skills_list` still reaches the rest on demand.
    60
}
fn default_skills_index_recent() -> u32 {
    20
}

/// Mechanisms whose transcript note is off BY DEFAULT.
///
/// `/mechanisms <name> off` silences one for a session; this is the
/// same choice made permanent. Without it the session toggle was the
/// only setting there was, and "always keep this quiet" meant saying so
/// again in every new session.
///
/// It governs the NOTE, not the behaviour — compaction still compacts
/// and guards still guard. What each mechanism DOES is set in the
/// section its catalogue entry names (`notes::catalogue`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MechanismsSection {
    #[serde(default)]
    pub silenced: Vec<String>,
    /// Per-connector overlay: `[mechanisms.platforms.irc]`.
    ///
    /// The global list above is one setting for every surface, which is
    /// the wrong shape for a bot that is BOTH a public channel member
    /// and the owner's desktop assistant: silencing guard notes so a
    /// channel of strangers is not narrated at also silences them where
    /// the owner actually reads them.
    ///
    /// Connector-agnostic by construction — keyed on the platform name
    /// the gateway already carries, not on anything IRC-shaped.
    #[serde(default)]
    pub platforms: std::collections::HashMap<String, PlatformNotes>,
}

/// How much one connector says about the engine's own machinery.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlatformNotes {
    /// `quiet` | `normal` | `loud`. The coarse knob people actually reach
    /// for; `silenced` below is the fine one.
    ///
    /// LEFT UNSET is not the same as `"normal"` on a connector, and that
    /// is deliberate: unset means "the shipped connector default", which
    /// silences the bookkeeping kinds (`CONNECTOR_DEFAULT_SILENCED`),
    /// while an explicit `"normal"` means somebody looked at it and wants
    /// every kind. That is the escape hatch, and it is one word.
    #[serde(default)]
    pub verbosity: Option<String>,
    /// Mechanisms silenced on THIS connector, on top of whatever the
    /// profile already silences.
    #[serde(default)]
    pub silenced: Vec<String>,
}

/// The surfaces whose transcript other people read.
///
/// Desktop, TUI and REPL are not in here on purpose: there the
/// transcript IS the product, and the standing "nothing implicit"
/// directive wants every mechanism shown. A channel is a shared room
/// where the same narration is noise to everybody but the operator.
pub const CONNECTORS: &[&str] = &["irc", "telegram"];

/// Is `platform` a connector — a chat surface somebody else reads?
pub fn is_connector(platform: &str) -> bool {
    let p = platform.trim().to_ascii_lowercase();
    CONNECTORS.iter().any(|c| *c == p)
}

/// The note kinds a connector silences when nobody set its `verbosity`.
///
/// USER (2026-09-10), watching `🧠 36k of context injected — …` land in
/// #flashsupport: "what is not fine is the guards still spewed into irc".
/// The per-connector knob shipped 2026-09-09 (`249df7e`) with `normal` as
/// the fallback for every platform, so a bot in a public channel narrated
/// its own bookkeeping until somebody wrote a config section — and the
/// deployed config had no `[mechanisms]` at all. The knob existed; the
/// default did not.
///
/// The split is by what a note is ABOUT, not by how long it is.
/// Bookkeeping the caller cannot act on is silenced; anything saying
/// WHAT THE CALLER GOT CHANGED stays, because a public channel is not a
/// debugging console but it is exactly where somebody learns the model
/// that answered them was not the model configured:
///
///   silenced — guard (⚠ harness caps), memory (🧠 context bill),
///              injection (⇒ what was fed in), cache (≡ prompt cache),
///              plugin (⚙ plugin/tool chatter), approval (⚖)
///   kept     — failover (⇄), aux_route (↗), compaction (⇲),
///              schedule (⏱), plugin_rewrite (✎)
///
/// `plugin_rewrite` stays with the kept half although its sibling is
/// silenced: ⚙ says a plugin ran, ✎ says a plugin CHANGED YOUR CONTENT,
/// and the second is the one a caller can be wronged by.
///
/// `approval` moved to the silenced half on 2026-10-13. One owner turn in
/// #flashsupport that poked at ~/.ssh, `sudo -n` and an `rm` put five
/// `⚖ allowed terminal without asking — …` lines in the channel. USER:
/// "the approvals themselves don't get suppressed but I never intended
/// them to be printed to the channel". Only the note is silenced: the
/// gate still gates, an `[approval needed]` ask is not a note and still
/// reaches whoever answers it, and the transcript keeps every ⚖ line.
pub const CONNECTOR_DEFAULT_SILENCED: &[&str] = &[
    "guard",
    "memory",
    "injection",
    "cache",
    "plugin",
    "approval",
];

/// The verbosity profiles.
///
/// It governs what a SURFACE RENDERS, never what the engine does: a
/// quiet channel still compacts, still guards, and the note is still in
/// the transcript for every other surface to show. Suppressing the
/// mechanism would be the thing `notes.rs` exists to prevent;
/// suppressing one connector's copy of the narration is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    /// No engine notes, no reasoning, no per-tool progress line. The
    /// public-channel posture.
    Quiet,
    /// What every surface did before this existed.
    Normal,
    /// Notes, reasoning and tool progress — the debugging posture.
    Loud,
}

impl Verbosity {
    pub fn parse(s: &str) -> Verbosity {
        match s.trim().to_ascii_lowercase().as_str() {
            "quiet" | "silent" | "off" => Verbosity::Quiet,
            "loud" | "verbose" | "debug" => Verbosity::Loud,
            "normal" | "default" | "" => Verbosity::Normal,
            other => {
                tracing::warn!("verbosity {other:?} is not quiet|normal|loud — using normal");
                Verbosity::Normal
            }
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Verbosity::Quiet => "quiet",
            Verbosity::Normal => "normal",
            Verbosity::Loud => "loud",
        }
    }
}

impl MechanismsSection {
    /// A section that quiets one connector — for tests and for the
    /// desktop's "public channel" preset.
    #[cfg(any(test, feature = "test-support"))]
    pub fn quiet_on(platform: &str) -> Self {
        let mut m = Self::default();
        m.platforms.insert(
            platform.to_string(),
            PlatformNotes {
                verbosity: Some("quiet".into()),
                silenced: vec![],
            },
        );
        m
    }
    /// Silenced everywhere (the global list).
    pub fn silences(&self, key: &str) -> bool {
        self.silenced.iter().any(|k| k == key)
    }

    /// The profile in force for a connector.
    pub fn verbosity(&self, platform: &str) -> Verbosity {
        self.platforms
            .get(platform)
            .and_then(|p| p.verbosity.as_deref())
            .map(Verbosity::parse)
            .unwrap_or(Verbosity::Normal)
    }

    /// Does the shipped connector default apply here?
    ///
    /// True for a connector whose `verbosity` nobody set — absent entry
    /// or present entry with no `verbosity` key. An explicit profile,
    /// INCLUDING `"normal"`, is an answer and displaces the default; so
    /// is `quiet`, which silences more than the default ever would.
    pub fn connector_default_applies(&self, platform: &str) -> bool {
        is_connector(platform)
            && self
                .platforms
                .get(platform)
                .and_then(|p| p.verbosity.as_deref())
                .is_none()
    }

    /// Silenced ON THIS CONNECTOR: the global list, plus the platform's
    /// own list, plus everything its profile silences, plus — on a
    /// connector whose verbosity is unset — the shipped default kinds.
    ///
    /// Additive on purpose. "Quiet except failover" is expressible as
    /// `verbosity = "normal"` with the rest listed in `silenced`; a
    /// third list for exceptions to the profile would be one concept
    /// too many for a knob whose whole point is being reachable.
    pub fn silences_on(&self, platform: &str, key: &str) -> bool {
        if self.silences(key) {
            return true;
        }
        if self.verbosity(platform) == Verbosity::Quiet {
            return true;
        }
        if self.connector_default_applies(platform) && CONNECTOR_DEFAULT_SILENCED.contains(&key) {
            return true;
        }
        self.platforms
            .get(platform)
            .is_some_and(|p| p.silenced.iter().any(|k| k == key))
    }
}

/// Hook handlers switched off individually.
///
/// A plugin may register several handlers, and "this one handler is
/// misbehaving" is a different problem from "switch the plugin off" —
/// which is why this exists alongside `[plugins] disabled`.
///
/// Entries are `"<hook> <handler-proc>"`, e.g.
/// `"post_llm_call ::reasoning_summary::handle"`. Neither half can
/// contain a space, so one is enough to separate them, and the row
/// stays readable when edited by hand.
///
/// The list is re-applied after every plugin (re)load — re-sourcing
/// resets the registry, and a switch that quietly came back on after a
/// file save would be worse than no switch.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HooksSection {
    #[serde(default)]
    pub disabled: Vec<String>,
}

impl HooksSection {
    /// Split an entry into (hook, handler). `None` for a malformed row,
    /// which is ignored rather than fatal — one typo in config must not
    /// stop the gateway loading its plugins.
    pub fn split_entry(entry: &str) -> Option<(&str, &str)> {
        let (hook, handler) = entry.trim().split_once(' ')?;
        let handler = handler.trim();
        (!hook.is_empty() && !handler.is_empty()).then_some((hook, handler))
    }

    pub fn entry(hook: &str, handler: &str) -> String {
        format!("{hook} {handler}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySection {
    #[serde(default = "default_memory_limit")]
    pub memory_char_limit: u64,
    #[serde(default = "default_user_limit")]
    pub user_char_limit: u64,
    /// Resident share of `system/core.md` injected into the prompt.
    /// Beyond this the prompt carries the head plus a pointer to read
    /// the rest with the `memory` tool — never a silent truncation.
    /// Distinct from `memory_char_limit` (the WRITE-time cap), which is
    /// how much the store may hold at all.
    #[serde(default = "default_core_resident")]
    pub core_char_limit: usize,
    /// Resident share of `system/user.md` (same contract as above).
    #[serde(default = "default_user_resident")]
    pub user_resident_char_limit: usize,
    /// Resident share of the project-context file (AGENTS.md /
    /// CLAUDE.md / .veles.md, first match wins). Same contract as the
    /// memory resident caps: beyond this the prompt carries the head
    /// plus a pointer naming the file — never a silent truncation.
    /// 0 = inject whole (the pre-cap behavior, one-line rollback).
    #[serde(default = "default_project_resident")]
    pub project_char_limit: usize,
    /// Resident share of the memory tree index injected into the
    /// prompt. A published hierarchy with many files (post-curate,
    /// for example) makes this derived index huge, and the point of
    /// the signpost is pointers, not payload. Beyond this the prompt
    /// carries the head plus a pointer to `memory search` — never a
    /// silent cut. 0 = inject whole.
    #[serde(default = "default_tree_resident")]
    pub tree_char_limit: usize,
    /// When the gateway should run `dream()` — the background
    /// reflection that consolidates recent sessions into
    /// memory `reference/lessons/`. None (the default) never runs it,
    /// which is why the lessons directory has historically not existed.
    /// Accepts anything `cron::normalize_schedule` does: `"30m"`,
    /// `"every 6h"`, a cron pattern like `"0 4 * * *"`, or an ISO
    /// timestamp for a one-shot. The scheduler claims each slot exactly
    /// once across processes, so running two gateways does not double
    /// the reflection.
    #[serde(default)]
    pub dream_schedule: Option<String>,
}
impl Default for MemorySection {
    fn default() -> Self {
        Self {
            memory_char_limit: default_memory_limit(),
            user_char_limit: default_user_limit(),
            core_char_limit: default_core_resident(),
            user_resident_char_limit: default_user_resident(),
            project_char_limit: default_project_resident(),
            tree_char_limit: default_tree_resident(),
            dream_schedule: None,
        }
    }
}
fn default_memory_limit() -> u64 {
    44_000
}
fn default_user_limit() -> u64 {
    25_050
}
fn default_core_resident() -> usize {
    6_000
}
fn default_user_resident() -> usize {
    3_000
}
fn default_project_resident() -> usize {
    12_000
}
fn default_tree_resident() -> usize {
    4_000
}

/// Prompt-cache accounting.
///
/// Most providers cache a repeated prefix on their own, keyed on the
/// request bytes, and need nothing on the wire; for them this section is
/// about OBSERVING the hit rate and warning when the prefix stops being
/// stable. The exceptions cache NOTHING without explicit breakpoints —
/// Anthropic's Messages API, and OpenRouter's Anthropic models — and the
/// default `auto` asks for them on exactly those routes
/// (`providers::resolve_cache_mode`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheSection {
    /// Master switch for the cache-accounting layer.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// auto | explicit | off. "auto" (default): breakpoints on the wires
    /// that need them, no cache fields anywhere else. "explicit": emit
    /// cache_control breakpoints on every route (normally set
    /// per-provider — see ProviderEntry.cache_mode). "off": never emit,
    /// for strict endpoints that 400 on unknown fields.
    #[serde(default = "default_cache_mode")]
    pub mode: String,
    /// Emit the Cache note (miss warnings) into the transcript.
    #[serde(default = "default_true")]
    pub report: bool,
    /// The note fires when a round's hit rate drops below this AFTER a
    /// round that was above it.
    #[serde(default = "default_min_hit_rate_warn")]
    pub min_hit_rate_warn: f64,
}

impl Default for CacheSection {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: default_cache_mode(),
            report: true,
            min_hit_rate_warn: default_min_hit_rate_warn(),
        }
    }
}

fn default_cache_mode() -> String {
    "auto".into()
}
fn default_min_hit_rate_warn() -> f64 {
    0.5
}

/// Tool residency: which builtins are always in the prompt (core) and
/// which are deferred — discoverable via tool_search/tool_describe and
/// invocable via tool_call, but not burning their schema into every
/// turn. `tier = "full"` restores the historical everything-resident
/// behavior exactly (the one-line rollback).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsSection {
    /// "core" (default) | "full".
    #[serde(default = "default_tool_tier")]
    pub tier: String,
    /// Per-tool overrides: `web_search = "core"` promotes one tool,
    /// `terminal = "deferred"` demotes it. Wins over `tier`.
    #[serde(default)]
    pub overrides: std::collections::HashMap<String, String>,
    /// Re-homed tools (PLAN-pool-round.md D1): a tool glob → the node
    /// that runs it, by name or id — `"web_*" = "laptop"` sends every web
    /// call to the laptop and its residential IP. `"local"` (or this
    /// node's own name) keeps a tool here. The most specific glob wins;
    /// a session's `/tools home` rules win over these. See remotetool.rs
    /// for what the other node runs without a ticket.
    ///
    /// A list is an order: `"web_*" = ["laptop", "vps"]` runs on the
    /// laptop, and on the vps while the laptop cannot be reached (down,
    /// asleep, its heartbeat stale). Only an UNREACHABLE node is passed
    /// over — one that answered and refused is the answer, so a call
    /// never shops for a node that allows it. This node in the list is
    /// "here, at that position". One node (a string) never falls back.
    #[serde(default)]
    pub home: std::collections::BTreeMap<String, HomeSpec>,
    /// Automatic homes (PLAN-phone-userland.md T2): a call to a tool
    /// whose program this node lacks (`remotetool::NEEDS` — rg for
    /// search_files, agent-browser for the browser, a container runtime
    /// for a sandboxed call) goes to a live `[cluster] task_peers` node
    /// that advertises one, least loaded first, and says so in the
    /// result. A `home` rule (or a session's `/tools home`) for the tool
    /// wins, `"local"` included. `false` runs every call here.
    #[serde(default = "default_true")]
    pub auto_home: bool,
}

/// One `[tools.home]` value: a node, or nodes in order of preference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum HomeSpec {
    One(String),
    Order(Vec<String>),
}

impl HomeSpec {
    /// The nodes, in order, trimmed, without blanks.
    pub fn nodes(&self) -> Vec<String> {
        let all: Vec<&String> = match self {
            HomeSpec::One(n) => vec![n],
            HomeSpec::Order(v) => v.iter().collect(),
        };
        all.into_iter()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect()
    }
}

impl From<&str> for HomeSpec {
    fn from(n: &str) -> Self {
        HomeSpec::One(n.to_string())
    }
}

impl std::fmt::Display for HomeSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.nodes().join(", then "))
    }
}

impl Default for ToolsSection {
    fn default() -> Self {
        Self {
            tier: default_tool_tier(),
            overrides: std::collections::HashMap::new(),
            home: std::collections::BTreeMap::new(),
            auto_home: true,
        }
    }
}

fn default_tool_tier() -> String {
    "core".into()
}

/// Approvals (M11). Defaults: screen risky calls with one small aux
/// model call, ask the human only when that screen is unsure. hermes'
/// unconditional hardline blocklist is deliberately NOT ported (USER).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalsSection {
    /// "smart" (default) | "manual" | "off".
    ///
    /// Deserialized leniently: YAML parses a bare `off` as boolean
    /// false, and the laptop's hermes config really does carry
    /// `approvals: mode: 'off'`. A strict String field would fail the
    /// WHOLE config load on the unquoted form.
    #[serde(default = "default_approval_mode", deserialize_with = "de_mode")]
    pub mode: String,
    /// What happens when nobody can be asked (cron jobs, A2A peers,
    /// `chat -q`, delegated children): "deny" (default) | "allow".
    #[serde(default = "default_non_interactive")]
    pub non_interactive: String,
    /// How long a question waits for a human before it counts as denied.
    #[serde(default = "default_approval_timeout")]
    pub timeout_secs: u64,
    /// Never ask about these: shell-style globs matched against the
    /// normalized command/path, or exact `tool:rule` keys.
    #[serde(default)]
    pub allowlist: Vec<String>,
    /// Which provider screens a risky call in `smart` mode.
    ///
    /// The screen is a small aux call and was going to the PRIMARY
    /// brain — `auxroute.rs` exists for exactly this consumer and the
    /// screen was never wired to it. Unset: the failover rule's
    /// `aux_fallback` decides, else the primary, as before. Set: this
    /// provider, with the ordinary chain behind it so a screen whose
    /// aux provider is unreachable still answers instead of falling
    /// through to a human.
    #[serde(default)]
    pub screen_provider: Option<String>,
    /// Model for the screen. Only meaningful with `screen_provider`.
    #[serde(default)]
    pub screen_model: Option<String>,
}

fn default_approval_mode() -> String {
    "smart".into()
}

fn de_mode<'de, D>(d: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_yaml::Value::deserialize(d).map_err(serde::de::Error::custom)?;
    Ok(match v {
        serde_yaml::Value::Bool(false) => "off".into(),
        serde_yaml::Value::Bool(true) => "manual".into(),
        serde_yaml::Value::String(s) => s,
        other => {
            tracing::warn!("approvals.mode: unexpected {other:?}, using default");
            default_approval_mode()
        }
    })
}
fn default_non_interactive() -> String {
    "deny".into()
}
fn default_approval_timeout() -> u64 {
    300
}

impl Default for ApprovalsSection {
    fn default() -> Self {
        Self {
            mode: default_approval_mode(),
            non_interactive: default_non_interactive(),
            timeout_secs: default_approval_timeout(),
            allowlist: Vec::new(),
            screen_provider: None,
            screen_model: None,
        }
    }
}

/// Checkpoints (M13): pre-mutation file snapshots the owner can restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointsSection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Skip snapshots of files larger than this (MB); a 2 GB blob is
    /// not worth keeping to undo one edit.
    #[serde(default = "default_checkpoint_max_file_mb")]
    pub max_file_mb: u64,
    /// Total blob budget; the oldest are pruned past it.
    #[serde(default = "default_checkpoint_max_total_mb")]
    pub max_total_mb: u64,
    #[serde(default = "default_checkpoint_retention_days")]
    pub retention_days: u32,
}

fn default_checkpoint_max_file_mb() -> u64 {
    8
}
fn default_checkpoint_max_total_mb() -> u64 {
    256
}
fn default_checkpoint_retention_days() -> u32 {
    14
}

impl Default for CheckpointsSection {
    fn default() -> Self {
        Self {
            enabled: true,
            max_file_mb: default_checkpoint_max_file_mb(),
            max_total_mb: default_checkpoint_max_total_mb(),
            retention_days: default_checkpoint_retention_days(),
        }
    }
}

/// The desktop API's port: `VELES_DESKTOP_PORT`, else
/// `[platforms.desktop.extra] port`, else 8642. In core rather than the
/// desktop server because delegated children dial it back
/// (`delegate.rs`); `veles_frontends::desktop_api::default_port` is this.
pub fn desktop_port(cfg: &VelesConfig) -> u16 {
    // .env-aware env read, matching A2A_PORT's chain (a2a_server.rs).
    if let Some(p) = crate::providers::resolve_env_or_dotenv("VELES_DESKTOP_PORT", &config_dir())
        .filter(|s| !s.is_empty())
    {
        if let Ok(n) = p.parse::<u16>() {
            return n;
        }
    }
    cfg.platforms
        .get("desktop")
        .and_then(|d| d.get("extra"))
        .and_then(|e| e.get("port"))
        .and_then(|p| p.as_u64())
        .unwrap_or_else(|| u64::from(crate::profiles::default_desktop_port())) as u16
}

#[cfg(test)]
mod approval_config_tests {
    use super::*;

    #[test]
    fn yaml_off_in_either_form_disables_approvals() {
        // The laptop's hermes config has the quoted form; a user typing
        // the bare one must not break the entire config load.
        let quoted: VelesConfig = serde_yaml::from_str("approvals:\n  mode: 'off'\n").unwrap();
        assert_eq!(quoted.approvals.mode, "off");
        let bare: VelesConfig = serde_yaml::from_str("approvals:\n  mode: off\n").unwrap();
        assert_eq!(bare.approvals.mode, "off");
        let smart: VelesConfig = serde_yaml::from_str("approvals:\n  mode: smart\n").unwrap();
        assert_eq!(smart.approvals.mode, "smart");
        // Absent section → the veles default, not hermes' posture.
        let none: VelesConfig = serde_yaml::from_str("model:\n  provider: x\n").unwrap();
        assert_eq!(none.approvals.mode, "smart");
        assert_eq!(none.approvals.non_interactive, "deny");
    }
}

#[cfg(test)]
mod discover_config_tests {
    use super::*;

    /// `discover` is opt-in per provider, in TOML (the native format)
    /// and YAML (the hermes import path) alike.
    #[test]
    fn discover_round_trips_in_both_config_formats() {
        let toml_cfg: VelesConfig = toml::from_str(
            "[[custom_providers]]\nname = \"router\"\nbase_url = \"https://r/v1\"\ndiscover = true\n",
        )
        .unwrap();
        assert_eq!(toml_cfg.custom_providers[0].discover, Some(true));

        let yaml_cfg: VelesConfig =
            serde_yaml::from_str("custom_providers:\n  - name: router\n    discover: false\n")
                .unwrap();
        assert_eq!(yaml_cfg.custom_providers[0].discover, Some(false));
    }

    /// An entry that never mentions `discover` leaves it absent, which
    /// is what makes "absent AND empty models ⇒ discover everything"
    /// distinguishable from an explicit opt-out.
    #[test]
    fn an_unmentioned_discover_stays_none_not_false() {
        let cfg: VelesConfig = toml::from_str("[[custom_providers]]\nname = \"plain\"\n").unwrap();
        assert_eq!(
            cfg.custom_providers[0].discover, None,
            "None and Some(false) mean different things to discovery::wanted"
        );
        assert!(cfg.custom_providers[0].models.is_empty());
    }

    /// Every provider in the real config on this machine enumerates by
    /// hand, so adding `discover` must not change what any of them does
    /// until someone opts in.
    #[test]
    fn adding_the_field_changes_nothing_for_a_curated_provider() {
        let cfg: VelesConfig =
            toml::from_str("[[custom_providers]]\nname = \"curated\"\nmodels = [\"a\", \"b\"]\n")
                .unwrap();
        assert!(!crate::discovery::wanted(&cfg.custom_providers[0]));
    }

    /// `[model] api_mode` is the wire default for providers that do
    /// not name their own — before this, the field parsed, validated,
    /// and did nothing.
    #[test]
    fn model_api_mode_fills_unset_providers_only() {
        let mut cfg: VelesConfig = toml::from_str(
            "[model]\napi_mode = \"responses\"\n\
             [[custom_providers]]\nname = \"inherit\"\nbase_url = \"https://a.example\"\n\
             [[custom_providers]]\nname = \"explicit\"\nbase_url = \"https://b.example\"\napi_mode = \"chat_completions\"\n",
        )
        .unwrap();
        apply_model_api_mode_default(&mut cfg);
        assert_eq!(cfg.custom_providers[0].api_mode, "responses");
        assert_eq!(
            cfg.custom_providers[1].api_mode, "chat_completions",
            "an explicit api_mode always wins"
        );
    }

    #[test]
    fn no_model_api_mode_leaves_everything_alone() {
        let mut cfg: VelesConfig = toml::from_str(
            "[[custom_providers]]\nname = \"p\"\nbase_url = \"https://a.example\"\n",
        )
        .unwrap();
        apply_model_api_mode_default(&mut cfg);
        assert!(cfg.custom_providers[0].api_mode.is_empty());
    }
}

fn default_true() -> bool {
    true
}

// `splice_failover` lived here: a line walker that deleted the
// fallback_providers array and every [[failover]] block, then re-inserted
// the new text where the first of them had been. It was the only config
// WRITER in the tree, and it only understood those two keys.
//
// It is now `configedit::replace_sections`, which does the same job
// through toml_edit for any key — and, being TOML-aware rather than
// line-aware, keeps a replaced key in its original position and no
// longer relocates one that was nested under another table. Its three
// invariants moved with it (see configedit::tests).

/// `<name>` beside the running binary or up to three levels above it:
/// an installed layout puts `skills/` and `veles-desktop/` next to
/// `bin/veles` (one up), a cargo build has them at the repo root above
/// `target/debug/veles` (two up; a test binary in `target/debug/deps`
/// is three). None when no such directory exists — a bundled thing is
/// optional everywhere it is looked for.
pub fn beside_binary(name: &str) -> Option<PathBuf> {
    let mut dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf));
    for _ in 0..4 {
        let d = dir?;
        let cand = d.join(name);
        if cand.is_dir() {
            return Some(cand);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

/// Resolve the veles home directory — veles' OWN state root.
///
/// NEVER resolves to ~/.hermes. Incident 2026-08-09 (root-caused by
/// Demon): veles ran Store::open against Hermes' live state.db and
/// recreated a `messages_ai` trigger that Hermes' schema doesn't have,
/// poisoning every session write. The migration-friendly "reuse
/// ~/.hermes" default is retired — config is READ from there when
/// present (see `hermes_config_yaml`), but state/plugins/db live here.
/// The user's home directory (`HOME`, or `USERPROFILE` on Windows), or
/// `.` when even that cannot be resolved.
pub fn user_home() -> PathBuf {
    directories::UserDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `VELES_HOME`, when set. It forces the single-directory
/// layout: config, state and cache all under one root.
///
/// This is not a leftover. The OCI image sets `VELES_HOME=/opt/data`
/// against a bind-mounted volume, and it wants exactly one directory.
/// XDG splitting is the behaviour for an ordinary user install; an
/// explicitly pinned home stays one place.
///
/// Tests no longer set this to point at a scratch directory — see
/// [`DirOverride`]. Reading it stays correct for production, where the
/// process is the scope the variable was always meant to have.
fn forced_home() -> Option<PathBuf> {
    std::env::var("VELES_HOME")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

// ── scoped directory override ───────────────────────────────────────
//
// `VELES_HOME` is process-global, and cargo runs tests in parallel
// threads. Every fixture that "points the config at a scratch dir"
// mutated the SAME variable under a shared lock, so any read that
// happened off the fixture's thread — or any test that forgot the
// lock, or any `config_dir()` call buried four layers under an agent
// turn — saw whichever scratch dir was ambient at that instant. The
// lock serialized the well-behaved tests against each other and did
// nothing at all for the reads that mattered: a gateway mid-turn
// calling `config::live()` is not holding anybody's test lock.
//
// The override below is a thread-local, so the scope is the test
// instead of the process: `config_dir()` on the test's thread answers
// from the override no matter what any other thread has done to the
// environment. Precedence is override → `VELES_HOME` → XDG; production
// never installs one, so nothing outside tests and embedders changes.
//
// Code that runs OFF the installing thread (`spawn_blocking` hooks,
// pipe-reader threads) cannot see a thread-local. `testenv` still
// publishes `VELES_HOME` for those, under the env lock — a race by
// nature, which is exactly why the deterministic path is this one and
// the env var is the compatibility net.

/// A thread-scoped replacement for the directory-resolving env vars.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirOverride {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
}

impl DirOverride {
    /// The single-directory layout `VELES_HOME` forces: config and data
    /// share a root, cache is `<root>/cache`.
    pub fn forced(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let cache = home.join("cache");
        Self {
            config: home.clone(),
            data: home,
            cache,
        }
    }
}

thread_local! {
    static DIRS: std::cell::RefCell<Option<DirOverride>> = const { std::cell::RefCell::new(None) };
}

/// The override installed on THIS thread, if any. Capture it to carry
/// into a thread you are about to spawn (see [`set_dirs`]).
pub fn dirs() -> Option<DirOverride> {
    DIRS.with(|d| d.borrow().clone())
}

/// A restored previous override. Dropping it puts back whatever was
/// installed before the matching [`set_dirs`], so scopes nest.
pub struct DirGuard(Option<DirOverride>);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let prev = self.0.take();
        DIRS.with(|d| *d.borrow_mut() = prev);
    }
}

/// Install (or with `None`, clear) the directory override for THIS
/// thread. The guard restores the previous value on drop — bind it,
/// never `let _ = …`.
pub fn set_dirs(o: Option<DirOverride>) -> DirGuard {
    let prev = DIRS.with(|d| std::mem::replace(&mut *d.borrow_mut(), o));
    DirGuard(prev)
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    if let Some(explicit) = std::env::var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && std::path::Path::new(v).is_absolute())
    {
        return PathBuf::from(explicit).join("veles");
    }
    if cfg!(windows) {
        if let Some(p) = windows_root(var) {
            return p;
        }
    }
    user_home().join(fallback).join("veles")
}

/// The Windows homes of the three roots when no XDG variable names
/// one: config in `%APPDATA%\veles` (roams with the profile), state
/// in `%LOCALAPPDATA%\veles` (a database does not roam), cache under
/// it. `None` when the variable is unset (a service account), and the
/// XDG spelling under the home applies, as on any other platform.
fn windows_root(var: &str) -> Option<PathBuf> {
    let env = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty() && std::path::Path::new(v).is_absolute())
            .map(PathBuf::from)
    };
    match var {
        "XDG_CONFIG_HOME" => env("APPDATA").map(|p| p.join("veles")),
        "XDG_DATA_HOME" => env("LOCALAPPDATA").map(|p| p.join("veles")),
        "XDG_CACHE_HOME" => env("LOCALAPPDATA").map(|p| p.join("veles").join("cache")),
        _ => None,
    }
}

/// Where CONFIGURATION lives: `config.toml`, `.env`, `secrets.env`,
/// `themes/`, `commands/`, `plugins/`, `skills/`, `failover-scripts/`.
///
/// `$XDG_CONFIG_HOME/veles`, else `~/.config/veles`. USER, 2026-09-04:
/// *"config should be migrated to ~/.config, state in ~/.local/lib or
/// ~/.local/share"*. `VELES_HOME` overrides both halves — see
/// `forced_home` — and a thread-scoped [`DirOverride`] overrides all
/// three (precedence: override → env → XDG).
/// The desktop app's saved-instance registry, `<config base>/veles-desktop/
/// instances.json` (`::vinst::path` in veles-desktop/lib/instances.tcl):
/// the config BASE — `$XDG_CONFIG_HOME`, `%APPDATA%` on Windows, else
/// `~/.config` — not the veles config dir, and never `VELES_HOME`, which
/// is the engine's and not the window's. `veles gui` reads it to tell a
/// first run from one that has chosen where to attach.
pub fn desktop_registry_path() -> PathBuf {
    if crate::profiles::selected_profile().is_ok_and(|p| p != "default") {
        return data_dir().join("instances.json");
    }
    let veles = xdg("XDG_CONFIG_HOME", ".config");
    let base = veles.parent().map(Path::to_path_buf).unwrap_or(veles);
    base.join("veles-desktop").join("instances.json")
}

/// The XDG config base used by shared user services, without appending
/// veles' own directory name or applying a named installation profile.
pub fn config_base() -> PathBuf {
    let base = unscoped_config_root();
    base.parent().map(Path::to_path_buf).unwrap_or(base)
}

pub fn config_dir() -> PathBuf {
    if let Some(o) = dirs() {
        return o.config;
    }
    if crate::profiles::selected_profile().is_ok_and(|p| p != "default") {
        // One profile root, not two unrelated XDG subtrees: everything
        // the profile owns lives beside its state under DATA/profiles.
        return data_dir();
    }
    unscoped_config_root()
}

/// The ordinary config root BEFORE a named profile is applied. Test
/// overrides and a forced home deliberately bypass XDG exactly as before.
pub(crate) fn unscoped_config_root() -> PathBuf {
    forced_home().unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config"))
}

/// Where STATE lives: `state.db` (sessions AND memory), `checkpoints/`,
/// `media/`, `certs/`, `backups/`, `logs/`, `sessions`, `profiles/`,
/// the gateway pid file and its logs.
///
/// `$XDG_DATA_HOME/veles`, else `~/.local/share/veles`.
pub fn data_dir() -> PathBuf {
    if let Some(o) = dirs() {
        return o.data;
    }
    scoped_home(unscoped_data_root())
}

/// The original state root: `$VELES_HOME`, or `$XDG_DATA_HOME/veles`.
/// Profile selection enumerates this root; the scoped directory must not
/// recurse into `profiles/<name>/profiles`.
pub(crate) fn unscoped_data_root() -> PathBuf {
    forced_home().unwrap_or_else(|| xdg("XDG_DATA_HOME", ".local/share"))
}

fn scoped_home(base: PathBuf) -> PathBuf {
    match crate::profiles::selected_profile() {
        Ok(name) if name != "default" => base.join("profiles").join(name),
        _ => base,
    }
}

/// Regenerable working files. `$XDG_CACHE_HOME/veles`, else
/// `~/.cache/veles`; under a forced home it is `<home>/cache`, which is
/// where it already was.
pub fn cache_dir() -> PathBuf {
    if let Some(o) = dirs() {
        return o.cache;
    }
    match forced_home() {
        Some(h) => scoped_home(h).join("cache"),
        None if crate::profiles::selected_profile().is_ok_and(|p| p == "default") => {
            xdg("XDG_CACHE_HOME", ".cache")
        }
        None => data_dir().join("cache"),
    }
}

/// The STATE root. Historically the one and only veles directory, which
/// is why 90-odd call sites reach for it; since the 2026-09-04 XDG
/// split it is `data_dir()`, and the config-side callers were moved to
/// `config_dir()` individually.
///
/// Kept (rather than renamed across the tree) because a mechanical
/// rename of every site would have hidden which of the three
/// directories each one actually meant — the interesting part.
pub fn veles_home() -> PathBuf {
    data_dir()
}

/// The MEDIA root: `<data_dir>/media`. Everything the engine produces for
/// a human to look at lands here — generated images under `generated/`,
/// TTS audio under `audio/`, files a connector downloaded from a chat
/// platform at the top level.
///
/// It is also the delivery contract: a `MEDIA:` marker naming a path
/// outside this root (and outside the sender's own sandbox workspace) is
/// withheld from every below-owner principal, because the marker is the one
/// file channel that never had a principal on it. See
/// [`crate::media::deliverable`].
pub fn media_dir() -> PathBuf {
    data_dir().join("media")
}

/// Path to hermes' config.yaml, if an install exists (read-only compat).
///
/// Under a [`DirOverride`] this is ALWAYS `None`. The override says
/// "these scratch directories are the whole world for this thread", and
/// `~/.hermes` is the developer's real install: on a box that has one,
/// `load()`'s fallback and `config_file_present()` would otherwise read
/// the live hermes config into a test that believed it was hermetic.
/// That is not theoretical — it is how a forked agent in three gateway
/// tests came to be built out of the ambient config.
///
/// A forced `VELES_HOME` gets the SAME treatment, for the same reason
/// and with a real incident behind it (2026-10-02, the zai-MCP leak).
/// `VELES_HOME` forces the single-directory layout — "config, state and
/// cache all under one root" — so it, too, says the ambient `~/.hermes`
/// is NOT part of this process's world. The `DirOverride` is a
/// thread-local a CHILD PROCESS cannot inherit, so a forked `veles chat`
/// run with a scratch `VELES_HOME` and no config.toml fell straight
/// through to the developer's live `~/.hermes/config.yaml`. veles' MCP
/// parser ignored hermes' `enabled: false`, and every such child
/// spawned `npx -y @z_ai/mcp-server@latest` — ten times in one day.
/// Reading `VELES_HOME` here closes the hole the thread-local cannot
/// reach across a `fork`.
pub fn hermes_config_yaml() -> Option<PathBuf> {
    if dirs().is_some()
        || forced_home().is_some()
        || crate::profiles::selected_profile().is_ok_and(|p| p != "default")
    {
        return None;
    }
    let home = directories::UserDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let p = home.join(".hermes").join("config.yaml");
    p.exists().then_some(p)
}

/// Names this process copied out of `.env` into its own environment,
/// with the value copied. `secrets::resolve` needs to tell those apart
/// from the operator's own environment: a real export (systemd's
/// `EnvironmentFile`, the shell) outranks the vault, a copy of a `.env`
/// line does not. A test or a caller that later sets a different value
/// makes it the operator's again, which is why the value is kept.
///
/// The FILE is kept too: a process can load more than one home (tests
/// do, all the time), and a copy of one home's `.env` line is no answer
/// for another's.
type EnvCopies = std::collections::HashMap<String, (String, PathBuf)>;
static FROM_ENV_FILE: std::sync::Mutex<Option<EnvCopies>> = std::sync::Mutex::new(None);

/// Did `load_env_file` put `name=value` in this process's environment?
pub fn env_copied_from_file(name: &str, value: &str) -> bool {
    env_copy_source(name, value).is_some()
}

/// The `.env` file `load_env_file` copied `name=value` from, if it did.
pub fn env_copy_source(name: &str, value: &str) -> Option<PathBuf> {
    FROM_ENV_FILE
        .lock()
        .ok()
        .and_then(|m| m.as_ref().and_then(|m| m.get(name).cloned()))
        .filter(|(v, _)| v == value)
        .map(|(_, p)| p)
}

/// Parse a .env file (KEY=VALUE per line, # comments, optional quotes)
/// and populate std::env. Does NOT overwrite already-set vars (the
/// shell/systemd wins). Fails silently if the file doesn't exist.
fn load_env_file(path: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let mut val = val.trim();
        // strip surrounding quotes
        if (val.starts_with('"') && val.ends_with('"'))
            || (val.starts_with('\'') && val.ends_with('\''))
        {
            val = &val[1..val.len() - 1];
        }
        if std::env::var(key).is_err() {
            std::env::set_var(key, val);
            if let Ok(mut m) = FROM_ENV_FILE.lock() {
                m.get_or_insert_with(std::collections::HashMap::new)
                    .insert(key.to_string(), (val.to_string(), path.to_path_buf()));
            }
        }
    }
}

// ── the live config ──────────────────────────────────────────────────
//
// Several subsystems have to answer "what does the config say RIGHT
// NOW" on a hot path — the per-message ACL gate, sandbox assignment —
// and the honest answer is the file, not a snapshot taken at boot.
// Doing that with a bare `load()` per message would re-parse the file
// (and re-run the .env load) for every line in a busy channel, so it
// is cached on what `stat` can honestly offer: path, mtime and length.
// Length alone would miss an in-place same-size edit; mtime alone can
// be coarse; together they are what an editor's write changes.

type LiveKey = (PathBuf, Option<std::time::SystemTime>, u64);

/// One path's live-config state.
///
/// `key` is `None` when the entry is stale (invalidated, or never
/// stat-matched) and a re-read is owed. `cfg` is the LAST config that
/// parsed for this path — it survives both a fresh read and an
/// invalidation, because its whole job is to be the answer while the
/// file on disk is broken. A path that has never parsed has no entry
/// yet, and the error path inserts one holding a default config, which
/// is the honest answer when there is no last-good to keep.
struct LiveEntry {
    key: Option<LiveKey>,
    cfg: Arc<VelesConfig>,
}

/// Keyed BY PATH, and that is load-bearing rather than tidy.
///
/// A single-slot cache made the error path a race: `live()` answers a
/// parse failure with "the last good config", and with one slot the
/// last good config is whichever path loaded most recently — under a
/// parallel test run, some other test's scratch home. A test that
/// broke its own config file on purpose was then answered with a
/// stranger's config, or with a default, depending on scheduling.
/// Keying by path means "last good" always means last good FOR THE
/// FILE BEING READ.
///
/// In production this map holds one entry: there is one config path
/// per process. It grows only where `config_dir()` itself moves —
/// i.e. under test, one entry per scratch home, bounded by the suite.
type LiveCell = std::sync::RwLock<std::collections::HashMap<PathBuf, LiveEntry>>;

static LIVE: std::sync::OnceLock<LiveCell> = std::sync::OnceLock::new();

fn live_cell() -> &'static LiveCell {
    LIVE.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}

fn live_key() -> LiveKey {
    let path = config_dir().join("config.toml");
    match std::fs::metadata(&path) {
        Ok(m) => (path, m.modified().ok(), m.len()),
        Err(_) => (path, None, 0),
    }
}

/// Is there a config FILE at all?
///
/// When there is not — a first run, a test with a scratch home, an
/// embedder that built its `VelesConfig` in memory — "the live config"
/// is a default config, and answering with it would silently discard
/// whatever the caller was constructed with.
pub fn config_file_present() -> bool {
    config_dir().join("config.toml").exists() || hermes_config_yaml().is_some()
}

/// The config as it is on disk right now, re-parsed only when the file
/// has actually changed.
///
/// A file that will not parse keeps the LAST GOOD config for that path
/// rather than becoming a default one. `unwrap_or_default()` here
/// looked harmless and was not: it cached an empty config under the
/// broken file's key, so a half-written `config.toml` — an editor's
/// write is not atomic, and `configedit` writes then validates — left
/// every live reader seeing no providers, no ACL and no routes until
/// the file changed again. For the ACL that fails closed and merely
/// refuses everything; for `build_routes` it is "no model.provider in
/// config", which is a session that cannot be created at all.
pub fn live() -> Arc<VelesConfig> {
    let key = live_key();
    let path = key.0.clone();
    {
        let g = live_cell().read().expect("live config lock");
        if let Some(e) = g.get(&path) {
            if e.key.as_ref() == Some(&key) {
                return e.cfg.clone();
            }
        }
    }
    let parsed = load();
    let mut cell = live_cell().write().expect("live config lock");
    match parsed {
        Ok(cfg) => {
            let fresh = Arc::new(cfg);
            cell.insert(
                path,
                LiveEntry {
                    key: Some(key),
                    cfg: fresh.clone(),
                },
            );
            fresh
        }
        Err(e) => {
            // Keep serving what worked. The broken key is CACHED under
            // the last-good config, so the same half-written file is
            // parsed (and this logged) once per version of it rather
            // than per message — and the moment the file changes again,
            // the key stops matching and the read retries for real.
            tracing::error!("config.toml does not parse — keeping the last good config: {e:#}");
            let entry = cell.entry(path).or_insert_with(|| LiveEntry {
                key: None,
                cfg: Arc::new(VelesConfig::default()),
            });
            entry.key = Some(key);
            entry.cfg.clone()
        }
    }
}

/// Mark every cached path stale, forcing a re-read on the next
/// [`live`]. For tests that write a config file and immediately read
/// it back — two writes inside one filesystem timestamp tick are
/// indistinguishable to `stat` when the length matches.
///
/// This drops the FRESHNESS key, never the last-good config: an
/// invalidation is "go look again", not "forget what worked", and
/// forgetting is exactly what turns a half-written file into a
/// default config.
pub fn invalidate_live() {
    let mut cell = live_cell().write().expect("live config lock");
    for e in cell.values_mut() {
        e.key = None;
    }
}

pub fn load() -> Result<VelesConfig> {
    let home = config_dir();
    // Load the .env into the process env FIRST so config fields that
    // reference env vars (API keys in provider URLs, bot tokens, etc.)
    // resolve. systemd's EnvironmentFile does this for managed runs;
    // this covers bare `veles gateway run` / `veles chat`.
    load_env_file(&home.join(".env"));
    let native = home.join("config.toml");
    if native.exists() {
        let raw = std::fs::read_to_string(&native)?;
        let mut cfg: VelesConfig = toml::from_str(&raw)?;
        finish_loaded(&mut cfg)?;
        return Ok(cfg);
    }
    if let Some(hermes) = hermes_config_yaml() {
        let raw = std::fs::read_to_string(hermes)?;
        let mut cfg = from_hermes_yaml(&raw)?;
        finish_loaded(&mut cfg)?;
        return Ok(cfg);
    }
    Ok(VelesConfig::default())
}

/// What every load does after parsing, in THE order: the node's
/// overrides first, then the defaults derived from them, then
/// validation. `model.api_mode` is overridable AND is what
/// `apply_model_api_mode_default` copies into every custom provider
/// that left its own empty — applied the other way round (as b8cc067
/// did), a node overriding `model.api_mode` kept the SHARED wire for
/// those providers.
pub(crate) fn finish_loaded(cfg: &mut VelesConfig) -> Result<()> {
    apply_node_overrides(cfg);
    apply_model_api_mode_default(cfg);
    validate_api_modes(cfg)?;
    validate_auth_styles(cfg)?;
    validate_cache_ttls(cfg)
}

/// The cache lifetimes a driver can be asked for (Anthropic's two).
pub const SUPPORTED_CACHE_TTLS: &[&str] = &["5m", "1h"];

/// Refuse a `cache_ttl` the wire does not have, by name, rather than let a
/// typo silently become the driver's default.
fn validate_cache_ttls(cfg: &VelesConfig) -> Result<()> {
    for p in &cfg.custom_providers {
        let Some(ttl) = p
            .cache_ttl
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if !SUPPORTED_CACHE_TTLS.contains(&ttl) {
            anyhow::bail!(
                "provider \"{}\": cache_ttl = \"{ttl}\" is not one this build can ask for — use \"5m\" or \"1h\" (or remove the line for the driver's default)",
                p.name
            );
        }
    }
    Ok(())
}

/// The credential styles a driver can be asked to send.
///
/// Two, because there are only two ways to carry a key on this wire and
/// every Anthropic-compatible endpoint in the wild uses one of them.
pub const SUPPORTED_AUTH_STYLES: &[&str] = &["x-api-key", "bearer"];

/// Refuse an `auth_style` no driver can honour, by name.
///
/// Same reasoning as [`validate_api_modes`], and the same failure shape:
/// an unknown value silently falling back to the default would
/// authenticate with the WRONG header. That does not read as a config
/// typo — it reads as a rejected API key, which sends the operator
/// rotating credentials instead of fixing one word.
fn validate_auth_styles(cfg: &VelesConfig) -> Result<()> {
    for p in &cfg.custom_providers {
        let Some(style) = p
            .auth_style
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue; // absent = the driver's own default
        };
        if !SUPPORTED_AUTH_STYLES.contains(&style) {
            anyhow::bail!(
                "provider \"{}\": auth_style = \"{style}\" is not one this build can send — \
                 use {} (or remove the line and let the driver default to \"x-api-key\")",
                p.name,
                SUPPORTED_AUTH_STYLES
                    .iter()
                    .map(|s| format!("\"{s}\""))
                    .collect::<Vec<_>>()
                    .join(" / ")
            );
        }
    }
    Ok(())
}

/// Keys a `[node.overrides]` shadow may set (B2). Deliberately
/// NARROW: only identity-routing choices where "same cluster, one
/// node reads a different route" is a coherent want. Anything that
/// wires the cluster itself (`cluster.*`), gates access (`acl.*`) or
/// describes the machine (`node.*`) is refused — an override that
/// forks pool wiring under a local key is a trap, not a feature.
pub const OVERRIDABLE_KEYS: &[&str] = &[
    // `model.default`, the field's name: the list said `model.model`,
    // so a per-node default model was refused, and `model.model` was
    // accepted and applied to nothing (2026-10-04 defaults review).
    "model.default",
    "model.provider",
    "model.api_mode",
    "vision.model",
    "vision.provider",
    "imagegen.model",
    "imagegen.provider",
    "videogen.model",
    "videogen.provider",
    "asr.model",
    "asr.provider",
    // The bot's Jetstream endpoint: the VPS reads its local relay
    // (`ws://jetstream-relay:8644/…`, a name only veles-net resolves);
    // every other node keeps the shared public URL.
    "platforms.atproto.jetstream_url",
];

/// Apply `[node.overrides]` shadows onto the loaded config: each
/// allowlisted dotted key is set to the local value, with the rest of
/// the process none the wiser. Unknown or non-overridable keys are
/// IGNORED with intent (the load still succeeds — a stray override
/// must not brick the gateway), and the scanner never logs them
/// because `node.*` is a Local section.
///
/// Implementation note: the round-trip goes through config.toml TEXT,
/// not `toml::Value::try_from(&cfg)` — `VelesConfig` carries fields
/// with no TOML spelling (an untagged `ModelsSpec::Empty` serializes
/// to a JSON/YAML null, which `toml::Value` refuses), so a Value
/// round-trip fails on a perfectly normal config and the shadow
/// silently no-ops'd. Serializing the STRUCT to text keeps every
/// serde attribute in force and parses back losslessly.
pub fn apply_node_overrides(cfg: &mut VelesConfig) {
    // Flatten the overrides table first: TOML parses a dotted key
    // under `[node.overrides]` as a NESTED table (`model.provider =
    // x` → {model: {provider: x}}), while a quoted key
    // (`"model.provider" = x`) stays flat. Operators will type the
    // dotted form; both spellings must work, so walk any nesting
    // into dotted keys before matching the allowlist.
    let flat = flatten_overrides(&cfg.node.overrides);
    if flat.is_empty() {
        return;
    }
    // Each bail-out below leaves the node reading the SHARED value while
    // its operator believes the shadow is in force — say so.
    let Ok(base_text) = toml::to_string_pretty(&*cfg) else {
        tracing::warn!("[node.overrides] not applied: the config did not serialize");
        return;
    };
    let mut doc: toml::map::Map<String, toml::Value> = match toml::from_str(&base_text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("[node.overrides] not applied: {e}");
            return;
        }
    };
    let mut changed = false;
    for (key, val) in flat {
        if !OVERRIDABLE_KEYS.contains(&key.as_str()) {
            // Ignored, not fatal (a stray shadow must not brick the
            // gateway) — but never silently.
            tracing::warn!("[node.overrides] {key} is not overridable here — ignored");
            continue;
        }
        let segs: Vec<&str> = key.split('.').collect();
        if set_path_in_map(&mut doc, &segs, val) {
            changed = true;
        }
    }
    if !changed {
        return;
    }
    let merged_text = toml::to_string(&doc).unwrap_or_default();
    match toml::from_str::<VelesConfig>(&merged_text) {
        Ok(mut back) => {
            // The overrides table itself must survive as-is (it lives in
            // `[node]`, which text round-trips like any other section).
            back.node.overrides = cfg.node.overrides.clone();
            *cfg = back;
        }
        Err(e) => tracing::warn!("[node.overrides] not applied: {e}"),
    }
}

/// The same shadows on the RAW config document: what the plugin and
/// module hosts hand their plugins (`pluginhost::raw_config_json`)
/// never passes through [`VelesConfig`], so without this a plugin read
/// the SHARED value of a key its node shadows. Same allowlist; the typed
/// load already warns about a key that is not on it.
pub fn apply_node_overrides_raw(root: &mut toml::Value) {
    let Some(tbl) = root.as_table_mut() else {
        return;
    };
    let overrides: std::collections::HashMap<String, toml::Value> = tbl
        .get("node")
        .and_then(|n| n.get("overrides"))
        .and_then(|o| o.as_table())
        .map(|t| t.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    for (key, val) in flatten_overrides(&overrides) {
        if !OVERRIDABLE_KEYS.contains(&key.as_str()) {
            continue;
        }
        let segs: Vec<&str> = key.split('.').collect();
        set_path_in_map(tbl, &segs, val);
    }
}

/// Flatten an overrides table: nested tables become dotted keys
/// (`{model: {provider: x}}` → `model.provider = x`), flat keys pass
/// through, non-table leaves under a nesting stay keyed by their full
/// path. Both `[node.overrides] model.provider = x` and
/// `[node.overrides] "model.provider" = x` end up as the same entry.
fn flatten_overrides(
    map: &std::collections::HashMap<String, toml::Value>,
) -> Vec<(String, toml::Value)> {
    fn walk(prefix: &str, v: &toml::Value, out: &mut Vec<(String, toml::Value)>) {
        match v {
            toml::Value::Table(t) => {
                for (k, sub) in t {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(&path, sub, out);
                }
            }
            other => out.push((prefix.to_string(), other.clone())),
        }
    }
    let mut out = Vec::new();
    for (k, v) in map {
        walk(k, v, &mut out);
    }
    out
}

/// Set a dotted path inside the serialized-config map. Returns false
/// when the path is blocked by a non-table.
fn set_path_in_map(
    root: &mut toml::map::Map<String, toml::Value>,
    segs: &[&str],
    val: toml::Value,
) -> bool {
    let mut cur = root;
    for seg in &segs[..segs.len() - 1] {
        let entry = cur
            .entry((*seg).to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        if !entry.is_table() {
            *entry = toml::Value::Table(Default::default());
        }
        cur = match entry.as_table_mut() {
            Some(t) => t,
            None => return false,
        };
    }
    cur.insert(segs[segs.len() - 1].to_string(), val);
    true
}

/// The wire protocols this build speaks. `responses` landed 2026-09-03
/// after a live probe of the token-plan endpoint proved the wire shape
/// (see COVERAGE.md for the probe findings; responses.rs for the
/// client).
pub const SUPPORTED_API_MODES: &[&str] = &["chat_completions", "responses", "plugin"];

/// `[model] api_mode` is the wire default for every provider entry
/// that does not set its own. Before this landed the field parsed and
/// validated but was never read — a config line that did nothing. An
/// entry's explicit `api_mode` always wins; empty (unset) entries
/// inherit the model-level value; with neither set the entry stays
/// empty, which means chat_completions exactly as before.
fn apply_model_api_mode_default(cfg: &mut VelesConfig) {
    let Some(mode) = cfg.model.api_mode.clone() else {
        return;
    };
    for p in &mut cfg.custom_providers {
        if p.api_mode.trim().is_empty() {
            p.api_mode = mode.clone();
        }
    }
}

/// Refuse an `api_mode` we cannot honour, by name.
///
/// `providers.rs`'s module doc has always advertised three modes —
/// `chat_completions`, `anthropic_messages`, `codex_responses` — and
/// exactly one was implemented. Every other value was silently treated
/// as `chat_completions`, so a config asking for the Anthropic wire
/// format got the OpenAI one and no indication whatsoever. Writing a
/// mode that does nothing should fail loudly at load, not succeed
/// quietly and behave as something else.
///
/// Empty/absent stays valid: it is serde's default and means "unset",
/// which legitimately resolves to `chat_completions`.
fn validate_api_modes(cfg: &VelesConfig) -> Result<()> {
    let check = |mode: &str, whose: &str| -> Result<()> {
        let m = mode.trim();
        if m.is_empty() || SUPPORTED_API_MODES.contains(&m) {
            return Ok(());
        }
        anyhow::bail!(
            "{whose}: api_mode = \"{m}\" is not implemented — this build speaks \
             only {}. Remove the line (or set it to \"chat_completions\") to use \
             the OpenAI-compatible chat wire format; it was previously accepted \
             and silently treated as \"chat_completions\", which is not what the \
             line said.",
            SUPPORTED_API_MODES
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(" / ")
        )
    };
    if let Some(m) = &cfg.model.api_mode {
        check(m, "[model]")?;
    }
    for p in &cfg.custom_providers {
        check(&p.api_mode, &format!("provider \"{}\"", p.name))?;
    }
    Ok(())
}

/// Translate a hermes config.yaml into VelesConfig (best-effort subset).
pub fn from_hermes_yaml(raw: &str) -> Result<VelesConfig> {
    Ok(from_hermes_yaml_report(raw)?.0)
}

/// What a migration did with each key it met.
///
/// Serde IGNORES unknown fields, which is what made the plain
/// `from_hermes_yaml` above quietly correct and quietly lossy: a hermes
/// key veles spells differently just vanished, and `veles migrate`
/// printed a tick. The user then runs on defaults they never chose and
/// has no way to find out. Every key is now accounted for in one of
/// these three lists.
#[derive(Debug, Default)]
pub struct MigrationReport {
    /// hermes key → the veles key it became.
    pub translated: Vec<(String, String)>,
    /// hermes key → why nothing was written for it.
    pub unsupported: Vec<(String, String)>,
    /// hermes-internal bookkeeping, deliberately not carried over.
    pub ignored: Vec<String>,
}

impl MigrationReport {
    pub fn is_clean(&self) -> bool {
        self.unsupported.is_empty()
    }
}

/// Hermes keys that veles spells differently, and hermes keys veles has
/// no home for. Kept as data next to the translation that uses it, so
/// adding a veles section is one edit rather than two that drift.
///
/// The `ignored` list is hermes' own bookkeeping — carrying it over
/// would put meaningless keys in a veles config.
const HERMES_IGNORED: &[&str] = &["_config_version", "onboarding"];

/// Translate a hermes config.yaml, reporting what happened to each key.
///
/// Most sections share a name and serde does the work. These do not,
/// and each was silently dropped before:
///
///   compression.threshold → agent.compaction.trigger_percent
///       A FRACTION in hermes (0.7) and a PERCENT in veles (85.0). The
///       unit change is the whole point of translating it: copied
///       across verbatim, 0.7 would mean "compact at 0.7% full", i.e.
///       on every turn.
///
/// `browser`, `lsp` and `terminal` have no live counterpart in veles
/// (secrets are scrubbed unconditionally and there is no private-URL
/// gate), so they are reported as unsupported rather than written
/// somewhere inert.
pub fn from_hermes_yaml_report(raw: &str) -> Result<(VelesConfig, MigrationReport)> {
    let mut v: serde_yaml::Value = serde_yaml::from_str(raw)?;
    let mut report = MigrationReport::default();

    // ── the renames ──
    if let Some(threshold) = take_yaml_f64(&mut v, &["compression", "threshold"]) {
        // hermes writes a fraction; anything > 1 is already a percent
        // and is taken at face value rather than multiplied into
        // nonsense.
        let percent = if threshold <= 1.0 {
            threshold * 100.0
        } else {
            threshold
        };
        set_yaml_f64(&mut v, &["agent", "compaction", "trigger_percent"], percent);
        report.translated.push((
            "compression.threshold".into(),
            format!("agent.compaction.trigger_percent = {percent}"),
        ));
    }

    // ── what has nowhere to go ──
    for (key, why) in [
        (
            "browser",
            "veles has no private-URL gate; secrets are scrubbed unconditionally",
        ),
        (
            "lsp",
            "the Dev pane starts its own language server on demand",
        ),
        (
            "terminal",
            "terminal appearance follows the desktop theme (ui.font_scale)",
        ),
        (
            "security",
            "veles scrubs secrets unconditionally rather than by a flag",
        ),
    ] {
        if yaml_has(&v, key) {
            report.unsupported.push((key.into(), why.into()));
        }
    }
    for key in HERMES_IGNORED {
        if yaml_has(&v, key) {
            report.ignored.push((*key).into());
        }
    }

    let cfg: VelesConfig =
        serde_yaml::from_value(v).map_err(|e| anyhow::anyhow!("hermes config compat: {e}"))?;
    Ok((cfg, report))
}

fn yaml_has(v: &serde_yaml::Value, key: &str) -> bool {
    v.get(key).is_some_and(|x| !x.is_null())
}

/// Remove and return a nested number, so the caller can re-home it.
fn take_yaml_f64(v: &mut serde_yaml::Value, path: &[&str]) -> Option<f64> {
    let (last, parents) = path.split_last()?;
    let mut cur = v;
    for p in parents {
        cur = cur.get_mut(*p)?;
    }
    let map = cur.as_mapping_mut()?;
    let got = map.remove(serde_yaml::Value::from(*last))?;
    got.as_f64()
}

fn set_yaml_f64(v: &mut serde_yaml::Value, path: &[&str], value: f64) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cur = v;
    for p in parents {
        if cur.get(*p).is_none() {
            if let Some(map) = cur.as_mapping_mut() {
                map.insert(
                    serde_yaml::Value::from(*p),
                    serde_yaml::Value::Mapping(Default::default()),
                );
            }
        }
        let Some(next) = cur.get_mut(*p) else { return };
        cur = next;
    }
    if let Some(map) = cur.as_mapping_mut() {
        map.insert(
            serde_yaml::Value::from(*last),
            serde_yaml::Value::from(value),
        );
    }
}

#[cfg(test)]
mod hermes_migration_tests {
    use super::*;

    /// `veles migrate` could not write a config AT ALL.
    ///
    /// `toml::to_string_pretty` was handed the whole struct, and three
    /// fields serialize as a UNIT — which TOML has no spelling for:
    /// `platforms` and `display` default to `serde_yaml::Value::Null`,
    /// and `ModelsSpec::Empty` is a unit variant. Even
    /// `VelesConfig::default()` failed, so the command has never worked
    /// on any input; on this machine it died with
    /// "unsupported unit type" against the real hermes config.
    #[test]
    fn a_default_config_can_be_written_as_toml() {
        let s = toml::to_string_pretty(&VelesConfig::default())
            .expect("a default config must be serializable");
        let _: VelesConfig = toml::from_str(&s).expect("and must read back");
    }

    #[test]
    fn a_provider_with_no_models_can_be_written() {
        let cfg = VelesConfig {
            custom_providers: vec![ProviderEntry {
                name: "p".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let s = toml::to_string_pretty(&cfg).expect("ModelsSpec::Empty must not break the write");
        let back: VelesConfig = toml::from_str(&s).expect("reads back");
        assert_eq!(back.custom_providers.len(), 1);
        assert!(back.custom_providers[0].models.is_empty());
    }

    /// THE UNIT CHANGE IS THE POINT. hermes writes a fraction, veles a
    /// percent; copied across verbatim, 0.7 would mean "compact when
    /// 0.7% full" — on every single turn.
    #[test]
    fn compression_threshold_becomes_a_percentage() {
        let (cfg, report) =
            from_hermes_yaml_report("compression:\n  threshold: 0.7\n").expect("translates");
        assert_eq!(cfg.agent.compaction.trigger_percent, 70.0);
        assert!(
            report
                .translated
                .iter()
                .any(|(from, _)| from == "compression.threshold"),
            "and it is reported, not silent: {report:?}"
        );
    }

    /// A value already expressed as a percent is taken at face value
    /// rather than multiplied into nonsense.
    #[test]
    fn a_threshold_over_one_is_already_a_percent() {
        let (cfg, _) = from_hermes_yaml_report("compression:\n  threshold: 85\n").unwrap();
        assert_eq!(cfg.agent.compaction.trigger_percent, 85.0);
    }

    /// Serde ignores unknown fields, so a hermes key veles spells
    /// differently used to vanish under a printed tick. Every key is
    /// accounted for now.
    #[test]
    fn keys_with_no_veles_home_are_reported_not_dropped_silently() {
        let yaml = "\
browser:\n  allow_private_urls: true\n\
lsp:\n  enabled: false\n\
terminal:\n  font_family: Whatever\n\
security:\n  redact_secrets: false\n\
onboarding:\n  seen: {}\n\
_config_version: 33\n";
        let (_, report) = from_hermes_yaml_report(yaml).expect("parses");
        let named: Vec<&str> = report.unsupported.iter().map(|(k, _)| k.as_str()).collect();
        for k in ["browser", "lsp", "terminal", "security"] {
            assert!(named.contains(&k), "{k} must be reported: {report:?}");
        }
        assert!(
            report.unsupported.iter().all(|(_, why)| !why.is_empty()),
            "each with a reason: {report:?}"
        );
        assert_eq!(report.ignored.len(), 2, "hermes bookkeeping: {report:?}");
        assert!(
            !report.is_clean(),
            "a config with dropped keys is not clean"
        );
    }

    /// The common case must stay quiet: a config veles understands
    /// entirely should report nothing to worry about.
    #[test]
    fn a_fully_supported_config_reports_clean() {
        let (cfg, report) =
            from_hermes_yaml_report("model:\n  provider: p\n  default: m\n").expect("parses");
        assert_eq!(cfg.model.provider.as_deref(), Some("p"));
        assert!(report.is_clean(), "{report:?}");
        assert!(report.translated.is_empty());
    }

    /// The whole `veles migrate` path, end to end: a hermes-shaped YAML
    /// in, a TOML file veles can load out.
    #[test]
    fn a_hermes_config_round_trips_to_loadable_toml() {
        let yaml = "\
model:\n  provider: custom:ali\n  default: deepseek-v4\n\
agent:\n  reasoning_effort: xhigh\n\
compression:\n  threshold: 0.7\n\
memory:\n  memory_char_limit: 100000\n\
custom_providers:\n\
\x20 - name: ali\n\
\x20   base_url: https://example.invalid/v1\n\
\x20   api_mode: chat_completions\n\
browser:\n  allow_private_urls: true\n";
        let (cfg, report) = from_hermes_yaml_report(yaml).expect("translates");
        let toml_str = toml::to_string_pretty(&cfg).expect("serializes");
        let back: VelesConfig = toml::from_str(&toml_str).expect("loads back");

        assert_eq!(back.model.default.as_deref(), Some("deepseek-v4"));
        assert_eq!(back.agent.reasoning_effort.as_deref(), Some("xhigh"));
        assert_eq!(back.agent.compaction.trigger_percent, 70.0);
        assert_eq!(back.memory.memory_char_limit, 100000);
        assert_eq!(back.custom_providers.len(), 1);
        assert_eq!(report.unsupported.len(), 1, "browser: {report:?}");
    }
}

/// Step 4.2 — the wire protocol a config may ask for.
#[cfg(test)]
mod api_mode_tests {
    use super::*;

    fn cfg_with_provider_mode(mode: &str) -> VelesConfig {
        VelesConfig {
            custom_providers: vec![ProviderEntry {
                name: "ali".into(),
                api_mode: mode.into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Empty is serde's default and means "unset" — the overwhelmingly
    /// common case, and legitimately `chat_completions`. It must stay
    /// valid, or every provider block that omits the key breaks.
    #[test]
    fn an_absent_or_empty_api_mode_is_accepted() {
        assert!(validate_api_modes(&cfg_with_provider_mode("")).is_ok());
        assert!(validate_api_modes(&cfg_with_provider_mode("   ")).is_ok());
        assert!(validate_api_modes(&cfg_with_provider_mode("chat_completions")).is_ok());
        assert!(validate_api_modes(&VelesConfig::default()).is_ok());
    }

    /// 2026-09-03: the Responses API is the second implemented mode.
    /// The live probes that settled its wire shape are in COVERAGE.md;
    /// the client is responses.rs.
    #[test]
    fn responses_is_an_implemented_api_mode() {
        assert!(validate_api_modes(&cfg_with_provider_mode("responses")).is_ok());
        let err = validate_api_modes(&cfg_with_provider_mode("codex_responses"))
            .expect_err("hermes' spelling is still not a mode here");
        assert!(err.to_string().contains("codex_responses"));
    }

    /// `built_in_tools` parses off a provider block and defaults empty.
    #[test]
    fn built_in_tools_round_trip() {
        let cfg: VelesConfig = toml::from_str(
            r#"
            [[custom_providers]]
            name = "ali"
            base_url = "https://x/compatible-mode/v1"
            api_mode = "responses"
            built_in_tools = ["web_search", "code_interpreter"]
            "#,
        )
        .expect("parse");
        assert_eq!(
            cfg.custom_providers[0].built_in_tools,
            vec!["web_search".to_string(), "code_interpreter".to_string()]
        );
        assert!(validate_api_modes(&cfg).is_ok());
        let bare: VelesConfig =
            toml::from_str("[[custom_providers]]\nname = \"x\"\n").expect("parse");
        assert!(bare.custom_providers[0].built_in_tools.is_empty());
    }

    /// A documented-but-unimplemented mode used to load fine and behave
    /// as chat_completions — the config said Anthropic, the wire said
    /// OpenAI, and nothing anywhere said so. Now it fails by name.
    #[test]
    fn an_unimplemented_api_mode_is_rejected_by_name() {
        let err = validate_api_modes(&cfg_with_provider_mode("anthropic_messages"))
            .expect_err("must not load");
        let msg = err.to_string();
        assert!(msg.contains("anthropic_messages"), "names the mode: {msg}");
        assert!(msg.contains("ali"), "names the provider: {msg}");
        assert!(
            msg.contains("chat_completions"),
            "and names what IS supported: {msg}"
        );
    }

    /// The same check covers `[model] api_mode`, not just providers.
    #[test]
    fn the_model_sections_api_mode_is_checked_too() {
        let cfg = VelesConfig {
            model: ModelSection {
                api_mode: Some("codex_responses".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let msg = validate_api_modes(&cfg)
            .expect_err("must not load")
            .to_string();
        assert!(
            msg.contains("codex_responses") && msg.contains("[model]"),
            "{msg}"
        );

        // …and None is fine.
        let ok = VelesConfig {
            model: ModelSection::default(),
            ..Default::default()
        };
        assert!(validate_api_modes(&ok).is_ok());
    }

    /// A provider whose `auth_style` is one of the two this build can send.
    fn cfg_with_auth_style(style: Option<&str>) -> VelesConfig {
        VelesConfig {
            custom_providers: vec![ProviderEntry {
                name: "anthropic".into(),
                auth_style: style.map(str::to_string),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Absent, empty and both supported styles load. Empty is serde's
    /// default and means "the driver decides", which is the overwhelmingly
    /// common case — it must stay valid or every provider block that omits
    /// the key breaks.
    #[test]
    fn an_absent_or_supported_auth_style_is_accepted() {
        assert!(validate_auth_styles(&cfg_with_auth_style(None)).is_ok());
        assert!(validate_auth_styles(&cfg_with_auth_style(Some(""))).is_ok());
        assert!(validate_auth_styles(&cfg_with_auth_style(Some("   "))).is_ok());
        assert!(validate_auth_styles(&cfg_with_auth_style(Some("x-api-key"))).is_ok());
        assert!(validate_auth_styles(&cfg_with_auth_style(Some("bearer"))).is_ok());
    }

    /// `cache_ttl` takes Anthropic's two lifetimes or nothing, and a typo is
    /// refused by name instead of silently becoming the driver's default.
    #[test]
    fn a_cache_ttl_is_one_of_the_wire_s_two() {
        let with = |ttl: Option<&str>| VelesConfig {
            custom_providers: vec![ProviderEntry {
                name: "anthropic".into(),
                cache_ttl: ttl.map(str::to_string),
                ..Default::default()
            }],
            ..Default::default()
        };
        for ok in [None, Some(""), Some("5m"), Some("1h")] {
            assert!(validate_cache_ttls(&with(ok)).is_ok(), "{ok:?}");
        }
        let msg = validate_cache_ttls(&with(Some("60m")))
            .expect_err("not a lifetime the wire has")
            .to_string();
        assert!(msg.contains("anthropic") && msg.contains("60m"), "{msg}");
    }

    /// An unknown style is refused BY NAME at load, not silently defaulted.
    ///
    /// The failure this prevents reads as a bad API key: the driver would
    /// authenticate with `x-api-key` against a gateway that wants a bearer
    /// token, get a 401, and the operator would go rotate a credential that
    /// was fine. One word in the config is the actual problem, so the load
    /// says so.
    #[test]
    fn an_unknown_auth_style_is_rejected_by_name() {
        let msg = validate_auth_styles(&cfg_with_auth_style(Some("api-key")))
            .expect_err("Azure's spelling is not one this build sends")
            .to_string();
        assert!(msg.contains("api-key"), "names the bad style: {msg}");
        assert!(msg.contains("anthropic"), "names the provider: {msg}");
        assert!(
            msg.contains("x-api-key") && msg.contains("bearer"),
            "and names what IS supported: {msg}"
        );
    }
}

#[cfg(test)]
mod node_override_tests {
    use super::*;

    /// The shadow applies at load: `[node.overrides]` changes what
    /// the process reads for an allowlisted key, nothing else moves.
    #[test]
    fn overrides_shadow_at_load() {
        let mut cfg: VelesConfig = toml::from_str(
            "[model]\nprovider = \"shared-default\"\n\n\
             [node.overrides]\nmodel.provider = \"vps-local-route\"\n",
        )
        .unwrap();
        apply_node_overrides(&mut cfg);
        assert_eq!(cfg.model.provider.as_deref(), Some("vps-local-route"));
    }

    /// The bot's Jetstream URL is a per-node route: the VPS reads its
    /// relay, a name only veles-net resolves. The plugin hosts hand the
    /// RAW document to the atproto plugin, so the shadow must hold there
    /// too, and leave the rest of `[platforms.atproto]` alone.
    #[test]
    fn a_node_can_shadow_the_jetstream_url_for_its_plugins() {
        let text = "[platforms.atproto]\nbot_handle = \"b.example\"\n\
                    jetstream_url = \"wss://public.example/x\"\n\n\
                    [node.overrides]\n\
                    \"platforms.atproto.jetstream_url\" = \"ws://jetstream-relay:8644/x\"\n\
                    cluster.seeds = [\"evil.example\"]\n";
        let mut raw: toml::Value = toml::from_str(text).unwrap();
        apply_node_overrides_raw(&mut raw);
        assert_eq!(
            raw["platforms"]["atproto"]["jetstream_url"].as_str(),
            Some("ws://jetstream-relay:8644/x")
        );
        assert_eq!(
            raw["platforms"]["atproto"]["bot_handle"].as_str(),
            Some("b.example")
        );
        assert!(
            raw.get("cluster").is_none(),
            "a key off the allowlist stays out"
        );
        // The dotted spelling (nested tables) shadows the same key.
        let mut raw: toml::Value =
            toml::from_str("[node.overrides]\nplatforms.atproto.jetstream_url = \"ws://r/x\"\n")
                .unwrap();
        apply_node_overrides_raw(&mut raw);
        assert_eq!(
            raw["platforms"]["atproto"]["jetstream_url"].as_str(),
            Some("ws://r/x")
        );
        // And the typed load agrees.
        let mut cfg: VelesConfig = toml::from_str(text).unwrap();
        apply_node_overrides(&mut cfg);
        assert_eq!(
            cfg.platforms["atproto"]["jetstream_url"].as_str(),
            Some("ws://jetstream-relay:8644/x")
        );
    }

    /// The default MODEL is overridable per node, under its real key.
    #[test]
    fn a_node_can_shadow_the_default_model() {
        let mut cfg: VelesConfig = toml::from_str(
            "[model]\ndefault = \"shared-model\"\n\n\
             [node.overrides]\nmodel.default = \"local-model\"\n",
        )
        .unwrap();
        apply_node_overrides(&mut cfg);
        assert_eq!(cfg.model.default.as_deref(), Some("local-model"));
    }

    /// `model.api_mode` is overridable, and it is also what fills the
    /// api_mode of every custom provider that left its own empty. The
    /// override must be in place BEFORE that fill, or the node keeps
    /// the SHARED wire for those providers.
    #[test]
    fn an_api_mode_override_reaches_the_providers_that_inherit_it() {
        let mut cfg: VelesConfig = toml::from_str(
            "[model]\napi_mode = \"chat_completions\"\n\n\
             [[custom_providers]]\nname = \"p\"\nbase_url = \"https://a.example\"\n\n\
             [node.overrides]\nmodel.api_mode = \"responses\"\n",
        )
        .unwrap();
        finish_loaded(&mut cfg).unwrap();
        assert_eq!(cfg.model.api_mode.as_deref(), Some("responses"));
        assert_eq!(
            cfg.custom_providers[0].api_mode, "responses",
            "the inheriting provider speaks the overridden wire"
        );
    }

    /// A non-allowlisted override is IGNORED — the load still
    /// succeeds, and the key keeps its shared value. Overriding
    /// `cluster.seeds` under a local shadow would fork the pool's own
    /// wiring; the allowlist exists so that cannot happen silently.
    #[test]
    fn non_overridable_keys_are_ignored() {
        let mut cfg: VelesConfig = toml::from_str(
            "[cluster]\nseeds = [\"10.0.20.1\"]\n\n\
             [node.overrides]\ncluster.seeds = [\"evil.example\"]\n",
        )
        .unwrap();
        apply_node_overrides(&mut cfg);
        assert_eq!(cfg.cluster.seeds, vec!["10.0.20.1".to_string()]);
        // And an unknown key is equally ignored, not an error.
        let mut cfg2: VelesConfig =
            toml::from_str("[node.overrides]\nbrand.new.key = 1\n").unwrap();
        apply_node_overrides(&mut cfg2); // must not panic
    }

    /// The overrides table parses in BOTH spellings: dotted keys
    /// arrive nested (`{model: {api_mode}}`), quoted keys flat. The
    /// flattener normalizes them, so the shadow applies either way.
    #[test]
    fn overrides_parse_any_value_shape() {
        let cfg: VelesConfig = toml::from_str(
            "[node.overrides]\nmodel.api_mode = \"responses\"\nasr.provider = \"kimi\"\n",
        )
        .unwrap();
        let flat = flatten_overrides(&cfg.node.overrides);
        let keys: Vec<&str> = flat.iter().map(|(k, _)| k.as_str()).collect();
        assert!(
            keys.contains(&"model.api_mode"),
            "dotted form flattens: {keys:?}"
        );
        assert!(
            keys.contains(&"asr.provider"),
            "dotted form flattens: {keys:?}"
        );
        let cfg2: VelesConfig =
            toml::from_str("[node.overrides]\n\"model.api_mode\" = \"responses\"\n").unwrap();
        let flat2 = flatten_overrides(&cfg2.node.overrides);
        assert!(
            flat2.iter().any(|(k, _)| k == "model.api_mode"),
            "quoted form stays flat: {flat2:?}"
        );
    }
}

#[cfg(test)]
mod hermes_fallback_tests {
    use super::*;

    /// The zai-MCP incident (2026-10-02), pinned at the source.
    ///
    /// A forced `VELES_HOME` says "this one directory is the whole
    /// world" — the same sentence a `DirOverride` says per-thread. The
    /// override is a thread-local, so it cannot reach a CHILD PROCESS:
    /// the e2e tests in veles-cli boot a real `veles` binary with a
    /// scratch `VELES_HOME` and no config.toml, `load()` fell through
    /// to the developer's live `~/.hermes/config.yaml`, and hermes'
    /// `enabled: false` MCP entries spawned anyway (veles' parser had
    /// no `enabled` key — pinned separately in `mcp::tests`). Ten
    /// `npx -y @z_ai/mcp-server@latest` runs in one day.
    ///
    /// Under the env lock (NOT `scratch_home`, which installs a
    /// DirOverride and would prove the wrong branch): with only
    /// `VELES_HOME` set, the compat root must be invisible — even when
    /// a hermes config.yaml sits inside the forced home, because the
    /// compat root is the DEVELOPER's real `~/.hermes`, and nothing
    /// under a forced home is that.
    #[test]
    fn a_forced_home_never_falls_back_to_the_developers_hermes() {
        let _g = crate::testenv::guard();
        // A fake hermes install INSIDE the forced home: still not the
        // compat root (same rule as skills::skill_roots).
        let home = tempfile::tempdir().expect("scratch");
        std::fs::create_dir_all(home.path().join(".hermes")).unwrap();
        std::fs::write(
            home.path().join(".hermes").join("config.yaml"),
            "model:\n  provider: fake\n",
        )
        .unwrap();
        std::env::set_var("VELES_HOME", home.path());

        assert!(
            hermes_config_yaml().is_none(),
            "a forced VELES_HOME must not read the ambient ~/.hermes — \
             that fallback is what spawned the zai MCP in every e2e child"
        );
        assert!(
            !config_file_present(),
            "and the compat file must not count as 'a config exists' \
             under a forced home with no config.toml of its own"
        );
    }
}

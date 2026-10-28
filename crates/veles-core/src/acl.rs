//! Multi-user, multilevel, capability-based access control.
//!
//! WHY (USER, 2026-08-22): "we do deploy on IRC channels with multiple
//! users, and only the owner should have access to many functions and
//! memories." Generalizes Letta's DM policies + admin command tiers +
//! group sender policy into one capability model for every platform.
//!
//! ## Model
//!
//! - **Identity is the hostmask.** USER, 2026-09-09: *"The bot should
//!   NOT have NickServ/services gating of any sort, the ACL host masks
//!   are authoritative."* Rules name `nick!user@host` patterns with
//!   globs or `re:` regexes per segment (`*!*@gateway/veles`), scoped
//!   to a network, a channel and/or a connector — the weechat/eggdrop
//!   hostmask model. Many networks have no services at all and many
//!   users never register, so a design that treats a mask as a
//!   degraded account is a design that treats the normal case as
//!   broken.
//! - **An account, where one exists, is one more spec form** —
//!   `irc:<account>` (casefolded; from account-tag/extended-join/WHOIS)
//!   or `telegram:<user id>`. It is the most specific thing a rule can
//!   name, because the server vouches for it; it is not a prerequisite
//!   for anything. `Principal.id` is None whenever there is none, which
//!   is why nothing downstream may key on it — see `subject`.
//! - **The most specific matching rule wins**, regardless of where it
//!   was declared: host beats ident beats nick, a literal beats a glob,
//!   an exact identity beats every mask, and an equal-specificity tie
//!   goes to the LOWER role. `Principal::explain` (`/acl explain`)
//!   shows the winner and what it beat, because with specificity
//!   deciding, "why does this person have this role" can no longer be
//!   answered by reading the file top to bottom.
//! - **Users**: identities — exact platform ids and/or masks — can be
//!   grouped under a named user whose role then holds across networks
//!   and platforms (the same human trusted on libera, oftc and telegram).
//! - **Roles** (multilevel): owner > admin > trusted > user > guest, plus
//!   banned (a refuser, not a level). Each role carries a default
//!   capability set; config can grant extra caps to an identity and can
//!   pin an identity to a different role.
//! - **Capabilities** gate: chatting (turns), each tool family, memory
//!   read/write, the builtin command tiers, approvals, cron, delegation,
//!   A2A. Capability checks are pure functions of (caps, action) so every
//!   enforcement point asks the same question.
//! - **Surfaces**: desktop/TUI/CLI are owner surfaces — the human at the
//!   keyboard IS the owner (full caps by definition). Only gateway
//!   platform sessions (IRC/telegram/…) go through identity resolution.
//!   The desktop's IRC tab manages the ACL directly (roster menu + /acl).
//! - **Sessions**: gateway sessions are keyed per SENDER, not per chat —
//!   `platform:chat:subject`. A guest in a shared channel gets their own
//!   session, their own history, and never the owner's context. Keyed on
//!   `subject` and not `id` since 2026-09-09: with no services `id` was
//!   None for everyone, so "per sender" was really "all of them
//!   together", sharing one history, one frozen prompt and one sandbox.
//!
//! ## Enforcement points (all in this crate, wired in this milestone)
//!
//! 1. Gateway inbound: banned ⇒ drop; no `chat` cap ⇒ refuse politely.
//! 2. Builtin slash commands: tiered (status/version/help = guest floor;
//!    guards/model/theme/bindings = user+; admin commands = admin+).
//! 3. System prompt: MEMORY / USER / skills index are injected ONLY with
//!    `memory-read` (+ skills for `skills` cap). Without it the session
//!    is a clean-slate assistant — this closes the leak where a channel
//!    guest's turn put the owner's memories in the model's context.
//! 4. Tool dispatch (run_turn): each tool call is checked against caps
//!    BEFORE dispatch; refusal is a tool result, so the model adapts.
//! 5. Memory tool: additionally refuses read vs write by direction.
//! 6. Approvals/asks: an ask belongs to the session that asked; only the
//!    SAME sender may answer it (an ask opened by the owner's turn can't
//!    be answered by a guest typing "1" in the channel).
//!
//! ## Fail-closed posture
//!
//! Unresolvable identity, missing config, unknown role strings, a tool
//! nobody classified, two rules of equal specificity that disagree —
//! all resolve to the LEAST privilege, never to openness.
//!
//! The one exception is the single-user legacy default: when NO acl
//! config exists at all the ACL is dormant and every sender the adapter
//! let through resolves as OWNER, which is pre-ACL behavior preserved.
//! That is why the adapters keep their allowlist in exactly that case,
//! and why the acting `.agent` subcommands refuse outright when there
//! is neither an `[acl]` section nor an allowlist: with no boundary
//! declared there is nobody to be privileged above. The owner is
//! expected to declare themselves in config.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

// ── capabilities ──────────────────────────────────────────────────────

/// A permission to do a class of things. Deliberately coarse-grained:
/// per-argument rules (allowlist globs, approval scopes) already live in
/// the approval gate; caps answer "may this identity use terminal at
/// all", not "may it run this exact command".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cap {
    /// Send messages that start agent turns.
    Chat,
    /// Read the owner's memory + user profile (prompt injection + the
    /// memory tool's read side).
    MemoryRead,
    /// Write to memory / user profile.
    MemoryWrite,
    /// Load skills (index in prompt + skill_view/skills_list).
    Skills,
    /// Manage skills (create/patch/delete).
    SkillManage,
    /// terminal + execute_code.
    Shell,
    /// read_file / search_files.
    FileRead,
    /// write_file / patch (checkpoints still apply).
    FileWrite,
    /// web_search / web_extract.
    Web,
    /// Browser automation (browser_*).
    Browser,
    /// computer_use.
    ComputerUse,
    /// vision_analyze.
    Vision,
    /// generate_image / transcribe_audio — the cheap media spend.
    MediaGen,
    /// generate_video / `/gen video`. Separate from MediaGen on
    /// purpose: one 5-second clip cost 1200 credits on the live plan
    /// (USER, 2026-09-11: "this needs to be a separate capability and
    /// gated behind trusted+").
    VideoGen,
    /// text_to_speech.
    Tts,
    /// delegate_task.
    Delegate,
    /// cronjob (create/list/manage scheduled jobs).
    Cron,
    /// a2a (talk to peer agents).
    A2a,
    /// session_search / session_export (other sessions' transcripts).
    SessionHistory,
    /// todo.
    Todo,
    /// Answer approvals/asks in this session.
    Approve,
    /// Open preview / focus pane on the owner's desktop.
    Desktop,
    /// MCP tools (tool_call mcp__*) + deferred discovery tools.
    Mcp,
    /// Builtin info commands: /status /version /help /guards.
    CmdStatus,
    /// Session-shaping builtins: /model /theme /bindings.
    CmdSession,
    /// Gateway admin builtins: /acl /reload-level, cancel own turns.
    CmdAdmin,
    /// Everything (owner). Implied by Role::Owner, never granted directly.
    All,
}

impl Cap {
    pub fn as_str(&self) -> &'static str {
        match self {
            Cap::Chat => "chat",
            Cap::MemoryRead => "memory-read",
            Cap::MemoryWrite => "memory-write",
            Cap::Skills => "skills",
            Cap::SkillManage => "skill-manage",
            Cap::Shell => "shell",
            Cap::FileRead => "file-read",
            Cap::FileWrite => "file-write",
            Cap::Web => "web",
            Cap::Browser => "browser",
            Cap::ComputerUse => "computer-use",
            Cap::Vision => "vision",
            Cap::MediaGen => "media-gen",
            Cap::VideoGen => "video-gen",
            Cap::Tts => "tts",
            Cap::Delegate => "delegate",
            Cap::Cron => "cron",
            Cap::A2a => "a2a",
            Cap::SessionHistory => "session-history",
            Cap::Todo => "todo",
            Cap::Approve => "approve",
            Cap::Desktop => "desktop",
            Cap::Mcp => "mcp",
            Cap::CmdStatus => "cmd-status",
            Cap::CmdSession => "cmd-session",
            Cap::CmdAdmin => "cmd-admin",
            Cap::All => "all",
        }
    }

    pub fn from_str_lossy(s: &str) -> Option<Cap> {
        Some(
            match s
                .trim()
                .trim_start_matches('-')
                .replace('_', "-")
                .to_ascii_lowercase()
                .as_str()
            {
                "chat" => Cap::Chat,
                "memoryread" | "memory-read" => Cap::MemoryRead,
                "memorywrite" | "memory-write" => Cap::MemoryWrite,
                "skills" => Cap::Skills,
                "skillmanage" | "skill-manage" => Cap::SkillManage,
                "shell" | "terminal" => Cap::Shell,
                "fileread" | "file-read" => Cap::FileRead,
                "filewrite" | "file-write" => Cap::FileWrite,
                "web" => Cap::Web,
                "browser" => Cap::Browser,
                "computeruse" | "computer-use" => Cap::ComputerUse,
                "vision" => Cap::Vision,
                "mediagen" | "media-gen" => Cap::MediaGen,
                "videogen" | "video-gen" => Cap::VideoGen,
                "tts" => Cap::Tts,
                "delegate" => Cap::Delegate,
                "cron" => Cap::Cron,
                "a2a" => Cap::A2a,
                "sessionhistory" | "session-history" => Cap::SessionHistory,
                "todo" => Cap::Todo,
                "approve" => Cap::Approve,
                "desktop" => Cap::Desktop,
                "mcp" => Cap::Mcp,
                "cmdstatus" | "cmd-status" => Cap::CmdStatus,
                "cmdsession" | "cmd-session" => Cap::CmdSession,
                "cmdadmin" | "cmd-admin" => Cap::CmdAdmin,
                "all" => Cap::All,
                _ => return None,
            },
        )
    }
}

/// (name, one-line description) for every capability, in a stable
/// order — the material the desktop ACL UI and /acl help render from.
/// Kept here rather than in each surface so the wording cannot drift.
pub fn cap_catalog() -> Vec<(String, String)> {
    use Cap::*;
    [
        (Chat, "start agent turns"),
        (MemoryRead, "read the owner's memory and profile"),
        (MemoryWrite, "write memory and the user profile"),
        (Skills, "load skills"),
        (SkillManage, "create, patch, delete skills"),
        (
            Shell,
            "run terminal and execute_code (sandbox-scoped below admin)",
        ),
        (FileRead, "read and search files"),
        (FileWrite, "write files and patches"),
        (Web, "search and fetch the web"),
        (Browser, "drive a browser"),
        (ComputerUse, "control the desktop"),
        (Vision, "see and analyze images"),
        (MediaGen, "generate images and video"),
        (Tts, "text to speech"),
        (Delegate, "spawn subagents"),
        (Cron, "schedule jobs"),
        (A2a, "talk to peer agents"),
        (SessionHistory, "read other sessions' transcripts"),
        (Todo, "track todos"),
        (Approve, "answer approvals"),
        (Desktop, "open panes and previews on the owner desktop"),
        (Mcp, "use MCP tools"),
        (CmdStatus, "builtin status commands"),
        (CmdSession, "session-shaping builtins (/model /theme …)"),
        (CmdAdmin, "gateway-admin builtins"),
        (All, "everything — owner only"),
    ]
    .into_iter()
    .map(|(c, d)| (c.as_str().to_string(), d.to_string()))
    .collect()
}

/// (role, one-line description) for the grant/decide UI, ordered
/// least→most trusted. Banned and Guest are resolution states, not
/// grant targets, but shown so the UI can explain resolutions.
pub fn role_catalog() -> Vec<(String, String)> {
    [
        (Role::Banned, "refused everything"),
        (Role::Guest, "unverified or unknown — read-only builtins"),
        (
            Role::User,
            "verified — chat plus sandbox-scoped shell and file tools",
        ),
        (
            Role::Trusted,
            "user + memory, skills, approvals (shell still sandbox-scoped)",
        ),
        (Role::Admin, "trusted + shell, browser, cron, a2a"),
        (Role::Owner, "the principal — everything"),
    ]
    .into_iter()
    .map(|(r, d)| (r.as_str().to_string(), d.to_string()))
    .collect()
}

// ── roles ─────────────────────────────────────────────────────────────

/// Multilevel roles. Ord matters: higher = more trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Refused everything (explicit ban). Not a level — checked first.
    Banned,
    /// Unknown identity: read-only builtins only, no turns.
    #[default]
    Guest,
    /// Verified identity, no extra trust: chat + shell/file tools, all
    /// sandbox-scoped (below-admin tool calls route into the
    /// principal's own container — `user_envs.rs`).
    User,
    /// user + memory read, skills, delegate, todo, approvals (shell and
    /// file write stay sandbox-scoped).
    Trusted,
    /// trusted + everything except identity/owner state: shell, browser,
    /// computer-use, cron, a2a, session history, desktop.
    Admin,
    /// The principal. Everything, implicitly.
    Owner,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Banned => "banned",
            Role::Guest => "guest",
            Role::User => "user",
            Role::Trusted => "trusted",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "banned" | "blocked" => Role::Banned,
            "guest" | "unknown" => Role::Guest,
            "user" => Role::User,
            "trusted" | "member" => Role::Trusted,
            "admin" => Role::Admin,
            "owner" => Role::Owner,
            _ => return None,
        })
    }

    /// Default capability set for the role.
    pub fn default_caps(&self) -> BTreeSet<Cap> {
        use Cap::*;
        let mut c = BTreeSet::new();
        match self {
            Role::Banned => {}
            Role::Guest => {
                // READ-ONLY, INSIDE THE SANDBOX. USER, 2026-09-09:
                // "Guests should have read-only access and only inside
                // of the sandbox." So: they can talk, they can read and
                // search the shared box they are routed to, and that is
                // all. No Shell, no FileWrite — the two caps that would
                // let them change something. No Web either: a fetch
                // costs money and reaches outward, which is not what
                // "read-only" buys you.
                //
                // Chat is deliberate rather than incidental: a guest
                // who cannot start a turn has no use for a context
                // ceiling, and the same session set one for them
                // (150k).
                c.extend([CmdStatus, Chat, FileRead]);
            }
            Role::User => {
                // Shell + FileWrite here are SANDBOX-SCOPED by
                // construction: the tool layer routes every
                // terminal/shell/read/write/execute_code call from a
                // below-admin principal into that principal's own
                // persistent container (`user_envs.rs`) — the caps
                // never buy host access. Admin+ and owner surfaces
                // keep the host.
                //
                // MediaGen here is the point, not a slip: USER,
                // 2026-09-11, on the round that gated the `MEDIA:`
                // convention — "users were supposed to have access to
                // image-gen, that was part of the previous session."
                // Generation was admin-only because DELIVERY used to be
                // an open file channel in both directions; once the
                // marker is a capability that `media::deliverable`
                // judges per principal (24de833), a user asking for a
                // picture is a user's own money in `media/generated/`
                // and nothing else. Guests still get nothing: their
                // role is read-only by explicit directive, and a
                // generation spends real provider money.
                //
                // Tts is here for the same reason and a cheaper one —
                // USER, 2026-09-11: "users should have access to tts
                // though because it's cheap." It was trusted-only while
                // `/gen audio` already reached it (the command gated on
                // MediaGen and never re-checked), so the two doors gave
                // two answers; the fix is to level UP to what the
                // pricing actually justifies rather than to add a
                // refusal nobody wanted. VideoGen stays at trusted:
                // that one is 1200 credits a clip.
                //
                // Browser is sandbox-scoped the same way (D3,
                // PLAN-challenge-relay.md): a below-owner session's
                // `browser_*` call drives the guest browser in its own
                // box, on a node that sends its traffic over Tor, and is
                // refused anywhere else (`browsertools::caller`). USER,
                // 2026-10-13: "if they're sandboxed then do D3 obviously".
                c.extend([
                    Chat, Web, FileRead, Todo, CmdStatus, Shell, FileWrite, MediaGen, Tts, Browser,
                ]);
            }
            Role::Trusted => {
                // VideoGen starts here, not at User: the 1200-credit
                // clip (USER, 2026-09-11: "gated behind trusted+").
                c.extend([
                    Chat, Web, FileRead, Todo, CmdStatus, CmdSession, MemoryRead, Skills,
                    FileWrite, Delegate, Approve, Vision, Tts, Shell, MediaGen, VideoGen, Browser,
                ]);
            }
            Role::Admin => {
                c.extend([
                    Chat,
                    Web,
                    FileRead,
                    Todo,
                    CmdStatus,
                    CmdSession,
                    CmdAdmin,
                    MemoryRead,
                    MemoryWrite,
                    Skills,
                    SkillManage,
                    FileWrite,
                    Delegate,
                    Approve,
                    Vision,
                    Tts,
                    Shell,
                    Browser,
                    ComputerUse,
                    MediaGen,
                    VideoGen,
                    Cron,
                    A2a,
                    SessionHistory,
                    Mcp,
                ]);
            }
            Role::Owner => {
                c.insert(All);
            }
        }
        c
    }
}

// ── config shapes ─────────────────────────────────────────────────────

/// One scoped ACL rule: an identity SPEC plus where it applies.
///
/// `identity` is either an exact `platform:principal` string or an IRC
/// mask pattern (`nick!user@host`, glob/`re:` per segment — see
/// `crate::ircmask`). `network` and `channel` narrow the rule to one
/// IRC network / one channel; absent means "any". Telegram (and any
/// account-verified platform) identities carry no mask, so they match
/// by the exact string and usually leave both scopes absent.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AclEntry {
    pub identity: String,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    /// Which connector this rule applies to (`irc`, `telegram`, …).
    /// Absent = any, which is what every rule written before this
    /// field existed means.
    #[serde(default)]
    pub platform: Option<String>,
    pub role: String,
    /// Capabilities on top of the role's defaults, for THIS rule. The
    /// `extra_caps`/`deny_caps` maps are keyed on an exact identity, so
    /// a mask-matched caller could never be given or docked one; this
    /// is the same knob where a mask can reach it.
    #[serde(default)]
    pub extra_caps: Vec<String>,
    #[serde(default)]
    pub deny_caps: Vec<String>,
    /// This rule's own context ceiling in tokens, overriding the
    /// role's. 0 = unlimited.
    #[serde(default)]
    pub max_context_tokens: Option<u64>,
    /// The role a TICKET raises this identity to, one call at a time
    /// (USER 2026-10-11: "another owner … that uses our ticketing to
    /// operate so that veles can't just escalate privileges on its
    /// own"). `role` stays the STANDING role, so an owner by ticket is
    /// `role = "user"` + `ticket_role = "owner"`, and a node too old to
    /// know this key reads plain `user` — never the owner. Anything above
    /// the standing role — one tool call, or one command sent as
    /// `/sudo <command>` — runs at this role only on a ticket for exactly
    /// that call, signed on a device the gateway does not hold (the
    /// owner's signer on Telegram or IRC, or a key the owner pastes
    /// from). For an account the agent itself can post from: holding its
    /// password buys nothing above `role`. Ignored unless it names a role
    /// above `role`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_role: Option<String>,
}

/// A named USER: one human (or bot) across networks. Identities —
/// exact platform ids and/or IRC masks — all resolve to the same name,
/// and the role is attached to the NAME, so "wrath is trusted" holds
/// on libera, oftc and telegram at once.
///
/// USER (2026-08-30): "ideally all the different kinds of ids would be
/// assigned to users who could be identified across networks as well".
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AclUser {
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub identities: Vec<String>,
    #[serde(default)]
    pub extra_caps: Vec<String>,
    #[serde(default)]
    pub deny_caps: Vec<String>,
    /// This user's own context ceiling in tokens, overriding the
    /// role's. 0 = unlimited.
    #[serde(default)]
    pub max_context_tokens: Option<u64>,
    /// The role a TICKET raises this identity to, one call at a time
    /// (USER 2026-10-11: "another owner … that uses our ticketing to
    /// operate so that veles can't just escalate privileges on its
    /// own"). `role` stays the STANDING role, so an owner by ticket is
    /// `role = "user"` + `ticket_role = "owner"`, and a node too old to
    /// know this key reads plain `user` — never the owner. Anything above
    /// the standing role — one tool call, or one command sent as
    /// `/sudo <command>` — runs at this role only on a ticket for exactly
    /// that call, signed on a device the gateway does not hold (the
    /// owner's signer on Telegram or IRC, or a key the owner pastes
    /// from). For an account the agent itself can post from: holding its
    /// password buys nothing above `role`. Ignored unless it names a role
    /// above `role`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_role: Option<String>,
}

/// `[acl]` config section.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AclConfig {
    /// `owner` — identities that are the principal. First match wins the
    /// owner role; typically exactly one entry, but a list allows e.g.
    /// irc + telegram identities of the same human.
    #[serde(default)]
    pub owner: Vec<String>,
    /// Explicit role pins by identity (`irc:wrath`, `telegram:12345`).
    /// Values: owner/admin/trusted/user/guest/banned. Role pins take
    /// precedence over extra_caps.
    #[serde(default)]
    pub roles: HashMap<String, String>,
    /// Extra capabilities granted on top of the resolved role's defaults.
    /// `extra_caps."irc:somefriend" = ["shell"]`.
    #[serde(default, rename = "extra_caps")]
    pub extra_caps: HashMap<String, Vec<String>>,
    /// Caps removed from the resolved role's defaults.
    /// `deny_caps."irc:somefriend" = ["file-write"]`.
    #[serde(default, rename = "deny_caps")]
    pub deny_caps: HashMap<String, Vec<String>>,
    /// Default role for a verified-but-unlisted identity. LEGACY MODE:
    /// when NO `[acl]` config exists at all, `owner` — the adapter
    /// allowlist (IRC_ALLOWED_USERS / TELEGRAM_ALLOWED_USERS) IS the
    /// trust boundary today, so pre-ACL behavior (full access) is
    /// preserved exactly; adding any `[acl]` key enters curated mode
    /// where the default becomes `guest` (fail-closed).
    #[serde(default)]
    pub default_role: Option<String>,
    /// Scoped rules (`[[acl.entries]]`): mask patterns, per-network and
    /// per-channel roles. Checked after owner/bans, before users and
    /// exact role pins — the place for "trusted in #veles on libera",
    /// "banned from the public channel", "anyone on this server is a
    /// guest".
    #[serde(default)]
    pub entries: Vec<AclEntry>,
    /// Cross-network users (`[[acl.users]]`): named identities that
    /// share one role across networks/platforms.
    #[serde(default)]
    pub users: Vec<AclUser>,
    /// Per-role ceilings (`[acl.limits]`).
    #[serde(default)]
    pub limits: LimitsSection,
    /// `lockdown = true` — refuse every connector, serve only the
    /// local frontends (`lockdown.rs`).
    ///
    /// It lives in `[acl]` rather than a section of its own because
    /// `[acl]` is the one section that is genuinely hot-reloaded, and a
    /// brake that needs a restart to engage is not a brake. Declared
    /// HERE it cannot be lifted from any frontend, which is the point
    /// of writing it in a file instead of typing `/lockdown on`.
    #[serde(default)]
    pub lockdown: bool,
    /// `[acl.strangers]` — per-platform policy for a sender NO rule
    /// names, keyed by connector (`telegram`, `irc`, `a2a`).
    ///
    /// `default_role` answers "what may an unlisted sender do"; this
    /// answers the question before it — "may an unlisted sender speak
    /// here at all". USER, 2026-09-16, about the telegram bot:
    /// *"That channel should only really resolve me as owner at the
    /// moment, I want to add any other people I want to speak to it
    /// explicitly and ignore everyone else."*
    ///
    /// Values: `ignore` (dropped at the door — no turn, no reply, one
    /// log line), or any role name, which overrides `default_role` for
    /// that connector only. An unrecognised value reads as `ignore`:
    /// this module fails closed, and for a policy that only ever
    /// applies to strangers, closed is the harmless direction.
    ///
    /// Absent = `default_role`, i.e. exactly the behaviour before this
    /// key existed. Nothing changes for a platform not listed.
    #[serde(default)]
    pub strangers: HashMap<String, String>,
}

/// What happens to a sender no `[acl]` rule names, on one connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrangerPolicy {
    /// `default_role` decides, as it always has.
    DefaultRole,
    /// Not admitted: the gateway drops the message before it can start
    /// a turn or draw a reply.
    Ignore,
    /// This role instead of `default_role`, on this connector only.
    Role(Role),
}

/// What a caller may spend, by role.
///
/// USER, 2026-09-09: *"The maximum context length for any non-admin
/// user is 250k tokens. Anything above that ask to compact or start a
/// new session"*, and *"maximum context for guests is 150k"*.
///
/// This is NOT the compaction bar (`[agent.compaction]`), which is
/// about fitting the model's window and rewrites history on its own.
/// This is a per-caller budget: crossing it stops the turn and says so.
/// Nothing implicit — the standing directive — so the engine asks
/// rather than quietly compacting someone else's conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsSection {
    /// Ceiling for any role without its own entry below. 0 =
    /// unlimited.
    #[serde(default = "default_nonadmin_context")]
    pub default_max_context_tokens: u64,
    /// Per-role overrides, keyed by role name (`guest`, `user`,
    /// `trusted`, `admin`, `owner`). 0 = unlimited.
    #[serde(default)]
    pub by_role: HashMap<String, u64>,
    /// Tokens (prompt + completion) one caller may spend per rolling
    /// day. 0 = unlimited.
    ///
    /// The context ceiling above caps how big ONE conversation may get;
    /// it does nothing about how many conversations a caller may have.
    /// A channel of strangers each entitled to a 250k-token turn, with
    /// no limit on turns, is an unbounded bill — and the only brake
    /// that existed was IRC's per-source LINE rate, which was built to
    /// stop flooding and knows nothing about money.
    #[serde(default = "default_nonadmin_tokens_per_day")]
    pub default_max_tokens_per_day: u64,
    /// Per-role token/day overrides. 0 = unlimited.
    #[serde(default)]
    pub tokens_per_day_by_role: HashMap<String, u64>,
    /// Turns one caller may start per rolling hour. 0 = unlimited.
    ///
    /// Enforced BEFORE the provider call, unlike the token ceiling
    /// which needs a measurement to compare against: a flood should
    /// cost nothing, not cost less.
    #[serde(default = "default_nonadmin_turns_per_hour")]
    pub default_max_turns_per_hour: u64,
    /// Per-role turns/hour overrides. 0 = unlimited.
    #[serde(default)]
    pub turns_per_hour_by_role: HashMap<String, u64>,
    /// Turns one caller may have IN FLIGHT at once. 0 = unbounded.
    ///
    /// The gateway already serializes per chat, so this is about one
    /// person in several chats at once — the shape that turns a rate
    /// limit into a parallelism limit. Default 2 rather than 1: talking
    /// to the bot in two channels at once is ordinary, twenty is not.
    #[serde(default = "default_max_concurrent_turns")]
    pub max_concurrent_turns: u32,
    /// Media generations (`/gen image|audio`, and the tools behind
    /// them) one caller may start per rolling hour. 0 = unlimited.
    ///
    /// A separate budget from turns because `/gen` is answered in the
    /// gateway's COMMAND region, which sits above both turn brakes so
    /// that `/status` is never billed — correct for `/status`, and
    /// wrong for a command that makes a provider call and holds a 300s
    /// HTTP request open. Until 2026-09-11 a `user` could run them in a
    /// loop with no rate cap and no concurrency cap.
    #[serde(default = "default_media_per_hour")]
    pub default_max_media_per_hour: u64,
    /// Per-role generations/hour overrides. 0 = unlimited.
    #[serde(default)]
    pub media_per_hour_by_role: HashMap<String, u64>,
    /// Video generations per rolling hour. 0 = unlimited, which is the
    /// default: this changes no behaviour on its own.
    ///
    /// Its own budget because a clip is an order of magnitude dearer
    /// than a picture — one 5-second job cost 1200 credits on the live
    /// plan — and `video-gen` starts at `trusted`, which the media
    /// ceiling deliberately leaves uncapped. The dial exists so that is
    /// a config edit rather than a code change.
    #[serde(default)]
    pub default_max_video_per_hour: u64,
    /// Per-role clips/hour overrides. 0 = unlimited.
    #[serde(default)]
    pub video_per_hour_by_role: HashMap<String, u64>,
    /// `/tcl` evaluations per rolling hour. 0 = unlimited.
    ///
    /// Same reasoning as media — answered above the brakes, so it never
    /// consumed a turn — but a far higher default, because the whole
    /// point of the command is that it is cheap: it costs no provider
    /// call at all. This is a flood brake, not a bill.
    #[serde(default = "default_eval_per_hour")]
    pub default_max_eval_per_hour: u64,
    /// Per-role evaluations/hour overrides. 0 = unlimited.
    #[serde(default)]
    pub eval_per_hour_by_role: HashMap<String, u64>,
    /// Per-SUBJECT overrides, every budget, keyed by the caller's
    /// `budget_key()` (the `/nick`-proof identity: account id, else
    /// `ident@host`, else subject) — the same key spend and turns are
    /// counted against, so a cap named here cannot be dodged by a
    /// rename. The literal `subject` is checked first, so `irc:xenia`
    /// and `~a@host` spellings both land.
    ///
    /// A subject entry beats the role ceiling for that budget; a budget
    /// a subject leaves unset falls through to the role. This is the
    /// answer to "a is one user, not the whole `user` rung": the role
    /// stays the default, the person gets their own number.
    #[serde(default)]
    pub by_subject: HashMap<String, SubjectBudgets>,
}

/// The budgets one named subject may override. Every field is optional:
/// an absent budget keeps the role's ceiling, a present one replaces
/// it. 0 = unlimited, same as the per-role tables.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubjectBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_day: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns_per_hour: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_per_hour: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_per_hour: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_per_hour: Option<u64>,
}

fn default_nonadmin_context() -> u64 {
    250_000
}

fn default_guest_context() -> u64 {
    150_000
}

/// Roughly eight full 250k conversations a day for a known user —
/// generous for anyone using the bot, bounded for anyone abusing it.
fn default_nonadmin_tokens_per_day() -> u64 {
    2_000_000
}

/// A guest is the cheapest rung: about one full context's worth a day.
fn default_guest_tokens_per_day() -> u64 {
    200_000
}

/// One a minute, sustained.
fn default_nonadmin_turns_per_hour() -> u64 {
    60
}

fn default_guest_turns_per_hour() -> u64 {
    20
}

fn default_max_concurrent_turns() -> u32 {
    2
}

/// USER, 2026-09-11: *"images are also super cheap so maybe set the rate
/// limit per hour caps at 40 user and unlimited above."* Forty an hour is
/// past any honest use of a picture command and short of a loop; trusted
/// and above are uncapped, which `LimitsSection::resolve` already does
/// for admin/owner and which the per-role table below does for trusted.
fn default_media_per_hour() -> u64 {
    40
}

/// Two a minute, sustained. `/tcl` costs no provider call, so this is a
/// flood brake rather than a budget — high enough that a person never
/// meets it and low enough that a script does.
fn default_eval_per_hour() -> u64 {
    120
}

impl Default for LimitsSection {
    fn default() -> Self {
        Self {
            default_max_context_tokens: default_nonadmin_context(),
            by_role: HashMap::new(),
            default_max_tokens_per_day: default_nonadmin_tokens_per_day(),
            tokens_per_day_by_role: HashMap::new(),
            default_max_turns_per_hour: default_nonadmin_turns_per_hour(),
            turns_per_hour_by_role: HashMap::new(),
            max_concurrent_turns: default_max_concurrent_turns(),
            default_max_media_per_hour: default_media_per_hour(),
            media_per_hour_by_role: HashMap::new(),
            default_max_video_per_hour: 0,
            video_per_hour_by_role: HashMap::new(),
            default_max_eval_per_hour: default_eval_per_hour(),
            eval_per_hour_by_role: HashMap::new(),
            by_subject: HashMap::new(),
        }
    }
}

impl LimitsSection {
    /// One resolution rule, three budgets: an explicit per-role entry
    /// wins, else admin/owner are uncapped and banned is moot, else the
    /// rung's own built-in default, else the section default.
    ///
    /// Written once because three copies of a precedence chain is three
    /// chances for one of them to disagree about whether admin is
    /// exempt.
    fn resolve(
        &self,
        role: Role,
        by_role: &HashMap<String, u64>,
        guest_default: u64,
        section_default: u64,
    ) -> u64 {
        if let Some(n) = by_role.get(role.as_str()) {
            return *n;
        }
        match role {
            // Admin and owner run the thing; they are not on a budget.
            Role::Admin | Role::Owner => 0,
            Role::Guest => guest_default,
            // Banned never gets a turn at all; the number is moot.
            Role::Banned => 0,
            _ => section_default,
        }
    }

    /// The `SubjectBudgets` naming this caller, if any. The literal
    /// `subject` is tried first, then `budget_key` — a config may spell
    /// either form, and on a services-less network the two differ (the
    /// budget key drops the nick), so both have to be consulted or a cap
    /// spelled one way would miss a caller keyed the other.
    ///
    /// Compared case-insensitively: IRC casefolds hosts and idents, and
    /// the wire mask preserves whatever case the server sent, so a cap
    /// keyed `~a@86b64c8e:…` must still land on a caller whose budget
    /// key arrived as `~a@86B64C8E:…`. A case-sensitive map read would
    /// make the config spelling and the wire casing a silent mismatch.
    fn subject_budgets(&self, subject: &str, budget_key: &str) -> Option<&SubjectBudgets> {
        self.by_subject
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(subject) || k.eq_ignore_ascii_case(budget_key))
            .map(|(_, v)| v)
    }

    /// The context ceiling for one caller. 0 = unlimited.
    pub fn for_subject(&self, role: Role, subject: &str, budget_key: &str) -> u64 {
        if let Some(n) = self
            .subject_budgets(subject, budget_key)
            .and_then(|s| s.context)
        {
            return n;
        }
        self.for_role(role)
    }

    /// The tokens-per-day ceiling for one caller. 0 = unlimited.
    pub fn tokens_per_day_for_subject(&self, role: Role, subject: &str, budget_key: &str) -> u64 {
        if let Some(n) = self
            .subject_budgets(subject, budget_key)
            .and_then(|s| s.tokens_per_day)
        {
            return n;
        }
        self.tokens_per_day_for_role(role)
    }

    /// The turns-per-hour ceiling for one caller. 0 = unlimited.
    pub fn turns_per_hour_for_subject(&self, role: Role, subject: &str, budget_key: &str) -> u64 {
        if let Some(n) = self
            .subject_budgets(subject, budget_key)
            .and_then(|s| s.turns_per_hour)
        {
            return n;
        }
        self.turns_per_hour_for_role(role)
    }

    /// Media generations/hour for one caller. 0 = unlimited.
    pub fn media_per_hour_for_subject(&self, role: Role, subject: &str, budget_key: &str) -> u64 {
        if let Some(n) = self
            .subject_budgets(subject, budget_key)
            .and_then(|s| s.media_per_hour)
        {
            return n;
        }
        self.media_per_hour_for_role(role)
    }

    /// Clips/hour for one caller. 0 = unlimited.
    pub fn video_per_hour_for_subject(&self, role: Role, subject: &str, budget_key: &str) -> u64 {
        if let Some(n) = self
            .subject_budgets(subject, budget_key)
            .and_then(|s| s.video_per_hour)
        {
            return n;
        }
        self.video_per_hour_for_role(role)
    }

    /// `/tcl` evaluations/hour for one caller. 0 = unlimited.
    pub fn eval_per_hour_for_subject(&self, role: Role, subject: &str, budget_key: &str) -> u64 {
        if let Some(n) = self
            .subject_budgets(subject, budget_key)
            .and_then(|s| s.eval_per_hour)
        {
            return n;
        }
        self.eval_per_hour_for_role(role)
    }

    /// The context ceiling for `role`. 0 = unlimited.
    pub fn for_role(&self, role: Role) -> u64 {
        self.resolve(
            role,
            &self.by_role,
            default_guest_context(),
            self.default_max_context_tokens,
        )
    }

    /// The tokens-per-day ceiling for `role`. 0 = unlimited.
    pub fn tokens_per_day_for_role(&self, role: Role) -> u64 {
        self.resolve(
            role,
            &self.tokens_per_day_by_role,
            default_guest_tokens_per_day(),
            self.default_max_tokens_per_day,
        )
    }

    /// The turns-per-hour ceiling for `role`. 0 = unlimited.
    pub fn turns_per_hour_for_role(&self, role: Role) -> u64 {
        self.resolve(
            role,
            &self.turns_per_hour_by_role,
            default_guest_turns_per_hour(),
            self.default_max_turns_per_hour,
        )
    }

    /// Media generations per hour for `role`. 0 = unlimited.
    ///
    /// Deliberately NOT `resolve`: that helper makes admin and owner
    /// exempt and puts everyone else on the section default, and USER
    /// drew the line one rung lower — *"40 user and unlimited above"*
    /// (2026-09-11). A guest stays on the user number rather than on
    /// "unlimited", because a guest reaches this only if someone
    /// explicitly granted them `media-gen`, and 0 would then read as a
    /// blank cheque.
    pub fn media_per_hour_for_role(&self, role: Role) -> u64 {
        if let Some(n) = self.media_per_hour_by_role.get(role.as_str()) {
            return *n;
        }
        match role {
            Role::Trusted | Role::Admin | Role::Owner => 0,
            _ => self.default_max_media_per_hour,
        }
    }

    /// Clips per hour for `role`. 0 = unlimited, and that is the
    /// default for every role — the knob exists so capping the
    /// expensive one is a config edit, not a code change.
    pub fn video_per_hour_for_role(&self, role: Role) -> u64 {
        if let Some(n) = self.video_per_hour_by_role.get(role.as_str()) {
            return *n;
        }
        self.default_max_video_per_hour
    }

    /// `/tcl` evaluations per hour for `role`. 0 = unlimited.
    ///
    /// This one DOES use `resolve`: an evaluation costs no provider
    /// call, so the brake is about flooding a channel rather than about
    /// money, and the ordinary "admin and owner run the thing" exemption
    /// is the right shape for it.
    pub fn eval_per_hour_for_role(&self, role: Role) -> u64 {
        self.resolve(
            role,
            &self.eval_per_hour_by_role,
            default_eval_per_hour(),
            self.default_max_eval_per_hour,
        )
    }

    /// In-flight turns allowed for `role`. 0 = unbounded; admin and
    /// owner are never held.
    pub fn concurrent_turns_for_role(&self, role: Role) -> u32 {
        match role {
            Role::Admin | Role::Owner => 0,
            _ => self.max_concurrent_turns,
        }
    }
}

/// Whether any meaningful acl config is present (drives the legacy
/// single-user default vs curated fail-closed default).
impl AclConfig {
    pub fn is_configured(&self) -> bool {
        !self.owner.is_empty()
            || !self.roles.is_empty()
            || !self.extra_caps.is_empty()
            || !self.deny_caps.is_empty()
            || self.default_role.is_some()
            || !self.entries.is_empty()
            || !self.users.is_empty()
            // A door policy IS an access-control declaration, and a
            // `[acl.strangers]`-only config that left the ACL dormant
            // would resolve every sender as OWNER — the exact inversion
            // of what writing it down asks for.
            || !self.strangers.is_empty()
        // Deliberately NOT limits: tightening a budget must not be
        // the thing that switches the whole ACL from dormant to
        // curated and locks the operator out of their own bot.
    }

    fn effective_default_role(&self) -> Role {
        if let Some(r) = self.default_role.as_deref().and_then(Role::parse) {
            return r;
        }
        if self.is_configured() {
            Role::Guest
        } else {
            // Legacy single-user mode: the adapter allowlist already
            // vetted this sender; pre-ACL behavior was full access.
            Role::Owner
        }
    }

    /// The door policy for one connector.
    ///
    /// Platform keys are matched case-insensitively, because
    /// `SenderContext::platform` is whatever the adapter spelled and a
    /// config that silently matches nothing is the failure this whole
    /// section exists to avoid.
    pub fn strangers_on(&self, platform: Option<&str>) -> StrangerPolicy {
        let Some(plat) = platform else {
            return StrangerPolicy::DefaultRole;
        };
        let Some((_, v)) = self
            .strangers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(plat))
        else {
            return StrangerPolicy::DefaultRole;
        };
        match v.trim().to_ascii_lowercase().as_str() {
            "ignore" | "drop" | "silence" => StrangerPolicy::Ignore,
            other => match Role::parse(other) {
                Some(r) => StrangerPolicy::Role(r),
                // Fail closed, and say so: a typo here must not quietly
                // widen the door back to `default_role`.
                None => {
                    tracing::warn!(
                        "acl: [acl.strangers] {plat} = {v:?} is not \"ignore\" or a role name — \
                         reading it as \"ignore\" (fail-closed)"
                    );
                    StrangerPolicy::Ignore
                }
            },
        }
    }

    /// Nothing in the ACL names this caller: they would fall through to
    /// `default_role`.
    ///
    /// This is the question [`StrangerPolicy`] turns on, and it is
    /// deliberately about RULES rather than about the resolved role — a
    /// sender explicitly pinned to `guest` has been thought about, and
    /// is not a stranger.
    pub fn names_no_rule(&self, id: Option<&str>, ctx: &SenderContext) -> bool {
        matching_rules(id.map(normalize_id).as_deref(), ctx, self).is_empty()
    }
}

// ── resolved identity ─────────────────────────────────────────────────

pub use veles_types::SenderContext;

/// A fully resolved caller: role + effective caps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Principal {
    /// The platform's own verified id (`irc:wrath` from a Services
    /// account, `telegram:12345`), or None when the platform has no
    /// such thing — which on a services-less IRC network is EVERYONE,
    /// and is the normal case rather than a degraded one.
    ///
    /// This is the matching identity: an exact `[acl]` spec compares
    /// against it. For KEYING — sessions, sandboxes, ask binding — use
    /// [`Principal::subject`], which is always populated.
    pub id: Option<String>,
    /// Who this is, for everything that has to tell two callers apart.
    ///
    /// Resolution order: the `[[acl.users]]` name that matched
    /// (`user:<name>`, so one human is one subject across networks and
    /// platforms) → the verified `id` → the observed wire mask
    /// (`irc:libera/nick!ident@host`) → `anon:<chat>` when the
    /// platform told us nothing at all.
    ///
    /// It is deliberately NOT stuffed into `id`: a mask-shaped id
    /// contains `!` and `@`, so pasting it into a config rule would be
    /// read as a mask PATTERN and match nothing. Keeping them separate
    /// keeps `/acl`'s pasteable output honest.
    pub subject: String,
    pub role: Role,
    pub caps: BTreeSet<Cap>,
    /// The winning rule's own context ceiling, when it set one. None =
    /// use the role's (`AclConfig::limits`).
    pub rule_context_limit: Option<u64>,
    /// The wire mask the adapter observed, when there was one. Kept
    /// because it is the PASTEABLE identity on a services-less network:
    /// `/acl` prints it as the spec to put in a rule, and the label
    /// names it rather than calling the normal case "unverified".
    pub mask: Option<String>,
    /// How the role was decided, when it came from somewhere other than
    /// an exact identity pin — `user wrath`, `entry *!*@gateway/veles`,
    /// `grant …`. Displayed by /acl so a shared channel can see WHY a
    /// stranger has the role they have.
    pub via: Option<String>,
    /// The role this caller holds BY TICKET (`ticket_role` in `[acl]`):
    /// `role` and `caps` are its standing level, and one call at a time
    /// runs at this role on a ticket for exactly that call
    /// ([`Principal::raised`]). None for everyone else. Skipped when
    /// None, so a principal carried to an older node reads as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_role: Option<Role>,
}

// ── rule collection and precedence ───────────────────────────────────

/// Where a matched rule came from. Only a tie-break — specificity
/// decides first — but a visible one: `/acl explain` names it, and the
/// rank encodes "a config file is a more deliberate statement than a
/// live grant".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSource {
    /// `[acl] owner = [...]`
    OwnerList,
    /// `[acl.roles]` exact pin
    RolePin,
    /// `[[acl.entries]]`
    Entry,
    /// `[[acl.users]]` in config
    User,
    /// a runtime user, saved from a frontend
    RuntimeUser,
    /// a runtime grant (pairing code, `.agent grant`, the desktop UI)
    Grant,
}

impl RuleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RuleSource::OwnerList => "owner list",
            RuleSource::RolePin => "role pin",
            RuleSource::Entry => "entry",
            RuleSource::User => "user",
            RuleSource::RuntimeUser => "runtime user",
            RuleSource::Grant => "grant",
        }
    }
    /// Higher wins a tie. Config before runtime.
    fn rank(self) -> u8 {
        match self {
            RuleSource::OwnerList => 5,
            RuleSource::RolePin => 4,
            RuleSource::Entry => 3,
            RuleSource::User => 2,
            RuleSource::Grant => 1,
            RuleSource::RuntimeUser => 0,
        }
    }
}

/// One rule that matches a caller, with everything needed to order it
/// against the others and to explain the outcome afterwards.
#[derive(Debug, Clone)]
pub struct MatchedRule {
    pub role: Role,
    /// The spec text that matched — pasteable back into config.
    pub identity: String,
    pub source: RuleSource,
    /// True when `identity` is an exact `platform:principal` rather
    /// than a mask pattern.
    pub exact_id: bool,
    pub mask_spec: crate::ircmask::MaskSpecificity,
    pub network: Option<String>,
    pub channel: Option<String>,
    pub platform: Option<String>,
    /// The `[[acl.users]]` name, when the rule came from one.
    pub user_name: Option<String>,
    pub via: Option<String>,
    pub extra: Vec<Cap>,
    pub deny: Vec<Cap>,
    pub max_context_tokens: Option<u64>,
    /// The role a ticket raises this rule's caller to (`ticket_role`),
    /// when it names one.
    pub ticket_role: Option<Role>,
}

impl MatchedRule {
    /// One line for `/acl explain`.
    pub fn describe(&self) -> String {
        let mut scope = Vec::new();
        if let Some(p) = &self.platform {
            scope.push(format!("platform {p}"));
        }
        if let Some(n) = &self.network {
            scope.push(format!("network {n}"));
        }
        if let Some(c) = &self.channel {
            scope.push(format!("channel {c}"));
        }
        let scope = if scope.is_empty() {
            "anywhere".to_string()
        } else {
            scope.join(", ")
        };
        let kind = if self.exact_id {
            "exact identity".to_string()
        } else {
            format!(
                "mask h{} i{} n{}",
                self.mask_spec.host, self.mask_spec.ident, self.mask_spec.nick
            )
        };
        // The identity in BACKTICKS. It is the pasteable half of the
        // answer, and a mask is full of markdown metacharacters: the
        // IRC adapter renders `*!*@host` as an italic run
        // (`\x1d!\x1d@host` on the wire), so an operator copying the
        // mask out of `/acl explain` copied formatting codes instead.
        // Inline code spans are protected byte-exact before any
        // conversion runs (`veles-irc/src/format.rs`), and on a
        // plain-text surface the backticks read as the convention they
        // are.
        let role = match self.ticket_role {
            Some(t) if t > self.role => format!("{}, {} by ticket", self.role.as_str(), t.as_str()),
            _ => self.role.as_str().to_string(),
        };
        format!(
            "`{}` → {} [{} · {} · {}]",
            self.identity,
            role,
            self.source.as_str(),
            kind,
            scope
        )
    }
}

/// The sort key. Compared as a tuple, so the fields ARE the precedence
/// documented on `resolve_ctx`: exact identity, then mask specificity,
/// then scope, then (Reverse) role so a tie goes to the lower one, then
/// source rank.
fn precedence_key(
    r: &MatchedRule,
) -> (
    bool,
    crate::ircmask::MaskSpecificity,
    u8,
    std::cmp::Reverse<Role>,
    u8,
) {
    let scope = (if r.channel.is_some() { 4 } else { 0 })
        + (if r.network.is_some() { 2 } else { 0 })
        + (if r.platform.is_some() { 1 } else { 0 });
    (
        r.exact_id,
        r.mask_spec,
        scope,
        std::cmp::Reverse(r.role),
        r.source.rank(),
    )
}

fn pick(rules: &[MatchedRule]) -> Option<&MatchedRule> {
    rules.iter().max_by_key(|r| precedence_key(r))
}

// ── comparing two rules before any caller exists ─────────────────────
//
// `/acl promote` has to answer "would the line I am about to write
// decide anything?" with nobody to resolve. The answer has to come out
// of the SAME comparison resolution runs — `precedence_key`, not a
// retelling of it — because a retelling drifts the first time the order
// changes, and that is exactly what `/acl list` did when it announced
// that the file wins ties: true of the last resort, false of the four
// comparisons before it, and printed as the verdict on all five.
//
// Two halves, and only one of them is about precedence. Which rule wins
// is the tuple; whether the loser still decides something depends on
// which CALLERS the two can meet, and a rule that loses for everybody it
// can reach is a duplicate while one that loses inside a narrower rule
// and wins outside it is not.

/// How the callers two rules can match relate: `ircmask`'s relation
/// over pattern texts, extended here with scope, because a rule pinned
/// to one channel reaches a subset of the same identity's callers.
pub type CallerSet = crate::ircmask::PatternRelation;

/// A rule described by its parts, with no caller attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSpec {
    /// An exact `platform:principal` identity, or a `nick!user@host`
    /// pattern.
    pub identity: String,
    pub role: Role,
    pub network: Option<String>,
    pub channel: Option<String>,
    pub platform: Option<String>,
    pub source: RuleSource,
}

impl RuleSpec {
    /// An unscoped rule, the shape a runtime grant or an owner line has.
    pub fn new(identity: &str, role: Role, source: RuleSource) -> Self {
        RuleSpec {
            identity: identity.trim().to_string(),
            role,
            network: None,
            channel: None,
            platform: None,
            source,
        }
    }

    /// The same rule narrowed to one network and/or channel.
    pub fn scoped(mut self, network: Option<&str>, channel: Option<&str>) -> Self {
        self.network = network
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.channel = channel
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self
    }
}

/// The precedence field that decided an order between two rules. Named
/// so the sentence an operator reads can say WHICH of the five
/// comparisons did the work instead of asserting an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// One is an exact identity and the other a mask: exact wins first.
    ExactIdentity,
    /// One mask pins more text than the other, segment by segment.
    MaskSpecificity,
    /// One is scoped narrower (channel, then network, then platform).
    Scope,
    /// Everything but the role is equal, and the LOWER role wins the tie.
    LowerRole,
    /// Indistinguishable but for where each was declared: config before
    /// the store, owners before pins before entries before users.
    Source,
    /// The comparison cannot tell them apart at all.
    Nothing,
}

impl Edge {
    pub fn as_str(self) -> &'static str {
        match self {
            Edge::ExactIdentity => "an exact identity beats a mask",
            Edge::MaskSpecificity => "the more specific mask",
            Edge::Scope => "the narrower scope",
            Edge::LowerRole => "the lower role, which wins a tie",
            Edge::Source => "where it was declared — config before the store",
            Edge::Nothing => "nothing: the comparison cannot tell them apart",
        }
    }
}

/// Two rules compared, or the fact that they cannot meet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleRelation {
    /// No caller can match both, so the order between them decides
    /// nothing and neither one is redundant.
    Disjoint,
    Meets {
        callers: CallerSet,
        /// `Greater` when the FIRST rule wins the comparison.
        order: std::cmp::Ordering,
        edge: Edge,
    },
}

/// What a spec text is, for the comparison: an exact identity, or a
/// mask that parsed. `None` is a mask that did not, which matches
/// nothing — the same fail-closed reading `spec_match` gives it, so a
/// malformed rule is disjoint from every caller rather than comparable
/// to one.
enum SpecShape {
    Exact,
    Mask(crate::ircmask::MaskPattern),
}

fn spec_shape(identity: &str) -> Option<SpecShape> {
    let id = identity.trim();
    if id.is_empty() {
        return None;
    }
    if crate::ircmask::looks_like_mask(id) {
        Some(SpecShape::Mask(
            crate::ircmask::MaskPattern::parse(id).ok()?,
        ))
    } else {
        Some(SpecShape::Exact)
    }
}

/// Compare two rules the way resolution would, if one caller matched
/// both.
pub fn compare_specs(a: &RuleSpec, b: &RuleSpec) -> RuleRelation {
    let (Some(sa), Some(sb)) = (spec_shape(&a.identity), spec_shape(&b.identity)) else {
        return RuleRelation::Disjoint;
    };
    let callers = caller_sets(a, b, &sa, &sb);
    if callers == CallerSet::Disjoint {
        return RuleRelation::Disjoint;
    }
    let (ka, kb) = (
        precedence_key(&matched(a, &sa)),
        precedence_key(&matched(b, &sb)),
    );
    let edge = if ka.0 != kb.0 {
        Edge::ExactIdentity
    } else if ka.1 != kb.1 {
        Edge::MaskSpecificity
    } else if ka.2 != kb.2 {
        Edge::Scope
    } else if ka.3 != kb.3 {
        Edge::LowerRole
    } else if ka.4 != kb.4 {
        Edge::Source
    } else {
        Edge::Nothing
    };
    RuleRelation::Meets {
        callers,
        order: ka.cmp(&kb),
        edge,
    }
}

/// The rule as the comparison sees it. The fields `precedence_key` does
/// not read are empty, which is the point: this is a comparison, not a
/// resolution, and nothing here gets to invent caps.
fn matched(s: &RuleSpec, shape: &SpecShape) -> MatchedRule {
    MatchedRule {
        role: s.role,
        identity: s.identity.clone(),
        source: s.source,
        exact_id: matches!(shape, SpecShape::Exact),
        mask_spec: match shape {
            SpecShape::Mask(p) => p.specificity(),
            SpecShape::Exact => Default::default(),
        },
        network: s.network.clone(),
        channel: s.channel.clone(),
        platform: s.platform.clone(),
        user_name: None,
        via: None,
        extra: Vec::new(),
        deny: Vec::new(),
        max_context_tokens: None,
        ticket_role: None,
    }
}

fn caller_sets(a: &RuleSpec, b: &RuleSpec, sa: &SpecShape, sb: &SpecShape) -> CallerSet {
    use crate::ircmask::PatternRelation;
    let identities = match (sa, sb) {
        (SpecShape::Exact, SpecShape::Exact) => {
            if normalize_id(&a.identity) == normalize_id(&b.identity) {
                PatternRelation::Equal
            } else {
                PatternRelation::Disjoint
            }
        }
        (SpecShape::Mask(_), SpecShape::Mask(_)) => {
            crate::ircmask::pattern_relation(&a.identity, &b.identity)
        }
        // An exact identity and a mask meet only on a caller carrying
        // both — a verified IRC account has an id AND a mask. Which
        // callers those are is not knowable from two texts, so nothing
        // is claimed rather than something invented.
        _ => PatternRelation::Unknown,
    };
    crate::ircmask::combine_relations(&[identities, scope_relation(a, b)])
}

/// Scope is a containment relation too: pinning a network or a channel
/// narrows which callers the rule can meet, and two different networks
/// cannot meet at all.
fn scope_relation(a: &RuleSpec, b: &RuleSpec) -> CallerSet {
    crate::ircmask::combine_relations(&[
        field_relation(a.network.as_deref(), b.network.as_deref()),
        field_relation(a.channel.as_deref(), b.channel.as_deref()),
        field_relation(a.platform.as_deref(), b.platform.as_deref()),
    ])
}

fn field_relation(a: Option<&str>, b: Option<&str>) -> CallerSet {
    use crate::ircmask::PatternRelation as R;
    match (a.map(str::trim), b.map(str::trim)) {
        (None, None) => R::Equal,
        (Some(_), None) => R::AWithinB,
        (None, Some(_)) => R::BWithinA,
        (Some(x), Some(y)) if x.eq_ignore_ascii_case(y) => R::Equal,
        // Two different networks, or two different channels: no caller
        // is in both.
        _ => R::Disjoint,
    }
}

/// Does this spec describe the caller, and if so how specifically?
fn spec_match(
    spec: &str,
    id: Option<&str>,
    ctx: &SenderContext,
) -> Option<(bool, crate::ircmask::MaskSpecificity)> {
    // An account identity may pin the network it is valid on with an
    // `@<network>` suffix: `irc:xenia@irc.synirc.net` matches the
    // xenia account on synirc ONLY, so the owner need not register the
    // same nick on every network the bot joins. A bare `irc:xenia`
    // stays network-agnostic (the pre-suffix behaviour).
    //
    // The discriminator is the `platform:` shape, NOT `looks_like_mask`
    // (which the `@` would trip): a hostmask is `nick!ident@host` or
    // `*@host`, neither of which carries a `scheme:` prefix before the
    // `@`. `account` is the part before the FIRST `@`, and a hostmask
    // never has a `:` there, so `xenia!~h@host` and `*@host` fall
    // through to the mask branch below while `irc:xenia@net` is an
    // account id with a network pin.
    if let Some((account, network)) = spec.split_once('@') {
        if account.contains(':') && !account.contains('!') {
            let on_net = ctx
                .network
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(network));
            if !on_net {
                return None;
            }
            return if id.is_some_and(|id| normalize_id(id) == normalize_id(account)) {
                Some((true, Default::default()))
            } else {
                None
            };
        }
    }
    if crate::ircmask::looks_like_mask(spec) {
        let mask = ctx.mask.as_deref()?;
        match crate::ircmask::MaskPattern::parse(spec) {
            Ok(p) if p.matches(mask) => Some((false, p.specificity())),
            // An unparseable pattern matches NOTHING — fail closed,
            // like everything else here.
            _ => None,
        }
    } else if id.is_some_and(|id| normalize_id(id) == normalize_id(spec)) {
        Some((true, Default::default()))
    } else {
        None
    }
}

/// Every rule in every source that matches this caller.
fn matching_rules(id: Option<&str>, ctx: &SenderContext, cfg: &AclConfig) -> Vec<MatchedRule> {
    let mut out: Vec<MatchedRule> = Vec::new();

    let mut push = |spec: &str,
                    role: Role,
                    source: RuleSource,
                    network: Option<String>,
                    channel: Option<String>,
                    platform: Option<String>,
                    user_name: Option<String>,
                    via: Option<String>,
                    extra: Vec<Cap>,
                    deny: Vec<Cap>,
                    max_context_tokens: Option<u64>,
                    ticket_role: Option<Role>| {
        if !scope_matches_ctx(
            network.as_deref(),
            channel.as_deref(),
            platform.as_deref(),
            ctx,
        ) {
            return;
        }
        if let Some((exact_id, mask_spec)) = spec_match(spec, id, ctx) {
            out.push(MatchedRule {
                role,
                identity: spec.to_string(),
                source,
                exact_id,
                mask_spec,
                network,
                channel,
                platform,
                user_name,
                via,
                extra,
                deny,
                max_context_tokens,
                ticket_role,
            });
        }
    };

    // `[acl] owner = [...]` — exact identities, no scope.
    for o in &cfg.owner {
        push(
            o,
            Role::Owner,
            RuleSource::OwnerList,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
        );
    }
    // `[acl.roles]` — exact pins.
    for (spec, role) in &cfg.roles {
        if let Some(role) = Role::parse(role) {
            push(
                spec,
                role,
                RuleSource::RolePin,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                None,
                None,
            );
        }
    }
    // `[[acl.entries]]` — masks and scopes.
    for e in &cfg.entries {
        if let Some(role) = Role::parse(&e.role) {
            push(
                &e.identity,
                role,
                RuleSource::Entry,
                e.network.clone(),
                e.channel.clone(),
                e.platform.clone(),
                None,
                Some(format!("entry {}", e.identity)),
                caps_list(&e.extra_caps),
                caps_list(&e.deny_caps),
                e.max_context_tokens,
                e.ticket_role.as_deref().and_then(Role::parse),
            );
        }
    }
    // `[[acl.users]]` — config, then runtime.
    let runtime_users = runtime_pairing_store()
        .map(|s| crate::pairing::list_users(&s))
        .unwrap_or_default();
    for (u, source) in cfg
        .users
        .iter()
        .map(|u| (u, RuleSource::User))
        .chain(runtime_users.iter().map(|u| (u, RuleSource::RuntimeUser)))
    {
        if let Some(role) = Role::parse(&u.role) {
            for spec in &u.identities {
                push(
                    spec,
                    role,
                    source,
                    None,
                    None,
                    None,
                    Some(u.name.clone()),
                    Some(format!("user {}", u.name)),
                    caps_list(&u.extra_caps),
                    caps_list(&u.deny_caps),
                    u.max_context_tokens,
                    u.ticket_role.as_deref().and_then(Role::parse),
                );
            }
        }
    }
    // Runtime grants — scoped, mask-aware, live.
    let grants = runtime_pairing_store()
        .map(|s| crate::pairing::list_grants(&s))
        .unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for g in &grants {
        if g.expires_at != 0 && g.expires_at < now {
            continue;
        }
        if let Some(role) = Role::parse(&g.role) {
            push(
                &g.identity,
                role,
                RuleSource::Grant,
                g.network.clone(),
                g.channel.clone(),
                None,
                None,
                Some(format!("grant {}", g.identity)),
                Vec::new(),
                Vec::new(),
                None,
                None,
            );
        }
    }
    out
}

fn caps_list(raw: &[String]) -> Vec<Cap> {
    raw.iter().filter_map(|s| Cap::from_str_lossy(s)).collect()
}

/// Scope check for one rule: an absent scope matches everywhere; a
/// present one must equal the observation (case-insensitively — IRC
/// casefolding applies to networks and channels alike).
fn scope_matches_ctx(
    network: Option<&str>,
    channel: Option<&str>,
    platform: Option<&str>,
    ctx: &SenderContext,
) -> bool {
    let eq = |want: &str, got: Option<&str>| match got {
        Some(g) => g.eq_ignore_ascii_case(want),
        None => false,
    };
    if let Some(want) = network {
        if !eq(want, ctx.network.as_deref()) {
            return false;
        }
    }
    if let Some(want) = channel {
        // `channel` is the IRC spelling; `chat_id` is the general one.
        // A rule naming a channel matches either, so one rule works
        // for an IRC channel and for another connector's room id.
        if !eq(want, ctx.channel.as_deref()) && !eq(want, ctx.chat_id.as_deref()) {
            return false;
        }
    }
    if let Some(want) = platform {
        if !eq(want, ctx.platform.as_deref()) {
            return false;
        }
    }
    true
}

/// The keying identity for a caller the platform could not name at all.
/// Scoped to the conversation, so two anonymous callers in different
/// chats are still different subjects — and two in the SAME chat share
/// one, which is the honest answer when nothing distinguishes them.
fn anon_subject(ctx: &SenderContext) -> String {
    let where_ = ctx
        .chat_id
        .as_deref()
        .or(ctx.channel.as_deref())
        .unwrap_or("unknown");
    format!("anon:{}", where_.to_ascii_lowercase())
}

/// The keying identity from a wire mask: platform, network and the
/// whole `nick!ident@host`. Not a config spec — see `Principal::subject`.
fn mask_subject(ctx: &SenderContext, mask: &str) -> String {
    let platform = ctx.platform.as_deref().unwrap_or("mask");
    match ctx.network.as_deref() {
        Some(net) => format!(
            "{platform}:{}/{}",
            net.to_ascii_lowercase(),
            mask.to_ascii_lowercase()
        ),
        None => format!("{platform}:{}", mask.to_ascii_lowercase()),
    }
}

/// `subject` for a caller, given whatever the adapter saw and the name
/// of the `[[acl.users]]` entry that matched (when one did).
fn subject_for(id: Option<&str>, ctx: &SenderContext, user_name: Option<&str>) -> String {
    if let Some(name) = user_name {
        return format!("user:{}", normalize_id(name));
    }
    if let Some(id) = id {
        return normalize_id(id);
    }
    match ctx.mask.as_deref() {
        Some(mask) => mask_subject(ctx, mask),
        None => anon_subject(ctx),
    }
}

impl Principal {
    /// The owner principal used by owner surfaces (CLI/TUI/desktop).
    pub fn owner() -> Self {
        Self {
            id: Some("local:owner".into()),
            subject: "local:owner".into(),
            rule_context_limit: None,
            mask: None,
            role: Role::Owner,
            caps: Role::Owner.default_caps(),
            via: None,
            ticket_role: None,
        }
    }

    /// A principal whose identity and role are already known, with the
    /// role's default capabilities: an A2A peer, a surface that
    /// resolved elsewhere, a test. `subject` follows `id`, which is
    /// what an exact identity means.
    pub fn known(id: &str, role: Role) -> Self {
        Self {
            id: Some(normalize_id(id)),
            subject: normalize_id(id),
            role,
            caps: role.default_caps(),
            rule_context_limit: None,
            mask: None,
            via: None,
            ticket_role: None,
        }
    }

    /// Same, minus one capability — for the "has the role but not THIS"
    /// case (`deny_caps` in config, and the tests that pin it).
    pub fn without(mut self, cap: Cap) -> Self {
        self.caps.remove(&cap);
        self
    }

    /// Unverified caller: guest floor.
    pub fn guest() -> Self {
        Self {
            id: None,
            subject: "anon:unknown".into(),
            rule_context_limit: None,
            mask: None,
            role: Role::Guest,
            caps: Role::Guest.default_caps(),
            via: None,
            ticket_role: None,
        }
    }

    /// Resolve a sender against config, ignoring any observed
    /// mask/network/channel. The backwards-compatible entry point;
    /// gateway paths call [`Principal::resolve_ctx`] with what the
    /// adapter saw.
    pub fn resolve(id: Option<&str>, cfg: &AclConfig) -> Self {
        Self::resolve_ctx(id, &SenderContext::default(), cfg)
    }

    /// Resolve a sender against config, with the adapter's observations.
    ///
    /// # The most specific rule wins
    ///
    /// USER, 2026-09-09: *"The ACLs should honour the most specific mask
    /// recognised, regardless of order of declaration."* So this is not
    /// a walk down a list of categories any more — it COLLECTS every
    /// rule that matches this caller, from every source, and picks the
    /// narrowest. Declaration order is not consulted at all.
    ///
    /// The precedence key, in order:
    ///
    /// 1. **an exact identity beats every mask.** `irc:wrath` names a
    ///    verified account; `wrath!*@*` names a pattern that a client
    ///    chose. (Consequence, deliberate: an exact `roles` pin
    ///    outranks a broad mask ban. To ban an account, pin the
    ///    account.)
    /// 2. **mask specificity** — `ircmask::MaskSpecificity`: host, then
    ///    ident, then nick, each scored by how much text it pins.
    /// 3. **scope** — a rule narrowed to a channel beats one narrowed
    ///    to a network beats an unscoped one.
    /// 4. **ties go to the LOWER role.** Two rules of equal
    ///    specificity disagreeing about someone is a config the ACL
    ///    cannot resolve, and fail-closed is this module's answer to
    ///    everything it cannot resolve. It is also what keeps
    ///    `owner = ["irc:x"]` + `roles = {"irc:x": banned}` a ban.
    /// 5. then config before runtime, so a file beats a live grant when
    ///    the two are otherwise indistinguishable.
    ///
    /// LEGACY MODE is untouched: with no `[acl]` config at all the ACL
    /// is dormant and every sender the adapter let through is owner.
    /// Resolve, but fall back to `unmatched` instead of the config's
    /// default role when NOTHING in the ACL names this caller.
    ///
    /// For callers who arrive holding a credential the operator minted
    /// rather than turning up unannounced. An A2A peer is the case this
    /// exists for: it authenticated with a per-peer token from
    /// `A2A_PEER_TOKENS`, so "unlisted" means "the operator issued a
    /// token and wrote no role rule", not "a stranger". Dropping it to
    /// `default_role` (guest, fail-closed) would be right for a stranger
    /// and wrong here — the credential IS the introduction.
    ///
    /// An explicit rule still wins in both directions, so an operator
    /// who wants that peer trusted, or banned, writes one and it decides.
    pub fn resolve_ctx_or(
        id: Option<&str>,
        ctx: &SenderContext,
        cfg: &AclConfig,
        unmatched: Role,
    ) -> Self {
        // Dormant mode hands every caller `Role::Owner`, which for a
        // credentialed peer is the hole `abb0894` closed. The fallback
        // is the answer there too.
        if !cfg.is_configured() {
            let norm = id.map(normalize_id);
            return Self {
                id: norm.clone(),
                subject: subject_for(id, ctx, None),
                rule_context_limit: None,
                mask: ctx.mask.clone(),
                caps: unmatched.default_caps(),
                role: unmatched,
                via: Some(format!(
                    "no [acl] section — {} by default",
                    unmatched.as_str()
                )),
                ticket_role: None,
            };
        }
        let norm = id.map(normalize_id);
        let idn = norm.as_deref();
        let rules = matching_rules(idn, ctx, cfg);
        match pick(&rules) {
            Some(w) => Self::from_rule(norm, ctx, w, cfg),
            None => Self {
                id: norm.clone(),
                subject: subject_for(idn, ctx, None),
                rule_context_limit: None,
                mask: ctx.mask.clone(),
                caps: caps_for(unmatched, idn, &[], &[], cfg),
                role: unmatched,
                via: Some(format!("no acl rule — {} by default", unmatched.as_str())),
                ticket_role: None,
            },
        }
    }

    pub fn resolve_ctx(id: Option<&str>, ctx: &SenderContext, cfg: &AclConfig) -> Self {
        // LEGACY MODE: no [acl] config at all ⇒ the ACL is dormant and
        // every sender that reached the gateway (the adapter allowlist
        // already vetted them, or the bot runs open by choice) gets the
        // pre-ACL behavior: full access, shared chat session. Adding
        // ANY [acl] key engages the model below, fail-closed.
        if !cfg.is_configured() {
            return Self {
                id: id.map(normalize_id),
                subject: subject_for(id, ctx, None),
                rule_context_limit: None,
                mask: ctx.mask.clone(),
                role: Role::Owner,
                caps: Role::Owner.default_caps(),
                via: None,
                ticket_role: None,
            };
        }
        let norm = id.map(normalize_id);
        let idn = norm.as_deref();
        let rules = matching_rules(idn, ctx, cfg);
        match pick(&rules) {
            Some(w) => Self::from_rule(norm, ctx, w, cfg),
            None => {
                let role = cfg.effective_default_role();
                Self {
                    id: norm.clone(),
                    subject: subject_for(idn, ctx, None),
                    rule_context_limit: None,
                    mask: ctx.mask.clone(),
                    caps: caps_for(role, idn, &[], &[], cfg),
                    role,
                    via: None,
                    ticket_role: None,
                }
            }
        }
    }

    /// The winning rule, applied.
    fn from_rule(
        id: Option<String>,
        ctx: &SenderContext,
        w: &MatchedRule,
        cfg: &AclConfig,
    ) -> Self {
        // A ban is a refuser, not a level: it carries no capabilities,
        // and no extra_caps line can put one back.
        if w.role == Role::Banned {
            return Self {
                id: id.clone(),
                subject: subject_for(id.as_deref(), ctx, w.user_name.as_deref()),
                role: Role::Banned,
                rule_context_limit: w.max_context_tokens,
                mask: ctx.mask.clone(),
                caps: Default::default(),
                via: w.via.clone(),
                ticket_role: None,
            };
        }
        // A role held by ticket (`ticket_role`): the caller stands at its
        // rule's role, and one call at a time may be raised to this one.
        // Only a role ABOVE the standing one means anything.
        let ticket_role = w.ticket_role.filter(|t| *t > w.role);
        let via = match (ticket_role, &w.via) {
            (Some(t), Some(v)) => Some(format!("{v}, {} by ticket", t.as_str())),
            (Some(t), None) => Some(format!("{} by ticket", t.as_str())),
            (None, v) => v.clone(),
        };
        Self {
            id: id.clone(),
            subject: subject_for(id.as_deref(), ctx, w.user_name.as_deref()),
            rule_context_limit: w.max_context_tokens,
            mask: ctx.mask.clone(),
            caps: caps_for(w.role, id.as_deref(), &w.extra, &w.deny, cfg),
            role: w.role,
            via,
            ticket_role,
        }
    }

    /// This caller at `role` for ONE call a ticket authorized: the same
    /// identity and subject, the role's own capabilities, and no ticket
    /// role of its own (a raised principal cannot be raised again).
    /// `how` says which ticket, for the label the call runs under.
    pub fn raised(&self, role: Role, how: &str) -> Self {
        Self {
            id: self.id.clone(),
            subject: self.subject.clone(),
            role,
            caps: role.default_caps(),
            rule_context_limit: self.rule_context_limit,
            mask: self.mask.clone(),
            via: Some(how.to_string()),
            ticket_role: None,
        }
    }

    /// Would a ticket let this caller run `tool`? True only for a role
    /// held by ticket whose level the tool's gate admits.
    pub fn ticket_would_admit(&self, tool: &str, cfg: &crate::config::VelesConfig) -> bool {
        match self.ticket_role {
            Some(role) => gate_denial(tool, cfg, &self.raised(role, "")).is_none(),
            None => false,
        }
    }

    /// Explain a resolution: the rule that won and the ones it beat,
    /// widest last. This is what `/acl explain` renders — with
    /// specificity deciding rather than declaration order, "why does
    /// this person have this role" needs an answer that is not "read
    /// the file top to bottom".
    pub fn explain(id: Option<&str>, ctx: &SenderContext, cfg: &AclConfig) -> Vec<MatchedRule> {
        let norm = id.map(normalize_id);
        let mut rules = matching_rules(norm.as_deref(), ctx, cfg);
        rules.sort_by_key(|r| std::cmp::Reverse(precedence_key(r)));
        rules
    }

    /// The context-token ceiling for this caller: the winning rule's
    /// own limit if it set one, else the role's, else unlimited.
    ///
    /// USER, 2026-09-09: *"The maximum context length for any non-admin
    /// user is 250k tokens"*, and *"maximum context for guests is
    /// 150k"*. Admin and owner are uncapped by default.
    pub fn max_context_tokens(&self, cfg: &AclConfig) -> u64 {
        if let Some(n) = self.rule_context_limit {
            return n;
        }
        cfg.limits
            .for_subject(self.role, &self.subject, &self.budget_key())
    }
    pub fn has(&self, cap: Cap) -> bool {
        self.caps.contains(&Cap::All) || self.caps.contains(&cap)
    }

    /// Role label for prompts/logs/status.
    ///
    /// Names the SUBJECT when there is no verified account, because on
    /// a services-less network that is everyone and "unverified" was
    /// both the normal case and a slur on it: the wire mask is the
    /// identity there, and the label says which one decided.
    pub fn label(&self) -> String {
        let who = match (&self.id, &self.mask) {
            (Some(id), _) => id.clone(),
            (None, Some(mask)) => mask.clone(),
            (None, None) => self.subject.clone(),
        };
        match &self.via {
            Some(via) => format!("{who} via {via} ({})", self.role.as_str()),
            None => format!("{who} ({})", self.role.as_str()),
        }
    }

    /// The key spend and turn budgets are counted against — a SECOND
    /// identity, deliberately coarser than `subject`.
    ///
    /// Why it exists: `subject` is an IDENTITY, and it keys sessions,
    /// sandboxes and ask bindings — things that must tell two callers
    /// apart. On a services-less IRC network a `/nick` is free, so a
    /// subject built from the mask mints a fresh identity (and a fresh
    /// daily token budget / turn allowance) on every rename. Coarsening
    /// `subject` itself is the wrong fix: folding two strangers behind
    /// one bouncer into one session is a privacy leak worse than the
    /// budget hole it closes (PENDING 2026-09-11).
    ///
    /// So the budget answers a different question — "which REAL person
    /// pays for this" — with the most stable thing the platform gave us:
    ///
    ///   1. the verified account (`id`) when one exists — a NickServ
    ///      account survives `/nick`;
    ///   2. else `ident@host` from the wire mask — a `/nick` keeps both,
    ///      and two nicks on one connection share one budget;
    ///   3. else the full `subject` (an `anon:` subject is already as
    ///      coarse as the chat; a `user:` subject is one human by
    ///      definition).
    ///
    /// Owner surfaces have no principal at all, so this is only ever
    /// asked of a budgeted caller.
    ///
    /// Owned, not `&str`: the `ident@host` arm borrows from `mask`
    /// while the fallback borrows from `subject`, and a caller should
    /// not have to care which field answered. Budget keys are built
    /// once per turn-brake, not in a hot loop.
    pub fn budget_key(&self) -> String {
        if let Some(id) = &self.id {
            return id.clone();
        }
        if let Some(mask) = &self.mask {
            // `nick!ident@host` → `ident@host`; a mask with no `!` is
            // used whole (a bare host is already nick-free). The host
            // is what a `/nick` cannot shake, so it is the honest
            // budget line on a services-less network.
            return match mask.split_once('!') {
                Some((_, ident_host)) => ident_host.to_string(),
                None => mask.clone(),
            };
        }
        self.subject.clone()
    }
}

/// Role → default caps, then the rule's and the identity's adjustments.
///
/// `extra`/`deny` come from the WINNING RULE (so a mask or a named user
/// can carry capabilities — before this they could not, because both
/// maps are keyed on an exact identity and a mask-matched caller has
/// none), layered under the id-keyed `extra_caps`/`deny_caps` maps.
fn caps_for(
    role: Role,
    id: Option<&str>,
    extra: &[Cap],
    deny: &[Cap],
    cfg: &AclConfig,
) -> BTreeSet<Cap> {
    let mut caps = role.default_caps();
    for c in extra {
        caps.insert(*c);
    }
    for c in deny {
        caps.remove(c);
    }
    if let Some(idn) = id {
        if let Some(extra) = cfg.extra_caps.get(idn) {
            for c in extra.iter().filter_map(|s| Cap::from_str_lossy(s)) {
                caps.insert(c);
            }
        }
        if let Some(deny) = cfg.deny_caps.get(idn) {
            for c in deny.iter().filter_map(|s| Cap::from_str_lossy(s)) {
                caps.remove(&c);
            }
        }
    }
    caps
}

/// Normalize an identity for matching: lowercase, trim. Identities are
/// `platform:principal`; IRC principals are casefolded by the adapter
/// already, telegram ids are numeric. We normalize the whole string
/// case-insensitively for config-matching convenience (telegram ids are
/// digits so this loses nothing).
fn normalize_id(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

// ── tool gating ───────────────────────────────────────────────────────

/// Which capability a tool requires, or None for "no gating" (should be
/// empty in practice — everything is classified).
pub fn tool_cap(tool: &str) -> Option<Cap> {
    use Cap::*;
    // MCP tools arrive by name (`mcp__<server>__<tool>`), never through
    // a fixed list — a server publishes what it publishes. The `Mcp`
    // arm on `tool_call` below could not gate them: the tool loop
    // resolves a `tool_call` envelope to its INNER name before
    // classifying (agent.rs:3463), so the envelope's own cap was never
    // consulted. Classify on the prefix, which is the only stable
    // thing about the name.
    if tool.starts_with("mcp__") {
        return Some(Mcp);
    }
    // A tool a MODULE registered declares its tier itself (R5): the
    // fixed table below is core's own tools.
    if let Some(c) = tool_caps().with(&tool.to_string(), |c| *c) {
        return Some(c);
    }
    Some(match tool {
        "terminal" | "execute_code" => Shell,
        // A monitor's `shell` predicate is a host command by another
        // name, and `shell` manages background host processes — both
        // were unclassified, i.e. ungated, until 2026-09-09.
        "monitor" | "shell" => Shell,
        // Presenting a plan is a chat act; plan mode's own gate
        // (`plan_refusal`) decides whether it is exempt from the
        // freeze, not the ACL.
        "present_plan" => Chat,
        // Arbitrary Tcl in the gateway's live interpreter — or, with
        // target=app, in the running desktop window. That is code
        // execution inside veles itself, above every sandbox: it is
        // owner-only, and `All` is the only cap that means owner.
        "tcl_eval" => All,
        "read_file" | "search_files" => FileRead,
        "write_file" | "patch" => FileWrite,
        "memory" => MemoryRead, // read+write; the tool re-checks direction
        "skills_list" | "skill_view" => Skills,
        "skill_manage" => SkillManage,
        "web_search" | "web_extract" => Web,
        // The guest browser: an internal operation no model can name
        // (browsertools::GUEST_OP). web_extract opens a challenged page in
        // it for a session that is not the owner's, behind guestproxy, and
        // the relay works it; so it takes what web_extract takes (USER
        // 2026-10-11, PLAN-challenge-relay.md D4).
        "browser_guest" => Web,
        "browser_navigate" | "browser_snapshot" | "browser_click" | "browser_type"
        | "browser_scroll" | "browser_press" | "browser_back" | "browser_console"
        | "browser_challenge" => Browser,
        "computer_use" => ComputerUse,
        "vision_analyze" => Vision,
        "generate_image" | "transcribe_audio" => MediaGen,
        "generate_video" => VideoGen,
        "text_to_speech" => Tts,
        "delegate_task" => Delegate,
        "cronjob" => Cron,
        "a2a" => A2a,
        "session_search" | "session_export" => SessionHistory,
        "todo" => Todo,
        // The plan read-back rides `todo`'s capability: it reads the
        // same two things the Tasks pane shows (the presented plan and
        // the list), it changes nothing, and it belongs at the same
        // floor. Reading ANOTHER session through its `session` argument
        // costs `session-history` on top, checked inside the tool — the
        // layer above cannot see which direction the read goes.
        "show_plan" => Todo,
        "clarify" => Chat,
        // Reading your own resolved identity back: the caller's role
        // and label are information they produce themselves. Floor it
        // at chat like `clarify`/`tool_search` — an unclassified
        // (owner-only) whoami would be useless to exactly the guest it
        // exists for.
        "whoami" => Chat,
        // Secrets: owner only. `api_request` sends a key and
        // `secret_request` stores one; `secret_list` names what exists.
        // The tools re-check the session themselves (a principal must
        // hold `All`), so a grant of these caps alone opens nothing.
        "secret_list" | "secret_request" | "api_request" => All,
        // Session mail (crate::mail): listing every live session on the
        // machine and starting turns in them is the owner's. The turn a
        // mail starts runs with no more than its sender's rights anyway,
        // but WHO may knock on which session is the owner's call — a
        // `[tool_caps]` line can widen it, nothing here does.
        "sessions" => All,
        // A connected device (crate::devices): a phone's location, its
        // notifications, its share sheet, the laptop's clipboard. The
        // owner's, like the rest of the owner's hardware.
        "device" => All,
        // veles' own settings (crate::config_tool). Each change is asked
        // of the session's human first, and in a non-owner's chat that
        // human is not the owner, so the tool is the owner's alone.
        "config" => All,
        // The person's apps (crate::apps): code the agent writes and runs
        // on the owner's own window.
        "app" => All,
        "open_preview" | "focus_pane" => Desktop,
        "tool_search" | "tool_describe" => Chat,
        "tool_call" => Mcp, // MCP tools (mcp__*) ride tool_call
        // The envelope's own cap is deliberately the floor: every
        // INNER tool still hits its own gate on its own resolved
        // target (agent.rs run_tool_call_expanded), so `batch` rides
        // wherever `chat` rides and the items decide the rest — the
        // same shape as tool_call judging inner MCP tools.
        "batch" => Chat,
        _ => return None,
    })
}

// ── the live [acl] section ───────────────────────────────────────────
//
// `configedit::effect_for("acl")` has always answered `Live`, and the
// settings UI has always shown "in effect now" — but the gateway snap-
// shots `Arc<VelesConfig>` at boot and its per-message gate read that
// snapshot, so an `[acl]` edit did nothing until a restart. Only the
// IRC `.agent` path reloaded (it calls `config::load()` per command).
// The claim is now true instead of aspirational: the ACL is re-read
// whenever the config FILE changes, and every gate reads it through
// here.
//
// Cheap enough to call per message: one `stat` in the common case, a
// parse only when the file actually moved. Keyed on (path, mtime, len)
// so a test that swaps VELES_HOME under us — or an editor that writes
// the same length in the same second — does not serve a stale answer
// (length alone would miss an in-place same-size edit; the two
// together are what `stat` can honestly offer).

/// The `[acl]` section as it is on disk RIGHT NOW.
///
/// Runtime grants were already live (they are read from the store on
/// every resolve); this makes the config half live too, which is what
/// "the ACL takes effect immediately" has to mean for a config file.
/// One cache serves every live-config reader — see `config::live`.
pub fn live_config() -> AclConfig {
    crate::config::live().acl.clone()
}

/// Is there a config FILE to be live about? See
/// [`crate::config::config_file_present`].
pub fn config_file_present() -> bool {
    crate::config::config_file_present()
}

/// The live `[acl]` section, or `fallback` when there is no config file
/// at all.
pub fn live_config_or(fallback: &AclConfig) -> AclConfig {
    if config_file_present() {
        live_config()
    } else {
        fallback.clone()
    }
}

/// The live `[acl]` section with the runtime overrides laid over it —
/// what ENFORCEMENT must read.
///
/// `live_config_or` alone is the file, and `/limits set` writes to the
/// store. Every report resolved through `with_runtime_limits` and every
/// gate did not, so an admin could lower a ceiling, watch `/limits`,
/// `/acl` and the desktop pane all agree it was lowered, and have
/// nothing be refused. One function so the two answers cannot drift
/// apart again.
pub fn live_limits_config(
    fallback: &AclConfig,
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
) -> AclConfig {
    with_runtime_limits(live_config_or(fallback), store)
}

/// Drop the cache. For tests that write a config file and immediately
/// resolve against it.
pub fn invalidate_live_config() {
    crate::config::invalidate_live();
}

// ── runtime context ceilings ─────────────────────────────────────────
//
// `[acl.limits]` is hand-authored config. `/limits set` is the live
// edit, and it lands in the runtime store — the `pairing.rs` idiom —
// so an admin can lower a ceiling from any frontend without a restart
// and without a config write. The runtime value WINS over config for
// the same key, because it is the more recent statement; clearing it
// returns to what the file says.

const LIMIT_PREFIX: &str = "acl_limit:";

/// Which budget a runtime override refers to.
///
/// There are three now — the size of one conversation, the tokens a
/// caller may spend in a day, and the turns they may start in an hour —
/// plus the concurrency cap, which is one number rather than a per-role
/// table. One enum so `/limits`, the desktop and the CLI cannot disagree
/// about the names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    Context,
    TokensPerDay,
    TurnsPerHour,
    ConcurrentTurns,
    /// Generations per hour (`/gen image|audio`). See
    /// `LimitsSection::media_per_hour_for_role`.
    MediaPerHour,
    /// Clips per hour (`/gen video`) — the expensive sibling.
    VideoPerHour,
    /// `/tcl` evaluations per hour.
    EvalPerHour,
}

impl Budget {
    pub fn as_str(&self) -> &'static str {
        match self {
            Budget::Context => "context",
            Budget::TokensPerDay => "tokens-per-day",
            Budget::TurnsPerHour => "turns-per-hour",
            Budget::ConcurrentTurns => "concurrent-turns",
            Budget::MediaPerHour => "media-per-hour",
            Budget::VideoPerHour => "video-per-hour",
            Budget::EvalPerHour => "eval-per-hour",
        }
    }

    pub fn parse(s: &str) -> Option<Budget> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "context" | "" => Some(Budget::Context),
            "tokens-per-day" | "tokens" | "spend" => Some(Budget::TokensPerDay),
            "turns-per-hour" | "turns" | "rate" => Some(Budget::TurnsPerHour),
            "concurrent-turns" | "concurrent" => Some(Budget::ConcurrentTurns),
            "media-per-hour" | "media" | "gen" => Some(Budget::MediaPerHour),
            "video-per-hour" | "video" | "clips" => Some(Budget::VideoPerHour),
            "eval-per-hour" | "eval" | "tcl" => Some(Budget::EvalPerHour),
            _ => None,
        }
    }

    pub fn all() -> &'static [Budget] {
        &[
            Budget::Context,
            Budget::TokensPerDay,
            Budget::TurnsPerHour,
            Budget::ConcurrentTurns,
            Budget::MediaPerHour,
            Budget::VideoPerHour,
            Budget::EvalPerHour,
        ]
    }

    /// Is this budget a per-role table, or one global number?
    pub fn is_per_role(&self) -> bool {
        !matches!(self, Budget::ConcurrentTurns)
    }

    /// The `meta` key infix. EMPTY for the context ceiling, which is
    /// deliberate: that budget shipped first with keys shaped
    /// `acl_limit:<role>`, and a runtime override an admin set before
    /// the other two existed must keep working rather than silently
    /// reverting to config.
    fn infix(&self) -> &'static str {
        match self {
            Budget::Context => "",
            Budget::TokensPerDay => "tokens-per-day:",
            Budget::TurnsPerHour => "turns-per-hour:",
            Budget::ConcurrentTurns => "concurrent-turns:",
            Budget::MediaPerHour => "media-per-hour:",
            Budget::VideoPerHour => "video-per-hour:",
            Budget::EvalPerHour => "eval-per-hour:",
        }
    }
}

/// Store a runtime ceiling. `which` is a role name or `default`.
pub fn set_runtime_limit(
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
    which: &str,
    tokens: u64,
) -> anyhow::Result<()> {
    set_runtime_budget(store, Budget::Context, which, tokens)
}

/// Store a runtime override for any budget.
pub fn set_runtime_budget(
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
    budget: Budget,
    which: &str,
    value: u64,
) -> anyhow::Result<()> {
    let key = limit_key(budget, which)?;
    let g = store
        .lock()
        .map_err(|_| anyhow::anyhow!("store lock poisoned"))?;
    g.meta_set(&format!("{LIMIT_PREFIX}{key}"), &value.to_string())?;
    Ok(())
}

/// Drop a runtime ceiling. True when one was there.
pub fn clear_runtime_limit(
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
    which: &str,
) -> bool {
    clear_runtime_budget(store, Budget::Context, which)
}

/// Drop a runtime override for any budget. True when one was there.
pub fn clear_runtime_budget(
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
    budget: Budget,
    which: &str,
) -> bool {
    let Ok(key) = limit_key(budget, which) else {
        return false;
    };
    let Ok(g) = store.lock() else { return false };
    let full = format!("{LIMIT_PREFIX}{key}");
    if g.meta_get(&full).is_none() {
        return false;
    }
    g.meta_delete(&full).unwrap_or(false)
}

fn limit_key(budget: Budget, which: &str) -> anyhow::Result<String> {
    if !budget.is_per_role() {
        // One number, so the role half is noise. Accepted and ignored
        // rather than rejected: `/limits set concurrent-turns 3` and
        // `… concurrent-turns default 3` should not disagree.
        return Ok(format!("{}default", budget.infix()));
    }
    let w = which.trim().to_lowercase();
    if w == "default" || Role::parse(&w).is_some() {
        return Ok(format!("{}{w}", budget.infix()));
    }
    // A SUBJECT, spelled as the budget is keyed — `irc:xenia`,
    // `~a@host`. A role is one bare word; a subject always carries a
    // `:` (account id) or an `@`/`*`/`!` (mask-derived budget key), so
    // the two cannot be confused. Stored with a `sub:` tag so
    // `with_runtime_limits` routes it to `by_subject`, not `by_role`.
    if w.contains(':') || w.contains('@') || w.contains('!') {
        return Ok(format!("{}sub:{w}", budget.infix()));
    }
    anyhow::bail!("{which:?} is not a role, `default`, or a subject (irc:acct / ident@host)")
}

/// Every runtime ceiling, as `(role-or-default, tokens)` — the CONTEXT
/// budget only, for the callers that predate the other two.
pub fn runtime_limits(
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
) -> Vec<(String, u64)> {
    runtime_budgets(store)
        .into_iter()
        .filter(|(b, _, _)| *b == Budget::Context)
        .map(|(_, which, v)| (which, v))
        .collect()
}

/// Every runtime override, as `(budget, role-or-default, value)`.
pub fn runtime_budgets(
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
) -> Vec<(Budget, String, u64)> {
    let Ok(g) = store.lock() else {
        return Vec::new();
    };
    let mut out: Vec<(Budget, String, u64)> = g
        .meta_scan_prefix(LIMIT_PREFIX)
        .into_iter()
        .filter_map(|k| {
            let v = g.meta_get(&k)?.trim().parse::<u64>().ok()?;
            let rest = k.trim_start_matches(LIMIT_PREFIX);
            // Split off the budget infix by PREFIX, not by the last
            // colon: a per-subject key is `<infix>sub:<subject>` and the
            // subject itself carries colons (`sub:irc:xenia`), which a
            // blind `rsplit_once(':')` would misread as the infix.
            // Every infix ends in ':' (or is empty for legacy context),
            // so a known-infix prefix match is unambiguous.
            let (budget, which) = Budget::all()
                .iter()
                .find_map(|b| {
                    let inf = b.infix();
                    if inf.is_empty() {
                        return None; // legacy handled below
                    }
                    rest.strip_prefix(inf).map(|w| (*b, w.to_string()))
                })
                .unwrap_or((Budget::Context, rest.to_string()));
            Some((budget, which, v))
        })
        .collect();
    out.sort_by(|a, b| (a.0.as_str(), &a.1).cmp(&(b.0.as_str(), &b.1)));
    out
}

/// Lay the runtime ceilings over a config's `[acl.limits]`.
///
/// Every ACL report and every ceiling check goes through this, so
/// `/limits set` is genuinely live rather than a value only one
/// surface remembers.
pub fn with_runtime_limits(
    mut cfg: AclConfig,
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
) -> AclConfig {
    for (budget, which, value) in runtime_budgets(store) {
        // A `sub:` prefix marks a per-SUBJECT override (see `limit_key`):
        // it lands in `by_subject` under the budget it names, not in the
        // role table. The role half is the subject string verbatim.
        if let Some(subject) = which.strip_prefix("sub:") {
            let entry = cfg
                .limits
                .by_subject
                .entry(subject.to_string())
                .or_default();
            match budget {
                Budget::Context => entry.context = Some(value),
                Budget::TokensPerDay => entry.tokens_per_day = Some(value),
                Budget::TurnsPerHour => entry.turns_per_hour = Some(value),
                Budget::MediaPerHour => entry.media_per_hour = Some(value),
                Budget::VideoPerHour => entry.video_per_hour = Some(value),
                Budget::EvalPerHour => entry.eval_per_hour = Some(value),
                // One number, no per-subject table.
                Budget::ConcurrentTurns => cfg.limits.max_concurrent_turns = value as u32,
            }
            continue;
        }
        let is_default = which == "default";
        match budget {
            Budget::Context if is_default => cfg.limits.default_max_context_tokens = value,
            Budget::Context => {
                cfg.limits.by_role.insert(which, value);
            }
            Budget::TokensPerDay if is_default => cfg.limits.default_max_tokens_per_day = value,
            Budget::TokensPerDay => {
                cfg.limits.tokens_per_day_by_role.insert(which, value);
            }
            Budget::TurnsPerHour if is_default => cfg.limits.default_max_turns_per_hour = value,
            Budget::TurnsPerHour => {
                cfg.limits.turns_per_hour_by_role.insert(which, value);
            }
            Budget::MediaPerHour if is_default => cfg.limits.default_max_media_per_hour = value,
            Budget::MediaPerHour => {
                cfg.limits.media_per_hour_by_role.insert(which, value);
            }
            Budget::VideoPerHour if is_default => cfg.limits.default_max_video_per_hour = value,
            Budget::VideoPerHour => {
                cfg.limits.video_per_hour_by_role.insert(which, value);
            }
            Budget::EvalPerHour if is_default => cfg.limits.default_max_eval_per_hour = value,
            Budget::EvalPerHour => {
                cfg.limits.eval_per_hour_by_role.insert(which, value);
            }
            // One number, no per-role table (see `Budget::is_per_role`).
            Budget::ConcurrentTurns => cfg.limits.max_concurrent_turns = value as u32,
        }
    }
    cfg
}

// ── tools nobody classified ──────────────────────────────────────────
//
// `tool_cap` knows the builtins. It cannot know what a PLUGIN or an
// ADAPTER registers — `veles::register_tool` and `register_global_tool`
// both take any name — and until 2026-09-09 an unknown name meant "no
// gate", so a Tcl plugin tool was reachable by every principal who
// could start a turn. Fail-closed is the posture everywhere else in
// this module, so it is the posture here: an unclassified tool is
// owner-only unless something says otherwise.
//
// Two things can say otherwise:
//
//   1. the ADAPTER that registered it declares that the tool gates
//      ITSELF, per action — `irc say` and `irc history` are not one
//      permission, so one cap for the whole tool would be either too
//      loose or useless (`declare_self_gated`);
//   2. the OWNER classifies it in config: `[tool_caps] wordcount =
//      "web"`. Only tools with no builtin classification can be named
//      there — a config that could rewrite `terminal`'s cap would make
//      the builtin table advisory.

static SELF_GATED: std::sync::OnceLock<crate::owned::OwnedRegistry<String, ()>> =
    std::sync::OnceLock::new();

fn self_gated() -> &'static crate::owned::OwnedRegistry<String, ()> {
    SELF_GATED.get_or_init(crate::owned::OwnedRegistry::default)
}

fn tool_caps() -> &'static crate::owned::OwnedRegistry<String, Cap> {
    static C: std::sync::OnceLock<crate::owned::OwnedRegistry<String, Cap>> =
        std::sync::OnceLock::new();
    C.get_or_init(crate::owned::OwnedRegistry::default)
}

/// A module registering a tool declares the capability its calls need
/// (`tool_cap` answers it before the builtin table). Without one a
/// module's tool is unclassified, which the engine's gate refuses for
/// everyone but the owner. The owner is read from the calling thread
/// (M4): purged with the module on reload.
pub fn declare_tool_cap(tool: &str, cap: Cap) {
    tool_caps().insert(tool.to_string(), cap);
}

/// Remove every tool-cap declaration `owner` made (R3 teardown). `None`
/// (core) removes nothing. Returns the count removed.
pub fn purge_owner_tool_caps(owner: Option<&'static str>) -> usize {
    tool_caps().purge(owner)
}

/// An adapter registering a multi-action tool declares here that the
/// tool performs its OWN per-action capability checks. Called next to
/// `tools::register_global_tool` at adapter startup. The owner is read
/// from the calling thread (M4): a module's self-gated declarations are
/// purged with it on reload.
pub fn declare_self_gated(tool: &str) {
    self_gated().insert(tool.to_string(), ());
}

/// Remove every self-gated declaration `owner` made (R3 teardown).
/// `None` (core) removes nothing. Returns the count removed.
pub fn purge_owner_self_gated(owner: Option<&'static str>) -> usize {
    self_gated().purge(owner)
}

pub fn is_self_gated(tool: &str) -> bool {
    self_gated().with(&tool.to_string(), |_| ()).is_some()
}

/// What the tool loop should do with a call to `tool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolGate {
    /// Needs this capability.
    Cap(Cap),
    /// The tool checks its own caps per action; pass it through.
    SelfGated,
    /// Nothing classified it: only the owner may call it.
    OwnerOnly,
}

/// The gate for one tool name: builtin classification, then the
/// adapter's self-gating declaration, then the owner's `[tool_caps]`
/// override for unclassified names, then fail-closed.
/// The capability decision for one tool call: `None` = run it,
/// `Some(row)` = don't, and hand `row` back to the model.
///
/// Extracted so the agent's tool loop and `ToolRegistry::dispatch` make
/// the SAME decision. It lived only in the loop, which meant any other
/// caller of `dispatch` — `veles::tool_call` from a plugin, and
/// anything added later — reached every tool with no capability check
/// at all.
///
/// Classify on the TARGET (after alias resolution), never on the alias:
/// that is what stops `bash` being a way to reach `terminal` past its
/// cap.
pub fn gate_denial(
    target: &str,
    cfg: &crate::config::VelesConfig,
    who: &Principal,
) -> Option<String> {
    match tool_gate(target, cfg) {
        ToolGate::Cap(cap) if !who.has(cap) => Some(tool_denial(target, who)),
        ToolGate::OwnerOnly if !who.has(Cap::All) => Some(unclassified_denial(target, who)),
        // SelfGated: the tool checks its own actions per call
        // (veles-irc/src/tools.rs).
        _ => None,
    }
}

pub fn tool_gate(tool: &str, cfg: &crate::config::VelesConfig) -> ToolGate {
    if let Some(c) = tool_cap(tool) {
        return ToolGate::Cap(c);
    }
    if let Some(raw) = cfg.tool_caps.get(tool) {
        let raw = raw.trim();
        if raw.eq_ignore_ascii_case("self") {
            return ToolGate::SelfGated;
        }
        if raw.eq_ignore_ascii_case("owner") {
            return ToolGate::OwnerOnly;
        }
        if let Some(c) = Cap::from_str_lossy(raw) {
            return ToolGate::Cap(c);
        }
        tracing::warn!(
            "[tool_caps] {tool} = {raw:?} is not a capability name — treating as owner-only"
        );
    }
    if is_self_gated(tool) {
        return ToolGate::SelfGated;
    }
    ToolGate::OwnerOnly
}

/// Refusal for a tool nobody classified. Distinct wording from
/// [`tool_denial`] because there is no capability to ask for: the fix
/// is a `[tool_caps]` line, and the model should say so rather than
/// invent a permission.
pub fn unclassified_denial(tool: &str, who: &Principal) -> String {
    format!(
        "error: access denied — tool `{tool}` has no declared capability, so it is owner-only and {} is not the owner. Do not retry; tell the user the owner can classify it with `[tool_caps] {tool} = \"<capability>\"` in config.toml.",
        who.label()
    )
}

/// Refusal text for a gated tool, phrased as a tool result so the model
/// adapts instead of crashing the turn.
pub fn tool_denial(tool: &str, who: &Principal) -> String {
    // `All` is not a grantable capability — it is what being the owner
    // means. "requires the `all` capability" would send the model off
    // to ask for something nobody can give, so an owner-only tool says
    // so instead.
    if tool_cap(tool) == Some(Cap::All) {
        return format!(
            "error: access denied — tool `{tool}` is owner-only and {} is not the owner. Do not retry or look for another route; tell the user the owner has to run it.",
            who.label()
        );
    }
    let cap = tool_cap(tool)
        .map(|c| c.as_str())
        .unwrap_or("required-capability");
    format!(
        "error: access denied — tool `{tool}` requires the `{cap}` capability, which {} does not have. Do not retry; tell the user they need to be granted it.",
        who.label()
    )
}

/// Refusal text for the F1 read-jail (a non-owner principal reading a
/// sensitive path). Same contract as [`tool_denial`]: a tool result the
/// model reads and adapts to, not a crash. Names WHO is allowed — the
/// owner — because there is no capability to grant here: the file is
/// the principal, and only the owner is it.
pub fn read_jail_denial(why: &str, who: &Principal) -> String {
    format!(
        "error: access denied — reading {why} is owner-only and {} is not the owner. Do not retry or work around it (no symlink, copy, or search trick will pass); tell the user the owner must read it themselves.",
        who.label()
    )
}

// ── builtin command tiers ─────────────────────────────────────────────

/// Minimum capability for gateway builtin slash commands.
/// Everything not listed falls back to the chat cap (it's a normal turn
/// starter like /model or /theme, which the model handles).
pub fn builtin_cap(cmd: &str) -> Option<Cap> {
    use Cap::*;
    Some(match cmd {
        "status" | "version" | "help" => CmdStatus,
        "guards" => CmdStatus,
        "context" => CmdStatus,
        "agents" => CmdStatus,
        // `/tasks` is the TODO list (the taxonomy split, USER 2026-09-17,
        // moved the running-work registry to /subagents + /monitors +
        // /shells). Read-only like `/todos`, which is the same list.
        "tasks" => CmdStatus,
        "shells" => CmdStatus,
        // The compute-pool registry view (B3a): read-only, and knowing
        // who holds the singleton fence is status, not privilege.
        "nodes" => CmdStatus,
        // Read-only introspection of engine state. RM4.3 moved these
        // into the shared dispatcher, which makes them gateway-
        // reachable — each needs a tier or `None` reads as "no gate".
        "usage" => CmdStatus,
        // `/quota` reads the UPSTREAM plan. The DOOR is open like
        // `/usage`: how much of the plan is left is what a caller needs
        // to know whether they may ask for something, and refusing that
        // only makes them find out by being refused mid-turn.
        //
        // The IDENTIFIERS are not: the payload carries seat and
        // subscription ids, the org member list and the month's dollar
        // spend, which together are an inventory of the account behind
        // the agent — `secret`'s family, not `usage`'s. So the tier is
        // split INSIDE the command (`quota_command`), the way `/acl`
        // splits its self-report from its rule listing: bare `/quota`
        // answers pools and percentages at this tier, and every verb
        // that names an id or an amount takes `cmd-admin`.
        "quota" => CmdStatus,
        "tools" => CmdStatus,
        "skills" => CmdStatus,
        "memory" => CmdStatus,
        "plugins" => CmdStatus,
        "profile" => CmdStatus,
        "commands" => CmdStatus,
        "sessions" => CmdStatus,
        "checkpoints" => CmdStatus,
        "todo" | "todos" => CmdStatus,
        "diff" => CmdStatus,
        "export" => CmdStatus,
        "title" => CmdStatus,
        "hooks" => CmdStatus,
        "reload" => CmdStatus,
        "failover" => CmdSession,
        "plan" => CmdSession,
        // Session-scoped provider override — same tier as failover and
        // plan, for the same reason: it changes this session's requests.
        "body" => CmdSession,
        // Where this session's web traffic leaves. Only the owner's
        // choice changes anything (everyone else's already goes the
        // proxied way), and the change only narrows: the door is the
        // session tier, like /plan.
        "egress" => CmdSession,
        // The door is status, like `/quota`: bare `/model` only says
        // what this session runs (USER 2026-10-02, from IRC: a user
        // asking which model answered was refused). Switching is the
        // session-shaping act and checks `cmd-session` INSIDE the
        // command (commands.rs, the `/model` arm).
        "model" => CmdStatus,
        // What a provider serves (the session's by default): a read,
        // and nothing in it switches.
        "models" => CmdStatus,
        "reasoning" => CmdSession,
        // The UI language. Session-shaping like /theme and /bindings:
        // on the gateway it sets what THIS SESSION renders
        // (`session_locale`), not who may chat. That was this comment's
        // claim from the start and it was false until 2026-09-25 — the
        // command switched the whole gateway process, i.e. every user of
        // every surface. The config-writing verbs (`primary`/`secondary`)
        // are an admin act and are checked for `cmd-admin` INSIDE the
        // command, like `/quota`'s id-bearing verbs. Declared late — the
        // arm existed in `dispatch` with no tier, which the gateway reads
        // as "no gate".
        "locale" | "language" | "lang" => CmdSession,
        "steer" => CmdSession,
        // The message queue (R4.6): listing is mild, editing/cancelling
        // what is ABOUT to run is session-shaping — one command with
        // subcommands, so it rides the higher tier, like /mech.
        "queue" => CmdSession,
        // Attaching a file from THIS box's filesystem into the next
        // prompt: trusted-and-up only.
        "image" | "attach" => CmdSession,
        // Transcript editing — session-shaping, same family as
        // compact.
        "undo" => CmdSession,
        "clear" => CmdSession,
        "branch" | "fork" => CmdSession,
        // Secret NAMES are visible, values never are — but the list
        // itself is an inventory of what this agent can reach.
        "secret" => CmdAdmin,
        "restore" => CmdSession,
        // `/acl` with no arguments answers "who am I and what may I
        // do", which is open on purpose: knowing your own tier is not
        // a privilege, and `.agent caps` has answered it since the ACL
        // existed. Reading the RULES and changing them are gated
        // INSIDE the command (`cmd-admin` to read, owner to write),
        // which is what `CommandCtx.principal` is for — one tier for
        // the whole name would either lock the self-report away from
        // the person it is about or open `grant` to them.
        "acl" | "access" => CmdStatus,
        // `/acl`'s top-level spellings. `caps` answers "what may I do",
        // which is the open half; `grant` and `revoke` are the write
        // half and take the owner floor here rather than inside,
        // because the whole NAME is a write.
        "caps" => CmdStatus,
        "grant" | "revoke" => All,
        // Joining someone else's session (the gateway's `/join`): the
        // joined chat's lines run there with the owner's rights.
        "join" | "leave" => All,
        // The pending questions. Listing them is a report; deciding one
        // is the gate in front of every tool call, and the BINDING
        // narrows it further inside the command — this cap only gets a
        // caller as far as the verb.
        "asks" => CmdStatus,
        "answer" | "approve" | "deny" => Approve,
        // Same shape: `/sandbox where` tells a caller which box their
        // own files are in — being told keeps them from assuming they
        // are on the host — while declaring, assigning and purging are
        // `cmd-admin` inside. `/limits` likewise: the number that is
        // about to refuse your turn is yours to know.
        "sandbox" | "sandboxes" => CmdStatus,
        "limits" => CmdStatus,
        // `/lockdown` reads open too — a sender who just got refused is
        // entitled to learn why, and by then the connector path has
        // already stopped serving them anyway. `on` takes `cmd-admin`
        // inside; `off` takes something no capability can express (a
        // LOCAL surface), which is exactly why the verbs are checked in
        // the command and not at the name.
        "lockdown" => CmdStatus,
        // What the engine may do UNATTENDED, and the command that
        // changes it. Reading is mild; setting is not, and it is one
        // command, so it takes the higher tier.
        "mech" | "mechanisms" => CmdAdmin,
        // Scheduling work that runs with no human present is the most
        // consequential thing a chat line can do here, so it sits at the
        // top tier even though listing on its own would be harmless.
        "crons" | "cron" => CmdAdmin,
        // Triggers are the same class of thing as cron — automated
        // reactions with no human in the loop — and a trigger row can
        // spend money (vision) or forward channel text to another
        // process (slopdrop), so the whole name takes the same tier.
        // The FIRE itself answers to min_role per row, tighter than
        // this door.
        "triggers" | "trigger" => CmdAdmin,
        // Rewriting the DEFAULT route changes what every later session
        // costs and which account it bills, and it writes a file on
        // disk. Owner, not merely cmd-admin: /model already serves
        // everyone who only needs to change their own session.
        "defaults" => All,
        // Landing a peer's change to [acl], [sandbox], hooks or plugins
        // is the decision the hold exists to leave to the owner.
        "configsync" => All,
        // Stopping YOUR OWN chat's turn is session-scoped, and the tier
        // has to stay low or the command is useless to the person most
        // likely to need it.
        "cancel" | "stop" => CmdSession,
        // Moving your own foreground command to the background is the
        // gentle sibling of cancel — same scope, same low tier (it is
        // useless to the person who needs it most if it is gated any
        // higher). It touches only the caller's session.
        "detach" => CmdSession,
        // Rewriting the session's history is session-shaping — and
        // until 2026-09-07 it had NO tier at all: `builtin_cap`
        // returned None and the gateway read None as "no gate", so
        // anyone who could chat could compact the session out from
        // under a running turn.
        "compact" | "compress" => CmdSession,
        // Starting a fresh session, hopping back to an old one, or
        // replaying the last prompt all reshape THIS session — same
        // family as /model and /clear.
        "new" | "reset" | "resume" | "retry" => CmdSession,
        // Session migration across pool nodes (B6). Admin: it moves
        // a session's ONLY WRITER to another machine — the single
        // operation that can double-run a session if aimed wrong.
        // The dispatcher re-checks per subcommand; this tier is the
        // floor.
        "session" => CmdAdmin,
        // Unattended execution started from a chat line. What runs
        // under the name changed with the taxonomy split (USER,
        // 2026-09-17) — `/background <cmd>` now starts a SHELL and
        // `/btw <prompt>` the agent subprocess this tier was written
        // for — but the tier did not move: the gated property is
        // "runs with nobody watching it", which both halves have.
        // Same reasoning as cron's tier.
        "background" | "bg" | "btw" => CmdAdmin,
        // The per-kind registry views. Listing and cancelling are a
        // report plus a brake on work the caller's session may reach
        // (the scoping is inside `kind_command`/`cancel_for`), the
        // same posture the aggregate `/tasks` command had at this tier
        // before the split.
        "subagents" | "monitors" => CmdStatus,
        // Media generation as a first-class act (USER, 2026-09-11:
        // "gated behind user+ acls"). This is the FLOOR: the gateway's
        // pre-dispatch gate reads it for every `/gen`, and the command
        // re-checks per kind inside `gen_command` — `/gen video`
        // additionally needs VideoGen (trusted+), which the floor
        // cannot express since the kind is only known after parsing.
        // Both doors still agree: the command never allows what the
        // floor denied.
        "gen" | "generate" => MediaGen,
        // Evaluating Tcl for a caller. `Cap::Shell` because that is the
        // tier the IRC arm has always answered to, and because the SAFE
        // interp is what a below-owner caller gets — the full `tclsh` is
        // a second decision (`tcleval::full_interp_allowed`), not this
        // one. Tiered HERE rather than only in `irc_command_cap` so the
        // LITERAL `/tcl` spelling is answered by the dispatcher instead
        // of falling through to the model as a prompt: USER,
        // 2026-09-11, "make sure that .tcl|/tcl bypass the llm so that
        // they don't consume a turn for a simple request from a dumb
        // machine". A guest is refused at this door and never costs a
        // provider call.
        "tcl" | "eval" => Shell,
        // The privileged spelling. `Cap::Shell` gets you to the door;
        // `tcl_command_as` then requires `Cap::All` for the admin path,
        // so the tier here is the FLOOR and not the whole answer — the
        // same shape `/gen` uses for video.
        "tcladmin" => Shell,
        // One-shot tcl-lsp diagnostics/formatting of a named host file
        // (2026-09-12 plan §2). Shell tier like `/tcl`: it names a
        // path, and the command arm itself re-checks the cap so the
        // dispatcher and the arm cannot drift.
        "lint" | "fmt" => Shell,
        _ => return None, // not a builtin; normal turn (chat-gated upstream)
    })
}

// ── per-session principals ────────────────────────────────────────────
//
// Tools are built once per session but need the caller's caps at call
// time (the memory tool checks read vs write direction). Same shape as
// ask.rs's SESSION_ASKERS: a process-global map keyed by session id,
// installed when the session's agent is created.

static SESSION_PRINCIPALS: std::sync::OnceLock<
    std::sync::Mutex<HashMap<String, std::sync::Arc<Principal>>>,
> = std::sync::OnceLock::new();

// ── S9 runtime role grants (pairing codes) ───────────────────────────
//
// The gateway installs its store here at boot; Principal::resolve
// consults it after config roles so `/pair <code>` redemptions take
// effect WITHOUT a config rewrite or restart. Tests install scratch
// stores via the same setter.

static RUNTIME_STORE: std::sync::OnceLock<
    std::sync::Mutex<Option<std::sync::Arc<std::sync::Mutex<crate::store::Store>>>>,
> = std::sync::OnceLock::new();

fn runtime_store_cell(
) -> &'static std::sync::Mutex<Option<std::sync::Arc<std::sync::Mutex<crate::store::Store>>>> {
    RUNTIME_STORE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Install the process-wide pairing store (gateway/CLI boot). REPLACES
/// any previous one: a process runs one gateway, but the live
/// adversarial suite boots several in sequence, and a OnceLock-only
/// `set` silently pinned every phase after the first to the FIRST
/// phase's store — grants landed in a database the resolver never
/// read. Same-value replacement in production is a no-op in effect.
pub fn set_runtime_pairing_store(store: std::sync::Arc<std::sync::Mutex<crate::store::Store>>) {
    *runtime_store_cell().lock().unwrap() = Some(store);
}

pub fn runtime_pairing_store() -> Option<std::sync::Arc<std::sync::Mutex<crate::store::Store>>> {
    runtime_store_cell().lock().unwrap().clone()
}

fn session_principals() -> &'static std::sync::Mutex<HashMap<String, std::sync::Arc<Principal>>> {
    SESSION_PRINCIPALS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

pub fn set_session_principal(session_id: &str, p: Principal) {
    session_principals()
        .lock()
        .unwrap()
        .insert(session_id.to_string(), std::sync::Arc::new(p));
}

/// Forget a session's principal: it acts for the owner surface again
/// (`Agent::cap_turn_principal(None)` ending a mail turn's cap in an
/// owner surface's session).
pub fn clear_session_principal(session_id: &str) {
    session_principals().lock().unwrap().remove(session_id);
}

pub fn session_principal(session_id: &str) -> Option<std::sync::Arc<Principal>> {
    session_principals()
        .lock()
        .unwrap()
        .get(session_id)
        .cloned()
}

// ── per-session sender context ────────────────────────────────────────
//
// `SESSION_PRINCIPALS` answers "who is this session for". This answers
// "where are they speaking from", which is a different question and one
// a tool cannot reconstruct: a session id names a conversation, not a
// channel on a network.
//
// It exists for the slopdrop bridge. slopdrop's interpreter reads its
// channel context out of globals its OWN IRC connection used to set —
// `[nick]`, `[names]`, `[log]` — and headless it has no connection, so
// the only place those can come from is the bot still in the channel.
// A tool built once per session has to look the conversation up at call
// time, exactly as it looks the principal up.

static SESSION_SENDERS: std::sync::OnceLock<
    std::sync::Mutex<HashMap<String, std::sync::Arc<SenderContext>>>,
> = std::sync::OnceLock::new();

fn session_senders() -> &'static std::sync::Mutex<HashMap<String, std::sync::Arc<SenderContext>>> {
    SESSION_SENDERS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Publish where this session's caller is speaking from. Re-set on every
/// message rather than once: a nick change rewrites the mask, and a stale
/// mask is what `[nick]` would answer with.
pub fn set_session_sender(session_id: &str, ctx: SenderContext) {
    session_senders()
        .lock()
        .unwrap()
        .insert(session_id.to_string(), std::sync::Arc::new(ctx));
}

/// Where this session is speaking from, if an adapter ever said.
pub fn session_sender(session_id: &str) -> Option<std::sync::Arc<SenderContext>> {
    session_senders().lock().unwrap().get(session_id).cloned()
}

/// The env var a `delegate_task` child is spawned with: the JSON of the
/// CALLER's principal, so the child's agent gate binds to the same
/// person instead of resolving to the owner surface a bare `veles chat`
/// is. Set by `delegate.rs` at spawn; read once by the chat boot path.
///
/// This is not a secret and cannot be one: it travels in the
/// environment, and anything able to set it already runs as the veles
/// user. So the honest description is that it NAMES who is asking, and
/// the process that could forge it could also read `state.db` and the
/// provider keys directly — it is not a trust boundary against the host.
///
/// It is a boundary against exactly one thing, which is the one that
/// matters inside the bot: the OWNER claim. `media::deliverable` leans
/// on admin not being owner ("an admin is a trusted operator of the
/// bot, not of the host"), and role/caps used to deserialize verbatim,
/// so `VELES_SESSION_PRINCIPAL='{"role":"Owner","caps":["All"],…}
/// veles chat` was an owner agent. [`clamp_owner_claim`] checks that
/// claim against the ACL's own answer for the identity; everything
/// below owner is taken as given, because a caller's role can come from
/// a channel-scoped rule this path cannot reproduce and a demotion
/// there would be a bug, not a defence.
///
/// A child spawned by hand with no var at all is the owner surface a
/// bare `veles chat` has always been — the documented second door of
/// the CLI dispatch surface, not a new one.
pub const PRINCIPAL_ENV: &str = "VELES_SESSION_PRINCIPAL";

/// Read [`PRINCIPAL_ENV`] and decode it.
///
/// Unset → `Ok(None)`: this is the owner surface, the way a plain CLI
/// has always been. Set but malformed → `Err`, and the boot path REFUSES
/// to start rather than falling back to the owner surface — a var we
/// set ourselves being unreadable is a bug worth stopping for, and
/// "unparseable ⇒ owner" is exactly the fail-open the delegate fix
/// exists to close.
pub fn principal_from_env() -> Result<Option<Principal>, String> {
    let raw = match std::env::var(PRINCIPAL_ENV) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let p: Principal = serde_json::from_str(&raw)
        .map_err(|e| format!("{PRINCIPAL_ENV} is set but not a valid principal: {e}"))?;
    Ok(Some(clamp_owner_claim(p, &live_config())))
}

/// Honour an OWNER claim only when the ACL independently agrees.
///
/// Deliberately not a general re-resolution. A caller's role can come
/// from a rule scoped to a network or a channel, and this path has
/// neither — so re-deriving everything would demote a legitimately
/// trusted delegate to whatever the default role is, which is a bug
/// wearing a security hat. Owner is different: it is the one rung that
/// buys `Cap::All`, it is pinned by identity (`[acl] owner` or a role
/// pin) rather than by scope, and it is the rung the media gate and the
/// F1 read jail are written against.
///
/// A dormant ACL resolves every identity as owner, so there is nothing
/// to disagree with and the claim passes through unchanged — the same
/// legacy posture every other door here keeps.
fn clamp_owner_claim(mut p: Principal, acl: &AclConfig) -> Principal {
    if p.role != Role::Owner && !p.caps.contains(&Cap::All) {
        return p;
    }
    if !acl.is_configured() {
        return p;
    }
    let ctx = SenderContext {
        mask: p.mask.clone(),
        ..Default::default()
    };
    if Principal::resolve_ctx(p.id.as_deref(), &ctx, acl).role == Role::Owner {
        return p;
    }
    p.role = Role::Admin;
    p.caps = Role::Admin.default_caps();
    p.via = Some(format!(
        "{PRINCIPAL_ENV} claimed owner; the acl does not agree — demoted to admin"
    ));
    p
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from_yaml(y: &str) -> AclConfig {
        serde_yaml::from_str(y).unwrap()
    }

    #[test]
    fn owner_resolution() {
        let cfg = cfg_from_yaml("owner: [\"irc:wrath\"]");
        let p = Principal::resolve(Some("IRC:Wrath"), &cfg);
        assert_eq!(p.role, Role::Owner);
        assert!(p.has(Cap::All) && p.has(Cap::Shell));
        // nobody else is owner
        let q = Principal::resolve(Some("irc:someone"), &cfg);
        assert_ne!(q.role, Role::Owner);
    }

    // ── network-scoped account identities (`irc:acct@network`) ──────

    /// `irc:xenia@irc.synirc.net` matches the account on synirc but is
    /// a guest everywhere else — so the owner need not register the nick
    /// on every network the bot joins. A bare `irc:xenia` is unchanged:
    /// network-agnostic.
    #[test]
    fn an_account_identity_scoped_to_a_network() {
        let cfg = cfg_from_yaml(
            r#"
users:
  - name: wrath
    role: owner
    identities: ["irc:xenia@irc.synirc.net", "irc:lewrker"]
default_role: guest
"#,
        );
        let scoped = |net: &str| SenderContext {
            network: Some(net.into()),
            platform: Some("irc".into()),
            ..Default::default()
        };
        // The scoped account matches on its own network …
        assert_eq!(
            Principal::resolve_ctx(Some("irc:xenia"), &scoped("irc.synirc.net"), &cfg).role,
            Role::Owner,
            "scoped account resolves on its own network"
        );
        // … and is a stranger everywhere else.
        assert_eq!(
            Principal::resolve_ctx(Some("irc:xenia"), &scoped("irc.IRC4Fun.net"), &cfg).role,
            Role::Guest,
            "the same account on another network is NOT the owner"
        );
        // The bare form is network-agnostic, as before.
        assert_eq!(
            Principal::resolve_ctx(Some("irc:lewrker"), &scoped("irc.IRC4Fun.net"), &cfg).role,
            Role::Owner,
            "a bare account identity still matches on any network"
        );
    }

    /// A hostmask carrying an `@` must NOT be read as a scoped account:
    /// `*@host` and `nick!u@host` are masks, not `account@network`.
    #[test]
    fn a_hostmask_is_not_a_scoped_account() {
        let cfg = cfg_from_yaml(
            r#"
entries:
  - identity: "*!~a@86B64C8E:81F44426:DD62833D:IP"
    network: "irc.synirc.net"
    role: user
default_role: guest
"#,
        );
        let p = Principal::resolve_ctx(
            None,
            &irc_mask_ctx("irc.synirc.net", "a!~a@86B64C8E:81F44426:DD62833D:IP"),
            &cfg,
        );
        assert_eq!(p.role, Role::User, "the mask entry still matches as a mask");
    }

    // ── per-subject budgets ──────────────────────────────────────────

    /// A subject entry beats the role ceiling for that budget; a budget
    /// the subject leaves unset falls through to the role.
    #[test]
    fn a_subject_budget_overrides_the_role_for_that_budget_only() {
        let mut cfg = cfg_from_yaml("default_role: user");
        cfg.limits.by_subject.insert(
            "~a@host".to_string(),
            SubjectBudgets {
                tokens_per_day: Some(5_000_000),
                ..Default::default()
            },
        );
        // tokens/day: the subject's own number, not the role's 2M.
        assert_eq!(
            cfg.limits
                .tokens_per_day_for_subject(Role::User, "irc:net/a!~a@host", "~a@host"),
            5_000_000
        );
        // turns/hour: unset on the subject → the role's number.
        assert_eq!(
            cfg.limits
                .turns_per_hour_for_subject(Role::User, "irc:net/a!~a@host", "~a@host"),
            cfg.limits.turns_per_hour_for_role(Role::User),
            "an unset budget falls through to the role"
        );
        // Another caller, same role, no subject entry: the role's 2M.
        assert_eq!(
            cfg.limits
                .tokens_per_day_for_subject(Role::User, "irc:net/b!~b@host", "~b@host"),
            cfg.limits.tokens_per_day_for_role(Role::User)
        );
    }

    /// The subject is checked as well as the budget key, so a cap
    /// spelled `irc:xenia` lands on a caller keyed `irc:xenia`.
    #[test]
    fn a_subject_budget_matches_the_literal_subject_too() {
        let mut cfg = cfg_from_yaml("default_role: user");
        cfg.limits.by_subject.insert(
            "irc:xenia".to_string(),
            SubjectBudgets {
                tokens_per_day: Some(1),
                ..Default::default()
            },
        );
        assert_eq!(
            cfg.limits
                .tokens_per_day_for_subject(Role::User, "irc:xenia", "irc:xenia"),
            1
        );
    }

    /// `/limits set tokens-per-day <subject> <n>` round-trips through the
    /// store and lands in `by_subject`, not `by_role` — and a subject
    /// with a colon in it (`irc:xenia`) survives the key parse that
    /// once split on the LAST colon and lost the account scheme.
    #[test]
    fn a_runtime_subject_limit_round_trips_through_the_store() {
        let _g = crate::testenv::lock();
        let dir = std::env::temp_dir().join(format!(
            "veles-subject-limit-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.join("db")).unwrap(),
        ));

        set_runtime_budget(&store, Budget::TokensPerDay, "irc:xenia", 7_500_000).unwrap();
        // A bare word is still a role, not a subject.
        set_runtime_budget(&store, Budget::TokensPerDay, "user", 3_000_000).unwrap();

        let cfg = with_runtime_limits(cfg_from_yaml("default_role: guest"), &store);
        // The subject override is keyed verbatim, scheme and all.
        assert_eq!(
            cfg.limits
                .tokens_per_day_for_subject(Role::User, "irc:xenia", "irc:xenia"),
            7_500_000
        );
        // … and the role override is untouched by it.
        assert_eq!(
            cfg.limits
                .tokens_per_day_for_subject(Role::User, "irc:someone", "irc:someone"),
            3_000_000
        );
    }

    /// IRC casefolds hosts and the wire mask preserves the server's
    /// case, so a config keyed lowercase must land on a caller whose
    /// budget key arrived uppercase.
    #[test]
    fn a_subject_budget_matches_case_insensitively() {
        let mut cfg = cfg_from_yaml("default_role: user");
        cfg.limits.by_subject.insert(
            "~a@86b64c8e:81f44426:dd62833d:ip".to_string(),
            SubjectBudgets {
                tokens_per_day: Some(50_000_000),
                ..Default::default()
            },
        );
        assert_eq!(
            cfg.limits.tokens_per_day_for_subject(
                Role::User,
                "irc:irc.synirc.net/a!~a@86B64C8E:81F44426:DD62833D:IP",
                "~a@86B64C8E:81F44426:DD62833D:IP",
            ),
            50_000_000,
            "the lowercase config key still lands on the uppercase wire key"
        );
    }

    // ── budget_key: the second identity a budget is counted against ──

    fn irc_mask_ctx(network: &str, mask: &str) -> SenderContext {
        SenderContext {
            mask: Some(mask.into()),
            network: Some(network.into()),
            platform: Some("irc".into()),
            channel: Some("#c".into()),
            ..Default::default()
        }
    }

    /// A `/nick` is free on a services-less network; it must NOT mint a
    /// fresh budget. Two masks differing only in nick share one
    /// budget_key, because the host is the part a rename cannot shake.
    #[test]
    fn a_nick_change_does_not_reset_the_budget() {
        let cfg = cfg_from_yaml("default_role: guest");
        let a =
            Principal::resolve_ctx(None, &irc_mask_ctx("irc4fun", "alice!u@host.example"), &cfg);
        let b = Principal::resolve_ctx(
            None,
            &irc_mask_ctx("irc4fun", "alice2!u@host.example"),
            &cfg,
        );
        // The identities stay distinct — sessions must not merge.
        assert_ne!(a.subject, b.subject, "a nick change is still a new subject");
        assert_eq!(
            a.budget_key(),
            b.budget_key(),
            "but the BUDGET is keyed on what survives the rename: u@host.example"
        );
        assert_eq!(a.budget_key(), "u@host.example");
    }

    /// Two nicks on the same host are one budget — the bouncer/shared-
    /// connection case. This is the intended coarsening, not a leak:
    /// budgets are about money, not privacy, and the sessions stay split.
    #[test]
    fn two_nicks_one_host_share_a_budget_not_a_session() {
        let cfg = cfg_from_yaml("default_role: guest");
        let a = Principal::resolve_ctx(None, &irc_mask_ctx("irc4fun", "n1!u@host.example"), &cfg);
        let b = Principal::resolve_ctx(None, &irc_mask_ctx("irc4fun", "n2!u@host.example"), &cfg);
        assert_eq!(a.budget_key(), b.budget_key());
        assert_ne!(a.subject, b.subject);
    }

    /// A verified account beats the mask: the budget follows the
    /// NickServ identity, which no amount of `/nick` or host-hopping
    /// changes.
    #[test]
    fn a_verified_account_is_the_budget_key() {
        let cfg = cfg_from_yaml("default_role: guest");
        let ctx = irc_mask_ctx("irc4fun", "alice!u@host.example");
        let p = Principal::resolve_ctx(Some("irc:alice"), &ctx, &cfg);
        assert_eq!(p.budget_key(), "irc:alice");
        // Same account behind a DIFFERENT mask (host hop, new nick):
        // the budget still follows the account, not the wire identity.
        let ctx2 = irc_mask_ctx("irc4fun", "alice_|u@other.example");
        let p2 = Principal::resolve_ctx(Some("irc:alice"), &ctx2, &cfg);
        assert_eq!(
            p2.budget_key(),
            "irc:alice",
            "the account, not the mask, pays"
        );
        assert_eq!(p.budget_key(), p2.budget_key());
    }

    /// No account, no mask (a platform that told us nothing): the
    /// budget key falls back to the subject, which for an anonymous
    /// caller is already chat-scoped.
    #[test]
    fn an_anonymous_caller_budgets_on_its_subject() {
        let cfg = cfg_from_yaml("default_role: guest");
        let ctx = SenderContext {
            platform: Some("irc".into()),
            channel: Some("#c".into()),
            ..Default::default()
        };
        let p = Principal::resolve_ctx(None, &ctx, &cfg);
        assert_eq!(p.budget_key(), p.subject);
    }

    // ── [acl.strangers]: the door, as opposed to the permissions ─────

    fn tg_ctx() -> SenderContext {
        SenderContext {
            platform: Some("telegram".into()),
            chat_id: Some("123456789".into()),
            direct: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_connector_can_be_told_to_ignore_everyone_no_rule_names() {
        let cfg: AclConfig = serde_yaml::from_str(
            "default_role: guest\n\
             strangers: {telegram: ignore, IRC: user}\n\
             users:\n\
             \x20 - name: wrath\n\
             \x20   role: owner\n\
             \x20   identities: [\"telegram:123456789\"]",
        )
        .unwrap();

        assert_eq!(cfg.strangers_on(Some("telegram")), StrangerPolicy::Ignore);
        // The key is matched case-insensitively: `SenderContext.platform`
        // is whatever the adapter spelled, and a policy that silently
        // matches nothing is the failure this section exists to prevent.
        assert_eq!(
            cfg.strangers_on(Some("irc")),
            StrangerPolicy::Role(Role::User)
        );
        assert_eq!(cfg.strangers_on(Some("a2a")), StrangerPolicy::DefaultRole);
        assert_eq!(cfg.strangers_on(None), StrangerPolicy::DefaultRole);

        // And who counts as a stranger: a RULE, not a role. The owner is
        // named; nobody else on this connector is.
        assert!(!cfg.names_no_rule(Some("telegram:123456789"), &tg_ctx()));
        assert!(cfg.names_no_rule(Some("telegram:111222333"), &tg_ctx()));
    }

    #[test]
    fn someone_pinned_to_guest_has_been_thought_about_and_is_not_a_stranger() {
        // The distinction the door turns on. `roles` puts this person at
        // the same rung the default would have, so a role comparison
        // could not tell them apart from a stranger — but one of them was
        // written down on purpose.
        let cfg: AclConfig = serde_yaml::from_str(
            "default_role: guest\nstrangers: {telegram: ignore}\nroles: {\"telegram:42\": guest}",
        )
        .unwrap();
        assert_eq!(
            Principal::resolve_ctx(Some("telegram:42"), &tg_ctx(), &cfg).role,
            Role::Guest
        );
        assert!(
            !cfg.names_no_rule(Some("telegram:42"), &tg_ctx()),
            "an explicit guest pin is an invitation, not a fall-through"
        );
    }

    #[test]
    fn an_unreadable_strangers_value_fails_closed() {
        // A typo here must not quietly reopen the door. Closed is the
        // harmless direction for a policy that only ever applies to
        // people nobody has vouched for.
        let cfg: AclConfig =
            serde_yaml::from_str("strangers: {telegram: guset}\ndefault_role: guest").unwrap();
        assert_eq!(cfg.strangers_on(Some("telegram")), StrangerPolicy::Ignore);
    }

    #[test]
    fn a_strangers_only_acl_is_not_dormant() {
        // `is_configured` is what separates curated mode from the legacy
        // one where EVERY sender resolves as owner. An operator who
        // writes only a door policy has unmistakably declared an ACL, and
        // reading that as "no ACL" would hand the whole world owner.
        let cfg: AclConfig = serde_yaml::from_str("strangers: {telegram: ignore}").unwrap();
        assert!(cfg.is_configured(), "a door is an access-control rule");
        assert_eq!(
            Principal::resolve_ctx(Some("telegram:111222333"), &tg_ctx(), &cfg).role,
            Role::Guest,
            "curated mode, not the dormant owner-for-everyone mode"
        );
    }

    #[test]
    fn legacy_default_is_owner_when_unconfigured() {
        // Legacy single-user mode: no [acl] config → the adapter
        // allowlist is the trust boundary → full access preserved
        // (pre-ACL behavior unchanged for existing deployments).
        let cfg = AclConfig::default();
        assert!(!cfg.is_configured());
        let p = Principal::resolve(Some("irc:anyone"), &cfg);
        assert_eq!(p.role, Role::Owner);
        assert!(p.has(Cap::Chat) && p.has(Cap::Shell) && p.has(Cap::MemoryRead));
    }

    #[test]
    fn curated_default_is_guest() {
        let cfg = cfg_from_yaml("owner: [\"irc:wrath\"]");
        let p = Principal::resolve(Some("irc:anyone"), &cfg);
        assert_eq!(p.role, Role::Guest);
        // A guest is read-only, not mute (USER, 2026-09-09). An
        // operator who wants strangers ignored says so:
        // `default_role = "banned"`.
        assert!(p.has(Cap::Chat) && p.has(Cap::FileRead));
        assert!(!p.has(Cap::FileWrite) && !p.has(Cap::Shell));
        let cfg = cfg_from_yaml("owner: [\"irc:wrath\"]\ndefault_role: banned");
        let p = Principal::resolve(Some("irc:anyone"), &cfg);
        assert_eq!(p.role, Role::Banned);
        assert!(!p.has(Cap::Chat));
    }

    #[test]
    fn unverified_is_guest_only_in_curated_mode() {
        // Legacy: unverified senders keep full access (open-mode IRC
        // deployments without account tags must not regress).
        let cfg = AclConfig::default();
        let p = Principal::resolve(None, &cfg);
        assert_eq!(p.role, Role::Owner);
        // Curated: unverified = guest floor, which is read-only rather
        // than mute.
        let cfg = cfg_from_yaml("owner: [\"irc:o\"]");
        let p = Principal::resolve(None, &cfg);
        assert_eq!(p.role, Role::Guest);
        assert!(p.has(Cap::CmdStatus) && p.has(Cap::Chat));
        assert!(!p.has(Cap::FileWrite) && !p.has(Cap::Shell));
    }

    #[test]
    fn banned_outranks_owner_listing() {
        let cfg = cfg_from_yaml("owner: [\"irc:x\"]\nroles: {\"irc:x\": banned}");
        let p = Principal::resolve(Some("irc:x"), &cfg);
        assert_eq!(p.role, Role::Banned);
        assert!(!p.has(Cap::Chat));
    }

    #[test]
    fn extra_and_deny_caps() {
        let cfg = cfg_from_yaml(
            "owner: [\"irc:o\"]\nextra_caps: {\"irc:friend\": [\"shell\"]}\ndeny_caps: {\"irc:friend\": [\"file-write\"]}",
        );
        let p = Principal::resolve(Some("irc:friend"), &cfg);
        // default role under curated config is guest, plus granted shell
        assert!(p.has(Cap::Shell));
        assert!(!p.has(Cap::FileWrite), "the denial applies over the floor");
    }

    #[test]
    fn curated_mode_downgrades_to_guest() {
        // The moment ANY acl key exists, unlisted verified senders are
        // guests — adding config must never widen legacy access.
        let cfg = cfg_from_yaml("extra_caps: {\"irc:friend\": [\"shell\"]}");
        let p = Principal::resolve(Some("irc:other"), &cfg);
        assert_eq!(p.role, Role::Guest);
        assert!(
            !p.has(Cap::Shell),
            "the guest floor grants nothing that writes"
        );
    }

    #[test]
    fn trusted_gets_memory_read_not_write() {
        let cfg = cfg_from_yaml("roles: {\"irc:t\": trusted}");
        let p = Principal::resolve(Some("irc:t"), &cfg);
        assert!(p.has(Cap::MemoryRead) && p.has(Cap::Skills));
        assert!(!p.has(Cap::MemoryWrite));
        // Shell/FileWrite ARE granted below admin now — but
        // sandbox-scoped by construction: the tool layer routes every
        // env-touching call from a below-admin principal into that
        // principal's container, so the cap never buys host access
        // (user_envs.rs owns that invariant).
        assert!(p.has(Cap::Shell) && p.has(Cap::FileWrite));
    }

    /// Cap sets must be MONOTONIC in role — a more trusted role with
    /// strictly fewer default caps would invert the whole lattice
    /// (deny_caps could "promote" someone by demotion). The
    /// sandbox-scoped Shell/FileWrite grants made this a real risk:
    /// User gained them first and Trusted had to follow.
    #[test]
    fn default_caps_are_monotonic_in_role() {
        let order = [
            Role::Guest,
            Role::User,
            Role::Trusted,
            Role::Admin,
            Role::Owner,
        ];
        for w in order.windows(2) {
            let lower = w[0].default_caps();
            let higher = w[1].default_caps();
            // Owner is the literal set {All} — everything else is
            // implied by `has`, so compare with has-semantics.
            let implies = higher.contains(&Cap::All) || lower.iter().all(|c| higher.contains(c));
            assert!(
                implies,
                "{} caps must be a superset of {} — missing: {:?}",
                w[1].as_str(),
                w[0].as_str(),
                lower.difference(&higher).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn media_gen_is_a_user_cap_not_an_admin_perk() {
        // USER, 2026-09-11: "users were supposed to have access to
        // image-gen, that was part of the previous session." It was
        // admin-only because delivery used to be an open file channel;
        // the marker round made delivery a judged capability, so the
        // grant could land. A mutation that lifts MediaGen back out of
        // User (or Trusted, which must stay a superset) fails HERE,
        // naming the regression as a policy break and not as a test
        // fixture drift.
        //
        // Tts joined it on the same reasoning (USER, 2026-09-11:
        // "users should have access to tts though because it's
        // cheap"). Both caps are asserted in ONE loop because they are
        // one policy — "a user may spend the cheap kinds of provider
        // money on their own request" — and splitting them into two
        // tests is how one of them quietly drifts.
        for cap in [Cap::MediaGen, Cap::Tts] {
            for role in [Role::User, Role::Trusted, Role::Admin] {
                assert!(
                    role.default_caps().contains(&cap),
                    "{} must hold {} by default",
                    role.as_str(),
                    cap.as_str()
                );
            }
            assert!(
                !Role::Guest.default_caps().contains(&cap),
                "guests are read-only by directive — {} spends provider money",
                cap.as_str()
            );
        }
        assert!(Role::Owner.default_caps().contains(&Cap::All));
    }

    #[test]
    fn principal_env_round_trips_and_fails_closed() {
        // The delegate contract in two directions: a set var decodes
        // back into the same principal (the child's gate binds to the
        // CALLER), and a var that does not decode is an ERROR, not a
        // silent fallthrough to the owner surface a bare `veles chat`
        // is — "unparseable ⇒ owner" is the fail-open the delegate fix
        // exists to close, so this is the test that mutation kills.
        let _g = crate::testenv::lock();
        std::env::remove_var(PRINCIPAL_ENV);
        assert!(principal_from_env().unwrap().is_none());

        let p = Principal {
            id: Some("irc:someone".into()),
            subject: "user:someone".into(),
            role: Role::Trusted,
            caps: Role::Trusted.default_caps(),
            rule_context_limit: Some(150_000),
            mask: Some("someone!u@h".into()),
            via: Some("entry *!*@example".into()),
            ticket_role: None,
        };
        std::env::set_var(PRINCIPAL_ENV, serde_json::to_string(&p).unwrap());
        let back = principal_from_env().unwrap().expect("must decode");
        assert_eq!(back, p, "the child must see exactly the caller");

        std::env::set_var(PRINCIPAL_ENV, "{not json");
        assert!(
            principal_from_env().is_err(),
            "a set but unreadable var must stop the boot, not resolve as owner"
        );
        std::env::remove_var(PRINCIPAL_ENV);
    }

    #[test]
    fn an_env_claim_of_owner_is_only_honoured_when_the_acl_agrees() {
        // The comment beside PRINCIPAL_ENV used to claim that forging it
        // "can only make a child look LESS privileged than its caller,
        // never more". It could not: role and caps were deserialized
        // verbatim, so `VELES_SESSION_PRINCIPAL='{"role":"Owner",…}'
        // veles chat` was an owner agent — the Admin→Owner boundary that
        // `media::deliverable` explicitly leans on ("an admin is a
        // trusted operator of the bot, not of the host").
        //
        // The fix is narrow on purpose. Only the OWNER claim is checked,
        // and only against the ACL's own answer for that identity, so a
        // real owner delegating from a connector still gets an owner
        // child while everything below owner — including channel-scoped
        // grants the re-resolution could not reproduce — is untouched.
        let owner_claim = |subject: &str, id: Option<&str>| Principal {
            id: id.map(String::from),
            subject: subject.into(),
            role: Role::Owner,
            caps: Role::Owner.default_caps(),
            rule_context_limit: None,
            mask: None,
            via: None,
            ticket_role: None,
        };

        let mut acl = AclConfig {
            default_role: Some("guest".into()),
            ..Default::default()
        };
        acl.owner.push("irc:realowner".into());
        assert!(acl.is_configured());

        // The ACL agrees: the claim stands.
        let kept = clamp_owner_claim(owner_claim("user:realowner", Some("irc:realowner")), &acl);
        assert_eq!(kept.role, Role::Owner);
        assert!(kept.caps.contains(&Cap::All));

        // The ACL does not: demoted to admin, and `via` says why rather
        // than leaving a silent downgrade to be discovered later.
        let cut = clamp_owner_claim(owner_claim("user:pretender", Some("irc:pretender")), &acl);
        assert_ne!(cut.role, Role::Owner, "a forged owner claim must not stand");
        assert!(
            !cut.caps.contains(&Cap::All),
            "and must not keep the capability that means owner"
        );
        assert!(
            cut.via.as_deref().unwrap_or("").contains("acl"),
            "the demotion explains itself: {:?}",
            cut.via
        );

        // Below owner is left exactly alone — this is not a general
        // re-resolution, and a trusted caller whose grant is scoped to
        // one channel must not be demoted by a check that cannot see the
        // channel.
        let trusted = Principal {
            role: Role::Trusted,
            caps: Role::Trusted.default_caps(),
            ..owner_claim("user:scoped", Some("irc:scoped"))
        };
        assert_eq!(clamp_owner_claim(trusted.clone(), &acl), trusted);

        // A DORMANT acl resolves everyone as owner anyway, so there is
        // nothing to disagree with and nothing to clamp.
        let dormant = AclConfig::default();
        let d = clamp_owner_claim(owner_claim("user:anyone", None), &dormant);
        assert_eq!(d.role, Role::Owner);
    }

    /// The user-env contract, pinned at the cap layer: below admin the
    /// shell/file caps exist and are sandbox-scoped; admin keeps them
    /// with host access; guest never had them.
    #[test]
    fn shell_file_caps_span_the_ladder() {
        assert!(!Role::Guest.default_caps().contains(&Cap::Shell));
        assert!(Role::User.default_caps().contains(&Cap::Shell));
        assert!(Role::User.default_caps().contains(&Cap::FileWrite));
        assert!(Role::Trusted.default_caps().contains(&Cap::Shell));
        assert!(Role::Admin.default_caps().contains(&Cap::Shell));
    }

    #[test]
    fn tool_classification_complete_for_builtins() {
        for t in [
            "terminal",
            "read_file",
            "write_file",
            "patch",
            "search_files",
            "memory",
            "skills_list",
            "skill_view",
            "skill_manage",
            "web_search",
            "web_extract",
            "browser_navigate",
            "computer_use",
            "vision_analyze",
            "generate_image",
            "transcribe_audio",
            "text_to_speech",
            "delegate_task",
            "cronjob",
            "a2a",
            "session_search",
            "session_export",
            "todo",
            "clarify",
            "open_preview",
            "focus_pane",
            "tool_search",
            "tool_describe",
            "tool_call",
            "execute_code",
            // Added 2026-09-09: each of these was reachable with NO
            // gate at all, because `tool_cap` returning None is read
            // as "no gate" by the tool loop.
            "monitor",
            "shell",
            "present_plan",
            "show_plan",
            "tcl_eval",
            "secret_list",
            "secret_request",
            "api_request",
        ] {
            assert!(tool_cap(t).is_some(), "tool {t} must be classified");
        }
        // Host shells by another name need the shell cap.
        assert_eq!(tool_cap("monitor"), Some(Cap::Shell));
        assert_eq!(tool_cap("shell"), Some(Cap::Shell));
        // Live Tcl inside veles itself is owner-only, and the denial
        // says so rather than naming an ungrantable capability.
        assert_eq!(tool_cap("tcl_eval"), Some(Cap::All));
        let d = tool_denial("tcl_eval", &Principal::guest());
        assert!(d.contains("owner-only"), "{d}");
        assert!(!d.contains("`all` capability"), "{d}");
        // MCP tools are named by their server, so they classify on the
        // prefix — the `tool_call` envelope's own cap never fires,
        // because the loop resolves to the inner name first.
        assert_eq!(tool_cap("mcp__files__read"), Some(Cap::Mcp));
        assert_eq!(tool_cap("mcp__"), Some(Cap::Mcp));
        // The IRC adapter's own tool stays unclassified HERE and gates
        // per ACTION inside itself (say vs history are not one
        // permission); veles-irc owns that check.
        assert!(tool_cap("irc").is_none());
    }

    #[test]
    fn denial_names_the_cap() {
        let p = Principal::guest();
        let d = tool_denial("terminal", &p);
        assert!(d.contains("shell") && d.contains("terminal"));
    }

    #[test]
    fn builtin_tiers() {
        assert_eq!(builtin_cap("status"), Some(Cap::CmdStatus));
        // `/acl`, `/sandbox` and `/limits` are three commands whose
        // BARE form answers a question about the caller themselves —
        // what may I do, which box am I in, what is my ceiling — and
        // whose subcommands manage the rules. One tier for the whole
        // name cannot express that: at cmd-admin the self-report is
        // locked away from the person it is about, and at cmd-status
        // `grant` would be open to them. So the name is cmd-status and
        // the subcommands are tiered inside (`commands::caller_is`).
        for open in ["acl", "access", "sandbox", "sandboxes", "limits"] {
            assert_eq!(
                builtin_cap(open),
                Some(Cap::CmdStatus),
                "`{open}`'s bare form reports on the caller and must not need admin"
            );
        }
        // /model moved into the shared dispatcher (RM4.3). Its door is
        // status (bare /model reads); switching — a session-scoped
        // provider override, /body's tier — is checked inside it.
        assert_eq!(builtin_cap("model"), Some(Cap::CmdStatus));
        assert_eq!(builtin_cap("models"), Some(Cap::CmdStatus));
    }

    // ── S9: runtime grants from pairing codes ────────────────────────

    #[test]
    fn runtime_grant_resolves_after_config_roles() {
        // NOTE: set_runtime_pairing_store is a process-global OnceLock —
        // the FIRST install wins for the whole test binary. Serialize
        // with the shared testenv lock and reuse whichever store got
        // installed; grants are per-identity so tests do not collide.
        let _g = crate::testenv::lock();
        let dir = std::env::temp_dir().join(format!(
            "veles-pairing-acl-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.join("db")).unwrap(),
        ));
        set_runtime_pairing_store(store.clone());
        let store = runtime_pairing_store().expect("store installed").clone();

        // Curated config: owner pinned, friend NOT listed → would be
        // guest without a grant.
        let cfg = cfg_from_yaml("owner: [\"irc:o\"]");
        assert_eq!(
            Principal::resolve(Some("irc:friend"), &cfg).role,
            Role::Guest
        );

        // Owner creates a trusted code; friend redeems it.
        let (code, _) = crate::pairing::create(&store, "trusted", Some(3600)).unwrap();
        assert!(crate::pairing::valid_shape(&code));
        let role = crate::pairing::redeem(&store, &code, "irc:friend").unwrap();
        assert_eq!(role, "trusted");

        // Resolution now returns trusted WITHOUT any config change.
        let p = Principal::resolve(Some("irc:friend"), &cfg);
        assert_eq!(p.role, Role::Trusted);
        assert!(p.has(Cap::MemoryRead));
        // Shell IS in trusted's defaults now — sandbox-scoped below
        // admin (user_envs.rs); the grant still cannot reach the host.
        assert!(p.has(Cap::Shell));

        // A config role PIN still outranks the grant.
        let cfg2 = cfg_from_yaml("owner: [\"irc:o\"]\nroles: {\"irc:friend\": user}");
        assert_eq!(
            Principal::resolve(Some("irc:friend"), &cfg2).role,
            Role::User
        );

        // Revoke → back to guest.
        assert!(crate::pairing::revoke(&store, "irc:friend"));
        assert_eq!(
            Principal::resolve(Some("irc:friend"), &cfg).role,
            Role::Guest
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn banned_pin_outranks_runtime_grant() {
        // Same process-global store as the other pairing test (first
        // install wins); use a distinct identity to avoid collisions.
        let _g = crate::testenv::lock();
        let store = pairing_store();
        let (code, _) = crate::pairing::create(&store, "trusted", None).unwrap();
        // Fixed identity: serialized under testenv::lock, so no collision.
        let identity = "irc:ban-test";
        crate::pairing::redeem(&store, &code, identity).unwrap();
        // Explicit ban pin beats the grant (defense-in-depth).
        let cfg = cfg_from_yaml("owner: [\"irc:o\"]\nroles: {\"irc:ban-test\": banned}");
        assert_eq!(Principal::resolve(Some(identity), &cfg).role, Role::Banned);
        crate::pairing::revoke(&store, identity);
    }

    // ── IRC masks, scoped entries, cross-network users (2026-08-30) ──

    fn ctx(mask: &str, network: &str, channel: &str) -> SenderContext {
        SenderContext {
            mask: Some(mask.into()),
            network: Some(network.into()),
            channel: if channel.is_empty() {
                None
            } else {
                Some(channel.into())
            },
            platform: Some("irc".into()),
            chat_id: if channel.is_empty() {
                None
            } else {
                Some(channel.into())
            },
            direct: channel.is_empty(),
            handle: None,
            post: None,
        }
    }

    /// The process-global pairing store, installed by whichever test gets
    /// there first. Every test that grants shares it — `testenv::lock()`
    /// serializes them and the identities are distinct — and the return is
    /// re-read from the global rather than the local `Arc`, because a first
    /// install that lost a race would otherwise hand back a store
    /// resolution never reads.
    fn pairing_store() -> std::sync::Arc<std::sync::Mutex<crate::store::Store>> {
        if let Some(s) = runtime_pairing_store() {
            return s;
        }
        let dir = crate::testenv::kept_dir();
        let s = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&dir.join("db")).unwrap(),
        ));
        set_runtime_pairing_store(s);
        runtime_pairing_store().expect("just installed")
    }

    #[test]
    fn mask_entry_matches_with_network_and_channel_scope() {
        let cfg = cfg_from_yaml(
            "owner: [\"irc:o\"]\n\
             entries:\n\
             \x20 - identity: \"*!*@gateway/veles\"\n\
             \x20   network: libera\n\
             \x20   channel: \"#veles\"\n\
             \x20   role: trusted",
        );
        // In scope: trusted.
        let p =
            Principal::resolve_ctx(None, &ctx("bob!~b@gateway/veles", "libera", "#veles"), &cfg);
        assert_eq!(p.role, Role::Trusted);
        assert!(p.via.unwrap().contains("gateway/veles"));
        // Wrong network: falls through to the curated default (guest).
        let p = Principal::resolve_ctx(None, &ctx("bob!~b@gateway/veles", "oftc", "#veles"), &cfg);
        assert_eq!(p.role, Role::Guest);
        // Wrong channel: guest.
        let p =
            Principal::resolve_ctx(None, &ctx("bob!~b@gateway/veles", "libera", "#other"), &cfg);
        assert_eq!(p.role, Role::Guest);
        // No mask observed (e.g. a DM without prefix info): guest.
        let p = Principal::resolve_ctx(None, &SenderContext::default(), &cfg);
        assert_eq!(p.role, Role::Guest);
        // Scope is case-insensitive.
        let p =
            Principal::resolve_ctx(None, &ctx("bob!~b@GATEWAY/veles", "LIBERA", "#VELES"), &cfg);
        assert_eq!(p.role, Role::Trusted);
    }

    #[test]
    fn mask_entry_can_ban() {
        let cfg = cfg_from_yaml(
            "owner: [\"irc:o\"]\n\
             entries:\n\
             \x20 - identity: \"spam!*@*\"\n\
             \x20   role: banned",
        );
        let p = Principal::resolve_ctx(
            Some("irc:spam"),
            &ctx("spam!~s@any.host", "libera", "#x"),
            &cfg,
        );
        assert_eq!(p.role, Role::Banned);
        assert!(p.caps.is_empty(), "a ban carries no capabilities");
    }

    /// The consequence of "an exact identity beats every mask", stated
    /// as a test because it is the one place the new precedence can
    /// surprise someone.
    ///
    /// Before 2026-09-09 a ban from ANY source outranked everything, so
    /// `spam!*@*` = banned beat `roles = {"irc:spam": trusted}`. Now
    /// specificity decides, and an exact account id is the most
    /// specific thing there is — so the pin wins, and banning an
    /// account means naming the account.
    #[test]
    fn an_exact_pin_outranks_a_broad_mask_ban() {
        let with_pin = "owner: [\"irc:o\"]\n\
             roles: {\"irc:spam\": trusted}\n\
             entries:\n\
             \x20 - identity: \"spam!*@*\"\n\
             \x20   role: banned";
        let p = Principal::resolve_ctx(
            Some("irc:spam"),
            &ctx("spam!~s@any.host", "libera", ""),
            &cfg_from_yaml(with_pin),
        );
        assert_eq!(
            p.role,
            Role::Trusted,
            "the exact identity pin is the more specific rule"
        );
        // The fix, and the thing to document: pin the ACCOUNT.
        let banned_properly = "owner: [\"irc:o\"]\n\
             roles: {\"irc:spam\": banned}\n\
             entries:\n\
             \x20 - identity: \"spam!*@*\"\n\
             \x20   role: trusted";
        let p = Principal::resolve_ctx(
            Some("irc:spam"),
            &ctx("spam!~s@any.host", "libera", ""),
            &cfg_from_yaml(banned_properly),
        );
        assert_eq!(p.role, Role::Banned);
        // And with no account at all — the services-less case, which is
        // now the normal one — the mask ban is the only rule that
        // matches, so it decides.
        let p = Principal::resolve_ctx(
            None,
            &ctx("spam!~s@any.host", "libera", ""),
            &cfg_from_yaml(with_pin),
        );
        assert_eq!(p.role, Role::Banned);
    }

    /// USER's own example, verbatim in shape: everyone on the network
    /// is a user, one mask is the owner, one is banned — and the answer
    /// must not depend on which order they were written in.
    #[test]
    fn the_most_specific_mask_wins_in_every_declaration_order() {
        let everyone = "\x20 - identity: \"*!*@*-network-default-masking\"\n\
             \x20   role: user\n";
        let me = "\x20 - identity: \"me!me@1234-network-default-masking\"\n\
             \x20   role: owner\n";
        let jimmy = "\x20 - identity: \"jimmy!asshole@444-network-default-masking\"\n\
             \x20   role: banned\n";
        // Every permutation of the three rules resolves identically.
        for order in [
            [everyone, me, jimmy],
            [everyone, jimmy, me],
            [me, everyone, jimmy],
            [me, jimmy, everyone],
            [jimmy, everyone, me],
            [jimmy, me, everyone],
        ] {
            let cfg = cfg_from_yaml(&format!(
                "default_role: guest\nentries:\n{}{}{}",
                order[0], order[1], order[2]
            ));
            let owner = Principal::resolve_ctx(
                None,
                &ctx("me!me@1234-network-default-masking", "n", "#c"),
                &cfg,
            );
            assert_eq!(owner.role, Role::Owner, "order {order:?}");
            assert!(owner.has(Cap::All));
            let jim = Principal::resolve_ctx(
                None,
                &ctx("jimmy!asshole@444-network-default-masking", "n", "#c"),
                &cfg,
            );
            assert_eq!(jim.role, Role::Banned, "order {order:?}");
            let anyone = Principal::resolve_ctx(
                None,
                &ctx("someone!else@777-network-default-masking", "n", "#c"),
                &cfg,
            );
            assert_eq!(anyone.role, Role::User, "order {order:?}");
            // Someone from another host matches nothing and lands on
            // the configured default.
            let stranger =
                Principal::resolve_ctx(None, &ctx("x!y@some.other.host", "n", "#c"), &cfg);
            assert_eq!(stranger.role, Role::Guest, "order {order:?}");
        }
    }

    /// Ties go to the LOWER role: two rules of equal specificity
    /// disagreeing is a config the ACL cannot resolve, and fail-closed
    /// is its answer to everything it cannot resolve.
    #[test]
    fn an_equal_specificity_tie_resolves_downward() {
        let cfg = cfg_from_yaml(
            "default_role: guest\n\
             entries:\n\
             \x20 - identity: \"vic!id@host\"\n\
             \x20   role: admin\n\
             \x20 - identity: \"vic!id@host\"\n\
             \x20   role: user",
        );
        let p = Principal::resolve_ctx(None, &ctx("vic!id@host", "n", ""), &cfg);
        assert_eq!(p.role, Role::User);
    }

    /// Segment weighting: host beats ident beats nick, because that is
    /// the order of how hard each one is to choose.
    #[test]
    fn host_outranks_ident_outranks_nick() {
        let cfg = cfg_from_yaml(
            "default_role: guest\n\
             entries:\n\
             \x20 - identity: \"vic!*@*\"\n\
             \x20   role: admin\n\
             \x20 - identity: \"*!ident@*\"\n\
             \x20   role: trusted\n\
             \x20 - identity: \"*!*@pinned.host\"\n\
             \x20   role: user",
        );
        // All three match; the host rule is the most specific.
        let p = Principal::resolve_ctx(None, &ctx("vic!ident@pinned.host", "n", ""), &cfg);
        assert_eq!(p.role, Role::User);
        // Without the host, the ident rule wins over the nick rule.
        let p = Principal::resolve_ctx(None, &ctx("vic!ident@elsewhere", "n", ""), &cfg);
        assert_eq!(p.role, Role::Trusted);
        // Nick alone is the weakest — and still a rule.
        let p = Principal::resolve_ctx(None, &ctx("vic!other@elsewhere", "n", ""), &cfg);
        assert_eq!(p.role, Role::Admin);
    }

    /// A mask or a named user can carry capabilities now. Both
    /// `extra_caps` maps are keyed on an exact identity, so a
    /// mask-matched caller — everyone, on a services-less network —
    /// could never be given or docked one.
    #[test]
    fn a_mask_rule_carries_its_own_caps_and_limit() {
        let cfg = cfg_from_yaml(
            "default_role: guest\n\
             entries:\n\
             \x20 - identity: \"*!*@trusted.host\"\n\
             \x20   role: user\n\
             \x20   extra_caps: [\"memory-read\"]\n\
             \x20   deny_caps: [\"web\"]\n\
             \x20   max_context_tokens: 42000",
        );
        let p = Principal::resolve_ctx(None, &ctx("a!b@trusted.host", "n", ""), &cfg);
        assert_eq!(p.role, Role::User);
        assert!(p.has(Cap::MemoryRead), "the rule's extra cap applies");
        assert!(!p.has(Cap::Web), "the rule's denial applies");
        assert_eq!(p.max_context_tokens(&cfg), 42_000);
    }

    /// Scope narrows, and only breaks a specificity tie — a rule for
    /// one channel does not outrank a more precise mask.
    #[test]
    fn scope_breaks_ties_but_does_not_beat_precision() {
        let cfg = cfg_from_yaml(
            "default_role: guest\n\
             entries:\n\
             \x20 - identity: \"*!*@*\"\n\
             \x20   channel: \"#vip\"\n\
             \x20   role: trusted\n\
             \x20 - identity: \"joe!joe@joe.host\"\n\
             \x20   role: user",
        );
        // In #vip both match; the exact mask is more precise than the
        // channel scope.
        let p = Principal::resolve_ctx(None, &ctx("joe!joe@joe.host", "n", "#vip"), &cfg);
        assert_eq!(p.role, Role::User);
        // Anyone else in #vip gets the scoped rule.
        let p = Principal::resolve_ctx(None, &ctx("kim!k@k.host", "n", "#vip"), &cfg);
        assert_eq!(p.role, Role::Trusted);
        // …and nothing outside it.
        let p = Principal::resolve_ctx(None, &ctx("kim!k@k.host", "n", "#other"), &cfg);
        assert_eq!(p.role, Role::Guest);
    }

    /// `explain` is what `/acl explain` renders: the winner first, then
    /// what it beat. With specificity deciding, "why does this person
    /// have this role" cannot be answered by reading the file in order.
    #[test]
    fn explain_ranks_the_rules_it_beat() {
        let cfg = cfg_from_yaml(
            "default_role: guest\n\
             entries:\n\
             \x20 - identity: \"*!*@*\"\n\
             \x20   role: user\n\
             \x20 - identity: \"vic!id@host\"\n\
             \x20   role: admin",
        );
        let rules = Principal::explain(None, &ctx("vic!id@host", "n", ""), &cfg);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].role, Role::Admin, "the winner comes first");
        assert!(rules[0].describe().contains("vic!id@host"));
        assert_eq!(rules[1].role, Role::User);
        assert!(
            rules[0].mask_spec > rules[1].mask_spec,
            "the ranking is by specificity: {rules:?}"
        );
    }

    /// Per-role context ceilings (USER: 250k for any non-admin, 150k
    /// for guests, and the people running the thing are not on a
    /// budget).
    #[test]
    fn context_ceilings_follow_the_role() {
        let cfg = cfg_from_yaml("owner: [\"irc:o\"]");
        assert_eq!(cfg.limits.for_role(Role::Guest), 150_000);
        assert_eq!(cfg.limits.for_role(Role::User), 250_000);
        assert_eq!(cfg.limits.for_role(Role::Trusted), 250_000);
        assert_eq!(cfg.limits.for_role(Role::Admin), 0, "0 = unlimited");
        assert_eq!(cfg.limits.for_role(Role::Owner), 0);
        // Configurable per role, and the section default covers the
        // rungs with no entry of their own.
        let cfg = cfg_from_yaml(
            "owner: [\"irc:o\"]\nlimits:\n  default_max_context_tokens: 90000\n  by_role: {guest: 1000, admin: 500000}",
        );
        assert_eq!(cfg.limits.for_role(Role::Guest), 1_000);
        assert_eq!(cfg.limits.for_role(Role::User), 90_000);
        assert_eq!(cfg.limits.for_role(Role::Admin), 500_000);
    }

    /// The three budgets share one precedence chain, so the thing worth
    /// pinning is that they AGREE about who is exempt and disagree only
    /// where config says so.
    #[test]
    fn every_budget_exempts_admin_and_tightens_for_guests() {
        let cfg = cfg_from_yaml("owner: [\"irc:o\"]");
        for role in [Role::Admin, Role::Owner] {
            assert_eq!(cfg.limits.for_role(role), 0, "context/{role:?}");
            assert_eq!(
                cfg.limits.tokens_per_day_for_role(role),
                0,
                "tokens/{role:?}"
            );
            assert_eq!(
                cfg.limits.turns_per_hour_for_role(role),
                0,
                "turns/{role:?}"
            );
            assert_eq!(
                cfg.limits.concurrent_turns_for_role(role),
                0,
                "concurrency/{role:?}"
            );
        }
        // A guest is the cheapest rung on every axis.
        assert!(
            cfg.limits.tokens_per_day_for_role(Role::Guest)
                < cfg.limits.tokens_per_day_for_role(Role::User)
        );
        assert!(
            cfg.limits.turns_per_hour_for_role(Role::Guest)
                < cfg.limits.turns_per_hour_for_role(Role::User)
        );
        // A below-admin caller IS held concurrently by default: this is
        // the brake against one person opening twenty parallel turns.
        assert_eq!(cfg.limits.concurrent_turns_for_role(Role::User), 2);

        // Every axis is configurable per role, and the section default
        // covers the rungs with no entry of their own.
        let cfg = cfg_from_yaml(
            "owner: [\"irc:o\"]\n\
             limits:\n\
             \x20 default_max_tokens_per_day: 5000\n\
             \x20 tokens_per_day_by_role: {guest: 10, admin: 7}\n\
             \x20 default_max_turns_per_hour: 9\n\
             \x20 turns_per_hour_by_role: {guest: 1}\n\
             \x20 max_concurrent_turns: 4",
        );
        assert_eq!(cfg.limits.tokens_per_day_for_role(Role::Guest), 10);
        assert_eq!(cfg.limits.tokens_per_day_for_role(Role::User), 5_000);
        // An explicit entry outranks even the admin exemption — an
        // operator who writes a number for admin meant it.
        assert_eq!(cfg.limits.tokens_per_day_for_role(Role::Admin), 7);
        assert_eq!(cfg.limits.turns_per_hour_for_role(Role::Guest), 1);
        assert_eq!(cfg.limits.turns_per_hour_for_role(Role::Trusted), 9);
        assert_eq!(cfg.limits.concurrent_turns_for_role(Role::User), 4);
    }

    /// A config written before these keys existed must keep working and
    /// must not silently acquire a budget of zero (which would read as
    /// "unlimited" for the ceilings and "refuse everything" if anyone
    /// ever flipped the sense).
    #[test]
    fn a_config_without_the_new_budgets_gets_the_defaults() {
        let cfg = cfg_from_yaml("owner: [\"irc:o\"]\nlimits:\n  default_max_context_tokens: 90000");
        assert_eq!(cfg.limits.default_max_context_tokens, 90_000);
        assert_eq!(cfg.limits.tokens_per_day_for_role(Role::User), 2_000_000);
        assert_eq!(cfg.limits.turns_per_hour_for_role(Role::User), 60);
        assert_eq!(cfg.limits.concurrent_turns_for_role(Role::User), 2);
    }

    #[test]
    fn regex_segment_in_entry() {
        let cfg = cfg_from_yaml(
            "owner: [\"irc:o\"]\n\
             entries:\n\
             \x20 - identity: \"re:^wrath|nyx$!*@*\"\n\
             \x20   role: user",
        );
        let p = Principal::resolve_ctx(None, &ctx("wrath!~w@h", "n", ""), &cfg);
        assert_eq!(p.role, Role::User);
        let p = Principal::resolve_ctx(None, &ctx("NYX!~w@h", "n", ""), &cfg);
        assert_eq!(p.role, Role::User);
        // The regex is anchored: a longer nick does not match.
        let p = Principal::resolve_ctx(None, &ctx("wrath2!~w@h", "n", ""), &cfg);
        assert_eq!(p.role, Role::Guest);
    }

    /// Was `entries_resolve_in_config_order`, and the assertions are
    /// unchanged — the ANSWER is the same for a better reason. The
    /// gateway-host rule pins eight characters of host; the vip rule
    /// pins only a nick, which is the segment a client chooses for
    /// itself. So on that host the host rule decides, and off it the
    /// nick rule is the only one that matches.
    #[test]
    fn a_pinned_host_outranks_a_pinned_nick() {
        let cfg = cfg_from_yaml(
            "owner: [\"irc:o\"]\n\
             entries:\n\
             \x20 - identity: \"*!*@gateway/*\"\n\
             \x20   role: user\n\
             \x20 - identity: \"vip!*@*\"\n\
             \x20   role: admin",
        );
        let p = Principal::resolve_ctx(None, &ctx("vip!~v@gateway/x", "n", ""), &cfg);
        assert_eq!(p.role, Role::User);
        let p = Principal::resolve_ctx(None, &ctx("vip!~v@other.host", "n", ""), &cfg);
        assert_eq!(p.role, Role::Admin);
        // And declaring them the other way round changes nothing.
        let cfg = cfg_from_yaml(
            "owner: [\"irc:o\"]\n\
             entries:\n\
             \x20 - identity: \"vip!*@*\"\n\
             \x20   role: admin\n\
             \x20 - identity: \"*!*@gateway/*\"\n\
             \x20   role: user",
        );
        let p = Principal::resolve_ctx(None, &ctx("vip!~v@gateway/x", "n", ""), &cfg);
        assert_eq!(p.role, Role::User);
    }

    #[test]
    fn users_resolve_their_identities_across_platforms() {
        let cfg = cfg_from_yaml(
            "owner: [\"irc:o\"]\n\
             users:\n\
             \x20 - name: wrath\n\
             \x20   role: trusted\n\
             \x20   identities: [\"irc:wrath\", \"telegram:77\", \"nyx!~n@*.example.com\"]",
        );
        // Verified irc account, no context at all.
        assert_eq!(
            Principal::resolve_ctx(Some("irc:wrath"), &SenderContext::default(), &cfg).role,
            Role::Trusted
        );
        // Telegram identity of the same human.
        assert_eq!(
            Principal::resolve_ctx(Some("telegram:77"), &SenderContext::default(), &cfg).role,
            Role::Trusted
        );
        // A mask identity, matched through context.
        let p = Principal::resolve_ctx(None, &ctx("nyx!~n@box.example.com", "oftc", ""), &cfg);
        assert_eq!(p.role, Role::Trusted);
        assert!(p.via.unwrap_or_default().contains("wrath"));
        // Someone else's mask does not inherit the user.
        let p = Principal::resolve_ctx(None, &ctx("eve!~e@box.example.com", "oftc", ""), &cfg);
        assert_eq!(p.role, Role::Guest);
    }

    #[test]
    fn owner_via_mask_entry() {
        // The cross-network owner story: one mask that is the principal
        // on a server without accounts.
        let cfg = cfg_from_yaml(
            "entries:\n\
             \x20 - identity: \"wrath!~w@*.home.example\"\n\
             \x20   role: owner",
        );
        let p = Principal::resolve_ctx(None, &ctx("wrath!~w@box.home.example", "n", ""), &cfg);
        assert_eq!(p.role, Role::Owner);
        assert!(p.has(Cap::All));
        // A spoofed nick on a different host is NOT the owner.
        let p = Principal::resolve_ctx(None, &ctx("wrath!~w@evil.host", "n", ""), &cfg);
        assert_ne!(p.role, Role::Owner);
    }

    #[test]
    fn scoped_runtime_grant_and_user_resolve() {
        // Shared process-global store (first install wins): unique
        // identity strings keep these serialized tests from colliding.
        let _g = crate::testenv::lock();
        let store = pairing_store();
        let cfg = cfg_from_yaml("owner: [\"irc:o\"]");

        // A grant scoped to one network: it applies there and only there.
        crate::pairing::grant_scoped(
            &store,
            "irc:scoped-friend",
            "trusted",
            None,
            Some("libera".into()),
            None,
        )
        .unwrap();
        let on = Principal::resolve_ctx(
            Some("irc:scoped-friend"),
            &ctx("f!~f@h", "libera", ""),
            &cfg,
        );
        assert_eq!(on.role, Role::Trusted);
        let off =
            Principal::resolve_ctx(Some("irc:scoped-friend"), &ctx("f!~f@h", "oftc", ""), &cfg);
        assert_eq!(off.role, Role::Guest);

        // A mask grant bans even an unverified sender.
        crate::pairing::grant_scoped(&store, "troll!*@trollfarm.net", "banned", None, None, None)
            .unwrap();
        let p = Principal::resolve_ctx(None, &ctx("troll!~t@trollfarm.net", "libera", ""), &cfg);
        assert_eq!(p.role, Role::Banned);

        // A runtime user groups identities across networks.
        crate::pairing::user_save(
            &store,
            "pal",
            "user",
            &["irc:pal".into(), "pal!~p@*.pal.net".into()],
        )
        .unwrap();
        assert_eq!(
            Principal::resolve_ctx(Some("irc:pal"), &SenderContext::default(), &cfg).role,
            Role::User
        );
        let p = Principal::resolve_ctx(None, &ctx("pal!~p@box.pal.net", "any", ""), &cfg);
        assert_eq!(p.role, Role::User);

        crate::pairing::revoke(&store, "irc:scoped-friend");
        crate::pairing::revoke(&store, "troll!*@trollfarm.net");
        crate::pairing::user_delete(&store, "pal");
    }

    /// `precedence_key` compares scope BEFORE role and role BEFORE source
    /// rank, so an unscoped file line does not cover a grant that was
    /// written with a scope: inside that scope the grant wins. This is
    /// what `/acl list` has to know before it can call a store copy
    /// redundant — a listing that says "the file decides this" about a
    /// grant the file loses to is the same lie in the other direction.
    #[test]
    fn a_scoped_grant_beats_the_files_unscoped_line() {
        let _g = crate::testenv::lock();
        let store = pairing_store();
        let cfg = cfg_from_yaml(
            "owner: [\"irc:o\"]\nentries:\n  - identity: \"sc!~s@*.box\"\n    role: user\n",
        );
        crate::pairing::grant_scoped(
            &store,
            "sc!~s@*.box",
            "trusted",
            None,
            Some("libera".into()),
            Some("#veles".into()),
        )
        .unwrap();

        // Inside the scope: the grant's narrower rule wins.
        let inside = Principal::resolve_ctx(None, &ctx("sc!~s@a.box", "libera", "#veles"), &cfg);
        assert_eq!(inside.role, Role::Trusted, "the scoped grant decides here");
        // Outside it: only the file's line matches.
        let outside = Principal::resolve_ctx(None, &ctx("sc!~s@a.box", "oftc", "#other"), &cfg);
        assert_eq!(outside.role, Role::User, "the file decides everywhere else");

        crate::pairing::revoke(&store, "sc!~s@*.box");
    }

    /// The other half of the same question: a lower role wins a tie no
    /// matter which source it came from, so a store grant that DOWNGRADES
    /// a file-declared owner is live, not a redundant copy. That is the
    /// emergency brake `pairing::grant` documents, and it is why "the file
    /// also declares this identity" is not by itself an answer.
    #[test]
    fn a_runtime_downgrade_beats_the_file_that_declares_the_identity() {
        let _g = crate::testenv::lock();
        let store = pairing_store();
        let cfg = cfg_from_yaml("owner: [\"irc:boss\"]");
        crate::pairing::grant(&store, "irc:boss", "user", None).unwrap();
        let p = Principal::resolve_ctx(Some("irc:boss"), &SenderContext::default(), &cfg);
        assert_eq!(p.role, Role::User, "the lower role wins the tie");
        crate::pairing::revoke(&store, "irc:boss");

        // The same identity with a HIGHER store role loses to the file,
        // which is the case where the store copy really is inert.
        let cfg = cfg_from_yaml("roles:\n  \"irc:pal\": user\n");
        crate::pairing::grant_scoped(&store, "irc:pal", "trusted", None, None, None).unwrap();
        let p = Principal::resolve_ctx(Some("irc:pal"), &SenderContext::default(), &cfg);
        assert_eq!(
            p.role,
            Role::User,
            "the file's lower role wins, so the store copy changes nothing"
        );
        crate::pairing::revoke(&store, "irc:pal");
    }

    /// `/acl promote` decides what to write by comparing two rules with
    /// no caller in sight. That comparison has to be the one resolution
    /// runs, or the report is a retelling of the precedence rules and
    /// drifts the first time they change — which is what `/acl list` did
    /// when it announced that the file wins ties. So for every pair whose
    /// callers can meet, `pick` over the same two rules chooses the
    /// winner `compare_specs` names.
    #[test]
    fn the_comparison_promote_uses_is_the_one_resolution_runs() {
        use crate::ircmask::PatternRelation as Callers;
        use std::cmp::Ordering;
        let spec = |i: &str, r: Role, s: RuleSource| RuleSpec::new(i, r, s);
        let g = |i: &str, r: Role| spec(i, r, RuleSource::Grant);
        let e = |i: &str, r: Role| spec(i, r, RuleSource::Entry);

        let pairs: Vec<(RuleSpec, RuleSpec)> = vec![
            (
                g("irc:boss", Role::User),
                spec("irc:boss", Role::Owner, RuleSource::OwnerList),
            ),
            (g("irc:boss", Role::Trusted), e("irc:pal", Role::User)),
            (
                g("octo!*@*", Role::User),
                e("octo!*@gateway/veles", Role::Banned),
            ),
            (g("octo!*@*", Role::User), e("*!*@gateway/*", Role::Trusted)),
            (g("*!*@*", Role::Guest), e("o*!*@*", Role::User)),
            (g("irc:octo", Role::User), e("octo!*@*", Role::Banned)),
            (
                g("octo!*@*", Role::User).scoped(Some("libera"), None),
                e("octo!*@*", Role::Trusted).scoped(None, Some("#veles")),
            ),
            (
                g("octo!*@*", Role::User).scoped(Some("libera"), None),
                e("octo!*@*", Role::User).scoped(Some("libera"), None),
            ),
            (g("nyx!~n@box", Role::User), e("nyx!~n@box", Role::User)),
            (
                g("octo!*@*", Role::User).scoped(Some("libera"), None),
                e("octo!*@*", Role::User).scoped(Some("oftc"), None),
            ),
        ];
        for (a, b) in &pairs {
            let RuleRelation::Meets { order, .. } = compare_specs(a, b) else {
                // Disjoint: no caller matches both, so resolution never
                // has to choose between them and there is nothing to
                // agree about.
                continue;
            };
            if order == Ordering::Equal {
                // Indistinguishable: `pick` has no opinion to check.
                continue;
            }
            let ra = matched(a, &spec_shape(&a.identity).expect("parses"));
            let rb = matched(b, &spec_shape(&b.identity).expect("parses"));
            let both = [ra, rb];
            let winner = pick(&both).expect("two rules, one winner");
            let expected = if order == Ordering::Greater {
                &a.identity
            } else {
                &b.identity
            };
            assert_eq!(
                &winner.identity,
                expected,
                "resolution picked a different rule than the comparison named: \
                 `{}` ({}) against `{}` ({})",
                a.identity,
                a.source.as_str(),
                b.identity,
                b.source.as_str()
            );
        }

        // The case a listing that compares identity STRINGS gets wrong:
        // the file's narrower mask beats the grant where it matches, and
        // the grant still decides for every caller outside it. Calling
        // that "no rule in the file names this identity" is how an
        // operator ends up with two lines for one nick and no idea which
        // is in force.
        assert_eq!(
            compare_specs(
                &g("octo!*@*", Role::User),
                &e("octo!*@gateway/veles", Role::Banned)
            ),
            RuleRelation::Meets {
                callers: Callers::BWithinA,
                order: Ordering::Less,
                edge: Edge::MaskSpecificity
            }
        );
        // Crossing scopes: the grant is pinned to a network, the file's
        // entry to a channel. Each reaches callers the other does not, so
        // neither is a duplicate; where both match, the channel wins on
        // scope.
        assert_eq!(
            compare_specs(
                &g("octo!*@*", Role::User).scoped(Some("libera"), None),
                &e("octo!*@*", Role::Trusted).scoped(None, Some("#veles"))
            ),
            RuleRelation::Meets {
                callers: Callers::Overlap,
                order: Ordering::Less,
                edge: Edge::Scope
            }
        );
        // Two networks cannot meet, so neither line is redundant.
        assert_eq!(
            compare_specs(
                &g("octo!*@*", Role::User).scoped(Some("libera"), None),
                &e("octo!*@*", Role::User).scoped(Some("oftc"), None)
            ),
            RuleRelation::Disjoint
        );
        // An exact identity against a mask: they meet only on a caller
        // carrying both, so the caller set is unsaid — while the order is
        // still resolution's, exact identity first.
        assert_eq!(
            compare_specs(&g("irc:octo", Role::User), &e("octo!*@*", Role::Banned)),
            RuleRelation::Meets {
                callers: Callers::Unknown,
                order: Ordering::Greater,
                edge: Edge::ExactIdentity
            }
        );
        // Same identity and scope, lower role in the store: the store
        // wins the tie. That is the downgrade brake, and it is why "the
        // file declares this identity too" is not by itself an answer.
        assert_eq!(
            compare_specs(
                &g("irc:boss", Role::User),
                &spec("irc:boss", Role::Owner, RuleSource::OwnerList)
            ),
            RuleRelation::Meets {
                callers: Callers::Equal,
                order: Ordering::Greater,
                edge: Edge::LowerRole
            }
        );
        // Same identity, same scope, same role: only the source is left,
        // the file outranks the store, and this — only this — is inert.
        assert_eq!(
            compare_specs(&g("nyx!~n@box", Role::User), &e("nyx!~n@box", Role::User)),
            RuleRelation::Meets {
                callers: Callers::Equal,
                order: Ordering::Less,
                edge: Edge::Source
            }
        );
    }

    /// `subject` is the keying identity — sessions, sandboxes, ask
    /// binding and `/cancel` all use it. It is ALWAYS populated, which
    /// is the whole point: with no Services account (the normal case
    /// on many networks, and the posture this bot is built for) `id` is
    /// None, and everything that keyed on `id` used to collapse every
    /// stranger in a channel onto one session, one sandbox and one
    /// answerable approval.
    #[test]
    fn subject_is_always_populated_and_distinguishes_callers() {
        let cfg = cfg_from_yaml("owner: [\"irc:o\"]");
        // A verified account: the id.
        let p = Principal::resolve_ctx(Some("IRC:Wrath"), &SenderContext::default(), &cfg);
        assert_eq!(p.subject, "irc:wrath");
        // No account: the wire mask, scoped by platform and network, so
        // the same nick!user@host on two networks is two subjects.
        let a = Principal::resolve_ctx(None, &ctx("nyx!~n@box.example", "libera", "#c"), &cfg);
        let b = Principal::resolve_ctx(None, &ctx("nyx!~n@box.example", "oftc", "#c"), &cfg);
        assert_eq!(a.subject, "irc:libera/nyx!~n@box.example");
        assert_ne!(a.subject, b.subject);
        // Two different people in one channel are two subjects.
        let c = Principal::resolve_ctx(None, &ctx("eve!~e@box.example", "libera", "#c"), &cfg);
        assert_ne!(a.subject, c.subject);
        // Nothing observable at all: scoped to the conversation, which
        // is the honest answer when nothing else distinguishes them.
        let mut bare = SenderContext {
            platform: Some("telegram".into()),
            chat_id: Some("#Room".into()),
            ..Default::default()
        };
        assert_eq!(
            Principal::resolve_ctx(None, &bare, &cfg).subject,
            "anon:#room"
        );
        bare.chat_id = None;
        assert_eq!(
            Principal::resolve_ctx(None, &bare, &cfg).subject,
            "anon:unknown"
        );
    }

    /// A named user is ONE subject across networks and platforms, so
    /// the same human gets one session and one sandbox wherever they
    /// talk from. `[[acl.users]]` already grouped identities for the
    /// ROLE; it now groups them for keying too.
    #[test]
    fn a_named_user_is_one_subject_everywhere() {
        let cfg = cfg_from_yaml(
            "owner: [\"irc:o\"]\n\
             users:\n\
             \x20 - name: Wrath\n\
             \x20   role: trusted\n\
             \x20   identities: [\"irc:wrath\", \"telegram:77\", \"nyx!~n@*.example.com\"]",
        );
        let by_account = Principal::resolve_ctx(Some("irc:wrath"), &SenderContext::default(), &cfg);
        let by_telegram =
            Principal::resolve_ctx(Some("telegram:77"), &SenderContext::default(), &cfg);
        let by_mask =
            Principal::resolve_ctx(None, &ctx("nyx!~n@box.example.com", "oftc", ""), &cfg);
        assert_eq!(by_account.subject, "user:wrath");
        assert_eq!(by_telegram.subject, "user:wrath");
        assert_eq!(by_mask.subject, "user:wrath");
        assert_eq!(by_mask.role, Role::Trusted);
        // …and the platform id is still the matching identity.
        assert_eq!(by_account.id.as_deref(), Some("irc:wrath"));
        assert_eq!(by_mask.id, None);
        // The label names whichever one actually identified them.
        assert!(by_mask.label().contains("nyx!~n@box.example.com"));
        assert!(!by_mask.label().contains("unverified"));
    }

    #[test]
    fn legacy_mode_still_returns_owner() {
        // Entries/users are part of "configured": with NONE present the
        // legacy owner default must be untouched.
        let cfg = AclConfig::default();
        let p = Principal::resolve_ctx(Some("irc:anyone"), &ctx("a!b@c", "n", "#x"), &cfg);
        assert_eq!(p.role, Role::Owner);
    }
}

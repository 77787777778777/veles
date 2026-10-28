//! What an adapter observed about a sender.
//!
//! Moved from `veles_core::acl` (2026-10-07), which re-exports it. The
//! ACL's resolution (`Principal::resolve_ctx`) stays in core: a plugin
//! sends this and core decides.

use serde::{Deserialize, Serialize};

/// What the adapter observed about the sender, beyond the verified
/// identity string: the wire mask, and where the message arrived.
/// IRC fills all three; platforms with no notion of a hostmask (telegram)
/// leave it at default, and resolution degrades to exact-identity
/// matching exactly as before.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SenderContext {
    /// `nick!user@host` as seen on the wire.
    pub mask: Option<String>,
    /// The network (config name of the IRC server) it arrived on.
    pub network: Option<String>,
    /// The channel it arrived in; None for a query/DM.
    pub channel: Option<String>,
    /// Which connector this arrived on (`irc`, `telegram`, …). Filled
    /// by every adapter. Not IRC-specific on purpose: rules and
    /// sandbox assignments scope on it, so "guests on this connector
    /// are read-only" is one line wherever the connector came from.
    pub platform: Option<String>,
    /// The conversation id as the platform names it — a channel, a
    /// telegram chat id, a thread. `channel` above is the IRC spelling
    /// of the same thing; this one every connector can fill.
    pub chat_id: Option<String>,
    /// A one-to-one conversation (a query, a DM) rather than a room.
    pub direct: bool,
    /// The name the platform SHOWS for the sender (a Bluesky handle), for
    /// display only — slopdrop's `[nick]`/`$who`. Never matched by any
    /// rule: identity matching is `mask` (IRC) and the exact id. A handle
    /// put in `mask` instead (68a5d8c) made every unscoped `*!*@*`-style
    /// mask rule match every Bluesky sender, because `ircmask::split`
    /// reads a bare handle as nick=handle, user="", host="".
    pub handle: Option<String>,
    /// What the platform knows about the message beyond its text, as
    /// data a module may read (display only, never matched by a rule).
    /// atproto: `{post, thread}` — the post the mention arrived in and
    /// the thread above it, flattened (`veles_atproto::rich::post_context`),
    /// which the slopdrop bridge hands to Tcl as `$::bsky::post` and
    /// `$::bsky::thread`. Absent elsewhere, and never serialized when
    /// absent, so a receiver that predates it reads the same bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post: Option<serde_json::Value>,
}

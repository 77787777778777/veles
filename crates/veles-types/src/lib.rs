// An adapter/plugin/types crate: its spawn sites are its OWN work on its
// OWN runtime (a connection task, a test driver), not a module calling
// into core. The M3 hop (clippy.toml) is veles-core/veles-frontends
// business; when THIS crate becomes a module (R4/R6) its spawn sites are
// reviewed and the allow is dropped.
#![allow(clippy::disallowed_methods)]
//! What veles and its process plugins both speak.
//!
//! Until 2026-10-07 every platform adapter was a crate linked into the
//! gateway, and rebuilding one meant restarting all of them
//! (PLAN-plugins-installer.md). A process plugin is a separate
//! executable the gateway spawns and talks JSON-RPC to; this crate is
//! the part of the vocabulary both sides need, and nothing else:
//!
//! - [`bus`]: the inbound and outbound messages a platform exchanges with
//!   the gateway;
//! - [`SenderContext`]: what an adapter observed about a sender;
//! - [`ask`]: the shapes of a question for the human and its answer;
//! - [`jsonrpc`]: the two-way peer the gateway and a plugin run on;
//! - [`plugin`]: the process-plugin protocol on top of it.
//!
//! veles-core re-exports every type here at its old path
//! (`veles_core::bus::InboundMessage`, `veles_core::acl::SenderContext`,
//! `veles_core::ask::AskRequest`, …), so nothing that used them moved.

pub mod ask;
pub mod authn;
pub mod bus;
pub mod irc;
pub mod jsonrpc;
pub mod knob;
pub mod plugin;
mod sender;
pub mod text;

pub use knob::{Knob, Shape};
pub use sender::SenderContext;

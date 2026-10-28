//! veles-atproto-cmd — `/atproto`, the owner command for basic
//! administration of the bot's atproto account, as an in-process
//! MODULE (the shape of PLAN-plugins-installer.md R5). The module FILE
//! is crates/veles-module-atproto-cmd; this crate is the library it
//! exports.
//!
//! Why a module of its own: until now the only way to take one of the
//! bot's posts down was `/slopdrop delete`, which resolves its target
//! through the slopdrop bridge's 7-day ledger of `/tcl` replies — a
//! post that is not in that ledger (most bot posts are not) has no
//! delete, and basic account administration depended on the slopdrop
//! bridge being deployed at all (USER 2026-10-20: "I don't want basic
//! functionality to depend on slopdrop"). This module talks only to
//! the atproto plugin's GENERIC repo services, found through the
//! service registry (`veles_core::services`), never by plugin name.
//!
//! `/atproto delete <target>` is VERIFY-FIRST: it asks the plugin who
//! it acts as (`repo.whoami` — the session DID, the only authoritative
//! ownership statement), fetches the record (`repo.get_record`), and
//! only then deletes (`repo.delete_record`). A record in someone
//! else's repo is refused before any delete call is made; a record
//! already gone says so instead of erroring.

pub mod command;
pub mod module;

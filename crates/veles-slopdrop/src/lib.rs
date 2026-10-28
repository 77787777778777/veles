//! veles-slopdrop — the slopdrop bridge as an in-process MODULE
//! (PLAN-plugins-installer.md R5).
//!
//! WHAT IT IS. Tcl evaluated in somebody else's interpreter: slopdrop,
//! a persistent, git-backed interpreter with a decade of a channel's
//! procs, behind an HTTP API. veles keeps the channel, slopdrop runs
//! headless, and `tcl` goes over the bridge (`bridge`). On a PUBLIC
//! feed what a `/tcl` reply may carry is a Terms-of-Service question,
//! answered by the output check (`check`), the verdict store and the
//! shaping (`safety`), the ledger of every public reply (`withheld`) and
//! the owner's approve/delete through the atproto plugin's generic repo
//! services (`publish`). The state mirror (`mirror`) materializes
//! slopdrop's procs for the desktop editor; the trigger forwarder
//! (`triggers`) relays channel events; `/slopdrop` (`command`) is the
//! operator's door. A Bluesky reply's text art goes out as a picture
//! (`art`), checked by the vision model first.
//!
//! WHERE IT PLUGS IN. Core knows none of this. It has seams, and the
//! module fills them in `start` (`module.rs`): the Tcl backend slot and
//! the public-reply policy slot (`tcleval`), the trigger forwarder slot
//! (`triggers`), a pending-decision source (`syncask`), HTTP routes
//! (`moduleroutes`), a delivery observer (`bus`), a direct command
//! (`commands`), a tool with its cap, and the names of its secrets.
//! Every one is owner-tracked and purged when the module unloads: with
//! the module gone, `tcl` is the local sandbox again, a public `/tcl`
//! fails closed, a Slopdrop trigger row refuses with a reason, and the
//! mirror's routes answer 503.
//!
//! The module FILE is crates/veles-module-slopdrop; this crate is the
//! plain library (its tests run as any crate's).

pub mod art;
pub mod ask;
pub mod bridge;
pub mod bsky;
pub mod check;
pub mod command;
pub mod data;
pub mod eval;
pub mod magick;
pub mod mirror;
pub mod module;
pub mod public;
pub mod publish;
pub mod quotecap;
pub mod receipts;
pub mod routes;
pub mod safety;
pub mod scheme;
pub mod settings;
pub mod triggers;
pub mod withheld;

pub use settings::config_surface;

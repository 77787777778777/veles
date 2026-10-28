//! veles-lang-racket — the Racket language host (PLAN-plugins-installer.md
//! R9), a module over the shared runtime.
//!
//! Racket CS 9.3 (`libracketcs.a`, the three boot files and the collects
//! of the install `VELES_RACKET_HOME` names) booted ONCE per process on
//! one engine thread; a namespace and a custodian per script, dropped
//! on unload. The host and Racket talk JSON through a mailbox and a
//! pipe (engine.rs); `runtime.rkt` is the loop on the Racket side and
//! `veles.rkt` the API every script requires, and every op it offers
//! goes through `veles_core::scripthost::ScriptCx::call`, so what a
//! Racket plugin can do is what a Tcl plugin can do, by construction.
//!
//! The runtime is process-global and boots once: the module stays
//! mapped for the gateway's life (the module host reports it `kept
//! mapped`, by design), a script reload is a new namespace, not a new
//! engine, and a second copy of this module refuses to boot Racket
//! again (core's boot mark).

use veles_runtime::veles_core;

pub mod engine;
pub mod ffi;
pub mod module;

/// The manifest kind this host serves (`kind = "racket"` in plugin.toml).
pub const KIND: &str = "racket";

/// The loop on the Racket side: `veles/runtime` in the collection the
/// engine writes into its state dir.
pub const RUNTIME_RKT: &str = include_str!("runtime.rkt");
/// The API a script requires: `veles/main.rkt` in that collection.
pub const VELES_RKT: &str = include_str!("veles.rkt");

pub use engine::RacketEngine;
pub use module::{manifest, RacketHost};

/// The owner tag of script `name` (`racket:<name>`).
pub fn owner_tag_for(name: &str) -> &'static str {
    veles_core::scripthost::owner_tag_for(KIND, name)
}

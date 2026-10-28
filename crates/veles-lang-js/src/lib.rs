//! veles-lang-js — the JavaScript/TypeScript language host
//! (PLAN-plugins-installer.md R9), a module over the shared runtime.
//!
//! One QuickJS-ng runtime (rquickjs, async) for the process; one
//! context per script, dropped on unload. TypeScript is stripped by oxc
//! before the engine sees it. The host binds exactly three natives —
//! `__veles_call`, `__veles_log`, `__veles_sleep` — and `prelude.js`
//! builds the `veles` object every script uses on top of them; every
//! op goes through `veles_core::scripthost::ScriptCx::call`, so what a
//! JS plugin can do is what a Tcl plugin can do, by construction.
//!
//! The runtime is process-global: the module stays mapped for the
//! gateway's life (the module host reports it `kept-mapped`); a script
//! reload is a new context, not a new engine.

use veles_runtime::veles_core;

pub mod engine;
pub mod module;
pub mod transpile;

/// The manifest kind this host serves (`kind = "js"` in plugin.toml).
pub const KIND: &str = "js";

/// The API every script's context gets before its own code.
pub const PRELUDE: &str = include_str!("prelude.js");

pub use engine::JsEngine;
pub use module::{manifest, JsHost};

/// The owner tag of script `name` (`js:<name>`).
pub fn owner_tag_for(name: &str) -> &'static str {
    veles_core::scripthost::owner_tag_for(KIND, name)
}

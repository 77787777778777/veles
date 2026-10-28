//! veles-lang-lisp — the Common Lisp language host
//! (PLAN-plugins-installer.md R9), a module over the shared runtime.
//!
//! ECL (`libecl.so` of the install `VELES_ECL_HOME` names, its Boehm GC
//! inside) booted ONCE per process on one engine thread; a package per
//! script and a thread per handler, killed and deleted on unload. The
//! host and Lisp talk ASCII JSON through a mailbox (engine.rs);
//! `runtime.lisp` is the loop on the Lisp side and `veles.lisp` the API
//! every script's package uses, and every op it offers goes through
//! `veles_core::scripthost::ScriptCx::call`, so what a Lisp plugin can
//! do is what a Tcl plugin can do, by construction.
//!
//! The runtime is process-global and boots once: the module stays mapped
//! for the gateway's life (the module host reports it `kept mapped`, by
//! design), a script reload is a new package, not a new engine, and a
//! second copy of this module refuses to boot ECL again (core's boot
//! mark).

use veles_runtime::veles_core;

pub mod engine;
pub mod ffi;
pub mod module;

/// The manifest kind this host serves (`kind = "lisp"` in plugin.toml).
pub const KIND: &str = "lisp";

/// The loop on the Lisp side, loaded at boot (package VELES-RUNTIME).
pub const RUNTIME_LISP: &str = include_str!("runtime.lisp");
/// The API a script's package uses (package VELES).
pub const VELES_LISP: &str = include_str!("veles.lisp");

pub use engine::LispEngine;
pub use module::{manifest, LispHost};

/// The owner tag of script `name` (`lisp:<name>`).
pub fn owner_tag_for(name: &str) -> &'static str {
    veles_core::scripthost::owner_tag_for(KIND, name)
}

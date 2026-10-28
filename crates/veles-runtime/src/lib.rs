//! veles-runtime — the shared core library (PLAN-plugins-installer.md M1).
//!
//! One dylib that carries `veles-core`, `veles-frontends` and
//! `veles-types` plus their shared dependency graph (tokio, serde,
//! tracing, rustls, …). The gateway binary links it dynamically; an
//! in-process module is a Rust dylib linking the SAME file, so a module
//! calls core directly: same statics, same tokio, same tracing
//! subscriber.
//!
//! This is the `rustc_driver` / `bevy_dylib` pattern: the crate itself
//! is empty apart from re-exports — its only job is to exist as the
//! single shared artifact every side links.

// The gateway and every module name `veles_core` / `veles_frontends` /
// `veles_types` through THIS crate's paths — that is what routes their
// symbols through this one dylib. A downstream crate writes
// `veles_runtime::veles_core::…` (or `use veles_runtime::veles_core as
// veles_core;` and keeps its old paths). Naming them through here is
// what makes rustc link this .so, and it then takes the shared crates
// from it rather than from their rlibs.
pub use veles_core;
pub use veles_frontends;
pub use veles_install;
pub use veles_types;

// Crates a module commonly needs, already inside this dylib. A module
// names them through here and declares no dependency of its own on
// them: its own dependency could change the features cargo unifies onto
// the runtime's copy, and the module would then link a runtime the
// gateway does not have (`module::check` refuses it).
pub use reqwest;
pub use tokio;
pub use tracing;

// anyhow, for the module contract's `start` return type (a module's
// start/stop signatures mention it, and a module links it from here).
pub use anyhow;

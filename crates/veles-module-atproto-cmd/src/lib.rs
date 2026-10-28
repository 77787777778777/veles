//! The /atproto module: the dylib the module host loads (the shape of
//! PLAN-plugins-installer.md R5). Everything it runs is the
//! veles-atproto-cmd library (`veles_atproto_cmd::module` has the
//! host-facing shell); this crate only gives it the `export!`, so the
//! module's file carries the key and the `veles_module` static.

use veles_runtime::veles_core;

veles_core::export!(
    veles_atproto_cmd::module::AtprotoCmdModule,
    veles_atproto_cmd::module::manifest()
);

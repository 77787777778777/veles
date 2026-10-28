//! The slopdrop module: the dylib the module host loads
//! (PLAN-plugins-installer.md R5). Everything it runs is the
//! veles-slopdrop library (`veles_slopdrop::module` has the host-facing
//! shell); this crate only gives it the `export!`, so the module's file
//! carries the key and the `veles_module` static.

use veles_runtime::veles_core;

veles_core::export!(
    veles_slopdrop::module::SlopdropModule,
    veles_slopdrop::module::manifest()
);

//! The Telegram module: the dylib the module host loads
//! (PLAN-plugins-installer.md R6). Everything it runs is the veles-telegram
//! library (`veles_telegram::module` has the host-facing shell); this
//! crate only gives it the `export!`, so the module's file carries the
//! key and the `veles_module` static.

use veles_runtime::veles_core;

veles_core::export!(
    veles_telegram::module::TelegramModule,
    veles_telegram::module::manifest()
);

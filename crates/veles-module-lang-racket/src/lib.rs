//! The Racket host module: the dylib the module host loads
//! (PLAN-plugins-installer.md R9). Everything it runs is the
//! veles-lang-racket library; this crate only gives it the `export!`,
//! so the module's file carries the key and the `veles_module` static.
//! Racket CS is process-global: this module stays mapped for the
//! gateway's life (its engine thread keeps it), and a second copy of it
//! refuses to boot Racket again.

use veles_runtime::veles_core;

veles_core::export!(
    veles_lang_racket::module::RacketHost,
    veles_lang_racket::module::manifest()
);

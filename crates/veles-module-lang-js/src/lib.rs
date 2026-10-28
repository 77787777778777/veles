//! The JavaScript/TypeScript host module: the dylib the module host
//! loads (PLAN-plugins-installer.md R9). Everything it runs is the
//! veles-lang-js library; this crate only gives it the `export!`, so the
//! module's file carries the key and the `veles_module` static.

use veles_runtime::veles_core;

veles_core::export!(
    veles_lang_js::module::JsHost,
    veles_lang_js::module::manifest()
);

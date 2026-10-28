//! The IRC module: the dylib the module host loads (PLAN-plugins-installer.md
//! R4). Everything it runs is the veles-irc library (`veles_irc::module`
//! has the host-facing shell); this crate only gives it the `export!`,
//! so the module's file carries the key and the `veles_module` static.
//!
//! A crate of its own because the gateway must never link the module:
//! veles-cli uses veles-irc for its config surfaces, and when veles-irc
//! had a dylib form that link was dynamic — the gateway mapped
//! libveles_irc.so at startup, and every module loaded later bound its
//! symbols to that copy.

use veles_runtime::veles_core;

veles_core::export!(veles_irc::module::IrcModule, veles_irc::module::manifest());

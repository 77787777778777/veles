//! The phone's userland: an exec broker in a second APK and the client
//! shell the gateway runs (PLAN-phone-userland.md T3, the T3 spike).
//!
//! Android denies an app that targets SDK 29 or later `execute` on its
//! own data files, so no package manager can run from the gateway APK
//! (`chat.janitor.veles`, targetSdk 36). A second package,
//! `chat.janitor.veles.userland`, targets SDK 28, carries proot and an
//! Alpine rootfs, and runs [`broker`] on the abstract Unix socket
//! `@veles-userland`. The gateway's shell is [`client`]'s `ulsh`:
//! `ulsh -c "<script>"` hands its own stdin/stdout/stderr to the broker
//! over `SCM_RIGHTS`, the broker runs the script under proot in the
//! rootfs on those very descriptors, and ulsh exits with the script's
//! status. `VELES_SHELL=…/libulsh.so` is then the whole gateway change.
//!
//! The wire ([`proto`]): length-prefixed JSON frames. The broker trusts
//! exactly one peer uid (the gateway app's, which the Java side looks up
//! and passes on the command line); ulsh may pin the broker's uid the
//! same way (`ULSH_BROKER_UID`), so a squatter on the socket name gets
//! neither the script nor the descriptors.
//!
//! On the phone the gateway's app may not connect to the userland app's
//! socket (their SELinux domains, PLAN-phone-userland.md T3 §2 R1b), so
//! the socket of a session comes over Binder: the userland service makes
//! a socketpair, hands the broker one end ([`broker::serve_handoff`]) and
//! the gateway the other, which the gateway's own relay passes to ulsh
//! ([`client::open_relay`], `ULSH_RELAY`). The conversation on it is the
//! same; [`proto`] draws the hops.

pub mod broker;
pub mod client;
pub mod proto;
pub mod sock;

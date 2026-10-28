//! veles-core: the agent engine.
//!
//! Layer map (bottom → top):
//!   store     — SQLite session/memory/FTS store (rusqlite, FTS5)
//!   tclrt     — Tcl interpreter runtime: thread-host, sandbox, bridge cmds
//!   providers — LLM wire protocols: chat_completions and responses
//!   agent     — conversation loop, tool dispatch, fallback, compression
//!   tools     — builtin tool registry (terminal, file, browser via
//!               agent-browser, tts, image, a2a, cronjob, …)
//!   plugins   — plugin loader: Tcl scripts (hot); Rust plugins were
//!               designed once and never built — see plugins.rs
//!   hooks     — lifecycle hook bus (pre_llm_call, post_llm_call, …)
//!   frontends — CLI REPL, TUI, HTTP gateway; telegram/irc adapters plug in here
//!   config    — hermes-compatible config.yaml reader + veles.toml native

pub mod a2a;
pub mod a2a_tool;
pub mod acl;
pub mod agent;
pub mod agents;
pub mod aliases;
#[cfg(target_os = "android")]
mod android_shims;
pub mod approval;
pub mod apps;
pub mod arbiter;
pub mod art;
pub mod ask;
pub mod askrelay;
pub mod asr;
pub mod authn;
pub mod auxroute;
pub mod backoff;
pub mod backup;
pub mod batch;
pub mod bg;
pub mod bridge;
pub mod bridgeops;
pub mod bskylink;
pub mod bus;
pub mod chatml;
pub mod checkpoints;
pub mod clarify;
pub mod commands;
pub mod compaction;
pub mod config;
pub mod config_surface;
pub mod config_tool;
pub mod configedit;
pub mod connect_proxy;
pub mod cron;
pub mod cronjob;
pub mod deferred;
pub mod delegate;
pub mod desktop_tools;
pub mod device_events;
pub mod devices;
pub mod discovery;
pub mod downloads;
pub mod dream;
pub mod egress;
pub mod failover;
pub mod filetools;
pub mod gossip;
pub mod guards;
pub mod harness;
pub mod hermes_import;
pub mod hermes_migrate;
pub mod home;
pub mod hooks;
pub mod i18n;
pub mod idle;
pub mod irc_tap;
pub mod ircmask;
pub mod join;
pub mod joinbind;
pub mod keymap;
pub mod layers;

pub mod clipboard;
pub mod configsync;
pub mod lifecycle;
pub mod lockdown;
pub mod lockutil;
pub mod logbuf;
pub mod lsp_check;
pub mod lsp_proxy;
pub mod mail;
pub mod mailnet;
pub mod mcp;
pub mod media;
pub mod memory;
pub mod memsync;
pub mod migration;
pub mod model_binds;
pub mod module;
pub mod modulehost;
pub mod moduleroutes;
pub mod monitor;
pub mod node;
pub mod notes;
pub mod owned;
pub mod pairing;
pub mod placement;
pub mod plan;
pub mod platform;
pub mod pluginhost;
pub mod plugins;
pub mod points;
pub mod pool;
pub mod presence;
pub mod prompt;
pub mod providers;
pub mod publish;
pub mod recover;
pub mod relay;
pub mod remotetask;
pub mod remotetool;
pub mod repair;
pub mod rooms;
pub mod rt;
pub mod runsock;
pub mod rustplugins;
pub mod sandbox;
pub mod sandboxes;
#[cfg(any(test, feature = "test-support"))]
pub mod scratch;
pub mod scripthost;
pub mod secret_tools;
pub mod secrets;
pub mod selector;
pub mod services;
pub mod session_search;
pub mod session_tap;
pub mod session_wake;
pub mod sessions_tool;
pub mod settings_form;
pub mod setup;
pub mod shell;
pub mod shells;
pub mod skills;
pub mod slash_names;
pub mod soservices;
pub mod whoami;

pub mod store;
pub mod storesync;
pub mod streamhttp;
pub mod syncask;
pub mod takeover;
pub mod tasks;
pub mod tcleval;
pub mod tclrt;
#[cfg(any(test, feature = "test-support"))]
pub mod testenv;
pub mod theme;
pub mod ticket;
pub mod ticketed;
pub mod titles;
pub mod todo;
pub mod tokenplan;
pub mod tools;
pub mod transcript;
pub mod triggers;
pub mod urlguard;
pub mod user_envs;
pub mod userland;
pub mod vault;
// Tier-2 tool modules (#25/#26/#28/#30/#33).
pub mod browsertools;
pub mod challenge;
pub mod executecode;
pub mod export;
pub mod guestproxy;
pub mod vision;
pub mod webtools;
// Tier-2 tool modules round 2 (#27/#29/#32/#34/#36).
pub mod computeruse;
pub mod imagegen;
pub mod profiles;
pub mod proxy;
pub mod responses;
mod sse;
pub mod tts;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

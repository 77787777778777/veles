//! The bridge side of a Slopdrop trigger row: the services
//! `triggers.forward` and `triggers.masks` (`veles_core::services`; until
//! 2026-10-10 core's `Forwarder` slot, PLAN-plugins-installer.md R5).
//! The trigger ENGINE — the rows, the
//! brakes, the mask cache — stays in core where every surface reaches
//! it; what needs the bridge is here: the network-name mapping, the
//! argument shapes slopdrop's handlers expect, the forward itself, the
//! mask derivation from slopdrop's own bindings, and the timer poll.

use veles_core::config::VelesConfig;
use veles_core::triggers::Event;
use veles_types::plugin::{
    method::{TRIGGERS_FORWARD, TRIGGERS_MASKS},
    ChannelEvents, ChannelReply, ServiceDecl, TriggerEvent, TriggersForwardParams,
    TriggersForwarded, TriggersMasks, TriggersMasksParams,
};

use crate::settings::settings;

/// slopdrop's network name for one of ours. Explicit, not derived:
/// veles says `irc.synirc.net`, slopdrop says `synirc`, and a wrong
/// guess silently matches nothing in the `disabled` array.
pub fn slopdrop_network(cfg: &VelesConfig, veles_name: &str) -> Option<String> {
    // Config: [[platforms.irc.networks]] slopdrop_network = "synirc".
    // The platform block is a serde_yaml::Value; read the mapping the
    // same way veles-irc does.
    let plat = cfg.platforms.get("irc")?;
    let nets = plat.get("networks")?.as_sequence()?;
    for entry in nets {
        let name = entry.get("name").and_then(|v| v.as_str());
        let sd = entry.get("slopdrop_network").and_then(|v| v.as_str());
        if let (Some(n), Some(s)) = (name, sd) {
            if n == veles_name {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Which of OUR network names map to a slopdrop one. The reverse
/// direction of `slopdrop_network`, for routing timer keys back.
pub fn veles_network_for(cfg: &VelesConfig, slopdrop_name: &str) -> Option<String> {
    let plat = cfg.platforms.get("irc")?;
    let nets = plat.get("networks")?.as_sequence()?;
    for entry in nets {
        let name = entry.get("name").and_then(|v| v.as_str());
        let sd = entry.get("slopdrop_network").and_then(|v| v.as_str());
        if let (Some(n), Some(s)) = (name, sd) {
            if s == slopdrop_name {
                return Some(n.to_string());
            }
        }
    }
    None
}

/// The raw shape `GET /api/triggers` answers, after JSON parsing.
#[derive(Debug, Clone, Default)]
pub struct SlopdropTriggers {
    pub bindings: Vec<(String, String, String)>, // event, pattern, proc
    pub disabled: Vec<(String, Vec<String>)>,    // key "net:#chan", procs
}

impl SlopdropTriggers {
    /// The events a channel still has a LIVE binding for, after
    /// applying slopdrop's own disable rules — the derived mask.
    ///
    /// Mirrors `triggers.tcl`'s `is_disabled`: a proc is disabled for
    /// a key when it (or `all`) is listed under `net:chan`, `net:*`,
    /// `*:chan` or `*:*`.
    pub fn derive(&self, slopdrop_net: &str, channel: &str) -> Vec<String> {
        let is_disabled = |proc: &str| -> bool {
            for (key, procs) in &self.disabled {
                let (n, c) = key.split_once(':').unwrap_or((key.as_str(), "*"));
                let net_hit = n == "*" || n == slopdrop_net;
                let chan_hit = c == "*" || c.eq_ignore_ascii_case(channel);
                if net_hit && chan_hit && (procs.iter().any(|p| p == proc || p == "all")) {
                    return true;
                }
            }
            false
        };
        let mut events: Vec<String> = Vec::new();
        for (event, pattern, proc) in &self.bindings {
            if is_disabled(proc) {
                continue;
            }
            if (pattern == "*" || veles_core::triggers::glob_match_ci(pattern, channel))
                && !events.iter().any(|e| e.eq_ignore_ascii_case(event))
            {
                events.push(event.to_ascii_uppercase());
            }
        }
        events
    }
}

/// The argument vector the slopdrop bridge receives for one event.
///
/// Shapes mirror slopdrop's own native plugin (`tcl_plugin.rs`) and
/// its documented contract (`tcl/triggers.tcl`):
///   TEXT/JOIN/PART `[nick, mask, channel, …]`
///   QUIT `[nick, mask, message]`
///   NICK `[old, new, mask]`
///   KICK `[kicked, kicker, channel, reason]` — the kicker as the BARE
///   nick (split from `ev.mask`'s `kicker!user@host`), matching what
///   the same proc sees when slopdrop holds its own connection. (The
///   KICK arm shipped `[nick, nick, channel, text]` for weeks: the
///   kicked user where the kicker belongs and the kicker nowhere.)
pub fn slopdrop_args(ev: &Event<'_>) -> Result<Vec<String>, String> {
    Ok(match ev.event {
        "TEXT" => vec![
            ev.nick.to_string(),
            ev.mask.to_string(),
            ev.channel.to_string(),
            ev.text.to_string(),
        ],
        "JOIN" | "PART" => vec![
            ev.nick.to_string(),
            ev.mask.to_string(),
            ev.channel.to_string(),
        ],
        "QUIT" => vec![
            ev.nick.to_string(),
            ev.mask.to_string(),
            ev.text.to_string(),
        ],
        "KICK" => vec![
            ev.nick.to_string(),
            // `ev.mask` is the KICKER's full mask (the IRC adapter's KICK
            // arm builds `kicker!user@host` from the message prefix;
            // `ev.nick` is the KICKED one). The nick before `!` is the
            // kicker's name; a mask with no `!` (server-sourced,
            // malformed) is passed whole rather than guessed at.
            ev.mask.split('!').next().unwrap_or(ev.mask).to_string(),
            ev.channel.to_string(),
            ev.text.to_string(),
        ],
        "NICK" => vec![
            ev.nick.to_string(),
            ev.text.to_string(),
            ev.mask.to_string(),
        ],
        _ => return Err(format!("unknown event {}", ev.event)),
    })
}

// ── the services this module provides (veles_core::services) ────────

/// The bridge side of a Slopdrop trigger row, as services: `triggers.forward`
/// and `triggers.masks`. Internal (no cap): the IRC adapter calls them,
/// and no script or process plugin may push events into the interpreter.
pub fn service_decls() -> Vec<ServiceDecl> {
    vec![
        ServiceDecl {
            name: TRIGGERS_FORWARD.into(),
            description: "forward one channel event to slopdrop's trigger engine; the replies \
                          to relay"
                .into(),
            cap: None,
        },
        ServiceDecl {
            name: TRIGGERS_MASKS.into(),
            description: "which events each channel still has a live slopdrop binding for".into(),
            cap: None,
        },
    ]
}

/// Provide both, under the calling thread's owner (the module's `start`),
/// so the host's purge on unload withdraws them.
pub fn register_services() -> Result<(), String> {
    let decls = service_decls();
    veles_core::services::register(
        decls[0].clone(),
        veles_core::services::handler(|p, _cx| forward_service(p)),
    )?;
    veles_core::services::register(
        decls[1].clone(),
        veles_core::services::handler(|p, _cx| masks_service(p)),
    )
}

async fn forward_service(p: serde_json::Value) -> Result<serde_json::Value, String> {
    let p: TriggersForwardParams =
        serde_json::from_value(p).map_err(|e| format!("{TRIGGERS_FORWARD}: {e}"))?;
    let cfg = veles_core::config::live();
    let replies = forward_event(&cfg, &p.event, &p.answered_urls).await?;
    serde_json::to_value(TriggersForwarded {
        replies: replies
            .into_iter()
            .map(|(channel, message)| ChannelReply { channel, message })
            .collect(),
    })
    .map_err(|e| e.to_string())
}

/// Forward one event to the bridge: the network mapping, the argument
/// shapes and the HTTP call. `answered` names the links the bot already
/// previewed, which slopdrop's linkresolver leaves alone.
pub async fn forward_event(
    cfg: &VelesConfig,
    wire: &TriggerEvent,
    answered: &[String],
) -> Result<Vec<(String, String)>, String> {
    let s = settings();
    if !s.is_enabled() {
        return Err("[slopdrop] is not enabled on this node".into());
    }
    let ev = Event {
        platform: &wire.platform,
        network: &wire.network,
        channel: &wire.channel,
        event: &wire.event,
        nick: &wire.nick,
        mask: &wire.mask,
        text: &wire.text,
        role: wire.role.as_deref().and_then(veles_core::acl::Role::parse),
    };
    let sd_net = slopdrop_network(cfg, ev.network)
        .ok_or_else(|| format!("no slopdrop_network mapping for {}", ev.network))?;
    let args = slopdrop_args(&ev)?;
    Ok(crate::bridge::event(
        &s,
        &sd_net,
        ev.event,
        &args,
        answered,
        true,
        None,
        ev.role == Some(veles_core::acl::Role::Owner),
    )
    .await?
    .into_iter()
    .map(|r| (r.channel, r.message))
    .collect())
}

async fn masks_service(p: serde_json::Value) -> Result<serde_json::Value, String> {
    let p: TriggersMasksParams =
        serde_json::from_value(p).map_err(|e| format!("{TRIGGERS_MASKS}: {e}"))?;
    let cfg = veles_core::config::live();
    let s = settings();
    let sd = crate::bridge::triggers_state(&s).await?;
    let masks = p
        .pairs
        .into_iter()
        .filter_map(|nc| {
            let sd_net = slopdrop_network(&cfg, &nc.network)?;
            Some(ChannelEvents {
                events: sd.derive(&sd_net, &nc.channel),
                network: nc.network,
                channel: nc.channel,
            })
        })
        .collect();
    serde_json::to_value(TriggersMasks { masks }).map_err(|e| e.to_string())
}

/// Poll slopdrop's timer queue and deliver what fired (plan item 1).
///
/// The timer KEY is `network:#channel` in SLOPDROP's network spelling
/// (its own interpreter built the key). Mapping it back to a veles
/// network is `[[platforms.irc.networks]] slopdrop_network = "…"`; a
/// key naming a network we hold no connection for is logged and
/// dropped — never broadcast to whichever network happens to be first.
/// `[triggers] timers_poll_secs = 0` or `enabled = false` idles the
/// loop (re-checked every 30 s, so a config edit takes effect).
pub async fn run_timers() {
    loop {
        let Ok(cfg) = veles_core::config::load() else {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        };
        let poll = cfg.triggers.timers_poll_secs;
        if poll == 0 || !cfg.triggers.enabled {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        }
        tokio::time::sleep(std::time::Duration::from_secs(poll)).await;
        let s = settings();
        if !s.is_enabled() {
            continue;
        }
        match crate::bridge::timers_check(&s).await {
            Ok(fired) => {
                for (key, message) in fired {
                    // Split the key on the FIRST ':' — a channel name
                    // may itself contain one (rare, but "#a:b" exists).
                    let (sd_net, channel) = match key.split_once(':') {
                        Some((n, c)) if !c.is_empty() => (n, c),
                        _ => {
                            veles_core::module::log(
                                tracing::Level::WARN,
                                &format!(
                                    "triggers: timer key {key:?} has no network:channel shape — dropped, not broadcast"
                                ),
                            );
                            continue;
                        }
                    };
                    let Some(veles_net) = veles_network_for(&cfg, sd_net) else {
                        veles_core::module::log(
                            tracing::Level::WARN,
                            &format!(
                                "triggers: timer named slopdrop network {sd_net:?} which no [[platforms.irc.networks]] entry maps (slopdrop_network) — dropped, not broadcast"
                            ),
                        );
                        continue;
                    };
                    let req = veles_core::irc_tap::SendRequest {
                        network: veles_net,
                        presence: "bot".into(),
                        target: channel.to_string(),
                        text: message,
                        kind: veles_core::irc_tap::SendKind::Privmsg,
                    };
                    if let Err(e) = veles_core::irc_tap::send(req) {
                        veles_core::module::log(
                            tracing::Level::WARN,
                            &format!("triggers: timer delivery failed: {e}"),
                        );
                    }
                }
            }
            // A slopdrop that is down must not spam the log every tick.
            Err(e) => veles_core::module::log(
                tracing::Level::DEBUG,
                &format!("triggers: timers check failed: {e}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_derived_mask_comes_from_slopdrop_truth() {
        let sd = SlopdropTriggers {
            bindings: vec![
                ("TEXT".into(), "*".into(), "::linkresolver::on_text".into()),
                ("TEXT".into(), "*".into(), "timtom_text_handler".into()),
                ("JOIN".into(), "*".into(), "timtom_welcome".into()),
            ],
            disabled: vec![(
                "irc4fun:#coven".into(),
                vec![
                    "::linkresolver::on_text".into(),
                    "timtom_welcome".into(),
                    "::timtom::timtom_text_handler".into(),
                    "timtom_text_handler".into(),
                ],
            )],
        };
        // The live split: flashsupport wants TEXT+JOIN, coven wants
        // nothing.
        let flash = sd.derive("synirc", "#flashsupport");
        assert!(flash.contains(&"TEXT".to_string()));
        assert!(flash.contains(&"JOIN".to_string()));
        let coven = sd.derive("irc4fun", "#coven");
        assert!(coven.is_empty(), "{coven:?}");
        // A disable for one proc leaves the other live: the timtom
        // disable here removes JOIN but linkresolver's TEXT stays.
        let sd2 = SlopdropTriggers {
            bindings: sd.bindings.clone(),
            disabled: vec![(
                "synirc:#flashsupport".into(),
                vec!["timtom_text_handler".into()],
            )],
        };
        let m = sd2.derive("synirc", "#flashsupport");
        assert!(m.contains(&"TEXT".to_string()), "linkresolver still bound");
        assert!(m.contains(&"JOIN".to_string()), "welcome still bound");
    }

    /// The network mapping is explicit config, both directions.
    #[test]
    fn the_network_mapping_reads_both_ways() {
        let cfg: VelesConfig = toml::from_str(
            "[[platforms.irc.networks]]\nname = \"irc.synirc.net\"\nslopdrop_network = \"synirc\"\n\
             [[platforms.irc.networks]]\nname = \"irc.IRC4Fun.net\"\n",
        )
        .unwrap();
        assert_eq!(
            slopdrop_network(&cfg, "irc.synirc.net").as_deref(),
            Some("synirc")
        );
        assert_eq!(slopdrop_network(&cfg, "irc.IRC4Fun.net"), None);
        assert_eq!(
            veles_network_for(&cfg, "synirc").as_deref(),
            Some("irc.synirc.net")
        );
        assert_eq!(veles_network_for(&cfg, "irc4fun"), None);
    }

    /// KICK: slopdrop's contract is `[kicked, kicker, channel, reason]`.
    #[test]
    fn kick_names_the_kicker_not_the_nick_twice() {
        let ev = Event {
            platform: "irc",
            network: "irc.synirc.net",
            channel: "#flashsupport",
            event: "KICK",
            nick: "victim",
            mask: "kicker!k@host.example",
            text: "flooding",
            role: Some(veles_core::acl::Role::Owner),
        };
        assert_eq!(
            slopdrop_args(&ev).unwrap(),
            vec![
                "victim".to_string(),
                "kicker".to_string(),
                "#flashsupport".to_string(),
                "flooding".to_string(),
            ]
        );
    }

    /// The other arms keep their shapes: TEXT/JOIN/PART
    /// `[nick, mask, channel, …]`, QUIT `[nick, mask, message]`, NICK
    /// `[old, new, mask]`.
    #[test]
    fn the_other_arms_keep_their_shapes() {
        let base = Event {
            platform: "irc",
            network: "irc.synirc.net",
            channel: "#flashsupport",
            event: "TEXT",
            nick: "wrath",
            mask: "wrath@test",
            text: "hello",
            role: None,
        };
        let text = slopdrop_args(&base).unwrap();
        assert_eq!(text, vec!["wrath", "wrath@test", "#flashsupport", "hello"]);
        let join = slopdrop_args(&Event {
            event: "JOIN",
            ..base.clone()
        })
        .unwrap();
        assert_eq!(join, vec!["wrath", "wrath@test", "#flashsupport"]);
        let quit = slopdrop_args(&Event {
            event: "QUIT",
            channel: "*",
            text: "gone to bed",
            ..base.clone()
        })
        .unwrap();
        assert_eq!(quit, vec!["wrath", "wrath@test", "gone to bed"]);
        let nick = slopdrop_args(&Event {
            event: "NICK",
            channel: "*",
            text: "wrath_",
            ..base
        })
        .unwrap();
        assert_eq!(nick, vec!["wrath", "wrath_", "wrath@test"]);
        let bad = Event {
            platform: "irc",
            network: "irc.synirc.net",
            channel: "#x",
            event: "TOPIC",
            nick: "a",
            mask: "a@b",
            text: "",
            role: None,
        };
        assert!(slopdrop_args(&bad).unwrap_err().contains("TOPIC"));
    }

    /// With the bridge off a forward says so; with it on, a network with
    /// no mapping is refused with the reason.
    #[tokio::test]
    async fn an_off_bridge_or_an_unmapped_network_is_refused_with_the_reason() {
        // The settings pin is process-wide: under the env lock.
        let (_home, _env) = veles_core::testenv::scratch_home();
        let cfg = VelesConfig::default();
        let ev = TriggerEvent {
            platform: "irc".into(),
            network: "irc.nowhere.net".into(),
            channel: "#x".into(),
            event: "TEXT".into(),
            nick: "a".into(),
            mask: "a@b".into(),
            text: "hi".into(),
            role: Some("user".into()),
        };
        crate::settings::pin_for_test(crate::settings::SlopdropSection::default());
        let err = forward_event(&cfg, &ev, &[]).await.unwrap_err();
        assert!(err.contains("not enabled"), "{err}");
        crate::settings::pin_for_test(crate::settings::SlopdropSection {
            enabled: true,
            url: Some("http://127.0.0.1:0".into()),
            ..Default::default()
        });
        let err = forward_event(&cfg, &ev, &[]).await.unwrap_err();
        assert!(err.contains("no slopdrop_network mapping"), "{err}");
    }

    /// Both services are internal: the IRC adapter (in-process) may call
    /// them, a script or process plugin may not push events in.
    #[test]
    fn the_trigger_services_are_internal() {
        let d = service_decls();
        assert_eq!(
            d.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            vec![TRIGGERS_FORWARD, TRIGGERS_MASKS]
        );
        assert!(d.iter().all(|d| d.cap.is_none()));
    }
}

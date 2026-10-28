//! IRC wire protocol → tap events.
//!
//! The tap's data model and its bus live in `veles_core::irc_tap`,
//! because everything that WATCHES the tap — the gateway, the desktop
//! API — is in veles-core, and veles-core does not depend on this
//! crate. What stays here is the only part that needs the protocol
//! types: turning an `irc::proto::Message` into an event.
//!
//! Re-exported so client.rs has a single name to reach for.

pub use veles_core::irc_tap::{
    as_action, backlog_context, buffer_for, context_block, context_mark, context_mark_key, event,
    forget_context_mark, latest_seq, mark_disconnected, needs_local_echo, now_ts, publish,
    put_state, rolling_context, set_context_mark, states, subscribe, ContextBlock, EventKind,
    IrcEvent, NetworkState, PRESENCE_BOT, SERVER_BUFFER,
};

use irc::proto::{Command, Message, Prefix};

/// Services pseudo-clients. A message to or from one of these is a
/// control channel, not conversation.
const SERVICE_NICKS: &[&str] = &[
    "nickserv", "chanserv", "hostserv", "memoserv", "operserv", "botserv", "authserv", "ns", "cs",
    "hs", "ms", "os", "bs", "x", "q",
];

/// Service verbs whose ARGUMENTS are a credential.
///
/// `GHOST`/`RECOVER`/`RELEASE`/`GROUP` take the password as the second
/// argument and a nick as the first; the rest take it first. Redacting
/// everything after the verb covers both and costs nothing worth
/// keeping — an operator reading the transcript needs to know that a
/// REGISTER happened, not what the password was.
const CREDENTIAL_VERBS: &[&str] = &[
    "register",
    "identify",
    "id",
    "confirm",
    "ghost",
    "recover",
    "release",
    "group",
    "drop",
    "setpass",
    "sendpass",
    "resetpass",
    "login",
    "auth",
    "cert",
];

/// Redact the credential out of a services command.
///
/// WHY THIS EXISTS (found live, 2026-09-11, by registering the bot's
/// own nick): `echo-message` is negotiated, so the bot's own
/// `PRIVMSG NickServ :REGISTER <password> <email>` came back off the
/// wire, went through `classify` like anyone else's line, and landed in
/// the tap ring AND in `state.db`'s `irc_lines`.
///
/// That is not merely untidy. The model-facing `irc tail` action takes
/// an ARBITRARY buffer name and is gated at `Cap::Chat` — which
/// `Role::Guest` holds. So a stranger who can get the model to call it
/// could read the bot's own NickServ password out of its query buffer.
///
/// The tap's contract with the desktop is to record everything, and
/// this keeps that: the line is still there, the verb is still there,
/// only the secret is gone. `SET PASSWORD` is handled by the two-word
/// case rather than needing its own list.
///
/// DIRECTION IS THE WHOLE RULE, and it is `target` rather than the
/// buffer for exactly that reason: the two share a buffer and only the
/// target says which way the line went. A credential is in something we
/// ADDRESSED TO a service. A line FROM a service is the service talking,
/// and its text is prose.
///
/// Taking the buffer instead cost legibility the same day it shipped
/// (2026-09-11, IRC4Fun): `NickServ HELP` came back as a table whose
/// rows each begin with a command name, so seven of them — CERT,
/// CONFIRM, DROP, GROUP, IDENTIFY, RECOVER, REGISTER — were recorded as
/// `REGISTER <redacted>`. That is the failure this tree already fixed
/// once in the secrets scrubber: a redaction marker on ordinary text
/// teaches an operator to skim past redactions, and the next one they
/// skim past is real.
///
/// What is NOT covered, stated rather than implied: a service that
/// restates a credential back at us in a notice. That was the
/// speculation the buffer form was carrying, it has never been observed
/// here, and it is not a syntax problem — the line would contain a
/// string we already hold, so the fix is to match the VALUE (the shape
/// `secrets::scrubbable` uses) and not to guess from a leading verb.
pub(crate) fn redact_service_command(target: &str, text: &str) -> Option<String> {
    let to = target.trim().to_ascii_lowercase();
    // Some networks want the FULLY-QUALIFIED form — `NickServ@services.int`
    // on ircd-seven, `AuthServ@Services.GameSurge.net` — and that is what
    // the command is addressed to. Match the local part and let the
    // service host be whatever the network calls it: an exact-match list
    // cannot cover those, which is how it came to carry a dead
    // `"authserv@services."` entry that could never equal a real target.
    let local = to.split('@').next().unwrap_or(to.as_str());
    if !SERVICE_NICKS.contains(&local) {
        return None;
    }
    let mut words = text.split_whitespace();
    let verb = words.next()?;
    let lower = verb.to_ascii_lowercase();
    // `SET PASSWORD hunter2` — the secret is behind a sub-verb.
    if lower == "set" {
        let sub = words.next()?;
        if !sub.eq_ignore_ascii_case("password") && !sub.eq_ignore_ascii_case("pass") {
            return None;
        }
        words.next()?; // `SET PASSWORD` with no argument reveals nothing
        return Some(format!("{verb} {sub} <redacted>"));
    }
    if !CREDENTIAL_VERBS.contains(&lower.as_str()) {
        return None;
    }
    words.next()?; // a bare verb carries no secret
    Some(format!("{verb} <redacted>"))
}

/// What a client would draw for this line: its buffer, its kind, and
/// the text to show. `None` for protocol traffic (numerics, CAP, PING)
/// that belongs in a log, not a transcript.
///
/// Deliberately NOT a mirror of `handle_incoming`'s match. That one
/// asks "does the agent care about this"; this asks "would a person see
/// it", and the answers differ for exactly the traffic that used to be
/// dropped — notices, parts, quits, topic changes.
///
/// Credential-bearing services commands are redacted on the way through
/// — see `redact_service_command`.
pub fn classify(msg: &Message, own_nick: &str) -> Option<(String, EventKind, String)> {
    let nick = sender_nick(msg);
    Some(match &msg.command {
        Command::PRIVMSG(target, text) => {
            let buf = buffer_for(target, &nick, own_nick);
            match as_action(text) {
                Some(inner) => (buf, EventKind::Action, inner.to_string()),
                None => {
                    // Credentials never enter the tap. See
                    // `redact_service_command`: with echo-message on,
                    // our OWN `REGISTER <password>` comes back off the
                    // wire and is recorded like any other line — into a
                    // buffer that `irc tail` can read at Cap::Chat.
                    //
                    // `target`, not `buf`: our command and the service's
                    // reply share a buffer, and only the target tells
                    // them apart.
                    let shown =
                        redact_service_command(target, text).unwrap_or_else(|| text.clone());
                    (buf, EventKind::Privmsg, shown)
                }
            }
        }
        Command::NOTICE(target, text) => {
            // A notice we address TO a service is the same case — some
            // clients and scripts send credentials that way. One from a
            // service is prose and stays verbatim; `target` is what
            // separates them.
            let buf = buffer_for(target, &nick, own_nick);
            let shown = redact_service_command(target, text).unwrap_or_else(|| text.clone());
            (buf, EventKind::Notice, shown)
        }
        Command::JOIN(channel, _, _) => (channel.clone(), EventKind::Join, String::new()),
        Command::PART(channel, reason) => (
            channel.clone(),
            EventKind::Part,
            reason.clone().unwrap_or_default(),
        ),
        // A quit is not scoped to one channel — the client fans it out
        // to whichever of its buffers had that nick in the roster.
        Command::QUIT(reason) => (
            SERVER_BUFFER.to_string(),
            EventKind::Quit,
            reason.clone().unwrap_or_default(),
        ),
        Command::NICK(new) => (SERVER_BUFFER.to_string(), EventKind::Nick, new.clone()),
        Command::TOPIC(channel, topic) => (
            channel.clone(),
            EventKind::Topic,
            topic.clone().unwrap_or_default(),
        ),
        Command::KICK(channel, who, reason) => (
            channel.clone(),
            EventKind::Kick,
            format!("{who} {}", reason.clone().unwrap_or_default())
                .trim_end()
                .to_string(),
        ),
        // irc-proto splits MODE by target kind; both are one event here.
        Command::ChannelMODE(target, modes) => (
            target.clone(),
            EventKind::Mode,
            modes
                .iter()
                .map(|m| m.to_string())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        Command::UserMODE(target, modes) => (
            target.clone(),
            EventKind::Mode,
            modes
                .iter()
                .map(|m| m.to_string())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => return None,
    })
}

/// The sender's nick, or "" for a line straight from the server.
pub fn sender_nick(msg: &Message) -> String {
    match &msg.prefix {
        Some(Prefix::Nickname(nick, _, _)) => nick.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(prefix: Option<&str>, cmd: Command) -> Message {
        Message {
            tags: None,
            prefix: prefix.map(Prefix::new_from_str),
            command: cmd,
        }
    }

    #[test]
    fn a_notice_is_client_traffic() {
        // It used to be `debug!`-logged and dropped.
        let msg = m(
            Some("NickServ!s@services."),
            Command::NOTICE("jackma".to_string(), "you are identified".to_string()),
        );
        let (buf, kind, text) = classify(&msg, "jackma").expect("a notice is renderable");
        assert_eq!(kind, EventKind::Notice);
        assert_eq!(buf, "NickServ", "a service notice opens a query with it");
        assert_eq!(text, "you are identified");
    }

    #[test]
    fn parts_quits_and_topics_are_client_traffic() {
        let part = m(
            Some("wrath!u@h"),
            Command::PART("#veles".to_string(), Some("bye".to_string())),
        );
        assert_eq!(
            classify(&part, "jackma"),
            Some(("#veles".to_string(), EventKind::Part, "bye".to_string()))
        );

        let quit = m(Some("nyx!u@h"), Command::QUIT(Some("timeout".to_string())));
        let (buf, kind, _) = classify(&quit, "jackma").unwrap();
        assert_eq!(kind, EventKind::Quit);
        assert_eq!(buf, SERVER_BUFFER, "a quit is not scoped to one channel");

        let topic = m(
            Some("wrath!u@h"),
            Command::TOPIC("#veles".to_string(), Some("new topic".to_string())),
        );
        assert_eq!(
            classify(&topic, "jackma"),
            Some((
                "#veles".to_string(),
                EventKind::Topic,
                "new topic".to_string()
            ))
        );
    }

    #[test]
    fn an_action_classifies_as_an_action() {
        let msg = m(
            Some("wrath!u@h"),
            Command::PRIVMSG("#veles".to_string(), "\u{1}ACTION waves\u{1}".to_string()),
        );
        assert_eq!(
            classify(&msg, "jackma"),
            Some(("#veles".to_string(), EventKind::Action, "waves".to_string()))
        );
    }

    #[test]
    fn a_kick_names_who_and_why() {
        let msg = m(
            Some("wrath!u@h"),
            Command::KICK(
                "#veles".to_string(),
                "nyx".to_string(),
                Some("spam".to_string()),
            ),
        );
        let (buf, kind, text) = classify(&msg, "jackma").unwrap();
        assert_eq!((buf.as_str(), kind), ("#veles", EventKind::Kick));
        assert_eq!(text, "nyx spam");

        // No reason given must not leave a trailing space.
        let msg2 = m(
            Some("wrath!u@h"),
            Command::KICK("#veles".to_string(), "nyx".to_string(), None),
        );
        assert_eq!(classify(&msg2, "jackma").unwrap().2, "nyx");
    }

    #[test]
    fn protocol_traffic_is_not_a_transcript_line() {
        assert!(classify(&m(None, Command::PING("s".to_string(), None)), "jackma").is_none());
        assert!(classify(&m(None, Command::PONG("s".to_string(), None)), "jackma").is_none());
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    fn privmsg(target: &str, text: &str) -> Message {
        Message {
            tags: None,
            prefix: Some(Prefix::Nickname(
                "jackma".into(),
                "~jackma".into(),
                "cloak".into(),
            )),
            command: Command::PRIVMSG(target.to_string(), text.to_string()),
        }
    }

    /// A notice FROM a service TO us — the shape every services reply
    /// has. Note the target: it is our own nick, which is what
    /// `buffer_for` turns back into the service's name.
    fn notice_from(service: &str, text: &str) -> Message {
        Message {
            tags: None,
            prefix: Some(Prefix::Nickname(
                service.into(),
                "service".into(),
                "services.".into(),
            )),
            command: Command::NOTICE("jackma".to_string(), text.to_string()),
        }
    }

    /// The live incident: `echo-message` is on, so the bot's own
    /// registration came back off the wire and was taped verbatim —
    /// into a buffer `irc tail` reads at `Cap::Chat`, i.e. one a guest
    /// can ask for.
    ///
    /// Mutation check: drop the `redact_service_command` call in
    /// `classify`'s PRIVMSG arm and this goes red.
    #[test]
    fn a_services_password_never_reaches_the_tap() {
        let (buf, _, text) = classify(
            &privmsg("NickServ", "REGISTER hunter2 bot@example.org"),
            "jackma",
        )
        .expect("a privmsg is renderable");
        assert_eq!(buf, "NickServ");
        assert!(!text.contains("hunter2"), "the password survived: {text}");
        assert_eq!(text, "REGISTER <redacted>");

        // Second-argument secrets (GHOST nick password) are covered by
        // redacting everything after the verb.
        let (_, _, ghost) =
            classify(&privmsg("NickServ", "GHOST jackma hunter2"), "jackma").unwrap();
        assert_eq!(ghost, "GHOST <redacted>");

        // …and the two-word form.
        let (_, _, set) = classify(&privmsg("nickserv", "SET PASSWORD hunter2"), "jackma").unwrap();
        assert_eq!(set, "SET PASSWORD <redacted>");
    }

    /// Networks that want the fully-qualified service target — the form
    /// ircd-seven documents precisely so the message cannot be
    /// intercepted by a user holding the nick — put THAT string in the
    /// buffer name. Redaction has to follow it there, or the one network
    /// careful enough to demand it is the one that leaks.
    #[test]
    fn a_qualified_service_target_is_redacted_too() {
        for target in [
            "NickServ@services.int",
            "AuthServ@Services.GameSurge.net",
            "nickserv@services.",
        ] {
            let (buf, _, text) = classify(&privmsg(target, "IDENTIFY jackma hunter2"), "jackma")
                .expect("a privmsg is renderable");
            assert_eq!(buf, target, "the buffer name is preserved verbatim");
            assert_eq!(text, "IDENTIFY <redacted>", "not redacted for {target}");
        }

        // The local part still has to BE a service: an ordinary user on a
        // network whose cloak looks like a service host is a person.
        let (_, _, dm) = classify(
            &privmsg("wrath@services.int", "identify yourself"),
            "jackma",
        )
        .unwrap();
        assert_eq!(dm, "identify yourself");
    }

    /// What a service SAYS is prose, and must survive verbatim.
    ///
    /// Found live the same day the redaction shipped (2026-09-11,
    /// IRC4Fun): `NickServ HELP` answers with a table whose every row
    /// begins with a command name, so keying the redaction on the BUFFER
    /// — which our command and the reply share — ate seven rows of it.
    /// A `<redacted>` marker on a help listing is how an operator learns
    /// that redactions are noise.
    ///
    /// Mutation check: pass `&buf` instead of `target` in either arm of
    /// `classify` and the help rows go back to being redacted.
    #[test]
    fn what_a_service_says_back_is_never_redacted() {
        // The real IRC4Fun rows, indentation and all.
        for row in [
            "    REGISTER    Register a nickname",
            "    IDENTIFY    Identify yourself with your password",
            "    CONFIRM     Confirm a passcode",
            "    DROP        Cancel the registration of a nickname",
        ] {
            let (buf, kind, text) =
                classify(&notice_from("NickServ", row), "jackma").expect("a notice is renderable");
            assert_eq!(buf, "NickServ", "it still files under the service");
            assert!(matches!(kind, EventKind::Notice));
            assert_eq!(text, row, "a help row was eaten");
        }

        // The syntax line from `HELP REGISTER`, which is the single most
        // misleading thing to see redacted: it documents the command
        // rather than carrying a password.
        let (_, _, syntax) = classify(
            &notice_from("NickServ", "Syntax: REGISTER password email"),
            "jackma",
        )
        .unwrap();
        assert_eq!(syntax, "Syntax: REGISTER password email");

        // …and a reply that happens to START with a credential verb.
        let (_, _, reply) = classify(
            &notice_from("NickServ", "IDENTIFY failed. Please try again."),
            "jackma",
        )
        .unwrap();
        assert_eq!(reply, "IDENTIFY failed. Please try again.");

        // The direction test cuts the other way too: a notice WE address
        // to a service is still a command, and still redacted.
        let ours = Message {
            tags: None,
            prefix: Some(Prefix::Nickname(
                "jackma".into(),
                "~jackma".into(),
                "cloak".into(),
            )),
            command: Command::NOTICE("NickServ".to_string(), "IDENTIFY hunter2".to_string()),
        };
        let (_, _, sent) = classify(&ours, "jackma").unwrap();
        assert_eq!(sent, "IDENTIFY <redacted>");
    }

    /// The tap's contract is to RECORD, so redaction has to be narrow:
    /// ordinary conversation, and even ordinary service chatter, must
    /// come through untouched.
    #[test]
    fn redaction_does_not_eat_ordinary_lines() {
        // A channel line that merely starts with a credential verb.
        let (_, _, chan) = classify(
            &privmsg("#coven", "register for the workshop at example.com"),
            "jackma",
        )
        .unwrap();
        assert_eq!(chan, "register for the workshop at example.com");

        // A non-credential service command keeps its arguments — `INFO
        // jackma` is how you find out whether a nick is taken.
        let (_, _, info) = classify(&privmsg("NickServ", "INFO jackma"), "jackma").unwrap();
        assert_eq!(info, "INFO jackma");

        // A bare verb has no secret to hide, and blanking it would lose
        // the fact that a help request was made.
        let (_, _, bare) = classify(&privmsg("NickServ", "IDENTIFY"), "jackma").unwrap();
        assert_eq!(bare, "IDENTIFY");

        // A DM from a person who happens to be called something else.
        let (_, _, dm) = classify(&privmsg("jackma", "identify yourself"), "wrath").unwrap();
        assert_eq!(dm, "identify yourself");
    }
}

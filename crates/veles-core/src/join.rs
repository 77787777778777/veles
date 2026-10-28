//! The owner as a second person in someone else's session.
//!
//! USER, 2026-10-12: "is there any way I can chime in and attach
//! (non-exclusive) sessions of other users? If I want to be the second
//! conversation partner in a user's session?" Decided 2026-10-13: the
//! owner's words appear in that person's chat, a line of the owner's
//! runs with the OWNER's rights, and the model is told who said what.
//!
//! The surface that holds the session (the gateway's `/join` and
//! `/leave`) does the routing: the owner's lines go in as
//! [`crate::mail::send_join`] rows, and the session's lines are mirrored
//! to the owner's chat. This module is what the ENGINE needs while it
//! runs a turn there:
//!
//! - who has joined, so the origin row of the session's own person can
//!   name the speaker once someone else is in the conversation;
//! - where an ask goes while one of the owner's lines runs: to the owner,
//!   in their own chat, answerable by them alone. An approval for a turn
//!   with the owner's rights is the owner's to give, never the session's
//!   person's (`Agent::asker`, `ask::ask_session`).

use crate::acl::Principal;
use crate::ask::Asker;
use crate::lockutil::LockOk;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// The owner, joined to a session.
pub struct Joined {
    /// The owner's name as their chat shows it (an IRC nick, a Telegram
    /// handle): what the conversation and the model call them.
    pub owner_name: String,
    /// The owner's own chat, answerable by the owner alone: where the
    /// questions of the owner's lines go.
    pub asker: Arc<dyn Asker>,
}

fn joins() -> &'static Mutex<HashMap<String, Arc<Joined>>> {
    static JOINS: OnceLock<Mutex<HashMap<String, Arc<Joined>>>> = OnceLock::new();
    JOINS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The owner joined `session_id` (replacing an earlier join of it).
pub fn set(session_id: &str, joined: Joined) {
    joins()
        .lock_ok()
        .insert(session_id.to_string(), Arc::new(joined));
}

/// The owner left `session_id`.
pub fn clear(session_id: &str) {
    joins().lock_ok().remove(session_id);
}

/// Who has joined `session_id`, if anyone has.
pub fn active(session_id: &str) -> Option<Arc<Joined>> {
    joins().lock_ok().get(session_id).cloned()
}

/// What the conversation calls the session's own person: the nick on
/// the wire, then the handle the platform shows, then their subject.
pub fn speaker_name(session_id: &str, standing: Option<&Principal>) -> Option<String> {
    let ctx = crate::acl::session_sender(session_id);
    ctx.as_deref()
        .and_then(|c| {
            c.mask
                .as_deref()
                .and_then(|m| m.split('!').next())
                .filter(|n| !n.is_empty())
                .map(String::from)
                .or_else(|| c.handle.clone().filter(|h| !h.is_empty()))
        })
        .or_else(|| standing.map(|p| p.subject.clone()))
}

/// The origin row of the session's own person while the owner is
/// joined: the speaker by name, and that someone else is here too.
pub fn speaker_origin_line(surface: &str, speaker: Option<&str>, owner_name: &str) -> String {
    let who = speaker
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("the person this conversation belongs to");
    format!(
        "[origin] this message arrived via the {surface} surface, from {who}. {owner_name}, \
         the owner of this agent, has joined this conversation and may write here too; their \
         messages are marked as theirs."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::{Role, SenderContext};

    struct Nobody;
    #[async_trait::async_trait]
    impl Asker for Nobody {
        async fn ask(&self, _req: crate::ask::AskRequest) -> crate::ask::AskAnswer {
            crate::ask::AskAnswer::choice("deny")
        }
    }

    #[test]
    fn a_join_is_per_session_and_ends_with_clear() {
        assert!(active("join-t1").is_none());
        set(
            "join-t1",
            Joined {
                owner_name: "wrath".into(),
                asker: Arc::new(Nobody),
            },
        );
        assert_eq!(active("join-t1").unwrap().owner_name, "wrath");
        assert!(active("join-t2").is_none(), "another session is not joined");
        clear("join-t1");
        assert!(active("join-t1").is_none());
    }

    #[test]
    fn the_speaker_is_the_nick_then_the_handle_then_the_subject() {
        crate::acl::set_session_sender(
            "join-sp1",
            SenderContext {
                mask: Some("alice!~a@host".into()),
                handle: Some("Alice A".into()),
                ..Default::default()
            },
        );
        assert_eq!(speaker_name("join-sp1", None).as_deref(), Some("alice"));
        crate::acl::set_session_sender(
            "join-sp2",
            SenderContext {
                handle: Some("bob_tg".into()),
                ..Default::default()
            },
        );
        assert_eq!(speaker_name("join-sp2", None).as_deref(), Some("bob_tg"));
        let p = Principal::known("telegram:42", Role::User);
        assert_eq!(
            speaker_name("join-sp3", Some(&p)).as_deref(),
            Some(p.subject.as_str())
        );
    }

    #[test]
    fn the_speaker_line_names_both_people() {
        let l = speaker_origin_line("irc", Some("alice"), "wrath");
        assert!(l.starts_with("[origin] this message arrived via the irc surface, from alice."));
        assert!(
            l.contains("wrath, the owner of this agent, has joined"),
            "{l}"
        );
    }
}

//! A role held BY TICKET (`ticket_role` on an `[acl]` rule).
//!
//! USER 2026-10-11, on the Bluesky test account: "make it another owner
//! of the veles account that uses our ticketing to operate so that veles
//! can't just escalate privileges on its own". The agent can post from
//! that account — whoever holds its password can — so the account must
//! not BE the owner. Its rule is `role = "user"` + `ticket_role =
//! "owner"`: it stands at `user` (a node too old to know `ticket_role`
//! reads just that), and one call at a time runs at the ticket role, on a
//! ticket for exactly that call:
//!
//! * a tool call its standing level is refused (the agent loop asks
//!   here instead of refusing outright, `agent.rs`);
//! * one command, sent as `/sudo <command>` (the gateway).
//!
//! The ticket is the D3 ticket (`ticket.rs`) with this node as both
//! origin and target: the call, the hash of its arguments, the session,
//! the grant and an expiry, signed with a key this gateway does not hold.
//! It is asked of the session's surface and of the headless signers
//! (`remotetool::ask_for_ticket`) — the owner's own bot or nick shows the
//! call in full and signs only on the owner's yes. Nothing the gateway
//! runs can produce one, so nothing it runs can raise itself.

use crate::acl::{Principal, Role};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// The name a raised command is ticketed under, with
/// `{"command": "<the command line>"}` as its arguments.
pub const SUDO_TOOL: &str = "/sudo";

/// The arguments a `/sudo` ticket signs: the command line, trimmed.
pub fn sudo_args(command: &str) -> Value {
    json!({ "command": command.trim() })
}

/// Ask for, verify and spend a ticket that raises `who` for ONE call —
/// `tool(args)` in `session` — and return the principal that call runs
/// as. `why` is the refusal the standing level got; the owner reads it
/// beside the call.
///
/// Every failure is an error that says what happened (declined, nobody
/// signed in time, a ticket for another call, an unknown key), because
/// the model or the sender reads it next.
pub async fn obtain(
    store: &Arc<Mutex<crate::store::Store>>,
    cfg: &crate::config::VelesConfig,
    who: &Principal,
    session: &str,
    tool: &str,
    args: &Value,
    why: &str,
) -> Result<Principal> {
    let role = who
        .ticket_role
        .context("this caller holds no role by ticket")?;
    let me = crate::node::NodeIdentity::load(&crate::config::config_dir())
        .ok()
        .flatten()
        .context("this node has no node id yet (node.toml), so no ticket can name it")?;
    let request = crate::ticket::TicketRequest::new(
        &me.id,
        &me.id,
        session,
        tool,
        args.clone(),
        role.as_str(),
        crate::ticket::DEFAULT_TTL_SECS,
    )?
    .encode()?;
    let node = if me.name.is_empty() {
        me.id.chars().take(8).collect()
    } else {
        me.name.clone()
    };
    tracing::info!(
        "ticketed: {} asks to run {tool} as {} in session {session}",
        who.subject,
        role.as_str()
    );
    let reason = format!(
        "{} holds {} by ticket only; at its standing level: {}",
        who.label(),
        role.as_str(),
        first_line(why)
    );
    let Some(text) =
        crate::remotetool::ask_for_ticket(session, tool, &node, &reason, &request).await
    else {
        tracing::info!(
            "ticketed: no ticket for {tool} in session {session} — declined or not signed"
        );
        anyhow::bail!(
            "no ticket: the owner declined, or nobody signed it within {} minutes",
            crate::ticket::DEFAULT_TTL_SECS / 60
        );
    };
    let ticket = crate::ticket::Ticket::parse(&text)?;
    let now = chrono::Utc::now().timestamp();
    let v = crate::ticket::verify(
        &ticket,
        &crate::ticket::trusted_signers(cfg),
        &crate::ticket::CallContext {
            me: &me.id,
            origin: &me.id,
            session,
            tool,
            args,
            now,
        },
    )?;
    // A pasted ticket may name any grant its signer chose; it raises no
    // further than the role the rule holds by ticket.
    let grant = Role::parse(&v.grant)
        .context("the ticket's grant is not a role")?
        .min(role);
    crate::ticket::spend(&store.lock().unwrap_or_else(|e| e.into_inner()), &v, now)?;
    tracing::info!(
        "ticketed: ticket {} (signed by {}) runs {tool} as {} for {} in session {session}",
        v.id,
        v.signer,
        grant.as_str(),
        who.subject
    );
    Ok(who.raised(grant, &format!("ticket {} signed by {}", v.id, v.signer)))
}

/// Run one tool call as `raised`, the way a ticketed call from a peer
/// runs (`remotetool::execute`): the tool itself, never through
/// `dispatch` (whose gate reads the session's standing principal), under
/// a principal slot of its own — none at all for the owner, which is the
/// owner surface's shape. Below admin the call keeps the sandbox.
pub async fn run_tool(
    store: Arc<Mutex<crate::store::Store>>,
    session: &str,
    raised: &Principal,
    tool: &str,
    mut args: Value,
) -> String {
    let sid = if raised.role == Role::Owner {
        String::new()
    } else {
        let sid = format!("ticket-{}-{session}", raised.role.as_str());
        crate::acl::set_session_principal(&sid, raised.clone());
        if raised.role < Role::Admin {
            crate::user_envs::strip_host_request(&mut args);
        }
        sid
    };
    // The session registry's shape, module tools included (a module's
    // tool is the likeliest thing a ticket raises), bound to this slot.
    let registry = crate::tools::builtin_registry(store, None, &sid);
    let Some(t) = registry.get(tool) else {
        return format!("error: there is no tool {tool} to run by ticket on this node");
    };
    let out = t.call(args).await;
    format!(
        "{out}\n[ran as {} — {}]",
        raised.role.as_str(),
        raised.via.as_deref().unwrap_or("by ticket")
    )
}

/// The words a refusal adds for a caller who holds a role by ticket
/// that `cap` is in: how to ask. Empty for everyone else.
pub fn sudo_hint(who: &Principal, cap: crate::acl::Cap) -> String {
    match who.ticket_role {
        Some(role)
            if role.default_caps().contains(&crate::acl::Cap::All)
                || role.default_caps().contains(&cap) =>
        {
            format!(
                " You hold {} by ticket: send it as `/sudo <command>` to ask the owner for a \
                 ticket for this one command.",
                role.as_str()
            )
        }
        _ => String::new(),
    }
}

/// The command a `/sudo` line raises, when the line is one: the rest
/// after the word, trimmed. `/sudoku` is not.
pub fn sudo_target(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix("/sudo")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim())
}

/// Why `inner` cannot be raised by `/sudo`, or None when it can: one
/// built-in command — never a prompt (a turn runs under the session's
/// standing principal, so a raised "prompt" would raise only its
/// budget), and never another `/sudo`.
pub fn sudo_refusal(inner: &str) -> Option<String> {
    let usage = "`/sudo <command>` raises one built-in command, for example `/sudo /tcladmin …`. \
                 A request to the agent needs no `/sudo`: a tool call your level is refused asks \
                 for its own ticket.";
    let Some(cmd) = inner.strip_prefix('/') else {
        return Some(usage.to_string());
    };
    let head = cmd.split_whitespace().next().unwrap_or("");
    if head.eq_ignore_ascii_case("sudo") || !crate::aliases::is_builtin(head) {
        return Some(usage.to_string());
    }
    // A ticket raises ONE call. `/join` leaves a binding behind that
    // outlives the call (the owner's lines route into that session until
    // `/leave`), so neither is a thing a ticket can grant.
    if head.eq_ignore_ascii_case("join") || head.eq_ignore_ascii_case("leave") {
        return Some(
            "`/join` and `/leave` are the owner's own, not a call a ticket raises: a join \
             outlives the one command the ticket grants."
                .to_string(),
        );
    }
    None
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::{AclConfig, AclUser, Cap};

    fn cfg_with(role: &str, ticket_role: Option<&str>) -> AclConfig {
        AclConfig {
            users: vec![AclUser {
                name: "tester".into(),
                role: role.into(),
                identities: vec!["atproto:did:plc:tester".into()],
                ticket_role: ticket_role.map(str::to_string),
                ..Default::default()
            }],
            default_role: Some("guest".into()),
            ..Default::default()
        }
    }

    /// An owner by ticket STANDS at its rule's role — user's
    /// capabilities — with the ticket role remembered for a ticket.
    #[test]
    fn a_ticketed_owner_stands_at_user() {
        let p = Principal::resolve(
            Some("atproto:did:plc:tester"),
            &cfg_with("user", Some("owner")),
        );
        assert_eq!(p.role, Role::User);
        assert_eq!(p.ticket_role, Some(Role::Owner));
        assert_eq!(
            p.caps,
            Role::User.default_caps(),
            "the standing caps are user's"
        );
        assert!(!p.has(Cap::All), "never the owner without a ticket");
        assert!(p.label().contains("owner by ticket"), "{}", p.label());
    }

    /// A rule with no ticket role is the role it says.
    #[test]
    fn an_unticketed_rule_is_unchanged() {
        let p = Principal::resolve(Some("atproto:did:plc:tester"), &cfg_with("owner", None));
        assert_eq!(p.role, Role::Owner);
        assert_eq!(p.ticket_role, None);
        assert!(p.has(Cap::All));
    }

    /// The flag on an `[[entries]]` rule works the same.
    #[test]
    fn a_ticketed_entry_stands_at_user() {
        let acl: AclConfig = toml::from_str(
            "default_role = \"guest\"\n[[entries]]\nidentity = \"atproto:did:plc:tester\"\n\
             role = \"user\"\nticket_role = \"admin\"\n",
        )
        .unwrap();
        let p = Principal::resolve(Some("atproto:did:plc:tester"), &acl);
        assert_eq!(p.role, Role::User);
        assert_eq!(p.ticket_role, Some(Role::Admin));
    }

    /// Nothing to raise: a ticket role at or below the standing role
    /// means nothing.
    #[test]
    fn a_ticketed_user_has_nothing_to_raise() {
        for (role, ticket) in [("admin", "user"), ("user", "user"), ("owner", "owner")] {
            let p = Principal::resolve(
                Some("atproto:did:plc:tester"),
                &cfg_with(role, Some(ticket)),
            );
            assert_eq!(p.ticket_role, None, "{role} by ticket {ticket}");
        }
    }

    /// The key is read from config TOML and left out when unset; a node
    /// that does not know it reads the standing role (serde ignores the
    /// key, which is the whole safety of this shape).
    #[test]
    fn the_flag_round_trips_through_toml() {
        let raw = "[[users]]\nname = \"tester\"\nrole = \"user\"\nticket_role = \"owner\"\n\
                   identities = [\"atproto:did:plc:tester\"]\n";
        let acl: AclConfig = toml::from_str(raw).unwrap();
        assert_eq!(acl.users[0].ticket_role.as_deref(), Some("owner"));
        assert_eq!(acl.users[0].role, "user");
        let plain = AclUser {
            name: "x".into(),
            role: "user".into(),
            ..Default::default()
        };
        assert!(!toml::to_string(&plain).unwrap().contains("ticket_role"));
    }

    /// A raised principal is the same caller at the granted role, and
    /// cannot be raised again.
    #[test]
    fn raised_keeps_the_subject_and_drops_the_ceiling() {
        let p = Principal::resolve(
            Some("atproto:did:plc:tester"),
            &cfg_with("user", Some("owner")),
        );
        let r = p.raised(Role::Owner, "ticket abc signed by vps-signer");
        assert_eq!(r.subject, p.subject);
        assert_eq!(r.role, Role::Owner);
        assert!(r.has(Cap::All));
        assert_eq!(r.ticket_role, None);
        assert!(r.label().contains("ticket abc"), "{}", r.label());
    }

    /// `ticket_would_admit` answers for the ceiling, and only for a
    /// ticketed caller.
    #[test]
    fn a_ticket_would_admit_what_the_ceiling_may_run() {
        let cfg = crate::config::VelesConfig::default();
        let t = Principal::resolve(
            Some("atproto:did:plc:tester"),
            &cfg_with("user", Some("admin")),
        );
        assert!(t.ticket_would_admit("cronjob", &cfg), "admin runs cron");
        assert!(
            !t.ticket_would_admit("tcl_eval", &cfg),
            "tcl_eval is owner-only; an admin ticket cannot reach it"
        );
        let u = Principal::known("atproto:did:plc:other", Role::User);
        assert!(!u.ticket_would_admit("cronjob", &cfg));
    }

    /// `/sudo` takes one built-in command and nothing else.
    #[test]
    fn sudo_parses_one_builtin_command() {
        assert_eq!(
            sudo_target("/sudo /tcladmin puts 1"),
            Some("/tcladmin puts 1")
        );
        assert_eq!(sudo_target("  /sudo   /acl list "), Some("/acl list"));
        assert_eq!(sudo_target("/sudo"), Some(""));
        assert_eq!(sudo_target("/sudoku"), None);
        assert_eq!(sudo_target("please /sudo /acl"), None);
        assert!(sudo_refusal("/tcladmin puts 1").is_none());
        assert!(sudo_refusal("/acl grant x user").is_none());
        assert!(
            sudo_refusal("read my memory").is_some(),
            "a prompt is not a command"
        );
        assert!(sudo_refusal("/sudo /acl").is_some(), "no nesting");
        assert!(
            sudo_refusal("/join alice").is_some() && sudo_refusal("/leave").is_some(),
            "a join outlives the one call a ticket grants"
        );
        assert!(sudo_refusal("/nosuchcommand").is_some());
        assert!(sudo_refusal("").is_some());
    }

    /// The hint goes to a ticketed caller only, and only for what the
    /// ticketed role holds.
    #[test]
    fn the_sudo_hint_is_for_a_ticketed_caller() {
        let t = Principal::resolve(
            Some("atproto:did:plc:tester"),
            &cfg_with("user", Some("owner")),
        );
        assert!(sudo_hint(&t, Cap::All).contains("/sudo"));
        let a = Principal::resolve(
            Some("atproto:did:plc:tester"),
            &cfg_with("user", Some("admin")),
        );
        assert!(sudo_hint(&a, Cap::Cron).contains("admin by ticket"));
        assert!(sudo_hint(&a, Cap::All).is_empty(), "admin is not the owner");
        let u = Principal::known("atproto:did:plc:other", Role::User);
        assert!(sudo_hint(&u, Cap::All).is_empty());
    }

    /// With no ticket role, `obtain` refuses before asking anyone.
    #[tokio::test]
    async fn obtain_refuses_a_caller_without_a_ticket_role() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            crate::store::Store::open(&dir.path().join("s.db")).unwrap(),
        ));
        let who = Principal::known("atproto:did:plc:other", Role::User);
        let err = obtain(
            &store,
            &crate::config::VelesConfig::default(),
            &who,
            "s",
            "terminal",
            &json!({"command": "id"}),
            "denied",
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("no role by ticket"), "{err:#}");
    }
}

//! `/atproto` — basic administration of the bot's own atproto account,
//! as a module's DIRECT command (PLAN-plugins-installer.md R5):
//! registered in core's command registry under this module's owner,
//! answered by every surface's dispatcher like a builtin. The whole
//! name is `Cap::All`: it deletes as the bot, the posture `/slopdrop
//! delete`'s ledger verbs have (owner-only, announced like a tool
//! call).
//!
//! `delete` never touches the slopdrop ledger: the target is what the
//! owner pasted — an at-uri, a post link from ANY client
//! (`https://<anything>/profile/<actor>/post/<rkey>`), or a bare rkey
//! (assumed `app.bsky.feed.post`). It is also verify-FIRST: the
//! plugin's `repo.whoami` (the session DID) proves which repo a delete
//! would land in, `repo.get_record` proves the record is there, and
//! only then does `repo.delete_record` run. A did:plc in the target
//! that is not the bot's is refused BEFORE any delete call: another
//! account's record cannot be deleted through the API, and the command
//! says so rather than trying.

use std::sync::Arc;

use veles_core::acl::{Cap, Principal};
use veles_core::commands::{CommandBody, DirectCall, DirectCommand, PluginCommand};
use veles_types::plugin as proto;

pub const NAME: &str = "atproto";

/// Register the command (the module's `start`; purged with the module).
pub fn register() -> Result<(), String> {
    veles_core::commands::register_command(PluginCommand {
        name: NAME.into(),
        description: "the bot's atproto account: delete one of its own records (owner, \
                      verify-first)"
            .into(),
        body: CommandBody::Direct(DirectCommand {
            cap: Cap::All,
            run: Arc::new(|call| Box::pin(run(call))),
        }),
    })
}

/// The collection a Bluesky post lives in; what a bare rkey or a
/// `…/post/<rkey>` link means.
const POST_COLLECTION: &str = "app.bsky.feed.post";

fn denied(principal: Option<&Principal>, what: &str) -> String {
    let who = principal
        .map(|p| p.label())
        .unwrap_or_else(|| "this caller".to_string());
    format!(
        "[acl] {what} requires the `{}` capability — denied for {who}.",
        Cap::All.as_str()
    )
}

/// The command body. `call.principal` is the resolved caller (`None` on
/// a local owner surface), exactly as the shared dispatcher passed it.
pub async fn run(call: DirectCall) -> String {
    let principal = call.principal.as_ref();
    if let Some(p) = principal {
        if !p.has(Cap::All) {
            return denied(principal, "/atproto");
        }
    }
    let mut parts = call.args.split_whitespace();
    match parts.next().unwrap_or_default() {
        "delete" => {
            let target = parts.next().unwrap_or_default();
            if target.is_empty() {
                return "usage: /atproto delete <at-uri | post link | rkey>".into();
            }
            match delete(target).await {
                Ok(report) => {
                    veles_core::module::log(
                        tracing::Level::INFO,
                        &format!("atproto-cmd: /atproto delete {target}: {report}"),
                    );
                    report
                }
                Err(e) => {
                    veles_core::module::log(
                        tracing::Level::WARN,
                        &format!("atproto-cmd: /atproto delete {target} failed: {e}"),
                    );
                    format!("delete {target}: {e}")
                }
            }
        }
        _ => "usage: /atproto delete <at-uri | post link | rkey>\n  delete — take one of the \
             bot's OWN records down: an at-uri, a post link from any client, or a bare rkey \
             (a post). Verify-first: fetches the record and checks it is the bot's own before \
             deleting. Someone else's record is refused, never attempted."
            .into(),
    }
}

/// What the owner named a record by.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// `at://<repo>/<collection>/<rkey>`: full, nothing assumed.
    AtUri {
        repo: String,
        collection: String,
        rkey: String,
    },
    /// `https://<any client>/profile/<actor>/post/<rkey>`: a post; the
    /// actor is a handle or DID, ownership is checked against whoami.
    PostLink { actor: String, rkey: String },
    /// A bare word: a post rkey in the bot's own repo.
    Rkey(String),
}

impl Target {
    fn parse(s: &str) -> Result<Target, String> {
        let s = s.trim().trim_matches(|c| c == '<' || c == '>');
        if s.is_empty() {
            return Err("an empty target".into());
        }
        if let Some(rest) = s.strip_prefix("at://") {
            let path = rest.split('#').next().unwrap_or(rest);
            let at = path_at(path);
            let (repo, collection, rkey) =
                proto::at_uri_parts(&at).ok_or_else(|| format!("not one record's AT URI: {s}"))?;
            return Ok(Target::AtUri {
                repo: repo.to_string(),
                collection: collection.to_string(),
                rkey: rkey.to_string(),
            });
        }
        if let Some(rest) = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
        {
            let path = rest.split('?').next().unwrap_or(rest);
            let path = path.split('#').next().unwrap_or(path);
            let segs: Vec<&str> = path.split('/').skip(1).filter(|x| !x.is_empty()).collect();
            return match segs.as_slice() {
                ["profile", actor, "post", rkey] => Ok(Target::PostLink {
                    actor: (*actor).to_string(),
                    rkey: (*rkey).to_string(),
                }),
                _ => Err(format!(
                    "not a post link (want https://<client>/profile/<actor>/post/<rkey>): {s}"
                )),
            };
        }
        // A bare rkey is one path segment of record-key characters.
        if s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:~-".contains(c))
        {
            return Ok(Target::Rkey(s.to_string()));
        }
        Err(format!("not an at-uri, a post link or an rkey: {s}"))
    }
}

/// `at_uri_parts` wants the scheme on; the at:// arm already stripped
/// it (the `#<cid>` fragment too).
fn path_at(path: &str) -> String {
    format!("at://{path}")
}

/// The record the target names, as `(repo, collection, rkey)`. `repo`
/// may still be a HANDLE (a post link's actor) — `delete` resolves the
/// ownership question against `repo.whoami` before trusting it.
fn parts_of(t: &Target, bot_did: &str) -> (String, String, String) {
    match t {
        Target::AtUri {
            repo,
            collection,
            rkey,
        } => (repo.clone(), collection.clone(), rkey.clone()),
        Target::PostLink { actor, rkey } => {
            (actor.clone(), POST_COLLECTION.to_string(), rkey.clone())
        }
        Target::Rkey(rkey) => (
            bot_did.to_string(),
            POST_COLLECTION.to_string(),
            rkey.clone(),
        ),
    }
}

/// One service call with the registry's own error when nothing
/// provides it (the plugin is off, or on another node).
async fn call(service: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    veles_core::services::call(service, params).await
}

/// The verify-first delete: who the plugin acts as, that the record
/// is there and is the bot's own, then the delete.
pub async fn delete(target: &str) -> Result<String, String> {
    let t = Target::parse(target)?;

    // 1. Who is the bot: the session DID, the only ownership statement
    //    worth trusting (a configured DID is checked against it at the
    //    plugin's start).
    let who: proto::WhoamiValue = {
        let v = call(proto::method::REPO_WHOAMI, serde_json::json!({})).await?;
        serde_json::from_value(v)
            .map_err(|e| format!("the plugin's repo.whoami answer did not parse: {e}"))?
    };
    if who.did.is_empty() {
        return Err("the plugin answered repo.whoami with no DID".into());
    }

    let (repo, collection, rkey) = parts_of(&t, &who.did);

    // 2. Ownership, checked BEFORE any delete call. A did: target must
    //    BE the bot's; a handle target must MATCH the bot's handle or
    //    DID (a handle is re-pointable, so a mismatch refuses rather
    //    than resolves-and-deletes).
    let repo_is_bot = if repo.starts_with("did:") {
        repo == who.did
    } else {
        repo == who.did || (!who.handle.is_empty() && repo.eq_ignore_ascii_case(&who.handle))
    };
    if !repo_is_bot {
        return Err(format!(
            "that record is in {repo}'s repo, not the bot's ({} — only the bot's own records \
             can be deleted; another account's post can be reported or blocked, not deleted)",
            who.did
        ));
    }

    let uri = format!("at://{}/{collection}/{rkey}", who.did);

    // 3. It must be there: a deleted (or never written) record is a
    //    plain answer, not an error.
    let got: proto::GetRecordValue = {
        let v = call(
            proto::method::REPO_GET_RECORD,
            serde_json::json!({ "uri": uri }),
        )
        .await?;
        serde_json::from_value(v)
            .map_err(|e| format!("the plugin's repo.get_record answer did not parse: {e}"))?
    };
    if !got.found {
        return Ok(format!("already gone: {uri} (nothing to delete)"));
    }

    // 4. The delete itself.
    let p = proto::DeleteRecordParams {
        collection: collection.clone(),
        rkey: rkey.clone(),
    };
    call(
        proto::method::REPO_DELETE_RECORD,
        serde_json::to_value(p).map_err(|e| e.to_string())?,
    )
    .await?;
    Ok(format!("deleted {uri}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_parser_takes_an_at_uri() {
        let t = Target::parse("at://did:plc:abc/app.bsky.feed.post/3kabcde").unwrap();
        assert_eq!(
            t,
            Target::AtUri {
                repo: "did:plc:abc".into(),
                collection: "app.bsky.feed.post".into(),
                rkey: "3kabcde".into(),
            }
        );
    }

    #[test]
    fn the_target_parser_strips_a_cid_fragment_off_an_at_uri() {
        let t = Target::parse("at://did:plc:abc/app.bsky.feed.post/3kabcde#bafy123").unwrap();
        assert_eq!(
            t,
            Target::AtUri {
                repo: "did:plc:abc".into(),
                collection: "app.bsky.feed.post".into(),
                rkey: "3kabcde".into(),
            }
        );
    }

    #[test]
    fn the_target_parser_takes_a_post_link_from_any_client() {
        for link in [
            "https://bsky.app/profile/jackma.veles.7778777.online/post/3myd6j5srvkwr",
            "https://impro.social/profile/jackma.veles.7778777.online/post/3myd6j5srvkwr",
            "https://deer.social/profile/did:plc:abc/post/3kabcde?foo=bar#frag",
        ] {
            let t = Target::parse(link).unwrap();
            let Target::PostLink { rkey, .. } = &t else {
                panic!("{link} parsed as {t:?}");
            };
            assert!(!rkey.is_empty());
        }
        let t = Target::parse(
            "https://impro.social/profile/jackma.veles.7778777.online/post/3myd6j5srvkwr",
        )
        .unwrap();
        assert_eq!(
            t,
            Target::PostLink {
                actor: "jackma.veles.7778777.online".into(),
                rkey: "3myd6j5srvkwr".into(),
            }
        );
    }

    #[test]
    fn the_target_parser_takes_a_bare_rkey_and_refuses_garbage() {
        assert_eq!(
            Target::parse("3myd6j5srvkwr").unwrap(),
            Target::Rkey("3myd6j5srvkwr".into())
        );
        assert!(Target::parse("https://bsky.app/profile/someone").is_err());
        assert!(Target::parse("at://did:plc:abc/not-a-post").is_err());
        assert!(Target::parse("two words").is_err());
        assert!(Target::parse("").is_err());
    }

    #[test]
    fn a_bare_rkey_means_a_post_in_the_bots_repo() {
        let (_, coll, rkey) = parts_of(&Target::Rkey("3kabcde".into()), "did:plc:bot");
        assert_eq!(coll, POST_COLLECTION);
        assert_eq!(rkey, "3kabcde");
    }

    // ── the verify-first flow, against stub services ────────────────
    //
    // The stubs sit in the REAL service registry, registered and
    // withdrawn as core (no thread owner), over nothing: no test here
    // runs against a live plugin. The delete stub records its params so
    // the order and the target are asserted, not just the outcome.

    use std::sync::Mutex;

    /// The service registry is process-global: two flow tests running
    /// at once would see each other's stubs, and a finishing test's
    /// guard would withdraw the services from under a still-running
    /// one. The flow tests take turns (an async-aware mutex: the turn
    /// IS held across the delete's awaits).
    static FLOW: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// What one run of the stubs saw, in call order.
    #[derive(Debug, Default)]
    struct Seen {
        calls: Vec<String>,
        deleted: Vec<(String, String)>,
    }

    fn stub_decl(name: &str) -> veles_core::services::ServiceDecl {
        veles_core::services::ServiceDecl {
            name: name.into(),
            description: String::new(),
            cap: Some(veles_core::services::SERVICE_CAP_ANY.into()),
        }
    }

    /// Register the three repo services as stubs answering `whoami`'s
    /// DID/handle; `found` is what repo.get_record answers. Returns the
    /// record of what they saw; the guard withdraws them on drop.
    fn stub_services(did: &str, handle: &str, found: bool) -> (std::sync::Arc<Mutex<Seen>>, Stubs) {
        let seen = std::sync::Arc::new(Mutex::new(Seen::default()));
        let note = |seen: &std::sync::Arc<Mutex<Seen>>, what: &str| {
            seen.lock()
                .unwrap_or_else(|p| p.into_inner())
                .calls
                .push(what.to_string());
        };
        let (s1, s2, s3) = (seen.clone(), seen.clone(), seen.clone());
        let (did, handle) = (did.to_string(), handle.to_string());
        veles_core::services::register(
            stub_decl(proto::method::REPO_WHOAMI),
            veles_core::services::handler(move |_p, _cx| {
                let s = s1.clone();
                let (did, handle) = (did.clone(), handle.clone());
                async move {
                    note(&s, "whoami");
                    serde_json::to_value(proto::WhoamiValue { did, handle })
                        .map_err(|e| e.to_string())
                }
            }),
        )
        .expect("whoami stub");
        veles_core::services::register(
            stub_decl(proto::method::REPO_GET_RECORD),
            veles_core::services::handler(move |_p, _cx| {
                let s = s2.clone();
                async move {
                    note(&s, "get_record");
                    serde_json::to_value(proto::GetRecordValue {
                        found,
                        cid: None,
                        value: serde_json::json!({}),
                    })
                    .map_err(|e| e.to_string())
                }
            }),
        )
        .expect("get_record stub");
        veles_core::services::register(
            stub_decl(proto::method::REPO_DELETE_RECORD),
            veles_core::services::handler(move |p, _cx| {
                let s = s3.clone();
                async move {
                    let mut g = s.lock().unwrap_or_else(|p| p.into_inner());
                    g.calls.push("delete_record".to_string());
                    g.deleted.push((
                        p["collection"].as_str().unwrap_or("").to_string(),
                        p["rkey"].as_str().unwrap_or("").to_string(),
                    ));
                    drop(g);
                    Ok(serde_json::Value::Null)
                }
            }),
        )
        .expect("delete_record stub");
        (seen, Stubs)
    }

    /// Withdraws the stubs when the test ends, pass or panic.
    struct Stubs;
    impl Drop for Stubs {
        fn drop(&mut self) {
            veles_core::services::withdraw(proto::method::REPO_WHOAMI);
            veles_core::services::withdraw(proto::method::REPO_GET_RECORD);
            veles_core::services::withdraw(proto::method::REPO_DELETE_RECORD);
        }
    }

    fn seen_of(seen: &std::sync::Arc<Mutex<Seen>>) -> Seen {
        let g = seen.lock().unwrap_or_else(|p| p.into_inner());
        Seen {
            calls: g.calls.clone(),
            deleted: g.deleted.clone(),
        }
    }

    #[tokio::test]
    async fn a_delete_of_the_bots_own_post_verifies_first() {
        let _turn = FLOW.lock().await;
        let (seen, _stubs) = stub_services("did:plc:bot", "bot.example", true);
        let out = delete("https://impro.social/profile/bot.example/post/3kabcde")
            .await
            .expect("delete");
        assert!(
            out.contains("deleted at://did:plc:bot/app.bsky.feed.post/3kabcde"),
            "{out}"
        );
        let s = seen_of(&seen);
        assert_eq!(s.calls, ["whoami", "get_record", "delete_record"]);
        assert_eq!(
            s.deleted,
            [(POST_COLLECTION.to_string(), "3kabcde".to_string())]
        );
    }

    #[tokio::test]
    async fn a_delete_by_bare_rkey_targets_the_bots_repo() {
        let _turn = FLOW.lock().await;
        let (seen, _stubs) = stub_services("did:plc:bot", "bot.example", true);
        delete("3kabcde").await.expect("delete");
        let s = seen_of(&seen);
        assert_eq!(s.calls, ["whoami", "get_record", "delete_record"]);
        assert_eq!(
            s.deleted,
            [(POST_COLLECTION.to_string(), "3kabcde".to_string())]
        );
    }

    #[tokio::test]
    async fn a_foreign_did_is_refused_before_any_delete() {
        let _turn = FLOW.lock().await;
        let (seen, _stubs) = stub_services("did:plc:bot", "bot.example", true);
        let e = delete("at://did:plc:someoneelse/app.bsky.feed.post/3kabcde")
            .await
            .unwrap_err();
        assert!(e.contains("not the bot's"), "{e}");
        let s = seen_of(&seen);
        assert_eq!(s.calls, ["whoami"], "no read, and no delete: {:?}", s.calls);
        assert!(s.deleted.is_empty());
    }

    #[tokio::test]
    async fn a_foreign_handle_is_refused_before_any_delete() {
        let _turn = FLOW.lock().await;
        let (seen, _stubs) = stub_services("did:plc:bot", "bot.example", true);
        let e = delete("https://bsky.app/profile/someone.else/post/3kabcde")
            .await
            .unwrap_err();
        assert!(e.contains("not the bot's"), "{e}");
        assert!(seen_of(&seen).deleted.is_empty());
    }

    #[tokio::test]
    async fn a_record_already_gone_is_an_answer_not_a_delete() {
        let _turn = FLOW.lock().await;
        let (seen, _stubs) = stub_services("did:plc:bot", "bot.example", false);
        let out = delete("3kabcde").await.expect("already gone is Ok");
        assert!(out.contains("already gone"), "{out}");
        let s = seen_of(&seen);
        assert_eq!(s.calls, ["whoami", "get_record"]);
        assert!(s.deleted.is_empty());
    }

    #[tokio::test]
    async fn the_bots_own_did_spelled_out_is_its_record() {
        let _turn = FLOW.lock().await;
        let (seen, _stubs) = stub_services("did:plc:bot", "bot.example", true);
        delete("at://did:plc:bot/app.bsky.feed.post/3kabcde")
            .await
            .expect("delete");
        assert_eq!(
            seen_of(&seen).deleted,
            [(POST_COLLECTION.to_string(), "3kabcde".to_string())]
        );
    }

    #[test]
    fn the_command_denies_a_guest_and_answers_usage() {
        let guest = Principal::known("irc:nobody", veles_core::acl::Role::Guest);
        let call = DirectCall {
            args: "delete 3kabcde".into(),
            principal: Some(guest),
            sender: None,
            session_id: String::new(),
        };
        let out = futures_lite_run(call);
        assert!(out.contains("[acl]"), "{out}");

        let usage = futures_lite_run(DirectCall {
            args: String::new(),
            principal: None,
            sender: None,
            session_id: String::new(),
        });
        assert!(usage.contains("usage: /atproto delete"), "{usage}");
    }

    /// `run` is async; the denial and usage paths never await anything
    /// pending, so a trivial block_on answers them.
    fn futures_lite_run(call: DirectCall) -> String {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt")
            .block_on(run(call))
    }
}

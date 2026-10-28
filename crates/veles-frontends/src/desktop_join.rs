//! Authenticated desktop owner-join endpoints. The target gateway owns
//! execution and persistence; the Tcl client owns only its return view.

use axum::Json;
use serde_json::{json, Value};

#[derive(serde::Deserialize)]
pub struct JoinRequest {
    session_id: String,
    #[serde(default)]
    instance: String,
    #[serde(default)]
    owner_session: String,
}

pub async fn join_request(Json(req): Json<JoinRequest>) -> (axum::http::StatusCode, Json<Value>) {
    let Some(core) = crate::gateway::global_core_arc() else {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(json!({"error": "target is not held by this gateway"})),
        );
    };
    let id = format!("j{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
    match core.desktop_join(&format!("desktop:join:{id}"), &req.session_id) {
        Ok(mut b) => {
            b.from_instance = req.instance;
            b.from_session = req.owner_session;
            (axum::http::StatusCode::OK, Json(json!({"join": b})))
        }
        Err(e) => (
            axum::http::StatusCode::CONFLICT,
            Json(json!({"error": format!("{e:#}")})),
        ),
    }
}

#[derive(serde::Deserialize)]
pub struct StatusRequest {
    join_id: String,
    session_id: String,
}

/// Read-only validation for a desktop's saved return view after a
/// restart. The target's binding and current ACL decide, never a role or
/// instance supplied by the client. No turn or leave is performed.
pub async fn status_request(
    axum::extract::Query(req): axum::extract::Query<StatusRequest>,
) -> (axum::http::StatusCode, Json<Value>) {
    let Some(core) = crate::gateway::global_core_arc() else {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(json!({"error": "target is not held by this gateway"})),
        );
    };
    if req.join_id.trim().is_empty() || req.session_id.trim().is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "join_id and session_id required"})),
        );
    }
    match core.desktop_status(&req.join_id, &req.session_id) {
        Ok(binding) => (axum::http::StatusCode::OK, Json(json!({"join": binding}))),
        Err(e) => (
            axum::http::StatusCode::CONFLICT,
            Json(json!({"error": format!("{e:#}")})),
        ),
    }
}

#[derive(serde::Deserialize)]
pub struct LeaveRequest {
    join_id: String,
    session_id: String,
}

pub async fn leave_request(Json(req): Json<LeaveRequest>) -> Json<Value> {
    let Some(core) = crate::gateway::global_core_arc() else {
        return Json(json!({"error": "target is not held by this gateway"}));
    };
    if req.session_id.trim().is_empty() {
        return Json(json!({"error": "session_id required"}));
    }
    match core.desktop_leave(&req.join_id, &req.session_id) {
        Some(_) => Json(json!({"ok": true})),
        None => Json(json!({"error": "join is no longer active"})),
    }
}

#[cfg(test)]
mod tests {
    use crate::gateway::GatewayCore;
    use std::sync::{Arc, Mutex};
    use veles_core::lockutil::LockOk;
    use veles_core::store::Store;

    fn fixture() -> (
        Arc<GatewayCore>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    ) {
        let (_home, env) = veles_core::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        (GatewayCore::new_join_test(store), dir, env)
    }

    fn sender(core: &Arc<GatewayCore>, who: &str) -> String {
        core.test_sender(
            "irc",
            "#chan",
            &format!("irc:{who}"),
            veles_core::acl::SenderContext {
                mask: Some(format!("{who}!u@example.invalid")),
                network: Some("testnet".into()),
                channel: Some("#chan".into()),
                platform: Some("irc".into()),
                chat_id: Some("#chan".into()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn joins_keep_the_exact_sender_session_across_a_core_restart() {
        let (core, dir, _h) = fixture();
        let sid = sender(&core, "alice");
        let binding = core.desktop_join("desktop:join:j-restart", &sid).unwrap();
        let raw = core
            .store
            .lock_ok()
            .meta_get("join:gateway:v1:desktop:join:j-restart")
            .unwrap();
        assert!(
            !raw.contains("from_instance") && !raw.contains("from_session"),
            "the target does not persist desktop-local return-view fields: {raw}"
        );
        assert_eq!(
            core.store
                .lock_ok()
                .meta_scan_prefix("join:gateway:v1:desktop:join:j-restart")
                .len(),
            1
        );
        assert!(
            core.store
                .lock_ok()
                .meta_get("join:desktop:v1:j-restart")
                .is_none(),
            "the desktop id must not open a second durable join"
        );
        // A development draft may have left this redundant row behind.
        // It is never consulted for execution and is cleared at restore.
        core.store
            .lock_ok()
            .meta_set("join:desktop:v1:j-restart", "stale draft")
            .unwrap();
        core.desktop_input("j-restart", &sid, "first owner line")
            .unwrap();
        drop(core);
        veles_core::join::clear(&sid);
        let store = Arc::new(Mutex::new(
            Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        let restored = GatewayCore::new_join_test(store);
        restored.test_join_restore();
        assert!(restored
            .store
            .lock_ok()
            .meta_get("join:desktop:v1:j-restart")
            .is_none());
        assert_eq!(
            restored.test_join_key(&sid).as_deref(),
            Some("irc:#chan:irc:alice")
        );
        assert!(restored.test_join_live(&sid));
        assert!(veles_core::join::active(&sid).is_some());
        restored
            .desktop_input(&binding.join_id, &sid, "after restart")
            .unwrap();
        assert_eq!(restored.store.lock_ok().queue_items(&sid).unwrap().len(), 2);
        restored.desktop_leave(&binding.join_id, &sid).unwrap();
        assert!(veles_core::join::active(&sid).is_none());
    }

    #[test]
    fn only_an_existing_non_owner_chat_session_can_be_joined() {
        let (core, _d, _h) = fixture();
        let sid = sender(&core, "alice");
        assert!(core.desktop_join("desktop:join:j0", "nobody").is_err());
        let binding = core.desktop_join("desktop:join:j1", &sid).unwrap();
        assert_eq!(binding.join_id, "j1");
        assert_eq!(binding.target_session, sid);
        assert!(veles_core::join::active(&sid).is_some());
        core.desktop_input("j1", &sid, "owner contribution")
            .unwrap();
        let rows = core.store.lock_ok().queue_items(&sid).unwrap();
        assert_eq!(rows.len(), 1);
        let id = veles_core::mail::parse_body(&rows[0].body).unwrap().0;
        assert_eq!(
            core.store.lock_ok().mail_get(id).unwrap().unwrap().kind,
            veles_core::mail::KIND_JOIN
        );
        assert!(core
            .desktop_command("j1", &sid, "/steer must stay local", &[])
            .is_err());
        assert_eq!(core.store.lock_ok().queue_items(&sid).unwrap().len(), 1);
        assert!(core.desktop_input("j1", "other", "wrong view").is_err());
        assert!(core.desktop_leave("j1", "other").is_none());
        assert!(veles_core::join::active(&sid).is_some());
        assert!(core.desktop_leave("j1", &sid).is_some());
        assert!(veles_core::join::active(&sid).is_none());
    }

    #[test]
    fn a_second_desktop_join_cannot_steal_an_active_owner_ask_route() {
        let (core, _d, _h) = fixture();
        let sid = sender(&core, "alice");
        let first = core.desktop_join("desktop:join:j-first", &sid).unwrap();
        assert!(core.desktop_join("desktop:join:j-second", &sid).is_err());
        core.desktop_input(&first.join_id, &sid, "original owner still routed")
            .unwrap();
        assert!(core
            .desktop_input("j-second", &sid, "must not run")
            .is_err());
        assert_eq!(core.store.lock_ok().queue_items(&sid).unwrap().len(), 1);
        core.desktop_leave(&first.join_id, &sid).unwrap();
        assert!(core.desktop_join("desktop:join:j-third", &sid).is_ok());
    }

    #[test]
    fn a_restored_join_is_revalidated_and_revocation_refuses_owner_input() {
        let (core, dir, _h) = fixture();
        let sid = sender(&core, "alice");
        let binding = core.desktop_join("desktop:join:j-revoke", &sid).unwrap();
        let revoked = veles_core::acl::Principal {
            subject: "owner".into(),
            role: veles_core::acl::Role::Owner,
            ..(*core
                .sessions
                .lock_ok()
                .values()
                .next()
                .unwrap()
                .standing_principal()
                .unwrap())
            .clone()
        };
        core.sessions
            .lock_ok()
            .values()
            .next()
            .unwrap()
            .set_principal(revoked);
        // The cached-agent check above is independent of a config-file
        // reload. The next test changes the real file-backed ACL.
        assert!(core
            .desktop_input("j-revoke", &sid, "must not run")
            .is_err());
        assert!(core.store.lock_ok().queue_items(&sid).unwrap().is_empty());
        drop(core);
        let store = Arc::new(Mutex::new(
            Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        let restored = GatewayCore::new_join_test_with_acl(store, "banned");
        restored.test_join_restore();
        assert!(!restored.test_join_live(&sid));
        assert!(restored
            .desktop_input(&binding.join_id, &sid, "must not run after restart")
            .is_err());
        assert!(restored
            .store
            .lock_ok()
            .queue_items(&sid)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_live_acl_ban_refuses_desktop_input_without_a_participant_message() {
        let (_home, env) = veles_core::testenv::scratch_home();
        let config = |role: &str| {
            format!(
                "[model]\nprovider = \"test-provider\"\ndefault = \"test-model\"\n\n\
                 [[custom_providers]]\nname = \"test-provider\"\n\
                 base_url = \"http://127.0.0.1:0/v1\"\nmodels = [\"test-model\"]\n\n\
                 [acl]\ndefault_role = \"{role}\"\n"
            )
        };
        std::fs::write(
            veles_core::config::config_dir().join("config.toml"),
            config("user"),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let core = GatewayCore::new_join_test_file_acl(Arc::new(Mutex::new(
            Store::open(&dir.path().join("state.db")).unwrap(),
        )));
        let sid = sender(&core, "alice");
        core.desktop_join("desktop:join:j-hot-ban", &sid).unwrap();
        std::fs::write(
            veles_core::config::config_dir().join("config.toml"),
            config("banned"),
        )
        .unwrap();
        assert!(core
            .desktop_input("j-hot-ban", &sid, "must not run after the live ban")
            .is_err());
        assert!(core.store.lock_ok().queue_items(&sid).unwrap().is_empty());
        drop(core);
        drop(env);
    }

    #[test]
    fn restore_refuses_a_changed_participant_identity_even_on_the_same_route() {
        let (core, dir, _h) = fixture();
        let sid = sender(&core, "alice");
        core.desktop_join("desktop:join:j-identity", &sid).unwrap();
        // Keep a syntactically valid persisted binding but change the
        // participant identity. A matching route alone is not proof that
        // this is still the person/session the owner joined.
        let key = "join:gateway:v1:desktop:join:j-identity";
        let raw = core.store.lock_ok().meta_get(key).unwrap();
        let mut binding: serde_json::Value = serde_json::from_str(&raw).unwrap();
        binding["target_principal"]["subject"] = serde_json::json!("irc:someone-else");
        core.store
            .lock_ok()
            .meta_set(key, &binding.to_string())
            .unwrap();
        drop(core);
        // Dropping the fixture does not clear the process-wide join
        // registry. Remove its old asker so this assertion measures the
        // attempted restore, not the previous fixture's registration.
        veles_core::join::clear(&sid);
        let store = Arc::new(Mutex::new(
            Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        let restored = GatewayCore::new_join_test(store);
        restored.test_join_restore();
        assert!(!restored.test_join_live(&sid));
        assert!(veles_core::join::active(&sid).is_none());
        assert!(restored
            .desktop_input("j-identity", &sid, "not this identity")
            .is_err());
        assert!(restored
            .store
            .lock_ok()
            .queue_items(&sid)
            .unwrap()
            .is_empty());
    }
}

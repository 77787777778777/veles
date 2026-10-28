//! The mirror's desktop routes, served by this module under
//! `/api/plugins/slopdrop/*` (PLAN-plugins-installer.md R5; core's
//! `moduleroutes`). The desktop editor's seam: `/api/fs/*` is the
//! generic file view, but a buffer under the slopdrop mirror saves
//! through `save` so the write reaches the interpreter (the only path
//! that persists and commits correctly) — and the status line can say
//! so, because a save that went to a different place than the user
//! thinks is the silent automation the transcript rules forbid.

use serde_json::json;
use veles_core::moduleroutes::{ModuleRoutes, RouteFuture, RouteRequest, RouteResponse};

use crate::settings::settings;

pub struct Routes;

impl ModuleRoutes for Routes {
    fn handle<'a>(&'a self, req: RouteRequest) -> RouteFuture<'a> {
        Box::pin(async move {
            match (req.method.as_str(), req.path.as_str()) {
                ("GET", "/status") => status(),
                ("POST", "/sync") => sync().await,
                ("POST", "/save") => save(&req.body).await,
                _ => RouteResponse::not_found(&req.path),
            }
        })
    }

    /// `/api/plugins/slopdrop/open/*` — outside the desktop's bearer
    /// gate, so every route here authenticates itself: `bsky-read` takes
    /// a per-evaluation read grant (`crate::bsky`).
    fn handle_open<'a>(&'a self, req: RouteRequest) -> RouteFuture<'a> {
        Box::pin(async move {
            match (req.method.as_str(), req.path.as_str()) {
                ("POST", "/bsky-read") => {
                    crate::bsky::read_route(req.bearer.as_deref(), &req.body).await
                }
                _ => RouteResponse::not_found(&req.path),
            }
        })
    }
}

/// GET /status — the mirror as it stands on THIS disk (no HTTP against
/// the bridge: the panel must be able to describe an absent or
/// unreachable slopdrop).
fn status() -> RouteResponse {
    let st = crate::mirror::status(&settings());
    RouteResponse::ok(json!({
        "instance": st.instance,
        "root": st.root.display().to_string(),
        "synced": st.tsv,
        "procs": st.procs,
        "vars": st.vars,
        "synced_at": st.synced_at,
    }))
}

/// POST /sync — fetch slopdrop's live state and rebuild the mirror.
async fn sync() -> RouteResponse {
    match crate::mirror::sync(&settings()).await {
        Ok(r) => RouteResponse::ok(json!({
            "ok": true,
            "procs": r.procs,
            "vars": r.vars,
            "dropped": r.dropped,
            "root": r.root.display().to_string(),
        })),
        Err(e) => RouteResponse::error(502, e),
    }
}

/// POST /save {path, user} — deploy one edited proc file through
/// slopdrop's interpreter, conflict-checked against drift.
///
/// CONFLICT (409) is deliberate: the desktop reads the status and
/// renders the refusal as "changed on slopdrop while you were editing",
/// not as a generic failure. Everything else (not a mirror file, no tsv
/// row, bridge down, a slopdrop refusal) is a plain 400.
async fn save(body: &serde_json::Value) -> RouteResponse {
    let path = body["path"].as_str().unwrap_or("").trim().to_string();
    let user = body["user"].as_str().unwrap_or("").trim().to_string();
    if path.is_empty() {
        return RouteResponse::error(400, "no path");
    }
    let caller = if user.is_empty() {
        "local:owner"
    } else {
        user.as_str()
    };
    match crate::mirror::save(&settings(), std::path::Path::new(&path), caller).await {
        Ok(r) => RouteResponse::ok(json!({
            "ok": true,
            "name": r.name,
            "message": format!(
                "deployed '{}' through the interpreter{}",
                r.name,
                r.commit
                    .as_deref()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default()
            ),
        })),
        Err(e) => {
            let conflict = e.starts_with("CONFLICT");
            RouteResponse::status(
                if conflict { 409 } else { 400 },
                json!({ "ok": false, "error": e, "conflict": conflict }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_describes_the_mirror_on_this_disk_and_unknown_paths_are_404() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        crate::settings::pin_for_test(crate::settings::SlopdropSection::default());
        let r = Routes
            .handle(RouteRequest {
                method: "GET".into(),
                path: "/status".into(),
                ..Default::default()
            })
            .await;
        assert_eq!(r.status, 200);
        assert_eq!(r.body["synced"], false, "{}", r.body);
        assert!(
            r.body["root"].as_str().unwrap().ends_with("/default"),
            "{}",
            r.body
        );
        let nf = Routes
            .handle(RouteRequest {
                method: "GET".into(),
                path: "/nope".into(),
                ..Default::default()
            })
            .await;
        assert_eq!(nf.status, 404);
        let bad = Routes
            .handle(RouteRequest {
                method: "POST".into(),
                path: "/save".into(),
                body: json!({"path": ""}),
                ..Default::default()
            })
            .await;
        assert_eq!(bad.status, 400);
    }
}

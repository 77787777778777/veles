//! HTTP routes a module serves under the desktop API
//! (PLAN-plugins-installer.md R5: "desktop routes under
//! `/api/plugins/<name>/*`").
//!
//! The desktop API (veles-frontends) owns the listener and the bearer
//! check; a module owns what it answers. It registers a handler under
//! its name, and every request to `/api/plugins/<name>/<rest>` the API
//! has no route of its own for lands here, as a [`RouteRequest`]: the
//! method, the path AFTER the name, the query and the JSON body. The
//! handler answers a status and a JSON body. A name nobody serves is
//! 503 ("not loaded"), so a desktop pane for a module that is not
//! running fails closed and says why.
//!
//! OWNER-TRACKED (M4): the handler records the registering thread's
//! owner, so a module's routes are purged with it on reload, and a
//! reloaded module re-registers them in its `start`.
//!
//! OPEN ROUTES (2026-10-10). `/api/plugins/<name>/open/<rest>` skips the
//! API's bearer gate and lands in [`ModuleRoutes::handle_open`] — the
//! challenge pages' shape (a credential of their own, alive only while
//! it is needed): the slopdrop module's Bluesky reads take a per-
//! evaluation token slopdrop holds for seconds, and slopdrop must never
//! hold the desktop's owner bearer. The default answers 404, so a module
//! that does not authenticate a request itself serves no open route.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

/// One request to a module's routes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteRequest {
    /// `GET`, `POST`, … (upper case).
    pub method: String,
    /// The path after `/api/plugins/<name>`, with its leading slash:
    /// `/status`, `/sync`, `/save`. Never empty (the bare name is not a
    /// module route).
    pub path: String,
    pub query: BTreeMap<String, String>,
    /// The JSON body, `Null` when there was none (or it did not parse).
    pub body: serde_json::Value,
    /// The request's `Authorization: Bearer` credential, given to an
    /// OPEN route only (an ordinary route is past the API's own bearer
    /// check, and never sees the owner's token).
    pub bearer: Option<String>,
}

/// What a module answers.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

impl RouteResponse {
    pub fn ok(body: serde_json::Value) -> Self {
        Self { status: 200, body }
    }

    pub fn status(status: u16, body: serde_json::Value) -> Self {
        Self { status, body }
    }

    /// `{"ok": false, "error": …}` with a status.
    pub fn error(status: u16, error: impl Into<String>) -> Self {
        Self {
            status,
            body: serde_json::json!({"ok": false, "error": error.into()}),
        }
    }

    /// 404 for a path the module does not serve.
    pub fn not_found(path: &str) -> Self {
        Self::error(404, format!("no such route {path}"))
    }
}

pub type RouteFuture<'a> = Pin<Box<dyn Future<Output = RouteResponse> + Send + 'a>>;

/// A module's route handler.
pub trait ModuleRoutes: Send + Sync {
    fn handle<'a>(&'a self, req: RouteRequest) -> RouteFuture<'a>;
    /// A request that did NOT pass the API's bearer gate
    /// (`/api/plugins/<name>/open/<rest>`; `path` is `<rest>`). The module
    /// authenticates it itself or refuses it. The default serves nothing.
    fn handle_open<'a>(&'a self, req: RouteRequest) -> RouteFuture<'a> {
        Box::pin(async move { RouteResponse::not_found(&req.path) })
    }
}

fn registry() -> &'static crate::owned::OwnedRegistry<String, Arc<dyn ModuleRoutes>> {
    static R: OnceLock<crate::owned::OwnedRegistry<String, Arc<dyn ModuleRoutes>>> =
        OnceLock::new();
    R.get_or_init(crate::owned::OwnedRegistry::default)
}

/// Serve `/api/plugins/<name>/*` with `routes`. The owner is read from
/// the calling thread (M4); a later registration under the same name
/// by the same owner replaces the first.
pub fn register_routes(name: &str, routes: Arc<dyn ModuleRoutes>) {
    registry().insert(name.to_string(), routes);
}

/// Remove every route set `owner` registered (R3 teardown). `None`
/// (core) removes nothing. Returns the count removed.
pub fn purge_owner_routes(owner: Option<&'static str>) -> usize {
    registry().purge(owner)
}

/// The names with routes registered right now.
pub fn served() -> Vec<String> {
    registry().keys()
}

/// Dispatch one request to the module named. `None` when no module
/// serves `name` (the API answers 503 for that).
pub async fn dispatch(name: &str, req: RouteRequest) -> Option<RouteResponse> {
    let handler = registry().with(&name.to_string(), Arc::clone)?;
    Some(handler.handle(req).await)
}

/// Dispatch one OPEN request (no bearer gate) to the module named.
pub async fn dispatch_open(name: &str, req: RouteRequest) -> Option<RouteResponse> {
    let handler = registry().with(&name.to_string(), Arc::clone)?;
    Some(handler.handle_open(req).await)
}

/// What the API answers when no module serves `name`.
pub fn not_loaded(name: &str) -> RouteResponse {
    RouteResponse::error(
        503,
        format!("the module `{name}` is not loaded on this node; its routes are unavailable"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl ModuleRoutes for Echo {
        fn handle<'a>(&'a self, req: RouteRequest) -> RouteFuture<'a> {
            Box::pin(async move {
                if req.path == "/status" {
                    RouteResponse::ok(serde_json::json!({"method": req.method, "q": req.query}))
                } else {
                    RouteResponse::not_found(&req.path)
                }
            })
        }
    }

    #[tokio::test]
    async fn a_registered_module_answers_and_an_unknown_one_is_none() {
        register_routes("echo-routes-test", Arc::new(Echo));
        let mut q = BTreeMap::new();
        q.insert("a".to_string(), "1".to_string());
        let r = dispatch(
            "echo-routes-test",
            RouteRequest {
                method: "GET".into(),
                path: "/status".into(),
                query: q,
                body: serde_json::Value::Null,
                bearer: None,
            },
        )
        .await
        .expect("served");
        assert_eq!(r.status, 200);
        assert_eq!(r.body["method"], "GET");
        assert_eq!(r.body["q"]["a"], "1");
        let nf = dispatch(
            "echo-routes-test",
            RouteRequest {
                method: "GET".into(),
                path: "/nope".into(),
                ..Default::default()
            },
        )
        .await
        .expect("served");
        assert_eq!(nf.status, 404);
        assert!(dispatch("nobody", RouteRequest::default()).await.is_none());
        assert_eq!(not_loaded("nobody").status, 503);
    }
}

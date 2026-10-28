//! Authenticated component installer routes. The implementation shares
//! veles-install with the CLI; HTTP never supplies a config output path.

use crate::desktop_api::DesktopServer;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;
use veles_core::lockutil::LockOk as _;
use veles_install::{ComponentRequest, InstallService, Paths};

static INSTALL_WORK: OnceLock<Semaphore> = OnceLock::new();

fn worker<F>(f: F) -> impl std::future::Future<Output = (StatusCode, Json<Value>)>
where
    F: FnOnce() -> anyhow::Result<Value> + Send + 'static,
{
    blocking(f)
}

pub(crate) async fn setup_combined_preview(
    dirs: Option<veles_core::config::DirOverride>,
    answers: veles_core::setup::SetupAnswers,
    component: Option<ComponentRequest>,
) -> (StatusCode, Json<Value>) {
    worker(move || {
        let _dirs = veles_core::config::set_dirs(dirs);
        let service = service();
        let plan = service.preview_workflow(&answers, component.as_ref())?;
        let existing = std::fs::read_to_string(service.config_path()).unwrap_or_default();
        Ok(plan.review(&existing))
    })
    .await
}

pub(crate) struct CombinedApply {
    pub(crate) answers: veles_core::setup::SetupAnswers,
    pub(crate) component: Option<ComponentRequest>,
    pub(crate) expect_base: Option<String>,
    pub(crate) plan_sig: Option<String>,
    pub(crate) secrets: BTreeMap<String, String>,
    pub(crate) confirm: bool,
}

pub(crate) async fn setup_combined_apply(
    dirs: Option<veles_core::config::DirOverride>,
    srv: Arc<DesktopServer>,
    request: CombinedApply,
) -> (StatusCode, Json<Value>) {
    let CombinedApply {
        answers,
        component,
        expect_base,
        plan_sig,
        secrets,
        confirm,
    } = request;
    let permit = INSTALL_WORK
        .get_or_init(|| Semaphore::new(1))
        .acquire()
        .await;
    let permit = match permit {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok":false,"error":e.to_string()})),
            )
        }
    };
    let Some(expect_base) = expect_base else {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"ok":false,"error":"combined apply requires expect_base from its preview"}),
            ),
        );
    };
    let Some(plan_sig) = plan_sig else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":"combined apply requires the reviewed plan_sig"})),
        );
    };
    let result = worker(move || {
        let _dirs = veles_core::config::set_dirs(dirs);
        let service = service();
        // The manifest's source is used only for its description and
        // declared options in this slice. Artifact installation is its
        // own separately confirmed operation, not an implicit side effect.
        if !confirm {
            return Ok(json!({
                "ok": true, "preview": true, "confirm_required": true,
                "note": "review the combined configuration; no component artifact is implicitly installed or executed",
            }));
        }
        service.apply_workflow(&answers, component.as_ref(), &expect_base, &plan_sig, &secrets)
    }).await;
    drop(permit);
    if result.0 == StatusCode::OK
        && result.1.get("action").and_then(Value::as_str) == Some("setup-and-components")
    {
        let (code, mut value) = result;
        let reloaded = srv.reload_config().is_ok();
        value["reloaded"] = json!(reloaded);
        value["reload_required"] = json!(!reloaded);
        return (code, value);
    }
    result
}

fn service() -> InstallService {
    InstallService::new(Paths::current())
}

/// Keep request threads responsive and carry a test/embedder directory
/// override into the worker, just like the rest of the config layer.
async fn blocking<F>(f: F) -> (StatusCode, Json<Value>)
where
    F: FnOnce() -> anyhow::Result<Value> + Send + 'static,
{
    let dirs = veles_core::config::dirs();
    match veles_core::rt::spawn_blocking(move || {
        let _dirs = veles_core::config::set_dirs(dirs);
        f()
    })
    .await
    {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)),
        Ok(Err(e)) => {
            let text = format!("{e:#}");
            let conflict = text.contains("changed after preview")
                || text.contains("changed after profile preview")
                || text.contains("answers changed after preview")
                || text.contains("installation directory changed after signed preview")
                || text.contains("archive changed after signed download")
                || text.contains("signed-download review is still awaiting")
                || text.contains("signed-download request was cancelled")
                || text.contains("rollback state changed after preview");
            (
                if conflict {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::BAD_REQUEST
                },
                Json(json!({"ok": false, "error": text})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": e.to_string()})),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProfileBody {
    answers: veles_install::ProfileAnswers,
    #[serde(default)]
    core: Option<std::path::PathBuf>,
    #[serde(default)]
    base_sig: Option<String>,
    #[serde(default)]
    plan_sig: Option<String>,
    #[serde(default)]
    secrets: BTreeMap<String, String>,
    #[serde(default)]
    confirm: bool,
}

fn profile_request(body: &ProfileBody) -> anyhow::Result<veles_install::ProfileRequest> {
    let answers = body.answers.clone();
    let container = answers.profile == veles_install::profiles::NamedProfile::ContainerHost;
    let core = if matches!(
        answers.profile,
        veles_install::profiles::NamedProfile::Nixos
    ) || container
        || !answers.host.host_components.is_empty()
    {
        std::path::PathBuf::new()
    } else if let Some(core) = &body.core {
        staged_bundle(&BundleBody {
            source: core.clone(),
            confirm: false,
            plan_sig: None,
        })?
    } else {
        anyhow::bail!(
            "native profile needs a core bundle under the managed install-sources directory"
        )
    };
    if container {
        veles_install::ProfileRequest::host(answers)
    } else {
        veles_install::ProfileRequest::local(answers, core)
    }
}

pub(crate) async fn profile(Json(body): Json<ProfileBody>) -> (StatusCode, Json<Value>) {
    if body.confirm {
        let Some(base) = body.base_sig.clone() else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok":false,"error":"profile apply requires the preview base_sig"})),
            );
        };
        let Some(sig) = body.plan_sig.clone() else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok":false,"error":"profile apply requires the preview plan_sig"})),
            );
        };
        return blocking(move || {
            let service = service();
            service.apply_profile(profile_request(&body)?, &base, &sig, &body.secrets)
        })
        .await;
    }
    blocking(move || {
        let service = service();
        Ok(service.preview_profile(profile_request(&body)?)?.review())
    })
    .await
}

pub(crate) async fn state() -> (StatusCode, Json<Value>) {
    blocking(|| service().state()).await
}

pub(crate) async fn components() -> (StatusCode, Json<Value>) {
    blocking(|| Ok(json!({"components": service().components(true)?}))).await
}

pub(crate) async fn options(Path(name): Path<String>) -> (StatusCode, Json<Value>) {
    blocking(move || {
        let spec = service().component(&name)?;
        let mut manifest = spec.manifest.clone();
        for knob in &mut manifest.knobs {
            if veles_core::config_tool::names_a_secret(&knob.path) {
                knob.default = None;
                knob.note = Some("secret: use the setup/vault flow".into());
            }
        }
        Ok(json!({"component": name, "kind": spec.kind, "manifest": manifest}))
    })
    .await
}

#[derive(Debug, Deserialize)]
pub(crate) struct PreviewBody {
    component: String,
    #[serde(default)]
    instance: String,
    #[serde(default)]
    options: BTreeMap<String, Value>,
}

pub(crate) async fn preview(Json(body): Json<PreviewBody>) -> (StatusCode, Json<Value>) {
    blocking(move || {
        let plan = service().preview_config(&body.component, &body.instance, &body.options)?;
        Ok(plan.review())
    })
    .await
}

#[derive(Debug, Deserialize)]
pub(crate) struct ApplyBody {
    component: String,
    #[serde(default)]
    instance: String,
    #[serde(default)]
    options: BTreeMap<String, Value>,
    #[serde(default)]
    base_sig: Option<String>,
    plan_sig: String,
}

pub(crate) async fn apply(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<ApplyBody>,
) -> (StatusCode, Json<Value>) {
    let permit = match INSTALL_WORK
        .get_or_init(|| Semaphore::new(1))
        .acquire()
        .await
    {
        Ok(permit) => permit,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok": false, "error": e.to_string()})),
            )
        }
    };
    let result = blocking(move || {
        service().apply_config_answers(
            &body.component,
            &body.instance,
            &body.options,
            &body.base_sig,
            &body.plan_sig,
        )
    })
    .await;
    drop(permit);
    if result.0 == StatusCode::OK {
        let (code, mut value) = result;
        let reloaded = srv.reload_config().is_ok();
        value["reloaded"] = json!(reloaded);
        value["reload_required"] = json!(!reloaded);
        return (code, value);
    }
    result
}

#[derive(Debug, Deserialize)]
pub(crate) struct BundleBody {
    source: std::path::PathBuf,
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    plan_sig: Option<String>,
}

/// Desktop bundle sources are confined to a server-created staging root.
/// The CLI remains the interface for an arbitrary operator-reviewed path.
fn staged_bundle(body: &BundleBody) -> anyhow::Result<std::path::PathBuf> {
    let source = &body.source;
    anyhow::ensure!(source.is_absolute(), "bundle source must be absolute");
    let root = Paths::current().data_dir.join("install-sources").canonicalize()
        .map_err(|_| anyhow::anyhow!("no install-sources staging directory; the CLI accepts a reviewed local bundle path"))?;
    let source = source.canonicalize()?;
    anyhow::ensure!(
        source.starts_with(&root) && source != root,
        "desktop bundle source must be a child of the managed install-sources directory"
    );
    Ok(source)
}

pub(crate) async fn bundle(Json(body): Json<BundleBody>) -> (StatusCode, Json<Value>) {
    if !body.confirm {
        return blocking(move || Ok(service().preview_bundle(&staged_bundle(&body)?)?.review()))
            .await;
    }
    let Some(plan_sig) = body.plan_sig.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":"bundle apply requires the reviewed plan_sig"})),
        );
    };
    let permit = match INSTALL_WORK
        .get_or_init(|| Semaphore::new(1))
        .acquire()
        .await
    {
        Ok(permit) => permit,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok":false,"error":e.to_string()})),
            )
        }
    };
    let result =
        blocking(move || service().install_local_bundle(&staged_bundle(&body)?, &plan_sig)).await;
    drop(permit);
    result
}

#[derive(Debug, Deserialize)]
pub(crate) struct ComponentBody {
    name: String,
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    plan_sig: Option<String>,
}

pub(crate) async fn component(Json(body): Json<ComponentBody>) -> (StatusCode, Json<Value>) {
    if !body.confirm {
        return blocking(move || service().preview_component(&body.name)).await;
    }
    let Some(plan_sig) = body.plan_sig else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":"artifact apply requires the reviewed plan_sig"})),
        );
    };
    let permit = match INSTALL_WORK
        .get_or_init(|| Semaphore::new(1))
        .acquire()
        .await
    {
        Ok(permit) => permit,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok":false,"error":e.to_string()})),
            )
        }
    };
    let result = blocking(move || service().install_component(&body.name, Some(&plan_sig))).await;
    drop(permit);
    result
}

pub(crate) async fn rollback(Json(body): Json<ComponentBody>) -> (StatusCode, Json<Value>) {
    if !body.confirm {
        return blocking(move || service().preview_rollback(&body.name)).await;
    }
    let Some(plan_sig) = body.plan_sig else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":"rollback apply requires the reviewed plan_sig"})),
        );
    };
    let permit = match INSTALL_WORK
        .get_or_init(|| Semaphore::new(1))
        .acquire()
        .await
    {
        Ok(permit) => permit,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok":false,"error":e.to_string()})),
            )
        }
    };
    let result = blocking(move || service().rollback(&body.name, &plan_sig)).await;
    drop(permit);
    result
}

pub(crate) async fn verify() -> (StatusCode, Json<Value>) {
    blocking(|| service().verify()).await
}

const SIGNED_REVIEW_SECONDS: u64 = 3600;
const SIGNED_CANCELLED_LIMIT: usize = 64;

fn valid_request_id<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let id = String::deserialize(deserializer)?;
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(serde::de::Error::custom(
            "request id must be 1-128 visible ASCII characters",
        ));
    }
    Ok(id)
}

type PendingSigned = (String, Paths, veles_install::SignedPlan, std::time::Instant);
#[derive(Default)]
pub(crate) struct SignedDownloadState {
    pub(crate) pending: Option<PendingSigned>,
    pub(crate) in_flight: Option<String>,
    pub(crate) applying: Option<String>,
    /// FIFO tombstones for cancelled attempt ids. A cancellation can
    /// legitimately precede its preview request; a retry needs a new id.
    pub(crate) cancelled: std::collections::VecDeque<String>,
}
pub(crate) type SignedDownloads = std::sync::Mutex<SignedDownloadState>;

#[derive(Debug, Deserialize)]
pub(crate) struct SignedDownloadBody {
    #[serde(deserialize_with = "valid_request_id")]
    request_id: String,
    release: String,
    repository: String,
    tag: String,
    commit: String,
    workflow_sha: String,
    #[serde(default)]
    confirm: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SignedConfirmation {
    #[serde(deserialize_with = "valid_request_id")]
    plan_id: String,
    #[serde(default)]
    expected_manifest_sha256: Option<String>,
    #[serde(default)]
    confirm: bool,
}

async fn install_permit() -> Result<tokio::sync::SemaphorePermit<'static>, (StatusCode, Json<Value>)>
{
    INSTALL_WORK
        .get_or_init(|| Semaphore::new(1))
        .acquire()
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok":false,"error":format!("installer work queue unavailable: {e}")})),
            )
        })
}

/// An expired or cancelled plan is dropped, not retained as a reusable
/// archive. Confirmation consumes its id before archive revalidation.
pub(crate) async fn signed_download(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<SignedDownloadBody>,
) -> (StatusCode, Json<Value>) {
    if body.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"ok":false,"error":"use the separate signed-download confirmation with its reviewed plan id"}),
            ),
        );
    }
    let permit = match install_permit().await {
        Ok(p) => p,
        Err(e) => return e,
    };
    let id = body.request_id;
    {
        let mut slot = srv.signed_download.lock_ok();
        if slot.pending.as_ref().is_some_and(|(_, _, _, created)| {
            created.elapsed() > std::time::Duration::from_secs(SIGNED_REVIEW_SECONDS)
        }) {
            slot.pending.take();
        }
        if slot.cancelled.contains(&id) {
            drop(slot);
            drop(permit);
            return (
                StatusCode::CONFLICT,
                Json(
                    json!({"ok":false,"error":"signed-download request was cancelled; use a fresh request id to retry"}),
                ),
            );
        }
        if slot.pending.is_some() || slot.in_flight.is_some() || slot.applying.is_some() {
            drop(slot);
            drop(permit);
            return (
                StatusCode::CONFLICT,
                Json(
                    json!({"ok":false,"error":"a signed-download review is still awaiting confirmation or cancellation"}),
                ),
            );
        }
        slot.in_flight = Some(id.clone());
    }
    let paths = Paths::current();
    let timer_srv = srv.clone();
    let work_srv = srv.clone();
    let request = veles_install::ReleaseRequest {
        release_url: body.release,
        repository: body.repository,
        tag: body.tag,
        commit: body.commit,
        workflow_sha: body.workflow_sha,
    };
    let work_id = id.clone();
    let result = blocking(move || {
        let id = work_id;
        let plan =
            veles_install::InstallService::new(paths.clone()).preview_signed_bundle(&request)?;
        #[cfg(test)]
        {
            let pause = tests::TEST_SIGNED_PREVIEW_PAUSE.lock_ok().clone();
            if let Some(pause) = pause {
                pause.verified.wait();
                pause.resume.wait();
            }
        }
        let mut review = plan.review();
        review["preview"] = json!(true);
        review["confirm_required"] = json!(true);
        review["plan_id"] = json!(id.clone());
        review["review_expires_seconds"] = json!(SIGNED_REVIEW_SECONDS);
        let mut slot = work_srv.signed_download.lock_ok();
        anyhow::ensure!(
            slot.in_flight.as_deref() == Some(id.as_str()) && !slot.cancelled.contains(&id),
            "signed-download request was cancelled or superseded; its staging was discarded"
        );
        let fresh = slot.pending.as_ref().is_none_or(|(_, _, _, created)| {
            created.elapsed() > std::time::Duration::from_secs(SIGNED_REVIEW_SECONDS)
        });
        anyhow::ensure!(
            fresh,
            "a signed-download review is still awaiting confirmation or cancellation"
        );
        slot.pending = Some((id.clone(), paths, plan, std::time::Instant::now()));
        slot.in_flight = None;
        Ok(review)
    })
    .await;
    if result.0 == StatusCode::OK {
        if let Some(id) = result.1["plan_id"].as_str().map(str::to_owned) {
            veles_core::rt::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(SIGNED_REVIEW_SECONDS)).await;
                let mut slot = timer_srv.signed_download.lock_ok();
                if slot
                    .pending
                    .as_ref()
                    .is_some_and(|(pending, _, _, created)| {
                        pending == &id
                            && created.elapsed()
                                >= std::time::Duration::from_secs(SIGNED_REVIEW_SECONDS)
                    })
                {
                    slot.pending.take();
                }
            });
        }
    } else {
        let mut slot = timer_srv.signed_download.lock_ok();
        if slot.in_flight.as_deref() == Some(id.as_str()) {
            slot.in_flight = None;
        }
        drop(slot);
    }
    drop(permit);
    result
}

pub(crate) async fn signed_download_confirm(
    State(srv): State<Arc<DesktopServer>>,
    Json(body): Json<SignedConfirmation>,
) -> (StatusCode, Json<Value>) {
    if body.confirm {
        let permit = match install_permit().await {
            Ok(p) => p,
            Err(e) => return e,
        };
        let (_id, paths, plan) = {
            let mut slot = srv.signed_download.lock_ok();
            let Some((id, paths, plan, created)) = slot.pending.take() else {
                drop(slot);
                drop(permit);
                return (
                    StatusCode::CONFLICT,
                    Json(
                        json!({"ok":false,"error":"signed-download preview is missing, cancelled, expired or already used; review a fresh download"}),
                    ),
                );
            };
            if id != body.plan_id
                || created.elapsed() > std::time::Duration::from_secs(SIGNED_REVIEW_SECONDS)
                || body.expected_manifest_sha256.as_deref()
                    != Some(plan.download.manifest_sha256.as_str())
            {
                drop(slot);
                drop(permit);
                return (
                    StatusCode::CONFLICT,
                    Json(
                        json!({"ok":false,"error":"signed-download preview is stale or does not match the reviewed manifest; its staging was discarded"}),
                    ),
                );
            }
            if slot.cancelled.contains(&body.plan_id) {
                drop(slot);
                drop(permit);
                return (
                    StatusCode::CONFLICT,
                    Json(
                        json!({"ok":false,"error":"signed-download request was cancelled before apply; its staging was discarded"}),
                    ),
                );
            }
            // Apply and cancellation are serialized by the same state lock.
            // Once this transition commits, cancel cannot claim to discard it.
            slot.applying = Some(id.clone());
            (id, paths, plan)
        };
        let result =
            blocking(move || veles_install::InstallService::new(paths).apply_signed_bundle(plan))
                .await;
        srv.signed_download.lock_ok().applying = None;
        drop(permit);
        return result;
    }
    let mut slot = srv.signed_download.lock_ok();
    if slot.applying.is_some() {
        // Even a different request cannot be truthfully discarded while
        // this exclusive installation is committing its reviewed plan.
        return (
            StatusCode::CONFLICT,
            Json(
                json!({"ok":false,"cancelled":false,"error":"signed-download apply has started; cancellation is no longer possible"}),
            ),
        );
    }
    if slot.cancelled.contains(&body.plan_id) {
        return (
            StatusCode::OK,
            Json(json!({"ok":true,"cancelled":true,"discarded":false})),
        );
    }
    let discarded = slot
        .pending
        .as_ref()
        .is_some_and(|(id, _, _, _)| id == &body.plan_id)
        || slot.in_flight.as_deref() == Some(body.plan_id.as_str());
    if slot
        .pending
        .as_ref()
        .is_some_and(|(id, _, _, _)| id == &body.plan_id)
    {
        slot.pending.take();
    }
    // Before, during or after preview. The worker observes the tombstone
    // and drops its private plan; in-flight ownership is retained so its
    // late response cannot publish. A different pending request is untouched.
    slot.cancelled.push_back(body.plan_id.clone());
    while slot.cancelled.len() > SIGNED_CANCELLED_LIMIT {
        slot.cancelled.pop_front();
    }
    drop(slot);
    (
        StatusCode::OK,
        Json(json!({"ok":true,"cancelled":true,"discarded":discarded})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    use veles_core::hooks::HookBus;
    use veles_core::store::Store;

    fn fixture(home: &std::path::Path) -> std::path::PathBuf {
        let root = home.join("process-plugins/echo");
        std::fs::create_dir_all(&root).unwrap();
        // Cargo builds the same real process plugin as the lifecycle
        // tests. Copying a shell fixture worked on Unix but cannot be
        // spawned as a program on Windows.
        let exe = root.join("plugin");
        std::fs::copy(veles_core::testenv::process_plugin_echo(), &exe).unwrap();
        std::fs::write(
            root.join("plugin.toml"),
            "name = \"echo\"\nkind = \"process\"\nentry = \"plugin\"\n",
        )
        .unwrap();
        let output = std::process::Command::new(&exe)
            .arg("--describe")
            .output()
            .expect("the real plugin describes itself");
        assert!(output.status.success());
        let manifest: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(manifest["knobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k["path"] == "platforms.echo.rooms"));
        exe
    }

    fn server() -> (
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
        Arc<crate::desktop_api::DesktopServer>,
    ) {
        let (home, env) = veles_core::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(&dir.path().join("state.db")).unwrap(),
        ));
        let srv = Arc::new(crate::desktop_api::DesktopServer::new(
            Arc::new(veles_core::config::VelesConfig::default()),
            store,
            HookBus::new(None),
        ));
        (home, env, srv)
    }

    fn preview_body() -> PreviewBody {
        serde_json::from_value(json!({
            "component": "echo", "instance": "",
            "options": {"platforms.echo.rooms.fail_start": true}
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn preview_and_apply_are_typed_and_secret_safe() {
        let (home, _env, srv) = server();
        fixture(home.path());
        let config = home.path().join("config.toml");
        std::fs::write(
            &config,
            "# keep\n[platforms.fixture]\ntoken = \"not-in-the-review\"\n",
        )
        .unwrap();
        let (code, axum::Json(preview)) = preview(axum::Json(preview_body())).await;
        assert_eq!(code, StatusCode::OK, "{preview}");
        let wire = preview.to_string();
        assert!(!wire.contains("not-in-the-review"), "{wire}");
        assert_eq!(preview["config_text"], Value::Null);
        assert!(preview["plan_sig"].is_string());
        let body: ApplyBody = serde_json::from_value(json!({
            "component": "echo", "instance": "",
            "options": {"platforms.echo.rooms.fail_start": true},
            "base_sig": preview["base_sig"], "plan_sig": preview["plan_sig"]
        }))
        .unwrap();
        let (code, axum::Json(applied)) = apply(State(srv), axum::Json(body)).await;
        assert_eq!(code, StatusCode::OK, "{applied}");
        let text = std::fs::read_to_string(config).unwrap();
        assert!(text.contains("fail_start = true"));
        assert!(text.contains("not-in-the-review"));
    }

    fn profile_bundle(home: &std::path::Path) -> std::path::PathBuf {
        let source = home.join("install-sources/core");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::write(source.join("bin/veles"), "profile core\n").unwrap();
        let architecture = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else {
            "aarch64"
        };
        std::fs::write(
            source.join("BUNDLE-INFO.json"),
            serde_json::to_vec(
                &json!({"version":"1","revision":"profile","architecture":architecture}),
            )
            .unwrap(),
        )
        .unwrap();
        source
    }

    // Windows: installing a core bundle needs the native junction selection
    // that bundle.rs refuses by name; preview works there.
    #[cfg(unix)]
    #[tokio::test]
    async fn profile_preview_and_apply_use_the_managed_bundle_and_typed_answers() {
        let (home, _env, _srv) = server();
        let source = profile_bundle(home.path());
        let body = || {
            serde_json::from_value::<ProfileBody>(json!({"answers":{"profile":"desktop","setup":{"skipped":veles_core::setup::SECTIONS}},"core":source})).unwrap()
        };
        let (code, axum::Json(preview)) = profile(axum::Json(body())).await;
        assert_eq!(code, StatusCode::OK, "{preview}");
        assert_eq!(preview["preview"], true);
        assert_eq!(preview["config_toml"], Value::Null);
        let mut apply_body = body();
        apply_body.confirm = true;
        apply_body.base_sig = preview["base_sig"].as_str().map(str::to_string);
        apply_body.plan_sig = preview["plan_sig"].as_str().map(str::to_string);
        let (code, axum::Json(applied)) = profile(axum::Json(apply_body)).await;
        assert_eq!(code, StatusCode::OK, "{applied}");
        assert_eq!(applied["applied"], true);
        assert!(home.path().join("install/current/bin/veles").is_file());
        assert!(home.path().join("config.toml").is_file());
    }

    // Host components and container hosts are Linux-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn pds_profile_is_typed_and_only_prepares_a_handoff() {
        let (_home, _env, _srv) = server();
        let component = json!({
            "kind":"pds", "public_url":"https://pds.example",
            "handle_domain":"pds.example", "acme_email":"acme@example",
            "backup_confirmed":false,
            "image":{"source":"ghcr.io/jazware/vlpds:1.2.2","digest":format!("sha256:{}", "a".repeat(64))},
            "s3_endpoint":"https://s3.example", "s3_region":"auto",
            "s3_bucket":"bucket", "s3_prefix":"pds",
            "gateway_unit":"veles-gateway.service",
            "gateway_user":veles_install::current_user_name().unwrap(),
            "network":"veles-net", "gateway_quadlet":"veles-gateway"
        });
        let body: ProfileBody = serde_json::from_value(json!({
            "answers":{"profile":"desktop","setup":{"skipped":veles_core::setup::SECTIONS},
                "host":{"host_components":[component]}}
        }))
        .unwrap();
        let (code, axum::Json(preview)) = profile(axum::Json(body)).await;
        assert_eq!(code, StatusCode::OK, "{preview}");
        assert_eq!(
            preview["host_components"][0]["host_apply_implemented"],
            true
        );
        assert_eq!(preview["host_components"][0]["backup_gate_required"], true);
        assert!(preview["secrets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "VLPDS_S3_SECRET_KEY"));
        assert!(!preview.to_string().contains("fixture-secret"));
        let mut apply: ProfileBody = serde_json::from_value(json!({
            "answers":{"profile":"desktop","setup":{"skipped":veles_core::setup::SECTIONS},
                "host":{"host_components":[component]}}, "confirm":true,
            "base_sig":preview["base_sig"], "plan_sig":preview["plan_sig"]
        }))
        .unwrap();
        apply
            .secrets
            .insert("VLPDS_S3_ACCESS_KEY".into(), "fixture".into());
        let (code, axum::Json(applied)) = profile(axum::Json(apply)).await;
        assert_eq!(code, StatusCode::OK, "{applied}");
        assert_eq!(applied["status"], "handoff-required");
        assert_eq!(applied["applied"], false);
        assert!(applied["command"]
            .as_str()
            .unwrap()
            .contains("profile-handoff"));
    }

    #[tokio::test]
    async fn profile_stale_plan_is_a_conflict_and_nixos_only_emits() {
        let (home, _env, _srv) = server();
        let source = profile_bundle(home.path());
        let body: ProfileBody = serde_json::from_value(json!({"answers":{"profile":"desktop","setup":{"skipped":veles_core::setup::SECTIONS}},"core":source})).unwrap();
        let (_, axum::Json(preview)) = profile(axum::Json(body)).await;
        std::fs::write(home.path().join("config.toml"), "# operator edit\n").unwrap();
        let changed: ProfileBody = serde_json::from_value(json!({"answers":{"profile":"desktop","setup":{"skipped":veles_core::setup::SECTIONS}},"core":home.path().join("install-sources/core"),"base_sig":preview["base_sig"],"plan_sig":preview["plan_sig"],"confirm":true})).unwrap();
        let (code, axum::Json(error)) = profile(axum::Json(changed)).await;
        assert_eq!(code, StatusCode::CONFLICT, "{error}");
        let nix: ProfileBody = serde_json::from_value(
            json!({"answers":{"profile":"nixos","setup":{"skipped":veles_core::setup::SECTIONS}}}),
        )
        .unwrap();
        let (code, axum::Json(preview)) = profile(axum::Json(nix)).await;
        assert_eq!(code, StatusCode::OK, "{preview}");
        assert_eq!(preview["emitted"][0]["name"], "veles-nixos.nix");
        assert!(preview["emitted"][0]["bytes"].as_u64().unwrap() > 0);
        let mut emit: ProfileBody = serde_json::from_value(
            json!({"answers":{"profile":"nixos","setup":{"skipped":veles_core::setup::SECTIONS}}}),
        )
        .unwrap();
        emit.confirm = true;
        emit.base_sig = preview["base_sig"].as_str().map(str::to_string);
        emit.plan_sig = preview["plan_sig"].as_str().map(str::to_string);
        let (code, axum::Json(emitted)) = profile(axum::Json(emit)).await;
        assert_eq!(code, StatusCode::OK, "{emitted}");
        assert_eq!(emitted["status"], "emitted");
        assert_eq!(emitted["applied"], false);
        assert!(emitted["emitted"][0]["contents"]
            .as_str()
            .unwrap()
            .contains("services.veles"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn container_profile_is_typed_and_only_prepares_a_handoff() {
        let (home, _env, _srv) = server();
        let root = home.path().join("storage");
        let image = json!({"source":"docker.io/library/alpine:3.20","digest":format!("sha256:{}", "a".repeat(64))});
        let gateway =
            json!({"name":"gw","home":root.join("gw"),"rootless":true,"system":true,"group":null});
        let sandbox = json!({"name":"sbx","home":root.join("sbx"),"rootless":true,"system":true,"group":null});
        let options = json!({"gateway":gateway,"sandbox":sandbox,"extra_users":[],"storage":{"root":root,"source":null,"requires_mounts_for":null,"bind":false,"mode":493,"home_mode":448},"socket":{"dir":"/run/sbx","user":"sbx","group":"gw","dir_mode":488,"socket_mode":438},"unit_prefix":"veles","packages":{"manager":"apt","install_missing":false},"images":[],"quadlets":[{"user":"gw","name":"gateway","container":{"description":"gateway fixture","image":image,"extra":[]}}],"tor":null,"firewall":null,"network":"velesnet","peer_sync":null});
        let request = || {
            serde_json::from_value::<ProfileBody>(
                json!({"answers":{"profile":"container-host","host":{"container":options}}}),
            )
            .unwrap()
        };
        let (code, axum::Json(preview)) = profile(axum::Json(request())).await;
        assert_eq!(code, StatusCode::OK, "{preview}");
        assert!(preview["diff"].as_str().unwrap().is_empty());
        assert!(preview["user_steps"].as_array().unwrap().is_empty());
        let mut body = request();
        body.confirm = true;
        body.base_sig = preview["base_sig"].as_str().map(str::to_string);
        body.plan_sig = preview["plan_sig"].as_str().map(str::to_string);
        let (code, axum::Json(prepared)) = profile(axum::Json(body)).await;
        assert_eq!(code, StatusCode::OK, "{prepared}");
        assert_eq!(prepared["status"], "handoff-required");
        assert_eq!(prepared["applied"], false);
        assert!(!home.path().join("install.toml").exists());
    }

    #[tokio::test]
    async fn profile_core_must_be_managed_and_native_profiles_require_it() {
        let (home, _env, _srv) = server();
        let missing: ProfileBody = serde_json::from_value(json!({"answers":{"profile":"desktop","setup":{"skipped":veles_core::setup::SECTIONS}}})).unwrap();
        let (code, axum::Json(error)) = profile(axum::Json(missing)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{error}");
        let outside = home.path().join("outside");
        std::fs::create_dir_all(outside.join("bin")).unwrap();
        let body: ProfileBody = serde_json::from_value(json!({"answers":{"profile":"desktop","setup":{"skipped":veles_core::setup::SECTIONS}},"core":outside})).unwrap();
        let (code, axum::Json(error)) = profile(axum::Json(body)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{error}");
    }

    #[tokio::test]
    async fn stale_apply_returns_conflict_and_does_not_write() {
        let (home, _env, srv) = server();
        fixture(home.path());
        let config = home.path().join("config.toml");
        std::fs::write(&config, "# original\n").unwrap();
        let (_, axum::Json(previewed)) = preview(axum::Json(preview_body())).await;
        std::fs::write(&config, "# concurrent edit\n").unwrap();
        let body: ApplyBody = serde_json::from_value(json!({
            "component": "echo", "instance": "",
            "options": {"platforms.echo.rooms.fail_start": true},
            "base_sig": previewed["base_sig"], "plan_sig": previewed["plan_sig"]
        }))
        .unwrap();
        let (code, axum::Json(result)) = apply(State(srv), axum::Json(body)).await;
        assert_eq!(code, StatusCode::CONFLICT, "{result}");
        assert_eq!(
            std::fs::read_to_string(config).unwrap(),
            "# concurrent edit\n"
        );
    }

    #[tokio::test]
    async fn component_confirmation_is_bound_to_the_reviewed_artifact_and_manifest() {
        let (home, _env, _srv) = server();
        let exe = fixture(home.path());
        let (_, axum::Json(review)) = component(axum::Json(ComponentBody {
            name: "echo".into(),
            confirm: false,
            plan_sig: None,
        }))
        .await;
        let signature = review["plan_sig"].as_str().unwrap_or("");
        assert!(
            !signature.is_empty(),
            "artifact preview must bind its reviewed source: {review}"
        );
        let mut payload = std::fs::read(&exe).unwrap();
        payload.extend_from_slice(b"\0changed after the operator reviewed it\0");
        std::fs::write(&exe, payload).unwrap();
        let (code, axum::Json(result)) = component(axum::Json(ComponentBody {
            name: "echo".into(),
            confirm: true,
            plan_sig: Some(signature.into()),
        }))
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{result}");
        assert!(!home.path().join("install.toml").exists());
    }

    #[cfg(target_arch = "x86_64")]
    #[tokio::test]
    async fn changed_bundle_payload_cannot_reuse_a_desktop_preview() {
        let (home, _env, _srv) = server();
        let root = home.path().join("install-sources/reviewed");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(
            root.join("BUNDLE-INFO.json"),
            "{\"version\":\"1\",\"revision\":\"a\",\"architecture\":\"x86_64\"}",
        )
        .unwrap();
        std::fs::write(root.join("bin/veles"), "reviewed\n").unwrap();
        let body = |confirm, sig| BundleBody {
            source: root.clone(),
            confirm,
            plan_sig: sig,
        };
        let (_, axum::Json(preview)) = bundle(axum::Json(body(false, None))).await;
        let signature = preview["plan_sig"].as_str().unwrap().to_string();
        std::fs::write(root.join("bin/veles"), "changed\n").unwrap();
        let (code, axum::Json(result)) = bundle(axum::Json(body(true, Some(signature)))).await;
        assert_eq!(code, StatusCode::CONFLICT, "{result}");
        assert!(!home.path().join("install.toml").exists());
        assert!(!home.path().join("install/current").exists());
    }

    #[cfg(test)]
    pub(crate) struct SignedPreviewTestPause {
        pub(crate) verified: std::sync::Barrier,
        pub(crate) resume: std::sync::Barrier,
    }

    #[cfg(all(test, target_os = "linux"))]
    impl SignedPreviewTestPause {
        fn new() -> Self {
            Self {
                verified: std::sync::Barrier::new(2),
                resume: std::sync::Barrier::new(2),
            }
        }
    }

    #[cfg(test)]
    pub(crate) static TEST_SIGNED_PREVIEW_PAUSE: Mutex<Option<Arc<SignedPreviewTestPause>>> =
        Mutex::new(None);

    fn signed_cancel(request_id: &str) -> SignedConfirmation {
        SignedConfirmation {
            plan_id: request_id.into(),
            expected_manifest_sha256: None,
            confirm: false,
        }
    }

    #[cfg(target_os = "linux")]
    async fn signed_request(release: String) -> SignedDownloadBody {
        signed_request_for(release, "signed-test-request").await
    }

    #[cfg(target_os = "linux")]
    async fn signed_request_for(release: String, request_id: &str) -> SignedDownloadBody {
        SignedDownloadBody {
            request_id: if request_id == "signed-test-request" {
                uuid::Uuid::new_v4().to_string()
            } else {
                request_id.into()
            },
            release,
            repository: "77787777778777/veles".into(),
            tag: "v1.0.0".into(),
            commit: "abcdef1234567890abcdef1234567890abcdef12".into(),
            workflow_sha: "1234567890abcdef1234567890abcdef12345678".into(),
            confirm: false,
        }
    }

    #[cfg(target_os = "linux")]
    async fn signed_review(srv: Arc<DesktopServer>, release: String) -> Value {
        let (code, axum::Json(review)) =
            signed_download(State(srv), axum::Json(signed_request(release).await)).await;
        assert_eq!(code, StatusCode::OK, "{review}");
        review
    }

    #[cfg(target_os = "linux")]
    fn signed_confirmation(review: &Value, confirm: bool) -> SignedConfirmation {
        SignedConfirmation {
            plan_id: review["plan_id"].as_str().unwrap().into(),
            expected_manifest_sha256: Some(
                review["download"]["manifest_sha256"]
                    .as_str()
                    .unwrap()
                    .into(),
            ),
            confirm,
        }
    }

    #[cfg(target_os = "linux")]
    fn signed_stage(home: &std::path::Path) -> std::path::PathBuf {
        let root = home.join("install-sources/.signed-download-staging");
        let mut children = std::fs::read_dir(&root).unwrap();
        let stage = children.next().unwrap().unwrap().path();
        assert!(
            children.next().is_none(),
            "one private plan owns one staging directory"
        );
        stage
    }

    #[cfg(target_os = "linux")]
    fn assert_no_signed_staging(home: &std::path::Path) {
        let root = home.join("install-sources/.signed-download-staging");
        assert!(
            !root.exists() || std::fs::read_dir(root).unwrap().next().is_none(),
            "no downloaded archive or reviewed tree remains"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn signed_download_review_is_server_owned_and_cancel_cleans_staging() {
        let (home, _env, srv) = server();
        let (release, socket) = veles_install::test_support::fixture(home.path(), false, false);
        std::env::set_var("VELES_TEST_COSIGN", home.path().join("cosign"));
        std::env::set_var("VELES_TEST_RELEASE_UNIX_SOCKET", socket);
        let review = signed_review(srv.clone(), release).await;
        assert_eq!(review["action"], "install-signed-bundle");
        assert!(review["preview"] == true && review["confirm_required"] == true);
        assert!(
            !review.to_string().contains("signed-download-staging"),
            "{review}"
        );
        let stage = signed_stage(home.path());
        assert!(stage.is_dir());
        let (code, axum::Json(cancelled)) =
            signed_download_confirm(State(srv), axum::Json(signed_confirmation(&review, false)))
                .await;
        assert_eq!(code, StatusCode::OK, "{cancelled}");
        assert!(!stage.exists());
        assert!(!home.path().join("install.toml").exists());
        assert!(!home.path().join("install/current").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn signed_download_apply_rechecks_staged_bytes_and_consumes_the_review() {
        let (home, _env, srv) = server();
        let (release, socket) = veles_install::test_support::fixture(home.path(), false, false);
        std::env::set_var("VELES_TEST_COSIGN", home.path().join("cosign"));
        std::env::set_var("VELES_TEST_RELEASE_UNIX_SOCKET", socket);
        let review = signed_review(srv.clone(), release).await;
        let archive = signed_stage(home.path()).join("veles-1.0.0-abcdef1-linux-x86_64.tar.zst");
        let bytes = std::fs::read(&archive).unwrap();
        std::fs::write(&archive, bytes).unwrap(); // same bytes, different file identity
        let (code, axum::Json(installed)) = signed_download_confirm(
            State(srv.clone()),
            axum::Json(signed_confirmation(&review, true)),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{installed}");
        assert_eq!(installed["action"], "install-signed-bundle");
        assert_no_signed_staging(home.path());
        let (code, axum::Json(stale)) =
            signed_download_confirm(State(srv), axum::Json(signed_confirmation(&review, true)))
                .await;
        assert_eq!(code, StatusCode::CONFLICT, "{stale}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn changed_archive_and_wrong_review_identity_cannot_reuse_a_desktop_plan() {
        let (home, _env, srv) = server();
        let (release, socket) = veles_install::test_support::fixture(home.path(), false, false);
        std::env::set_var("VELES_TEST_COSIGN", home.path().join("cosign"));
        std::env::set_var("VELES_TEST_RELEASE_UNIX_SOCKET", socket);
        let review = signed_review(srv.clone(), release).await;
        let archive = signed_stage(home.path()).join("veles-1.0.0-abcdef1-linux-x86_64.tar.zst");
        let mut bytes = std::fs::read(&archive).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&archive, bytes).unwrap();
        let (code, axum::Json(refused)) =
            signed_download_confirm(State(srv), axum::Json(signed_confirmation(&review, true)))
                .await;
        assert_eq!(code, StatusCode::CONFLICT, "{refused}");
        assert!(!home.path().join("install.toml").exists());
        assert!(!home.path().join("install/current").exists());
        assert_no_signed_staging(home.path());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_old_cancel_cannot_discard_a_new_signed_review() {
        let (home, _env, srv) = server();
        let (release, socket) = veles_install::test_support::fixture(home.path(), false, false);
        std::env::set_var("VELES_TEST_COSIGN", home.path().join("cosign"));
        std::env::set_var("VELES_TEST_RELEASE_UNIX_SOCKET", socket);
        let first = signed_review(srv.clone(), release.clone()).await;
        let (_, axum::Json(first_cancel)) = signed_download_confirm(
            State(srv.clone()),
            axum::Json(signed_confirmation(&first, false)),
        )
        .await;
        assert_eq!(first_cancel["discarded"], true);
        let second = signed_review(srv.clone(), release).await;
        assert_ne!(first["plan_id"], second["plan_id"]);
        let (_, axum::Json(cancel)) = signed_download_confirm(
            State(srv.clone()),
            axum::Json(signed_confirmation(&first, false)),
        )
        .await;
        assert_eq!(cancel["discarded"], false);
        let (_, axum::Json(cancel)) = signed_download_confirm(
            State(srv.clone()),
            axum::Json(signed_confirmation(&second, false)),
        )
        .await;
        assert_eq!(cancel["discarded"], true);
        assert_no_signed_staging(home.path());
        let (_, axum::Json(cancel)) =
            signed_download_confirm(State(srv), axum::Json(signed_confirmation(&second, false)))
                .await;
        assert_eq!(cancel["discarded"], false, "cancellation is idempotent");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn signed_confirmation_must_match_the_reviewed_manifest() {
        let (home, _env, srv) = server();
        let (release, socket) = veles_install::test_support::fixture(home.path(), false, false);
        std::env::set_var("VELES_TEST_COSIGN", home.path().join("cosign"));
        std::env::set_var("VELES_TEST_RELEASE_UNIX_SOCKET", socket);
        let review = signed_review(srv.clone(), release).await;
        let mut body = signed_confirmation(&review, true);
        body.expected_manifest_sha256 = Some("not-the-reviewed-manifest".into());
        let (code, axum::Json(refused)) =
            signed_download_confirm(State(srv), axum::Json(body)).await;
        assert_eq!(code, StatusCode::CONFLICT, "{refused}");
        assert_no_signed_staging(home.path());
        assert!(!home.path().join("install.toml").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn pending_review_is_not_silently_replaced_and_expiry_cleans_it() {
        let (home, _env, srv) = server();
        let (release, socket) = veles_install::test_support::fixture(home.path(), false, false);
        std::env::set_var("VELES_TEST_COSIGN", home.path().join("cosign"));
        std::env::set_var("VELES_TEST_RELEASE_UNIX_SOCKET", socket);
        let first = signed_review(srv.clone(), release.clone()).await;
        let (code, axum::Json(busy)) = signed_download(
            State(srv.clone()),
            axum::Json(signed_request(release.clone()).await),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{busy}");
        let stage = signed_stage(home.path());
        assert!(stage.is_dir());
        srv.signed_download.lock_ok().pending.as_mut().unwrap().3 -=
            std::time::Duration::from_secs(SIGNED_REVIEW_SECONDS + 1);
        let (code, axum::Json(refused)) = signed_download_confirm(
            State(srv.clone()),
            axum::Json(signed_confirmation(&first, true)),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{refused}");
        assert!(!stage.exists());
        let second = signed_review(srv.clone(), release).await;
        assert_ne!(first["plan_id"], second["plan_id"]);
        let (_, axum::Json(cancel)) =
            signed_download_confirm(State(srv), axum::Json(signed_confirmation(&second, false)))
                .await;
        assert_eq!(cancel["discarded"], true);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_an_inflight_download_invalidates_its_late_preview() {
        let (home, _env, srv) = server();
        let (release, socket) = veles_install::test_support::fixture(home.path(), false, false);
        std::env::set_var("VELES_TEST_COSIGN", home.path().join("cosign"));
        std::env::set_var("VELES_TEST_RELEASE_UNIX_SOCKET", socket);
        let pause = Arc::new(SignedPreviewTestPause::new());
        *TEST_SIGNED_PREVIEW_PAUSE.lock_ok() = Some(pause.clone());
        let review = veles_core::rt::spawn({
            let srv = srv.clone();
            async move {
                signed_download(
                    State(srv),
                    axum::Json(signed_request_for(release, "signed-race").await),
                )
                .await
            }
        });
        // Wait for genuine download/cosign/archive verification to finish;
        // the worker has not yet published a plan for this request.
        pause.verified.wait();
        let (_, axum::Json(cancelled)) =
            signed_download_confirm(State(srv.clone()), axum::Json(signed_cancel("signed-race")))
                .await;
        pause.resume.wait();
        let (code, axum::Json(review)) = review.await.unwrap();
        assert_eq!(cancelled["cancelled"], true);
        assert_eq!(cancelled["discarded"], true);
        assert_eq!(code, StatusCode::CONFLICT, "{review}");
        assert!(
            review["error"].as_str().unwrap().contains("cancelled"),
            "{review}"
        );
        assert!(srv.signed_download.lock_ok().pending.is_none());
        assert!(srv.signed_download.lock_ok().in_flight.is_none());
        assert_no_signed_staging(home.path());
        assert!(!home.path().join("install.toml").exists());
        *TEST_SIGNED_PREVIEW_PAUSE.lock_ok() = None;
    }

    #[tokio::test]
    async fn a_cancellation_cannot_discard_an_apply_already_in_progress() {
        let (_home, _env, srv) = server();
        let request_id = uuid::Uuid::new_v4().to_string();
        srv.signed_download.lock_ok().applying = Some(request_id.clone());
        let (code, axum::Json(result)) =
            signed_download_confirm(State(srv), axum::Json(signed_cancel(&request_id))).await;
        assert_eq!(code, StatusCode::CONFLICT, "{result}");
        assert_eq!(result["cancelled"], false);
    }

    #[test]
    fn request_ids_are_bounded_and_visible() {
        for id in ["", "contains space", "line\nfeed", "xé"] {
            assert!(
                serde_json::from_value::<SignedDownloadBody>(json!({
                    "request_id": id, "release": "", "repository": "", "tag": "",
                    "commit": "", "workflow_sha": ""
                }))
                .is_err(),
                "accepted {id:?}"
            );
        }
        assert!(serde_json::from_value::<SignedConfirmation>(json!({
            "plan_id": "x".repeat(129), "confirm": false
        }))
        .is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_failed_download_does_not_leave_its_request_in_flight() {
        let (_home, _env, srv) = server();
        let request = SignedDownloadBody {
            request_id: uuid::Uuid::new_v4().to_string(),
            release: "https://not-github.example/release".into(),
            repository: "77787777778777/veles".into(),
            tag: "v1.0.0".into(),
            commit: "abcdef1234567890abcdef1234567890abcdef12".into(),
            workflow_sha: "1234567890abcdef1234567890abcdef12345678".into(),
            confirm: false,
        };
        let (code, _) = signed_download(State(srv.clone()), axum::Json(request)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(srv.signed_download.lock_ok().in_flight.is_none());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancelling_before_a_download_rejects_that_request_id_and_retry_needs_a_new_one() {
        let (home, _env, srv) = server();
        let (release, socket) = veles_install::test_support::fixture(home.path(), false, false);
        std::env::set_var("VELES_TEST_COSIGN", home.path().join("cosign"));
        std::env::set_var("VELES_TEST_RELEASE_UNIX_SOCKET", socket);
        let (_, axum::Json(cancelled)) = signed_download_confirm(
            State(srv.clone()),
            axum::Json(signed_cancel("cancel-before-preview")),
        )
        .await;
        assert_eq!(cancelled["cancelled"], true);
        assert_eq!(cancelled["discarded"], false);
        let (code, axum::Json(review)) = signed_download(
            State(srv.clone()),
            axum::Json(signed_request_for(release.clone(), "cancel-before-preview").await),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{review}");
        let fresh = signed_review(srv.clone(), release).await;
        assert_ne!(fresh["plan_id"], "cancel-before-preview");
        let (_, axum::Json(applied)) =
            signed_download_confirm(State(srv), axum::Json(signed_confirmation(&fresh, true)))
                .await;
        assert_eq!(applied["ok"], true, "{applied}");
        assert!(home.path().join("install.toml").is_file());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancelling_a_published_review_rejects_apply_with_a_fresh_confirmation() {
        let (home, _env, srv) = server();
        let (release, socket) = veles_install::test_support::fixture(home.path(), false, false);
        std::env::set_var("VELES_TEST_COSIGN", home.path().join("cosign"));
        std::env::set_var("VELES_TEST_RELEASE_UNIX_SOCKET", socket);
        let review = signed_review(srv.clone(), release).await;
        let (_, axum::Json(cancelled)) = signed_download_confirm(
            State(srv.clone()),
            axum::Json(signed_confirmation(&review, false)),
        )
        .await;
        assert_eq!(cancelled["cancelled"], true);
        assert_eq!(cancelled["discarded"], true);
        let (code, axum::Json(result)) =
            signed_download_confirm(State(srv), axum::Json(signed_confirmation(&review, true)))
                .await;
        assert_eq!(code, StatusCode::CONFLICT, "{result}");
        assert!(!home.path().join("install.toml").exists());
        assert!(!home.path().join("install/current").exists());
        assert_no_signed_staging(home.path());
    }

    #[tokio::test]
    async fn state_does_not_run_plugins_but_options_describes_the_selected_one() {
        let (home, _env, _srv) = server();
        let root = home.path().join("process-plugins/echo");
        std::fs::create_dir_all(&root).unwrap();
        // If state tried --describe, this non-program would fail the
        // request. Discovery must need only the on-disk plugin.toml.
        std::fs::write(root.join("plugin"), "not an executable").unwrap();
        std::fs::write(
            root.join("plugin.toml"),
            "name = \"echo\"\nkind = \"process\"\nentry = \"plugin\"\n",
        )
        .unwrap();
        let (code, axum::Json(state)) = state().await;
        assert_eq!(code, StatusCode::OK, "{state}");
        assert!(state["available"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "echo"));
        fixture(home.path());
        let (code, axum::Json(options)) = options(axum::extract::Path("echo".into())).await;
        assert_eq!(code, StatusCode::OK, "{options}");
        assert!(options["manifest"]["knobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k["path"] == "platforms.echo.rooms.fail_start"));
    }
}

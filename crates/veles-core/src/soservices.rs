//! Host services for ABI 4 native plugins (R8 / M8): the handle table
//! every registration lives in, chat platforms, timers and TCP/TLS
//! sockets.
//!
//! Everything here is OWNED by the plugin that made it — the owner tag
//! `so:<name>` — and purged on unload whether or not the plugin cleaned
//! up after itself (M3's rule: the host owns every resource). Every
//! task spawned here holds the plugin's `Library`, so no plugin code is
//! ever run after its library was unmapped: a purge aborts the tasks,
//! an in-flight callback finishes on its blocking thread with the
//! library still mapped, and only then does the loader's quiescence
//! check see the last reference go.
//!
//! Callbacks reach the plugin on blocking threads (`rt::spawn_blocking`),
//! one at a time per platform and per socket, in order: the task that
//! owns the resource awaits each callback before it takes the next
//! event, which is what makes "events arrive in order" true rather
//! than likely.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use veles_plugin_abi::*;
use veles_types::bus::{InboundMessage, DEFAULT_ROUTE};

/// A plugin's `ctx` pointer, carried across threads unchanged.
#[derive(Clone, Copy)]
pub(crate) struct Ctx(pub *mut c_void);
// SAFETY: the pointer is opaque to the host; the plugin promised it is
// safe to hand back from any thread (ABI docs, "Threading").
unsafe impl Send for Ctx {}
unsafe impl Sync for Ctx {}
impl Ctx {
    /// Read through a method so a closure captures the whole `Ctx`
    /// (which is Send), not its raw-pointer field (which is not).
    fn get(self) -> *mut c_void {
        self.0
    }
}

/// What one handle names.
pub(crate) enum Resource {
    Tool(String),
    Command(String),
    Provider(String),
    /// A service (`register_json` of kind `service`), by name.
    Service(String),
    Hook,
    Filter,
    Platform {
        key: String,
        task: Option<AbortHandle>,
    },
    Timer(Option<AbortHandle>),
    Socket {
        tx: mpsc::UnboundedSender<SockCmd>,
        task: Option<AbortHandle>,
    },
}

pub(crate) struct Entry {
    pub plugin: VelesPluginId,
    pub owner: &'static str,
    pub resource: Resource,
}

pub(crate) enum SockCmd {
    Send(Vec<u8>),
    Close,
}

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

fn table() -> &'static Mutex<HashMap<VelesHandle, Entry>> {
    static T: OnceLock<Mutex<HashMap<VelesHandle, Entry>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<VelesHandle, Entry>> {
    table().lock().unwrap_or_else(|p| p.into_inner())
}

pub(crate) fn new_handle() -> VelesHandle {
    NEXT_HANDLE.fetch_add(1, Ordering::Relaxed)
}

/// Record a resource under a fresh handle.
pub(crate) fn insert(
    plugin: VelesPluginId,
    owner: &'static str,
    resource: Resource,
) -> VelesHandle {
    let h = new_handle();
    insert_with(h, plugin, owner, resource);
    h
}

pub(crate) fn insert_with(
    handle: VelesHandle,
    plugin: VelesPluginId,
    owner: &'static str,
    resource: Resource,
) {
    lock().insert(
        handle,
        Entry {
            plugin,
            owner,
            resource,
        },
    );
}

fn set_task(handle: VelesHandle, abort: AbortHandle) {
    if let Some(e) = lock().get_mut(&handle) {
        match &mut e.resource {
            Resource::Platform { task, .. } | Resource::Socket { task, .. } => *task = Some(abort),
            Resource::Timer(task) => *task = Some(abort),
            _ => {}
        }
    }
}

/// Take `handle` out of the table if `plugin` owns it.
pub(crate) fn take(handle: VelesHandle, plugin: VelesPluginId) -> Option<Entry> {
    let mut t = lock();
    match t.get(&handle) {
        Some(e) if e.plugin == plugin => t.remove(&handle),
        _ => None,
    }
}

/// Drop `handle` from the table without side effects (a task that
/// ended on its own).
fn forget(handle: VelesHandle) {
    lock().remove(&handle);
}

/// The platform keys `plugin` registered.
pub(crate) fn platform_keys(plugin: VelesPluginId) -> Vec<String> {
    lock()
        .values()
        .filter(|e| e.plugin == plugin)
        .filter_map(|e| match &e.resource {
            Resource::Platform { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect()
}

/// How many handles `owner` still holds (status surfaces, tests).
pub fn count_owner(owner: &str) -> usize {
    lock().values().filter(|e| e.owner == owner).count()
}

/// Release a platform, timer or socket: its task is aborted (so no
/// further callback reaches the plugin) and a platform's route is
/// released. Registrations are the caller's to remove from their
/// registries.
pub(crate) fn release(entry: Entry) {
    match entry.resource {
        Resource::Platform { key, task } => {
            if let Some(t) = task {
                t.abort();
            }
            if let Some(bus) = crate::bus::global_bus() {
                bus.unregister_outbound_route(&key, DEFAULT_ROUTE);
            }
        }
        Resource::Timer(task) => {
            if let Some(t) = task {
                t.abort();
            }
        }
        Resource::Socket { tx, task } => {
            drop(tx);
            if let Some(t) = task {
                t.abort();
            }
        }
        Resource::Tool(_) | Resource::Command(_) | Resource::Provider(_) | Resource::Service(_) => {
        }
        Resource::Hook | Resource::Filter => {}
    }
}

/// Everything `owner` holds, released. Returns the count. The owner's
/// registry entries (tools, commands, providers, the plugin's own hook
/// and filter lists) are purged by the loader alongside.
pub fn purge_owner(owner: &str) -> usize {
    let taken: Vec<Entry> = {
        let mut t = lock();
        let keys: Vec<VelesHandle> = t
            .iter()
            .filter(|(_, e)| e.owner == owner)
            .map(|(h, _)| *h)
            .collect();
        keys.into_iter().filter_map(|h| t.remove(&h)).collect()
    };
    let n = taken.len();
    for e in taken {
        release(e);
    }
    n
}

// ── platforms ───────────────────────────────────────────────────────

/// Make `plugin` the platform `key`: its outbound route is claimed on
/// the gateway bus and every outbound message is handed to `deliver`
/// as JSON on a blocking thread, in order.
#[allow(clippy::too_many_arguments)]
pub(crate) fn register_platform(
    plugin: VelesPluginId,
    owner: &'static str,
    lib: Arc<libloading::Library>,
    key: String,
    public_feed: bool,
    deliver: VelesDeliverFn,
    ctx: Ctx,
) -> Result<VelesHandle, String> {
    let Some(bus) = crate::bus::global_bus() else {
        return Err("no gateway bus in this process (platforms live in the gateway)".into());
    };
    if key.is_empty() {
        return Err("a platform needs a key".into());
    }
    if bus.has_adapter(&key) {
        return Err(format!(
            "platform `{key}` is already served in this gateway"
        ));
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<veles_types::bus::OutboundMessage>();
    bus.register_outbound_route(&key, DEFAULT_ROUTE, tx);
    if public_feed {
        crate::lifecycle::with_thread_owner(owner, || crate::bus::declare_public_feed(&key));
    }
    let handle = new_handle();
    insert_with(
        handle,
        plugin,
        owner,
        Resource::Platform {
            key: key.clone(),
            task: None,
        },
    );
    let task = crate::rt::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!("native platform `{key}`: outbound not serialisable: {e}");
                    continue;
                }
            };
            let keep = Arc::clone(&lib);
            let key2 = key.clone();
            let rc = crate::rt::spawn_blocking(move || {
                let _keep = keep;
                let Ok(c) = std::ffi::CString::new(json.replace('\0', "")) else {
                    return VELES_ERR_BAD_ARGUMENT;
                };
                // SAFETY: the plugin's deliver fn with its own ctx; the
                // string lives for the call; the library is held.
                unsafe { deliver(c.as_ptr(), ctx.get()) }
            })
            .await
            .unwrap_or(VELES_ERR_REFUSED);
            if rc != VELES_OK {
                tracing::warn!("native platform `{key2}`: delivery answered {rc}");
            }
        }
    });
    set_task(handle, task.abort_handle());
    Ok(handle)
}

/// Publish an inbound message for one of `plugin`'s platforms.
pub(crate) fn platform_inbound(plugin: VelesPluginId, json: &str) -> i32 {
    let msg: InboundMessage = match serde_json::from_str(json) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("native plugin: platform_inbound is not an InboundMessage: {e}");
            return VELES_ERR_BAD_ARGUMENT;
        }
    };
    if !platform_keys(plugin).contains(&msg.platform) {
        tracing::warn!(
            "native plugin: platform_inbound for `{}`, which it did not register",
            msg.platform
        );
        return VELES_ERR_REFUSED;
    }
    let Some(bus) = crate::bus::global_bus() else {
        return VELES_ERR_REFUSED;
    };
    bus.send_inbound(msg);
    VELES_OK
}

// ── timers ──────────────────────────────────────────────────────────

/// Call `cb(ctx)` once, `ms` from now, on a blocking thread — unless
/// the timer is cancelled or purged first.
pub(crate) fn timer_after(
    plugin: VelesPluginId,
    owner: &'static str,
    lib: Arc<libloading::Library>,
    ms: u64,
    cb: VelesTimerFn,
    ctx: Ctx,
) -> VelesHandle {
    let handle = new_handle();
    // In the table BEFORE the task runs: a 0 ms timer must find itself.
    insert_with(handle, plugin, owner, Resource::Timer(None));
    let task = crate::rt::spawn(timer_task(
        handle,
        plugin,
        Arc::downgrade(&lib),
        ms,
        cb,
        ctx,
    ));
    set_task(handle, task.abort_handle());
    handle
}

/// A timer's task: sleep, then fire ONLY if the row is still in the
/// table — a cancel or a purge in the meantime took it, and a cancel
/// that lands after the sleep ended but before this check still wins.
/// A sleeping timer holds the library weakly, so it pins nothing an
/// unload waits for; the library is held for the call alone.
async fn timer_task(
    handle: VelesHandle,
    plugin: VelesPluginId,
    lib: Weak<libloading::Library>,
    ms: u64,
    cb: VelesTimerFn,
    ctx: Ctx,
) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
    // A cancel in the meantime took the row; then this never fires.
    if take(handle, plugin).is_none() {
        return;
    }
    let Some(keep) = lib.upgrade() else {
        return;
    };
    let _ = crate::rt::spawn_blocking(move || {
        let _keep = keep;
        // SAFETY: the plugin's callback with its own ctx; the
        // library is held for the call.
        unsafe { cb(ctx.get()) }
    })
    .await;
}

pub(crate) fn timer_cancel(plugin: VelesPluginId, handle: VelesHandle) -> i32 {
    match take(handle, plugin) {
        Some(e) if matches!(e.resource, Resource::Timer(_)) => {
            release(e);
            VELES_OK
        }
        Some(e) => {
            // Not a timer: put it back untouched.
            insert_with(handle, e.plugin, e.owner, e.resource);
            VELES_ERR_NO_SUCH_HANDLE
        }
        None => VELES_ERR_NO_SUCH_HANDLE,
    }
}

// ── sockets ─────────────────────────────────────────────────────────

fn tls_connector() -> tokio_rustls::TlsConnector {
    static CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let cfg = CFG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let builder = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap_or_else(|e| {
                    // ring supports every default version; this cannot
                    // fail, and a panic here would be a host bug, not
                    // a plugin's.
                    panic!("rustls default protocol versions: {e}")
                });
            Arc::new(builder.with_root_certificates(roots).with_no_client_auth())
        })
        .clone();
    tokio_rustls::TlsConnector::from(cfg)
}

/// A plain or TLS stream, boxed: one trait so the object type can name
/// both halves.
trait Rw: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Rw for T {}
type Stream = Box<dyn Rw>;

/// Open a TCP (or TLS) connection on the host's runtime; events reach
/// `on_event` on blocking threads, one at a time, in order.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tcp_connect(
    plugin: VelesPluginId,
    owner: &'static str,
    lib: Arc<libloading::Library>,
    host: String,
    port: u16,
    tls: bool,
    on_event: VelesSocketEventFn,
    ctx: Ctx,
) -> VelesHandle {
    let handle = new_handle();
    let (tx, rx) = mpsc::unbounded_channel::<SockCmd>();
    insert_with(handle, plugin, owner, Resource::Socket { tx, task: None });
    let task = crate::rt::spawn(socket_task(handle, lib, host, port, tls, on_event, ctx, rx));
    set_task(handle, task.abort_handle());
    handle
}

/// Deliver one socket event on a blocking thread and wait for it, so
/// the next event cannot overtake it.
async fn emit(
    lib: &Arc<libloading::Library>,
    on_event: VelesSocketEventFn,
    ctx: Ctx,
    kind: i32,
    data: Vec<u8>,
) {
    let keep = Arc::clone(lib);
    let _ = crate::rt::spawn_blocking(move || {
        let _keep = keep;
        let (ptr, len) = if data.is_empty() {
            (std::ptr::null(), 0)
        } else {
            (data.as_ptr(), data.len())
        };
        // SAFETY: the plugin's callback with its own ctx; the buffer
        // lives for the call; the library is held.
        unsafe { on_event(kind, ptr, len, ctx.get()) }
    })
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn socket_task(
    handle: VelesHandle,
    lib: Arc<libloading::Library>,
    host: String,
    port: u16,
    tls: bool,
    on_event: VelesSocketEventFn,
    ctx: Ctx,
    mut rx: mpsc::UnboundedReceiver<SockCmd>,
) {
    let tcp = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
        Ok(s) => s,
        Err(e) => {
            emit(
                &lib,
                on_event,
                ctx,
                SOCK_ERROR,
                format!("connect {host}:{port}: {e}").into_bytes(),
            )
            .await;
            forget(handle);
            return;
        }
    };
    let stream: Stream = if tls {
        let name = match rustls::pki_types::ServerName::try_from(host.clone()) {
            Ok(n) => n,
            Err(e) => {
                emit(
                    &lib,
                    on_event,
                    ctx,
                    SOCK_ERROR,
                    format!("tls server name `{host}`: {e}").into_bytes(),
                )
                .await;
                forget(handle);
                return;
            }
        };
        match tls_connector().connect(name, tcp).await {
            Ok(t) => Box::new(t),
            Err(e) => {
                emit(
                    &lib,
                    on_event,
                    ctx,
                    SOCK_ERROR,
                    format!("tls handshake with {host}:{port}: {e}").into_bytes(),
                )
                .await;
                forget(handle);
                return;
            }
        }
    } else {
        Box::new(tcp)
    };
    emit(&lib, on_event, ctx, SOCK_CONNECTED, Vec::new()).await;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        tokio::select! {
            read = reader.read(&mut buf) => match read {
                Ok(0) => {
                    emit(&lib, on_event, ctx, SOCK_CLOSED, Vec::new()).await;
                    break;
                }
                Ok(n) => {
                    emit(&lib, on_event, ctx, SOCK_DATA, buf[..n].to_vec()).await;
                }
                Err(e) => {
                    emit(&lib, on_event, ctx, SOCK_ERROR, format!("read: {e}").into_bytes()).await;
                    break;
                }
            },
            cmd = rx.recv() => match cmd {
                Some(SockCmd::Send(data)) => {
                    if let Err(e) = writer.write_all(&data).await {
                        emit(&lib, on_event, ctx, SOCK_ERROR, format!("write: {e}").into_bytes()).await;
                        break;
                    }
                }
                Some(SockCmd::Close) | None => {
                    let _ = writer.shutdown().await;
                    emit(&lib, on_event, ctx, SOCK_CLOSED, Vec::new()).await;
                    break;
                }
            },
        }
    }
    forget(handle);
}

pub(crate) fn tcp_send(plugin: VelesPluginId, handle: VelesHandle, data: Vec<u8>) -> i32 {
    let t = lock();
    match t.get(&handle) {
        Some(e) if e.plugin == plugin => match &e.resource {
            Resource::Socket { tx, .. } => {
                if tx.send(SockCmd::Send(data)).is_ok() {
                    VELES_OK
                } else {
                    VELES_ERR_REFUSED
                }
            }
            _ => VELES_ERR_NO_SUCH_HANDLE,
        },
        _ => VELES_ERR_NO_SUCH_HANDLE,
    }
}

/// A polite close: the task shuts the stream and delivers
/// `SOCK_CLOSED`, then forgets the handle itself.
pub(crate) fn tcp_close(plugin: VelesPluginId, handle: VelesHandle) -> i32 {
    let t = lock();
    match t.get(&handle) {
        Some(e) if e.plugin == plugin => match &e.resource {
            Resource::Socket { tx, .. } => {
                let _ = tx.send(SockCmd::Close);
                VELES_OK
            }
            _ => VELES_ERR_NO_SUCH_HANDLE,
        },
        _ => VELES_ERR_NO_SUCH_HANDLE,
    }
}

// unix: the one test drives the timer through a C callback the plugin
// tier's `.so` fixtures provide.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    unsafe extern "C" fn mark(ctx: *mut c_void) {
        // SAFETY: the test passes a leaked AtomicBool.
        let flag = unsafe { &*(ctx as *const AtomicBool) };
        flag.store(true, Ordering::SeqCst);
    }

    /// The race the fire-time check exists for, made deterministic: a
    /// timer cancelled after its sleep ended but before it fired must
    /// not fire. (`timer_cancel` also aborts the task, so the plugin
    /// test cannot reach this window; driving the task by hand can.)
    #[tokio::test]
    async fn a_cancelled_timer_that_is_already_due_does_not_fire() {
        let lib = Arc::new(libloading::Library::from(
            libloading::os::unix::Library::this(),
        ));
        let plugin: VelesPluginId = 7_777;
        let fired: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let ctx = Ctx(fired as *const AtomicBool as *mut c_void);

        let handle = new_handle();
        insert_with(handle, plugin, "so:timer-test", Resource::Timer(None));
        assert_eq!(timer_cancel(plugin, handle), VELES_OK);
        timer_task(handle, plugin, Arc::downgrade(&lib), 0, mark, ctx).await;
        assert!(!fired.load(Ordering::SeqCst), "a cancelled timer fired");

        // Not cancelled: the same path fires, and the row is consumed.
        let handle = new_handle();
        insert_with(handle, plugin, "so:timer-test", Resource::Timer(None));
        timer_task(handle, plugin, Arc::downgrade(&lib), 0, mark, ctx).await;
        assert!(fired.load(Ordering::SeqCst), "a live timer did not fire");
        assert_eq!(timer_cancel(plugin, handle), VELES_ERR_NO_SUCH_HANDLE);
    }
}

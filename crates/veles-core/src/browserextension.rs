//! Optional native-messaging bridge to an explicitly connected Chrome,
//! Chromium, Edge or Firefox tab, on Linux, Windows and macOS (see
//! [`install_extension`]).
//!
//! `veles browser-extension install` writes a small launcher and points
//! each browser's native-host manifest at it: a file in the browser's own
//! directory on Linux and macOS, a file in the bridge directory named by a
//! per-user registry key on Windows (BX-9). A browser starts a native host
//! with its own argv (Chrome: the caller's origin, plus `--parent-window=`
//! on Windows; Firefox: the manifest path and the extension id) and its own
//! environment, so the launcher fixes both: it runs `veles
//! browser-extension host` from a path that survives upgrades, with the
//! bridge directory baked in. The binary also recognises a browser launch
//! of itself ([`is_browser_launch`]), for manifests written before the
//! launcher existed. `veles browser-extension uninstall` removes what the
//! install wrote.
//!
//! Native messaging carries authentication and tab actions on stdio. A
//! loopback TCP socket, bound only while that host is alive and answering
//! only requests that carry the owner-only token, lets the agent's tool
//! reach the SAME host across calls. It is the same transport on every
//! platform. There is no remote debugging endpoint and no arbitrary tool
//! or shell input.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

pub const NATIVE_NAME: &str = "veles_browser";
/// The unpacked extension's Chrome origin, fixed by the manifest's `key`.
pub const CHROME_ORIGIN: &str = "chrome-extension://ndalephnnclghcacbljmhngnpcagpgdh/";
/// The extension's Firefox id (`browser_specific_settings.gecko.id`).
pub const FIREFOX_EXTENSION_ID: &str = "veles@7778777.online";
/// The launcher sets this to the directory holding the token and the
/// endpoint, so a browser-started host agrees with the gateway that was
/// configured at install time whatever environment the browser has.
pub const BRIDGE_DIR_ENV: &str = "VELES_BROWSER_BRIDGE_DIR";
/// Agent requests, and every frame the host writes to the browser.
const MAX_FRAME: usize = 64 * 1024;
/// Frames FROM the browser: a snapshot result. background.js keeps one
/// near 48 KB of JSON; this is the hard bound (BX-4).
const MAX_REPLY: usize = 1024 * 1024;
/// A reply to the agent is the browser's frame re-serialised.
const MAX_AGENT_REPLY: usize = 2 * MAX_REPLY;
/// How long one tab action may take before its caller is told so.
const ACTION_TIMEOUT: Duration = Duration::from_secs(30);

fn bridge_dir() -> PathBuf {
    std::env::var_os(BRIDGE_DIR_ENV)
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(crate::config::data_dir)
}

fn endpoint_path() -> PathBuf {
    bridge_dir().join("browser-extension-endpoint")
}

pub fn token_path() -> PathBuf {
    bridge_dir().join("browser-extension-token")
}

/// Owner-only from the first byte: 0600 on unix, an owner-only DACL on
/// Windows (the platform layer's `write_private`).
fn private_write(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::platform::write_private(path, text.as_bytes())
        .with_context(|| format!("write {}", path.display()))
}

pub fn create_token() -> Result<String> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    private_write(&token_path(), &format!("{token}\n"))?;
    Ok(token)
}

pub fn token() -> Option<String> {
    std::fs::read_to_string(token_path())
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

/// Is this argv a browser starting the native host? Chrome passes the
/// caller's origin (plus `--parent-window=` on Windows); Firefox passes
/// the manifest's path and the extension id. No veles subcommand looks
/// like either.
pub fn is_browser_launch(args: &[std::ffi::OsString]) -> bool {
    let arg = |i: usize| args.get(i).and_then(|a| a.to_str());
    arg(1).is_some_and(|a| a.starts_with("chrome-extension://"))
        || arg(2) == Some(FIREFOX_EXTENSION_ID)
}

fn read_frame(input: &mut impl Read) -> Result<Option<Value>> {
    read_frame_up_to(input, MAX_FRAME)
}

fn read_frame_up_to(input: &mut impl Read, limit: usize) -> Result<Option<Value>> {
    let mut length = [0u8; 4];
    let mut read = 0;
    while read < length.len() {
        match input.read(&mut length[read..]) {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => bail!("truncated native message length"),
            Ok(n) => read += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > limit {
        bail!("native message of {length} bytes exceeds {limit} bytes");
    }
    let mut body = vec![0u8; length];
    input.read_exact(&mut body)?;
    Ok(Some(
        serde_json::from_slice(&body).context("native message is not JSON")?,
    ))
}

fn write_frame(output: &mut impl Write, value: &Value) -> Result<()> {
    write_frame_up_to(output, value, MAX_FRAME)
}

fn write_frame_up_to(output: &mut impl Write, value: &Value, limit: usize) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    if body.len() > limit {
        bail!("native reply exceeds {limit} bytes");
    }
    output.write_all(&(body.len() as u32).to_le_bytes())?;
    output.write_all(&body)?;
    output.flush()?;
    Ok(())
}

fn authenticated(request: &Value, expected: &str) -> bool {
    request["token"]
        .as_str()
        .is_some_and(|token| token.len() == expected.len() && constant_time_eq(token, expected))
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let mut difference = 0u8;
    for (a, b) in left.bytes().zip(right.bytes()) {
        difference |= a ^ b;
    }
    difference == 0
}

fn valid_request(request: &Value) -> bool {
    let action = request["action"].as_str().unwrap_or_default();
    match action {
        "snapshot" => true,
        "navigate" => request["url"]
            .as_str()
            .is_some_and(|url| url.starts_with("https://") || url.starts_with("http://")),
        "click" => request["ref"]
            .as_str()
            .is_some_and(|r| !r.is_empty() && r.len() <= 16),
        "fill" => {
            request["ref"]
                .as_str()
                .is_some_and(|r| !r.is_empty() && r.len() <= 16)
                && request["text"].as_str().is_some_and(|t| t.len() <= 10_000)
        }
        _ => false,
    }
}

struct EndpointGuard {
    value: String,
}

impl Drop for EndpointGuard {
    fn drop(&mut self) {
        let owned = std::fs::read_to_string(endpoint_path())
            .ok()
            .is_some_and(|value| value == self.value);
        if owned {
            let _ = std::fs::remove_file(endpoint_path());
        }
    }
}

/// What the browser's stdin reader passes to the host loop.
enum FromBrowser {
    Message(Value),
    /// The stream is unusable (an oversized or malformed frame): the
    /// host reports it to a waiting caller and stops.
    Broken(String),
}

#[derive(PartialEq)]
enum Flow {
    Continue,
    Stop,
}

/// A browser-launched process: one authenticated tab at a time.
pub fn run_host_with_channels(
    mut browser_in: impl Read + Send + 'static,
    mut browser_out: impl Write + Send + 'static,
) -> Result<()> {
    let Some(hello) = read_frame(&mut browser_in)? else {
        return Ok(());
    };
    let Some(expected) = token() else {
        write_frame(
            &mut browser_out,
            &json!({"type":"error","id":hello["id"],"error":"the bridge is not installed; run `veles browser-extension install`"}),
        )?;
        return Ok(());
    };
    if hello["type"] != "hello" || !authenticated(&hello, &expected) {
        write_frame(
            &mut browser_out,
            &json!({"type":"error","id":hello["id"],"error":"authentication failed"}),
        )?;
        return Ok(());
    }
    let Some(connect) = read_frame(&mut browser_in)? else {
        return Ok(());
    };
    let id = connect["id"].as_str().unwrap_or_default().to_owned();
    if connect["type"] != "connect" || id.is_empty() || id.len() > 128 {
        bail!("invalid tab connection id");
    }
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => listener,
        Err(e) => {
            let _ = std::fs::remove_file(endpoint_path());
            return Err(e.into());
        }
    };
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    private_write(&endpoint_path(), &format!("{port} {id}\n"))?;
    let _endpoint_guard = EndpointGuard {
        value: format!("{port} {id}\n"),
    };
    write_frame(&mut browser_out, &json!({"type":"hello-ok","id":id}))?;
    let (browser_tx, browser_rx) = std::sync::mpsc::channel();
    let _browser_thread = std::thread::spawn(move || loop {
        match read_frame_up_to(&mut browser_in, MAX_REPLY) {
            Ok(Some(message)) => {
                if browser_tx.send(FromBrowser::Message(message)).is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = browser_tx.send(FromBrowser::Broken(format!("{e:#}")));
                break;
            }
        }
    });
    let mut seq: u64 = 0;
    let result = loop {
        match listener.accept() {
            Ok((agent, address)) => {
                if !address.ip().is_loopback() {
                    continue;
                }
                seq += 1;
                match serve_agent(agent, &expected, &id, seq, &mut browser_out, &browser_rx) {
                    Ok(Flow::Continue) => {}
                    Ok(Flow::Stop) => break Ok(()),
                    Err(e) => break Err(e),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => match browser_rx.try_recv() {
                Ok(FromBrowser::Message(message)) if message["type"] == "close" => break Ok(()),
                // A result whose caller already gave up, or any stray
                // message: dropped. It is never handed to the next caller
                // and it does not end the bridge (BX-11).
                Ok(FromBrowser::Message(_)) => {}
                Ok(FromBrowser::Broken(why)) => {
                    tracing::warn!("browser bridge: the browser stream broke: {why}");
                    break Ok(());
                }
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(10)),
                Err(TryRecvError::Disconnected) => break Ok(()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => break Err(e.into()),
        }
    };
    drop(browser_rx);
    // The browser may keep its native port open after sending `close`.
    // The reader thread owns only stdin and exits when the browser exits;
    // do not join it after an explicit close or we would wait forever.
    result
}

/// One loopback caller. Anything wrong with the CALLER is answered when
/// it can be and then forgotten: the port is open to every local user,
/// and no connection may end the owner's bridge (BX-10). Only a failure
/// toward the browser is fatal.
fn serve_agent(
    mut agent: TcpStream,
    expected: &str,
    id: &str,
    seq: u64,
    browser_out: &mut impl Write,
    browser_rx: &Receiver<FromBrowser>,
) -> Result<Flow> {
    let answer = |agent: &mut TcpStream, value: &Value| {
        if let Err(e) = write_frame_up_to(agent, value, MAX_AGENT_REPLY) {
            tracing::debug!("browser bridge: could not answer a caller: {e:#}");
        }
    };
    // An accepted socket inherits the listener's non-blocking mode on
    // Windows and the BSDs; a caller that connects and says nothing must
    // not hold the host for long.
    let request = agent
        .set_nonblocking(false)
        .and_then(|()| agent.set_read_timeout(Some(Duration::from_secs(5))))
        .map_err(anyhow::Error::from)
        .and_then(|()| read_frame(&mut agent));
    let request = match request {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(Flow::Continue),
        Err(e) => {
            answer(
                &mut agent,
                &json!({"ok":false,"error":format!("bad request: {e:#}")}),
            );
            return Ok(Flow::Continue);
        }
    };
    if !authenticated(&request, expected) || !valid_request(&request) {
        answer(
            &mut agent,
            &json!({"ok":false,"error":"unauthorized or invalid browser action"}),
        );
        return Ok(Flow::Continue);
    }
    let mut action = request;
    if let Some(fields) = action.as_object_mut() {
        fields.remove("token");
    }
    action["type"] = json!("action");
    action["id"] = json!(id);
    action["seq"] = json!(seq);
    write_frame(browser_out, &action)?;
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let reply = browser_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        let (value, flow) = match reply {
            Ok(FromBrowser::Message(reply))
                if reply["type"] == "result" && reply["id"] == id && reply["seq"] == seq =>
            {
                (reply, Flow::Continue)
            }
            Ok(FromBrowser::Message(message)) if message["type"] == "close" => (
                json!({"ok":false,"error":"the tab was disconnected"}),
                Flow::Stop,
            ),
            // An earlier caller's late result: not this caller's.
            Ok(FromBrowser::Message(_)) => continue,
            Ok(FromBrowser::Broken(why)) => (
                json!({"ok":false,"error":format!("the browser bridge failed ({why}); reconnect the tab")}),
                Flow::Stop,
            ),
            Err(RecvTimeoutError::Timeout) => (
                json!({"ok":false,"error":"browser action timed out"}),
                Flow::Continue,
            ),
            Err(RecvTimeoutError::Disconnected) => (
                json!({"ok":false,"error":"the browser closed the bridge; reconnect the tab"}),
                Flow::Stop,
            ),
        };
        answer(&mut agent, &value);
        return Ok(flow);
    }
}

// ── install and uninstall ────────────────────────────────────────────

/// The platforms whose browsers [`install_extension`] registers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostOs {
    Linux,
    MacOs,
    Windows,
}

impl HostOs {
    fn current() -> Option<Self> {
        if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else if cfg!(target_os = "macos") {
            Some(Self::MacOs)
        } else if cfg!(windows) {
            Some(Self::Windows)
        } else {
            None
        }
    }
}

/// Which form of manifest a browser reads: a Chromium-based one names the
/// extension's origin, Firefox its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    Chromium,
    Firefox,
}

/// One browser's native-host registration.
#[derive(Clone, Debug, PartialEq)]
struct Registration {
    browser: &'static str,
    flavor: Flavor,
    /// The manifest file.
    manifest: PathBuf,
    /// Windows: the key under HKCU whose default value is `manifest`.
    key: Option<String>,
}

/// Everything an install writes for one platform and native-host name.
#[derive(Debug)]
struct Layout {
    /// The launcher, the extension copy and, on Windows, the manifests.
    dir: PathBuf,
    launcher: PathBuf,
    extension: PathBuf,
    registrations: Vec<Registration>,
}

/// Where each browser looks for a native host named `name`, and where the
/// install keeps its own files (BX-9).
///
///   * Linux: Chrome `~/.config/google-chrome/NativeMessagingHosts`,
///     Chromium `~/.config/chromium/NativeMessagingHosts`, Firefox
///     `~/.mozilla/native-messaging-hosts`.
///   * macOS: `~/Library/Application Support/` + `Google/Chrome/`,
///     `Chromium/` and `Mozilla/` + `NativeMessagingHosts`.
///   * Windows: no directory is searched. Chrome, Chromium, Edge and
///     Firefox read the default value of
///     `HKCU\Software\{Google\Chrome,Chromium,Microsoft\Edge,Mozilla}\NativeMessagingHosts\<name>`,
///     the path of a manifest that may live anywhere: here, beside the
///     launcher.
fn layout(os: HostOs, home: &Path, bridge: &Path, name: &str) -> Layout {
    // The shipped name keeps the 0.2.0 layout; a test name (`--native-name`)
    // gets its own directory, so removing it cannot take the real one's
    // launcher or extension copy with it.
    let dir = if name == NATIVE_NAME {
        bridge.join("browser-extension")
    } else {
        bridge.join(format!("browser-extension-{name}"))
    };
    let file = format!("{name}.json");
    let entry = |browser, flavor, manifest: PathBuf, key: Option<String>| Registration {
        browser,
        flavor,
        manifest,
        key,
    };
    let registrations = match os {
        HostOs::Linux => vec![
            entry(
                "Chrome",
                Flavor::Chromium,
                home.join(".config/google-chrome/NativeMessagingHosts")
                    .join(&file),
                None,
            ),
            entry(
                "Chromium",
                Flavor::Chromium,
                home.join(".config/chromium/NativeMessagingHosts")
                    .join(&file),
                None,
            ),
            entry(
                "Firefox",
                Flavor::Firefox,
                home.join(".mozilla/native-messaging-hosts").join(&file),
                None,
            ),
        ],
        HostOs::MacOs => {
            let support = home.join("Library").join("Application Support");
            vec![
                entry(
                    "Chrome",
                    Flavor::Chromium,
                    support
                        .join("Google/Chrome/NativeMessagingHosts")
                        .join(&file),
                    None,
                ),
                entry(
                    "Chromium",
                    Flavor::Chromium,
                    support.join("Chromium/NativeMessagingHosts").join(&file),
                    None,
                ),
                entry(
                    "Firefox",
                    Flavor::Firefox,
                    support.join("Mozilla/NativeMessagingHosts").join(&file),
                    None,
                ),
            ]
        }
        HostOs::Windows => {
            let chromium = dir.join(format!("{name}.chromium.json"));
            let firefox = dir.join(format!("{name}.firefox.json"));
            let key =
                |vendor: &str| Some(format!(r"Software\{vendor}\NativeMessagingHosts\{name}"));
            vec![
                entry(
                    "Chrome",
                    Flavor::Chromium,
                    chromium.clone(),
                    key(r"Google\Chrome"),
                ),
                entry(
                    "Chromium",
                    Flavor::Chromium,
                    chromium.clone(),
                    key("Chromium"),
                ),
                entry("Edge", Flavor::Chromium, chromium, key(r"Microsoft\Edge")),
                entry("Firefox", Flavor::Firefox, firefox, key("Mozilla")),
            ]
        }
    };
    let launcher = dir.join(match os {
        HostOs::Windows => "native-host.cmd",
        _ => "native-host",
    });
    Layout {
        extension: dir.join("extension"),
        launcher,
        dir,
        registrations,
    }
}

/// A native-host name as Chrome and Firefox accept one: lowercase ASCII
/// letters, digits, `_`, and dots that neither lead, trail nor repeat. The
/// check also keeps any other name out of file names and registry paths.
fn check_native_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.contains("..");
    if !ok {
        bail!(
            "{name:?} is not a native messaging host name (lowercase letters, digits, `_` and \
             single inner dots)"
        );
    }
    Ok(())
}

/// The per-user registry, as far as the Windows install uses it: the
/// default value of keys under HKCU. A trait so the install's Windows
/// logic is tested on every platform against a fake; the real one is
/// [`UserRegistry`].
trait Registry {
    fn default_value(&self, key: &str) -> Result<Option<String>>;
    fn set_default(&mut self, key: &str, value: &str) -> Result<()>;
    /// `true` when the key was there.
    fn delete(&mut self, key: &str) -> Result<bool>;
    /// Delete the key if it has no subkey and no value; `true` when it did.
    fn delete_if_empty(&mut self, key: &str) -> Result<bool>;
}

/// No registration on Linux or macOS lives in a registry.
#[cfg(any(not(windows), test))]
struct NoRegistry;

#[cfg(any(not(windows), test))]
impl Registry for NoRegistry {
    fn default_value(&self, _key: &str) -> Result<Option<String>> {
        Ok(None)
    }
    fn set_default(&mut self, key: &str, _value: &str) -> Result<()> {
        bail!("there is no registry on this platform for {key}")
    }
    fn delete(&mut self, _key: &str) -> Result<bool> {
        Ok(false)
    }
    fn delete_if_empty(&mut self, _key: &str) -> Result<bool> {
        Ok(false)
    }
}

/// HKCU, through the platform layer.
#[cfg(windows)]
struct UserRegistry;

#[cfg(windows)]
impl Registry for UserRegistry {
    fn default_value(&self, key: &str) -> Result<Option<String>> {
        crate::platform::user_registry_default(key).with_context(|| format!(r"read HKCU\{key}"))
    }
    fn set_default(&mut self, key: &str, value: &str) -> Result<()> {
        crate::platform::user_registry_set_default(key, value)
            .with_context(|| format!(r"write HKCU\{key}"))
    }
    fn delete(&mut self, key: &str) -> Result<bool> {
        crate::platform::user_registry_delete(key).with_context(|| format!(r"delete HKCU\{key}"))
    }
    fn delete_if_empty(&mut self, key: &str) -> Result<bool> {
        crate::platform::user_registry_delete_if_empty(key)
            .with_context(|| format!(r"delete HKCU\{key}"))
    }
}

fn os_registry() -> Box<dyn Registry> {
    #[cfg(windows)]
    {
        Box::new(UserRegistry)
    }
    #[cfg(not(windows))]
    {
        Box::new(NoRegistry)
    }
}

fn supported_os() -> Result<HostOs> {
    HostOs::current().context(
        "the browser tab bridge installs on Linux, Windows and macOS only: this platform's \
         browsers have no native messaging host locations veles knows",
    )
}

/// The home the Linux and macOS manifests go under: `HOME`, as the
/// browsers read it. On Windows the registry is per user already.
fn manifest_home() -> Result<PathBuf> {
    if cfg!(windows) {
        return Ok(crate::config::user_home());
    }
    match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        Some(home) => Ok(PathBuf::from(home)),
        None => bail!("HOME is not set"),
    }
}

/// The bridge directory as browsers will be told it: absolute, whatever
/// `VELES_HOME` said.
fn absolute_bridge_dir() -> Result<PathBuf> {
    let dir = bridge_dir();
    Ok(crate::platform::plain_path(
        std::path::absolute(&dir).with_context(|| format!("make {} absolute", dir.display()))?,
    ))
}

/// Write the token, the launcher, a user-owned copy of the unpacked
/// extension, and the Chrome, Chromium, Edge (Windows) and Firefox
/// native-host registrations for `name` ([`NATIVE_NAME`] unless a test
/// says otherwise). Linux, Windows and macOS; see [`layout`] for where.
pub fn install_extension(overwrite: bool, name: &str) -> Result<String> {
    let os = supported_os()?;
    check_native_name(name)?;
    let home = manifest_home()?;
    let bridge = absolute_bridge_dir()?;
    let entry = host_entry_point()?;
    install_into(
        os,
        &home,
        &bridge,
        &entry,
        name,
        overwrite,
        os_registry().as_mut(),
    )
}

/// Remove every registration [`install_extension`] wrote for `name`, the
/// launcher and the extension copy, and (for the shipped name) the token.
pub fn uninstall_extension(name: &str) -> Result<String> {
    let os = supported_os()?;
    check_native_name(name)?;
    let home = manifest_home()?;
    let bridge = absolute_bridge_dir()?;
    uninstall_from(os, &home, &bridge, name, os_registry().as_mut())
}

/// The veles to launch, from a path that outlives this process.
#[cfg(unix)]
fn host_entry_point() -> Result<PathBuf> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let home = crate::config::user_home();
    let mut profiles = vec![home.join(".nix-profile/bin/veles")];
    if let Some(user) = env("USER") {
        profiles.push(
            Path::new("/etc/profiles/per-user")
                .join(user)
                .join("bin/veles"),
        );
    }
    profiles.push(
        env("XDG_STATE_HOME")
            .unwrap_or_else(|| home.join(".local/state"))
            .join("nix/profile/bin/veles"),
    );
    profiles.push(PathBuf::from("/run/current-system/sw/bin/veles"));
    Ok(stable_entry_point(
        &std::env::current_exe()?,
        env("APPIMAGE"),
        env("VELES_BUNDLE_ROOT"),
        Path::new("/nix/store"),
        &profiles,
    ))
}

/// The veles to launch on Windows: the running `veles.exe` as Windows
/// names it (never `canonicalize`'s `\\?\` form, which cmd.exe cannot
/// run), or the installer's `current\bin\veles.exe` for a release
/// install (scripts/install/install.ps1).
#[cfg(windows)]
fn host_entry_point() -> Result<PathBuf> {
    let exe = crate::platform::plain_path(std::env::current_exe()?);
    Ok(release_current(&exe).unwrap_or(exe))
}

/// An installer release lives at `<root>/releases/<id>/bin/veles` and is
/// run through `<root>/current`, which the next upgrade switches
/// (scripts/install/install.sh: a symlink; install.ps1: a junction). Name
/// `current`, not this release's own directory, which an upgrade leaves
/// behind and a later one prunes.
fn release_current(exe: &Path) -> Option<PathBuf> {
    let bin = exe.parent()?;
    let release = bin.parent()?;
    let releases = release.parent()?;
    if releases.file_name()? != "releases" {
        return None;
    }
    let current = releases
        .parent()?
        .join("current")
        .join(bin.file_name()?)
        .join(exe.file_name()?);
    current.is_file().then_some(current)
}

/// [`host_entry_point`]'s choice, pure (BX-8). In order:
///   1. the AppImage file itself: its mount point changes every run;
///   2. a bundle's `bin/veles` launcher: the binary inside runs in a
///      namespace whose `/nix` the browser cannot see;
///   3. for a binary in the Nix store, a profile's `veles`: the store path
///      (`.veles-wrapped`, behind the wrapper) is gone after an upgrade
///      and a GC; failing that the wrapper beside it, still pinned;
///   4. the running binary.
///
/// A bundle launcher or a binary inside an installer release is then
/// named through the release's `current` ([`release_current`]).
#[cfg(unix)]
fn stable_entry_point(
    exe: &Path,
    appimage: Option<PathBuf>,
    bundle_root: Option<PathBuf>,
    store: &Path,
    profiles: &[PathBuf],
) -> PathBuf {
    if let Some(image) = appimage.filter(|p| p.is_file()) {
        return image;
    }
    if let Some(launcher) = bundle_root
        .map(|root| root.join("bin/veles"))
        .filter(|p| p.is_file())
    {
        return release_current(&launcher).unwrap_or(launcher);
    }
    let canonical = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    if canonical.starts_with(store) {
        if let Some(profile) = profiles.iter().find(|p| p.is_file()) {
            return profile.clone();
        }
        if canonical.file_name().is_some_and(|n| n == ".veles-wrapped") {
            let wrapper = canonical.with_file_name("veles");
            if wrapper.is_file() {
                return wrapper;
            }
        }
    }
    release_current(&canonical).unwrap_or(canonical)
}

fn sh_quote(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))?;
    Ok(format!("'{}'", text.replace('\'', "'\\''")))
}

/// The launcher a manifest names. Browsers pass arguments a veles
/// subcommand would not take and their own environment; this fixes both
/// (BX-1, BX-20).
fn launcher_script(os: HostOs, entry: &Path, bridge: &Path) -> Result<String> {
    match os {
        HostOs::Windows => cmd_launcher(entry, bridge),
        HostOs::Linux | HostOs::MacOs => sh_launcher(entry, bridge),
    }
}

fn sh_launcher(entry: &Path, bridge: &Path) -> Result<String> {
    Ok(format!(
        "#!/bin/sh\n\
         # Written by `veles browser-extension install`. Chrome and Firefox start\n\
         # this with their own arguments and environment; it runs the veles\n\
         # native host for the tab bridge. Rerun the install to rewrite it.\n\
         {BRIDGE_DIR_ENV}={}\n\
         export {BRIDGE_DIR_ENV}\n\
         exec {} browser-extension host \"$@\"\n",
        sh_quote(bridge)?,
        sh_quote(entry)?
    ))
}

/// Windows: the `.cmd` a registration's manifest names (BX-9). Chrome runs
/// it as `cmd.exe /d /c "<launcher>" <origin> --parent-window=<n>` with
/// stdio redirected to two named pipes; Firefox as `cmd.exe /s/c
/// ""<launcher>" <manifest> <extension id>"` (subprocess_win.worker.js).
/// `%*` passes those arguments on and `browser-extension host` takes and
/// ignores them.
///
/// stdout is the native messaging stream, so nothing may reach it before
/// veles does: `@echo off` comes first. Inside a quoted batch string `%` is
/// the one character cmd does not keep (it is doubled here), and `!` would
/// be eaten by delayed expansion (turned off here, whatever the registry
/// says). cmd reads a batch file in the console code page, so a path with
/// characters outside ASCII is preceded by `chcp 65001`, after which the
/// UTF-8 lines are read as written.
fn cmd_launcher(entry: &Path, bridge: &Path) -> Result<String> {
    let text = |path: &Path| -> Result<String> {
        let text = path
            .to_str()
            .with_context(|| format!("{} is not UTF-8", path.display()))?;
        if text.contains(['"', '\r', '\n']) {
            bail!("{text} cannot be written into a batch file");
        }
        Ok(text.replace('%', "%%"))
    };
    let (entry, bridge) = (text(entry)?, text(bridge)?);
    let mut script = String::from("@echo off\r\n");
    if !(entry.is_ascii() && bridge.is_ascii()) {
        script.push_str("chcp 65001 >nul\r\n");
    }
    script.push_str(
        "rem Written by veles browser-extension install. Chrome, Edge and Firefox\r\n\
         rem start this through cmd.exe with their own arguments and environment;\r\n\
         rem it runs the veles native host for the tab bridge. Rerun the install\r\n\
         rem to rewrite it.\r\n\
         setlocal DisableDelayedExpansion\r\n",
    );
    script.push_str(&format!(
        "set \"{BRIDGE_DIR_ENV}={bridge}\"\r\n\"{entry}\" browser-extension host %*\r\n"
    ));
    Ok(script)
}

/// Where the shipped extension is: beside the binary (a release archive,
/// the Windows zip and install), in `share/veles` (the Nix package and
/// bundles), or this source tree.
fn shipped_extension() -> Option<PathBuf> {
    crate::config::beside_binary("browser-extension")
        .or_else(|| crate::config::beside_binary("share/veles/browser-extension"))
        .or_else(|| {
            Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../browser-extension"))
                .filter(|p| p.is_dir())
        })
}

/// Copy the unpacked extension to a directory the user owns, so the path
/// a browser remembers survives upgrades, a GC and a bundle's namespace.
fn copy_extension(dest: &Path) -> Result<Option<PathBuf>> {
    let Some(source) = shipped_extension() else {
        return Ok(None);
    };
    if dest.exists() {
        std::fs::remove_dir_all(dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(&source)? {
        let entry = entry?;
        let name = entry.file_name();
        if !entry.file_type()?.is_file() || name.to_string_lossy().ends_with("-test.cjs") {
            continue;
        }
        let target = dest.join(&name);
        std::fs::copy(entry.path(), &target)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644))?;
        }
    }
    Ok(Some(dest.to_path_buf()))
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

fn install_into(
    os: HostOs,
    home: &Path,
    bridge: &Path,
    entry: &Path,
    name: &str,
    overwrite: bool,
    registry: &mut dyn Registry,
) -> Result<String> {
    let layout = layout(os, home, bridge, name);
    // All or nothing: refuse before writing anything (BX-17). On Windows a
    // key is a registration even when its manifest is gone.
    if !overwrite {
        for r in &layout.registrations {
            if r.manifest.exists() {
                bail!(
                    "{} already exists; rerun with --overwrite to replace the browser manifests",
                    r.manifest.display()
                );
            }
            if let Some(key) = &r.key {
                if let Some(old) = registry.default_value(key)? {
                    bail!(
                        "HKCU\\{key} already names {old}; rerun with --overwrite to replace the \
                         browser registrations"
                    );
                }
            }
        }
    }
    let token = match token() {
        Some(existing) => existing,
        None => create_token()?,
    };
    private_write(&layout.launcher, &launcher_script(os, entry, bridge)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&layout.launcher, std::fs::Permissions::from_mode(0o700))?;
    }
    let extension = copy_extension(&layout.extension)?;
    let base = json!({
        "name": name,
        "description": "veles local tab bridge",
        "path": path_text(&layout.launcher)?,
        "type": "stdio"
    });
    let mut registered = Vec::new();
    for r in &layout.registrations {
        let mut manifest = base.clone();
        match r.flavor {
            Flavor::Chromium => manifest["allowed_origins"] = json!([CHROME_ORIGIN]),
            Flavor::Firefox => manifest["allowed_extensions"] = json!([FIREFOX_EXTENSION_ID]),
        }
        if let Some(parent) = r.manifest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&r.manifest, serde_json::to_vec_pretty(&manifest)?)
            .with_context(|| format!("write {}", r.manifest.display()))?;
        match &r.key {
            Some(key) => {
                registry.set_default(key, path_text(&r.manifest)?)?;
                registered.push(format!(r"  {}: HKCU\{key}", r.browser));
            }
            None => registered.push(format!("  {}: {}", r.browser, r.manifest.display())),
        }
    }
    let extension = extension
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<installed veles share/veles/browser-extension>".into());
    let renamed = if name == NATIVE_NAME {
        String::new()
    } else {
        format!(
            "Native host name: {name} (the shipped extension connects to {NATIVE_NAME}; this \
             name is for tests)\n"
        )
    };
    Ok(format!(
        "Bridge token: {token}\n\
         Extension directory: {extension}\n\
         Native host: {} (runs {})\n\
         {renamed}\
         Registered for:\n{}\n\
         Load that directory on the browser's extensions page (Developer mode, Load unpacked) \
         or as a Firefox temporary add-on, then use its popup to connect a tab. The token is \
         also in {}.",
        layout.launcher.display(),
        entry.display(),
        registered.join("\n"),
        token_path().display()
    ))
}

fn remove_file_if_present(path: &Path, removed: &mut Vec<String>) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            removed.push(path.display().to_string());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

fn uninstall_from(
    os: HostOs,
    home: &Path,
    bridge: &Path,
    name: &str,
    registry: &mut dyn Registry,
) -> Result<String> {
    let layout = layout(os, home, bridge, name);
    let mut removed = Vec::new();
    // The registry keys first: once they are gone no browser starts the
    // host, whatever happens to the files. Then the keys above each one
    // (`NativeMessagingHosts`, the vendor's), for as long as nothing else
    // is in them: an install creates them where no browser had.
    for key in layout.registrations.iter().filter_map(|r| r.key.as_ref()) {
        if registry.delete(key)? {
            removed.push(format!(r"HKCU\{key}"));
        }
        let mut below = key.as_str();
        while let Some((above, _)) = below.rsplit_once('\\') {
            if above.eq_ignore_ascii_case("Software") || !registry.delete_if_empty(above)? {
                break;
            }
            removed.push(format!(r"HKCU\{above}"));
            below = above;
        }
    }
    let mut manifests: Vec<&Path> = Vec::new();
    for r in &layout.registrations {
        if !manifests.contains(&r.manifest.as_path()) {
            manifests.push(&r.manifest);
        }
    }
    for manifest in manifests {
        remove_file_if_present(manifest, &mut removed)?;
    }
    remove_file_if_present(&layout.launcher, &mut removed)?;
    if layout.extension.is_dir() {
        std::fs::remove_dir_all(&layout.extension)
            .with_context(|| format!("remove {}", layout.extension.display()))?;
        removed.push(layout.extension.display().to_string());
    }
    // The token belongs to the bridge, not to a test name's registrations.
    if name == NATIVE_NAME {
        remove_file_if_present(&token_path(), &mut removed)?;
    }
    // Only when empty: nothing else of the user's is in it.
    let _ = std::fs::remove_dir(&layout.dir);
    if removed.is_empty() {
        return Ok(format!(
            "Nothing to remove: the {name} native host is not registered here."
        ));
    }
    let connected = if endpoint_path().is_file() {
        "\nA tab is still connected: click Disconnect in the extension popup to stop the native \
         host."
    } else {
        ""
    };
    Ok(format!(
        "Removed:\n  {}\nRemove the veles tab bridge extension on the browser's extensions page \
         too.{connected}",
        removed.join("\n  ")
    ))
}

pub fn status() -> Value {
    json!({"installed":token().is_some(),"connected":endpoint_path().is_file(),"browser":"Chrome, Chromium, Edge or Firefox native messaging"})
}

/// The tool's half of one exchange, blocking: run it off the runtime.
fn exchange(request: Value) -> String {
    let endpoint = match std::fs::read_to_string(endpoint_path()) {
        Ok(value) => value,
        Err(_) => {
            return "error: no tab is connected; use the extension popup's Connect this tab".into()
        }
    };
    let mut parts = endpoint.split_whitespace();
    let Some(port) = parts.next().and_then(|p| p.parse::<u16>().ok()) else {
        return "error: the extension bridge has invalid state; reconnect the tab".into();
    };
    if parts.next().unwrap_or_default().is_empty() {
        return "error: the extension bridge has invalid state; reconnect the tab".into();
    }
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
        Ok(s) => s,
        Err(e) => return format!("error: the extension bridge is unavailable: {e}"),
    };
    let _ = stream.set_read_timeout(Some(ACTION_TIMEOUT + Duration::from_secs(5)));
    let reply = (|| -> Result<Value> {
        write_frame(&mut stream, &request)?;
        read_frame_up_to(&mut stream, MAX_AGENT_REPLY)?.context("extension disconnected")
    })();
    match reply {
        Ok(value) => value.to_string(),
        Err(e) => format!("error: {e}"),
    }
}

pub struct BrowserExtensionTool {
    pub session_id: String,
}

#[async_trait]
impl crate::tools::Tool for BrowserExtensionTool {
    fn name(&self) -> &str {
        "browser_extension"
    }
    fn description(&self) -> &str {
        "Use a Chrome, Chromium, Edge or Firefox tab explicitly connected with the veles tab bridge extension. status shows setup state; snapshot reads the tab and returns element refs (secret fields such as passwords show only whether they are filled); click uses a ref; fill sets a text field; navigate opens an http(s) URL, after which the owner may need to reconnect the tab. Only sessions on the machine running the browser can use it. Install with `veles browser-extension install` (Linux, Windows, macOS), load the extension, and click Connect this tab."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["status","snapshot","click","fill","navigate"]},
            "ref":{"type":"string","description":"Element ref from snapshot, for click/fill"},
            "text":{"type":"string","description":"Text for fill"},
            "url":{"type":"string","description":"http(s) URL for navigate"}
        },"required":["action"]})
    }
    async fn call(&self, args: Value) -> String {
        if !crate::ask::owner_session(&self.session_id) {
            // A call routed here from another node arrives as a capped
            // principal, never the owner (BX-23): say where it works.
            return "error: the browser tab bridge is owner-only and works only from the owner's sessions on the machine running the browser".into();
        }
        if args["action"] == "status" {
            return status().to_string();
        }
        if !valid_request(&args) {
            return "error: invalid browser action, ref, text, or http(s) URL".into();
        }
        if args["action"] == "navigate" {
            if let Err(e) = crate::urlguard::check(args["url"].as_str().unwrap_or_default()).await {
                return format!("error: {e}");
            }
        }
        let Some(token) = token() else {
            return "error: no extension token; install the bridge".into();
        };
        let mut request = args;
        request["token"] = json!(token);
        // Up to 35 s of socket I/O: never on a runtime worker (BX-21).
        crate::rt::spawn_blocking(move || exchange(request))
            .await
            .unwrap_or_else(|e| format!("error: the extension bridge call failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    #[test]
    fn frames_are_bounded_and_authentication_is_required() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &json!({"type":"hello","token":"secret"})).unwrap();
        assert_eq!(
            read_frame(&mut bytes.as_slice()).unwrap().unwrap()["type"],
            "hello"
        );
        assert!(read_frame(&mut (MAX_FRAME as u32 + 1).to_le_bytes().to_vec().as_slice()).is_err());
        assert!(authenticated(&json!({"token":"secret"}), "secret"));
        assert!(!authenticated(&json!({"token":"other"}), "secret"));
    }

    #[test]
    fn tool_actions_cannot_be_shell_commands_or_file_urls() {
        assert!(valid_request(&json!({"action":"snapshot"})));
        assert!(valid_request(&json!({"action":"click","ref":"12"})));
        assert!(valid_request(
            &json!({"action":"fill","ref":"12","text":"hello"})
        ));
        assert!(valid_request(
            &json!({"action":"navigate","url":"https://example.org"})
        ));
        assert!(!valid_request(
            &json!({"action":"navigate","url":"file:///etc/passwd"})
        ));
        assert!(!valid_request(&json!({"action":"terminal","command":"id"})));
    }

    #[test]
    fn the_tool_checks_the_calling_session() {
        let tool = BrowserExtensionTool {
            session_id: "someone-else".into(),
        };
        let (_home, _env) = crate::testenv::scratch_home();
        crate::acl::set_session_principal("someone-else", crate::acl::Principal::guest());
        let result = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(tool.call(json!({"action":"snapshot"})));
        assert!(result.contains("owner-only"));
        crate::acl::clear_session_principal("someone-else");
    }

    #[cfg(unix)]
    #[test]
    fn the_native_host_forwards_an_action_to_the_same_tab_and_closes() {
        use std::os::unix::net::UnixStream;
        let (_home, _env) = crate::testenv::scratch_home();
        let secret = create_token().unwrap();
        let (host, mut browser) = UnixStream::pair().unwrap();
        let host_in = host.try_clone().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let host_thread = std::thread::spawn(move || {
            let result = run_host_with_channels(host_in, host);
            done_tx.send(result).unwrap();
        });
        write_frame(&mut browser, &json!({"type":"hello","token":secret})).unwrap();
        write_frame(&mut browser, &json!({"type":"connect","id":"tab-1"})).unwrap();
        let hello = read_frame(&mut browser).unwrap().unwrap();
        assert_eq!(hello["type"], "hello-ok");
        let endpoint = std::fs::read_to_string(endpoint_path()).unwrap();
        let port = endpoint
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let mut agent = TcpStream::connect(("127.0.0.1", port)).unwrap();
        agent
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write_frame(&mut agent, &json!({"token":secret,"action":"snapshot"})).unwrap();
        let action = read_frame(&mut browser).unwrap().unwrap();
        assert_eq!(action["type"], "action");
        assert_eq!(action["id"], "tab-1");
        assert!(
            action.get("token").is_none(),
            "the agent token never reaches page work"
        );
        write_frame(
            &mut browser,
            &json!({"type":"result","id":"tab-1","seq":action["seq"],"ok":true,"result":{"title":"Example"}}),
        )
        .unwrap();
        let reply = read_frame(&mut agent).unwrap().unwrap();
        assert_eq!(reply["result"]["title"], "Example");
        write_frame(&mut browser, &json!({"type":"close","id":"tab-1"})).unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("host must stop on close")
            .unwrap();
        host_thread.join().unwrap();
        assert!(!endpoint_path().exists());
    }

    #[test]
    fn browser_launches_are_recognised_and_subcommands_are_not() {
        let argv = |a: &[&str]| a.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert!(is_browser_launch(&argv(&["veles", CHROME_ORIGIN])));
        assert!(is_browser_launch(&argv(&[
            "veles",
            CHROME_ORIGIN,
            "--parent-window=0"
        ])));
        assert!(is_browser_launch(&argv(&[
            "veles",
            "/home/u/.mozilla/native-messaging-hosts/veles_browser.json",
            FIREFOX_EXTENSION_ID
        ])));
        assert!(!is_browser_launch(&argv(&["veles"])));
        assert!(!is_browser_launch(&argv(&[
            "veles",
            "browser-extension",
            "host"
        ])));
        assert!(!is_browser_launch(&argv(&["veles", "chat", "-q", "hi"])));
    }

    /// BX-8: the manifest must name a veles that is still there after an
    /// upgrade and a GC, and that a browser outside a bundle can run.
    #[cfg(unix)]
    #[test]
    fn the_host_entry_point_survives_upgrades_and_bundles() {
        let dir = tempfile::tempdir().unwrap();
        let file = |rel: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            p
        };
        let store = dir.path().join("store");
        let wrapped = file("store/abc-veles/bin/.veles-wrapped");
        let wrapper = file("store/abc-veles/bin/veles");
        let profile = file("profile/bin/veles");
        let missing = dir.path().join("nowhere/bin/veles");
        let image = file("Veles.AppImage");
        let bundle = dir.path().join("bundle");
        let bundle_launcher = file("bundle/bin/veles");
        let pick = |appimage: Option<&PathBuf>, root: Option<&PathBuf>, profiles: &[PathBuf]| {
            stable_entry_point(&wrapped, appimage.cloned(), root.cloned(), &store, profiles)
        };
        let only_profile = std::slice::from_ref(&profile);
        assert_eq!(pick(Some(&image), Some(&bundle), only_profile), image);
        assert_eq!(pick(None, Some(&bundle), only_profile), bundle_launcher);
        assert_eq!(
            pick(None, None, &[missing.clone(), profile.clone()]),
            profile
        );
        assert_eq!(
            pick(None, None, std::slice::from_ref(&missing)),
            wrapper.canonicalize().unwrap()
        );
        let plain = file("opt/veles/bin/veles");
        assert_eq!(
            stable_entry_point(&plain, None, None, &store, only_profile),
            plain.canonicalize().unwrap(),
            "a binary outside the store is used as it is"
        );
        // scripts/install/install.sh: the bundle unpacked under
        // releases/<id>, run through `current`, which an upgrade switches.
        let release = dir.path().join("install/releases/0.2.0-abc1234");
        let released = file("install/releases/0.2.0-abc1234/bin/veles");
        let current = file("install/current/bin/veles");
        assert_eq!(
            stable_entry_point(&wrapped, None, Some(release), &store, &[]),
            current,
            "a bundle inside an installer release is launched through current"
        );
        assert_eq!(
            stable_entry_point(&released, None, None, &store, &[]),
            dir.path()
                .canonicalize()
                .unwrap()
                .join("install/current/bin/veles"),
            "so is a plain binary inside one"
        );
    }

    /// install.ps1 runs `current\bin\veles.exe` through a junction that
    /// each upgrade switches; install.sh does the same with a symlink.
    #[test]
    fn an_installer_release_is_launched_through_current() {
        let dir = tempfile::tempdir().unwrap();
        let file = |rel: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            p
        };
        let exe = file("veles/releases/0.2.0-abc1234/bin/veles.exe");
        assert_eq!(release_current(&exe), None, "no current yet");
        let current = file("veles/current/bin/veles.exe");
        assert_eq!(release_current(&exe), Some(current));
        assert_eq!(release_current(&file("opt/veles/bin/veles.exe")), None);
        assert_eq!(release_current(Path::new("veles.exe")), None);
    }

    #[test]
    fn the_launcher_runs_the_host_with_the_bridge_dir_fixed() {
        for os in [HostOs::Linux, HostOs::MacOs] {
            let script =
                launcher_script(os, Path::new("/opt/it's/veles"), Path::new("/data/veles"))
                    .unwrap();
            assert!(script.starts_with("#!/bin/sh\n"));
            assert!(script.contains(
                "VELES_BROWSER_BRIDGE_DIR='/data/veles'\nexport VELES_BROWSER_BRIDGE_DIR\n"
            ));
            assert!(
                script.ends_with("exec '/opt/it'\\''s/veles' browser-extension host \"$@\"\n"),
                "{script}"
            );
        }
    }

    /// BX-9: Chrome and Firefox start a Windows native host through
    /// cmd.exe. The `.cmd` must put nothing on stdout, set the bridge
    /// directory however odd its path, and hand the browser's arguments on.
    #[test]
    fn the_cmd_launcher_runs_the_host_with_the_bridge_dir_fixed() {
        let script = launcher_script(
            HostOs::Windows,
            Path::new(r"C:\Program Files\veles 100%!\bin\veles.exe"),
            Path::new(r"C:\Users\u\AppData\Local\veles"),
        )
        .unwrap();
        assert!(
            script.starts_with("@echo off\r\n"),
            "an echoed command would corrupt the native messaging stream: {script}"
        );
        assert!(!script.contains("chcp"), "ASCII paths need no code page");
        assert!(script.contains("setlocal DisableDelayedExpansion\r\n"));
        assert!(
            script.contains(
                "set \"VELES_BROWSER_BRIDGE_DIR=C:\\Users\\u\\AppData\\Local\\veles\"\r\n"
            ),
            "{script}"
        );
        assert!(
            script.ends_with(
                "\"C:\\Program Files\\veles 100%%!\\bin\\veles.exe\" browser-extension host %*\r\n"
            ),
            "{script}"
        );
        assert_eq!(
            script.matches('\n').count(),
            script.matches("\r\n").count(),
            "batch files end their lines with CRLF"
        );
        let unicode = launcher_script(
            HostOs::Windows,
            Path::new(r"C:\Users\Ђорђе\AppData\Local\Programs\veles\current\bin\veles.exe"),
            Path::new(r"C:\Users\Ђорђе\AppData\Local\veles"),
        )
        .unwrap();
        assert!(
            unicode.starts_with("@echo off\r\nchcp 65001 >nul\r\n"),
            "cmd reads a batch file in the console code page: {unicode}"
        );
        assert!(launcher_script(
            HostOs::Windows,
            Path::new("C:\\a\"b\\veles.exe"),
            Path::new(r"C:\d")
        )
        .is_err());
    }

    #[test]
    fn native_host_names_are_checked() {
        for good in [NATIVE_NAME, "veles_browser_e2e_1a2b", "a.b_c"] {
            assert!(check_native_name(good).is_ok(), "{good}");
        }
        let long = "a".repeat(65);
        for bad in [
            "",
            "Veles",
            "a..b",
            ".a",
            "a.",
            "a/b",
            r"a\b",
            "a b",
            "a-b",
            long.as_str(),
        ] {
            assert!(check_native_name(bad).is_err(), "{bad:?}");
        }
        let (_home, _env) = crate::testenv::scratch_home();
        assert!(install_extension(false, r"..\x").is_err());
        assert!(token().is_none(), "a refused name writes nothing");
    }

    /// The Windows registry, in memory: what the install's Windows logic
    /// is tested against off Windows. platform/windows.rs tests the real
    /// HKCU calls.
    /// Keys exist apart from values, and setting a value creates every
    /// key above it, as `RegCreateKeyExW` does.
    #[derive(Default)]
    struct FakeRegistry {
        keys: std::collections::BTreeSet<String>,
        values: std::collections::BTreeMap<String, String>,
    }

    impl FakeRegistry {
        fn create(&mut self, key: &str) {
            let mut path = String::new();
            for part in key.split('\\') {
                if !path.is_empty() {
                    path.push('\\');
                }
                path.push_str(part);
                self.keys.insert(path.clone());
            }
        }
        fn has_children(&self, key: &str) -> bool {
            let prefix = format!("{key}\\");
            self.keys.iter().any(|k| k.starts_with(&prefix))
        }
    }

    impl Registry for FakeRegistry {
        fn default_value(&self, key: &str) -> Result<Option<String>> {
            Ok(self.values.get(key).cloned())
        }
        fn set_default(&mut self, key: &str, value: &str) -> Result<()> {
            self.create(key);
            self.values.insert(key.to_owned(), value.to_owned());
            Ok(())
        }
        fn delete(&mut self, key: &str) -> Result<bool> {
            if !self.keys.contains(key) {
                return Ok(false);
            }
            let prefix = format!("{key}\\");
            self.keys.retain(|k| k != key && !k.starts_with(&prefix));
            self.values
                .retain(|k, _| k != key && !k.starts_with(&prefix));
            Ok(true)
        }
        fn delete_if_empty(&mut self, key: &str) -> Result<bool> {
            if !self.keys.contains(key) || self.values.contains_key(key) || self.has_children(key) {
                return Ok(false);
            }
            self.keys.remove(key);
            Ok(true)
        }
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap_or_else(|e| panic!("{path:?}: {e}")))
            .unwrap()
    }

    fn assert_flavor(manifest: &Value, flavor: Flavor, name: &str, launcher: &Path) {
        assert_eq!(manifest["name"], name);
        assert_eq!(manifest["type"], "stdio");
        assert_eq!(manifest["path"], launcher.to_str().unwrap());
        match flavor {
            Flavor::Chromium => {
                assert_eq!(manifest["allowed_origins"], json!([CHROME_ORIGIN]));
                assert!(manifest.get("allowed_extensions").is_none());
            }
            Flavor::Firefox => {
                assert_eq!(
                    manifest["allowed_extensions"],
                    json!([FIREFOX_EXTENSION_ID])
                );
                assert!(manifest.get("allowed_origins").is_none());
            }
        }
    }

    /// BX-9: on Windows the browsers find a native host only through
    /// `HKCU\Software\<vendor>\NativeMessagingHosts\<name>`, whose default
    /// value is the manifest's path. Uninstall removes every key and file.
    #[test]
    fn a_windows_install_registers_every_browser_and_uninstall_removes_it() {
        let (_scratch, _env) = crate::testenv::scratch_home();
        let bridge = crate::config::data_dir();
        let home = tempfile::tempdir().unwrap();
        let entry = Path::new(r"C:\Users\Ђорђе\AppData\Local\Programs\veles\current\bin\veles.exe");
        // What the user already had: another host under Mozilla, and keys
        // under Microsoft that are no browser's.
        let mut registry = FakeRegistry::default();
        let foreign = r"Software\Mozilla\NativeMessagingHosts\com.example.other";
        registry.set_default(foreign, r"C:\other.json").unwrap();
        registry.create(r"Software\Microsoft\Windows");
        let out = install_into(
            HostOs::Windows,
            home.path(),
            &bridge,
            entry,
            NATIVE_NAME,
            false,
            &mut registry,
        )
        .unwrap();
        let keys = [
            (
                r"Software\Google\Chrome\NativeMessagingHosts\veles_browser",
                Flavor::Chromium,
            ),
            (
                r"Software\Chromium\NativeMessagingHosts\veles_browser",
                Flavor::Chromium,
            ),
            (
                r"Software\Microsoft\Edge\NativeMessagingHosts\veles_browser",
                Flavor::Chromium,
            ),
            (
                r"Software\Mozilla\NativeMessagingHosts\veles_browser",
                Flavor::Firefox,
            ),
        ];
        assert_eq!(
            registry
                .values
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            {
                let mut sorted = keys.map(|(k, _)| k).to_vec();
                sorted.push(foreign);
                sorted.sort();
                sorted
            },
            "Chrome, Chromium, Edge and Firefox, each under HKCU"
        );
        let dir = bridge.join("browser-extension");
        let launcher = dir.join("native-host.cmd");
        for (key, flavor) in keys {
            let manifest = PathBuf::from(&registry.values[key]);
            assert!(manifest.is_absolute(), "{key} must name an absolute path");
            assert!(manifest.starts_with(&dir), "{manifest:?}");
            assert_flavor(&read_json(&manifest), flavor, NATIVE_NAME, &launcher);
            assert!(out.contains(&format!(r"HKCU\{key}")), "{out}");
        }
        assert_eq!(
            std::fs::read_to_string(&launcher).unwrap(),
            cmd_launcher(entry, &bridge).unwrap(),
            "the manifests name the .cmd launcher"
        );
        assert!(dir.join("extension/manifest.json").is_file());
        assert!(token().is_some());
        assert_eq!(
            std::fs::read_dir(home.path()).unwrap().count(),
            0,
            "nothing goes where Linux browsers look"
        );

        // A key is a registration even when its manifest is gone.
        for (key, _) in keys {
            let _ = std::fs::remove_file(&registry.values[key]);
        }
        let refused = install_into(
            HostOs::Windows,
            home.path(),
            &bridge,
            entry,
            NATIVE_NAME,
            false,
            &mut registry,
        )
        .unwrap_err()
        .to_string();
        assert!(
            refused.contains(r"HKCU\Software\") && refused.contains("--overwrite"),
            "{refused}"
        );
        install_into(
            HostOs::Windows,
            home.path(),
            &bridge,
            entry,
            NATIVE_NAME,
            true,
            &mut registry,
        )
        .unwrap();

        let out = uninstall_from(
            HostOs::Windows,
            home.path(),
            &bridge,
            NATIVE_NAME,
            &mut registry,
        )
        .unwrap();
        for (key, _) in keys {
            assert!(out.contains(&format!(r"HKCU\{key}")), "{out}");
        }
        // The keys the install created above its registrations go while
        // nothing else is in them; the user's own stay.
        assert_eq!(
            registry.keys.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "Software",
                r"Software\Microsoft",
                r"Software\Microsoft\Windows",
                r"Software\Mozilla",
                r"Software\Mozilla\NativeMessagingHosts",
                foreign,
            ],
            "{out}"
        );
        assert_eq!(
            registry
                .values
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [foreign]
        );
        assert!(out.contains(r"HKCU\Software\Chromium"), "{out}");
        assert!(
            !dir.exists(),
            "launcher, manifests and the extension copy are gone"
        );
        assert!(token().is_none(), "and the token with them");
        let again = uninstall_from(
            HostOs::Windows,
            home.path(),
            &bridge,
            NATIVE_NAME,
            &mut registry,
        )
        .unwrap();
        assert!(again.starts_with("Nothing to remove"), "{again}");
    }

    /// BX-9: macOS browsers read user-level hosts from
    /// `~/Library/Application Support/<browser>/NativeMessagingHosts`.
    /// Untested on a Mac: this is what the browsers' documentation says.
    #[test]
    fn a_macos_install_writes_the_library_manifests_and_uninstall_removes_them() {
        let (_scratch, _env) = crate::testenv::scratch_home();
        let bridge = crate::config::data_dir();
        let home = tempfile::tempdir().unwrap();
        let entry = Path::new("/Applications/veles/bin/veles");
        let support = home.path().join("Library").join("Application Support");
        let manifests = [
            (
                support
                    .join("Google/Chrome/NativeMessagingHosts")
                    .join("veles_browser.json"),
                Flavor::Chromium,
            ),
            (
                support
                    .join("Chromium/NativeMessagingHosts")
                    .join("veles_browser.json"),
                Flavor::Chromium,
            ),
            (
                support
                    .join("Mozilla/NativeMessagingHosts")
                    .join("veles_browser.json"),
                Flavor::Firefox,
            ),
        ];
        let install = |overwrite| {
            install_into(
                HostOs::MacOs,
                home.path(),
                &bridge,
                entry,
                NATIVE_NAME,
                overwrite,
                &mut NoRegistry,
            )
        };
        let out = install(false).unwrap();
        let launcher = bridge.join("browser-extension").join("native-host");
        for (path, flavor) in &manifests {
            assert_flavor(&read_json(path), *flavor, NATIVE_NAME, &launcher);
            assert!(out.contains(&path.display().to_string()), "{out}");
        }
        assert!(std::fs::read_to_string(&launcher)
            .unwrap()
            .starts_with("#!/bin/sh\n"));
        assert!(
            install(false)
                .unwrap_err()
                .to_string()
                .contains("--overwrite"),
            "an existing manifest needs --overwrite"
        );
        install(true).unwrap();
        uninstall_from(
            HostOs::MacOs,
            home.path(),
            &bridge,
            NATIVE_NAME,
            &mut NoRegistry,
        )
        .unwrap();
        for (path, _) in &manifests {
            assert!(!path.exists(), "{path:?}");
        }
        assert!(!bridge.join("browser-extension").exists());
        assert!(token().is_none());
    }

    /// A test's `--native-name` install keeps to its own files, so its
    /// uninstall cannot take the owner's bridge with it.
    #[test]
    fn a_test_name_install_keeps_away_from_the_shipped_one() {
        let (_scratch, _env) = crate::testenv::scratch_home();
        let bridge = crate::config::data_dir();
        let home = tempfile::tempdir().unwrap();
        let entry = Path::new("/opt/veles/bin/veles");
        let install = |name| {
            install_into(
                HostOs::Linux,
                home.path(),
                &bridge,
                entry,
                name,
                false,
                &mut NoRegistry,
            )
            .unwrap()
        };
        install(NATIVE_NAME);
        let out = install("veles_browser_e2e");
        assert!(out.contains("Native host name: veles_browser_e2e"), "{out}");
        let test_manifest = home
            .path()
            .join(".mozilla/native-messaging-hosts")
            .join("veles_browser_e2e.json");
        assert_flavor(
            &read_json(&test_manifest),
            Flavor::Firefox,
            "veles_browser_e2e",
            &bridge
                .join("browser-extension-veles_browser_e2e")
                .join("native-host"),
        );
        uninstall_from(
            HostOs::Linux,
            home.path(),
            &bridge,
            "veles_browser_e2e",
            &mut NoRegistry,
        )
        .unwrap();
        assert!(!test_manifest.exists());
        assert!(!bridge.join("browser-extension-veles_browser_e2e").exists());
        assert!(home
            .path()
            .join(".mozilla/native-messaging-hosts/veles_browser.json")
            .is_file());
        assert!(bridge.join("browser-extension/native-host").is_file());
        assert!(bridge
            .join("browser-extension/extension/manifest.json")
            .is_file());
        assert!(token().is_some(), "the bridge's token stays");
    }

    // ── Host tests that speak raw frames, with no size cap of their own,
    // so they can play a browser that sends more than the host allows.

    #[cfg(unix)]
    fn raw_send(out: &mut impl Write, value: &Value) {
        let body = serde_json::to_vec(value).unwrap();
        out.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
        out.write_all(&body).unwrap();
        out.flush().unwrap();
    }

    #[cfg(unix)]
    fn raw_recv(input: &mut impl Read) -> Value {
        let mut length = [0u8; 4];
        input.read_exact(&mut length).unwrap();
        let mut body = vec![0u8; u32::from_le_bytes(length) as usize];
        input.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[cfg(unix)]
    struct Host {
        browser: std::os::unix::net::UnixStream,
        done: std::sync::mpsc::Receiver<Result<()>>,
        secret: String,
        port: u16,
    }

    /// A host connected as tab `tab-1`, with the browser end of its stdio.
    #[cfg(unix)]
    fn connected_host() -> Host {
        use std::os::unix::net::UnixStream;
        let secret = create_token().unwrap();
        let (host, mut browser) = UnixStream::pair().unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let host_in = host.try_clone().unwrap();
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(run_host_with_channels(host_in, host));
        });
        raw_send(
            &mut browser,
            &json!({"type":"hello","id":"tab-1","token":secret}),
        );
        raw_send(&mut browser, &json!({"type":"connect","id":"tab-1"}));
        assert_eq!(raw_recv(&mut browser)["type"], "hello-ok");
        let endpoint = std::fs::read_to_string(endpoint_path()).unwrap();
        let port = endpoint
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<u16>()
            .unwrap();
        Host {
            browser,
            done,
            secret,
            port,
        }
    }

    #[cfg(unix)]
    fn agent(port: u16) -> TcpStream {
        let agent = TcpStream::connect(("127.0.0.1", port)).unwrap();
        agent
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        agent
    }

    #[cfg(unix)]
    #[test]
    fn a_wrong_token_hello_is_refused_with_its_id_and_the_host_exits() {
        use std::os::unix::net::UnixStream;
        let (_home, _env) = crate::testenv::scratch_home();
        create_token().unwrap();
        let (host, mut browser) = UnixStream::pair().unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let host_in = host.try_clone().unwrap();
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(run_host_with_channels(host_in, host));
        });
        raw_send(
            &mut browser,
            &json!({"type":"hello","id":"c-1","token":"not-the-token"}),
        );
        let refused = raw_recv(&mut browser);
        assert_eq!(refused["type"], "error");
        assert_eq!(refused["id"], "c-1", "the extension matches errors by id");
        assert_eq!(refused["error"], "authentication failed");
        done.recv_timeout(Duration::from_secs(5))
            .expect("a refused host exits")
            .unwrap();
        assert!(!endpoint_path().exists(), "no endpoint for a refused tab");
    }

    #[cfg(unix)]
    #[test]
    fn a_wrong_token_or_an_invalid_action_never_reaches_the_tab() {
        let (_home, _env) = crate::testenv::scratch_home();
        let mut host = connected_host();
        let mut a = agent(host.port);
        raw_send(&mut a, &json!({"token":"wrong","action":"snapshot"}));
        assert_eq!(raw_recv(&mut a)["ok"], false);
        let mut a = agent(host.port);
        raw_send(
            &mut a,
            &json!({"token":host.secret,"action":"navigate","url":"file:///etc/passwd"}),
        );
        assert_eq!(raw_recv(&mut a)["ok"], false);
        let mut a = agent(host.port);
        raw_send(&mut a, &json!({"token":host.secret,"action":"snapshot"}));
        let first = raw_recv(&mut host.browser);
        assert_eq!(
            first["action"], "snapshot",
            "the refused requests must not have been forwarded: {first}"
        );
        raw_send(
            &mut host.browser,
            &json!({"type":"result","id":"tab-1","seq":first["seq"],"ok":true,"result":{}}),
        );
        assert_eq!(raw_recv(&mut a)["ok"], true);
        raw_send(&mut host.browser, &json!({"type":"close","id":"tab-1"}));
        host.done
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
    }

    /// BX-10: the loopback port is open to every local user. Nothing a
    /// connection sends or fails to send may end the owner's bridge.
    #[cfg(unix)]
    #[test]
    fn garbage_on_the_loopback_port_does_not_kill_the_host() {
        let (_home, _env) = crate::testenv::scratch_home();
        let mut host = connected_host();
        // An absurd length prefix.
        let mut a = agent(host.port);
        a.write_all(&u32::MAX.to_le_bytes()).unwrap();
        let _ = a.read(&mut [0u8; 64]);
        drop(a);
        // Half a frame, then hang up.
        let mut a = agent(host.port);
        a.write_all(&100u32.to_le_bytes()).unwrap();
        a.write_all(b"{\"tok").unwrap();
        drop(a);
        // Not JSON.
        let mut a = agent(host.port);
        a.write_all(&3u32.to_le_bytes()).unwrap();
        a.write_all(b"xyz").unwrap();
        let _ = a.read(&mut [0u8; 64]);
        drop(a);
        // A valid request still gets through.
        let mut a = agent(host.port);
        raw_send(&mut a, &json!({"token":host.secret,"action":"snapshot"}));
        let action = raw_recv(&mut host.browser);
        assert_eq!(action["action"], "snapshot");
        raw_send(
            &mut host.browser,
            &json!({"type":"result","id":"tab-1","seq":action["seq"],"ok":true,"result":{"title":"still here"}}),
        );
        assert_eq!(raw_recv(&mut a)["result"]["title"], "still here");
        raw_send(&mut host.browser, &json!({"type":"close","id":"tab-1"}));
        host.done
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
    }

    /// BX-4: a snapshot of an ordinary Cyrillic or CJK article is more
    /// than 64 KiB of UTF-8. It must reach the agent, not end the bridge.
    #[cfg(unix)]
    #[test]
    fn a_large_non_ascii_snapshot_reaches_the_agent() {
        let (_home, _env) = crate::testenv::scratch_home();
        let mut host = connected_host();
        let text = "Ћирилица и ијекавица. ".repeat(3000);
        assert!(text.len() > 100 * 1024);
        let mut a = agent(host.port);
        raw_send(&mut a, &json!({"token":host.secret,"action":"snapshot"}));
        let action = raw_recv(&mut host.browser);
        raw_send(
            &mut host.browser,
            &json!({"type":"result","id":"tab-1","seq":action["seq"],"ok":true,"result":{"text":text}}),
        );
        let reply = raw_recv(&mut a);
        assert_eq!(reply["result"]["text"].as_str().unwrap().len(), text.len());
        raw_send(&mut host.browser, &json!({"type":"close","id":"tab-1"}));
        host.done
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
    }

    /// BX-11: a reply that arrives late (after its caller gave up), or
    /// with no action pending, is dropped. It is never handed to the
    /// next caller and it does not end the host.
    #[cfg(unix)]
    #[test]
    fn a_stale_reply_is_dropped_not_misattributed() {
        let (_home, _env) = crate::testenv::scratch_home();
        let mut host = connected_host();
        // Idle host, stray result.
        raw_send(
            &mut host.browser,
            &json!({"type":"result","id":"tab-1","seq":999,"ok":true,"result":{"title":"stale"}}),
        );
        std::thread::sleep(Duration::from_millis(200));
        let mut a = agent(host.port);
        raw_send(&mut a, &json!({"token":host.secret,"action":"snapshot"}));
        let action = raw_recv(&mut host.browser);
        assert_eq!(action["action"], "snapshot");
        assert!(action["seq"].is_u64(), "each action carries its own seq");
        // A stale result first, then the real one.
        raw_send(
            &mut host.browser,
            &json!({"type":"result","id":"tab-1","seq":998,"ok":true,"result":{"title":"stale"}}),
        );
        raw_send(
            &mut host.browser,
            &json!({"type":"result","id":"tab-1","seq":action["seq"],"ok":true,"result":{"title":"fresh"}}),
        );
        assert_eq!(raw_recv(&mut a)["result"]["title"], "fresh");
        raw_send(&mut host.browser, &json!({"type":"close","id":"tab-1"}));
        host.done
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
    }

    /// BX-2/BX-3/BX-19: the shipped extension manifest. No browser ran
    /// this before 0.2.0, which is how a manifest without
    /// `nativeMessaging` and without a Firefox background shipped green.
    #[test]
    fn the_extension_manifest_can_reach_the_native_host_in_both_browsers() {
        let manifest: Value =
            serde_json::from_str(include_str!("../../../browser-extension/manifest.json")).unwrap();
        let permissions = manifest["permissions"].as_array().unwrap();
        assert!(
            permissions.iter().any(|p| p == "nativeMessaging"),
            "runtime.connectNative needs the nativeMessaging permission"
        );
        assert_eq!(
            manifest["background"]["scripts"],
            json!(["background.js"]),
            "Firefox runs only background.scripts"
        );
        assert_eq!(
            manifest["background"]["service_worker"], "background.js",
            "Chrome runs only background.service_worker"
        );
        assert_eq!(
            manifest["browser_specific_settings"]["gecko"]["id"],
            "veles@7778777.online"
        );
        assert_eq!(
            manifest["version"],
            env!("CARGO_PKG_VERSION"),
            "the extension version moves with the release"
        );
    }
}

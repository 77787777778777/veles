//! The `container-host` profile: a generalised, pure builder for the plan
//! `deploy/vps/setup.sh` performed by hand on one host.
//!
//! Nothing here touches the system. `ContainerHostOptions::plan()` is a
//! total function from validated options to a [`ContainerHostPlan`]: the
//! typed files and quadlets it writes, the images it pulls, the optional
//! Tor and nft configuration, and ONE reviewed privileged shell script
//! that the caller runs through sudo (PLAN-plugins-installer.md, Phase 8
//! "Hardened container host").
//!
//! Two rules shape the types:
//!
//! * Every host, site, account and path is an OPTION. No personal host,
//!   owner, repository, storage root or domain value is compiled in; the
//!   tests scan the rendered plan for the values the hand-written script
//!   carried and fail if one reappears.
//! * Every option is validated before it is rendered. A user name is a
//!   POSIX name, a path is absolute and normalised, a firewall rule is a
//!   real address, an image is digest-pinned. The script quotes what it
//!   accepts, so no accepted value can become a second shell word.
//!
//! A pinned image is the only way to name one: [`PinnedImage::new`]
//! refuses an empty or malformed digest and refuses the tags that mean
//! "whatever this host built locally" (`current`, `latest`, an untagged
//! `localhost/...`). A remote image is therefore always
//! `repo/name:tag@sha256:...`, and a quadlet cannot re-introduce an
//! unpinned `Image=` line because [`ContainerSpec::extra`] is checked for
//! it.

use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component as PathComponent, Path, PathBuf};

// ── primitives ─────────────────────────────────────────────────────────

/// Lowercase hex SHA-256. The installer crate's `sha256` returns the same
/// bytes; kept here so the module compiles into a test on its own.
pub fn digest_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut out = String::with_capacity(64);
    for b in sha2::Sha256::digest(bytes) {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// The heredoc terminator for a file, derived from its content so two
/// files never share one and no file can contain its own terminator.
pub fn heredoc_marker(contents: impl AsRef<[u8]>) -> String {
    format!("__VELES_PLAN_{}__", &digest_hex(contents.as_ref())[..24])
}

/// Quote one shell word. Single quotes are the only safe form here: the
/// values are paths, names and addresses, never expressions.
fn sq(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn sq_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    // A path that cannot be rendered as UTF-8 could not have passed
    // `valid_absolute_path`; the caller sees an empty quoted word rather
    // than a lossy one silently used.
    if text.contains('\u{fffd}') {
        return "''".into();
    }
    sq(&text)
}

/// A POSIX user or group name, and the reason the host may have it.
fn valid_account_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && name != "root"
        && name != "nobody"
        // A trailing dash is legal in some tools and invalid in others;
        // refuse it rather than find out in useradd.
        && !name.ends_with('-')
}

/// An absolute, normalised path with no `..`, no `.`, and no empty part.
/// Symlinks are not resolved: this is a pure check on the string.
fn valid_absolute_path(path: &Path) -> Result<()> {
    anyhow::ensure!(!path.as_os_str().is_empty(), "path cannot be empty");
    anyhow::ensure!(
        path.is_absolute(),
        "path {} must be absolute",
        path.display()
    );
    let text = path.to_string_lossy();
    anyhow::ensure!(
        !text.contains('\u{fffd}'),
        "path {} is not valid UTF-8",
        path.display()
    );
    for ch in text.chars() {
        // What a shell would interpret, plus whitespace: these paths also
        // land in fstab and unit files, where whitespace ends a field.
        anyhow::ensure!(
            !ch.is_whitespace()
                && !matches!(
                    ch,
                    '\0' | '\''
                        | '"'
                        | '`'
                        | '$'
                        | '\\'
                        | ';'
                        | '&'
                        | '|'
                        | '<'
                        | '>'
                        | '*'
                        | '?'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                ),
            "path {} contains a character a shell or an fstab field would interpret",
            path.display()
        );
    }
    anyhow::ensure!(
        !text.ends_with('/'),
        "path {} must not end with '/': a unit or account path is not a directory reference",
        path.display()
    );
    // `Path::components()` silently folds "." away, so the raw string is
    // checked too: an accepted path must be exactly what the script gets.
    for part in text.split('/') {
        anyhow::ensure!(
            part != "." && part != "..",
            "path {} contains '.' or '..'",
            path.display()
        );
    }
    let mut normal = 0usize;
    for component in path.components() {
        match component {
            PathComponent::RootDir => {}
            PathComponent::Normal(_) => normal += 1,
            PathComponent::CurDir => bail!("path {} contains '.'", path.display()),
            PathComponent::ParentDir => bail!("path {} contains '..'", path.display()),
            PathComponent::Prefix(_) => bail!("path {} has a windows prefix", path.display()),
        }
    }
    anyhow::ensure!(normal > 0, "path {} is the filesystem root", path.display());
    Ok(())
}

/// An IPv4/IPv6 network in CIDR form, kept as parsed values so the
/// firewall text is generated, never pasted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Cidr {
    address: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(text: &str) -> Result<Self> {
        let (address, prefix) = text.split_once('/').with_context(|| {
            format!("network {text:?} needs a prefix length, as ADDRESS/PREFIX")
        })?;
        let address: IpAddr = address
            .parse()
            .with_context(|| format!("network {text:?} has an invalid address"))?;
        let prefix: u8 = prefix
            .parse()
            .with_context(|| format!("network {text:?} has an invalid prefix length"))?;
        let max = if address.is_ipv4() { 32 } else { 128 };
        anyhow::ensure!(
            prefix <= max,
            "network {text:?} has a prefix longer than {max}"
        );
        Ok(Self { address, prefix })
    }
    pub fn address(&self) -> IpAddr {
        self.address
    }
    pub fn prefix(&self) -> u8 {
        self.prefix
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

/// A container image fixed to a digest. The only constructor either
/// refuses the reference or takes the digest; there is no way to build
/// an [`Image`] without one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PinnedImage {
    /// Repository and optional tag, without the digest.
    pub source: String,
    /// `sha256:` followed by 64 lowercase hex digits.
    pub digest: String,
}

impl PinnedImage {
    pub fn new(source: &str, digest: &str) -> Result<Self> {
        let image = Self {
            source: source.to_string(),
            digest: digest.to_string(),
        };
        image.validate()?;
        Ok(image)
    }

    fn validate(&self) -> Result<()> {
        let source = self.source.as_str();
        anyhow::ensure!(!source.is_empty(), "an image needs a repository");
        anyhow::ensure!(
            !source.contains('@'),
            "image {source:?} carries its digest in the source; pass it to the digest argument"
        );
        anyhow::ensure!(
            !source
                .chars()
                .any(|c| c.is_whitespace()
                    || matches!(c, '\'' | '"' | '`' | '$' | '\\' | ';' | '\n')),
            "image {source:?} contains a character a shell would interpret"
        );
        let (repository, tag) = match source.rsplit_once(':') {
            // A colon after the last slash is a tag; a colon in a
            // registry host:port is not.
            Some((repo, tag)) if !tag.contains('/') => (repo, Some(tag)),
            _ => (source, None),
        };
        anyhow::ensure!(
            !repository.is_empty(),
            "image {source:?} has no repository before its tag"
        );
        let local = repository.starts_with("localhost/")
            || repository.starts_with("localhost:")
            || !repository.contains('/')
            || !repository.contains('.');
        if let Some(tag) = tag {
            // Tags that follow a moving build: an installer that
            // accepted them would pin the digest and then pull a
            // different image on the next run.
            anyhow::ensure!(
                !matches!(
                    tag,
                    "current" | "latest" | "dev" | "main" | "master" | "edge" | "nightly"
                ),
                "image {source:?} uses the moving tag {tag:?}: name a version and pin a digest"
            );
        }
        anyhow::ensure!(
            !local || tag.is_some(),
            "local image {source:?} has no tag: a remote image needs a registry-qualified name"
        );
        let hex = self.digest.strip_prefix("sha256:").with_context(|| {
            format!("image {source:?} needs a digest of the form sha256:<64 hex>")
        })?;
        anyhow::ensure!(
            hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "image {source:?} digest {:?} must be 64 lowercase hex digits",
            self.digest
        );
        Ok(())
    }

    /// What `podman pull` is given, and what a quadlet's `Image=` says.
    pub fn reference(&self) -> String {
        format!("{}@{}", self.source, self.digest)
    }
}

/// One loginless system account that runs containers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RootlessUser {
    /// Account name. Lowercase, starts with a letter or underscore.
    pub name: String,
    /// Home directory; container storage lives under it. Absolute.
    pub home: PathBuf,
    /// True to give the account a subuid/subgid range and linger, which
    /// rootless Podman needs for a user manager that outlives a login.
    pub rootless: bool,
    /// True for a system account (`useradd --system`); false to modify a
    /// pre-existing login account.
    pub system: bool,
    /// An extra group (the gateway's, for the socket directory gate).
    pub group: Option<String>,
}

impl RootlessUser {
    pub fn system(name: &str, home: PathBuf) -> Self {
        Self {
            name: name.to_string(),
            home,
            rootless: true,
            system: true,
            group: None,
        }
    }

    fn validate(&self, role: &str) -> Result<()> {
        anyhow::ensure!(
            valid_account_name(&self.name),
            "{role} user {:?} is not a valid account name: lowercase letters, digits, '-' or '_', not root",
            self.name
        );
        valid_absolute_path(&self.home)
            .with_context(|| format!("{role} user {} home", self.name))?;
        if let Some(group) = &self.group {
            anyhow::ensure!(
                valid_account_name(group),
                "{role} user {} group {group:?} is not a valid group name",
                self.name
            );
        }
        Ok(())
    }
}

/// Where container storage lives, and whether a separate filesystem is
/// bind-mounted to it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StorageOptions {
    /// The root every rootless user's home sits under.
    pub root: PathBuf,
    /// A mounted filesystem to bind onto `root` (nothing is formatted or
    /// partitioned by this plan).
    pub source: Option<PathBuf>,
    /// The mount unit that must be up before the bind, for
    /// `x-systemd.requires-mounts-for=`. A subdirectory's mountpoint
    /// cannot be derived from its path, so this is stated, not guessed.
    pub requires_mounts_for: Option<PathBuf>,
    /// Emit the fstab entry and mount it. Without this the plan only
    /// checks that `root` exists and is a mountpoint when a source is
    /// given.
    pub bind: bool,
    /// Mode of the root directory.
    pub mode: u32,
    /// Permission bits of each user's home.
    pub home_mode: u32,
}

impl StorageOptions {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            source: None,
            requires_mounts_for: None,
            bind: false,
            mode: 0o755,
            home_mode: 0o700,
        }
    }
}

/// The sandbox user's Podman API socket: where it listens, and who may
/// reach it. The directory is the gate; the socket itself is 0666 inside.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SocketOptions {
    pub dir: PathBuf,
    /// The system account whose containers the socket serves.
    pub user: String,
    /// The group that must own the directory. The gateway's primary group
    /// stays mapped in its own keep-id container, so it can reach the
    /// socket and no other account can.
    pub group: String,
    pub dir_mode: u32,
    pub socket_mode: u32,
}

/// Whether the script installs missing packages, and which manager.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PackageOptions {
    pub manager: PackageManager,
    /// False: the script only verifies the binaries and stops with a
    /// message naming what to install.
    pub install_missing: bool,
}

impl Default for PackageOptions {
    fn default() -> Self {
        Self {
            manager: PackageManager::Apt,
            install_missing: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageManager {
    Apt,
    Dnf,
    Pacman,
}

impl PackageManager {
    fn install_command(self, packages: &[String]) -> Option<String> {
        if packages.is_empty() {
            return None;
        }
        let list = packages.iter().map(|p| sq(p)).collect::<Vec<_>>().join(" ");
        Some(match self {
            Self::Apt => format!(
                "DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends {list}"
            ),
            Self::Dnf => format!("dnf install -y {list}"),
            Self::Pacman => format!("pacman -S --noconfirm --needed {list}"),
        })
    }
}

/// A Tor client instance. The `sandbox` instance is the one the firewall
/// redirects a sandbox's IPv4 TCP into; the optional `web` instance is a
/// separate SOCKS port for fetches made on someone else's behalf.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TorInstance {
    /// Instance name. The distribution's `tor-instance-create` skips any
    /// name that is not alphanumeric, so this is restricted to that.
    pub name: String,
    /// TransPort the firewall sends redirected connections to.
    pub transport_port: Option<SocketAddr>,
    /// SOCKS port for callers that ask for one explicitly.
    pub socks_port: Option<SocketAddr>,
    /// Give each SOCKS user name a circuit of its own.
    pub isolate_socks_auth: bool,
    /// Order this instance after an interface's unit (a WireGuard device).
    pub requires_interface: Option<String>,
}

impl TorInstance {
    fn validate(&self, role: &str) -> Result<()> {
        anyhow::ensure!(
            !self.name.is_empty()
                && self.name.len() <= 32
                && self.name.bytes().all(|b| b.is_ascii_alphanumeric()),
            "{role} Tor instance name {:?} must be alphanumeric (tor-instance-create skips any other)",
            self.name
        );
        anyhow::ensure!(
            self.transport_port.is_some() || self.socks_port.is_some(),
            "{role} Tor instance {} has no port: set a transport port (to carry redirected connections) or a SOCKS port",
            self.name
        );
        if let Some(interface) = &self.requires_interface {
            anyhow::ensure!(
                valid_interface_name(interface),
                "{role} Tor instance {} requires interface {interface:?}, which is not a network interface name",
                self.name
            );
        }
        if let Some(port) = self.socks_port {
            anyhow::ensure!(
                self.isolate_socks_auth,
                "{role} Tor SOCKS port {} serves several callers without IsolateSOCKSAuth; give each a circuit",
                port
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TorOptions {
    pub sandbox: TorInstance,
    pub web: Option<TorInstance>,
}

impl TorOptions {
    fn validate(&self) -> Result<()> {
        self.sandbox.validate("sandbox")?;
        anyhow::ensure!(
            self.sandbox.transport_port.is_some(),
            "the sandbox Tor instance {} needs a transport port: the firewall redirects IPv4 TCP there",
            self.sandbox.name
        );
        if let Some(web) = &self.web {
            web.validate("web")?;
            anyhow::ensure!(
                web.name != self.sandbox.name,
                "the sandbox and web Tor instances cannot share the name {:?}",
                web.name
            );
        }
        Ok(())
    }
}

fn valid_interface_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// One destination an account may reach.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AllowRule {
    pub address: IpAddr,
    pub ports: Vec<u16>,
    pub tcp: bool,
    pub udp: bool,
}

impl AllowRule {
    fn validate(&self, user: &str) -> Result<()> {
        anyhow::ensure!(
            self.tcp || self.udp,
            "allow rule for {user} names neither tcp nor udp"
        );
        anyhow::ensure!(
            !self.ports.is_empty() && self.ports.iter().all(|p| *p != 0),
            "allow rule for {user} has no valid port"
        );
        anyhow::ensure!(
            !self.address.is_unspecified() && !self.address.is_multicast(),
            "allow rule for {user} names {address}, which is not a unicast destination",
            address = self.address
        );
        Ok(())
    }

    fn render(&self) -> String {
        let address = match self.address {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => v6.to_string(),
        };
        let ports = self
            .ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        if self.tcp && !self.udp {
            format!("ip daddr {address} tcp dport {{ {ports} }} accept")
        } else if self.udp && !self.tcp {
            format!("ip daddr {address} udp dport {{ {ports} }} accept")
        } else {
            format!("ip daddr {address} meta l4proto {{ tcp, udp }} th dport {{ {ports} }} accept")
        }
    }
}

/// What an account may reach once the firewall confines it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Egress {
    /// Local services it is allowed to use, then the internet, but no
    /// private or local address.
    Gateway { allow: Vec<AllowRule> },
    /// No direct egress: IPv4 TCP through Tor, everything else refused.
    Sandbox {
        /// True when the Tor redirect chain is planned for this user.
        tor: bool,
    },
}

impl Egress {
    fn chain(&self) -> &'static str {
        match self {
            Self::Gateway { .. } => "gateway",
            Self::Sandbox { .. } => "sandbox",
        }
    }
}

impl std::fmt::Display for Egress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gateway { allow } => write!(f, "gateway ({} local service rules)", allow.len()),
            Self::Sandbox { tor } => write!(f, "sandbox (tor: {tor})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConfinedUser {
    pub user: String,
    pub egress: Egress,
}

/// The firewall table, as options. Every address and port is data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FirewallOptions {
    /// Table name; the plan owns only `inet <table>`.
    pub table: String,
    pub confined: Vec<ConfinedUser>,
    /// Ports refused from every interface except the peer one, and from
    /// every source outside `peer_source`.
    pub protected_ports: Vec<u16>,
    /// The interface A2A peers arrive on (`wg0` on a WireGuard mesh).
    pub peer_interface: Option<String>,
    /// The source network those peers come from.
    pub peer_source: Option<Cidr>,
}

impl FirewallOptions {
    fn validate(&self, tor: Option<&TorOptions>) -> Result<()> {
        anyhow::ensure!(
            !self.table.is_empty()
                && self.table.len() <= 32
                && self.table.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'_'
                    || b == b'-'),
            "firewall table name {:?} must be lowercase letters, digits, '-' or '_'",
            self.table
        );
        anyhow::ensure!(
            !self.confined.is_empty(),
            "the firewall confines nobody: give at least one confined user, or turn the firewall off"
        );
        let mut seen = BTreeSet::new();
        for entry in &self.confined {
            anyhow::ensure!(
                valid_account_name(&entry.user),
                "confined user {:?} is not a valid account name",
                entry.user
            );
            anyhow::ensure!(
                seen.insert(entry.user.clone()),
                "user {} is confined twice",
                entry.user
            );
            match &entry.egress {
                Egress::Gateway { allow } => {
                    for rule in allow {
                        rule.validate(&entry.user)?;
                    }
                }
                Egress::Sandbox { tor: wants_tor } => {
                    anyhow::ensure!(
                        !*wants_tor || tor.is_some(),
                        "user {} is redirected into Tor, but Tor is off; enable Tor or set tor: false",
                        entry.user
                    );
                }
            }
        }
        anyhow::ensure!(
            self.peer_interface.is_some() == self.peer_source.is_some(),
            "a peer allow rule needs both an interface and a source network: set peer_interface and peer_source, or neither"
        );
        if let Some(interface) = &self.peer_interface {
            anyhow::ensure!(
                valid_interface_name(interface),
                "peer interface {interface:?} is not a network interface name"
            );
        }
        anyhow::ensure!(
            self.protected_ports.iter().all(|p| *p != 0),
            "protected port 0 is not a port"
        );
        Ok(())
    }
}

/// Description of one container, rendered into a quadlet.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerSpec {
    pub description: String,
    pub image: PinnedImage,
    /// Extra quadlet keys, already in `Key=value` form. Refused if they
    /// try to set `Image=`, a command, or a unit-level key that would
    /// escape the container.
    pub extra: Vec<String>,
}

impl ContainerSpec {
    const FORBIDDEN_KEYS: &'static [&'static str] = &[
        "Image",
        "Exec",
        "Entrypoint",
        "RunInit",
        "PodmanArgs",
        "User",
        "Group",
        "Network",
        "Secret",
        "Pull",
    ];

    fn validate(&self, user: &str, name: &str) -> Result<()> {
        anyhow::ensure!(
            !self.description.is_empty() && !self.description.contains(['\n', '\r']),
            "container {name} for {user} needs a one-line description"
        );
        for line in &self.extra {
            let (key, value) = line.split_once('=').with_context(|| {
                format!("container {name} for {user} has {line:?}, which is not Key=value")
            })?;
            anyhow::ensure!(
                !key.is_empty()
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "container {name} for {user} has an invalid quadlet key {key:?}"
            );
            anyhow::ensure!(
                !value.contains('\n') && !value.contains('\r'),
                "container {name} for {user} sets {key}= over several lines"
            );
            anyhow::ensure!(
                !Self::FORBIDDEN_KEYS.iter().any(|k| k.eq_ignore_ascii_case(key)),
                "container {name} for {user} sets {key}=; the plan owns that key (the image is pinned by the typed field)",
            );
        }
        Ok(())
    }

    /// `Network=` is owned by the plan (refused in `extra`): every
    /// quadlet joins the host's shared network when one is planned, so the
    /// gateway reaches a PDS or other container by name.
    fn render(&self, name: &str, network: Option<&str>) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "[Unit]");
        let _ = writeln!(out, "Description={}", self.description);
        let _ = writeln!(out);
        let _ = writeln!(out, "[Container]");
        let _ = writeln!(out, "Image={}", self.image.reference());
        // The image is pulled by digest in the reviewed script; a start
        // must never pull a different one.
        let _ = writeln!(out, "Pull=never");
        if let Some(network) = network {
            let _ = writeln!(out, "Network={network}");
        }
        for line in &self.extra {
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "[Service]");
        let _ = writeln!(out, "Restart=on-failure");
        // A first start may still pull layers into a fresh id map.
        let _ = writeln!(out, "TimeoutStartSec=900");
        let _ = writeln!(out);
        let _ = writeln!(out, "[Install]");
        let _ = writeln!(out, "WantedBy=default.target");
        let _ = name;
        out
    }
}

/// A container of one account, installed as a quadlet under that
/// account's home.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QuadletSpec {
    pub user: String,
    /// Unit stem, without the `.container` suffix.
    pub name: String,
    pub container: ContainerSpec,
}

/// An image to pull into one account's store before its quadlet starts.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PullRequest {
    pub user: String,
    pub image: PinnedImage,
}

/// The root helper that turns a queued peer list into interface config, if
/// the operator has one. It is installed disabled.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PeerSyncOptions {
    /// Unit stem, e.g. `veles-wg-peers`.
    pub unit_name: String,
    /// Absolute path of the helper script the path unit runs.
    pub script: PathBuf,
    /// The directory a privileged writer may queue requests in.
    pub queue: PathBuf,
    /// The account the queue belongs to.
    pub owner: String,
    /// The interface the helper applies peers to.
    pub interface: String,
}

/// Everything the profile can be told to do.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerHostOptions {
    pub gateway: RootlessUser,
    pub sandbox: RootlessUser,
    /// More accounts that run containers, beyond the gateway and sandbox.
    pub extra_users: Vec<RootlessUser>,
    pub storage: StorageOptions,
    pub socket: SocketOptions,
    /// Prefix for every unit and file the plan installs.
    pub unit_prefix: String,
    pub packages: PackageOptions,
    pub images: Vec<PullRequest>,
    pub quadlets: Vec<QuadletSpec>,
    pub tor: Option<TorOptions>,
    pub firewall: Option<FirewallOptions>,
    /// Network name rootless containers share (`pasta` has no bridge).
    pub network: Option<String>,
    pub peer_sync: Option<PeerSyncOptions>,
}

impl ContainerHostOptions {
    /// A plan with the container engine and the rootless plumbing only:
    /// no Tor, no firewall, no images, no quadlets. Every optional piece
    /// is off until an option turns it on.
    pub fn new(storage: StorageOptions, gateway: RootlessUser, sandbox: RootlessUser) -> Self {
        Self {
            socket: SocketOptions {
                dir: PathBuf::from(format!("/run/{}", sandbox.name)),
                user: sandbox.name.clone(),
                group: gateway.name.clone(),
                dir_mode: 0o750,
                socket_mode: 0o666,
            },
            unit_prefix: "veles".into(),
            gateway,
            sandbox,
            extra_users: Vec::new(),
            storage,
            packages: PackageOptions::default(),
            images: Vec::new(),
            quadlets: Vec::new(),
            tor: None,
            firewall: None,
            network: None,
            peer_sync: None,
        }
    }

    /// Every account the plan manages, gateway first.
    pub fn users(&self) -> Vec<&RootlessUser> {
        let mut out = vec![&self.gateway, &self.sandbox];
        out.extend(self.extra_users.iter());
        out
    }

    fn user(&self, name: &str) -> Option<&RootlessUser> {
        self.users().into_iter().find(|u| u.name == name)
    }

    /// Validate every option. `plan()` calls this, so a caller cannot
    /// render a script from a struct it built by hand.
    pub fn validate(&self) -> Result<()> {
        self.gateway.validate("gateway")?;
        self.sandbox.validate("sandbox")?;
        anyhow::ensure!(
            self.gateway.name != self.sandbox.name,
            "the gateway and sandbox accounts cannot both be {:?}",
            self.gateway.name
        );
        for extra in &self.extra_users {
            extra.validate("extra")?;
            anyhow::ensure!(
                self.users().iter().filter(|u| u.name == extra.name).count() == 1,
                "account {:?} is listed twice",
                extra.name
            );
        }

        valid_absolute_path(&self.storage.root).context("storage root")?;
        anyhow::ensure!(
            self.storage.mode & !0o7777 == 0,
            "storage root mode {:o} is not a permission mode",
            self.storage.mode
        );
        anyhow::ensure!(
            self.storage.home_mode & !0o7777 == 0,
            "storage home mode {:o} is not a permission mode",
            self.storage.home_mode
        );
        match (&self.storage.source, self.storage.bind) {
            (Some(source), _) => valid_absolute_path(source).context("storage source")?,
            (None, true) => bail!("storage.bind is set but there is no storage.source to bind"),
            (None, false) => {}
        }
        if let (Some(source), root) = (&self.storage.source, &self.storage.root) {
            anyhow::ensure!(
                source != root,
                "storage source and root are both {}: nothing to bind",
                root.display()
            );
        }
        for user in self.users() {
            // The bind mount reads the whole source; a home on another
            // filesystem would be mounted over by it or invisible.
            anyhow::ensure!(
                user.home.starts_with(&self.storage.root),
                "{} home {} is not under the storage root {}",
                user.name,
                user.home.display(),
                self.storage.root.display()
            );
        }

        anyhow::ensure!(
            !self.unit_prefix.is_empty()
                && self.unit_prefix.len() <= 32
                && self
                    .unit_prefix
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "unit prefix {:?} must be lowercase letters, digits or '-'",
            self.unit_prefix
        );

        anyhow::ensure!(
            valid_account_name(&self.socket.user),
            "socket user {:?} is not a valid account name",
            self.socket.user
        );
        anyhow::ensure!(
            self.socket.user == self.sandbox.name,
            "socket user {} is not the sandbox account {}; the socket serves the sandbox store",
            self.socket.user,
            self.sandbox.name
        );
        anyhow::ensure!(
            valid_account_name(&self.socket.group),
            "socket group {:?} is not a valid group name",
            self.socket.group
        );
        anyhow::ensure!(
            self.socket.group != self.socket.user,
            "socket directory group {} is the sandbox's own group: the gateway could not reach it",
            self.socket.group
        );
        valid_absolute_path(&self.socket.dir).context("socket directory")?;
        anyhow::ensure!(
            self.socket.dir.starts_with("/run"),
            "socket directory {} must be under /run (it is a tmpfs recreated at boot)",
            self.socket.dir.display()
        );
        for (label, mode) in [
            ("socket directory", self.socket.dir_mode),
            ("socket", self.socket.socket_mode),
        ] {
            anyhow::ensure!(
                mode & !0o7777 == 0,
                "{label} mode {mode:o} is not a permission mode"
            );
        }

        if let Some(network) = &self.network {
            anyhow::ensure!(
                !network.is_empty()
                    && network.len() <= 32
                    && network
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "network name {network:?} is not a container network name"
            );
        }

        if let Some(tor) = &self.tor {
            tor.validate()?;
        }
        if let Some(firewall) = &self.firewall {
            firewall.validate(self.tor.as_ref())?;
            for entry in &firewall.confined {
                // A rule about an account nobody creates is a typo.
                anyhow::ensure!(
                    self.user(&entry.user).is_some(),
                    "the firewall names user {}, which this plan does not create",
                    entry.user
                );
            }
        }

        for request in &self.images {
            request.image.validate()?;
            anyhow::ensure!(
                self.user(&request.user).is_some(),
                "image {} is pulled for user {}, which this plan does not create",
                request.image.source,
                request.user
            );
        }
        let mut seen = BTreeSet::new();
        for quadlet in &self.quadlets {
            let user = self.user(&quadlet.user).with_context(|| {
                format!(
                    "quadlet {} is installed for user {}, which this plan does not create",
                    quadlet.name, quadlet.user
                )
            })?;
            anyhow::ensure!(
                !quadlet.name.is_empty()
                    && quadlet.name.len() <= 64
                    && quadlet
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "quadlet name {:?} must be letters, digits, '-' or '_'",
                quadlet.name
            );
            anyhow::ensure!(
                seen.insert((quadlet.user.clone(), quadlet.name.clone())),
                "user {} has two quadlets named {}",
                quadlet.user,
                quadlet.name
            );
            quadlet.container.validate(&user.name, &quadlet.name)?;
        }

        if let Some(peer) = &self.peer_sync {
            anyhow::ensure!(
                !peer.unit_name.is_empty()
                    && peer
                        .unit_name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "peer sync unit name {:?} is not a unit name",
                peer.unit_name
            );
            valid_absolute_path(&peer.script).context("peer sync script")?;
            valid_absolute_path(&peer.queue).context("peer sync queue")?;
            anyhow::ensure!(
                valid_account_name(&peer.owner),
                "peer sync owner {:?} is not a valid account name",
                peer.owner
            );
            anyhow::ensure!(
                valid_interface_name(&peer.interface),
                "peer sync interface {:?} is not an interface name",
                peer.interface
            );
            anyhow::ensure!(
                self.user(&peer.owner).is_some(),
                "peer sync owner {} is not an account this plan creates",
                peer.owner
            );
            anyhow::ensure!(
                self.firewall.as_ref().and_then(|f| f.peer_interface.as_deref())
                    == Some(peer.interface.as_str()),
                "peer sync writes to interface {}, which the firewall does not name as its peer interface",
                peer.interface
            );
        }
        Ok(())
    }

    /// The packages the plan needs, given what is enabled.
    pub fn packages(&self) -> Vec<String> {
        let mut out = vec!["podman".to_string(), "shadow".to_string()];
        if self.tor.is_some() {
            out.push("tor".into());
        }
        if self.firewall.is_some() {
            out.push("nftables".into());
        }
        out
    }

    /// Build the plan. Pure: no filesystem, no network, no process.
    pub fn plan(&self) -> Result<ContainerHostPlan> {
        self.validate()?;

        let mut files: Vec<PlannedFile> = Vec::new();
        let mut quadlets: Vec<PlannedQuadlet> = Vec::new();

        // The socket's tmpfiles entry: its directory is the gate.
        files.push(PlannedFile::root(
            PathBuf::from("/etc/tmpfiles.d").join(format!("{}.conf", self.unit_prefix)),
            format!(
                "# {prefix} → /etc/tmpfiles.d/{prefix}.conf\n\
                 #\n\
                 # The {socket_user} Podman API socket's directory. Its group is {group},\n\
                 # which stays mapped inside a keep-id container running as {gw}, so that\n\
                 # account reaches the socket and no other one does.\n\
                 d {dir} {dir_mode:04o} {socket_user} {group} -\n",
                prefix = self.unit_prefix,
                socket_user = self.socket.user,
                group = self.socket.group,
                gw = self.gateway.name,
                dir = self.socket.dir.display(),
                dir_mode = self.socket.dir_mode,
            ),
            Some(format!(
                "{socket_user}:{group}",
                socket_user = self.socket.user,
                group = self.socket.group
            )),
        ));

        // Each rootless account's podman socket service and socket unit.
        let socket_files = self.socket_units();
        files.extend(socket_files);

        // The firewall, its unit and the user-manager drop-in.
        let mut firewall_file = None;
        if let Some(firewall) = &self.firewall {
            let path = PathBuf::from(format!("/etc/{}/veles.nft", self.unit_prefix));
            let contents = self.render_nft(firewall);
            let file = PlannedFile {
                path,
                contents,
                mode: 0o644,
                owner: Some("root:root".into()),
            };
            files.push(file.clone());
            firewall_file = Some(file);
            files.push(self.nft_service());
        }
        if self.firewall.is_some() {
            // The user managers must not start containers unconfined.
            files.push(self.user_manager_dropin());
        }

        // Tor instances and their drop-ins.
        if let Some(tor) = &self.tor {
            for instance in std::iter::once(&tor.sandbox).chain(tor.web.iter()) {
                files.push(PlannedFile {
                    path: PathBuf::from("/etc/tor/instances")
                        .join(&instance.name)
                        .join("torrc"),
                    contents: render_torrc(instance),
                    mode: 0o644,
                    owner: Some("root:root".into()),
                });
                if instance.requires_interface.is_some() {
                    files.push(PlannedFile {
                        path: PathBuf::from("/etc/systemd/system")
                            .join(format!("tor@{}.service.d", instance.name))
                            .join(format!("{}.conf", self.unit_prefix)),
                        contents: render_tor_dropin(instance),
                        mode: 0o644,
                        owner: Some("root:root".into()),
                    });
                }
            }
        }

        // Quadlets and their account's quadlet directory.
        for quadlet in &self.quadlets {
            let user = self
                .user(&quadlet.user)
                .expect("validated: every quadlet has a user");
            let path = user
                .home
                .join(".config/containers/systemd")
                .join(format!("{}.container", quadlet.name));
            quadlets.push(PlannedQuadlet {
                path,
                contents: quadlet
                    .container
                    .render(&quadlet.name, self.network.as_deref()),
                user: user.name.clone(),
            });
        }

        // The root helper for peer sync, if the operator has one.
        if let Some(peer) = &self.peer_sync {
            files.push(PlannedFile::root(
                PathBuf::from("/etc/systemd/system").join(format!("{}.path", peer.unit_name)),
                render_peer_path_unit(peer),
                None,
            ));
            files.push(PlannedFile::root(
                PathBuf::from("/etc/systemd/system").join(format!("{}.service", peer.unit_name)),
                render_peer_service_unit(peer),
                None,
            ));
        }

        let images = self.images.clone();
        let network = self.network.clone();
        let script = self.render_script(&files, &quadlets, &images, &network);
        let script_sha256 = digest_hex(script.as_bytes());

        Ok(ContainerHostPlan {
            scope: Scope::of(self),
            files,
            quadlets,
            images,
            packages: if self.packages.install_missing {
                self.packages()
            } else {
                Vec::new()
            },
            tor: self.tor.clone(),
            firewall: firewall_file,
            script,
            script_sha256,
        })
    }

    fn socket_units(&self) -> Vec<PlannedFile> {
        let socket_name = format!("{}-podman.socket", self.socket.user);
        let service_name = format!("{}-podman.service", self.socket.user);
        let dir = self.socket.dir.join("podman.sock");
        let root = self
            .user(&self.socket.user)
            .map(|u| u.home.join(".config/systemd/user"))
            .unwrap_or_default();
        vec![
            PlannedFile {
                path: root.join(&socket_name),
                contents: format!(
                    "# {name} — the {user} account's rootless Podman API socket.\n\
                     # World-writable on its own; the {dir_mode:04o} directory around it\n\
                     # (tmpfiles) is the gate.\n\
                     [Unit]\n\
                     Description={prefix} containers: podman API socket for {user}\n\
                     \n\
                     [Socket]\n\
                     ListenStream={socket}\n\
                     SocketMode={socket_mode:04o}\n\
                     \n\
                     [Install]\n\
                     WantedBy=sockets.target\n",
                    name = socket_name,
                    user = self.socket.user,
                    dir_mode = self.socket.dir_mode,
                    prefix = self.unit_prefix,
                    socket = dir.display(),
                    socket_mode = self.socket.socket_mode,
                ),
                mode: 0o644,
                owner: Some(format!("{}:{}", self.socket.user, self.socket.group)),
            },
            PlannedFile {
                path: root.join(&service_name),
                contents: render_podman_service(
                    &format!(
                        "{} containers: podman API service for {}",
                        self.unit_prefix, self.socket.user
                    ),
                    &socket_name,
                ),
                mode: 0o644,
                owner: Some(format!("{}:{}", self.socket.user, self.socket.group)),
            },
        ]
    }

    fn nft_service(&self) -> PlannedFile {
        PlannedFile::root(
            PathBuf::from("/etc/systemd/system").join(format!("{}-nft.service", self.unit_prefix)),
            render_nft_service(
                &self.unit_prefix,
                self.firewall
                    .as_ref()
                    .map(|f| f.table.as_str())
                    .unwrap_or(""),
                &self.firewall_path(),
            ),
            None,
        )
    }

    fn firewall_path(&self) -> PathBuf {
        PathBuf::from(format!("/etc/{}/veles.nft", self.unit_prefix))
    }

    /// Where the user-manager drop-in template is installed.
    fn dropin_path(&self) -> PathBuf {
        PathBuf::from("/etc/systemd/system")
            .join("user@.service.d")
            .join(format!("{}.conf", self.unit_prefix))
    }

    fn user_manager_dropin(&self) -> PlannedFile {
        let dropin = format!(
            "# {prefix} → /etc/systemd/system/user@<uid>.service.d/{prefix}.conf\n\
             #\n\
             # Each container starts with its account's user manager (linger), at\n\
             # boot. The firewall table must be loaded first: without it a sandbox\n\
             # leaves from this host's address and the gateway reaches every local\n\
             # service. When the table is missing the manager (and every container\n\
             # under it) stays DOWN rather than start unconfined.\n\
             #\n\
             # After=/Wants= rather than Requires=, so reloading the table does not\n\
             # restart every container.\n\
             [Unit]\n\
             After={prefix}-nft.service\n\
             Wants={prefix}-nft.service\n\
             {after_iface}\
             \n\
             [Service]\n\
             # Reads the table back: a unit that is not loaded fails here.\n\
             ExecStartPre=+/usr/sbin/nft list table inet {table}\n",
            prefix = self.unit_prefix,
            after_iface = match &self.firewall {
                Some(f) => f
                    .peer_interface
                    .as_ref()
                    .map(|i| format!("After={i}.service\nWants={i}.service\n"))
                    .unwrap_or_default(),
                None => String::new(),
            },
            table = self
                .firewall
                .as_ref()
                .map(|f| f.table.as_str())
                .unwrap_or_default(),
        );
        // A real drop-in for the user managers systemd generates, so the
        // file is a loadable unit fragment and `systemd-analyze verify`
        // has something to check. The script copies it per uid.
        PlannedFile::root(
            PathBuf::from("/etc/systemd/system")
                .join("user@.service.d")
                .join(format!("{}.conf", self.unit_prefix)),
            dropin,
            None,
        )
    }

    fn render_nft(&self, firewall: &FirewallOptions) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "#!/usr/sbin/nft -f");
        let _ = writeln!(
            out,
            "# Generated by veles: what each managed account may reach."
        );
        let _ = writeln!(
            out,
            "# Loaded by {}-nft.service. It owns only `inet {}`: netavark's and\n# fail2ban's tables are left alone.",
            self.unit_prefix, firewall.table
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "table inet {0}\ndelete table inet {0}", firewall.table);
        let _ = writeln!(out);
        let _ = writeln!(out, "table inet {} {{", firewall.table);

        // Address sets.
        let _ = writeln!(out, "\tset private4 {{");
        let _ = writeln!(out, "\t\ttype ipv4_addr");
        let _ = writeln!(out, "\t\tflags interval");
        let _ = writeln!(
            out,
            "\t\telements = {{ 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16,"
        );
        let _ = writeln!(
            out,
            "\t\t\t     172.16.0.0/12, 192.168.0.0/16, 224.0.0.0/4 }}"
        );
        let _ = writeln!(out, "\t}}");
        let _ = writeln!(out);
        let _ = writeln!(out, "\tset private6 {{");
        let _ = writeln!(out, "\t\ttype ipv6_addr");
        let _ = writeln!(out, "\t\tflags interval");
        let _ = writeln!(
            out,
            "\t\telements = {{ ::1/128, fc00::/7, fe80::/10, ff00::/8 }}"
        );
        let _ = writeln!(out, "\t}}");
        let _ = writeln!(out);

        // The output chain.
        let names = firewall
            .confined
            .iter()
            .map(|c| format!("\"{}\"", c.user))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "\tchain out {{");
        let _ = writeln!(
            out,
            "\t\ttype filter hook output priority filter; policy accept;"
        );
        let _ = writeln!(out, "\t\tct state established,related accept");
        let _ = writeln!(out, "\t\tmeta skuid {{ {names} }} jump confined");
        let _ = writeln!(out, "\t}}");
        let _ = writeln!(out);

        let _ = writeln!(out, "\tchain confined {{");
        let _ = writeln!(
            out,
            "\t\t# The host's resolver: pasta forwards a container's queries to it."
        );
        let _ = writeln!(
            out,
            "\t\tip daddr {{ 127.0.0.53, 127.0.0.54 }} meta l4proto {{ tcp, udp }} th dport 53 accept"
        );
        for entry in &firewall.confined {
            let _ = writeln!(
                out,
                "\t\tmeta skuid \"{}\" jump {}",
                entry.user,
                entry.egress.chain()
            );
        }
        let _ = writeln!(out, "\t\tfib daddr type local drop");
        let _ = writeln!(out, "\t\tip daddr @private4 drop");
        let _ = writeln!(out, "\t\tip6 daddr @private6 drop");
        let _ = writeln!(out, "\t}}");
        let _ = writeln!(out);

        for entry in &firewall.confined {
            match &entry.egress {
                Egress::Gateway { allow } => {
                    let _ = writeln!(out, "\tchain gateway {{");
                    let _ = writeln!(
                        out,
                        "\t\t# {}: local services, then the internet; no private address.",
                        entry.user
                    );
                    for rule in allow {
                        let _ = writeln!(out, "\t\t{}", rule.render());
                    }
                    let _ = writeln!(out, "\t\tfib daddr type local drop");
                    let _ = writeln!(out, "\t\tip daddr @private4 drop");
                    let _ = writeln!(out, "\t\tip6 daddr @private6 drop");
                    let _ = writeln!(out, "\t}}");
                    let _ = writeln!(out);
                }
                Egress::Sandbox { .. } => {}
            }
        }

        // The Tor redirect, when any sandbox asks for it.
        let tor_users: Vec<&str> = firewall
            .confined
            .iter()
            .filter(|c| matches!(c.egress, Egress::Sandbox { tor: true }))
            .map(|c| c.user.as_str())
            .collect();
        let transport = self
            .tor
            .as_ref()
            .and_then(|t| t.sandbox.transport_port)
            .map(|p| p.port())
            .unwrap_or(0);
        if !tor_users.is_empty() && transport != 0 {
            let list = tor_users
                .iter()
                .map(|u| format!("\"{u}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                out,
                "\t# Sandboxes leave through Tor or not at all: every IPv4 TCP\n\
                 \t# connection they open goes to the local TransPort, which learns the\n\
                 \t# real destination from conntrack. Local and private addresses are\n\
                 \t# excluded here and refused by the sandbox chain below."
            );
            let _ = writeln!(
                out,
                "\tchain sbx_tor {{\n\t\ttype nat hook output priority dstnat; policy accept;"
            );
            let _ = writeln!(out, "\t\tmeta skuid {{ {list} }} jump to_tor\n\t}}");
            let _ = writeln!(out);
            let _ = writeln!(out, "\tchain to_tor {{");
            let _ = writeln!(out, "\t\tmeta nfproto != ipv4 return");
            let _ = writeln!(out, "\t\tfib daddr type local return");
            let _ = writeln!(out, "\t\tip daddr @private4 return");
            let _ = writeln!(out, "\t\tmeta l4proto tcp redirect to :{transport}");
            let _ = writeln!(out, "\t}}");
            let _ = writeln!(out);

            let _ = writeln!(out, "\tchain sandbox {{");
            let _ = writeln!(
                out,
                "\t\tip daddr 127.0.0.1 tcp dport {transport} ct status dnat accept"
            );
            let _ = writeln!(
                out,
                "\t\t# Everything else: UDP, ICMP, IPv6, local and private addresses.\n\
                 \t\t# Refused rather than dropped, so a client falls back at once\n\
                 \t\t# (IPv6 to IPv4, QUIC to TCP) instead of timing out."
            );
            let _ = writeln!(out, "\t\tmeta l4proto tcp reject with tcp reset");
            let _ = writeln!(out, "\t\treject with icmpx admin-prohibited");
            let _ = writeln!(out, "\t}}");
            let _ = writeln!(out);
        } else if !tor_users.is_empty() {
            // validate() refuses this; keep the rendered text closed anyway.
            let _ = writeln!(out, "\t# sandbox users confined with no transport port");
        }

        // The input chain: published ports, peers only from their own net.
        let _ = writeln!(out, "\tchain in {{");
        let _ = writeln!(
            out,
            "\t\ttype filter hook input priority filter; policy accept;"
        );
        let _ = writeln!(out, "\t\tiifname \"lo\" accept");
        if let (Some(interface), Some(source)) = (&firewall.peer_interface, &firewall.peer_source) {
            let _ = writeln!(
                out,
                "\t\t# Peers arrive on {interface} from {source}; nothing else may use these."
            );
            if !firewall.protected_ports.is_empty() {
                let ports = firewall
                    .protected_ports
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(
                    out,
                    "\t\ttcp dport {{ {ports} }} iifname != \"{interface}\" drop"
                );
                let _ = writeln!(out, "\t\ttcp dport {{ {ports} }} ip saddr != {source} drop");
            }
        } else if !firewall.protected_ports.is_empty() {
            let ports = firewall
                .protected_ports
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "\t\ttcp dport {{ {ports} }} drop");
        }
        if let Some(web) = self.tor.as_ref().and_then(|t| t.web.as_ref()) {
            if let Some(socks) = web.socks_port {
                let _ = writeln!(
                    out,
                    "\t\t# The web Tor's SOCKS port is for this host's gateway only."
                );
                let _ = writeln!(out, "\t\ttcp dport {{ {} }} drop", socks.port());
            }
        }
        let _ = writeln!(out, "\t}}");
        let _ = writeln!(out, "}}");
        out
    }

    fn render_script(
        &self,
        files: &[PlannedFile],
        quadlets: &[PlannedQuadlet],
        images: &[PullRequest],
        network: &Option<String>,
    ) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "#!/bin/sh");
        let _ = writeln!(
            s,
            "# Generated by veles: the privileged half of a container-host install."
        );
        let _ = writeln!(
            s,
            "#\n# Read it before running it. It creates system accounts, container storage,\n\
             # subuid ranges, linger, the Podman API socket, and (if enabled) Tor and the\n\
             # firewall table. It never formats a disk, changes DNS or touches a table it\n\
             # does not own."
        );
        let _ = writeln!(s);
        let _ = writeln!(s, "set -eu");
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "die() {{ echo \"veles container-host: $*\" >&2; exit 1; }}"
        );
        let _ = writeln!(
            s,
            "[ \"$(id -u)\" = 0 ] || die 'run this script as root (sudo)'"
        );
        let _ = writeln!(s);

        // Constants, quoted once.
        let _ = writeln!(s, "# ── options ──");
        let _ = writeln!(s, "storage_root={}", sq_path(&self.storage.root));
        let _ = writeln!(s, "storage_mode={:04o}", self.storage.mode);
        let _ = writeln!(s, "home_mode={:04o}", self.storage.home_mode);
        let _ = writeln!(s, "prefix={}", sq(&self.unit_prefix));
        if let Some(source) = &self.storage.source {
            let _ = writeln!(s, "storage_source={}", sq_path(source));
            let _ = writeln!(s, "storage_bind={}", if self.storage.bind { 1 } else { 0 });
        }
        let _ = writeln!(s);

        // ── packages ──────────────────────────────────────────────────
        let packages = self.packages();
        if self.packages.install_missing {
            if let Some(command) = self.packages.manager.install_command(&packages) {
                let _ = writeln!(s, "# ── packages ──");
                let _ = writeln!(s, "{command}");
                let _ = writeln!(s);
            }
        } else {
            let _ = writeln!(s, "# ── packages (not installed: verify only) ──");
            for (binary, package) in [
                ("podman", "podman"),
                ("usermod", "shadow (or passwd)"),
                ("nft", "nftables"),
                ("tor", "tor"),
                ("tor-instance-create", "tor"),
            ] {
                if package == "tor" && self.tor.is_none() {
                    continue;
                }
                if package == "nftables" && self.firewall.is_none() {
                    continue;
                }
                let _ = writeln!(
                    s,
                    "command -v {bin} >/dev/null 2>&1 || die {msg}",
                    bin = sq(binary),
                    msg = sq(&format!(
                        "{binary} is missing; install the {package} package (or allow this plan to install it) and re-run"
                    )),
                );
            }
            let _ = writeln!(s);
        }

        // ── storage ──────────────────────────────────────────────────
        let _ = writeln!(s, "# ── storage ──");
        if let (Some(source), root) = (&self.storage.source, &self.storage.root) {
            if self.storage.bind {
                let _ = writeln!(
                    s,
                    "install -d -m {mode:04o} \"$storage_source\" \"$storage_root\"",
                    mode = self.storage.mode
                );
                // The mount's own unit must be up first; naming the path
                // here is the operator's option, because a subdirectory's
                // mountpoint cannot be derived from the path alone.
                let requires = self
                    .storage
                    .requires_mounts_for
                    .as_ref()
                    .map(|p| format!(",x-systemd.requires-mounts-for={}", p.display()))
                    .unwrap_or_default();
                let _ = writeln!(
                    s,
                    "if ! grep -qF {line} /etc/fstab; then",
                    line = sq(&format!("{} {} ", source.display(), root.display()))
                );
                let _ = writeln!(
                    s,
                    "    printf '%s %s none bind{requires} 0 0\\n' \\\n        \
                     \"$storage_source\" \"$storage_root\" >>/etc/fstab"
                );
                let _ = writeln!(s, "    systemctl daemon-reload");
                let _ = writeln!(s, "fi");
                let _ = writeln!(
                    s,
                    "mountpoint -q \"$storage_root\" || mount \"$storage_root\""
                );
                let _ = writeln!(
                    s,
                    "mountpoint -q \"$storage_root\" || die {msg}",
                    msg = sq(&format!(
                        "{} is not a mountpoint after mounting; fix it before creating homes",
                        root.display()
                    )),
                );
            } else {
                let _ = writeln!(s, "install -d -m \"$storage_mode\" \"$storage_root\"");
                let _ = writeln!(
                    s,
                    "mountpoint -q \"$storage_source\" || die {msg}",
                    msg = sq("the storage source is not mounted; mount it, or enable storage.bind")
                );
            }
        } else {
            let _ = writeln!(s, "install -d -m \"$storage_mode\" \"$storage_root\"");
        }
        let _ = writeln!(s);

        // ── users ────────────────────────────────────────────────────
        let _ = writeln!(s, "# ── accounts ──");
        let _ = writeln!(
            s,
            "# The first id past every range already in /etc/subuid and /etc/subgid."
        );
        let _ = writeln!(
            s,
            "next_subid() {{\n    awk -F: 'BEGIN {{ m = 100000 }} \
             {{ e = $2 + $3; if (e > m) m = e }} END {{ print m }}' \\\n        /etc/subuid /etc/subgid\n}}"
        );
        let _ = writeln!(
            s,
            "# Run a command as $1 with its systemd user manager reachable.\n\
             as_user() {{\n    u=$1\n    shift\n    uid=$(id -u \"$u\")\n    i=0\n    \
             while [ ! -S \"/run/user/$uid/bus\" ] && [ \"$i\" -lt 100 ]; do\n        \
             sleep 0.1\n        i=$((i + 1))\n    done\n    \
             # From /: podman refuses to start in a directory its user cannot read.\n    \
             cd /\n    runuser -u \"$u\" -- env XDG_RUNTIME_DIR=\"/run/user/$uid\" \\\n        \
             DBUS_SESSION_BUS_ADDRESS=\"unix:path=/run/user/$uid/bus\" \"$@\"\n}}"
        );
        let _ = writeln!(s);
        for user in self.users() {
            let name = sq(&user.name);
            let home = sq_path(&user.home);
            let _ = writeln!(s, "# {} ({})", user.name, user.home.display());
            if user.system {
                let _ = writeln!(
                    s,
                    "id -u {name} >/dev/null 2>&1 || useradd --system --user-group \\\n    \
                     --create-home --home-dir {home} --shell /usr/sbin/nologin {name}"
                );
            } else {
                let _ = writeln!(
                    s,
                    "id -u {name} >/dev/null 2>&1 || die {msg}",
                    msg = sq(&format!(
                        "account {} does not exist; this plan only manages a system account",
                        user.name
                    ))
                );
            }
            let _ = writeln!(s, "chmod \"$home_mode\" {home}");
            if let Some(group) = &user.group {
                let _ = writeln!(s, "chgrp {} {home}", sq(group));
            }
            if user.rootless {
                // The pattern is built from the validated name alone:
                // quoting it would put literal quote characters into the
                // regex and never match.
                let _ = writeln!(
                    s,
                    "if ! grep -q \"^{name}:\" /etc/subuid; then\n    \
                     s=$(next_subid)\n    \
                     usermod --add-subuids \"$s-$((s + 65535))\" \\\n        \
                     --add-subgids \"$s-$((s + 65535))\" {name}\nfi",
                    name = user.name,
                );
                let _ = writeln!(s, "loginctl enable-linger {name}");
                // Linger starts the user manager asynchronously; the steps below
                // talk to it. Wait for it explicitly (user@.service is
                // Type=notify) instead of racing a fixed poll on a fresh host.
                let _ = writeln!(s, "systemctl start \"user@$(id -u {name}).service\"");
            }
            let _ = writeln!(s);
        }
        // Group memberships the keep-id containers need.
        for user in self.users() {
            if let Some(group) = &user.group {
                let _ = writeln!(
                    s,
                    "# {} must be in {} so its keep-id container can reach the socket gate.",
                    user.name, group
                );
                let _ = writeln!(s, "usermod -a -G {} {}", sq(group), sq(&user.name));
            }
        }
        let _ = writeln!(s);

        // ── files ────────────────────────────────────────────────────
        let _ = writeln!(s, "# ── files ──");
        for file in files {
            self.write_file_step(&mut s, file);
        }

        // ── the sandbox socket ───────────────────────────────────────────────
        // The tmpfiles entry creates the directory gate; it must exist
        // before the socket can be created in it.
        if let Some(tmpfiles) = files.iter().find(|f| f.path.starts_with("/etc/tmpfiles.d")) {
            let _ = writeln!(s, "# ── the socket directory (the gate) ──");
            let _ = writeln!(
                s,
                "systemd-tmpfiles --create {path}",
                path = sq_path(&tmpfiles.path)
            );
            let _ = writeln!(
                s,
                "install -d -m {mode:04o} -o {user} -g {group} {dir}",
                mode = self.socket.dir_mode,
                user = sq(&self.socket.user),
                group = sq(&self.socket.group),
                dir = sq_path(&self.socket.dir),
            );
            let _ = writeln!(s);
        }

        let _ = writeln!(s, "# ── the sandbox account's Podman API socket ──");
        // Every level of the unit directory is created explicitly, so no
        // path is ever handed over containing "..".
        let mut units = self
            .user(&self.socket.user)
            .map(|u| u.home.clone())
            .unwrap_or_default();
        let mut levels = Vec::new();
        for part in [".config", "systemd", "user"] {
            units = units.join(part);
            levels.push(units.clone());
        }
        let _ = writeln!(
            s,
            "install -d -o {user} -g {group} {dirs}",
            user = sq(&self.socket.user),
            group = sq(&self.socket.group),
            dirs = levels
                .iter()
                .map(|p| sq_path(p))
                .collect::<Vec<_>>()
                .join(" "),
        );
        let _ = writeln!(
            s,
            "as_user {user} systemctl --user daemon-reload",
            user = sq(&self.socket.user)
        );
        let _ = writeln!(
            s,
            "as_user {user} systemctl --user enable --now {unit}",
            user = sq(&self.socket.user),
            unit = sq(&format!("{}-podman.socket", self.socket.user)),
        );
        let _ = writeln!(s);

        // ── quadlets ─────────────────────────────────────────────────
        if !quadlets.is_empty() {
            let _ = writeln!(s, "# ── quadlets ──");
            for quadlet in quadlets {
                let user = self
                    .user(&quadlet.user)
                    .expect("validated: every quadlet has a user");
                let dir = quadlet
                    .path
                    .parent()
                    .expect("a quadlet file has a parent directory");
                let _ = writeln!(s, "# {} ({})", quadlet.path.display(), user.name);
                // Every level below the home is created and owned explicitly:
                // `install -d` would leave a missing ~/.config root-owned, and
                // rootless podman then refuses the whole account.
                let mut levels = Vec::new();
                if let Ok(below) = dir.strip_prefix(&user.home) {
                    let mut level = user.home.clone();
                    for part in below.components() {
                        level.push(part);
                        levels.push(sq_path(&level));
                    }
                } else {
                    levels.push(sq_path(dir));
                }
                let _ = writeln!(
                    s,
                    "install -d -o {u} -g {u} {dirs}",
                    u = sq(&user.name),
                    dirs = levels.join(" "),
                );
                // Written as a heredoc like every other planned file, then
                // handed to the account that runs it.
                let marker = heredoc_marker(&quadlet.contents);
                let _ = writeln!(s, "cat >{path} <<'{marker}'", path = sq_path(&quadlet.path));
                let _ = write!(s, "{}", quadlet.contents);
                let _ = writeln!(s, "{marker}");
                let _ = writeln!(
                    s,
                    "chown {u}:{u} {path} && chmod 0644 {path}",
                    u = sq(&user.name),
                    path = sq_path(&quadlet.path),
                );
                let _ = writeln!(
                    s,
                    "as_user {u} systemctl --user daemon-reload",
                    u = sq(&user.name)
                );
            }
            let _ = writeln!(s);
        }

        // ── images ───────────────────────────────────────────────────
        if !images.is_empty() {
            let _ = writeln!(s, "# ── images (digest-pinned) ──");
            for request in images {
                let reference = request.image.reference();
                let _ = writeln!(
                    s,
                    "as_user {u} podman image exists {ref_quoted} || as_user {u} podman pull -q {ref_quoted}",
                    u = sq(&request.user),
                    ref_quoted = sq(&reference),
                );
            }
            let _ = writeln!(s);
        }

        // ── warming keep-id layers ───────────────────────────────────
        // A keep-id container's first start copies the image layers into the
        // mapped id space, which can outlast the unit's start timeout on a
        // fresh host (deploy/README.md runbook). Do that copy here.
        let mut warmed = false;
        for quadlet in &self.quadlets {
            let Some(userns) = quadlet
                .container
                .extra
                .iter()
                .find_map(|line| line.strip_prefix("UserNS="))
            else {
                continue;
            };
            if !warmed {
                let _ = writeln!(
                    s,
                    "# ── warm keep-id image layers before the first start ──"
                );
                warmed = true;
            }
            let _ = writeln!(
                s,
                "warm=$(as_user {u} podman create --pull=never --userns={ns} {image})\nas_user {u} podman rm \"$warm\" >/dev/null",
                u = sq(&quadlet.user),
                ns = sq(userns),
                image = sq(&quadlet.container.image.reference()),
            );
        }
        if warmed {
            let _ = writeln!(s);
        }

        // ── the network ──────────────────────────────────────────────
        if let Some(network) = network {
            let _ = writeln!(s, "# ── container network ──");
            let owner = self.gateway.name.as_str();
            let _ = writeln!(
                s,
                "as_user {u} podman network exists {n} >/dev/null 2>&1 || as_user {u} podman network create {n}",
                u = sq(owner),
                n = sq(network),
            );
            let _ = writeln!(s);
        }

        // ── Tor ──────────────────────────────────────────────────────
        if let Some(tor) = &self.tor {
            let _ = writeln!(s, "# ── Tor instances ──");
            for instance in std::iter::once(&tor.sandbox).chain(tor.web.iter()) {
                let _ = writeln!(
                    s,
                    "# Instance {name}: {ports}",
                    name = instance.name,
                    ports = describe_ports(instance),
                );
                let _ = writeln!(s, "systemctl mask tor@default.service");
                let _ = writeln!(
                    s,
                    "if [ ! -x /usr/sbin/tor-instance-create ]; then\n    \
                     die {msg}\nfi",
                    msg = sq("tor-instance-create is missing; install the tor package and re-run")
                );
                let _ = writeln!(
                    s,
                    "[ -d {dir} ] || tor-instance-create {name}",
                    dir = sq_path(&PathBuf::from("/etc/tor/instances").join(&instance.name)),
                    name = sq(&instance.name),
                );
                let _ = writeln!(s, "systemctl daemon-reload");
                let _ = writeln!(
                    s,
                    "systemctl enable {unit}",
                    unit = sq(&format!("tor@{}.service", instance.name))
                );
                let _ = writeln!(
                    s,
                    "systemctl reload-or-restart {unit}",
                    unit = sq(&format!("tor@{}.service", instance.name))
                );
                let _ = writeln!(s);
            }
        }

        // ── the firewall ─────────────────────────────────────────────
        if let Some(_firewall) = &self.firewall {
            let path = self.firewall_path();
            let unit = format!("{}-nft.service", self.unit_prefix);
            let _ = writeln!(s, "# ── the firewall table ──");
            let _ = writeln!(s, "nft -c -f {path}", path = sq_path(&path));
            let _ = writeln!(s, "systemctl daemon-reload");
            let _ = writeln!(
                s,
                "# reenable: the unit's [Install] target can move between releases."
            );
            let _ = writeln!(s, "systemctl reenable {unit}", unit = sq(&unit));
            let _ = writeln!(
                s,
                "# A reload swaps the table in one transaction; a restart would leave\n\
                 # the accounts unconfined between its stop and its start."
            );
            let _ = writeln!(s, "systemctl reload-or-restart {unit}", unit = sq(&unit));
            for user in self.users() {
                let _ = writeln!(
                    s,
                    "uid_{ident}=$(id -u {name})\n\
                     install -d -m 0755 \"/etc/systemd/system/user@${{uid_{ident}}}.service.d\"\n\
                     install -m 0644 {dropin} \"/etc/systemd/system/user@${{uid_{ident}}}.service.d/{prefix}.conf\"",
                    ident = sanitize_ident(&user.name),
                    name = sq(&user.name),
                    dropin = sq_path(&self.dropin_path()),
                    prefix = self.unit_prefix,
                );
            }
            let _ = writeln!(s, "systemctl daemon-reload");
            let _ = writeln!(s);
            let _ = writeln!(s, "nft -c -f {path}", path = sq_path(&path));
        }

        // ── peer sync ────────────────────────────────────────────────
        if let Some(peer) = &self.peer_sync {
            let _ = writeln!(s, "# ── peer sync ──");
            let _ = writeln!(
                s,
                "install -d -o {o} -g {o} -m 0700 {q}",
                o = sq(&peer.owner),
                q = sq_path(&peer.queue)
            );
            let _ = writeln!(s, "systemctl daemon-reload");
            let _ = writeln!(
                s,
                "systemctl enable --now {unit}.path",
                unit = sq(&peer.unit_name)
            );
            let _ = writeln!(s);
        }

        let _ = writeln!(
            s,
            "echo {msg}",
            msg = sq(&format!(
                "{} container-host: done (accounts: {}; storage: {})",
                self.unit_prefix,
                self.users()
                    .iter()
                    .map(|u| u.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                self.storage.root.display()
            ))
        );
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "# What this script did NOT do: {}",
            Scope::of(self).excludes.join("; ")
        );
        s
    }

    fn write_file_step(&self, s: &mut String, file: &PlannedFile) {
        let marker = heredoc_marker(&file.contents);
        let _ = writeln!(s, "# {}", file.path.display());
        let _ = writeln!(
            s,
            "install -d -m 0755 {parent}",
            parent = sq_path(file.path.parent().unwrap_or(Path::new("/"))),
        );
        let _ = writeln!(s, "cat >{} <<'{marker}'", sq_path(&file.path));
        let _ = write!(s, "{}", file.contents);
        let _ = writeln!(s, "{marker}");
        let owner = file.owner.as_deref().unwrap_or("root:root");
        let _ = writeln!(
            s,
            "chmod {mode:04o} {path}",
            mode = file.mode,
            path = sq_path(&file.path)
        );
        if owner != "root:root" {
            let _ = writeln!(s, "chown {} {path}", sq(owner), path = sq_path(&file.path));
        }
        let _ = writeln!(s);
    }
}

fn sanitize_ident(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn describe_ports(instance: &TorInstance) -> String {
    let mut parts = Vec::new();
    if let Some(port) = instance.transport_port {
        parts.push(format!("TransPort {port}"));
    }
    if let Some(port) = instance.socks_port {
        parts.push(format!(
            "SocksPort {port}{}",
            if instance.isolate_socks_auth {
                " (IsolateSOCKSAuth)"
            } else {
                ""
            }
        ));
    }
    if let Some(interface) = &instance.requires_interface {
        parts.push(format!("after {interface}"));
    }
    parts.join(", ")
}

// ── rendered units ─────────────────────────────────────────────────────

fn render_torrc(instance: &TorInstance) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Generated by veles: Tor instance {}. A client only.",
        instance.name
    );
    if let Some(port) = instance.transport_port {
        let _ = writeln!(out, "TransPort {port}");
    }
    match instance.socks_port {
        Some(port) if instance.isolate_socks_auth => {
            let _ = writeln!(out, "SocksPort {port} IsolateSOCKSAuth");
        }
        Some(port) => {
            let _ = writeln!(out, "SocksPort {port}");
        }
        None => {
            let _ = writeln!(out, "SocksPort 0");
        }
    }
    let _ = writeln!(out, "ClientOnly 1");
    let _ = writeln!(out, "AvoidDiskWrites 1");
    out
}

fn render_tor_dropin(instance: &TorInstance) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Generated by veles: {} starts after its interface, and retries a bind\n\
         # that failed because the interface was not up yet.",
        instance.name
    );
    if let Some(interface) = &instance.requires_interface {
        let _ = writeln!(out, "[Unit]");
        let _ = writeln!(out, "After={interface}.service");
        let _ = writeln!(out, "Wants={interface}.service");
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "[Service]");
    let _ = writeln!(out, "Restart=on-failure");
    let _ = writeln!(out, "RestartSec=10");
    out
}

fn render_nft_service(prefix: &str, table: &str, path: &Path) -> String {
    format!(
        "# Generated by veles: loads {path} at boot.\n\
         #\n\
         # Only this plan's table: the file deletes and re-creates it in one\n\
         # transaction, so a reload re-applies it with no gap and nothing else in the\n\
         # ruleset is touched. Early, like the stock nftables.service: no default\n\
         # dependencies, so it runs before the network and orders nothing behind it.\n\
         [Unit]\n\
         Description={prefix}: what the managed accounts may reach\n\
         DefaultDependencies=no\n\
         Wants=network-pre.target\n\
         Before=network-pre.target shutdown.target\n\
         Conflicts=shutdown.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart=/usr/sbin/nft -f {path}\n\
         ExecReload=/usr/sbin/nft -f {path}\n\
         ExecStop=/usr/sbin/nft delete table inet {table}\n\
         \n\
         [Install]\n\
         WantedBy=sysinit.target\n",
        path = path.display(),
        table = table,
        prefix = prefix,
    )
}

fn render_podman_service(description: &str, socket: &str) -> String {
    format!(
        "# Generated by veles: the stock podman.service, on this account's socket.\n\
         [Unit]\n\
         Description={description}\n\
         Requires={socket}\n\
         After={socket}\n\
         StartLimitIntervalSec=0\n\
         \n\
         [Service]\n\
         Delegate=true\n\
         Type=exec\n\
         KillMode=process\n\
         # A manager that stops without a reboot kills the boxes' conmons behind\n\
         # podman's back: its database still says they run, and the service then\n\
         # fails to join a dead namespace. A reboot is spared, so repair only when\n\
         # podman cannot answer at all.\n\
         ExecStartPre=/bin/sh -c '/usr/bin/podman info >/dev/null 2>&1 || /usr/bin/podman system migrate'\n\
         ExecStart=/usr/bin/podman --log-level=info system service --time=0\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn render_peer_path_unit(peer: &PeerSyncOptions) -> String {
    format!(
        "# Generated by veles: runs {} when a peer request lands in {}.\n\
         [Unit]\n\
         Description=veles: apply queued peers to {iface}\n\
         \n\
         [Path]\n\
         PathExists={queue}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        peer.script.display(),
        peer.queue.display(),
        iface = peer.interface,
        queue = peer.queue.display(),
    )
}

fn render_peer_service_unit(peer: &PeerSyncOptions) -> String {
    format!(
        "# Generated by veles: the root helper behind {unit}.path.\n\
         [Unit]\n\
         Description=veles: apply queued peers to {iface}\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         Environment=VELES_PEER_QUEUE={queue}\n\
         Environment=VELES_PEER_INTERFACE={iface}\n\
         ExecStart={script}\n\
         # The helper writes the interface, and nothing else.\n\
         CapabilityBoundingSet=CAP_NET_ADMIN\n\
         ProtectSystem=strict\n\
         ProtectHome=yes\n\
         PrivateTmp=yes\n\
         NoNewPrivileges=yes\n",
        unit = peer.unit_name,
        iface = peer.interface,
        queue = peer.queue.display(),
        script = peer.script.display(),
    )
}

// ── the plan ───────────────────────────────────────────────────────────

/// What this profile does, and what it deliberately does not. Phase 8
/// lists several components that share the host but are separate plans;
/// naming them here keeps a reader from taking this one for the whole
/// deployment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Scope {
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
}

impl Scope {
    fn of(options: &ContainerHostOptions) -> Self {
        let mut includes = vec![
            "rootless container accounts, their storage, subuid and subgid ranges and linger"
                .to_string(),
            format!(
                "the {} account's Podman API socket and its directory gate",
                options.socket.user
            ),
            "systemd units: the socket, the user-manager ordering, and the podman service"
                .to_string(),
        ];
        if options.firewall.is_some() {
            includes.push("the nft table that confines the managed accounts".into());
        }
        if options.tor.is_some() {
            includes.push("Tor client instances for sandbox egress and web fetches".into());
        }
        if !options.quadlets.is_empty() {
            includes.push("digest-pinned quadlets for the containers named in the options".into());
        }
        Self {
            includes,
            excludes: vec![
                "the PDS (vlpds quadlet, its secrets, nginx and TLS): not part of this plan"
                    .into(),
                "media publishing (vhost, certificate, expiry timer): not part of this plan".into(),
                "pool and multi-node pairing (WireGuard keys, cluster peers, vault join, the signer): not part of this plan"
                    .into(),
                "SearXNG, the challenge relay and anything else served over Tor for other people: not part of this plan"
                    .into(),
                "DNS: never changed here".into(),
                "disk partitioning and formatting: never done here".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlannedFile {
    pub path: PathBuf,
    pub contents: String,
    pub mode: u32,
    /// `user:group` when the file is not root's.
    pub owner: Option<String>,
}

impl PlannedFile {
    fn root(path: PathBuf, contents: String, owner: Option<String>) -> Self {
        Self {
            path,
            contents,
            mode: 0o644,
            owner,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlannedQuadlet {
    pub path: PathBuf,
    pub contents: String,
    pub user: String,
}

/// The reviewed plan. `script` is the ONE privileged script; everything
/// else is shown to the operator as typed data before it runs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerHostPlan {
    pub scope: Scope,
    pub files: Vec<PlannedFile>,
    pub quadlets: Vec<PlannedQuadlet>,
    pub images: Vec<PullRequest>,
    /// Packages the plan wants, when it was allowed to install them.
    pub packages: Vec<String>,
    pub tor: Option<TorOptions>,
    /// The nft ruleset, when the firewall is planned.
    pub firewall: Option<PlannedFile>,
    pub script: String,
    pub script_sha256: String,
}

impl ContainerHostPlan {
    /// Every rendered byte, for a preview that has to show all of it.
    pub fn render_all(&self) -> String {
        let mut out = self.script.clone();
        for file in &self.files {
            out.push_str(&file.path.to_string_lossy());
            out.push('\n');
            out.push_str(&file.contents);
        }
        for quadlet in &self.quadlets {
            out.push_str(&quadlet.path.to_string_lossy());
            out.push('\n');
            out.push_str(&quadlet.contents);
        }
        out
    }

    /// The lines a human should read before sudo.
    pub fn preview(&self) -> Vec<String> {
        let mut out = vec![format!(
            "one privileged script (sha256 {}, {} lines)",
            self.script_sha256,
            self.script.lines().count()
        )];
        for file in &self.files {
            out.push(format!(
                "file {} ({:04o}{}, {} bytes)",
                file.path.display(),
                file.mode,
                file.owner
                    .as_deref()
                    .map(|o| format!(" {o}"))
                    .unwrap_or_default(),
                file.contents.len()
            ));
        }
        for quadlet in &self.quadlets {
            out.push(format!(
                "quadlet {} for {} ({} bytes)",
                quadlet.path.display(),
                quadlet.user,
                quadlet.contents.len()
            ));
        }
        for image in &self.images {
            out.push(format!(
                "pull {} for {}",
                image.image.reference(),
                image.user
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting_survives_a_quote_in_the_value() {
        assert_eq!(sq("plain"), "'plain'");
        assert_eq!(sq("it's"), "'it'\\''s'");
    }

    #[test]
    fn account_names_follow_posix() {
        for good in ["veles", "gw", "_svc", "a-b_c9"] {
            assert!(valid_account_name(good), "{good} refused");
        }
        for bad in [
            "", "Root", "root", "9lives", "-x", "x-", "a b", "a;b", "a\nb",
        ] {
            assert!(!valid_account_name(bad), "{bad:?} accepted");
        }
    }

    #[cfg(unix)]
    #[test]
    fn paths_are_absolute_and_normalised() {
        for good in ["/srv/containers", "/run/x"] {
            assert!(valid_absolute_path(Path::new(good)).is_ok(), "{good}");
        }
        for bad in [
            "",
            "relative",
            "/",
            "/srv/../etc",
            "/srv/./x",
            "/srv/x/",
            "/a b",
            "/a\nb",
        ] {
            assert!(
                valid_absolute_path(Path::new(bad)).is_err(),
                "{bad:?} accepted"
            );
        }
    }

    #[test]
    fn a_digest_is_exactly_a_sha256() {
        assert!(PinnedImage::new("q.io/a/b:1", &format!("sha256:{}", "a".repeat(64))).is_ok());
        for bad in [
            String::new(),
            "sha256:".into(),
            format!("sha256:{}", "A".repeat(64)),
            format!("sha256:{}", "a".repeat(63)),
            format!("md5:{}", "a".repeat(64)),
            "a".repeat(64),
        ] {
            assert!(
                PinnedImage::new("q.io/a/b:1", &bad).is_err(),
                "{bad:?} accepted"
            );
        }
    }

    #[test]
    fn a_registry_port_is_not_a_tag() {
        let image = PinnedImage::new(
            "registry.example:5000:5000/team/app:1.2",
            &format!("sha256:{}", "b".repeat(64)),
        );
        assert!(image.is_ok(), "{image:?}");
        assert!(image.unwrap().source.contains("registry.example:5000"));
    }
}

//! Host components are typed installation inputs, not executable plugin
//! manifests and not arbitrary privileged shell. Each one is a validated
//! options struct with a read-only `preview` (what the operator reviews)
//! and an `apply` that runs ONLY inside the explicit CLI handoff
//! (`veles install profile-handoff`), as root, through one typed argv
//! boundary ([`HostCommands`]) and one host transaction ([`apply_all`]).
//!
//! The transaction is the rollback contract (PLAN-plugins-installer.md
//! R11, "privileged rollback"): every host file, link and directory a
//! component writes is captured immediately before the write and marked
//! after it. When any later step fails, those paths are restored (or
//! removed when they were new), and the service effects already performed
//! are compensated in reverse order (nginx reloaded on the restored
//! configuration, systemd reloaded, units this run enabled disabled
//! again). Deliberately retained on failure, and named in the error:
//! private key material the operator may already have backed up (PDS KEK,
//! PLC key, credentials) and pulled images.
//!
//! Secret values are supplied to the CLI handoff or generated there;
//! previews never contain them. DNS is never changed.
use crate::container_host::PinnedImage;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The reviewed helper that applies the gateway's queued WireGuard peers
/// (deploy/vps/wg). Embedded so the pool hub installs the same bytes the
/// VPS runbook and scripts/test-wg-peers.sh pin.
const WG_PEERS_HELPER: &str = include_str!("../../../deploy/vps/wg/veles-wg-peers.sh");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostComponent {
    Pds(Box<PdsOptions>),
    Media(MediaOptions),
    PoolHub(PoolHubOptions),
    PoolMember(PoolMemberOptions),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaOptions {
    pub public_url: String,
    pub publish_dir: PathBuf,
    pub gateway_user: String,
    pub acme_email: String,
    #[serde(default)]
    pub max_age_days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsRecord {
    pub name: String,
    pub kind: String,
    pub proxied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdsOptions {
    pub public_url: String,
    pub handle_domain: String,
    /// Used only for ACME account registration, never the PDS identity.
    pub acme_email: String,
    /// Not a durable file toggle: the signed request records that the
    /// operator confirmed an offline KEK and PLC-key backup for this apply.
    #[serde(default)]
    pub backup_confirmed: bool,
    pub image: PinnedImage,
    pub s3_endpoint: String,
    pub s3_region: String,
    pub s3_bucket: String,
    pub s3_prefix: String,
    pub gateway_unit: String,
    pub gateway_user: String,
    /// Host-side state root is fixed by the installer. It is not part of
    /// the accepted input at all: an answers/API body naming it is refused
    /// (deny_unknown_fields), not silently ignored.
    #[serde(skip, default = "pds_state_root")]
    state_root: PathBuf,
    pub network: String,
    pub gateway_quadlet: String,
    #[serde(default)]
    pub recovery_did_key: Option<String>,
}

fn pds_state_root() -> PathBuf {
    PathBuf::from("/srv/veles/vlpds")
}

/// The pool's WireGuard hub: the arbiter's tunnel interface and the root
/// helper that applies the peers `veles pool invite` queues (docs/pool.md
/// "The hub's WireGuard"). The gateway configuration (`[pool.wireguard]`,
/// `[cluster] arbiter`, `[node] endpoint`) stays the operator's edit; the
/// apply report prints the exact section with the hub's public key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolHubOptions {
    #[serde(default = "default_wg_interface")]
    pub interface: String,
    /// The tunnel network, `a.b.c.0/24` (the helper handles /24 only).
    pub subnet: String,
    /// The hub's own address inside it.
    pub address: String,
    #[serde(default = "default_wg_port")]
    pub listen_port: u16,
    /// Where peers dial the hub, `host:port` (public name or address).
    pub endpoint: String,
    /// The account the gateway runs as; it owns the peer queue.
    pub gateway_user: String,
    /// The gateway's data directory (`VELES_HOME` for a system service).
    /// The queue is `<it>/pool/wg-pending`, exactly where `veles pool
    /// invite` writes it.
    pub gateway_data_dir: PathBuf,
}

/// A pool member's end of the tunnel, from the WireGuard code `veles pool
/// invite` printed on the hub, saved to a file. The file holds the
/// member's private key: it is read by the preview and again, as root, by
/// the handoff; the review shows only its public fields and its digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolMemberOptions {
    #[serde(default = "default_wg_interface")]
    pub interface: String,
    pub client_config: PathBuf,
}

fn default_wg_interface() -> String {
    "wg0".into()
}

fn default_wg_port() -> u16 {
    51820
}

impl HostComponent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pds(_) => "own-pds",
            Self::Media(_) => "local-media",
            Self::PoolHub(_) => "pool-hub",
            Self::PoolMember(_) => "pool-member",
        }
    }

    pub fn preview(&self) -> Result<Value> {
        match self {
            Self::Pds(options) => options.preview(),
            Self::Media(options) => options.preview(),
            Self::PoolHub(options) => options.preview(),
            Self::PoolMember(options) => options.preview(),
        }
    }

    /// Secret names the CLI handoff must supply (prompted, never previewed).
    pub fn secrets(&self) -> &'static [&'static str] {
        match self {
            Self::Pds(_) => &["VLPDS_S3_ACCESS_KEY", "VLPDS_S3_SECRET_KEY"],
            _ => &[],
        }
    }
}

// ── the typed command boundary ───────────────────────────────────────

/// The ONE way a host component runs a program: a typed argv, empty
/// stdin, no shell. Production is [`ProcessCommands`]; tests inject a
/// recorder that can fail a chosen call, which is how the rollback
/// contract is exercised without touching the machine.
pub trait HostCommands {
    /// Run; a non-zero exit is an error.
    fn run(&mut self, argv: &[String]) -> Result<()>;
    /// A yes/no question (`systemctl is-enabled`); non-zero is `false`.
    fn query(&mut self, argv: &[String]) -> Result<bool>;
    /// Run with captured stdout and stderr; non-zero is an error that
    /// carries only stderr (stdout may be private).
    fn output(&mut self, argv: &[String]) -> Result<(Vec<u8>, Vec<u8>)>;
    /// Wait between observations (handshake, health). A fixture skips it.
    fn pause(&mut self, seconds: u64) {
        std::thread::sleep(std::time::Duration::from_secs(seconds));
    }
}

pub struct ProcessCommands;

impl HostCommands for ProcessCommands {
    fn run(&mut self, argv: &[String]) -> Result<()> {
        let (program, args) = argv.split_first().context("host command has no argv")?;
        let status = std::process::Command::new(program)
            .args(args)
            // From /: commands run as another account (runuser) must not inherit
            // a working directory that account cannot enter.
            .current_dir("/")
            .stdin(std::process::Stdio::null())
            // stdout carries the handoff's JSON report; diagnostics go to stderr.
            .stdout(std::process::Stdio::from(std::io::stderr()))
            .status()
            .with_context(|| format!("running reviewed host command {}", argv.join(" ")))?;
        anyhow::ensure!(
            status.success(),
            "reviewed host command failed ({status}): {}",
            argv.join(" ")
        );
        Ok(())
    }

    fn query(&mut self, argv: &[String]) -> Result<bool> {
        let (program, args) = argv.split_first().context("host query has no argv")?;
        Ok(std::process::Command::new(program)
            .args(args)
            // From /: commands run as another account (runuser) must not inherit
            // a working directory that account cannot enter.
            .current_dir("/")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .with_context(|| format!("running host query {}", argv.join(" ")))?
            .success())
    }

    fn output(&mut self, argv: &[String]) -> Result<(Vec<u8>, Vec<u8>)> {
        ProcessSecretRunner.run_secret(argv)
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_string()).collect()
}

// ── media ────────────────────────────────────────────────────────────

impl MediaOptions {
    fn validate(&self) -> Result<url::Url> {
        let url = https_url(&self.public_url, "media public URL")?;
        anyhow::ensure!(
            url.path() == "/" || url.path().is_empty(),
            "media public URL must be the site root"
        );
        absolute_path(&self.publish_dir, "media publication directory")?;
        anyhow::ensure!(
            [
                Path::new("/srv/veles"),
                Path::new("/var/www"),
                Path::new("/var/lib/veles")
            ]
            .iter()
            .any(|base| self.publish_dir.starts_with(base) && self.publish_dir != *base),
            "media publication directory must be a dedicated /srv/veles, /var/www or /var/lib/veles child"
        );
        anyhow::ensure!(
            !self
                .publish_dir
                .to_string_lossy()
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, ';' | '{' | '}' | '"' | '\'' | '$' | '#')),
            "media publication directory must not contain whitespace or nginx syntax"
        );
        crate::valid_component_name(&self.gateway_user).context("media gateway account")?;
        acme_mailbox(&self.acme_email)?;
        anyhow::ensure!(
            self.max_age_days != Some(0),
            "media expiry must be at least one day"
        );
        Ok(url)
    }

    fn domain(&self) -> Result<String> {
        let url = self.validate()?;
        let domain = url.host_str().context("media URL has no host")?.to_string();
        dns_name(&domain, "media host")?;
        Ok(domain)
    }

    /// The gateway configuration this site serves. Printed, never written:
    /// the gateway's config.toml is its own account's file.
    pub fn gateway_configuration(&self) -> Result<String> {
        self.validate()?;
        let mut out = format!(
            "[platforms.irc]\nmedia_public_dir = \"{}\"\nmedia_public_url = \"{}\"\n",
            self.publish_dir.display(),
            self.public_url.trim_end_matches('/')
        );
        if let Some(days) = self.max_age_days {
            let _ = writeln!(out, "media_public_max_age_days = {days}");
        }
        Ok(out)
    }

    pub fn preview(&self) -> Result<Value> {
        let domain = self.domain()?;
        Ok(json!({
            "component": "local-media",
            "host_apply_implemented": true,
            "public_url": self.public_url,
            "publish_dir": self.publish_dir,
            "host_effects": {
                "directory": {"path": self.publish_dir, "mode": "0755", "owner": self.gateway_user,
                    "note": "created only when missing; an existing directory must already belong to the gateway account. Nothing in it is chowned, rewritten or deleted"},
                "files": [
                    {"path": "/etc/nginx/sites-available/veles-media.conf", "mode": "0644", "contents": self.nginx(false, &domain)?, "when": "certificate missing"},
                    {"path": "/etc/nginx/sites-available/veles-media.conf", "mode": "0644", "contents": self.nginx(true, &domain)?, "when": "certificate ready"}
                ],
                "link": {"link": "/etc/nginx/sites-enabled/veles-media.conf", "target": "/etc/nginx/sites-available/veles-media.conf"},
                "commands": [
                    {"argv": ["nginx", "-t"], "when": "before every reload"},
                    {"argv": ["systemctl", "reload", "nginx.service"], "when": "after nginx -t passes"},
                    {"argv": ["certbot", "certonly", "--webroot", "-w", "/var/www/veles-media-acme", "-d", domain, "--non-interactive", "--agree-tos", "-m", self.acme_email, "--keep-until-expiring"], "when": "certificate missing"},
                    {"argv": ["curl", "--fail", "--silent", "--show-error", "--max-time", "10", "--resolve", format!("{domain}:443:127.0.0.1"), format!("https://{domain}/veles-media-probe-<random>.txt")], "when": "after TLS: a probe file is published and fetched back, then deleted"}
                ]
            },
            "dns_changed": false,
            "dns_plan": [{"name": domain, "kind": "A/AAAA", "proxied": false}],
            "existing_media_preserved": true,
            "max_age_days": self.max_age_days,
            "gateway_configuration": self.gateway_configuration()?,
            "rollback": "a failure restores the vhost and the enabled link (or removes them when new) and reloads nginx on the restored configuration",
            "note": "Local static media only. The gateway configuration printed here is the operator's edit; remote SSH sync, mount mode and DNS are not part of this component."
        }))
    }

    fn nginx(&self, certificate_ready: bool, domain: &str) -> Result<String> {
        self.validate()?;
        let root = self.publish_dir.display();
        let mut out = format!(
            r#"server {{
    listen 80;
    listen [::]:80;
    server_name {domain};
    location /.well-known/acme-challenge/ {{
        root /var/www/veles-media-acme;
        try_files $uri =404;
    }}
    location / {{ return 301 https://$host$request_uri; }}
}}
"#
        );
        if certificate_ready {
            let _ = write!(
                out,
                r#"server {{
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name {domain};
    ssl_certificate /etc/letsencrypt/live/{domain}/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/{domain}/privkey.pem;
    ssl_trusted_certificate /etc/letsencrypt/live/{domain}/chain.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_prefer_server_ciphers off;
    ssl_session_tickets off;
    root {root};
    autoindex off;
    add_header X-Content-Type-Options nosniff always;
    add_header Cache-Control "public, max-age=31536000, immutable" always;
    location ~ /\. {{ deny all; }}
    location ~* \.(png|jpe?g|gif|webp|bmp|avif|ico|mp4|webm|mov|mkv|mp3|wav|ogg|oga|opus|flac|m4a|aac|pdf)$ {{
        try_files $uri =404;
    }}
    location / {{
        default_type application/octet-stream;
        types {{ }}
        add_header Content-Disposition "attachment" always;
        add_header Content-Security-Policy "default-src 'none'; sandbox" always;
        add_header X-Content-Type-Options nosniff always;
        add_header Cache-Control "public, max-age=31536000, immutable" always;
        try_files $uri =404;
    }}
}}
"#
            );
        }
        Ok(out)
    }
}

// ── pool ─────────────────────────────────────────────────────────────

fn wg_interface(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 15
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "WireGuard interface must be 1-15 of a-z, 0-9 and -"
    );
    Ok(())
}

fn wg_key(value: &str, field: &str) -> Result<()> {
    anyhow::ensure!(
        value.len() == 44
            && value.ends_with('=')
            && value[..43]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/'),
        "{field} is not a WireGuard key"
    );
    Ok(())
}

fn host_port(value: &str, field: &str) -> Result<()> {
    let (host, port) = value
        .rsplit_once(':')
        .with_context(|| format!("{field} must be host:port"))?;
    let port: u16 = port
        .parse()
        .with_context(|| format!("{field} port is not a number"))?;
    anyhow::ensure!(port != 0, "{field} port must be 1-65535");
    if host.parse::<std::net::Ipv4Addr>().is_err() {
        dns_name(host, field)?;
    }
    Ok(())
}

/// No whitespace, quote or systemd specifier: the path is written into
/// unit files (ExecStart, ReadWritePaths, PathChanged).
fn unit_safe_path(path: &Path, field: &str) -> Result<()> {
    absolute_path(path, field)?;
    anyhow::ensure!(
        path.to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-')),
        "{field} may contain only letters, digits, '/', '.', '_' and '-'"
    );
    Ok(())
}

impl PoolHubOptions {
    fn net(&self) -> Result<(std::net::Ipv4Addr, std::net::Ipv4Addr)> {
        let (net, bits) = self
            .subnet
            .split_once('/')
            .context("pool subnet must be a.b.c.0/24")?;
        anyhow::ensure!(
            bits == "24",
            "pool subnet must be a /24 (the peer helper handles /24 only)"
        );
        let net: std::net::Ipv4Addr = net.parse().context("pool subnet address")?;
        anyhow::ensure!(
            net.octets()[3] == 0,
            "pool subnet must be the network address (a.b.c.0/24)"
        );
        let address: std::net::Ipv4Addr = self.address.parse().context("hub address")?;
        anyhow::ensure!(
            address.octets()[..3] == net.octets()[..3] && !matches!(address.octets()[3], 0 | 255),
            "hub address must be a host inside the pool subnet"
        );
        Ok((net, address))
    }

    fn queue(&self) -> PathBuf {
        self.gateway_data_dir.join("pool/wg-pending")
    }

    fn validate(&self) -> Result<()> {
        wg_interface(&self.interface)?;
        self.net()?;
        anyhow::ensure!(self.listen_port != 0, "listen port must be 1-65535");
        host_port(&self.endpoint, "hub endpoint")?;
        crate::valid_component_name(&self.gateway_user).context("pool gateway account")?;
        unit_safe_path(&self.gateway_data_dir, "gateway data directory")?;
        // The helper runs with ProtectHome=yes: a queue under a home or a
        // runtime directory would be invisible to it.
        for hidden in ["/home", "/root", "/run/user", "/tmp", "/var/tmp"] {
            anyhow::ensure!(
                !self.gateway_data_dir.starts_with(hidden),
                "the hub's gateway data directory must not be under {hidden}: the root helper cannot see it (use a system server, VELES_HOME=/var/lib/veles)"
            );
        }
        Ok(())
    }

    fn conf_path(&self) -> PathBuf {
        PathBuf::from("/etc/wireguard").join(format!("{}.conf", self.interface))
    }

    fn service_unit(&self) -> String {
        format!(
            "# Installed by `veles install host-component` (pool-hub). The\n\
             # reviewed helper applies the gateway's queued WireGuard peers.\n\
             [Unit]\nDescription=Apply the veles gateway's queued WireGuard peers\nAfter=wg-quick@{iface}.service\n\n\
             [Service]\nType=oneshot\nExecStart=/usr/local/sbin/veles-wg-peers {iface} {subnet} {queue}\n\
             NoNewPrivileges=yes\nProtectHome=yes\nPrivateTmp=yes\nProtectSystem=strict\n\
             ReadWritePaths=/etc/wireguard {queue}\n",
            iface = self.interface,
            subnet = self.subnet,
            queue = self.queue().display()
        )
    }

    fn path_unit(&self) -> String {
        format!(
            "# Installed by `veles install host-component` (pool-hub).\n\
             [Unit]\nDescription=Watch the veles gateway's WireGuard peer queue\n\n\
             [Path]\nPathChanged={queue}\nMakeDirectory=no\nUnit=veles-wg-peers.service\n\n\
             [Install]\nWantedBy=multi-user.target\n",
            queue = self.queue().display()
        )
    }

    /// What the gateway's config.toml needs, given the hub's public key.
    pub fn gateway_configuration(&self, public_key: &str) -> String {
        format!(
            "[pool.wireguard]\ninterface = \"{}\"\nsubnet = \"{}\"\naddress = \"{}\"\nendpoint = \"{}\"\npublic_key = \"{}\"\n",
            self.interface, self.subnet, self.address, self.endpoint, public_key
        )
    }

    pub fn preview(&self) -> Result<Value> {
        self.validate()?;
        let unit = |name: &str| format!("/etc/systemd/system/{name}");
        Ok(json!({
            "component": "pool-hub",
            "host_apply_implemented": true,
            "interface": self.interface,
            "subnet": self.subnet,
            "address": self.address,
            "listen_port": self.listen_port,
            "endpoint": self.endpoint,
            "queue": self.queue(),
            "host_effects": {
                "files": [
                    {"path": self.conf_path(), "mode": "0600", "when": "missing: a new hub key pair is generated here; an existing file is kept (its key is never rotated) and must name the same address and port",
                     "contents": format!("[Interface]\nAddress = {}/24\nListenPort = {}\nPrivateKey = <generated, never shown>\n", self.address, self.listen_port)},
                    {"path": "/usr/local/sbin/veles-wg-peers", "mode": "0755", "sha256": crate::sha256(WG_PEERS_HELPER.as_bytes()), "contents": "deploy/vps/wg/veles-wg-peers.sh"},
                    {"path": unit("veles-wg-peers.service"), "mode": "0644", "contents": self.service_unit()},
                    {"path": unit("veles-wg-peers.path"), "mode": "0644", "contents": self.path_unit()}
                ],
                "directories": [
                    {"path": "/etc/wireguard", "mode": "0700", "owner": "root"},
                    {"path": self.gateway_data_dir.join("pool"), "mode": "0700", "owner": self.gateway_user, "when": "missing"},
                    {"path": self.queue(), "mode": "0700", "owner": self.gateway_user, "when": "missing"}
                ],
                "commands": [
                    {"argv": ["systemctl", "daemon-reload"]},
                    {"argv": ["systemctl", "enable", "--now", format!("wg-quick@{}.service", self.interface)], "when": "not already enabled"},
                    {"argv": ["systemctl", "enable", "--now", "veles-wg-peers.path"], "when": "not already enabled"},
                    {"argv": ["wg", "show", self.interface.as_str(), "public-key"], "when": "verify: the running interface carries the configured key"}
                ]
            },
            "dns_changed": false,
            "firewall": format!("UDP {} must reach this host; no firewall rule is changed", self.listen_port),
            "rollback": "a failure restores or removes the files above, reloads systemd, and disables units this run enabled",
            "note": "The gateway's [pool.wireguard], [cluster] and [node] settings are printed for the operator after apply; this component does not edit the gateway configuration."
        }))
    }
}

/// A parsed WireGuard client configuration, exactly the shape
/// `veles_core::pool::wg_client_config` writes. Anything else is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WgClient {
    private_key: String,
    address: std::net::Ipv4Addr,
    peer_public_key: String,
    endpoint: String,
    allowed_ips: String,
    keepalive: u16,
}

impl WgClient {
    fn parse(text: &str) -> Result<Self> {
        let mut section = "";
        let mut fields = std::collections::BTreeMap::new();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[Interface]" || line == "[Peer]" {
                anyhow::ensure!(
                    !(line == "[Interface]" && !section.is_empty())
                        && !(line == "[Peer]" && section != "Interface"),
                    "the WireGuard client config must have one [Interface] then one [Peer]"
                );
                section = &line[1..line.len() - 1];
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .context("the WireGuard client config has a line that is not key = value")?;
            let key = format!("{section}.{}", key.trim());
            anyhow::ensure!(
                fields
                    .insert(key.clone(), value.trim().to_string())
                    .is_none(),
                "the WireGuard client config repeats {key}"
            );
        }
        let take = |fields: &mut std::collections::BTreeMap<String, String>, key: &str| {
            fields
                .remove(key)
                .with_context(|| format!("the WireGuard client config lacks {key}"))
        };
        let private_key = take(&mut fields, "Interface.PrivateKey")?;
        let address = take(&mut fields, "Interface.Address")?;
        let peer_public_key = take(&mut fields, "Peer.PublicKey")?;
        let endpoint = take(&mut fields, "Peer.Endpoint")?;
        let allowed_ips = take(&mut fields, "Peer.AllowedIPs")?;
        let keepalive = take(&mut fields, "Peer.PersistentKeepalive")?;
        anyhow::ensure!(
            fields.is_empty(),
            "the WireGuard client config has unexpected keys: {:?}",
            fields.keys().collect::<Vec<_>>()
        );
        wg_key(&private_key, "the member's private key")?;
        wg_key(&peer_public_key, "the hub's public key")?;
        let address = address
            .strip_suffix("/32")
            .context("the member address must be a /32")?
            .parse::<std::net::Ipv4Addr>()
            .context("the member address is not IPv4")?;
        host_port(&endpoint, "the hub endpoint")?;
        let (net, bits) = allowed_ips
            .split_once('/')
            .context("AllowedIPs must be the pool subnet a.b.c.d/nn")?;
        net.parse::<std::net::Ipv4Addr>()
            .context("AllowedIPs is not an IPv4 subnet")?;
        let bits: u8 = bits.parse().context("AllowedIPs prefix")?;
        anyhow::ensure!(
            (8..=30).contains(&bits),
            "AllowedIPs must be the pool subnet, not a default route"
        );
        let keepalive: u16 = keepalive.parse().context("PersistentKeepalive")?;
        Ok(Self {
            private_key,
            address,
            peer_public_key,
            endpoint,
            allowed_ips,
            keepalive,
        })
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn render(&self) -> String {
        format!(
            "# Installed by `veles install host-component` (pool-member).\n[Interface]\nPrivateKey = {}\nAddress = {}/32\n\n[Peer]\nPublicKey = {}\nEndpoint = {}\nAllowedIPs = {}\nPersistentKeepalive = {}\n",
            self.private_key, self.address, self.peer_public_key, self.endpoint, self.allowed_ips, self.keepalive
        )
    }
}

impl PoolMemberOptions {
    fn read(&self) -> Result<(WgClient, String)> {
        wg_interface(&self.interface)?;
        absolute_path(&self.client_config, "WireGuard client config")?;
        let meta = std::fs::symlink_metadata(&self.client_config).with_context(|| {
            format!(
                "reading the WireGuard client config {}",
                self.client_config.display()
            )
        })?;
        anyhow::ensure!(
            meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 4096,
            "the WireGuard client config must be a small regular file"
        );
        let bytes = std::fs::read(&self.client_config)?;
        let text = String::from_utf8(bytes.clone()).context("the client config is not UTF-8")?;
        Ok((WgClient::parse(&text)?, crate::sha256(&bytes)))
    }

    fn conf_path(&self) -> PathBuf {
        PathBuf::from("/etc/wireguard").join(format!("{}.conf", self.interface))
    }

    pub fn preview(&self) -> Result<Value> {
        let (client, digest) = self.read()?;
        Ok(json!({
            "component": "pool-member",
            "host_apply_implemented": true,
            "interface": self.interface,
            "client_config": self.client_config,
            "client_config_sha256": digest,
            "address": format!("{}/32", client.address),
            "hub_public_key": client.peer_public_key,
            "hub_endpoint": client.endpoint,
            "allowed_ips": client.allowed_ips,
            "keepalive": client.keepalive,
            "host_effects": {
                "files": [{"path": self.conf_path(), "mode": "0600", "contents": "re-rendered from the parsed client config; the private key is never shown", "when": "missing or identical; a different existing file is refused"}],
                "commands": [
                    {"argv": ["systemctl", "enable", "--now", format!("wg-quick@{}.service", self.interface)], "when": "not already enabled"},
                    {"argv": ["wg", "show", self.interface.as_str(), "latest-handshakes"], "when": "verify: observe a handshake with the hub (reported, not required: the hub applies its side when its queue changes)"}
                ]
            },
            "dns_changed": false,
            "rollback": "a failure restores or removes the interface config and disables the unit if this run enabled it",
            "note": "Join the pool afterwards with `veles pool join '<invite text>'` as the gateway's account. Delete the saved client config once applied: it holds the member's private key."
        }))
    }
}

// ── shared validation helpers ────────────────────────────────────────

fn acme_mailbox(value: &str) -> Result<()> {
    let (local, domain) = value
        .rsplit_once('@')
        .context("ACME email must be a mailbox address")?;
    dns_name(domain, "ACME email domain")?;
    anyhow::ensure!(
        !local.is_empty()
            && local.len() <= 64
            && local
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(&b)),
        "ACME email local part is invalid"
    );
    Ok(())
}

fn https_url(value: &str, field: &str) -> Result<url::Url> {
    let url = url::Url::parse(value).with_context(|| format!("{field} is not a URL"))?;
    anyhow::ensure!(url.scheme() == "https", "{field} must use https");
    anyhow::ensure!(url.host_str().is_some(), "{field} has no hostname");
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "{field} must not contain credentials"
    );
    anyhow::ensure!(
        url.fragment().is_none() && url.query().is_none(),
        "{field} must not contain a query or fragment"
    );
    Ok(url)
}

fn dns_name(value: &str, field: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= 253,
        "{field} is not a DNS name"
    );
    anyhow::ensure!(
        value.split('.').all(|part| !part.is_empty()
            && part.len() <= 63
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !part.starts_with('-')
            && !part.ends_with('-')),
        "{field} is not a DNS name"
    );
    Ok(())
}

fn one_line(value: &str, field: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && !value.contains(['\r', '\n', '\0']),
        "{field} must be one non-empty line"
    );
    Ok(())
}

fn unit_name(value: &str) -> Result<()> {
    anyhow::ensure!(
        value.ends_with(".service")
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@')),
        "gateway unit must be a systemd service name"
    );
    Ok(())
}

fn absolute_path(path: &Path, field: &str) -> Result<()> {
    anyhow::ensure!(
        path.is_absolute()
            && path != Path::new("/")
            && path.components().all(|part| matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )),
        "{field} must be a normalized absolute non-root path"
    );
    anyhow::ensure!(
        !path.as_os_str().as_encoded_bytes().contains(&b'\n')
            && !path.as_os_str().as_encoded_bytes().contains(&b'\r')
            && !path.as_os_str().as_encoded_bytes().contains(&0),
        "{field} contains a control character"
    );
    Ok(())
}

fn quadlet_stem(value: &str, field: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "{field} must be a quadlet name"
    );
    Ok(())
}

fn create_private_if_missing(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o400);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(contents)?;
            file.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(path)?;
            anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "PDS secret {} is not a regular file",
                path.display()
            );
            // Existing credentials are not read into a report or compared
            // by logging their values. Keeping them is the safe rerun.
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("creating private PDS secret {}", path.display()))
        }
    }
}

/// Create the missing components of `path` below `root`, refusing a
/// symlink or special file anywhere on the way. Untracked: used only for
/// the PDS secret tree, which a failed apply deliberately keeps.
fn ensure_real_directory(root: &Path, path: &Path) -> Result<()> {
    anyhow::ensure!(
        path.starts_with(root) && path != root,
        "directory is outside the explicit host root"
    );
    let relative = path.strip_prefix(root)?;
    let mut current = root.to_path_buf();
    for part in relative.components() {
        let std::path::Component::Normal(part) = part else {
            anyhow::bail!("host directory is not normalized: {}", path.display());
        };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) => anyhow::ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "host directory is a symlink or special file: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)
                    .with_context(|| format!("creating host directory {}", current.display()))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Narrow process boundary for commands whose stdout is private. Tests
/// inject a fixture; production supplies the reviewed runuser/Podman argv.
pub trait SecretCommandRunner {
    fn run_secret(&mut self, argv: &[String]) -> Result<(Vec<u8>, Vec<u8>)>;
}

impl<F> SecretCommandRunner for F
where
    F: FnMut(&[String]) -> Result<(Vec<u8>, Vec<u8>)>,
{
    fn run_secret(&mut self, argv: &[String]) -> Result<(Vec<u8>, Vec<u8>)> {
        self(argv)
    }
}

/// Execute a typed command with empty stdin and captured private stdout.
/// Failures return the public diagnostic only, never the secret stream.
pub struct ProcessSecretRunner;

impl SecretCommandRunner for ProcessSecretRunner {
    fn run_secret(&mut self, argv: &[String]) -> Result<(Vec<u8>, Vec<u8>)> {
        let (program, args) = argv.split_first().context("secret command has no argv")?;
        let output = std::process::Command::new(program)
            .args(args)
            // From /: commands run as another account (runuser) must not inherit
            // a working directory that account cannot enter.
            .current_dir("/")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .with_context(|| format!("running reviewed secret command {program}"))?;
        anyhow::ensure!(
            output.status.success(),
            "reviewed secret command {program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok((output.stdout, output.stderr))
    }
}

#[cfg(unix)]
struct CommandsAsSecret<'a>(&'a mut dyn HostCommands);

#[cfg(unix)]
impl SecretCommandRunner for CommandsAsSecret<'_> {
    fn run_secret(&mut self, argv: &[String]) -> Result<(Vec<u8>, Vec<u8>)> {
        self.0.output(argv)
    }
}

// ── PDS ──────────────────────────────────────────────────────────────

impl PdsOptions {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        public_url: String,
        handle_domain: String,
        acme_email: String,
        image: PinnedImage,
        s3_endpoint: String,
        s3_region: String,
        s3_bucket: String,
        s3_prefix: String,
        gateway_unit: String,
        gateway_user: String,
        network: String,
        gateway_quadlet: String,
        recovery_did_key: Option<String>,
    ) -> Self {
        Self {
            public_url,
            handle_domain,
            acme_email,
            backup_confirmed: false,
            image,
            s3_endpoint,
            s3_region,
            s3_bucket,
            s3_prefix,
            gateway_unit,
            gateway_user,
            state_root: pds_state_root(),
            network,
            gateway_quadlet,
            recovery_did_key,
        }
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// The non-secret unit files owned by this component. Secret files are
    /// intentionally absent: a failed TLS/health phase must not delete or
    /// rotate the only KEK/PLC copy before its operator backup.
    #[cfg(unix)]
    pub fn recovery_paths(&self) -> Result<Vec<PathBuf>> {
        let account = crate::ownership::account(&self.gateway_user, None)?;
        Ok(vec![
            account
                .home
                .join(".config/containers/systemd/vlpds.container"),
            account
                .home
                .join(".config/systemd/user")
                .join(format!("{}.d", self.gateway_unit))
                .join("50-vlpds.conf"),
        ])
    }

    #[cfg(all(unix, debug_assertions))]
    #[doc(hidden)]
    pub fn set_state_root_for_test(&mut self, root: PathBuf) {
        self.state_root = root;
    }

    fn validate(&self) -> Result<()> {
        let public = https_url(&self.public_url, "PDS public URL")?;
        anyhow::ensure!(
            public.path() == "/" || public.path().is_empty(),
            "PDS public URL must be the service root"
        );
        https_url(&self.s3_endpoint, "S3 endpoint")?;
        dns_name(&self.handle_domain, "handle domain")?;
        acme_mailbox(&self.acme_email)?;
        for (value, name) in [
            (&self.s3_region, "S3 region"),
            (&self.s3_bucket, "S3 bucket"),
            (&self.s3_prefix, "S3 prefix"),
        ] {
            one_line(value, name)?;
        }
        unit_name(&self.gateway_unit)?;
        crate::valid_component_name(&self.gateway_user).context("PDS gateway account")?;
        absolute_path(&self.state_root, "PDS state root")?;
        quadlet_stem(&self.network, "container network")?;
        quadlet_stem(&self.gateway_quadlet, "gateway quadlet")?;
        if let Some(key) = &self.recovery_did_key {
            anyhow::ensure!(
                key.starts_with("did:key:") && !key.contains(['\r', '\n', '\0']),
                "recovery identity must be a did:key"
            );
        }
        Ok(())
    }

    pub fn preview(&self) -> Result<Value> {
        self.validate()?;
        Ok(json!({
            "component": "own-pds",
            "image": self.image,
            "public_url": self.public_url,
            "handle_domain": self.handle_domain,
            "s3_endpoint": self.s3_endpoint,
            "s3_region": self.s3_region,
            "s3_bucket": self.s3_bucket,
            "s3_prefix": self.s3_prefix,
            "gateway_ordering": format!("PDS starts before {}", self.gateway_unit),
            "recovery_did_key": self.recovery_did_key,
            "secrets": ["VLPDS_S3_ACCESS_KEY", "VLPDS_S3_SECRET_KEY"],
            "generated_secret_files": ["VLPDS_KEK_FILE", "VLPDS_PLC_ROTATION_KEY_FILE", "VLPDS_JWT_SECRET_FILE", "VLPDS_ADMIN_TOKEN_FILE", "VLPDS_INTERNAL_TOKEN_FILE"],
            "backup_gate_required": true,
            "backup_gate": "Back up the KEK and PLC rotation key offline. They are not in the bucket; losing them can make accounts unrecoverable.",
            "host_apply_implemented": true,
            "host_apply_scope": if self.backup_confirmed {
                "runtime preparation, then HTTP-01 TLS, activation and a public health check"
            } else {
                "runtime preparation only (secrets, pinned image, PLC key, unit files); a second request with backup acknowledgement sets up TLS and activation"
            },
            "host_effects": self.host_effects()?,
            "dns_plan": [
                {"name": self.handle_domain, "kind": "A/AAAA", "proxied": false},
                {"name": format!("*.{}", self.handle_domain), "kind": "A/AAAA", "proxied": false}
            ],
            "rollback": "a failure restores or removes the unit files, vhost and link written by this run, reloads nginx and the gateway account's systemd, and stops vlpds if this run started it. Generated secrets and the pulled image are kept.",
            "note": "Preview only. The CLI owns private key generation and host apply. No secret values, DNS changes, service activation or health claim are part of this preview."
        }))
    }

    fn host_effects(&self) -> Result<Value> {
        let home = self.preview_home()?;
        let mut commands = vec![
            json!({"argv": self.rootless_argv(&["podman", "pull", "-q", &self.image.reference()]), "when": "always"}),
            json!({"argv": self.wrap_plc_argv(), "when": "no existing PLC rotation key"}),
        ];
        if self.backup_confirmed {
            commands.extend([
                json!({"argv": ["nginx", "-t"], "when": "offline backup acknowledged; before every nginx reload"}),
                json!({"argv": ["systemctl", "reload", "nginx.service"], "when": "offline backup acknowledged and nginx -t passes"}),
                json!({"argv": ["certbot", "certonly", "--webroot", "-w", "/var/www/veles-pds-acme", "-d", self.handle_domain, "--non-interactive", "--agree-tos", "-m", self.acme_email, "--keep-until-expiring"], "when": "offline backup acknowledged and certificate missing"}),
                json!({"argv": self.rootless_argv(&["systemctl", "--user", "daemon-reload"]), "when": "offline backup acknowledged and TLS ready"}),
                json!({"argv": self.rootless_argv(&["systemctl", "--user", "start", "vlpds.service"]), "when": "offline backup acknowledged and TLS ready"}),
                json!({"argv": self.rootless_argv(&["systemctl", "--user", "restart", &self.gateway_unit]), "when": "offline backup acknowledged and TLS ready"}),
            ]);
        }
        let mut files = vec![
            json!({"path": home.join(".config/containers/systemd/vlpds.container"), "mode": "0644", "contents": self.quadlet()?, "when": "always"}),
            json!({"path": home.join(".config/systemd/user").join(format!("{}.d", self.gateway_unit)).join("50-vlpds.conf"), "mode": "0644", "contents": self.gateway_ordering(), "when": "always"}),
        ];
        if self.backup_confirmed {
            files.extend([
                json!({"path": "/etc/nginx/sites-available/veles-pds.conf", "mode": "0644", "contents": self.nginx(false)?, "when": "certificate missing"}),
                json!({"path": "/etc/nginx/sites-available/veles-pds.conf", "mode": "0644", "contents": self.nginx(true)?, "when": "certificate ready"}),
                json!({"link": "/etc/nginx/sites-enabled/veles-pds.conf", "target": "/etc/nginx/sites-available/veles-pds.conf", "when": "before ACME (port 80) and after"}),
            ]);
        }
        Ok(json!({
            "files": files,
            "commands": commands,
            "activation_conditional_on": "offline KEK and PLC-key backup acknowledgement",
            "health_url": format!("{}/xrpc/_health", self.public_url.trim_end_matches('/')),
            "ownership": "secret directory and files map container uid/gid 10001 through the gateway account's live /etc/subuid and /etc/subgid ranges; units belong to the gateway account",
            "service_commands": "runuser supplies the gateway account's systemd user bus; no shell or arbitrary argv is accepted from the request"
        }))
    }

    #[cfg(unix)]
    fn preview_home(&self) -> Result<PathBuf> {
        Ok(crate::ownership::account(&self.gateway_user, None)?.home)
    }

    #[cfg(not(unix))]
    fn preview_home(&self) -> Result<PathBuf> {
        anyhow::bail!("PDS host components require a Unix rootless account")
    }

    fn gateway_ordering(&self) -> String {
        "# PDS must be ready before the gateway's atproto adapter.\n[Unit]\nWants=vlpds.service\nAfter=vlpds.service\n".into()
    }

    /// The gateway account's user bus, reached by runuser from the root
    /// handoff. The uid is resolved when the argv is built, never taken
    /// from the request.
    fn rootless_argv(&self, args: &[&str]) -> Vec<String> {
        let uid = self.account_uid().unwrap_or(u32::MAX);
        let mut out = vec![
            "runuser".into(),
            "-u".into(),
            self.gateway_user.clone(),
            "--".into(),
            "env".into(),
            format!("XDG_RUNTIME_DIR=/run/user/{uid}"),
            format!("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus"),
        ];
        out.extend(args.iter().map(|arg| (*arg).to_string()));
        out
    }

    #[cfg(unix)]
    fn account_uid(&self) -> Option<u32> {
        crate::ownership::account(&self.gateway_user, None)
            .ok()
            .map(|a| a.uid)
    }

    #[cfg(not(unix))]
    fn account_uid(&self) -> Option<u32> {
        None
    }

    fn wrap_plc_argv(&self) -> Vec<String> {
        self.rootless_argv(&[
            "podman",
            "run",
            "--rm",
            "--interactive",
            "--pull=never",
            "--network=none",
            "--user=10001:10001",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--volume",
            &format!(
                "{}:/run/vlpds:ro",
                self.state_root.join("secrets").display()
            ),
            &self.image.reference(),
            "--wrap-plc-rotation-key",
            "--kek-file=/run/vlpds/kek",
        ])
    }

    /// Prepare PDS-owned files under an explicit test/CLI root, as its own
    /// transaction. Never pulls, starts a unit or edits DNS. Existing
    /// secrets are kept; a later failure removes only the unit files.
    #[cfg(unix)]
    pub fn apply_files(
        &self,
        root: &Path,
        secrets: &std::collections::BTreeMap<String, String>,
    ) -> Result<Value> {
        absolute_path(root, "host file root")?;
        anyhow::ensure!(
            self.state_root.starts_with(root) && self.state_root != root,
            "PDS state root must be inside the explicit host file root"
        );
        let mut commands = NoCommands;
        let mut txn = apply::HostTxn::new(root, &mut commands);
        let result = self.files_phase(&mut txn, secrets);
        txn.finish(result)
    }

    #[cfg(unix)]
    fn files_phase(
        &self,
        txn: &mut apply::HostTxn<'_>,
        secrets: &std::collections::BTreeMap<String, String>,
    ) -> Result<Value> {
        let quadlet = self.quadlet()?;
        let root = txn.root().to_path_buf();
        anyhow::ensure!(
            self.state_root.starts_with(&root) && self.state_root != root,
            "PDS state root must be inside the explicit host file root"
        );
        let account = crate::ownership::account(&self.gateway_user, None)?;
        let secret_dir = self.state_root.join("secrets");
        ensure_real_directory(&root, &secret_dir)?;
        veles_core::platform::private_dir(&secret_dir)?;
        for (key, file) in [
            ("VLPDS_S3_ACCESS_KEY", "s3-access-key"),
            ("VLPDS_S3_SECRET_KEY", "s3-secret-key"),
        ] {
            let value = secrets
                .get(key)
                .filter(|value| !value.is_empty())
                .with_context(|| format!("missing required secret {key}"))?;
            create_private_if_missing(&secret_dir.join(file), value.as_bytes())?;
        }
        for file in ["kek", "jwt-secret", "admin-token", "internal-token"] {
            let value = veles_core::challenge::random_hex(32)
                .context("secure random source unavailable for PDS secret")?;
            create_private_if_missing(&secret_dir.join(file), value.as_bytes())?;
        }
        // The wrapped PLC key is generated by the pinned vlpds command in
        // the runtime phase. Never replace it with a bare random token: it
        // is bound to the KEK and has a public identity.
        let home = account.home.clone();
        txn.write_under_home(
            &home,
            &home.join(".config/containers/systemd/vlpds.container"),
            quadlet.as_bytes(),
            &account,
        )?;
        txn.write_under_home(
            &home,
            &home
                .join(".config/systemd/user")
                .join(format!("{}.d", self.gateway_unit))
                .join("50-vlpds.conf"),
            self.gateway_ordering().as_bytes(),
            &account,
        )?;
        Ok(json!({
            "applied": true,
            "status": "files-prepared",
            "backup_required": true,
            "service_running": false,
            "image_pulled": false,
            "dns_changed": false,
            "note": "PDS files and missing secrets were prepared. Back up the KEK and PLC rotation key offline before activation. No service or health check was run."
        }))
    }

    /// Assign host ownership after the reviewed files exist. The vlpds
    /// container reads uid 10001, mapped through this account's actual
    /// subuid range. Cache belongs to the gateway account.
    #[cfg(unix)]
    fn assign_ownership(&self, root: &Path) -> Result<Value> {
        let account = crate::ownership::account(&self.gateway_user, None)?;
        let mapped = account.container_uid_10001(&self.gateway_user, Path::new("/etc/subuid"))?;
        let mapped_gid =
            account.container_gid_10001(&self.gateway_user, Path::new("/etc/subgid"))?;
        let secret_owner = crate::ownership::Account {
            uid: mapped,
            gid: mapped_gid,
            home: account.home.clone(),
        };
        let secrets = self.state_root.join("secrets");
        ensure_real_directory(root, &secrets)?;
        crate::ownership::ensure_dir(&secrets, 0o700, &secret_owner)?;
        for entry in std::fs::read_dir(&secrets)? {
            let path = entry?.path();
            crate::ownership::chown(&path, &secret_owner)?;
        }
        let cache = self.state_root.join("cache");
        ensure_real_directory(root, &cache)?;
        crate::ownership::chown_reviewed_tree(&cache, &account)?;
        Ok(json!({
            "applied": true,
            "ownership_assigned": true,
            "container_uid": 10001,
            "mapped_host_uid": mapped,
            "service_running": false,
        }))
    }

    #[cfg(unix)]
    fn apply(
        &self,
        txn: &mut apply::HostTxn<'_>,
        secrets: &std::collections::BTreeMap<String, String>,
    ) -> Result<Value> {
        let root = txn.root().to_path_buf();
        let mut report = self.files_phase(txn, secrets)?;
        txn.commands()
            .run(&self.rootless_argv(&["podman", "pull", "-q", &self.image.reference()]))
            .context("pulling the reviewed PDS image as the gateway account")?;
        report["image_pulled"] = json!(true);
        report["ownership"] = self.assign_ownership(&root)?;
        report["plc"] = if self.state_root.join("secrets/plc-rotation.key").is_file() {
            json!({
                "status": "plc-key-retained",
                "backup_required": true,
                "note": "The existing PLC rotation key was retained; no key was rotated or printed."
            })
        } else {
            self.wrap_plc_key(&root, CommandsAsSecret(txn.commands()))?
        };
        report["status"] = json!("runtime-prepared");
        if !self.backup_confirmed {
            report["note"] = json!("The pinned image and PDS runtime files are prepared. Back up the KEK and PLC rotation key offline, then review a new request with backup_confirmed for TLS and activation. No health check was run.");
            return Ok(report);
        }
        report["tls"] = self.tls_phase(txn)?;
        report["activation"] = self.activate(txn)?;
        report["status"] = json!("healthy");
        report["service_running"] = json!(true);
        report["backup_required"] = json!(false);
        Ok(report)
    }

    /// Public PDS files and the TLS vhost are separate from secret state.
    /// The port-80 vhost is ENABLED before ACME so HTTP-01 can be served.
    #[cfg(unix)]
    fn tls_phase(&self, txn: &mut apply::HostTxn<'_>) -> Result<Value> {
        anyhow::ensure!(
            self.backup_confirmed,
            "offline KEK and PLC-key backup acknowledgement is required before public TLS setup"
        );
        apply::tls_site(
            txn,
            &apply::Site {
                name: "veles-pds",
                domain: &self.handle_domain,
                webroot: "/var/www/veles-pds-acme",
                acme_email: &self.acme_email,
                http_vhost: self.nginx(false)?,
                tls_vhost: self.nginx(true)?,
            },
        )?;
        Ok(json!({
            "applied": true,
            "status": "tls-ready",
            "acme": "http-01",
            "dns_records": self.dns_records()?,
            "nginx_validated": true,
        }))
    }

    /// Start the reviewed user units only after the explicit backup gate
    /// and TLS setup. Health is an observed HTTP response, never a synonym
    /// for a zero exit status from systemctl.
    #[cfg(unix)]
    fn activate(&self, txn: &mut apply::HostTxn<'_>) -> Result<Value> {
        let account = crate::ownership::account(&self.gateway_user, None)?;
        txn.run_with_undo(
            &self.rootless_argv(&["systemctl", "--user", "daemon-reload"]),
            Some(apply::Undo::Argv(self.rootless_argv(&[
                "systemctl",
                "--user",
                "daemon-reload",
            ]))),
        )?;
        let was_active = txn.commands().query(&self.rootless_argv(&[
            "systemctl",
            "--user",
            "is-active",
            "--quiet",
            "vlpds.service",
        ]))?;
        txn.run_with_undo(
            &self.rootless_argv(&["systemctl", "--user", "start", "vlpds.service"]),
            (!was_active).then(|| {
                apply::Undo::Argv(self.rootless_argv(&[
                    "systemctl",
                    "--user",
                    "stop",
                    "vlpds.service",
                ]))
            }),
        )?;
        txn.commands().run(&self.rootless_argv(&[
            "systemctl",
            "--user",
            "restart",
            &self.gateway_unit,
        ]))?;
        let health_url = format!("{}/xrpc/_health", self.public_url.trim_end_matches('/'));
        let probe = argv(&[
            "curl",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "10",
            &health_url,
        ]);
        let mut healthy = false;
        for attempt in 0..12 {
            if attempt > 0 {
                txn.commands().pause(5);
            }
            if txn.commands().query(&probe)? {
                healthy = true;
                break;
            }
        }
        let _ = account;
        anyhow::ensure!(
            healthy,
            "PDS did not become healthy at {health_url}; inspect its unit and TLS before use"
        );
        Ok(json!({
            "activated": true,
            "service_running": true,
            "health_url": health_url,
            "health_verified": true,
        }))
    }

    /// Generate a new PLC rotation key with vlpds's own wrapping command.
    /// The caller must have pulled the exact pinned image beforehand.
    /// Existing keys are never rotated by this installer phase.
    pub fn wrap_plc_key<R: SecretCommandRunner>(
        &self,
        root: &Path,
        mut runner: R,
    ) -> Result<Value> {
        if root != Path::new("/") {
            absolute_path(root, "host file root")?;
        }
        let secrets = self.state_root.join("secrets");
        ensure_real_directory(root, &secrets)?;
        let kek = secrets.join("kek");
        let kek_meta = std::fs::symlink_metadata(&kek)
            .context("PDS KEK is missing; prepare its files before wrapping the PLC key")?;
        anyhow::ensure!(
            kek_meta.is_file() && !kek_meta.file_type().is_symlink(),
            "PDS KEK is not a regular file"
        );
        let destination = secrets.join("plc-rotation.key");
        anyhow::ensure!(
            std::fs::symlink_metadata(&destination).is_err(),
            "PLC rotation key already exists; refusing to rotate it"
        );
        let argv = self.wrap_plc_argv();
        let (wrapped, public) = runner.run_secret(&argv)?;
        anyhow::ensure!(
            wrapped.starts_with(b"vw1."),
            "vlpds did not return a wrapped PLC key"
        );
        let public = String::from_utf8(public).context("vlpds public key output is not UTF-8")?;
        let did = public
            .split_whitespace()
            .find(|word| word.starts_with("did:key:"))
            .map(str::to_string)
            .context("vlpds did not report the PLC public did:key")?;
        create_private_if_missing(&destination, &wrapped)?;
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(&secrets) {
            // The key must be readable by the same mapped container uid as
            // the other secrets in its directory.
            use std::os::unix::fs::MetadataExt;
            let owner = crate::ownership::Account {
                uid: meta.uid(),
                gid: meta.gid(),
                home: PathBuf::from("/"),
            };
            if unsafe { libc::geteuid() } == 0 {
                crate::ownership::chown(&destination, &owner)?;
            }
        }
        Ok(json!({
            "applied": true,
            "status": "plc-key-generated",
            "plc_did_key": did,
            "backup_required": true,
            "service_running": false,
            "note": "The pinned vlpds image generated a KEK-wrapped PLC rotation key. Back it up offline before activation."
        }))
    }

    /// DNS records the operator must publish. A proxy/CDN must be disabled:
    /// the PDS firehose and repository uploads are not proxied by veles.
    pub fn dns_records(&self) -> Result<Vec<DnsRecord>> {
        self.validate()?;
        Ok(vec![
            DnsRecord {
                name: self.handle_domain.clone(),
                kind: "A/AAAA".into(),
                proxied: false,
            },
            DnsRecord {
                name: format!("*.{}", self.handle_domain),
                kind: "A/AAAA".into(),
                proxied: false,
            },
        ])
    }

    /// A port-80-only vhost is required until ACME has issued the
    /// certificate. Enabling a TLS vhost with missing files would break
    /// unrelated nginx sites, so the caller chooses this phase explicitly.
    pub fn nginx(&self, certificate_ready: bool) -> Result<String> {
        self.validate()?;
        let domain = &self.handle_domain;
        if !certificate_ready {
            return Ok(format!(
                r#"server {{
    listen 80;
    listen [::]:80;
    server_name {domain} *.{domain};
    location /.well-known/acme-challenge/ {{
        root /var/www/veles-pds-acme;
        try_files $uri =404;
    }}
    location / {{ return 301 https://$host$request_uri; }}
}}
"#
            ));
        }
        Ok(format!(
            r#"server {{
    listen 80;
    listen [::]:80;
    server_name {domain} *.{domain};
    location /.well-known/acme-challenge/ {{
        root /var/www/veles-pds-acme;
        try_files $uri =404;
    }}
    location / {{ return 301 https://$host$request_uri; }}
}}
server {{
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name {domain} *.{domain};
    ssl_certificate     /etc/letsencrypt/live/{domain}/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/{domain}/privkey.pem;
    ssl_trusted_certificate /etc/letsencrypt/live/{domain}/chain.pem;
    ssl_session_timeout 1d;
    ssl_session_cache shared:MozSSL:10m;
    ssl_session_tickets off;
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_prefer_server_ciphers off;
    client_max_body_size 50m;
    location ~ ^/(admin|metrics|internal)(/|$) {{ return 404; }}
    location ~ ^/xrpc/vlpds\.admin\. {{ return 404; }}
    location /xrpc/com.atproto.sync.subscribeRepos {{
        proxy_pass http://127.0.0.1:2583;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $http_host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
        proxy_buffering off;
        proxy_read_timeout 86400s;
        proxy_send_timeout 86400s;
    }}
    location / {{
        proxy_pass http://127.0.0.1:2583;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $http_host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
        proxy_buffering off;
        proxy_read_timeout 3600s;
    }}
}}
"#
        ))
    }

    pub fn quadlet(&self) -> Result<String> {
        // Validation is shared with preview; the artifact cannot be built
        // from settings the review would reject.
        self.validate()?;
        let mut out = String::new();
        let _ = writeln!(out, "[Unit]\nDescription=vlpds atproto PDS ({})\nWants=network-online.target\nAfter=network-online.target\nBefore={}\n", self.handle_domain, self.gateway_unit);
        let _ = writeln!(out, "[Container]\nContainerName=vlpds\nImage={}\nPull=never\nNetwork={}\nPublishPort=127.0.0.1:2583:2583\nPublishPort=127.0.0.1:9583:9583", self.image.reference(), self.network);
        let _ = writeln!(
            out,
            "Volume={}/secrets:/run/vlpds:ro\nVolume={}/cache:/var/lib/vlpds/cache",
            self.state_root.display(),
            self.state_root.display()
        );
        for (key, value) in [
            ("VLPDS_PUBLIC_URL", self.public_url.clone()),
            ("VLPDS_HANDLE_DOMAIN", self.handle_domain.clone()),
            (
                "VLPDS_SERVICE_DID",
                format!("did:web:{}", self.handle_domain),
            ),
            ("VLPDS_S3_ENDPOINT", self.s3_endpoint.clone()),
            ("VLPDS_S3_REGION", self.s3_region.clone()),
            ("VLPDS_S3_BUCKET", self.s3_bucket.clone()),
            ("VLPDS_PREFIX", self.s3_prefix.clone()),
            ("VLPDS_NODE_ID", "single".into()),
            ("VLPDS_SHARDS", "1".into()),
            ("VLPDS_LEASE_TTL_MS", "60000".into()),
            ("VLPDS_SLATEDB_MANIFEST_POLL", "60s".into()),
            ("VLPDS_CACHE_DIR", "/var/lib/vlpds/cache".into()),
            ("VLPDS_DISK_CACHE_MB", "4096".into()),
            ("VLPDS_METRICS_LISTEN", "127.0.0.1:9583".into()),
            ("VLPDS_LOG_FORMAT", "json".into()),
            ("VLPDS_INVITE_REQUIRED", "true".into()),
            (
                "VLPDS_APPVIEW",
                "https://api.bsky.app,did:web:api.bsky.app".into(),
            ),
            ("VLPDS_TRUSTED_PROXIES", "127.0.0.1".into()),
        ] {
            let _ = writeln!(out, "Environment={key}={value}");
        }
        for (key, file) in [
            ("VLPDS_S3_ACCESS_KEY_FILE", "s3-access-key"),
            ("VLPDS_S3_SECRET_KEY_FILE", "s3-secret-key"),
            ("VLPDS_JWT_SECRET_FILE", "jwt-secret"),
            ("VLPDS_ADMIN_TOKEN_FILE", "admin-token"),
            ("VLPDS_INTERNAL_TOKEN_FILE", "internal-token"),
            ("VLPDS_KEK_FILE", "kek"),
            ("VLPDS_PLC_ROTATION_KEY_FILE", "plc-rotation.key"),
        ] {
            let _ = writeln!(out, "Environment={key}=/run/vlpds/{file}");
        }
        if let Some(key) = &self.recovery_did_key {
            let _ = writeln!(out, "Environment=VLPDS_PLC_RECOVERY_DID_KEY={key}");
        }
        let _ = writeln!(out, "StopTimeout=90\nHealthCmd=curl -sf http://127.0.0.1:2583/xrpc/_health\nHealthInterval=2m\nHealthOnFailure=kill\n\n[Service]\nRestart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target");
        Ok(out)
    }
}

/// A command boundary that refuses everything: the files-only PDS phase
/// must never run a program.
#[cfg(unix)]
struct NoCommands;

#[cfg(unix)]
impl HostCommands for NoCommands {
    fn run(&mut self, argv: &[String]) -> Result<()> {
        anyhow::bail!("this phase runs no commands: {}", argv.join(" "))
    }
    fn query(&mut self, argv: &[String]) -> Result<bool> {
        anyhow::bail!("this phase runs no commands: {}", argv.join(" "))
    }
    fn output(&mut self, argv: &[String]) -> Result<(Vec<u8>, Vec<u8>)> {
        anyhow::bail!("this phase runs no commands: {}", argv.join(" "))
    }
}

// ── apply: the host transaction ──────────────────────────────────────

/// Apply reviewed host components under `root` ("/" from the CLI handoff;
/// a temporary root in tests) as ONE transaction. Serialized by a lock
/// in a root-owned directory, never in an account's own data directory.
#[cfg(unix)]
pub fn apply_all(
    components: &[HostComponent],
    root: &Path,
    secrets: &std::collections::BTreeMap<String, String>,
    commands: &mut dyn HostCommands,
) -> Result<Value> {
    absolute_path(root, "host root").or_else(|_| {
        anyhow::ensure!(root == Path::new("/"), "host root must be absolute");
        Ok::<(), anyhow::Error>(())
    })?;
    let _lock = crate::OperationLock::acquire(&root.join("var/lib/veles-install/host"))?;
    let mut txn = apply::HostTxn::new(root, commands);
    let result = (|| -> Result<Vec<Value>> {
        let mut reports = Vec::new();
        for component in components {
            let report = match component {
                HostComponent::Pds(options) => options.apply(&mut txn, secrets),
                HostComponent::Media(options) => options.apply(&mut txn),
                HostComponent::PoolHub(options) => options.apply(&mut txn),
                HostComponent::PoolMember(options) => options.apply(&mut txn),
            }
            .with_context(|| format!("host component {}", component.name()))?;
            reports.push(report);
        }
        Ok(reports)
    })();
    let reports = txn.finish(result)?;
    Ok(json!({
        "ok": true,
        "status": "applied",
        "applied": true,
        "host_components": reports,
        "dns_changed": false,
    }))
}

#[cfg(unix)]
impl MediaOptions {
    fn apply(&self, txn: &mut apply::HostTxn<'_>) -> Result<Value> {
        let domain = self.domain()?;
        let account = crate::ownership::account(&self.gateway_user, None)?;
        let publish = txn.host(&self.publish_dir);
        match std::fs::symlink_metadata(&publish) {
            Ok(meta) => {
                use std::os::unix::fs::MetadataExt;
                anyhow::ensure!(
                    meta.is_dir() && !meta.file_type().is_symlink(),
                    "media publication path is not a real directory"
                );
                anyhow::ensure!(
                    meta.uid() == account.uid,
                    "the existing media directory {} does not belong to {}; the gateway could not publish into it. Nothing was changed",
                    self.publish_dir.display(),
                    self.gateway_user
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                txn.mkdirs(&self.publish_dir, 0o755, &account)?;
            }
            Err(error) => return Err(error.into()),
        }
        apply::tls_site(
            txn,
            &apply::Site {
                name: "veles-media",
                domain: &domain,
                webroot: "/var/www/veles-media-acme",
                acme_email: &self.acme_email,
                http_vhost: self.nginx(false, &domain)?,
                tls_vhost: self.nginx(true, &domain)?,
            },
        )?;
        // The probe proves the chain this host serves: a file the gateway
        // account could have published, fetched back over the TLS vhost.
        let name = format!(
            "veles-media-probe-{}.txt",
            veles_core::challenge::random_hex(8).context("random probe name")?
        );
        let content = format!("veles media probe {name}\n");
        let probe = publish.join(&name);
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)
                .context("writing the media probe file")?;
            file.write_all(content.as_bytes())?;
        }
        crate::ownership::chown(&probe, &account)?;
        std::fs::set_permissions(&probe, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
        let fetched = txn.commands().output(&argv(&[
            "curl",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "10",
            "--resolve",
            &format!("{domain}:443:127.0.0.1"),
            &format!("https://{domain}/{name}"),
        ]));
        let _ = std::fs::remove_file(&probe);
        let (body, _) = fetched.context("fetching the media probe over this host's TLS vhost")?;
        anyhow::ensure!(
            body == content.as_bytes(),
            "the media vhost answered, but not with the probe file"
        );
        Ok(json!({
            "component": "local-media",
            "applied": true,
            "status": "media-serving-verified",
            "nginx_validated": true,
            "probe_verified": true,
            "public_url": self.public_url,
            "existing_media_preserved": true,
            "dns_changed": false,
            "gateway_configuration": self.gateway_configuration()?,
            "next": "Add the [platforms.irc] keys above to the gateway's config.toml and restart the gateway. A probe was fetched over https from this host (resolved to 127.0.0.1); public DNS reachability is the operator's check."
        }))
    }
}

#[cfg(unix)]
impl PoolHubOptions {
    fn apply(&self, txn: &mut apply::HostTxn<'_>) -> Result<Value> {
        self.validate()?;
        let gateway = crate::ownership::account(&self.gateway_user, None)?;
        let data = txn.host(&self.gateway_data_dir);
        let meta = std::fs::symlink_metadata(&data).with_context(|| {
            format!(
                "the gateway data directory {} must exist (apply the server profile first)",
                self.gateway_data_dir.display()
            )
        })?;
        {
            use std::os::unix::fs::MetadataExt;
            anyhow::ensure!(
                meta.is_dir() && !meta.file_type().is_symlink() && meta.uid() == gateway.uid,
                "the gateway data directory must be a real directory owned by {}",
                self.gateway_user
            );
        }
        let rootacct = crate::ownership::process_account();
        txn.mkdirs(Path::new("/etc/wireguard"), 0o700, &rootacct)?;
        let conf = self.conf_path();
        let public_key = match std::fs::read_to_string(txn.host(&conf)) {
            Ok(existing) => {
                // Never rotate an existing hub key: every member's tunnel
                // names it. The interface section must agree with the review.
                let mut private = None;
                let mut address = None;
                let mut port = None;
                for line in existing.lines().map(str::trim) {
                    if line.starts_with('[') && line != "[Interface]" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once('=') {
                        match key.trim() {
                            "PrivateKey" => private = Some(value.trim().to_string()),
                            "Address" => address = Some(value.trim().to_string()),
                            "ListenPort" => port = Some(value.trim().to_string()),
                            _ => {}
                        }
                    }
                }
                anyhow::ensure!(
                    address.as_deref() == Some(&format!("{}/24", self.address))
                        && port.as_deref() == Some(&self.listen_port.to_string()),
                    "{} exists with a different Address/ListenPort; review it by hand (its key is never rotated)",
                    conf.display()
                );
                veles_core::pool::wg_public_b64(
                    private
                        .as_deref()
                        .context("the existing hub config has no PrivateKey")?,
                )?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let (private, public) = veles_core::pool::wg_keypair()?;
                txn.write(
                    &conf,
                    format!(
                        "# Installed by `veles install host-component` (pool-hub).\n# veles-wg-peers appends the pool's peers below.\n[Interface]\nAddress = {}/24\nListenPort = {}\nPrivateKey = {}\n",
                        self.address, self.listen_port, private
                    )
                    .as_bytes(),
                    0o600,
                    &rootacct,
                )?;
                public
            }
            Err(error) => return Err(error.into()),
        };
        txn.write(
            Path::new("/usr/local/sbin/veles-wg-peers"),
            WG_PEERS_HELPER.as_bytes(),
            0o755,
            &rootacct,
        )?;
        txn.write(
            Path::new("/etc/systemd/system/veles-wg-peers.service"),
            self.service_unit().as_bytes(),
            0o644,
            &rootacct,
        )?;
        txn.write(
            Path::new("/etc/systemd/system/veles-wg-peers.path"),
            self.path_unit().as_bytes(),
            0o644,
            &rootacct,
        )?;
        txn.mkdirs(&self.gateway_data_dir.join("pool"), 0o700, &gateway)?;
        txn.mkdirs(&self.queue(), 0o700, &gateway)?;
        txn.run_with_undo(
            &argv(&["systemctl", "daemon-reload"]),
            Some(apply::Undo::Argv(argv(&["systemctl", "daemon-reload"]))),
        )?;
        let wg_unit = format!("wg-quick@{}.service", self.interface);
        txn.enable_now(&wg_unit)?;
        txn.enable_now("veles-wg-peers.path")?;
        let (shown, _) =
            txn.commands()
                .output(&argv(&["wg", "show", &self.interface, "public-key"]))?;
        anyhow::ensure!(
            String::from_utf8_lossy(&shown).trim() == public_key,
            "the running {} does not carry the configured hub key",
            self.interface
        );
        Ok(json!({
            "component": "pool-hub",
            "applied": true,
            "status": "hub-up",
            "interface": self.interface,
            "public_key": public_key,
            "gateway_configuration": self.gateway_configuration(&public_key),
            "next": format!("Add the [pool.wireguard] section above to the gateway's config.toml, with [cluster] arbiter = <this node id>, [node] endpoint = \"http://{}:9900\" and [platforms.a2a] enabled = true, [platforms.a2a.extra] host = \"{}\" (docs/pool.md). A2A binds beyond 127.0.0.1 only with a token: until the pool has peers, put A2A_BEARER_TOKEN=<random> in the gateway's .env. Restart the gateway, then `veles pool invite <name> --wireguard-file <file>`. UDP {} must reach this host.", self.address, self.address, self.listen_port)
        }))
    }
}

#[cfg(unix)]
impl PoolMemberOptions {
    fn apply(&self, txn: &mut apply::HostTxn<'_>) -> Result<Value> {
        let (client, _) = self.read()?;
        let rootacct = crate::ownership::process_account();
        txn.mkdirs(Path::new("/etc/wireguard"), 0o700, &rootacct)?;
        txn.write(
            &self.conf_path(),
            client.render().as_bytes(),
            0o600,
            &rootacct,
        )?;
        let unit = format!("wg-quick@{}.service", self.interface);
        txn.enable_now(&unit)?;
        let mut handshake = false;
        for attempt in 0..8 {
            if attempt > 0 {
                txn.commands().pause(2);
            }
            let (shown, _) = txn.commands().output(&argv(&[
                "wg",
                "show",
                &self.interface,
                "latest-handshakes",
            ]))?;
            if String::from_utf8_lossy(&shown)
                .split_whitespace()
                .nth(1)
                .is_some_and(|stamp| stamp != "0")
            {
                handshake = true;
                break;
            }
        }
        Ok(json!({
            "component": "pool-member",
            "applied": true,
            "status": "member-up",
            "interface": self.interface,
            "address": format!("{}/32", client.address),
            "handshake_observed": handshake,
            "next": if handshake {
                "The tunnel answers. Run `veles pool join '<invite text>'` as the gateway's account, then restart its gateway. Delete the saved client config file."
            } else {
                "The interface is up but no handshake was seen yet: the hub applies its side when its peer queue changes. Check `wg show` on both ends, then run `veles pool join`."
            }
        }))
    }
}

#[cfg(unix)]
pub(crate) mod apply {
    use super::{argv, HostCommands};
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};

    /// A compensating action for a service effect that file recovery does
    /// not undo. Executed in reverse order after the files are restored.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Undo {
        Argv(Vec<String>),
    }

    pub(crate) struct HostTxn<'a> {
        root: PathBuf,
        commands: &'a mut dyn HostCommands,
        recovery: crate::Recovery,
        undo: Vec<Undo>,
    }

    impl<'a> HostTxn<'a> {
        pub(crate) fn new(root: &Path, commands: &'a mut dyn HostCommands) -> Self {
            Self {
                root: root.to_path_buf(),
                commands,
                recovery: crate::Recovery::capture(Vec::<PathBuf>::new())
                    .expect("an empty recovery cannot fail"),
                undo: Vec::new(),
            }
        }

        pub(crate) fn root(&self) -> &Path {
            &self.root
        }

        pub(crate) fn commands(&mut self) -> &mut dyn HostCommands {
            &mut *self.commands
        }

        /// The host path for an absolute logical path.
        pub(crate) fn host(&self, path: &Path) -> PathBuf {
            self.root.join(path.strip_prefix("/").unwrap_or(path))
        }

        /// Create every missing directory of `path` (logical, absolute):
        /// the last with `mode` and `owner`, parents 0755 as this process.
        /// Each one created is tracked, so a failure removes it again if
        /// it is still empty of anything foreign.
        pub(crate) fn mkdirs(
            &mut self,
            path: &Path,
            mode: u32,
            owner: &crate::ownership::Account,
        ) -> Result<()> {
            super::absolute_path(path, "host directory")?;
            let mut logical = PathBuf::from("/");
            let parts: Vec<_> = path.components().skip(1).collect();
            for (n, part) in parts.iter().enumerate() {
                logical.push(part);
                let host = self.host(&logical);
                match std::fs::symlink_metadata(&host) {
                    Ok(meta) => anyhow::ensure!(
                        meta.is_dir() && !meta.file_type().is_symlink(),
                        "host directory is a symlink or special file: {}",
                        logical.display()
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        self.recovery.track(&host)?;
                        let last = n + 1 == parts.len();
                        let account = crate::ownership::process_account();
                        crate::ownership::ensure_dir(
                            &host,
                            if last { mode } else { 0o755 },
                            if last { owner } else { &account },
                        )?;
                        std::fs::set_permissions(
                            &host,
                            std::os::unix::fs::PermissionsExt::from_mode(if last {
                                mode
                            } else {
                                0o755
                            }),
                        )?;
                        self.recovery.mark_after(&host)?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(())
        }

        /// Write one reviewed file. A missing file is created; an identical
        /// one is left; a DIFFERENT existing one is replaced only because
        /// it is captured first (and restored on failure).
        pub(crate) fn write(
            &mut self,
            path: &Path,
            contents: &[u8],
            mode: u32,
            owner: &crate::ownership::Account,
        ) -> Result<()> {
            super::absolute_path(path, "host file")?;
            if let Some(parent) = path.parent() {
                let account = crate::ownership::process_account();
                self.mkdirs(parent, 0o755, &account)?;
            }
            let host = self.host(path);
            self.recovery.track(&host)?;
            // Replacing a different existing file is allowed only because it
            // was captured on the line above; name its exact bytes.
            let existing = match std::fs::symlink_metadata(&host) {
                Ok(meta) if meta.is_file() => Some(crate::sha256(&std::fs::read(&host)?)),
                _ => None,
            };
            crate::Action::WriteFile {
                path: path.strip_prefix("/")?.to_path_buf(),
                contents: contents.to_vec(),
                mode: Some(mode),
                expected_sha256: existing,
            }
            .apply_privileged(&self.root)?;
            crate::ownership::chown(&host, owner)?;
            self.recovery.mark_after(&host)
        }

        /// A file under an account's home: missing directories between the
        /// home and the file are created as that account, so its own
        /// systemd/podman can read them. An existing different file is
        /// refused, never replaced: it is the operator's.
        pub(crate) fn write_under_home(
            &mut self,
            home: &Path,
            path: &Path,
            contents: &[u8],
            owner: &crate::ownership::Account,
        ) -> Result<()> {
            anyhow::ensure!(path.starts_with(home), "file is outside the account home");
            let host_home = self.host(home);
            let meta = std::fs::symlink_metadata(&host_home)
                .with_context(|| format!("the account home {} must exist", home.display()))?;
            anyhow::ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "the account home is not a real directory"
            );
            let host = self.host(path);
            match std::fs::read(&host) {
                Ok(existing) => {
                    anyhow::ensure!(
                        existing == contents,
                        "existing host file {} differs; review its replacement explicitly",
                        path.display()
                    );
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            if let Some(parent) = path.parent() {
                let mut logical = home.to_path_buf();
                for part in parent.strip_prefix(home)?.components() {
                    logical.push(part);
                    let host_dir = self.host(&logical);
                    match std::fs::symlink_metadata(&host_dir) {
                        Ok(meta) => anyhow::ensure!(
                            meta.is_dir() && !meta.file_type().is_symlink(),
                            "host directory is a symlink or special file: {}",
                            logical.display()
                        ),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            self.recovery.track(&host_dir)?;
                            crate::ownership::ensure_dir(&host_dir, 0o755, owner)?;
                            self.recovery.mark_after(&host_dir)?;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            self.write(path, contents, 0o644, owner)
        }

        pub(crate) fn link(&mut self, target: &Path, link: &Path) -> Result<()> {
            if let Some(parent) = link.parent() {
                let account = crate::ownership::process_account();
                self.mkdirs(parent, 0o755, &account)?;
            }
            let host = self.host(link);
            self.recovery.track(&host)?;
            crate::profiles::SystemStep::Link {
                target: target.to_path_buf(),
                link: link.to_path_buf(),
            }
            .apply_link(&self.root)?;
            self.recovery.mark_after(&host)
        }

        pub(crate) fn run_with_undo(&mut self, argv: &[String], undo: Option<Undo>) -> Result<()> {
            self.commands.run(argv)?;
            if let Some(undo) = undo {
                self.undo.push(undo);
            }
            Ok(())
        }

        /// `systemctl enable --now`, undone (disable --now) on a later
        /// failure only when this run is what enabled it.
        pub(crate) fn enable_now(&mut self, unit: &str) -> Result<()> {
            let enabled =
                self.commands
                    .query(&argv(&["systemctl", "is-enabled", "--quiet", unit]))?;
            if enabled {
                return self.commands.run(&argv(&["systemctl", "start", unit]));
            }
            self.run_with_undo(
                &argv(&["systemctl", "enable", "--now", unit]),
                Some(Undo::Argv(argv(&["systemctl", "disable", "--now", unit]))),
            )
        }

        /// Restore files on failure, then compensate service effects. A
        /// failed compensation is reported in the error, never hidden.
        pub(crate) fn finish<T>(self, result: Result<T>) -> Result<T> {
            let Self {
                commands,
                recovery,
                undo,
                ..
            } = self;
            match recovery.finish(result) {
                Ok(value) => Ok(value),
                Err(error) => {
                    let mut failed = Vec::new();
                    for step in undo.iter().rev() {
                        let Undo::Argv(argv) = step;
                        if let Err(e) = commands.run(argv) {
                            failed.push(format!("{}: {e:#}", argv.join(" ")));
                        }
                    }
                    if failed.is_empty() {
                        Err(error.context(format!(
                            "host service effects were compensated ({} step(s)); generated secrets and pulled images, if any, were kept",
                            undo.len()
                        )))
                    } else {
                        Err(error
                            .context(format!("COMPENSATION INCOMPLETE: {}", failed.join("; "))))
                    }
                }
            }
        }
    }

    pub(crate) struct Site<'a> {
        pub name: &'a str,
        pub domain: &'a str,
        pub webroot: &'a str,
        pub acme_email: &'a str,
        pub http_vhost: String,
        pub tls_vhost: String,
    }

    fn nginx_reload(txn: &mut HostTxn<'_>) -> Result<()> {
        txn.commands()
            .run(&argv(&["nginx", "-t"]))
            .context("nginx -t rejected the configuration; it was not reloaded")?;
        let undo = Undo::Argv(argv(&["systemctl", "reload", "nginx.service"]));
        let already = txn.undo.contains(&undo);
        txn.run_with_undo(
            &argv(&["systemctl", "reload", "nginx.service"]),
            (!already).then_some(undo),
        )
    }

    /// The shared nginx/TLS sub-component (PLAN-plugins-installer.md Phase
    /// 8: "nginx and TLS are a shared sub-component of the PDS, media and
    /// the viewer"). Port 80 is enabled first so HTTP-01 can be served; a
    /// TLS vhost is written only once the certificate files exist, because
    /// one enabled vhost naming a missing certificate fails every site.
    pub(crate) fn tls_site(txn: &mut HostTxn<'_>, site: &Site<'_>) -> Result<()> {
        let root = crate::ownership::process_account();
        txn.mkdirs(Path::new(site.webroot), 0o755, &root)?;
        let available = PathBuf::from(format!("/etc/nginx/sites-available/{}.conf", site.name));
        let enabled = PathBuf::from(format!("/etc/nginx/sites-enabled/{}.conf", site.name));
        let live = PathBuf::from("/etc/letsencrypt/live").join(site.domain);
        let ready = |txn: &HostTxn<'_>| {
            ["fullchain.pem", "privkey.pem", "chain.pem"]
                .iter()
                .all(|file| txn.host(&live.join(file)).is_file())
        };
        if !ready(txn) {
            txn.write(&available, site.http_vhost.as_bytes(), 0o644, &root)?;
            txn.link(&available, &enabled)?;
            nginx_reload(txn)?;
            txn.commands()
                .run(&argv(&[
                    "certbot",
                    "certonly",
                    "--webroot",
                    "-w",
                    site.webroot,
                    "-d",
                    site.domain,
                    "--non-interactive",
                    "--agree-tos",
                    "-m",
                    site.acme_email,
                    "--keep-until-expiring",
                ]))
                .with_context(|| {
                    format!(
                        "ACME HTTP-01 for {} failed (the name must resolve to this host and port 80 must reach it); no TLS vhost was enabled",
                        site.domain
                    )
                })?;
            anyhow::ensure!(
                ready(txn),
                "certbot succeeded but {} has no fullchain/privkey/chain",
                live.display()
            );
        }
        txn.write(&available, site.tls_vhost.as_bytes(), 0o644, &root)?;
        txn.link(&available, &enabled)?;
        nginx_reload(txn)
    }
}

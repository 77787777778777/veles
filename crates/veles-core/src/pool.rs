//! Joining the pool by scanning a QR code (PLAN-sessions-and-phone.md
//! P2d; USER decisions D8–D10, 2026-10-11).
//!
//! Until this, a node joined by hand: two tokens per link (one each way),
//! an `[a2a_agents.X]` entry on both sides, the `[cluster]` peer lists,
//! `A2A_PEER_TOKENS` in each `.env`, and on a phone all of it typed over
//! `adb run-as … cat >`. Now:
//!
//! 1. **Invite, on the arbiter** (`veles pool invite <name>`). It mints a
//!    one-time invite: the new node's bearer at the arbiter and the
//!    arbiter's at the new node, valid [`DEFAULT_TTL_SECS`], single use.
//!    With `[pool.wireguard]` set, the hub also mints the new node's
//!    tunnel (D10): a key pair and the next free address. The hub-side
//!    peer is queued for a root helper; the client side is a WireGuard QR.
//! 2. **Scan, on the new node.** The invite is the second QR (D9:
//!    short-lived secrets in the image). The phone's camera, or pasted
//!    text, gives it to [`join`].
//! 3. **Redeem, on the arbiter** ([`redeem`]). The new node's first call
//!    names the invite with its new bearer; until then that bearer opens
//!    nothing else ([`invite_bearer`]). The arbiter writes its own side,
//!    then introduces the node to every member (D8): a fresh token pair
//!    per member, sent over the link the arbiter already has
//!    ([`accept_member`]). It answers with the members, and the new node
//!    writes its side of every link.
//!
//! What joining does NOT widen: what a peer may do. A2A peers run as a
//! User (`peer_principal`) unless the owner names them higher or a ticket
//! says more. Joining widens who can *call*, not what a call can do.
//! Vault admission stays the owner's own step (`veles vault admit`, with
//! a fingerprint read off the other machine).

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::VelesConfig;

/// What every invite's text starts with: the QR's payload is this, then
/// the invite as JSON.
pub const QR_PREFIX: &str = "veles-pool:1:";
/// The new node's redeem call, to the arbiter.
pub const JOIN_MARKER: &str = "[veles pool join]";
/// The arbiter's introduction of a new node, to a member.
pub const MEMBER_MARKER: &str = "[veles pool member]";
/// Every answer to either.
pub const REPLY_MARKER: &str = "[veles pool reply]";
/// How long an invite is good for.
pub const DEFAULT_TTL_SECS: u64 = 600;
/// The `[cluster]` lists a node's peers can be named in.
const LISTS: &[&str] = &[
    "gossip_peers",
    "memsync_peers",
    "task_peers",
    "storesync_peers",
    "migrate_peers",
];

/// A node, as the pool dials it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeRef {
    pub id: String,
    pub name: String,
    pub url: String,
}

/// What the QR carries (D9). Everything a fresh node needs to make its
/// first call to the arbiter, and nothing that outlives the invite: the
/// two bearers are this link's own, minted for it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Invite {
    pub v: u32,
    pub id: String,
    pub secret: String,
    /// What the pool will call the new node.
    pub name: String,
    /// How the pool reaches it: its tunnel address when the hub minted
    /// one, else the URL the owner named.
    pub url: String,
    pub arbiter: NodeRef,
    /// The new node's bearer at the arbiter.
    pub to_arbiter: String,
    /// The arbiter's bearer at the new node.
    pub from_arbiter: String,
    /// Unix seconds.
    pub expires: u64,
    #[serde(default)]
    pub signers: Vec<String>,
    /// `[cluster.ticket_signers]`: name → `ed25519:…` key.
    #[serde(default)]
    pub ticket_signers: BTreeMap<String, String>,
    /// A recovery invite (`veles pool invite <name> --recover`): the
    /// member it re-admits, as this arbiter knows it. The node that
    /// redeems it must ask as that id, which `veles recover --as` gives
    /// it back; the arbiter then rotates every link the member had
    /// (PLAN-recover.md R1). Absent on a plain invite, whose text the
    /// field leaves unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recover: Option<NodeRef>,
}

/// The QR's text.
pub fn encode(invite: &Invite) -> Result<String> {
    Ok(format!("{QR_PREFIX}{}", serde_json::to_string(invite)?))
}

/// An invite from the QR's text (or pasted text with spaces around it).
pub fn decode(text: &str) -> Result<Invite> {
    let t = text.trim();
    if t.starts_with("[Interface]") {
        bail!(
            "{WIREGUARD_CODE}: scan it with the WireGuard app. The veles code is the second one."
        );
    }
    let json = t
        .strip_prefix(QR_PREFIX)
        .with_context(|| format!("{NOT_AN_INVITE} (it should start with {QR_PREFIX})"))?;
    let inv: Invite = serde_json::from_str(json).context("the invite is damaged")?;
    if inv.v != 1 {
        bail!(
            "this invite is version {}; this veles reads version 1",
            inv.v
        );
    }
    Ok(inv)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    use ring::rand::SecureRandom as _;
    let mut b = [0u8; N];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| anyhow::anyhow!("the system's random source failed"))?;
    Ok(b)
}

fn token() -> Result<String> {
    Ok(random_bytes::<24>()?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn sha_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// ── the arbiter's ledger of invites ──────────────────────────────────

/// One invite the arbiter has minted. The bearer the new node will send
/// and the secret are kept as hashes; the arbiter's own bearer at the new
/// node is kept as written (it goes into this node's `[a2a_agents]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pending {
    pub id: String,
    pub name: String,
    pub url: String,
    pub secret_sha: String,
    pub to_arbiter_sha: String,
    pub from_arbiter: String,
    pub expires: u64,
    #[serde(default)]
    pub redeemed_at: Option<u64>,
    #[serde(default)]
    pub wg: Option<WgLease>,
    /// The node id a recovery invite re-admits: its redeem must ask as
    /// it, and while the recovery window is open the peer it re-links
    /// speaks for it ([`recovery_for`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recover: Option<String>,
    /// The node id that redeemed it: which node the peer it created is,
    /// before any gossip says so ([`invited_node`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// The node that redeemed the newest invite for `peer` on this arbiter —
/// how it knows which node a member that joined (or came back) a moment
/// ago is, before its first gossip. `crate::recover` binds a peer by it.
pub fn invited_node(peer: &str) -> Option<String> {
    ledger()
        .into_iter()
        .filter(|p| p.name == peer)
        .filter_map(|p| Some((p.node.or(p.recover)?, p.redeemed_at?)))
        .max_by_key(|(_, at)| *at)
        .map(|(id, _)| id)
}

/// How long after a recovery invite is redeemed the node it re-admitted
/// may pull what a recovery pulls beyond its own data: the peer's skills
/// and commands (`crate::recover`). A day: a recovery that has not
/// finished by then is started again with a new invite.
pub const RECOVERY_WINDOW_SECS: u64 = 24 * 60 * 60;

/// The meta key a member keeps for a peer the arbiter introduced as a
/// recovering node: `<node id> <window end, unix seconds>`.
pub const RECOVERY_META_PREFIX: &str = "veles.recover.peer.";

/// The node a recovering peer named `peer` speaks for, and when its
/// recovery window closes (unix seconds): from this arbiter's ledger
/// (the newest redeemed recovery invite for that name), else from the
/// mark an introduction left in this member's store. `None` when no
/// recovery re-linked that peer here.
pub fn recovery_for(store: &crate::store::Store, peer: &str) -> Option<(String, u64)> {
    let from_ledger = ledger()
        .into_iter()
        .filter(|p| p.name == peer)
        .filter_map(|p| Some((p.recover?, p.redeemed_at?)))
        .max_by_key(|(_, at)| *at)
        .map(|(id, at)| (id, at + RECOVERY_WINDOW_SECS));
    if from_ledger.is_some() {
        return from_ledger;
    }
    let raw: String = store
        .conn()
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            rusqlite::params![format!("{RECOVERY_META_PREFIX}{peer}")],
            |r| r.get(0),
        )
        .ok()?;
    let (id, until) = raw.split_once(' ')?;
    Some((id.to_string(), until.trim().parse().ok()?))
}

/// An address and key the hub handed out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WgLease {
    pub address: String,
    pub public_key: String,
}

fn pool_dir() -> PathBuf {
    crate::config::data_dir().join("pool")
}

fn ledger_path() -> PathBuf {
    pool_dir().join("invites.json")
}

/// One writer at a time through the ledger's read-modify-write: the
/// A2A server redeems invites from concurrent handlers, and two of them
/// reading the same unredeemed row and both saving would redeem one
/// invite twice. Taken for the length of a synchronous block only (never
/// across an await).
fn ledger_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn ledger() -> Vec<Pending> {
    std::fs::read_to_string(ledger_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_private(path: &std::path::Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, text.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn save_ledger(rows: &[Pending]) -> Result<()> {
    write_private(&ledger_path(), &serde_json::to_string_pretty(rows)?)
}

/// The invite whose not-yet-registered bearer this is, while it can
/// still be redeemed: the A2A server lets that one call through, and only
/// a join (`a2a_server`). The bearer opens nothing else.
pub fn invite_bearer(auth_header: Option<&str>) -> Option<String> {
    let tok = auth_header?.strip_prefix("Bearer ")?.trim();
    if tok.is_empty() {
        return None;
    }
    let sha = sha_hex(tok);
    let t = now();
    ledger()
        .into_iter()
        .find(|p| p.to_arbiter_sha == sha && p.redeemed_at.is_none() && p.expires > t)
        .map(|p| p.name)
}

// ── WireGuard (D10) ──────────────────────────────────────────────────

/// A new peer's tunnel.
#[derive(Debug, Clone, PartialEq)]
pub struct WgPeer {
    pub address: Ipv4Addr,
    pub private_key: String,
    pub public_key: String,
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The public key for a private one, as WireGuard computes it (X25519
/// with the base point; the scalar is clamped inside).
pub fn wg_public(private: [u8; 32]) -> [u8; 32] {
    x25519_dalek::x25519(private, x25519_dalek::X25519_BASEPOINT_BYTES)
}

/// The base64 public key for a base64 private key (`wg pubkey`). Refuses
/// anything that is not exactly 32 bytes of standard base64.
pub fn wg_public_b64(private: &str) -> Result<String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(private.trim())
        .context("a WireGuard key is not base64")?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("a WireGuard key is 32 bytes"))?;
    Ok(b64(&wg_public(key)))
}

/// A fresh WireGuard key pair, base64 (private, public), as `wg genkey` and
/// `wg pubkey` would print them. Also the installer's hub key (pool host
/// component), so there is one key generator.
pub fn wg_keypair() -> Result<(String, String)> {
    let mut sk = random_bytes::<32>()?;
    // Clamped as `wg genkey` does, so the stored key reads the same.
    sk[0] &= 248;
    sk[31] &= 127;
    sk[31] |= 64;
    Ok((b64(&sk), b64(&wg_public(sk))))
}

/// The first address in `subnet` (`a.b.c.d/nn`) that is not the network,
/// the broadcast or `taken`.
pub fn next_address(subnet: &str, taken: &[Ipv4Addr]) -> Result<Ipv4Addr> {
    let (net, bits) = subnet
        .trim()
        .split_once('/')
        .with_context(|| format!("[pool.wireguard] subnet {subnet:?} is not a.b.c.d/nn"))?;
    let net: Ipv4Addr = net.parse().context("the subnet's address does not parse")?;
    let bits: u32 = bits.parse().context("the subnet's prefix does not parse")?;
    if !(8..=30).contains(&bits) {
        bail!("a /{bits} subnet has no room for peers");
    }
    let mask = u32::MAX << (32 - bits);
    let base = u32::from(net) & mask;
    let last = base | !mask;
    for a in (base + 1)..last {
        let ip = Ipv4Addr::from(a);
        if !taken.contains(&ip) {
            return Ok(ip);
        }
    }
    bail!("every address in {subnet} is taken")
}

/// The addresses this hub knows are in use: its own, every lease it ever
/// handed out, and every host its peers are dialled at.
fn taken_addresses(cfg: &VelesConfig, rows: &[Pending]) -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    if let Ok(a) = cfg.pool.wireguard.address.parse() {
        out.push(a);
    }
    for r in rows {
        if let Some(Ok(a)) = r.wg.as_ref().map(|w| w.address.parse()) {
            out.push(a);
        }
    }
    let urls = cfg
        .a2a_agents
        .values()
        .map(|p| p.url.clone())
        .chain(cfg.node.endpoint.clone());
    for u in urls {
        if let Some(Ok(a)) = host_of(&u).map(|h| h.parse()) {
            out.push(a);
        }
    }
    out
}

/// The host part of `http://host:port/…`.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let hostport = rest.split('/').next()?;
    let host = hostport
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(hostport);
    (!host.is_empty()).then(|| host.trim_matches(['[', ']']).to_string())
}

/// The WireGuard app's side: what its QR carries.
pub fn wg_client_config(wg: &crate::config::PoolWireguard, peer: &WgPeer) -> String {
    format!(
        "[Interface]\nPrivateKey = {}\nAddress = {}/32\n\n[Peer]\nPublicKey = {}\nEndpoint = {}\nAllowedIPs = {}\nPersistentKeepalive = {}\n",
        peer.private_key, peer.address, wg.public_key, wg.endpoint, wg.subnet, wg.keepalive
    )
}

/// The hub's side, for the root helper to add to the interface.
pub fn wg_hub_stanza(name: &str, peer: &WgPeer) -> String {
    format!(
        "# veles pool: {name}\n[Peer]\nPublicKey = {}\nAllowedIPs = {}/32\n",
        peer.public_key, peer.address
    )
}

/// Where the hub queues peers for its root helper (deploy/veles-wg-peers):
/// `<name>.conf` adds one, `<name>.remove` (its public key) removes one.
pub fn wg_queue_dir() -> PathBuf {
    pool_dir().join("wg-pending")
}

// ── minting ──────────────────────────────────────────────────────────

/// What `veles pool invite` hands the owner.
#[derive(Debug, Clone)]
pub struct Minted {
    pub invite: Invite,
    /// The WireGuard app's config, when the hub minted a tunnel.
    pub wg_client: Option<String>,
    /// Where the hub-side peer was queued.
    pub wg_queued: Option<PathBuf>,
    /// For a recovery invite: how this arbiter knows which node `name`
    /// is (`gossip`, `store sync`, `registry`, `--node`).
    pub recover_via: Option<&'static str>,
}

/// This node, when it is the arbiter, and the endpoint a new node dials it
/// at — what every invite needs. Only the arbiter invites (D8).
fn inviting_arbiter(cfg: &VelesConfig, name: &str) -> Result<(crate::node::NodeIdentity, String)> {
    if !valid_name(name) {
        bail!("a node name is 1-32 of a-z, 0-9 and -: {name:?}");
    }
    let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?;
    match cfg.cluster.arbiter.as_deref() {
        Some(a) if a == me.id || me.id.starts_with(a) => {}
        Some(a) => bail!(
            "only the arbiter invites, and this node is not it: invite from {a} (`veles pool \
             invite {name}` there)"
        ),
        None => bail!("this node has no [cluster] arbiter: a pool needs one before it can invite"),
    }
    let my_url = cfg
        .node
        .endpoint
        .clone()
        .filter(|e| !e.trim().is_empty())
        .context("set [node] endpoint first: the new node dials the arbiter there")?;
    Ok((me, my_url))
}

/// Mint an invite for a node the pool will call `name`. Only the arbiter
/// invites (D8): it is the node that introduces the newcomer to everyone.
/// `url` is how the pool will reach the node; with `[pool.wireguard]` set
/// it defaults to the minted tunnel address on A2A's port.
pub fn mint(cfg: &VelesConfig, name: &str, url: Option<&str>, ttl_secs: u64) -> Result<Minted> {
    let (me, my_url) = inviting_arbiter(cfg, name)?;
    if cfg.a2a_agents.contains_key(name) {
        bail!("{name} is already a peer of this node ([a2a_agents.{name}])");
    }
    let _ledger = ledger_lock();
    let mut rows = ledger();
    let t = now();
    if rows
        .iter()
        .any(|r| r.name == name && r.redeemed_at.is_none() && r.expires > t)
    {
        bail!("an invite for {name} is still open; `veles pool invites --revoke {name}` first");
    }

    let wg = &cfg.pool.wireguard;
    let peer = if wg.enabled() {
        if wg.public_key.trim().is_empty() || wg.endpoint.trim().is_empty() {
            bail!("[pool.wireguard] needs public_key and endpoint to mint a tunnel");
        }
        let (private_key, public_key) = wg_keypair()?;
        let address = next_address(&wg.subnet, &taken_addresses(cfg, &rows))?;
        Some(WgPeer {
            address,
            private_key,
            public_key,
        })
    } else {
        None
    };
    let url = match (url, &peer) {
        (Some(u), _) => u.trim().to_string(),
        (None, Some(p)) => format!("http://{}:9900", p.address),
        (None, None) => bail!(
            "say how the pool will reach {name} (--url http://<its address>:9900), or set \
             [pool.wireguard] so the hub mints its tunnel"
        ),
    };

    let invite = Invite {
        v: 1,
        id: token()?[..12].to_string(),
        secret: token()?,
        name: name.to_string(),
        url: url.clone(),
        arbiter: NodeRef {
            id: me.id.clone(),
            name: me.name.clone(),
            url: my_url,
        },
        to_arbiter: token()?,
        from_arbiter: token()?,
        expires: t + ttl_secs,
        signers: cfg.cluster.signers.clone(),
        ticket_signers: cfg
            .cluster
            .ticket_signers
            .iter()
            .map(|(k, v)| (k.clone(), v.key.clone()))
            .collect(),
        recover: None,
    };
    let mut wg_queued = None;
    if let Some(p) = &peer {
        let path = wg_queue_dir().join(format!("{name}.conf"));
        write_private(&path, &wg_hub_stanza(name, p))?;
        wg_queued = Some(path);
    }
    rows.push(Pending {
        id: invite.id.clone(),
        name: name.to_string(),
        url,
        secret_sha: sha_hex(&invite.secret),
        to_arbiter_sha: sha_hex(&invite.to_arbiter),
        from_arbiter: invite.from_arbiter.clone(),
        expires: invite.expires,
        redeemed_at: None,
        wg: peer.as_ref().map(|p| WgLease {
            address: p.address.to_string(),
            public_key: p.public_key.clone(),
        }),
        recover: None,
        node: None,
    });
    save_ledger(&rows)?;
    Ok(Minted {
        wg_client: peer.as_ref().map(|p| wg_client_config(wg, p)),
        wg_queued,
        invite,
        recover_via: None,
    })
}

/// The node a member named `name` is, as this node knows it: an explicit
/// `node` (an id or a unique prefix of one in the registry), else the
/// node that peer's gossip is about, else the one its store offers spoke
/// for, else the one registry row of that name. Returns the id, the
/// registry's name for it (the peer name when the registry has no row),
/// and which of those answered. Never this node itself.
pub fn member_node(
    store: &crate::store::Store,
    me: &str,
    name: &str,
    node: Option<&str>,
) -> Result<(String, String, &'static str)> {
    let rows = store.nodes_list().unwrap_or_default();
    let named = |id: &str| {
        rows.iter()
            .find(|r| r.id == id)
            .map(|r| r.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| name.to_string())
    };
    let (id, via) = if let Some(n) = node.map(str::trim).filter(|n| !n.is_empty()) {
        let hits: Vec<&crate::store::NodeRow> = rows
            .iter()
            .filter(|r| r.id == n || r.id.starts_with(n))
            .collect();
        match hits.as_slice() {
            [one] => (one.id.clone(), "--node"),
            [] => bail!("no node {n} in this node's registry (`veles nodes`)"),
            _ => bail!(
                "{n} matches {} nodes in the registry — give more of the id",
                hits.len()
            ),
        }
    } else if let Some(id) = store.node_bound_to_peer(name, me)? {
        (id, "gossip")
    } else if let Some(id) = crate::storesync::sender_binding(store, name) {
        (id, "store sync")
    } else {
        let hits: Vec<&crate::store::NodeRow> = rows.iter().filter(|r| r.name == name).collect();
        match hits.as_slice() {
            [one] => (one.id.clone(), "registry"),
            [] => bail!(
                "this node does not know which node {name} is: no gossip from it, no store \
                 offer, no registry row of that name — name it with --node <id> (`veles nodes` \
                 here, or the node.toml it had)"
            ),
            _ => bail!(
                "{} registry rows are named {name}: name the one to recover with --node <id>",
                hits.len()
            ),
        }
    };
    if id == me {
        bail!("{name} is this node itself: a node does not recover itself from itself");
    }
    let registry_name = named(&id);
    Ok((id, registry_name, via))
}

/// A recovery invite (`veles pool invite <name> --recover`): re-admit a
/// MEMBER whose home was lost, under the node id it had. Only the
/// arbiter mints one, for a peer it already links (`[a2a_agents.<name>]`)
/// — the url is that link's, the id is what [`member_node`] says the
/// peer is. The node redeeming it must ask as that id; the redeem then
/// rotates every link the member had (this node's own and, by
/// introduction, each member's), and opens the recovery window in which
/// the re-linked peer may pull its own image back (`crate::recover`).
/// No tunnel is minted: the member's address stays what it was.
pub fn mint_recovery(
    cfg: &VelesConfig,
    store: &crate::store::Store,
    name: &str,
    node: Option<&str>,
    ttl_secs: u64,
) -> Result<Minted> {
    let (me, my_url) = inviting_arbiter(cfg, name)?;
    let Some(link) = cfg.a2a_agents.get(name) else {
        bail!(
            "{name} is not a peer of this node ([a2a_agents.{name}]): a recovery invite \
             re-admits a member — a new node joins with `veles pool invite {name}`"
        );
    };
    let (id, registry_name, via) = member_node(store, &me.id, name, node)?;
    let _ledger = ledger_lock();
    let mut rows = ledger();
    let t = now();
    if rows
        .iter()
        .any(|r| r.name == name && r.redeemed_at.is_none() && r.expires > t)
    {
        bail!("an invite for {name} is still open; `veles pool invites --revoke {name}` first");
    }
    let url = link.url.trim().to_string();
    let invite = Invite {
        v: 1,
        id: token()?[..12].to_string(),
        secret: token()?,
        name: name.to_string(),
        url: url.clone(),
        arbiter: NodeRef {
            id: me.id.clone(),
            name: me.name.clone(),
            url: my_url,
        },
        to_arbiter: token()?,
        from_arbiter: token()?,
        expires: t + ttl_secs,
        signers: cfg.cluster.signers.clone(),
        ticket_signers: cfg
            .cluster
            .ticket_signers
            .iter()
            .map(|(k, v)| (k.clone(), v.key.clone()))
            .collect(),
        recover: Some(NodeRef {
            id: id.clone(),
            name: registry_name,
            url: url.clone(),
        }),
    };
    rows.push(Pending {
        id: invite.id.clone(),
        name: name.to_string(),
        url,
        secret_sha: sha_hex(&invite.secret),
        to_arbiter_sha: sha_hex(&invite.to_arbiter),
        from_arbiter: invite.from_arbiter.clone(),
        expires: invite.expires,
        redeemed_at: None,
        wg: None,
        recover: Some(id),
        node: None,
    });
    save_ledger(&rows)?;
    Ok(Minted {
        invite,
        wg_client: None,
        wg_queued: None,
        recover_via: Some(via),
    })
}

/// Drop an open invite (and queue its tunnel's removal). Returns whether
/// there was one.
pub fn revoke(name: &str) -> Result<bool> {
    let _ledger = ledger_lock();
    let mut rows = ledger();
    let before = rows.len();
    let mut gone = Vec::new();
    rows.retain(|r| {
        let open = r.name == name && r.redeemed_at.is_none();
        if open {
            gone.push(r.clone());
        }
        !open
    });
    for r in &gone {
        if let Some(w) = &r.wg {
            write_private(
                &wg_queue_dir().join(format!("{}.remove", r.name)),
                &w.public_key,
            )?;
        }
    }
    save_ledger(&rows)?;
    Ok(rows.len() != before)
}

// ── writing a node's side of its links ───────────────────────────────

/// One link, as THIS node writes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Link {
    pub name: String,
    pub url: String,
    /// What this node sends there.
    pub out_token: String,
    /// What this node accepts from there.
    pub in_token: String,
}

/// `A2A_PEER_TOKENS` (`name:token,…`) with `links`' entries set.
pub fn merge_peer_tokens(existing: &str, links: &[Link]) -> String {
    let mut pairs: Vec<(String, String)> = existing
        .split(',')
        .filter_map(|p| {
            let (n, t) = p.trim().split_once(':')?;
            (!n.trim().is_empty() && !t.trim().is_empty())
                .then(|| (n.trim().to_string(), t.trim().to_string()))
        })
        .filter(|(n, _)| !links.iter().any(|l| &l.name == n))
        .collect();
    for l in links {
        pairs.push((l.name.clone(), l.in_token.clone()));
    }
    pairs
        .iter()
        .map(|(n, t)| format!("{n}:{t}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// What one side of a join wrote, for the reply and the person.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Written {
    pub peers: Vec<String>,
    pub lists: Vec<String>,
    /// Said when the process environment holds `A2A_PEER_TOKENS`, which
    /// beats `.env`: the tokens written there do nothing until it goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// Write `links` into config.toml (`[a2a_agents]`, the `lists`) and
/// `.env` (`A2A_PEER_TOKENS`), then `extra` edits in the same write.
fn write_links(
    links: &[Link],
    lists: &[String],
    extra: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<()>,
) -> Result<Written> {
    crate::configedit::edit("a2a_agents", |doc| {
        for l in links {
            crate::configedit::set_value(
                doc,
                &["a2a_agents", &l.name, "url"],
                l.url.as_str().into(),
            )?;
            crate::configedit::set_value(
                doc,
                &["a2a_agents", &l.name, "auth"],
                crate::configedit::json_to_toml(&json!({"type": "bearer", "token": l.out_token}))?,
            )?;
            for list in lists {
                crate::configedit::array_add(doc, &["cluster", list], &l.name)?;
            }
        }
        extra(doc)
    })?;
    let existing = crate::authn::dotenv_value("A2A_PEER_TOKENS").unwrap_or_default();
    crate::authn::write_dotenv("A2A_PEER_TOKENS", &merge_peer_tokens(&existing, links))?;
    let warning = std::env::var("A2A_PEER_TOKENS")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|_| {
            "this process's environment sets A2A_PEER_TOKENS, which beats .env: the peer tokens \
         written to .env take effect only once that variable is gone (or carries them too)"
                .to_string()
        });
    Ok(Written {
        peers: links.iter().map(|l| l.name.clone()).collect(),
        lists: lists.to_vec(),
        warning,
    })
}

/// The `[cluster]` lists this node uses (non-empty).
fn used_lists(cfg: &VelesConfig) -> Vec<String> {
    let c = &cfg.cluster;
    let all: [(&str, &Vec<String>); 5] = [
        ("gossip_peers", &c.gossip_peers),
        ("memsync_peers", &c.memsync_peers),
        ("task_peers", &c.task_peers),
        ("storesync_peers", &c.storesync_peers),
        ("migrate_peers", &c.migrate_peers),
    ];
    let used: Vec<String> = all
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(n, _)| n.to_string())
        .collect();
    // A two-node pool that names nothing yet still gossips and syncs.
    if used.is_empty() {
        vec![
            "gossip_peers".into(),
            "memsync_peers".into(),
            "task_peers".into(),
        ]
    } else {
        used
    }
}

// ── the wire ─────────────────────────────────────────────────────────

/// The new node's redeem call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JoinAsk {
    pub invite: String,
    pub secret: String,
    pub node_id: String,
}

/// The arbiter's introduction of a new node to a member.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemberAsk {
    pub arbiter: String,
    pub name: String,
    pub node_id: String,
    pub url: String,
    /// The member's bearer at the new node.
    pub to_new: String,
    /// The new node's bearer at the member.
    pub from_new: String,
    pub lists: Vec<String>,
    /// The node is a member coming back from a lost home (a recovery
    /// invite): the member marks it so it may pull its own image back
    /// for [`RECOVERY_WINDOW_SECS`] (`crate::recover`). Old arbiters
    /// send no field: an ordinary introduction.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recover: bool,
}

/// A member, as the new node gets it from the arbiter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemberLink {
    pub name: String,
    pub id: Option<String>,
    pub url: String,
    /// The new node's bearer there.
    pub to_member: String,
    /// The member's bearer at the new node.
    pub from_member: String,
}

pub fn is_join(raw: &str) -> bool {
    raw.trim_start().starts_with(JOIN_MARKER)
}

pub fn is_member(raw: &str) -> bool {
    raw.trim_start().starts_with(MEMBER_MARKER)
}

pub fn encode_join(ask: &JoinAsk) -> Result<String> {
    Ok(crate::mailnet::fence(
        JOIN_MARKER,
        &serde_json::to_string(ask)?,
    ))
}

pub fn decode_join(raw: &str) -> Result<JoinAsk> {
    serde_json::from_str(&crate::mailnet::unfence(raw)?).context("not a pool join")
}

pub fn encode_member(ask: &MemberAsk) -> Result<String> {
    Ok(crate::mailnet::fence(
        MEMBER_MARKER,
        &serde_json::to_string(ask)?,
    ))
}

pub fn decode_member(raw: &str) -> Result<MemberAsk> {
    serde_json::from_str(&crate::mailnet::unfence(raw)?).context("not a pool member introduction")
}

pub fn encode_reply(v: &Value) -> Result<String> {
    Ok(crate::mailnet::fence(
        REPLY_MARKER,
        &serde_json::to_string(v)?,
    ))
}

pub fn decode_reply(raw: &str) -> Result<Value> {
    if !raw.trim_start().starts_with(REPLY_MARKER) {
        bail!("{}", raw.chars().take(300).collect::<String>());
    }
    serde_json::from_str(&crate::mailnet::unfence(raw)?).context("not a pool reply")
}

fn refused(e: impl std::fmt::Display) -> Value {
    json!({"status": "refused", "error": e.to_string()})
}

async fn call(url: &str, bearer: &str, text: &str) -> Result<Value> {
    let client = crate::a2a::A2AClient::with_timeout(std::time::Duration::from_secs(60));
    let result = client
        .call(url, Some(bearer), text, &crate::a2a::new_context_id())
        .await
        .with_context(|| format!("{url} did not answer"))?;
    let v = decode_reply(&crate::a2a::extract_text(&result))?;
    if v["status"] != "ok" {
        bail!("{}", v["error"].as_str().unwrap_or("refused"));
    }
    Ok(v)
}

// ── the arbiter: redeem ──────────────────────────────────────────────

/// The arbiter's side of a join (D8): check the invite, write this node's
/// side, introduce the newcomer to every member, answer with them.
/// `bearer` is the token the call came with — the invite's own.
pub async fn redeem(
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
    cfg: &VelesConfig,
    bearer: &str,
    ask: JoinAsk,
) -> Value {
    match redeem_inner(store, cfg, bearer, ask).await {
        Ok(v) => v,
        Err(e) => refused(format!("{e:#}")),
    }
}

async fn redeem_inner(
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
    cfg: &VelesConfig,
    bearer: &str,
    ask: JoinAsk,
) -> Result<Value> {
    // Claim the invite first, and for good: single use even if a later
    // step fails (a half-joined node starts over with a new invite).
    let pending = {
        let _ledger = ledger_lock();
        let mut rows = ledger();
        let t = now();
        let Some(row) = rows.iter_mut().find(|r| r.id == ask.invite) else {
            bail!("no such invite here");
        };
        if row.redeemed_at.is_some() {
            bail!("this invite was used already");
        }
        if row.expires <= t {
            bail!("this invite has expired; ask for a new one");
        }
        if row.secret_sha != sha_hex(&ask.secret) || row.to_arbiter_sha != sha_hex(bearer) {
            bail!("the invite's secret does not match");
        }
        // A recovery invite re-admits ONE node: asked as any other id, it
        // would hand that node's name, links and recovery window to a
        // stranger. Refused before the claim, so the right node can still
        // use it (`veles recover --as`).
        if let Some(want) = &row.recover {
            if &ask.node_id != want {
                bail!(
                    "this invite re-admits node {want}, and the node redeeming it is {}: run \
                     `veles recover` with --as {want}",
                    ask.node_id
                );
            }
        }
        row.redeemed_at = Some(t);
        row.node = Some(ask.node_id.clone());
        let row = row.clone();
        save_ledger(&rows)?;
        row
    };
    let lists = used_lists(cfg);
    let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?;

    // The members, as this node dials them: everyone in its lists.
    let mut members: Vec<String> = Vec::new();
    for list in &lists {
        let names = match list.as_str() {
            "gossip_peers" => &cfg.cluster.gossip_peers,
            "memsync_peers" => &cfg.cluster.memsync_peers,
            "task_peers" => &cfg.cluster.task_peers,
            "storesync_peers" => &cfg.cluster.storesync_peers,
            _ => &cfg.cluster.migrate_peers,
        };
        for n in names {
            if n != &pending.name && !members.contains(n) && cfg.a2a_agents.contains_key(n) {
                members.push(n.clone());
            }
        }
    }

    // Introduce the newcomer to each member over the link this node has.
    let mut links = Vec::new();
    let mut not_introduced = Vec::new();
    for m in &members {
        let peer = &cfg.a2a_agents[m];
        let (to_member, from_member) = (token()?, token()?);
        let ask_m = MemberAsk {
            arbiter: me.id.clone(),
            name: pending.name.clone(),
            node_id: ask.node_id.clone(),
            url: pending.url.clone(),
            to_new: from_member.clone(),
            from_new: to_member.clone(),
            lists: lists.clone(),
            recover: pending.recover.is_some(),
        };
        match call(&peer.url, peer.auth.token.trim(), &encode_member(&ask_m)?).await {
            Ok(v) => links.push(MemberLink {
                name: m.clone(),
                id: v["node_id"].as_str().map(str::to_string),
                url: peer.url.clone(),
                to_member,
                from_member,
            }),
            Err(e) => not_introduced.push(json!({"name": m, "error": format!("{e:#}")})),
        }
    }

    // This node's own side of the newcomer's link.
    let written = write_links(
        &[Link {
            name: pending.name.clone(),
            url: pending.url.clone(),
            out_token: pending.from_arbiter.clone(),
            in_token: bearer.to_string(),
        }],
        &lists,
        |_| Ok(()),
    )?;
    let _ = store; // registry rows arrive by gossip once the node is up
    crate::a2a::audit_log(&json!({
        "event": if pending.recover.is_some() { "pool_recover" } else { "pool_join" },
        "name": pending.name, "node": ask.node_id,
        "members": links.iter().map(|l| l.name.clone()).collect::<Vec<_>>(),
        "not_introduced": not_introduced,
    }));
    Ok(json!({
        "status": "ok",
        "arbiter": {"id": me.id, "name": me.name},
        "members": links,
        "lists": lists,
        "not_introduced": not_introduced,
        "written": written,
        "recover": pending.recover,
    }))
}

// ── the arbiter: introduce two members ───────────────────────────────

/// The two asks of one introduction: `a` hears about `b`, `b` about `a`,
/// with one fresh token pair between them. `a`/`b` are (name, node id,
/// url). What one side sends is what the other accepts.
fn introduction_pair(
    arbiter: &str,
    a: (&str, &str, &str),
    b: (&str, &str, &str),
    lists: &[String],
) -> Result<(MemberAsk, MemberAsk)> {
    let a_at_b = token()?;
    let b_at_a = token()?;
    let to_a = MemberAsk {
        arbiter: arbiter.to_string(),
        name: b.0.to_string(),
        node_id: b.1.to_string(),
        url: b.2.to_string(),
        to_new: a_at_b.clone(),
        from_new: b_at_a.clone(),
        lists: lists.to_vec(),
        recover: false,
    };
    let to_b = MemberAsk {
        arbiter: arbiter.to_string(),
        name: a.0.to_string(),
        node_id: a.1.to_string(),
        url: a.2.to_string(),
        to_new: b_at_a,
        from_new: a_at_b,
        lists: lists.to_vec(),
        recover: false,
    };
    Ok((to_a, to_b))
}

/// The arbiter introducing two members to each other, by hand (D8's
/// introduction, outside a join): a fresh token pair, sent to each over
/// the link this node already has. For a member that was off when the
/// other joined — a join introduces the newcomer only to the members
/// that answer, and an ephemeral node is often not one of them — and
/// for rotating a pair's tokens: a member that already has the peer, at
/// the same url, takes the new tokens in place of the old. Neither side
/// is widened: both are members already, and each takes an introduction
/// only from its arbiter.
pub async fn introduce(
    store: &crate::store::Store,
    cfg: &VelesConfig,
    a: &str,
    b: &str,
) -> Result<Value> {
    let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?;
    match cfg.cluster.arbiter.as_deref() {
        Some(x) if x == me.id || me.id.starts_with(x) => {}
        Some(x) => bail!(
            "only the arbiter introduces, and this node is not it: run `veles pool introduce {a} \
             {b}` on {x}"
        ),
        None => bail!("this node has no [cluster] arbiter"),
    }
    if a == b {
        bail!("a node is not introduced to itself");
    }
    let peer = |name: &str| {
        cfg.a2a_agents.get(name).with_context(|| {
            format!(
                "{name} is not a peer of this node ([a2a_agents.{name}]): the arbiter introduces \
                 only members it holds a link to"
            )
        })
    };
    let (peer_a, peer_b) = (peer(a)?, peer(b)?);
    let id_of = |name: &str| -> String {
        store
            .nodes_list()
            .unwrap_or_default()
            .into_iter()
            .find(|n| n.name == name)
            .map(|n| n.id)
            .unwrap_or_default()
    };
    let lists = used_lists(cfg);
    let (to_a, to_b) = introduction_pair(
        &me.id,
        (a, &id_of(a), &peer_a.url),
        (b, &id_of(b), &peer_b.url),
        &lists,
    )?;
    let rb = call(
        &peer_b.url,
        peer_b.auth.token.trim(),
        &encode_member(&to_b)?,
    )
    .await
    .with_context(|| format!("{b} did not take the introduction; nothing was written"))?;
    let ra = call(
        &peer_a.url,
        peer_a.auth.token.trim(),
        &encode_member(&to_a)?,
    )
    .await
    .with_context(|| {
        format!(
            "{b} took the new tokens but {a} did not: the pair's link is down until this is \
                 run again with {a} answering"
        )
    })?;
    crate::a2a::audit_log(&json!({
        "event": "pool_introduce", "a": a, "b": b, "lists": lists,
    }));
    Ok(json!({
        "status": "ok",
        "lists": lists,
        a: {"node_id": ra["node_id"], "written": ra["written"]},
        b: {"node_id": rb["node_id"], "written": rb["written"]},
    }))
}

// ── a member: accept an introduction ─────────────────────────────────

/// A member's side (D8): only the arbiter may introduce, over its own
/// link — `peer` is the A2A identity the call authenticated as.
pub fn accept_member(
    store: &crate::store::Store,
    cfg: &VelesConfig,
    peer: &str,
    ask: MemberAsk,
) -> Value {
    match accept_inner(store, cfg, peer, ask) {
        Ok(v) => v,
        Err(e) => refused(format!("{e:#}")),
    }
}

fn accept_inner(
    store: &crate::store::Store,
    cfg: &VelesConfig,
    peer: &str,
    ask: MemberAsk,
) -> Result<Value> {
    let Some(arbiter) = cfg.cluster.arbiter.as_deref() else {
        bail!("this node has no [cluster] arbiter, so nobody may introduce peers to it");
    };
    if ask.arbiter != arbiter && !ask.arbiter.starts_with(arbiter) {
        bail!("only this node's arbiter introduces peers");
    }
    // The call must come over the arbiter's own link: the peer it
    // authenticated as is the one this node dials the arbiter at.
    let link = crate::mailnet::remote(store, cfg, arbiter)
        .context("this node does not know how it dials its arbiter")?;
    if link.name != peer {
        bail!("only the arbiter introduces peers, and {peer} is not it");
    }
    if !valid_name(&ask.name) {
        bail!("{:?} is not a node name", ask.name);
    }
    if let Some(have) = cfg.a2a_agents.get(&ask.name) {
        if have.url.trim_end_matches('/') != ask.url.trim_end_matches('/') {
            bail!("{} is already a peer here, at {}", ask.name, have.url);
        }
    }
    if ask.recover {
        // A recovery re-admits the node this member already knows under
        // that name; one that names another node is not a recovery.
        let mine = crate::node::NodeIdentity::load(&crate::config::config_dir())?
            .map(|n| n.id)
            .unwrap_or_default();
        if let Some(bound) = store.node_bound_to_peer(&ask.name, &mine)? {
            if bound != ask.node_id {
                bail!(
                    "{} is node {bound} here (gossip), and the recovery names {}: refused",
                    ask.name,
                    ask.node_id
                );
            }
        }
    }
    let lists: Vec<String> = ask
        .lists
        .iter()
        .filter(|l| LISTS.contains(&l.as_str()))
        .cloned()
        .collect();
    let written = write_links(
        &[Link {
            name: ask.name.clone(),
            url: ask.url.clone(),
            out_token: ask.to_new.clone(),
            in_token: ask.from_new.clone(),
        }],
        &lists,
        |_| Ok(()),
    )?;
    let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?;
    if ask.recover {
        // The member coming back may pull its own image from here for a
        // while (`crate::recover`); the mark names the node it is, as the
        // arbiter vouched for it over its own link.
        let until = now() + RECOVERY_WINDOW_SECS;
        store.conn().execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![
                format!("{RECOVERY_META_PREFIX}{}", ask.name),
                format!("{} {until}", ask.node_id)
            ],
        )?;
    }
    Ok(json!({"status": "ok", "node_id": me.id, "name": me.name, "written": written}))
}

// ── the new node: join ───────────────────────────────────────────────

/// What a join did on the new node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Joined {
    pub arbiter: NodeRef,
    pub members: Vec<String>,
    pub not_introduced: Vec<Value>,
    pub written: Written,
    /// The registry rows to seed ([`seed_registry`]): the arbiter and
    /// every member whose id the arbiter knew.
    pub seeds: Vec<NodeRef>,
}

/// Seed the registry with what a join learned. The fence asks the arbiter
/// for leases at its REGISTRY endpoint, and a fresh node's registry is
/// empty until the first gossip — the "restart once more" the pool docs
/// describe. Separate from [`join`] so no store borrow lives across its
/// network call.
pub fn seed_registry(store: &crate::store::Store, joined: &Joined) {
    for n in &joined.seeds {
        let _ = store.node_register(&n.id, &n.name, "", "", "", None, Some(&n.url), None, None);
    }
}

/// The new node's side: redeem first (nothing is written here unless the
/// arbiter accepted), then write every link, this node's own endpoint and
/// the cluster settings. The caller then seeds the registry
/// ([`seed_registry`]).
pub async fn join(invite: &Invite) -> Result<Joined> {
    if invite.expires <= now() {
        bail!("this invite has expired; ask for a new one");
    }
    let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?;
    let ask = JoinAsk {
        invite: invite.id.clone(),
        secret: invite.secret.clone(),
        node_id: me.id.clone(),
    };
    let reply = call(&invite.arbiter.url, &invite.to_arbiter, &encode_join(&ask)?).await?;
    let members: Vec<MemberLink> =
        serde_json::from_value(reply["members"].clone()).context("the arbiter's member list")?;
    let lists: Vec<String> = serde_json::from_value(reply["lists"].clone()).unwrap_or_default();
    let lists: Vec<String> = lists
        .into_iter()
        .filter(|l| LISTS.contains(&l.as_str()))
        .collect();

    let mut links = vec![Link {
        name: invite.arbiter.name.clone(),
        url: invite.arbiter.url.clone(),
        out_token: invite.to_arbiter.clone(),
        in_token: invite.from_arbiter.clone(),
    }];
    for m in &members {
        links.push(Link {
            name: m.name.clone(),
            url: m.url.clone(),
            out_token: m.to_member.clone(),
            in_token: m.from_member.clone(),
        });
    }
    let host = host_of(&invite.url);
    let written = write_links(&links, &lists, |doc| {
        use crate::configedit::set_value;
        set_value(
            doc,
            &["cluster", "arbiter"],
            invite.arbiter.id.as_str().into(),
        )?;
        if !invite.signers.is_empty() {
            set_value(
                doc,
                &["cluster", "signers"],
                crate::configedit::json_to_toml(&json!(invite.signers))?,
            )?;
        }
        for (name, key) in &invite.ticket_signers {
            set_value(
                doc,
                &["cluster", "ticket_signers", name, "key"],
                key.as_str().into(),
            )?;
        }
        set_value(doc, &["node", "endpoint"], invite.url.as_str().into())?;
        // The pool calls in over A2A: the server has to run.
        set_value(doc, &["platforms", "a2a", "enabled"], true.into())?;
        // A token alone does not widen A2A's bind; the address does. It is
        // this machine's own (configsync::NODE_KEYS keeps it here), and it
        // is config, not a secret: config.toml beside the port, with
        // `A2A_HOST` in the environment or `.env` still an override.
        if let Some(h) = &host {
            set_value(
                doc,
                &["platforms", "a2a", "extra", "host"],
                h.as_str().into(),
            )?;
        }
        Ok(())
    })?;
    let mut seeds = vec![invite.arbiter.clone()];
    for m in &members {
        if let Some(id) = &m.id {
            seeds.push(NodeRef {
                id: id.clone(),
                name: m.name.clone(),
                url: m.url.clone(),
            });
        }
    }
    Ok(Joined {
        arbiter: invite.arbiter.clone(),
        members: members.iter().map(|m| m.name.clone()).collect(),
        not_introduced: serde_json::from_value(reply["not_introduced"].clone()).unwrap_or_default(),
        written,
        seeds,
    })
}

// ── QR codes ─────────────────────────────────────────────────────────

/// `text` as a QR code drawn with half-block characters, two rows of
/// modules per line, with the quiet zone: what `veles pool invite`
/// prints for a phone to scan off the terminal.
pub fn qr_text(text: &str) -> Result<String> {
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L)
        .context("the text does not fit in a QR code")?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build())
}

/// `text` as a PNG, `scale` pixels per module, quiet zone included.
pub fn qr_png(text: &str, scale: u32) -> Result<Vec<u8>> {
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L)
        .context("the text does not fit in a QR code")?;
    let w = code.width() as u32;
    let quiet = 4;
    let side = (w + 2 * quiet) * scale;
    let colors = code.to_colors();
    let img = image::GrayImage::from_fn(side, side, |x, y| {
        let (mx, my) = (x / scale, y / scale);
        let dark = mx >= quiet
            && my >= quiet
            && mx < w + quiet
            && my < w + quiet
            && colors[((my - quiet) * w + (mx - quiet)) as usize] == qrcode::Color::Dark;
        image::Luma([if dark { 0 } else { 255 }])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

/// Why a scan failed, as a window shows it in its own language: the
/// error's text starts with one of these.
pub const NO_CODE: &str = "no QR code in the photo";
pub const UNREADABLE: &str = "a QR code is there but could not be read";
pub const WIREGUARD_CODE: &str = "this is the WireGuard code";
pub const NOT_AN_INVITE: &str = "this is not a veles pool invite";

/// The kind of a scan error (`no_code`, `unreadable`, `wireguard`,
/// `not_invite`), or `None` for anything else.
pub fn scan_error_kind(e: &str) -> Option<&'static str> {
    [
        (NO_CODE, "no_code"),
        (UNREADABLE, "unreadable"),
        (WIREGUARD_CODE, "wireguard"),
        (NOT_AN_INVITE, "not_invite"),
    ]
    .iter()
    .find(|(p, _)| e.starts_with(p))
    .map(|(_, k)| *k)
}

/// The text of the QR code in a photo (JPEG or PNG): what the phone's
/// camera saw. The first code found wins.
pub fn qr_read(image_bytes: &[u8]) -> Result<String> {
    let img = image::load_from_memory(image_bytes)
        .context("the photo could not be read")?
        .to_luma8();
    let (w, h) = img.dimensions();
    let mut prepared =
        rqrr::PreparedImage::prepare_from_greyscale(w as usize, h as usize, |x, y| {
            img.get_pixel(x as u32, y as u32).0[0]
        });
    let grids = prepared.detect_grids();
    if grids.is_empty() {
        bail!("{NO_CODE}; hold the camera closer, straight on");
    }
    let mut last = None;
    for g in grids {
        match g.decode() {
            Ok((_, content)) => return Ok(content),
            Err(e) => last = Some(e),
        }
    }
    bail!(
        "{UNREADABLE} ({}); try again in better light",
        last.map(|e| e.to_string()).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Invite {
        Invite {
            v: 1,
            id: "abc123".into(),
            secret: "s".repeat(48),
            name: "phone2".into(),
            url: "http://10.0.30.4:9900".into(),
            arbiter: NodeRef {
                id: "495908ea".into(),
                name: "vps".into(),
                url: "http://10.0.30.1:9900".into(),
            },
            to_arbiter: "t".repeat(48),
            from_arbiter: "f".repeat(48),
            expires: now() + 600,
            signers: vec!["http://10.0.30.1:9911".into()],
            ticket_signers: [("vps-signer".to_string(), "ed25519:AAAA".to_string())].into(),
            recover: None,
        }
    }

    #[test]
    fn an_invite_survives_its_text_and_refuses_other_codes() {
        let inv = sample();
        let text = encode(&inv).unwrap();
        assert!(text.starts_with(QR_PREFIX));
        assert_eq!(decode(&format!("  {text}\n")).unwrap(), inv);
        let wg = decode("[Interface]\nPrivateKey = x\n")
            .unwrap_err()
            .to_string();
        assert!(wg.contains("WireGuard app"), "{wg}");
        let other = decode("https://example.org").unwrap_err().to_string();
        assert_eq!(scan_error_kind(&other), Some("not_invite"), "{other}");
        assert_eq!(scan_error_kind(&wg), Some("wireguard"));
        let v2 = text.replace("{\"v\":1", "{\"v\":2");
        assert!(decode(&v2).unwrap_err().to_string().contains("version 2"));
    }

    #[test]
    fn a_qr_code_reads_back_from_its_own_picture() {
        let text = encode(&sample()).unwrap();
        let png = qr_png(&text, 4).unwrap();
        assert_eq!(qr_read(&png).unwrap(), text);
        // Through JPEG, as a camera hands it over.
        let img = image::load_from_memory(&png).unwrap();
        let mut jpg = std::io::Cursor::new(Vec::new());
        img.write_to(&mut jpg, image::ImageFormat::Jpeg).unwrap();
        assert_eq!(qr_read(jpg.get_ref()).unwrap(), text);
        assert!(qr_text(&text).unwrap().lines().count() > 20);
        // A picture with no code says so.
        let blank = image::GrayImage::from_pixel(200, 200, image::Luma([255]));
        let mut b = std::io::Cursor::new(Vec::new());
        blank.write_to(&mut b, image::ImageFormat::Png).unwrap();
        let none = qr_read(b.get_ref()).unwrap_err().to_string();
        assert_eq!(scan_error_kind(&none), Some("no_code"), "{none}");
    }

    #[test]
    fn wireguard_keys_and_addresses_are_what_wg_would_make() {
        // RFC 7748 §6.1: Alice's private key and her public key.
        let sk = hex_bytes("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let pk = hex_bytes("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
        assert_eq!(wg_public(sk), pk);
        let (priv_b64, pub_b64) = wg_keypair().unwrap();
        use base64::Engine;
        let sk2: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(priv_b64)
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(b64(&wg_public(sk2)), pub_b64);
        assert_eq!(sk2[0] & 7, 0, "clamped as wg genkey clamps");

        let taken: Vec<Ipv4Addr> = ["10.0.30.1", "10.0.30.2", "10.0.30.3"]
            .iter()
            .map(|a| a.parse().unwrap())
            .collect();
        assert_eq!(
            next_address("10.0.30.0/24", &taken).unwrap(),
            "10.0.30.4".parse::<Ipv4Addr>().unwrap()
        );
        assert_eq!(
            next_address("10.0.30.0/30", &taken[..1]).unwrap(),
            "10.0.30.2".parse::<Ipv4Addr>().unwrap()
        );
        assert!(
            next_address("10.0.30.0/30", &taken[..2]).is_err(),
            "only .1 and .2 fit"
        );
        assert!(next_address("10.0.30.0", &[]).is_err());
        assert_eq!(
            host_of("http://10.0.30.4:9900/a2a").as_deref(),
            Some("10.0.30.4")
        );
        assert_eq!(host_of("http://[fd00::1]:9900").as_deref(), Some("fd00::1"));

        let wg = crate::config::PoolWireguard {
            subnet: "10.0.30.0/24".into(),
            address: "10.0.30.1".into(),
            endpoint: "hub.example:51820".into(),
            public_key: "HUBKEY=".into(),
            ..Default::default()
        };
        let peer = WgPeer {
            address: "10.0.30.4".parse().unwrap(),
            private_key: "PRIV=".into(),
            public_key: "PUB=".into(),
        };
        let c = wg_client_config(&wg, &peer);
        for line in [
            "PrivateKey = PRIV=",
            "Address = 10.0.30.4/32",
            "PublicKey = HUBKEY=",
            "Endpoint = hub.example:51820",
            "AllowedIPs = 10.0.30.0/24",
            "PersistentKeepalive = 25",
        ] {
            assert!(c.contains(line), "{line}: {c}");
        }
        let h = wg_hub_stanza("phone2", &peer);
        assert!(
            h.contains("PublicKey = PUB=") && h.contains("AllowedIPs = 10.0.30.4/32"),
            "{h}"
        );
    }

    fn hex_bytes(s: &str) -> [u8; 32] {
        let v: Vec<u8> = (0..32)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        v.try_into().unwrap()
    }

    #[test]
    fn peer_tokens_merge_by_name() {
        let links = [Link {
            name: "phone2".into(),
            url: "u".into(),
            out_token: "o".into(),
            in_token: "NEW".into(),
        }];
        assert_eq!(merge_peer_tokens("", &links), "phone2:NEW");
        assert_eq!(
            merge_peer_tokens("laptop:a, phone2:old,junk,", &links),
            "laptop:a,phone2:NEW"
        );
    }

    /// Only the arbiter invites; an invite is single use, expires, and its
    /// bearer opens the join and nothing after it.
    #[test]
    fn an_invite_is_the_arbiters_once_and_briefly() {
        let (_home, _g) = crate::testenv::scratch_home();
        let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir()).unwrap();
        let mut cfg = VelesConfig::default();
        cfg.node.endpoint = Some("http://10.0.30.1:9900".into());
        cfg.cluster.arbiter = Some("someone-else".into());
        assert!(mint(&cfg, "phone2", Some("http://10.0.30.4:9900"), 600)
            .unwrap_err()
            .to_string()
            .contains("only the arbiter"));
        cfg.cluster.arbiter = Some(me.id.clone());
        assert!(
            mint(&cfg, "Phone 2", Some("x"), 600).is_err(),
            "names are a-z0-9-"
        );
        assert!(mint(&cfg, "phone2", None, 600)
            .unwrap_err()
            .to_string()
            .contains("--url"));

        let m = mint(&cfg, "phone2", Some("http://10.0.30.4:9900"), 600).unwrap();
        assert!(m.wg_client.is_none());
        let auth = format!("Bearer {}", m.invite.to_arbiter);
        assert_eq!(invite_bearer(Some(&auth)).as_deref(), Some("phone2"));
        assert_eq!(invite_bearer(Some("Bearer nope")), None);
        assert!(mint(&cfg, "phone2", Some("http://10.0.30.4:9900"), 600)
            .unwrap_err()
            .to_string()
            .contains("still open"));
        // Nothing in the ledger keeps the new node's bearer or the secret.
        let raw = std::fs::read_to_string(ledger_path()).unwrap();
        assert!(!raw.contains(&m.invite.to_arbiter) && !raw.contains(&m.invite.secret));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(ledger_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // Revoked: the bearer opens nothing.
        assert!(revoke("phone2").unwrap());
        assert_eq!(invite_bearer(Some(&auth)), None);

        // Expired: the same.
        let m = mint(&cfg, "phone3", Some("http://10.0.30.5:9900"), 0).unwrap();
        assert_eq!(
            invite_bearer(Some(&format!("Bearer {}", m.invite.to_arbiter))),
            None
        );
    }

    /// The hub mints the tunnel: the next address, the queued peer for
    /// the root helper, the WireGuard app's config.
    #[test]
    fn the_hub_mints_a_tunnel_with_the_invite() {
        let (_home, _g) = crate::testenv::scratch_home();
        let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir()).unwrap();
        let mut cfg = VelesConfig::default();
        cfg.node.endpoint = Some("http://10.0.30.1:9900".into());
        cfg.cluster.arbiter = Some(me.id.clone());
        cfg.pool.wireguard = crate::config::PoolWireguard {
            subnet: "10.0.30.0/24".into(),
            address: "10.0.30.1".into(),
            endpoint: "hub.example:51820".into(),
            public_key: "HUBKEY=".into(),
            ..Default::default()
        };
        cfg.a2a_agents.insert(
            "laptop".into(),
            crate::config::A2APeer {
                url: "http://10.0.30.2:9900".into(),
                ..Default::default()
            },
        );
        let m = mint(&cfg, "phone2", None, 600).unwrap();
        assert_eq!(
            m.invite.url, "http://10.0.30.3:9900",
            "the next free address"
        );
        let client = m.wg_client.unwrap();
        assert!(client.contains("Address = 10.0.30.3/32"), "{client}");
        let queued = std::fs::read_to_string(m.wg_queued.unwrap()).unwrap();
        assert!(queued.contains("AllowedIPs = 10.0.30.3/32"), "{queued}");
        // The next invite does not hand out the same address.
        let m2 = mint(&cfg, "tablet", None, 600).unwrap();
        assert_eq!(m2.invite.url, "http://10.0.30.4:9900");
        // A revoked tunnel is queued for removal by its key.
        assert!(revoke("tablet").unwrap());
        assert!(wg_queue_dir().join("tablet.remove").exists());
    }

    /// A member takes an introduction only from its arbiter, over the
    /// arbiter's own link, and writes its side of the new link.
    #[test]
    fn a_member_takes_introductions_only_from_its_arbiter() {
        let (_home, _g) = crate::testenv::scratch_home();
        let store = crate::store::Store::open(&crate::config::data_dir().join("state.db")).unwrap();
        store
            .node_register(
                "arbiterid",
                "vps",
                "",
                "",
                "",
                None,
                Some("http://10.0.30.1:9900"),
                None,
                None,
            )
            .unwrap();
        std::fs::write(
            crate::configedit::config_path(),
            "[cluster]\narbiter = \"arbiterid\"\ngossip_peers = [\"vps\"]\n\n\
             [a2a_agents.vps]\nurl = \"http://10.0.30.1:9900\"\nauth = { type = \"bearer\", token = \"tv\" }\n",
        )
        .unwrap();
        let cfg = crate::config::load().unwrap();
        let ask = MemberAsk {
            arbiter: "arbiterid".into(),
            name: "phone2".into(),
            node_id: "newid".into(),
            url: "http://10.0.30.4:9900".into(),
            to_new: "member-to-new".into(),
            from_new: "new-to-member".into(),
            lists: vec!["gossip_peers".into(), "bogus_list".into()],
            recover: false,
        };
        let r = accept_member(&store, &cfg, "laptop", ask.clone());
        assert_eq!(r["status"], "refused", "{r}");
        let mut wrong = ask.clone();
        wrong.arbiter = "someone".into();
        assert_eq!(
            accept_member(&store, &cfg, "vps", wrong)["status"],
            "refused"
        );

        let r = accept_member(&store, &cfg, "vps", ask);
        assert_eq!(r["status"], "ok", "{r}");
        let after = crate::config::load().unwrap();
        assert_eq!(after.a2a_agents["phone2"].url, "http://10.0.30.4:9900");
        assert_eq!(after.a2a_agents["phone2"].auth.token, "member-to-new");
        assert!(after.cluster.gossip_peers.contains(&"phone2".to_string()));
        let env = crate::authn::dotenv_value("A2A_PEER_TOKENS").unwrap();
        assert!(env.contains("phone2:new-to-member"), "{env}");
    }

    /// A re-introduction mints one pair and sends each member its mirror
    /// of it; a member that already has the peer, at the same url, takes
    /// the new tokens in place of the old — which is how a pair's tokens
    /// are rotated — and keeps its other peers' tokens.
    #[test]
    fn a_reintroduction_mirrors_one_pair_and_replaces_the_old_tokens() {
        let lists = vec!["gossip_peers".to_string()];
        let (to_a, to_b) = introduction_pair(
            "arbiterid",
            ("laptop", "lid", "http://10.0.30.2:9900"),
            ("node-b", "zid", "http://10.0.30.4:9900"),
            &lists,
        )
        .unwrap();
        assert_eq!(
            (to_a.name.as_str(), to_a.node_id.as_str()),
            ("node-b", "zid")
        );
        assert_eq!(
            (to_b.name.as_str(), to_b.node_id.as_str()),
            ("laptop", "lid")
        );
        assert_eq!(to_a.url, "http://10.0.30.4:9900");
        assert_eq!(to_b.url, "http://10.0.30.2:9900");
        // What one side sends is what the other accepts, and the two
        // directions are different secrets.
        assert_eq!(to_a.to_new, to_b.from_new);
        assert_eq!(to_a.from_new, to_b.to_new);
        assert_ne!(to_a.to_new, to_a.from_new);
        assert!(to_a.to_new.len() >= 32);

        // As the laptop, which already has node-b on old tokens.
        let (_home, _g) = crate::testenv::scratch_home();
        let store = crate::store::Store::open(&crate::config::data_dir().join("state.db")).unwrap();
        store
            .node_register(
                "arbiterid",
                "vps",
                "",
                "",
                "",
                None,
                Some("http://10.0.30.1:9900"),
                None,
                None,
            )
            .unwrap();
        std::fs::write(
            crate::configedit::config_path(),
            "[cluster]\narbiter = \"arbiterid\"\ngossip_peers = [\"vps\", \"node-b\"]\n\n\
             [a2a_agents.vps]\nurl = \"http://10.0.30.1:9900\"\nauth = { type = \"bearer\", token = \"tv\" }\n\n\
             [a2a_agents.node-b]\nurl = \"http://10.0.30.4:9900\"\nauth = { type = \"bearer\", token = \"old-out\" }\n",
        )
        .unwrap();
        crate::authn::write_dotenv("A2A_PEER_TOKENS", "vps:tv,node-b:old-in").unwrap();
        let cfg = crate::config::load().unwrap();
        let r = accept_member(&store, &cfg, "vps", to_a.clone());
        assert_eq!(r["status"], "ok", "{r}");
        let after = crate::config::load().unwrap();
        assert_eq!(after.a2a_agents["node-b"].auth.token, to_a.to_new);
        assert_eq!(after.a2a_agents["vps"].auth.token, "tv");
        assert_eq!(
            after
                .cluster
                .gossip_peers
                .iter()
                .filter(|p| *p == "node-b")
                .count(),
            1,
            "the peer is listed once: {:?}",
            after.cluster.gossip_peers
        );
        let env = crate::authn::dotenv_value("A2A_PEER_TOKENS").unwrap();
        assert!(env.contains(&format!("node-b:{}", to_a.from_new)), "{env}");
        assert!(
            !env.contains("old-in"),
            "the old inbound token is gone: {env}"
        );
        assert!(env.contains("vps:tv"), "other peers keep theirs: {env}");

        // The same peer at another url is not re-pointed by an introduction.
        let mut moved = to_a;
        moved.url = "http://10.0.30.9:9900".into();
        let r = accept_member(&store, &cfg, "vps", moved);
        assert_eq!(r["status"], "refused", "{r}");
    }

    /// The arbiter's redeem: the secret and the invite's own bearer must
    /// both match, an expired invite is refused, and a good one is claimed
    /// once — it writes this node's side of the new link.
    #[tokio::test]
    async fn a_redeem_checks_the_invite_and_claims_it_once() {
        let (_home, _g) = crate::testenv::scratch_home();
        let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir()).unwrap();
        let mut cfg = VelesConfig::default();
        cfg.node.endpoint = Some("http://10.0.30.1:9900".into());
        cfg.cluster.arbiter = Some(me.id.clone());
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open(&crate::config::data_dir().join("state.db")).unwrap(),
        ));
        let m = mint(&cfg, "phone2", Some("http://10.0.30.4:9900"), 600).unwrap();
        let ask = |secret: &str| JoinAsk {
            invite: m.invite.id.clone(),
            secret: secret.to_string(),
            node_id: "newid".into(),
        };

        let r = redeem(&store, &cfg, &m.invite.to_arbiter, ask("not the secret")).await;
        assert_eq!(r["status"], "refused", "{r}");
        let r = redeem(&store, &cfg, "not the bearer", ask(&m.invite.secret)).await;
        assert_eq!(r["status"], "refused", "{r}");
        assert!(
            r["error"].as_str().unwrap().contains("does not match"),
            "{r}"
        );

        let r = redeem(&store, &cfg, &m.invite.to_arbiter, ask(&m.invite.secret)).await;
        assert_eq!(r["status"], "ok", "{r}");
        assert_eq!(r["members"], json!([]), "no members to introduce: {r}");
        let after = crate::config::load().unwrap();
        assert_eq!(after.a2a_agents["phone2"].auth.token, m.invite.from_arbiter);
        let env = crate::authn::dotenv_value("A2A_PEER_TOKENS").unwrap();
        assert!(
            env.contains(&format!("phone2:{}", m.invite.to_arbiter)),
            "{env}"
        );
        // The ledger knows which node the new peer is; a plain invite
        // opens no recovery window.
        assert_eq!(invited_node("phone2").as_deref(), Some("newid"));
        assert_eq!(invited_node("phone3"), None);
        assert_eq!(recovery_for(&store.lock().unwrap(), "phone2"), None);

        let r = redeem(&store, &cfg, &m.invite.to_arbiter, ask(&m.invite.secret)).await;
        assert!(
            r["error"].as_str().unwrap_or("").contains("used already"),
            "{r}"
        );

        let late = mint(&cfg, "phone3", Some("http://10.0.30.5:9900"), 0).unwrap();
        let r = redeem(
            &store,
            &cfg,
            &late.invite.to_arbiter,
            JoinAsk {
                invite: late.invite.id.clone(),
                secret: late.invite.secret.clone(),
                node_id: "late".into(),
            },
        )
        .await;
        assert!(r["error"].as_str().unwrap_or("").contains("expired"), "{r}");
    }

    const LAPTOP: &str = "38a5e4dd-0000-4000-8000-00000000000a";

    /// An arbiter that links `laptop` at 10.0.30.2, for the recovery
    /// invite tests: its config is on disk (a redeem rewrites it) and
    /// loaded back.
    fn arbiter_linking_laptop() -> (VelesConfig, crate::store::Store, String) {
        let me = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir()).unwrap();
        std::fs::write(
            crate::configedit::config_path(),
            format!(
                "[node]\nendpoint = \"http://10.0.30.1:9900\"\n\n[cluster]\narbiter = \"{}\"\n\
                 gossip_peers = [\"laptop\"]\n\n[a2a_agents.laptop]\n\
                 url = \"http://10.0.30.2:9900\"\nauth = {{ type = \"bearer\", token = \"old-to-laptop\" }}\n",
                me.id
            ),
        )
        .unwrap();
        crate::authn::write_dotenv("A2A_PEER_TOKENS", "laptop:old-from-laptop").unwrap();
        let cfg = crate::config::load().unwrap();
        let store = crate::store::Store::open(&crate::config::data_dir().join("state.db")).unwrap();
        (cfg, store, me.id)
    }

    /// A recovery invite names the member it re-admits — the id this
    /// node knows that peer by, its link's url — and only for a peer it
    /// already links; a plain invite for that name is still refused.
    #[test]
    fn a_recovery_invite_names_the_member_it_readmits() {
        let (_home, _g) = crate::testenv::scratch_home();
        let (cfg, store, _me) = arbiter_linking_laptop();
        let e = mint_recovery(&cfg, &store, "phone", None, 600).unwrap_err();
        assert!(e.to_string().contains("not a peer of this node"), "{e}");
        let e = mint_recovery(&cfg, &store, "laptop", None, 600).unwrap_err();
        assert!(e.to_string().contains("does not know which node"), "{e}");

        store
            .node_register(LAPTOP, "laptop", "", "", "", None, None, None, None)
            .unwrap();
        let m = mint_recovery(&cfg, &store, "laptop", None, 600).unwrap();
        assert_eq!(m.recover_via, Some("registry"));
        let r = m
            .invite
            .recover
            .clone()
            .expect("a recovery invite says whom");
        assert_eq!((r.id.as_str(), r.name.as_str()), (LAPTOP, "laptop"));
        assert_eq!(m.invite.url, "http://10.0.30.2:9900");
        assert!(m.wg_client.is_none(), "the member keeps its tunnel");
        assert_eq!(
            decode(&encode(&m.invite).unwrap()).unwrap(),
            m.invite,
            "the field survives the QR text"
        );
        let row = ledger().into_iter().find(|p| p.id == m.invite.id).unwrap();
        assert_eq!(row.recover.as_deref(), Some(LAPTOP));
        assert!(mint_recovery(&cfg, &store, "laptop", None, 600)
            .unwrap_err()
            .to_string()
            .contains("still open"));
        assert!(revoke("laptop").unwrap());
        assert!(mint(&cfg, "laptop", Some("http://10.0.30.2:9900"), 600)
            .unwrap_err()
            .to_string()
            .contains("already a peer"));

        // What the peer's store offers spoke for beats a registry name;
        // an explicit --node beats both.
        let other = "77aa0000-0000-4000-8000-0000000000bb";
        crate::storesync::check_sender(&store, "laptop", other, "arbiter-self").unwrap();
        let m = mint_recovery(&cfg, &store, "laptop", None, 600).unwrap();
        assert_eq!(m.recover_via, Some("store sync"));
        assert_eq!(m.invite.recover.unwrap().id, other);
        assert!(revoke("laptop").unwrap());
        let m = mint_recovery(&cfg, &store, "laptop", Some("38a5e4dd"), 600).unwrap();
        assert_eq!(m.recover_via, Some("--node"));
        assert_eq!(m.invite.recover.unwrap().id, LAPTOP);
        assert!(mint_recovery(&cfg, &store, "laptop", Some("ffff"), 600).is_err());
    }

    /// The redeem of a recovery invite: asked as another id it is refused
    /// AND left unclaimed (the right node can still use it); asked as the
    /// node it names, it rotates this node's side of the link — both
    /// tokens new, the old ones gone — and opens the recovery window.
    #[tokio::test]
    async fn a_recovery_redeem_rotates_the_link_for_the_node_it_names_only() {
        let (_home, _g) = crate::testenv::scratch_home();
        let (cfg, store, _me) = arbiter_linking_laptop();
        store
            .node_register(LAPTOP, "laptop", "", "", "", None, None, None, None)
            .unwrap();
        let m = mint_recovery(&cfg, &store, "laptop", None, 600).unwrap();
        assert_eq!(recovery_for(&store, "laptop"), None, "not until redeemed");
        let store = std::sync::Arc::new(std::sync::Mutex::new(store));
        let ask = |node: &str| JoinAsk {
            invite: m.invite.id.clone(),
            secret: m.invite.secret.clone(),
            node_id: node.to_string(),
        };
        let r = redeem(&store, &cfg, &m.invite.to_arbiter, ask("a-new-id")).await;
        assert_eq!(r["status"], "refused", "{r}");
        assert!(r["error"].as_str().unwrap().contains("--as"), "{r}");
        let row = ledger().into_iter().find(|p| p.id == m.invite.id).unwrap();
        assert_eq!(row.redeemed_at, None, "a wrong id does not burn the invite");

        let r = redeem(&store, &cfg, &m.invite.to_arbiter, ask(LAPTOP)).await;
        assert_eq!(r["status"], "ok", "{r}");
        assert_eq!(r["recover"], LAPTOP);
        let after = crate::config::load().unwrap();
        assert_eq!(after.a2a_agents["laptop"].url, "http://10.0.30.2:9900");
        assert_eq!(after.a2a_agents["laptop"].auth.token, m.invite.from_arbiter);
        let env = crate::authn::dotenv_value("A2A_PEER_TOKENS").unwrap();
        assert_eq!(env, format!("laptop:{}", m.invite.to_arbiter));
        let (id, until) = recovery_for(&store.lock().unwrap(), "laptop").unwrap();
        assert_eq!(id, LAPTOP);
        assert!(until > now() + RECOVERY_WINDOW_SECS - 60, "{until}");
        assert_eq!(recovery_for(&store.lock().unwrap(), "phone"), None);
    }

    /// A member takes a recovery introduction from its arbiter like any
    /// other — new tokens in place of the old — and marks the window;
    /// one that names a different node than the member's gossip says
    /// that peer is, is refused and writes nothing.
    #[test]
    fn a_member_marks_a_recovering_peer_it_knows_and_refuses_a_stranger() {
        let (_home, _g) = crate::testenv::scratch_home();
        let store = crate::store::Store::open(&crate::config::data_dir().join("state.db")).unwrap();
        store
            .node_register(
                "arbiterid",
                "vps",
                "",
                "",
                "",
                None,
                Some("http://10.0.30.1:9900"),
                None,
                None,
            )
            .unwrap();
        std::fs::write(
            crate::configedit::config_path(),
            "[cluster]\narbiter = \"arbiterid\"\ngossip_peers = [\"vps\", \"laptop\"]\n\n\
             [a2a_agents.vps]\nurl = \"http://10.0.30.1:9900\"\nauth = { type = \"bearer\", token = \"tv\" }\n\n\
             [a2a_agents.laptop]\nurl = \"http://10.0.30.2:9900\"\nauth = { type = \"bearer\", token = \"old\" }\n",
        )
        .unwrap();
        let cfg = crate::config::load().unwrap();
        // This member's gossip says `laptop` is LAPTOP.
        store
            .node_register(LAPTOP, "laptop", "", "", "", None, None, None, None)
            .unwrap();
        store.node_observe(LAPTOP, "10.0.30.2", "laptop").unwrap();
        let ask = MemberAsk {
            arbiter: "arbiterid".into(),
            name: "laptop".into(),
            node_id: "someone-else".into(),
            url: "http://10.0.30.2:9900".into(),
            to_new: "member-to-laptop".into(),
            from_new: "laptop-to-member".into(),
            lists: vec!["gossip_peers".into()],
            recover: true,
        };
        let r = accept_member(&store, &cfg, "vps", ask.clone());
        assert_eq!(r["status"], "refused", "{r}");
        assert_eq!(
            crate::config::load().unwrap().a2a_agents["laptop"]
                .auth
                .token,
            "old"
        );
        assert_eq!(recovery_for(&store, "laptop"), None);

        let mut ok = ask;
        ok.node_id = LAPTOP.into();
        let r = accept_member(&store, &cfg, "vps", ok);
        assert_eq!(r["status"], "ok", "{r}");
        assert_eq!(
            crate::config::load().unwrap().a2a_agents["laptop"]
                .auth
                .token,
            "member-to-laptop"
        );
        let (id, _until) = recovery_for(&store, "laptop").unwrap();
        assert_eq!(id, LAPTOP);
    }
}

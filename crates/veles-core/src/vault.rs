//! The secrets vault: encrypted, signed, replicated between the nodes
//! of one pool (USER, 2026-09-27: *"copying them across nodes sounds
//! kind of bad, we should have a shared replicated vault"*).
//!
//! ## What is on disk
//!
//! `vault_dir()` (`<config_dir>/vault`, or `VELES_VAULT_DIR`) holds:
//!
//! - `identity.txt` — this node's age X25519 identity, in the file
//!   format `age-keygen` writes, so `age -d -i identity.txt` opens any
//!   record by hand. Never leaves the machine.
//! - `signing.key` — this node's Ed25519 signing key (PKCS#8, base64).
//!   Never leaves the machine either.
//! - `vault.json` — the replicated part. Every value in it is an age
//!   ciphertext; names and policies are cleartext (names were already
//!   in the system prompt), and every row is signed by its writer.
//!
//! All three are 0600 in a 0700 directory. The replicated file lives
//! beside the keys, not in `state.db`, because the resolver runs on
//! every provider request from code that holds no store handle, and
//! `state.db` is the gateway's busiest file.
//!
//! ## Why each piece exists
//!
//! - **One age file per value, encrypted to every member.** age wraps
//!   the file key once per recipient, so there is no separate data key
//!   to rotate or lose.
//! - **Signatures.** Recipients are public keys; anyone who can put a
//!   row in the inbox could encrypt a forged value to them. A row only
//!   counts when a member signed it.
//! - **Membership is rows too.** `init` makes the first node the root;
//!   every later admit or revoke is signed by a member. A node syncing
//!   for the first time adopts a vault only by its root's fingerprint
//!   (`trust`), which the operator reads off the other machine.
//! - **Rekeys never win.** An admit or revoke re-encrypts the current
//!   values under the new member set as `Rekey` rows that point at the
//!   write they re-encode. Which write is current is decided by the
//!   writes alone (last writer wins on `(at, by, seq)`, configsync's
//!   rule), so re-encrypting can never beat a concurrent real write.
//! - **A revoke vouches.** A revoked node still holds its signing key
//!   and could sign rows with any timestamp it likes. The revoke row
//!   lists the revoked node's rows the revoker held; everything else
//!   that node ever signs is refused.
//!
//! No recovery key (USER, 2026-09-27): losing every node loses the
//! vault, and the keys are entered again.

use anyhow::{bail, Context, Result};
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const IDENTITY_FILE: &str = "identity.txt";
const SIGNING_FILE: &str = "signing.key";
const VAULT_FILE: &str = "vault.json";
const LOCK_FILE: &str = "vault.lock";
const SIG_DOMAIN: &str = "veles-vault-v1";

/// Where the vault lives: `VELES_VAULT_DIR` when it names an absolute
/// path (the NixOS module's `vault.identityDir`), else
/// `<config_dir>/vault`.
pub fn vault_dir() -> PathBuf {
    if crate::config::dirs().is_none() {
        if let Ok(v) = std::env::var("VELES_VAULT_DIR") {
            let p = PathBuf::from(v.trim());
            if p.is_absolute() {
                return p;
            }
        }
    }
    crate::config::config_dir().join("vault")
}

// ─────────────────────────────────────────────────────────────────
// Policy: what the model may do with a value
// ─────────────────────────────────────────────────────────────────

/// What a secret is for. The kind picks the default [`Policy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A model provider's API key: the engine uses it, and so may the
    /// model (usage checks, image and video pipelines, vendor CLIs).
    Provider,
    /// A credential that IS a surface: a bot token, an IRC password, a
    /// bearer token. The engine uses it; the model has no reason to.
    Platform,
    /// Anything the operator stored for the model to use.
    User,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Provider => "provider",
            Kind::Platform => "platform",
            Kind::User => "user",
        }
    }
}

/// Where a value may go besides the engine itself, which may always
/// use it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub kind: Kind,
    /// `$NAME` in terminal and execute_code.
    pub shell: bool,
    /// Hosts `api_request` may send it to. Empty = none.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// One line for `secret_list`.
    #[serde(default)]
    pub note: String,
}

impl Policy {
    /// The defaults per kind. Provider keys keep shell use because the
    /// model needs them for usage checks and multi-step media jobs
    /// (USER, 2026-09-27); platform credentials never reach the model.
    pub fn default_for(kind: Kind, hosts: Vec<String>) -> Self {
        let shell = !matches!(kind, Kind::Platform);
        let hosts = if matches!(kind, Kind::Platform) {
            Vec::new()
        } else {
            hosts
        };
        Self {
            kind,
            shell,
            hosts,
            note: String::new(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Rows
// ─────────────────────────────────────────────────────────────────

/// A row's identity: the node that signed it and that node's own
/// sequence number. Imports are idempotent on it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RowId {
    pub node: String,
    pub seq: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberOp {
    Admit,
    Revoke,
}

/// An admit or revoke of `node`, signed by `by`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub node: String,
    pub name: String,
    /// age recipient (`age1…`).
    pub recipient: String,
    /// Ed25519 public key, base64.
    pub sign_pk: String,
    pub op: MemberOp,
    /// On a revoke: the revoked node's rows the revoker vouches for.
    #[serde(default)]
    pub vouched: Vec<RowId>,
    pub by: String,
    pub seq: i64,
    pub at: String,
    #[serde(default)]
    pub sig: String,
}

/// One write of one secret. `cipher == ""` is a tombstone, and a
/// tombstone outranks silence the way configsync's does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub name: String,
    pub policy: Policy,
    /// Armored age file; "" = removed.
    pub cipher: String,
    /// The node ids the ciphertext is encrypted to.
    #[serde(default)]
    pub to: Vec<String>,
    /// The fingerprint of each recipient in `to`, same order: WHICH key
    /// of that node it was encrypted to. A node id alone cannot tell a
    /// node from the same node with regenerated keys (pitfall 76).
    /// Empty on rows written before 2026-10-20 — and then left out of
    /// the JSON, so the bytes those rows were signed over are unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to_fp: Vec<String>,
    pub by: String,
    pub seq: i64,
    pub at: String,
    #[serde(default)]
    pub sig: String,
}

/// A re-encryption of the write `of` for a newer member set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rekey {
    pub of: RowId,
    pub cipher: String,
    pub to: Vec<String>,
    /// As [`Record::to_fp`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to_fp: Vec<String>,
    pub by: String,
    pub seq: i64,
    pub at: String,
    #[serde(default)]
    pub sig: String,
}

/// The replicated file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultFile {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub members: Vec<Member>,
    #[serde(default)]
    pub records: Vec<Record>,
    #[serde(default)]
    pub rekeys: Vec<Rekey>,
}

fn one() -> u32 {
    1
}

impl Member {
    /// The fingerprint of the keys this row admits.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.recipient, &self.sign_pk)
    }
    fn id(&self) -> RowId {
        RowId {
            node: self.by.clone(),
            seq: self.seq,
        }
    }
    fn signed_bytes(&self) -> Vec<u8> {
        let mut c = self.clone();
        c.sig.clear();
        signed_bytes("member", &c)
    }
}

impl Record {
    pub fn id(&self) -> RowId {
        RowId {
            node: self.by.clone(),
            seq: self.seq,
        }
    }
    pub fn is_tombstone(&self) -> bool {
        self.cipher.is_empty()
    }
    fn signed_bytes(&self) -> Vec<u8> {
        let mut c = self.clone();
        c.sig.clear();
        signed_bytes("record", &c)
    }
}

impl Rekey {
    fn id(&self) -> RowId {
        RowId {
            node: self.by.clone(),
            seq: self.seq,
        }
    }
    fn signed_bytes(&self) -> Vec<u8> {
        let mut c = self.clone();
        c.sig.clear();
        signed_bytes("rekey", &c)
    }
}

/// The exact bytes a signature covers: a domain tag, the row kind, and
/// the row's JSON with `sig` empty. Struct field order is fixed, so the
/// JSON is canonical for rows this code wrote.
fn signed_bytes<T: Serialize>(kind: &str, row: &T) -> Vec<u8> {
    let json = serde_json::to_string(row).unwrap_or_default();
    format!("{SIG_DOMAIN}\n{kind}\n{json}").into_bytes()
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn verify(sign_pk_b64: &str, msg: &[u8], sig_b64: &str) -> bool {
    use base64::Engine as _;
    let (Ok(pk), Ok(sig)) = (b64().decode(sign_pk_b64), b64().decode(sig_b64)) else {
        return false;
    };
    UnparsedPublicKey::new(&ED25519, pk)
        .verify(msg, &sig)
        .is_ok()
}

/// A short, speakable fingerprint of a node's two public keys — what
/// the operator compares on both screens before an admit or a first
/// sync.
pub fn fingerprint(recipient: &str, sign_pk: &str) -> String {
    use sha2::Digest as _;
    let d = sha2::Sha256::digest(format!("{recipient}\n{sign_pk}").as_bytes());
    let hex: String = d[..8].iter().map(|b| format!("{b:02x}")).collect();
    hex.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("-")
}

/// Fingerprints compare without dashes, spaces or case, so a spoken or
/// retyped one still matches.
pub fn same_fingerprint(a: &str, b: &str) -> bool {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let (a, b) = (norm(a), norm(b));
    !a.is_empty() && a == b
}

// ─────────────────────────────────────────────────────────────────
// This node's keys
// ─────────────────────────────────────────────────────────────────

/// This node's private keys and the public halves derived from them.
pub struct NodeKeys {
    pub node: String,
    pub name: String,
    identity: age::x25519::Identity,
    signing: Ed25519KeyPair,
}

impl std::fmt::Debug for NodeKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeKeys")
            .field("node", &self.node)
            .field("recipient", &self.recipient())
            .finish_non_exhaustive()
    }
}

impl NodeKeys {
    /// Load this node's keys from `dir`, or `None` when it has none.
    pub fn load(dir: &Path, node: &str, name: &str) -> Result<Option<Self>> {
        use age::secrecy::ExposeSecret as _;
        use base64::Engine as _;
        let id_path = dir.join(IDENTITY_FILE);
        let sk_path = dir.join(SIGNING_FILE);
        if !id_path.exists() && !sk_path.exists() {
            return Ok(None);
        }
        let id_raw = std::fs::read_to_string(&id_path)
            .with_context(|| format!("read {}", id_path.display()))?;
        let line = id_raw
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("AGE-SECRET-KEY-"))
            .with_context(|| format!("{}: no AGE-SECRET-KEY line", id_path.display()))?;
        let identity: age::x25519::Identity = line
            .parse()
            .map_err(|e: &str| anyhow::anyhow!("{}: {e}", id_path.display()))?;
        // Round-trip guard: what parsed is what is on disk.
        debug_assert_eq!(identity.to_string().expose_secret(), line);
        let sk_raw = std::fs::read_to_string(&sk_path)
            .with_context(|| format!("read {}", sk_path.display()))?;
        let pkcs8 = b64()
            .decode(sk_raw.trim())
            .with_context(|| format!("{}: not base64", sk_path.display()))?;
        let signing = Ed25519KeyPair::from_pkcs8(&pkcs8)
            .map_err(|e| anyhow::anyhow!("{}: {e}", sk_path.display()))?;
        Ok(Some(Self {
            node: node.to_string(),
            name: name.to_string(),
            identity,
            signing,
        }))
    }

    /// Load the keys, minting both on first use.
    pub fn load_or_create(dir: &Path, node: &str, name: &str) -> Result<Self> {
        if let Some(k) = Self::load(dir, node, name)? {
            return Ok(k);
        }
        use age::secrecy::ExposeSecret as _;
        use base64::Engine as _;
        ensure_private_dir(dir)?;
        let identity = age::x25519::Identity::generate();
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| anyhow::anyhow!("could not generate a signing key"))?;
        let signing = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|e| anyhow::anyhow!("signing key: {e}"))?;
        let id_text = format!(
            "# veles vault identity for node {node} — never copy this file off the machine\n\
             # created: {}\n# public key: {}\n{}\n",
            now_stamp(),
            identity.to_public(),
            identity.to_string().expose_secret()
        );
        write_private_atomic(&dir.join(IDENTITY_FILE), id_text.as_bytes())?;
        write_private_atomic(
            &dir.join(SIGNING_FILE),
            format!("{}\n", b64().encode(pkcs8.as_ref())).as_bytes(),
        )?;
        Ok(Self {
            node: node.to_string(),
            name: name.to_string(),
            identity,
            signing,
        })
    }

    pub fn recipient(&self) -> String {
        self.identity.to_public().to_string()
    }

    pub fn sign_pk(&self) -> String {
        use base64::Engine as _;
        b64().encode(self.signing.public_key().as_ref())
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.recipient(), &self.sign_pk())
    }

    fn sign(&self, msg: &[u8]) -> String {
        use base64::Engine as _;
        b64().encode(self.signing.sign(msg).as_ref())
    }

    fn decrypt(&self, cipher: &str) -> Result<String> {
        let bytes = age::decrypt(&self.identity, cipher.as_bytes())
            .map_err(|e| anyhow::anyhow!("decrypt: {e}"))?;
        String::from_utf8(bytes).context("decrypted value is not UTF-8")
    }

    /// This node as a member row (unsigned; the caller fills by/seq).
    fn member_row(&self, op: MemberOp) -> Member {
        Member {
            node: self.node.clone(),
            name: self.name.clone(),
            recipient: self.recipient(),
            sign_pk: self.sign_pk(),
            op,
            vouched: Vec::new(),
            by: String::new(),
            seq: 0,
            at: String::new(),
            sig: String::new(),
        }
    }
}

fn encrypt_to(value: &str, recipients: &[String]) -> Result<String> {
    use std::io::Write as _;
    let parsed: Vec<age::x25519::Recipient> = recipients
        .iter()
        .map(|r| {
            r.parse()
                .map_err(|e: &str| anyhow::anyhow!("recipient {r}: {e}"))
        })
        .collect::<Result<_>>()?;
    if parsed.is_empty() {
        bail!("no recipients to encrypt to");
    }
    let enc = age::Encryptor::with_recipients(parsed.iter().map(|r| r as &dyn age::Recipient))
        .map_err(|e| anyhow::anyhow!("encrypt: {e}"))?;
    let mut out = Vec::new();
    let armor = age::armor::ArmoredWriter::wrap_output(&mut out, age::armor::Format::AsciiArmor)?;
    let mut w = enc.wrap_output(armor)?;
    w.write_all(value.as_bytes())?;
    w.finish()?.finish()?;
    String::from_utf8(out).context("armored output is not UTF-8")
}

// ─────────────────────────────────────────────────────────────────
// Verification and merge
// ─────────────────────────────────────────────────────────────────

/// The membership a set of rows proves, after every signature and the
/// revoke rule have been checked.
#[derive(Debug, Clone, Default)]
pub struct Membership {
    /// The root's admit row, if any.
    pub root: Option<Member>,
    /// Current members by node id: their latest admit row.
    pub current: BTreeMap<String, Member>,
    /// Revoked nodes: the rows each revoke vouched for.
    pub revoked: BTreeMap<String, BTreeSet<RowId>>,
    /// Every signing key a node has had, for checking its rows.
    keys: BTreeMap<String, Vec<String>>,
}

impl Membership {
    pub fn is_member(&self, node: &str) -> bool {
        self.current.contains_key(node)
    }

    /// Recipients of the current member set, in node order.
    pub fn recipients(&self) -> (Vec<String>, Vec<String>) {
        let nodes: Vec<String> = self.current.keys().cloned().collect();
        let recips = self.current.values().map(|m| m.recipient.clone()).collect();
        (nodes, recips)
    }

    /// The fingerprint of each current member's keys, in node order
    /// (parallel to [`Membership::recipients`]).
    pub fn fingerprints(&self) -> Vec<String> {
        self.current.values().map(Member::fingerprint).collect()
    }

    /// May a row signed by `by` with identity `id` count?
    fn signer_ok(&self, by: &str, id: &RowId) -> bool {
        if self.current.contains_key(by) {
            return true;
        }
        self.revoked.get(by).is_some_and(|v| v.contains(id))
    }

    fn sig_ok(&self, by: &str, msg: &[u8], sig: &str) -> bool {
        self.keys
            .get(by)
            .is_some_and(|ks| ks.iter().any(|k| verify(k, msg, sig)))
    }
}

fn row_order(at: &str, by: &str, seq: i64) -> (chrono::DateTime<chrono::Utc>, String, i64) {
    let ts = chrono::DateTime::parse_from_rfc3339(at)
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::DateTime::from_timestamp(0, 0).unwrap());
    (ts, by.to_string(), seq)
}

/// Replay membership rows in time order. The first row must be a root:
/// a self-signed admit. Every later row must be signed by a member at
/// that point. Rows signed by a node that ends up revoked count only
/// when its revoke vouched for them — iterated to a fixpoint, since
/// dropping a row can change who was a member when.
///
/// A node admitted again after a revoke is trusted again, rows and all;
/// re-admitting is the operator saying so.
pub fn membership(members: &[Member]) -> Membership {
    membership_and_accepted(members).0
}

fn membership_and_accepted(members: &[Member]) -> (Membership, BTreeSet<RowId>) {
    let mut excluded: BTreeSet<RowId> = BTreeSet::new();
    loop {
        let (m, accepted) = replay(members, &excluded);
        let newly: BTreeSet<RowId> = members
            .iter()
            .map(Member::id)
            .filter(|id| !excluded.contains(id))
            .filter(|id| {
                !m.current.contains_key(&id.node)
                    && m.revoked.get(&id.node).is_some_and(|v| !v.contains(id))
            })
            .collect();
        if newly.is_empty() {
            return (m, accepted);
        }
        excluded.extend(newly);
    }
}

fn replay(members: &[Member], excluded: &BTreeSet<RowId>) -> (Membership, BTreeSet<RowId>) {
    let mut rows: Vec<&Member> = members
        .iter()
        .filter(|r| !excluded.contains(&r.id()))
        .collect();
    rows.sort_by_key(|r| row_order(&r.at, &r.by, r.seq));
    let mut m = Membership::default();
    let mut accepted = BTreeSet::new();
    for row in rows {
        let msg = row.signed_bytes();
        if m.root.is_none() {
            // The root: a self-admit, verified against its own key.
            if row.op == MemberOp::Admit
                && row.by == row.node
                && verify(&row.sign_pk, &msg, &row.sig)
            {
                m.root = Some(row.clone());
                m.keys
                    .entry(row.node.clone())
                    .or_default()
                    .push(row.sign_pk.clone());
                m.current.insert(row.node.clone(), row.clone());
                accepted.insert(row.id());
            }
            continue;
        }
        let Some(signer) = m.current.get(&row.by) else {
            continue;
        };
        if !verify(&signer.sign_pk, &msg, &row.sig) {
            continue;
        }
        accepted.insert(row.id());
        match row.op {
            MemberOp::Admit => {
                let ks = m.keys.entry(row.node.clone()).or_default();
                if !ks.contains(&row.sign_pk) {
                    ks.push(row.sign_pk.clone());
                }
                m.revoked.remove(&row.node);
                m.current.insert(row.node.clone(), row.clone());
            }
            MemberOp::Revoke => {
                if m.current.remove(&row.node).is_some() {
                    m.revoked
                        .insert(row.node.clone(), row.vouched.iter().cloned().collect());
                }
            }
        }
    }
    (m, accepted)
}

/// What a merge kept, refused, and changed.
#[derive(Debug, Default)]
pub struct MergeReport {
    pub new_members: usize,
    pub new_records: usize,
    pub new_rekeys: usize,
    /// Rows refused, each with why. Never a value — names only.
    pub refused: Vec<String>,
    /// Names whose current write changed.
    pub changed: Vec<String>,
    /// Anything else the operator should read (adoption decisions).
    pub notes: Vec<String>,
}

impl MergeReport {
    pub fn summarize(&self) -> String {
        let mut out = format!(
            "{} member row(s), {} write(s), {} rekey(s) new",
            self.new_members, self.new_records, self.new_rekeys
        );
        if !self.changed.is_empty() {
            out.push_str(&format!("; changed: {}", self.changed.join(", ")));
        }
        for r in &self.refused {
            out.push_str(&format!("\n  refused: {r}"));
        }
        for n in &self.notes {
            out.push_str(&format!("\n  note: {n}"));
        }
        out
    }
}

/// Keep only rows the membership vouches for, verified. Used on every
/// load of an incoming file and after every merge, so a local file
/// holds nothing that would not verify from scratch.
fn verified(file: &VaultFile) -> (VaultFile, Vec<String>) {
    let (m, accepted) = membership_and_accepted(&file.members);
    let mut refused = Vec::new();
    let mut out = VaultFile {
        version: 1,
        ..Default::default()
    };
    // Member rows: keep what the replay accepted (root, and rows signed
    // by a member at their time and not excluded by a revoke).
    for row in &file.members {
        if accepted.contains(&row.id()) {
            out.members.push(row.clone());
        } else {
            refused.push(format!(
                "member row {}#{} ({:?} {}): not signed by a member",
                row.by, row.seq, row.op, row.node
            ));
        }
    }
    for r in &file.records {
        if m.signer_ok(&r.by, &r.id()) && m.sig_ok(&r.by, &r.signed_bytes(), &r.sig) {
            out.records.push(r.clone());
        } else {
            refused.push(format!(
                "write of {} by {}#{}: not signed by a member",
                r.name, r.by, r.seq
            ));
        }
    }
    for k in &file.rekeys {
        if m.signer_ok(&k.by, &k.id()) && m.sig_ok(&k.by, &k.signed_bytes(), &k.sig) {
            out.rekeys.push(k.clone());
        } else {
            refused.push(format!("rekey {}#{}: not signed by a member", k.by, k.seq));
        }
    }
    (out, refused)
}

/// Union two files and keep what verifies. `trust` is the root
/// fingerprint the operator read off the other machine; it is required
/// only when `local` has no root yet (the first sync). A different
/// root than the local one is refused outright: that is another vault.
pub fn merge(
    local: &VaultFile,
    incoming: &VaultFile,
    trust: Option<&str>,
) -> Result<(VaultFile, MergeReport)> {
    let local_root = membership(&local.members).root;
    let incoming_root = membership(&incoming.members).root;
    match (&local_root, &incoming_root) {
        (Some(l), Some(i)) if l.id() != i.id() || l.sign_pk != i.sign_pk => bail!(
            "this offer belongs to another vault (root {} {}, ours is {} {}) — refused",
            i.name,
            fingerprint(&i.recipient, &i.sign_pk),
            l.name,
            fingerprint(&l.recipient, &l.sign_pk)
        ),
        (None, Some(i)) => {
            let fp = fingerprint(&i.recipient, &i.sign_pk);
            match trust {
                Some(t) if same_fingerprint(t, &fp) => {}
                Some(t) => bail!(
                    "the offer's vault root is {} {fp}, not {t} — refused",
                    i.name
                ),
                None => bail!(
                    "first sync: this node has no vault yet. The offer's root is {} with \
                     fingerprint {fp}; check it on that machine (`veles vault whoami`) and \
                     apply again with --trust {fp}",
                    i.name
                ),
            }
        }
        _ => {}
    }
    let mut union = local.clone();
    let mut report = MergeReport::default();
    let have_m: BTreeSet<RowId> = union.members.iter().map(Member::id).collect();
    let have_r: BTreeSet<RowId> = union.records.iter().map(Record::id).collect();
    let have_k: BTreeSet<RowId> = union.rekeys.iter().map(Rekey::id).collect();
    for row in &incoming.members {
        match union.members.iter().find(|x| x.id() == row.id()) {
            Some(x) if x != row => report.refused.push(format!(
                "member row {}#{}: differs from the copy held here",
                row.by, row.seq
            )),
            Some(_) => {}
            None if !have_m.contains(&row.id()) => union.members.push(row.clone()),
            None => {}
        }
    }
    for row in &incoming.records {
        match union.records.iter().find(|x| x.id() == row.id()) {
            Some(x) if x != row => report.refused.push(format!(
                "write {}#{}: differs from the copy held here",
                row.by, row.seq
            )),
            Some(_) => {}
            None if !have_r.contains(&row.id()) => union.records.push(row.clone()),
            None => {}
        }
    }
    for row in &incoming.rekeys {
        match union.rekeys.iter().find(|x| x.id() == row.id()) {
            Some(x) if x != row => report.refused.push(format!(
                "rekey {}#{}: differs from the copy held here",
                row.by, row.seq
            )),
            Some(_) => {}
            None if !have_k.contains(&row.id()) => union.rekeys.push(row.clone()),
            None => {}
        }
    }
    let before = current_writes(local);
    let (kept, refused) = verified(&union);
    report.refused.extend(refused);
    report.new_members = kept
        .members
        .iter()
        .filter(|r| !have_m.contains(&r.id()))
        .count();
    report.new_records = kept
        .records
        .iter()
        .filter(|r| !have_r.contains(&r.id()))
        .count();
    report.new_rekeys = kept
        .rekeys
        .iter()
        .filter(|r| !have_k.contains(&r.id()))
        .count();
    let after = current_writes(&kept);
    let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for n in names {
        if before.get(n).map(|r| r.id()) != after.get(n).map(|r| r.id()) {
            report.changed.push(n.clone());
        }
    }
    Ok((kept, report))
}

/// The current write per name (tombstones included): last writer wins
/// on `(at, by, seq)`, decided by the writes alone.
pub fn current_writes(file: &VaultFile) -> BTreeMap<String, Record> {
    let mut out: BTreeMap<String, Record> = BTreeMap::new();
    for r in &file.records {
        let k = row_order(&r.at, &r.by, r.seq);
        match out.get(&r.name) {
            Some(cur) if row_order(&cur.at, &cur.by, cur.seq) >= k => {}
            _ => {
                out.insert(r.name.clone(), r.clone());
            }
        }
    }
    out
}

/// One encoding of a write: its ciphertext and who it is addressed to.
struct Encoding<'a> {
    cipher: &'a str,
    to: &'a [String],
    to_fp: &'a [String],
    at: &'a str,
}

impl Encoding<'_> {
    /// The (node id, fingerprint) pairs it is encrypted to, or `None`
    /// for a row written before `to_fp` existed: then WHICH key of each
    /// node it was encrypted to is unknown.
    fn addressed(&self) -> Option<BTreeSet<(&str, &str)>> {
        (self.to_fp.len() == self.to.len() && !self.to.is_empty()).then(|| {
            self.to
                .iter()
                .map(String::as_str)
                .zip(self.to_fp.iter().map(String::as_str))
                .collect()
        })
    }
}

/// The encodings of one write, newest first: its own cipher and every
/// rekey of it.
fn ciphers_of<'a>(file: &'a VaultFile, w: &'a Record) -> Vec<Encoding<'a>> {
    let mut v = vec![Encoding {
        cipher: &w.cipher,
        to: &w.to,
        to_fp: &w.to_fp,
        at: &w.at,
    }];
    let id = w.id();
    for k in &file.rekeys {
        if k.of == id {
            v.push(Encoding {
                cipher: &k.cipher,
                to: &k.to,
                to_fp: &k.to_fp,
                at: &k.at,
            });
        }
    }
    v.sort_by_key(|c| std::cmp::Reverse(row_order(c.at, "", 0)));
    v
}

// ─────────────────────────────────────────────────────────────────
// The local vault: file, lock, writes
// ─────────────────────────────────────────────────────────────────

/// An open vault directory: this node's keys and the replicated file.
pub struct Vault {
    pub dir: PathBuf,
    pub keys: NodeKeys,
    pub file: VaultFile,
}

/// A value as the resolver hands it out.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret {
    pub name: String,
    pub value: String,
    pub policy: Policy,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the value, not even in a panic message.
        f.debug_struct("Secret")
            .field("name", &self.name)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

/// One name as `list` shows it: never a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listing {
    pub name: String,
    pub policy: Policy,
    pub by: String,
    pub at: String,
    /// Can THIS node decrypt the current value?
    pub readable: bool,
}

/// Where this node's identity stands against the member row the vault
/// holds for its node id. Membership is keyed by node id, so a node
/// whose key files were regenerated (a lost home, a restore without
/// `vault/`) still LOOKS like a member — every row says "encrypted to
/// it" — while nothing opens with its keys (pitfall 76).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityStatus {
    /// No current member row for this node id: not (or no longer) a
    /// member. Minting keys here replaces nothing.
    Fresh,
    /// The member row's keys are this node's keys.
    Member,
    /// The member row admits other keys than the ones on disk here: the
    /// key files were replaced since the admit.
    Mismatch {
        recorded_fp: String,
        local_fp: String,
    },
    /// The vault holds a member row for this node, but its key files
    /// are gone.
    MissingKeys,
}

impl IdentityStatus {
    /// A state the operator has to act on.
    pub fn is_problem(&self) -> bool {
        matches!(
            self,
            IdentityStatus::Mismatch { .. } | IdentityStatus::MissingKeys
        )
    }

    /// One line for `vault status`, `vault whoami`, `doctor` and the
    /// gateway's start-up warning.
    pub fn summary(&self, node: &str, dir: &Path) -> String {
        match self {
            IdentityStatus::Fresh => {
                "not a member — `veles vault join` and have a member admit this node".into()
            }
            IdentityStatus::Member => {
                "OK — this node's keys are the ones the vault admitted".into()
            }
            IdentityStatus::Mismatch {
                recorded_fp,
                local_fp,
            } => format!(
                "MISMATCH — the vault admitted this node ({node}) with fingerprint \
                 {recorded_fp}, but its keys in {} are {local_fp}: the key files were \
                 replaced, and nothing encrypted to the old key opens here. Restore the old \
                 identity.txt + signing.key from a backup, or send `veles vault join` and have \
                 a member run `veles vault admit {node} --fingerprint {local_fp}`",
                dir.display()
            ),
            IdentityStatus::MissingKeys => format!(
                "LOST — the vault has a member row for this node ({node}) but identity.txt / \
                 signing.key are gone from {}. Restore them from a backup (`veles restore`), \
                 or run `veles vault reidentify` for new keys and have a member admit the \
                 new fingerprint",
                dir.display()
            ),
        }
    }
}

/// The refusal for a node whose key files are gone while the vault
/// still names it. Minting new keys here would replace its identity
/// without a word — what happened to the laptop on 2026-10-19.
fn lost_identity_error(dir: &Path, node: &str, file: &VaultFile) -> anyhow::Error {
    let recorded = membership(&file.members)
        .current
        .get(node)
        .map(Member::fingerprint)
        .unwrap_or_default();
    anyhow::anyhow!(
        "this node's vault identity is lost: the vault admitted {node} with fingerprint \
         {recorded}, but identity.txt / signing.key are missing from {}. Restore them from a \
         backup (`veles backup` keeps `vault/`), or run `veles vault reidentify` to mint new \
         keys and have a member admit the new fingerprint — no keys were created",
        dir.display()
    )
}

/// What [`Vault::admit`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdmitOutcome {
    /// Names re-encrypted so the admitted keys can read them.
    pub rekeyed: Vec<String>,
    /// The node was a member with other keys: (old fp, new fp).
    pub replaced: Option<(String, String)>,
}

/// What [`reidentify`] did.
pub struct Reidentified {
    pub vault: Vault,
    /// Where the old key files went (empty: there were none).
    pub backed_up: Vec<PathBuf>,
    /// The fingerprint of the keys that were replaced, if they loaded.
    pub old_fp: Option<String>,
    /// The fingerprint the vault's member row for this node admits.
    pub recorded_fp: Option<String>,
}

impl Vault {
    /// Open `dir` with this node's EXISTING keys. Never mints: a node
    /// with no keys is told how it gets some, and one whose keys are
    /// gone while the vault names it is told it lost them.
    pub fn open(dir: &Path, node: &str, name: &str) -> Result<Self> {
        let file = read_file(dir)?;
        let Some(keys) = NodeKeys::load(dir, node, name)? else {
            if membership(&file.members).is_member(node) {
                return Err(lost_identity_error(dir, node, &file));
            }
            bail!(
                "this node has no vault keys yet — `veles vault init` (the first node), \
                 `veles vault join` (the others) or `veles secret set` creates them"
            );
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            keys,
            file,
        })
    }

    /// Open `dir`, minting this node's keys if it has none — for `init`,
    /// `join` and a first `secret set`. Still refuses when the vault
    /// holds a member row for this node: new keys then would REPLACE an
    /// identity, which only `veles vault reidentify` does.
    pub fn open_or_create(dir: &Path, node: &str, name: &str) -> Result<Self> {
        let file = read_file(dir)?;
        let keys = match NodeKeys::load(dir, node, name)? {
            Some(k) => k,
            None if membership(&file.members).is_member(node) => {
                return Err(lost_identity_error(dir, node, &file));
            }
            None => NodeKeys::load_or_create(dir, node, name)?,
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            keys,
            file,
        })
    }

    /// Open the vault of this machine: `vault_dir()` and the node id
    /// from `node.toml`. Never mints keys (see [`Vault::open`]).
    pub fn open_local() -> Result<Self> {
        let id = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?;
        Self::open(&vault_dir(), &id.id, &id.name)
    }

    /// [`Vault::open_or_create`] on this machine.
    pub fn open_or_create_local() -> Result<Self> {
        let id = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?;
        Self::open_or_create(&vault_dir(), &id.id, &id.name)
    }

    /// Where this node's keys stand against its member row.
    pub fn identity_status(&self) -> IdentityStatus {
        match self.membership().current.get(&self.keys.node) {
            None => IdentityStatus::Fresh,
            Some(m) => {
                let (recorded_fp, local_fp) = (m.fingerprint(), self.keys.fingerprint());
                if same_fingerprint(&recorded_fp, &local_fp) {
                    IdentityStatus::Member
                } else {
                    IdentityStatus::Mismatch {
                        recorded_fp,
                        local_fp,
                    }
                }
            }
        }
    }

    /// This machine's vault only if it already has keys — for read-only
    /// questions (`veles vault status`), which must not mint anything.
    pub fn open_local_if_any() -> Result<Option<Self>> {
        let Some(id) = crate::node::NodeIdentity::load(&crate::config::config_dir())? else {
            return Ok(None);
        };
        let dir = vault_dir();
        let Some(keys) = NodeKeys::load(&dir, &id.id, &id.name)? else {
            return Ok(None);
        };
        let file = read_file(&dir)?;
        Ok(Some(Self { dir, keys, file }))
    }

    pub fn membership(&self) -> Membership {
        membership(&self.file.members)
    }

    pub fn is_initialized(&self) -> bool {
        self.membership().root.is_some()
    }

    fn next_seq(&self) -> i64 {
        let me = &self.keys.node;
        let a = self
            .file
            .members
            .iter()
            .filter(|r| &r.by == me)
            .map(|r| r.seq);
        let b = self
            .file
            .records
            .iter()
            .filter(|r| &r.by == me)
            .map(|r| r.seq);
        let c = self
            .file
            .rekeys
            .iter()
            .filter(|r| &r.by == me)
            .map(|r| r.seq);
        a.chain(b).chain(c).max().unwrap_or(0) + 1
    }

    fn require_member(&self) -> Result<Membership> {
        let m = self.membership();
        if m.root.is_none() {
            bail!("this node has no vault yet — `veles vault init` on the first node, `veles vault join` on the others");
        }
        if !m.is_member(&self.keys.node) {
            bail!(
                "this node is not a member of the vault — ask a member to `veles vault admit` it"
            );
        }
        // A row signed with keys the member row does not carry would be
        // refused by every peer, and a value encrypted "to the members"
        // would go to this node's OLD key: write nothing.
        if let st @ IdentityStatus::Mismatch { .. } = self.identity_status() {
            bail!(
                "this node's keys are not the vault's record of it: {}",
                st.summary(&self.keys.node, &self.dir)
            );
        }
        Ok(m)
    }

    /// Make this node the root of a new vault.
    pub fn init(&mut self) -> Result<()> {
        if self.is_initialized() {
            bail!("this node already has a vault");
        }
        let mut row = self.keys.member_row(MemberOp::Admit);
        row.by = self.keys.node.clone();
        row.seq = self.next_seq();
        row.at = now_stamp();
        row.sig = self.keys.sign(&row.signed_bytes());
        self.file.members.push(row);
        Ok(())
    }

    /// Store `value` under `name`, encrypted to every current member.
    pub fn set(&mut self, name: &str, value: &str, policy: Policy) -> Result<()> {
        if crate::secrets::is_node_secret(name) {
            anyhow::bail!(
                "${name} is this node's own credential (every node has its own), so it \
                 stays in this node's .env or secrets.env and never enters the shared vault"
            );
        }
        if !crate::secrets::valid_name(name) {
            bail!("invalid secret name '{name}': use letters, digits and underscores; start with a letter or _");
        }
        if value.is_empty() {
            bail!("empty value for {name} — nothing stored (use unset to remove one)");
        }
        let m = self.require_member()?;
        let (to, recips) = m.recipients();
        let cipher = encrypt_to(value, &recips)?;
        self.push_record(name, policy, cipher, (to, m.fingerprints()));
        Ok(())
    }

    /// Change a name's policy, keeping its value.
    pub fn set_policy(&mut self, name: &str, policy: Policy) -> Result<()> {
        let value = self
            .get(name)?
            .with_context(|| format!("{name} is not in the vault"))?
            .value;
        self.set(name, &value, policy)
    }

    /// Remove `name` (a tombstone, so the removal replicates).
    pub fn unset(&mut self, name: &str) -> Result<()> {
        self.require_member()?;
        let cur = current_writes(&self.file);
        let Some(w) = cur.get(name).filter(|w| !w.is_tombstone()) else {
            bail!("{name} is not in the vault");
        };
        let policy = w.policy.clone();
        self.push_record(name, policy, String::new(), (Vec::new(), Vec::new()));
        Ok(())
    }

    fn push_record(
        &mut self,
        name: &str,
        policy: Policy,
        cipher: String,
        (to, to_fp): (Vec<String>, Vec<String>),
    ) {
        let mut r = Record {
            name: name.to_string(),
            policy,
            cipher,
            to,
            to_fp,
            by: self.keys.node.clone(),
            seq: self.next_seq(),
            at: now_stamp(),
            sig: String::new(),
        };
        // Two writes in the same nanosecond from one node must still
        // order by seq, and a clock that stepped back must not make a
        // new write lose to this node's own older one.
        if let Some(prev) = current_writes(&self.file).get(name) {
            if row_order(&prev.at, "", 0) >= row_order(&r.at, "", 0) {
                let bumped = row_order(&prev.at, "", 0).0 + chrono::Duration::nanoseconds(1);
                r.at = bumped.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            }
        }
        r.sig = self.keys.sign(&r.signed_bytes());
        self.file.records.push(r);
    }

    /// Decrypt one name's current value, if this node can.
    pub fn get(&self, name: &str) -> Result<Option<Secret>> {
        let cur = current_writes(&self.file);
        let Some(w) = cur.get(name) else {
            return Ok(None);
        };
        if w.is_tombstone() {
            return Ok(None);
        }
        let value = self.decrypt_write(w)?;
        Ok(Some(Secret {
            name: w.name.clone(),
            value,
            policy: w.policy.clone(),
        }))
    }

    fn decrypt_write(&self, w: &Record) -> Result<String> {
        let me = &self.keys.node;
        let mut tried = false;
        for enc in ciphers_of(&self.file, w) {
            if !enc.to.iter().any(|n| n == me) {
                continue;
            }
            tried = true;
            if let Ok(v) = self.keys.decrypt(enc.cipher) {
                return Ok(v);
            }
        }
        if tried {
            // Addressed to this node id, but not to the key it has now.
            bail!(
                "{} is encrypted to this node's id but does not open with its key (fingerprint \
                 {}) — the key changed since; a member must run `veles vault rekey` and push",
                w.name,
                self.keys.fingerprint()
            );
        }
        bail!(
            "{} is not encrypted to this node yet — a member must run `veles vault rekey` and push",
            w.name
        )
    }

    /// Every current value this node can read, and the names of the
    /// ones it cannot (not encrypted to it yet, or to a key it no longer
    /// has). The second list used to be dropped on the floor, and the
    /// resolver fell back to the env files without a word.
    pub fn values(&self) -> (Vec<Secret>, Vec<String>) {
        let mut ok = Vec::new();
        let mut unreadable = Vec::new();
        for w in current_writes(&self.file).values() {
            if w.is_tombstone() {
                continue;
            }
            match self.decrypt_write(w) {
                Ok(value) => ok.push(Secret {
                    name: w.name.clone(),
                    value,
                    policy: w.policy.clone(),
                }),
                Err(_) => unreadable.push(w.name.clone()),
            }
        }
        (ok, unreadable)
    }

    /// The names of the current values this node cannot decrypt.
    pub fn unreadable(&self) -> Vec<String> {
        self.values().1
    }

    /// Names, policies, and whether this node can read each. Never a value.
    pub fn list(&self) -> Vec<Listing> {
        current_writes(&self.file)
            .values()
            .filter(|w| !w.is_tombstone())
            .map(|w| Listing {
                name: w.name.clone(),
                policy: w.policy.clone(),
                by: w.by.clone(),
                at: w.at.clone(),
                // A real decrypt: "addressed to my id" is true of a node
                // whose keys were regenerated, too.
                readable: self.decrypt_write(w).is_ok(),
            })
            .collect()
    }

    /// Re-encrypt every current value whose newest encoding is not
    /// addressed to exactly the current members' CURRENT keys — node
    /// ids and fingerprints both: a member re-admitted with new keys
    /// keeps its id, and an id-only comparison re-encrypted nothing for
    /// it (pitfall 76). An encoding from before fingerprints were
    /// recorded is re-encrypted once. Returns the names that needed it
    /// and the ones this node could not read (another member has to
    /// rekey those).
    pub fn rekey(&mut self) -> Result<(Vec<String>, Vec<String>)> {
        let m = self.require_member()?;
        let (to, recips) = m.recipients();
        let to_fp = m.fingerprints();
        let want: BTreeSet<(&str, &str)> = to
            .iter()
            .map(String::as_str)
            .zip(to_fp.iter().map(String::as_str))
            .collect();
        let mut done = Vec::new();
        let mut unreadable = Vec::new();
        let writes: Vec<Record> = current_writes(&self.file)
            .into_values()
            .filter(|w| !w.is_tombstone())
            .collect();
        for w in writes {
            let current = ciphers_of(&self.file, &w)
                .first()
                .and_then(Encoding::addressed)
                .is_some_and(|have| have == want);
            if current {
                continue;
            }
            let value = match self.decrypt_write(&w) {
                Ok(v) => v,
                Err(_) => {
                    unreadable.push(w.name.clone());
                    continue;
                }
            };
            let mut k = Rekey {
                of: w.id(),
                cipher: encrypt_to(&value, &recips)?,
                to: to.clone(),
                to_fp: to_fp.clone(),
                by: self.keys.node.clone(),
                seq: self.next_seq(),
                at: now_stamp(),
                sig: String::new(),
            };
            k.sig = self.keys.sign(&k.signed_bytes());
            self.file.rekeys.push(k);
            done.push(w.name.clone());
        }
        Ok((done, unreadable))
    }

    /// Admit a node from its join request, after the operator compared
    /// the fingerprint on both machines. Rekeys so it can read.
    ///
    /// A node already a member with OTHER keys (its key files were lost
    /// and regenerated, or it ran `veles vault reidentify`) is admitted
    /// again with the new ones: the outcome names the key it replaced,
    /// and the rekey re-wraps every value to the new recipient.
    pub fn admit(&mut self, req: &JoinRequest, fingerprint_seen: &str) -> Result<AdmitOutcome> {
        let m = self.require_member()?;
        req.verify()?;
        let fp = fingerprint(&req.recipient, &req.sign_pk);
        if !same_fingerprint(fingerprint_seen, &fp) {
            bail!(
                "fingerprint mismatch: {} ({}) has {fp}, not {fingerprint_seen} — not admitted",
                req.name,
                req.node
            );
        }
        let mut replaced = None;
        if let Some(cur) = m.current.get(&req.node) {
            if cur.recipient == req.recipient && cur.sign_pk == req.sign_pk {
                // An already-delivered duplicate of this exact join is
                // settled. It still had to pass the fingerprint check;
                // this is not an automatic trust of a new node/key.
                return Ok(AdmitOutcome {
                    rekeyed: Vec::new(),
                    replaced: None,
                });
            }
            replaced = Some((cur.fingerprint(), fp.clone()));
        }
        let mut row = Member {
            node: req.node.clone(),
            name: req.name.clone(),
            recipient: req.recipient.clone(),
            sign_pk: req.sign_pk.clone(),
            op: MemberOp::Admit,
            vouched: Vec::new(),
            by: self.keys.node.clone(),
            seq: self.next_seq(),
            at: now_stamp(),
            sig: String::new(),
        };
        row.sig = self.keys.sign(&row.signed_bytes());
        self.file.members.push(row);
        let (rekeyed, _) = self.rekey()?;
        Ok(AdmitOutcome { rekeyed, replaced })
    }

    /// Revoke a member. Returns the names it could read — the values to
    /// rotate at their providers, because it may have kept them.
    pub fn revoke(&mut self, node: &str) -> Result<Vec<String>> {
        let m = self.require_member()?;
        if node == self.keys.node {
            bail!("a node cannot revoke itself — revoke it from another member");
        }
        let Some(target) = m.current.get(node).cloned() else {
            bail!("{node} is not a member");
        };
        let could_read: Vec<String> = current_writes(&self.file)
            .values()
            .filter(|w| !w.is_tombstone())
            .filter(|w| {
                ciphers_of(&self.file, w)
                    .iter()
                    .any(|e| e.to.iter().any(|n| n == node))
            })
            .map(|w| w.name.clone())
            .collect();
        let mut vouched: Vec<RowId> = Vec::new();
        vouched.extend(
            self.file
                .members
                .iter()
                .filter(|r| r.by == node)
                .map(Member::id),
        );
        vouched.extend(
            self.file
                .records
                .iter()
                .filter(|r| r.by == node)
                .map(Record::id),
        );
        vouched.extend(
            self.file
                .rekeys
                .iter()
                .filter(|r| r.by == node)
                .map(Rekey::id),
        );
        vouched.sort();
        let mut row = Member {
            op: MemberOp::Revoke,
            vouched,
            by: self.keys.node.clone(),
            seq: self.next_seq(),
            at: now_stamp(),
            sig: String::new(),
            ..target
        };
        row.sig = self.keys.sign(&row.signed_bytes());
        self.file.members.push(row);
        self.rekey()?;
        Ok(could_read)
    }

    /// Merge an incoming file, then rekey anything the new membership
    /// needs (when this node is a member).
    ///
    /// A node whose own vault has only ever had itself in it (a single
    /// machine that stored keys before joining a pool) ADOPTS the
    /// pool's vault on a trusted first sync instead of being refused as
    /// "another vault": see [`Vault::adopt`].
    pub fn apply(&mut self, incoming: &VaultFile, trust: Option<&str>) -> Result<MergeReport> {
        let local = self.membership();
        let theirs = membership(&incoming.members);
        if let (Some(l), Some(i), Some(t)) = (&local.root, &theirs.root, trust) {
            let other_vault = l.id() != i.id() || l.sign_pk != i.sign_pk;
            let solo = self.file.members.len() == 1 && l.node == self.keys.node;
            if other_vault && solo {
                return self.adopt(incoming, t);
            }
        }
        // A first sync into a vault that has not admitted this node would
        // leave a file of ciphertext it can never read — refused, so the
        // order is admit first, then apply.
        if local.root.is_none() && theirs.root.is_some() && !theirs.is_member(&self.keys.node) {
            bail!(
                "that vault has not admitted this node yet — send `veles vault join` to a \
                 member, have it `veles vault admit` this node, then apply its next offer"
            );
        }
        let (merged, mut report) = merge(&self.file, incoming, trust)?;
        self.file = merged;
        // A member re-admitted elsewhere with new keys arrives here as a
        // new member row with the same id: the rekey sees its
        // fingerprint change and re-wraps what this node can read.
        match self.identity_status() {
            IdentityStatus::Member => {
                let (done, _) = self.rekey()?;
                if !done.is_empty() {
                    report
                        .changed
                        .push(format!("(rekeyed {} for the current members)", done.len()));
                }
            }
            st @ IdentityStatus::Mismatch { .. } => report.notes.push(format!(
                "this node's identity: {}",
                st.summary(&self.keys.node, &self.dir)
            )),
            IdentityStatus::Fresh | IdentityStatus::MissingKeys => {}
        }
        Ok(report)
    }

    /// Replace this node's one-member vault with the pool's, keeping
    /// its values: every name the pool does not already have is stored
    /// into the pool's vault as a new write (encrypted to the pool's
    /// members); a name the pool has keeps the pool's value, and the
    /// report says so rather than overwriting a shared key with a local
    /// one. Refused until the pool has admitted this node — adopting a
    /// vault it cannot write to would drop the local values.
    pub fn adopt(&mut self, incoming: &VaultFile, trust: &str) -> Result<MergeReport> {
        let (mine, _) = self.values();
        let empty = VaultFile {
            version: 1,
            ..Default::default()
        };
        let (pool, mut report) = merge(&empty, incoming, Some(trust))?;
        if !membership(&pool.members).is_member(&self.keys.node) {
            bail!(
                "that vault has not admitted this node yet — send `veles vault join` to a \
                 member, have it `veles vault admit` this node, then apply its next offer"
            );
        }
        self.file = pool;
        let have: BTreeSet<String> = current_writes(&self.file)
            .into_iter()
            .filter(|(_, w)| !w.is_tombstone())
            .map(|(k, _)| k)
            .collect();
        for s in mine {
            if have.contains(&s.name) {
                report.notes.push(format!(
                    "{}: the pool already has one; kept the pool's, dropped this node's",
                    s.name
                ));
            } else {
                self.set(&s.name, &s.value, s.policy)?;
                report
                    .notes
                    .push(format!("{}: carried over from this node", s.name));
            }
        }
        self.rekey()?;
        report.notes.insert(
            0,
            "adopted the pool's vault in place of this node's own".into(),
        );
        Ok(report)
    }

    /// Run `op` against the freshest file on disk, under the lock, and
    /// write the result. Every write goes through here: two writers on
    /// one node (the CLI and the gateway) each take the next sequence
    /// number from what is really on disk, so neither can mint a row id
    /// the other already used, and neither drops the other's rows.
    pub fn update<T>(&mut self, op: impl FnOnce(&mut Vault) -> Result<T>) -> Result<T> {
        let _lock = lock(&self.dir)?;
        self.file = read_file(&self.dir)?;
        let out = op(self)?;
        write_file(&self.dir, &self.file)?;
        invalidate_cache();
        Ok(out)
    }

    /// A join request for this node, to hand to a member.
    pub fn join_request(&self) -> JoinRequest {
        let mut r = JoinRequest {
            node: self.keys.node.clone(),
            name: self.keys.name.clone(),
            recipient: self.keys.recipient(),
            sign_pk: self.keys.sign_pk(),
            at: now_stamp(),
            sig: String::new(),
        };
        r.sig = self.keys.sign(&r.signed_bytes());
        r
    }
}

/// A node asking to be admitted: its public keys, self-signed so the
/// request proves it holds the private half.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    pub node: String,
    pub name: String,
    pub recipient: String,
    pub sign_pk: String,
    pub at: String,
    #[serde(default)]
    pub sig: String,
}

impl JoinRequest {
    fn signed_bytes(&self) -> Vec<u8> {
        let mut c = self.clone();
        c.sig.clear();
        signed_bytes("join", &c)
    }

    pub fn verify(&self) -> Result<()> {
        if !verify(&self.sign_pk, &self.signed_bytes(), &self.sig) {
            bail!(
                "join request from {} is not signed by the key it carries",
                self.name
            );
        }
        self.recipient
            .parse::<age::x25519::Recipient>()
            .map_err(|e| anyhow::anyhow!("join request recipient: {e}"))?;
        Ok(())
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.recipient, &self.sign_pk)
    }
}

pub fn now_stamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// Store a value in this machine's vault, creating the vault on first
/// use: a single machine never has to know the word "init".
pub fn set_local(name: &str, value: &str, policy: Policy) -> Result<()> {
    let mut v = Vault::open_or_create_local()?;
    v.update(|v| {
        if !v.is_initialized() {
            v.init()?;
        }
        v.set(name, value, policy)
    })
}

/// Remove a name from this machine's vault.
pub fn unset_local(name: &str) -> Result<()> {
    let mut v = Vault::open_local()?;
    v.update(|v| v.unset(name))
}

/// [`Vault::identity_status`] for `dir`, including the state a `Vault`
/// cannot be opened in: key files gone while the vault names this node.
pub fn identity_status_in(dir: &Path, node: &str, name: &str) -> Result<IdentityStatus> {
    let file = read_file(dir)?;
    Ok(match NodeKeys::load(dir, node, name)? {
        Some(keys) => Vault {
            dir: dir.to_path_buf(),
            keys,
            file,
        }
        .identity_status(),
        None if membership(&file.members).is_member(node) => IdentityStatus::MissingKeys,
        None => IdentityStatus::Fresh,
    })
}

/// Mint this node new vault keys, setting the old key files aside
/// (`identity.txt.bak-<stamp>`, `signing.key.bak-<stamp>`, still 0600)
/// rather than deleting them: a key believed lost and then found is
/// still the cheaper way back. The new keys belong to nobody's member
/// row until a member admits their fingerprint; the caller sends the
/// join request (`veles vault reidentify`).
pub fn reidentify(dir: &Path, node: &str, name: &str) -> Result<Reidentified> {
    let _lock = lock(dir)?;
    let file = read_file(dir)?;
    let old_fp = NodeKeys::load(dir, node, name)
        .ok()
        .flatten()
        .map(|k| k.fingerprint());
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.fZ").to_string();
    let mut backed_up = Vec::new();
    for f in [IDENTITY_FILE, SIGNING_FILE] {
        let p = dir.join(f);
        if p.exists() {
            let bak = dir.join(format!("{f}.bak-{stamp}"));
            std::fs::rename(&p, &bak)
                .with_context(|| format!("set {} aside as {}", p.display(), bak.display()))?;
            backed_up.push(bak);
        }
    }
    let keys = NodeKeys::load_or_create(dir, node, name)?;
    let recorded_fp = membership(&file.members)
        .current
        .get(node)
        .map(Member::fingerprint);
    Ok(Reidentified {
        vault: Vault {
            dir: dir.to_path_buf(),
            keys,
            file,
        },
        backed_up,
        old_fp,
        recorded_fp,
    })
}

/// [`reidentify`] on this machine.
pub fn reidentify_local() -> Result<Reidentified> {
    let id = crate::node::NodeIdentity::load_or_create(&crate::config::config_dir())?;
    reidentify(&vault_dir(), &id.id, &id.name)
}

/// One node's vault at a glance: what `veles vault status`, `veles
/// doctor` and the gateway's start-up check print. Names only, never a
/// value.
#[derive(Debug, Clone)]
pub struct Health {
    pub node: String,
    pub dir: PathBuf,
    /// Is there a vault (a root) at all?
    pub initialized: bool,
    pub identity: IdentityStatus,
    /// This node's fingerprint, when it has keys.
    pub fingerprint: Option<String>,
    /// Current (not removed) values.
    pub total: usize,
    /// The ones this node cannot decrypt.
    pub unreadable: Vec<String>,
}

/// The line every surface prints for values a node cannot decrypt.
pub fn unreadable_line(names: &[String]) -> String {
    format!(
        "{} secret(s) not readable on this node (fingerprint mismatch / not rekeyed): a member \
         must run `veles vault rekey` and push — {}",
        names.len(),
        names.join(", ")
    )
}

impl Health {
    /// Anything the operator has to act on: a replaced or lost
    /// identity, or values this member cannot read.
    pub fn is_problem(&self) -> bool {
        self.identity.is_problem()
            || (self.identity == IdentityStatus::Member && !self.unreadable.is_empty())
    }

    /// The lines, without a prefix.
    pub fn lines(&self) -> Vec<String> {
        if !self.initialized {
            return vec!["none on this node".into()];
        }
        let mut out = vec![format!(
            "identity {}",
            self.identity.summary(&self.node, &self.dir)
        )];
        if let Some(fp) = &self.fingerprint {
            out[0].push_str(&format!(" (fingerprint here {fp})"));
        }
        out.push(format!(
            "{} secret(s), {} readable here",
            self.total,
            self.total - self.unreadable.len()
        ));
        if !self.unreadable.is_empty() && self.identity != IdentityStatus::Fresh {
            out.push(unreadable_line(&self.unreadable));
        }
        out
    }
}

/// [`Health`] for `dir`.
pub fn health_in(dir: &Path, node: &str, name: &str) -> Result<Health> {
    let file = read_file(dir)?;
    let initialized = membership(&file.members).root.is_some();
    let total = current_writes(&file)
        .values()
        .filter(|w| !w.is_tombstone())
        .count();
    let (identity, fingerprint, unreadable) = match NodeKeys::load(dir, node, name)? {
        Some(keys) => {
            let v = Vault {
                dir: dir.to_path_buf(),
                keys,
                file,
            };
            (
                v.identity_status(),
                Some(v.keys.fingerprint()),
                v.unreadable(),
            )
        }
        None => {
            let names: Vec<String> = current_writes(&file)
                .into_values()
                .filter(|w| !w.is_tombstone())
                .map(|w| w.name)
                .collect();
            let st = if membership(&file.members).is_member(node) {
                IdentityStatus::MissingKeys
            } else {
                IdentityStatus::Fresh
            };
            (st, None, names)
        }
    };
    Ok(Health {
        node: node.to_string(),
        dir: dir.to_path_buf(),
        initialized,
        identity,
        fingerprint,
        total,
        unreadable,
    })
}

/// [`Health`] of this machine's vault; `None` when the node has no
/// identity yet (a machine veles never ran on).
pub fn local_health() -> Result<Option<Health>> {
    let Some(id) = crate::node::NodeIdentity::load(&crate::config::config_dir())? else {
        return Ok(None);
    };
    health_in(&vault_dir(), &id.id, &id.name).map(Some)
}

/// `veles vault status`, minus the inbox line: the root, this node's
/// identity against its member row, the members (the row's fingerprint
/// AND, for this node, the one its keys really have), and what this
/// node cannot read. Never a value.
pub fn status_lines(dir: &Path, node: &str, name: &str) -> Result<Vec<String>> {
    let file = read_file(dir)?;
    let m = membership(&file.members);
    let Some(root) = &m.root else {
        return Ok(vec![
            "no vault yet — `veles secret set` or `veles vault init` makes one".into(),
        ]);
    };
    let h = health_in(dir, node, name)?;
    let mut out = vec![
        format!(
            "vault root {} ({}), fingerprint {}",
            root.name,
            root.node,
            root.fingerprint()
        ),
        format!("identity: {}", h.identity.summary(node, dir)),
    ];
    for (id, row) in &m.current {
        let me = match (id == node, &h.fingerprint) {
            (false, _) => String::new(),
            (true, Some(fp)) if !same_fingerprint(fp, &row.fingerprint()) => {
                format!("  (this node — its keys here are {fp})")
            }
            (true, None) => "  (this node — its keys here are GONE)".into(),
            (true, _) => "  (this node)".into(),
        };
        out.push(format!(
            "  member {} ({id}) {}{me}",
            row.name,
            row.fingerprint()
        ));
    }
    for id in m.revoked.keys() {
        out.push(format!("  revoked {id}"));
    }
    out.push(format!(
        "{} secret(s), {} readable here",
        h.total,
        h.total - h.unreadable.len()
    ));
    if !h.unreadable.is_empty() && h.identity != IdentityStatus::Fresh {
        out.push(format!("  {}", unreadable_line(&h.unreadable)));
    }
    Ok(out)
}

fn read_file(dir: &Path) -> Result<VaultFile> {
    let path = dir.join(VAULT_FILE);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(VaultFile {
            version: 1,
            ..Default::default()
        }),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn write_file(dir: &Path, file: &VaultFile) -> Result<()> {
    ensure_private_dir(dir)?;
    let json = serde_json::to_vec_pretty(file)?;
    write_private_atomic(&dir.join(VAULT_FILE), &json)
}

fn ensure_private_dir(dir: &Path) -> Result<()> {
    crate::platform::private_dir(dir)
        .with_context(|| format!("create {} (owner-only)", dir.display()))
}

/// Write via a temp file created 0600 and renamed over the target, so
/// a crash never leaves a truncated key or vault and the bytes are
/// never readable at the umask for an instant.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        crate::platform::private_open_options(&mut opts);
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        crate::platform::restrict_file(&tmp)
            .with_context(|| format!("restrict {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))
}

/// An exclusive advisory lock on the vault directory, held until drop.
struct Lock(#[allow(dead_code)] std::fs::File);

fn lock(dir: &Path) -> Result<Lock> {
    ensure_private_dir(dir)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    crate::platform::private_open_options(&mut opts);
    let f = opts
        .open(dir.join(LOCK_FILE))
        .with_context(|| format!("open {}", dir.join(LOCK_FILE).display()))?;
    // std's advisory lock: flock on unix, LockFileEx on Windows (there
    // was no lock at all off unix before, and two writers raced).
    // Released when the File drops.
    f.lock()
        .with_context(|| format!("lock {}", dir.display()))?;
    Ok(Lock(f))
}

// ─────────────────────────────────────────────────────────────────
// The resolver's cache
// ─────────────────────────────────────────────────────────────────

type CacheKey = (PathBuf, Option<std::time::SystemTime>, u64);

/// What the resolver holds from one read of `vault.json`.
#[derive(Clone, Default)]
struct Loaded {
    values: HashMap<String, Secret>,
    /// Current names this node cannot decrypt.
    unreadable: Vec<String>,
}

static CACHE: Mutex<Option<(CacheKey, Loaded)>> = Mutex::new(None);

fn invalidate_cache() {
    if let Ok(mut c) = CACHE.lock() {
        *c = None;
    }
}

/// Every value this node can read from the local vault, decrypted once
/// per change of `vault.json` (path, mtime, length — the same key the
/// live config uses). A node with no vault, or no keys, has none; that
/// is not an error, the resolver just falls through to the env files.
pub fn local_values() -> HashMap<String, Secret> {
    local_loaded().values
}

/// The names in the local vault this node cannot decrypt: encrypted to
/// a key it no longer has, or not rekeyed for it yet. Every name, when
/// its key files are gone while the vault still names it.
pub fn local_unreadable() -> Vec<String> {
    local_loaded().unreadable
}

fn local_loaded() -> Loaded {
    // A test that installed no scratch home must never decrypt the
    // operator's real vault.
    #[cfg(test)]
    if crate::config::dirs().is_none() && std::env::var_os("VELES_HOME").is_none() {
        return Loaded::default();
    }
    let dir = vault_dir();
    let path = dir.join(VAULT_FILE);
    let Ok(meta) = std::fs::metadata(&path) else {
        return Loaded::default();
    };
    let key: CacheKey = (path, meta.modified().ok(), meta.len());
    if let Ok(c) = CACHE.lock() {
        if let Some((k, v)) = c.as_ref() {
            if *k == key {
                return v.clone();
            }
        }
    }
    let loaded = match load_values(&dir) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("vault: {e:#}");
            Loaded::default()
        }
    };
    if let Ok(mut c) = CACHE.lock() {
        *c = Some((key, loaded.clone()));
    }
    loaded
}

fn load_values(dir: &Path) -> Result<Loaded> {
    let Some(id) = crate::node::NodeIdentity::load(&crate::config::config_dir())? else {
        return Ok(Loaded::default());
    };
    let file = read_file(dir)?;
    let Some(keys) = NodeKeys::load(dir, &id.id, &id.name)? else {
        // Keys gone while the vault names this node: nothing opens, and
        // every name is one the resolver must not pretend is unset.
        let unreadable = if membership(&file.members).is_member(&id.id) {
            current_writes(&file)
                .into_values()
                .filter(|w| !w.is_tombstone())
                .map(|w| w.name)
                .collect()
        } else {
            Vec::new()
        };
        return Ok(Loaded {
            values: HashMap::new(),
            unreadable,
        });
    };
    let v = Vault {
        dir: dir.to_path_buf(),
        keys,
        file,
    };
    let (ok, unreadable) = v.values();
    Ok(Loaded {
        values: ok.into_iter().map(|s| (s.name.clone(), s)).collect(),
        unreadable,
    })
}

/// The resolver's warning for a name the vault has but this node
/// cannot decrypt: once per name per process, not once per lookup (the
/// resolver runs on every provider request). Without it the lookup fell
/// through to secrets.env / .env — or to nothing — in silence.
pub fn warn_if_unreadable(name: &str) {
    static WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    if !local_unreadable().iter().any(|n| n == name) {
        return;
    }
    if first_warning(&WARNED, name) {
        tracing::warn!(
            "vault: ${name} is in the vault but not readable on this node (fingerprint \
             mismatch / not rekeyed): a member must run `veles vault rekey` and push — falling \
             back to the env files (`veles vault status` says more)"
        );
    }
}

/// Has `name` not been warned about yet in `seen`? Records it.
fn first_warning(seen: &Mutex<BTreeSet<String>>, name: &str) -> bool {
    seen.lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(name.to_string())
}

// ─────────────────────────────────────────────────────────────────
// Transport: the configsync shape (marker, fence, inbox, apply)
// ─────────────────────────────────────────────────────────────────

pub const OFFER_MARKER: &str = "[veles vault sync offer]";
pub const JOIN_MARKER: &str = "[veles vault join request]";
const FENCE: &str = "```veles-vault-b64";

/// What travels: the whole file (ciphertext only — the file is small
/// and import is idempotent, so there is no cursor), or a join request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Offer {
        from_node: String,
        from_name: String,
        file: VaultFile,
    },
    Join(JoinRequest),
}

pub fn is_vault_message(text: &str) -> bool {
    let t = text.trim_start();
    (t.starts_with(OFFER_MARKER) || t.starts_with(JOIN_MARKER)) && text.contains(FENCE)
}

pub fn encode_message(msg: &Message) -> Result<String> {
    use base64::Engine as _;
    let json = serde_json::to_string(msg)?;
    let sha = crate::memory::MemoryStore::content_sha(&json);
    let enc = b64().encode(json.as_bytes());
    let (marker, line) = match msg {
        Message::Offer { file, .. } => (
            OFFER_MARKER,
            format!(
                "{} member row(s), {} write(s), {} rekey(s); values are age ciphertext. \
                 Stored in the receiver's vault inbox, and merged on arrival into a vault it \
                 already has when its configsync_auto is on; else `veles vault apply --inbox`.",
                file.members.len(),
                file.records.len(),
                file.rekeys.len()
            ),
        ),
        Message::Join(r) => (
            JOIN_MARKER,
            format!(
                "{} ({}) asks to join the vault, fingerprint {}. Admit it only after comparing \
                 that fingerprint with `veles vault whoami` on {}.",
                r.name,
                r.node,
                r.fingerprint(),
                r.name
            ),
        ),
    };
    Ok(format!(
        "{marker} {line}\n{FENCE}\nsha256:{sha}\n{enc}\n```"
    ))
}

pub fn decode_message(raw: &str) -> Result<Message> {
    use base64::Engine as _;
    let Some(start) = raw.find(FENCE) else {
        // A bare JSON file (`veles vault offer --out`).
        return serde_json::from_str(raw.trim()).context("vault message: not a vault message");
    };
    let body = &raw[start + FENCE.len()..];
    let body = &body[..body.find("```").unwrap_or(body.len())];
    let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());
    let sha = lines
        .next()
        .and_then(|l| l.strip_prefix("sha256:"))
        .context("vault message: missing sha256 line")?
        .to_string();
    let enc: String = lines.collect();
    let bytes = b64()
        .decode(enc.as_bytes())
        .context("vault message: payload is not base64")?;
    let json = String::from_utf8(bytes).context("vault message: payload is not UTF-8")?;
    if crate::memory::MemoryStore::content_sha(&json) != sha {
        bail!("vault message: checksum mismatch — the payload changed in transit");
    }
    serde_json::from_str(&json).context("vault message: payload is not a vault message")
}

/// Where received vault messages wait: `<veles_home>/vault/inbox/`.
pub fn inbox_dir() -> PathBuf {
    crate::config::veles_home().join("vault").join("inbox")
}

const INBOX_MAX: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEntry {
    pub received_at: String,
    pub peer: String,
    pub task_id: String,
    pub message: Message,
}

pub fn inbox_store(dir: &Path, peer: &str, task_id: &str, message: Message) -> Result<PathBuf> {
    ensure_private_dir(dir)?;
    if inbox_pending(dir)?.len() >= INBOX_MAX {
        bail!("vault inbox full — apply or clear it with `veles vault apply --inbox` first");
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let safe: String = task_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(40)
        .collect();
    let path = dir.join(format!("{nanos:020}-{safe}.json"));
    let entry = InboxEntry {
        received_at: chrono::Utc::now().to_rfc3339(),
        peer: peer.to_string(),
        task_id: task_id.to_string(),
        message,
    };
    let offer = matches!(entry.message, Message::Offer { .. });
    write_private_atomic(&path, &serde_json::to_vec_pretty(&entry)?)?;
    // An offer is the sender's whole file, so its newest supersedes the
    // ones before it. Once offers came on every change, a node with no
    // vault to merge them into would have kept one per change.
    if offer {
        for p in inbox_pending(dir)? {
            if p == path {
                continue;
            }
            if let Ok(e) = inbox_read(&p) {
                if e.peer == peer && matches!(e.message, Message::Offer { .. }) {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }
    Ok(path)
}

pub fn inbox_pending(dir: &Path) -> Result<Vec<PathBuf>> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<_> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .collect();
    out.sort();
    Ok(out)
}

pub fn inbox_read(path: &Path) -> Result<InboxEntry> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if let Ok(e) = serde_json::from_str::<InboxEntry>(&raw) {
        return Ok(e);
    }
    Ok(InboxEntry {
        received_at: String::new(),
        peer: String::new(),
        task_id: String::new(),
        message: decode_message(&raw)?,
    })
}

/// Send a vault message (an offer, or this node's join request) to a
/// peer's vault inbox over the A2A link. The peer stores it only if it
/// names this node in `[cluster] memsync_peers`. An offer merges on
/// arrival into a vault the peer already has, when its
/// `configsync_auto` is on ([`apply_inbox_auto`]); a first sync is its
/// operator's `veles vault apply --inbox --trust`, and a join request
/// always waits for `veles vault admit`.
pub async fn push_to_peer(msg: &Message, peer_url: &str, bearer: Option<&str>) -> Result<String> {
    let text = encode_message(msg)?;
    let client = crate::a2a::A2AClient::shared();
    let ctx = crate::a2a::new_context_id();
    let result = client
        .call(peer_url, bearer, &text, &ctx)
        .await
        .with_context(|| format!("push to {peer_url}"))?;
    let state = result["status"]["state"].as_str().unwrap_or("?");
    let reply = crate::a2a::summarize_result(&result);
    if state != crate::a2a::STATE_COMPLETED {
        bail!("{peer_url} refused it:\n{reply}");
    }
    Ok(reply)
}

// ─────────────────────────────────────────────────────────────────
// Automatic sync: `[cluster] configsync_auto` carries the vault too
// ─────────────────────────────────────────────────────────────────
//
// USER 2026-10-04: "let's wrap this all up, including automatic vault
// push". The gateway's sync loop sends the whole file to a peer whose
// copy lacks rows (by `rows_digest`), and the receiver merges it on
// arrival into the vault it already has. Two things stay the
// operator's: adopting a vault on the first sync (the root fingerprint
// is read off the other machine, `--trust`) and admitting a node (its
// fingerprint is compared with `whoami` there). Neither can be decided
// by a machine on the operator's behalf.

/// The rows a file holds, as one digest: the same rows in any order
/// digest the same. A peer that stored an offer with this digest holds
/// these rows, so the loop sends again only when the local file has
/// rows beyond them, and a merge that added nothing goes nowhere.
pub fn rows_digest(file: &VaultFile) -> String {
    let json = |v: serde_json::Result<String>| v.unwrap_or_default();
    let mut rows: Vec<String> = Vec::new();
    rows.extend(
        file.members
            .iter()
            .map(|r| format!("m{}", json(serde_json::to_string(r)))),
    );
    rows.extend(
        file.records
            .iter()
            .map(|r| format!("r{}", json(serde_json::to_string(r)))),
    );
    rows.extend(
        file.rekeys
            .iter()
            .map(|r| format!("k{}", json(serde_json::to_string(r)))),
    );
    rows.sort();
    crate::memory::MemoryStore::content_sha(&rows.join("\n"))
}

/// The store meta key holding the digest of what `peer` is known to
/// have: set when it stored an offer from here, or sent one here.
pub fn peer_has_key(peer: &str) -> String {
    format!("vaultsync:has:{peer}")
}

/// One inbox offer's fate under [`apply_inbox_auto`].
#[derive(Debug)]
pub enum AutoOutcome {
    /// Merged into this node's vault; the file moved to `applied/`.
    Merged(MergeReport),
    /// This node has no vault: the offer waits for `veles vault apply
    /// --inbox --trust <root fingerprint>`.
    NoVault,
    /// The merge refused it (another vault, a node not yet admitted):
    /// it stays in the inbox, and says why.
    Refused(String),
}

#[derive(Debug)]
pub struct AutoApplied {
    pub file: String,
    /// The A2A peer it came through.
    pub peer: String,
    pub outcome: AutoOutcome,
}

/// `configsync_auto`, the vault's receiving half: merge every offer in
/// the local inbox into this node's vault. Only into a vault it already
/// has (see the section note); a row still counts only when a member
/// signed it (`merge`), which is what makes merging on arrival safe.
/// Join requests are left alone.
pub fn apply_inbox_auto() -> Result<Vec<AutoApplied>> {
    let mut v = Vault::open_local_if_any()?;
    apply_inbox_into(v.as_mut(), &inbox_dir())
}

/// A join is finished only when the current member row carries the
/// request's exact age recipient AND Ed25519 public key. Same node id
/// with different keys is a re-identification, still owner-reviewed.
pub fn join_is_current(vault: &Vault, req: &JoinRequest) -> bool {
    vault
        .membership()
        .current
        .get(&req.node)
        .is_some_and(|m| m.recipient == req.recipient && m.sign_pk == req.sign_pk)
}

/// [`apply_inbox_auto`] against a given vault and inbox (`None`: this
/// node has none). One at a time per process: the receiver and the
/// loop's drain may run together.
pub fn apply_inbox_into(vault: Option<&mut Vault>, dir: &Path) -> Result<Vec<AutoApplied>> {
    static ONE: Mutex<()> = Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
    let mut offers = Vec::new();
    for p in inbox_pending(dir)? {
        if let Ok(e) = inbox_read(&p) {
            match e.message {
                Message::Offer { file, .. } => {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    offers.push((p, name, e.peer, file));
                }
                Message::Join(req) => {
                    if let Some(v) = vault.as_deref().filter(|v| v.is_initialized()) {
                        if join_is_current(v, &req) {
                            let done = dir.join("applied");
                            std::fs::create_dir_all(&done)?;
                            let name = p.file_name().unwrap_or_default();
                            std::fs::rename(&p, done.join(name)).with_context(|| {
                                format!("retire already-admitted join {}", p.display())
                            })?;
                            tracing::info!(
                                "vault: retired a join from {}: its exact keys are already admitted",
                                req.name
                            );
                        }
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    let Some(v) = vault.filter(|v| v.is_initialized()) else {
        for (_, file, peer, _) in offers {
            out.push(AutoApplied {
                file,
                peer,
                outcome: AutoOutcome::NoVault,
            });
        }
        return Ok(out);
    };
    for (path, file, peer, incoming) in offers {
        let outcome = match v.update(|v| v.apply(&incoming, None)) {
            Ok(report) => {
                let done = dir.join("applied");
                std::fs::create_dir_all(&done)?;
                // Gone already: a CLI `apply --inbox` took it meanwhile.
                let _ = std::fs::rename(&path, done.join(&file));
                AutoOutcome::Merged(report)
            }
            Err(e) => AutoOutcome::Refused(format!("{e:#}")),
        };
        out.push(AutoApplied {
            file,
            peer,
            outcome,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(dir: &Path, id: &str) -> Vault {
        Vault::open_or_create(&dir.join(id), id, id).unwrap()
    }

    fn user() -> Policy {
        Policy::default_for(Kind::User, vec![])
    }

    /// Two nodes, A the root and B admitted, with B holding A's file.
    fn pair() -> (tempfile::TempDir, Vault, Vault) {
        let t = tempfile::tempdir().unwrap();
        let mut a = node(t.path(), "a");
        let mut b = node(t.path(), "b");
        a.init().unwrap();
        a.admit(&b.join_request(), &b.keys.fingerprint()).unwrap();
        let root_fp = a.keys.fingerprint();
        b.apply(&a.file, Some(&root_fp)).unwrap();
        (t, a, b)
    }

    #[test]
    fn keys_persist_and_are_private() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("v");
        let k1 = NodeKeys::load_or_create(&dir, "n1", "n1").unwrap();
        let k2 = NodeKeys::load_or_create(&dir, "n1", "n1").unwrap();
        assert_eq!(
            k1.recipient(),
            k2.recipient(),
            "the identity is minted once"
        );
        assert_eq!(k1.sign_pk(), k2.sign_pk());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in [IDENTITY_FILE, SIGNING_FILE] {
                let mode = std::fs::metadata(dir.join(f)).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{f}");
            }
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "the vault dir");
        }
        // The identity file is what `age -d -i` reads.
        let raw = std::fs::read_to_string(dir.join(IDENTITY_FILE)).unwrap();
        assert!(
            raw.contains(&format!("# public key: {}", k1.recipient())),
            "{raw}"
        );
    }

    #[test]
    fn a_value_round_trips_and_is_ciphertext_on_disk() {
        let t = tempfile::tempdir().unwrap();
        let mut a = node(t.path(), "a");
        a.update(|v| v.init()).unwrap();
        a.update(|v| v.set("API_KEY", "sk-live-value-123456", user()))
            .unwrap();
        let raw = std::fs::read_to_string(a.dir.join(VAULT_FILE)).unwrap();
        assert!(!raw.contains("sk-live-value-123456"), "plaintext on disk");
        assert!(raw.contains("BEGIN AGE ENCRYPTED FILE"));
        let back = Vault::open(&a.dir, "a", "a").unwrap();
        assert_eq!(
            back.get("API_KEY").unwrap().unwrap().value,
            "sk-live-value-123456"
        );
    }

    #[test]
    fn a_member_reads_what_another_member_wrote() {
        let (_t, mut a, mut b) = pair();
        a.set("K", "value-from-a", user()).unwrap();
        b.apply(&a.file, None).unwrap();
        assert_eq!(b.get("K").unwrap().unwrap().value, "value-from-a");
        b.set("K", "value-from-b", user()).unwrap();
        a.apply(&b.file, None).unwrap();
        assert_eq!(a.get("K").unwrap().unwrap().value, "value-from-b");
    }

    #[test]
    fn a_non_member_cannot_decrypt() {
        let (t, mut a, _b) = pair();
        a.set("K", "for-members-only", user()).unwrap();
        // C holds the file (it could, from any inbox) but was never admitted.
        let mut c = node(t.path(), "c");
        c.file = a.file.clone();
        assert!(c.get("K").is_err(), "a non-member decrypted a value");
        assert!(c.values().0.is_empty());
    }

    /// Recipients are public, so anyone could encrypt a forged value to
    /// them. Only a member's signature makes a write count.
    #[test]
    fn a_write_not_signed_by_a_member_is_refused() {
        let (t, a, mut b) = pair();
        let mallory = node(t.path(), "m");
        let (to, recips) = a.membership().recipients();
        let mut forged = Record {
            name: "K".into(),
            policy: user(),
            cipher: encrypt_to("attacker-value", &recips).unwrap(),
            to,
            to_fp: Vec::new(),
            by: "m".into(),
            seq: 1,
            at: now_stamp(),
            sig: String::new(),
        };
        forged.sig = mallory.keys.sign(&forged.signed_bytes());
        let mut incoming = a.file.clone();
        incoming.records.push(forged.clone());
        let report = b.apply(&incoming, None).unwrap();
        assert!(b.get("K").unwrap().is_none(), "the forged write counted");
        assert!(!report.refused.is_empty());
        // Claiming to be a member does not help without its key.
        forged.by = "a".into();
        let mut incoming = a.file.clone();
        incoming.records.push(forged);
        b.apply(&incoming, None).unwrap();
        assert!(
            b.get("K").unwrap().is_none(),
            "a write signed with the wrong key counted"
        );
    }

    /// A tampered policy (shell turned on for a platform credential) is
    /// a changed row: its signature no longer verifies.
    #[test]
    fn a_tampered_policy_is_refused() {
        let (_t, mut a, mut b) = pair();
        a.set(
            "BOT",
            "bot-token-value",
            Policy::default_for(Kind::Platform, vec![]),
        )
        .unwrap();
        let mut incoming = a.file.clone();
        incoming.records.last_mut().unwrap().policy.shell = true;
        b.apply(&incoming, None).unwrap();
        assert!(b.get("BOT").unwrap().is_none(), "a tampered row counted");
    }

    #[test]
    fn the_first_sync_needs_the_roots_fingerprint() {
        let t = tempfile::tempdir().unwrap();
        let mut a = node(t.path(), "a");
        a.init().unwrap();
        let mut b = node(t.path(), "b");
        a.admit(&b.join_request(), &b.keys.fingerprint()).unwrap();
        let err = b.apply(&a.file, None).unwrap_err().to_string();
        assert!(err.contains("--trust"), "{err}");
        let err = b
            .apply(&a.file, Some("0000-0000-0000-0000"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("refused"), "{err}");
        b.apply(&a.file, Some(&a.keys.fingerprint().to_uppercase()))
            .unwrap();
        assert!(b.membership().is_member("b"));
    }

    /// A node that applies a vault before being admitted gets nothing it
    /// could never read — it is told to be admitted first, and its own
    /// (empty) state is unchanged.
    #[test]
    fn a_first_sync_before_admission_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let mut a = node(t.path(), "a");
        a.init().unwrap();
        a.set("K", "v-for-members", user()).unwrap();
        let mut b = node(t.path(), "b");
        let err = b
            .apply(&a.file, Some(&a.keys.fingerprint()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("has not admitted this node"), "{err}");
        assert!(b.file.members.is_empty() && b.file.records.is_empty());
    }

    #[test]
    fn another_vaults_offer_is_refused() {
        let (_t, a, mut b) = pair();
        let t2 = tempfile::tempdir().unwrap();
        let mut other = node(t2.path(), "a");
        other.init().unwrap();
        assert!(b
            .apply(&other.file, Some(&other.keys.fingerprint()))
            .is_err());
        let _ = a;
    }

    #[test]
    fn admit_needs_the_fingerprint_the_operator_saw() {
        let t = tempfile::tempdir().unwrap();
        let mut a = node(t.path(), "a");
        a.init().unwrap();
        let b = node(t.path(), "b");
        let err = a
            .admit(&b.join_request(), "dead-beef-dead-beef")
            .unwrap_err()
            .to_string();
        assert!(err.contains("fingerprint mismatch"), "{err}");
        assert!(!a.membership().is_member("b"));
        // A request whose signature does not match its key is refused
        // whatever fingerprint is typed.
        let mut forged = b.join_request();
        forged.recipient = a.keys.recipient();
        let fp = forged.fingerprint();
        assert!(a.admit(&forged, &fp).is_err());
    }

    /// A duplicate join carrying the exact keys already admitted is a
    /// no-op after the normal fingerprint check. Different keys for the
    /// same node are still a reviewed re-identification, never this path.
    #[test]
    fn readmitting_the_same_keys_is_a_fingerprint_checked_noop() {
        let (_t, mut a, b) = pair();
        let before = a.file.clone();
        let out = a.admit(&b.join_request(), &b.keys.fingerprint()).unwrap();
        assert!(out.rekeyed.is_empty() && out.replaced.is_none());
        assert_eq!(a.file, before, "no new member or rekey rows");
        assert!(join_is_current(&a, &b.join_request()));
        assert!(a.admit(&b.join_request(), "dead-beef-dead-beef").is_err());
    }

    /// The receiving drain retires the already-decided request so the
    /// owner is not asked about it on every retry interval.
    #[test]
    fn the_inbox_drain_retires_only_an_exact_already_admitted_join() {
        let (_t, mut a, b) = pair();
        let inbox = tempfile::tempdir().unwrap();
        let path = inbox_store(
            inbox.path(),
            "b",
            "already-joined",
            Message::Join(b.join_request()),
        )
        .unwrap();
        apply_inbox_into(Some(&mut a), inbox.path()).unwrap();
        assert!(!path.exists());
        assert!(inbox
            .path()
            .join("applied")
            .join(path.file_name().unwrap())
            .exists());

        // A request with a replaced identity is NOT silently retired.
        let b2 = with_new_keys(&b);
        let path2 = inbox_store(
            inbox.path(),
            "b",
            "new-keys",
            Message::Join(b2.join_request()),
        )
        .unwrap();
        assert!(!join_is_current(&a, &b2.join_request()));
        apply_inbox_into(Some(&mut a), inbox.path()).unwrap();
        assert!(path2.exists(), "the new identity still needs its owner");
    }

    // ── a lost or replaced identity (pitfall 76) ────────────────────

    /// B lost its key files and minted new ones under the same node id
    /// (the laptop, 2026-10-19). Its rows still name it, so every
    /// encoding is "to b" by id — and none opens with the new key.
    fn with_new_keys(v: &Vault) -> Vault {
        std::fs::remove_file(v.dir.join(IDENTITY_FILE)).unwrap();
        std::fs::remove_file(v.dir.join(SIGNING_FILE)).unwrap();
        let keys = NodeKeys::load_or_create(&v.dir, &v.keys.node, &v.keys.name).unwrap();
        assert_ne!(keys.fingerprint(), v.keys.fingerprint());
        Vault {
            dir: v.dir.clone(),
            keys,
            file: v.file.clone(),
        }
    }

    /// THE re-admit case: the admitting member must re-wrap every value
    /// to the node's NEW recipient. The rekey used to compare node-id
    /// sets only, saw {a, b} == {a, b}, and re-encrypted 0 values.
    #[test]
    fn readmitting_a_node_with_new_keys_rewraps_every_value_for_them() {
        let (_t, mut a, mut b) = pair();
        a.set("K1", "value-one", user()).unwrap();
        a.set("K2", "value-two", user()).unwrap();
        b.apply(&a.file, None).unwrap();
        assert_eq!(b.get("K1").unwrap().unwrap().value, "value-one");
        let mut b2 = with_new_keys(&b);
        assert!(b2.get("K1").is_err(), "the new key opened an old value");
        let out = a.admit(&b2.join_request(), &b2.keys.fingerprint()).unwrap();
        let mut rekeyed = out.rekeyed.clone();
        rekeyed.sort();
        assert_eq!(rekeyed, vec!["K1".to_string(), "K2".to_string()]);
        assert_eq!(
            out.replaced,
            Some((b.keys.fingerprint(), b2.keys.fingerprint())),
            "the admit names the key it replaced"
        );
        b2.apply(&a.file, None).unwrap();
        assert_eq!(b2.identity_status(), IdentityStatus::Member);
        assert_eq!(b2.get("K1").unwrap().unwrap().value, "value-one");
        assert_eq!(b2.get("K2").unwrap().unwrap().value, "value-two");
        assert!(b2.unreadable().is_empty());
        // And it can write again: its rows verify on the other member.
        b2.set("K3", "value-three", user()).unwrap();
        a.apply(&b2.file, None).unwrap();
        assert_eq!(a.get("K3").unwrap().unwrap().value, "value-three");
    }

    /// A third member that receives the new member row without the
    /// admitting member's rekeys (they crossed, or that member could not
    /// read a value) notices the fingerprint change on apply and re-wraps
    /// what it can read.
    #[test]
    fn a_peer_that_learns_of_a_new_key_rekeys_on_apply() {
        let (t, mut a, mut b) = pair();
        let mut c = node(t.path(), "c");
        a.admit(&c.join_request(), &c.keys.fingerprint()).unwrap();
        c.apply(&a.file, Some(&a.keys.fingerprint())).unwrap();
        a.set("K", "for-everyone", user()).unwrap();
        b.apply(&a.file, None).unwrap();
        c.apply(&a.file, None).unwrap();
        let b2 = with_new_keys(&b);
        let before: BTreeSet<RowId> = a.file.rekeys.iter().map(Rekey::id).collect();
        a.admit(&b2.join_request(), &b2.keys.fingerprint()).unwrap();
        let mut incoming = a.file.clone();
        incoming.rekeys.retain(|k| before.contains(&k.id()));
        let report = c.apply(&incoming, None).unwrap();
        assert!(
            report.changed.iter().any(|s| s.contains("rekeyed 1")),
            "{}",
            report.summarize()
        );
        let mut b2 = b2;
        b2.apply(&c.file, None).unwrap();
        assert_eq!(b2.get("K").unwrap().unwrap().value, "for-everyone");
    }

    #[test]
    fn identity_status_tells_member_mismatch_and_missing_keys_apart() {
        let (t, mut a, b) = pair();
        a.set("K", "v", user()).unwrap();
        assert_eq!(a.identity_status(), IdentityStatus::Member);
        assert_eq!(b.identity_status(), IdentityStatus::Member);
        let stranger = node(t.path(), "s");
        assert_eq!(stranger.identity_status(), IdentityStatus::Fresh);
        // B's key files replaced: the row still admits the old keys.
        persist(&b);
        let b2 = with_new_keys(&b);
        assert_eq!(
            b2.identity_status(),
            IdentityStatus::Mismatch {
                recorded_fp: b.keys.fingerprint(),
                local_fp: b2.keys.fingerprint(),
            }
        );
        assert_eq!(
            identity_status_in(&b.dir, "b", "b").unwrap(),
            b2.identity_status()
        );
        // B's key files gone: a `Vault` cannot even be opened, but the
        // status still says what happened.
        std::fs::remove_file(b.dir.join(IDENTITY_FILE)).unwrap();
        std::fs::remove_file(b.dir.join(SIGNING_FILE)).unwrap();
        assert_eq!(
            identity_status_in(&b.dir, "b", "b").unwrap(),
            IdentityStatus::MissingKeys
        );
        assert!(IdentityStatus::MissingKeys.is_problem());
        assert!(!IdentityStatus::Member.is_problem());
    }

    /// No silent re-keying: a node whose key files are gone while the
    /// vault names it is refused — by `open` and by the minting
    /// `open_or_create` alike — and nothing is written.
    #[test]
    fn open_refuses_when_the_keys_are_gone_but_the_vault_names_this_node() {
        let (_t, a, b) = pair();
        persist(&b);
        std::fs::remove_file(b.dir.join(IDENTITY_FILE)).unwrap();
        std::fs::remove_file(b.dir.join(SIGNING_FILE)).unwrap();
        for e in [
            Vault::open(&b.dir, "b", "b").err().unwrap(),
            Vault::open_or_create(&b.dir, "b", "b").err().unwrap(),
        ] {
            let e = format!("{e:#}");
            assert!(e.contains("identity is lost"), "{e}");
            assert!(e.contains("veles vault reidentify"), "{e}");
            assert!(e.contains(&b.keys.fingerprint()), "{e}");
        }
        assert!(!b.dir.join(IDENTITY_FILE).exists(), "keys were minted");
        assert!(!b.dir.join(SIGNING_FILE).exists(), "keys were minted");
        // A node the vault does not name still gets keys on first use,
        // from the minting open only.
        let t2 = tempfile::tempdir().unwrap();
        let e = format!("{:#}", Vault::open(t2.path(), "n", "n").err().unwrap());
        assert!(e.contains("no vault keys yet"), "{e}");
        assert!(Vault::open_or_create(t2.path(), "n", "n").is_ok());
        let _ = a;
    }

    /// The names a node cannot decrypt are reported, not dropped: the
    /// resolver used to fall back to the env files in silence.
    #[test]
    fn values_reports_the_names_this_node_cannot_read() {
        let (_t, mut a, mut b) = pair();
        a.set("K1", "one", user()).unwrap();
        a.set("K2", "two", user()).unwrap();
        b.apply(&a.file, None).unwrap();
        let (ok, unreadable) = b.values();
        assert_eq!(ok.len(), 2);
        assert!(unreadable.is_empty());
        let b2 = with_new_keys(&b);
        let (ok, mut unreadable) = b2.values();
        unreadable.sort();
        assert!(ok.is_empty());
        assert_eq!(unreadable, vec!["K1".to_string(), "K2".to_string()]);
        // `list` decrypts rather than trusting "addressed to my id".
        assert!(b2.list().iter().all(|l| !l.readable));
        let e = format!("{:#}", b2.get("K1").unwrap_err());
        assert!(e.contains("does not open with its key"), "{e}");
        // A mismatched node writes nothing its peers would refuse.
        let mut b2 = b2;
        let e = format!("{:#}", b2.set("K9", "x", user()).unwrap_err());
        assert!(e.contains("MISMATCH"), "{e}");
    }

    /// Rows written before `to_fp` existed: the JSON has no such key,
    /// their signatures still verify (the field is left out of the bytes
    /// when empty), and the first rekey re-wraps them once.
    #[test]
    fn an_old_shape_vault_file_loads_verifies_and_is_rekeyed_once() {
        let (_t, mut a, mut b) = pair();
        a.set("K", "old-shape", user()).unwrap();
        // Make A's write what the code before 2026-10-20 wrote.
        let r = a.file.records.last_mut().unwrap();
        assert!(!r.to_fp.is_empty(), "new rows carry fingerprints");
        r.to_fp.clear();
        r.sig = a.keys.sign(&r.signed_bytes());
        let json = serde_json::to_string_pretty(&a.file).unwrap();
        assert!(!json.contains("to_fp"), "{json}");
        let old: VaultFile = serde_json::from_str(&json).unwrap();
        let (kept, refused) = verified(&old);
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(kept, old);
        b.apply(&old, None).unwrap();
        assert_eq!(b.get("K").unwrap().unwrap().value, "old-shape");
        // B's apply already rekeyed the unknown encoding once; nothing
        // is left to do on either side after that.
        assert!(b.file.rekeys.iter().any(|k| !k.to_fp.is_empty()));
        assert!(b.rekey().unwrap().0.is_empty());
        a.apply(&b.file, None).unwrap();
        assert!(a.rekey().unwrap().0.is_empty());
    }

    /// `veles vault reidentify`: new keys, the old ones set aside, and
    /// the node a stranger to its row until a member admits the new
    /// fingerprint — after which it reads everything.
    #[test]
    fn reidentify_sets_the_old_keys_aside_and_a_readmit_restores_access() {
        let (_t, mut a, mut b) = pair();
        a.set("K", "after-reidentify", user()).unwrap();
        b.apply(&a.file, None).unwrap();
        persist(&b);
        let old_fp = b.keys.fingerprint();
        let r = reidentify(&b.dir, "b", "b").unwrap();
        assert_eq!(r.old_fp.as_deref(), Some(old_fp.as_str()));
        assert_eq!(r.recorded_fp.as_deref(), Some(old_fp.as_str()));
        assert_eq!(r.backed_up.len(), 2);
        assert!(r.backed_up.iter().all(|p| p.exists()));
        let mut b2 = r.vault;
        let new_fp = b2.keys.fingerprint();
        assert_ne!(new_fp, old_fp);
        assert!(matches!(
            b2.identity_status(),
            IdentityStatus::Mismatch { .. }
        ));
        let out = a.admit(&b2.join_request(), &new_fp).unwrap();
        assert_eq!(out.replaced, Some((old_fp, new_fp)));
        b2.apply(&a.file, None).unwrap();
        assert_eq!(b2.get("K").unwrap().unwrap().value, "after-reidentify");
    }

    /// What `vault status`, `doctor` and the gateway print.
    #[test]
    fn health_lines_name_the_identity_and_the_unreadable_values() {
        let (_t, mut a, mut b) = pair();
        a.set("K1", "one", user()).unwrap();
        b.apply(&a.file, None).unwrap();
        persist(&b);
        let h = health_in(&b.dir, "b", "b").unwrap();
        assert!(!h.is_problem(), "{:?}", h.lines());
        assert!(h.lines()[0].starts_with("identity OK"), "{:?}", h.lines());
        assert_eq!(h.lines()[1], "1 secret(s), 1 readable here");
        let _b2 = with_new_keys(&b);
        let h = health_in(&b.dir, "b", "b").unwrap();
        assert!(h.is_problem());
        let lines = h.lines();
        assert!(lines[0].starts_with("identity MISMATCH"), "{lines:?}");
        assert!(lines[0].contains(&b.keys.fingerprint()), "{lines:?}");
        assert_eq!(lines[1], "1 secret(s), 0 readable here");
        assert_eq!(
            lines[2],
            "1 secret(s) not readable on this node (fingerprint mismatch / not rekeyed): a \
             member must run `veles vault rekey` and push — K1"
        );
        std::fs::remove_file(b.dir.join(IDENTITY_FILE)).unwrap();
        std::fs::remove_file(b.dir.join(SIGNING_FILE)).unwrap();
        let h = health_in(&b.dir, "b", "b").unwrap();
        assert!(h.is_problem());
        assert!(h.lines()[0].starts_with("identity LOST"), "{:?}", h.lines());
        assert!(h.lines()[0].contains("veles vault reidentify"));
        let t2 = tempfile::tempdir().unwrap();
        let h = health_in(t2.path(), "n", "n").unwrap();
        assert_eq!(h.lines(), vec!["none on this node".to_string()]);
        assert!(!h.is_problem());
    }

    /// `veles vault status`: it used to decide membership by node id,
    /// print the ROW's fingerprint as this node's, and call a value
    /// readable when it was addressed to this id — all three said
    /// "fine" for a node whose keys had been regenerated.
    #[test]
    fn status_lines_show_the_rows_fingerprint_and_the_one_the_keys_have() {
        let (_t, mut a, mut b) = pair();
        a.set("K1", "one", user()).unwrap();
        b.apply(&a.file, None).unwrap();
        persist(&b);
        let fa = a.keys.fingerprint();
        let fb = b.keys.fingerprint();
        let lines = status_lines(&b.dir, "b", "b").unwrap();
        assert_eq!(
            lines,
            vec![
                format!("vault root a (a), fingerprint {fa}"),
                "identity: OK — this node's keys are the ones the vault admitted".to_string(),
                format!("  member a (a) {fa}"),
                format!("  member b (b) {fb}  (this node)"),
                "1 secret(s), 1 readable here".to_string(),
            ]
        );
        let b2 = with_new_keys(&b);
        let fb2 = b2.keys.fingerprint();
        let lines = status_lines(&b.dir, "b", "b").unwrap();
        assert!(lines[1].starts_with("identity: MISMATCH"), "{lines:#?}");
        assert_eq!(
            lines[3],
            format!("  member b (b) {fb}  (this node — its keys here are {fb2})")
        );
        assert_eq!(lines[4], "1 secret(s), 0 readable here");
        assert!(
            lines[5].starts_with("  1 secret(s) not readable on this node"),
            "{lines:#?}"
        );
    }

    /// The resolver's warning: once per name per process.
    #[test]
    fn an_unreadable_name_is_warned_about_once() {
        let seen = Mutex::new(BTreeSet::new());
        assert!(first_warning(&seen, "K"));
        assert!(!first_warning(&seen, "K"));
        assert!(first_warning(&seen, "OTHER"));
    }

    /// Re-encrypting for a new member must never beat a real write made
    /// concurrently on another node.
    #[test]
    fn a_rekey_never_beats_a_real_write() {
        let (t, mut a, mut b) = pair();
        a.set("K", "old", user()).unwrap();
        b.apply(&a.file, None).unwrap();
        // B writes a new value; A, not yet having it, admits C (rekeying
        // the OLD value later than B's write).
        b.set("K", "new-from-b", user()).unwrap();
        let c = node(t.path(), "c");
        a.admit(&c.join_request(), &c.keys.fingerprint()).unwrap();
        a.apply(&b.file, None).unwrap();
        assert_eq!(a.get("K").unwrap().unwrap().value, "new-from-b");
        b.apply(&a.file, None).unwrap();
        assert_eq!(b.get("K").unwrap().unwrap().value, "new-from-b");
    }

    #[test]
    fn an_admitted_node_can_read_older_values_after_the_rekey() {
        let (t, mut a, _b) = pair();
        a.set("K", "written-before-c", user()).unwrap();
        let mut c = node(t.path(), "c");
        a.admit(&c.join_request(), &c.keys.fingerprint()).unwrap();
        c.apply(&a.file, Some(&a.keys.fingerprint())).unwrap();
        assert_eq!(c.get("K").unwrap().unwrap().value, "written-before-c");
    }

    #[test]
    fn a_revoked_node_cannot_read_new_values() {
        let (_t, mut a, mut b) = pair();
        a.set("K", "b-could-read-this", user()).unwrap();
        b.apply(&a.file, None).unwrap();
        let rotate = a.revoke("b").unwrap();
        assert_eq!(rotate, vec!["K".to_string()], "the rotate list");
        a.set("K2", "after-the-revoke", user()).unwrap();
        b.apply(&a.file, None).unwrap();
        assert!(b.get("K2").is_err() || b.get("K2").unwrap().is_none());
        assert!(!b.membership().is_member("b"));
    }

    /// A revoked node keeps its signing key. Rows it signs afterwards —
    /// backdated or not — must not count; rows it made while a member
    /// still do, because the revoke vouched for them.
    #[test]
    fn a_revoked_nodes_later_rows_are_refused_and_its_earlier_ones_kept() {
        let (_t, mut a, mut b) = pair();
        b.set("FROM_B", "legit-while-member", user()).unwrap();
        a.apply(&b.file, None).unwrap();
        a.revoke("b").unwrap();
        // B, not having seen the revoke, writes again — and backdates.
        b.set("FORGED", "after-revoke", user()).unwrap();
        let last = b.file.records.last_mut().unwrap();
        last.at = "2000-01-01T00:00:00.000000000Z".into();
        last.sig = b.keys.sign(&last.signed_bytes());
        a.apply(&b.file, None).unwrap();
        assert!(
            a.get("FORGED").unwrap().is_none(),
            "a revoked node's new write counted"
        );
        assert_eq!(
            a.get("FROM_B").unwrap().unwrap().value,
            "legit-while-member"
        );
    }

    /// A machine that stored keys on its own before joining the pool
    /// keeps them: it adopts the pool's vault, and its names the pool
    /// lacks are carried over; a name the pool has keeps the pool's.
    #[test]
    fn a_solo_vault_is_adopted_into_the_pool_keeping_its_names() {
        let t = tempfile::tempdir().unwrap();
        let mut a = node(t.path(), "a");
        a.init().unwrap();
        a.set("SHARED", "pool-value", user()).unwrap();
        let mut b = node(t.path(), "b");
        b.init().unwrap();
        b.set("SHARED", "b-local-value", user()).unwrap();
        b.set("ONLY_B", "b-only-value", user()).unwrap();
        // Not yet admitted: refused, nothing lost.
        assert!(b.apply(&a.file, Some(&a.keys.fingerprint())).is_err());
        assert_eq!(b.get("ONLY_B").unwrap().unwrap().value, "b-only-value");
        a.admit(&b.join_request(), &b.keys.fingerprint()).unwrap();
        let report = b.apply(&a.file, Some(&a.keys.fingerprint())).unwrap();
        assert!(
            report.summarize().contains("adopted"),
            "{}",
            report.summarize()
        );
        assert_eq!(b.get("SHARED").unwrap().unwrap().value, "pool-value");
        assert_eq!(b.get("ONLY_B").unwrap().unwrap().value, "b-only-value");
        // And A reads what B carried over.
        a.apply(&b.file, None).unwrap();
        assert_eq!(a.get("ONLY_B").unwrap().unwrap().value, "b-only-value");
    }

    #[test]
    fn a_tombstone_replicates() {
        let (_t, mut a, mut b) = pair();
        a.set("K", "v", user()).unwrap();
        b.apply(&a.file, None).unwrap();
        b.unset("K").unwrap();
        a.apply(&b.file, None).unwrap();
        assert!(a.get("K").unwrap().is_none());
        assert!(a.list().is_empty());
    }

    #[test]
    fn two_writers_on_one_node_keep_each_others_rows() {
        let t = tempfile::tempdir().unwrap();
        let mut a = node(t.path(), "a");
        a.update(|v| v.init()).unwrap();
        let mut first = Vault::open(&a.dir, "a", "a").unwrap();
        let mut second = Vault::open(&a.dir, "a", "a").unwrap();
        first.update(|v| v.set("ONE", "1111", user())).unwrap();
        // `second` opened before `first` wrote. Its write must neither
        // drop ONE nor reuse ONE's row id.
        second.update(|v| v.set("TWO", "2222", user())).unwrap();
        let back = Vault::open(&a.dir, "a", "a").unwrap();
        assert_eq!(back.get("ONE").unwrap().unwrap().value, "1111");
        assert_eq!(back.get("TWO").unwrap().unwrap().value, "2222");
    }

    #[test]
    fn a_message_round_trips_and_a_changed_payload_is_refused() {
        let (_t, a, _b) = pair();
        let msg = Message::Offer {
            from_node: "a".into(),
            from_name: "a".into(),
            file: a.file.clone(),
        };
        let text = encode_message(&msg).unwrap();
        assert!(is_vault_message(&text));
        match decode_message(&text).unwrap() {
            Message::Offer { file, .. } => assert_eq!(file, a.file),
            _ => panic!("not an offer"),
        }
        let bad = text.replacen("sha256:", "sha256:0", 1);
        assert!(decode_message(&bad).is_err());
    }

    #[test]
    fn debug_output_never_carries_a_value() {
        let s = Secret {
            name: "K".into(),
            value: "do-not-print-me".into(),
            policy: user(),
        };
        assert!(!format!("{s:?}").contains("do-not-print-me"));
    }

    #[test]
    fn fingerprints_compare_loosely() {
        assert!(same_fingerprint("ab12-cd34", "AB12 CD34"));
        assert!(!same_fingerprint("", ""));
        assert!(!same_fingerprint("ab12-cd34", "ab12-cd35"));
    }

    // ── automatic sync (configsync_auto) ────────────────────────────

    fn offer_of(v: &Vault) -> Message {
        Message::Offer {
            from_node: v.keys.node.clone(),
            from_name: v.keys.name.clone(),
            file: v.file.clone(),
        }
    }

    /// `pair()` builds its vaults in memory; `update` reads the disk.
    fn persist(v: &Vault) {
        write_file(&v.dir, &v.file).unwrap();
    }

    #[test]
    fn an_offer_merges_on_arrival_into_the_vault_this_node_has() {
        let (t, mut a, mut b) = pair();
        persist(&b);
        a.set("K", "value-from-a", user()).unwrap();
        let inbox = t.path().join("inbox");
        inbox_store(&inbox, "laptop", "t1", offer_of(&a)).unwrap();
        let done = apply_inbox_into(Some(&mut b), &inbox).unwrap();
        assert_eq!(done.len(), 1);
        match &done[0].outcome {
            AutoOutcome::Merged(r) => assert!(r.changed.contains(&"K".to_string()), "{r:?}"),
            o => panic!("not merged: {o:?}"),
        }
        assert_eq!(done[0].peer, "laptop");
        // On disk, not only in memory: the resolver reads the file.
        let back = Vault::open(&b.dir, "b", "b").unwrap();
        assert_eq!(back.get("K").unwrap().unwrap().value, "value-from-a");
        assert!(inbox_pending(&inbox).unwrap().is_empty());
        assert_eq!(inbox_pending(&inbox.join("applied")).unwrap().len(), 1);
        // The same rows again change nothing — what keeps two nodes
        // from sending a merge back and forth.
        let before = rows_digest(&back.file);
        inbox_store(&inbox, "laptop", "t2", offer_of(&a)).unwrap();
        let mut b = back;
        apply_inbox_into(Some(&mut b), &inbox).unwrap();
        assert_eq!(rows_digest(&b.file), before);
    }

    /// Adopting a vault takes the root fingerprint the operator read off
    /// the other machine: no offer is merged into a node without one,
    /// and the inbox keeps only the newest offer per peer meanwhile.
    #[test]
    fn a_node_without_a_vault_keeps_the_newest_offer_and_merges_nothing() {
        let (t, mut a, _b) = pair();
        let mut c = node(t.path(), "c");
        let inbox = t.path().join("inbox-c");
        inbox_store(&inbox, "vps", "t1", offer_of(&a)).unwrap();
        a.set("K", "v", user()).unwrap();
        inbox_store(&inbox, "vps", "t2", offer_of(&a)).unwrap();
        inbox_store(&inbox, "vps", "t3", Message::Join(a.join_request())).unwrap();
        inbox_store(&inbox, "laptop", "t4", offer_of(&a)).unwrap();
        let pending = inbox_pending(&inbox).unwrap();
        assert_eq!(pending.len(), 3, "newest vps offer, the join, laptop's");
        let vps_offer = pending
            .iter()
            .map(|p| inbox_read(p).unwrap())
            .find(|e| e.peer == "vps" && matches!(e.message, Message::Offer { .. }))
            .unwrap();
        assert_eq!(vps_offer.task_id, "t2");
        let done = apply_inbox_into(Some(&mut c), &inbox).unwrap();
        assert!(done
            .iter()
            .all(|d| matches!(d.outcome, AutoOutcome::NoVault)));
        assert!(!c.is_initialized());
        assert_eq!(inbox_pending(&inbox).unwrap().len(), 3);
        let done = apply_inbox_into(None, &inbox).unwrap();
        assert_eq!(done.len(), 2);
    }

    #[test]
    fn an_offer_from_another_vault_is_refused_and_stays() {
        let (t, _a, mut b) = pair();
        persist(&b);
        let mut x = node(t.path(), "x");
        x.init().unwrap();
        let inbox = t.path().join("inbox-b");
        inbox_store(&inbox, "stranger", "t1", offer_of(&x)).unwrap();
        let done = apply_inbox_into(Some(&mut b), &inbox).unwrap();
        match &done[0].outcome {
            AutoOutcome::Refused(e) => assert!(e.contains("another vault"), "{e}"),
            o => panic!("not refused: {o:?}"),
        }
        assert_eq!(inbox_pending(&inbox).unwrap().len(), 1);
    }

    #[test]
    fn the_rows_digest_ignores_order_and_sees_a_new_row() {
        let (_t, mut a, _b) = pair();
        a.set("K1", "v1", user()).unwrap();
        a.set("K2", "v2", user()).unwrap();
        let d = rows_digest(&a.file);
        let mut shuffled = a.file.clone();
        shuffled.records.reverse();
        shuffled.members.reverse();
        assert_eq!(rows_digest(&shuffled), d);
        a.set("K3", "v3", user()).unwrap();
        assert_ne!(rows_digest(&a.file), d);
    }
}

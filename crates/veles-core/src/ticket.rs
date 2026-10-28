//! Tickets: cross-node calls that a FRONTEND authorized
//! (PLAN-pool-round.md Part D3; USER 2026-09-29).
//!
//! A peer may ask this node to run what the ACL's User role may run, plus
//! browser tools and read-only file tools, without asking anyone. Anything
//! more needs a ticket, which authorizes ONE exact call — the tool, the
//! sha256 of its canonical arguments, the target node, the origin node,
//! the session, the grant level and an expiry — signed with an Ed25519 key
//! that a frontend holds. The point (USER): "not cross-node
//! confidentiality but rather limiting the blast radius of a node
//! compromise".
//!
//! **Gateways only verify.** They hold public keys
//! (`[cluster.ticket_signers]`), never a private key and never a
//! passphrase, so a node cannot authorize itself. A gateway that received
//! a ticket may forward it with the call, and the target verifies it
//! again.
//!
//! **Where a key lives does not matter.** A key on the requesting node's
//! own machine signs that node's calls like any other (USER 2026-09-30):
//! a compromised gateway already runs anything on its own machine, and
//! what a ticket must stop is that gateway compromising its PEERS at once.
//! It holds no key, so every raised call waits on a person reading the
//! exact call. A rule refusing same-machine keys (in the plan until
//! 2026-09-30) bought little more than that, and it broke the two-device
//! pool: with the arbiter gone, the survivor's own key is the only one
//! left to sign its takeover.
//!
//! **Wire shape.** A ticket is one line, short enough for an IRC message
//! (the paste path: the owner signs on any key-holding device and pastes
//! the ticket as a plain chat message):
//!
//! ```text
//! veles-ticket:1.<key id>.<base64url payload>.<base64url signature>
//! ```
//!
//! The payload carries only what the verifier cannot know on its own —
//! the ticket id, session, tool, grant and expiry. The SIGNED bytes are
//! the full call ([`signing_bytes`]): the verifier rebuilds them from the
//! payload plus its own node id, the authenticated peer's node id and the
//! hash of the arguments it was actually handed. A ticket for any other
//! call, node or argument fails the signature, however its payload reads.
//!
//! A request (what a signer is shown and asked to sign) is the other
//! direction and may be long: it carries the arguments IN FULL, so the
//! person approving reads what will run, not a summary written by the
//! node asking.

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const TICKET_PREFIX: &str = "veles-ticket:1.";
pub const REQUEST_PREFIX: &str = "veles-ticket-request:1.";
/// How long a ticket is good for, unless the request says otherwise.
pub const DEFAULT_TTL_SECS: i64 = 600;
/// Clock slack a verifier allows past `exp` — the signer's clock and the
/// target's are different machines.
pub const CLOCK_SKEW_SECS: i64 = 60;

/// The roles a ticket may grant, lowest first. A ticket never grants more
/// than the target's own ACL gives that role.
pub const GRANTS: [&str; 4] = ["user", "trusted", "admin", "owner"];

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// JSON with every object's keys sorted, compact — the one byte form an
/// argument set hashes to, whatever order a model or a surface wrote the
/// keys in.
pub fn canonical_json(v: &serde_json::Value) -> String {
    fn walk(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(k).unwrap_or_default());
                    out.push(':');
                    walk(&m[k.as_str()], out);
                }
                out.push('}');
            }
            serde_json::Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    walk(x, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    walk(v, &mut out);
    out
}

/// sha256 of the canonical arguments, hex.
pub fn args_hash(args: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(canonical_json(args).as_bytes());
    format!("{:x}", h.finalize())
}

/// A signer key's id: the first 8 bytes of sha256(public key), hex. It
/// names the key in a ticket; the key itself comes from config.
pub fn key_id_of(public: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(public);
    d[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn random_id() -> Result<String> {
    use ring::rand::SecureRandom as _;
    let mut buf = [0u8; 9];
    ring::rand::SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| anyhow::anyhow!("no system randomness"))?;
    Ok(b64().encode(buf))
}

// ── the request: what a signer is shown ────────────────────────────

/// One call, in full, as a signer is asked to authorize it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TicketRequest {
    /// Becomes the ticket's id — the unit a target spends.
    pub id: String,
    /// The node that will make the call (full node id).
    pub origin: String,
    /// The node that will run it (full node id).
    pub target: String,
    pub session: String,
    pub tool: String,
    /// The arguments, in full.
    pub args: serde_json::Value,
    /// The role the call runs as on the target (see [`GRANTS`]).
    pub grant: String,
    /// Unix seconds after which no target accepts the ticket.
    pub exp: i64,
}

impl TicketRequest {
    /// A new request for `tool(args)` from `origin` to `target`, good for
    /// `ttl_secs` from now.
    pub fn new(
        origin: &str,
        target: &str,
        session: &str,
        tool: &str,
        args: serde_json::Value,
        grant: &str,
        ttl_secs: i64,
    ) -> Result<Self> {
        anyhow::ensure!(GRANTS.contains(&grant), "unknown grant {grant:?}");
        Ok(Self {
            id: random_id()?,
            origin: origin.to_string(),
            target: target.to_string(),
            session: session.to_string(),
            tool: tool.to_string(),
            args,
            grant: grant.to_string(),
            exp: chrono::Utc::now().timestamp() + ttl_secs,
        })
    }

    /// The request as one pasteable token.
    pub fn encode(&self) -> Result<String> {
        Ok(format!(
            "{REQUEST_PREFIX}{}",
            b64().encode(serde_json::to_vec(self)?)
        ))
    }

    /// Parse a pasted request (surrounding text ignored: the token is
    /// found wherever it sits in the line).
    pub fn decode(text: &str) -> Result<Self> {
        let start = text
            .find(REQUEST_PREFIX)
            .context("no ticket request in this text")?;
        let body: String = text[start + REQUEST_PREFIX.len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let bytes = b64()
            .decode(body.as_bytes())
            .context("the ticket request is not base64url")?;
        serde_json::from_slice(&bytes).context("the ticket request is not a request")
    }

    /// What the person approving reads: the call, the nodes, the grant.
    /// The arguments are shown in full up to `max_arg_chars`, and the
    /// remainder is counted, never silently cut.
    pub fn describe(&self, max_arg_chars: usize) -> String {
        let args = serde_json::to_string_pretty(&self.args).unwrap_or_default();
        let total = args.chars().count();
        let shown: String = args.chars().take(max_arg_chars).collect();
        let more = if total > max_arg_chars {
            format!(
                "\n… {} more characters (sha256 {})",
                total - max_arg_chars,
                args_hash(&self.args)
            )
        } else {
            String::new()
        };
        format!(
            "{tool} on node {target}, asked by node {origin} for session {session}, as {grant}, \
             until {exp}\nargs:\n{shown}{more}",
            tool = self.tool,
            target = self.target,
            origin = self.origin,
            session = self.session,
            grant = self.grant,
            exp = chrono::DateTime::from_timestamp(self.exp, 0)
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| self.exp.to_string()),
        )
    }
}

/// The exact bytes a signature covers: the whole call. Built the same way
/// by the signer (from the request) and by the verifier (from the ticket's
/// payload, its own knowledge of both nodes, and the arguments it holds).
#[allow(clippy::too_many_arguments)]
pub fn signing_bytes(
    id: &str,
    origin: &str,
    target: &str,
    session: &str,
    tool: &str,
    args_sha256: &str,
    grant: &str,
    exp: i64,
) -> Vec<u8> {
    format!(
        "veles-ticket-v1\n{id}\n{origin}\n{target}\n{session}\n{tool}\n{args_sha256}\n{grant}\n{exp}\n"
    )
    .into_bytes()
}

// ── the ticket: what a target verifies ─────────────────────────────

/// The part of the call a ticket carries on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Payload {
    i: String,
    s: String,
    n: String,
    g: String,
    e: i64,
}

/// A parsed ticket (not yet verified).
#[derive(Debug, Clone, PartialEq)]
pub struct Ticket {
    pub key_id: String,
    pub id: String,
    pub session: String,
    pub tool: String,
    pub grant: String,
    pub exp: i64,
    sig: Vec<u8>,
}

impl Ticket {
    /// Find and parse the first ticket in `text`.
    pub fn parse(text: &str) -> Result<Self> {
        let start = text.find(TICKET_PREFIX).context("no ticket in this text")?;
        let token: String = text[start + TICKET_PREFIX.len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect();
        let token = token.trim_end_matches('.');
        let mut parts = token.split('.');
        let (Some(key_id), Some(payload), Some(sig), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            anyhow::bail!("a ticket is <key id>.<payload>.<signature>");
        };
        let p: Payload = serde_json::from_slice(
            &b64()
                .decode(payload.as_bytes())
                .context("ticket payload is not base64url")?,
        )
        .context("ticket payload is not a ticket")?;
        Ok(Self {
            key_id: key_id.to_string(),
            id: p.i,
            session: p.s,
            tool: p.n,
            grant: p.g,
            exp: p.e,
            sig: b64()
                .decode(sig.as_bytes())
                .context("ticket signature is not base64url")?,
        })
    }

    /// Is `text` (a chat line, a pasted message) carrying a ticket?
    pub fn is_in(text: &str) -> bool {
        text.contains(TICKET_PREFIX)
    }

    /// Is `text` a pasted ticket, the token alone or with a few words
    /// around it ("here you go: …")? A ticket quoted inside a longer
    /// message, such as a pasted log or a question about a refusal, is not:
    /// that message is a prompt like any other.
    pub fn is_pasted(text: &str) -> bool {
        let Some(at) = text.find(TICKET_PREFIX) else {
            return false;
        };
        let end = text[at..]
            .find(char::is_whitespace)
            .map_or(text.len(), |n| at + n);
        text[..at].split_whitespace().count() + text[end..].split_whitespace().count() <= 6
    }
}

/// A public key this node trusts to sign tickets.
#[derive(Debug, Clone)]
pub struct TrustedSigner {
    pub name: String,
    pub key_id: String,
    pub public: Vec<u8>,
}

impl TrustedSigner {
    /// From a config entry: `key = "ed25519:<base64url>"`.
    pub fn from_config(name: &str, key: &str) -> Result<Self> {
        let b = key
            .strip_prefix("ed25519:")
            .with_context(|| format!("ticket signer {name}: the key must start with ed25519:"))?;
        let public = b64()
            .decode(b.trim().as_bytes())
            .with_context(|| format!("ticket signer {name}: the key is not base64url"))?;
        anyhow::ensure!(
            public.len() == 32,
            "ticket signer {name}: not an Ed25519 public key"
        );
        Ok(Self {
            name: name.to_string(),
            key_id: key_id_of(&public),
            public,
        })
    }
}

/// The signers this node trusts, from `[cluster.ticket_signers]`. An
/// entry that does not parse is skipped with a warning — it can only ever
/// refuse tickets, never grant one.
pub fn trusted_signers(cfg: &crate::config::VelesConfig) -> Vec<TrustedSigner> {
    cfg.cluster
        .ticket_signers
        .iter()
        .filter_map(|(name, e)| match TrustedSigner::from_config(name, &e.key) {
            Ok(t) => Some(t),
            Err(err) => {
                tracing::warn!("[cluster.ticket_signers.{name}] ignored: {err:#}");
                None
            }
        })
        .collect()
}

/// Everything a verifier knows about the call in front of it.
pub struct CallContext<'a> {
    /// This node's id — the ticket's target must be it.
    pub me: &'a str,
    /// The authenticated peer's node id — the ticket's origin must be it.
    pub origin: &'a str,
    pub session: &'a str,
    pub tool: &'a str,
    pub args: &'a serde_json::Value,
    pub now: i64,
}

/// What a verified ticket allows.
#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    pub id: String,
    pub grant: String,
    pub signer: String,
    pub exp: i64,
}

/// Verify `ticket` for exactly the call in `ctx`. Every check names what
/// failed — a refusal is read by a person deciding what to do next.
pub fn verify(ticket: &Ticket, trusted: &[TrustedSigner], ctx: &CallContext) -> Result<Verified> {
    let signer = trusted
        .iter()
        .find(|s| s.key_id == ticket.key_id)
        .with_context(|| {
            format!(
                "ticket signed by key {} — not in [cluster.ticket_signers] here",
                ticket.key_id
            )
        })?;
    anyhow::ensure!(
        GRANTS.contains(&ticket.grant.as_str()),
        "ticket grants {:?}, which is not a role",
        ticket.grant
    );
    anyhow::ensure!(
        ticket.tool == ctx.tool && ticket.session == ctx.session,
        "ticket is for {} in session {} — this call is {} in {}",
        ticket.tool,
        ticket.session,
        ctx.tool,
        ctx.session
    );
    anyhow::ensure!(
        ctx.now <= ticket.exp + CLOCK_SKEW_SECS,
        "ticket expired at {} (now {})",
        ticket.exp,
        ctx.now
    );
    let msg = signing_bytes(
        &ticket.id,
        ctx.origin,
        ctx.me,
        ctx.session,
        ctx.tool,
        &args_hash(ctx.args),
        &ticket.grant,
        ticket.exp,
    );
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &signer.public)
        .verify(&msg, &ticket.sig)
        .map_err(|_| {
            anyhow::anyhow!(
                "ticket signature does not match this call — a different node, session, \
                 tool or argument set was signed"
            )
        })?;
    Ok(Verified {
        id: ticket.id.clone(),
        grant: ticket.grant.clone(),
        signer: signer.name.clone(),
        exp: ticket.exp,
    })
}

/// Record a verified ticket as used. A ticket is good for ONE call: a
/// second presentation is refused by id. Expired ids are pruned on the
/// way (they could not verify again anyway).
pub fn spend(store: &crate::store::Store, v: &Verified, now: i64) -> Result<()> {
    let conn = store.conn();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ticket_spent (
            id TEXT PRIMARY KEY,
            exp INTEGER NOT NULL,
            spent_at TEXT DEFAULT (datetime('now'))
        );",
    )?;
    conn.execute(
        "DELETE FROM ticket_spent WHERE exp < ?1",
        rusqlite::params![now - CLOCK_SKEW_SECS],
    )?;
    let n = conn.execute(
        "INSERT OR IGNORE INTO ticket_spent (id, exp) VALUES (?1, ?2)",
        rusqlite::params![v.id, v.exp],
    )?;
    anyhow::ensure!(
        n == 1,
        "ticket {} was already used — a ticket runs one call",
        v.id
    );
    Ok(())
}

// ── keys: held by frontends, never by a gateway ────────────────────

/// A signing key, decrypted, in a frontend's memory for its unlock window.
pub struct SignerKey {
    pair: ring::signature::Ed25519KeyPair,
}

impl SignerKey {
    /// A fresh key: the key pair, and its PKCS#8 bytes to store encrypted.
    pub fn generate() -> Result<(Self, Vec<u8>)> {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| anyhow::anyhow!("key generation failed"))?;
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|_| anyhow::anyhow!("key generation produced an unreadable key"))?;
        Ok((Self { pair }, pkcs8.as_ref().to_vec()))
    }

    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self> {
        Ok(Self {
            pair: ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8)
                .map_err(|_| anyhow::anyhow!("not an Ed25519 key"))?,
        })
    }

    /// The public key as a config value: `ed25519:<base64url>`.
    pub fn public_config(&self) -> String {
        use ring::signature::KeyPair as _;
        format!("ed25519:{}", b64().encode(self.pair.public_key().as_ref()))
    }

    pub fn key_id(&self) -> String {
        use ring::signature::KeyPair as _;
        key_id_of(self.pair.public_key().as_ref())
    }

    /// Sign `req`, producing the one-line ticket.
    pub fn sign(&self, req: &TicketRequest) -> Result<String> {
        anyhow::ensure!(
            GRANTS.contains(&req.grant.as_str()),
            "unknown grant {:?}",
            req.grant
        );
        let msg = signing_bytes(
            &req.id,
            &req.origin,
            &req.target,
            &req.session,
            &req.tool,
            &args_hash(&req.args),
            &req.grant,
            req.exp,
        );
        let sig = self.pair.sign(&msg);
        let payload = serde_json::to_vec(&Payload {
            i: req.id.clone(),
            s: req.session.clone(),
            n: req.tool.clone(),
            g: req.grant.clone(),
            e: req.exp,
        })?;
        Ok(format!(
            "{TICKET_PREFIX}{}.{}.{}",
            self.key_id(),
            b64().encode(payload),
            b64().encode(sig.as_ref())
        ))
    }
}

/// Where a device keeps its signing key: `veles-signer/` beside the
/// gateway's config dir, NOT inside it. The file is encrypted to a
/// passphrase either way; keeping it apart is so nothing mistakes it for
/// gateway state (a backup, a configsync, a scratch copy).
pub fn key_dir() -> std::path::PathBuf {
    let cfg = crate::config::config_dir();
    cfg.parent()
        .map(|p| p.join("veles-signer"))
        .unwrap_or_else(|| cfg.join("signer"))
}

/// Encrypt `pkcs8` to `passphrase` (age, scrypt) and write it 0600.
pub fn save_key(path: &std::path::Path, pkcs8: &[u8], passphrase: &str) -> Result<()> {
    anyhow::ensure!(
        passphrase.chars().count() >= 8,
        "the passphrase must be at least 8 characters"
    );
    let recipient = age::scrypt::Recipient::new(passphrase.to_string().into());
    let enc = age::encrypt(&recipient, pkcs8).context("encrypting the key")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let armored = format!(
        "# veles ticket signing key (age, passphrase)\n{}\n",
        b64().encode(enc)
    );
    crate::setup::write_private(path, &armored)
}

/// Decrypt a key file with `passphrase`.
pub fn load_key(path: &std::path::Path, passphrase: &str) -> Result<SignerKey> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the signing key {}", path.display()))?;
    let body: String = text
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("");
    let enc = b64()
        .decode(body.trim().as_bytes())
        .context("the key file is not a veles signing key")?;
    let identity = age::scrypt::Identity::new(passphrase.to_string().into());
    let pkcs8 = age::decrypt(&identity, &enc)
        .map_err(|_| anyhow::anyhow!("wrong passphrase (or not a veles signing key)"))?;
    SignerKey::from_pkcs8(&pkcs8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A paste is the ticket and a few words; a ticket inside a longer
    /// message leaves that message a prompt.
    #[test]
    fn a_paste_is_the_ticket_and_a_few_words() {
        let t = "veles-ticket:1.key.payload.signature";
        assert!(Ticket::is_pasted(t));
        assert!(Ticket::is_pasted(&format!("  {t}\n")));
        assert!(Ticket::is_pasted(&format!("here you go: {t}")));
        assert!(Ticket::is_pasted(&format!("{t} — signed it, thanks")));
        assert!(!Ticket::is_pasted("no ticket in this message"));
        assert!(!Ticket::is_pasted(&format!(
            "the gateway log says it refused {t} at 11:02, what does that mean for the deploy?"
        )));
        assert!(!Ticket::is_pasted(&format!(
            "error: ticket refused\n  input: {t}\n  reason: expired\n  at remotetool.rs:520"
        )));
    }

    fn a_signer() -> (SignerKey, TrustedSigner) {
        let (k, _) = SignerKey::generate().unwrap();
        let t = TrustedSigner::from_config("phone-app", &k.public_config()).unwrap();
        (k, t)
    }

    fn req() -> TicketRequest {
        TicketRequest::new(
            "node-laptop",
            "node-phone",
            "20260929_120000_abcdef",
            "terminal",
            serde_json::json!({"command": "rm -rf build", "timeout": 30}),
            "owner",
            DEFAULT_TTL_SECS,
        )
        .unwrap()
    }

    fn ctx<'a>(r: &'a TicketRequest, args: &'a serde_json::Value) -> CallContext<'a> {
        CallContext {
            me: &r.target,
            origin: &r.origin,
            session: &r.session,
            tool: &r.tool,
            args,
            now: chrono::Utc::now().timestamp(),
        }
    }

    #[test]
    fn a_ticket_verifies_for_exactly_its_call_and_fits_an_irc_line() {
        let (key, trusted) = a_signer();
        let r = req();
        let line = key.sign(&r).unwrap();
        assert!(line.len() < 400, "{} chars: {line}", line.len());
        let t = Ticket::parse(&format!("here you go: {line} thanks")).unwrap();
        let v = verify(&t, &[trusted], &ctx(&r, &r.args)).unwrap();
        assert_eq!(v.grant, "owner");
        assert_eq!(v.signer, "phone-app");
    }

    #[test]
    fn argument_order_does_not_matter_but_their_content_does() {
        let (key, trusted) = a_signer();
        let r = req();
        let t = Ticket::parse(&key.sign(&r).unwrap()).unwrap();
        let reordered = serde_json::json!({"timeout": 30, "command": "rm -rf build"});
        assert!(verify(&t, std::slice::from_ref(&trusted), &ctx(&r, &reordered)).is_ok());
        let other = serde_json::json!({"command": "rm -rf /", "timeout": 30});
        let e = verify(&t, &[trusted], &ctx(&r, &other))
            .unwrap_err()
            .to_string();
        assert!(e.contains("does not match this call"), "{e}");
    }

    #[test]
    fn a_ticket_is_refused_for_another_node_session_tool_signer_or_time() {
        let (key, trusted) = a_signer();
        let r = req();
        let t = Ticket::parse(&key.sign(&r).unwrap()).unwrap();
        let tr = std::slice::from_ref(&trusted);
        let mut c = ctx(&r, &r.args);
        c.me = "node-other";
        assert!(verify(&t, tr, &c).is_err(), "wrong audience");
        let mut c = ctx(&r, &r.args);
        c.origin = "node-rogue";
        assert!(verify(&t, tr, &c).is_err(), "wrong origin");
        let mut c = ctx(&r, &r.args);
        c.tool = "write_file";
        assert!(verify(&t, tr, &c).is_err(), "wrong tool");
        let mut c = ctx(&r, &r.args);
        c.session = "another";
        assert!(verify(&t, tr, &c).is_err(), "wrong session");
        let mut c = ctx(&r, &r.args);
        c.now = r.exp + CLOCK_SKEW_SECS + 1;
        assert!(verify(&t, tr, &c)
            .unwrap_err()
            .to_string()
            .contains("expired"));
        let (_, stranger) = a_signer();
        assert!(
            verify(&t, &[stranger], &ctx(&r, &r.args)).is_err(),
            "unknown key"
        );
        let mut forged = t.clone();
        forged.grant = "admin".into();
        assert!(
            verify(&forged, tr, &ctx(&r, &r.args)).is_err(),
            "a raised grant breaks the signature"
        );
    }

    #[test]
    fn a_ticket_runs_one_call() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("s.db")).unwrap();
        let (key, trusted) = a_signer();
        let r = req();
        let t = Ticket::parse(&key.sign(&r).unwrap()).unwrap();
        let v = verify(&t, &[trusted], &ctx(&r, &r.args)).unwrap();
        let now = chrono::Utc::now().timestamp();
        spend(&store, &v, now).expect("first use");
        let e = spend(&store, &v, now).unwrap_err().to_string();
        assert!(e.contains("already used"), "{e}");
    }

    #[test]
    fn a_request_round_trips_and_shows_the_call_in_full() {
        let r = req();
        let back =
            TicketRequest::decode(&format!("ticket needed: {} (paste)", r.encode().unwrap()))
                .unwrap();
        assert_eq!(back, r);
        let d = r.describe(1000);
        assert!(
            d.contains("rm -rf build") && d.contains("node-phone"),
            "{d}"
        );
        let short = r.describe(10);
        assert!(short.contains("more characters"), "{short}");
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_needs_its_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.key");
        let (key, pkcs8) = SignerKey::generate().unwrap();
        save_key(&path, &pkcs8, "correct horse battery").unwrap();
        assert!(load_key(&path, "wrong passphrase").is_err());
        let back = load_key(&path, "correct horse battery").unwrap();
        assert_eq!(back.key_id(), key.key_id());
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(
            save_key(&path, &pkcs8, "short").is_err(),
            "a real passphrase"
        );
    }
}

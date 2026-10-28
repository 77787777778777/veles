//! Secrets: exec-time injection + output scrubbing (S4, Letta-parity
//! design — the OPPOSITE of hermes' failed redaction approach).
//!
//! The model writes `$SECRET_NAME` in terminal/execute_code commands.
//! At exec time veles:
//!   1. resolves the name from the secrets store (`config_dir()/secrets.env`,
//!      chmod 600) or the agent .env (read-only borrow — secrets.env
//!      wins),
//!   2. passes the value as an ENV VAR to the process (never spliced
//!      into the command line — ps-safe),
//!   3. scrubs every occurrence of every known value from stdout/stderr
//!      before the result reaches the model,
//!   4. exposes NAMES ONLY in the system prompt (frozen snapshot).
//!
//! The conversation history keeps the `$NAME` placeholder forever; the
//! real value never enters model context through this path.

use anyhow::Context as _;
use std::collections::HashMap;
use std::path::PathBuf;

/// Names must be UPPERCASE letters/digits/underscore, letter or _ first
/// (Letta's rules — sane and grep-safe).
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.is_empty()
}

fn secrets_path() -> PathBuf {
    crate::config::config_dir().join("secrets.env")
}

/// Load the secrets map: secrets.env (owned) layered over the agent's
/// own .env (borrowed read — e.g. provider keys the operator already
/// trusts the agent with). Never writes to .env.
pub fn load() -> HashMap<String, String> {
    let mut map = HashMap::new();
    // agent .env first (lower precedence). `.env` is CONFIG — it moved
    // to config_dir() in the 2026-09-04 XDG split, with secrets.env.
    if let Ok(raw) = std::fs::read_to_string(crate::config::config_dir().join(".env")) {
        for (k, v) in parse_env(&raw) {
            map.insert(k, v);
        }
    }
    // secrets.env wins
    if let Ok(raw) = std::fs::read_to_string(secrets_path()) {
        for (k, v) in parse_env(&raw) {
            map.insert(k, v);
        }
    }
    map
}

/// The values that must never appear in outbound text.
///
/// NOT the same map as [`load`], and the difference is the point.
/// `load` answers "what may `$NAME` expand to in a shell command", so it
/// is every key in both files. Scrubbing used that map too, which meant
/// every `.env` value ≥4 chars was replaced wherever it appeared —
/// including keys that are not credentials. Seen live on 2026-09-11:
/// with `IRC_SERVER=127.0.0.1`, an ACL refusal reached the channel as
///
///   denied for stranger3!~stranger3@IRC_SERVER=<REDACTED>
///
/// and with `IRC_CHANNELS=#flashsupport` any reply naming the channel
/// would be mangled the same way. It fails SAFE, so it was never a
/// leak — but a redaction that fires on the server's own hostname
/// teaches an operator to stop reading redactions, which is how a real
/// one gets missed.
///
/// So origin decides:
///
///   - `secrets.env` — EVERY value. The operator put it there with
///     `veles secret set`; that is a declaration, and second-guessing
///     it by name would be the actual security regression.
///   - `.env` — only entries that are credentials: the key looks like
///     one ([`name_is_secret_shaped`], the same predicate F3 uses for
///     child environments, so there is one definition), or the VALUE
///     carries a known credential prefix whatever the key is called.
///
/// The backstop for anything this misses is unchanged:
/// `a2a::redact_outbound` runs over the same text at the gateway seam
/// and redacts credential-SHAPED strings that were never registered at
/// all.
pub fn scrubbable() -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(raw) = std::fs::read_to_string(crate::config::config_dir().join(".env")) {
        for (k, v) in parse_env(&raw) {
            if name_is_secret_shaped(&k) || value_is_credential_shaped(&v) {
                map.insert(k, v);
            }
        }
    }
    // Declared secrets, wholesale and unconditionally.
    if let Ok(raw) = std::fs::read_to_string(secrets_path()) {
        for (k, v) in parse_env(&raw) {
            map.insert(k, v);
        }
    }
    // The vault: every value in it was declared a secret by being put
    // there, whatever its policy says the model may do with it.
    for (k, s) in crate::vault::local_values() {
        map.insert(k, s.value);
    }
    map
}

/// Does this VALUE announce itself as a credential, whatever its key is
/// called? The prefixes the industry actually uses, so
/// `MY_WRAPPER_VAR=sk-ant-…` is still scrubbed even though its name
/// says nothing.
fn value_is_credential_shaped(v: &str) -> bool {
    let t = v.trim();
    if t.len() < 16 || t.contains(char::is_whitespace) {
        return false;
    }
    [
        "sk-", "sk_", "pk-", "ghp_", "gho_", "ghu_", "ghs_", "github_pat_", "xox", "AKIA", "ASIA",
        "AIza", "ya29.", "hf_", "glpat-", "npm_", "dop_v1_", "shpat_", "SG.", "Bearer ",
    ]
    .iter()
    .any(|p| t.starts_with(p))
        // A JWT: three dot-separated base64url segments.
        || (t.starts_with("ey") && t.matches('.').count() == 2)
}

pub fn parse_env(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        })
        .collect()
}

// ── the engine's resolver ────────────────────────────────────────────

/// Credentials that belong to ONE node, so they never enter the shared
/// vault: each node presents its own A2A bearer and accepts its own
/// peer list, and the desktop token and the signer token are per
/// gateway. A replicated copy would have every node present one node's
/// identity — the vault wins over `.env` — and the pool's own links
/// would refuse each other. Found before the laptop's first `vault
/// import` (2026-10-04), which would have copied all four.
pub const NODE_SECRETS: &[&str] = &[
    "A2A_BEARER_TOKEN",
    "A2A_PEER_TOKENS",
    "DESKTOP_BEARER_TOKEN",
    "VELES_SIGNER_TOKEN",
];

/// Is `name` one node's own credential ([`NODE_SECRETS`])?
pub fn is_node_secret(name: &str) -> bool {
    NODE_SECRETS.contains(&name)
}

/// The value of a credential the ENGINE uses — a provider key, a bot
/// token, a bearer token — from the one place that decides where
/// credentials come from:
///
///   1. the operator's own environment (systemd's `EnvironmentFile`, the
///      shell) — but not a copy `config::load` made of a `.env` line,
///      which would otherwise outrank the vault;
///   2. the vault ([`crate::vault`]);
///   3. `secrets.env`, then `.env`, in `dir`;
///   4. a copy `config::load` made of `dir`'s OWN `.env`, for a line
///      since deleted from the file — the behavior before the vault. A
///      copy of another home's `.env` (a process that loaded two) is no
///      answer for this one.
///
/// Engine use is always allowed. A secret's policy governs what the
/// MODEL may do with it, never whether veles may call its own provider.
/// An empty value is unset at every step.
pub fn resolve(name: &str) -> Option<String> {
    resolve_in(name, &crate::config::config_dir())
}

/// [`resolve`] with the env files read from `dir`.
pub fn resolve_in(name: &str, dir: &std::path::Path) -> Option<String> {
    let env = std::env::var(name).ok().filter(|v| !v.is_empty());
    if let Some(v) = &env {
        if !crate::config::env_copied_from_file(name, v) {
            return Some(v.clone());
        }
    }
    // A node's own credential is never taken from the shared vault, even
    // one that holds it (written before the rule): it would be ANOTHER
    // node's.
    if !is_node_secret(name) {
        if let Some(s) = crate::vault::local_values().get(name) {
            return Some(s.value.clone());
        }
        crate::vault::warn_if_unreadable(name);
    }
    for file in ["secrets.env", ".env"] {
        if let Ok(raw) = std::fs::read_to_string(dir.join(file)) {
            if let Some((_, v)) = parse_env(&raw)
                .into_iter()
                .find(|(k, v)| k == name && !v.is_empty())
            {
                return Some(v);
            }
        }
    }
    let own = dir.join(".env");
    env.filter(|v| crate::config::env_copy_source(name, v).as_deref() == Some(own.as_path()))
}

/// A provider entry's `key_env`, as the media tools (image generation,
/// vision, speech-to-text) read it.
///
/// A variable NAME resolves, or is an error that names it: these tools
/// used to send the name itself to the provider as the key when the
/// variable was unset, and the operator saw a 401 about a key they had
/// never typed. Anything that is not shaped like a name keeps the old
/// leniency — a literal key pasted into `key_env` — and is never echoed.
pub fn resolve_key_env(key_env: &str) -> Result<String, String> {
    if let Some(v) = resolve(key_env) {
        return Ok(v);
    }
    let name_shaped = valid_name(key_env)
        && key_env.contains('_')
        && !key_env.chars().any(|c| c.is_ascii_lowercase());
    if name_shaped {
        return Err(format!(
            "{key_env} is not set — store it with `veles secret set {key_env}` (or the setup wizard)"
        ));
    }
    Ok(key_env.to_string())
}

/// Read the file veles owns, not [`load`]'s merged view of it and `.env`.
///
/// A set or unset is a read-modify-write of `secrets.env` only. Using the
/// merged map here would duplicate every borrowed `.env` credential into
/// a second plaintext file whose copy then shadows the original forever.
fn owned_secrets(
    path: &std::path::Path,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let mut map = std::collections::BTreeMap::new();
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            for (k, v) in parse_env(&raw) {
                map.insert(k, v);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    }
    Ok(map)
}

fn write_owned_secrets(
    path: &std::path::Path,
    map: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = map
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n");
    let body = if body.is_empty() {
        String::new()
    } else {
        body + "\n"
    };
    crate::platform::write_private(path, body.as_bytes())?;
    Ok(())
}

/// Set/unset a secret (persisted to secrets.env, chmod 600).
pub fn set(name: &str, value: &str) -> anyhow::Result<()> {
    if !valid_name(name) {
        anyhow::bail!(
            "invalid secret name '{name}': use UPPERCASE letters, digits, underscores; start with a letter or _"
        );
    }
    let path = secrets_path();
    let mut map = owned_secrets(&path)?;
    map.insert(name.to_string(), value.to_string());
    write_owned_secrets(&path, &map)
}

pub fn unset(name: &str) -> anyhow::Result<()> {
    let path = secrets_path();
    let mut map = owned_secrets(&path)?;
    if map.remove(name).is_none() {
        anyhow::bail!("secret ${name} is not set in secrets.env");
    }
    write_owned_secrets(&path, &map)
}

/// Find `$NAME` references in a command string that have known values.
///
/// Token-aware: `$API_KEY` does NOT match inside `$API_KEY_V2` (the
/// character after the name must not continue an identifier). The Tcl
/// access form `$env(NAME)` counts too — execute_code scripts read the
/// injected variables that way.
pub fn referenced(cmd: &str, secrets: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (k, v) in secrets {
        let needle = format!("${k}");
        let mut hit = false;
        let mut from = 0usize;
        while let Some(pos) = cmd[from..].find(&needle) {
            let after = from + pos + needle.len();
            match cmd[after..].chars().next() {
                Some(c) if c.is_ascii_alphanumeric() || c == '_' => {
                    // Longer identifier continues — keep scanning past
                    // this occurrence for a real standalone one.
                    from += pos + 1;
                }
                _ => {
                    hit = true;
                    break;
                }
            }
        }
        if !hit && cmd.contains(&format!("$env({k})")) {
            hit = true;
        }
        if hit {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

/// Rewrite bare `$NAME` tokens to `$env(NAME)` for embedding in Tcl
/// source (bash expands `$NAME` natively; Tcl needs the env array).
/// Longest names first so `$API_KEY` can't corrupt `$API_KEY_V2`.
pub fn expand_for_tcl(code: &str, names: &[String]) -> String {
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.len()));
    let mut out = code.to_string();
    for k in sorted {
        out = out.replace(&format!("${k}"), &format!("$env({k})"));
    }
    out
}

/// Scrub every known secret value from tool output before the model
/// sees it. `NAME=<REDACTED>` marks a successful injection echo (the
/// placeholder proving injection worked, Letta-parity).
pub fn scrub(text: &str, secrets: &HashMap<String, String>) -> String {
    let mut out = text.to_string();
    for (k, v) in secrets {
        if v.len() >= 4 && out.contains(v.as_str()) {
            out = out.replace(v.as_str(), &format!("{k}=<REDACTED>"));
        }
    }
    // The same values ENCODED: a debug dump, a `curl -v`, a script that
    // base64s a header or hexdumps a buffer puts the key in the output
    // without its literal bytes. Long values only — an encoding of a
    // short one is a short string that turns up by chance.
    for (k, v) in secrets {
        if v.len() < ENCODED_MIN {
            continue;
        }
        for (enc, how) in encodings(v) {
            if enc.len() >= ENCODED_MIN && out.contains(enc.as_str()) {
                out = out.replace(enc.as_str(), &format!("{k}=<REDACTED:{how}>"));
            }
        }
    }
    out
}

/// Values shorter than this are scrubbed literally only.
const ENCODED_MIN: usize = 12;

/// The encoded spellings of `v` that `scrub` looks for: hex (both
/// cases), percent-encoding, and base64 in both alphabets at each of
/// the three byte alignments. A value embedded in a larger base64 blob
/// (`echo "Authorization: Bearer $KEY" | base64`) only shows up as the
/// middle of an encoding whose first and last characters depend on its
/// neighbours, so each alignment contributes its stable core.
fn encodings(v: &str) -> Vec<(String, &'static str)> {
    use base64::Engine as _;
    let bytes = v.as_bytes();
    let mut out = Vec::new();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    out.push((hex.to_ascii_uppercase(), "hex"));
    out.push((hex, "hex"));
    let pct: String = bytes
        .iter()
        .map(|&b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    if pct != v {
        out.push((pct, "url"));
    }
    for offset in 0..3usize {
        let mut buf = vec![0u8; offset];
        buf.extend_from_slice(bytes);
        let full = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&buf);
        // Characters fully determined by the value's own bytes: from
        // the first 3-byte group that starts inside it to the last
        // group that ends inside it.
        let start = offset.div_ceil(3) * 4;
        let end = ((offset + bytes.len()) / 3) * 4;
        if end > start {
            let core = &full[start..end];
            out.push((core.to_string(), "base64"));
            out.push((core.replace('+', "-").replace('/', "_"), "base64"));
        }
    }
    out.sort();
    out.dedup();
    // Longest first, so a shorter spelling never cuts a longer one.
    out.sort_by_key(|(e, _)| std::cmp::Reverse(e.len()));
    out
}

/// F3: which inherited env vars must a NON-OWNER principal's child NOT
/// see? `Some(names)` = strip these before spawn; `None` = owner (or
/// an ownerless surface) — inherit freely, the pre-F3 behavior.
///
/// Two nets, deliberately both:
/// 1. NAME shape — anything password/secret/token/api-key/credential/
///    sasl-shaped, however it got into the gateway's environment
///    (direnv, a service unit, an exported key the operator forgot).
/// 2. VALUE equality — any var whose value IS a loaded secret value,
///    whatever its name (MY_WRAPPER_VAR=sk-ant-…). This catches the
///    values that `scrub` would only redact AFTER the leak reached
///    model context; the point of F3 is that they never get there.
///
/// NOT env_clear: PATH, HOME, locale etc. survive so legitimate
/// tooling keeps working. Pinned the other way by
/// `shell.rs::child_processes_inherit_the_parent_environment` — the
/// OWNER's children still inherit everything; this narrows it by
/// principal, not by mode.
pub fn inherited_secret_env(
    session_id: &str,
    secrets: &HashMap<String, String>,
) -> Option<Vec<String>> {
    let who = crate::acl::session_principal(session_id)?;
    if who.has(crate::acl::Cap::All) {
        return None;
    }
    let mut names: Vec<String> = std::env::vars()
        .filter(|(k, v)| {
            name_is_secret_shaped(k) || (v.len() >= 4 && secrets.values().any(|s| s == v))
        })
        .map(|(k, _)| k)
        .collect();
    names.sort();
    Some(names)
}

/// Does this env NAME look like it carries a credential? Substring,
/// case-insensitive — same spirit as the prefilter's rule names.
fn name_is_secret_shaped(k: &str) -> bool {
    let lk = k.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "secret",
        "token",
        "api_key",
        "apikey",
        "credential",
        "sasl",
    ]
    .iter()
    .any(|m| lk.contains(m))
}

// ── what the model may do with a secret ──────────────────────────────

/// Credentials that ARE a surface veles runs. Nothing the model does
/// needs them — it talks to Telegram through the gateway, not with the
/// bot's token — so by default they never reach a command.
const PLATFORM_NAMES: &[&str] = &[
    "TELEGRAM_BOT_TOKEN",
    "DESKTOP_BEARER_TOKEN",
    "DESKTOP_PEER_TOKENS",
    "A2A_BEARER_TOKEN",
    "A2A_PEER_TOKENS",
    "VELES_PROXY_TOKEN",
    "VELES_TOKEN",
    "ALIBABA_CONSOLE_TOKEN",
];

fn platform_secrets() -> &'static crate::owned::OwnedRegistry<String, ()> {
    static P: std::sync::OnceLock<crate::owned::OwnedRegistry<String, ()>> =
        std::sync::OnceLock::new();
    P.get_or_init(crate::owned::OwnedRegistry::default)
}

/// A module declares the env names of its own credentials (the slopdrop
/// bridge's bearers, R5), so the vault infers `platform` for them —
/// engine only — the way it does for core's own list. The owner is read
/// from the calling thread (M4): purged with the module on reload.
pub fn declare_platform_secret(name: &str) {
    platform_secrets().insert(name.to_string(), ());
}

/// Remove every declaration `owner` made (R3 teardown). `None` (core)
/// removes nothing. Returns the count removed.
pub fn purge_owner_platform_secrets(owner: Option<&'static str>) -> usize {
    platform_secrets().purge(owner)
}

/// Did a module declare `name` as one of its credentials?
pub fn is_declared_platform_secret(name: &str) -> bool {
    platform_secrets().with(&name.to_string(), |_| ()).is_some()
}

/// The policy a name gets when the vault was not told one: from what
/// the config uses the name for.
///
/// - a surface's own credential (the list above, IRC passwords, any
///   `*_BOT_TOKEN`, the Alibaba account keys tokenplan reads, the names
///   a module declared) is `platform`: engine only;
/// - a name some provider entry reads as its key (`key_env`, or
///   `VELES_API_KEY` for an entry with none, or tokenplan's zai/kimi
///   keys) is `provider`: commands may use it, and `api_request` may
///   send it to that provider's own hosts;
/// - anything else is `user`: commands may use it.
pub fn inferred_policy(name: &str, cfg: &crate::config::VelesConfig) -> crate::vault::Policy {
    use crate::vault::{Kind, Policy};
    let tp = &cfg.tokenplan;
    let platform = PLATFORM_NAMES.contains(&name)
        || (name.starts_with("IRC") && (name.contains("PASS") || name.contains("SASL")))
        || name.ends_with("_BOT_TOKEN")
        || name == tp.access_key_id_env
        || name == tp.access_key_secret_env
        || is_declared_platform_secret(name);
    if platform {
        return Policy::default_for(Kind::Platform, Vec::new());
    }
    let hosts = provider_hosts(name, cfg);
    if !hosts.is_empty()
        || name == "VELES_API_KEY"
        || name == tp.zai_key_env
        || name == tp.kimi_key_env
    {
        return Policy::default_for(Kind::Provider, hosts);
    }
    Policy::default_for(Kind::User, Vec::new())
}

/// The hosts of every provider entry that reads `name` as its key —
/// where `api_request` may send it.
pub fn provider_hosts(name: &str, cfg: &crate::config::VelesConfig) -> Vec<String> {
    let mut out: Vec<String> = cfg
        .custom_providers
        .iter()
        .filter(|p| p.key_env.as_deref().unwrap_or("VELES_API_KEY") == name)
        .filter_map(|p| host_of(&p.base_url))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `host` or `host:port` (an explicit non-default port) of a URL.
pub fn host_of(url: &str) -> Option<String> {
    let u = reqwest::Url::parse(url.trim()).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    })
}

/// Does `url` go to one of `hosts`? A bound `host` matches any port on
/// it; a bound `host:port` only that port.
pub fn url_allowed(url: &str, hosts: &[String]) -> bool {
    let Ok(u) = reqwest::Url::parse(url.trim()) else {
        return false;
    };
    let Some(host) = u.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    let port = u.port_or_known_default();
    hosts.iter().any(|h| {
        let h = h.trim().to_ascii_lowercase();
        match h.rsplit_once(':') {
            Some((hh, p)) if p.chars().all(|c| c.is_ascii_digit()) && !hh.contains(':') => {
                hh == host && p.parse::<u16>().ok() == port
            }
            _ => h == host,
        }
    })
}

/// May a caller at `ip` send this gateway secret VALUES? Loopback
/// always; otherwise only from a network in `[vault] trusted_networks`
/// (a WireGuard subnet — the transport has to be encrypted, since the
/// desktop API is plain HTTP). Entries are CIDRs (`10.0.30.0/24`,
/// `fd00::/8`) or single addresses; one that does not parse matches
/// nothing.
pub fn accepts_secret_values(ip: std::net::IpAddr, cfg: &crate::config::VelesConfig) -> bool {
    if ip.is_loopback() {
        return true;
    }
    // An IPv4 peer on a dual-stack socket arrives as ::ffff:a.b.c.d.
    let ip = match ip {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(std::net::IpAddr::V4)
            .unwrap_or(std::net::IpAddr::V6(v6)),
        v4 => v4,
    };
    cfg.vault
        .trusted_networks
        .iter()
        .any(|net| in_network(ip, net.trim()))
}

fn in_network(ip: std::net::IpAddr, net: &str) -> bool {
    use std::net::IpAddr;
    let (addr, bits) = match net.split_once('/') {
        Some((a, b)) => match b.trim().parse::<u32>() {
            Ok(n) => (a.trim(), Some(n)),
            Err(_) => return false,
        },
        None => (net, None),
    };
    let Ok(base) = addr.parse::<IpAddr>() else {
        return false;
    };
    match (ip, base) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let n = bits.unwrap_or(32);
            if n > 32 {
                return false;
            }
            let mask = if n == 0 { 0 } else { u32::MAX << (32 - n) };
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let n = bits.unwrap_or(128);
            if n > 128 {
                return false;
            }
            let mask = if n == 0 { 0 } else { u128::MAX << (128 - n) };
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}

/// A name's policy: the vault's when the vault holds it, else inferred./// A name's policy: the vault's when the vault holds it, else inferred.
pub fn policy_of(name: &str, cfg: &crate::config::VelesConfig) -> crate::vault::Policy {
    match crate::vault::local_values().get(name) {
        Some(s) => s.policy.clone(),
        None => inferred_policy(name, cfg),
    }
}

/// May this session's commands and requests carry secrets at all?
/// Owner sessions only: a session with no principal is a local surface
/// (an owner surface by construction), and a chat principal must hold
/// `Cap::All`. A guest who can run a terminal still gets no `$NAME`.
pub fn session_may_use_secrets(session_id: &str) -> bool {
    match crate::acl::session_principal(session_id) {
        None => true,
        Some(p) => p.has(crate::acl::Cap::All),
    }
}

/// Every known value by name — the env files and the vault, whatever
/// the policy. For stripping and scrubbing, never for injection.
pub fn known_values() -> HashMap<String, String> {
    let mut map = load();
    for (k, s) in crate::vault::local_values() {
        map.insert(k, s.value);
    }
    map
}

/// What `$NAME` may expand to in this session's commands: nothing for a
/// non-owner session; otherwise every value whose policy allows shell
/// use. The vault's policy governs a name it holds — an engine-only
/// vault entry hides the same name's `.env` line too.
pub fn injectable(session_id: &str) -> HashMap<String, String> {
    if !session_may_use_secrets(session_id) {
        return HashMap::new();
    }
    let cfg = crate::config::live();
    let mut out: HashMap<String, String> = load()
        .into_iter()
        .filter(|(k, _)| inferred_policy(k, &cfg).shell)
        .collect();
    for (k, s) in crate::vault::local_values() {
        if s.policy.shell {
            out.insert(k, s.value);
        } else {
            out.remove(&k);
        }
    }
    out
}

/// What one child process (a terminal command, an execute_code script,
/// a background shell) gets.
#[derive(Default)]
pub struct ChildEnv {
    /// Inherited variables to remove before spawning.
    pub strip: Vec<String>,
    /// `$NAME` values the command text references and may have.
    pub inject: Vec<(String, String)>,
    /// Names the text references that this session may NOT have — said
    /// in the result, so a failing command is not a mystery.
    pub withheld: Vec<String>,
}

impl ChildEnv {
    /// One line for the tool result when something was withheld.
    pub fn note(&self) -> Option<String> {
        if self.withheld.is_empty() {
            return None;
        }
        let names: Vec<String> = self.withheld.iter().map(|n| format!("${n}")).collect();
        Some(format!(
            "[secrets] not available to commands in this session: {} \
             (engine-only, or not an owner session — see secret_list)",
            names.join(", ")
        ))
    }
}

/// The environment rules for one child, from its command text.
///
/// - A non-owner session: F3's strip of every secret-shaped inherited
///   variable, and nothing injected.
/// - An owner session: the inherited environment minus the variables
///   whose policy is engine-only (a `.env` bot token that
///   `config::load` copied into the gateway's own environment), and
///   every referenced `$NAME` the policy allows.
pub fn child_env(session_id: &str, text: &str) -> ChildEnv {
    let usable = injectable(session_id);
    let known = known_values();
    let inject = referenced(text, &usable);
    let mut withheld: Vec<String> = referenced(text, &known)
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| !usable.contains_key(k))
        .collect();
    withheld.sort();
    let strip = match inherited_secret_env(session_id, &known) {
        Some(names) => names,
        None => {
            let cfg = crate::config::live();
            let mut names: Vec<String> = std::env::vars()
                .map(|(k, _)| k)
                .filter(|k| !policy_of(k, &cfg).shell)
                .collect();
            names.sort();
            names
        }
    };
    ChildEnv {
        strip,
        inject,
        withheld,
    }
}

/// One name as `secret_list` and `/secret list` show it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Listed {
    pub name: String,
    pub policy: crate::vault::Policy,
    /// "vault", "secrets.env" or ".env".
    pub source: &'static str,
}

impl Listed {
    pub fn line(&self) -> String {
        let p = &self.policy;
        let mut uses = vec!["engine"];
        if p.shell {
            uses.push("commands");
        }
        if !p.hosts.is_empty() {
            uses.push("api_request");
        }
        let mut line = format!(
            "${} — {} ({}; {})",
            self.name,
            p.kind.as_str(),
            uses.join(", "),
            self.source
        );
        if !p.hosts.is_empty() {
            line.push_str(&format!(" → {}", p.hosts.join(", ")));
        }
        if !p.note.is_empty() {
            line.push_str(&format!(" — {}", p.note));
        }
        line
    }
}

/// The credentials in this node's env files: every `secrets.env` line,
/// and the `.env` lines that are credentials — by name shape, by value
/// shape, or because the config reads the name as a key or a token
/// (`OLLAMA_KEY` is none of the first two). The rest of `.env` is
/// ordinary settings (IRC_SERVER, A2A_PORT) and is not a secret.
/// `secrets.env` wins a name both files have, as it does in [`load`].
pub fn env_file_secrets(
    cfg: &crate::config::VelesConfig,
) -> std::collections::BTreeMap<String, (String, &'static str)> {
    let mut out = std::collections::BTreeMap::new();
    let dir = crate::config::config_dir();
    for (file, source) in [(".env", ".env"), ("secrets.env", "secrets.env")] {
        let Ok(raw) = std::fs::read_to_string(dir.join(file)) else {
            continue;
        };
        for (k, v) in parse_env(&raw) {
            if v.is_empty() {
                continue;
            }
            let credential = source == "secrets.env"
                || name_is_secret_shaped(&k)
                || value_is_credential_shaped(&v)
                || !matches!(inferred_policy(&k, cfg).kind, crate::vault::Kind::User);
            if credential {
                out.insert(k, (v, source));
            }
        }
    }
    out
}

/// Every secret this node knows, names and policies only, sorted.
pub fn listing() -> Vec<Listed> {
    let cfg = crate::config::live();
    let mut by: std::collections::BTreeMap<String, Listed> = env_file_secrets(&cfg)
        .into_iter()
        .map(|(k, (_, source))| {
            (
                k.clone(),
                Listed {
                    policy: inferred_policy(&k, &cfg),
                    name: k,
                    source,
                },
            )
        })
        .collect();
    for (k, s) in crate::vault::local_values() {
        by.insert(
            k.clone(),
            Listed {
                name: k,
                policy: s.policy,
                source: "vault",
            },
        );
    }
    by.into_values().collect()
}

/// Prompt line for the frozen system prompt: names only, never values.
pub fn prompt_names() -> Option<String> {
    let all = listing();
    if all.is_empty() {
        // One line, in the owner's volatile section: the one moment it
        // matters is the first time a task needs a key, and the model's
        // instinct then is to ask for it in chat.
        return Some(
            "No secrets are stored. If a task needs an API key or token, call \
secret_request through tool_call — it asks the operator privately. Never ask for a \
secret in chat."
                .into(),
        );
    }
    let shell: Vec<String> = all
        .iter()
        .filter(|l| l.policy.shell)
        .map(|l| format!("- ${}", l.name))
        .collect();
    let http: Vec<String> = all
        .iter()
        .filter(|l| !l.policy.shell && !l.policy.hosts.is_empty())
        .map(|l| format!("- ${} → {}", l.name, l.policy.hosts.join(", ")))
        .collect();
    let mut out = String::new();
    if !shell.is_empty() {
        out.push_str(&format!(
            "These secrets are set for shell commands (write $NAME in the command; the \
value is injected when it runs and never shown to you; a program that reads the \
variable itself still needs $NAME mentioned in the command):\n{}\n",
            shell.join("\n")
        ));
    }
    if !http.is_empty() {
        out.push_str(&format!(
            "These are for api_request only, to the hosts listed:\n{}\n",
            http.join("\n")
        ));
    }
    out.push_str(
        "Through tool_call: api_request sends a secret to its own hosts without it \
touching a command; secret_list shows every name and what it may be used for; \
secret_request asks the operator for a missing one privately. Never ask for a secret \
in chat.",
    );
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch config home on this thread, with a vault whose one
    /// member is this node.
    fn vault_home() -> (tempfile::TempDir, crate::config::DirGuard) {
        let t = tempfile::tempdir().unwrap();
        let g = crate::config::set_dirs(Some(crate::config::DirOverride::forced(t.path())));
        let mut v = crate::vault::Vault::open_or_create_local().unwrap();
        v.update(|v| v.init()).unwrap();
        (t, g)
    }

    fn vault_set(name: &str, value: &str) {
        let mut v = crate::vault::Vault::open_local().unwrap();
        v.update(|v| {
            v.set(
                name,
                value,
                crate::vault::Policy::default_for(crate::vault::Kind::Provider, vec![]),
            )
        })
        .unwrap();
    }

    fn entry(key_env: &str) -> crate::config::ProviderEntry {
        crate::config::ProviderEntry {
            name: "p".into(),
            key_env: Some(key_env.into()),
            ..Default::default()
        }
    }

    /// The provider path reads the vault: a key the wizard stored there
    /// is the key the next request sends, with no restart.
    #[test]
    fn a_provider_key_in_the_vault_is_the_key_sent() {
        let (_t, _g) = vault_home();
        let name = "VELES_T_VAULT_PROVIDER_KEY";
        let dir = crate::config::config_dir();
        assert_eq!(crate::providers::resolve_api_key(&entry(name), &dir), "");
        vault_set(name, "sk-from-the-vault-0001");
        assert_eq!(
            crate::providers::resolve_api_key(&entry(name), &dir),
            "sk-from-the-vault-0001"
        );
        // A later write is seen by the next call — the cache follows
        // the file.
        vault_set(name, "sk-from-the-vault-0002");
        assert_eq!(
            crate::providers::resolve_api_key(&entry(name), &dir),
            "sk-from-the-vault-0002"
        );
    }

    /// A node's own credential stays out of the shared vault: the vault
    /// refuses it, and the resolver reads this node's file for it.
    #[test]
    fn a_node_secret_never_enters_the_vault() {
        let (_t, _g) = vault_home();
        let dir = crate::config::config_dir();
        std::fs::write(dir.join(".env"), "DESKTOP_BEARER_TOKEN=this-nodes-own\n").unwrap();
        let mut v = crate::vault::Vault::open_local().unwrap();
        let refused = v.update(|v| {
            v.set(
                "DESKTOP_BEARER_TOKEN",
                "another-nodes",
                crate::vault::Policy::default_for(crate::vault::Kind::Platform, vec![]),
            )
        });
        assert!(
            refused.is_err(),
            "the vault must refuse a node's own credential"
        );
        assert!(!crate::vault::local_values().contains_key("DESKTOP_BEARER_TOKEN"));
        assert_eq!(
            resolve_in("DESKTOP_BEARER_TOKEN", &dir).as_deref(),
            Some("this-nodes-own")
        );
    }

    /// Precedence: the operator's own export beats the vault (systemd's
    /// EnvironmentFile is a deliberate override); the vault beats a
    /// `.env` line, and a copy `config::load` made of that line.
    #[test]
    fn the_vault_outranks_env_files_and_yields_to_a_real_export() {
        let (_t, _g) = vault_home();
        let name = "VELES_T_VAULT_PRECEDENCE";
        let dir = crate::config::config_dir();
        std::fs::write(dir.join(".env"), format!("{name}=from-dotenv\n")).unwrap();
        assert_eq!(resolve(name).as_deref(), Some("from-dotenv"));
        vault_set(name, "from-the-vault");
        assert_eq!(resolve(name).as_deref(), Some("from-the-vault"));
        // config::load copies .env into the environment; that copy must
        // not outrank the vault.
        let _ = crate::config::load();
        assert_eq!(std::env::var(name).as_deref(), Ok("from-dotenv"));
        assert_eq!(resolve(name).as_deref(), Some("from-the-vault"));
        // A real export does.
        std::env::set_var(name, "from-the-operator");
        assert_eq!(resolve(name).as_deref(), Some("from-the-operator"));
        std::env::remove_var(name);
    }

    fn guest(session: &str) {
        let role = crate::acl::Role::Trusted;
        crate::acl::set_session_principal(
            session,
            crate::acl::Principal {
                id: Some("irc:guest".into()),
                subject: "user:guest".into(),
                role,
                caps: role.default_caps(),
                rule_context_limit: None,
                mask: None,
                via: None,
                ticket_role: None,
            },
        );
    }

    /// A key that reaches the output ENCODED — base64'd inside a bigger
    /// blob, hexdumped, URL-encoded — is scrubbed like the literal one.
    #[test]
    fn encoded_forms_of_a_value_are_scrubbed() {
        use base64::Engine as _;
        let key = "sk-live-0123456789abcdef";
        let s = HashMap::from([("K".to_string(), key.to_string())]);
        for prefix in ["", "x", "xy", "Authorization: Bearer "] {
            let blob = base64::engine::general_purpose::STANDARD
                .encode(format!("{prefix}{key} and more after it"));
            let out = scrub(&blob, &s);
            assert!(
                out.contains("K=<REDACTED:base64>"),
                "prefix {prefix:?}: {out}"
            );
        }
        let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
        assert!(scrub(&format!("dump: {hex}"), &s).contains("K=<REDACTED:hex>"));
        let url = "https://x/?k=p%40ss%2Fw0rd-long-enough";
        let s2 = HashMap::from([("P".to_string(), "p@ss/w0rd-long-enough".to_string())]);
        assert!(
            scrub(url, &s2).contains("P=<REDACTED:url>"),
            "{}",
            scrub(url, &s2)
        );
        // A short value is scrubbed literally only: its encodings are
        // short strings that turn up by chance.
        let short = HashMap::from([("S".to_string(), "abcd".to_string())]);
        assert_eq!(scrub("YWJjZA", &short), "YWJjZA");
    }

    /// A guest session gets no `$NAME` at all, even with a terminal: it
    /// is told the name was withheld, and nothing is injected.
    #[test]
    fn a_guest_session_gets_no_secrets() {
        let (_t, _g) = vault_home();
        vault_set("VELES_T_GUEST_KEY", "sk-not-for-guests-0001");
        let sid = "secrets-guest-session";
        guest(sid);
        let env = child_env(sid, "curl -H \"x: $VELES_T_GUEST_KEY\" https://x");
        assert!(env.inject.is_empty(), "a guest got a secret injected");
        assert_eq!(env.withheld, vec!["VELES_T_GUEST_KEY".to_string()]);
        assert!(env.note().unwrap().contains("$VELES_T_GUEST_KEY"));
        // The owner's session does get it.
        let env = child_env("secrets-owner-session", "echo $VELES_T_GUEST_KEY");
        assert_eq!(env.inject.len(), 1);
    }

    /// An owner's command no longer inherits a bot token `config::load`
    /// copied into the gateway's environment, and cannot inject it.
    #[test]
    fn an_owner_command_never_carries_a_platform_credential() {
        let (_t, _g) = vault_home();
        let name = "VELES_T_BOT_TOKEN";
        std::env::set_var(name, "123456:inherited-bot-token");
        let env = child_env("secrets-owner-bot", &format!("echo ${name}"));
        std::env::remove_var(name);
        assert!(env.strip.contains(&name.to_string()), "{:?}", env.strip);
        assert!(env.inject.is_empty());
        // A provider key stays usable in commands (usage checks, media).
        let cfg = crate::config::VelesConfig {
            custom_providers: vec![crate::config::ProviderEntry {
                name: "p".into(),
                base_url: "https://api.example.com/v1".into(),
                key_env: Some("VELES_T_PROVIDER".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = inferred_policy("VELES_T_PROVIDER", &cfg);
        assert_eq!(p.kind, crate::vault::Kind::Provider);
        assert!(p.shell);
        assert_eq!(p.hosts, vec!["api.example.com".to_string()]);
        for platform in ["TELEGRAM_BOT_TOKEN", "IRC_SASL_PASSWORD", "A2A_PEER_TOKENS"] {
            let p = inferred_policy(platform, &cfg);
            assert_eq!(p.kind, crate::vault::Kind::Platform, "{platform}");
            assert!(!p.shell && p.hosts.is_empty(), "{platform}");
        }
        assert_eq!(
            inferred_policy("GITHUB_TOKEN_X", &cfg).kind,
            crate::vault::Kind::User
        );
    }

    /// The vault's policy governs a name it holds: an engine-only entry
    /// hides the same name's `.env` line from commands too.
    #[test]
    fn the_vaults_policy_governs_a_name_it_holds() {
        let (_t, _g) = vault_home();
        let dir = crate::config::config_dir();
        std::fs::write(dir.join(".env"), "VELES_T_GOVERNED=from-dotenv-value\n").unwrap();
        assert_eq!(
            injectable("secrets-owner-gov")
                .get("VELES_T_GOVERNED")
                .map(String::as_str),
            Some("from-dotenv-value")
        );
        crate::vault::set_local(
            "VELES_T_GOVERNED",
            "from-vault-value",
            crate::vault::Policy::default_for(crate::vault::Kind::Platform, vec![]),
        )
        .unwrap();
        assert!(!injectable("secrets-owner-gov").contains_key("VELES_T_GOVERNED"));
    }

    /// `veles vault import` and the listings take a `.env` line as a
    /// credential by name, by value, or because a provider reads it as
    /// its key — and leave ordinary settings alone.
    #[test]
    fn env_file_secrets_finds_credentials_and_skips_settings() {
        let (_t, _g) = vault_home();
        let dir = crate::config::config_dir();
        std::fs::write(
            dir.join(".env"),
            "IRC_SERVER=irc.libera.chat\nA2A_PORT=9900\nOLLAMA_KEY=plainvalue\n\
             SOME_TOKEN=abc123\nWRAPPED=sk-ant-0123456789abcdef\n",
        )
        .unwrap();
        std::fs::write(dir.join("secrets.env"), "MY_THING=whatever\n").unwrap();
        let cfg = crate::config::VelesConfig {
            custom_providers: vec![crate::config::ProviderEntry {
                name: "ollama".into(),
                base_url: "http://127.0.0.1:11434/v1".into(),
                key_env: Some("OLLAMA_KEY".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let found = env_file_secrets(&cfg);
        let names: Vec<&str> = found.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec!["MY_THING", "OLLAMA_KEY", "SOME_TOKEN", "WRAPPED"],
            "{found:?}"
        );
    }

    /// Loopback always; a listed network (a WireGuard subnet) too; any
    /// other address, a malformed entry, or the wrong family never.
    #[test]
    fn secret_values_are_accepted_from_loopback_and_listed_networks_only() {
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        let mut cfg = crate::config::VelesConfig::default();
        assert!(accepts_secret_values(ip("127.0.0.1"), &cfg));
        assert!(accepts_secret_values(ip("::1"), &cfg));
        assert!(
            !accepts_secret_values(ip("10.0.30.2"), &cfg),
            "nothing listed"
        );
        cfg.vault.trusted_networks = vec![
            "10.0.30.0/24".into(),
            "fd00:abcd::/32".into(),
            "192.168.1.7".into(),
            "not a network".into(),
            "10.9.0.0/99".into(),
        ];
        assert!(
            accepts_secret_values(ip("10.0.30.3"), &cfg),
            "the WireGuard subnet"
        );
        assert!(
            accepts_secret_values(ip("::ffff:10.0.30.3"), &cfg),
            "v4-mapped"
        );
        assert!(!accepts_secret_values(ip("10.0.31.3"), &cfg));
        assert!(accepts_secret_values(ip("fd00:abcd::5"), &cfg));
        assert!(!accepts_secret_values(ip("fd00:abce::5"), &cfg));
        assert!(
            accepts_secret_values(ip("192.168.1.7"), &cfg),
            "a single address"
        );
        assert!(!accepts_secret_values(ip("192.168.1.8"), &cfg));
        assert!(
            !accepts_secret_values(ip("10.9.0.1"), &cfg),
            "a bad prefix matches nothing"
        );
    }

    /// Media tools sent an unset variable's NAME to the provider as the
    /// key. An unset name is an error that names it; a literal key in
    /// `key_env` (the old leniency) still passes and is never echoed.
    #[test]
    fn an_unset_key_env_name_is_an_error_not_the_key() {
        let (_t, _g) = vault_home();
        let err = resolve_key_env("VELES_T_NEVER_SET_KEY").unwrap_err();
        assert!(err.contains("VELES_T_NEVER_SET_KEY is not set"), "{err}");
        assert_eq!(
            resolve_key_env("sk-literal-key-pasted-in").as_deref(),
            Ok("sk-literal-key-pasted-in")
        );
        vault_set("VELES_T_MEDIA_KEY", "sk-media-from-vault");
        assert_eq!(
            resolve_key_env("VELES_T_MEDIA_KEY").as_deref(),
            Ok("sk-media-from-vault")
        );
    }

    /// Setting one node credential must not clone the whole borrowed
    /// environment into the owned secrets file, where it would become a
    /// second plaintext store whose stale values shadow `.env`.
    #[test]
    fn set_changes_only_the_owned_secret_file() {
        let (home, _env) = crate::testenv::scratch_home();
        std::fs::write(
            home.path().join(".env"),
            "DESKTOP_BEARER_TOKEN=dotenv-token\nOTHER_PROVIDER_KEY=do-not-copy\n",
        )
        .unwrap();
        std::fs::write(home.path().join("secrets.env"), "EXISTING_SECRET=keep-me\n").unwrap();

        set("DESKTOP_BEARER_TOKEN", "new-node-token").unwrap();

        assert_eq!(
            std::fs::read_to_string(home.path().join("secrets.env")).unwrap(),
            "DESKTOP_BEARER_TOKEN=new-node-token\nEXISTING_SECRET=keep-me\n"
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join(".env")).unwrap(),
            "DESKTOP_BEARER_TOKEN=dotenv-token\nOTHER_PROVIDER_KEY=do-not-copy\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.path().join("secrets.env"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    /// The same boundary applies to removal. An unrelated `.env` key is
    /// neither copied nor accidentally removed by unsetting an owned row.
    #[test]
    fn unset_removes_only_the_named_owned_row() {
        let (home, _env) = crate::testenv::scratch_home();
        std::fs::write(
            home.path().join(".env"),
            "OTHER_PROVIDER_KEY=do-not-copy\nA2A_PEER_TOKENS=dotenv-peers\n",
        )
        .unwrap();
        std::fs::write(
            home.path().join("secrets.env"),
            "FIRST_SECRET=one\nSECOND_SECRET=two\n",
        )
        .unwrap();

        unset("FIRST_SECRET").unwrap();

        assert_eq!(
            std::fs::read_to_string(home.path().join("secrets.env")).unwrap(),
            "SECOND_SECRET=two\n"
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join(".env")).unwrap(),
            "OTHER_PROVIDER_KEY=do-not-copy\nA2A_PEER_TOKENS=dotenv-peers\n"
        );
        assert!(unset("NOT_AN_OWNED_SECRET").is_err());
    }

    #[test]
    fn name_rules() {
        assert!(valid_name("API_KEY"));
        assert!(valid_name("_PRIVATE"));
        assert!(valid_name("MY_TOKEN_123"));
        assert!(!valid_name("api-key"));
        assert!(!valid_name("123_TOKEN"));
        assert!(!valid_name("my secret"));
        assert!(!valid_name(""));
    }

    #[test]
    fn referenced_finds_dollar_names() {
        let mut m = HashMap::new();
        m.insert("GITHUB_TOKEN".to_string(), "ghp_abc".to_string());
        m.insert("OTHER".to_string(), "xyz".to_string());
        let r = referenced("curl -H \"Authorization: Bearer $GITHUB_TOKEN\" api.x", &m);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "GITHUB_TOKEN");
    }

    /// The live symptom, pinned: with `IRC_SERVER=127.0.0.1` in `.env`,
    /// an ACL refusal reached an IRC channel reading
    /// `denied for stranger3!~stranger3@IRC_SERVER=<REDACTED>`, because
    /// scrubbing used the INJECTION map — every `.env` key, whatever it
    /// holds. A redaction that fires on the server's own hostname
    /// teaches an operator to stop reading redactions.
    #[test]
    fn scrubbing_keeps_the_credentials_and_lets_the_config_through() {
        // `scratch_home` takes the env lock itself; taking it again here
        // deadlocks on a non-reentrant mutex.
        let (home, _env) = crate::testenv::scratch_home();
        std::fs::write(
            home.path().join(".env"),
            "IRC_SERVER=127.0.0.1\n\
             IRC_CHANNELS=#flashsupport\n\
             IRC_NICKNAME=jackma\n\
             A2A_HOST=127.0.0.1\n\
             OPENROUTER_API_KEY=or-v1-abcdefghijklmnop\n\
             A2A_BEARER_TOKEN=beefbeefbeefbeefbeef\n\
             MY_WRAPPER_VAR=sk-ant-aaaaaaaaaaaaaaaaaaaa\n",
        )
        .unwrap();

        let s = scrubbable();
        // Configuration is not a secret.
        for k in ["IRC_SERVER", "IRC_CHANNELS", "IRC_NICKNAME", "A2A_HOST"] {
            assert!(!s.contains_key(k), "{k} is config, not a credential");
        }
        // Credentials are, by NAME…
        assert!(s.contains_key("OPENROUTER_API_KEY"));
        assert!(s.contains_key("A2A_BEARER_TOKEN"));
        // …and by VALUE, whatever the key is called.
        assert!(
            s.contains_key("MY_WRAPPER_VAR"),
            "an sk- value is a credential however it is named"
        );

        // End to end on the sentence that actually went out.
        let line = "[acl] denied for stranger3!~stranger3@127.0.0.1 (user) in #flashsupport";
        assert_eq!(scrub(line, &s), line, "nothing in this line is a secret");
        assert!(
            scrub("key=or-v1-abcdefghijklmnop", &s).contains("<REDACTED>"),
            "and the real one still goes"
        );
    }

    /// A declared secret is scrubbed whatever it is called: the
    /// operator ran `veles secret set`, which is a decision, and
    /// second-guessing it by name would be the real regression.
    #[test]
    fn an_explicitly_declared_secret_needs_no_credential_shaped_name() {
        let (home, _env) = crate::testenv::scratch_home();
        std::fs::write(home.path().join(".env"), "PLAIN_THING=notasecret\n").unwrap();
        let sp = secrets_path();
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, "MY_DIARY=hunter2hunter2\n").unwrap();

        let s = scrubbable();
        assert!(
            s.contains_key("MY_DIARY"),
            "secrets.env is a declaration, not a guess"
        );
        assert!(!s.contains_key("PLAIN_THING"));
        assert!(scrub("it was hunter2hunter2", &s).contains("<REDACTED>"));
    }

    /// The ORIGIN split, pinned directly. `scrubbable()` reads TWO files
    /// and treats them differently: `secrets.env` is a declaration, so
    /// every key in it is scrubbable wholesale; `.env` is the operator's
    /// config, so only credential-SHAPED keys or values are. The two
    /// prior tests cover each arm in isolation; this one puts content
    /// that is neither name- nor value-credential-shaped in BOTH files
    /// and asserts the same-shaped key is kept or dropped purely by its
    /// origin. That is the property PENDING (2026-09-11 round) said no
    /// test pinned, and it is the one a future reader would most plausibly
    /// "simplify" away by merging the two loops.
    #[test]
    fn the_scrubbable_split_is_by_origin_not_by_shape() {
        let (home, _env) = crate::testenv::scratch_home();
        // Identical shape, different origin: a plain word value under a
        // plain name. Neither `name_is_secret_shaped` nor
        // `value_is_credential_shaped` fires on either.
        std::fs::write(home.path().join(".env"), "PLAIN_WORD=ordinaryvalue\n").unwrap();
        let sp = secrets_path();
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, "PLAIN_WORD=ordinaryvalue\n").unwrap();

        // Same key, same value, present in both files. The split says:
        // it is scrubbable BECAUSE it is in secrets.env, and would NOT
        // be if it were only in .env.
        let s = scrubbable();
        assert!(
            s.contains_key("PLAIN_WORD"),
            "a plain-named, plain-valued key in secrets.env is scrubbable \
             by declaration; the same key in .env alone would not be"
        );
        assert_eq!(
            s.get("PLAIN_WORD").map(String::as_str),
            Some("ordinaryvalue")
        );

        // Prove the origin is what carries it, not the shape: drop
        // secrets.env and re-read. The identical .env line must now be
        // NOT scrubbable.
        std::fs::remove_file(&sp).unwrap();
        let s2 = scrubbable();
        assert!(
            !s2.contains_key("PLAIN_WORD"),
            "with only the .env copy left, a plain key is config, not a \
             secret — this is the arm that keeps `IRC_SERVER` legible"
        );

        // And end to end: while declared, the value is redacted; once it
        // is only config, it passes through untouched.
        assert!(
            scrub("the word ordinaryvalue here", &s).contains("PLAIN_WORD=<REDACTED>"),
            "declared secret is redacted in running text"
        );
        assert_eq!(
            scrub("the word ordinaryvalue here", &s2),
            "the word ordinaryvalue here",
            "the same text, with the key now only in .env, is left alone"
        );
    }

    #[test]
    fn scrub_replaces_values_not_names() {
        let mut m = HashMap::new();
        m.insert("K".to_string(), "supersecret".to_string());
        let out = scrub("token=supersecret done", &m);
        assert!(out.contains("K=<REDACTED>"));
        assert!(!out.contains("supersecret"));
    }

    #[test]
    fn scrub_ignores_tiny_values() {
        // 1-3 char values are too collision-prone to scrub.
        let mut m = HashMap::new();
        m.insert("ABBR".to_string(), "ab".to_string());
        let out = scrub("a b c ab", &m);
        assert_eq!(out, "a b c ab");
    }

    #[test]
    fn parse_env_skips_comments_and_quotes() {
        let raw = "# comment\nA=1\nB=\"two words\"\n\nC=\n";
        let v = parse_env(raw);
        assert!(v.contains(&("A".to_string(), "1".to_string())));
        assert!(v.contains(&("B".to_string(), "two words".to_string())));
        assert!(v.contains(&("C".to_string(), "".to_string())));
    }

    #[test]
    fn referenced_is_token_aware() {
        // $API_KEY_V2 must NOT trigger injection for $API_KEY.
        let mut m = HashMap::new();
        m.insert("API_KEY".to_string(), "v1".to_string());
        assert!(referenced("use $API_KEY now", &m).len() == 1);
        assert!(
            referenced("use $API_KEY_V2 now", &m).is_empty(),
            "prefix collision must not match"
        );
        // The Tcl env-array form counts as a reference.
        assert_eq!(referenced("set x $env(API_KEY)", &m).len(), 1);
    }

    #[test]
    fn expand_for_tcl_rewrites_bare_names_longest_first() {
        let names = vec!["API_KEY".to_string(), "API_KEY_V2".to_string()];
        let out = expand_for_tcl("a=$API_KEY b=$API_KEY_V2", &names);
        assert_eq!(out, "a=$env(API_KEY) b=$env(API_KEY_V2)");
        // Already-expanded form is untouched.
        assert_eq!(expand_for_tcl("$env(API_KEY)", &names), "$env(API_KEY)");
    }

    /// F3 unit: the strip list is built from NAME shape AND value
    /// equality, and only for principals below the owner.
    #[test]
    fn inherited_secret_env_is_per_principal_and_catches_both_nets() {
        let (home, _env) = crate::testenv::scratch_home();
        std::fs::write(
            home.path().join("secrets.env"),
            "REG=shapeless-canary-value-99\n",
        )
        .unwrap();
        let map = load();

        // A shape-named var and a shapeless var whose value IS a secret.
        const SHAPED: &str = "VELES_TEST_SASL_PASSWORD";
        const BY_VALUE: &str = "VELES_TEST_INNOCENT_NAME";
        let undo = [
            (SHAPED, std::env::var(SHAPED).ok()),
            (BY_VALUE, std::env::var(BY_VALUE).ok()),
        ];
        std::env::set_var(SHAPED, "whatever");
        std::env::set_var(BY_VALUE, "shapeless-canary-value-99");

        let sid = "f3-secrets-unit-user";
        crate::acl::set_session_principal(
            sid,
            crate::acl::Principal::known("irc:tester", crate::acl::Role::Trusted),
        );
        let strip = inherited_secret_env(sid, &map).expect("non-owner gets a strip list");
        assert!(
            strip.contains(&SHAPED.to_string()),
            "shape-named vars are stripped: {strip:?}"
        );
        assert!(
            strip.contains(&BY_VALUE.to_string()),
            "vars whose VALUE is a secret are stripped whatever the name: {strip:?}"
        );
        assert!(
            !strip.iter().any(|n| n == "PATH" || n == "HOME"),
            "ordinary vars survive: {strip:?}"
        );

        // The owner inherits freely — None, not an empty list.
        let owner_sid = "f3-secrets-unit-owner";
        crate::acl::set_session_principal(
            owner_sid,
            crate::acl::Principal::known("irc:owner", crate::acl::Role::Owner),
        );
        assert!(
            inherited_secret_env(owner_sid, &map).is_none(),
            "the owner's children inherit everything (pinned by shell.rs)"
        );
        // No principal at all (owner surfaces) — same.
        assert!(inherited_secret_env("f3-secrets-unit-nobody", &map).is_none());

        for (k, v) in undo {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    /// `.env` is CONFIG, and the scrubber has to look where it lives.
    ///
    /// The 2026-09-04 XDG split moved `.env` to `config_dir()` —
    /// `authn::env_or_dotenv` says so, and
    /// every provider-key reader passes `config_dir()`. `secrets_path()`
    /// moved with them; these two `.env` reads did not, and kept asking
    /// `veles_home()` (= `data_dir()`).
    ///
    /// Nothing caught it because every other test here uses
    /// `scratch_home`, which sets `VELES_HOME` and so collapses config
    /// and data into ONE directory — the single arrangement in which the
    /// two spellings agree. On a migrated box they do not: there is no
    /// `.env` under data, `scrubbable()` returns an EMPTY map, and every
    /// credential in `.env` reaches the model verbatim out of a terminal
    /// or execute_code result. `scratch_xdg` is the fixture that can
    /// tell the difference.
    #[test]
    fn a_dot_env_credential_is_read_from_the_config_dir_not_the_data_dir() {
        let (_cfg, _data, _env) = crate::testenv::scratch_xdg();
        let config = crate::config::config_dir();
        let data = crate::config::data_dir();
        assert_ne!(config, data, "the fixture must actually split the dirs");

        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join(".env"),
            "ALIBABA_ACCESS_KEY_SECRET=wLeEtSeCrEtwLeEtSeCrEt\n",
        )
        .unwrap();

        let s = scrubbable();
        assert!(
            s.contains_key("ALIBABA_ACCESS_KEY_SECRET"),
            "a credential in $XDG_CONFIG_HOME/veles/.env must be scrubbable; got {:?}",
            s.keys().collect::<Vec<_>>()
        );
        assert!(
            scrub("export KEY=wLeEtSeCrEtwLeEtSeCrEt", &s).contains("<REDACTED>"),
            "and the value must actually be redacted out of tool output"
        );
        // `load` reads the same file for `$NAME` expansion.
        assert!(
            load().contains_key("ALIBABA_ACCESS_KEY_SECRET"),
            "the injection map reads the same file"
        );
    }
}

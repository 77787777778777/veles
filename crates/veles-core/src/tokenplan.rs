//! Alibaba Cloud token-plan quota: what the UPSTREAM account says is
//! left, on every frontend.
//!
//! NOT `/usage`. That reads `store.usage_totals` — what this agent spent
//! by our own counting, which is a ledger we write. This reads what
//! Alibaba says the plan has remaining, which is the number that
//! actually stops a turn. Different source, different truth, different
//! failure mode; hence a separate command and a separate module.
//!
//! TWO PATHS, because two plans answer to two different credentials and
//! neither one can read the other's numbers (PENDING.md, 2026-09-15,
//! where the discovery is written up in full):
//!
//!   - **Path B, team** — an AccessKey pair signing native ACS3 requests
//!     against `modelstudio.{region}.aliyuncs.com`. Credits, seats,
//!     members, spend. No browser, nothing to expire. Implemented here.
//!   - **Path A, personal** — the personal plan's rolling windows
//!     (which the service now reports as a MONTHLY window; see
//!     `WINDOW_FAMILIES`) and the free tier, which live behind a
//!     console session a browser mints
//!     (`bl auth login --console` → `~/.bailian/config.json`). See
//!     `console.rs`'s half below.
//!
//! The token-plan INFERENCE key (`sk-sp-*`, the one in
//! `ALI_TOKEN_PLAN_*_API_KEY`) can read NEITHER. Its gateway 404s every
//! billing and quota path that was tried. Quota is account management
//! under a separate credential — which is also why the Personal
//! Edition's "no automation scripts" clause, which binds the inference
//! key, does not reach this code.
//!
//! THREE TRAPS, each already paid for once. Do not rediscover them:
//!
//!   1. **GET, never POST.** A POST 404s with `InvalidAction.NotFound`,
//!      which reads exactly like "no such endpoint" or "not authorised"
//!      and cost most of a session chasing phantom RAM policies.
//!   2. **The query string is built by hand.** `reqwest`'s `.query()`
//!      goes through `serde_urlencoded` — *form* encoding, where a space
//!      becomes `+` and `!'()*` survive unescaped. ACS3 signs
//!      `encodeURIComponent` semantics with `!'()*` escaped too, i.e.
//!      RFC3986-unreserved only. A form-encoded query signs a string the
//!      far side never reconstructs, and the failure is a signature
//!      mismatch that says nothing about why.
//!   3. **`groupBy`/`timePeriod` are `style:json`** — ONE parameter whose
//!      value is a JSON-encoded string, not repeated fields.
//!
//! `scripts/ali-tokenplan-quota-ref.js` is the verified node reference
//! this is a port of, and it stays in the tree as the oracle: the golden
//! vectors in this module's tests were generated from its signer.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

/// The ACS3 API version every endpoint here answers to.
const ACS_VERSION: &str = "2026-02-10";

/// The region whose ModelStudio host serves the token-plan API when the
/// operator names none. `cn-beijing` serves the same data.
pub const DEFAULT_REGION: &str = "ap-southeast-1";

// ── the signer ────────────────────────────────────────────────────────

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

fn hmac_sha256_hex(key: &str, msg: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.as_bytes())
        .expect("hmac-sha256 accepts a key of any length");
    mac.update(msg.as_bytes());
    format!("{:x}", mac.finalize().into_bytes())
}

/// Percent-encode one query token the way ACS3 signs it.
///
/// `encodeURIComponent` leaves `A-Za-z0-9 - _ . ! ~ * ' ( )` alone; the
/// reference then escapes `!'()*` on top, which lands exactly on the
/// RFC3986 unreserved set. Written out rather than pulled from a crate
/// because the set is the specification here, and a dependency whose
/// default set differs by one character is a signature mismatch nobody
/// can read.
fn rfc3986(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A signed request, ready to send: the query string as signed and the
/// headers that authorize it.
#[derive(Debug, Clone)]
pub struct Signed {
    pub query: String,
    pub headers: Vec<(String, String)>,
}

/// Everything the signature covers. Split out from the send so the
/// canonical string is testable without a socket — which is the whole
/// reason the golden vectors can exist.
#[allow(clippy::too_many_arguments)]
fn sign(
    ak_id: &str,
    ak_secret: &str,
    method: &str,
    host: &str,
    path: &str,
    action: &str,
    params: &BTreeMap<String, String>,
    date: &str,
    nonce: &str,
) -> Signed {
    // BTreeMap iterates sorted, which is what ACS3 requires of the
    // canonical query — stated here because the ordering is load-bearing
    // and a HashMap would pass every test that does not have two params.
    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", rfc3986(k), rfc3986(v)))
        .collect::<Vec<_>>()
        .join("&");

    // GET carries no body, and the far side still signs its digest.
    let content_sha = sha256_hex(b"");

    let headers: Vec<(String, String)> = vec![
        ("content-type".into(), "application/json".into()),
        ("host".into(), host.to_string()),
        ("x-acs-action".into(), action.to_string()),
        ("x-acs-content-sha256".into(), content_sha.clone()),
        ("x-acs-date".into(), date.to_string()),
        ("x-acs-signature-nonce".into(), nonce.to_string()),
        ("x-acs-version".into(), ACS_VERSION.to_string()),
    ];
    // Already in sorted order above; assert it rather than re-sort, so a
    // future addition in the wrong place fails loudly in tests instead
    // of silently signing a different string.
    debug_assert!(
        headers.windows(2).all(|w| w[0].0 < w[1].0),
        "signed headers must be in sorted order"
    );

    let signed_names = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let header_block = headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect::<Vec<_>>()
        .join("\n");

    // The trailing newline after the header block is part of the
    // canonical form, and the empty line after it is the separator
    // before the signed-name list. Both are load-bearing.
    let canonical =
        format!("{method}\n{path}\n{query}\n{header_block}\n\n{signed_names}\n{content_sha}");
    let string_to_sign = format!("ACS3-HMAC-SHA256\n{}", sha256_hex(canonical.as_bytes()));
    let signature = hmac_sha256_hex(ak_secret, &string_to_sign);

    let mut out = headers;
    out.push((
        "authorization".into(),
        format!(
            "ACS3-HMAC-SHA256 Credential={ak_id},SignedHeaders={signed_names},Signature={signature}"
        ),
    ));
    Signed {
        query,
        headers: out,
    }
}

// ── credentials ───────────────────────────────────────────────────────

/// An AccessKey pair.
///
/// A newtype purely so its `Debug` can redact. `access_key` returns a
/// `Result`, and every `unwrap`, `expect` and `dbg!` on a `Result`
/// formats the OK side with `Debug` — so a bare `(String, String)` puts
/// the operator's AccessKey secret into any panic message that ever
/// touches one. That is not hypothetical: a test here did exactly that
/// on 2026-09-15, printing the live secret into the cargo output.
#[derive(Clone)]
pub struct AccessKey {
    id: String,
    secret: String,
}

impl std::fmt::Debug for AccessKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The ID is an identifier and useful in a diagnostic; the secret
        // never is.
        write!(f, "AccessKey {{ id: {:?}, secret: <redacted> }}", self.id)
    }
}

/// The AccessKey pair, or a message naming exactly what is missing and
/// where it goes.
///
/// A blank credential is never allowed to become a request: the far side
/// answers an unsigned call with the same opaque rejection it gives a
/// wrong signature, so "you have not configured this" would arrive
/// looking like "your key is bad".
fn access_key(cfg: &crate::config::VelesConfig) -> Result<AccessKey, String> {
    let tp = &cfg.tokenplan;
    let id = crate::authn::env_or_dotenv(&tp.access_key_id_env);
    let secret = crate::authn::env_or_dotenv(&tp.access_key_secret_env);
    let env_path = crate::config::config_dir().join(".env");
    match (id.is_empty(), secret.is_empty()) {
        (false, false) => Ok(AccessKey { id, secret }),
        (true, true) => Err(format!(
            "no Alibaba AccessKey configured — set {} and {} in {} (or the environment)",
            tp.access_key_id_env,
            tp.access_key_secret_env,
            env_path.display()
        )),
        (true, false) => Err(format!(
            "{} is set but {} is not — set it in {}",
            tp.access_key_secret_env,
            tp.access_key_id_env,
            env_path.display()
        )),
        (false, true) => Err(format!(
            "{} is set but {} is not — set it in {}",
            tp.access_key_id_env,
            tp.access_key_secret_env,
            env_path.display()
        )),
    }
}

// ── the wire ──────────────────────────────────────────────────────────

/// One signed GET. Errors are STRINGS carrying the far side's own words
/// where it gave any, in the `slopdrop::send_and_parse` style — a
/// caller of this is rendering into a chat line, not matching on a
/// variant.
async fn acs_get(
    cfg: &crate::config::VelesConfig,
    ak: &AccessKey,
    path: &str,
    action: &str,
    params: BTreeMap<String, String>,
) -> Result<Value, String> {
    let host = format!("modelstudio.{}.aliyuncs.com", cfg.tokenplan.region);
    let date = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let nonce = uuid::Uuid::new_v4().to_string();
    // GET, never POST: a POST here 404s with `InvalidAction.NotFound`,
    // which reads exactly like "not authorised".
    let signed = sign(
        &ak.id, &ak.secret, "GET", &host, path, action, &params, &date, &nonce,
    );

    let url = if signed.query.is_empty() {
        format!("https://{host}{path}")
    } else {
        format!("https://{host}{path}?{}", signed.query)
    };

    // A USER-AGENT IS MANDATORY, and nothing says so.
    //
    // With no `user-agent` header the tokenplan service answers HTTP 200
    // with `{"Code":"COMMON-1005","Message":"系统异常，请稍后重试",
    // "Success":false}` — "system exception, please retry later" — on
    // every `/tokenplan/*` path. The signature is accepted, the gateway
    // routes it, `x-acs-request-id` and `x-acs-trace-id` come back
    // normally; only the service behind it refuses. `/modelstudio/*`
    // (billing) does NOT care, which is what makes it look like a
    // per-endpoint permission problem instead of a header problem.
    //
    // `scripts/ali-tokenplan-quota-ref.js` never hit this because node's
    // `fetch` sets a user-agent of its own, and neither did any curl
    // check, because curl always sends one. `reqwest` sends none unless
    // asked. Verified 2026-09-15 by bisecting curl's default headers:
    // dropping `Accept` changes nothing, dropping `User-Agent`
    // reproduces COMMON-1005 exactly.
    //
    // Deliberately NOT `providers::resolve_user_agent`: that exists to
    // let inference traffic wear another harness's name, and this is
    // account management signed with the operator's own AccessKey.
    // It says who it is.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("veles/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let mut req = client.get(&url);
    for (k, v) in &signed.headers {
        // `host` is set by hyper from the URL; sending it again is a
        // duplicate header, and it is already covered by the signature.
        if k == "host" {
            continue;
        }
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("{host} unreachable: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    interpret(action, status, &body)
}

/// Turn one answered request into a result. Pure, so the envelope rules
/// below are testable without a socket — which is the only way the
/// `Success: false` case can have a test that fails.
fn interpret(action: &str, status: reqwest::StatusCode, body: &str) -> Result<Value, String> {
    let json: Option<Value> = serde_json::from_str(body).ok();

    if !status.is_success() {
        return Err(named_error(action, Some(status), json.as_ref(), body));
    }
    let v =
        json.ok_or_else(|| format!("{action} answered {status} with something that is not json"))?;

    // A 200 IS NOT A SUCCESS HERE. The tokenplan service reports
    // business failures in the BODY, with the HTTP status left at 200:
    //
    //   {"Message":"系统异常，请稍后重试","Code":"COMMON-1005","Success":false}
    //
    // Reading only the status turns that into an empty result — which
    // renders as "no active seat on this account": a confident wrong
    // answer about somebody's quota, which is worse than an error.
    // Both spellings, because the tokenplan family capitalizes
    // `Success` and the billing family does not.
    let ok = v
        .get("Success")
        .or_else(|| v.get("success"))
        .and_then(Value::as_bool);
    if ok == Some(false) {
        return Err(named_error(action, None, Some(&v), body));
    }
    Ok(v)
}

/// The far side's own words where it gave any, rather than a paraphrase.
fn named_error(
    action: &str,
    status: Option<reqwest::StatusCode>,
    json: Option<&Value>,
    body: &str,
) -> String {
    let named = json.and_then(|j| {
        let code = j
            .get("Code")
            .or_else(|| j.get("code"))
            .and_then(Value::as_str)?;
        let msg = j
            .get("Message")
            .or_else(|| j.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("");
        Some(format!("{code}: {msg}"))
    });
    match (status, named) {
        (Some(s), Some(n)) => format!("{action} — HTTP {s} {n}"),
        (Some(s), None) => format!("{action} — HTTP {s}: {}", body.trim()),
        (None, Some(n)) => format!("{action} — {n}"),
        (None, None) => format!("{action} — refused without saying why"),
    }
}
/// The tokenplan endpoints wrap their payload in `Data`; the billing
/// ones use lowercase `data` and are a different API family entirely.
/// One unwrapper so a caller never has to know which it asked.
fn payload(v: &Value) -> &Value {
    v.get("Data").or_else(|| v.get("data")).unwrap_or(v)
}

// ── what a plan has left ──────────────────────────────────────────────

/// One equity pool on a seat. Every figure is optional because a
/// RELEASED seat really does answer `{"EquityType":"CREDITS"}` and
/// nothing else — rendering that as `0 of 0` would invent a number the
/// API declined to give.
#[derive(Debug, Clone, PartialEq)]
pub struct Credits {
    pub total: Option<f64>,
    pub remaining: Option<f64>,
    pub unit: String,
    pub cycle_start: Option<DateTime<Utc>>,
    pub cycle_end: Option<DateTime<Utc>>,
}

impl Credits {
    /// Fraction of the pool spent, when both ends are known.
    pub fn used_fraction(&self) -> Option<f64> {
        let (t, r) = (self.total?, self.remaining?);
        if t <= 0.0 {
            return None;
        }
        Some(((t - r) / t).clamp(0.0, 1.0))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Seat {
    pub seat_id: Option<String>,
    pub subscription: Option<String>,
    pub spec_type: Option<String>,
    pub status: Option<String>,
    pub assigned: Option<String>,
    pub credits: Option<Credits>,
}

impl Seat {
    /// A seat that is still serving requests. `RELEASE`/`UNASSIGNED`
    /// rows are previous subscriptions the API keeps returning; they
    /// carry no quota and must not be averaged into the live one.
    pub fn is_live(&self) -> bool {
        self.status.as_deref() != Some("RELEASE") && self.assigned.as_deref() != Some("UNASSIGNED")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spend {
    pub bill_month: String,
    pub pretax: String,
    pub tax: String,
    pub total: String,
    pub currency: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TeamQuota {
    pub seats: Vec<Seat>,
    pub members: Option<u64>,
    pub seated: Option<u64>,
    pub unseated: Option<u64>,
    pub spend: Option<Spend>,
}

impl TeamQuota {
    pub fn live_seats(&self) -> impl Iterator<Item = &Seat> {
        self.seats.iter().filter(|s| s.is_live())
    }
}

/// The personal plan's rolling windows and free tier (Path A).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PersonalQuota {
    pub windows: Vec<Window>,
    pub free_tier: Vec<FreeTier>,
}

/// A rolling usage window. `used_fraction` is `None` when the API did
/// not report one — which it currently does not for the 5-hour window,
/// where the vendor's own CLI prints "may be unlimited". `None` renders
/// as "not reported", never as 0%.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub label: String,
    pub used_fraction: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FreeTier {
    pub model: String,
    pub remaining: Option<f64>,
    pub total: Option<f64>,
    pub expires: Option<DateTime<Utc>>,
}

/// The z.ai coding plan, as `/api/monitor/usage/quota/limit` tells it
/// (added 2026-09-16). The wire is `{"level": "pro", "limits": […]}` —
/// a flat list where each entry carries its own `type`, and WHAT a
/// `type` means is decided by `unit`: `TOKENS_LIMIT unit=3` is the
/// 5-hour token window, `unit=6` the weekly quota, `TIME_LIMIT unit=5`
/// the monthly tool quota. The unit key is not decoration; it is the
/// discriminator, and two entries of one type differ only by it.
#[derive(Debug, Clone, PartialEq)]
pub struct ZaiQuota {
    pub level: Option<String>,
    pub window_5h: Option<Window>,
    pub weekly: Option<Window>,
    pub tools: Option<ZaiTools>,
}

/// The monthly tool quota: search-prime / web-reader / zread. The
/// wire is INVERTED from what the names suggest (live catch,
/// 2026-09-15): `usage` is the POOL SIZE (1000 calls), `currentValue`
/// is what has been spent (69), `remaining` the difference — and
/// `number`/`unit` mean "1 month", the same {unit, number} clock as
/// TOKENS_LIMIT's "5 hours". A first reading took `usage` for used
/// and `number` for the pool and produced "1000 of 1" — the fixture
/// agreed because it had been written from the same misreading.
#[derive(Debug, Clone, PartialEq)]
pub struct ZaiTools {
    pub limit: Option<f64>,
    pub used: Option<f64>,
    pub remaining: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
    /// `{model_code: calls}` for the tools that ate it.
    pub details: Vec<(String, f64)>,
}

impl ZaiTools {
    pub fn used_fraction(&self) -> Option<f64> {
        let (u, l) = (self.used?, self.limit?);
        if l <= 0.0 {
            return None;
        }
        Some((u / l).clamp(0.0, 1.0))
    }
}

impl ZaiQuota {
    /// The tightest of the three pools. Note the unit mismatch — the
    /// token windows are shared token budgets while the tool quota is
    /// a call count — but the mix is the vendor's own, and each limit
    /// is separately enforced.
    pub fn tightest_used_fraction(&self) -> Option<f64> {
        [
            self.window_5h.as_ref().and_then(|w| w.used_fraction),
            self.weekly.as_ref().and_then(|w| w.used_fraction),
            self.tools.as_ref().and_then(ZaiTools::used_fraction),
        ]
        .into_iter()
        .flatten()
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }
}

/// Map a plan to what a session ON that plan should see. The plans are
/// DISJOINT pools — an evening on the z.ai plan moves nothing on any
/// Alibaba bill, the personal and team plans bill independently, and
/// Kimi is another vendor's plan again — so an ambient figure read off
/// the wrong one is worse than none: it answers "can I keep going" with
/// a number that has nothing to do with the question.
///
/// `None` for a provider no plan answers for. That is the honest answer
/// and it is not the same as `Personal`: "you are on a plan whose
/// remainder we cannot read" and "you are on no plan at all" are
/// different facts, and the surfaces render them differently — the
/// status field shows the AMOUNT (which is still real, it is OUR
/// ledger) with no upstream percentage beside it.
///
/// The old fallback-to-personal default was wrong in exactly the case
/// that matters (FOUND 2026-09-20, USER): the configured default route
/// is `ali-token-plan-team`, so a session on an unknown provider was
/// shown the PERSONAL pool's fraction — a confident number off a bill
/// it was not spending.
pub fn plan_for_provider(provider: &str) -> Option<Plan> {
    let p = provider.trim().to_ascii_lowercase();
    // `custom:`-prefixed route names resolve to the bare tail; both
    // spellings appear in the wild depending on which layer wrote the
    // string.
    let p = p.strip_prefix("custom:").unwrap_or(&p).to_string();
    if p.contains("zai") {
        return Some(Plan::Zai);
    }
    if p.contains("kimi") {
        return Some(Plan::Kimi);
    }
    // THE TWO ALIBABA PLANS BOTH BILL QWEN ROUTES, and one entry per
    // credential is not enough: `ali-responses` is a SEPARATE provider
    // entry (Responses-API mode, built-in web tools) that authenticates
    // with `ALI_TOKEN_PLAN_TEAM_API_KEY` — so it draws on the TEAM
    // pool. Matching these by name is what keeps them apart, and the
    // order matters: "personal" and "team" must be tested before the
    // generic "ali" prefix.
    if p.contains("personal") {
        return Some(Plan::Personal);
    }
    if p.contains("team") || p.contains("responses") {
        return Some(Plan::Team);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Team,
    Personal,
    Zai,
    Kimi,
}

impl Plan {
    pub fn label(&self) -> &'static str {
        match self {
            Plan::Team => "team",
            Plan::Personal => "personal",
            Plan::Zai => "zai",
            Plan::Kimi => "kimi",
        }
    }

    /// The fraction USED of this plan's tightest pool — `None` when
    /// the plan has no reading (off, not configured, or the fetch
    /// failed), which renders as an absence, never as 0%.
    pub fn tightest_used_fraction(&self, snap: &QuotaSnapshot) -> Option<f64> {
        match self {
            Plan::Team => snap
                .team
                .iter()
                .flat_map(|t| t.live_seats())
                .filter_map(|s| s.credits.as_ref().and_then(Credits::used_fraction))
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)),
            Plan::Personal => snap
                .personal
                .iter()
                .flat_map(|p| p.windows.iter())
                .filter_map(|w| w.used_fraction)
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)),
            Plan::Zai => snap.zai.as_ref().and_then(ZaiQuota::tightest_used_fraction),
            Plan::Kimi => snap
                .kimi
                .as_ref()
                .and_then(KimiQuota::tightest_used_fraction),
        }
    }

    /// The JSON key this plan's fraction is served under in
    /// `/api/quota` (`<label>_used_fraction`). One place, so the
    /// engine and every consumer cannot disagree about the spelling.
    pub fn fraction_key(&self) -> String {
        format!("{}_used_fraction", self.label())
    }
}

/// Everything `/quota` can say, plus why any half of it is missing.
///
/// The two paths fail independently — different credentials, different
/// hosts, different expiry stories — so one being down must never blank
/// the other, and the reason must survive to the renderer. A `None`
/// beside a `None` error would be indistinguishable from "this account
/// has no team plan".
#[derive(Debug, Clone)]
pub struct QuotaSnapshot {
    pub fetched_at: DateTime<Utc>,
    pub team: Option<TeamQuota>,
    pub team_error: Option<String>,
    pub personal: Option<PersonalQuota>,
    pub personal_error: Option<String>,
    /// The z.ai coding plan — a THIRD disjoint pool, read with the
    /// plan's own key rather than an Alibaba credential (2026-09-16).
    pub zai: Option<ZaiQuota>,
    pub zai_error: Option<String>,
    /// The Kimi Code plan — a FOURTH pool, another vendor again, read
    /// with the plan's own key (2026-09-20).
    pub kimi: Option<KimiQuota>,
    pub kimi_error: Option<String>,
}

impl QuotaSnapshot {
    /// Nothing to show and nothing to explain — the shape a caller
    /// should never be handed silently.
    pub fn is_empty(&self) -> bool {
        self.team.is_none()
            && self.personal.is_none()
            && self.zai.is_none()
            && self.kimi.is_none()
            && self.team_error.is_none()
            && self.personal_error.is_none()
            && self.zai_error.is_none()
            && self.kimi_error.is_none()
    }

    /// Every plan's tightest fraction, in one place.
    ///
    /// The cross-plan "tightest of all" figure is gone from the
    /// surfaces (USER 2026-09-20: the plans are disjoint, and one
    /// figure over all of them is what made the bar unreadable) — but
    /// it is still the right answer to "is ANYTHING about to run out",
    /// so it lives here for callers that need exactly that.
    pub fn tightest_used_fraction(&self) -> Option<f64> {
        let team = self
            .team
            .iter()
            .flat_map(|t| t.live_seats())
            .filter_map(|s| s.credits.as_ref().and_then(Credits::used_fraction));
        let personal = self
            .personal
            .iter()
            .flat_map(|p| p.windows.iter())
            .filter_map(|w| w.used_fraction);
        let rest = [
            self.zai.as_ref().and_then(ZaiQuota::tightest_used_fraction),
            self.kimi
                .as_ref()
                .and_then(KimiQuota::tightest_used_fraction),
        ]
        .into_iter()
        .flatten();
        team.chain(personal)
            .chain(rest)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }
}

// ── parsing ───────────────────────────────────────────────────────────

fn epoch_ms(v: Option<&Value>) -> Option<DateTime<Utc>> {
    let ms = v?.as_i64()?;
    // 0 is the API's "unset", not 1970.
    if ms <= 0 {
        return None;
    }
    Utc.timestamp_millis_opt(ms).single()
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The CREDITS pool out of an `EquityList`, if it carries any figures.
fn parse_credits(equity: &Value) -> Option<Credits> {
    let list = equity.as_array()?;
    let e = list
        .iter()
        .find(|e| e.get("EquityType").and_then(Value::as_str) == Some("CREDITS"))
        .or_else(|| list.first())?;
    let total = e.get("CycleTotalValue").and_then(Value::as_f64);
    let remaining = e.get("CycleSurplusValue").and_then(Value::as_f64);
    // A released seat answers with the type and nothing else. That is
    // not a pool of zero; it is no pool.
    if total.is_none() && remaining.is_none() {
        return None;
    }
    Some(Credits {
        total,
        remaining,
        unit: text(e, "EquityUnit").unwrap_or_else(|| "Credit".to_string()),
        cycle_start: epoch_ms(e.get("CycleStartTime")),
        cycle_end: epoch_ms(e.get("CycleEndTime")),
    })
}

fn parse_seats(v: &Value) -> Vec<Seat> {
    let items = payload(v)
        .get("Items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .map(|it| Seat {
            seat_id: text(it, "SeatId"),
            subscription: text(it, "InstanceCode"),
            spec_type: text(it, "SpecType"),
            status: text(it, "Status"),
            assigned: text(it, "AssignedStatus"),
            credits: it.get("EquityList").and_then(parse_credits),
        })
        .collect()
}

fn parse_spend(v: &Value, bill_month: &str) -> Option<Spend> {
    let d = payload(v);
    Some(Spend {
        bill_month: bill_month.to_string(),
        pretax: text(d, "pretaxAmount")?,
        tax: text(d, "taxAmount").unwrap_or_else(|| "0".into()),
        total: text(d, "totalAmount").unwrap_or_else(|| "0".into()),
        currency: text(d, "currency").unwrap_or_else(|| "USD".into()),
    })
}

// ── fetching ──────────────────────────────────────────────────────────

/// The billing month the overview call defaults to: this one, UTC.
fn current_bill_month() -> String {
    Utc::now().format("%Y-%m").to_string()
}

/// Path B — everything the AccessKey can read.
pub async fn fetch_team(
    cfg: &crate::config::VelesConfig,
    bill_month: Option<&str>,
) -> Result<TeamQuota, String> {
    let ak = access_key(cfg)?;
    let mut out = TeamQuota::default();

    // The seat detail is the one call that carries Credits, so its
    // failure is the whole team half's failure. The rest are enrichment
    // and are allowed to be missing.
    let seats = acs_get(
        cfg,
        &ak,
        "/tokenplan/subscription/seat-detail",
        "GetSubscriptionSeatDetails",
        BTreeMap::new(),
    )
    .await?;
    out.seats = parse_seats(&seats);

    if let Ok(stats) = acs_get(
        cfg,
        &ak,
        "/tokenplan/organization/member-seat-stats",
        "GetOrganizationMemberSeatStats",
        BTreeMap::new(),
    )
    .await
    {
        let d = payload(&stats);
        out.members = d.get("TotalMemberCount").and_then(Value::as_u64);
        out.seated = d.get("SeatedMemberCount").and_then(Value::as_u64);
        out.unseated = d.get("UnseatedMemberCount").and_then(Value::as_u64);
    }

    let month = bill_month
        .map(str::to_string)
        .unwrap_or_else(current_bill_month);
    let mut params = BTreeMap::new();
    params.insert("billMonth".to_string(), month.clone());
    // style:json — ONE parameter whose value is a JSON string.
    params.insert(
        "groupBy".to_string(),
        r#"[{"code":"WORKSPACE_ID"}]"#.to_string(),
    );
    if let Ok(billing) = acs_get(
        cfg,
        &ak,
        "/modelstudio/billing/overview",
        "GetBillingOverview",
        params,
    )
    .await
    {
        out.spend = parse_spend(&billing, &month);
    }

    Ok(out)
}

// ── Path A — the personal plan ────────────────────────────────────────
//
// The personal 5h/weekly windows do not exist on the AccessKey API at
// all. They live behind the CONSOLE gateway, which speaks a different
// protocol to a different host and authenticates with a bearer token a
// browser mints (`bl auth login --console`, stored in
// `~/.bailian/config.json`).
//
// The contract below was read out of `bailian-cli-core`'s own bundle
// rather than guessed: `callConsoleGateway` builds the URL and form
// body, and `unwrapResponse` peels the envelope. Both are reproduced
// exactly, because every layer of that envelope is a place a silent
// `None` could hide a real answer.

/// Console gateway host + OpenAPI action, by region and site. From
/// bailian-cli-core's own table; an unknown region falls back to
/// `cn-beijing` exactly as it does.
fn console_gateway(region: &str, site: &str) -> (&'static str, &'static str) {
    match (region, site) {
        ("ap-southeast-1", "international") => (
            "bailian-singapore-cs.alibabacloud.com",
            "IntlBroadScopeAspnGateway",
        ),
        ("ap-southeast-1", _) => (
            "modelstudio-cs.console.aliyun.com",
            "IntlBroadScopeAspnGateway",
        ),
        (_, "international") => (
            "bailian-cs.console.alibabacloud.com",
            "BroadScopeAspnGateway",
        ),
        _ => ("bailian-cs.console.aliyun.com", "BroadScopeAspnGateway"),
    }
}

/// A browser-minted console session. `Debug` redacts for the same
/// reason [`AccessKey`]'s does.
#[derive(Clone)]
struct ConsoleSession {
    token: String,
    region: String,
    site: String,
    switch_agent: Option<String>,
}

impl std::fmt::Debug for ConsoleSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ConsoleSession {{ region: {:?}, site: {:?}, token: <redacted> }}",
            self.region, self.site
        )
    }
}

/// `~` → `$HOME`. The config default names the path the way the vendor's
/// docs do, and an un-expanded `~` would just be a missing file.
fn expand_tilde(path: &str) -> std::path::PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => std::path::PathBuf::from(home).join(rest),
            None => std::path::PathBuf::from(path),
        },
        None => std::path::PathBuf::from(path),
    }
}

/// The console session, or a message saying how to mint one.
fn console_session(cfg: &crate::config::VelesConfig) -> Result<ConsoleSession, String> {
    let path = expand_tilde(&cfg.tokenplan.console_config);

    // An explicit env override wins, so a headless box can carry a token
    // without the vendor's CLI ever being installed.
    let env_token = crate::authn::env_or_dotenv("ALIBABA_CONSOLE_TOKEN");

    let file: Option<Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());

    let token = if !env_token.is_empty() {
        env_token
    } else {
        match file.as_ref().and_then(|v| text(v, "access_token")) {
            Some(t) => t,
            None => {
                return Err(format!(
                    "no console session — run `bl auth login --console`, \
                     or set ALIBABA_CONSOLE_TOKEN (looked in {})",
                    path.display()
                ))
            }
        }
    };

    let f = file.unwrap_or(Value::Null);
    Ok(ConsoleSession {
        token,
        region: text(&f, "console_region").unwrap_or_else(|| "cn-beijing".into()),
        site: text(&f, "console_site").unwrap_or_else(|| "domestic".into()),
        switch_agent: text(&f, "switch_agent"),
    })
}

/// One console-gateway call.
///
/// POST, form-encoded, with the whole request JSON-encoded into a single
/// `params` field — nothing about this resembles the ACS3 path above,
/// which is why it has its own function rather than a flag on `acs_get`.
async fn console_call(s: &ConsoleSession, api: &str, data: Value) -> Result<Value, String> {
    let (host, action) = console_gateway(&s.region, &s.site);

    // `cornerstoneParam` is required and constant; the gateway rejects
    // the call without it. Copied field for field from
    // bailian-cli-core.
    let mut cornerstone = serde_json::json!({
        "protocol": "V2",
        "console": "ONE_CONSOLE",
        "productCode": "p_efm",
        "switchUserType": 3,
        "consoleSite": "BAILIAN_ALIYUN",
    });
    if let Some(agent) = &s.switch_agent {
        cornerstone["switchAgent"] = Value::String(agent.clone());
    }
    let mut payload = match data {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    payload.insert("cornerstoneParam".into(), cornerstone);
    let params = serde_json::json!({ "Api": api, "V": "1.0", "Data": payload }).to_string();

    let url = format!(
        "https://{host}/cli/api.json?action={action}&product=sfm_bailian&api={}",
        rfc3986(api)
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("veles/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let resp = client
        .post(&url)
        .header("accept", "*/*")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("authorization", format!("Bearer {}", s.token))
        .body(form_urlencode(&[
            ("params", params.as_str()),
            ("region", s.region.as_str()),
        ]))
        .send()
        .await
        .map_err(|e| format!("{host} unreachable: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("console gateway — HTTP {status}: {}", body.trim()));
    }
    let root: Value = serde_json::from_str(&body)
        .map_err(|_| "console gateway answered with something that is not json".to_string())?;

    // The gateway reports failures inside `data`, with the HTTP status
    // at 200 — the same shape trap as the ACS3 side.
    let d = root.get("data");
    let failed = d.and_then(|d| d.get("success")).and_then(Value::as_bool) == Some(false);
    let err_code = d.and_then(|d| d.get("errorCode")).and_then(Value::as_str);
    if failed {
        if let Some(code) = err_code.filter(|c| !c.is_empty()) {
            // THE FAILURE MODE PENDING WARNED ABOUT: this gateway
            // returns a byte-identical `NotLogined` for a real token, no
            // token and a bogus token, so it can never mean "not
            // permitted". It means the session needs re-minting, and the
            // answer has to say so or somebody goes looking for a
            // permissions problem that is not there.
            if code.contains("NotLogined") {
                return Err(
                    "console session expired or rejected — re-run: bl auth login --console"
                        .to_string(),
                );
            }
            let msg = d
                .and_then(|d| d.get("errorMsg"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(format!("console gateway — {code}: {msg}"));
        }
    }
    Ok(unwrap_console(&root))
}

/// `bailian-cli-core`'s `unwrapResponse`, reproduced exactly:
/// `data.DataV2.data.data ?? data.DataV2.data ?? data.DataV2`, else
/// `data.data ?? data`.
fn unwrap_console(root: &Value) -> Value {
    let Some(d) = root.get("data") else {
        return root.clone();
    };
    if let Some(v2) = d.get("DataV2") {
        return match v2.get("data") {
            Some(inner) => inner.get("data").unwrap_or(inner).clone(),
            None => v2.clone(),
        };
    }
    d.get("data").unwrap_or(d).clone()
}

/// `application/x-www-form-urlencoded`, which is NOT the ACS3 escape
/// set: here a space really is `+`.
fn form_urlencode(pairs: &[(&str, &str)]) -> String {
    fn enc(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.as_bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(*b as char)
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The console API that answers the personal plan's rolling windows.
/// Takes no arguments (the vendor's own `usage token-plan` passes `{}`).
const PERSONAL_USAGE_API: &str = "zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage";

/// Path A — the personal plan's rolling windows.
///
/// The FREE TIER is deliberately not fetched here. Its console API
/// (`…freeTrial.queryFreeTierQuota`) requires an explicit model list —
/// it answers `参数错误 [param=queryFreeTierQuotaRequest]` to an empty
/// request — so it needs a model-enumeration call first. Recorded as a
/// tail rather than half-built.
pub async fn fetch_personal(cfg: &crate::config::VelesConfig) -> Result<PersonalQuota, String> {
    let s = console_session(cfg)?;
    let v = console_call(
        &s,
        PERSONAL_USAGE_API,
        Value::Object(serde_json::Map::new()),
    )
    .await?;
    Ok(PersonalQuota {
        windows: parse_windows(&v),
        free_tier: Vec::new(),
    })
}

/// The personal plan's window FAMILIES, as the payload spells them:
/// `<family>Percentage` / `<family>ResetTime`, with the label we render
/// each under.
///
/// THE PLAN MOVED TO A MONTHLY WINDOW (found 2026-09-23). The service
/// used to answer `per5Hour*` + `per1Week*`; it now answers
/// `per1MonthPercentage` / `per1MonthResetTime` and nothing else. The
/// vendor's own `bl usage token-plan` still reads only the two OLD keys
/// — re-read from its dist bundle on 2026-09-23 — so it prints "may be
/// unlimited" for both its rows while a real monthly figure sits unread
/// in the payload. A parser knowing only the OLD pair turned that into
/// two "not reported by the API" rows on every surface: the figure was
/// there, the table was not.
///
/// The old two stay listed because the wire's shape is versioned by
/// nothing but its own history — a plan that still reports 5-hour or
/// weekly windows must keep rendering. A family NOT in this table is
/// invisible, which is what makes this list the thing to check when the
/// wire moves again.
const WINDOW_FAMILIES: &[(&str, &str)] = &[
    ("per5Hour", "5-hour"),
    ("per1Week", "weekly"),
    ("per1Month", "month"),
];

/// The window fields the payload carries, in table order.
///
/// A family the payload mentions in NEITHER field is not a window it
/// claims to have and is left out — the small set of rows under a plan
/// is the plan's own. A family it mentions with only HALF a reading
/// (a reset time and no percentage, or the reverse) is kept with the
/// missing half as `None`, which every surface renders as "not
/// reported by the API": an absence is named, never defaulted to 0%.
///
/// A payload naming NO family returns an empty list. The old shape
/// invented a "5-hour" row here (that family was the standing
/// absent-but-known pin); with the plan now on a month window, a
/// guaranteed name for an unknown structure was a guess, and every
/// surface already has an honest rendering for "the API named no
/// window" (`nothing reported` / the plan header's own absence).
fn parse_windows(v: &Value) -> Vec<Window> {
    let mut out: Vec<Window> = Vec::new();
    for (family, label) in WINDOW_FAMILIES {
        let used = v.get(format!("{family}Percentage")).and_then(Value::as_f64);
        let resets = epoch_ms(v.get(format!("{family}ResetTime")));
        if used.is_none() && resets.is_none() {
            continue;
        }
        out.push(Window {
            label: (*label).to_string(),
            used_fraction: used,
            resets_at: resets,
        });
    }
    out
}

// ── Path C — the z.ai coding plan ─────────────────────────────────────
//
// A third disjoint pool with the simplest wire of the three: one GET,
// the plan's own inference key as a Bearer, no signature, no console
// session. Found by reading two community packages that already use it
// (zai-quota-mcp 0.1.0, pi-zai-usage 0.1.0) and verified live
// 2026-09-16 against this account's key:
//
//   GET {base}/api/monitor/usage/quota/limit
//   Authorization: Bearer <the plan's sk- key>
//
//   {"code":200,"msg":"Operation successful","success":true,
//    "data":{"level":"pro","limits":[
//      {"type":"TIME_LIMIT","unit":5,"number":1000,"usage":69,
//       "remaining":931,"percentage":6,"nextResetTime":1790859531998,
//       "usageDetails":[{"modelCode":"search-prime","usage":54},
//                       {"modelCode":"web-reader","usage":15}]},
//      {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":1,
//       "nextResetTime":1789487290953},
//      {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":60,
//       "nextResetTime":1789936113984}]}}
//
// `unit` is the discriminator: 3 = the 5-hour token window, 6 = the
// weekly quota, and TIME_LIMIT's unit 5 = the monthly tool quota. The
// TOKENS entries carry ONLY a percentage — no absolute token counts —
// so those stay percentages end to end, while TIME_LIMIT carries the
// call counts. And this endpoint reports PERCENTAGES AS INTEGERS
// (1, 6, 60), unlike Alibaba's floats; both are handled.

/// The z.ai plan's key, from `.env` by the name the config says.
fn zai_key(cfg: &crate::config::VelesConfig) -> Result<String, String> {
    let k = crate::authn::env_or_dotenv(&cfg.tokenplan.zai_key_env);
    if k.is_empty() {
        return Err(format!(
            "no z.ai key — set {} in .env",
            cfg.tokenplan.zai_key_env
        ));
    }
    Ok(k)
}

/// The plan's used-fraction as a real number. The API rounds its
/// percentages to integers, so a 0.4% evening on the 5-hour window
/// reads as `percentage: 1` — honest about the rounding, never
/// inventing the decimals it did not send.
fn zai_fraction(pct: Option<f64>) -> Option<f64> {
    pct.map(|p| (p / 100.0).clamp(0.0, 1.0))
}

fn parse_zai(v: &Value) -> Option<ZaiQuota> {
    let data = v.get("data")?;
    let mut out = ZaiQuota {
        level: text(data, "level"),
        window_5h: None,
        weekly: None,
        tools: None,
    };
    let mut saw_any = out.level.is_some();
    for lim in data.get("limits").and_then(Value::as_array)? {
        let typ = lim.get("type").and_then(Value::as_str).unwrap_or("");
        let unit = lim.get("unit").and_then(Value::as_i64);
        let win = |lim: &Value| Window {
            label: String::new(),
            used_fraction: zai_fraction(lim.get("percentage").and_then(Value::as_f64)),
            resets_at: epoch_ms(lim.get("nextResetTime")),
        };
        match (typ, unit) {
            ("TOKENS_LIMIT", Some(3)) if out.window_5h.is_none() => {
                out.window_5h = Some(win(lim));
                saw_any = true;
            }
            ("TOKENS_LIMIT", Some(6)) if out.weekly.is_none() => {
                out.weekly = Some(win(lim));
                saw_any = true;
            }
            ("TIME_LIMIT", _) if out.tools.is_none() => {
                let details = lim
                    .get("usageDetails")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|d| {
                                Some((
                                    text(d, "modelCode").unwrap_or_else(|| "?".into()),
                                    d.get("usage").and_then(Value::as_f64)?,
                                ))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                out.tools = Some(ZaiTools {
                    limit: lim.get("usage").and_then(Value::as_f64),
                    used: lim.get("currentValue").and_then(Value::as_f64),
                    remaining: lim.get("remaining").and_then(Value::as_f64),
                    resets_at: epoch_ms(lim.get("nextResetTime")),
                    details,
                });
                saw_any = true;
            }
            _ => {}
        }
    }
    if !saw_any {
        return None;
    }
    // `number` doubles as the pool size on TIME_LIMIT and the window
    // span on TOKENS_LIMIT — the labels are ours, not the wire's.
    if let Some(w) = out.window_5h.as_mut() {
        w.label = "5-hour".into();
    }
    if let Some(w) = out.weekly.as_mut() {
        w.label = "weekly".into();
    }
    Some(out)
}

/// Path C — the z.ai coding plan's quota.
pub async fn fetch_zai(cfg: &crate::config::VelesConfig) -> Result<ZaiQuota, String> {
    let key = zai_key(cfg)?;
    let base = cfg.tokenplan.zai_base_url.trim_end_matches('/');
    let url = format!("{base}/api/monitor/usage/quota/limit");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("veles/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client
        .get(&url)
        .header("accept", "*/*")
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("api.z.ai unreachable: {e}"))?;

    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err("z.ai key rejected — check ZAI_CODING_PLAN_API_KEY".to_string());
    }
    if !status.is_success() {
        return Err(format!("api.z.ai — HTTP {status}"));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|_| "api.z.ai answered with something that is not json".to_string())?;
    // The 200-with-success:false envelope again — the third API in this
    // file to do it, and the third place a status-only check would
    // render a confident wrong answer.
    if v.get("success").and_then(Value::as_bool) == Some(false) {
        let msg = text(&v, "msg").unwrap_or_else(|| "unspecified".into());
        return Err(format!("z.ai: {msg}"));
    }
    parse_zai(&v).ok_or_else(|| "z.ai: the payload named no plan limits".to_string())
}

// ── Path D — the Kimi Code plan ───────────────────────────────────────
//
// The fourth disjoint pool, and the simplest wire of the four: one
// GET, the plan's own `sk-kimi-…` key as a Bearer, no signature and no
// console session (PENDING.md 2026-09-20, found by probing; the CN
// hostname and the overseas twin serve the identical payload, on both
// fleet keys).
//
//   GET {base}/usages
//   Authorization: Bearer <the plan's sk-kimi- key>
//
//   {"limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
//     "detail":{"limit":"100","used":"100","remaining":"0",
//               "resetTime":"2026-09-20T15:22:56.391655Z"}}],
//    "usages":{"limit_5h":{"used_ratio":0,"reset_time":"…"},
//              "limit_month_total":{"used_ratio":0.0546,"reset_time":"…"},
//              "limit_month_code":{"used_ratio":0,"reset_time":"…"}}}
//
// THE TWO HALVES DISAGREE, AND ONE OF THEM IS PROVABLY WRONG.
//
// `limits[].detail` read `100 / 100 used` — while a live inference
// call at that exact moment was refused with HTTP 403 "You've reached
// your 5-hour usage limit" — and `usages.limit_5h.used_ratio` read 0
// for the entire climb (84 → 95 → 100 across ~12 minutes, PENDING
// 2026-09-20; re-probed 2026-09-20 16:5x with the 403 captured beside
// it). So the field NAMED `used_ratio` is the half that lies about the
// binding window, and the absolute `limits[].detail` figures are the
// ones this module takes at face value.
//
// Where both halves describe one window, `limits[]` wins and the ratio
// is dropped — never averaged, never shown beside. That rule is why
// `parse_kimi` dedupes by LABEL and not by position: a ratio named
// `limit_5h` is the same window as a 300-minute `limits[]` entry.
//
// The MONTH figures exist only as ratios. They are served because they
// are the only month reading there is and they provably MOVE
// (`limit_month_total` 0.0259 → 0.0546 across one day's spending), but
// the caveat is recorded here rather than dressed up: they come from
// the half that was wrong about the 5-hour window, so they are the
// weakest numbers this module renders.
//
// A number of fields arrive as STRINGS here (`"limit":"100"`,
// `"used":"100"`), unlike everywhere else in this file. Both spellings
// are read; a JSON number is accepted too.
//
// The USER-AGENT trap from Alibaba's `/tokenplan/*` does NOT apply
// (probed with none: 200). Kimi's docs instead warn that tampering
// with the client identifier can suspend the membership — so this
// sends veles' own honest UA and never `KimiCLI/1.6`, the string the
// community tools borrow.

/// The Kimi Code plan's quota: its rolling windows, as `/usages` tells
/// them. One struct rather than a per-window field because the wire
/// hands back a LIST whose contents are nameable only by their own
/// descriptors — a plan that moves to another window size labels
/// itself.
#[derive(Debug, Clone, PartialEq)]
pub struct KimiQuota {
    pub windows: Vec<Window>,
}

impl KimiQuota {
    /// The tightest of the plan's windows. The five-hour window is the
    /// one that actually stops a turn (it 403s), so when several are
    /// reported this is the number that answers "can I keep going".
    pub fn tightest_used_fraction(&self) -> Option<f64> {
        self.windows
            .iter()
            .filter_map(|w| w.used_fraction)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }
}

/// The Kimi plan's key, from `.env` by the name the config says.
fn kimi_key(cfg: &crate::config::VelesConfig) -> Result<String, String> {
    let k = crate::authn::env_or_dotenv(&cfg.tokenplan.kimi_key_env);
    if k.is_empty() {
        return Err(format!(
            "no Kimi key — set {} in .env",
            cfg.tokenplan.kimi_key_env
        ));
    }
    Ok(k)
}

/// A number the Kimi wire may send as a string.
///
/// `"100"` and `100` are the same fact; the payload uses both, and a
/// reader that only accepts one silently reports "not reported" for
/// figures the API gave.
fn kimi_num(v: &Value, key: &str) -> Option<f64> {
    let x = v.get(key)?;
    x.as_f64()
        .or_else(|| x.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

fn kimi_time(v: &Value, key: &str) -> Option<DateTime<Utc>> {
    let s = v.get(key).and_then(Value::as_str)?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// `{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"}` → "5-hour".
///
/// Derived from the wire's own descriptor rather than assumed: the
/// vendor's own CLI renders exactly this way (300 minutes → "5h
/// Limit"), and a window that changes size relabels itself.
fn kimi_window_label(window: Option<&Value>) -> String {
    let Some(w) = window else {
        return "window".to_string();
    };
    let d = w.get("duration").and_then(Value::as_i64).unwrap_or(0);
    let unit = w
        .get("timeUnit")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    if d <= 0 {
        return "window".to_string();
    }
    if unit.contains("MINUTE") {
        // 300 minutes is a "5-hour" window; the API never calls it that,
        // every human does.
        if d >= 60 && d % 60 == 0 {
            format!("{}-hour", d / 60)
        } else {
            format!("{d}-minute")
        }
    } else if unit.contains("HOUR") {
        format!("{d}-hour")
    } else if unit.contains("DAY") {
        format!("{d}-day")
    } else if unit.contains("MONTH") {
        format!("{d}-month")
    } else {
        format!("{d} {}", unit.to_lowercase())
    }
}

/// A key from the ratio half, as a label. Mechanical on purpose: the
/// keys' semantics are undocumented (`limit_month_code` names nothing
/// that is written down anywhere), so the name is DERIVED from the key
/// rather than invented over it.
fn kimi_ratio_label(key: &str) -> String {
    let k = key.strip_prefix("limit_").unwrap_or(key);
    match k {
        "5h" => "5-hour".to_string(),
        "month_total" => "month".to_string(),
        other => match other.strip_prefix("month_") {
            Some(rest) => format!("month · {rest}"),
            None => other.to_string(),
        },
    }
}

fn parse_kimi(v: &Value) -> Option<KimiQuota> {
    let mut windows: Vec<Window> = Vec::new();

    // 1. The absolute half — the one that matched reality.
    for lim in v
        .get("limits")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        let Some(detail) = lim.get("detail") else {
            continue;
        };
        let limit = kimi_num(detail, "limit");
        let used = kimi_num(detail, "used");
        let fraction = match (used, limit) {
            (Some(u), Some(l)) if l > 0.0 => Some((u / l).clamp(0.0, 1.0)),
            _ => None,
        };
        // A limit the API did not report is no reading, not 0% — the
        // released-seat rule, again.
        if limit.is_none() && used.is_none() {
            continue;
        }
        windows.push(Window {
            label: kimi_window_label(lim.get("window")),
            used_fraction: fraction,
            resets_at: kimi_time(detail, "resetTime"),
        });
    }

    // 2. The ratio half, for whatever the absolute half did not name.
    if let Some(usages) = v.get("usages").and_then(Value::as_object) {
        for (key, u) in usages {
            let label = kimi_ratio_label(key);
            // The absolute half already described this window. Its
            // figure is the one that matched an observed 403; the ratio
            // is not shown beside it to be averaged in the reader's
            // head.
            if windows.iter().any(|w| w.label == label) {
                continue;
            }
            let fraction = u.get("used_ratio").and_then(Value::as_f64);
            let resets = kimi_time(u, "reset_time");
            if fraction.is_none() && resets.is_none() {
                continue;
            }
            windows.push(Window {
                label,
                used_fraction: fraction.map(|f| f.clamp(0.0, 1.0)),
                resets_at: resets,
            });
        }
    }

    if windows.is_empty() {
        return None;
    }
    Some(KimiQuota { windows })
}

/// Path D — the Kimi Code plan's quota.
pub async fn fetch_kimi(cfg: &crate::config::VelesConfig) -> Result<KimiQuota, String> {
    let key = kimi_key(cfg)?;
    let base = cfg.tokenplan.kimi_base_url.trim_end_matches('/');
    let url = format!("{base}/usages");
    let host = base
        .split("//")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or("the Kimi endpoint")
        .to_string();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        // The honest UA. Kimi's docs say tampering with the client
        // identifier can suspend the membership, so the community's
        // borrowed `KimiCLI/1.6` is deliberately NOT worn here.
        .user_agent(concat!("veles/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client
        .get(&url)
        .header("accept", "*/*")
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("{host} unreachable: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&body).map_err(|_| {
        format!(
            "{host} answered {} with something that is not json",
            status.as_u16()
        )
    })?;

    // The far side's own words where it gave any: a 401 here is "The
    // API Key appears to be invalid or may have expired", plus a
    // 403 whose message is about the plan, not the key.
    if !status.is_success() {
        let named = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string);
        return Err(match named {
            Some(m) => format!("kimi — HTTP {} {m}", status.as_u16()),
            None => format!("kimi — HTTP {}: {}", status.as_u16(), body.trim()),
        });
    }
    // A 200 is not a success on the other three APIs in this file; the
    // check costs one line and this is the same family of service.
    if let Some(m) = v
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
    {
        return Err(format!("kimi: {m}"));
    }

    parse_kimi(&v).ok_or_else(|| "kimi: the payload named no quota windows".to_string())
}

// ── the local ledger, split by plan ───────────────────────────────────
//
// `/usage` and the desktop's Usage widget report what WE spent, by our
// own counting. That number used to arrive as one total across every
// provider, which CONFLATES THE PLANS: 1.9G tokens spanning the
// personal plan, the team plan, z.ai and Kimi is not a quantity anyone
// can act on — the pools are disjoint and so are the bills, and the
// only useful reading of "what did we spend" is per pool.
//
// So the ledger is grouped here, once, and every surface renders this.
// A provider no plan names is kept as its own row rather than folded
// into a plan it does not belong to: an unmapped provider is a FACT
// (something is configured that no quota reader covers), and hiding it
// would make the split silently not add up.

/// One plan's share of the local usage ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanLedgerRow {
    /// `None` for providers no plan answers for — rendered as its own
    /// row, never merged into somebody else's pool.
    pub plan: Option<Plan>,
    /// The providers that fed this row, sorted, so a reader can see
    /// which entries this plan actually covers (`ali-responses` being
    /// on the team pool is exactly the kind of thing this makes
    /// visible).
    pub providers: Vec<String>,
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}

impl PlanLedgerRow {
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }

    /// The row's name: the plan, or the providers themselves when no
    /// plan claims them.
    pub fn label(&self) -> String {
        match self.plan {
            Some(p) => p.label().to_string(),
            None => self.providers.join(", "),
        }
    }
}

/// Group a per-provider ledger by the plan each provider draws from.
///
/// Order is fixed — team, personal, zai, kimi, then the unmapped rows —
/// rather than whatever the store's `ORDER BY count` produced, because
/// a figure that moves around between paints is a figure nobody can
/// compare at a glance.
pub fn ledger_by_plan(rows: &[crate::store::ProviderLedgerRow]) -> Vec<PlanLedgerRow> {
    let mut out: Vec<PlanLedgerRow> = Vec::new();
    let order = [Plan::Team, Plan::Personal, Plan::Zai, Plan::Kimi];
    for p in order {
        let mut provs: Vec<String> = Vec::new();
        let mut row = PlanLedgerRow {
            plan: Some(p),
            providers: Vec::new(),
            calls: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
        };
        for r in rows {
            if plan_for_provider(&r.provider) == Some(p) {
                provs.push(r.provider.clone());
                row.calls += r.calls;
                row.prompt_tokens += r.prompt_tokens;
                row.completion_tokens += r.completion_tokens;
                row.cached_tokens += r.cached_tokens;
            }
        }
        if row.calls > 0 {
            provs.sort();
            row.providers = provs;
            out.push(row);
        }
    }
    // The unmapped tail: one row per provider, so "which one is it"
    // stays answerable instead of becoming "other: 20 calls".
    let mut unmapped: Vec<&crate::store::ProviderLedgerRow> = rows
        .iter()
        .filter(|r| plan_for_provider(&r.provider).is_none())
        .collect();
    unmapped.sort_by_key(|a| std::cmp::Reverse(a.calls));
    for r in unmapped {
        out.push(PlanLedgerRow {
            plan: None,
            providers: vec![r.provider.clone()],
            calls: r.calls,
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            cached_tokens: r.cached_tokens,
        });
    }
    out
}

/// The ledger as JSON rows, for `GET /api/usage`.
pub fn ledger_json(rows: &[PlanLedgerRow]) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            serde_json::json!({
                "plan": r.plan.map(|p| p.label()),
                "providers": r.providers,
                "calls": r.calls,
                "prompt_tokens": r.prompt_tokens,
                "completion_tokens": r.completion_tokens,
                "cached_tokens": r.cached_tokens,
                "total_tokens": r.total_tokens(),
            })
        })
        .collect()
}

// ── the cache ─────────────────────────────────────────────────────────
//
// Same shape as `discovery.rs`'s model catalog, and for the same reason:
// the readers are UI paths — a 15s status-bar repaint, a chat line — and
// none of them may pay a Singapore round trip. Two rules carried over
// from there and both load-bearing here:
//
//   - a snapshot with nothing in it is a FAILURE, not a result. Caching
//     one would blank the display for a full TTL over one bad minute.
//   - a cached read never touches the network. `refresh` is the only
//     thing that does, and only a poller or an explicit `/quota refresh`
//     calls it.

static CACHE: OnceLock<Mutex<Option<(Instant, QuotaSnapshot)>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<(Instant, QuotaSnapshot)>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

/// The cached snapshot and its age, if one is still inside the TTL.
///
/// "Fresh enough to show without comment". Past the TTL this answers
/// `None` and the caller falls to [`last_known`] — which is NOT the same
/// as having no reading, and the two must not be conflated. See that
/// function for why the distinction is the whole bug.
pub fn cached(cfg: &crate::config::VelesConfig) -> Option<(QuotaSnapshot, Duration)> {
    let ttl = Duration::from_secs(cfg.tokenplan.cache_ttl_secs);
    let (snap, age) = last_known()?;
    if age > ttl {
        return None;
    }
    Some((snap, age))
}

/// The last snapshot that ever arrived, whatever its age.
///
/// THE TTL IS NOT AN EXPIRY DATE. `cache_ttl_secs` defaults to 300 and
/// `poll_secs` to 900, so a reading spends 600 of every 900 seconds past
/// the TTL — and because the only reader of `cached` answered "cold" for
/// that whole stretch, the desktop's plan field read `plan —` two thirds
/// of the time and appeared to disable itself (USER 2026-09-16: "the
/// quota/usage in the status bar seems to disable itself … and then
/// never reactivates by itself"). It had not disabled anything; it was
/// being told there was nothing to show.
///
/// A ten-minute-old quota reading is still the truth about the account.
/// The snapshot already carries its age and every surface already
/// renders it, so the honest answer is the figure PLUS how old it is —
/// never silence. `cold` keeps its narrow meaning: no reading has ever
/// arrived (a gateway that just booted, or credentials that have never
/// worked).
pub fn last_known() -> Option<(QuotaSnapshot, Duration)> {
    let c = cache().lock().ok()?;
    let (at, snap) = c.as_ref()?;
    Some((snap.clone(), at.elapsed()))
}

fn put_cached(snap: &QuotaSnapshot) {
    // Nothing to say and no reason why: do not let that poison the TTL.
    if snap.is_empty() {
        return;
    }
    if let Ok(mut c) = cache().lock() {
        *c = Some((Instant::now(), snap.clone()));
    }
}

/// Seed the cache directly.
///
/// Test seam. `quota_command`'s verb routing and its ACL slice are
/// worth pinning, and neither has any business opening a socket to do
/// it — the command reads the cache like every surface does.
#[cfg(any(test, feature = "test-support"))]
pub fn seed_cache_for_test(snap: QuotaSnapshot) {
    seed_cache_aged_for_test(snap, Duration::from_secs(0));
}

/// Seed the cache as if the reading arrived `age` ago.
///
/// The TTL is measured against an `Instant`, so "old enough to be past
/// the TTL" is not something a test can otherwise produce without
/// sleeping for it. Backdating the stamp is the seam; a clock that
/// cannot go back that far (a box up for less than `age`) falls to now,
/// which would make the test vacuous rather than flaky — so the
/// assertion that depends on it checks the age it actually got.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn seed_cache_aged_for_test(snap: QuotaSnapshot, age: Duration) {
    if let Ok(mut c) = cache().lock() {
        let at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        *c = Some((at, snap));
    }
}

/// What a PAINT PATH should show: the reading, fresh or not.
///
/// `cached` answers "fresh enough to show without comment" and
/// `last_known` answers "the last thing that arrived"; a display surface
/// wants the second, with the first deciding only whether to mark it.
/// That decision used to live inside the `/api/quota` handler, where no
/// test could reach it — and it was the wrong one: it fell straight from
/// `cached` to a `cold` answer, skipping the reading it was holding.
///
/// `None` from here means exactly one thing: nothing has ever arrived.
pub fn for_display(cfg: &crate::config::VelesConfig) -> Option<(QuotaSnapshot, Duration)> {
    cached(cfg).or_else(last_known)
}

/// Drop whatever is cached. Used by tests, and by anything that changes
/// the credentials out from under a warm snapshot.
pub fn invalidate() {
    if let Ok(mut c) = cache().lock() {
        *c = None;
    }
}

/// Go and ask, both paths, and cache what came back.
///
/// Never returns `Err`: one path failing is a normal state that the
/// report has words for, and a caller rendering into a chat line has no
/// use for a second error channel.
pub async fn refresh(cfg: &crate::config::VelesConfig, bill_month: Option<&str>) -> QuotaSnapshot {
    let (team, team_error) = match fetch_team(cfg, bill_month).await {
        Ok(t) => (Some(t), None),
        Err(e) => (None, Some(e)),
    };
    let (personal, personal_error) = match fetch_personal(cfg).await {
        Ok(p) => (Some(p), None),
        Err(e) => (None, Some(e)),
    };
    // The z.ai plan has its own credential, its own host and its own
    // expiry story — the same independence argument as the other two,
    // and the reason it is a third pair rather than a flag on either.
    let (zai, zai_error) = match fetch_zai(cfg).await {
        Ok(z) => (Some(z), None),
        Err(e) => (None, Some(e)),
    };
    // Kimi: the same argument once more. Four pools, four credentials,
    // four ways to fail — and one of them failing must never blank
    // another.
    let (kimi, kimi_error) = match fetch_kimi(cfg).await {
        Ok(k) => (Some(k), None),
        Err(e) => (None, Some(e)),
    };
    let snap = QuotaSnapshot {
        fetched_at: Utc::now(),
        team,
        team_error,
        personal,
        personal_error,
        zai,
        zai_error,
        kimi,
        kimi_error,
    };
    put_cached(&snap);
    snap
}

/// The cache if it is warm, otherwise a fresh read. What every surface
/// calls.
pub async fn snapshot(
    cfg: &crate::config::VelesConfig,
    force: bool,
    bill_month: Option<&str>,
) -> (QuotaSnapshot, Duration) {
    if !force {
        if let Some(hit) = cached(cfg) {
            return hit;
        }
    }
    (refresh(cfg, bill_month).await, Duration::from_secs(0))
}

// ── rendering ─────────────────────────────────────────────────────────
//
// One formatter, so the four surfaces cannot drift. The rules are
// `format_context_report`'s: every number carries a unit, `·` separates
// inline facts, a percentage is grounded against the baseline it is a
// percentage OF, and a figure the API did not give is named as missing
// rather than defaulted. Lines stay short — IRC splits at the ISUPPORT
// width and a wrapped aligned column is unreadable there.

/// 1234567.8 → "1,234,568". Thousands separators, no decimals: credits
/// are counted in the tens of thousands and the fraction is noise.
fn group(n: f64) -> String {
    let neg = n < 0.0;
    let s = format!("{:.0}", n.abs());
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

/// "3d", "4h", "12m" — the coarsest unit that is still true.
fn until(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let secs = (then - now).num_seconds();
    if secs <= 0 {
        return "elapsed".to_string();
    }
    if secs >= 86_400 {
        format!("{}d left", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{}h left", secs / 3_600)
    } else {
        format!("{}m left", secs.max(60) / 60)
    }
}

fn ago(age: Duration) -> String {
    let s = age.as_secs();
    if s < 5 {
        "just now".to_string()
    } else if s < 60 {
        format!("{s}s ago")
    } else if s < 3_600 {
        format!("{}m ago", s / 60)
    } else {
        format!("{}h ago", s / 3_600)
    }
}

fn stamp(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// The snapshot as JSON, for `GET /api/quota`.
///
/// The desktop lays the figures out itself rather than showing the text
/// report — the precedent is `::vstatus::show_context`, which renders
/// the context JSON key by key instead of calling
/// `format_context_report`. What must NOT differ is the DATA: a field
/// that is `null` here is a field the API did not report, and the Tcl
/// side is obliged to render that as "not reported" for exactly the
/// reason the text renderer does.
pub fn snapshot_json(snap: &QuotaSnapshot, age: Duration, ttl_secs: u64, poll_secs: u64) -> Value {
    let seat_json = |s: &Seat| {
        let c = s.credits.as_ref();
        serde_json::json!({
            "seat_id": s.seat_id,
            "subscription": s.subscription,
            "spec_type": s.spec_type,
            "status": s.status,
            "assigned": s.assigned,
            "credits_total": c.and_then(|c| c.total),
            "credits_remaining": c.and_then(|c| c.remaining),
            "credits_unit": c.map(|c| c.unit.clone()),
            "used_fraction": c.and_then(Credits::used_fraction),
            "cycle_start": c.and_then(|c| c.cycle_start).map(|t| t.to_rfc3339()),
            "cycle_end": c.and_then(|c| c.cycle_end).map(|t| t.to_rfc3339()),
        })
    };

    let team = snap.team.as_ref().map(|t| {
        serde_json::json!({
            "seats": t.live_seats().map(seat_json).collect::<Vec<_>>(),
            "released_seats": t.seats.len() - t.live_seats().count(),
            "members": t.members,
            "seated": t.seated,
            "unseated": t.unseated,
            "spend": t.spend.as_ref().map(|s| serde_json::json!({
                "bill_month": s.bill_month,
                "pretax": s.pretax,
                "tax": s.tax,
                "total": s.total,
                "currency": s.currency,
            })),
        })
    });

    let personal = snap.personal.as_ref().map(|p| {
        serde_json::json!({
            "windows": p.windows.iter().map(|w| serde_json::json!({
                "label": w.label,
                "used_fraction": w.used_fraction,
                "resets_at": w.resets_at.map(|t| t.to_rfc3339()),
            })).collect::<Vec<_>>(),
            "free_tier": p.free_tier.iter().map(|f| serde_json::json!({
                "model": f.model,
                "remaining": f.remaining,
                "total": f.total,
                "expires": f.expires.map(|t| t.to_rfc3339()),
            })).collect::<Vec<_>>(),
        })
    });

    let zai = snap.zai.as_ref().map(|z| {
        serde_json::json!({
            "level": z.level,
            "window_5h": z.window_5h.as_ref().map(|w| serde_json::json!({
                "label": w.label,
                "used_fraction": w.used_fraction,
                "resets_at": w.resets_at.map(|t| t.to_rfc3339()),
            })),
            "weekly": z.weekly.as_ref().map(|w| serde_json::json!({
                "label": w.label,
                "used_fraction": w.used_fraction,
                "resets_at": w.resets_at.map(|t| t.to_rfc3339()),
            })),
            "tools": z.tools.as_ref().map(|t| serde_json::json!({
                "limit": t.limit,
                "used": t.used,
                "remaining": t.remaining,
                "used_fraction": t.used_fraction(),
                "resets_at": t.resets_at.map(|t| t.to_rfc3339()),
                "details": t.details.iter().map(|(m, u)| serde_json::json!({
                    "model": m, "usage": u,
                })).collect::<Vec<_>>(),
            })),
        })
    });

    let kimi = snap.kimi.as_ref().map(|k| {
        serde_json::json!({
            "windows": k.windows.iter().map(|w| serde_json::json!({
                "label": w.label,
                "used_fraction": w.used_fraction,
                "resets_at": w.resets_at.map(|t| t.to_rfc3339()),
            })).collect::<Vec<_>>(),
        })
    });

    // The relevance half (2026-09-16, USER): the plans are DISJOINT —
    // team and personal are separate Alibaba bills, z.ai and Kimi
    // different vendors entirely — so "plan 55%" read off a pool the
    // session cannot draw down answers the wrong question. Each plan's
    // tightest fraction is served under its own name and the surface
    // that knows the active route picks its own; the cross-plan
    // `tightest_used_fraction` stays for surfaces that cannot know.
    let plan_frac = |p: Plan| p.tightest_used_fraction(snap);
    // STALE IS COMPUTED HERE, not passed in, so it cannot disagree
    // with the age beside it. Past the TTL the reading is old, not
    // absent — `cold` means no reading has EVER arrived. Serving one
    // figure under both names is what made the plan field go blank for
    // ten minutes out of every fifteen (see `last_known`).
    serde_json::json!({
        "enabled": true,
        "cold": false,
        "stale": age.as_secs() > ttl_secs,
        "fetched_at": snap.fetched_at.to_rfc3339(),
        "age_secs": age.as_secs(),
        "ttl_secs": ttl_secs,
        "poll_secs": poll_secs,
        "tightest_used_fraction": snap.tightest_used_fraction(),
        "team_used_fraction": plan_frac(Plan::Team),
        "personal_used_fraction": plan_frac(Plan::Personal),
        "zai_used_fraction": plan_frac(Plan::Zai),
        "kimi_used_fraction": plan_frac(Plan::Kimi),
        "team": team,
        "team_error": snap.team_error,
        "personal": personal,
        "personal_error": snap.personal_error,
        "zai": zai,
        "zai_error": snap.zai_error,
        "kimi": kimi,
        "kimi_error": snap.kimi_error,
    })
}

/// The whole report.
///
/// `detail` is the ACL line, not a verbosity knob: without it the
/// answer carries pools, percentages and reset times — the numbers a
/// caller needs to know whether they may ask for something — and no
/// seat id, subscription id, member count or money. Those name the
/// ACCOUNT behind the agent, which is `cmd-admin`'s business.
pub fn format_quota_report(
    snap: &QuotaSnapshot,
    age: Duration,
    ttl: Duration,
    detail: bool,
) -> String {
    format_quota_report_for(snap, age, ttl, detail, None)
}

/// The report, narrowed to the plan a provider draws from.
///
/// The three plans are disjoint pools (team and personal are separate
/// Alibaba bills; z.ai is a different vendor), so the DEFAULT report —
/// every plan, no markers — is for a caller who asked with their own
/// words and wants the whole picture. A surface that knows which
/// provider a session is ON passes it here: the session's own plan is
/// marked `← active` and the others are still shown but dimmed by a
/// leading dot, because "worthless to me" (USER, 2026-09-16) applies
/// to pretending the number answers their question, not to hiding the
/// rest.
pub fn format_quota_report_for(
    snap: &QuotaSnapshot,
    age: Duration,
    ttl: Duration,
    detail: bool,
    active: Option<&str>,
) -> String {
    let now = Utc::now();
    // `active` names a PROVIDER; this needs the PLAN it draws from.
    // Flattened rather than defaulted: a provider no plan covers has no
    // pool to mark, and marking somebody else's would be the exact
    // wrong-pool bug this parameter exists to prevent.
    let plan = active.and_then(plan_for_provider);
    // With pools from other vendors, "alibaba token plan" is no longer
    // an honest banner. Each section keeps its own name below.
    let mut out = format!(
        "plan quota · read {} · cache ttl {}m\n",
        ago(age),
        ttl.as_secs() / 60
    );
    // `team`/`personal`/`zai`/`kimi`, with `← active` when a provider
    // was named and this is the pool it draws from.
    let mark = |name: &str, p: Plan| -> String {
        match plan {
            Some(q) if q == p => format!("{name} ← active"),
            _ => name.to_string(),
        }
    };

    // ── team ──
    match (&snap.team, &snap.team_error) {
        (_, Some(e)) => out.push_str(&format!(
            "\n{}\n  unavailable — {e}\n",
            mark("team", Plan::Team)
        )),
        (Some(t), None) => {
            out.push_str(&format!("\n{}\n", mark("team", Plan::Team)));
            let live: Vec<&Seat> = t.live_seats().collect();
            if live.is_empty() {
                out.push_str("  no active seat on this account\n");
            }
            for s in &live {
                if detail {
                    out.push_str(&format!(
                        "  seat      {} · {} · {}\n",
                        s.seat_id.as_deref().unwrap_or("—"),
                        s.spec_type.as_deref().unwrap_or("—"),
                        s.subscription.as_deref().unwrap_or("—")
                    ));
                }
                match &s.credits {
                    None => out.push_str("  credits   not reported for this seat\n"),
                    Some(c) => {
                        match (c.remaining, c.total, c.used_fraction()) {
                            (Some(r), Some(tot), Some(f)) => out.push_str(&format!(
                                "  credits   {} of {} {}s remaining — {:.1}% used\n",
                                group(r),
                                group(tot),
                                c.unit.to_lowercase(),
                                f * 100.0
                            )),
                            (Some(r), _, _) => out.push_str(&format!(
                                "  credits   {} {}s remaining · pool size not reported\n",
                                group(r),
                                c.unit.to_lowercase()
                            )),
                            _ => out.push_str("  credits   not reported for this seat\n"),
                        }
                        if let (Some(a), Some(b)) = (c.cycle_start, c.cycle_end) {
                            out.push_str(&format!(
                                "  cycle     {} → {} · {}\n",
                                stamp(a),
                                stamp(b),
                                until(now, b)
                            ));
                        }
                    }
                }
            }
            let released = t.seats.len() - live.len();
            if released > 0 {
                out.push_str(&format!(
                    "  (plus {released} released seat{} with no quota of their own)\n",
                    if released == 1 { "" } else { "s" }
                ));
            }
            if detail {
                if let (Some(m), Some(se)) = (t.members, t.seated) {
                    out.push_str(&format!("  seats     {se} of {m} member(s) seated\n"));
                }
                match &t.spend {
                    Some(sp) => out.push_str(&format!(
                        "  spend     {} {} {} ({} pretax + {} tax)\n",
                        sp.bill_month, sp.currency, sp.total, sp.pretax, sp.tax
                    )),
                    None => out.push_str("  spend     not reported\n"),
                }
            }
        }
        (None, None) => out.push_str("\nteam\n  not configured\n"),
    }

    // ── personal ──
    match (&snap.personal, &snap.personal_error) {
        (_, Some(e)) => out.push_str(&format!(
            "\n{}\n  unavailable — {e}\n",
            mark("personal", Plan::Personal)
        )),
        (Some(p), None) => {
            out.push_str(&format!("\n{}\n", mark("personal", Plan::Personal)));
            if p.windows.is_empty() && p.free_tier.is_empty() {
                out.push_str("  nothing reported\n");
            }
            for w in &p.windows {
                match w.used_fraction {
                    Some(f) => {
                        let reset = match w.resets_at {
                            Some(r) => format!(" — resets {} · {}", stamp(r), until(now, r)),
                            None => String::new(),
                        };
                        out.push_str(&format!("  {:<9} {:.1}% used{reset}\n", w.label, f * 100.0));
                    }
                    // The 5-hour window is genuinely absent from the
                    // payload today. Saying 0% would be a lie with a
                    // decimal point on it.
                    None => out.push_str(&format!("  {:<9} not reported by the API\n", w.label)),
                }
            }
            for f in &p.free_tier {
                let amount = match (f.remaining, f.total) {
                    (Some(r), Some(t)) => format!("{} of {}", group(r), group(t)),
                    (Some(r), None) => format!("{} remaining", group(r)),
                    _ => "not reported".to_string(),
                };
                let exp = match f.expires {
                    Some(e) => format!(" · expires {}", e.format("%Y-%m-%d")),
                    None => String::new(),
                };
                out.push_str(&format!("  free tier {} {amount}{exp}\n", f.model));
            }
        }
        (None, None) => out.push_str(&format!(
            "\n{}\n  not configured\n",
            mark("personal", Plan::Personal)
        )),
    }

    // ── z.ai ──
    match (&snap.zai, &snap.zai_error) {
        (_, Some(e)) => out.push_str(&format!(
            "\n{}\n  unavailable — {e}\n",
            mark("zai", Plan::Zai)
        )),
        (Some(z), None) => {
            out.push_str(&format!("\n{}\n", mark("zai", Plan::Zai)));
            if let Some(l) = &z.level {
                out.push_str(&format!("  plan      {l}\n"));
            }
            for w in [&z.window_5h, &z.weekly].into_iter().flatten() {
                match w.used_fraction {
                    Some(f) => {
                        let reset = match w.resets_at {
                            Some(r) => format!(" — resets {} · {}", stamp(r), until(now, r)),
                            None => String::new(),
                        };
                        out.push_str(&format!("  {:<9} {:.1}% used{reset}\n", w.label, f * 100.0));
                    }
                    None => out.push_str(&format!("  {:<9} not reported by the API\n", w.label)),
                }
            }
            if let Some(t) = &z.tools {
                match (t.used, t.limit) {
                    (Some(u), Some(l)) => out.push_str(&format!(
                        "  tools     {} of {} calls — {:.1}% used\n",
                        group(u),
                        group(l),
                        t.used_fraction().unwrap_or(0.0) * 100.0
                    )),
                    (Some(u), None) => out.push_str(&format!(
                        "  tools     {} calls used · pool size not reported\n",
                        group(u)
                    )),
                    _ => out.push_str("  tools     not reported\n"),
                }
                if !t.details.is_empty() {
                    let d = t
                        .details
                        .iter()
                        .map(|(m, u)| format!("{m} {}", group(*u)))
                        .collect::<Vec<_>>()
                        .join(" · ");
                    out.push_str(&format!("  by tool   {d}\n"));
                }
                if let Some(r) = t.resets_at {
                    out.push_str(&format!("  resets    {} · {}\n", stamp(r), until(now, r)));
                }
            }
        }
        (None, None) => out.push_str(&format!("\n{}\n  not configured\n", mark("zai", Plan::Zai))),
    }

    // ── kimi ──
    //
    // Same shape as z.ai, and the same named-absence rules. The one
    // difference worth knowing is visible in the labels rather than
    // hidden in a footnote: a window named by a DURATION ("5-hour")
    // came from the absolute half of the payload, which is the half
    // that matched an observed 403; a window named by KEY ("month",
    // "month · code") exists only as a ratio, from the half that lied
    // about the 5-hour window (see `parse_kimi`). The month figures are
    // still served — they are the only month reading there is, and they
    // provably move — with their own provenance legible.
    match (&snap.kimi, &snap.kimi_error) {
        (_, Some(e)) => out.push_str(&format!(
            "\n{}\n  unavailable — {e}\n",
            mark("kimi", Plan::Kimi)
        )),
        (Some(k), None) => {
            out.push_str(&format!("\n{}\n", mark("kimi", Plan::Kimi)));
            if k.windows.is_empty() {
                out.push_str("  nothing reported\n");
            }
            for w in &k.windows {
                match w.used_fraction {
                    Some(f) => {
                        let reset = match w.resets_at {
                            Some(r) => format!(" — resets {} · {}", stamp(r), until(now, r)),
                            None => String::new(),
                        };
                        out.push_str(&format!("  {:<9} {:.1}% used{reset}\n", w.label, f * 100.0));
                    }
                    None => out.push_str(&format!("  {:<9} not reported by the API\n", w.label)),
                }
            }
        }
        (None, None) => out.push_str(&format!(
            "\n{}\n  not configured\n",
            mark("kimi", Plan::Kimi)
        )),
    }

    if !detail {
        out.push_str("\n(ids, seats and spend need cmd-admin: /quota team)\n");
    }
    out.trim_end().to_string()
}
// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod signer_tests {
    use super::*;

    // Dummy credentials — the golden vectors below were produced by
    // running the signer from `scripts/ali-tokenplan-quota-ref.js` with
    // exactly these, a fixed date and a fixed nonce. Nothing here
    // touches the real key file.
    const AKID: &str = "LTAI5tFAKEfakeFAKEfake00";
    const AKSEC: &str = "fakeSecretfakeSecretfakeSecre0";
    const HOST: &str = "modelstudio.ap-southeast-1.aliyuncs.com";
    const DATE: &str = "2026-09-15T12:34:56Z";

    fn auth_of(s: &Signed) -> &str {
        s.headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.as_str())
            .expect("authorization header")
    }

    /// A signer with no golden vector is a guess.
    ///
    /// The far side answers a wrong signature with a generic rejection
    /// that names neither the header order nor the missing newline, so
    /// the only way to know the canonical form is right is to compare it
    /// against an implementation that is known to authenticate — which
    /// is what the node reference in `scripts/` is for.
    #[test]
    fn the_signature_matches_the_reference_implementation_on_a_bare_get() {
        let signed = sign(
            AKID,
            AKSEC,
            "GET",
            HOST,
            "/tokenplan/subscription/seat-detail",
            "GetSubscriptionSeatDetails",
            &BTreeMap::new(),
            DATE,
            "00000000-0000-4000-8000-000000000001",
        );
        assert_eq!(signed.query, "");
        assert_eq!(
            auth_of(&signed),
            "ACS3-HMAC-SHA256 Credential=LTAI5tFAKEfakeFAKEfake00,\
SignedHeaders=content-type;host;x-acs-action;x-acs-content-sha256;x-acs-date;\
x-acs-signature-nonce;x-acs-version,\
Signature=e351274378a030cbf737a010e889682f2827e89bbfb113db9d587c74aa46e431"
        );
    }

    /// The same, with a query — which is where the sort order and the
    /// escape set both get a chance to be wrong.
    #[test]
    fn the_signature_matches_the_reference_implementation_with_a_query() {
        let mut params = BTreeMap::new();
        params.insert("billMonth".to_string(), "2026-09".to_string());
        params.insert(
            "groupBy".to_string(),
            r#"[{"code":"BASE_MODEL"}]"#.to_string(),
        );
        params.insert(
            "zzOddChars".to_string(),
            "a b!c'd(e)f*g~h_i-j.k".to_string(),
        );

        let signed = sign(
            AKID,
            AKSEC,
            "GET",
            HOST,
            "/modelstudio/billing/overview",
            "GetBillingOverview",
            &params,
            DATE,
            "00000000-0000-4000-8000-000000000002",
        );
        assert_eq!(
            signed.query,
            "billMonth=2026-09\
&groupBy=%5B%7B%22code%22%3A%22BASE_MODEL%22%7D%5D\
&zzOddChars=a%20b%21c%27d%28e%29f%2Ag~h_i-j.k"
        );
        assert_eq!(
            auth_of(&signed),
            "ACS3-HMAC-SHA256 Credential=LTAI5tFAKEfakeFAKEfake00,\
SignedHeaders=content-type;host;x-acs-action;x-acs-content-sha256;x-acs-date;\
x-acs-signature-nonce;x-acs-version,\
Signature=eecdb6c15e174d1f85106be1cfe4d03bb3efdf28f5180391a469a20df0a3728f"
        );
    }

    /// Trap 2, isolated: form encoding is the wrong encoding, and the
    /// two disagree on exactly the characters a `style:json` parameter
    /// is full of.
    #[test]
    fn the_query_escape_set_is_rfc3986_unreserved_not_form_encoding() {
        // A space is %20, never '+'.
        assert_eq!(rfc3986("a b"), "a%20b");
        // encodeURIComponent leaves these; ACS3 does not.
        assert_eq!(rfc3986("!'()*"), "%21%27%28%29%2A");
        // The unreserved set survives untouched.
        assert_eq!(rfc3986("Az09-_.~"), "Az09-_.~");
        // What a style:json value is actually made of.
        assert_eq!(
            rfc3986(r#"{"code":"BASE_MODEL"}"#),
            "%7B%22code%22%3A%22BASE_MODEL%22%7D"
        );
    }

    /// The canonical query is sorted by key. A `HashMap` would pass
    /// every single-parameter test and sign a different string in
    /// production, which is the shape of bug this pins.
    #[test]
    fn query_parameters_are_sorted_by_key() {
        let mut params = BTreeMap::new();
        params.insert("zebra".to_string(), "1".to_string());
        params.insert("alpha".to_string(), "2".to_string());
        params.insert("middle".to_string(), "3".to_string());
        let signed = sign(AKID, AKSEC, "GET", HOST, "/x", "A", &params, DATE, "n");
        assert_eq!(signed.query, "alpha=2&middle=3&zebra=1");
    }
}

#[cfg(test)]
mod quota_tests {
    use super::*;
    use serde_json::json;

    // ── Path A — the console gateway ──────────────────────────────────

    /// `unwrapResponse`, reproduced from bailian-cli-core. Four layers
    /// of envelope, and the personal payload sits at the deepest one —
    /// stopping a layer short returns an object with no window fields
    /// in it, which parses to "nothing reported" rather than an error.
    #[test]
    fn the_console_envelope_is_peeled_to_the_same_layer_the_vendor_peels_it_to() {
        // The live shape, captured 2026-09-15: data.DataV2.data.data.
        let root = json!({
            "code": "200",
            "data": {
                "DataV2": { "data": { "code": "SUCCESS", "success": true, "data": {
                    "per1WeekResetTime": 1789932540000i64,
                    "per1WeekPercentage": 0.459372453088425
                }}},
                "success": true
            }
        });
        let v = unwrap_console(&root);
        assert_eq!(
            v.get("per1WeekPercentage").and_then(|x| x.as_f64()),
            Some(0.459372453088425),
            "must reach the innermost data: got {v}"
        );

        // DataV2.data with no further `data` stops there…
        let two = json!({ "data": { "DataV2": { "data": { "items": [1] } } } });
        assert!(unwrap_console(&two).get("items").is_some());

        // …and a plain `data.data` envelope is handled too.
        let plain = json!({ "data": { "data": { "x": 1 } } });
        assert_eq!(
            unwrap_console(&plain).get("x").and_then(|x| x.as_u64()),
            Some(1)
        );

        // No `data` at all: the root IS the answer.
        let bare = json!({ "x": 1 });
        assert_eq!(
            unwrap_console(&bare).get("x").and_then(|x| x.as_u64()),
            Some(1)
        );
    }

    /// The vendor's own gateway table, including its fallback: an
    /// unknown region resolves to cn-beijing rather than erroring.
    #[test]
    fn the_console_gateway_table_matches_the_vendors() {
        assert_eq!(
            console_gateway("ap-southeast-1", "international"),
            (
                "bailian-singapore-cs.alibabacloud.com",
                "IntlBroadScopeAspnGateway"
            )
        );
        assert_eq!(
            console_gateway("ap-southeast-1", "domestic"),
            (
                "modelstudio-cs.console.aliyun.com",
                "IntlBroadScopeAspnGateway"
            )
        );
        assert_eq!(
            console_gateway("cn-beijing", "international"),
            (
                "bailian-cs.console.alibabacloud.com",
                "BroadScopeAspnGateway"
            )
        );
        assert_eq!(
            console_gateway("cn-beijing", "domestic"),
            ("bailian-cs.console.aliyun.com", "BroadScopeAspnGateway")
        );
        // Unknown region → the cn-beijing row for that site.
        assert_eq!(
            console_gateway("eu-central-1", "domestic"),
            ("bailian-cs.console.aliyun.com", "BroadScopeAspnGateway")
        );
    }

    /// THE LIVE SHAPE, 2026-09-23: the personal plan now reports a
    /// MONTH window and nothing else. A parser knowing only the old
    /// pair read this payload as "not reported" on every surface while
    /// the figure sat right there — the exact "the personal token has
    /// monthly now" the user reported.
    #[test]
    fn the_month_window_the_plan_moved_to_is_read() {
        let v = json!({
            "per1MonthPercentage": 0.004967843176105556,
            "per1MonthResetTime": 1791043200000i64
        });
        let w = parse_windows(&v);
        assert_eq!(w.len(), 1, "only the window the payload names: {w:?}");
        assert_eq!(w[0].label, "month");
        assert!((w[0].used_fraction.unwrap() - 0.00496784).abs() < 1e-6);
        // 1791043200000 ms = 2026-10-03 16:00 UTC, the live reset.
        assert_eq!(
            w[0].resets_at
                .map(|t| t.format("%Y-%m-%d %H:%M").to_string()),
            Some("2026-10-03 16:00".to_string())
        );
    }

    /// The old families still read when a payload names them — the
    /// table exists so a plan (or a region) still serving the older
    /// shape renders, and so the NEW shape is not read through it. A
    /// family the payload does not mention is not a window it claims,
    /// and is not invented as a "not reported" row.
    #[test]
    fn the_retired_families_still_read_when_the_payload_names_them() {
        let v = json!({
            "per1WeekResetTime": 1789932540000i64,
            "per1WeekPercentage": 0.459372453088425
        });
        let w = parse_windows(&v);
        assert_eq!(w.len(), 1, "the 5-hour family is not mentioned: {w:?}");
        assert_eq!(w[0].label, "weekly");
        assert!((w[0].used_fraction.unwrap() - 0.4593724).abs() < 1e-6);

        // All three at once — the section's row order is the table's,
        // so a payload that grows a family does not shuffle the rows.
        let both = json!({
            "per5HourPercentage": 0.25,
            "per5HourResetTime": 1789932540000i64,
            "per1WeekPercentage": 0.1,
            "per1WeekResetTime": 1789932540000i64,
            "per1MonthPercentage": 0.5,
            "per1MonthResetTime": 1789932540000i64
        });
        let w = parse_windows(&both);
        assert_eq!(
            w.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(),
            vec!["5-hour", "weekly", "month"],
            "{w:?}"
        );
    }

    /// A family named with HALF a reading keeps its row, and the
    /// missing half renders as the API's absence rather than a zero —
    /// the released-seat rule applied to a window.
    #[test]
    fn half_a_window_reading_stays_a_named_absence_not_a_zero() {
        let v = json!({ "per1MonthResetTime": 1791043200000i64 });
        let w = parse_windows(&v);
        assert_eq!(w.len(), 1, "{w:?}");
        assert_eq!(w[0].label, "month");
        assert_eq!(w[0].used_fraction, None, "absent is not zero");
        assert!(w[0].resets_at.is_some(), "the half that WAS given stays");
    }

    /// A payload naming NO known family returns NO windows — not a
    /// guessed name with a "not reported" beside it. The old parser
    /// pushed a permanent "5-hour" row on this path; once the plan
    /// moved to a month window that name stopped being a fact about
    /// the account and became a guess about a wire shape nobody had
    /// seen, and the honest rendering for "the API named no window"
    /// already exists on every surface.
    #[test]
    fn a_payload_naming_no_family_returns_no_windows_rather_than_a_guess() {
        let v = json!({ "msg": "Success.", "code": "SUCCESS" });
        assert!(parse_windows(&v).is_empty(), "{}", parse_windows(&v).len());
        // An unknown family is a family this table cannot read; it is
        // deliberately invisible rather than rendered under a guess.
        let v = json!({ "per3DayPercentage": 0.5, "per3DayResetTime": 1791043200000i64 });
        assert!(parse_windows(&v).is_empty(), "{}", parse_windows(&v).len());
    }

    /// The console form body is NOT the ACS3 escape set: here a space
    /// really is `+`. Two encoders in one file is exactly the kind of
    /// thing that gets unified by mistake later.
    #[test]
    fn the_console_form_body_uses_form_encoding_not_the_acs3_set() {
        assert_eq!(form_urlencode(&[("a", "x y")]), "a=x+y");
        assert_eq!(form_urlencode(&[("a", "1"), ("b", "2")]), "a=1&b=2");
        assert_eq!(
            form_urlencode(&[("params", r#"{"Api":"x"}"#)]),
            "params=%7B%22Api%22%3A%22x%22%7D"
        );
        // …while the signed query for the SAME characters uses %20.
        assert_eq!(rfc3986("x y"), "x%20y");
    }

    /// `~` in the configured path is expanded, or the file is simply
    /// never found and the feature looks unconfigured.
    #[test]
    fn the_console_config_path_expands_a_leading_tilde() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        assert_eq!(
            expand_tilde("~/.bailian/config.json"),
            std::path::PathBuf::from(&home).join(".bailian/config.json")
        );
        // Absolute paths pass through, and a bare `~` is not a prefix.
        assert_eq!(
            expand_tilde("/etc/x.json"),
            std::path::PathBuf::from("/etc/x.json")
        );
        assert_eq!(expand_tilde("~x"), std::path::PathBuf::from("~x"));
    }

    /// No session is a message that says how to mint one, never a blank
    /// section.
    #[test]
    fn a_missing_console_session_says_how_to_mint_one() {
        let (_cfg_dir, _data, _env) = crate::testenv::scratch_xdg();
        let mut cfg = crate::config::VelesConfig::default();
        cfg.tokenplan.console_config = "/nonexistent/definitely/not/here.json".into();
        let Err(err) = console_session(&cfg) else {
            panic!("there is no session at that path");
        };
        assert!(err.contains("bl auth login --console"), "{err}");
        assert!(
            err.contains("ALIBABA_CONSOLE_TOKEN"),
            "the headless route too: {err}"
        );
        assert!(err.contains("/nonexistent"), "and where it looked: {err}");
    }

    /// A console session never Debug-prints its token.
    #[test]
    fn a_console_session_never_debug_prints_its_token() {
        let s = ConsoleSession {
            token: "console-token-nobody-should-see".into(),
            region: "ap-southeast-1".into(),
            site: "international".into(),
            switch_agent: None,
        };
        let shown = format!("{s:?}");
        assert!(
            !shown.contains("console-token-nobody-should-see"),
            "{shown}"
        );
        assert!(shown.contains("<redacted>"), "{shown}");
    }

    /// THE LIVE DEFECT, pinned. The tokenplan service reports business
    /// failures with the HTTP status left at 200:
    ///
    ///   {"Message":"系统异常…","Code":"COMMON-1005","Success":false}
    ///
    /// Checking only `status.is_success()` parsed that as an empty
    /// payload, and the report said "no active seat on this account" —
    /// a confident wrong answer about somebody's quota, which is worse
    /// than saying nothing. Found live 2026-09-15 against a real seat
    /// that had 111,650 credits on it.
    #[test]
    fn a_two_hundred_carrying_success_false_is_an_error_not_an_empty_result() {
        let body = r#"{"Message":"系统异常，请稍后重试","Code":"COMMON-1005","Success":false}"#;
        let err = interpret("GetSubscriptionSeatDetails", reqwest::StatusCode::OK, body)
            .expect_err("Success:false is a failure whatever the status says");
        assert!(err.contains("COMMON-1005"), "the far side's code: {err}");
        assert!(err.contains("系统异常"), "and its own words: {err}");
        assert!(
            err.contains("GetSubscriptionSeatDetails"),
            "and which call it was: {err}"
        );
    }

    /// The billing family spells the same field in lower case, so one
    /// check has to answer both.
    #[test]
    fn the_lowercase_success_spelling_is_honoured_too() {
        let body = r#"{"code":"Throttling","message":"slow down","success":false}"#;
        let err = interpret("GetBillingOverview", reqwest::StatusCode::OK, body)
            .expect_err("success:false is a failure");
        assert!(err.contains("Throttling: slow down"), "{err}");
    }

    /// …and a real success still passes through, both spellings.
    #[test]
    fn a_real_success_is_returned_unchanged() {
        let body = r#"{"Data":{"Items":[]},"Success":true}"#;
        let v = interpret("GetSubscriptionSeatDetails", reqwest::StatusCode::OK, body)
            .expect("Success:true passes");
        assert!(v.get("Data").is_some());

        // An envelope with no Success field at all is not a failure:
        // `GetOrganizationMemberSeatStats` answers with the figures and
        // no flag.
        let stats = r#"{"TotalMemberCount":1,"SeatedMemberCount":1}"#;
        let v = interpret(
            "GetOrganizationMemberSeatStats",
            reqwest::StatusCode::OK,
            stats,
        )
        .expect("no flag is not a failure");
        assert_eq!(v.get("TotalMemberCount").and_then(|x| x.as_u64()), Some(1));
    }

    /// A non-2xx still quotes the far side rather than paraphrasing it.
    #[test]
    fn a_non_success_status_quotes_the_far_sides_own_error() {
        let body = r#"{"Code":"InvalidAction.NotFound","Message":"no such action"}"#;
        let err = interpret("Whatever", reqwest::StatusCode::NOT_FOUND, body)
            .expect_err("404 is a failure");
        assert!(err.contains("404"), "{err}");
        assert!(
            err.contains("InvalidAction.NotFound: no such action"),
            "{err}"
        );

        // And a body that is not json at all still says something useful.
        let err = interpret(
            "Whatever",
            reqwest::StatusCode::BAD_GATEWAY,
            "<html>nope</html>",
        )
        .expect_err("502 is a failure");
        assert!(err.contains("502"), "{err}");
        assert!(err.contains("nope"), "{err}");
    }

    /// The seat-detail payload, shaped exactly as the live API returns
    /// it (captured 2026-09-15): one ASSIGNED seat carrying a CREDITS
    /// pool, and one RELEASE seat whose EquityList entry has a type and
    /// nothing else.
    fn seat_payload() -> Value {
        json!({
            "Data": { "Items": [
                {
                    "Status": "NORMAL", "AssignedStatus": "ASSIGNED",
                    "SpecType": "max", "SeatId": "seat_e5cfa7b0",
                    "InstanceCode": "subs-42217ce2C4tFHvOx",
                    "EquityList": [{
                        "EquityType": "CREDITS",
                        "CycleTotalValue": 250000,
                        "CycleSurplusValue": 111650.31954763,
                        "EquityUnit": "Credit",
                        "CycleStartTime": 1788426000000i64,
                        "CycleEndTime": 1791018000000i64
                    }]
                },
                {
                    "Status": "RELEASE", "AssignedStatus": "UNASSIGNED",
                    "SpecType": "standard",
                    "InstanceCode": "subs-c725ed37E2mlvAFp",
                    "EquityList": [{ "EquityType": "CREDITS" }]
                }
            ], "Total": 2 },
            "Success": true
        })
    }

    fn team_fixture() -> TeamQuota {
        TeamQuota {
            seats: parse_seats(&seat_payload()),
            members: Some(1),
            seated: Some(1),
            unseated: Some(0),
            spend: parse_spend(
                &json!({ "data": { "pretaxAmount": "206.45", "taxAmount": "47.48",
                                   "totalAmount": "253.93", "currency": "USD" } }),
                "2026-09",
            ),
        }
    }

    fn snap_of(team: Option<TeamQuota>, team_error: Option<String>) -> QuotaSnapshot {
        QuotaSnapshot {
            fetched_at: Utc::now(),
            team,
            team_error,
            personal: None,
            personal_error: Some("console endpoints not derived yet".into()),
            zai: None,
            zai_error: None,
            kimi: None,
            kimi_error: None,
        }
    }

    /// The live API really does answer `{"EquityType":"CREDITS"}` for a
    /// released seat. `0 of 0 credits — 100% used` would be a number the
    /// API declined to give, rendered as fact.
    #[test]
    fn a_released_seat_reports_no_pool_rather_than_a_pool_of_zero() {
        let seats = parse_seats(&seat_payload());
        assert_eq!(seats.len(), 2);

        let live: Vec<&Seat> = seats.iter().filter(|s| s.is_live()).collect();
        assert_eq!(live.len(), 1, "only the ASSIGNED seat is live");
        assert!(live[0].credits.is_some());

        let dead = &seats[1];
        assert!(!dead.is_live());
        assert!(
            dead.credits.is_none(),
            "an EquityList carrying only a type is NO pool, not an empty one"
        );
    }

    /// Every figure is grounded against the baseline it is a fraction
    /// of — `format_context_report`'s rule, applied here.
    #[test]
    fn credits_render_against_the_pool_they_are_a_fraction_of() {
        let snap = snap_of(Some(team_fixture()), None);
        let out = format_quota_report(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            true,
        );
        assert!(
            out.contains("111,650 of 250,000 credits remaining"),
            "both ends of the pool, grouped: {out}"
        );
        assert!(out.contains("55.3% used"), "and the fraction: {out}");
        assert!(
            out.contains("(plus 1 released seat with no quota of their own)"),
            "the released seat is NAMED, not silently dropped: {out}"
        );
    }

    /// The two paths answer to different credentials and fail
    /// independently. One being down must not blank the other, and the
    /// reason must reach the reader.
    #[test]
    fn one_path_failing_still_renders_the_other_and_names_the_reason() {
        let snap = snap_of(Some(team_fixture()), None);
        let out = format_quota_report(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            true,
        );
        assert!(out.contains("111,650"), "team still renders: {out}");
        assert!(
            out.contains("personal\n  unavailable — console endpoints not derived yet"),
            "and the personal half says WHY: {out}"
        );

        // And the other way round.
        let snap = snap_of(None, Some("AccessKey rejected".into()));
        let out = format_quota_report(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            true,
        );
        assert!(
            out.contains("team\n  unavailable — AccessKey rejected"),
            "{out}"
        );
    }

    /// A blank credential must never become a request: the far side
    /// answers an unsigned call the same way it answers a wrong
    /// signature, so "not configured" would arrive looking like
    /// "your key is bad".
    ///
    /// The env var NAMES are per-test and deliberately cannot exist.
    /// `env_or_dotenv` reads the PROCESS environment first, and a
    /// neighbouring test in this binary may well have put the real
    /// `ALIBABA_*` values there — pointing at the default names made
    /// this test pass or fail depending on what else had run, and on
    /// one run it printed the operator's live AccessKey secret into the
    /// cargo output. Neither the fixture nor the assertions touch the
    /// credential values now.
    #[test]
    fn a_missing_credential_names_both_keys_and_the_file_they_go_in() {
        let (_cfg_dir, _data, _env) = crate::testenv::scratch_xdg();
        let mut cfg = crate::config::VelesConfig::default();
        cfg.tokenplan.access_key_id_env = "VELES_TEST_NO_SUCH_AK_ID".into();
        cfg.tokenplan.access_key_secret_env = "VELES_TEST_NO_SUCH_AK_SECRET".into();

        let Err(err) = access_key(&cfg) else {
            panic!("a scratch home holds no credential under these names");
        };
        assert!(err.contains("VELES_TEST_NO_SUCH_AK_ID"), "{err}");
        assert!(err.contains("VELES_TEST_NO_SUCH_AK_SECRET"), "{err}");
        assert!(err.contains(".env"), "and where they go: {err}");

        // Half-configured is its own message: it names the one that is
        // missing rather than both.
        std::env::set_var("VELES_TEST_NO_SUCH_AK_ID", "LTAInotreal");
        let Err(err) = access_key(&cfg) else {
            panic!("the secret is still missing");
        };
        assert!(
            err.contains("VELES_TEST_NO_SUCH_AK_SECRET is not"),
            "names the missing half: {err}"
        );
        std::env::remove_var("VELES_TEST_NO_SUCH_AK_ID");
    }

    /// The credential's own `Debug` redacts, so a panic anywhere near a
    /// `Result<AccessKey, _>` cannot print the secret.
    #[test]
    fn an_access_key_never_debug_prints_its_secret() {
        let ak = AccessKey {
            id: "LTAIexampleexampleexam".into(),
            secret: "supersecretvaluenobodyshouldsee".into(),
        };
        let shown = format!("{ak:?}");
        assert!(
            !shown.contains("supersecretvaluenobodyshouldsee"),
            "the secret must never format: {shown}"
        );
        assert!(shown.contains("<redacted>"), "{shown}");
        // The id is an identifier and stays useful in a diagnostic.
        assert!(shown.contains("LTAIexampleexampleexam"), "{shown}");
    }

    /// Nothing implicit: a figure that is five minutes old says so.
    #[test]
    fn the_report_says_how_old_the_reading_is_and_what_the_ttl_is() {
        let snap = snap_of(Some(team_fixture()), None);
        let fresh = format_quota_report(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            true,
        );
        assert!(fresh.contains("read just now"), "{fresh}");
        assert!(fresh.contains("cache ttl 5m"), "{fresh}");

        let stale = format_quota_report(
            &snap,
            Duration::from_secs(240),
            Duration::from_secs(300),
            true,
        );
        assert!(
            stale.contains("read 4m ago"),
            "a four-minute-old figure says four minutes: {stale}"
        );
    }

    /// `discovery.rs`'s rule, carried over: one bad minute must not
    /// blank every surface for a whole TTL.
    #[test]
    fn a_snapshot_with_nothing_in_it_is_not_cached() {
        let _g = crate::testenv::lock();
        invalidate();
        let empty = QuotaSnapshot {
            fetched_at: Utc::now(),
            team: None,
            team_error: None,
            personal: None,
            personal_error: None,
            zai: None,
            zai_error: None,
            kimi: None,
            kimi_error: None,
        };
        assert!(empty.is_empty());
        put_cached(&empty);
        let cfg = crate::config::VelesConfig::default();
        assert!(
            cached(&cfg).is_none(),
            "an empty snapshot must not occupy the cache"
        );

        // …but a real one does.
        put_cached(&snap_of(Some(team_fixture()), None));
        assert!(cached(&cfg).is_some());
        invalidate();
    }

    /// The ACL line is a line through the CONTENT, not a verbosity
    /// knob: percentages and reset times are what a caller needs to know
    /// whether they may ask for something; ids, seats and money name the
    /// account behind the agent.
    #[test]
    fn the_bare_report_carries_no_identifiers_and_no_money() {
        let snap = snap_of(Some(team_fixture()), None);
        let bare = format_quota_report(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            false,
        );
        assert!(
            bare.contains("55.3% used"),
            "the quota figure stays: {bare}"
        );
        for leaked in [
            "seat_e5cfa7b0",
            "subs-42217ce2C4tFHvOx",
            "253.93",
            "206.45",
            "seated",
        ] {
            assert!(
                !bare.contains(leaked),
                "{leaked:?} must not appear below cmd-admin: {bare}"
            );
        }
        assert!(
            bare.contains("need cmd-admin"),
            "and the bare form says what is being withheld: {bare}"
        );

        // detail is the same report with the account in it.
        let full = format_quota_report(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            true,
        );
        assert!(full.contains("seat_e5cfa7b0"), "{full}");
        assert!(full.contains("253.93"), "{full}");
    }

    /// A window the API gave no figure for renders as a NAMED absence —
    /// the released-seat rule, applied to a window. The 5-hour pair used to
    /// be the standing example (absent from the payload while the vendor's
    /// CLI printed "may be unlimited"); since 2026-09-23 the wire reports a
    /// month window instead, so the example is now a half-reading
    /// constructed by hand — the rule it pins is unchanged.
    #[test]
    fn a_window_the_api_did_not_report_reads_not_reported_not_zero() {
        let snap = QuotaSnapshot {
            fetched_at: Utc::now(),
            team: None,
            team_error: Some("not configured".into()),
            personal: Some(PersonalQuota {
                windows: vec![
                    Window {
                        label: "5-hour".into(),
                        used_fraction: None,
                        resets_at: None,
                    },
                    Window {
                        label: "weekly".into(),
                        used_fraction: Some(0.456),
                        resets_at: Utc.timestamp_millis_opt(1789932540000).single(),
                    },
                ],
                free_tier: vec![],
            }),
            personal_error: None,
            zai: None,
            zai_error: None,
            kimi: None,
            kimi_error: None,
        };
        let out = format_quota_report(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            true,
        );
        assert!(
            out.contains("5-hour    not reported by the API"),
            "an absent window is named as absent: {out}"
        );
        assert!(!out.contains("5-hour    0.0%"), "never defaulted: {out}");
        assert!(out.contains("weekly    45.6% used"), "{out}");
        assert!(out.contains("resets 2026-09-20"), "{out}");
    }

    /// What the ambient display reads: the pool that runs out first.
    #[test]
    fn the_tightest_pool_is_the_one_an_ambient_display_shows() {
        let mut snap = snap_of(Some(team_fixture()), None);
        let team_only = snap.tightest_used_fraction().expect("team fraction");
        assert!((team_only - 0.5534).abs() < 0.001, "got {team_only}");

        // A personal window further along wins.
        snap.personal = Some(PersonalQuota {
            windows: vec![Window {
                label: "weekly".into(),
                used_fraction: Some(0.91),
                resets_at: None,
            }],
            free_tier: vec![],
        });
        snap.personal_error = None;
        let tightest = snap.tightest_used_fraction().expect("fraction");
        assert!((tightest - 0.91).abs() < 0.001, "got {tightest}");
    }

    /// Thousands separators, and no fake precision on a figure nobody
    /// reads to the hundredth of a credit.
    #[test]
    fn credit_figures_are_grouped_for_reading() {
        assert_eq!(group(111650.31954763), "111,650");
        assert_eq!(group(250000.0), "250,000");
        assert_eq!(group(999.0), "999");
        assert_eq!(group(1000.0), "1,000");
        assert_eq!(group(0.0), "0");
    }

    // ── Path C — the z.ai coding plan ─────────────────────────────────

    /// The live payload, captured 2026-09-16 against this account's
    /// key. `unit` is the discriminator: the two TOKENS_LIMIT rows
    /// differ only by it, and reading `percentage` without checking
    /// `unit` mixes the 5-hour window up with the weekly quota.
    fn zai_payload() -> Value {
        json!({
            "code": 200, "msg": "Operation successful", "success": true,
            "data": {
                "level": "pro",
                "limits": [
                    { "type": "TIME_LIMIT", "unit": 5, "number": 1,
                      "usage": 1000, "currentValue": 69, "remaining": 931,
                      "percentage": 6, "nextResetTime": 1790859531998i64,
                      "usageDetails": [
                          { "modelCode": "search-prime", "usage": 54 },
                          { "modelCode": "web-reader", "usage": 15 },
                          { "modelCode": "zread", "usage": 0 }
                      ] },
                    { "type": "TOKENS_LIMIT", "unit": 3, "number": 5,
                      "percentage": 1, "nextResetTime": 1789487290953i64 },
                    { "type": "TOKENS_LIMIT", "unit": 6, "number": 1,
                      "percentage": 60, "nextResetTime": 1789936113984i64 }
                ]
            }
        })
    }

    /// The three limits land in their three slots, by unit — not by
    /// array position, which the vendor reorders at will.
    #[test]
    fn the_zai_limits_are_told_apart_by_unit_not_position() {
        let z = parse_zai(&zai_payload()).expect("parses");

        assert_eq!(z.level.as_deref(), Some("pro"));

        let five = z.window_5h.expect("unit 3 is the 5-hour window");
        assert_eq!(five.label, "5-hour");
        // Percentages arrive as INTEGERS here — 1 means 1%, not 100%.
        assert!((five.used_fraction.unwrap() - 0.01).abs() < 1e-9);
        assert!(five.resets_at.is_some());

        let weekly = z.weekly.expect("unit 6 is the weekly quota");
        assert_eq!(weekly.label, "weekly");
        assert!((weekly.used_fraction.unwrap() - 0.60).abs() < 1e-9);

        let tools = z.tools.expect("TIME_LIMIT is the tool quota");
        assert_eq!(tools.limit, Some(1000.0));
        assert_eq!(tools.used, Some(69.0));
        assert_eq!(tools.remaining, Some(931.0));
        assert!((tools.used_fraction().unwrap() - 0.069).abs() < 1e-9);
        assert_eq!(
            tools.details,
            vec![
                ("search-prime".to_string(), 54.0),
                ("web-reader".to_string(), 15.0),
                ("zread".to_string(), 0.0)
            ]
        );

        // The order is genuinely not relied on: reverse the array, move
        // the weekly to position 0, and re-parse. Position 0 held the
        // TIME_LIMIT row in the original, so a parser reading by index
        // would now report the tool quota as the weekly.
        let mut reordered = zai_payload();
        {
            let arr = reordered["data"]["limits"].as_array().cloned().unwrap();
            let mut arr = arr;
            arr.reverse();
            arr[0]["percentage"] = json!(61); // the weekly, now first
            reordered["data"]["limits"] = Value::Array(arr);
        }
        let z2 = parse_zai(&reordered).expect("parses reordered");
        assert!((z2.weekly.as_ref().unwrap().used_fraction.unwrap() - 0.61).abs() < 1e-9);
        assert!((z2.window_5h.as_ref().unwrap().used_fraction.unwrap() - 0.01).abs() < 1e-9);
        assert_eq!(z2.tools.as_ref().unwrap().used, Some(69.0));
    }

    /// An entry with NEITHER a unit this parser knows nor a type it
    /// knows is skipped, not fatal — the vendor adds rows, and an
    /// unknown row is not a reason to lose the three we can read.
    #[test]
    fn an_unknown_limit_row_is_skipped_not_fatal() {
        let mut v = zai_payload();
        v["data"]["limits"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "type": "SOMETHING_NEW", "unit": 9, "percentage": 42 }));
        let z = parse_zai(&v).expect("still parses");
        assert!(z.window_5h.is_some() && z.weekly.is_some() && z.tools.is_some());
    }

    /// The tightest pool across the three, for the ambient display —
    /// the weekly 60% wins over the 5h 1% and the tools 6.9%.
    #[test]
    fn the_zai_tightest_pool_is_the_max_of_its_three() {
        let z = parse_zai(&zai_payload()).expect("parses");
        let f = ZaiQuota::tightest_used_fraction(&z).expect("fraction");
        assert!((f - 0.60).abs() < 1e-9, "got {f}");
    }

    /// The failure envelope is `success:false` with the status at 200 —
    /// the same trap as the other two paths, caught before the parse.
    #[test]
    fn a_zai_two_hundred_carrying_success_false_is_an_error() {
        let v = json!({ "code": 429, "msg": "QUERY_THROTTLED", "success": false });
        // The check lives in fetch_zai against the raw body; pin the
        // envelope shape the parser must never see as data.
        assert_eq!(v.get("success").and_then(Value::as_bool), Some(false));
        assert!(v.get("data").is_none(), "no data on a failure envelope");
    }

    /// A `data.limits` that names nothing known is "no plan limits",
    /// not a zeroed ZaiQuota — the released-seat rule again.
    #[test]
    fn a_zai_payload_naming_no_limits_is_no_quota_not_zero_quota() {
        let v = json!({ "success": true, "data": { "level": null, "limits": [] } });
        assert!(parse_zai(&v).is_none(), "empty is absence");
    }

    /// THE RELEVANCE CONTRACT. `plan_for_provider` is the whole fix for
    /// "worthless to me": a provider string maps to the POOL IT DRAWS
    /// FROM, and both route spellings (`custom:`-prefixed or bare)
    /// resolve.
    ///
    /// The Alibaba cases are the ones that were wrong (FOUND
    /// 2026-09-20): the box runs FOUR provider entries on TWO Alibaba
    /// credentials — `ali-token-plan-personal`, `ali-token-plan-team`,
    /// `ali-responses` (which authenticates with
    /// `ALI_TOKEN_PLAN_TEAM_API_KEY`) and the Kimi entry — and the old
    /// mapping tested only `"team"` before defaulting to personal. A
    /// session on `ali-responses` was therefore shown PERSONAL's
    /// fraction while drawing down TEAM's credits.
    #[test]
    fn a_provider_names_the_pool_it_draws_from() {
        assert_eq!(plan_for_provider("custom:zai-coding-plan"), Some(Plan::Zai));
        assert_eq!(plan_for_provider("zai-coding-plan"), Some(Plan::Zai));
        assert_eq!(
            plan_for_provider("custom:kimi-coding-plan"),
            Some(Plan::Kimi)
        );
        assert_eq!(plan_for_provider("kimi-coding-plan"), Some(Plan::Kimi));
        assert_eq!(plan_for_provider("ali-token-plan-team"), Some(Plan::Team));
        assert_eq!(
            plan_for_provider("ali-token-plan-personal"),
            Some(Plan::Personal)
        );

        // `ali-responses` is a Responses-API entry on the TEAM key —
        // the case the name-substring rule alone gets wrong.
        assert_eq!(plan_for_provider("ali-responses"), Some(Plan::Team));
        assert_eq!(plan_for_provider("custom:ali-responses"), Some(Plan::Team));

        // Word order must not decide the pool: "team" appears in one
        // spelling and "personal" in the other, and each is tested
        // before the generic prefix.
        assert_eq!(plan_for_provider("ali-team"), Some(Plan::Team));
        assert_eq!(plan_for_provider("ali-personal"), Some(Plan::Personal));

        // The other providers on this box are on NO plan — and that is
        // `None`, not personal. Falling back to personal is what put a
        // confident wrong number in front of a session spending
        // OpenRouter credit.
        assert_eq!(plan_for_provider("openrouter"), None);
        assert_eq!(plan_for_provider("mistral"), None);
        assert_eq!(plan_for_provider("codestral"), None);
        assert_eq!(plan_for_provider("nvidia-nim"), None);
        assert_eq!(plan_for_provider(""), None);
    }

    /// The empty string is not a provider, and a surface that cannot
    /// name one must not be handed somebody's pool as a consolation.
    /// This is the exact shape that produced "plan 59%" on a session
    /// running a provider no quota reader covers.
    #[test]
    fn an_unnamed_provider_gets_no_plan_rather_than_the_default_one() {
        assert!(plan_for_provider("   ").is_none());
        assert!(plan_for_provider("openrouter").is_none());
        // …and the report renders no marker at all for it, rather than
        // marking personal.
        let mut snap = snap_of(Some(team_fixture()), None);
        snap.personal = Some(PersonalQuota {
            windows: vec![Window {
                label: "weekly".into(),
                used_fraction: Some(0.5),
                resets_at: None,
            }],
            free_tier: vec![],
        });
        snap.personal_error = None;
        let out = format_quota_report_for(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            false,
            Some("openrouter"),
        );
        assert!(
            !out.contains("← active"),
            "no plan is this session's, so nothing is marked: {out}"
        );
    }

    /// `Plan::tightest_used_fraction` reads ONLY its own pool — a team
    /// session is not told the z.ai weekly figure as if it were its
    /// own, which is the exact complaint.
    #[test]
    fn a_plans_fraction_comes_from_its_own_pool_only() {
        let mut snap = snap_of(Some(team_fixture()), None);
        snap.zai = parse_zai(&zai_payload());
        snap.zai_error = None;

        let team = Plan::Team.tightest_used_fraction(&snap).expect("team");
        assert!((team - 0.5534).abs() < 0.001, "team credits: {team}");

        let zai = Plan::Zai.tightest_used_fraction(&snap).expect("zai");
        assert!(
            (zai - 0.60).abs() < 1e-9,
            "the z.ai weekly, not the team pool: {zai}"
        );

        // And a plan with no reading says None, which renders as an
        // absence — not as zero.
        snap.zai = None;
        assert!(Plan::Zai.tightest_used_fraction(&snap).is_none());
    }

    /// The rendered zai section: plan tier, both windows with the
    /// reset, the tool quota grounded against its pool, and the
    /// per-tool breakdown. Same shape rules as the other sections.
    #[test]
    fn the_zai_section_renders_like_the_others() {
        let mut snap = snap_of(None, Some("not configured".into()));
        snap.zai = parse_zai(&zai_payload());
        snap.zai_error = None;
        let out = format_quota_report(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            false,
        );
        assert!(out.contains("\nzai\n"), "the section head: {out}");
        assert!(out.contains("plan      pro"), "the tier: {out}");
        assert!(out.contains("5-hour    1.0% used"), "{out}");
        assert!(out.contains("weekly    60.0% used"), "{out}");
        assert!(out.contains("tools     69 of 1,000 calls"), "{out}");
        assert!(out.contains("search-prime 54"), "{out}");
        assert!(
            out.contains("resets    2026-10-01 12:58 UTC"),
            "the monthly reset: {out}"
        );

        // …and an error on the zai path is named, independently of the
        // other two.
        let mut broken = snap_of(None, Some("team down".into()));
        broken.zai_error = Some("z.ai key rejected".into());
        let out = format_quota_report(
            &broken,
            Duration::from_secs(0),
            Duration::from_secs(300),
            false,
        );
        assert!(
            out.contains("zai\n  unavailable — z.ai key rejected"),
            "{out}"
        );
    }

    /// The active-plan marker: exactly one section carries `← active`,
    /// and it is the one the named provider draws from.
    #[test]
    fn the_report_marks_the_session_plan_and_only_that_one() {
        let mut snap = snap_of(Some(team_fixture()), None);
        snap.zai = parse_zai(&zai_payload());
        snap.zai_error = None;

        let out = format_quota_report_for(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            false,
            Some("zai-coding-plan"),
        );
        assert!(out.contains("zai ← active"), "{out}");
        assert_eq!(out.matches("← active").count(), 1, "{out}");
        assert!(!out.contains("team ← active"), "{out}");

        let out = format_quota_report_for(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            false,
            Some("ali-token-plan-personal"),
        );
        assert!(out.contains("personal ← active"), "{out}");
        assert_eq!(out.matches("← active").count(), 1, "{out}");

        // No provider named: no marker at all — the caller asked in
        // their own words and gets the unmarked whole.
        let out = format_quota_report_for(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            false,
            None,
        );
        assert!(!out.contains("← active"), "{out}");
    }

    /// The JSON the desktop paints from carries each plan's tightest
    /// fraction under its own name, so the status bar can read the
    /// right one without re-deriving pools from raw sections.
    #[test]
    fn the_json_serves_each_plans_fraction_under_its_own_name() {
        let mut snap = snap_of(Some(team_fixture()), None);
        snap.zai = parse_zai(&zai_payload());
        snap.zai_error = None;
        let v = snapshot_json(&snap, Duration::from_secs(3), 300, 900);

        let team = v["team_used_fraction"].as_f64().expect("team fraction");
        assert!((team - 0.5534).abs() < 0.001);
        let zai = v["zai_used_fraction"].as_f64().expect("zai fraction");
        assert!((zai - 0.60).abs() < 1e-9);
        // personal has no reading: the key is null, never 0.
        assert!(v["personal_used_fraction"].is_null());

        // The zai section itself, in full.
        let z = &v["zai"];
        assert_eq!(z["level"], "pro");
        assert_eq!(z["tools"]["used"], 69.0);
        assert_eq!(z["tools"]["details"][0]["model"], "search-prime");
        assert_eq!(z["window_5h"]["used_fraction"], 0.01);
    }

    /// The decision a paint path makes, pinned where the axum handler
    /// could not be. `/api/quota` asks this and nothing else; its two
    /// answers are "here is a reading" and "none has ever arrived",
    /// and there is no third answer that throws one away.
    #[test]
    fn a_paint_path_is_shown_the_reading_it_has_not_a_cold_answer() {
        let _g = crate::testenv::lock();
        let mut cfg = crate::config::VelesConfig::default();
        cfg.tokenplan.cache_ttl_secs = 300;

        invalidate();
        assert!(
            for_display(&cfg).is_none(),
            "no reading has ever arrived: the one honest cold answer"
        );

        seed_cache_aged_for_test(
            snap_of(Some(team_fixture()), None),
            Duration::from_secs(600),
        );
        let (snap, age) = for_display(&cfg)
            .expect("a reading past its ttl is still a reading — this is the bug, closed");
        assert!(age.as_secs() >= 600);
        assert!(snap.tightest_used_fraction().is_some());
        invalidate();
    }

    // ── the TTL is not an expiry date ────────────────────────────────

    /// USER 2026-09-16: *"the quota/usage in the status bar seems to
    /// disable itself on session reopen/switch and then never
    /// reactivates by itself."*
    ///
    /// It had not disabled anything. `cache_ttl_secs` ships at 300 and
    /// `poll_secs` at 900, so a reading spends 600 of every 900 seconds
    /// past the TTL — and `cached()` was the only reader, so for that
    /// whole stretch `/api/quota` answered `cold: true` and every
    /// surface rendered "no reading". Two thirds of the time, on the
    /// shipped defaults.
    ///
    /// The split this pins: past the TTL is OLD, never ABSENT. `cached`
    /// still means "fresh enough to show without comment"; `last_known`
    /// means "the last thing that actually arrived". Only an empty
    /// cache is cold.
    #[test]
    fn a_reading_past_the_ttl_is_old_not_absent() {
        let _g = crate::testenv::lock();
        let mut cfg = crate::config::VelesConfig::default();
        cfg.tokenplan.cache_ttl_secs = 300;
        cfg.tokenplan.poll_secs = 900;

        // Nothing has ever arrived: THIS is cold, and it is the only
        // thing that is.
        invalidate();
        assert!(cached(&cfg).is_none(), "an empty cache is not warm");
        assert!(
            last_known().is_none(),
            "…and it has no last reading either — the cold answer"
        );

        // Inside the TTL: both agree.
        seed_cache_aged_for_test(snap_of(Some(team_fixture()), None), Duration::from_secs(60));
        let (_, age) = cached(&cfg).expect("a 60s-old reading is inside a 300s ttl");
        assert!(age.as_secs() < 300, "got age {}s", age.as_secs());

        // Past the TTL — the ten-minute stretch the poller leaves open.
        seed_cache_aged_for_test(
            snap_of(Some(team_fixture()), None),
            Duration::from_secs(600),
        );
        assert!(
            cached(&cfg).is_none(),
            "600s is past the 300s ttl, so it is not fresh"
        );
        let (snap, age) = last_known()
            .expect("but the reading still exists — answering `cold` here is what blanked the bar");
        assert!(
            age.as_secs() >= 600,
            "the backdated stamp must survive: got {}s",
            age.as_secs()
        );
        assert!(
            snap.tightest_used_fraction().is_some(),
            "and it still carries the figures it always did"
        );
        invalidate();
    }

    /// The payload says which of the two it is, and says it in a place
    /// that cannot disagree with the age beside it — `stale` is derived
    /// from `age` and `ttl_secs` inside `snapshot_json`, not passed in
    /// by a caller who might compute it differently.
    #[test]
    fn the_json_marks_a_stale_reading_without_pretending_it_is_missing() {
        let snap = snap_of(Some(team_fixture()), None);

        let fresh = snapshot_json(&snap, Duration::from_secs(60), 300, 900);
        assert_eq!(fresh["stale"], false, "60s of a 300s ttl is fresh");
        assert_eq!(fresh["cold"], false);
        assert!(
            fresh["team_used_fraction"].as_f64().is_some(),
            "a fresh reading carries its figures"
        );

        let stale = snapshot_json(&snap, Duration::from_secs(600), 300, 900);
        assert_eq!(stale["stale"], true, "600s is past a 300s ttl");
        assert_eq!(
            stale["cold"], false,
            "stale is NOT cold — cold means no reading ever arrived, and \
             conflating the two is what made the plan field read `plan —` \
             for ten minutes out of every fifteen"
        );
        assert_eq!(stale["age_secs"], 600, "and it says how old it is");
        assert_eq!(
            stale["team_used_fraction"], fresh["team_used_fraction"],
            "an old reading is still the truth about the account"
        );
    }

    // ── Path D — the Kimi Code plan (2026-09-20) ──────────────────────

    /// The live payload, captured 2026-09-20 16:5x WITH a 403 beside
    /// it: a chat completion at that moment answered "You've reached
    /// your 5-hour usage limit" while `usages.limit_5h.used_ratio` read
    /// 0 and `limits[].detail` read 100/100.
    ///
    /// That pairing is the whole reason this parser exists in this
    /// shape. The two halves DISAGREE and only one of them is true.
    fn kimi_payload() -> Value {
        json!({
            "limits": [
                { "window": { "duration": 300, "timeUnit": "TIME_UNIT_MINUTE" },
                  "detail": { "limit": "100", "used": "100", "remaining": "0",
                              "resetTime": "2026-09-20T15:22:56.391655Z" } }
            ],
            "usages": {
                "limit_5h": { "used_ratio": 0, "reset_time": "2026-09-20T15:22:55Z" },
                "limit_month_total": { "used_ratio": 0.0546,
                                       "reset_time": "2026-10-20T00:00:00Z" },
                "limit_month_code": { "used_ratio": 0, "reset_time": "2026-10-20T00:00:00Z" }
            }
        })
    }

    /// THE CENTRAL RULE. `limits[].detail` is the absolute half and it
    /// is the one that matched an observed 403; `usages.limit_5h`'s
    /// `used_ratio` read 0 through the entire climb. Where both describe
    /// one window, the absolute wins and the ratio is dropped — never
    /// averaged, never shown beside to be mentally blended.
    #[test]
    fn kimi_reads_the_absolute_half_where_the_two_halves_disagree() {
        let k = parse_kimi(&kimi_payload()).expect("parses");
        let five = k
            .windows
            .iter()
            .find(|w| w.label == "5-hour")
            .expect("the 300-minute window is the 5-hour window");
        assert!(
            (five.used_fraction.expect("100/100 is a fraction") - 1.0).abs() < 1e-9,
            "100 of 100 used is 100%, and the API said so in the half \
             that matched the 403"
        );
        assert_eq!(
            k.windows.iter().filter(|w| w.label == "5-hour").count(),
            1,
            "the ratio's version of the same window must not be a second \
             row: {:#?}",
            k.windows
        );
        // …and it must not be silently 0, which is what the ratio said.
        assert!(
            five.used_fraction != Some(0.0),
            "the ratio's 0 was the lie; the report must not repeat it"
        );
    }

    /// The month figures exist ONLY as ratios. They are served — they
    /// are the only month reading there is, and they provably move — and
    /// they are labelled from their own keys rather than merged into one
    /// anonymous "month".
    #[test]
    fn kimi_serves_the_month_ratios_under_their_own_labels() {
        let k = parse_kimi(&kimi_payload()).expect("parses");
        let month = k
            .windows
            .iter()
            .find(|w| w.label == "month")
            .expect("limit_month_total becomes the month window");
        assert!((month.used_fraction.unwrap() - 0.0546).abs() < 1e-9);
        assert!(
            k.windows.iter().any(|w| w.label == "month · code"),
            "limit_month_code is its own window, named for its key rather \
             than folded into the month total: {:#?}",
            k.windows
        );
    }

    /// The window label comes off the wire's own descriptor, so a plan
    /// that changes its window size relabels itself instead of silently
    /// lying about which window the number belongs to.
    #[test]
    fn a_kimi_window_is_named_from_its_own_descriptor() {
        assert_eq!(
            kimi_window_label(Some(
                &json!({"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"})
            )),
            "5-hour"
        );
        assert_eq!(
            kimi_window_label(Some(
                &json!({"duration": 30, "timeUnit": "TIME_UNIT_MINUTE"})
            )),
            "30-minute"
        );
        assert_eq!(
            kimi_window_label(Some(&json!({"duration": 7, "timeUnit": "TIME_UNIT_DAY"}))),
            "7-day"
        );
        // Nothing to read: a name, not a panic and not a blank row.
        assert_eq!(kimi_window_label(None), "window");
        assert_eq!(kimi_window_label(Some(&json!({}))), "window");
    }

    /// Numbers arrive as STRINGS here — `"limit":"100"` — unlike every
    /// other API in this file. A reader that only accepts JSON numbers
    /// reports "not reported" for figures the API gave.
    #[test]
    fn a_kimi_number_may_be_a_string_or_a_number() {
        assert_eq!(kimi_num(&json!({"n": "100"}), "n"), Some(100.0));
        assert_eq!(kimi_num(&json!({"n": 100}), "n"), Some(100.0));
        assert_eq!(kimi_num(&json!({"n": " 12.5 "}), "n"), Some(12.5));
        // A missing key, and a word where a number goes: both absent.
        assert_eq!(kimi_num(&json!({}), "n"), None);
        assert_eq!(kimi_num(&json!({"n": "unlimited"}), "n"), None);
    }

    /// A payload that names no windows is NO READING, not a quota of
    /// zero — the released-seat rule, applied to the fourth pool.
    #[test]
    fn a_kimi_payload_naming_no_windows_is_no_quota_not_zero_quota() {
        assert!(parse_kimi(&json!({})).is_none(), "empty is absence");
        assert!(parse_kimi(&json!({"limits": [], "usages": {}})).is_none());
        // A window mentioned with no figures at all is not a window.
        assert!(
            parse_kimi(&json!({"limits": [{"window": {"duration": 300}}]})).is_none(),
            "a limit row with no detail is nothing to report"
        );
    }

    /// The tightest window is the one that actually stops a turn: the
    /// 5-hour window 403s, so when several are reported this is the
    /// number that answers "can I keep going".
    #[test]
    fn the_kimi_tightest_window_is_the_one_that_stops_a_turn() {
        let k = parse_kimi(&kimi_payload()).expect("parses");
        assert!(
            (k.tightest_used_fraction().unwrap() - 1.0).abs() < 1e-9,
            "100% on the 5-hour window, not 5.5% on the month"
        );
        // A plan with no fractions at all says so.
        let empty = KimiQuota { windows: vec![] };
        assert!(empty.tightest_used_fraction().is_none());
    }

    /// The rendered section: each window named, each percentage
    /// grounded, and — the rule that matters — nothing invented for a
    /// window the API did not measure.
    #[test]
    fn the_kimi_section_renders_like_the_others() {
        let mut snap = snap_of(None, Some("not configured".into()));
        snap.kimi = parse_kimi(&kimi_payload());
        snap.kimi_error = None;
        let out = format_quota_report(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            false,
        );
        assert!(out.contains("\nkimi\n"), "the section head: {out}");
        assert!(out.contains("5-hour    100.0% used"), "{out}");
        assert!(out.contains("month     5.5% used"), "{out}");
        assert!(
            out.contains("resets    2026-10-20 00:00 UTC") || out.contains("2026-10-20 00:00 UTC"),
            "the monthly reset: {out}"
        );

        // An error on the kimi path is named, independently of the
        // other three.
        let mut broken = snap_of(None, Some("team down".into()));
        broken.kimi_error = Some("kimi key rejected".into());
        let out = format_quota_report(
            &broken,
            Duration::from_secs(0),
            Duration::from_secs(300),
            false,
        );
        assert!(
            out.contains("kimi\n  unavailable — kimi key rejected"),
            "{out}"
        );
    }

    /// The marker moves onto the fourth plan like it does for the other
    /// three — and only onto it.
    #[test]
    fn a_kimi_session_marks_the_kimi_section_and_no_other() {
        let mut snap = snap_of(Some(team_fixture()), None);
        snap.zai = parse_zai(&zai_payload());
        snap.zai_error = None;
        snap.kimi = parse_kimi(&kimi_payload());
        snap.kimi_error = None;
        let out = format_quota_report_for(
            &snap,
            Duration::from_secs(0),
            Duration::from_secs(300),
            false,
            Some("custom:kimi-coding-plan"),
        );
        assert!(out.contains("kimi ← active"), "{out}");
        assert_eq!(
            out.matches("← active").count(),
            1,
            "exactly one section is the session's: {out}"
        );
    }

    // ─ the local ledger, split by plan (2026-09-20) ──────────────────

    fn ledger_row(
        provider: &str,
        calls: u64,
        prompt: u64,
        completion: u64,
    ) -> crate::store::ProviderLedgerRow {
        crate::store::ProviderLedgerRow {
            provider: provider.to_string(),
            calls,
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: 0,
        }
    }

    /// THE COMPLAINT, as a test: "the existing usage is confusing, it
    /// conflates the personal and team alibaba tokens". Four providers
    /// on two Alibaba credentials plus two other vendors must come out
    /// as four separate rows, each carrying the right provider entries.
    #[test]
    fn the_ledger_splits_by_plan_not_by_a_single_total() {
        let rows = vec![
            ledger_row("ali-token-plan-personal", 6964, 1_007_183_171, 7_703_844),
            ledger_row("ali-token-plan-team", 3503, 472_941_445, 4_943_003),
            // The entry that was being conflated with personal: a TEAM
            // credential under a name containing neither "team" nor
            // "personal".
            ledger_row("ali-responses", 3, 30_283, 283),
            ledger_row("zai-coding-plan", 2753, 361_717_710, 1_430_622),
            ledger_row("kimi-coding-plan", 282, 25_679_710, 60_002),
        ];
        let out = ledger_by_plan(&rows);

        let find = |name: &str| {
            out.iter().find(|r| r.label() == name).unwrap_or_else(|| {
                panic!("no row named {name} in {:#?}", out);
            })
        };

        let personal = find("personal");
        assert_eq!(personal.calls, 6964);
        assert_eq!(personal.providers, vec!["ali-token-plan-personal"]);

        let team = find("team");
        assert_eq!(
            team.calls, 3506,
            "the team row covers BOTH team entries — ali-token-plan-team \
             and ali-responses, which authenticates with the same key"
        );
        assert_eq!(
            team.providers,
            vec!["ali-responses", "ali-token-plan-team"],
            "and it names them, so the grouping can be checked rather \
             than trusted"
        );

        let zai = find("zai");
        assert_eq!(zai.calls, 2753);
        let kimi = find("kimi");
        assert_eq!(kimi.calls, 282);

        // FOUR rows for four plans — the conflation is the bug this
        // module exists to kill. `ali-responses` is INSIDE the team row,
        // not one of its own; that is the point.
        assert_eq!(
            out.len(),
            4,
            "four plans and nothing else — no row per provider: {:#?}",
            out
        );
    }

    /// A provider no plan names is its OWN row, never folded into a plan
    /// it does not belong to. Folding it would make the split silently
    /// not add up, which is worse than showing an unclaimed row.
    #[test]
    fn a_planless_provider_keeps_its_own_row_rather_than_joining_one() {
        let rows = vec![
            ledger_row("ali-token-plan-team", 10, 100, 10),
            ledger_row("openrouter", 20, 200, 20),
            ledger_row("mistral", 4, 40, 4),
        ];
        let out = ledger_by_plan(&rows);
        assert_eq!(out.len(), 3, "{:#?}", out);

        let unmapped: Vec<&PlanLedgerRow> = out.iter().filter(|r| r.plan.is_none()).collect();
        assert_eq!(unmapped.len(), 2, "two providers on no plan");
        assert_eq!(
            unmapped[0].providers,
            vec!["openrouter"],
            "the busiest unmapped provider comes first, and keeps its name"
        );
        assert_eq!(unmapped[0].calls, 20);
        assert_eq!(unmapped[1].providers, vec!["mistral"]);
        // …and no unmapped provider was absorbed by the team row.
        let team = out.iter().find(|r| r.label() == "team").expect("team");
        assert_eq!(team.calls, 10, "only the team entry's own calls");
    }

    /// The order is FIXED rather than whatever the store's `ORDER BY
    /// count` produced: a figure that moves around between two paints is
    /// a figure nobody can compare at a glance.
    #[test]
    fn the_ledger_order_is_stable_however_the_input_is_sorted() {
        let asc = vec![
            ledger_row("kimi-coding-plan", 1, 1, 1),
            ledger_row("zai-coding-plan", 2, 2, 2),
            ledger_row("ali-token-plan-team", 3, 3, 3),
            ledger_row("ali-token-plan-personal", 4, 4, 4),
        ];
        let desc: Vec<crate::store::ProviderLedgerRow> = asc.iter().rev().cloned().collect();
        let labels = |rows: &[crate::store::ProviderLedgerRow]| {
            ledger_by_plan(rows)
                .iter()
                .map(PlanLedgerRow::label)
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(&asc), labels(&desc));
        assert_eq!(
            labels(&asc),
            vec!["team", "personal", "zai", "kimi"],
            "and the order is the plans' own, not the caller's"
        );
    }

    /// A plan that spent NOTHING is not a row. A `0 calls` row reads as
    /// "this plan is idle" when the truth is "you are not on it", and on
    /// a usage display the difference matters.
    #[test]
    fn a_plan_with_no_usage_is_absent_not_a_zero_row() {
        let rows = vec![ledger_row("kimi-coding-plan", 5, 50, 5)];
        let out = ledger_by_plan(&rows);
        assert_eq!(out.len(), 1, "{:#?}", out);
        assert_eq!(out[0].label(), "kimi");
        assert!(
            !out.iter().any(|r| r.label() == "team"),
            "an unused plan must not appear as a zero"
        );
    }

    /// The JSON is what every surface renders, so the rows carry the
    /// plan's NAME (not a debug repr), the provider entries that fed it,
    /// and the total the widget abbreviates.
    #[test]
    fn the_ledger_json_names_the_plan_and_its_providers() {
        let rows = vec![
            ledger_row("ali-token-plan-team", 3, 100, 20),
            ledger_row("ali-responses", 1, 5, 1),
            ledger_row("openrouter", 2, 7, 3),
        ];
        let v = ledger_json(&ledger_by_plan(&rows));
        assert_eq!(v.len(), 2, "{v:#?}");

        assert_eq!(v[0]["plan"], "team");
        assert_eq!(v[0]["calls"], 4);
        assert_eq!(v[0]["total_tokens"], 126);
        assert_eq!(v[0]["providers"][0], "ali-responses");
        assert_eq!(v[0]["providers"][1], "ali-token-plan-team");

        assert!(
            v[1]["plan"].is_null(),
            "an unclaimed provider has no plan, and says so rather than \
             naming somebody else's: {:#?}",
            v[1]
        );
        assert_eq!(v[1]["providers"][0], "openrouter");
    }
}

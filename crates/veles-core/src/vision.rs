//! vision_analyze tool — send an image to a vision-capable model for analysis.
//!
//! #30 in the feature matrix. Accepts a URL or local file path, loads the
//! image, sends it to a vision model with a question, returns the text
//! response. Uses the configured provider chain — the caller specifies a
//! model or we auto-select a vision-capable one.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::Tool;

pub struct VisionAnalyzeTool {
    /// The calling session — the local-path jail below resolves the
    /// principal from it. Owner surfaces have no principal and keep
    /// the whole filesystem, exactly as the F1 read jail leaves them.
    pub session_id: String,
}

#[async_trait]
impl Tool for VisionAnalyzeTool {
    fn name(&self) -> &str {
        "vision_analyze"
    }
    fn description(&self) -> &str {
        "Analyze an image with a vision-capable model. Accepts an image URL (http/https) or local file path, plus a question about the image."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "image_url": {
                    "type": "string",
                    "description": "Image URL (http/https) or local file path"
                },
                "question": {
                    "type": "string",
                    "description": "Question or instruction for the image"
                }
            },
            "required": ["image_url", "question"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let image_url = args["image_url"].as_str().unwrap_or_default().to_string();
        let question = args["question"].as_str().unwrap_or_default().to_string();
        if image_url.is_empty() {
            return "error: image_url is required".into();
        }
        if question.is_empty() {
            return "error: question is required".into();
        }

        // ── the local-path jail ──
        //
        // A tool is not a marker, so the gateway's media gate never
        // saw this path: until here a trusted non-admin could hand the
        // vision tool ANY readable file on the host — a webcam frame,
        // a screenshot of another session, `.env` rendered as an
        // image's alt text — and read back what the model saw in it.
        // The SAME rule the marker gate uses, judged HERE, off the
        // session's principal: the media root, or the sender's own
        // sandbox workspace. Owner surfaces (no principal) are
        // unrestricted, as they are everywhere else.
        let is_url = image_url.starts_with("http://") || image_url.starts_with("https://");

        // A sandboxed session's own files are in its box, where its
        // write_file and terminal put them (`/work/x.png`, `~/x.png`, a
        // relative path): read there first, the way read_file reads them.
        // Read on this node they were "file not found", and a model built
        // a base64 bridge to the host to see its own picture (VPS IRC,
        // 2026-10-13). The box is the session's own, so the jail below,
        // which is about this node's files, is not asked about it. What
        // is not in the box (the media root, where generated images land)
        // is read here as before, through the jail.
        let in_box = if is_url {
            None
        } else {
            read_in_box(&self.session_id, &image_url).await
        };
        let data_url = match in_box {
            Some(Ok(d)) => d,
            in_box => {
                let boxed = in_box.and_then(Result::err);
                if !is_url {
                    let who = crate::acl::session_principal(&self.session_id);
                    if let Some(why) = crate::media::deliverable(&image_url, who.as_deref()) {
                        return match boxed {
                            Some(e) => format!("vision_analyze error: {e}"),
                            None => format!(
                                "vision_analyze: {image_url} is not available to this session \
                                 ({why}); files under the media root or your own workspace can \
                                 be analyzed"
                            ),
                        };
                    }
                }
                // Read the image: either from URL or local path → base64
                // data URL. A sandboxed session's miss reports the box.
                match resolve_image(&image_url).await {
                    Ok(d) => d,
                    Err(e) => return format!("vision_analyze error: {}", boxed.unwrap_or(e)),
                }
            }
        };

        // Call the vision model via OpenAI-compatible chat completions.
        // We use the same provider config the brain uses — any
        // vision-capable model on the configured route.
        match call_vision_model(&data_url, &question).await {
            Ok(answer) => answer,
            Err(e) => format!("vision_analyze error: {e}"),
        }
    }
}

/// The image at `path` in `session`'s sandbox box, when the session is
/// sandboxed and the path is one of the box's: `/work/…`, `~/…` or
/// relative, the paths its own tools report. `None` otherwise: the path
/// is this node's (the media root, a host path the jail judges).
async fn read_in_box(session: &str, path: &str) -> Option<Result<String, String>> {
    if path.starts_with('/') && path != "/work" && !path.starts_with("/work/") {
        return None;
    }
    let cpath = crate::user_envs::container_resolve(path);
    Some(
        match crate::user_envs::read_session_bytes(session, path).await? {
            Ok(b) if b.len() > crate::imagegen::MAX_FETCH_BYTES => Err(format!(
                "{cpath} is {} bytes, over the {}-byte limit",
                b.len(),
                crate::imagegen::MAX_FETCH_BYTES
            )),
            Ok(b) => Ok(format!(
                "data:{};base64,{}",
                guess_mime(std::path::Path::new(&cpath)),
                base64_encode(&b)
            )),
            Err(e) => Err(e.to_string()),
        },
    )
}

async fn resolve_image(src: &str) -> Result<String, String> {
    if src.starts_with("http://") || src.starts_with("https://") {
        // Guarded: this tool fetches a URL the CALLER chose and then
        // describes what the model saw in it, which makes a fetch into
        // a read. Unguarded it was `http://127.0.0.1:8642/api/...`, the
        // LAN, or `169.254.169.254` — the local-path jail beside it
        // closed the same hole for paths and left this one open.
        //
        // Someone else's URL leaves the way their web_extract would
        // (egress.rs): over the node's proxy, or not from this node.
        let proxy = crate::egress::fetch_proxy("vision_analyze")
            .map_err(|e| e.trim_start_matches("vision_analyze error: ").to_string())?;
        let resp = crate::urlguard::get_through(
            src,
            std::time::Duration::from_secs(30),
            None,
            proxy.as_deref(),
        )
        .await?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/png")
            .to_string();

        // Capped, and this is the call site that needs it MOST.
        // `read_capped`'s own doc describes bounding "a URL the PROVIDER
        // chose"; here the URL was chosen by whoever typed it — a tool
        // argument from a model, or, once triggers land, a link a
        // stranger pasted into a channel. `bytes()` read the whole body
        // into memory with no bound and base64 then inflated it by a
        // third into the request body, so one link to a large object was
        // an OOM in the process that is also every chat and the IRC
        // connection. Same class as `lsp_proxy`'s MAX_FRAME_BYTES: read
        // into a growable buffer and never ask what happens if it does
        // not end.
        let bytes = crate::imagegen::read_capped(resp, "the image").await?;
        let b64 = base64_encode(&bytes);
        Ok(format!("data:{content_type};base64,{b64}"))
    } else {
        // Local file path
        let path = std::path::Path::new(src);
        if !path.exists() {
            return Err(format!("file not found: {src}"));
        }
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let mime = guess_mime(path);
        let b64 = base64_encode(&bytes);
        Ok(format!("data:{mime};base64,{b64}"))
    }
}

fn guess_mime(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("bmp") => "image/bmp",
        _ => "image/png",
    }
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Decode standard base64 — the shape `/images/generations` answers
/// with in `b64_json`. Shared with the imagegen route.
pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// Which provider and model the vision tool should call.
///
/// Pure, so the rule can be tested without a socket — it could not be
/// before, which is how the bug below survived in a function whose own
/// comment described the opposite behaviour.
///
/// The rule: `[vision]` names BOTH halves or it names neither. A
/// provider with no model used to fall back to a hard-coded id, and the
/// id it fell back to was `google/lyria-3-pro-preview` — Google's MUSIC
/// generation model, priced per song. So a half-configured `[vision]`
/// sent the image to a model that cannot see, on a route that bills, and
/// the failure arrived as an opaque 400 several seconds into a turn. The
/// comment three lines above it already said "unset is an honest error,
/// not a silent trip back to a model that will 400"; the code did the
/// trip anyway.
///
/// No default model is named here on purpose. Which model can see is a
/// fact about the operator's account, not about this tree, and a
/// hard-coded guess is what produced the music model in the first place.
pub(crate) fn vision_route(cfg: &crate::config::VelesConfig) -> Result<(String, String), String> {
    const HOW: &str = "set both, e.g.\n\
         [vision]\n\
         provider = \"custom:openrouter\"\n\
         model    = \"<a model your account lists with image input>\"\n\
         — `/defaults vision <provider> <model>` writes it for you";
    match (&cfg.vision.provider, &cfg.vision.model) {
        (Some(p), Some(m)) if !p.trim().is_empty() && !m.trim().is_empty() => {
            Ok((p.clone(), m.clone()))
        }
        (Some(p), _) if !p.trim().is_empty() => Err(format!(
            "[vision].provider is '{p}' but [vision].model is unset — {HOW}"
        )),
        _ => Err(format!("no [vision] route in config.toml — {HOW}")),
    }
}

async fn call_vision_model(data_url: &str, question: &str) -> Result<String, String> {
    // Load config to find the provider + model
    let config = crate::config::load().map_err(|e| format!("config load: {e}"))?;

    // The VISION route, not the text default. The default models
    // (deepseek-v4-flash, glm-5.3) have no vision and reject an
    // `image_url` part with a 400 — which is why "the model can't use
    // its vision tools" looked broken. `[vision]` names a model that can
    // actually see the image; unset is an honest error, not a silent
    // trip back to a model that will 400.
    let (provider_name, model) = vision_route(&config)?;

    // Resolve provider details via the typed config API
    let entry = config
        .provider(&provider_name)
        .ok_or_else(|| format!("provider {provider_name} not found in config"))?;
    let base_url = entry.base_url.clone();
    let key_env = entry.key_env.as_deref().unwrap_or("API_KEY");
    // key_env might be a direct key, not an env var name; an unset NAME
    // is an error, not the key.
    let api_key = crate::secrets::resolve_key_env(key_env)?;

    // The request's own budget is 5 minutes, inside the 10-minute
    // deadline `register_service` gives the service: when a model run
    // overruns, the inner timeout fires first and returns a real HTTP
    // error the caller can log, instead of the outer deadline's opaque
    // "did not answer" — the two used to be equal, so the useful one
    // always lost the race.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;

    // Build OpenAI-compatible vision request
    let body = serde_json::json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": question},
                {"type": "image_url", "image_url": {"url": data_url}}
            ]
        }],
        "stream": false,
        "max_tokens": 2000
    });

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {text}"));
    }

    let json: Value = resp.json().await.map_err(|e| e.to_string())?;
    let answer = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("no response")
        .to_string();

    Ok(answer)
}

/// Describe one image URL — the trigger engine's entry point.
///
/// A thin public wrapper over the same pipeline the tool uses (same
/// urlguard, same capped read, same `[vision]` route), because a
/// second implementation of "fetch, base64, ask" would be a second
/// place for the SSRF jail and the size cap to drift. The caller has
/// already matched an ImageUrl trigger and passed the row's question.
pub async fn describe_url(
    cfg: &crate::config::VelesConfig,
    url: &str,
    question: &str,
) -> Result<String, String> {
    // Fail fast on a route that cannot answer, with the same how-to
    // the tool's own errors carry: a trigger that fires on every image
    // with a 400 behind it looks exactly like a broken trigger.
    let (provider, model) = vision_route(cfg)?;
    let _ = (provider, model);
    let data_url = resolve_image(url).await?;
    call_vision_model(&data_url, question).await
}

/// `vision.describe` — [`describe_url`] as a service (USER 2026-10-10:
/// "automatic alt-text for me if I post an image without any alt text").
/// Open to every caller (`cap = "any"`): the atproto process plugin calls
/// it with no session behind it, and a describe is a read the caller
/// already could make with the `vision_analyze` tool. The URL goes
/// through the same urlguard and size cap.
pub fn register_service() -> Result<(), String> {
    use veles_types::plugin::{method::VISION_DESCRIBE, VisionDescribeParams, VisionDescribed};
    // A describe waits on a vision model that may reason for minutes
    // (transcribing a dense screenshot), so this service gets a longer
    // deadline than the global 2 minutes — 10 minutes, over the inner
    // request's own 5-minute timeout (`call_vision_model`). Per-service,
    // so no other service's ceiling moves.
    crate::services::set_deadline(VISION_DESCRIBE, std::time::Duration::from_secs(600));
    crate::services::register(
        crate::services::ServiceDecl {
            name: VISION_DESCRIBE.into(),
            description: "describe one image URL through the [vision] route".into(),
            cap: Some(crate::services::SERVICE_CAP_ANY.into()),
        },
        crate::services::handler(|p, _cx| async move {
            let p: VisionDescribeParams =
                serde_json::from_value(p).map_err(|e| format!("{VISION_DESCRIBE}: {e}"))?;
            if p.url.trim().is_empty() {
                return Err(format!("{VISION_DESCRIBE}: no url"));
            }
            let cfg = crate::config::live();
            let prompt = if p.prompt.trim().is_empty() {
                "Describe this image."
            } else {
                p.prompt.as_str()
            };
            let text = describe_url(&cfg, p.url.trim(), prompt).await?;
            serde_json::to_value(VisionDescribed { text }).map_err(|e| e.to_string())
        }),
    )
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    /// The service opts into a 10-minute deadline at registration, over
    /// the global 2-minute ceiling: a describe waits on a vision model
    /// that may reason for minutes (transcribing a dense screenshot), and
    /// the automatic alt-text lane lost its first real post to the old
    /// 120 s ceiling. The global ceiling must stay where it is — this is
    /// per-service, so no other service's budget moves.
    #[test]
    fn the_describe_service_asks_for_a_long_deadline() {
        use veles_types::plugin::method::VISION_DESCRIBE;
        register_service().expect("register_service");
        let d = crate::services::deadline_for(VISION_DESCRIBE).expect("a deadline");
        assert_eq!(d, std::time::Duration::from_secs(600));
        assert!(
            d > crate::services::CALL_TIMEOUT,
            "the opt-in must exceed the global ceiling, or the fix does nothing"
        );
    }
}

#[cfg(test)]
mod jail_tests {
    use super::*;
    use serde_json::json;

    fn bind_trusted(sess: &str) {
        crate::acl::set_session_principal(
            sess,
            crate::acl::Principal {
                id: None,
                subject: format!("user:{sess}"),
                role: crate::acl::Role::Trusted,
                caps: crate::acl::Role::Trusted.default_caps(),
                rule_context_limit: None,
                mask: None,
                via: None,
                ticket_role: None,
            },
        );
    }

    #[tokio::test]
    async fn a_trusted_session_cannot_point_vision_at_the_host() {
        // The jail is the media rule applied to a TOOL: until it landed,
        // `vision_analyze` was the one surface that read ANY local path
        // for trusted+ — the media gate judges markers, and a tool call
        // is not a marker. The assertions only look for the jail's own
        // refusal: past it, both branches fail on vision config, which
        // is a different error and none of this test's business.
        let (_home, _env) = crate::testenv::scratch_home();
        let tool = VisionAnalyzeTool {
            session_id: "vision-jail-1".into(),
        };
        bind_trusted("vision-jail-1");

        let out = tool
            .call(json!({"image_url": "/etc/passwd", "question": "what is this?"}))
            .await;
        assert!(
            out.contains("not available to this session"),
            "a trusted non-admin must not read host files through the vision tool: {out}"
        );

        // The media root is the sanctioned surface — the jail lets it
        // through (and the call then fails on unconfigured vision,
        // which is fine and not the jail's business).
        let media = crate::config::media_dir().join("generated/x.png");
        let out2 = tool
            .call(json!({"image_url": media.to_str().unwrap(), "question": "q"}))
            .await;
        assert!(
            !out2.contains("not available to this session"),
            "the media root must pass the jail: {out2}"
        );

        // Owner surfaces keep the whole filesystem, as everywhere.
        let owner = VisionAnalyzeTool {
            session_id: "vision-jail-owner".into(),
        };
        let out3 = owner
            .call(json!({"image_url": "/etc/passwd", "question": "q"}))
            .await;
        assert!(
            !out3.contains("not available to this session"),
            "no principal means owner surface: {out3}"
        );
    }

    /// A sandboxed session's own picture is read in its box, where its
    /// tools wrote it (`/work/…`, `~/…`, relative), not on this node,
    /// where it was "file not found" (VPS IRC, 2026-10-13). No podman
    /// here, so the box answers with its error, which names the box: the
    /// call went there. An owner surface's `/work/…` stays this node's.
    #[tokio::test]
    async fn a_sandboxed_sessions_picture_is_read_in_its_box() {
        let (_home, _env) = crate::testenv::scratch_home();
        let bin = std::env::var("VELES_PODMAN_BIN").ok();
        std::env::set_var("VELES_PODMAN_BIN", "/nonexistent/veles-test-podman");
        bind_trusted("vision-box-1");
        let tool = VisionAnalyzeTool {
            session_id: "vision-box-1".into(),
        };
        let mut said = Vec::new();
        for path in ["/work/cat.png", "~/cat.png", "cat.png"] {
            said.push(tool.call(json!({"image_url": path, "question": "q"})).await);
        }
        let owner = VisionAnalyzeTool {
            session_id: "vision-box-owner".into(),
        };
        let owners = owner
            .call(json!({"image_url": "/work/cat.png", "question": "q"}))
            .await;
        match bin {
            Some(b) => std::env::set_var("VELES_PODMAN_BIN", b),
            None => std::env::remove_var("VELES_PODMAN_BIN"),
        }
        for out in &said {
            assert!(
                out.contains("/work/cat.png in sandbox"),
                "read in the box: {out}"
            );
        }
        assert!(
            owners.contains("file not found: /work/cat.png"),
            "an owner surface's path is this node's: {owners}"
        );
    }

    /// The jail above covers PATHS. A URL was the other half of the same
    /// hole and stayed open: this tool fetches what it is given and then
    /// reports what the model saw in it, so `http://127.0.0.1:8642/...`
    /// or the cloud metadata service was a read of the gateway's own
    /// host, dressed up as "describe this picture".
    ///
    /// Refused for EVERY caller, owner included — unlike the path jail.
    /// A local surface reading its own files is the operator reading
    /// their own machine; a local surface reaching the metadata service
    /// is still SSRF, because the interesting thing about that address
    /// is not who asked.
    #[tokio::test]
    async fn a_url_pointed_at_the_host_is_refused_for_everyone() {
        let (_home, _env) = crate::testenv::scratch_home();
        for sess in ["vision-ssrf-bound", "vision-ssrf-owner"] {
            if sess.ends_with("bound") {
                bind_trusted(sess);
            }
            let tool = VisionAnalyzeTool {
                session_id: sess.into(),
            };
            for url in [
                "http://127.0.0.1:8642/api/logs",
                "http://169.254.169.254/latest/meta-data/",
                "http://192.168.1.1/",
                "http://[::1]:9900/",
            ] {
                let out = tool
                    .call(json!({"image_url": url, "question": "what is this?"}))
                    .await;
                assert!(
                    out.contains("refused:"),
                    "{sess} must not reach {url}: {out}"
                );
            }
        }
    }

    /// The route is all-or-nothing, and the reason is a live bug.
    ///
    /// A `[vision]` with a provider and no model used to fall back to a
    /// hard-coded `google/lyria-3-pro-preview`. That is Google's MUSIC
    /// model, billed per song — confirmed against openrouter's own
    /// catalogue, which describes it as "Google's family of music
    /// generation models". So a half-written section pointed the image at
    /// something that cannot see, on a route that charges, and the
    /// failure surfaced as an opaque 400 inside a turn.
    ///
    /// The same id was also what the "no [vision] section" error told the
    /// operator to configure, so following the advice reproduced the bug.
    #[test]
    fn a_half_configured_vision_route_is_an_error_not_a_music_model() {
        use crate::config::{VelesConfig, VisionSection};
        let with = |p: Option<&str>, m: Option<&str>| VelesConfig {
            vision: VisionSection {
                provider: p.map(str::to_string),
                model: m.map(str::to_string),
            },
            ..Default::default()
        };

        let (p, m) = vision_route(&with(Some("custom:openrouter"), Some("some/vlm"))).unwrap();
        assert_eq!((p.as_str(), m.as_str()), ("custom:openrouter", "some/vlm"));

        let e = vision_route(&with(Some("custom:openrouter"), None)).unwrap_err();
        assert!(e.contains("[vision].model is unset"), "{e}");

        let e = vision_route(&with(Some("custom:openrouter"), Some(""))).unwrap_err();
        assert!(e.contains("[vision].model is unset"), "empty is unset: {e}");

        let e = vision_route(&with(None, None)).unwrap_err();
        assert!(e.contains("no [vision] route"), "{e}");

        // No error text may recommend a model this tree picked. Which
        // model can see is a fact about the operator's account, and the
        // hard-coded guess is exactly what produced the music model.
        for cfg in [with(Some("p"), None), with(None, None)] {
            let e = vision_route(&cfg).unwrap_err();
            assert!(
                !e.contains("lyria"),
                "the advice must not name the music model: {e}"
            );
            assert!(
                e.contains("/defaults vision"),
                "it must name the command that writes it instead: {e}"
            );
        }
    }

    /// And no code path anywhere still reaches for that id.
    #[test]
    fn the_music_model_is_not_named_in_this_module_at_all() {
        let src = include_str!("vision.rs");
        // Two mentions are allowed and both are prose ABOUT the bug: this
        // test's own name for it, and the doc comment on `vision_route`.
        // Built from parts so this line is not itself a mention.
        let needle = format!("{}-{}", "lyria-3", "pro-preview");
        let hits = src.matches(&needle).count();
        assert!(
            hits <= 2,
            "{hits} mentions of the music model — a fallback or a \
             recommendation has come back"
        );
    }

    /// The image fetch is bounded.
    ///
    /// Pinned by SHAPE rather than by driving a wire, and the reason is
    /// the guard two tests above: `urlguard` refuses loopback for every
    /// caller, so a local server serving an oversized body cannot be
    /// reached from this path at all. Between "assert on the source" and
    /// "weaken the SSRF guard to stage a test", the first is the trade
    /// worth making.
    ///
    /// What it protects: `resolve_image` fetches a URL the CALLER chose
    /// — a model's tool argument, or a link a stranger pasted in a
    /// channel — and `bytes()` read the whole body with no bound, which
    /// base64 then inflated by a third into the request body. The cap is
    /// not about tokens (a vision model tiles and downsamples the image,
    /// and the provider's own body limit binds long before 64 MiB); it
    /// is about not turning one link into an OOM in the process that
    /// also holds every chat and the IRC connection.
    #[test]
    fn the_image_fetch_is_capped_and_not_a_bare_read() {
        let src = include_str!("vision.rs");
        let f = src
            .split("async fn resolve_image(")
            .nth(1)
            .expect("resolve_image");
        let f = &f[..f.find("\nfn ").unwrap_or(f.len())];
        assert!(
            f.contains("read_capped(resp"),
            "the remote branch must read through the cap: {f}"
        );
        assert!(
            !f.contains("resp.bytes()"),
            "an unbounded body read has come back: {f}"
        );
    }
}

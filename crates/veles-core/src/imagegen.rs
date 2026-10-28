//! Image/video generation — ComfyUI-backed generation tool.
//!
//! #36 in the feature matrix. Generates images and video via the local
//! ComfyUI instance (already configured on this machine via the comfyui
//! skill and Nix flake). The tool sends a prompt to ComfyUI's API,
//! polls for completion, downloads the result, and returns a MEDIA: path.
//!
//! This is NOT a FLUX/DALL-E wrapper — it uses the local ComfyUI
//! instance and its installed models. The user has explicitly chosen
//! ComfyUI over cloud APIs.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::Tool;

pub struct ImageGenTool;

#[async_trait]
impl Tool for ImageGenTool {
    fn name(&self) -> &str {
        "generate_image"
    }
    fn description(&self) -> &str {
        "Generate an image via the configured image route: the local ComfyUI instance, or an image-capable provider endpoint (OpenAI-compatible /images/generations, or DashScope-native when [imagegen].api = \"dashscope\"). Provide a text prompt and optional negative prompt, dimensions, steps, and CFG. Returns a MEDIA: path to the generated image."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "Positive prompt for image generation"},
                "provider": {"type": "string", "description": "Route this one call to a configured provider by name (`comfyui` for the local instance). Default: the [imagegen] section, else comfyui"},
                "negative_prompt": {"type": "string", "description": "Negative prompt (what to avoid)"},
                "width": {"type": "integer", "description": "Image width (default 1024)"},
                "height": {"type": "integer", "description": "Image height (default 1024)"},
                "steps": {"type": "integer", "description": "Sampling steps (default 20) — ComfyUI route only"},
                "cfg": {"type": "number", "description": "CFG scale (default 7.0) — ComfyUI route only"},
                "seed": {"type": "integer", "description": "Random seed (-1 = random) — ComfyUI route only"},
                "sampler": {"type": "string", "description": "Sampler name (default 'euler') — ComfyUI route only"},
                "model": {"type": "string", "description": "ComfyUI route: checkpoint name. Endpoint route: the provider's model id (default: [imagegen].model, else the provider entry's own model)"},
                "batch_size": {"type": "integer", "description": "Number of images (default 1, max 4)"}
            },
            "required": ["prompt"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let prompt = args["prompt"].as_str().unwrap_or_default().to_string();
        if prompt.is_empty() {
            return "error: prompt is required".into();
        }

        // ── routing ──
        //
        // USER, 2026-09-11: "not just comfyui, we have
        // image-generation-capable endpoints." The tool was born as the
        // local ComfyUI and that stays the default; a `[imagegen]`
        // provider (or a per-call `provider` argument, which is what
        // `/gen image <name>/...` feeds) sends the call to that
        // endpoint's OpenAI-compatible /images/generations instead.
        // Per-call beats config beats the local instance, so one
        // default and one-off detours coexist.
        let cfg = match crate::config::load() {
            Ok(c) => c,
            Err(e) => return format!("generate_image error: config load: {e}"),
        };
        match resolve_route(&cfg, args["provider"].as_str(), args["model"].as_str()) {
            Ok(Route::Comfyui) => generate_via_comfyui(prompt, &args).await,
            Ok(Route::Endpoint { entry, model, api }) => {
                // The sampling knobs are ComfyUI concepts; say so rather
                // than silently dropping them.
                let mut note = String::new();
                for (flag, what) in [
                    ("steps", "steps"),
                    ("cfg", "cfg"),
                    ("sampler", "sampler"),
                    ("seed", "seed"),
                ] {
                    if !args[flag].is_null() {
                        note.push_str(&format!(" ({what} applies to the comfyui route only)"));
                        break;
                    }
                }
                match generate_via_endpoint(&entry, &model, api, &prompt, &args).await {
                    Ok(path) => {
                        if note.is_empty() {
                            path
                        } else {
                            format!("{path}\n{note}")
                        }
                    }
                    Err(e) => format!("generate_image error: {e}"),
                }
            }
            Err(e) => format!("generate_image error: {e}"),
        }
    }
}

/// Where one image call goes.
#[derive(Debug, Clone)]
pub enum Route {
    /// The local ComfyUI instance (`COMFYUI_URL`).
    Comfyui,
    /// A configured provider's image endpoint, with the model id and
    /// wire shape resolved for it.
    Endpoint {
        entry: Box<crate::config::ProviderEntry>,
        model: String,
        api: EndpointApi,
    },
}

/// The two image-API wire shapes a configured provider can speak.
/// "openai" (`{base}/images/generations`, `data[]` back) is what every
/// aggregator grew; "dashscope" is Aliyun's native synchronous
/// multimodal-generation route — a different body, a different path,
/// and a different failure when you guess: the compat base answers an
/// `/images/generations` POST with a bare, bodyless 404 (discovered
/// live, from inside someone else's `/gen` attempt).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointApi {
    Openai,
    Dashscope,
}

impl EndpointApi {
    fn parse(field: &str, s: Option<&str>) -> Result<Self, String> {
        match s.map(str::trim).filter(|s| !s.is_empty()) {
            None | Some("openai") => Ok(EndpointApi::Openai),
            Some("dashscope") => Ok(EndpointApi::Dashscope),
            Some(other) => Err(format!(
                "{field} '{other}' is not a wire shape — known: openai, dashscope"
            )),
        }
    }

    /// The shape for one provider: the explicit config knob when set,
    /// otherwise INFERRED from the base_url — a `/compatible-mode` or
    /// `maas.`/`dashscope` host is DashScope's, and nothing else speaks
    /// its native paths. Inference is what makes `/gen --provider`
    /// switches work without config edits: the wire shape belongs to
    /// the provider, not to a global preference.
    pub(crate) fn resolve(
        field: &str,
        explicit: Option<&str>,
        base_url: &str,
    ) -> Result<Self, String> {
        if explicit.map(str::trim).filter(|s| !s.is_empty()).is_some() {
            return Self::parse(field, explicit);
        }
        Ok(Self::infer(base_url))
    }

    pub(crate) fn infer(base_url: &str) -> Self {
        let b = base_url.to_ascii_lowercase();
        if b.contains("compatible-mode") || b.contains("maas.") || b.contains("dashscope") {
            EndpointApi::Dashscope
        } else {
            EndpointApi::Openai
        }
    }
}

/// The native DashScope API base (`https://host/api/v1`) for a
/// provider's `base_url`, which may be written either way: the
/// OpenAI-compatible `.../compatible-mode/v1` (what chat routing
/// stores) or the native `.../api/v1`. The compat path does NOT carry
/// the media routes (it answers them bare 404), so the mapping has to
/// be exact — and appending to an already-native base must not double
/// it.
pub(crate) fn dashscope_native_base(base_url: &str) -> String {
    let b = base_url.trim_end_matches('/');
    if let Some(stripped) = b.strip_suffix("/compatible-mode/v1") {
        stripped.to_string() + "/api/v1"
    } else if b.ends_with("/api/v1") {
        b.to_string()
    } else {
        b.to_string() + "/api/v1"
    }
}

/// The native DashScope IMAGE generation URL for a provider's base.
fn dashscope_url(base_url: &str) -> String {
    dashscope_url_for(base_url, "aigc/multimodal-generation/generation")
}

/// A DashScope-native service URL: `{host}/api/v1/services/{tail}`.
/// Shared by image, video and ASR so the base-mapping rule is written
/// once.
pub(crate) fn dashscope_url_for(base_url: &str, tail: &str) -> String {
    format!("{}/services/{}", dashscope_native_base(base_url), tail)
}

/// Resolve an endpoint route generically for one `[imagegen]`/
/// `[videogen]`/`[asr]`-shaped section. `local_default` is the name
/// that means "no provider configured" (ComfyUI for image/video; ASR
/// passes `None`, so an unset provider is an error). The model falls
/// back: per-call > section.model > the provider entry's own `model` >
/// an error naming the choice — never a guess, because an endpoint with
/// an unnamable model is a 400 with a bill attached.
#[allow(clippy::type_complexity)]
pub(crate) fn resolve_endpoint(
    cfg: &crate::config::VelesConfig,
    section: &crate::config::GenSection,
    field_prefix: &str,
    provider_arg: Option<&str>,
    model_arg: Option<&str>,
    local_default: Option<&str>,
) -> Result<Option<(crate::config::ProviderEntry, String, EndpointApi)>, String> {
    let provider = provider_arg
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(section.provider.as_deref())
        .or(local_default)
        .map(|s| s.to_string());

    let provider = match provider {
        Some(p) => p,
        None => {
            let known: Vec<String> = cfg
                .custom_providers
                .iter()
                .map(|p| p.name.clone())
                .collect();
            return Err(format!(
                "no {field_prefix} provider set — configure `{field_prefix}.provider` or pass one; known: {}",
                known.join(", ")
            ));
        }
    };

    if provider.eq_ignore_ascii_case("comfyui") {
        return Ok(None);
    }

    let entry = cfg.provider(&provider).cloned().ok_or_else(|| {
        let mut known: Vec<String> = cfg
            .custom_providers
            .iter()
            .map(|p| p.name.clone())
            .collect();
        if local_default.is_some() {
            known.insert(0, "comfyui".to_string());
        }
        format!(
            "provider '{provider}' is not configured — known routes: {}",
            known.join(", ")
        )
    })?;

    let model = model_arg
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(section.model.as_deref())
        .or(entry.model.as_deref())
        .ok_or_else(|| {
            format!(
                "provider '{provider}' has no model: set {field_prefix}.model or name one per call"
            )
        })?
        .to_string();

    // `{field_prefix}.api` describes the section's OWN provider. A
    // provider named per call speaks its own shape, read from its base:
    // the knob applied to every route sent a `/gen` on an OpenAI-shaped
    // provider to DashScope's native path, which it does not have.
    let own = section
        .provider
        .as_deref()
        .is_some_and(|d| d.eq_ignore_ascii_case(&provider));
    let api = EndpointApi::resolve(
        &format!("{field_prefix}.api"),
        section.api.as_deref().filter(|_| own),
        &entry.base_url,
    )?;
    Ok(Some((entry, model, api)))
}

/// Resolve the image route: per-call `provider`, then `[imagegen].provider`,
/// then the local ComfyUI (`comfyui` is a name for it, so the shorthand
/// works in both directions).
pub(crate) fn resolve_route(
    cfg: &crate::config::VelesConfig,
    provider_arg: Option<&str>,
    model_arg: Option<&str>,
) -> Result<Route, String> {
    match resolve_endpoint(
        cfg,
        &cfg.imagegen,
        "[imagegen]",
        provider_arg,
        model_arg,
        Some("comfyui"),
    )? {
        None => Ok(Route::Comfyui),
        Some((entry, model, api)) => Ok(Route::Endpoint {
            entry: Box::new(entry),
            model,
            api,
        }),
    }
}

/// The names `/gen image --list` (and the video lister) quote: every
/// configured provider plus the local instance, with the model and the
/// INFERRED wire shape each would use. `local_label` is the line for
/// the no-provider fallback (None for ASR, which has no local route).
pub fn gen_listing(
    cfg: &crate::config::VelesConfig,
    section: &crate::config::GenSection,
    field_prefix: &str,
    local_label: Option<&str>,
) -> String {
    let mut out = match local_label {
        Some(l) => l.to_string(),
        None => String::new(),
    };
    for p in &cfg.custom_providers {
        // The resolver a call uses, so the listing cannot name a model or
        // a wire shape the call would not use. It once preferred each
        // provider's CHAT model to `{field_prefix}.model`, the reverse of
        // the call, and listed `qwen-image-3.0-pro` only beside a
        // provider whose key lacks it.
        let (model, api) =
            match resolve_endpoint(cfg, section, field_prefix, Some(&p.name), None, None) {
                Ok(Some((_, m, api))) => (format!(" — {m}"), api),
                Ok(None) => continue,
                Err(e) if e.contains("has no model") => (
                    " — no model set".to_string(),
                    EndpointApi::infer(&p.base_url),
                ),
                Err(e) => (format!(" — {e}"), EndpointApi::infer(&p.base_url)),
            };
        let is_default = section.provider.as_deref() == Some(p.name.as_str());
        let api_note = match api {
            EndpointApi::Openai => "",
            EndpointApi::Dashscope => "  [dashscope]",
        };
        out.push_str(&format!(
            "{}\n  {}{model}{}{}",
            if out.is_empty() { "" } else { "\n  " },
            p.name,
            if is_default { "  (default)" } else { "" },
            api_note
        ));
    }
    if section.provider.is_none() {
        match local_label {
            Some(_) => out.push_str("\n  default: comfyui"),
            None => out.push_str(&format!("\n  default: unset — set {field_prefix}.provider")),
        }
    }
    out
}

/// The names `/gen image --list` and the errors above quote: every
/// configured provider plus the local instance, with the model each
/// would use.
pub fn route_listing(cfg: &crate::config::VelesConfig) -> String {
    gen_listing(
        cfg,
        &cfg.imagegen,
        "[imagegen]",
        Some("comfyui (local ComfyUI, COMFYUI_URL)"),
    )
}

/// The most we will read from a provider's body.
///
/// Every media fetch here reads a URL the PROVIDER chose — the hosted
/// image, the finished clip, the synthesized audio — and `bytes()` reads
/// it all into memory with no bound. A provider that is hostile, broken,
/// or simply pointed at the wrong object is then an OOM in the gateway
/// process, which is also the IRC connection and every other chat. 64
/// MiB is past any picture, clip or waveform a model produces and is the
/// same number IRC already refuses to publish above.
pub(crate) const MAX_FETCH_BYTES: usize = 64 * 1024 * 1024;

/// Read a response body, refusing past [`MAX_FETCH_BYTES`].
///
/// Chunked rather than `bytes()` so the cap bounds what is ALLOCATED
/// rather than what is reported: `Content-Length` is the server's claim
/// about itself, so it is used only as an early no, never as the reason
/// to stop reading.
pub(crate) async fn read_capped(
    mut resp: reqwest::Response,
    what: &str,
) -> Result<Vec<u8>, String> {
    if let Some(len) = resp.content_length() {
        if len > MAX_FETCH_BYTES as u64 {
            return Err(format!(
                "{what} is {len} bytes, over the {MAX_FETCH_BYTES}-byte limit"
            ));
        }
    }
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if out.len() + chunk.len() > MAX_FETCH_BYTES {
            return Err(format!(
                "{what} exceeded the {MAX_FETCH_BYTES}-byte limit mid-transfer"
            ));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Save one generated image under the media root's `generated/` —
/// because that root is the delivery contract: an image written
/// anywhere else is an image only its maker could ever be sent.
///
/// When the caller knows the prompt, write a `<file>.alt` sidecar holding
/// it: the prompt is the best descriptive ALT text a downstream surface
/// (Bluesky embed, a screen reader) could ask for, and the bare filename
/// (`gen_…png`) is useless as alt (USER 2026-10-05). A surface that can
/// read the sidecar uses it; one that can't falls back to the filename.
fn save_generated_alt(bytes: &[u8], alt: Option<&str>) -> Result<String, String> {
    let ext = if bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        "jpg"
    } else if bytes.len() > 8 && &bytes[4..8] == b"ftyp" {
        "mp4"
    } else {
        "png"
    };
    let dir = crate::config::media_dir().join("generated");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!(
        "gen_{}.{ext}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    if let Some(a) = alt {
        let a = a.trim();
        if !a.is_empty() {
            let _ = std::fs::write(path.with_extension(format!("{ext}.alt")), a);
        }
    }
    Ok(format!("MEDIA:{}", path.display()))
}

/// Back-compat wrapper: no alt text known (video path, older callers).
fn save_generated(bytes: &[u8]) -> Result<String, String> {
    save_generated_alt(bytes, None)
}

/// One generation through an image endpoint, in one of the two wire
/// shapes a configured provider speaks.
///
/// Accepts every response shape in the wild: `b64_json` inline, a
/// hosted `url` to fetch, and DashScope's synchronous answer (a URL
/// inside `output.choices[].message.content[]`). The hosted URLs
/// EXPIRE — minutes on DashScope — so fetching here is not a
/// convenience; deferring to whoever reads the reply loses the image.
async fn generate_via_endpoint(
    entry: &crate::config::ProviderEntry,
    model: &str,
    api: EndpointApi,
    prompt: &str,
    args: &Value,
) -> Result<String, String> {
    let key_env = entry.key_env.as_deref().unwrap_or("API_KEY");
    // Same leniency as vision: the value may be the env var NAME or a
    // literal key (a `custom:` route pasted from a provider console).
    // An unset NAME is an error now, not the key.
    let api_key = crate::secrets::resolve_key_env(key_env)?;

    let width = args["width"].as_u64().unwrap_or(1024);
    let height = args["height"].as_u64().unwrap_or(1024);
    let batch = args["batch_size"].as_u64().unwrap_or(1).clamp(1, 4);
    let negative = args["negative_prompt"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let (url, body) = match api {
        EndpointApi::Openai => {
            let mut body = serde_json::json!({"model": model, "prompt": prompt});
            if !args["width"].is_null() || !args["height"].is_null() {
                body["size"] = format!("{width}x{height}").into();
            }
            if batch > 1 {
                body["n"] = batch.into();
            }
            if let Some(neg) = negative {
                // OpenAI's shape has no negative prompt; the common
                // convention is a body extension, and providers that do not
                // know it ignore it.
                body["negative_prompt"] = neg.into();
            }
            (
                format!(
                    "{}/images/generations",
                    entry.base_url.trim_end_matches('/')
                ),
                body,
            )
        }
        EndpointApi::Dashscope => {
            let mut params = serde_json::json!({
                "size": format!("{width}*{height}"),
                "n": batch,
                "watermark": false,
            });
            if let Some(neg) = negative {
                params["negative_prompt"] = neg.into();
            }
            (
                dashscope_url(&entry.base_url),
                serde_json::json!({
                    "model": model,
                    "input": {"messages": [{"role": "user", "content": [{"text": prompt}]}]},
                    "parameters": params,
                }),
            )
        }
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        // The URL rides the error. A bare 404 with no body — what a
        // compat base answers when it carries no image route at all —
        // is unactionable without it, and that exact 404 is how the
        // dashscope shape was discovered live.
        return Err(format!("HTTP {status} from {url}: {text}"));
    }
    let json: Value = resp.json().await.map_err(|e| e.to_string())?;

    // One collection of sources, whatever the shape: inline base64 or
    // a (short-lived) URL to fetch now.
    let mut b64s: Vec<&str> = Vec::new();
    let mut urls: Vec<&str> = Vec::new();
    let empty: Vec<Value> = Vec::new();
    match api {
        EndpointApi::Openai => {
            for item in json["data"].as_array().unwrap_or(&empty).iter() {
                if let Some(b) = item["b64_json"].as_str() {
                    b64s.push(b);
                } else if let Some(u) = item["url"].as_str() {
                    urls.push(u);
                } else {
                    return Err("endpoint returned neither b64_json nor url".into());
                }
            }
        }
        EndpointApi::Dashscope => {
            if let Some(task_id) = json["output"]["task_id"].as_str() {
                // The async submit shape arrives with an async header
                // we never send; if a provider answers this way anyway,
                // say so rather than reporting "no images".
                return Err(format!(
                    "provider returned an async task ({task_id}); this route speaks only the synchronous shape"
                ));
            }
            for choice in json["output"]["choices"]
                .as_array()
                .unwrap_or(&empty)
                .iter()
            {
                for part in choice["message"]["content"]
                    .as_array()
                    .unwrap_or(&empty)
                    .iter()
                {
                    if let Some(u) = part["image"].as_str() {
                        urls.push(u);
                    }
                }
            }
        }
    }
    if b64s.is_empty() && urls.is_empty() {
        let why = json["message"].as_str().unwrap_or("empty response");
        return Err(format!("endpoint returned no images: {why}"));
    }

    let mut images_out = Vec::new();
    for b64 in b64s {
        let bytes =
            crate::vision::base64_decode(b64).ok_or("endpoint returned an undecodable b64_json")?;
        images_out.push(save_generated_alt(&bytes, Some(prompt))?);
    }
    for u in urls {
        let resp = client
            .get(u)
            .send()
            .await
            .map_err(|e| format!("fetching the hosted image: {e}"))?;
        let bytes = read_capped(resp, "the hosted image").await?;
        images_out.push(save_generated_alt(&bytes, Some(prompt))?);
    }
    Ok(images_out.join("\n"))
}

async fn generate_via_comfyui(prompt: String, args: &Value) -> String {
    let base_url =
        std::env::var("COMFYUI_URL").unwrap_or_else(|_| "http://127.0.0.1:8188".to_string());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Check ComfyUI is reachable
    let health = client.get(format!("{base_url}/system_stats")).send().await;
    if health.is_err() {
        return format!(
            "generate_image error: ComfyUI not reachable at {base_url}. \
                 Start it with: nix run .#comfyui (or check COMFYUI_URL env)"
        );
    }

    let width = args["width"].as_u64().unwrap_or(1024) as u32;
    let height = args["height"].as_u64().unwrap_or(1024) as u32;
    let steps = args["steps"].as_u64().unwrap_or(20) as u32;
    let cfg = args["cfg"].as_f64().unwrap_or(7.0);
    let seed = args["seed"].as_i64().unwrap_or(-1);
    let sampler = args["sampler"].as_str().unwrap_or("euler");
    let model_name = args["model"]
        .as_str()
        .unwrap_or("sd_xl_base_1.0.safetensors");
    let batch = args["batch_size"].as_u64().unwrap_or(1).clamp(1, 4) as u32;
    let negative = args["negative_prompt"].as_str().unwrap_or("");

    // Build a standard ComfyUI API workflow (txt2img)
    let actual_seed = if seed < 0 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64
    } else {
        seed
    };

    let workflow = serde_json::json!({
        "3": {
            "class_type": "KSampler",
            "inputs": {
                "seed": actual_seed,
                "steps": steps,
                "cfg": cfg,
                "sampler_name": sampler,
                "scheduler": "normal",
                "denoise": 1.0,
                "model": ["4", 0],
                "positive": ["6", 0],
                "negative": ["7", 0],
                "latent_image": ["5", 0]
            }
        },
        "4": {
            "class_type": "CheckpointLoaderSimple",
            "inputs": {"ckpt_name": model_name}
        },
        "5": {
            "class_type": "EmptyLatentImage",
            "inputs": {"width": width, "height": height, "batch_size": batch}
        },
        "6": {
            "class_type": "CLIPTextEncode",
            "inputs": {"text": prompt, "clip": ["4", 1]}
        },
        "7": {
            "class_type": "CLIPTextEncode",
            "inputs": {"text": negative, "clip": ["4", 1]}
        },
        "8": {
            "class_type": "VAEDecode",
            "inputs": {"samples": ["3", 0], "vae": ["4", 2]}
        },
        "9": {
            "class_type": "SaveImage",
            "inputs": {"images": ["8", 0]}
        }
    });

    // Queue the prompt
    let queue_resp: Value = match client
        .post(format!("{base_url}/prompt"))
        .json(&serde_json::json!({"prompt": workflow}))
        .send()
        .await
    {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(e) => return format!("generate_image error: failed to queue: {e}"),
    };

    let prompt_id = queue_resp["prompt_id"].as_str().unwrap_or("").to_string();
    if prompt_id.is_empty() {
        return format!("generate_image error: ComfyUI returned no prompt_id: {queue_resp}");
    }

    // Poll for completion (up to 5 minutes)
    let mut elapsed = 0u64;
    loop {
        if elapsed > 300 {
            return format!("generate_image error: timeout (300s) waiting for {prompt_id}");
        }

        let status: Value = match client
            .get(format!("{base_url}/history/{prompt_id}"))
            .send()
            .await
        {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => Value::Null,
        };

        // Check if the prompt completed
        if let Some(outputs) = status.get(&prompt_id).and_then(|p| p.get("outputs")) {
            if let Some(node9) = outputs.get("9") {
                if let Some(images) = node9["images"].as_array() {
                    if !images.is_empty() {
                        // Download the first image
                        let filename = images[0]["filename"].as_str().unwrap_or("output.png");
                        let subfolder = images[0]["subfolder"].as_str().unwrap_or("");
                        let img_url = format!(
                            "{base_url}/view?filename={filename}&subfolder={subfolder}&type=output"
                        );

                        let img_resp = match client.get(&img_url).send().await {
                            Ok(r) => r,
                            Err(e) => return format!("generate_image error: download failed: {e}"),
                        };

                        let bytes = match img_resp.bytes().await {
                            Ok(b) => b,
                            Err(e) => return format!("generate_image error: read failed: {e}"),
                        };

                        // Under the media root: that root is the
                        // delivery contract (`media::deliverable`), so
                        // an image written anywhere else is an image
                        // only its maker could ever be sent.
                        let img_dir = crate::config::media_dir().join("generated");
                        let _ = std::fs::create_dir_all(&img_dir);
                        let out_path = img_dir.join(format!(
                            "{}_{}.png",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis(),
                            actual_seed
                        ));
                        std::fs::write(&out_path, bytes).ok();
                        // The prompt is the descriptive alt text for this
                        // image; write the sidecar a downstream surface reads.
                        let _ = std::fs::write(out_path.with_extension("png.alt"), prompt.trim());

                        return format!(
                            "MEDIA:{}\n(seed: {actual_seed}, {width}x{height}, {steps} steps)",
                            out_path.display()
                        );
                    }
                }
            }
        }

        // Check for execution error
        if let Some(status_obj) = status.get(&prompt_id) {
            if let Some(status) = status_obj.get("status") {
                if status
                    .get("status_str")
                    .and_then(|s| s.as_str())
                    .map(|s| s.contains("error"))
                    .unwrap_or(false)
                {
                    return format!("generate_image error: ComfyUI execution failed: {status}");
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        elapsed += 3;
    }
}

// ── Video generation ─────────────────────────────────────────────────

pub struct VideoGenTool;

/// One video through the DashScope-native route: submit (the async
/// flag is MANDATORY — without it the host answers
/// "current user api does not support asynchronous calls"), poll the
/// shared task endpoint until a terminal state, then fetch the result
/// URL IMMEDIATELY: it expires in hours, and a video nobody fetched is
/// a video that was billed and lost. Live shape, probed the day this
/// shipped: PENDING → minutes of RUNNING → SUCCEEDED with
/// `output.video_url`, ~4 min for 5s at 1080p.
async fn generate_video_via_dashscope(
    entry: &crate::config::ProviderEntry,
    model: &str,
    prompt: &str,
    args: &Value,
) -> Result<String, String> {
    // Minutes per job is normal here, and a caller that cannot wait
    // 15 minutes should not submit at all. The timings are parameters
    // so the poll loop is testable against a local listener.
    generate_video_via_dashscope_timed(
        entry,
        model,
        prompt,
        args,
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(900),
    )
    .await
}

async fn generate_video_via_dashscope_timed(
    entry: &crate::config::ProviderEntry,
    model: &str,
    prompt: &str,
    args: &Value,
    poll_interval: std::time::Duration,
    poll_budget: std::time::Duration,
) -> Result<String, String> {
    let key_env = entry.key_env.as_deref().unwrap_or("API_KEY");
    let api_key = crate::secrets::resolve_key_env(key_env)?;
    let native = dashscope_native_base(&entry.base_url);
    let auth = format!("Bearer {api_key}");

    let mut input = serde_json::json!({"prompt": prompt});
    if let Some(img) = args["input_image"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if img.starts_with("http://") || img.starts_with("https://") {
            input["img_url"] = img.into();
        } else {
            // The i2v/r2v models fetch the image server-side; a local
            // path is unreachable to them, and silently base64-stuffing
            // an unknown-size file into a field shaped for URLs is a
            // 400 someone else has to debug.
            return Err(
                "input_image must be a public http(s) URL — the provider fetches it; a local path is unreachable to it"
                    .into(),
            );
        }
    }
    let mut parameters = serde_json::Map::new();
    if !args["width"].is_null() && !args["height"].is_null() {
        parameters.insert(
            "size".into(),
            format!("{}*{}", args["width"], args["height"]).into(),
        );
    }
    if let Some(d) = args["duration"].as_u64() {
        parameters.insert("duration".into(), d.into());
    }
    let body = serde_json::json!({
        "model": model,
        "input": input,
        "parameters": parameters,
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| e.to_string())?;
    let submit_url = format!("{native}/services/aigc/video-generation/video-synthesis");
    let resp = client
        .post(&submit_url)
        .header("Authorization", &auth)
        .header("X-DashScope-Async", "enable")
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status} from {submit_url}: {text}"));
    }
    let json: Value = resp.json().await.map_err(|e| e.to_string())?;
    let task_id = json["output"]["task_id"]
        .as_str()
        .ok_or_else(|| format!("no task_id in submit response: {json}"))?
        .to_string();
    tracing::info!("generate_video: dashscope task {task_id} submitted on '{model}'");

    let poll_url = format!("{native}/tasks/{task_id}");
    let deadline = std::time::Instant::now() + poll_budget;
    loop {
        if std::time::Instant::now() >= deadline {
            // The task keeps running and its result URL will exist for
            // hours — name both instead of swallowing the spend.
            return Err(format!(
                "task {task_id} did not finish within 15 minutes; poll {poll_url} — the result URL, once present, expires within hours"
            ));
        }
        tokio::time::sleep(poll_interval).await;
        let r = client
            .get(&poll_url)
            .header("Authorization", &auth)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            return Err(format!(
                "task {task_id}: HTTP {status} from {poll_url}: {text}"
            ));
        }
        let r: Value = r.json().await.map_err(|e| e.to_string())?;
        match r["output"]["task_status"].as_str().unwrap_or("") {
            "SUCCEEDED" => {
                let url = r["output"]["video_url"]
                    .as_str()
                    .or_else(|| {
                        r["output"]["results"]
                            .get(0)
                            .and_then(|v| v["video_url"].as_str())
                    })
                    .ok_or_else(|| format!("task {task_id} SUCCEEDED with no video_url: {r}"))?;
                let resp = client
                    .get(url)
                    .send()
                    .await
                    .map_err(|e| format!("fetching the finished video: {e}"))?;
                let bytes = read_capped(resp, "the finished video").await?;
                return save_generated(&bytes);
            }
            "FAILED" | "CANCELED" | "UNKNOWN" => {
                let status = r["output"]["task_status"].as_str().unwrap_or("?");
                let code = r["output"]["code"].as_str().unwrap_or("");
                let msg = r["output"]["message"].as_str().unwrap_or("");
                return Err(format!("task {task_id} {status}: {code}: {msg}"));
            }
            // PENDING / RUNNING: keep waiting.
            _ => continue,
        }
    }
}

#[async_trait]
impl Tool for VideoGenTool {
    fn name(&self) -> &str {
        "generate_video"
    }
    fn description(&self) -> &str {
        "Generate a short video: routed to a configured provider endpoint when one is set ([videogen], DashScope async task — happyhorse t2v/i2v models; the i2v/r2v models take input_image as a public URL), otherwise the local ComfyUI workflow placeholder. Returns a MEDIA: path."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "Text prompt for video"},
                "input_image": {"type": "string", "description": "Public http(s) URL of a starting frame (for the i2v models)"},
                "provider": {"type": "string", "description": "Route this one call to a configured provider by name (default: [videogen].provider; unset = comfyui)"},
                "model": {"type": "string", "description": "Video model id (default: [videogen].model)"},
                "duration": {"type": "integer", "description": "Seconds of video (provider-bounded)"},
                "width": {"type": "integer", "description": "Width"},
                "height": {"type": "integer", "description": "Height"},
                "frames": {"type": "integer", "description": "Number of frames (comfyui route only)"},
                "fps": {"type": "integer", "description": "Frames per second (comfyui route only)"}
            },
            "required": ["prompt"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let prompt = args["prompt"].as_str().unwrap_or_default().to_string();
        if prompt.is_empty() {
            return "error: prompt is required".into();
        }
        let cfg = match crate::config::load() {
            Ok(c) => c,
            Err(e) => return format!("generate_video error: config load: {e}"),
        };
        match resolve_endpoint(
            &cfg,
            &cfg.videogen,
            "[videogen]",
            args["provider"].as_str(),
            args["model"].as_str(),
            Some("comfyui"),
        ) {
            Ok(None) => format!(
                "Video generation on the local ComfyUI needs a workflow JSON. \
                 Please load a video workflow (AnimateDiff, SVD, etc.) \
                 via ComfyUI directly, or configure [videogen] to route the \
                 prompt to a video-capable provider endpoint.\n\n\
                 Prompt received: {prompt}"
            ),
            Ok(Some((entry, model, api))) => match api {
                EndpointApi::Openai => "generate_video error: the OpenAI-compatible shape has no video route — set [videogen].api = \"dashscope\" for providers that speak video natively".into(),
                EndpointApi::Dashscope => {
                    match generate_video_via_dashscope(&entry, &model, &prompt, &args).await {
                        Ok(path) => path,
                        Err(e) => format!("generate_video error: {e}"),
                    }
                }
            },
            Err(e) => format!("generate_video error: {e}"),
        }
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::config::{GenSection, ProviderEntry, VelesConfig};

    fn cfg(provider: Option<&str>, model: Option<&str>) -> VelesConfig {
        VelesConfig {
            imagegen: GenSection {
                provider: provider.map(String::from),
                model: model.map(String::from),
                api: None,
            },
            custom_providers: vec![ProviderEntry {
                name: "router".into(),
                base_url: "http://127.0.0.1:0/v1".into(),
                model: Some("entry-model".into()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn the_route_is_call_config_then_comfyui_in_that_order() {
        // Unset everywhere: the local instance the tool was born as.
        assert!(matches!(
            resolve_route(&cfg(None, None), None, None),
            Ok(Route::Comfyui)
        ));
        // And `comfyui` is a NAME for it, so the shorthand can say what
        // it means even when a config default exists.
        assert!(matches!(
            resolve_route(&cfg(Some("router"), None), Some("comfyui"), None),
            Ok(Route::Comfyui)
        ));
        // Config default routes without a per-call word.
        match resolve_route(&cfg(Some("router"), Some("cfg-model")), None, None) {
            Ok(Route::Endpoint { model, .. }) => assert_eq!(model, "cfg-model"),
            other => panic!("config default must route: {other:?}"),
        }
        // Per-call beats config.
        match resolve_route(
            &cfg(Some("router"), Some("cfg-model")),
            None,
            Some("call-model"),
        ) {
            Ok(Route::Endpoint { model, .. }) => assert_eq!(model, "call-model"),
            other => panic!("call arg must win: {other:?}"),
        }
        // Model falls back to the provider entry's own.
        match resolve_route(&cfg(None, None), Some("router"), None) {
            Ok(Route::Endpoint { model, .. }) => assert_eq!(model, "entry-model"),
            other => panic!("entry model must backstop: {other:?}"),
        }
    }

    #[test]
    fn an_unknown_route_or_a_modelless_one_is_an_error_that_names_the_choice() {
        // A wrong provider name lists the routes that exist; a route
        // with no model anywhere REFUSES rather than guessing — an
        // endpoint with an unnamable model is a 400 with a bill.
        let e = resolve_route(&cfg(None, None), Some("nope"), None).unwrap_err();
        assert!(e.contains("comfyui"), "the local route is named: {e}");
        assert!(e.contains("router"), "and the configured one: {e}");

        let bare = ProviderEntry {
            name: "modelless".into(),
            base_url: "http://127.0.0.1:0/v1".into(),
            ..Default::default()
        };
        let mut c = cfg(None, None);
        c.custom_providers.push(bare);
        let e2 = resolve_route(&c, Some("modelless"), None).unwrap_err();
        assert!(e2.contains("no model"), "{e2}");
    }

    #[test]
    fn the_listing_names_every_route_and_the_default() {
        let text = route_listing(&cfg(Some("router"), None));
        assert!(text.contains("comfyui"), "{text}");
        assert!(text.contains("router"), "{text}");
        assert!(text.contains("(default)"), "{text}");
        assert!(route_listing(&cfg(None, None)).contains("default: comfyui"));
    }

    /// Both endpoint response shapes through a real socket: `b64_json`
    /// inline, and a hosted `url` the generator has to fetch. The file
    /// must land under the media root — that root is the delivery
    /// contract, and an image written elsewhere could never be sent.
    #[tokio::test]
    async fn the_endpoint_route_speaks_both_response_shapes_and_lands_in_the_media_root() {
        let (home, _env) = crate::testenv::scratch_home();
        // The route's key, where an operator keeps one. An unset key is
        // an error now; these tests used to pass by sending the NAME.
        std::fs::write(home.path().join(".env"), "API_KEY=sk-route-test\n").unwrap();
        use base64::Engine as _;
        let png = [0x89, b'P', b'N', b'G', 1, 2, 3, 4];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let png2 = png;

        // Two POSTs (each answered with JSON naming its shape) and, for
        // the url shape, one hosted GET answered with the PNG itself.
        let png_b64 = base64::engine::general_purpose::STANDARD.encode(png);
        let body1 = format!("{{\"data\":[{{\"b64_json\":\"{png_b64}\"}}]}}");
        let body_url = format!("{{\"data\":[{{\"url\":\"http://127.0.0.1:{port}/img.png\"}}]}}");
        let bodies = [body1, body_url];

        let srv = crate::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let mut next = 0usize;
            let mut served = 0usize;
            // 3 connections: two POSTs, one hosted GET.
            while served < 3 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                served += 1;
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                if req.starts_with("GET /img") {
                    let head =
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n";
                    sock.write_all(head.as_bytes()).await.unwrap();
                    sock.write_all(&png2).await.unwrap();
                } else {
                    let body = &bodies[next.min(1)];
                    next += 1;
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
                    sock.write_all(head.as_bytes()).await.unwrap();
                    sock.write_all(body.as_bytes()).await.unwrap();
                }
            }
        });

        let entry = ProviderEntry {
            name: "router".into(),
            base_url: format!("http://127.0.0.1:{port}/v1"),
            ..Default::default()
        };
        // b64 shape
        let out = generate_via_endpoint(
            &entry,
            "m",
            EndpointApi::Openai,
            "a red boat",
            &serde_json::json!({}),
        )
        .await
        .expect("b64 shape must work");
        assert!(out.starts_with("MEDIA:"), "{out}");
        let p1 = out.trim_start_matches("MEDIA:");
        let bytes = std::fs::read(p1).unwrap();
        assert_eq!(bytes[..4], png[..4], "the payload, not an error page");
        assert!(
            p1.replace('\\', "/").contains("media/generated"),
            "under the delivery contract: {p1}"
        );
        // hosted-url shape
        let out2 = generate_via_endpoint(
            &entry,
            "m",
            EndpointApi::Openai,
            "a blue boat",
            &serde_json::json!({}),
        )
        .await
        .expect("url shape must work");
        assert!(out2.starts_with("MEDIA:"), "{out2}");
        // Same order rule as the dashscope test: the fetched bytes are
        // asserted before awaiting, so a skipped fetch fails cleanly.
        let p2 = out2.trim_start_matches("MEDIA:");
        assert_eq!(
            std::fs::read(p2).unwrap()[..4],
            png[..4],
            "the hosted bytes landed"
        );
        srv.await.unwrap();
    }

    #[test]
    fn the_dashscope_url_maps_both_base_spellings_without_doubling() {
        // The base as chat routing stores it (compatible-mode) and the
        // native one must both land on the same generation path — and
        // an already-native base must not grow a second /api/v1.
        let gen = "/services/aigc/multimodal-generation/generation";
        assert_eq!(
            dashscope_url("https://host.example/compatible-mode/v1"),
            format!("https://host.example/api/v1{gen}")
        );
        assert_eq!(
            dashscope_url("https://host.example/api/v1/"),
            format!("https://host.example/api/v1{gen}")
        );
        assert_eq!(
            dashscope_url("https://host.example"),
            format!("https://host.example/api/v1{gen}")
        );
    }

    /// The listing says what a call would do. `/gen image --list` once
    /// named each provider's chat model where the call uses
    /// `[imagegen].model`, and stamped `[dashscope]` on every provider.
    #[test]
    fn the_listing_names_the_model_and_shape_a_call_would_use() {
        let mut c = cfg(Some("router"), Some("img-model"));
        c.imagegen.api = Some("dashscope".into());
        c.custom_providers.push(ProviderEntry {
            name: "plain".into(),
            base_url: "https://api.example/v1".into(),
            model: Some("chat-model".into()),
            ..Default::default()
        });
        let text = route_listing(&c);
        assert!(
            text.contains("router — img-model  (default)  [dashscope]"),
            "{text}"
        );
        assert!(
            text.contains("plain — img-model\n") || text.ends_with("plain — img-model"),
            "{text}"
        );
        assert!(
            !text.contains("chat-model") && !text.contains("entry-model"),
            "{text}"
        );
        // The section's `api` is its own provider's; another provider
        // named per call keeps the shape of its base.
        let shape = |name: &str| match resolve_route(&c, Some(name), None) {
            Ok(Route::Endpoint { api, .. }) => api,
            other => panic!("{other:?}"),
        };
        assert_eq!(shape("router"), EndpointApi::Dashscope);
        assert_eq!(shape("plain"), EndpointApi::Openai);
    }

    #[test]
    fn the_wire_shape_knob_accepts_two_names_and_no_guessing() {
        assert_eq!(
            EndpointApi::parse("[imagegen].api", None).unwrap(),
            EndpointApi::Openai
        );
        assert_eq!(
            EndpointApi::parse("[imagegen].api", Some("dashscope")).unwrap(),
            EndpointApi::Dashscope
        );
        let e = EndpointApi::parse("[imagegen].api", Some("grok-images")).unwrap_err();
        assert!(e.contains("openai") && e.contains("dashscope"), "{e}");
        // And resolve refuses BEFORE any request is made or billed.
        let mut c = cfg(Some("router"), Some("m"));
        c.imagegen.api = Some("grok-images".into());
        assert!(resolve_route(&c, None, None)
            .unwrap_err()
            .contains("grok-images"));
    }

    /// The native shape end to end through a real socket: a POST to the
    /// `/api/v1` path (NOT the compat base's, which 404s — discovered
    /// live), an `input.messages` body with `W*H` sizes, and a response
    /// URL that expires in minutes, so the generator must fetch it now.
    #[tokio::test]
    async fn the_dashscope_route_posts_the_native_shape_and_fetches_the_expiring_url() {
        let (home, _env) = crate::testenv::scratch_home();
        // The route's key, where an operator keeps one. An unset key is
        // an error now; these tests used to pass by sending the NAME.
        std::fs::write(home.path().join(".env"), "API_KEY=sk-route-test\n").unwrap();
        let png = [0x89, b'P', b'N', b'G', 9, 8, 7, 6];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let png2 = png;
        let srv = crate::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let head_end = req.find("\r\n\r\n").unwrap_or(0);
            let (head, body) = req.split_at(head_end);
            let ok =
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
            sock.write_all(ok.as_bytes()).await.unwrap();
            sock.write_all(
                format!(
                    "{{\"output\":{{\"choices\":[{{\"message\":{{\"content\":[{{\"image\":\"http://127.0.0.1:{port}/pic.png\"}}]}}}}]}}}}"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            drop(sock);
            let (mut sock2, _) = listener.accept().await.unwrap();
            let mut buf2 = vec![0u8; 2048];
            let _ = sock2.read(&mut buf2).await.unwrap();
            let head2 = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n";
            sock2.write_all(head2.as_bytes()).await.unwrap();
            sock2.write_all(&png2).await.unwrap();
            (head.to_string(), body.to_string())
        });
        let entry = ProviderEntry {
            name: "ali".into(),
            // Stored the way a CHAT provider stores it; the image call
            // has to find the native path anyway.
            base_url: format!("http://127.0.0.1:{port}/compatible-mode/v1"),
            ..Default::default()
        };
        let out = generate_via_endpoint(
            &entry,
            "qwen-image-2.0",
            EndpointApi::Dashscope,
            "a cat",
            &serde_json::json!({}),
        )
        .await
        .expect("native shape must work");
        // The SAVED BYTES are the fetch proof, asserted before awaiting
        // the server task: if a future change stops fetching, the test
        // fails here with a message instead of hanging on a GET that
        // never comes.
        let p1 = out.trim_start_matches("MEDIA:");
        assert_eq!(
            std::fs::read(p1).unwrap()[..4],
            png[..4],
            "the fetched bytes, not a JSON stub"
        );
        assert!(
            p1.replace('\\', "/").contains("media/generated"),
            "under the delivery contract: {p1}"
        );
        let (head, body) = srv.await.unwrap();
        assert!(
            head.starts_with("POST /api/v1/services/aigc/multimodal-generation/generation"),
            "native path, not the compat one: {head}"
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: bearer sk-route-test"),
            "the resolved key, not the variable's name: {head}"
        );
        assert!(
            body.contains("\"input\":{\"messages\""),
            "messages body: {body}"
        );
        assert!(
            body.contains("\"size\":\"1024*1024\""),
            "asterisk size: {body}"
        );
        assert!(body.contains("qwen-image-2.0"), "the model rides: {body}");
    }

    /// A provider that answers the sync call with an async TASK is
    /// refused loudly, naming the task — "no images" would send someone
    /// hunting an empty response when the image is coming and nobody is
    /// polling for it.
    #[tokio::test]
    async fn an_async_task_answer_is_refused_by_name_not_reported_empty() {
        let (home, _env) = crate::testenv::scratch_home();
        // The route's key, where an operator keeps one. An unset key is
        // an error now; these tests used to pass by sending the NAME.
        std::fs::write(home.path().join(".env"), "API_KEY=sk-route-test\n").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = crate::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let ok =
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
            sock.write_all(ok.as_bytes()).await.unwrap();
            sock.write_all(b"{\"output\":{\"task_id\":\"task-77\",\"task_status\":\"PENDING\"}}")
                .await
                .unwrap();
        });
        let entry = ProviderEntry {
            name: "ali".into(),
            base_url: format!("http://127.0.0.1:{port}/compatible-mode/v1"),
            ..Default::default()
        };
        let e = generate_via_endpoint(
            &entry,
            "m",
            EndpointApi::Dashscope,
            "x",
            &serde_json::json!({}),
        )
        .await
        .unwrap_err();
        assert!(e.contains("task-77"), "the task is named: {e}");
        assert!(e.contains("async"), "{e}");
        srv.await.unwrap();
    }

    /// The 404 that started all this: a compat base with no image route
    /// answers with an empty body. Unactionable — unless the error
    /// carries the URL that refused.
    #[tokio::test]
    async fn an_endpoint_error_names_the_url_that_answered() {
        let (home, _env) = crate::testenv::scratch_home();
        // The route's key, where an operator keeps one. An unset key is
        // an error now; these tests used to pass by sending the NAME.
        std::fs::write(home.path().join(".env"), "API_KEY=sk-route-test\n").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = crate::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            sock.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        });
        let entry = ProviderEntry {
            name: "ali".into(),
            base_url: format!("http://127.0.0.1:{port}/compatible-mode/v1"),
            ..Default::default()
        };
        let e = generate_via_endpoint(
            &entry,
            "m",
            EndpointApi::Openai,
            "x",
            &serde_json::json!({}),
        )
        .await
        .unwrap_err();
        assert!(e.contains("404"), "{e}");
        assert!(
            e.contains("images/generations"),
            "the URL that 404'd must be IN the error: {e}"
        );
        srv.await.unwrap();
    }

    /// Video on the native route is a THREE-step dance — submit (async
    /// header mandatory), poll until terminal, fetch the expiring mp4 —
    /// driven here against a local listener that plays all three parts.
    #[tokio::test]
    async fn the_video_route_submits_polls_and_fetches_the_finished_clip() {
        let (home, _env) = crate::testenv::scratch_home();
        // The route's key, where an operator keeps one. An unset key is
        // an error now; these tests used to pass by sending the NAME.
        std::fs::write(home.path().join(".env"), "API_KEY=sk-route-test\n").unwrap();
        let mp4: Vec<u8> = vec![
            0, 0, 0, 0x18, b'f', b't', b'y', b'p', b'm', b'p', b'4', b'2',
        ];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mp4srv = mp4.clone();
        let srv = crate::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            // 1: submit → PENDING task; 2: poll RUNNING; 3: poll
            // SUCCEEDED with the url; 4: the mp4 itself.
            let answers: Vec<(bool, Vec<u8>)> = vec![
                (
                    true,
                    b"{\"output\":{\"task_id\":\"vid-1\",\"task_status\":\"PENDING\"}}"
                        .to_vec(),
                ),
                (true, b"{\"output\":{\"task_id\":\"vid-1\",\"task_status\":\"RUNNING\"}}".to_vec()),
                (
                    true,
                    format!("{{\"output\":{{\"task_id\":\"vid-1\",\"task_status\":\"SUCCEEDED\",\"video_url\":\"http://127.0.0.1:{port}/clip.mp4\"}}}}")
                        .into_bytes(),
                ),
                (false, mp4srv),
            ];
            let mut seen = Vec::new();
            for (i, (is_json, payload)) in answers.into_iter().enumerate() {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                seen.push(head);
                if i == 0 {
                    let whole = String::from_utf8_lossy(&buf[..n]).to_string();
                    // The header is not optional: without it the host
                    // rejects the submit outright.
                    seen.push(
                        if whole.contains("X-DashScope-Async")
                            || whole.contains("x-dashscope-async")
                        {
                            "ASYNC-HEADER".into()
                        } else {
                            "NO-ASYNC-HEADER".into()
                        },
                    );
                }
                let ctype = if is_json {
                    "application/json"
                } else {
                    "video/mp4"
                };
                let head_line = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nConnection: close\r\n\r\n"
                );
                sock.write_all(head_line.as_bytes()).await.unwrap();
                sock.write_all(&payload).await.unwrap();
            }
            seen
        });
        let entry = ProviderEntry {
            name: "ali".into(),
            base_url: format!("http://127.0.0.1:{port}/compatible-mode/v1"),
            ..Default::default()
        };
        let out = generate_video_via_dashscope_timed(
            &entry,
            "happyhorse-1.1-t2v",
            "a red ball bouncing",
            &serde_json::json!({"duration": 5}),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_secs(10),
        )
        .await
        .expect("the submit-poll-fetch chain must complete");
        let seen = srv.await.unwrap();
        assert!(
            seen[0].starts_with("POST /api/v1/services/aigc/video-generation/video-synthesis"),
            "submit goes to the native synthesis path: {:?}",
            seen[0]
        );
        assert_eq!(
            seen[1], "ASYNC-HEADER",
            "the mandatory async header must ride: {seen:?}"
        );
        assert!(
            seen[2].starts_with("GET /api/v1/tasks/vid-1"),
            "poll by task id: {:?}",
            seen[2]
        );
        assert!(
            seen[3].starts_with("GET /api/v1/tasks/vid-1"),
            "poll again while RUNNING: {:?}",
            seen[3]
        );
        let path = out.trim_start_matches("MEDIA:");
        assert!(path.ends_with(".mp4"), "content-sniffed extension: {path}");
        assert_eq!(
            std::fs::read(path).unwrap(),
            mp4,
            "the fetched clip, not a stub"
        );
        assert!(
            path.replace('\\', "/").contains("media/generated"),
            "under the delivery contract: {path}"
        );
    }

    #[tokio::test]
    async fn a_failed_video_task_is_reported_with_the_providers_reason() {
        let (home, _env) = crate::testenv::scratch_home();
        // The route's key, where an operator keeps one. An unset key is
        // an error now; these tests used to pass by sending the NAME.
        std::fs::write(home.path().join(".env"), "API_KEY=sk-route-test\n").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = crate::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            for payload in [
                "{\"output\":{\"task_id\":\"vid-9\",\"task_status\":\"PENDING\"}}",
                "{\"output\":{\"task_id\":\"vid-9\",\"task_status\":\"FAILED\",\"code\":\"DataInspection\",\"message\":\"prompt refused\"}}",
            ] {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
                sock.write_all(head.as_bytes()).await.unwrap();
                sock.write_all(payload.as_bytes()).await.unwrap();
            }
        });
        let entry = ProviderEntry {
            name: "ali".into(),
            base_url: format!("http://127.0.0.1:{port}/compatible-mode/v1"),
            ..Default::default()
        };
        let e = generate_video_via_dashscope_timed(
            &entry,
            "happyhorse-1.1-t2v",
            "x",
            &serde_json::json!({}),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_secs(10),
        )
        .await
        .unwrap_err();
        assert!(e.contains("vid-9"), "the task is named: {e}");
        assert!(e.contains("prompt refused"), "and its reason too: {e}");
        assert!(e.contains("DataInspection"), "{e}");
        srv.await.unwrap();
    }

    #[test]
    fn the_wire_shape_follows_the_provider_when_config_stays_silent() {
        // A `/gen --provider` switch cannot strand the call on the wrong
        // door: unset api INFERS from the base, and an explicit api
        // still overrides both ways.
        assert_eq!(
            EndpointApi::resolve("[imagegen].api", None, "https://h/compatible-mode/v1").unwrap(),
            EndpointApi::Dashscope
        );
        assert_eq!(
            EndpointApi::resolve(
                "[imagegen].api",
                None,
                "https://dashscope.aliyuncs.com/compatible-mode/v1"
            )
            .unwrap(),
            EndpointApi::Dashscope
        );
        assert_eq!(
            EndpointApi::resolve("[imagegen].api", None, "https://api.openai.com/v1").unwrap(),
            EndpointApi::Openai
        );
        // Explicit wins — even against the inference.
        assert_eq!(
            EndpointApi::resolve(
                "[imagegen].api",
                Some("openai"),
                "https://h/compatible-mode/v1"
            )
            .unwrap(),
            EndpointApi::Openai
        );
    }

    #[test]
    fn asr_has_no_local_route_and_says_so_with_the_config_name() {
        // Image and video fall back to ComfyUI; ASR does not, and the
        // error must name the key that would fix it.
        let mut c = cfg(Some("router"), Some("m"));
        let e = resolve_endpoint(&c, &c.asr, "[asr]", None, None, None).unwrap_err();
        assert!(e.contains("[asr].provider"), "{e}");
        // And the section itself resolves like any other when set.
        c.asr.provider = Some("router".into());
        c.asr.model = Some("asr-1".into());
        let got = resolve_endpoint(&c, &c.asr, "[asr]", None, None, None)
            .unwrap()
            .expect("configured provider routes");
        assert_eq!(got.1, "asr-1");
    }
}

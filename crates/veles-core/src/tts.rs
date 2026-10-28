//! TTS tool — text-to-speech via OpenAI-compatible / edge-tts.
//!
//! #29 in the feature matrix. Converts text to speech audio. Uses the
//! configured provider's TTS endpoint (OpenAI /v1/audio/speech format)
//! or falls back to `edge-tts` (Microsoft Edge TTS, free, no API key).
//! The audio file is saved to `<data_dir>/media/audio/` and the path
//! returned so frontends can play or send it. Under the media root because
//! that root is the delivery contract: a `MEDIA:` marker naming a path
//! outside it is withheld from below-owner principals
//! ([`crate::media::deliverable`]), so audio written anywhere else would be
//! audio only its maker could hear.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::Tool;

pub struct TtsTool {
    /// The calling session — the `output_path` jail below resolves the
    /// principal from it.
    ///
    /// The jail itself is not new; it lived in the agent's tool loop
    /// (`agent.rs`, the "media-write jail"). That was the only caller of
    /// a Tool until `/gen` started invoking `TtsTool`, `ImageGenTool`
    /// and `VideoGenTool` DIRECTLY — at which point a gate outside the
    /// tool is a gate that depends on who calls it. `/gen` parses no
    /// `output_path`, so nothing was reachable; `vision.rs` and `asr.rs`
    /// had already moved their jails inside for the same reason, and
    /// this makes the three agree.
    pub session_id: String,
}

impl TtsTool {
    /// A TTS tool with no session — the owner surfaces, and the tests.
    /// No principal means no jail, exactly as everywhere else.
    pub fn unbound() -> Self {
        Self {
            session_id: String::new(),
        }
    }
}

#[async_trait]
impl Tool for TtsTool {
    fn name(&self) -> &str {
        "text_to_speech"
    }
    fn description(&self) -> &str {
        "Convert text to speech audio. Returns a MEDIA: path for delivery. Uses the configured TTS provider — OpenAI-compatible /audio/speech, or DashScope-native SpeechSynthesizer when the provider's base is a DashScope one (or agent.tts_api names it) — and falls back to edge-tts (free)."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "Text to convert to speech"},
                "voice": {
                    "type": "string",
                    "description": "Voice name (provider-specific: 'alloy' for OpenAI-shape, 'longanhuan_v3.6'-style for DashScope, 'en-US-AriaNeural' for edge)"
                },
                "model": {
                    "type": "string",
                    "description": "TTS model id (default: agent.tts_model)"
                },
                "speed": {
                    "type": "number",
                    "description": "Playback speed multiplier (0.25-4.0, default 1.0)"
                },
                "provider": {
                    "type": "string",
                    "description": "TTS provider override: 'openai', 'edge', or a configured provider name"
                },
                "output_path": {
                    "type": "string",
                    "description": "Custom output file path (default: <media_dir>/audio/<timestamp>.mp3). Below owner, a path outside the media directory is refused rather than written."
                },
                "instructions": {
                    "type": "string",
                    "description": "Voice-design guidance (OpenAI gpt-4o-mini-tts): tone, emotion, pacing"
                }
            },
            "required": ["text"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let text = args["text"].as_str().unwrap_or_default().to_string();
        if text.is_empty() {
            return "error: text is required".into();
        }
        // Cap text length (OpenAI limit is 4096)
        let text = if text.len() > 4096 {
            tracing::warn!("TTS text truncated from {} chars to 4096", text.len());
            text[..4096].to_string()
        } else {
            text
        };

        let provider_override = args["provider"].as_str();
        let speed = args["speed"].as_f64().unwrap_or(1.0);

        let audio_dir = crate::config::media_dir().join("audio");
        let _ = std::fs::create_dir_all(&audio_dir);

        let output_path = args["output_path"]
            .as_str()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                audio_dir.join(format!("{stamp}.mp3"))
            });

        // ── the media-write jail ──
        //
        // This tool WRITES a path the caller names, which makes it the
        // write-side sibling of the F1 read jail: a principal who may
        // not have an arbitrary file DELIVERED must not get to choose
        // where one is CREATED, or the marker rule is a suggestion
        // ("write it anywhere, then ask for the file back"). Unlike the
        // read jail this binds admin too, and binds every below-owner
        // role rather than only the ones whose shell stays on the host —
        // TTS runs in-process, so its `output_path` is the HOST
        // filesystem even for a caller whose terminal is a container.
        if let Some(named) = args["output_path"].as_str().filter(|s| !s.is_empty()) {
            let who = crate::acl::session_principal(&self.session_id);
            if let Some(why) = crate::media::deliverable(named, who.as_deref()) {
                return format!(
                    "[acl] denied: `output_path` is {why}. Write into {} (or omit \
                     `output_path` and the default under it is used) — a file anywhere \
                     else could not be delivered to this chat anyway.",
                    crate::config::media_dir().display()
                );
            }
        }

        // Try provider TTS first (unless explicitly edge). The failure
        // is KEPT rather than discarded: a `if let Ok(..)` here meant a
        // misconfigured route silently became edge-tts, so the operator
        // saw a different voice and no reason — "nothing implicit" is
        // the standing directive, and a route that quietly stopped being
        // the route is exactly the kind of thing it is about.
        let mut provider_err: Option<String> = None;
        if provider_override != Some("edge") {
            match try_provider_tts(&text, &args, &output_path, speed).await {
                Ok(path) => return format!("MEDIA:{}", path.display()),
                Err(e) => {
                    tracing::warn!("tts: provider route failed, falling back to edge-tts: {e}");
                    provider_err = Some(e);
                }
            }
        }

        // Fallback: edge-tts (free, no API key, uses Microsoft Edge TTS)
        match try_edge_tts(&text, &args, &output_path, speed).await {
            Ok(path) => match &provider_err {
                Some(e) => format!(
                    "MEDIA:{}\n(the configured tts route failed, so this is edge-tts: {e})",
                    path.display()
                ),
                None => format!("MEDIA:{}", path.display()),
            },
            Err(e) => format!(
                "TTS error: all routes failed. Provider: {}; edge-tts: {e}",
                provider_err.as_deref().unwrap_or("not tried")
            ),
        }
    }
}

/// Try the configured provider's TTS endpoint (OpenAI-compatible
/// /v1/audio/speech).
async fn try_provider_tts(
    text: &str,
    args: &Value,
    output_path: &std::path::Path,
    speed: f64,
) -> Result<std::path::PathBuf, String> {
    let config = crate::config::load().map_err(|e| e.to_string())?;
    let provider_key = args["provider"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| config.agent.tts_provider.clone())
        .or_else(|| config.model.provider.clone())
        .ok_or("no tts provider configured")?;

    let entry = config
        .provider(&provider_key)
        .ok_or_else(|| format!("provider {provider_key} not found"))?;
    let api_key = crate::providers::resolve_api_key(entry, &crate::config::config_dir());

    // The wire shape belongs to the PROVIDER: a DashScope base cannot
    // serve /audio/speech (its compat layer answers media routes with a
    // bare 404), and the native door wants a different body and a
    // different voice namespace. `agent.tts_api` overrides; unset
    // infers from the base_url, so a `/gen --provider` switch follows.
    let api = crate::imagegen::EndpointApi::resolve(
        "agent.tts_api",
        config.agent.tts_api.as_deref(),
        &entry.base_url,
    )?;

    let model = args["model"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(config.agent.tts_model.as_deref())
        .map(|s| s.to_string());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;

    if api == crate::imagegen::EndpointApi::Dashscope {
        // Native one-shot speech synthesis: `input` carries the text AND
        // the voice/format knobs (putting voice in `parameters` is the
        // OpenAI habit, and the cosyvoice engine answers it with a
        // contentless engine 411 — learned live). The reply is a JSON
        // envelope around an EXPIRING audio url, so the fetch happens
        // here, now; a url handed to a connector is an audio clip that
        // disappears on a timer.
        let model = model.ok_or_else(|| {
            "no dashscope tts model: set agent.tts_model (e.g. \"qwen-audio-3.0-tts-plus\")"
                .to_string()
        })?;
        let voice = args["voice"]
            .as_str()
            .or(config.agent.tts_voice.as_deref())
            .unwrap_or("longanhuan_v3.6");
        let format = match output_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp3")
        {
            "wav" => "wav",
            "pcm" => "pcm",
            "opus" => "opus",
            _ => "mp3",
        };
        let mut input = serde_json::json!({"text": text, "voice": voice, "format": format});
        if (speed - 1.0).abs() > f64::EPSILON {
            // Cosyvoice's knob is `rate`, bounded half-open from
            // OpenAI's `speed` range.
            input["rate"] = speed.clamp(0.5, 2.0).into();
        }
        let url =
            crate::imagegen::dashscope_url_for(&entry.base_url, "audio/tts/SpeechSynthesizer");
        let body = serde_json::json!({"model": model, "input": input});
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
            return Err(format!("HTTP {status} from {url}: {text}"));
        }
        let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let bytes: Vec<u8> = if let Some(u) = json["output"]["audio"]["url"].as_str() {
            let resp = client
                .get(u)
                .send()
                .await
                .map_err(|e| format!("fetching the synthesized audio: {e}"))?;
            crate::imagegen::read_capped(resp, "the synthesized audio").await?
        } else if let Some(b64) = json["output"]["audio"]["data"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| format!("decoding audio data: {e}"))?
        } else {
            let msg = json["message"].as_str().unwrap_or("no audio url or data");
            return Err(format!("dashscope tts returned no audio: {msg}"));
        };
        std::fs::write(output_path, bytes).map_err(|e| e.to_string())?;
        return Ok(output_path.to_path_buf());
    }

    let voice = args["voice"]
        .as_str()
        .or(config.agent.tts_voice.as_deref())
        .unwrap_or("alloy");

    let model = model.unwrap_or_else(|| "tts-1".to_string());

    let url = format!("{}/audio/speech", entry.base_url.trim_end_matches('/'));

    let mut body = serde_json::json!({
        "model": model,
        "input": text,
        "voice": voice,
        "speed": speed,
    });

    if let Some(instructions) = args["instructions"].as_str() {
        body["instructions"] = serde_json::Value::String(instructions.to_string());
    }

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
        return Err(format!("HTTP {status} from {url}: {text}"));
    }

    let bytes = crate::imagegen::read_capped(resp, "the synthesized audio").await?;
    std::fs::write(output_path, bytes).map_err(|e| e.to_string())?;
    Ok(output_path.to_path_buf())
}

/// Try edge-tts (free Microsoft Edge TTS). Uses the `edge-tts` command
/// if available, otherwise downloads via Python pip-install.
async fn try_edge_tts(
    text: &str,
    args: &Value,
    output_path: &std::path::Path,
    _speed: f64,
) -> Result<std::path::PathBuf, String> {
    let voice = args["voice"].as_str().unwrap_or("en-US-AriaNeural");

    // Try the edge-tts CLI
    let result = tokio::process::Command::new("edge-tts")
        .arg("--voice")
        .arg(voice)
        .arg("--text")
        .arg(text)
        .arg("--write-media")
        .arg(output_path)
        .output()
        .await;

    match result {
        Ok(out) if out.status.success() => Ok(output_path.to_path_buf()),
        Ok(out) => Err(format!(
            "edge-tts exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )),
        Err(_) => {
            // edge-tts is not installed. Installing it is a HOST change
            // — `pip install --user` runs code from an index — and this
            // path is reachable from a channel: any caller with `tts`
            // whose provider route fails lands here. A package install
            // is not a thing a stranger's "say hello" should trigger, so
            // it is opt-in and says so when it refuses.
            if !crate::config::load()
                .map(|c| c.agent.tts_autoinstall)
                .unwrap_or(false)
            {
                return Err(
                    "edge-tts is not installed. Install it yourself (`pip install --user \
                     edge-tts`) or set `[agent] tts_autoinstall = true` — veles will not \
                     install packages on the host because a chat message asked it to"
                        .into(),
                );
            }
            try_install_edge_tts().await?;
            let result2 = tokio::process::Command::new("edge-tts")
                .arg("--voice")
                .arg(voice)
                .arg("--text")
                .arg(text)
                .arg("--write-media")
                .arg(output_path)
                .output()
                .await;
            match result2 {
                Ok(out) if out.status.success() => Ok(output_path.to_path_buf()),
                Ok(out) => Err(format!(
                    "edge-tts exited {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                )),
                _ => Err("edge-tts not available (pip install failed)".into()),
            }
        }
    }
}

async fn try_install_edge_tts() -> Result<(), String> {
    let out = tokio::process::Command::new("pip")
        .args(["install", "--user", "edge-tts"])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "pip install edge-tts failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

#[cfg(test)]
mod jail_tests {
    use super::*;

    /// The write-side sibling of the F1 read jail, held INSIDE the tool.
    ///
    /// It used to live in the agent's tool loop, which was the only
    /// caller of a Tool — until `/gen` began invoking `TtsTool`,
    /// `ImageGenTool` and `VideoGenTool` directly. A gate outside the
    /// thing it guards is a gate that depends on who calls it, and
    /// `vision.rs` and `asr.rs` had already moved theirs inside. The
    /// assertions look only for the JAIL's refusal: past it the call
    /// fails on unconfigured TTS, which is a different error and none of
    /// this test's business.
    #[tokio::test]
    async fn a_below_owner_caller_cannot_choose_where_the_audio_lands() {
        let (_home, _env) = crate::testenv::scratch_home();
        let sess = "tts-jail-1";
        crate::acl::set_session_principal(
            sess,
            crate::acl::Principal {
                id: None,
                subject: "user:tts-jail".into(),
                role: crate::acl::Role::Trusted,
                caps: crate::acl::Role::Trusted.default_caps(),
                rule_context_limit: None,
                mask: None,
                via: None,
                ticket_role: None,
            },
        );
        let tool = TtsTool {
            session_id: sess.into(),
        };
        let out = tool
            .call(serde_json::json!({
                "text": "hello",
                "output_path": "/home/someone/.config/veles/.env"
            }))
            .await;
        assert!(
            out.contains("[acl] denied") && out.contains("output_path"),
            "a trusted caller must not name a host path: {out}"
        );

        // The media root is the sanctioned surface, so the jail lets it
        // through (and the call then fails on TTS config, which is fine).
        let inside = crate::config::media_dir().join("audio/ok.mp3");
        let out2 = tool
            .call(serde_json::json!({
                "text": "hello",
                "output_path": inside.to_str().unwrap()
            }))
            .await;
        assert!(
            !out2.contains("[acl] denied"),
            "the media root must pass the jail: {out2}"
        );

        // No principal is an owner surface, here as everywhere else.
        let owner = TtsTool::unbound();
        let out3 = owner
            .call(serde_json::json!({"text": "hi", "output_path": "/tmp/anywhere.mp3"}))
            .await;
        assert!(!out3.contains("[acl] denied"), "{out3}");
    }
}

#[cfg(test)]
mod dashscope_tests {
    use super::*;

    /// The native speech route against a local listener: voice and
    /// format ride INSIDE `input` (the OpenAI habit of a top-level
    /// voice gets a contentless engine 411 from the cosyvoice engine —
    /// learned live), the reply is a JSON envelope around an EXPIRING
    /// url, and the audio bytes must land in the file before that url
    /// dies.
    #[tokio::test]
    async fn the_dashscope_tts_shape_carries_voice_in_input_and_fetches_the_audio() {
        let (home, _env) = crate::testenv::scratch_home();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mp3: Vec<u8> = vec![0xFF, 0xFB, 0x90, 0x00, b'm', b'p', b'3', 1, 2, 3];
        let mp3srv = mp3.clone();
        let srv = crate::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            // 1: the synthesis POST; 2: the expiring url, fetched now.
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let head_end = req.find("\r\n\r\n").unwrap_or(0);
            let (head, body) = req.split_at(head_end);
            let ok =
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
            sock.write_all(ok.as_bytes()).await.unwrap();
            sock.write_all(
                format!("{{\"output\":{{\"audio\":{{\"url\":\"http://127.0.0.1:{port}/a.mp3\",\"data\":\"\"}},\"finish_reason\":\"stop\"}}}}")
                    .as_bytes(),
            )
            .await
            .unwrap();
            drop(sock);
            let (mut sock2, _) = listener.accept().await.unwrap();
            let mut buf2 = vec![0u8; 2048];
            let _ = sock2.read(&mut buf2).await.unwrap();
            let h2 = "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nConnection: close\r\n\r\n";
            sock2.write_all(h2.as_bytes()).await.unwrap();
            sock2.write_all(&mp3srv).await.unwrap();
            (head.to_string(), body.to_string())
        });
        std::fs::write(
            home.path().join("config.toml"),
            format!(
                "[[custom_providers]]\nname = \"ali\"\nbase_url = \"http://127.0.0.1:{port}/compatible-mode/v1\"\nkey_env = \"TEST_TTS_KEY\"\n\n[agent]\ntts_provider = \"ali\"\ntts_model = \"qwen-audio-3.0-tts-plus\"\n"
            ),
        )
        .unwrap();
        let out_path = home.path().join("say.mp3");
        let path = try_provider_tts("hello there", &serde_json::json!({}), &out_path, 1.0)
            .await
            .expect("the dashscope tts shape must work");
        let (head, body) = srv.await.unwrap();
        assert!(
            head.starts_with("POST /api/v1/services/audio/tts/SpeechSynthesizer"),
            "native tts path, not /audio/speech: {head}"
        );
        let v: serde_json::Value = serde_json::from_str(body.trim()).expect("json body");
        assert_eq!(
            v["input"]["voice"], "longanhuan_v3.6",
            "voice INSIDE input: {body}"
        );
        assert_eq!(v["input"]["text"], "hello there");
        assert_eq!(v["model"], "qwen-audio-3.0-tts-plus");
        assert!(
            v.get("parameters")
                .map(|p| p["voice"].is_null())
                .unwrap_or(true),
            "voice must NOT sit in parameters: {body}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            mp3,
            "the fetched audio, not the envelope"
        );
    }
}

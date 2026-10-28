//! Speech recognition — the `transcribe_audio` tool.
//!
//! Routes a recording to the configured `[asr]` provider. DashScope's
//! flash ASR is a synchronous `multimodal-generation` call whose input
//! is either a public URL or a base64 data URI, and whose answer is the
//! text itself (`output.text`) — no task to poll, no expiring result.
//! There is no local fallback: an unset provider is an error naming the
//! config, the way `[vision]` refuses rather than guessing.
//!
//! Like every media producer, the audio file path is judged by
//! [`crate::media::deliverable`] against the session's principal: a
//! below-owner caller can transcribe its own media, not the host's.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::Tool;

pub struct AsrTool {
    /// The calling session — the local-path jail resolves the principal
    /// from it. Owner surfaces have no principal and keep the whole
    /// filesystem, exactly as the F1 read jail and vision leave them.
    pub session_id: String,
}

/// The MIME type the base64 data URI needs, from the file extension.
fn audio_mime(path: &str) -> &'static str {
    match PathExt::ext(path).as_deref() {
        Some("mp3") | Some("mpeg") => "audio/mpeg",
        Some("ogg") | Some("oga") => "audio/ogg",
        Some("flac") => "audio/flac",
        Some("aac") => "audio/aac",
        Some("opus") => "audio/opus",
        Some("amr") => "audio/amr",
        _ => "audio/wav",
    }
}

/// The `format` field DashScope's ASR `parameters` wants, from the
/// extension — wav for anything unknown (the safe default it accepts).
fn audio_format(path: &str) -> &'static str {
    match PathExt::ext(path).as_deref() {
        Some("mp3") | Some("mpeg") => "mp3",
        Some("opus") => "opus",
        Some("ogg") | Some("oga") => "ogg",
        Some("pcm") | Some("raw") => "pcm",
        Some("aac") => "aac",
        Some("amr") => "amr",
        _ => "wav",
    }
}

struct PathExt;
impl PathExt {
    fn ext(p: &str) -> Option<String> {
        let tail = p.rsplit(['/', '\\']).next().unwrap_or(p);
        let dot = tail.rfind('.')?;
        let e = &tail[dot + 1..];
        (!e.is_empty() && e.len() <= 5).then(|| e.to_ascii_lowercase())
    }
}

/// Read an audio path where the session's other file tools read it.
/// Sandbox-relative paths are guest paths even under remote podman or
/// Android userland, where the host cannot open `/work`. Absolute host
/// paths keep the existing media-root jail; a box error is reported
/// rather than retried as a host path.
async fn read_audio(session: &str, path: &str) -> Result<Vec<u8>, String> {
    if let Some(result) = crate::user_envs::read_session_bytes(session, path).await {
        return result.map_err(|e| e.to_string());
    }
    let who = crate::acl::session_principal(session);
    if let Some(why) = crate::media::deliverable(path, who.as_deref()) {
        return Err(why.to_string());
    }
    std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))
}

#[async_trait]
impl Tool for AsrTool {
    fn name(&self) -> &str {
        "transcribe_audio"
    }
    fn description(&self) -> &str {
        "Transcribe a recorded audio file to text via the configured [asr] provider (DashScope flash ASR). Provide a local audio path (wav/mp3/opus/ogg/aac) reachable under the media root or the session's own workspace, or a public http(s) URL to the audio. Returns the transcription text."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "audio": {"type": "string", "description": "Path to an audio file, or a public http(s) audio URL"},
                "language": {"type": "string", "description": "Optional language hint code (e.g. 'en', 'zh'); unset auto-detects"},
                "hotwords": {"type": "object", "description": "Optional instant hotwords: {term: weight} with weight 1-5, to bias recognition toward domain terms"},
                "provider": {"type": "string", "description": "Route this one call to a configured provider by name (default: [asr].provider)"},
                "model": {"type": "string", "description": "ASR model id (default: [asr].model, else the provider entry's own model)"}
            },
            "required": ["audio"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let audio = args["audio"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        if audio.is_empty() {
            return "error: audio is required".into();
        }

        // Resolve the audio FIRST — a public URL passes through
        // untouched, a local path is jail-checked against the session's
        // principal before any provider is contacted. The jail must be
        // the error a misconfigured `[asr]` cannot hide.
        let is_url = audio.starts_with("http://") || audio.starts_with("https://");
        let format = audio_format(if is_url {
            // A signed url's QUERY contains dots; only the path names
            // the format.
            audio.split(['?', '#']).next().unwrap_or(&audio)
        } else {
            &audio
        });
        let data = if is_url {
            audio.clone()
        } else {
            let bytes = match read_audio(&self.session_id, &audio).await {
                Ok(b) => b,
                Err(e) => return format!("transcribe_audio error: {e}"),
            };
            // The inline cap: base64 grows ~4/3, and the endpoint
            // rejects oversize data URIs. A bigger file is fine if it is
            // hosted; a 10 MB inline audio is not.
            if bytes.len() > 7 * 1024 * 1024 {
                return format!(
                    "transcribe_audio error: {audio} is {} bytes — over the inline base64 limit; host it and pass a public URL instead",
                    bytes.len()
                );
            }
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let m = audio_mime(&audio);
            format!("data:{m};base64,{b64}")
        };

        let cfg = match crate::config::load() {
            Ok(c) => c,
            Err(e) => return format!("transcribe_audio error: config load: {e}"),
        };
        // No local fallback for ASR: unset provider is an error that
        // names the config, never a silent guess.
        let (entry, model, api) =
            match resolve_asr_route(&cfg, args["provider"].as_str(), args["model"].as_str()) {
                Ok(r) => r,
                Err(e) => return format!("transcribe_audio error: {e}"),
            };
        if api != crate::imagegen::EndpointApi::Dashscope {
            return "transcribe_audio error: only the DashScope-native ASR is implemented; set [asr].api = \"dashscope\"".into();
        }

        let key_env = entry.key_env.as_deref().unwrap_or("API_KEY");
        let api_key = match crate::secrets::resolve_key_env(key_env) {
            Ok(k) => k,
            Err(e) => return format!("transcribe_audio error: {e}"),
        };
        let url = crate::imagegen::dashscope_url_for(
            &entry.base_url,
            "aigc/multimodal-generation/generation",
        );

        let content = serde_json::json!({"type": "input_audio", "input_audio": {"data": data}});
        // `parameters.format` is required by the endpoint for BOTH input
        // kinds — the live probe that returned `{}` with an empty body
        // was exactly a missing-parameters rejection at the proxy.
        let mut params = serde_json::Map::new();
        params.insert("format".into(), format.into());
        if let Some(lang) = args["language"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            params.insert("language_hints".into(), serde_json::json!([lang]));
        }
        if let Some(hw) = args["hotwords"].as_object() {
            params.insert("vocabulary".into(), Value::Object(hw.clone()));
        }
        let body = serde_json::json!({
            "model": model,
            "input": {"messages": [{"role": "user", "content": [content]}]},
            "parameters": params,
        });

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())
            .unwrap_or_else(|_| reqwest::Client::new());
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => return format!("transcribe_audio error: {e}"),
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return format!("transcribe_audio error: HTTP {status} from {url}: {text}");
        }
        let json: Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => return format!("transcribe_audio error: decoding response: {e}"),
        };
        // The documented envelope nests under `output`; the live answer
        // from this host flattened `sentence` to the top level. Read
        // both rather than trusting either.
        let text = json["output"]["text"]
            .as_str()
            .or_else(|| json["output"]["sentence"]["text"].as_str())
            .or_else(|| json["text"].as_str())
            .or_else(|| json["sentence"]["text"].as_str())
            .unwrap_or("");
        if text.trim().is_empty() {
            return "transcribe_audio: no speech recognized".into();
        }
        text.to_string()
    }
}

/// The ASR route: per-call > `[asr].provider`, NO local fallback, model
/// per-call > `[asr].model` > entry > error.
fn resolve_asr_route(
    cfg: &crate::config::VelesConfig,
    provider_arg: Option<&str>,
    model_arg: Option<&str>,
) -> Result<
    (
        crate::config::ProviderEntry,
        String,
        crate::imagegen::EndpointApi,
    ),
    String,
> {
    crate::imagegen::resolve_endpoint(cfg, &cfg.asr, "[asr]", provider_arg, model_arg, None)?
        .ok_or_else(|| "[asr] unexpectedly resolved to a local route".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_and_format_follow_the_extension_not_the_filename() {
        assert_eq!(audio_mime("/x/voice.MP3"), "audio/mpeg");
        assert_eq!(audio_mime("/x/a.opus"), "audio/opus");
        assert_eq!(audio_mime("/x/sound"), "audio/wav");
        assert_eq!(audio_format("/x/voice.mp3"), "mp3");
        assert_eq!(audio_format("/x/a.opus"), "opus");
        assert_eq!(audio_format("/x/sound"), "wav");
        // A dotted directory component must not be read as the ext.
        assert_eq!(audio_format("/v1.2.3/audio"), "wav");
    }

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

    /// The whole call chain against a local listener: the data URI
    /// leaves as the endpoint's docs demand (input_audio under
    /// messages, `parameters.format` present), and the transcript comes
    /// back as the tool's answer.
    #[tokio::test]
    async fn the_asr_route_posts_the_data_uri_and_returns_the_transcript() {
        let (home, _env) = crate::testenv::scratch_home();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = crate::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let head_end = req.find("\r\n\r\n").unwrap_or(0);
            let (head, body) = req.split_at(head_end);
            let ok =
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
            sock.write_all(ok.as_bytes()).await.unwrap();
            sock.write_all(
                b"{\"output\":{\"text\":\"the quick brown fox\",\"sentence\":{\"text\":\"the quick brown fox\"}}}",
            )
            .await
            .unwrap();
            (head.to_string(), body.to_string())
        });
        std::fs::write(home.path().join(".env"), "TEST_ASR_KEY=sk-asr-test\n").unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            format!(
                "[[custom_providers]]\nname = \"ali\"\nbase_url = \"http://127.0.0.1:{port}/compatible-mode/v1\"\nkey_env = \"TEST_ASR_KEY\"\n\n[asr]\nprovider = \"ali\"\nmodel = \"qwen-audio-3.0-asr-flash\"\n"
            ),
        )
        .unwrap();
        let wav = crate::config::media_dir().join("say.wav");
        std::fs::create_dir_all(wav.parent().unwrap()).unwrap();
        std::fs::write(&wav, b"RIFF\x24\x00\x00\x00WAVEfmt ").unwrap();
        let tool = AsrTool {
            session_id: "asr-owner-tests".into(),
        };
        let out = tool
            .call(serde_json::json!({"audio": wav.display().to_string()}))
            .await;
        assert_eq!(out, "the quick brown fox");
        let (head, body) = srv.await.unwrap();
        assert!(
            head.starts_with("POST /api/v1/services/aigc/multimodal-generation/generation"),
            "native generation path: {head}"
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: bearer sk-asr-test"),
            "the resolved key, not the variable's name: {head}"
        );
        assert!(
            body.contains("data:audio/wav;base64,"),
            "data uri in: {body}"
        );
        assert!(
            body.contains("\"format\":\"wav\""),
            "parameters.format rides: {body}"
        );
        assert!(
            body.contains("qwen-audio-3.0-asr-flash"),
            "model named: {body}"
        );
        assert!(body.contains("input_audio"), "messages shape: {body}");
    }

    #[tokio::test]
    async fn a_sandbox_audio_path_is_read_in_its_box_not_on_the_host() {
        let (_home, _env) = crate::testenv::scratch_home();
        bind_trusted("asr-box");
        let out = read_audio("asr-box", "/work/voice.wav")
            .await
            .expect_err("the test guest is deliberately offline");
        assert!(out.contains("user envs are offline under tests"), "{out}");
        assert!(!out.contains("No such file"), "{out}");
        assert!(!out.contains("outside the media"), "{out}");
    }

    #[tokio::test]
    async fn a_trusted_session_cannot_transcribe_the_host() {
        // The vision jail's rule, applied to a tool that reads the
        // file's CONTENTS: a trusted caller may transcribe its own
        // media, not /etc/passwd. And it fails BEFORE any provider is
        // contacted — no config, no request, no bill.
        let (_home, _env) = crate::testenv::scratch_home();
        bind_trusted("asr-jail");
        let tool = AsrTool {
            session_id: "asr-jail".into(),
        };
        let out = tool.call(serde_json::json!({"audio": "/etc/passwd"})).await;
        assert!(out.starts_with("transcribe_audio error:"), "{out}");
        assert!(!out.contains("config load"), "the jail fires first: {out}");
        assert!(!out.contains("[asr]"), "not the route error either: {out}");
    }
}

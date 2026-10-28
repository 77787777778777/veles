//! `echo` — a toy platform as a process plugin.
//!
//! One instance per `[[platforms.echo.rooms]]` entry. There is no real
//! network: a test (or a person, through `veles plugins call`) makes a
//! "platform message" arrive with `echo.say`, and every delivery is
//! appended to `delivered.jsonl` in the instance's state dir. The other
//! `echo.*` methods let the plugin host's tests look inside the process
//! (its pid, its environment) and break it on purpose (`echo.crash`).
//!
//! A room with `fail_start = true` refuses to start, for the host's
//! back-off test; one with `hang_shutdown = true` never finishes
//! shutting down, for the SDK's time limit.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

use serde_json::{json, Value};
use veles_plugin_sdk::*;

#[derive(Default)]
struct Echo {
    host: OnceLock<Host>,
    state: OnceLock<std::path::PathBuf>,
    delivered: AtomicU64,
    /// `hang_shutdown = true`: never finish shutting down (the SDK's
    /// time limit test).
    hang_shutdown: AtomicBool,
}

impl Echo {
    fn host(&self) -> Result<&Host, RpcError> {
        self.host
            .get()
            .ok_or_else(|| RpcError::refused("not started"))
    }

    fn append(&self, file: &str, line: &str) {
        let Some(dir) = self.state.get() else { return };
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(file))
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn manifest() -> Manifest {
    Manifest {
        name: "echo".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: PROTOCOL,
        description: "a toy platform for the plugin host's tests".into(),
        instances: Instances::PerEntry {
            array: "platforms.echo.rooms".into(),
            name: "name".into(),
        },
        lease: Some("singleton:echo:{instance}".into()),
        platforms: vec![PlatformDecl {
            key: "echo".into(),
            public_feed: false,
        }],
        config_sections: vec!["platforms.echo".into()],
        knobs: vec![
            veles_plugin_sdk::Knob::blob_array("platforms.echo.rooms"),
            veles_plugin_sdk::Knob::blob_documented(
                "platforms.echo.rooms.fail_start",
                "false",
                "refuse startup; exercises the host's restart policy",
            ),
            veles_plugin_sdk::Knob::blob_documented(
                "platforms.echo.rooms.hang_shutdown",
                "false",
                "ignore shutdown; exercises the SDK's bounded teardown",
            ),
        ],
        secrets: vec![SecretDecl {
            name: "ECHO_TOKEN".into(),
            description: "a token the toy platform pretends to need".into(),
            required: false,
        }],
        // A service any caller may use, answered by `request` below, and
        // one it uses (the host test provides it from core).
        services: vec![
            ServiceDecl {
                name: "echo.upper".into(),
                description: "the text, upper-cased".into(),
                cap: Some("any".into()),
            },
            // An override of the host test's point (PLAN-extension-api.md
            // D2): its request is the envelope {input, next, caller,
            // session_id}, and `service.next` asks the layer below.
            ServiceDecl {
                name: "echotest.point".into(),
                description: "the host test's override point, wrapped".into(),
                cap: None,
            },
        ],
        uses: vec![UseDecl {
            name: "echotest.open".into(),
            optional: true,
            description: "the host test's core service".into(),
        }],
        hooks: vec![],
        enabled_env: None,
        required_env: None,
        points: vec![],
    }
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

#[async_trait]
impl Plugin for Echo {
    async fn start(&self, host: Host, init: InitParams) -> Result<(), String> {
        let room = init
            .config
            .get("platforms.echo")
            .and_then(|e| e.get("rooms"))
            .and_then(Value::as_array)
            .and_then(|rooms| {
                rooms
                    .iter()
                    .find(|r| r.get("name").and_then(Value::as_str) == Some(init.instance.as_str()))
            })
            .cloned()
            .unwrap_or(Value::Null);
        let _ = self.state.set(std::path::PathBuf::from(&init.dirs.state));
        if room.get("hang_shutdown").and_then(Value::as_bool) == Some(true) {
            self.hang_shutdown.store(true, Ordering::Release);
        }
        self.append("events.log", &format!("start {}", std::process::id()));
        if room.get("fail_start").and_then(Value::as_bool) == Some(true) {
            return Err(format!(
                "room {} is configured to fail at start",
                init.instance
            ));
        }
        tracing::info!("echo room {} up", init.instance);
        let _ = self.host.set(host);
        Ok(())
    }

    async fn deliver(&self, msg: OutboundMessage) -> Result<Receipt, String> {
        let n = self.delivered.fetch_add(1, Ordering::AcqRel) + 1;
        let line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
        self.append("delivered.jsonl", &line);
        let inst = self
            .host
            .get()
            .map(|h| h.instance().to_string())
            .unwrap_or_default();
        Ok(Receipt {
            ids: vec![format!("{inst}-{n}")],
            ..Default::default()
        })
    }

    async fn request(&self, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            // A message "arrives" on the toy platform. `platform`,
            // `route` and `ctx_platform` may be set to anything, to check
            // the host corrects or drops what is not the plugin's to say.
            "echo.say" => {
                let msg = InboundMessage {
                    platform: s(&p, "platform").unwrap_or_else(|| "echo".into()),
                    route: s(&p, "route"),
                    chat_id: s(&p, "chat_id").unwrap_or_else(|| "room".into()),
                    msg_id: s(&p, "msg_id"),
                    dedup_id: None,
                    sender: s(&p, "sender").unwrap_or_else(|| "someone".into()),
                    sender_id: s(&p, "sender_id"),
                    sender_ctx: SenderContext {
                        platform: Some(s(&p, "ctx_platform").unwrap_or_else(|| "echo".into())),
                        chat_id: s(&p, "chat_id"),
                        ..Default::default()
                    },
                    text: s(&p, "text").unwrap_or_default(),
                    context: None,
                };
                self.host()?.inbound(&msg)?;
                Ok(Value::Null)
            }
            "echo.secret" => {
                let name = s(&p, "name").unwrap_or_default();
                let v = self.host()?.secret(&name).await?;
                Ok(json!({"value": v}))
            }
            "echo.resolve" => {
                let ctx = SenderContext {
                    platform: Some("echo".into()),
                    ..Default::default()
                };
                self.host()?
                    .resolve(s(&p, "sender_id").as_deref(), &ctx)
                    .await
            }
            // Drive the gateway's guard rails from the tests: a grant the
            // host must refuse (a foreign platform, `owner`), and a config
            // section the manifest does not declare.
            "echo.grant" => {
                let r = self
                    .host()?
                    .grant_scoped(
                        &s(&p, "identity").unwrap_or_default(),
                        &s(&p, "role").unwrap_or_else(|| "user".into()),
                        None,
                    )
                    .await;
                Ok(match r {
                    Ok(g) => json!({"granted": g.granted, "detail": g.detail}),
                    Err(e) => json!({"refused": e.message}),
                })
            }
            "echo.config_get" => {
                let r = self
                    .host()?
                    .config_get(&s(&p, "path").unwrap_or_default())
                    .await;
                Ok(match r {
                    Ok(v) => json!({"value": v}),
                    Err(e) => json!({"refused": e.message}),
                })
            }
            "echo.note" => {
                self.host()?.note(
                    &s(&p, "summary").unwrap_or_default(),
                    &s(&p, "detail").unwrap_or_default(),
                )?;
                Ok(Value::Null)
            }
            // The service it provides: what another plugin or core gets.
            "echo.upper" => Ok(json!({"text": s(&p, "text").unwrap_or_default().to_uppercase()})),
            // The point it overrides: what the layer below answers, with
            // "process" after it.
            "echotest.point" => {
                let next = s(&p, "next").unwrap_or_default();
                let input = p.get("input").cloned().unwrap_or(Value::Null);
                let mut v = self.host()?.service_next(&next, input).await?;
                match v.get_mut("via").and_then(Value::as_array_mut) {
                    Some(via) => via.push(json!("process")),
                    None => return Err(RpcError::internal(format!("no via list in {v}"))),
                }
                Ok(v)
            }
            // Call a service through the gateway, as the tests say.
            "echo.service_call" => {
                let r = self
                    .host()?
                    .call_service(
                        &s(&p, "name").unwrap_or_default(),
                        p.get("params").cloned().unwrap_or(Value::Null),
                        None,
                    )
                    .await;
                Ok(match r {
                    Ok(v) => v,
                    Err(e) => json!({"refused": e.message}),
                })
            }
            "echo.env" => Ok(json!(std::env::vars().map(|(k, _)| k).collect::<Vec<_>>())),
            "echo.pid" => Ok(json!(std::process::id())),
            "echo.crash" => {
                let code = p.get("code").and_then(Value::as_i64).unwrap_or(3) as i32;
                tracing::warn!("echo: crashing with {code}, as asked");
                std::process::exit(code);
            }
            other => Err(RpcError::method_not_found(other)),
        }
    }

    async fn shutdown(&self, reason: &str) {
        self.append("events.log", &format!("shutdown {reason}"));
        if self.hang_shutdown.load(Ordering::Acquire) {
            std::future::pending::<()>().await;
        }
    }
}

fn main() -> std::process::ExitCode {
    run(manifest(), Echo::default())
}

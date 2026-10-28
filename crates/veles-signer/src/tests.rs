//! The signer against stub wires: a stub Telegram Bot API and a stub IRC
//! server stand where the real ones do, and the real legs, the real HTTP
//! endpoint and a real Ed25519 key do everything else. Stub the wire,
//! not the function.

use super::*;
use std::collections::VecDeque;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const ORIGIN: &str = "bbbbbbbb-0000-4000-8000-00000000000b";
const TARGET: &str = "cccccccc-0000-4000-8000-00000000000c";
const OWNER: i64 = 4242;

fn request(origin: &str) -> (TicketRequest, String) {
    let req = TicketRequest::new(
        origin,
        TARGET,
        "sess-1",
        "cronjob",
        json!({ "action": "create", "schedule": "every 1h" }),
        "admin",
        600,
    )
    .expect("request");
    let token = req.encode().expect("encode");
    (req, token)
}

fn config(api_url: Option<String>, irc: Option<IrcConfig>) -> SignerConfig {
    SignerConfig {
        key: "test".into(),
        listen: "127.0.0.1:0".into(),
        wait_secs: 20,
        telegram: api_url.map(|api_url| TelegramConfig {
            owner_id: OWNER,
            chat_id: None,
            api_url,
        }),
        irc,
    }
}

fn secrets() -> Secrets {
    Secrets {
        gateway_tokens: vec![("phone".into(), "gw-token".into())],
        telegram_token: Some("TESTTOKEN".into()),
        irc_password: None,
    }
}

/// Start the HTTP side on a free port; returns its base URL and the
/// signer's public key config.
async fn start(cfg: SignerConfig, desk: Arc<Desk>) -> (String, String) {
    let (key, _) = SignerKey::generate().expect("key");
    let public = key.public_config();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(serve(cfg, secrets().gateway_tokens, key, desk, listener));
    (url, public)
}

async fn post_sign(url: &str, token: &str, bearer: &str) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("{url}/v1/sign"))
        .bearer_auth(bearer)
        .json(&json!({ "request": token }))
        .send()
        .await
        .expect("post");
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

/// The ticket verifies at the target, for exactly the call requested.
fn verifies(ticket: &str, public: &str, req: &TicketRequest) -> bool {
    let t = veles_core::ticket::Ticket::parse(ticket).expect("parse");
    let trusted =
        vec![veles_core::ticket::TrustedSigner::from_config("signer", public).expect("trusted")];
    let ctx = veles_core::ticket::CallContext {
        me: TARGET,
        origin: &req.origin,
        session: &req.session,
        tool: &req.tool,
        args: &req.args,
        now: chrono::Utc::now().timestamp(),
    };
    veles_core::ticket::verify(&t, &trusted, &ctx).is_ok()
}

// ── a stub Telegram Bot API ──────────────────────────────────────────

#[derive(Default)]
struct Tg {
    calls: Vec<(String, Value)>,
    updates: VecDeque<Value>,
    next_update: i64,
    next_message: i64,
}

type TgState = Arc<Mutex<Tg>>;

async fn tg_method(
    State(st): State<TgState>,
    axum::extract::Path((bot, method)): axum::extract::Path<(String, String)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_eq!(
        bot, "botTESTTOKEN",
        "the signer's own token, on its own bot"
    );
    if method == "getUpdates" {
        // A short long-poll: hand out what is queued, or nothing.
        for _ in 0..10 {
            let batch: Vec<Value> = st.lock().unwrap().updates.drain(..).collect();
            if !batch.is_empty() {
                return Json(json!({ "ok": true, "result": batch }));
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        return Json(json!({ "ok": true, "result": [] }));
    }
    let mut g = st.lock().unwrap();
    g.calls.push((method.clone(), body));
    g.next_message += 1;
    Json(json!({ "ok": true, "result": { "message_id": g.next_message } }))
}

async fn start_tg() -> (String, TgState) {
    let st: TgState = Arc::new(Mutex::new(Tg::default()));
    let app = Router::new()
        .route("/:bot/:method", post(tg_method))
        .with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, st)
}

fn push_update(st: &TgState, update: Value) {
    let mut g = st.lock().unwrap();
    g.next_update += 1;
    let mut u = update;
    u["update_id"] = json!(g.next_update);
    g.updates.push_back(u);
}

/// Wait until the stub has seen a call matching `pred`.
async fn tg_saw(st: &TgState, pred: impl Fn(&str, &Value) -> bool) -> Option<Value> {
    for _ in 0..200 {
        if let Some((_, b)) = st.lock().unwrap().calls.iter().find(|(m, b)| pred(m, b)) {
            return Some(b.clone());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

fn code_in(ask: &Value) -> String {
    ask["reply_markup"]["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .and_then(|d| d.strip_prefix("sign:"))
        .expect("a Sign button")
        .to_string()
}

#[tokio::test]
async fn the_owner_signs_with_the_button_and_nobody_else_can() {
    let (tg, st) = start_tg().await;
    let desk = Desk::new();
    tokio::spawn(telegram::run(
        telegram::Bot::new(&tg, "TESTTOKEN"),
        OWNER,
        None,
        desk.clone(),
    ));
    let (url, public) = start(config(Some(tg.clone()), None), desk.clone()).await;
    let (req, token) = request(ORIGIN);
    let pending = tokio::spawn({
        let url = url.clone();
        async move { post_sign(&url, &token, "gw-token").await }
    });
    let ask = tg_saw(&st, |m, b| {
        m == "sendMessage" && b["reply_markup"].is_object()
    })
    .await
    .expect("the owner was asked");
    assert_eq!(ask["chat_id"], json!(OWNER), "asked the owner, nobody else");
    let text = ask["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("cronjob") && text.contains("every 1h") && text.contains(ORIGIN),
        "the owner sees the whole call: {text}"
    );
    let code = code_in(&ask);
    // Somebody else presses Sign.
    push_update(
        &st,
        json!({ "callback_query": { "id": "q1", "from": { "id": 7 }, "data": format!("sign:{code}") } }),
    );
    tg_saw(&st, |m, b| {
        m == "answerCallbackQuery" && b["callback_query_id"] == "q1"
    })
    .await
    .expect("the stranger was answered");
    assert_eq!(
        desk.waiting(),
        vec![code.clone()],
        "a stranger's press signs nothing — the request still waits"
    );
    // The owner presses Sign.
    push_update(
        &st,
        json!({ "callback_query": { "id": "q2", "from": { "id": OWNER }, "data": format!("sign:{code}") } }),
    );
    let (status, body) = pending.await.expect("join");
    assert_eq!(status, 200, "{body}");
    let ticket = body["ticket"].as_str().expect("a ticket");
    assert!(
        verifies(ticket, &public, &req),
        "the ticket verifies at the target"
    );
    assert_eq!(body["approved_by"], json!(format!("telegram:{OWNER}")));
    tg_saw(&st, |m, b| {
        m == "editMessageText" && b["text"].as_str().is_some_and(|t| t.ends_with("signed"))
    })
    .await
    .expect("the question says it was signed");
}

#[tokio::test]
async fn a_typed_no_declines() {
    let (tg, st) = start_tg().await;
    let desk = Desk::new();
    tokio::spawn(telegram::run(
        telegram::Bot::new(&tg, "TESTTOKEN"),
        OWNER,
        None,
        desk.clone(),
    ));
    let (url, _) = start(config(Some(tg.clone()), None), desk).await;
    let (_, token) = request(ORIGIN);
    let pending = tokio::spawn({
        let url = url.clone();
        async move { post_sign(&url, &token, "gw-token").await }
    });
    let ask = tg_saw(&st, |m, b| {
        m == "sendMessage" && b["reply_markup"].is_object()
    })
    .await
    .expect("asked");
    let code = code_in(&ask);
    push_update(
        &st,
        json!({ "message": { "from": { "id": OWNER }, "chat": { "id": OWNER }, "text": format!("no {code}") } }),
    );
    let (status, body) = pending.await.expect("join");
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["declined"], json!(format!("telegram:{OWNER}")));
}

#[tokio::test]
async fn an_unknown_gateway_token_is_refused() {
    let desk = Desk::new();
    let (url, _) = start(config(Some("http://127.0.0.1:0".into()), None), desk).await;
    let (_, token) = request(ORIGIN);
    let (status, _) = post_sign(&url, &token, "not-the-token").await;
    assert_eq!(status, 401);
}

#[test]
fn replies_are_a_verb_and_a_code() {
    assert_eq!(parse_reply("yes ab12cd"), Some((true, "AB12CD".into())));
    assert_eq!(parse_reply("No AB12CD."), Some((false, "AB12CD".into())));
    assert_eq!(parse_reply("yes"), None);
    assert_eq!(
        parse_reply("yes AB12CD please"),
        None,
        "exactly the two words"
    );
    assert_eq!(parse_reply("maybe AB12CD"), None);
    assert_eq!(parse_reply("yes AB12"), None, "a code is six characters");
}

// ── a stub IRC server ────────────────────────────────────────────────

/// Accept one client, negotiate like a server granting account-tag, and
/// hand the test a way to send lines and read what the client sent.
async fn start_irc() -> (
    u16,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::sync::mpsc::UnboundedSender<String>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (seen_tx, seen_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (say_tx, mut say_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let (sock, _) = listener.accept().await.expect("accept");
        let (r, mut w) = sock.into_split();
        let mut lines = BufReader::new(r).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { return };
                    if line.starts_with("CAP REQ") {
                        let _ = w.write_all(b":stub CAP * ACK :account-tag\r\n").await;
                    } else if line.starts_with("USER") {
                        let _ = w.write_all(b":stub 001 vsigner :welcome\r\n").await;
                    }
                    let _ = seen_tx.send(line);
                }
                say = say_rx.recv() => {
                    let Some(say) = say else { return };
                    let _ = w.write_all(format!("{say}\r\n").as_bytes()).await;
                }
            }
        }
    });
    (port, seen_rx, say_tx)
}

async fn irc_saw(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    pred: impl Fn(&str) -> bool,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Some(line)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if pred(&line) {
            return Some(line);
        }
    }
    None
}

#[tokio::test]
async fn on_irc_only_the_owners_account_signs() {
    let (port, mut seen, say) = start_irc().await;
    let irc = IrcConfig {
        server: "127.0.0.1".into(),
        port,
        tls: false,
        nick: "vsigner".into(),
        owner_nick: "wrath".into(),
        owner_account: Some("wrath".into()),
    };
    let desk = Desk::new();
    tokio::spawn(irc_leg::run(irc.clone(), None, desk.clone()));
    irc_saw(&mut seen, |l| l.starts_with("CAP END"))
        .await
        .expect("negotiation ended");
    let (url, public) = start(config(None, Some(irc)), desk).await;
    let (req, token) = request(ORIGIN);
    let pending = tokio::spawn({
        let url = url.clone();
        async move { post_sign(&url, &token, "gw-token").await }
    });
    let ask = irc_saw(&mut seen, |l| {
        l.starts_with("PRIVMSG wrath :ticket request")
    })
    .await
    .expect("the owner's nick was asked");
    let code: String = ask
        .split("ticket request ")
        .nth(1)
        .unwrap_or_default()
        .chars()
        .take(6)
        .collect();
    // The nick, but not the account: someone who took the nick.
    say.send(format!(
        "@account=mallory :wrath!u@h PRIVMSG vsigner :yes {code}"
    ))
    .expect("say");
    irc_saw(&mut seen, |l| l.starts_with("NOTICE wrath :not counted"))
        .await
        .expect("the impostor was told");
    // The owner, logged in.
    say.send(format!(
        "@account=wrath :wrath!u@h PRIVMSG vsigner :yes {code}"
    ))
    .expect("say");
    let (status, body) = pending.await.expect("join");
    assert_eq!(status, 200, "{body}");
    assert!(verifies(
        body["ticket"].as_str().expect("ticket"),
        &public,
        &req
    ));
    assert_eq!(body["approved_by"], json!("irc:wrath"));
}

/// USER 2026-10-21 ("Same bot, dedicated ops chat"): the request is
/// shown in the ops chat when `[telegram] chat_id` names one, and the
/// owner answers it there; the owner's user id, not the chat, is what
/// signs.
#[tokio::test]
async fn an_ops_chat_shows_the_request_and_the_owner_answers_there() {
    const OPS: i64 = -1001234;
    let (tg, st) = start_tg().await;
    let desk = Desk::new();
    tokio::spawn(telegram::run(
        telegram::Bot::new(&tg, "TESTTOKEN"),
        OWNER,
        Some(OPS),
        desk.clone(),
    ));
    let (url, public) = start(config(Some(tg.clone()), None), desk.clone()).await;
    let (req, token) = request(ORIGIN);
    let pending = tokio::spawn({
        let url = url.clone();
        async move { post_sign(&url, &token, "gw-token").await }
    });
    let ask = tg_saw(&st, |m, b| {
        m == "sendMessage" && b["reply_markup"].is_object()
    })
    .await
    .expect("the request was shown");
    assert_eq!(
        ask["chat_id"],
        json!(OPS),
        "shown in the ops chat, not the DM"
    );
    let code = code_in(&ask);
    // Somebody else in the ops chat presses Sign: nothing.
    push_update(
        &st,
        json!({ "callback_query": { "id": "q1", "from": { "id": 7 }, "data": format!("sign:{code}") } }),
    );
    tg_saw(&st, |m, b| {
        m == "answerCallbackQuery" && b["callback_query_id"] == "q1"
    })
    .await
    .expect("the stranger was answered");
    assert_eq!(desk.waiting(), vec![code.clone()], "still waiting");
    // The owner presses Sign, in the ops chat.
    push_update(
        &st,
        json!({ "callback_query": { "id": "q2", "from": { "id": OWNER }, "data": format!("sign:{code}") } }),
    );
    let (status, body) = pending.await.expect("join");
    assert_eq!(status, 200, "{body}");
    let ticket = body["ticket"].as_str().expect("a ticket");
    assert!(verifies(ticket, &public, &req));
    assert_eq!(body["approved_by"], json!(format!("telegram:{OWNER}")));
    let settled = tg_saw(&st, |m, b| {
        m == "editMessageText" && b["text"].as_str().is_some_and(|t| t.ends_with("signed"))
    })
    .await
    .expect("the question says it was signed");
    assert_eq!(settled["chat_id"], json!(OPS), "edited where it was shown");
}

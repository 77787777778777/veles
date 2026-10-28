//! Web tools — web_search + web_extract.
//!
//! #25 in the feature matrix. Two tools:
//! - `web_search`: queries the backends configured under `[web].backends`
//!   in order, first answer wins. Default ladder: local SearXNG
//!   (keyless, aggregates brave/mojeek/ddg/wikipedia, immune to any one
//!   engine's bot wall) → Brave HTML scraping → DuckDuckGo HTML/Lite
//!   scraping. DDG alone was measured at 1-of-5 queries on 2026-09-15
//!   (bot-challenged); it is a fallback, not a primary.
//! - `web_extract`: fetches a URL and returns cleaned text (HTML→text,
//!   readability-style extraction). No JS execution — for dynamic pages
//!   the model should reach for the browser tool.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::Tool;

/// Search results, normalized across backends.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub(crate) fn render_results(results: &[SearchResult], max: usize) -> String {
    results
        .iter()
        .take(max)
        .enumerate()
        .map(|(i, r)| {
            if r.snippet.is_empty() {
                format!("{}. {}\n   {}", i + 1, r.title, r.url)
            } else {
                format!("{}. {}\n   {}\n   {}", i + 1, r.title, r.url, r.snippet)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ── web_search ───────────────────────────────────────────────────────

pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web. Returns titles, URLs, and snippets for the top results."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "max_results": {
                    "type": "integer",
                    "description": "Max results to return (default 8, max 20)"
                }
            },
            "required": ["query"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let query = args["query"].as_str().unwrap_or_default().to_string();
        if query.is_empty() {
            return "error: query is required".into();
        }
        let max = args["max_results"].as_u64().unwrap_or(8).clamp(1, 20) as usize;

        // Fresh load per call, the house pattern (configedit.rs): flipping
        // [web] takes effect without a restart.
        let cfg = crate::config::load().unwrap_or_default();
        let mut web = cfg.web.clone();
        let mut backends: Vec<String> = if web.backends.is_empty() {
            vec!["searxng".into(), "brave".into(), "ddg".into()]
        } else {
            web.backends.clone()
        };
        // Someone other than the owner (egress.rs): their search goes
        // only where `[web] others` sends it. The direct backends would
        // leave from this node's own address.
        if crate::egress::others().is_some() {
            match crate::egress::Others::from_web(&web) {
                crate::egress::Others::Direct => {}
                crate::egress::Others::Refuse => return crate::egress::refusal("web_search"),
                crate::egress::Others::Proxy { searxng: None, .. } => {
                    return crate::egress::unset("web_search", "others_searxng_url")
                }
                crate::egress::Others::Proxy {
                    searxng: Some(url), ..
                } => {
                    web.searxng_url = url;
                    web.searxng_wake_cmd = None;
                    backends = vec!["searxng".into()];
                }
            }
        }
        let web = &web;

        let mut errs = Vec::new();
        for b in backends.iter().map(String::as_str) {
            let attempt = match b {
                "searxng" => searxng_search(web, &query).await,
                "brave" => brave_search(web, &query).await,
                "ddg" => match ddg_search(web, &query).await {
                    Ok(r) => Ok(r),
                    Err(e) => ddg_lite_search(web, &query)
                        .await
                        .map_err(|e2| format!("{e}; lite: {e2}")),
                },
                other => {
                    errs.push(format!("{other}: unknown backend"));
                    continue;
                }
            };
            match attempt {
                Ok(results) if !results.is_empty() => return render_results(&results, max),
                Ok(_) => errs.push(format!("{b}: no results")),
                Err(e) => errs.push(format!("{b}: {e}")),
            }
        }
        format!(
            "web_search error: all backends failed — {}",
            errs.join(" | ")
        )
    }
}

fn web_client(timeout_secs: u64) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0")
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| e.to_string())
}

/// Local SearXNG JSON API. The primary keyless backend: it fans the
/// query out to its own engine list server-side, so one loopback
/// request answers even when a single engine is bot-blocking us.
async fn searxng_search(
    web: &crate::config::WebSection,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    // mtime of the stamp is the idle-watcher's only signal of use.
    touch_use_stamp();
    let client = web_client(web.searxng_timeout_secs)?;
    let resp = match client
        .get(format!("{}/search", web.searxng_url.trim_end_matches('/')))
        .query(&[("q", query), ("format", "json")])
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            // Refused = the container is asleep (the idle-watcher stops
            // it after a quiet spell). Wake it DETACHED for the next
            // query; this one falls through to brave, which is exactly
            // the deal the ladder makes with a cold window. Only
            // connect-class errors wake — a timeout or an HTTP 5xx is a
            // different problem that a restart cannot fix, and firing
            // systemctl on every one of those would be noise.
            if e.is_connect() {
                wake_searxng(web);
            }
            return Err(format!("unreachable: {e}"));
        }
    };
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| format!("bad json: {e}"))?;
    let results = body["results"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|r| SearchResult {
            title: strip_tags(r["title"].as_str().unwrap_or_default()),
            url: r["url"].as_str().unwrap_or_default().to_string(),
            snippet: strip_tags(r["content"].as_str().unwrap_or_default()),
        })
        .filter(|r| !r.url.is_empty() && !r.title.is_empty())
        .collect::<Vec<_>>();
    if results.is_empty() {
        return Err("no results parsed".into());
    }
    Ok(results)
}

/// `$XDG_CACHE_HOME/veles/searxng-last-use`. Content is irrelevant;
/// the mtime is what `searxng-idle.timer` reads to decide whether the
/// container has been quiet long enough to stop. Paired with the
/// quadlet + watcher units documented in COVERAGE.md (this laptop's
/// `~/.config/containers/systemd/`).
pub fn searxng_stamp_path() -> std::path::PathBuf {
    crate::config::cache_dir().join("searxng-last-use")
}

fn touch_use_stamp() {
    let p = searxng_stamp_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&p, b"");
}

/// Fire the configured wake command, detached. `systemctl start` on a
/// quadlet blocks until the container is up (~5s), and the caller is
/// already on its way to the fallback backend — a awaited wake would
/// just make the cold query slower for no gain. Failure is swallowed
/// by design: the ladder answers either way, and this runs on boxes
/// with no such unit at all.
fn wake_searxng(web: &crate::config::WebSection) {
    let Some(cmd) = web
        .searxng_wake_cmd
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    else {
        return;
    };
    let cmd = cmd.to_string();
    crate::rt::spawn(async move {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .output(),
        )
        .await;
    });
}

/// Brave search HTML scraping (https://search.brave.com/search?q=...).
/// No API key required. Measured 5-of-5 clean from this box on
/// 2026-09-15 while DDG was bot-challenged; parse shape documented in
/// parse_brave_html.
async fn brave_search(
    web: &crate::config::WebSection,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let client = web_client(web.remote_timeout_secs)?;
    let resp = client
        .get("https://search.brave.com/search")
        .query(&[("q", query)])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let html = resp.text().await.map_err(|e| e.to_string())?;
    let results = parse_brave_html(&html);
    if results.is_empty() {
        return Err("no results parsed".into());
    }
    Ok(results)
}

/// Brave lays each web result out as
/// `<div data-type="web" …><div class="result-body …">
///    <a href="URL" …> … <div class="title …" title="TITLE">…</div></a>
///    … <div class="snippet-content …">SNIPPET</div> …`
/// Split on the data-type marker and pull the three fields out of the
/// block. Empty href (video/discussion cards) → skip.
fn parse_brave_html(html: &str) -> Vec<SearchResult> {
    let mut out = Vec::new();
    for block in html.split("data-type=\"web\"").skip(1) {
        let body = block.split("data-type=").next().unwrap_or(block);
        let url = match extract_between(body, "<a href=\"", "\"") {
            Some(u) if u.starts_with("http") => u.to_string(),
            _ => continue,
        };
        let title = div_text(body, "class=\"title")
            .map(strip_tags)
            .unwrap_or_default();
        let snippet = div_text(body, "class=\"snippet-content")
            .map(strip_tags)
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        out.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    out
}

/// DuckDuckGo HTML search (https://html.duckduckgo.com/html/?q=...).
/// No API key required. Parse the result blocks.
async fn ddg_search(
    web: &crate::config::WebSection,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let client = web_client(web.remote_timeout_secs)?;
    let resp = client
        .get("https://html.duckduckgo.com/html/")
        .query(&[("q", query)])
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let html = resp.text().await.map_err(|e| e.to_string())?;
    let results = parse_ddg_html(&html);
    if results.is_empty() {
        return Err("no results parsed".into());
    }
    Ok(results)
}

/// DuckDuckGo Lite fallback (https://lite.duckduckgo.com/lite/).
async fn ddg_lite_search(
    web: &crate::config::WebSection,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let client = web_client(web.remote_timeout_secs)?;
    let resp = client
        .get("https://lite.duckduckgo.com/lite/")
        .query(&[("q", query)])
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let html = resp.text().await.map_err(|e| e.to_string())?;
    let results = parse_ddg_lite(&html);
    if results.is_empty() {
        return Err("no results parsed".into());
    }
    Ok(results)
}

/// Parse DuckDuckGo HTML results page. Result links are in
/// `<a class="result__a" href="...">Title</a>` with snippets in
/// `<a class="result__snippet">...</a>`.
fn parse_ddg_html(html: &str) -> Vec<SearchResult> {
    let mut out = Vec::new();

    // Split by result blocks
    for block in html.split("class=\"result__body\"") {
        if block.is_empty() {
            continue;
        }

        // Extract URL: find the result__a anchor, then href="..." within it
        let anchor_start = block.find("class=\"result__a\"");
        let url = if let Some(idx) = anchor_start {
            let after = &block[idx..];
            extract_between(after, "href=\"", "\"")
                .map(decode_ddg_url)
                .unwrap_or_default()
        } else {
            String::new()
        };

        // Extract title: text between the first '>' and '</a>' after result__a
        let title = if let Some(idx) = anchor_start {
            let after = &block[idx..];
            if let Some(gt) = after.find('>') {
                let inner = &after[gt + 1..];
                let end = inner.find("</a>").unwrap_or(inner.len());
                Some(strip_tags(&inner[..end]))
            } else {
                None
            }
        } else {
            None
        }
        .unwrap_or_default();

        // Extract snippet
        let snippet = extract_between(block, "class=\"result__snippet\"", "</a>")
            .or_else(|| extract_between(block, "class=\"result__snippet\">", "</td>"))
            .map(strip_tags)
            .unwrap_or_default();

        if !title.is_empty() {
            out.push(SearchResult {
                title,
                url,
                snippet,
            });
        }
    }

    out
}

/// Parse DuckDuckGo Lite results (table-based layout).
fn parse_ddg_lite(html: &str) -> Vec<SearchResult> {
    let mut out = Vec::new();

    for block in html.split("<a").skip(1) {
        // href in lite is direct
        let url = extract_between(block, "href=\"", "\"")
            .unwrap_or_default()
            .to_string();
        if url.is_empty() || url.starts_with("javascript:") {
            continue;
        }

        let title = extract_between(block, ">", "</a>")
            .map(strip_tags)
            .unwrap_or_default();

        if title.is_empty() || title.len() < 3 {
            continue;
        }

        out.push(SearchResult {
            title,
            url,
            snippet: String::new(),
        });
    }

    out
}

/// Text content of the first `<div ...{marker}...>` after which the
/// marker occurs: skip to the `>` that ends the opening tag, then read
/// to that div's `</div>`. Brave's title div carries a `title=`
/// attribute whose value is often entity-encoded junk — the visible
/// text is the honest one.
fn div_text<'a>(hay: &'a str, marker: &str) -> Option<&'a str> {
    let i = hay.find(marker)?;
    let after = &hay[i..];
    let gt = after.find('>')?;
    let inner = &after[gt + 1..];
    inner.find("</div>").map(|e| &inner[..e])
}

/// DuckDuckGo wraps result URLs in a redirect: //duckduckgo.com/l/?uddg=ENCODED
/// Decode to the real URL.
fn decode_ddg_url(raw: &str) -> String {
    // Strip leading // or https://
    let raw = raw.strip_prefix("//").unwrap_or(raw);
    if let Some(rest) = raw.strip_prefix("duckduckgo.com/l/?uddg=") {
        // URL-decode
        let decoded = url_decode(rest);
        // Remove trailing & params
        decoded.split('&').next().unwrap_or(&decoded).to_string()
    } else {
        raw.to_string()
    }
}

fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"),
                16,
            ) {
                out.push(b as char);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    // Collapse whitespace and decode common entities
    out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let si = s.find(start)?;
    let after = &s[si + start.len()..];
    let ei = after.find(end)?;
    Some(&after[..ei])
}

// ── web_extract ──────────────────────────────────────────────────────

pub struct WebExtractTool;

#[async_trait]
impl Tool for WebExtractTool {
    fn name(&self) -> &str {
        "web_extract"
    }
    fn description(&self) -> &str {
        "Fetch a web page and extract the main text content as clean markdown-ish text. No JavaScript execution — for dynamic pages use the browser tool."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "URL to fetch"},
                "max_chars": {
                    "type": "integer",
                    "description": "Max characters to return (default 50000)"
                }
            },
            "required": ["url"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let url = args["url"].as_str().unwrap_or_default().to_string();
        if url.is_empty() {
            return "error: url is required".into();
        }
        let max_chars = args["max_chars"].as_u64().unwrap_or(50_000) as usize;
        // Someone other than the owner (egress.rs): their fetch leaves
        // through `[web] others_proxy`, on a circuit of its own, or not
        // from this node at all.
        let proxy = match crate::egress::fetch_proxy("web_extract") {
            Ok(p) => p,
            Err(e) => return e,
        };

        shape_output(fetch_and_extract(&url, max_chars, proxy.as_deref()).await)
    }
}

/// The tool's text. Its first line is where the session's node looks for
/// a challenge (`challenge::marked`): a challenge refusal keeps the
/// marker there, and page text may not start with it — a plain-text page
/// could otherwise claim a challenge and send itself to the browser.
fn shape_output(r: Result<String, String>) -> String {
    match r {
        Ok(text) if text.starts_with(crate::challenge::MARKER) => format!(" {text}"),
        Ok(text) => text,
        Err(e) if e.starts_with(crate::challenge::MARKER) => e,
        Err(e) => format!("web_extract error: {e}"),
    }
}

/// How long a page has to answer and arrive.
const PAGE_SECS: std::time::Duration = std::time::Duration::from_secs(20);
/// How long a downloaded file has to arrive.
const FILE_SECS: std::time::Duration = std::time::Duration::from_secs(300);

async fn fetch_and_extract(
    url: &str,
    max_chars: usize,
    proxy: Option<&str>,
) -> Result<String, String> {
    // Guarded: `web_extract` takes a URL from the model, which takes it
    // from whoever is talking, and returns the page's text to them.
    // That is a read of anything the gateway can reach.
    // A page answers, and arrives, within PAGE_SECS. A file may take
    // longer to arrive (a 60 MB scan), so the client's whole-request
    // budget is FILE_SECS and the page's is kept by hand.
    let started = std::time::Instant::now();
    let page_left = || PAGE_SECS.saturating_sub(started.elapsed());
    let resp = tokio::time::timeout(
        PAGE_SECS,
        crate::urlguard::get_through(
            url,
            FILE_SECS,
            Some("Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0"),
            proxy,
        ),
    )
    .await
    .map_err(|_| format!("no answer within {}s", PAGE_SECS.as_secs()))??;
    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    // Text is decoded as the server says (its charset). Anything else is
    // read as bytes: a PDF is downloaded rather than read, and the
    // session's node puts it where the model's commands run
    // (`downloads::deliver`).
    if is_text_type(&header(&headers, "content-type")) {
        let body = tokio::time::timeout(page_left(), resp.text())
            .await
            .map_err(|_| format!("the page did not arrive within {}s", PAGE_SECS.as_secs()))?
            .map_err(|e| e.to_string())?;
        return extract_response(url, status, &headers, body, max_chars);
    }
    let bytes = read_capped(resp, crate::downloads::MAX_BYTES).await?;
    if (200..300).contains(&status) && crate::downloads::is_pdf(&bytes) {
        return download_pdf(&final_url, &headers, &bytes);
    }
    let body = String::from_utf8_lossy(&bytes).into_owned();
    extract_response(url, status, &headers, body, max_chars)
}

fn header(headers: &[(String, String)], name: &str) -> String {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_text_type(ctype: &str) -> bool {
    ctype.starts_with("text/")
        || ["html", "json", "xml", "javascript"]
            .iter()
            .any(|t| ctype.contains(t))
}

/// The body, refused past `cap` without reading the rest.
async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    let over = || {
        format!(
            "the response is over {}, more than a fetch reads",
            crate::downloads::human_size(cap)
        )
    };
    if resp.content_length().is_some_and(|n| n > cap as u64) {
        return Err(over());
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if buf.len() + chunk.len() > cap {
            return Err(over());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// A PDF is staged as a download: the tool's output is its one
/// `downloads::line`, which the session's node turns into a path.
fn download_pdf(url: &str, headers: &[(String, String)], bytes: &[u8]) -> Result<String, String> {
    let meta = crate::downloads::Meta {
        name: disposition_name(headers).unwrap_or_else(|| crate::downloads::name_from_url(url)),
        mime: "application/pdf".into(),
        url: url.to_string(),
    };
    let token = crate::downloads::stage(bytes, &meta).map_err(|e| format!("{e:#}"))?;
    Ok(crate::downloads::line(&token))
}

/// `Content-Disposition`'s file name, when the server gives one.
fn disposition_name(headers: &[(String, String)]) -> Option<String> {
    let raw = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-disposition"))?
        .1
        .clone();
    raw.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        let v = match k.trim().to_ascii_lowercase().as_str() {
            // RFC 5987: `UTF-8''name.pdf`
            "filename*" => v.rsplit("''").next()?,
            "filename" => v,
            _ => return None,
        };
        let v = v.trim().trim_matches('"');
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// Everything after the fetch: what the page says, or why it says
/// nothing. Pure, so the challenge refusal is testable without a network
/// (urlguard refuses loopback, as it should).
fn extract_response(
    url: &str,
    status: u16,
    headers: &[(String, String)],
    body: String,
    max_chars: usize,
) -> Result<String, String> {
    // A bot challenge is not the page (PLAN-pool-round.md Part E). The
    // fetch cannot solve it, and handing the model "Just a moment…" as if
    // it were the content is how a research turn quietly learns nothing.
    // The browser can: there it is put in front of the user.
    if let Some(c) = crate::challenge::in_response(status, headers, &body) {
        return Err(challenge_refusal(url, &c));
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    let content_type = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();

    // If it's not HTML, return raw text (truncated). Cut on a character,
    // not a byte: slicing inside a multi-byte character panics.
    if !content_type.contains("text/html") && !content_type.contains("application/xhtml") {
        let truncated = match body.char_indices().nth(max_chars) {
            Some((i, _)) => format!("{}...\n[truncated at {max_chars} chars]", &body[..i]),
            None => body,
        };
        return Ok(truncated);
    }

    // HTML → text extraction
    let extracted = html_to_text(&body, max_chars);
    if extracted.is_empty() {
        return Err("no text content extracted".into());
    }
    Ok(extracted)
}

/// The refusal. Its FIRST line is the challenge marker, as a browser
/// tool's is: on the session's node that line opens the page in a
/// browser and puts the challenge to a person (`tools.rs`, the
/// web_extract escalation, PLAN-challenge-relay.md step 1). Where
/// nothing does that (no session), the rest says what happened.
fn challenge_refusal(url: &str, c: &crate::challenge::Challenge) -> String {
    format!(
        "{}\n{url} is behind a {} — a fetch cannot get past it, and nobody was \
         asked to solve it.",
        crate::challenge::marker_line(c, url),
        c.line()
    )
}

/// Convert HTML to clean text. A simplified readability extraction:
/// 1. Remove script/style/nav/footer/aside tags
/// 2. Extract <title>, <h1>-<h6>, <p>, <li>, <pre>, <code>, <blockquote>
/// 3. Collapse whitespace
fn html_to_text(html: &str, max_chars: usize) -> String {
    // Extract <title>
    let title = extract_between(html, "<title>", "</title>")
        .map(|t| strip_tags(t).trim().to_string())
        .unwrap_or_default();

    // Remove unwanted sections
    let mut cleaned = html.to_string();
    for tag in &[
        "script", "style", "nav", "footer", "aside", "noscript", "header",
    ] {
        cleaned = remove_tag(&cleaned, tag);
    }

    // Convert relevant tags to text with markers
    let mut out = String::with_capacity(max_chars.min(cleaned.len()));

    if !title.is_empty() {
        out.push_str("# ");
        out.push_str(&title);
        out.push_str("\n\n");
    }

    // Extract article/main/body content
    let body = extract_between(&cleaned, "<body", "</body>").unwrap_or(&cleaned);

    // Tokenize into blocks: headings, paragraphs, list items, pre, blockquote
    let chars = body.chars().peekable();
    let mut current_tag = String::new();
    let mut in_tag = false;
    let mut text_buf = String::new();

    for ch in chars {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let tag_lower = current_tag.to_lowercase();
                // Check for block-level closing tags → flush text
                if tag_lower.starts_with("/p")
                    || tag_lower.starts_with("/h")
                    || tag_lower.starts_with("/li")
                    || tag_lower.starts_with("/pre")
                    || tag_lower.starts_with("/blockquote")
                    || tag_lower.starts_with("/div")
                    || tag_lower.starts_with("/td")
                    || tag_lower.starts_with("/br")
                {
                    let cleaned_text = strip_tags(&text_buf).trim().to_string();
                    if !cleaned_text.is_empty() {
                        out.push_str(&cleaned_text);
                        out.push_str("\n\n");
                        if out.len() >= max_chars {
                            out.push_str("[...truncated]");
                            return out;
                        }
                    }
                    text_buf.clear();
                }
                current_tag.clear();
            } else {
                current_tag.push(ch);
            }
        } else if ch == '<' {
            in_tag = true;
            // Flush accumulated text
            if !text_buf.trim().is_empty() {
                text_buf.push(' ');
            }
        } else {
            text_buf.push(ch);
        }
    }

    // Flush remaining
    let remaining = strip_tags(&text_buf).trim().to_string();
    if !remaining.is_empty() {
        out.push_str(&remaining);
    }

    // Collapse multiple blank lines
    let mut final_out = String::with_capacity(out.len());
    let mut blank = false;
    for line in out.lines() {
        if line.trim().is_empty() {
            if !blank {
                final_out.push('\n');
                blank = true;
            }
        } else {
            final_out.push_str(line.trim_end());
            final_out.push('\n');
            blank = false;
        }
    }

    final_out
}

fn remove_tag(html: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.to_lowercase().find(&open.to_lowercase()) {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        if let Some(end_idx) = after.to_lowercase().find(&close.to_lowercase()) {
            rest = &after[end_idx + close.len()..];
        } else {
            // No closing tag; skip to end
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A challenge page is not the page: web_extract says so, and says
    /// to open it in the browser, where the user can solve it.
    #[test]
    fn a_challenge_page_is_refused_with_the_way_through() {
        let h = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let cf = extract_response(
            "https://shop.example/item",
            403,
            &h(&[("cf-mitigated", "challenge"), ("content-type", "text/html")]),
            "<title>Just a moment...</title>".into(),
            50_000,
        )
        .unwrap_err();
        assert!(cf.contains("cloudflare challenge"), "{cf}");
        assert!(
            crate::challenge::marked(&cf),
            "the first line starts the escalation: {cf}"
        );
        let ok = extract_response(
            "https://example.com/",
            200,
            &h(&[("content-type", "text/html"), ("server", "cloudflare")]),
            "<html><head><title>Example</title></head><body><p>Plain page.</p></body></html>"
                .into(),
            50_000,
        )
        .unwrap();
        assert!(ok.contains("Plain page."), "{ok}");
    }

    /// A PDF is downloaded rather than read (USER, 2026-10-12): its bytes
    /// are staged and the output is one download line, which the
    /// session's node turns into a path. The server's file name wins over
    /// the URL's.
    #[tokio::test]
    async fn a_pdf_is_downloaded_not_read() {
        let (_home, _env) = crate::testenv::scratch_home();
        assert!(crate::downloads::is_pdf(b"%PDF-1.6\n%\xe2\xe3"));
        assert!(
            crate::downloads::is_pdf(b"\xef\xbb\xbf\r\n%PDF-1.4"),
            "the format allows a lead-in"
        );
        assert!(!crate::downloads::is_pdf(b"PK\x03\x04 a zip"));
        for (ctype, text) in [
            ("text/html; charset=utf-8", true),
            ("application/json", true),
            ("application/xhtml+xml", true),
            ("application/pdf", false),
            ("application/octet-stream", false),
            ("", false),
        ] {
            assert_eq!(is_text_type(ctype), text, "{ctype}");
        }
        let url = "https://pure.example/ws/portalfiles/portal/60371196/Published.pdf";
        let out = download_pdf(url, &[], b"%PDF-1.6 body").unwrap();
        assert_eq!(out.lines().count(), 1, "{out}");
        let got = crate::downloads::deliver("", out).await;
        assert!(
            got.replace('\\', "/").contains("/downloads/Published-") && got.contains(url),
            "{got}"
        );
        let named = [(
            "Content-Disposition".to_string(),
            "attachment; filename=\"report 2024.pdf\"".to_string(),
        )];
        let got =
            crate::downloads::deliver("", download_pdf(url, &named, b"%PDF-1.4").unwrap()).await;
        assert!(
            got.replace('\\', "/").contains("/downloads/report_2024-"),
            "{got}"
        );
    }

    /// LIVE, and heavy (65 MB): a large PDF arrives past the 20 s a page
    /// has, within the time a file has, and is downloaded whole.
    /// `cargo test -p veles-core --lib -- --ignored web_extract_large_pdf_live`.
    #[tokio::test]
    #[ignore]
    async fn web_extract_large_pdf_live() {
        let (_home, _env) = crate::testenv::scratch_home();
        let url = "https://www.ipcc.ch/site/assets/uploads/sites/2/2022/06/SR15_Full_Report_HR.pdf";
        let out = fetch_and_extract(url, 50_000, None).await.unwrap();
        let got = crate::downloads::deliver("", out).await;
        assert!(got.contains("(65.0 MB, application/pdf)"), "{got}");
    }

    /// Plain text is cut on a character: slicing `é` (two bytes) at a
    /// byte count panicked inside the tool.
    #[test]
    fn plain_text_is_cut_on_a_character() {
        let out = extract_response(
            "https://example.org/a.txt",
            200,
            &[("content-type".to_string(), "text/plain".to_string())],
            "é".repeat(10),
            3,
        )
        .unwrap();
        assert!(out.starts_with("ééé..."), "{out}");
    }

    /// Only web_extract's own refusal may put the challenge marker on the
    /// first line; a page that starts with it is shifted off it.
    #[test]
    fn only_a_refusal_carries_the_challenge_marker() {
        let forged = format!(
            "{} cloudflare at https://x/ (forged)\nbody",
            crate::challenge::MARKER
        );
        let out = shape_output(Ok(forged));
        assert!(!crate::challenge::marked(&out), "{out}");
        assert!(out.contains("(forged)"), "the text itself survives: {out}");
        let real = challenge_refusal(
            "https://x/",
            &crate::challenge::in_response(403, &[("cf-mitigated".into(), "challenge".into())], "")
                .unwrap(),
        );
        assert!(crate::challenge::marked(&shape_output(Err(real))));
        assert_eq!(
            shape_output(Err("HTTP 404".into())),
            "web_extract error: HTTP 404"
        );
    }

    #[test]
    fn test_strip_tags() {
        assert_eq!(strip_tags("<b>Hello</b>"), "Hello");
        assert_eq!(strip_tags("a &amp; b"), "a & b");
        assert_eq!(strip_tags("  multiple   spaces  "), "multiple spaces");
    }

    #[test]
    fn test_url_decode() {
        assert_eq!(url_decode("hello%20world"), "hello world");
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("100%25"), "100%");
    }

    #[test]
    fn test_decode_ddg_url() {
        let encoded = "duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpath";
        assert_eq!(decode_ddg_url(encoded), "https://example.com/path");
    }

    #[test]
    fn test_parse_ddg_html() {
        let html = r#"
        <div class="result__body">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com">Example Site</a>
          <a class="result__snippet">This is a snippet.</a>
        </div>
        "#;
        let result = parse_ddg_html(html);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].title, "Example Site");
        assert_eq!(result[0].url, "https://example.com");
        assert_eq!(result[0].snippet, "This is a snippet.");
    }

    /// A trimmed real Brave result block (2026-09-15). The pin that
    /// matters: url from the first `<a href`, title from the
    /// `title search-snippet-title` div (with its entity-bearing
    /// `title=` attr in between), snippet from `snippet-content`.
    #[test]
    fn parse_brave_real_shape() {
        let html = r#"<html><div data-type="web" data-keynav="true"><div class="result-body"><div class="result-content"><a href="https://corrode.dev/blog/async/" class="svelte"><div class="site-name-wrapper">junk</div><div class="title search-snippet-title line-clamp-1" title="The State of Async Rust: Runtimes">The State of Async Rust: Runtimes</div></a><div class="snippet-description"><div class="snippet-content svelte-3n8qdv">An overview of async runtimes &amp; tools.</div></div></div></div><div data-type="video"><a href="https://youtu.be/x">should be skipped</a></div></html>"#;
        let results = parse_brave_html(html);
        assert_eq!(results.len(), 1, "video block must not parse as web");
        assert_eq!(results[0].url, "https://corrode.dev/blog/async/");
        assert_eq!(results[0].title, "The State of Async Rust: Runtimes");
        assert_eq!(results[0].snippet, "An overview of async runtimes & tools.");
    }

    #[test]
    fn render_results_numbers_and_caps() {
        let r = |i: usize| SearchResult {
            title: format!("t{i}"),
            url: format!("u{i}"),
            snippet: String::new(),
        };
        let all = vec![r(1), r(2), r(3)];
        let out = render_results(&all, 2);
        assert!(out.starts_with("1. t1\n   u1\n\n2. t2\n   u2"));
        assert!(!out.contains("t3"), "max truncates");
    }

    #[test]
    fn web_section_defaults_and_parse() {
        let d = crate::config::WebSection::default();
        assert_eq!(d.backends, vec!["searxng", "brave", "ddg"]);
        assert_eq!(d.searxng_url, "http://127.0.0.1:8888");
        assert_eq!(
            d.searxng_wake_cmd.as_deref(),
            Some("systemctl --user start searxng.service")
        );
        // Absent section = defaults, not an error.
        let cfg: crate::config::VelesConfig = toml::from_str("").expect("parses");
        assert_eq!(cfg.web.backends, d.backends);
        assert_eq!(cfg.web.searxng_wake_cmd, d.searxng_wake_cmd);
        let cfg: crate::config::VelesConfig =
            toml::from_str("[web]\nbackends = [\"brave\"]\n").expect("parses");
        assert_eq!(cfg.web.backends, vec!["brave"]);
        // Empty (or whitespace) disables the wake attempt on boxes
        // with no such unit — wake_searxng trims and checks, pinned by
        // the None/empty early-returns there.
        let cfg: crate::config::VelesConfig =
            toml::from_str("[web]\nsearxng_wake_cmd = \"\"\n").expect("parses");
        assert_eq!(cfg.web.searxng_wake_cmd.as_deref(), Some(""));
        let cfg: crate::config::VelesConfig =
            toml::from_str("[web]\nsearxng_wake_cmd = \"true\"\n").expect("parses");
        assert_eq!(cfg.web.searxng_wake_cmd.as_deref(), Some("true"));
    }

    /// The stamp lives exactly where the idle-watcher shell script
    /// reads it: `$XDG_CACHE_HOME/veles/searxng-last-use`, honoring a
    /// thread dir override the way every tests' scratch_home does.
    /// This is a cross-language contract — the script hardcodes the
    /// path, so drift here silently breaks idle-stop.
    #[test]
    fn stamp_path_follows_the_cache_dir_contract() {
        let tmp = std::env::temp_dir().join(format!("veles-stamp-{}", std::process::id()));
        let o = crate::config::DirOverride {
            config: tmp.join("config"),
            data: tmp.join("data"),
            cache: tmp.join("cache"),
        };
        let _g = crate::config::set_dirs(Some(o.clone()));
        let p = searxng_stamp_path();
        assert_eq!(p, tmp.join("cache/searxng-last-use"));
        touch_use_stamp();
        assert!(p.exists(), "touch must create parents and the file");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_html_to_text() {
        let html = "<html><head><title>Test Page</title></head><body><h1>Heading</h1><p>This is a paragraph.</p></body></html>";
        let text = html_to_text(html, 10000);
        assert!(text.contains("Test Page"));
        assert!(text.contains("This is a paragraph."));
    }

    #[test]
    fn test_remove_tag() {
        let html = "<p>before</p><script>alert('xss')</script><p>after</p>";
        let cleaned = remove_tag(html, "script");
        assert!(!cleaned.contains("alert"));
        assert!(cleaned.contains("before"));
        assert!(cleaned.contains("after"));
    }
}

//! lucida.to helper — resolves lossless FLAC stream URLs for Spotify tracks.
//!
//! This is the single source of truth for the lucida integration used by the
//! audio proxy. Keeping the token extraction here (instead of duplicating it)
//! avoids the drift that previously broke CSRF/expiry parsing in the proxy.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

/// The lucida.to API base URL.
pub const LUCIDA_BASE: &str = "https://lucida.to";

// ===== Cloudflare cookie passthrough =====
//
// lucida.to protects its endpoints with a Cloudflare "managed challenge".
// A plain HTTP client is answered with a 403 "Just a moment…" page, so the
// handshake dies before the first audio byte. Unlock: solve the challenge once
// in a real browser and give the proxy the resulting cookies. `kebabify cookie
// "..."` stores them; `open_stream` replays them on every lucida request. The
// stored UA must match the browser that solved the challenge — Cloudflare pins
// cf_clearance to User-Agent.

/// Public accessor so the browser import can tell the user where cookies went.
pub fn cookies_file_path() -> PathBuf {
    cookies_path()
}

fn cookies_path() -> PathBuf {
    if let Some(p) = cookies_override() {
        return p;
    }
    default_cookies_path()
}

/// Explicit file override, when set. Accepts the current name plus the
/// historical spellings so existing setups keep working.
fn cookies_override() -> Option<PathBuf> {
    // NOTE: "KEBACCFIY_COOKIES_PATH" is a historical typo (F/I swapped) that
    // shipped in early versions — keep reading it for compat, but prefer the
    // correctly-spelled variables.
    for var in [
        "KEBABIFY_COOKIES_PATH",
        "KEBACCIFY_COOKIES_PATH",
        "KEBACCFIY_COOKIES_PATH",
    ] {
        if let Some(p) = std::env::var_os(var) {
            return Some(PathBuf::from(p));
        }
    }
    None
}

fn default_cookies_path() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        PathBuf::from(appdata).join("Kebabify").join("cookies.txt")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".kebabify").join("cookies.txt")
    } else {
        PathBuf::from("kebabify_cookies.txt")
    }
}

/// Previous install location (pre-rename "Kebaccify"). Only used as a
/// read-fallback so existing users don't lose their stored session.
fn legacy_cookies_path() -> Option<PathBuf> {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        Some(PathBuf::from(appdata).join("Kebaccify").join("cookies.txt"))
    } else if let Some(home) = std::env::var_os("HOME") {
        Some(PathBuf::from(home).join(".kebaccify").join("cookies.txt"))
    } else {
        Some(PathBuf::from("kebaccify_cookies.txt"))
    }
}

/// Captured session: line 1 = the User-Agent used to solve the challenge,
/// line 2 = the Cookie header value to send (cf_clearance / __cf_bm / ...).
pub struct CloudflareSession {
    pub user_agent: String,
    pub cookie: String,
}

fn load_session() -> Option<CloudflareSession> {
    // An explicit override pins the exact file (no legacy fallback), so tests
    // and portable setups stay isolated from any machine-wide session.
    if let Some(pinned) = cookies_override() {
        return read_session(&pinned);
    }
    if let Some(s) = read_session(&cookies_path()) {
        return Some(s);
    }
    if let Some(legacy) = legacy_cookies_path() {
        if legacy != cookies_path() {
            return read_session(&legacy);
        }
    }
    None
}

fn read_session(path: &std::path::Path) -> Option<CloudflareSession> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let user_agent = lines.next()?.trim().to_string();
    let cookie = lines.next()?.trim().to_string();
    if user_agent.is_empty() || cookie.is_empty() {
        return None;
    }
    Some(CloudflareSession { user_agent, cookie })
}

/// Persists a browser session for the lucida Cloudflare challenge.
/// `cookie_header` is the raw `Cookie` value, e.g. "cf_clearance=…; __cf_bm=…".
pub fn save_cookies(user_agent: &str, cookie_header: &str) -> Result<()> {
    let path = cookies_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    std::fs::write(
        &path,
        format!("{}\n{}\n", user_agent.trim(), cookie_header.trim()),
    )
    .context("Failed to write cookies file")?;
    Ok(())
}

/// Applies the shared identity (captured UA + cookies, or the stock UA) to a
/// lucida request. Browser-shaped headers (Accept/Accept-Language/Referer)
/// ride along: with Cloudflare in front, a bare UA+Cookie request looks
/// scripted and draws challenges faster.
fn identify(
    req: reqwest::RequestBuilder,
    session: Option<&CloudflareSession>,
) -> reqwest::RequestBuilder {
    let ua = session.map(|s| s.user_agent.as_str()).unwrap_or(STOCK_UA);
    let req = req
        .header("User-Agent", ua)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Referer", "https://lucida.to/");
    match session {
        Some(s) => req.header("Cookie", &s.cookie),
        None => req,
    }
}

/// Whether a Cloudflare session is stored (new path or legacy fallback).
/// Used at startup to warn instead of letting every track fail silently.
pub fn has_session() -> bool {
    load_session().is_some()
}

/// Whether the stored session carries `cf_clearance` — the cookie that
/// proves the challenge was actually solved. A session without it (e.g. a
/// manual paste of only `__cf_bm`) still 403s on every track.
pub fn has_cf_clearance() -> bool {
    load_session()
        .map(|s| cookie_has_clearance(&s.cookie))
        .unwrap_or(false)
}

/// Checks a raw `Cookie` header value for the clearance cookie. Pure.
pub fn cookie_has_clearance(cookie_header: &str) -> bool {
    cookie_header.split(';').any(|pair| {
        let name = pair.split('=').next().unwrap_or("").trim();
        name.eq_ignore_ascii_case("cf_clearance")
    })
}

/// The lucida API endpoint for requesting a track stream.
pub const LUCIDA_API_LOAD: &str = "https://lucida.to/api/load?url=%2Fapi%2Ffetch%2Fstream%2Fv2";

/// Number of status polls before giving up.
const MAX_POLLS: u32 = 30;

/// Interval between status polls.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Timeout for the single-shot lucida calls (resolve page, API post).
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Timeout for each status poll.
const STATUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

// NOTE: the final download is intentionally *not* given a total timeout: it
// streams the whole FLAC body, so bounding the full request would cut
// long/slow tracks mid-flight (same reason as the shared client in
// `audio_proxy`, which only sets a connect timeout).

/// A resolved, ready-to-download FLAC stream from lucida.to.
pub struct LucidaStream {
    /// The HTTP response for the audio download, ready to be streamed.
    pub response: reqwest::Response,
}

/// Resolves a Spotify track URL through lucida.to and returns a streaming
/// response ready to be relayed downstream.
///
/// Steps: resolve page → extract CSRF token → request stream → poll status
/// until ready → open the download endpoint.
///
/// `range` is optional (`bytes=START-END`): when provided it is forwarded to
/// the download endpoint so seekable clients get a `206 Partial Content` with
/// `Content-Range`, which is what re-enables seeking in the Spotify player.
pub async fn open_stream(
    client: &reqwest::Client,
    spotify_url: &str,
    range: Option<&str>,
) -> Result<LucidaStream> {
    // Load the captured browser session (UA + Cloudflare cookies) once, so the
    // user can rotate cookies and re-open a stream without restarting.
    let session = load_session();

    // Step 1: resolve the track page to obtain the CSRF token.
    let resolve_url = format!("{}/{}", LUCIDA_BASE, urlencoding::encode(spotify_url));
    let resolve_resp = identify(client.get(&resolve_url), session.as_ref())
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| anyhow!("Failed to request lucida.to: {}", e))?;

    if !resolve_resp.status().is_success() {
        return Err(anyhow!(
            "lucida returned HTTP {} — service may be down or behind Cloudflare",
            resolve_resp.status()
        ));
    }

    let html = resolve_resp
        .text()
        .await
        .context("Failed to read lucida response")?;

    let token = extract_csrf_token(&html).ok_or_else(|| {
        anyhow!("Could not extract CSRF token from lucida page — page structure may have changed")
    })?;
    let token_expiry = extract_token_expiry(&html).unwrap_or(0u64);

    // Step 2: request the stream from the lucida API.
    let download_req = serde_json::json!({
        "account": { "id": "auto", "type": "country" },
        "compat": false,
        "downscale": "original",
        "handoff": true,
        "metadata": true,
        "private": false,
        "token": {
            "expiry": token_expiry,
            "primary": token,
            "secondary": null,
        },
        "upload": { "enabled": false },
        "url": spotify_url,
    });

    let dl_resp = identify(client.post(LUCIDA_API_LOAD), session.as_ref())
        .timeout(REQUEST_TIMEOUT)
        .json(&download_req)
        .send()
        .await
        .context("Failed to request stream from lucida API")?;

    if !dl_resp.status().is_success() {
        return Err(anyhow!(
            "lucida API returned HTTP {} — rate limited or blocked",
            dl_resp.status()
        ));
    }

    let dl: serde_json::Value = dl_resp
        .json()
        .await
        .context("Failed to parse lucida API response")?;

    let handoff = dl.get("handoff").and_then(|v| v.as_str()).unwrap_or("");
    let server = dl.get("server").and_then(|v| v.as_str()).unwrap_or("api");
    if handoff.is_empty() {
        return Err(anyhow!("lucida API returned no handoff ID"));
    }

    // Step 3: poll until the track is ready to stream.
    let status_url = format!("https://{}.lucida.to/api/fetch/request/{}", server, handoff);
    let download_url = format!("{}/download", status_url);

    let mut ready = false;
    for _ in 0..MAX_POLLS {
        match identify(client.get(&status_url), session.as_ref())
            .timeout(STATUS_TIMEOUT)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(status_json) = resp.json::<serde_json::Value>().await {
                    let status = status_json
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if status == "ready" || status == "done" {
                        ready = true;
                        break;
                    }
                }
            }
            _ => {}
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    if !ready {
        return Err(anyhow!("Track processing timed out on lucida"));
    }

    // Step 4: open the audio download endpoint.
    let mut audio_req = identify(client.get(&download_url), session.as_ref());
    if let Some(r) = range {
        audio_req = audio_req.header("Range", r);
    }
    let audio_resp = audio_req
        .send()
        .await
        .context("Failed to start FLAC download")?;

    if !audio_resp.status().is_success() {
        return Err(anyhow!(
            "FLAC download failed with HTTP {}",
            audio_resp.status()
        ));
    }

    Ok(LucidaStream {
        response: audio_resp,
    })
}

/// Stock User-Agent for requests that carry no captured browser session.
/// Shared with the Saavn fallback, which is sessionless by design.
pub const STOCK_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

/// Extracts the CSRF token from the lucida HTML page.
///
/// The page embeds its data in a script block with `"token":"value"` patterns.
/// Whitespace around the colon is tolerated (`"token" : "value"`), since
/// minifiers/pretty-printers vary.
fn extract_csrf_token(html: &str) -> Option<String> {
    for key in &["\"token\"", "\"csrf\""] {
        let mut search_from = 0;
        while let Some(rel) = html[search_from..].find(key) {
            let mut after = &html[search_from + rel + key.len()..];
            after = after.trim_start();
            if !after.starts_with(':') {
                search_from += rel + key.len();
                continue;
            }
            after = after[1..].trim_start();
            if let Some(inner) = after.strip_prefix('"') {
                let end = inner.find('"')?;
                return Some(inner[..end].to_string());
            }
            search_from += rel + key.len();
        }
    }
    None
}

/// Extracts the token expiry timestamp from the lucida HTML page.
fn extract_token_expiry(html: &str) -> Option<u64> {
    let key = "\"token_expiry\"";
    let mut search_from = 0;
    while let Some(rel) = html[search_from..].find(key) {
        let mut after = html[search_from + rel + key.len()..].trim_start();
        if !after.starts_with(':') {
            search_from += rel + key.len();
            continue;
        }
        after = after[1..].trim_start();
        let end = after.find(|c: char| !c.is_ascii_digit())?;
        if end == 0 {
            search_from += rel + key.len();
            continue;
        }
        return after[..end].parse::<u64>().ok();
    }
    None
}

/// Percent-encoding for the lucida resolve URL: the full Spotify track URL
/// rides as a single path segment, so everything outside RFC 3986 unreserved
/// (`A-Z a-z 0-9 - _ . ~`) is escaped. Backed by the audited
/// `percent-encoding` crate instead of a hand-rolled byte loop.
pub mod urlencoding {
    use percent_encoding::{AsciiSet, CONTROLS};

    /// Everything except unreserved chars must be escaped.
    const SEGMENT: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'!')
        .add(b'"')
        .add(b'#')
        .add(b'$')
        .add(b'%')
        .add(b'&')
        .add(b'\'')
        .add(b'(')
        .add(b')')
        .add(b'*')
        .add(b'+')
        .add(b',')
        .add(b'/')
        .add(b':')
        .add(b';')
        .add(b'<')
        .add(b'=')
        .add(b'>')
        .add(b'?')
        .add(b'@')
        .add(b'[')
        .add(b'\\')
        .add(b']')
        .add(b'^')
        .add(b'`')
        .add(b'{')
        .add(b'|')
        .add(b'}');

    pub fn encode(s: &str) -> String {
        percent_encoding::utf8_percent_encode(s, SEGMENT).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csrf_token_extracted() {
        let html = r#"<html><body><script>window.settings={"csrf":"aBcD1234","other":1};</script></body></html>"#;
        assert_eq!(extract_csrf_token(html).as_deref(), Some("aBcD1234"));
    }

    #[test]
    fn csrf_token_token_key() {
        let html = r#"<script>const x={"token":"xyz987","expiry":100};</script>"#;
        assert_eq!(extract_csrf_token(html).as_deref(), Some("xyz987"));
    }

    #[test]
    fn csrf_token_tolerates_whitespace() {
        let html = r#"<script>const x = { "token" : "spaced123" };</script>"#;
        assert_eq!(extract_csrf_token(html).as_deref(), Some("spaced123"));
    }

    #[test]
    fn csrf_token_missing() {
        assert_eq!(extract_csrf_token("<html></html>"), None);
    }

    #[test]
    fn token_expiry_extracted() {
        let html = r#"{"token_expiry":1780000000000,"ok":true}"#;
        assert_eq!(extract_token_expiry(html), Some(1_780_000_000_000));
    }

    #[test]
    fn token_expiry_tolerates_whitespace() {
        let html = r#"{ "token_expiry" : 1780000000000 }"#;
        assert_eq!(extract_token_expiry(html), Some(1_780_000_000_000));
    }

    #[test]
    fn token_expiry_default_when_absent() {
        assert_eq!(extract_token_expiry("<html></html>"), None);
    }

    #[test]
    fn url_encode_reserved_chars() {
        assert_eq!(urlencoding::encode("a b&c=/x"), "a%20b%26c%3D%2Fx");
        assert_eq!(urlencoding::encode("A-Z_0.9~"), "A-Z_0.9~");
        assert_eq!(urlencoding::encode("caf\u{e9}"), "caf%C3%A9");
        assert_eq!(
            urlencoding::encode("https://open.spotify.com/track/11dFghVXANMlKmJXsNCbNl"),
            "https%3A%2F%2Fopen.spotify.com%2Ftrack%2F11dFghVXANMlKmJXsNCbNl"
        );
    }

    #[test]
    fn clearance_cookie_detected() {
        assert!(cookie_has_clearance("cf_clearance=abc; __cf_bm=def"));
        assert!(cookie_has_clearance("__cf_bm=def; CF_CLEARANCE=abc"));
        assert!(cookie_has_clearance("  cf_clearance =abc"));
        assert!(!cookie_has_clearance("__cf_bm=def; __cfruid=xyz"));
        assert!(!cookie_has_clearance(""));
        assert!(!cookie_has_clearance("not_cf_clearance=abc"));
    }

    #[test]
    fn cookies_roundtrip_and_missing() {
        let dir = std::env::temp_dir().join(format!("kebabify_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cookies.txt");
        std::env::set_var("KEBABIFY_COOKIES_PATH", &path);

        assert!(load_session().is_none());

        save_cookies(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/126",
            "cf_clearance=abc; __cf_bm=def",
        )
        .unwrap();
        let s = load_session().expect("session should load");
        assert!(s.user_agent.contains("Chrome/126"));
        assert_eq!(s.cookie, "cf_clearance=abc; __cf_bm=def");

        std::fs::remove_file(&path).unwrap();
        std::env::remove_var("KEBABIFY_COOKIES_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

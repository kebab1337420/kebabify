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
    if let Some(override_path) = std::env::var_os("KEBABIFY_COOKIES_PATH")
        .or_else(|| std::env::var_os("KEBACCFIY_COOKIES_PATH")) {
        return PathBuf::from(override_path);
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        PathBuf::from(appdata).join("Kebaccify").join("cookies.txt")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".kebaccify").join("cookies.txt")
    } else {
        PathBuf::from("kebaccify_cookies.txt")
    }
}

/// Captured session: line 1 = the User-Agent used to solve the challenge,
/// line 2 = the Cookie header value to send (cf_clearance / __cf_bm / ...).
pub struct CloudflareSession {
    pub user_agent: String,
    pub cookie: String,
}

fn load_session() -> Option<CloudflareSession> {
    let text = std::fs::read_to_string(cookies_path()).ok()?;
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
/// lucida request.
fn identify(req: reqwest::RequestBuilder, session: Option<&CloudflareSession>) -> reqwest::RequestBuilder {
    let ua = session
        .map(|s| s.user_agent.as_str())
        .unwrap_or(USER_AGENT);
    let req = req.header("User-Agent", ua);
    match session {
        Some(s) => req.header("Cookie", &s.cookie),
        None => req,
    }
}

/// The lucida API endpoint for requesting a track stream.
pub const LUCIDA_API_LOAD: &str = "https://lucida.to/api/load?url=%2Fapi%2Ffetch%2Fstream%2Fv2";

/// Number of status polls before giving up.
const MAX_POLLS: u32 = 30;

/// Interval between status polls.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

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
        match identify(client.get(&status_url), session.as_ref()).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(status_json) = resp.json::<serde_json::Value>().await {
                    let status = status_json.get("status").and_then(|v| v.as_str()).unwrap_or("");
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

/// User agent used for all lucida requests.
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Extracts the CSRF token from the lucida HTML page.
///
/// The page embeds its data in a script block with `"token":"value"` patterns.
fn extract_csrf_token(html: &str) -> Option<String> {
    for keyword in &["\"token\":", "\"csrf\":" ] {
        if let Some(idx) = html.find(keyword) {
            let after = &html[idx + keyword.len()..];
            let after = after.trim_start();
            if let Some(inner) = after.strip_prefix('"') {
                let end = inner.find('"')?;
                return Some(inner[..end].to_string());
            }
        }
    }
    None
}

/// Extracts the token expiry timestamp from the lucida HTML page.
fn extract_token_expiry(html: &str) -> Option<u64> {
    if let Some(idx) = html.find("\"token_expiry\":") {
        let after = &html[idx + 15..];
        let after = after.trim_start();
        let end = after.find(|c: char| !c.is_ascii_digit())?;
        return after[..end].parse::<u64>().ok();
    }
    None
}

/// Lightweight URL encoder.
pub mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut result = String::with_capacity(s.len() * 3);
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    result.push(byte as char);
                }
                _ => {
                    result.push('%');
                    result.push_str(&format!("{:02X}", byte));
                }
            }
        }
        result
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
    fn csrf_token_missing() {
        assert_eq!(extract_csrf_token("<html></html>"), None);
    }

    #[test]
    fn token_expiry_extracted() {
        let html = r#"{"token_expiry":1780000000000,"ok":true}"#;
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
    }

    #[test]
    fn cookies_roundtrip_and_missing() {
        let dir = std::env::temp_dir().join(format!("kebabify_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cookies.txt");
        std::env::set_var("KEBACCFIY_COOKIES_PATH", &path);

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
        std::env::remove_var("KEBACCFIY_COOKIES_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
//! lucida.to helper — resolves lossless FLAC stream URLs for Spotify tracks.
//!
//! This is the single source of truth for the lucida integration used by the
//! audio proxy. Keeping the token extraction here (instead of duplicating it)
//! avoids the drift that previously broke CSRF/expiry parsing in the proxy.

use anyhow::{Context, Result, anyhow};
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
        log_source_once(&format!("cookies file {} (from env override)", p.display()));
        return p;
    }
    let p = default_cookies_path();
    log_source_once(&format!("cookies file {} (default location)", p.display()));
    p
}

/// One-shot stderr note naming which env/dir source won (observability for
/// hijacked-`APPDATA`/`PATH` debugging). Silent in tests after first call.
fn log_source_once(msg: &str) {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!("[kebabify] {}", msg);
    }
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

/// Cap on the cookies file read: only two short lines are ever used, so a
/// multi-megabyte file (hostile or accidental) must not be fully allocated
/// per track resolution.
const MAX_COOKIES_READ: usize = 16 * 1024;

/// Reads at most [`MAX_COOKIES_READ`] bytes as text, stopping at a character
/// boundary (never panics on a split UTF-8 sequence).
fn read_head(path: &std::path::Path, max: usize) -> Option<String> {
    use std::io::Read;
    // Bounded read: `fs::read` would allocate the whole file before the
    // truncation, so the cap it claims to enforce was not enforced at all.
    let mut bytes = Vec::with_capacity(max.min(64 * 1024));
    std::fs::File::open(path)
        .ok()?
        .take(max as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let len = bytes.len().min(max);
    match std::str::from_utf8(&bytes[..len]) {
        Ok(s) => Some(s.to_string()),
        // Cut mid-character: keep the longest valid prefix.
        Err(e) => std::str::from_utf8(&bytes[..e.valid_up_to()])
            .ok()
            .map(str::to_string),
    }
}

fn read_session(path: &std::path::Path) -> Option<CloudflareSession> {
    let text = read_head(path, MAX_COOKIES_READ)?;
    let mut lines = text.lines();
    // A UTF-8 BOM (Notepad saves one) would otherwise poison the User-Agent
    // with an invisible prefix and 403 every handshake.
    let user_agent = strip_bom(lines.next()?.trim()).trim().to_string();
    let cookie = lines.next()?.trim().to_string();
    if user_agent.is_empty() || cookie.is_empty() {
        return None;
    }
    Some(CloudflareSession { user_agent, cookie })
}

/// Strips a UTF-8 byte-order mark left by editors like Notepad.
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{FEFF}').unwrap_or(s)
}

/// Persists a browser session for the lucida Cloudflare challenge.
/// `cookie_header` is the raw `Cookie` value, e.g. "cf_clearance=…; __cf_bm=…".
/// Written atomically (tmp + rename): proxy tasks re-read the file on every
/// track, and a torn write would silently downgrade one track to no cookies.
pub fn save_cookies(user_agent: &str, cookie_header: &str) -> Result<()> {
    let path = cookies_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension("txt.tmp");
    std::fs::write(
        &tmp,
        format!(
            "{}\n{}\n",
            strip_bom(user_agent.trim()),
            strip_bom(cookie_header.trim())
        ),
    )
    .with_context(|| format!("Failed to write {}", tmp.display()))?;
    // Live Cloudflare session: owner-only on Unix (Windows inherits the
    // per-user %APPDATA% ACL, so nothing to do there).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .context("Failed to restrict cookies file permissions")?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("Failed to move {} into place", path.display()))?;
    // A legacy Kebaccify file would otherwise rot forever, never read again
    // now that the new path exists.
    if let Some(legacy) = legacy_cookies_path() {
        if legacy != path {
            let _ = std::fs::remove_file(&legacy);
        }
    }
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

/// Checks a raw `Cookie` header value for a usable clearance cookie: the
/// `cf_clearance` name must carry a non-empty value (`cf_clearance` alone or
/// `cf_clearance=` proves nothing and still 403s). Pure.
pub fn cookie_has_clearance(cookie_header: &str) -> bool {
    cookie_header.split(';').any(|pair| {
        let Some((name, value)) = pair.split_once('=') else {
            return false;
        };
        name.trim().eq_ignore_ascii_case("cf_clearance") && !value.trim().is_empty()
    })
}

/// The lucida API endpoint for requesting a track stream.
pub const LUCIDA_API_LOAD: &str = "https://lucida.to/api/load?url=%2Fapi%2Ffetch%2Fstream%2Fv2";

/// Number of status polls before giving up.
const MAX_POLLS: u32 = 30;

/// Consecutive transport failures (DNS/TLS/refused) that abort polling early
/// instead of burning all 30 polls when the network itself is down.
const MAX_TRANSPORT_ERRORS: u32 = 3;

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
#[derive(Debug)]
pub struct LucidaStream {
    response: reqwest::Response,
}

impl LucidaStream {
    /// Consumes the stream into the underlying HTTP response for piping.
    pub fn into_response(self) -> reqwest::Response {
        self.response
    }
}

/// Endpoint set for the lucida flow. `Default` is production; tests inject a
/// local mock. Injectable bases also leave the door open to mirrors.
#[derive(Clone, Debug)]
pub struct LucidaEndpoints {
    /// Resolve + load host, e.g. `https://lucida.to`.
    pub base: String,
    /// Full stream-request URL.
    pub api_load: String,
    /// Host for the poll/download `/api/load` route (== `base` in
    /// production; a separate field so tests can point status at a dead
    /// port while resolve/load hit the mock).
    pub status_base: String,
}

impl Default for LucidaEndpoints {
    fn default() -> Self {
        Self {
            base: LUCIDA_BASE.to_string(),
            api_load: LUCIDA_API_LOAD.to_string(),
            status_base: LUCIDA_BASE.to_string(),
        }
    }
}

/// Builds the poll + download URLs for a `handoff`/`name` pair.
///
/// The lucida client polls through the same `/api/load` route (no more
/// `{server}.lucida.to` subdomain):
/// `GET /api/load?url=<enc(/api/fetch/request/<handoff>)>&force=<name>`
/// and downloads from the sibling `…/download` path with `&redirect=true`.
/// `handoff` comes from the network so it is validated as a path segment —
/// anything else is rejected instead of being interpolated. `name` is echoed
/// raw like the site client does (the API matches on it verbatim). Pure for
/// tests.
fn poll_urls(status_base: &str, handoff: &str, name: &str) -> Result<(String, String)> {
    let handoff_ok = !handoff.is_empty()
        && handoff
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b));
    if !handoff_ok {
        return Err(anyhow!("lucida API returned a bad handoff ID"));
    }
    if name.is_empty() {
        return Err(anyhow!("lucida API returned no request name"));
    }
    let poll_inner = format!("/api/fetch/request/{}", handoff);
    let download_inner = format!("{}/download", poll_inner);
    let base = status_base.trim_end_matches('/');
    let poll_url = format!(
        "{}/api/load?url={}&force={}",
        base,
        urlencoding::encode(&poll_inner),
        name
    );
    let download_url = format!(
        "{}/api/load?url={}&force={}&redirect=true",
        base,
        urlencoding::encode(&download_inner),
        name
    );
    Ok((poll_url, download_url))
}

/// Resolves a Spotify track URL through lucida.to and returns a streaming
/// response ready to be relayed downstream.
///
/// Steps: resolve page → extract CSRF token → request stream → poll status
/// until ready → open the download endpoint.
///
/// `range`/`if_range` are optional (`bytes=START-END` + validator): when
/// provided they are forwarded to the download endpoint so seekable clients
/// get a `206 Partial Content` with `Content-Range`, which is what re-enables
/// seeking in the Spotify player.
pub async fn open_stream(
    client: &reqwest::Client,
    spotify_url: &str,
    range: Option<&str>,
    if_range: Option<&str>,
) -> Result<LucidaStream> {
    open_stream_with(
        client,
        spotify_url,
        range,
        if_range,
        &LucidaEndpoints::default(),
    )
    .await
}

/// Same as [`open_stream`] with injectable endpoints (tests, mirrors).
async fn open_stream_with(
    client: &reqwest::Client,
    spotify_url: &str,
    range: Option<&str>,
    if_range: Option<&str>,
    ep: &LucidaEndpoints,
) -> Result<LucidaStream> {
    // Load the captured browser session (UA + Cloudflare cookies) once, so the
    // user can rotate cookies and re-open a stream without restarting.
    let session = load_session();

    // Step 1: resolve the track through the page resolver (`/?url=`).
    // A resolvable track yields SvelteKit page-data carrying
    // `token:"…",tokenExpiry:<ms>`; an unavailable one yields `token:null`.
    let resolve_url = format!("{}?url={}", ep.base, urlencoding::encode(spotify_url));
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

    let (page_token, token_expiry) = extract_page_token(&html).ok_or_else(|| {
        anyhow!("Could not parse lucida page-data — page structure may have changed")
    })?;
    let page_token = page_token.ok_or_else(|| {
        anyhow!("lucida has no stream for this track (page token is null — not on its providers)")
    })?;
    // The site client sends `atob(atob(token))`: the page token is
    // base64-of-base64.
    let primary = double_b64_decode(&page_token).ok_or_else(|| {
        anyhow!("Could not decode the lucida page token (expected base64-of-base64)")
    })?;

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
            "primary": primary,
            "secondary": null,
        },
        "upload": { "enabled": false },
        "url": spotify_url,
    });

    let dl_resp = identify(client.post(&ep.api_load), session.as_ref())
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
    let name = dl.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if dl.get("success").and_then(|v| v.as_bool()) != Some(true)
        || handoff.is_empty()
        || name.is_empty()
    {
        let msg = dl
            .get("message")
            .or_else(|| dl.get("error"))
            .and_then(|v| v.as_str())
            .unwrap_or("no handoff");
        return Err(anyhow!("lucida refused the stream request: {}", msg));
    }
    let (status_url, download_url) = poll_urls(&ep.status_base, handoff, name)?;

    // Step 3: poll until the track is ready to stream.
    let mut ready = false;
    let mut transport_errors = 0u32;
    for _ in 0..MAX_POLLS {
        match identify(client.get(&status_url), session.as_ref())
            .timeout(STATUS_TIMEOUT)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                transport_errors = 0;
                if let Ok(status_json) = resp.json::<serde_json::Value>().await {
                    let ok = status_json
                        .get("success")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let status = status_json
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if status == "completed" || status == "ready" || status == "done" {
                        ready = true;
                        break;
                    }
                    if status == "error" || !ok {
                        let msg = status_json
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("processing failed");
                        return Err(anyhow!("lucida stream failed: {}", msg));
                    }
                }
            }
            Ok(_) => {
                // Server answered with an error status: alive but unhappy —
                // keep polling, the track may still resolve.
            }
            Err(_) => {
                // Transport failure (DNS/TLS/refused): the network itself is
                // down, not the track — fail fast instead of burning polls.
                transport_errors += 1;
                if transport_errors >= MAX_TRANSPORT_ERRORS {
                    return Err(anyhow!(
                        "lucida unreachable (network error {}× in a row)",
                        transport_errors
                    ));
                }
            }
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
    if let Some(v) = if_range {
        audio_req = audio_req.header("If-Range", v);
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
pub const STOCK_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

/// Extracts the page-data token + expiry from the lucida resolver page.
///
/// SvelteKit serializes page data with `devalue`, i.e. a JS object literal,
/// not JSON: a resolvable track carries `token:"<b64-of-b64>",tokenExpiry:<ms>`
/// (bare keys, quoted string value), while an unavailable track carries
/// `token:null,tokenExpiry:null`.
///
/// Returns `None` when no `tokenExpiry` field exists at all (page structure
/// changed); `(None, 0)` when the page resolves but the track is unavailable.
/// The lookup scans leftwards from the *last* `tokenExpiry` and requires the
/// `token:` key to start at a key boundary — the Cloudflare beacon's
/// `"token":"…"` (quoted key) and substrings inside other identifiers are
/// skipped.
fn extract_page_token(html: &str) -> Option<(Option<String>, u64)> {
    const EXPIRY_KEY: &str = "tokenExpiry:";
    let pos = html.rfind(EXPIRY_KEY)?;
    let mut expiry: u64 = 0;
    for c in html[pos + EXPIRY_KEY.len()..].chars() {
        match c {
            '0'..='9' => {
                expiry = expiry
                    .saturating_mul(10)
                    .saturating_add(c as u64 - '0' as u64);
            }
            _ => break,
        }
    }

    let mut token: Option<String> = None;
    let mut rest = &html[..pos];
    while let Some(idx) = rest.rfind("token:") {
        let before = rest[..idx].chars().last().unwrap_or(',');
        if !before.is_ascii_alphanumeric() && before != '_' && before != '"' {
            let after = rest[idx + "token:".len()..].trim_start();
            if let Some(quoted) = after.strip_prefix('"') {
                if let Some(end) = quoted.find('"') {
                    token = Some(quoted[..end].to_string());
                    break;
                }
            } else if after.starts_with("null") {
                break;
            }
        }
        rest = &rest[..idx];
    }
    Some((token, expiry))
}

/// Decodes the page token. The lucida client sends `atob(atob(token))`: the
/// page value is base64-of-base64 (standard alphabet; URL-safe tolerated).
fn double_b64_decode(s: &str) -> Option<String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE};

    let layer1 = STANDARD
        .decode(s.trim())
        .ok()
        .or_else(|| URL_SAFE.decode(s.trim()).ok())?;
    let layer1 = String::from_utf8(layer1).ok()?;
    STANDARD
        .decode(layer1.trim())
        .ok()
        .or_else(|| URL_SAFE.decode(layer1.trim()).ok())
        .and_then(|v| String::from_utf8(v).ok())
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
    fn poll_urls_validated() {
        let (p, d) = poll_urls("https://lucida.to", "h1-abc_XY.9~", "track-name").unwrap();
        assert_eq!(
            p,
            "https://lucida.to/api/load?url=%2Fapi%2Ffetch%2Frequest%2Fh1-abc_XY.9~&force=track-name"
        );
        assert_eq!(
            d,
            "https://lucida.to/api/load?url=%2Fapi%2Ffetch%2Frequest%2Fh1-abc_XY.9~%2Fdownload&force=track-name&redirect=true"
        );
        assert!(poll_urls("https://lucida.to", "", "n").is_err());
        assert!(poll_urls("https://lucida.to", "a/b", "n").is_err());
        assert!(poll_urls("https://lucida.to", "../x", "n").is_err());
        assert!(poll_urls("https://lucida.to", "a b", "n").is_err());
        assert!(poll_urls("https://lucida.to", "a?b", "n").is_err());
        assert!(poll_urls("https://lucida.to", "h1", "").is_err());
    }

    #[test]
    fn page_token_and_expiry_extracted() {
        let html = r#"<script>var data=[{...,token:"QUJDRDEyMw==",tokenExpiry:1820000000000},"uses"];</script>"#;
        let (tok, exp) = extract_page_token(html).unwrap();
        assert_eq!(tok.as_deref(), Some("QUJDRDEyMw=="));
        assert_eq!(exp, 1_820_000_000_000);
    }

    #[test]
    fn page_token_null_when_track_unavailable() {
        let html = r#"<script>var data=[{...,token:null,tokenExpiry:null},"uses"];</script>"#;
        let (tok, exp) = extract_page_token(html).unwrap();
        assert_eq!(tok, None);
        assert_eq!(exp, 0);
    }

    #[test]
    fn page_token_ignores_quoted_beacon_token() {
        // The Cloudflare beacon uses a *quoted* key ("token":) — the page-data
        // token uses a bare key (token:) and sits right before tokenExpiry.
        let html = concat!(
            r#"<script src="beacon.js" data-cf-beacon='{"token":"beecon123"}'></script>"#,
            r#"<script>var data=[{info:{},token:"cGFnZXZvbyJiYXI=","#,
            r#"tokenExpiry:77},"uses"];</script>"#,
        );
        let (tok, exp) = extract_page_token(html).unwrap();
        assert_eq!(tok.as_deref(), Some("cGFnZXZvbyJiYXI="));
        assert_eq!(exp, 77);
    }

    #[test]
    fn page_token_missing_when_structure_changed() {
        assert_eq!(extract_page_token("<html>no data</html>"), None);
    }

    #[test]
    fn double_base64_roundtrip() {
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD;
        let once = STANDARD.encode("tok123-primary");
        let twice = STANDARD.encode(&once);
        assert_eq!(double_b64_decode(&twice).as_deref(), Some("tok123-primary"));
        assert_eq!(double_b64_decode("!!!not-base64!!!"), None);
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
        // Name without value (or empty value) proves nothing.
        assert!(!cookie_has_clearance("cf_clearance"));
        assert!(!cookie_has_clearance("cf_clearance="));
        assert!(!cookie_has_clearance("cf_clearance=; __cf_bm=x"));
    }

    #[test]
    fn cookies_with_bom_and_oversize_load() {
        let dir = std::env::temp_dir().join(format!("kebabify_bom_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // BOM + oversized junk after two valid lines: UA clean, tail cut.
        let path = dir.join("cookies.txt");
        let mut content = String::from("\u{FEFF}UA-1\ncf_clearance=abc\n");
        content.push_str(&"x".repeat(100_000));
        std::fs::write(&path, &content).unwrap();
        let s = read_session(&path).expect("session should load");
        assert_eq!(s.user_agent, "UA-1");
        assert_eq!(s.cookie, "cf_clearance=abc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Serializes tests that touch the process-global env block (parallel
    /// test threads share one env; without this a concurrent
    /// `load_session` reader flakes).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn cookies_roundtrip_and_missing() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("kebabify_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cookies.txt");
        // Edition 2024: set_var is unsafe (process-global mutation).
        unsafe { std::env::set_var("KEBABIFY_COOKIES_PATH", &path) };

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
        unsafe { std::env::remove_var("KEBABIFY_COOKIES_PATH") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    use crate::mock::{MockServer, Route};

    const MOCK_AUDIO: &[u8] = b"fLaC-audio-bytes-0123456789";

    fn mock_endpoints(mock: &MockServer) -> LucidaEndpoints {
        LucidaEndpoints {
            base: mock.base_url.clone(),
            api_load: format!("{}/api/load", mock.base_url),
            status_base: mock.base_url.clone(),
        }
    }

    /// Page token for the mock resolver: twice-base64 of "tok123-primary",
    /// like the real devalue page-data (`token:"…",tokenExpiry:…`).
    fn page_token_twice() -> String {
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD;
        STANDARD.encode(STANDARD.encode("tok123-primary"))
    }

    fn resolve_html() -> Vec<u8> {
        format!(
            "<html><script>var data=[{{info:{{}},token:\"{}\",tokenExpiry:999}},\"uses\"];</script></html>",
            page_token_twice()
        )
        .into_bytes()
    }

    /// Full flow against a canned upstream: resolve → page token → load →
    /// processing → completed → download, plain and ranged.
    #[tokio::test]
    async fn full_flow_polls_then_streams_with_range() {
        let mock = MockServer::start(vec![
            // Download first: its URL also contains "request".
            Route::new("/api/load", vec![(200, MOCK_AUDIO.to_vec())])
                .containing("download")
                .ranged(),
            Route::new(
                "/api/load",
                vec![
                    (200, br#"{"success":true,"status":"processing"}"#.to_vec()),
                    (200, br#"{"success":true,"status":"completed"}"#.to_vec()),
                ],
            )
            .containing("request"),
            Route::new(
                "/api/load",
                vec![(
                    200,
                    br#"{"success":true,"handoff":"h1","name":"n1"}"#.to_vec(),
                )],
            ),
            Route::catch_all(200, resolve_html()),
        ])
        .await;
        let ep = mock_endpoints(&mock);
        let client = reqwest::Client::new();

        let s = open_stream_with(
            &client,
            "https://open.spotify.com/track/abc",
            None,
            None,
            &ep,
        )
        .await
        .unwrap();
        let r = s.into_response();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap().as_ref(), MOCK_AUDIO);

        let s = open_stream_with(
            &client,
            "https://open.spotify.com/track/abc",
            Some("bytes=0-3"),
            None,
            &ep,
        )
        .await
        .unwrap();
        let r = s.into_response();
        assert_eq!(r.status(), 206);
        assert_eq!(r.bytes().await.unwrap().as_ref(), &MOCK_AUDIO[..4]);
    }

    /// Status endpoint dead (connection refused): fail fast on transport
    /// errors instead of burning all 30 polls.
    #[tokio::test]
    async fn status_transport_errors_fail_fast() {
        let mock = MockServer::start(vec![
            Route::new(
                "/api/load",
                vec![(
                    200,
                    br#"{"success":true,"handoff":"h1","name":"n1"}"#.to_vec(),
                )],
            ),
            Route::catch_all(200, resolve_html()),
        ])
        .await;
        // Reserve-then-drop races a port thief (localhost-only, microseconds):
        // retry with a fresh port instead of flaking.
        let client = reqwest::Client::new();
        let mut last = String::new();
        for _ in 0..3 {
            let dead = MockServer::reserve_port().await;
            let ep = LucidaEndpoints {
                base: mock.base_url.clone(),
                api_load: format!("{}/api/load", mock.base_url),
                status_base: format!("http://127.0.0.1:{}/status/{{handoff}}", dead),
            };
            let err = open_stream_with(
                &client,
                "https://open.spotify.com/track/abc",
                None,
                None,
                &ep,
            )
            .await
            .unwrap_err();
            last = format!("{:#}", err);
            if last.contains("unreachable") {
                return;
            }
        }
        panic!("unexpected error after retries: {}", last);
    }
}

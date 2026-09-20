//! Audio proxy server — intercepts Spotify audio requests and redirects to FLAC.
//!
//! Runs a local HTTP server on 127.0.0.1:18900. When Spotify sends an audio
//! request (to audio-spclient.wg.spotify.com), the JS layer redirects it to
//! this proxy. The proxy then:
//! 1. Extracts the track ID from the original Spotify URL
//! 2. Requests the FLAC version from lucida.to via [`crate::lucida`]
//! 3. Streams the FLAC data back to Spotify's player

use anyhow::{anyhow, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

/// Host the proxy binds to. Loopback only — never expose this on a network.
pub const PROXY_HOST: &str = "127.0.0.1";

/// Port the proxy listens on. Keep in sync with the JS extension constant.
pub const PROXY_PORT: u16 = 18900;

/// How long to wait for the HTTP request line before giving up.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of simultaneous client connections.
const MAX_CONCURRENT: usize = 64;

pub struct AudioProxy {
    port: u16,
    /// Currently playing track ID (for the /health endpoint).
    current_track: Arc<Mutex<Option<String>>>,
    /// Signals the accept loop to terminate (graceful shutdown).
    shutdown: Arc<Notify>,
    /// Latched shutdown flag — closes the Notify race window.
    shutting_down: Arc<AtomicBool>,
    /// Reused HTTP client — one TLS pool, not one per connection.
    client: reqwest::Client,
    /// Bounds concurrent connections to avoid a trivial local DoS.
    semaphore: Arc<tokio::sync::Semaphore>,
}

impl AudioProxy {
    pub fn new(port: u16) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("kebabify/0.1 (Spotify FLAC proxy)")
            .connect_timeout(Duration::from_secs(10))
            // NOTE: no total .timeout() here — it would bound the full FLAC
            // body as well and cut long/slow streams mid-flight.
            .build()
            .expect("failed to build HTTP client");

        Self {
            port,
            current_track: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(Notify::new()),
            shutting_down: Arc::new(AtomicBool::new(false)),
            client,
            semaphore: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)),
        }
    }

    /// Starts the proxy server. Runs until a `/shutdown` request arrives
    /// or Ctrl+C is pressed (graceful stop for foreground runs).
    pub async fn start(&self) -> Result<()> {
        let listener = tokio::net::TcpListener::bind((PROXY_HOST, self.port))
            .await
            .context("Failed to bind audio proxy. Port may be in use.")?;

        println!(
            "[kebabify] Audio proxy listening on {}:{}",
            PROXY_HOST, self.port
        );

        loop {
            // Check the latched flag before each select: if a /shutdown arrived
            // in the window where no waiter was registered, the Notify alone
            // would be missed and the proxy would need a second /shutdown.
            if self.shutting_down.load(Ordering::Relaxed) {
                break;
            }

            let (socket, _) = match tokio::select! {
                _ = self.shutdown.notified() => break,
                _ = tokio::signal::ctrl_c() => {
                    self.shutting_down.store(true, Ordering::Relaxed);
                    break;
                }
                accepted = listener.accept() => accepted,
            } {
                Ok(conn) => conn,
                Err(e) => {
                    // Transient accept errors must not kill the whole proxy.
                    eprintln!("[kebabify] Proxy accept error: {}", e);
                    continue;
                }
            };

            let current_track = self.current_track.clone();
            let shutting_down = self.shutting_down.clone();
            let shutdown = self.shutdown.clone();
            let client = self.client.clone();
            let semaphore = self.semaphore.clone();

            tokio::spawn(async move {
                // Holds the permit for the whole connection. Fails only if the
                // semaphore was closed during shutdown — then drop the conn.
                let Ok(_permit) = semaphore.acquire().await else {
                    return;
                };
                if let Err(e) =
                    handle_client(socket, current_track, shutting_down, shutdown, client).await
                {
                    eprintln!("[kebabify] Proxy error: {}", e);
                }
            });
        }

        println!("[kebabify] Audio proxy stopped");
        Ok(())
    }

    /// Sends a shutdown request to a running proxy instance.
    pub async fn request_shutdown() -> Result<()> {
        let url = format!("http://{}:{}/shutdown", PROXY_HOST, PROXY_PORT);
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .timeout(Duration::from_secs(3))
            .header("Origin", format!("http://{}:{}", PROXY_HOST, PROXY_PORT))
            .send()
            .await
            .context("Failed to contact audio proxy for shutdown")?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "Proxy shutdown request returned HTTP {}",
                resp.status()
            ));
        }
        Ok(())
    }

    /// Returns `true` if a proxy instance is currently responding on the port.
    pub async fn is_running() -> bool {
        let url = format!("http://{}:{}/health", PROXY_HOST, PROXY_PORT);
        let client = reqwest::Client::new();
        match client
            .get(&url)
            .timeout(Duration::from_millis(400))
            .send()
            .await
        {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }
}

/// Handles a single HTTP request to the proxy.
async fn handle_client(
    stream: tokio::net::TcpStream,
    current_track: Arc<Mutex<Option<String>>>,
    shutting_down: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
    client: reqwest::Client,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);

    // Read the request head (request line + headers) with a timeout — a client
    // that connects and sends nothing must not hold a task open forever (local
    // DoS). Loop until the blank line: a request may arrive split over several
    // TCP segments, and a single read() would truncate Origin/Host.
    let mut header_block = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = tokio::time::timeout(READ_TIMEOUT, reader.read_line(&mut line))
            .await
            .context("Timed out waiting for HTTP request")?
            .context("Failed to read HTTP request")?;
        if n == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        header_block.push_str(&line);
        if header_block.len() > 8192 {
            return Err(anyhow!("Request headers too large"));
        }
    }

    if header_block.is_empty() {
        return Ok(());
    }

    // Parse the request line: "GET /path HTTP/1.1"
    let parts: Vec<&str> = header_block
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    if parts.len() < 2 {
        return Err(anyhow!("Malformed HTTP request"));
    }

    let path = parts[1];
    let origin = request_origin(&header_block);

    // ===== Admin endpoints =====
    if path == "/health" {
        // Only answer to requests from allowed origins (or non-browser clients).
        if let Some(o) = origin.as_deref() {
            if !is_allowed_origin(o) {
                return Err(anyhow!("Forbidden origin for /health: {}", o));
            }
        }

        let track = current_track.lock().await.clone();
        // Built with serde_json rather than format!: track IDs are validated
        // alphanumerics today, but manual quoting would silently break on the
        // first value that ever needs escaping.
        let body = serde_json::json!({"status": "ok", "track": track, "flac": true}).to_string();

        let mut resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if let Some(o) = origin {
            resp.push_str(&format!("Access-Control-Allow-Origin: {}\r\n", o));
        }
        resp.push_str("\r\n");
        resp.push_str(&body);

        write_half.write_all(resp.as_bytes()).await?;
        write_half.flush().await?;
        return Ok(());
    }

    if path == "/shutdown" {
        // Require a valid Origin. Media requests (<audio> embeds, etc.) don't
        // send an Origin header, so a hostile web page could otherwise kill
        // the proxy with <audio src="http://127.0.0.1:18900/shutdown">.
        match origin.as_deref() {
            Some(o) if is_shutdown_origin(o) => {}
            Some(o) => {
                write_forbidden(&mut write_half).await?;
                return Err(anyhow!("Forbidden origin for /shutdown: {}", o));
            }
            None => {
                write_forbidden(&mut write_half).await?;
                return Err(anyhow!("Refusing shutdown without Origin header"));
            }
        }

        // Set the latched flag BEFORE waking the accept loop: even if the
        // notifier is missed, the loop-top check breaks on the next iteration.
        shutting_down.store(true, Ordering::Relaxed);
        let body = serde_json::json!({"status": "stopping"}).to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        write_half.write_all(resp.as_bytes()).await?;
        write_half.flush().await?;
        shutdown.notify_waiters();
        return Ok(());
    }

    // ===== Audio stream request =====
    // The JS sends us ?track=TRACKID, or the original Spotify URL.
    let track_id: Option<String> = if let Some(qpos) = path.find('?') {
        let query_str = &path[qpos + 1..];
        let mut found_track_id = None;
        for param in query_str.split('&') {
            let kv: Vec<&str> = param.splitn(2, '=').collect();
            if kv.len() == 2 && kv[0] == "track" {
                found_track_id = Some(kv[1].to_string());
                break;
            }
        }
        found_track_id
    } else if path.starts_with("http") || path.contains("spotify") {
        extract_track_id(path)
    } else {
        let host = header_block
            .lines()
            .find(|l| l.to_lowercase().starts_with("host:"))
            .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
            .unwrap_or_else(|| "audio-spclient.wg.spotify.com".to_string());
        let full_url = format!("https://{}{}", host, path);
        println!("[kebabify] Proxy request: GET {}", full_url);
        extract_track_id(&full_url)
    };

    let Some(tid) = track_id else {
        return Err(anyhow!("Could not extract a track ID from request"));
    };

    *current_track.lock().await = Some(tid.clone());
    println!("[kebabify] Track ID: {} — fetching FLAC via lucida.to", tid);

    let spotify_url = format!("https://open.spotify.com/track/{}", tid);

    // Optional Range header — forwarded so the player can seek. When lucida's
    // download endpoint honors it (206 + Content-Range) the player gets real
    // byte-range seeking; when it doesn't (200 full body) behavior is unchanged.
    let range_header = request_range(&header_block);

    // Resolve and stream from lucida.to inside a block: whatever happens, the
    // indicator is reset afterwards so /health never reports a ghost track.
    let mut response_started = false;
    let result: Result<()> = async {
        let lucida_stream =
            crate::lucida::open_stream(&client, &spotify_url, range_header.as_deref()).await?;
        let mut audio_resp = lucida_stream.response;

        let content_type = audio_resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("audio/flac")
            .to_string();

        if !content_type.contains("flac") && !content_type.contains("audio") {
            eprintln!(
                "[kebabify] WARNING: content-type is not FLAC: {}",
                content_type
            );
        }

        // Send valid HTTP response headers. The terminating \r\n\r\n is
        // required before the body — without it Chromium treats the start of
        // the FLAC stream as part of the header block and rejects the response.
        //
        // Relay the upstream status + byte-range headers so seekable responses
        // (206 Partial Content) behave transparently. Transfer-Encoding is
        // deliberately NOT relayed: reqwest de-chunks the stream, and forwarding
        // the header without the chunk framing would hang the client.
        let status = audio_resp.status();
        let status_line = format!(
            "HTTP/1.1 {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("OK")
        );
        let mut response_header = format!(
            "{}\r\nContent-Type: {}\r\nConnection: close\r\nCache-Control: no-cache",
            status_line, content_type
        );
        for name in ["content-range", "accept-ranges", "content-length"] {
            if let Some(v) = audio_resp.headers().get(name) {
                if let Ok(s) = v.to_str() {
                    response_header.push_str(&format!("\r\n{}: {}", name, s));
                }
            }
        }
        response_header.push_str("\r\n\r\n");

        response_started = true;
        write_half
            .write_all(response_header.as_bytes())
            .await
            .map_err(|e| anyhow!("Failed to write response headers: {}", e))?;

        write_half
            .flush()
            .await
            .map_err(|e| anyhow!("Failed to flush headers: {}", e))?;

        // Stream the body.
        let mut total_bytes: u64 = 0;
        while let Some(chunk) = audio_resp.chunk().await? {
            write_half.write_all(&chunk).await?;
            total_bytes += chunk.len() as u64;
        }

        write_half.flush().await?;
        println!(
            "[kebabify] Served FLAC for track {} ({} bytes)",
            tid, total_bytes
        );

        Ok(())
    }
    .await;

    // Track finished (or failed) — drop the indicator so /health reports null.
    *current_track.lock().await = None;

    if let Err(e) = &result {
        eprintln!("[kebabify] Stream for track {} failed: {:#}", tid, e);
        write_stream_error(&mut write_half, response_started).await?;
    }

    result
}

async fn write_stream_error<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    response_started: bool,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    if response_started {
        return Ok(());
    }
    writer
        .write_all(
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: 18\r\nConnection: close\r\n\r\nProxy stream error",
        )
        .await?;
    writer.flush().await?;
    Ok(())
}

/// Writes a minimal `403 Forbidden` JSON response, then flushes.
async fn write_forbidden<W: tokio::io::AsyncWrite + Unpin>(write_half: &mut W) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let body = serde_json::json!({"status": "forbidden"}).to_string();
    let resp = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    write_half.write_all(resp.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

/// Returns the value of the `Origin` header, if present.
fn request_origin(header_block: &str) -> Option<String> {
    header_block
        .lines()
        .find(|l| l.to_lowercase().starts_with("origin:"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Returns the value of the `Range` header, if present.
fn request_range(header_block: &str) -> Option<String> {
    header_block
        .lines()
        .find(|l| l.to_lowercase().starts_with("range:"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Origins allowed to use the admin endpoints. Spotify desktop serves the
/// extension from `app://spotify` or the open.spotify.com origin. Loopback
/// traffic must come from our own proxy port — anything else on 127.0.0.1
/// must not be able to shut the proxy down.
fn is_allowed_origin(origin: &str) -> bool {
    origin == "https://open.spotify.com"
        || origin == "https://xpui.app.spotify.com"
        || origin == "app://spotify"
        || origin.starts_with("spotify://")
        || origin == format!("http://{}:{}", PROXY_HOST, PROXY_PORT)
}

/// Only the loopback origin the binary itself uses may shut the proxy down.
/// A hostile web page could forge any host in its Origin header; what it
/// cannot do is make Spotify send 127.0.0.1:18900 — that origin only comes
/// from our own /shutdown caller (main.rs).
fn is_shutdown_origin(origin: &str) -> bool {
    origin == format!("http://{}:{}", PROXY_HOST, PROXY_PORT)
}

/// Extracts a Spotify track ID from a URL.
///
/// Only returns IDs that look like a real Spotify track ID (exactly 22
/// base62 chars). Shorter fragments (e.g. `?id=abc`) are ignored so callers
/// never build a bogus `open.spotify.com/track/abc` URL downstream.
fn extract_track_id(url: &str) -> Option<String> {
    // Structured parsing first (single parse, not one per lookup).
    if let Ok(parsed) = url::Url::parse(url) {
        let path = parsed.path();
        for marker in ["/tracks/", "/track/"] {
            if let Some(idx) = path.find(marker) {
                let rest = &path[idx + marker.len()..];
                let end = rest.find('/').unwrap_or(rest.len());
                if is_track_id(&rest[..end]) {
                    return Some(rest[..end].to_string());
                }
            }
        }

        if let Some(id) = parsed
            .query_pairs()
            .find(|(k, _)| k == "track_id" || k == "id" || k == "cid")
        {
            if is_track_id(&id.1) {
                return Some(id.1.to_string());
            }
        }
    }

    // Manual fallback — look for track IDs near known keys.
    // ("spotify:track:" is matched whole so "soundtrack:…" can't hit it.)
    for key in &[
        "spotify:track:",
        "track/",
        "tracks/",
        "track=",
        "id=",
        "track_id=",
    ] {
        let mut search_from = 0;
        while let Some(rel) = url[search_from..].find(key) {
            let start = search_from + rel + key.len();
            let rest = &url[start..];
            let mut id_end = 0;
            for (i, c) in rest.char_indices() {
                if c.is_ascii_alphanumeric() {
                    id_end = i + 1;
                } else {
                    break;
                }
            }
            // Boundary check: a 32-char hex UUID in /track/{uuid} must not
            // count — only an exact 22-char base62 token does.
            let next_is_alnum = rest[id_end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric());
            if id_end == 22 && !next_is_alnum && is_track_id(&rest[..22]) {
                return Some(rest[..22].to_string());
            }
            search_from = start + id_end.max(1);
        }
    }

    None
}

fn is_track_id(s: &str) -> bool {
    s.len() == 22 && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACK_ID: &str = "4uLU6hMCjMI75M1A2tKUQC";

    #[tokio::test]
    async fn stream_error_before_response_sends_bad_gateway() {
        let mut output = Vec::new();
        write_stream_error(&mut output, false).await.unwrap();
        assert_eq!(
            output,
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: 18\r\nConnection: close\r\n\r\nProxy stream error"
        );
    }

    #[tokio::test]
    async fn stream_error_after_response_does_not_append_bytes() {
        for prefix in [
            b"HTTP/1.1 200".as_slice(),
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nfLaC".as_slice(),
        ] {
            let mut output = prefix.to_vec();
            write_stream_error(&mut output, true).await.unwrap();
            assert_eq!(output, prefix);
        }
    }

    #[test]
    fn track_id_from_tracks_path() {
        assert_eq!(
            extract_track_id(&format!(
                "https://audio-spclient.wg.spotify.com/tracks/{}",
                TRACK_ID
            )),
            Some(TRACK_ID.to_string())
        );
    }

    #[test]
    fn track_id_from_track_query() {
        assert_eq!(
            extract_track_id(&format!("?track={}", TRACK_ID)),
            Some(TRACK_ID.to_string())
        );
        assert_eq!(
            extract_track_id(&format!("https://open.spotify.com/track?id={}", TRACK_ID)),
            Some(TRACK_ID.to_string())
        );
    }

    #[test]
    fn track_id_from_track_path() {
        assert_eq!(
            extract_track_id(&format!("https://open.spotify.com/track/{}", TRACK_ID)),
            Some(TRACK_ID.to_string())
        );
    }

    #[test]
    fn track_id_absent() {
        assert_eq!(extract_track_id("https://example.com/"), None);
    }

    #[test]
    fn track_id_rejects_short_fragments() {
        assert_eq!(
            extract_track_id("https://open.spotify.com/track?id=abc"),
            None
        );
        assert_eq!(extract_track_id("?track=abc"), None);
    }

    #[test]
    fn track_id_rejects_uuid() {
        // 32-char hex file UUIDs ride in /track/{uuid} URLs — not track IDs.
        assert_eq!(
            extract_track_id("https://audio-spotify.com/track/1234567890abcdef1234567890abcdef"),
            None
        );
    }

    #[test]
    fn track_id_from_spotify_uri() {
        assert_eq!(
            extract_track_id(&format!("spotify:track:{}", TRACK_ID)),
            Some(TRACK_ID.to_string())
        );
    }

    #[test]
    fn allowed_origins_are_strict() {
        assert!(is_allowed_origin("https://open.spotify.com"));
        assert!(is_allowed_origin("https://xpui.app.spotify.com"));
        assert!(is_allowed_origin("app://spotify"));
        assert!(is_allowed_origin("spotify://user"));
        assert!(is_allowed_origin(&format!(
            "http://{}:{}",
            PROXY_HOST, PROXY_PORT
        )));
        assert!(!is_allowed_origin("http://127.0.0.1:9999"));
        assert!(!is_allowed_origin("http://192.168.1.10:18900"));
        assert!(!is_allowed_origin("https://evil.example"));
    }

    #[test]
    fn shutdown_origin_is_loopback_only() {
        let own = format!("http://{}:{}", PROXY_HOST, PROXY_PORT);
        assert!(is_shutdown_origin(&own));
        assert!(!is_shutdown_origin("https://open.spotify.com"));
        assert!(!is_shutdown_origin("https://xpui.app.spotify.com"));
        assert!(!is_shutdown_origin("app://spotify"));
        assert!(!is_shutdown_origin("http://192.168.1.10:18900"));
        assert!(!is_shutdown_origin("http://127.0.0.1:12345"));
    }

    #[test]
    fn header_parsers() {
        let block = "GET /health HTTP/1.1\r\nHost: 127.0.0.1:18900\r\nOrigin: https://open.spotify.com\r\nRange: bytes=0-1023\r\n\r\n";
        assert_eq!(
            request_origin(block).as_deref(),
            Some("https://open.spotify.com")
        );
        assert_eq!(request_range(block).as_deref(), Some("bytes=0-1023"));
        assert_eq!(request_origin("GET / HTTP/1.1\r\n\r\n"), None);
        assert_eq!(request_range("GET / HTTP/1.1\r\n\r\n"), None);
    }
}

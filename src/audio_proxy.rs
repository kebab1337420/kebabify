//! Audio proxy server — intercepts Spotify audio requests and redirects to FLAC.
//!
//! Runs a local HTTP server on 127.0.0.1:18900. When Spotify sends an audio
//! request (to audio-spclient.wg.spotify.com), the JS layer redirects it to
//! this proxy. The proxy then:
//! 1. Extracts the track ID from the original Spotify URL
//! 2. Resolves FLAC via the source chain: Soulseek P2P → lucida.to → Saavn
//! 3. Streams the audio back to Spotify's player (relayed HTTP, or sliced
//!    from the local Soulseek cache with Range support)

use anyhow::{Context, Result, anyhow};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

/// Host the proxy binds to. Loopback only — never expose this on a network.
pub const PROXY_HOST: &str = "127.0.0.1";

/// Port the proxy listens on. Keep in sync with the JS extension constant.
pub const PROXY_PORT: u16 = 18900;

/// The only Origin allowed to drive `/shutdown` — and the loopback identity
/// echoed back for CORS on admin endpoints. A const (not `format!`) so the
/// per-request checks allocate nothing.
pub const SHUTDOWN_ORIGIN: &str = "http://127.0.0.1:18900";

/// How long to wait for the HTTP request line before giving up.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Total budget for the whole request-head read: per-line timeouts alone let
/// a slow sender hold a connection slot nearly forever within the 8K cap.
const HEADER_TOTAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on the request head, enforced while reading (not after): a line
/// without CRLF must not be allowed to allocate past this.
const MAX_HEADER_BYTES: usize = 8192;

/// Maximum number of simultaneous client connections. Admission happens at
/// accept time, before a task is spawned: the audio semaphore alone let a
/// flood of half-open connections occupy every task and socket.
const MAX_CONNECTIONS: usize = 128;

/// Per-write budget for the downstream socket. `STREAM_IDLE_TIMEOUT` only
/// bounds the upstream; a player that stops reading must not pin a stream
/// slot forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// Ceiling on one relayed body. A healthy FLAC is far below this, so hitting
/// it means a broken or hostile upstream, not a long track.
const MAX_STREAM_BYTES: u64 = 512 * 1024 * 1024;

/// Absolute lifetime cap for one relayed stream.
const MAX_STREAM_DURATION: Duration = Duration::from_secs(60 * 60);

/// Consecutive accept errors tolerated before the listener is considered dead
/// (descriptor exhaustion, socket-table pressure) instead of spinning.
const ACCEPT_MAX_ERRORS: u32 = 16;

/// Maximum number of simultaneous client connections.
const MAX_CONCURRENT: usize = 64;

/// Concurrent upstream resolutions (lucida/saavn handshakes + polls). Bands
/// the fan-out so one client can't turn into thousands of upstream requests.
/// Released before the body streams — long tracks never hold a slot here.
static RESOLVE_PERMITS: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(16));

/// How long an in-flight stream may go without a single upstream byte before
/// it is cut. Bounds a stalled transfer; healthy streams chunk continuously.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Total budget for one track resolution (lucida chain + saavn chain).
/// Uncapped, an unlucky track burns minutes (30 polls × timeouts, then the
/// whole Saavn chain) while the player hangs: fail at 60s so it falls back
/// to native audio instead.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(60);

/// Grace period for in-flight streams after shutdown before teardown.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Single-flight guard for `/update/apply`: concurrent callers shared one
/// fixed staging path and could truncate or delete each other's download.
static UPDATE_INFLIGHT: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Latches the shutdown flag and guarantees the accept loop is woken, even
/// when the response write or the helper spawn fails on the way out. A latch
/// without a wake leaves the proxy bound but unresponsive.
struct ShutdownLatch {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl ShutdownLatch {
    fn trip(&self) {
        if !self.flag.swap(true, Ordering::AcqRel) {
            self.notify.notify_waiters();
        }
    }
}

impl Drop for ShutdownLatch {
    fn drop(&mut self) {
        self.trip();
    }
}

/// One shared client for the lightweight control calls (health/shutdown
/// probes). Building a `Client` per call wastes a connection pool + TLS
/// context every time `status`/`apply`/health-poll runs.
static SHARED_CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

fn shared_client() -> reqwest::Client {
    SHARED_CLIENT.clone()
}

pub struct AudioProxy {
    port: u16,
    /// Currently playing track ID (for the /health endpoint).
    current_track: Arc<Mutex<Option<String>>>,
    /// Which upstream serves it: "soulseek" (FLAC, P2P), "lucida" (FLAC)
    /// or "saavn" (320kbps).
    current_source: Arc<Mutex<Option<&'static str>>>,
    /// Signals the accept loop to terminate (graceful shutdown).
    shutdown: Arc<Notify>,
    /// Latched shutdown flag — closes the Notify race window.
    shutting_down: Arc<AtomicBool>,
    /// Reused HTTP client — one TLS pool, not one per connection.
    client: reqwest::Client,
    /// Bounds concurrent connections to avoid a trivial local DoS.
    semaphore: Arc<tokio::sync::Semaphore>,
    /// Bounds accepted sockets (including ones that never send a request).
    conn_semaphore: Arc<tokio::sync::Semaphore>,
}

impl AudioProxy {
    pub fn new(port: u16) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                "kebabify/",
                env!("CARGO_PKG_VERSION"),
                " (Spotify FLAC proxy)"
            ))
            .connect_timeout(Duration::from_secs(10))
            // NOTE: no total .timeout() here — it would bound the full FLAC
            // body as well and cut long/slow streams mid-flight.
            .build()
            .expect("failed to build HTTP client");

        Self {
            port,
            current_track: Arc::new(Mutex::new(None)),
            current_source: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(Notify::new()),
            shutting_down: Arc::new(AtomicBool::new(false)),
            client,
            semaphore: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)),
            conn_semaphore: Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS)),
        }
    }

    /// Starts the proxy server. Runs until a `/shutdown` request arrives
    /// or Ctrl+C is pressed (graceful stop for foreground runs).
    pub async fn start(&self) -> Result<()> {
        let listener = tokio::net::TcpListener::bind((PROXY_HOST, self.port))
            .await
            .context("Failed to bind audio proxy. Port may be in use.")?;
        // Mint the per-boot shutdown token before serving: only readers of
        // our own APPDATA dir can stop this instance from here on. The value
        // is kept in memory for the lifetime of the loop, so deleting the
        // file on disk cannot make the check fail open.
        let admin_token = Arc::new(
            crate::shutdown_token::load_or_create().context("Failed to mint shutdown token")?,
        );

        println!(
            "[kebabify] Audio proxy listening on {}:{}",
            PROXY_HOST, self.port
        );

        // Tracked so shutdown drains in-flight streams instead of aborting
        // them mid-chunk when the runtime tears down.
        let mut tasks = tokio::task::JoinSet::new();
        let mut consecutive_errors: u32 = 0;
        let mut rejected: u64 = 0;

        loop {
            // Reap finished tasks: JoinSet keeps every completed entry (and
            // its output) alive until join_all, so a long-lived proxy would
            // accumulate them for as long as it serves.
            while tasks.try_join_next().is_some() {}

            // Check the latched flag before each select: if a /shutdown arrived
            // in the window where no waiter was registered, the Notify alone
            // would be missed and the proxy would need a second /shutdown.
            if self.shutting_down.load(Ordering::Acquire) {
                break;
            }

            let (socket, _) = match tokio::select! {
                _ = self.shutdown.notified() => break,
                _ = tokio::signal::ctrl_c() => {
                    self.shutting_down.store(true, Ordering::Release);
                    break;
                }
                accepted = listener.accept() => accepted,
            } {
                Ok(conn) => {
                    consecutive_errors = 0;
                    conn
                }
                Err(e) => {
                    // Transient accept errors must not kill the whole proxy,
                    // but a persistent one (descriptor exhaustion) must not
                    // spin a worker at 100% CPU either.
                    consecutive_errors += 1;
                    eprintln!("[kebabify] Proxy accept error: {}", e);
                    if consecutive_errors >= ACCEPT_MAX_ERRORS {
                        return Err(anyhow!(
                            "Proxy listener failing repeatedly ({} consecutive errors): {}",
                            consecutive_errors,
                            e
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            // Loopback bulk audio: disable Nagle so headers + first chunk go
            // out immediately instead of waiting for a delayed ACK.
            let _ = socket.set_nodelay(true);

            // Admission before spawning: the audio semaphore is taken later
            // (admin endpoints must stay live while all stream slots are
            // busy), so without this a flood of silent connections would
            // occupy a task and a socket each.
            let conn_permit = match self.conn_semaphore.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    rejected += 1;
                    if rejected % 64 == 1 {
                        eprintln!(
                            "[kebabify] Connection limit reached ({}), dropping new connections",
                            MAX_CONNECTIONS
                        );
                    }
                    drop(socket);
                    continue;
                }
            };

            let current_track = self.current_track.clone();
            let current_source = self.current_source.clone();
            let shutting_down = self.shutting_down.clone();
            let shutdown = self.shutdown.clone();
            let client = self.client.clone();
            let semaphore = self.semaphore.clone();
            let admin_token = admin_token.clone();

            tasks.spawn(async move {
                let _conn_permit = conn_permit;
                if let Err(e) = handle_client(
                    socket,
                    current_track,
                    current_source,
                    shutting_down,
                    shutdown,
                    client,
                    semaphore,
                    admin_token,
                )
                .await
                {
                    eprintln!("[kebabify] Proxy error: {}", e);
                }
            });
        }

        // Drain: refuse new bulk work, let in-flight streams finish briefly,
        // then return (the runtime aborts whatever is left).
        self.semaphore.close();
        drop(listener);
        let _ = tokio::time::timeout(DRAIN_TIMEOUT, tasks.join_all()).await;

        println!("[kebabify] Audio proxy stopped");
        Ok(())
    }

    /// Sends a shutdown request to a running proxy instance. Attaches the
    /// per-boot token when this machine minted one (older proxies predate
    /// the file and accept the Origin-only request).
    pub async fn request_shutdown() -> Result<()> {
        let url = format!("http://{}:{}/shutdown", PROXY_HOST, PROXY_PORT);
        let mut req = shared_client()
            .get(&url)
            .timeout(Duration::from_secs(3))
            .header("Origin", SHUTDOWN_ORIGIN);
        if let Some(token) = crate::shutdown_token::load() {
            req = req.header(crate::shutdown_token::TOKEN_HEADER, token);
        }
        let resp = req
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
    /// Strict: the body must parse as our `/health` JSON with `"status":"ok"`,
    /// so a foreign server squatting the port is never adopted as a proxy.
    pub async fn is_running() -> bool {
        proxy_health().await.is_some_and(|h| h.status_ok)
    }

    /// Reported `/health` version, if a real proxy answers. `None` covers
    /// down, foreign, and ancient (pre-version-field) proxies.
    pub async fn proxy_version() -> Option<String> {
        proxy_health().await.and_then(|h| h.version)
    }

    /// Waits up to `timeout` for the proxy to answer `/health` at all
    /// (readiness after spawn — any status counts, the port is bound).
    pub async fn wait_until_ready(timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            // Our own health shape, not just any 200: a foreign port
            // squatter must not count as "ready".
            if proxy_health().await.is_some() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        false
    }

    /// Waits up to `timeout` for the proxy to stop answering (inverse of
    /// readiness — used after requesting a shutdown before respawning, so a
    /// draining old instance still holding the port doesn't make the new
    /// spawn fail its bind).
    pub async fn wait_until_stopped(timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if !Self::is_running().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        false
    }
}

/// Parsed `/health` state: liveness plus the serving binary's version.
struct HealthState {
    status_ok: bool,
    version: Option<String>,
}

/// Fetches and parses `/health`. `None` on any failure or foreign body.
async fn proxy_health() -> Option<HealthState> {
    let url = format!("http://{}:{}/health", PROXY_HOST, PROXY_PORT);
    let resp = shared_client()
        .get(&url)
        .timeout(Duration::from_millis(400))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    Some(HealthState {
        status_ok: json.get("status").and_then(|v| v.as_str()) == Some("ok"),
        version: json
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// Update-check responses cached here so the proxy doesn't hit the GitHub
/// API (60 anonymous req/h/IP) on every badge poll.
static UPDATE_CACHE: std::sync::LazyLock<tokio::sync::Mutex<Option<(std::time::Instant, String)>>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(None));

/// Cached `{"update_available", "current", "latest"}` body, refreshed past
/// [`crate::updater::CHECK_TTL`]. Errors resolve to "no update" and are NOT
/// cached, so recovery after an outage is immediate.
async fn cached_update_check(client: &reqwest::Client) -> String {
    if let Some((at, body)) = UPDATE_CACHE.lock().await.clone()
        && at.elapsed() < crate::updater::CHECK_TTL
    {
        return body;
    }
    // Errors are deliberately NOT cached: an offline proxy would otherwise
    // report "no update" for a full hour.
    let (available, body) = match crate::updater::check_update(client).await {
        Ok(Some(info)) => (
            true,
            serde_json::json!({
                "update_available": true,
                "current": crate::updater::current_version(),
                "latest": info.version,
            })
            .to_string(),
        ),
        _ => (
            false,
            serde_json::json!({
                "update_available": false,
                "current": crate::updater::current_version(),
                "latest": serde_json::Value::Null,
            })
            .to_string(),
        ),
    };
    if available {
        *UPDATE_CACHE.lock().await = Some((std::time::Instant::now(), body.clone()));
    }
    body
}

/// What a source handed back: either a live HTTP response to relay
/// (lucida/saavn), or a validated local file to slice (Soulseek downloads
/// whole files — seeking is served from disk, not forwarded upstream).
enum Upstream {
    Http {
        resp: reqwest::Response,
        is_flac: bool,
    },
    File {
        path: std::path::PathBuf,
        len: u64,
    },
}

/// Serves a cached local file (Soulseek source) with manual Range support.
/// `headers_only` (HEAD) sends headers without the body, like the HTTP path.
async fn serve_file<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    path: &std::path::Path,
    len: u64,
    range: Option<&str>,
    origin: Option<&str>,
    headers_only: bool,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let (status_line, content_range, start, end) =
        match range.and_then(|r| parse_range_header(r, len)) {
            Some((s, e)) => (
                "HTTP/1.1 206 Partial Content",
                Some(format!("bytes {}-{}/{}", s, e, len)),
                s,
                e,
            ),
            None => ("HTTP/1.1 200 OK", None, 0, len.saturating_sub(1)),
        };
    let body_len = end.saturating_sub(start) + 1;
    let mut header = format!(
        "{}\r\nContent-Type: audio/flac\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-cache{}",
        status_line,
        body_len,
        cors_header_line(origin)
    );
    if let Some(cr) = content_range {
        header.push_str(&format!("\r\nContent-Range: {}", cr));
    }
    header.push_str("\r\n\r\n");
    write_bounded(writer, header.as_bytes()).await?;
    flush_bounded(writer).await?;

    if !headers_only {
        let mut file = tokio::fs::File::open(path)
            .await
            .context("Failed to open cached FLAC")?;
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .context("Failed to seek cached FLAC")?;
        // Chunked with a per-step budget instead of `tokio::copy`: a reader
        // that stalls (or stops reading) must not pin the stream slot.
        let mut remaining = body_len;
        let mut buf = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            let read = tokio::time::timeout(STREAM_IDLE_TIMEOUT, file.read(&mut buf[..want]))
                .await
                .context("Cached FLAC read stalled")??;
            if read == 0 {
                return Err(anyhow!("Cached FLAC ended {} bytes early", remaining));
            }
            write_bounded(writer, &buf[..read]).await?;
            remaining -= read as u64;
        }
        flush_bounded(writer).await?;
    }
    Ok(())
}

/// Parses a `Range` header against a known length. Returns the inclusive
/// (start, end) byte span, or `None` when the range is missing, malformed
/// or unsatisfiable (caller then serves 200 full body). Pure for tests.
fn parse_range_header(range: &str, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let spec = range.strip_prefix("bytes=")?;
    let (start_str, end_str) = spec.split_once('-')?;
    if start_str.is_empty() {
        // Suffix range: last N bytes.
        let n: u64 = end_str.parse().ok()?;
        if n == 0 {
            return None;
        }
        Some((len.saturating_sub(n), len - 1))
    } else {
        let start: u64 = start_str.parse().ok()?;
        if start >= len {
            return None;
        }
        if end_str.is_empty() {
            Some((start, len - 1))
        } else {
            let end: u64 = end_str.parse().ok()?;
            if end < start || end >= len {
                return None;
            }
            Some((start, end))
        }
    }
}

/// Accumulates a request head from raw socket reads, enforcing the byte cap
/// *while* reading. `read_line` would grow its String to whatever the peer
/// sends before any check runs, so a single line without CRLF could allocate
/// megabytes per connection. Pure for tests.
struct HeadReader {
    block: String,
    pending: Vec<u8>,
}

impl HeadReader {
    fn new() -> Self {
        Self {
            block: String::new(),
            pending: Vec::new(),
        }
    }

    /// Feeds one socket read. Returns `true` once the blank line terminating
    /// the head has been consumed.
    fn feed(&mut self, chunk: &[u8]) -> Result<bool> {
        for &byte in chunk {
            if self.pending.len() >= MAX_HEADER_BYTES {
                return Err(anyhow!("Request headers too large"));
            }
            self.pending.push(byte);
            if byte != b'\n' {
                continue;
            }
            let line = std::mem::take(&mut self.pending);
            self.block.push_str(&String::from_utf8_lossy(&line));
            if self.block.len() > MAX_HEADER_BYTES {
                return Err(anyhow!("Request headers too large"));
            }
            if line == b"\r\n" || line == b"\n" {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The head collected so far, including a last line left unterminated by
    /// EOF (single-line requests are valid enough to classify).
    fn finish(mut self) -> String {
        if !self.pending.is_empty() {
            self.block.push_str(&String::from_utf8_lossy(&self.pending));
        }
        self.block
    }
}

/// Writes to the client with a budget: a peer that stops reading must not pin
/// a stream slot (or a connection task) forever.
async fn write_bounded<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    data: &[u8],
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    tokio::time::timeout(WRITE_TIMEOUT, writer.write_all(data))
        .await
        .context("Timed out writing to client")??;
    Ok(())
}

/// Flushes to the client with the same budget.
async fn flush_bounded<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    tokio::time::timeout(WRITE_TIMEOUT, writer.flush())
        .await
        .context("Timed out flushing to client")??;
    Ok(())
}

/// Handles a single HTTP request to the proxy.
///
/// Lock order (never inverted anywhere): `current_track`, then
/// `current_source`. Neither is ever held across network I/O.
#[allow(clippy::too_many_arguments)]
async fn handle_client(
    stream: tokio::net::TcpStream,
    current_track: Arc<Mutex<Option<String>>>,
    current_source: Arc<Mutex<Option<&'static str>>>,
    shutting_down: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
    client: reqwest::Client,
    semaphore: Arc<tokio::sync::Semaphore>,
    admin_token: Arc<String>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);

    // Read the request head (request line + headers). Two budgets: 5s per read
    // so a stalled sender is cut, plus 10s total so thousands of tiny reads
    // within the 8K cap can't hold a slot nearly forever. Fixed-size reads:
    // the cap is enforced by `HeadReader` as bytes arrive, not after a whole
    // line has been buffered.
    let header_block = match tokio::time::timeout(HEADER_TOTAL_TIMEOUT, async {
        let mut head = HeadReader::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = tokio::time::timeout(READ_TIMEOUT, reader.read(&mut buf))
                .await
                .context("Timed out waiting for HTTP request")?
                .context("Failed to read HTTP request")?;
            if n == 0 {
                break;
            }
            if head.feed(&buf[..n])? {
                break;
            }
        }
        Ok::<_, anyhow::Error>(head.finish())
    })
    .await
    {
        Ok(Ok(block)) => block,
        // Unreadable head (timeout, oversize, non-UTF8, EOF): answer 400
        // instead of dropping the connection silently — players report
        // "bad request", not "connection reset".
        _ => {
            let _ = write_bad_request(&mut write_half, None, "malformed request head").await;
            return Err(anyhow!("Malformed HTTP request head"));
        }
    };

    if header_block.is_empty() {
        return Ok(());
    }

    // Origin is needed for CORS even on error responses below.
    let origin = request_origin(&header_block);
    let shutdown_token = header_value(&header_block, crate::shutdown_token::TOKEN_HEADER);

    if has_conflicting_headers(&header_block) {
        let _ = write_bad_request(
            &mut write_half,
            origin.as_deref(),
            "conflicting duplicate headers",
        )
        .await;
        return Err(anyhow!("Conflicting duplicate headers"));
    }

    // Parse the request line: "GET /path HTTP/1.1"
    let parts: Vec<&str> = header_block
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    if parts.len() != 3 || !parts[2].starts_with("HTTP/") {
        let _ =
            write_bad_request(&mut write_half, origin.as_deref(), "malformed request line").await;
        return Err(anyhow!("Malformed HTTP request"));
    }

    let method = parts[0];
    let path = parts[1];
    let endpoint = endpoint_for(method, path);

    // CORS preflight: Spotify's fetch() from xpui would otherwise be blocked
    // before the real request happens. Answered for any path, no auth needed.
    if endpoint == Endpoint::Preflight {
        write_preflight(&mut write_half, origin.as_deref()).await?;
        return Ok(());
    }

    // Only GET serves content here (plus POST for /update/apply): anything
    // else is a client bug or a probe — answer 400 instead of treating it
    // as audio.
    if method != "GET" && endpoint != Endpoint::UpdateApply {
        write_bad_request(
            &mut write_half,
            origin.as_deref(),
            "unsupported HTTP method",
        )
        .await?;
        return Err(anyhow!("Unsupported HTTP method: {}", method));
    }

    // ===== Admin endpoints =====
    if endpoint == Endpoint::Health {
        // Only answer to requests from allowed origins (or non-browser clients).
        if let Some(o) = origin.as_deref()
            && !is_allowed_origin(o)
        {
            return Err(anyhow!("Forbidden origin for /health: {}", o));
        }

        // The now-playing track ID is listening-habit data: only allowlisted
        // Spotify callers see it. Unauthenticated callers (curl, widgets)
        // still get status/source/version.
        let track = match origin.as_deref() {
            Some(o) if is_allowed_origin(o) => current_track.lock().await.clone(),
            _ => None,
        };
        // Built with serde_json rather than format!: track IDs are validated
        // alphanumerics today, but manual quoting would silently break on the
        // first value that ever needs escaping.
        let source = (*current_source.lock().await).unwrap_or("none");
        // Soulseek serves validated FLAC too — the badge must not lie.
        let flac = source == "lucida" || source == "soulseek";
        let body = serde_json::json!({
            "status": "ok",
            "track": track,
            "flac": flac,
            "source": source,
            // Lets apply/run detect a stale detached proxy from an older
            // release and restart it instead of reusing it forever.
            "version": env!("CARGO_PKG_VERSION"),
        })
        .to_string();

        // Forbidden origins were rejected above, so the shared builder's
        // CORS echo (`*` for curl, echo for Spotify) matches the siblings.
        let resp = json_response("HTTP/1.1 200 OK", &body, origin.as_deref());

        write_half.write_all(&resp).await?;
        write_half.flush().await?;
        return Ok(());
    }

    if endpoint == Endpoint::UpdateCheck {
        // Same origin rule as /health: allowlisted callers, or non-browser
        // clients with no Origin at all.
        if let Some(o) = origin.as_deref()
            && !is_allowed_origin(o)
        {
            return Err(anyhow!("Forbidden origin for /update/check: {}", o));
        }
        let body = cached_update_check(&client).await;
        let resp = json_response("HTTP/1.1 200 OK", &body, origin.as_deref());
        write_half.write_all(&resp).await?;
        write_half.flush().await?;
        return Ok(());
    }

    if endpoint == Endpoint::UpdateApply {
        // Strict: Spotify UI origins only. Unlike /health, an absent Origin
        // is refused — no curl-triggered self-replacement.
        match origin.as_deref() {
            Some(o) if is_allowed_origin(o) => {}
            _ => {
                write_forbidden(&mut write_half).await?;
                return Err(anyhow!("Forbidden origin for /update/apply"));
            }
        }
        // Single-flight: concurrent callers shared one fixed staging path, so
        // one download could truncate or delete another's staged binary.
        let Ok(_update_guard) = UPDATE_INFLIGHT.try_lock() else {
            write_update_busy(&mut write_half, origin.as_deref()).await?;
            return Err(anyhow!("Update already in progress"));
        };
        let (version, error_hint): (String, Option<String>) =
            match crate::updater::check_update(&client).await {
                Ok(Some(info)) => {
                    let exe = std::env::current_exe().context("Cannot find kebabify.exe path")?;
                    let staged = crate::updater::staged_path(&exe);
                    match crate::updater::download_release(&client, &info, &staged).await {
                        Ok(()) => (info.version.clone(), None),
                        Err(e) => (String::new(), Some(format!("{:#}", e))),
                    }
                }
                Ok(None) => (String::new(), None),
                Err(e) => (String::new(), Some(format!("{:#}", e))),
            };
        // A failed update must not look like "up to date" (the old `?`
        // propagation closed the connection with no response, and the badge
        // just disappeared). Report it as a 502 JSON instead.
        if let Some(hint) = error_hint {
            write_update_error(&mut write_half, origin.as_deref(), &hint).await?;
            return Err(anyhow!("Update failed: {}", hint));
        }
        let body = if version.is_empty() {
            serde_json::json!({"status": "up-to-date"}).to_string()
        } else {
            serde_json::json!({"status": "updating", "version": version}).to_string()
        };
        let resp = json_response("HTTP/1.1 200 OK", &body, origin.as_deref());
        write_bounded(&mut write_half, &resp).await?;
        flush_bounded(&mut write_half).await?;
        if !version.is_empty() {
            // Answer went out: stage the swap helper, then die so it can
            // replace us and relaunch `apply` with the new binary. The latch
            // guarantees the accept loop wakes even if the spawn failed.
            let exe = std::env::current_exe().context("Cannot find kebabify.exe path")?;
            crate::updater::stage_and_relaunch(&exe, &crate::updater::staged_path(&exe))?;
            ShutdownLatch {
                flag: shutting_down.clone(),
                notify: shutdown.clone(),
            }
            .trip();
        }
        return Ok(());
    }

    if endpoint == Endpoint::Shutdown {
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
        // Plus the per-boot token (an Origin header is forgeable by any local
        // process). Compared against the copy minted at startup: deleting the
        // file on disk must not turn this check into a no-op.
        let token_ok = shutdown_token
            .as_deref()
            .is_some_and(|t| crate::shutdown_token::verify(t, admin_token.as_str()));
        if !token_ok {
            write_forbidden(&mut write_half).await?;
            return Err(anyhow!("Refusing shutdown without a valid token"));
        }

        // Latch before writing: even if the response write fails, the Drop
        // impl still wakes the accept loop instead of leaving the proxy bound
        // and apparently alive.
        let latch = ShutdownLatch {
            flag: shutting_down.clone(),
            notify: shutdown.clone(),
        };
        latch.trip();
        let body = serde_json::json!({"status": "stopping"}).to_string();
        let resp = json_response("HTTP/1.1 200 OK", &body, None);
        write_bounded(&mut write_half, &resp).await?;
        flush_bounded(&mut write_half).await?;
        return Ok(());
    }

    // ===== Audio stream request =====
    // The JS sends us ?track=TRACKID, or the original Spotify URL.
    // The query value is validated like any other source: an unchecked
    // `track=` would otherwise build bogus open.spotify.com/track/… URLs.
    // Same key set as `extract_track_id` so direct and proxified URLs agree.
    let track_id: Option<String> = if let Some(qpos) = path.find('?') {
        let query_str = &path[qpos + 1..];
        let mut found_track_id = None;
        for param in query_str.split('&') {
            if let Some((k, v)) = param.split_once('=') {
                // Strip a #fragment: clients may append one to an otherwise
                // valid ID, and it must not fail validation.
                let v = v.split('#').next().unwrap_or("");
                if (k == "track" || k == "track_id" || k == "id" || k == "cid") && is_track_id(v) {
                    found_track_id = Some(v.to_string());
                    break;
                }
            }
        }
        found_track_id
    } else if path.starts_with("http") || path.contains("spotify") {
        extract_track_id(path)
    } else {
        let host = header_value(&header_block, "host")
            .unwrap_or_else(|| "audio-spclient.wg.spotify.com".to_string());
        let full_url = format!("https://{}{}", host, path);
        eprintln!("[kebabify] Proxy request: GET {}", full_url);
        extract_track_id(&full_url)
    };

    let Some(tid) = track_id else {
        write_bad_request(
            &mut write_half,
            origin.as_deref(),
            "missing or invalid track id",
        )
        .await?;
        return Err(anyhow!("Could not extract a track ID from request"));
    };

    // Bulk admission, fail-fast: when all stream slots are busy (or the
    // server is draining), answer 503 instead of queueing behind minutes of
    // audio. Admin endpoints never reach this point, so they stay live.
    let _permit = match semaphore.try_acquire() {
        Ok(p) => p,
        Err(_) => {
            write_busy(&mut write_half, origin.as_deref()).await?;
            return Ok(());
        }
    };

    *current_track.lock().await = Some(tid.clone());
    eprintln!("[kebabify] Track ID: {} — resolving upstream", tid);

    let spotify_url = format!("https://open.spotify.com/track/{}", tid);

    // Optional Range header — forwarded so the player can seek. When lucida's
    // download endpoint honors it (206 + Content-Range) the player gets real
    // byte-range seeking; when it doesn't (200 full body) behavior is unchanged.
    // If-Range rides along so conditional seeks stay conditional.
    let range_header = request_range(&header_block);
    let if_range_header = request_if_range(&header_block);

    // Resolve and stream inside a block: whatever happens, the indicator is
    // reset afterwards so /health never reports a ghost track.
    let mut response_started = false;
    let result: Result<()> = async {
        // Source chain, in priority order:
        // 1. Soulseek P2P FLAC — no central server to take down, so first.
        // 2. lucida FLAC. 3. Saavn 320kbps — a track playing in high quality
        // beats a 502. The resolve permit is scoped to this block:
        // handshakes, polls and the P2P download are bounded, bodies stream
        // after (Soulseek serves from its local cache file instead).
        // The whole resolution is additionally capped: an unlucky track must
        // fail into native playback, not hang the player for minutes.
        // `stage` names the in-flight step so the timeout error says WHERE
        // the 60s went (soulseek handshake vs lucida poll vs saavn fallback),
        // not just THAT.
        let stage = Arc::new(Mutex::new("starting"));
        let stage_inner = stage.clone();
        let (upstream, source): (Upstream, &'static str) =
            match tokio::time::timeout(RESOLVE_TIMEOUT, async {
                let _resolve_permit = RESOLVE_PERMITS
                    .acquire()
                    .await
                    .context("resolve pool shut down")?;
                *stage_inner.lock().await = "soulseek";
                // Per-source deadline: a P2P download that has not produced a
                // file in 25s must not eat the whole 60s budget and leave the
                // player with no fallback at all.
                let soulseek = tokio::time::timeout(
                    crate::soulseek::OPEN_FILE_TIMEOUT,
                    crate::soulseek::open_file(&client, &tid),
                )
                .await;
                let slsk_err: Option<String> = match soulseek {
                    Ok(Ok(f)) => {
                        return Ok((
                            Upstream::File {
                                path: f.path,
                                len: f.len,
                            },
                            "soulseek",
                        ));
                    }
                    Ok(Err(e)) => Some(format!("{:#}", e)),
                    Err(_) => Some(format!(
                        "timed out after {}s",
                        crate::soulseek::OPEN_FILE_TIMEOUT.as_secs()
                    )),
                };
                if let Some(detail) = &slsk_err {
                    eprintln!(
                        "[kebabify] Soulseek failed for track {}: {} — trying lucida",
                        tid, detail
                    );
                }
                match resolve_fallbacks(
                    &client,
                    &tid,
                    &spotify_url,
                    range_header.as_deref(),
                    if_range_header.as_deref(),
                    &stage_inner,
                )
                .await
                {
                    Ok(found) => Ok(found),
                    Err(fallback_err) => Err(anyhow!(
                        "soulseek: {}; {}",
                        slsk_err.unwrap_or_else(|| "unknown".to_string()),
                        fallback_err
                    )),
                }
            })
            .await
            {
                Ok(Ok((upstream, source))) => (upstream, source),
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(anyhow!(
                        "upstream resolution timed out after {}s (in stage: {})",
                        RESOLVE_TIMEOUT.as_secs(),
                        *stage.lock().await
                    ));
                }
            };
        *current_source.lock().await = Some(source);

        match upstream {
            Upstream::File { path, len } => {
                serve_file(
                    &mut write_half,
                    &path,
                    len,
                    range_header.as_deref(),
                    origin.as_deref(),
                    method == "HEAD",
                )
                .await?;
                eprintln!(
                    "[kebabify] Served FLAC (soulseek P2P) for track {} ({} bytes)",
                    tid, len
                );
            }
            Upstream::Http { resp: mut audio_resp, is_flac } => {
        let default_type = if is_flac { "audio/flac" } else { "audio/mp4" };
        let content_type = audio_resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(default_type)
            .to_string();

        if !content_type.contains("flac") && !content_type.contains("audio") {
            eprintln!(
                "[kebabify] WARNING: content-type is not audio: {}",
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
            "{}\r\nContent-Type: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\nCache-Control: no-cache{}",
            status_line,
            content_type,
            cors_header_line(origin.as_deref())
        );
        for name in ["content-range", "accept-ranges", "content-length"] {
            if let Some(v) = audio_resp.headers().get(name)
                && let Ok(s) = v.to_str() {
                    response_header.push_str(&format!("\r\n{}: {}", name, s));
                }
        }
        response_header.push_str("\r\n\r\n");

        response_started = true;
        write_bounded(&mut write_half, response_header.as_bytes())
            .await
            .map_err(|e| anyhow!("Failed to write response headers: {}", e))?;

        flush_bounded(&mut write_half)
            .await
            .map_err(|e| anyhow!("Failed to flush headers: {}", e))?;

        // HEAD can never reach this point: the method gate above rejects
        // everything but GET (and POST /update/apply), so there is no
        // headers-only branch to maintain here.
        // Stream the body. Three independent bounds: an idle timeout (no byte
        // for 60s), a total duration (a track is minutes, not hours), and a
        // byte ceiling (an upstream trickling one byte per minute must not
        // hold the slot forever). Healthy streams trip none of them.
        let mut total_bytes: u64 = 0;
        let relay = async {
            while let Some(chunk) =
                tokio::time::timeout(STREAM_IDLE_TIMEOUT, audio_resp.chunk())
                    .await
                    .context("Upstream stalled mid-stream")??
            {
                total_bytes += chunk.len() as u64;
                if total_bytes > MAX_STREAM_BYTES {
                    return Err(anyhow!(
                        "Upstream body exceeds {} byte cap",
                        MAX_STREAM_BYTES
                    ));
                }
                write_bounded(&mut write_half, &chunk).await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        tokio::time::timeout(MAX_STREAM_DURATION, relay)
            .await
            .map_err(|_| {
                anyhow!(
                    "Upstream stream exceeded {}s duration cap",
                    MAX_STREAM_DURATION.as_secs()
                )
            })??;

        flush_bounded(&mut write_half).await?;
        eprintln!(
            "[kebabify] Served {} for track {} ({} bytes)",
            if is_flac {
                "FLAC"
            } else {
                "AAC-320 (saavn fallback)"
            },
            tid,
            total_bytes
        );
            } // end Upstream::Http arm
        } // end match upstream

        Ok(())
    }
    .await;

    // Track finished (or failed) — drop the indicators so /health reports null.
    // Only when they still describe this request: with prefetching, a newer
    // track may already own them, and blanking would report a ghost null.
    {
        let mut track = current_track.lock().await;
        if track.as_deref() == Some(tid.as_str()) {
            *track = None;
            *current_source.lock().await = None;
        }
    }

    if let Err(e) = &result {
        // JSON reason (bounded) + CORS: the extension diagnoses 502s via
        // fetch, which needs ACAO to read the body at all. A Cloudflare 403
        // from lucida means "run kebabify import-cookies" — say so.
        // The full reason names the playing track: only callers that get an
        // ACAO header see it, everyone else gets a coarse class.
        let reason: String = format!("{:#}", e).chars().take(240).collect();
        eprintln!("[kebabify] Stream for track {} failed: {}", tid, reason);
        let public = cors_allow_origin(origin.as_deref()).is_some();
        let reason_out = if public {
            reason
        } else {
            error_class(&reason).to_string()
        };
        write_stream_error(
            &mut write_half,
            response_started,
            origin.as_deref(),
            &reason_out,
        )
        .await?;
    }

    result
}

/// Coarse failure class for callers that must not see the full reason
/// (track titles and upstream internals). Pure for tests.
fn error_class(reason: &str) -> &'static str {
    if reason.contains("403") || reason.contains("Cloudflare") {
        "lucida-403"
    } else if reason.contains("no good match") {
        "saavn-no-match"
    } else if reason.contains("timed out") {
        "upstream-timeout"
    } else {
        "proxy-error"
    }
}

/// One shared builder for the admin `200 JSON` responses (health,
/// update/check, update/apply, shutdown): status line + fixed headers +
/// CORS echo, so the seven hand-rolled responders can't drift apart.
fn json_response(status_line: &str, body: &str, origin: Option<&str>) -> Vec<u8> {
    format!(
        "{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close{}\r\n\r\n{}",
        status_line,
        body.len(),
        cors_header_line(origin),
        body
    )
    .into_bytes()
}

/// Second and third choices after Soulseek: lucida FLAC, then Saavn 320kbps.
/// Isolated so a Soulseek failure and a Soulseek timeout share one fallback
/// path instead of two copies of the same chain.
async fn resolve_fallbacks(
    client: &reqwest::Client,
    tid: &str,
    spotify_url: &str,
    range: Option<&str>,
    if_range: Option<&str>,
    stage: &Mutex<&'static str>,
) -> Result<(Upstream, &'static str)> {
    *stage.lock().await = "lucida";
    match crate::lucida::open_stream(client, spotify_url, range, if_range).await {
        Ok(s) => Ok((
            Upstream::Http {
                resp: s.into_response(),
                is_flac: true,
            },
            "lucida",
        )),
        Err(lucida_err) => {
            eprintln!(
                "[kebabify] lucida failed for track {}: {:#} — trying Saavn fallback",
                tid, lucida_err
            );
            *stage.lock().await = "saavn";
            match crate::saavn::open_stream(client, tid, range, if_range).await {
                Ok(s) => Ok((
                    Upstream::Http {
                        resp: s.into_response(),
                        is_flac: false,
                    },
                    "saavn",
                )),
                Err(saavn_err) => Err(anyhow!("lucida: {:#}; saavn: {:#}", lucida_err, saavn_err)),
            }
        }
    }
}

/// Writes a `409 Conflict` JSON response when an update is already running:
/// the second caller must not start a competing download on the same path.
async fn write_update_busy<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    origin: Option<&str>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let body =
        serde_json::json!({"status": "error", "hint": "update already in progress"}).to_string();
    writer
        .write_all(&json_response("HTTP/1.1 409 Conflict", &body, origin))
        .await?;
    writer.flush().await?;
    Ok(())
}

/// Writes a `502 Bad Gateway` JSON response when the self-update flow fails
/// (check or download), so the badge can show the hint instead of vanishing.
async fn write_update_error<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    origin: Option<&str>,
    hint: &str,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let body = serde_json::json!({"status": "error", "hint": hint}).to_string();
    writer
        .write_all(&json_response("HTTP/1.1 502 Bad Gateway", &body, origin))
        .await?;
    writer.flush().await?;
    Ok(())
}

async fn write_stream_error<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    response_started: bool,
    origin: Option<&str>,
    reason: &str,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    if response_started {
        return Ok(());
    }
    let hint = if reason.contains("no credentials") {
        "Soulseek login missing — run `kebabify soulseek <user> <pass>` (chain falls through to lucida/saavn anyway)"
    } else if reason.contains("403") || reason.contains("Cloudflare") {
        "lucida.to is behind Cloudflare — run `kebabify import-cookies` for FLAC (Saavn fallback also failed)"
    } else if reason.contains("no good match") || reason.contains("saavn-no-match") {
        "Saavn fallback found no reliable 320kbps match — track may be missing or mislabeled there"
    } else if reason.contains("timed out") || reason.contains("upstream-timeout") {
        "upstreams took too long — retry, or check your connection"
    } else {
        "proxy stream error"
    };
    let body = stream_error_body(hint, reason);
    let resp = format!(
        "HTTP/1.1 502 Bad Gateway\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close{}\r\n\r\n{}",
        body.len(),
        cors_header_line(origin),
        body
    );
    writer.write_all(resp.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Builds the 502 JSON body. Pure for tests.
fn stream_error_body(hint: &str, reason: &str) -> String {
    serde_json::json!({"status": "error", "hint": hint, "reason": reason}).to_string()
}

/// Which handler serves a request. Pure for tests.
#[derive(Debug, PartialEq, Eq)]
enum Endpoint {
    Preflight,
    Health,
    Shutdown,
    UpdateCheck,
    UpdateApply,
    Audio,
}

/// Classifies a request by method + path. Preflight wins over everything so
/// a CORS probe never reaches an admin handler. Update-apply is POST-only,
/// shutdown is GET-only (a POST /shutdown is a client bug, not a command).
fn endpoint_for(method: &str, path: &str) -> Endpoint {
    if method == "OPTIONS" {
        Endpoint::Preflight
    } else if path == "/health" {
        Endpoint::Health
    } else if path == "/shutdown" && method == "GET" {
        Endpoint::Shutdown
    } else if path == "/update/check" {
        Endpoint::UpdateCheck
    } else if path == "/update/apply" && method == "POST" {
        Endpoint::UpdateApply
    } else {
        Endpoint::Audio
    }
}

/// Writes a `400 Bad Request` JSON response for unparseable track requests,
/// then flushes. (Previously the connection just dropped — clients hung.)
async fn write_bad_request<W: tokio::io::AsyncWrite + Unpin>(
    write_half: &mut W,
    origin: Option<&str>,
    hint: &str,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let body = serde_json::json!({"status": "error", "hint": hint}).to_string();
    let resp = format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close{}\r\n\r\n{}",
        body.len(),
        cors_header_line(origin),
        body
    );
    write_half.write_all(resp.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

/// Writes a `503 Service Unavailable` JSON response when all stream slots
/// are busy (or the server is draining). Fail-fast beats hanging: the player
/// retries or falls back to native audio.
async fn write_busy<W: tokio::io::AsyncWrite + Unpin>(
    write_half: &mut W,
    origin: Option<&str>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let body = serde_json::json!({"status": "busy", "hint": "proxy at capacity — retry shortly"})
        .to_string();
    let resp = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nRetry-After: 2\r\nConnection: close{}\r\n\r\n{}",
        body.len(),
        cors_header_line(origin),
        body
    );
    write_half.write_all(resp.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

/// Value for `Access-Control-Allow-Origin`, or `None` when the header must be
/// omitted: an allowlisted Spotify caller is echoed back, callers with no
/// `Origin` (curl, media elements) get `*` so local debugging keeps working.
/// Anything else — a DNS-rebound `evil.com` or a sandboxed `null` iframe that
/// *can* read `*` responses — gets nothing, so hostile pages can neither read
/// audio bytes nor error bodies fetched cross-origin.
fn cors_allow_origin(origin: Option<&str>) -> Option<&str> {
    match origin {
        None => Some("*"),
        Some(o) if is_allowed_origin(o) => Some(o),
        _ => None,
    }
}

/// Renders the `Access-Control-Allow-Origin` header line, or nothing when
/// [`cors_allow_origin`] denies the caller.
fn cors_header_line(origin: Option<&str>) -> String {
    match cors_allow_origin(origin) {
        Some(v) => format!("\r\nAccess-Control-Allow-Origin: {}", v),
        None => String::new(),
    }
}

/// Answers a CORS preflight for any path: the Spotify client fetches audio
/// cross-origin (https xpui → http loopback), so OPTIONS must succeed or the
/// real request never leaves the browser.
async fn write_preflight<W: tokio::io::AsyncWrite + Unpin>(
    write_half: &mut W,
    origin: Option<&str>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let resp = format!(
        "HTTP/1.1 204 No Content\r\nX-Content-Type-Options: nosniff\r\nAccess-Control-Allow-Methods: GET, OPTIONS\r\nAccess-Control-Allow-Headers: Range, Origin, Content-Type\r\nAccess-Control-Max-Age: 86400\r\nContent-Length: 0\r\nConnection: close{}\r\n\r\n",
        cors_header_line(origin)
    );
    write_half.write_all(resp.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

/// Writes a minimal `403 Forbidden` JSON response, then flushes.
async fn write_forbidden<W: tokio::io::AsyncWrite + Unpin>(write_half: &mut W) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let body = serde_json::json!({"status": "forbidden"}).to_string();
    let resp = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    write_half.write_all(resp.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

/// Returns the value of a request header, matched case-insensitively without
/// allocating per line (the old `to_lowercase().starts_with(..)` built a
/// `String` for every header line on every request).
fn header_value(header_block: &str, name: &str) -> Option<String> {
    header_values(header_block, name)
        .into_iter()
        .next()
        .map(str::to_string)
}

/// All values of a request header in order. Pure for tests.
fn header_values<'a>(header_block: &'a str, name: &str) -> Vec<&'a str> {
    header_block
        .lines()
        .filter_map(|l| {
            let (key, value) = l.split_once(':')?;
            if !key.trim().eq_ignore_ascii_case(name) {
                return None;
            }
            let v = value.trim();
            if v.is_empty() { None } else { Some(v) }
        })
        .collect()
}

/// Rejects requests whose security headers disagree with themselves: with
/// first-wins parsing, a smuggled second `Origin`/`Range` would otherwise
/// ride along unnoticed.
fn has_conflicting_headers(header_block: &str) -> bool {
    ["origin", "range", "if-range", "host"].iter().any(|name| {
        let values = header_values(header_block, name);
        values.iter().skip(1).any(|v| *v != values[0])
    })
}

/// Returns the value of the `Origin` header, if present.
fn request_origin(header_block: &str) -> Option<String> {
    header_value(header_block, "origin")
}

/// Returns the value of the `Range` header, if present.
fn request_range(header_block: &str) -> Option<String> {
    header_value(header_block, "range")
}

/// Returns the value of the `If-Range` header, if present. Forwarded with
/// `Range` so conditional seeks stay conditional instead of silently
/// turning into unconditional ones.
fn request_if_range(header_block: &str) -> Option<String> {
    header_value(header_block, "if-range")
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
        || origin == SHUTDOWN_ORIGIN
}

/// Only the loopback origin the binary itself uses may shut the proxy down.
/// A hostile web page could forge any host in its Origin header; what it
/// cannot do is make Spotify send 127.0.0.1:18900 — that origin only comes
/// from our own /shutdown caller (main.rs).
fn is_shutdown_origin(origin: &str) -> bool {
    origin == SHUTDOWN_ORIGIN
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
            && is_track_id(&id.1)
        {
            return Some(id.1.to_string());
        }
    }

    // Manual fallback — look for track IDs near known keys.
    // ("spotify:track:" is matched whole so "soundtrack:…" can't hit it, and
    // every other key needs a left delimiter so "?sid=<id>" (?valid=, …)
    // doesn't match the "id=" inside it.)
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
            let key_at = search_from + rel;
            let start = key_at + key.len();
            let rest = &url[start..];
            let delimited = key_at == 0
                || matches!(
                    url[..key_at].chars().next_back(),
                    Some('/' | '?' | '&' | '=' | ':' | '#' | ';')
                );
            if !delimited {
                search_from = start.max(1);
                continue;
            }
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
    async fn stream_error_before_response_sends_json_with_cors() {
        let mut output = Vec::new();
        write_stream_error(
            &mut output,
            false,
            Some("https://xpui.app.spotify.com"),
            "lucida returned HTTP 403",
        )
        .await
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(text.contains("Content-Type: application/json\r\n"));
        assert!(text.contains("Access-Control-Allow-Origin: https://xpui.app.spotify.com\r\n"));
        assert!(text.contains("import-cookies"));
        // Content-Length matches the body.
        let body = text.split("\r\n\r\n").nth(1).unwrap();
        let len: usize = text
            .lines()
            .find(|l| l.starts_with("Content-Length:"))
            .unwrap()["Content-Length:".len()..]
            .trim()
            .parse()
            .unwrap();
        assert_eq!(len, body.len());
    }

    #[tokio::test]
    async fn stream_error_without_origin_allows_star() {
        let mut output = Vec::new();
        write_stream_error(&mut output, false, None, "boom")
            .await
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Access-Control-Allow-Origin: *\r\n"));
    }

    #[tokio::test]
    async fn stream_error_after_response_does_not_append_bytes() {
        for prefix in [
            b"HTTP/1.1 200".as_slice(),
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nfLaC".as_slice(),
        ] {
            let mut output = prefix.to_vec();
            write_stream_error(&mut output, true, None, "boom")
                .await
                .unwrap();
            assert_eq!(output, prefix);
        }
    }

    #[tokio::test]
    async fn cors_denies_unlisted_origin() {
        // DNS-rebound evil.com must get no ACAO anywhere: neither readable
        // errors nor usable preflights.
        let mut output = Vec::new();
        write_stream_error(&mut output, false, Some("https://evil.com"), "boom")
            .await
            .unwrap();
        assert!(
            !String::from_utf8(output)
                .unwrap()
                .contains("Access-Control-Allow-Origin")
        );

        let mut output = Vec::new();
        write_preflight(&mut output, Some("https://evil.com"))
            .await
            .unwrap();
        assert!(
            !String::from_utf8(output)
                .unwrap()
                .contains("Access-Control-Allow-Origin")
        );
    }

    #[tokio::test]
    async fn preflight_answers_no_content_with_cors() {
        let mut output = Vec::new();
        write_preflight(&mut output, Some("https://open.spotify.com"))
            .await
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(text.contains("Access-Control-Allow-Origin: https://open.spotify.com\r\n"));
        assert!(text.contains("Access-Control-Allow-Methods: GET, OPTIONS\r\n"));
        assert!(text.contains("Range"));
    }

    #[tokio::test]
    async fn bad_request_answers_400_with_cors() {
        let mut output = Vec::new();
        write_bad_request(
            &mut output,
            Some("https://open.spotify.com"),
            "missing or invalid track id",
        )
        .await
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(text.contains("Access-Control-Allow-Origin: https://open.spotify.com\r\n"));
        assert!(text.contains("invalid track id"));
    }

    #[tokio::test]
    async fn busy_answers_503_with_retry_after() {
        let mut output = Vec::new();
        write_busy(&mut output, None).await.unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(text.contains("Retry-After: 2\r\n"));
        assert!(text.contains("Access-Control-Allow-Origin: *\r\n"));
        assert!(text.contains("X-Content-Type-Options: nosniff\r\n"));
    }

    #[test]
    fn head_reader_splits_and_stops_at_blank_line() {
        let mut head = HeadReader::new();
        assert!(!head.feed(b"GET /health HTTP/1.1\r\n").unwrap());
        assert!(!head.feed(b"Origin: http://127.0.0.1:18900\r\n").unwrap());
        assert!(head.feed(b"\r\n").unwrap());
        let block = head.finish();
        assert!(block.starts_with("GET /health HTTP/1.1\r\n"));
        assert!(block.contains("Origin: http://127.0.0.1:18900"));
    }

    /// A request split over several reads must be reassembled, and a head
    /// ending at EOF without a blank line must still classify.
    #[test]
    fn head_reader_handles_split_and_eof() {
        let mut head = HeadReader::new();
        for byte in b"GET / HTTP/1.1\r\n" {
            assert!(!head.feed(&[*byte]).unwrap());
        }
        let block = head.finish();
        assert_eq!(block, "GET / HTTP/1.1\r\n");
    }

    /// The cap must bite DURING the read: an unterminated line far larger than
    /// the cap is rejected without ever being accumulated in full.
    #[test]
    fn head_reader_rejects_oversize_line_while_reading() {
        let mut head = HeadReader::new();
        let mut rejected = false;
        for _ in 0..64 {
            match head.feed(&vec![b'A'; 1024]) {
                Ok(done) => assert!(!done),
                Err(_) => {
                    rejected = true;
                    break;
                }
            }
        }
        assert!(rejected, "an endless header line must be refused");
        assert!(head.pending.len() <= MAX_HEADER_BYTES);
    }

    #[test]
    fn head_reader_accepts_head_just_under_cap() {
        let mut head = HeadReader::new();
        let padding = MAX_HEADER_BYTES - 20;
        let line = format!("X-Pad: {}\r\n", "a".repeat(padding));
        assert!(!head.feed(line.as_bytes()).unwrap());
        assert!(head.feed(b"\r\n").unwrap());
        assert!(head.finish().len() <= MAX_HEADER_BYTES);
    }

    #[tokio::test]
    async fn shutdown_latch_wakes_the_loop() {
        let flag = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let notified = notify.notified();
        tokio::pin!(notified);
        {
            let latch = ShutdownLatch {
                flag: flag.clone(),
                notify: notify.clone(),
            };
            latch.trip();
        }
        assert!(flag.load(Ordering::Acquire));
        tokio::time::timeout(Duration::from_millis(500), notified)
            .await
            .expect("accept loop must be woken");
    }

    /// A latch dropped without an explicit trip (a failed response write on the
    /// way out) must still wake the loop instead of leaving the proxy bound.
    #[tokio::test]
    async fn shutdown_latch_notifies_on_drop() {
        let flag = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let notified = notify.notified();
        tokio::pin!(notified);
        drop(ShutdownLatch {
            flag: flag.clone(),
            notify: notify.clone(),
        });
        assert!(flag.load(Ordering::Acquire));
        tokio::time::timeout(Duration::from_millis(500), notified)
            .await
            .expect("a dropped latch must wake the loop");
    }

    #[test]
    fn endpoints_classified() {
        assert_eq!(endpoint_for("GET", "/health"), Endpoint::Health);
        assert_eq!(endpoint_for("GET", "/shutdown"), Endpoint::Shutdown);
        assert_eq!(endpoint_for("GET", "/update/check"), Endpoint::UpdateCheck);
        assert_eq!(endpoint_for("POST", "/update/apply"), Endpoint::UpdateApply);
        // POST anywhere else is not special (→ 400 downstream).
        assert_eq!(endpoint_for("POST", "/shutdown"), Endpoint::Audio);
        assert_eq!(
            endpoint_for("GET", "/?track=4uLU6hMCjMI75M1A2tKUQC"),
            Endpoint::Audio
        );
        // Preflight wins regardless of path.
        assert_eq!(endpoint_for("OPTIONS", "/health"), Endpoint::Preflight);
        assert_eq!(endpoint_for("OPTIONS", "/shutdown"), Endpoint::Preflight);
        assert_eq!(endpoint_for("OPTIONS", "/?track=x"), Endpoint::Preflight);
        // Near-misses are audio (→ 400 downstream), not admin endpoints.
        assert_eq!(endpoint_for("GET", "/health?x=1"), Endpoint::Audio);
        assert_eq!(endpoint_for("DELETE", "/shutdown"), Endpoint::Audio);
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
    fn track_id_keys_need_left_delimiter() {
        // "id=" inside "sid="/valid=, "track/" inside "soundtrack/" must not
        // match — only delimited keys count.
        assert_eq!(
            extract_track_id(&format!("https://x/?sid={}", TRACK_ID)),
            None
        );
        assert_eq!(
            extract_track_id(&format!("https://x/?valid={}", TRACK_ID)),
            None
        );
        assert_eq!(
            extract_track_id(&format!("https://x/soundtrack/{}", TRACK_ID)),
            None
        );
        // Delimited forms still work.
        assert_eq!(
            extract_track_id(&format!("https://x/?track_id={}", TRACK_ID)),
            Some(TRACK_ID.to_string())
        );
        assert_eq!(
            extract_track_id(&format!("https://x/a/track/{}", TRACK_ID)),
            Some(TRACK_ID.to_string())
        );
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
        assert_eq!(request_if_range(block), None);
        assert_eq!(request_origin("GET / HTTP/1.1\r\n\r\n"), None);
        assert_eq!(request_range("GET / HTTP/1.1\r\n\r\n"), None);
    }

    #[test]
    fn duplicate_security_headers_rejected() {
        let same_twice = "GET / HTTP/1.1\r\nOrigin: https://open.spotify.com\r\nOrigin: https://open.spotify.com\r\n\r\n";
        assert!(!has_conflicting_headers(same_twice));
        let conflict = "GET / HTTP/1.1\r\nOrigin: https://evil.com\r\nOrigin: https://open.spotify.com\r\n\r\n";
        assert!(has_conflicting_headers(conflict));
        let ranges = "GET / HTTP/1.1\r\nRange: bytes=0-1\r\nRange: bytes=2-3\r\n\r\n";
        assert!(has_conflicting_headers(ranges));
        assert!(!has_conflicting_headers("GET / HTTP/1.1\r\n\r\n"));
    }

    #[test]
    fn range_header_slices_local_files() {
        const LEN: u64 = 25_278_482;
        assert_eq!(parse_range_header("bytes=0-15", LEN), Some((0, 15)));
        assert_eq!(parse_range_header("bytes=100-", LEN), Some((100, LEN - 1)));
        assert_eq!(
            parse_range_header("bytes=-500", LEN),
            Some((LEN - 500, LEN - 1))
        );
        // Malformed or unsatisfiable → None (caller serves 200 full body).
        assert_eq!(parse_range_header("bytes=99-10", LEN), None);
        assert_eq!(parse_range_header("bytes=99999999-", LEN), None);
        assert_eq!(parse_range_header("bytes=0-99999999", LEN), None);
        assert_eq!(parse_range_header("bytes=-0", LEN), None);
        assert_eq!(parse_range_header("items=0-10", LEN), None);
        assert_eq!(parse_range_header("bytes=abc-", LEN), None);
        assert_eq!(parse_range_header("bytes=0-15", 0), None);
    }
}

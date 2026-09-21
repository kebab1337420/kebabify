//! One-command Cloudflare cookie import for lucida.to.
//!
//! Spawns a browser with a throwaway profile pointed at lucida.to, waits for
//! the user to solve the Cloudflare challenge in that window, then reads the
//! resulting cookies and stores them with `lucida::save_cookies`. No
//! copy/paste, no console spelunking.
//!
//! Two mechanisms, by browser family:
//!
//! - Chromium (Chrome, Edge, Brave, Vivaldi, Opera, Arc): straight from the
//!   browser through the Chrome DevTools Protocol (`Network.getCookies`).
//! - Firefox family (Firefox, Zen, LibreWolf, Waterfox): no CDP — the cookies
//!   are read from the throwaway profile's `cookies.sqlite` instead.
//!
//! The User-Agent of the same browser is captured too, because Cloudflare
//! pins `cf_clearance` to it.

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::lucida;

const CHROME_START_TIMEOUT: Duration = Duration::from_secs(30);
const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(600);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long to wait for a single CDP round-trip before giving up, so a
/// frozen renderer can't hang `import-cookies` past the challenge deadline.
const CDP_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared client for CDP endpoint polling (one pool, not one per poll).
static SHARED_CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Small CDP client over one WebSocket.
struct Cdp {
    ws: Ws,
    next_id: u64,
}

impl Cdp {
    async fn connect(url: &str) -> Result<Self> {
        let (ws, _) = connect_async(url)
            .await
            .context("Cannot connect to Chrome DevTools")?;
        Ok(Self { ws, next_id: 1 })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({ "id": id, "method": method, "params": params });
        self.ws
            .send(Message::Text(payload.to_string().into()))
            .await
            .context("CDP send failed")?;
        loop {
            let next = tokio::time::timeout(CDP_CALL_TIMEOUT, self.ws.next())
                .await
                .context("Timed out waiting for Chrome DevTools response")?;
            match next {
                Some(Ok(Message::Text(text))) => {
                    let msg: Value = serde_json::from_str(&text).context("Bad CDP response")?;
                    // Some endpoints echo the id back as a string: accept
                    // both shapes instead of burning the full timeout.
                    let matched = msg.get("id").is_some_and(|v| {
                        v.as_u64() == Some(id) || v.as_str() == Some(id.to_string().as_str())
                    });
                    if matched {
                        return Ok(msg);
                    }
                    // Ignore events and other method responses.
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(anyhow!("CDP stream error: {}", e)),
                None => return Err(anyhow!("Chrome DevTools closed the connection")),
            }
        }
    }
}

/// Main entry point for `kebabify import-cookies`.
pub async fn import_from_browser() -> Result<()> {
    let found = find_browser().context(
        "No supported browser found (Chrome, Edge, Brave, Vivaldi, Opera, Firefox, Zen, LibreWolf, Waterfox)",
    )?;
    match found.kind {
        BrowserKind::Chromium => import_chromium(&found.exe).await,
        BrowserKind::FirefoxFamily { label } => import_firefox(&found.exe, label).await,
    }
}

/// A located browser executable and how to talk to it.
struct FoundBrowser {
    exe: PathBuf,
    kind: BrowserKind,
}

#[derive(Clone, Copy)]
enum BrowserKind {
    /// DevTools Protocol (`Network.getCookies`).
    Chromium,
    /// Cookie store on disk (`cookies.sqlite`).
    FirefoxFamily { label: &'static str },
}

/// Chromium path for `import-cookies` (CDP flow).
async fn import_chromium(browser: &Path) -> Result<()> {
    let port = free_port().context("Could not reserve a debug port")?;
    let profile = temp_profile_dir()?;
    println!(
        "Opening a fresh Chrome window on lucida.to (port {} for cookies)...",
        port
    );
    println!("Solve the Cloudflare challenge in that window.");

    let mut child = launch_browser(browser, port, &profile)
        .with_context(|| format!("Failed to launch {}", browser.display()))?;

    let ws_url = wait_for_page(port).await;

    let result = async {
        let ws_url = ws_url.ok_or_else(|| {
            anyhow!("Chrome started but no lucida.to page appeared within {}s", CHROME_START_TIMEOUT.as_secs())
        })?;

        let mut cdp = Cdp::connect(&ws_url).await?;
        let (ua, cookie_header) = collect(&mut cdp).await?;
        lucida::save_cookies(&ua, &cookie_header)?;
        println!("Cookies stored in {}", lucida::cookies_file_path().display());
        println!("They will be replayed on all lucida.to requests.");

        // Flipped: the tab navigates to a confirmation page before Chrome is killed.
        let _ = cdp.call(
            "Page.navigate",
            json!({ "url": "data:text/html,%3Cbody%20style%3D%22font-family%3Asans-serif%3Bpadding%3A2rem%22%3E%3Ch1%3ECookies%20lucida%20import%C3%A9es%20%E2%9C%85%3C%2Fh1%3E%3Cp%3Ekebabify%20les%20r%C3%A9utilise%20pour%20le%20FLAC.%20Tu%20peux%20fermer%20cette%20fen%C3%AAtre.%3C%2Fp%3E%3C%2Fbody%3E" }),
        ).await;
        Ok(())
    }
    .await;

    // Give the user a beat to read the confirmation page before Chrome dies.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&profile);

    result
}

/// Firefox-family path for `import-cookies`: no CDP here, so a throwaway
/// profile is opened on lucida.to and its `cookies.sqlite` is polled until
/// the challenge cookies land.
async fn import_firefox(exe: &Path, label: &str) -> Result<()> {
    let profile = temp_profile_dir()?;
    println!(
        "Opening a fresh {} window on lucida.to (throwaway profile)...",
        label
    );
    println!("Solve the Cloudflare challenge in that window, then come back here.");

    let mut child = std::process::Command::new(exe)
        .arg("-profile")
        .arg(&profile)
        .arg("--no-remote")
        .arg("https://lucida.to")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to launch {}", exe.display()))?;

    // Cloudflare pins cf_clearance to the solving browser's UA: rebuild this
    // binary's UA from its application.ini so the replay matches exactly.
    let ua = firefox_user_agent(exe).with_context(|| {
        format!(
            "Could not determine the {} version (needed for its User-Agent)",
            label
        )
    })?;

    let result = async {
        let header = poll_firefox_cookies(&profile).await?;
        lucida::save_cookies(&ua, &header)?;
        println!(
            "Cookies stored in {}",
            lucida::cookies_file_path().display()
        );
        println!("They will be replayed on all lucida.to requests.");
        Ok(())
    }
    .await;

    tokio::time::sleep(Duration::from_secs(4)).await;
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&profile);

    result
}

/// Polls the throwaway profile's cookie store until `cf_clearance` appears.
/// Transient store hiccups (AV scan, WAL checkpoint) skip the tick instead
/// of aborting the whole 10-minute wait.
async fn poll_firefox_cookies(profile: &Path) -> Result<String> {
    let deadline = std::time::Instant::now() + CHALLENGE_TIMEOUT;
    while std::time::Instant::now() < deadline {
        // Absent or transiently unreadable: keep polling. Only the deadline
        // aborts, never a single bad tick.
        if let Ok(Some(header)) = read_firefox_cookies(profile) {
            return Ok(header);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Err(anyhow!(
        "Timeout after {}s — the Cloudflare challenge was not completed in the browser window",
        CHALLENGE_TIMEOUT.as_secs()
    ))
}

/// Copies the profile's `cookies.sqlite` aside (the live file may be
/// locked/WAL-mode) and returns the `Cookie` header value when it holds
/// `cf_clearance`. `Ok(None)` = not there yet.
fn read_firefox_cookies(profile: &Path) -> Result<Option<String>> {
    let src = profile.join("cookies.sqlite");
    if !src.exists() {
        return Ok(None);
    }
    let scratch = profile.join("kebabify-cookies-copy.sqlite");
    std::fs::copy(&src, &scratch).context("Failed to copy cookies.sqlite")?;
    for suffix in ["-wal", "-shm"] {
        let extra = profile.join(format!("cookies.sqlite{}", suffix));
        if extra.exists() {
            let _ = std::fs::copy(
                &extra,
                profile.join(format!("kebabify-cookies-copy.sqlite{}", suffix)),
            );
        }
    }
    let out = query_cookie_header(&scratch);
    let _ = std::fs::remove_file(&scratch);
    out
}

fn query_cookie_header(db: &Path) -> Result<Option<String>> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("Failed to open cookies copy")?;
    let mut stmt = conn
        .prepare(
            "SELECT name, value FROM moz_cookies WHERE host LIKE '%lucida.to' AND name IN ('cf_clearance', '__cf_bm', '__cfruid')",
        )
        .context("Failed to query cookies")?;
    let mut rows = stmt.query([]).context("Failed to read cookies")?;
    let mut pairs = Vec::new();
    while let Some(row) = rows.next().context("Failed to read cookie row")? {
        let name: String = row.get(0).context("Bad cookie row")?;
        let value: String = row.get(1).context("Bad cookie row")?;
        pairs.push((name, value));
    }
    if !pairs.iter().any(|(n, _)| n == "cf_clearance") {
        return Ok(None);
    }
    Ok(Some(
        pairs
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("; "),
    ))
}

/// Rebuilds this Firefox-family binary's User-Agent from its
/// `application.ini` (`Version=` under `[App]`), e.g.
/// `Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0`.
/// Pure parser tested below; file IO stays in the caller.
fn firefox_user_agent(exe: &Path) -> Result<String> {
    let dir = exe.parent().context("Browser path has no parent dir")?;
    let ini = std::fs::read_to_string(dir.join("application.ini"))
        .context("Failed to read application.ini next to the browser")?;
    firefox_ua_from_ini(&ini).ok_or_else(|| anyhow!("No Version= found in application.ini"))
}

fn firefox_ua_from_ini(ini: &str) -> Option<String> {
    let mut in_app = false;
    for line in ini.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_app = t.eq_ignore_ascii_case("[App]");
            continue;
        }
        if in_app {
            if let Some(v) = t.strip_prefix("Version=").map(str::trim) {
                if !v.is_empty() {
                    return Some(format!(
                        "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:{}) Gecko/20100101 Firefox/{}",
                        v, v
                    ));
                }
            }
        }
    }
    None
}

/// Opens the CDP page, polls for the Cloudflare cookies, returns (UA, Cookie).
async fn collect(cdp: &mut Cdp) -> Result<(String, String)> {
    let urls = json!({ "urls": [
        "https://lucida.to/",
        "https://www.lucida.to/",
        "https://lucida.to",
        "https://www.lucida.to"
    ] });

    let deadline = std::time::Instant::now() + CHALLENGE_TIMEOUT;
    while std::time::Instant::now() < deadline {
        let resp = cdp.call("Network.getCookies", urls.clone()).await?;
        let cookies = response_cookies(&resp)?;
        // Require cf_clearance specifically: __cf_bm/__cfruid alone show up
        // mid-challenge and are NOT enough for the lucida API — returning
        // early on them fakes a success that still 403s every track.
        if cookies.iter().any(|c| c.0 == "cf_clearance") {
            let header = cookies
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join("; ");
            let ua = user_agent_of(cdp).await?;
            return Ok((ua, header));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Err(anyhow!(
        "Timeout after {}s — the Cloudflare challenge was not completed in the Chrome window",
        CHALLENGE_TIMEOUT.as_secs()
    ))
}

fn response_cookies(resp: &Value) -> Result<Vec<(String, String)>> {
    let Some(list) = resp
        .get("result")
        .and_then(|r| r.get("cookies"))
        .and_then(Value::as_array)
    else {
        return Err(anyhow!("Unexpected CDP getCookies response: {}", resp));
    };
    let mut out = Vec::new();
    for c in list {
        if let (Some(name), Some(value)) = (
            c.get("name").and_then(Value::as_str),
            c.get("value").and_then(Value::as_str),
        ) {
            if name == "cf_clearance" || name == "__cf_bm" || name == "__cfruid" {
                out.push((name.to_string(), value.to_string()));
            }
        }
    }
    Ok(out)
}

async fn user_agent_of(cdp: &mut Cdp) -> Result<String> {
    let resp = cdp
        .call(
            "Runtime.evaluate",
            json!({ "expression": "navigator.userAgent", "returnByValue": true }),
        )
        .await?;
    resp.get("result")
        .and_then(|r| r.get("result"))
        .and_then(|r| r.get("value"))
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("Could not read the User-Agent from Chrome"))
}

/// Polls the CDP HTTP endpoint until a page target opens, returns its ws URL.
async fn wait_for_page(port: u16) -> Option<String> {
    let list_url = format!("http://127.0.0.1:{}/json", port);
    let deadline = std::time::Instant::now() + CHROME_START_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = SHARED_CLIENT
            .get(&list_url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            if let Ok(targets) = resp.json::<Value>().await {
                if let Some(list) = targets.as_array() {
                    // Two passes: an exact lucida.to tab first, a blank/new tab
                    // only as fallback. Grabbing the blank tab when the lucida
                    // tab exists would still work (cookies are filtered by URL,
                    // UA is browser-wide), but the exact tab is unambiguous.
                    for pass_exact in [true, false] {
                        for t in list {
                            let is_page = t.get("type").and_then(Value::as_str) == Some("page");
                            let url = t.get("url").and_then(Value::as_str).unwrap_or("");
                            let matches =
                                url.contains("lucida.to") || (!pass_exact && url.is_empty());
                            if is_page && matches {
                                if let Some(ws) =
                                    t.get("webSocketDebuggerUrl").and_then(Value::as_str)
                                {
                                    return Some(ws.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    None
}

/// Known install layouts: (relative path segments, exe file, kind).
/// `None` env vars are skipped; the first existing layout wins, Chromium
/// before Firefox-family (CDP is smoother than sqlite polling).
fn known_browser_layouts() -> Vec<(Vec<&'static str>, &'static str, BrowserKind)> {
    use BrowserKind::*;
    vec![
        // Chromium family (CDP).
        (
            vec!["Google", "Chrome", "Application"],
            "chrome.exe",
            Chromium,
        ),
        (
            vec!["Microsoft", "Edge", "Application"],
            "msedge.exe",
            Chromium,
        ),
        (
            vec!["BraveSoftware", "Brave-Browser", "Application"],
            "brave.exe",
            Chromium,
        ),
        (vec!["Vivaldi", "Application"], "vivaldi.exe", Chromium),
        (vec!["Programs", "Opera"], "opera.exe", Chromium),
        (vec!["Programs", "Opera GX"], "opera.exe", Chromium),
        (vec!["Arc", "Application"], "arc.exe", Chromium),
        // Firefox family (cookies.sqlite).
        (
            vec!["Mozilla Firefox"],
            "firefox.exe",
            FirefoxFamily { label: "Firefox" },
        ),
        (
            vec!["Zen Browser"],
            "zen.exe",
            FirefoxFamily { label: "Zen" },
        ),
        (
            vec!["LibreWolf"],
            "librewolf.exe",
            FirefoxFamily { label: "LibreWolf" },
        ),
        (
            vec!["Waterfox"],
            "waterfox.exe",
            FirefoxFamily { label: "Waterfox" },
        ),
    ]
}

fn find_browser() -> Option<FoundBrowser> {
    for var in [
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
        "LOCALAPPDATA",
    ] {
        let Some(root) = std::env::var_os(var) else {
            continue;
        };
        let root = PathBuf::from(root);
        for (segments, exe, kind) in known_browser_layouts() {
            let mut path = root.clone();
            for s in segments {
                path = path.join(s);
            }
            let path = path.join(exe);
            if path.exists() {
                return Some(FoundBrowser { exe: path, kind });
            }
        }
    }
    if let Some(exe) = find_browser_registry() {
        return Some(exe);
    }
    // PATH fallback, Chromium first for the same CDP-first reason.
    for name in [
        "chrome",
        "chromium",
        "msedge",
        "brave",
        "vivaldi",
        "opera",
        "arc",
        "firefox",
        "zen",
        "librewolf",
        "waterfox",
    ] {
        if let Ok(path) = which::which(name) {
            return Some(FoundBrowser {
                kind: classify_exe(&path),
                exe: path,
            });
        }
    }
    None
}

/// Best-effort kind from an exe file name (registry/PATH hits).
fn classify_exe(path: &Path) -> BrowserKind {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();
    if name.contains("firefox") {
        BrowserKind::FirefoxFamily { label: "Firefox" }
    } else if name.contains("zen") {
        BrowserKind::FirefoxFamily { label: "Zen" }
    } else if name.contains("librewolf") {
        BrowserKind::FirefoxFamily { label: "LibreWolf" }
    } else if name.contains("waterfox") {
        BrowserKind::FirefoxFamily { label: "Waterfox" }
    } else {
        BrowserKind::Chromium
    }
}

/// Windows: read the canonical path from the `App Paths` registry keys, which
/// browsers register when installed. Classified by exe name so Chromium forks
/// (Helium…) and Firefox-family browsers resolve to the right import flow.
#[cfg(target_os = "windows")]
fn find_browser_registry() -> Option<FoundBrowser> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ};

    const SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\App Paths";

    for root in [
        winreg::RegKey::predef(HKEY_CURRENT_USER),
        winreg::RegKey::predef(HKEY_LOCAL_MACHINE),
    ] {
        let Ok(app_paths) = root.open_subkey_with_flags(SUBKEY, KEY_READ) else {
            continue;
        };
        for exe in [
            "chrome.exe",
            "msedge.exe",
            "brave.exe",
            "vivaldi.exe",
            "opera.exe",
            "arc.exe",
            "firefox.exe",
            "zen.exe",
            "librewolf.exe",
            "waterfox.exe",
        ] {
            let Ok(key) = app_paths.open_subkey_with_flags(exe, KEY_READ) else {
                continue;
            };
            let Ok(path) = key.get_value::<String, _>("") else {
                continue;
            };
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(FoundBrowser {
                    kind: classify_exe(&path),
                    exe: path,
                });
            }
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
fn find_browser_registry() -> Option<FoundBrowser> {
    None
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .context("Failed to bind a loopback port for browser debugging")?;
    listener
        .local_addr()
        .context("Failed to read the bound debug port")
        .map(|a| a.port())
}

fn temp_profile_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "kebabify-cdp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create temp profile at {}", dir.display()))?;
    Ok(dir)
}

fn launch_browser(browser: &Path, port: u16, profile: &Path) -> std::io::Result<Child> {
    Command::new(browser)
        .arg(format!("--remote-debugging-port={}", port))
        .arg(format!("--user-data-dir={}", profile.display()))
        // Least privilege: only our own loopback origin may talk to this
        // debugging endpoint — never `*`, which would let any local page in.
        .arg(format!("--remote-allow-origins=http://127.0.0.1:{}", port))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-session-crashed-bubble")
        .arg("https://lucida.to")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cdp_cookies(names: &[(&str, &str)]) -> Value {
        json!({
            "id": 1,
            "result": {
                "cookies": names
                    .iter()
                    .map(|(n, v)| json!({"name": n, "value": v}))
                    .collect::<Vec<_>>(),
            },
        })
    }

    #[test]
    fn keeps_only_cloudflare_cookies() {
        let resp = cdp_cookies(&[
            ("cf_clearance", "abc"),
            ("sessionid", "drop-me"),
            ("__cf_bm", "def"),
            ("__cfruid", "ghi"),
            ("tracking", "drop-me-too"),
        ]);
        let got = response_cookies(&resp).unwrap();
        assert_eq!(
            got,
            vec![
                ("cf_clearance".to_string(), "abc".to_string()),
                ("__cf_bm".to_string(), "def".to_string()),
                ("__cfruid".to_string(), "ghi".to_string()),
            ]
        );
    }

    #[test]
    fn empty_cookie_list_is_ok() {
        let got = response_cookies(&cdp_cookies(&[])).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn malformed_cdp_response_is_error() {
        assert!(response_cookies(&json!({"id": 1})).is_err());
        assert!(response_cookies(&json!({"result": {}})).is_err());
        assert!(response_cookies(&json!({"result": {"cookies": "nope"}})).is_err());
    }

    #[test]
    fn exe_names_classified() {
        assert!(matches!(
            classify_exe(&PathBuf::from("C:\\x\\chrome.exe")),
            BrowserKind::Chromium
        ));
        assert!(matches!(
            classify_exe(&PathBuf::from("C:\\x\\brave.exe")),
            BrowserKind::Chromium
        ));
        assert!(matches!(
            classify_exe(&PathBuf::from("C:\\x\\firefox.exe")),
            BrowserKind::FirefoxFamily { label: "Firefox" }
        ));
        assert!(matches!(
            classify_exe(&PathBuf::from("C:\\x\\zen.exe")),
            BrowserKind::FirefoxFamily { label: "Zen" }
        ));
    }

    #[test]
    fn firefox_ua_built_from_ini() {
        let ini = "[App]\nVendor=Mozilla\nVersion=140.0\nBuildID=20260601\n";
        assert_eq!(
            firefox_ua_from_ini(ini).as_deref(),
            Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0"
            )
        );
        assert_eq!(firefox_ua_from_ini("[App]\nVendor=x\n"), None);
        assert_eq!(firefox_ua_from_ini(""), None);
    }

    #[test]
    fn firefox_cookies_filtered_like_cdp() {
        let dir = std::env::temp_dir().join(format!("kebabify_sqlite_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("cookies.sqlite");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE moz_cookies(name TEXT, value TEXT, host TEXT);
                 INSERT INTO moz_cookies VALUES
                   ('cf_clearance', 'abc', '.lucida.to'),
                   ('sessionid', 'drop', '.lucida.to'),
                   ('__cf_bm', 'def', '.lucida.to'),
                   ('cf_clearance', 'other', '.example.com');",
            )
            .unwrap();
        }
        let header = query_cookie_header(&db).unwrap().unwrap();
        assert!(header.contains("cf_clearance=abc"));
        assert!(header.contains("__cf_bm=def"));
        assert!(!header.contains("sessionid"));
        assert!(!header.contains("other"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn firefox_cookies_missing_clearance_is_none() {
        let dir =
            std::env::temp_dir().join(format!("kebabify_sqlite_test2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("cookies.sqlite");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE moz_cookies(name TEXT, value TEXT, host TEXT);
                 INSERT INTO moz_cookies VALUES ('__cf_bm', 'def', '.lucida.to');",
            )
            .unwrap();
        }
        assert_eq!(query_cookie_header(&db).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

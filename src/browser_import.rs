//! One-command Cloudflare cookie import for lucida.to.
//!
//! Spawns Chrome with a throwaway profile pointed at lucida.to, waits for the
//! user to solve the Cloudflare challenge in that window, then reads the
//! resulting cookies straight from the browser through the Chrome DevTools
//! Protocol (`Network.getCookies`) and stores them with `lucida::save_cookies`.
//! No copy/paste, no console spelunking. The User-Agent of the same tab is
//! captured too, because Cloudflare pins `cf_clearance` to it.

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
            .send(Message::Text(payload.to_string()))
            .await
            .context("CDP send failed")?;
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    let msg: Value = serde_json::from_str(&text).context("Bad CDP response")?;
                    if msg.get("id").and_then(Value::as_u64) == Some(id) {
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
    let browser = find_browser().context("No Chrome or Edge installation found")?;
    let port = free_port().context("Could not reserve a debug port")?;
    let profile = temp_profile_dir();
    println!(
        "Opening a fresh Chrome window on lucida.to (port {} for cookies)...",
        port
    );
    println!("Solve the Cloudflare challenge in that window.");

    let mut child = launch_browser(&browser, port, &profile)
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
        if cookies.iter().any(|c| c.0 == "cf_clearance") || !cookies.is_empty() {
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
    let client = reqwest::Client::new();
    let list_url = format!("http://127.0.0.1:{}/json", port);
    let deadline = std::time::Instant::now() + CHROME_START_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = client
            .get(&list_url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            if let Ok(targets) = resp.json::<Value>().await {
                if let Some(list) = targets.as_array() {
                    for t in list {
                        let is_page = t.get("type").and_then(Value::as_str) == Some("page");
                        let url = t.get("url").and_then(Value::as_str).unwrap_or("");
                        let is_lucida = url.contains("lucida.to") || url.is_empty();
                        if is_page && is_lucida {
                            if let Some(ws) = t.get("webSocketDebuggerUrl").and_then(Value::as_str)
                            {
                                return Some(ws.to_string());
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

fn find_browser() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for var in [
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
        "LOCALAPPDATA",
    ] {
        if let Some(dir) = std::env::var_os(var) {
            let dir = PathBuf::from(dir);
            candidates.push(
                dir.join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe"),
            );
            candidates.push(
                dir.join("Microsoft")
                    .join("Edge")
                    .join("Application")
                    .join("msedge.exe"),
            );
        }
    }
    candidates
        .into_iter()
        .find(|p| p.exists())
        .or_else(find_browser_registry)
        .or_else(|| which::which("chrome.exe").ok())
        .or_else(|| which::which("msedge.exe").ok())
}

/// Windows: read the canonical path from the `App Paths` registry keys, which
/// browsers register when installed (covers Chromium forks like Helium).
#[cfg(target_os = "windows")]
fn find_browser_registry() -> Option<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ};

    const SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\App Paths";

    for root in [
        winreg::RegKey::predef(HKEY_CURRENT_USER),
        winreg::RegKey::predef(HKEY_LOCAL_MACHINE),
    ] {
        let Ok(app_paths) = root.open_subkey_with_flags(SUBKEY, KEY_READ) else {
            continue;
        };
        for exe in ["chrome.exe", "msedge.exe"] {
            let Ok(key) = app_paths.open_subkey_with_flags(exe, KEY_READ) else {
                continue;
            };
            let Ok(path) = key.get_value::<String, _>("") else {
                continue;
            };
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
fn find_browser_registry() -> Option<PathBuf> {
    None
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn temp_profile_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kebabify-cdp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn launch_browser(browser: &Path, port: u16, profile: &Path) -> std::io::Result<Child> {
    Command::new(browser)
        .arg(format!("--remote-debugging-port={}", port))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--remote-allow-origins=*")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-session-crashed-bubble")
        .arg("https://lucida.to")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

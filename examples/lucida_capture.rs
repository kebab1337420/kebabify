use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Cdp {
    ws: Ws,
    next_id: u64,
}

impl Cdp {
    async fn connect(url: &str) -> Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(url).await?;
        Ok(Cdp { ws, next_id: 1 })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({"id": id, "method": method, "params": params});
        self.ws.send(Message::Text(payload.to_string())).await?;
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(txt))) => {
                    let v: Value = serde_json::from_str(&txt)?;
                    if v.get("id").and_then(Value::as_u64) == Some(id) {
                        return Ok(v);
                    }
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(anyhow!("ws error: {e}")),
                None => return Err(anyhow!("ws closed")),
            }
        }
    }
}

fn find_browser() -> Result<PathBuf> {
    for var in [
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
        "LOCALAPPDATA",
    ] {
        if let Some(dir) = std::env::var_os(var) {
            let dir = PathBuf::from(dir);
            for p in [
                dir.join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe"),
                dir.join("Microsoft")
                    .join("Edge")
                    .join("Application")
                    .join("msedge.exe"),
            ] {
                if p.exists() {
                    return Ok(p);
                }
            }
        }
    }
    which::which("chrome.exe")
        .or_else(|_| which::which("chrome"))
        .or_else(|_| which::which("msedge.exe"))
        .or_else(|_| which::which("msedge"))
        .or_else(|_| which::which("chromium"))
        .context("no Chromium browser found")
}

fn cookies_file() -> PathBuf {
    for var in [
        "KEBABIFY_COOKIES_PATH",
        "KEBACCIFY_COOKIES_PATH",
        "KEBACCFIY_COOKIES_PATH",
    ] {
        if let Some(p) = std::env::var_os(var) {
            return PathBuf::from(p);
        }
    }
    std::env::var("APPDATA")
        .ok()
        .map(|a| PathBuf::from(a).join("Kebabify").join("cookies.txt"))
        .unwrap_or(PathBuf::from("cookies.txt"))
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

fn launch_browser(browser: &PathBuf, port: u16, url: &str) -> Result<Child> {
    Ok(Command::new(browser)
        .args([
            &format!("--remote-debugging-port={port}"),
            &format!(
                "--user-data-dir={}",
                std::env::temp_dir().join("kebabify-cap").display()
            ),
            "--remote-allow-origins=*",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-session-crashed-bubble",
            url,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?)
}

async fn wait_for_page(port: u16) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let client = reqwest::Client::new();
    while Instant::now() < deadline {
        if let Ok(r) = client
            .get(format!("http://127.0.0.1:{port}/json"))
            .send()
            .await
        {
            if let Ok(v) = r.json::<Value>().await {
                if let Some(pages) = v.as_array() {
                    for p in pages {
                        let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
                        if ty == "page" {
                            if let Some(ws) = p.get("webSocketDebuggerUrl").and_then(Value::as_str)
                            {
                                return Ok(ws.to_string());
                            }
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    Err(anyhow!("CDP page endpoint timed out"))
}

async fn capture(cdp: &mut Cdp, secs: u64) -> Result<()> {
    cdp.call("Network.enable", json!({})).await?;
    cdp.call("Page.enable", json!({})).await?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        // Network/Page domains are already enabled above — just drain events.
        // read events for 500ms
        let ev_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < ev_deadline {
            match tokio::time::timeout(Duration::from_millis(300), cdp.ws.next()).await {
                Ok(Some(Ok(Message::Text(txt)))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                        let method = v.get("method").and_then(Value::as_str).unwrap_or("");
                        if method == "Network.requestWillBeSent" {
                            if let Some(params) = v.get("params") {
                                if let Some(req) = params.get("request") {
                                    let url = req.get("url").and_then(Value::as_str).unwrap_or("");
                                    if url.contains("lucida") || url.contains("api") {
                                        println!(
                                            ">> REQ {} {}",
                                            req.get("method").and_then(Value::as_str).unwrap_or(""),
                                            url
                                        );
                                    }
                                }
                            }
                        } else if method == "Network.responseReceived" {
                            if let Some(params) = v.get("params") {
                                let url = params
                                    .pointer("/response/url")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if url.contains("lucida") || url.contains("api") {
                                    let status = params
                                        .pointer("/response/status")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(0);
                                    println!("<< RESP {status} {url}");
                                }
                            }
                        } else if method == "Page.loadEventFired" {
                            println!("-- loadEventFired");
                        } else if method == "Page.frameNavigated" {
                            if let Some(p) = v.pointer("/params/frame/url") {
                                println!("-- NAVIGATED {}", p.as_str().unwrap_or(""));
                            }
                        }
                    }
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(e))) => return Err(anyhow!("ws error: {e}")),
                Ok(None) => return Err(anyhow!("ws closed")),
                Err(_elapsed) => break,
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let url = "https://lucida.to/https%3A%2F%2Fopen.spotify.com%2Ftrack%2F11dFghVXANMlKmJXsNCbNl";
    let browser = find_browser()?;
    println!("using browser: {}", browser.display());
    let port = free_port()?;
    let mut child = launch_browser(&browser, port, "about:blank")?;
    let ws_url = wait_for_page(port).await?;
    println!("cdp: {ws_url}");
    let mut cdp = Cdp::connect(&ws_url).await?;

    // inject cookies
    let cookie_line = std::fs::read_to_string(cookies_file())?;
    let lines: Vec<&str> = cookie_line.lines().collect();
    let cookie = lines.last().copied().unwrap_or("").trim();
    for c in cookie.split(';') {
        let c = c.trim();
        if c.is_empty() {
            continue;
        }
        let mut it = c.splitn(2, '=');
        let name = it.next().unwrap_or("");
        let value = it.next().unwrap_or("");
        cdp.call(
            "Network.setCookie",
            json!({"name": name, "value": value, "domain": ".lucida.to", "url": "https://lucida.to/"}),
        )
        .await?;
    }
    println!("cookies injected");

    cdp.call("Network.enable", json!({})).await?;
    cdp.call("Page.enable", json!({})).await?;
    cdp.call("Page.navigate", json!({"url": url})).await?;
    println!("-- navigated to resolve URL, capturing 25s…");
    capture(&mut cdp, 25).await?;

    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

//! Canned HTTP/1.1 server for offline integration tests (lucida/saavn flows).
//!
//! Routes match in order by path prefix (+ optional query substring); each
//! hit pops the next canned response, the last one repeating when exhausted
//! (handy for poll sequences). Byte routes can honor `Range` with `206`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub struct Route {
    prefix: String,
    contains: Option<String>,
    suffix: Option<String>,
    responses: VecDeque<(u16, Vec<u8>)>,
    ranged: bool,
}

impl Route {
    pub fn new(prefix: &str, responses: Vec<(u16, Vec<u8>)>) -> Self {
        Self {
            prefix: prefix.to_string(),
            contains: None,
            suffix: None,
            responses: responses.into(),
            ranged: false,
        }
    }

    pub fn containing(mut self, needle: &str) -> Self {
        self.contains = Some(needle.to_string());
        self
    }

    /// Only match paths ending with `suffix` (e.g. `/download` under a
    /// `/status/` prefix).
    pub fn ending_with(mut self, suffix: &str) -> Self {
        self.suffix = Some(suffix.to_string());
        self
    }

    pub fn catch_all(status: u16, body: Vec<u8>) -> Self {
        Self {
            prefix: String::new(),
            contains: None,
            suffix: None,
            responses: VecDeque::from(vec![(status, body)]),
            ranged: false,
        }
    }

    pub fn ranged(mut self) -> Self {
        self.ranged = true;
        self
    }
}

pub struct MockServer {
    pub base_url: String,
}

impl MockServer {
    pub async fn start(routes: Vec<Route>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock bind");
        let port = listener.local_addr().expect("mock port").port();
        Self::serve_on(listener, routes);
        Self {
            base_url: format!("http://127.0.0.1:{}", port),
        }
    }

    /// Start on a pre-reserved port (for fixtures that must embed the URL,
    /// e.g. an encrypted CDN link pointing back at the mock).
    pub async fn start_on(port: u16, routes: Vec<Route>) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("mock bind on reserved port");
        Self::serve_on(listener, routes);
        Self {
            base_url: format!("http://127.0.0.1:{}", port),
        }
    }

    fn serve_on(listener: tokio::net::TcpListener, routes: Vec<Route>) {
        let routes = Arc::new(Mutex::new(routes));
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    serve_one(sock, &routes).await;
                });
            }
        });
    }

    /// Reserves a loopback port without listening (caller passes it to
    /// [`MockServer::start_on`]); tiny reuse race, localhost-only tests.
    pub async fn reserve_port() -> u16 {
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve port")
            .local_addr()
            .expect("reserved port")
            .port()
    }
}

async fn serve_one(sock: tokio::net::TcpStream, routes: &Arc<Mutex<Vec<Route>>>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (rh, mut wh) = sock.into_split();
    let mut reader = tokio::io::BufReader::new(rh);
    let mut head = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut line),
        )
        .await
        {
            Ok(Ok(n)) => n,
            _ => return,
        };
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        head.push_str(&line);
        if head.len() > 16384 {
            return;
        }
    }

    let path = head
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let range = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("range") {
                Some(v.trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default();

    let (status, body, ranged) = {
        let mut routes = routes.lock().expect("mock routes");
        let hit = routes.iter_mut().find(|r| {
            path.starts_with(&r.prefix)
                && r.contains.as_ref().is_none_or(|c| path.contains(c))
                && r.suffix.as_ref().is_none_or(|s| path.ends_with(s))
        });
        match hit {
            Some(route) => {
                let resp = if route.responses.len() > 1 {
                    route.responses.pop_front().expect("mock response")
                } else {
                    route
                        .responses
                        .front()
                        .cloned()
                        .unwrap_or((404, b"mock: out of responses".to_vec()))
                };
                (resp.0, resp.1, route.ranged)
            }
            None => (404u16, b"mock: no route".to_vec(), false),
        }
    };

    let (status, body, content_range) = if ranged {
        match parse_range(&range, body.len()) {
            Some((a, b)) => (
                206u16,
                body[a..=b].to_vec(),
                format!("bytes {}-{}/{}", a, b, body.len()),
            ),
            None => (200u16, body, String::new()),
        }
    } else {
        (status, body, String::new())
    };

    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        404 => "Not Found",
        _ => "Error",
    };
    let mut resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
        status,
        reason,
        body.len()
    );
    if !content_range.is_empty() {
        resp.push_str(&format!("Content-Range: {}\r\n", content_range));
    }
    resp.push_str("\r\n");
    let _ = wh.write_all(resp.as_bytes()).await;
    let _ = wh.write_all(&body).await;
    let _ = wh.flush().await;
}

/// Parses `bytes=A-B` / `bytes=A-` against `total`. Pure for tests.
fn parse_range(header: &str, total: usize) -> Option<(usize, usize)> {
    let spec = header.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let a: usize = a.parse().ok()?;
    let b: usize = if b.is_empty() {
        total.saturating_sub(1)
    } else {
        b.parse().ok()?
    };
    if a <= b && b < total {
        Some((a, b))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_slicing() {
        assert_eq!(parse_range("bytes=0-3", 10), Some((0, 3)));
        assert_eq!(parse_range("bytes=5-", 10), Some((5, 9)));
        assert_eq!(parse_range("bytes=0-99", 10), None);
        assert_eq!(parse_range("bytes=9-3", 10), None);
        assert_eq!(parse_range("items=0-3", 10), None);
        assert_eq!(parse_range("", 10), None);
    }
}

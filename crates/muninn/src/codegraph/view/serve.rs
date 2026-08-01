//! A localhost server for the code view.
//!
//! Hand-rolled HTTP/1.1 over `tokio::net::TcpListener`. The crate already
//! carries tokio with `features = ["full"]`, so this costs no new dependency;
//! pulling in a framework would mean enabling `hyper/server` (currently off —
//! reqwest only asks for the client) plus tower, against a binary whose build
//! profile is tuned for size. The MCP server already hand-rolls JSON-RPC over
//! stdio, so this matches the house pattern.
//!
//! # Threat model
//!
//! A local server is reachable by every page the user visits, so "it's only on
//! localhost" is not a control. Exposed local debug endpoints are a recurring
//! incident class, not a hypothesis. Four things guard this one:
//!
//! 1. **Loopback only.** Bound to `127.0.0.1`, never `0.0.0.0`, so nothing off
//!    the machine can reach it at all.
//! 2. **An unguessable path prefix.** Every URL carries a 128-bit token from a
//!    CSPRNG. A page that guesses the port still cannot guess the path.
//! 3. **`Host` header validation.** DNS rebinding lets an attacker's domain
//!    resolve to 127.0.0.1 and defeats the same-origin policy; the browser
//!    still sends the attacker's hostname in `Host`, so rejecting anything but
//!    loopback names closes it.
//! 4. **Read-only and GET-only.** The store is opened read-only and no route
//!    mutates anything. No CORS headers are emitted, so a foreign origin that
//!    somehow issued a request still cannot read the reply.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::super::store::CodeGraphStore;

/// A request whose headers exceed this is refused rather than buffered. The
/// only legitimate requests here are short GETs.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Long enough that a stalled peer cannot hold a task open indefinitely.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct Server {
    pub addr: SocketAddr,
    pub token: String,
    listener: TcpListener,
    store: Arc<CodeGraphStore>,
}

impl Server {
    /// Bind, but do not serve. Split from `serve` so a test can learn the port
    /// before any request is made; port 0 asks the OS for a free one.
    pub async fn bind(store: CodeGraphStore, port: u16) -> anyhow::Result<Self> {
        // Loopback literal, not "localhost": the name can resolve elsewhere.
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
        let addr = listener.local_addr()?;
        Ok(Self {
            addr,
            token: uuid::Uuid::new_v4().simple().to_string(),
            listener,
            store: Arc::new(store),
        })
    }

    /// The one URL worth printing: without the token nothing else resolves.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/{}/", self.addr.port(), self.token)
    }

    pub async fn serve(self) -> anyhow::Result<()> {
        loop {
            let (stream, _peer) = match self.listener.accept().await {
                Ok(v) => v,
                // One failed accept must not end the session.
                Err(e) => {
                    tracing::warn!(error = %e, "graph serve: accept failed");
                    continue;
                }
            };
            let store = Arc::clone(&self.store);
            let token = self.token.clone();
            let port = self.addr.port();
            tokio::spawn(async move {
                if let Err(e) = handle(stream, store, token, port).await {
                    tracing::debug!(error = %e, "graph serve: connection ended");
                }
            });
        }
    }
}

/// What a request line and headers boil down to for routing.
struct Req {
    method: String,
    path: String,
    query: String,
    host: String,
}

fn parse(raw: &str) -> Option<Req> {
    let mut lines = raw.split("\r\n");
    let mut start = lines.next()?.split(' ');
    let method = start.next()?.to_string();
    let target = start.next()?;

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut host = String::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("host") {
                host = v.trim().to_string();
            }
        }
    }
    Some(Req {
        method,
        path,
        query,
        host,
    })
}

/// Only loopback names. A DNS-rebinding page reaches 127.0.0.1 but still
/// announces its own hostname here, which is what makes this the check that
/// closes that hole.
fn host_is_loopback(host: &str, port: u16) -> bool {
    let expected = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    expected.iter().any(|e| e == host)
}

/// Length-checked, non-short-circuiting compare. A 128-bit token over loopback
/// is not realistically timing-attackable, but the cost of not inviting the
/// question is one loop.
fn token_matches(given: &str, token: &str) -> bool {
    if given.len() != token.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in given.bytes().zip(token.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

fn response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn text(status: &str, msg: &str) -> Vec<u8> {
    response(status, "text/plain; charset=utf-8", msg.as_bytes())
}

async fn handle(
    mut stream: TcpStream,
    store: Arc<CodeGraphStore>,
    token: String,
    port: u16,
) -> anyhow::Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    // Read only until the end of the headers. There is no body to consume:
    // every route is a GET.
    let head = loop {
        let n = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut chunk)).await??;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = find_headers_end(&buf) {
            break String::from_utf8_lossy(&buf[..end]).into_owned();
        }
        if buf.len() > MAX_HEADER_BYTES {
            stream
                .write_all(&text("431 Request Header Fields Too Large", "too large"))
                .await?;
            return Ok(());
        }
    };

    let Some(req) = parse(&head) else {
        stream
            .write_all(&text("400 Bad Request", "malformed request"))
            .await?;
        return Ok(());
    };

    if req.method != "GET" {
        stream
            .write_all(&text("405 Method Not Allowed", "read-only server"))
            .await?;
        return Ok(());
    }
    if !host_is_loopback(&req.host, port) {
        // Deliberately terse: naming the check would help an attacker tune it.
        stream
            .write_all(&text("403 Forbidden", "forbidden"))
            .await?;
        return Ok(());
    }

    // Everything lives under /<token>/. Anything else is a stranger.
    let rest = match req.path.strip_prefix('/') {
        Some(r) => r,
        None => {
            stream
                .write_all(&text("404 Not Found", "not found"))
                .await?;
            return Ok(());
        }
    };
    let (given, tail) = match rest.split_once('/') {
        Some((t, tail)) => (t, tail),
        None => (rest, ""),
    };
    if !token_matches(given, &token) {
        stream
            .write_all(&text("404 Not Found", "not found"))
            .await?;
        return Ok(());
    }
    // `/<token>` without the slash would make the page's relative fetches
    // resolve one level too high, so send the browser to the canonical form.
    if !rest.contains('/') {
        let body = format!(
            "HTTP/1.1 308 Permanent Redirect\r\nLocation: /{token}/\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(body.as_bytes()).await?;
        return Ok(());
    }

    let reply = match tail {
        "" | "index.html" => response(
            "200 OK",
            "text/html; charset=utf-8",
            super::page("runar graph", None).as_bytes(),
        ),

        "api/projects" => {
            let rows = tokio::task::spawn_blocking({
                let store = Arc::clone(&store);
                move || store.project_rows()
            })
            .await?;
            match rows {
                Ok(rows) => json_reply(&super::projects_json(&rows)),
                Err(e) => text("500 Internal Server Error", &format!("{e}")),
            }
        }

        "api/graph" => {
            let wanted = param(&req.query, "project").map(decode);
            // Blocking SQLite off the reactor: a 12k-symbol payload takes long
            // enough that doing it inline would stall every other connection.
            let built = tokio::task::spawn_blocking({
                let store = Arc::clone(&store);
                move || {
                    let project = match wanted {
                        Some(p) => p,
                        None => store
                            .project_rows()?
                            .into_iter()
                            .max_by_key(|p| p.symbols)
                            .map(|p| p.project)
                            .unwrap_or_default(),
                    };
                    super::payload(&store, &project)
                }
            })
            .await?;
            match built {
                Ok(v) => json_reply(&v),
                Err(e) => text("500 Internal Server Error", &format!("{e}")),
            }
        }

        _ => text("404 Not Found", "not found"),
    };

    stream.write_all(&reply).await?;
    stream.flush().await?;
    Ok(())
}

fn json_reply(v: &serde_json::Value) -> Vec<u8> {
    match serde_json::to_vec(v) {
        Ok(body) => response("200 OK", "application/json; charset=utf-8", &body),
        Err(e) => text("500 Internal Server Error", &format!("{e}")),
    }
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Best-effort: a browser that will not open is not a failure worth aborting on.
pub fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let cmd = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let cmd = ("cmd", vec!["/C", "start", "", url]);

    let _ = std::process::Command::new(cmd.0)
        .args(cmd.1)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_hosts_are_accepted() {
        assert!(host_is_loopback("127.0.0.1:7000", 7000));
        assert!(host_is_loopback("localhost:7000", 7000));
        assert!(host_is_loopback("[::1]:7000", 7000));
        // The DNS-rebinding case: resolves to loopback, announces itself.
        assert!(!host_is_loopback("evil.example.com:7000", 7000));
        // Right name, wrong port — a different server's origin.
        assert!(!host_is_loopback("127.0.0.1:7001", 7000));
        assert!(!host_is_loopback("", 7000));
    }

    #[test]
    fn token_compare_is_length_checked_and_exact() {
        assert!(token_matches("abc123", "abc123"));
        assert!(!token_matches("abc124", "abc123"));
        assert!(!token_matches("abc12", "abc123"));
        assert!(!token_matches("abc1234", "abc123"));
        assert!(!token_matches("", "abc123"));
    }

    #[test]
    fn a_request_line_and_host_are_parsed() {
        let raw = "GET /tok/api/graph?project=a%20b HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n";
        let r = parse(raw).unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/tok/api/graph");
        assert_eq!(r.host, "127.0.0.1:9");
        assert_eq!(decode(param(&r.query, "project").unwrap()), "a b");
        assert!(param(&r.query, "missing").is_none());
    }

    #[test]
    fn responses_carry_the_hardening_headers() {
        let r = String::from_utf8(text("200 OK", "hi")).unwrap();
        assert!(r.contains("Cache-Control: no-store"));
        assert!(r.contains("X-Content-Type-Options: nosniff"));
        // No CORS: a foreign origin must not be able to read a reply.
        assert!(!r.to_lowercase().contains("access-control-allow-origin"));
        assert!(r.contains("Content-Length: 2"));
    }

    /// The index returned is the START of the blank-line separator, so the
    /// slice `buf[..end]` is exactly the request line plus headers with no
    /// trailing CRLF — which is what `parse` splits on.
    #[test]
    fn header_end_is_the_start_of_the_separator() {
        assert_eq!(find_headers_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(14));
        assert!(find_headers_end(b"GET / HTTP/1.1\r\n").is_none());

        // and the slice it produces really does still carry the headers
        let raw = b"GET /t/ HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n";
        let end = find_headers_end(raw).unwrap();
        let head = String::from_utf8(raw[..end].to_vec()).unwrap();
        let r = parse(&head).unwrap();
        assert_eq!(
            r.host, "127.0.0.1:9",
            "Host was lost when the block was sliced"
        );
        assert_eq!(r.path, "/t/");
    }
}

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::config::Upstream;

const MAX_HEAD: usize = 64 * 1024;
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Clone, Debug, Default)]
pub struct Counters {
    pub active: Arc<AtomicU64>,
    pub total: Arc<AtomicU64>,
    pub failed: Arc<AtomicU64>,
}

/// A running local proxy: dropping/aborting `handle` stops accepting new work.
pub struct RunningProxy {
    pub addr: SocketAddr,
    pub handle: tokio::task::JoinHandle<()>,
    pub counters: Counters,
}

pub type LogSink = mpsc::UnboundedSender<String>;

fn log(sink: &LogSink, msg: impl Into<String>) {
    let _ = sink.send(msg.into());
}

/// Bind the local listener, then serve connections in a background task.
pub async fn spawn(
    bind: SocketAddr,
    upstream: Upstream,
    logs: LogSink,
) -> Result<RunningProxy, String> {
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|e| format!("failed to bind {bind}: {e}"))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let counters = Counters::default();
    let upstream = Arc::new(upstream);

    let task_counters = counters.clone();
    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let upstream = Arc::clone(&upstream);
                    let logs = logs.clone();
                    let counters = task_counters.clone();
                    tokio::spawn(async move {
                        counters.active.fetch_add(1, Ordering::Relaxed);
                        counters.total.fetch_add(1, Ordering::Relaxed);
                        if let Err(e) = handle_client(stream, peer, &upstream, &logs).await {
                            counters.failed.fetch_add(1, Ordering::Relaxed);
                            log(&logs, format!("[{peer}] error: {e}"));
                        }
                        counters.active.fetch_sub(1, Ordering::Relaxed);
                    });
                }
                Err(e) => {
                    log(&logs, format!("accept failed: {e}"));
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    });

    Ok(RunningProxy {
        addr,
        handle,
        counters,
    })
}

/// Read the request head (up to and including the blank line separator).
/// Returns (head, leftover bytes that already belong to the body/tunnel).
async fn read_head(stream: &mut TcpStream) -> Result<(String, Vec<u8>), String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| format!("failed to read request: {e}"))?;
        if n == 0 {
            return Err("client disconnected before sending request headers".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_head_end(&buf) {
            let leftover = buf.split_off(pos);
            let head = String::from_utf8_lossy(&buf).to_string();
            return Ok((head, leftover));
        }
        if buf.len() > MAX_HEAD {
            return Err("request headers too large".into());
        }
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Drop hop-by-hop proxy auth headers from the client and inject our own.
fn rewrite_head(head: &str, upstream: &Upstream) -> Result<(String, String), String> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    if request_line.is_empty() {
        return Err("empty request line".into());
    }
    let mut out = String::with_capacity(head.len() + 96);
    out.push_str(&request_line);
    out.push_str("\r\n");
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("proxy-authorization:") || lower.starts_with("proxy-connection:") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    if !upstream.user.is_empty() || !upstream.password.is_empty() {
        out.push_str(&upstream.basic_auth_header());
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    Ok((out, request_line))
}

async fn handle_client(
    mut client: TcpStream,
    peer: SocketAddr,
    upstream: &Upstream,
    logs: &LogSink,
) -> Result<(), String> {
    let (head, leftover) = read_head(&mut client).await?;
    let (mut rewritten, request_line) = rewrite_head(&head, upstream)?;
    let is_connect = request_line.to_ascii_uppercase().starts_with("CONNECT ");

    // Plain HTTP: force one request per connection so every request carries
    // the upstream auth header (we do not parse pipelined follow-up requests).
    if !is_connect {
        rewritten = force_close(&rewritten);
    }

    let target = upstream.addr();
    let mut server = tokio::time::timeout(UPSTREAM_TIMEOUT, TcpStream::connect(&target))
        .await
        .map_err(|_| format!("timed out connecting to upstream {target}"))?
        .map_err(|e| format!("failed to connect to upstream {target}: {e}"))?;
    let _ = client.set_nodelay(true);
    let _ = server.set_nodelay(true);

    server
        .write_all(rewritten.as_bytes())
        .await
        .map_err(|e| format!("failed to send request headers upstream: {e}"))?;
    if !leftover.is_empty() {
        server
            .write_all(&leftover)
            .await
            .map_err(|e| format!("failed to forward request body: {e}"))?;
    }
    server.flush().await.ok();

    let summary = request_line
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    log(logs, format!("[{peer}] {summary} -> {target}"));

    let (up, down) = tokio::io::copy_bidirectional(&mut client, &mut server)
        .await
        .map_err(|e| format!("tunnel broken: {e}"))?;
    log(logs, format!("[{peer}] closed ↑{up}B ↓{down}B"));
    Ok(())
}

/// Replace any Connection header with `Connection: close`.
fn force_close(head: &str) -> String {
    let mut out = String::with_capacity(head.len() + 20);
    for line in head.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        if line.to_ascii_lowercase().starts_with("connection:") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up() -> Upstream {
        Upstream {
            ip: "10.0.0.1".into(),
            port: 3128,
            user: "alice".into(),
            password: "s3cret".into(),
        }
    }

    #[test]
    fn head_end_detected() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(18));
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n"), None);
    }

    #[test]
    fn injects_auth_and_strips_client_proxy_headers() {
        let head = "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\
                    Proxy-Authorization: Basic bogus\r\nProxy-Connection: keep-alive\r\n\r\n";
        let (out, line) = rewrite_head(head, &up()).unwrap();
        assert_eq!(line, "CONNECT example.com:443 HTTP/1.1");
        assert!(out.contains("Proxy-Authorization: Basic YWxpY2U6czNjcmV0\r\n"));
        assert!(!out.contains("Basic bogus"));
        assert!(!out.to_ascii_lowercase().contains("proxy-connection"));
        assert!(out.ends_with("\r\n\r\n"));
    }

    #[test]
    fn skips_auth_when_no_credentials() {
        let mut u = up();
        u.user.clear();
        u.password.clear();
        let (out, _) = rewrite_head("GET http://a/ HTTP/1.1\r\nHost: a\r\n\r\n", &u).unwrap();
        assert!(!out.contains("Proxy-Authorization"));
    }

    #[test]
    fn force_close_replaces_connection_header() {
        let out = force_close("GET / HTTP/1.1\r\nConnection: keep-alive\r\nHost: a\r\n\r\n");
        assert!(out.contains("Connection: close\r\n"));
        assert_eq!(out.matches("Connection:").count(), 1);
    }
}

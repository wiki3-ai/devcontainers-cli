//! CONNECT-only forward proxy server.
//!
//! Accepts plain-text HTTP/1.1 `CONNECT host:port HTTP/1.1` requests
//! on a configured listener and bridges the resulting TCP tunnel to
//! the upstream. We deliberately do **not** support absolute-URI
//! plain HTTP forwarding (e.g. `GET http://foo/bar HTTP/1.1`):
//! everything modern uses HTTPS, and refusing the cleartext path
//! keeps the surface small and the bandwidth-accounting honest
//! (Phase 2 caching needs to see CONNECT volumes per host to know
//! where to invest).
//!
//! The listener should be bound on the host's bridge gateway IP
//! (`192.168.64.1` on Apple Containers) so dev containers can reach
//! it. Binding on `0.0.0.0` is also valid in development but exposes
//! the proxy to the LAN.

use crate::stats::ProxyStats;
use crate::ProxyStatsSnapshot;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tracing::{debug, warn};

/// Maximum bytes we'll read while parsing the request line + headers
/// from a client. Real CONNECT requests are typically <200 bytes;
/// 8 KiB is generous and keeps a buggy or hostile client from
/// exhausting memory.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// Time budget for receiving the request line from a freshly accepted
/// client. Keeps idle/half-open connections from piling up.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Time budget for the upstream TCP connect.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Configuration for [`ProxyServer::start`].
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Address to bind the listener on. Use `192.168.64.1:<port>` on
    /// macOS so containers can reach the proxy via the bridge.
    pub bind_addr: SocketAddr,
}

impl ProxyConfig {
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self { bind_addr }
    }
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("failed to bind proxy listener on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

/// Handle to a running proxy. Drop the handle (or call
/// [`ProxyHandle::shutdown`]) to stop the listener.
pub struct ProxyHandle {
    bound_addr: SocketAddr,
    stats: Arc<ProxyStats>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl ProxyHandle {
    /// The address the listener is actually bound on (with any
    /// ephemeral-port resolution applied).
    pub fn bound_addr(&self) -> SocketAddr {
        self.bound_addr
    }

    /// Proxy URL suitable for `HTTP_PROXY` / `HTTPS_PROXY`
    /// environment variables.
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.bound_addr)
    }

    /// Snapshot of current stats. Cheap; safe to poll on a UI timer.
    pub fn stats(&self) -> ProxyStatsSnapshot {
        self.stats.snapshot()
    }

    /// Stop the listener. The accept-loop task will terminate
    /// shortly. In-flight tunnels are not forcibly closed; they wind
    /// down naturally as their TCP connections close.
    pub fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Forward proxy server. Spawn one per process.
pub struct ProxyServer;

impl ProxyServer {
    /// Bind the listener and spawn the accept loop. Returns once the
    /// listener is bound; further work happens on background tasks.
    pub async fn start(config: ProxyConfig) -> Result<ProxyHandle, ProxyError> {
        let listener = TcpListener::bind(config.bind_addr)
            .await
            .map_err(|source| ProxyError::Bind {
                addr: config.bind_addr,
                source,
            })?;
        let bound_addr = listener.local_addr().unwrap_or(config.bind_addr);

        let stats = Arc::new(ProxyStats::default());
        stats.mark_started();
        let stats_for_loop = stats.clone();

        let (tx, mut rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut rx => {
                        debug!("proxy: shutdown signalled, exiting accept loop");
                        break;
                    }
                    res = listener.accept() => {
                        match res {
                            Ok((sock, peer)) => {
                                let stats = stats_for_loop.clone();
                                tokio::spawn(async move {
                                    if let Err(err) = handle_client(sock, &stats).await {
                                        debug!(?peer, ?err, "proxy: client terminated with error");
                                    }
                                });
                            }
                            Err(err) => {
                                warn!(?err, "proxy: accept failed");
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
            }
        });

        Ok(ProxyHandle {
            bound_addr,
            stats,
            shutdown: Some(tx),
        })
    }
}

#[derive(Debug, Error)]
enum ClientError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("client sent oversize request line ({0} bytes)")]
    Oversize(usize),
    #[error("client sent malformed request: {0}")]
    BadRequest(&'static str),
    #[error("only CONNECT is supported, got {0:?}")]
    Unsupported(String),
    #[error("upstream connect timed out: {0}")]
    UpstreamTimeout(String),
    #[error("upstream connect failed: {0}: {1}")]
    UpstreamConnect(String, std::io::Error),
}

async fn handle_client(mut client: TcpStream, stats: &Arc<ProxyStats>) -> Result<(), ClientError> {
    let request =
        match tokio::time::timeout(REQUEST_READ_TIMEOUT, read_request_head(&mut client)).await {
            Ok(r) => r?,
            Err(_) => {
                stats.record_bad_request();
                return Err(ClientError::BadRequest("timeout reading request line"));
            }
        };

    let (method, target) = parse_request_line(&request).map_err(|e| {
        stats.record_bad_request();
        e
    })?;

    if !method.eq_ignore_ascii_case("CONNECT") {
        stats.record_bad_request();
        let _ = client
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n")
            .await;
        return Err(ClientError::Unsupported(method.to_string()));
    }

    let (host, port) = split_host_port(target).map_err(|e| {
        stats.record_bad_request();
        e
    })?;

    stats.record_connect(host);
    debug!(%host, port, "proxy: CONNECT");

    let upstream = match tokio::time::timeout(
        UPSTREAM_CONNECT_TIMEOUT,
        TcpStream::connect((host, port)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(err)) => {
            stats.record_upstream_failure();
            let _ = client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                .await;
            return Err(ClientError::UpstreamConnect(host.to_string(), err));
        }
        Err(_) => {
            stats.record_upstream_failure();
            let _ = client
                .write_all(b"HTTP/1.1 504 Gateway Timeout\r\nConnection: close\r\n\r\n")
                .await;
            return Err(ClientError::UpstreamTimeout(host.to_string()));
        }
    };

    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;

    bridge(client, upstream, host, stats).await
}

/// Bridge the two TCP streams and tally per-direction bytes against
/// the host. Returns when either side closes its read half.
async fn bridge(
    client: TcpStream,
    upstream: TcpStream,
    host: &str,
    stats: &Arc<ProxyStats>,
) -> Result<(), ClientError> {
    let (mut cr, mut cw) = client.into_split();
    let (mut ur, mut uw) = upstream.into_split();

    let up_to_upstream = async {
        // client → upstream: bytes we count as `bytes_up`
        let n = tokio::io::copy(&mut cr, &mut uw).await.unwrap_or(0);
        let _ = uw.shutdown().await;
        n
    };
    let down_to_client = async {
        let n = tokio::io::copy(&mut ur, &mut cw).await.unwrap_or(0);
        let _ = cw.shutdown().await;
        n
    };

    let (up, down) = tokio::join!(up_to_upstream, down_to_client);
    stats.record_traffic(host, up, down);
    debug!(%host, up, down, "proxy: tunnel closed");
    Ok(())
}

/// Read until we see the terminating `\r\n\r\n` of the request head,
/// or until we hit [`MAX_REQUEST_BYTES`].
async fn read_request_head(client: &mut TcpStream) -> Result<Vec<u8>, ClientError> {
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 1024];
    loop {
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            return Err(ClientError::BadRequest("client closed before request"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_REQUEST_BYTES {
            return Err(ClientError::Oversize(buf.len()));
        }
        if find_double_crlf(&buf).is_some() {
            return Ok(buf);
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_request_line(buf: &[u8]) -> Result<(&str, &str), ClientError> {
    let line_end = buf
        .windows(2)
        .position(|w| w == b"\r\n")
        .ok_or(ClientError::BadRequest("no CRLF"))?;
    let line = std::str::from_utf8(&buf[..line_end])
        .map_err(|_| ClientError::BadRequest("non-utf8 request line"))?;
    let mut parts = line.splitn(3, ' ');
    let method = parts
        .next()
        .ok_or(ClientError::BadRequest("missing method"))?;
    let target = parts
        .next()
        .ok_or(ClientError::BadRequest("missing target"))?;
    let _version = parts
        .next()
        .ok_or(ClientError::BadRequest("missing version"))?;
    Ok((method, target))
}

fn split_host_port(target: &str) -> Result<(&str, u16), ClientError> {
    // CONNECT targets are always `host:port`. Strip an IPv6 bracketed
    // form if present.
    let (host, port_str) = if let Some(rest) = target.strip_prefix('[') {
        let close = rest
            .find(']')
            .ok_or(ClientError::BadRequest("unterminated IPv6 literal"))?;
        let host = &rest[..close];
        let after = &rest[close + 1..];
        let port_str = after
            .strip_prefix(':')
            .ok_or(ClientError::BadRequest("missing port after IPv6 literal"))?;
        (host, port_str)
    } else {
        let colon = target
            .rfind(':')
            .ok_or(ClientError::BadRequest("missing port"))?;
        (&target[..colon], &target[colon + 1..])
    };
    let port: u16 = port_str
        .parse()
        .map_err(|_| ClientError::BadRequest("invalid port"))?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_connect() {
        let (m, t) =
            parse_request_line(b"CONNECT pypi.org:443 HTTP/1.1\r\nHost: pypi.org:443\r\n\r\n")
                .unwrap();
        assert_eq!(m, "CONNECT");
        assert_eq!(t, "pypi.org:443");
    }

    #[test]
    fn splits_ipv4_host_port() {
        let (h, p) = split_host_port("192.168.64.1:3128").unwrap();
        assert_eq!(h, "192.168.64.1");
        assert_eq!(p, 3128);
    }

    #[test]
    fn splits_ipv6_host_port() {
        let (h, p) = split_host_port("[2001:db8::1]:8080").unwrap();
        assert_eq!(h, "2001:db8::1");
        assert_eq!(p, 8080);
    }

    #[test]
    fn rejects_invalid_port() {
        assert!(split_host_port("pypi.org:notaport").is_err());
        assert!(split_host_port("pypi.org").is_err());
    }
}

//! End-to-end smoke test for the CONNECT proxy.
//!
//! Stands up an in-process echo TCP server, points the proxy at it via
//! a `CONNECT 127.0.0.1:<port>` request, and verifies the bytes round-
//! trip and the per-host stats reflect what flowed.

use devcontainer_proxy::{ProxyConfig, ProxyServer};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = sock.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    if sock.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

async fn read_until(stream: &mut TcpStream, needle: &[u8], cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut tmp = [0u8; 256];
    while out.len() < cap {
        let n = stream.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&tmp[..n]);
        if out.windows(needle.len()).any(|w| w == needle) {
            break;
        }
    }
    out
}

#[tokio::test]
async fn connect_tunnel_round_trips_bytes_and_records_stats() {
    let upstream = echo_server().await;

    let proxy = ProxyServer::start(ProxyConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let proxy_addr = proxy.bound_addr();

    // Open a client connection to the proxy and send a CONNECT.
    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let req = format!("CONNECT {upstream} HTTP/1.1\r\nHost: {upstream}\r\n\r\n");
    client.write_all(req.as_bytes()).await.unwrap();

    // Read the proxy's success line.
    let head = read_until(&mut client, b"\r\n\r\n", 256).await;
    let head_str = std::str::from_utf8(&head).unwrap();
    assert!(
        head_str.starts_with("HTTP/1.1 200"),
        "proxy did not establish tunnel: {head_str:?}",
    );

    // Send some application data; expect it echoed back.
    client.write_all(b"hello, proxy\n").await.unwrap();
    let mut echo_buf = vec![0u8; 13];
    client.read_exact(&mut echo_buf).await.unwrap();
    assert_eq!(&echo_buf, b"hello, proxy\n");

    // Close client; the bridge should drain and stats should land.
    drop(client);

    // Give the bridge a moment to record its byte tally.
    for _ in 0..50 {
        let s = proxy.stats();
        if s.total_bytes_up >= 13 && s.total_bytes_down >= 13 {
            assert_eq!(s.total_connections, 1);
            assert_eq!(s.upstream_failures, 0);
            assert_eq!(s.bad_requests, 0);
            assert_eq!(s.per_host.len(), 1);
            assert_eq!(s.per_host[0].host, "127.0.0.1");
            assert!(s.per_host[0].connections >= 1);
            assert!(s.per_host[0].bytes_up >= 13);
            assert!(s.per_host[0].bytes_down >= 13);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("stats never settled: {:?}", proxy.stats());
}

#[tokio::test]
async fn rejects_non_connect_method() {
    let proxy = ProxyServer::start(ProxyConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut client = TcpStream::connect(proxy.bound_addr()).await.unwrap();
    client
        .write_all(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
        .await
        .unwrap();
    let head = read_until(&mut client, b"\r\n\r\n", 256).await;
    let s = std::str::from_utf8(&head).unwrap();
    assert!(s.starts_with("HTTP/1.1 405"), "unexpected response: {s:?}");
}

#[tokio::test]
async fn surfaces_upstream_failure() {
    // Bind an ephemeral port and immediately drop the listener so the
    // port is unbound by the time we ask the proxy to CONNECT to it.
    let bad = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };

    let proxy = ProxyServer::start(ProxyConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut client = TcpStream::connect(proxy.bound_addr()).await.unwrap();
    let req = format!("CONNECT {bad} HTTP/1.1\r\nHost: {bad}\r\n\r\n");
    client.write_all(req.as_bytes()).await.unwrap();
    let head = read_until(&mut client, b"\r\n\r\n", 256).await;
    let s = std::str::from_utf8(&head).unwrap();
    assert!(
        s.starts_with("HTTP/1.1 502") || s.starts_with("HTTP/1.1 504"),
        "expected 502/504, got: {s:?}",
    );

    // The failure should be reflected in stats.
    for _ in 0..50 {
        let snap = proxy.stats();
        if snap.upstream_failures >= 1 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!(
        "upstream failure not reflected in stats: {:?}",
        proxy.stats()
    );
}

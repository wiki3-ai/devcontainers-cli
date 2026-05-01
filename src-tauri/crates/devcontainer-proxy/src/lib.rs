//! Forward proxy for dev containers.
//!
//! # Phase 1 (this module today)
//!
//! A CONNECT-only HTTP forward proxy. Every `CONNECT host:port` request
//! is answered with a TCP tunnel to the requested upstream and the two
//! sides are bridged via `tokio::io::copy_bidirectional`. No TLS
//! interception, no caching — this just replaces the user-managed
//! Squid-in-Docker proxy and gives us a single point where we can
//! observe what containers are fetching.
//!
//! Per-host counters (connections, bytes up, bytes down, last seen) are
//! exposed through [`ProxyHandle::stats`]. They're the foundation for
//! Phase 2's caching: by recording every CONNECT target we can see
//! exactly which hosts are worth implementing protocol-aware cache
//! handlers for.
//!
//! # Future phases
//!
//! Phase 2 will add per-protocol *named-mirror* caching (pip, npm,
//! GitHub release tarballs). Containers will reach those mirrors by
//! URL rather than via this generic CONNECT path, which lets us cache
//! HTTPS bodies without MITM-ing every TLS connection. See
//! `docs/proposals/internal-caching-proxy.md` for the full plan.

mod server;
mod stats;

pub use server::{ProxyConfig, ProxyError, ProxyHandle, ProxyServer};
pub use stats::{HostStats, ProxyStatsSnapshot};

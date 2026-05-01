//! Per-host and aggregate counters for the forward proxy.
//!
//! We track these from day one — even though Phase 1 has no caching
//! to measure cache-hit-rate against, the per-host CONNECT counts are
//! what we'll use to decide which protocols are worth a Phase 2
//! cache handler.

use parking_lot::RwLock;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Mutable internal counters. The proxy server owns one of these and
/// publishes immutable [`ProxyStatsSnapshot`]s on demand for the UI.
#[derive(Default)]
pub(crate) struct ProxyStats {
    pub started_at: parking_lot::Mutex<Option<SystemTime>>,
    pub total_connections: AtomicU64,
    pub total_bytes_up: AtomicU64,
    pub total_bytes_down: AtomicU64,
    pub upstream_failures: AtomicU64,
    pub bad_requests: AtomicU64,
    pub per_host: RwLock<HashMap<String, MutHostStats>>,
}

#[derive(Default)]
pub(crate) struct MutHostStats {
    pub connections: AtomicU64,
    pub bytes_up: AtomicU64,
    pub bytes_down: AtomicU64,
    pub last_seen: parking_lot::Mutex<Option<SystemTime>>,
}

impl ProxyStats {
    pub fn record_connect(self: &Arc<Self>, host: &str) {
        self.total_connections.fetch_add(1, Ordering::Relaxed);
        self.touch_host(host, |h| {
            h.connections.fetch_add(1, Ordering::Relaxed);
            *h.last_seen.lock() = Some(SystemTime::now());
        });
    }

    pub fn record_traffic(self: &Arc<Self>, host: &str, up: u64, down: u64) {
        if up == 0 && down == 0 {
            return;
        }
        self.total_bytes_up.fetch_add(up, Ordering::Relaxed);
        self.total_bytes_down.fetch_add(down, Ordering::Relaxed);
        self.touch_host(host, |h| {
            h.bytes_up.fetch_add(up, Ordering::Relaxed);
            h.bytes_down.fetch_add(down, Ordering::Relaxed);
            *h.last_seen.lock() = Some(SystemTime::now());
        });
    }

    pub fn record_upstream_failure(&self) {
        self.upstream_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_bad_request(&self) {
        self.bad_requests.fetch_add(1, Ordering::Relaxed);
    }

    fn touch_host<F: FnOnce(&MutHostStats)>(&self, host: &str, f: F) {
        // Fast path: read lock, host already present.
        if let Some(h) = self.per_host.read().get(host) {
            f(h);
            return;
        }
        // Slow path: insert under write lock, re-check first.
        let mut w = self.per_host.write();
        let h = w.entry(host.to_string()).or_default();
        f(h);
    }

    pub fn snapshot(&self) -> ProxyStatsSnapshot {
        let started_at = *self.started_at.lock();
        let uptime = started_at
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .unwrap_or_default();
        let per_host = self
            .per_host
            .read()
            .iter()
            .map(|(name, h)| {
                let last_seen_unix = h
                    .last_seen
                    .lock()
                    .as_ref()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                HostStats {
                    host: name.clone(),
                    connections: h.connections.load(Ordering::Relaxed),
                    bytes_up: h.bytes_up.load(Ordering::Relaxed),
                    bytes_down: h.bytes_down.load(Ordering::Relaxed),
                    last_seen_unix,
                }
            })
            .collect();
        ProxyStatsSnapshot {
            uptime_secs: uptime.as_secs(),
            total_connections: self.total_connections.load(Ordering::Relaxed),
            total_bytes_up: self.total_bytes_up.load(Ordering::Relaxed),
            total_bytes_down: self.total_bytes_down.load(Ordering::Relaxed),
            upstream_failures: self.upstream_failures.load(Ordering::Relaxed),
            bad_requests: self.bad_requests.load(Ordering::Relaxed),
            per_host,
        }
    }

    pub fn mark_started(&self) {
        *self.started_at.lock() = Some(SystemTime::now());
    }
}

/// Immutable snapshot of proxy counters, suitable for serializing to
/// the UI.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyStatsSnapshot {
    pub uptime_secs: u64,
    pub total_connections: u64,
    pub total_bytes_up: u64,
    pub total_bytes_down: u64,
    pub upstream_failures: u64,
    pub bad_requests: u64,
    pub per_host: Vec<HostStats>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStats {
    pub host: String,
    pub connections: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    /// Seconds since unix epoch. `None` when no traffic has been
    /// observed for this host yet.
    pub last_seen_unix: Option<u64>,
}

impl ProxyStatsSnapshot {
    pub fn uptime(&self) -> Duration {
        Duration::from_secs(self.uptime_secs)
    }
}

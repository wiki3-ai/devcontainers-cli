//! Lazy-started internal forward proxy.
//!
//! [`ProxyManager`] wraps a [`devcontainer_proxy::ProxyServer`] so the
//! orchestrator can opportunistically point containers at an internal
//! proxy when the host has no `HTTP_PROXY` of its own. The bind is
//! deferred until the first `ensure_started` call so we don't pay for
//! the listener on construction (and tests with the default
//! [`ProxyManager::disabled`] never pay at all).
//!
//! Failure modes are graceful: if `bind` fails (port in use,
//! interface not yet up, etc.) the manager remembers that with a
//! cached `None` and silently falls back to "no auto-injection". The
//! container still comes up; it just gets whatever proxy
//! configuration was on the host (or none).

use std::net::SocketAddr;

use devcontainer_proxy::{ProxyConfig, ProxyHandle, ProxyServer, ProxyStatsSnapshot};
use tokio::sync::OnceCell;
use tracing::{info, warn};

/// Forward-proxy lifecycle wrapper held by [`LifecycleOrchestrator`].
///
/// [`LifecycleOrchestrator`]: super::lifecycle::LifecycleOrchestrator
pub struct ProxyManager {
    config: Option<ProxyConfig>,
    /// `OnceCell<Option<...>>` so a bind failure is cached as `None`
    /// and we don't keep retrying on every `up`.
    handle: OnceCell<Option<ProxyHandle>>,
}

impl Default for ProxyManager {
    fn default() -> Self {
        Self::disabled()
    }
}

impl std::fmt::Debug for ProxyManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyManager")
            .field("config", &self.config)
            .field(
                "running",
                &self.handle.get().map(|o| o.is_some()).unwrap_or(false),
            )
            .finish()
    }
}

impl ProxyManager {
    /// A disabled manager. [`Self::ensure_started`] is a no-op,
    /// [`Self::url`] / [`Self::stats`] return `None`. Used in tests
    /// and by callers that opt out of the internal proxy entirely.
    pub fn disabled() -> Self {
        Self {
            config: None,
            handle: OnceCell::new(),
        }
    }

    /// Configure the manager to bind on `addr` on first
    /// [`Self::ensure_started`].
    pub fn with_bind(addr: SocketAddr) -> Self {
        Self {
            config: Some(ProxyConfig::new(addr)),
            handle: OnceCell::new(),
        }
    }

    /// Convenience: bind on Apple Containers' default bridge gateway
    /// IP (`192.168.64.1`) at the canonical Devcontainers port
    /// (31280). Containers spawned on the default bridge can reach
    /// the proxy at this address.
    pub fn with_apple_containers_default() -> Self {
        // SAFETY: literal is a valid SocketAddr.
        Self::with_bind("192.168.64.1:31280".parse().unwrap())
    }

    /// Lazily bind the listener and start the accept loop. Returns
    /// the proxy URL on success, or `None` if the manager is
    /// disabled or the bind failed.
    pub async fn ensure_started(&self) -> Option<String> {
        let h = self
            .handle
            .get_or_init(|| async {
                let cfg = self.config.clone()?;
                match ProxyServer::start(cfg).await {
                    Ok(h) => {
                        info!(addr = %h.bound_addr(), "proxy: listening");
                        Some(h)
                    }
                    Err(err) => {
                        warn!(
                            ?err,
                            "proxy: bind failed; auto-injection disabled for this session"
                        );
                        None
                    }
                }
            })
            .await;
        h.as_ref().map(|h| h.proxy_url())
    }

    /// Current proxy URL, or `None` if not started / disabled / bind
    /// failed. Cheap; suitable for synchronous callers.
    pub fn url(&self) -> Option<String> {
        self.handle
            .get()
            .and_then(|o| o.as_ref())
            .map(|h| h.proxy_url())
    }

    /// Stats snapshot, or `None` if not running.
    pub fn stats(&self) -> Option<ProxyStatsSnapshot> {
        self.handle
            .get()
            .and_then(|o| o.as_ref())
            .map(|h| h.stats())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_manager_yields_no_url() {
        let m = ProxyManager::disabled();
        assert!(m.ensure_started().await.is_none());
        assert!(m.url().is_none());
        assert!(m.stats().is_none());
    }

    #[tokio::test]
    async fn with_bind_starts_lazily_and_records_url() {
        let m = ProxyManager::with_bind("127.0.0.1:0".parse().unwrap());
        assert!(m.url().is_none(), "should not bind eagerly");
        let url = m.ensure_started().await.expect("bind succeeded");
        assert!(url.starts_with("http://127.0.0.1:"), "url: {url}");
        // Idempotent.
        assert_eq!(m.ensure_started().await.as_deref(), Some(url.as_str()));
        assert_eq!(m.url().as_deref(), Some(url.as_str()));
    }

    #[tokio::test]
    async fn bind_failure_caches_disabled_state() {
        // Hold a port to force a bind conflict.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let busy_addr = listener.local_addr().unwrap();
        let m = ProxyManager::with_bind(busy_addr);
        assert!(m.ensure_started().await.is_none());
        // Still none after the holder is released — failure is cached.
        drop(listener);
        assert!(m.ensure_started().await.is_none());
    }
}

//! App configuration: trusted-origin allowlist, dev URL override, default
//! runtime preference. Ported in shape from `wiki3-ai/wiki3-app`'s
//! `config.rs`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Origins that may be opened in app windows. The Tauri WebView itself
    /// always loads `tauri://localhost`; this list governs *additional*
    /// windows opened from the dashboard (e.g. external help URLs).
    pub trusted_origins: Vec<String>,
    /// Default container runtime when none is explicitly selected by the
    /// user. The registry will fall back to capability probing if the
    /// preferred runtime is unavailable.
    pub preferred_runtime: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            trusted_origins: vec![
                "https://containers.apple.com".into(),
                "https://github.com".into(),
            ],
            preferred_runtime: "apple-containers".into(),
        }
    }
}

impl AppConfig {
    /// Whether `origin` is in the allowlist.
    pub fn is_trusted(&self, origin: &str) -> bool {
        self.trusted_origins.iter().any(|o| o == origin)
    }
}

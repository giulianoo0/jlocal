//! Shared process status.

/// Build-time version. Release workflow injects `JLOCAL_VERSION` (the git
/// tag); local builds fall back to `CARGO_PKG_VERSION`.
pub const VERSION: &str = env!("JLOCAL_VERSION");
pub const NAME: &str = "jlocal";

/// Fixed loopback port default (overridable via `--port` / `JLOCAL_PORT`).
pub const DEFAULT_PORT: u16 = 4173;

#[derive(Clone, Debug)]
pub struct AppState {
    pub started_unix: u64,
    pub allowed_origins: Vec<String>,
}

impl AppState {
    pub fn new() -> Self {
        let allowed_origins = std::env::var("JLOCAL_ALLOWED_ORIGINS")
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let started_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            started_unix,
            allowed_origins,
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

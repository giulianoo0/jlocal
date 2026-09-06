//! Shared process status.

/// Build-time version. Release workflow injects `JLOCAL_VERSION` (the git
/// tag); local builds fall back to `CARGO_PKG_VERSION`.
pub const VERSION: &str = env!("JLOCAL_VERSION");
pub const NAME: &str = "jlocal";

/// Fixed loopback port default (overridable via `--port` / `JLOCAL_PORT`).
pub const DEFAULT_PORT: u16 = 40392;

/// Origins the browser UI may probe us from. Used when
/// `JLOCAL_ALLOWED_ORIGINS` is unset so a fresh install just works;
/// setting the env replaces this list entirely.
const DEFAULT_ORIGINS: [&str; 3] = [
    "https://juntos.lol",
    "https://www.juntos.lol",
    "https://beta.juntos.lol",
];

fn parse_origins(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn default_origins() -> Vec<String> {
    DEFAULT_ORIGINS.iter().map(|s| s.to_string()).collect()
}
#[cfg(test)]
pub fn default_origins_for_tests() -> Vec<String> {
    default_origins()
}

#[derive(Clone, Debug)]
pub struct AppState {
    pub started_unix: u64,
    pub allowed_origins: Vec<String>,
}

impl AppState {
    pub fn new() -> Self {
        let allowed_origins = std::env::var("JLOCAL_ALLOWED_ORIGINS")
            .map_or_else(|_| default_origins(), |v| parse_origins(&v));
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

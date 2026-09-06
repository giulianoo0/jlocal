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

#[derive(Clone, Copy, Debug, Default)]
pub struct CapabilityFlags {
    /// Screen capture + relay publish both work end to end.
    pub screen: bool,
    /// The app list is truly listable on this machine.
    pub app_list: bool,
    /// Local torrenting serves ranged bytes.
    pub torrent: bool,
}

#[derive(Clone, Debug)]
pub struct AppState {
    pub started_unix: u64,
    pub allowed_origins: Vec<String>,
    pub caps: CapabilityFlags,
    pub capture: std::sync::Arc<parking_lot::Mutex<Option<crate::capture::CaptureSession>>>,
    pub audio: std::sync::Arc<parking_lot::Mutex<crate::audio::AudioState>>,
    /// Local torrent engine. `None` until the async session boots at
    /// startup; handlers answer 503 while it is absent.
    pub torrent: Option<crate::torrent::TorrentManager>,
}

impl AppState {
    pub fn new() -> Self {
        let allowed_origins = std::env::var("JLOCAL_ALLOWED_ORIGINS")
            .map_or_else(|_| default_origins(), |v| parse_origins(&v));
        let started_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Evaluated once: the list is a real tap check, not a promise.
        // Screen stays false until capture + publish both work end to end.
        let caps = CapabilityFlags {
            screen: false,
            app_list: !crate::audio::list_apps().is_empty(),
            torrent: false,
        };
        Self {
            started_unix,
            allowed_origins,
            caps,
            capture: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            audio: std::sync::Arc::new(parking_lot::Mutex::new(crate::audio::AudioState::new())),
            torrent: None,
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

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

#[derive(Clone, Copy, Debug)]
pub struct CapabilityFlags {
    /// Screen capture + relay publish both work end to end.
    pub screen: bool,
    /// Native frame capture is live: the xcap backend is compiled in on
    /// every supported OS, so the `/capture/*` preview endpoints serve real
    /// frames with no runtime probe. Relay publish is still unwired, so
    /// `screen` stays false and gates publish only.
    pub screen_capture: bool,
    /// The app list is truly listable on this machine.
    pub app_list: bool,
    /// Per-app system-audio capture is wired: `GET /audio/stream` serves
    /// the mixed PCM stream on macOS/Windows, `501` on Linux. See
    /// [`crate::audio_engine::capture_supported`].
    pub audio_capture: bool,
    /// Local torrenting serves ranged bytes.
    pub torrent: bool,
}

impl Default for CapabilityFlags {
    fn default() -> Self {
        Self {
            screen: false,
            screen_capture: true,
            app_list: false,
            audio_capture: crate::audio_engine::capture_supported(),
            torrent: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AppState {
    pub started_unix: u64,
    pub allowed_origins: Vec<String>,
    pub caps: CapabilityFlags,
    pub capture: std::sync::Arc<parking_lot::Mutex<Option<crate::capture::CaptureSession>>>,
    pub audio: std::sync::Arc<parking_lot::Mutex<crate::audio::AudioState>>,
    /// Shared system-audio tap feeding `GET /audio/stream`. Started lazily
    /// by the first stream request while a capture session is live, stopped
    /// by `/capture/stop` alongside the video session.
    pub audio_tap: std::sync::Arc<parking_lot::Mutex<crate::audio_engine::AudioTap>>,
    /// Local torrent engine. `None` until the async session boots at
    /// startup; handlers answer 503 while it is absent.
    pub torrent: Option<crate::torrent::TorrentManager>,
    /// Self-update status: the background poller records the newest newer
    /// tag here; the window title and tray menu read it.
    pub update: std::sync::Arc<parking_lot::Mutex<crate::update::UpdateState>>,
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
        // Screen stays false until capture + publish both work end to end;
        // screen_capture rides along from Default (constant true: the xcap
        // backend is compiled in on every supported OS).
        let caps = CapabilityFlags {
            app_list: !crate::audio::list_apps().is_empty(),
            ..Default::default()
        };
        Self {
            started_unix,
            allowed_origins,
            caps,
            capture: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            audio: std::sync::Arc::new(parking_lot::Mutex::new(crate::audio::AudioState::new())),
            audio_tap: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::audio_engine::AudioTap::new(),
            )),
            torrent: None,
            update: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::update::UpdateState::default(),
            )),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Window title + console status line: version, loopback endpoint, live
/// capture-permission state, and the pending update when one is known.
/// `update_tag` is `AppState.update.latest_tag` — `None` on a fresh boot
/// (the first check hasn't landed yet) or when up-to-date.
pub fn window_title(port: u16, update_tag: Option<&str>) -> String {
    let permission = if crate::permissions::screen_capture_granted() {
        "capture allowed"
    } else {
        "capture blocked — enable Screen Recording"
    };
    let mut title = format!("jlocal {VERSION} — connected (127.0.0.1:{port}) — {permission}");
    if let Some(tag) = update_tag {
        title.push_str(&format!(" — update available {tag}"));
    }
    title
}

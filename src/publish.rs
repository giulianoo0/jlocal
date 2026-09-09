//! Best-effort MoQ publish of the local screen to the Cloudflare relay.
//!
//! # Status: STUB with a pinned wire contract (September 2026)
//!
//! The relay this deployment uses (`MOQ_RELAY_URL=https://draft-16.…`) speaks
//! draft-ietf-moq-transport-14 and -16, and the Rust side of the same
//! monorepo the web publisher comes from (moq-dev/moq: `moq-net` with
//! `Version::Ietf(Draft16)`, `moq-native` for WebTransport over quinn, `hang`
//! for the catalog and containers) speaks draft-16 too. The transport is
//! therefore buildable; what is missing is the encoder pipeline (hardware
//! H.264 at 4K60 + Opus) and the wiring. `rs/hang/examples/video.rs` in that
//! repo is the publisher shape to copy. Until it lands every `push_*` fails
//! and the `screen.available` capability flag MUST stay `false`.
//!
//! # Wire contract this module pins (byte-compatible with the web publisher)
//!
//! When a transport lands, the session MUST reproduce exactly what
//! `@moq/publish` emits, or existing `@moq/watch` viewers break:
//!
//! - Broadcast path: `juntos/<room>/<secret>/<member>.hang` (server builds
//!   the base in `ScreenBroadcastBase`, the web adds `/<member>.hang` in
//!   `screenPath`; the `.hang` suffix selects the hang catalog). One broadcast
//!   per publishing member, since several members may share at once.
//! - Tracks inside the broadcast: [`CATALOG_TRACK`] (`catalog.json`), its
//!   DEFLATE sibling [`CATALOG_TRACK_COMPRESSED`] (`catalog.json.z`),
//!   [`VIDEO_TRACK`] (`video`), [`AUDIO_TRACK`] (`audio`).
//! - Catalog JSON: `{ video: { renditions: { video: { codec, container,
//!   codedWidth, codedHeight, framerate, bitrate } } },
//!   audio: { renditions: { audio: { codec: "opus", container, sampleRate,
//!   numberOfChannels, bitrate } } } }` (see [`catalog_json`]).
//! - Media framing: the hang `legacy` container, which is what
//!   `@moq/publish` 0.4.6 emits and `@moq/watch` 0.5.3 consumes: a varint
//!   timestamp in microseconds followed by the codec bitstream; keyframes
//!   start a new group. (LOC exists on both sides but is not what viewers
//!   read today.) Video codec is a WebCodecs string negotiated
//!   by the browser encoder (e.g. `avc1.640028`); audio is Opus stereo at
//!   [`SCREEN_AUDIO_BITRATE`] (160_000, music use, mirroring the web default).
//! - Auth: the publish token is path-appended to the relay URL
//!   (`<relay>/<token>`); the WebTransport handshake carries it, no headers.
//!
//! [`PublishSession`] keeps this API shape stable so the future transport only
//! fills in the `Err(_)` arms below. Until then every `push_*` fails and the
//! `screen.available` capability flag MUST stay `false`.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;

/// Opus bitrate for screen audio: stereo music, mirroring the web default.
pub const SCREEN_AUDIO_BITRATE: u32 = 160_000;

/// Opus sample rate the catalog advertises.
pub const SCREEN_AUDIO_SAMPLE_RATE: u64 = 48_000;

/// Opus channel count the catalog advertises.
pub const SCREEN_AUDIO_CHANNELS: u64 = 2;

/// Broadcast-name suffix that tells viewers to expect a hang catalog.
/// Mirrors `ScreenBroadcastPath` (`internal/httpapi/screenshare.go`) and
/// `@moq/hang` `detectFormat`.
pub const BROADCAST_SUFFIX: &str = ".hang";

/// Track serving the uncompressed hang catalog inside a broadcast.
/// Mirrors `CatalogProducer::CATALOG_TRACK` in `@moq/publish`.
pub const CATALOG_TRACK: &str = "catalog.json";

/// Track serving the DEFLATE-compressed hang catalog (identical content).
/// Mirrors `CatalogProducer::CATALOG_TRACK_COMPRESSED` in `@moq/publish`.
pub const CATALOG_TRACK_COMPRESSED: &str = "catalog.json.z";

/// Single video rendition track. The web publisher constructs
/// `Video.Encoder('video', …)`, so the full track name is exactly this.
pub const VIDEO_TRACK: &str = "video";

/// Single audio rendition track. The web publisher constructs
/// `Audio.Encoder('audio', …)` with `{ mime: 'opus', bitrate: 160_000 }` and
/// `kind: 'music'`.
pub const AUDIO_TRACK: &str = "audio";

/// Container label the catalog assigns to the renditions: the `legacy`
/// framing `@moq/publish` emits today. One of the known `@moq/hang` kinds
/// (`legacy`, `cmaf`, `loc`).
pub const CONTAINER: &str = "legacy";
/// Error text returned by every network path until the transport lands.
pub const TRANSPORT_UNAVAILABLE: &str =
    "moq publish unavailable: draft-ietf-moq-transport-16 publisher not wired yet (see publish.rs)";

/// Cumulative publish totals. All counters are monotonic; diff over an
/// interval for rates. All zero while the transport is a stub.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct PublishStats {
    pub video_frames: u64,
    pub video_bytes: u64,
    pub video_keyframes: u64,
    pub audio_frames: u64,
    pub audio_bytes: u64,
}

/// One video rendition as the hang catalog describes it.
/// Field names mirror WebCodecs `VideoDecoderConfig` via `@moq/hang`.
#[derive(Clone, Debug, Serialize)]
#[allow(non_snake_case)]
pub struct VideoRendition {
    pub codec: String,
    pub container: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codedWidth: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codedHeight: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framerate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u32>,
}

/// One audio rendition as the hang catalog describes it.
/// Field names mirror WebCodecs `AudioDecoderConfig` via `@moq/hang`.
#[derive(Clone, Debug, Serialize)]
#[allow(non_snake_case)]
pub struct AudioRendition {
    pub codec: String,
    pub container: BTreeMap<String, String>,
    #[serde(rename = "sampleRate")]
    pub sample_rate: u64,
    #[serde(rename = "numberOfChannels")]
    pub number_of_channels: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u32>,
}

/// Builds one member's broadcast path, mirroring `ScreenBroadcastBase` +
/// the web's `screenPath`: `juntos/<room>/<secret>/<member>.hang`.
pub fn broadcast_path(room_id: &str, secret: &str, member_id: &str) -> String {
    format!("juntos/{room_id}/{secret}/{member_id}{BROADCAST_SUFFIX}")
}

/// Builds the hang catalog JSON the viewers' `@moq/watch` parses: rendition
/// maps keyed by the exact track names [`VIDEO_TRACK`]/[`AUDIO_TRACK`].
/// `video_codec` is a WebCodecs string (e.g. `avc1.640028`).
pub fn catalog_json(
    video_codec: &str,
    width: u32,
    height: u32,
    fps: u32,
    video_bitrate: u32,
) -> serde_json::Value {
    let mut video_container = BTreeMap::new();
    video_container.insert("kind".to_string(), CONTAINER.to_string());
    let mut audio_container = BTreeMap::new();
    audio_container.insert("kind".to_string(), CONTAINER.to_string());
    let mut video_renditions = serde_json::Map::new();
    video_renditions.insert(
        VIDEO_TRACK.to_string(),
        serde_json::to_value(VideoRendition {
            codec: video_codec.to_string(),
            container: video_container,
            codedWidth: Some(width),
            codedHeight: Some(height),
            framerate: Some(fps),
            bitrate: Some(video_bitrate),
        })
        .expect("VideoRendition serializes"),
    );
    let mut audio_renditions = serde_json::Map::new();
    audio_renditions.insert(
        AUDIO_TRACK.to_string(),
        serde_json::to_value(AudioRendition {
            codec: "opus".to_string(),
            container: audio_container,
            sample_rate: SCREEN_AUDIO_SAMPLE_RATE,
            number_of_channels: SCREEN_AUDIO_CHANNELS,
            bitrate: Some(SCREEN_AUDIO_BITRATE),
        })
        .expect("AudioRendition serializes"),
    );
    serde_json::json!({
        "video": { "renditions": video_renditions },
        "audio": { "renditions": audio_renditions },
    })
}

/// Guards the frame timestamp invariant (microseconds, monotonic per track):
/// viewers compute jitter from these, so a regression corrupts A/V sync.
/// Returns the timestamp to publish (unchanged); errors on regression.
#[derive(Clone, Copy, Debug, Default)]
pub struct MonotonicClock {
    last_us: Option<u64>,
}

impl MonotonicClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check(&mut self, timestamp_us: u64) -> Result<u64> {
        if let Some(last) = self.last_us {
            if timestamp_us < last {
                anyhow::bail!("non-monotonic media timestamp: {timestamp_us} < {last}");
            }
        }
        self.last_us = Some(timestamp_us);
        Ok(timestamp_us)
    }
}

/// Best-effort MoQ publisher. Inert until a draft-16 transport lands:
/// [`PublishSession::connect`] records validated endpoint intent, and every
/// `push_*` returns [`TRANSPORT_UNAVAILABLE`].
///
/// Parent wiring (when viable): hold one session behind
/// `Arc<tokio::sync::Mutex<…>>` (e.g. an `AppState.screen_publish` field);
/// `POST /capture/start` creates it from the relay handoff
/// (`url`, `path`, publish token) and answers 501 while pushes fail;
/// `POST /capture/stop` calls [`PublishSession::close`].
#[derive(Clone, Debug)]
pub struct PublishSession {
    video_clock: MonotonicClock,
    audio_clock: MonotonicClock,
    stats: PublishStats,
    closed: bool,
}

impl PublishSession {
    /// Validates the endpoint shape (https relay URL, non-empty broadcast
    /// path and token) so misconfiguration fails here, not mid-stream
    /// later. Performs no I/O and stores no endpoint: the transport
    /// re-supplies it when a Rust MoQ transport exists. See the module
    /// docs for why pushes fail by design today.
    pub fn connect(url: &str, path: &str, publish_token: &str) -> Result<Self> {
        if !(url.starts_with("https://") && url.len() > "https://".len()) {
            anyhow::bail!("moq relay url must be https, got {url:?}");
        }
        if path.is_empty() {
            anyhow::bail!("moq broadcast path must not be empty");
        }
        if publish_token.is_empty() {
            anyhow::bail!("moq publish token must not be empty");
        }
        Ok(Self {
            video_clock: MonotonicClock::new(),
            audio_clock: MonotonicClock::new(),
            stats: PublishStats::default(),
            closed: false,
        })
    }

    /// Publishes one H.264 Annex-B frame with its presentation timestamp in
    /// microseconds. Keyframes MUST start a new group; the future
    /// transport takes that from `keyframe`.
    pub fn push_video(
        &mut self,
        _h264_annex_b: Vec<u8>,
        timestamp_us: u64,
        _keyframe: bool,
    ) -> Result<()> {
        self.ensure_open()?;
        self.video_clock
            .check(timestamp_us)
            .context("dropping video frame")?;
        anyhow::bail!(TRANSPORT_UNAVAILABLE);
    }

    /// Publishes one Opus packet (160 kbps stereo, music) with its
    /// presentation timestamp in microseconds.
    pub fn push_audio(&mut self, _opus: Vec<u8>, timestamp_us: u64) -> Result<()> {
        self.ensure_open()?;
        self.audio_clock
            .check(timestamp_us)
            .context("dropping audio frame")?;
        anyhow::bail!(TRANSPORT_UNAVAILABLE);
    }

    pub fn stats(&self) -> PublishStats {
        self.stats
    }

    /// Idempotent: safe to call from both `/capture/stop` and drop paths.
    pub fn close(&mut self) {
        self.closed = true;
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed {
            anyhow::bail!("moq publish session is closed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_names_match_web_publisher() {
        // byte-compat with @moq/publish: Video.Encoder('video'),
        // Audio.Encoder('audio'), CatalogProducer::CATALOG_TRACK*.
        assert_eq!(VIDEO_TRACK, "video");
        assert_eq!(AUDIO_TRACK, "audio");
        assert_eq!(CATALOG_TRACK, "catalog.json");
        assert_eq!(CATALOG_TRACK_COMPRESSED, "catalog.json.z");
    }

    #[test]
    fn broadcast_path_matches_server() {
        // Mirrors ScreenBroadcastPath in internal/httpapi/screenshare.go.
        assert_eq!(
            broadcast_path("room1", "s3cret", "m1"),
            "juntos/room1/s3cret/m1.hang"
        );
        assert!(broadcast_path("r", "s", "m").ends_with(BROADCAST_SUFFIX));
    }

    #[test]
    fn catalog_shape_matches_hang_schema() {
        let catalog = catalog_json("avc1.640028", 1920, 1080, 30, 2_000_000);
        let video = &catalog["video"]["renditions"]["video"];
        assert_eq!(video["codec"], "avc1.640028");
        assert_eq!(video["container"]["kind"], "legacy");
        assert_eq!(video["codedWidth"], 1920);
        assert_eq!(video["codedHeight"], 1080);
        assert_eq!(video["framerate"], 30);
        let audio = &catalog["audio"]["renditions"]["audio"];
        assert_eq!(audio["codec"], "opus");
        assert_eq!(audio["container"]["kind"], "legacy");
        assert_eq!(audio["sampleRate"], 48_000);
        assert_eq!(audio["numberOfChannels"], 2);
        assert_eq!(audio["bitrate"], 160_000);
        // No stray rendition keys: viewers subscribe exactly these tracks.
        assert_eq!(
            catalog["video"]["renditions"]
                .as_object()
                .expect("object")
                .len(),
            1
        );
        assert_eq!(
            catalog["audio"]["renditions"]
                .as_object()
                .expect("object")
                .len(),
            1
        );
    }

    #[test]
    fn clock_rejects_regression_accepts_equal() {
        let mut clock = MonotonicClock::new();
        assert_eq!(clock.check(100).unwrap(), 100);
        assert_eq!(clock.check(100).unwrap(), 100);
        assert!(clock.check(99).is_err());
    }

    #[test]
    fn connect_validates_endpoint_shape() {
        assert!(PublishSession::connect(
            "https://draft-16.cloudflare.mediaoverquic.com/tok",
            "juntos/r/s/m.hang",
            "tok",
        )
        .is_ok());
        assert!(PublishSession::connect("http://relay/token", "p", "t").is_err());
        assert!(PublishSession::connect("https://r/t", "", "t").is_err());
        assert!(PublishSession::connect("https://r/t", "p", "").is_err());
    }

    #[test]
    fn pushes_fail_without_transport_close_is_idempotent() {
        let mut session = PublishSession::connect(
            "https://draft-16.cloudflare.mediaoverquic.com/tok",
            "juntos/r/s/m.hang",
            "tok",
        )
        .unwrap();
        let err = session
            .push_video(vec![0, 0, 0, 1], 1_000, true)
            .unwrap_err();
        assert!(err.to_string().contains("draft-ietf-moq-transport-16"));
        let err = session.push_audio(vec![0xF8], 1_000).unwrap_err();
        assert!(err.to_string().contains("draft-ietf-moq-transport-16"));
        // Timestamps are still guarded even though nothing is sent.
        assert!(session.push_video(vec![], 999, false).is_err());
        session.close();
        session.close();
        assert!(session.is_closed());
        assert!(session.push_audio(vec![], 2_000).is_err());
        let stats = session.stats();
        assert_eq!(stats.video_frames, 0);
        assert_eq!(stats.audio_frames, 0);
    }
}

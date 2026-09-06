//! Local torrenting for jlocal, backed by `librqbit`.
//!
//! Pure logic only: no HTTP, no router, no `AppState`. The parent crate owns
//! the axum handlers and calls into [`TorrentManager`].
//!
//! # Parent wiring (informative, not normative)
//!
//! - `Cargo.toml` must add `librqbit = "9"` (default features). See the tool
//!   result notes for the exact version this was written against.
//! - `AppState` (in `status.rs`) needs a field like
//!   `pub torrent: Option<TorrentManager>` (or `Arc<TorrentManager>`).
//! - `lib.rs` needs `pub mod torrent;`.
//!
//! # Conventions the handlers must follow
//!
//! - Torrent ids are **lowercase infohash hex** (40 chars), stable across
//!   restarts. This module rejects anything else in [`parse_id_hex`].
//! - [`TorrentManager::read_range`] takes `Range<u64>` with an **inclusive**
//!   `end` (HTTP semantics: `bytes=a-b` maps to `a..b`, `bytes=a-` maps to
//!   `a..total-1`, `bytes=-n` maps to `total-n..total-1`). The parent parses
//!   the RFC 9110 `Range` header; unsatisfiable/out-of-range requests surface
//!   as [`TorrentError::Unsatisfiable`] (map to `416`, with a
//!   `Content-Range: bytes */total` header), unknown ids as
//!   [`TorrentError::NotFound`] (map to `404`).
//! - All errors are `anyhow::Error`s wrapping a [`TorrentError`] where the
//!   HTTP status is derivable. Handlers should try
//!   `err.downcast_ref::<TorrentError>()` first, then fall back to `500`.
use std::{
    collections::HashSet, io::SeekFrom, ops::Range, path::PathBuf, str::FromStr, sync::Arc,
    time::Duration,
};

use anyhow::Context;
use librqbit::{
    api::TorrentIdOrHash, dht::Id20, AddTorrent, AddTorrentOptions, ListenerOptions,
    ManagedTorrent, Session, SessionOptions, SessionPersistenceConfig,
};
/// 9.0.1 keeps the handle alias private (`torrent_state` is not public);
/// spell it: every `Session` API that takes one accepts `&Arc<ManagedTorrent>`.
type ManagedTorrentHandle = std::sync::Arc<ManagedTorrent>;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// How long `add_magnet` waits for magnet metadata (DHT + trackers) before
/// giving up. DHT fetches usually resolve in seconds; 60s covers slow swarms
/// without hanging the loopback caller forever.
const METADATA_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `select_file` waits for a torrent to leave `initializing` before
/// applying the file selection (`update_only_files` rejects initializing
/// torrents upstream).
const SELECT_INIT_WAIT: Duration = Duration::from_secs(30);
/// How long `remove` waits for a metadata-less torrent to initialize before
/// giving up. Upstream `Session::delete` panics without resolved metadata, so
/// we refuse rather than crash.
const REMOVE_INIT_WAIT: Duration = Duration::from_secs(10);
/// Upper bound for a single `read_range` call. Ranges must be materialized
/// into one buffer, so cap them to avoid OOM from adversarial `Range`
/// headers. Video clients request ~1-8 MiB chunks; 64 MiB is generous.
pub const MAX_RANGE_BYTES: u64 = 64 << 20;

/// Suggested default download dir when the caller provides none:
/// `std::env::temp_dir().join("jlocal-torrents")`. The caller (parent) owns
/// this choice; this helper just spells it.
pub fn default_dir() -> PathBuf {
    std::env::temp_dir().join("jlocal-torrents")
}

/// Machine-readable failures. Returned inside `anyhow::Error` so handlers can
/// map them to HTTP statuses via `downcast_ref::<TorrentError>()`:
/// `NotFound` -> 404, `Unsatisfiable` -> 416, everything else -> 502/500.
#[derive(Debug)]
pub enum TorrentError {
    /// No managed torrent with this infohash hex.
    NotFound(String),
    /// The torrent exists but its metadata (file list) is not resolved yet.
    /// Retry after `POST /torrent/add` has returned.
    NoMetadata(String),
    /// Magnet metadata (or init) did not finish within the timeout. The
    /// torrent stays managed; retry the call later.
    MetadataTimeout(String),
    /// `file_idx` is out of range for this torrent's file list.
    BadFile { id: String, file: usize },
    /// Range cannot be satisfied by a file of `total` bytes (RFC 9110 `416`).
    Unsatisfiable { start: u64, end: u64, total: u64 },
    /// A single range larger than [`MAX_RANGE_BYTES`] was requested.
    RangeTooLarge { requested: u64, max: u64 },
}

impl std::fmt::Display for TorrentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TorrentError::NotFound(id) => write!(f, "torrent not found: {id}"),
            TorrentError::NoMetadata(id) => {
                write!(f, "torrent metadata not resolved yet: {id}")
            }
            TorrentError::MetadataTimeout(id) => {
                write!(f, "timed out waiting for torrent metadata: {id}")
            }
            TorrentError::BadFile { id, file } => {
                write!(f, "torrent {id} has no file index {file}")
            }
            TorrentError::Unsatisfiable { start, end, total } => {
                write!(
                    f,
                    "unsatisfiable range {start}-{end} for file of {total} bytes"
                )
            }
            TorrentError::RangeTooLarge { requested, max } => {
                write!(f, "range of {requested} bytes exceeds limit of {max} bytes")
            }
        }
    }
}
impl std::error::Error for TorrentError {}

/// One file inside a torrent. Serialized with exact contract field names.
#[derive(Debug, Clone, Serialize)]
pub struct TorrentFile {
    pub index: usize,
    pub path: String,
    pub size: u64,
}

/// Torrent summary for `GET /torrent/list`. Serialized with exact contract
/// field names (`downBps` camelCase).
#[derive(Debug, Clone, Serialize)]
pub struct TorrentInfo {
    /// Lowercase infohash hex.
    pub id: String,
    pub name: String,
    /// Total torrent bytes (0 while magnet metadata is unresolved).
    pub size: u64,
    /// 0.0..=1.0 fraction of verified bytes.
    pub progress: f64,
    /// librqbit state name: `initializing` | `live` | `paused` | `error`.
    pub state: String,
    #[serde(rename = "downBps")]
    pub down_bps: u64,
    /// Empty while magnet metadata is unresolved.
    pub files: Vec<TorrentFile>,
}

/// Live counters for `GET /torrent/stats/{id}`. Exact contract field names.
#[derive(Debug, Clone, Serialize)]
pub struct TorrentStats {
    /// Currently connected (live) peers; 0 unless the torrent is live.
    pub peers: u32,
    #[serde(rename = "downBps")]
    pub down_bps: u64,
    /// Verified downloaded bytes.
    pub downloaded: u64,
    /// 0.0..=1.0 fraction of verified bytes.
    pub progress: f64,
}

/// Validate + normalize a user-supplied torrent id: exactly 40 hex chars,
/// normalized to lowercase. Session-local numeric ids are intentionally
/// rejected: they are unstable across restarts, while infohash hex is not.
pub fn parse_id_hex(id: &str) -> anyhow::Result<Id20> {
    let id = id.trim();
    if id.len() != 40 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("invalid torrent id (want 40-char infohash hex): {id:?}");
    }
    Id20::from_str(&id.to_ascii_lowercase()).with_context(|| format!("invalid torrent id: {id:?}"))
}

/// Resolve `(start, end_inclusive, total)` to `(start, len)`.
///
/// Errors:
/// - empty file, `start > end`, or `end >= total` ->
///   [`TorrentError::Unsatisfiable`] (parent maps to `416`)
/// - `len > max_bytes` -> [`TorrentError::RangeTooLarge`]
pub fn resolve_range(
    start: u64,
    end_inclusive: u64,
    total: u64,
    max_bytes: u64,
) -> Result<(u64, u64), TorrentError> {
    if total == 0 || start > end_inclusive || end_inclusive >= total {
        return Err(TorrentError::Unsatisfiable {
            start,
            end: end_inclusive,
            total,
        });
    }
    // No overflow: end_inclusive < total <= u64::MAX, so end - start + 1 <= u64::MAX.
    let len = end_inclusive - start + 1;
    if len > max_bytes {
        return Err(TorrentError::RangeTooLarge {
            requested: len,
            max: max_bytes,
        });
    }
    Ok((start, len))
}

/// 0.0..=1.0 verified-bytes fraction. `total == 0` (unresolved magnet) -> 0.0.
pub fn progress_fraction(progress_bytes: u64, total_bytes: u64) -> f64 {
    if total_bytes == 0 {
        return 0.0;
    }
    ((progress_bytes as f64) / (total_bytes as f64)).min(1.0)
}
#[derive(Clone)]
pub struct TorrentManager {
    session: Arc<Session>,
    dir: PathBuf,
}

impl std::fmt::Debug for TorrentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TorrentManager")
            .field("dir", &self.dir)
            .finish()
    }
}

impl TorrentManager {
    /// Build a session downloading into `dir`:
    /// - DHT + trackers on (librqbit defaults), incoming TCP listener with
    ///   UPnP/NAT-PMP port mapping **off** (`enable_upnp_port_forwarding: false`)
    /// - session state (managed torrents, resume info) persisted as JSON under
    ///   `<dir>/session-state`; fastresume on so restarts skip re-hashing
    ///
    /// Must be called from async context (session startup spawns tasks).
    pub async fn new(dir: PathBuf) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("error creating torrent dir {}", dir.display()))?;
        let state_dir = dir.join("session-state");
        tokio::fs::create_dir_all(&state_dir)
            .await
            .with_context(|| format!("error creating session state dir {}", state_dir.display()))?;

        let listen = ListenerOptions {
            // Explicit: never punch holes in the user's NAT for a loopback companion.
            enable_upnp_port_forwarding: false,
            ..Default::default()
        };

        // `dht: Some(..)` (on) and `disable_trackers: false` (on) are already
        // the defaults; set explicitly so a future default change can't
        // silently flip us.
        let opts = SessionOptions {
            dht: Some(librqbit::DhtSessionConfig::default()),
            disable_trackers: false,
            listen: Some(listen),
            fastresume: true,
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(state_dir),
            }),
            ..Default::default()
        };

        let session = Session::new_with_opts(dir.clone(), opts)
            .await
            .context("error creating torrent session")?;
        Ok(Self { session, dir })
    }

    /// Download/output root this manager was built with.
    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    fn resolve(&self, id: &str) -> anyhow::Result<ManagedTorrentHandle> {
        let hash = parse_id_hex(id)?;
        self.session
            .get(TorrentIdOrHash::Hash(hash))
            .ok_or_else(|| anyhow::Error::from(TorrentError::NotFound(id.to_string())))
    }

    fn describe(&self, handle: &ManagedTorrentHandle) -> TorrentInfo {
        let stats = handle.stats();
        let id = handle.info_hash().as_string();
        let name = handle.name().unwrap_or_else(|| id.clone());
        let files = handle
            .with_metadata(|m| {
                m.info
                    .iter_file_details()
                    .enumerate()
                    .map(|(index, d)| TorrentFile {
                        index,
                        path: d.filename.to_string(),
                        size: d.len,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        TorrentInfo {
            id,
            name,
            size: stats.total_bytes,
            progress: progress_fraction(stats.progress_bytes, stats.total_bytes),
            state: stats.state.to_string(),
            down_bps: stats
                .live
                .as_ref()
                .map(|l| l.download_speed.as_bytes())
                .unwrap_or(0),
            files,
        }
    }

    /// Add a magnet link (managed torrent, resumed/persisted). Idempotent:
    /// re-adding a known magnet returns the existing torrent. Waits up to
    /// [`METADATA_TIMEOUT`] for metadata so the returned `name` is real.
    /// Returns `(id_hex, name)`.
    pub async fn add_magnet(&self, magnet: &str) -> anyhow::Result<(String, String)> {
        let response = self
            .session
            .add_torrent(
                AddTorrent::from_url(magnet),
                Some(AddTorrentOptions {
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
            .context("error adding torrent")?;
        let handle = match response {
            librqbit::AddTorrentResponse::Added(_, handle)
            | librqbit::AddTorrentResponse::AlreadyManaged(_, handle) => handle,
            librqbit::AddTorrentResponse::ListOnly(_) => {
                anyhow::bail!("unexpected list-only response adding torrent")
            }
        };
        let id_hex = handle.info_hash().as_string();
        match tokio::time::timeout(METADATA_TIMEOUT, handle.wait_until_initialized()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e).context(format!("torrent {id_hex} failed")),
            Err(_) => {
                return Err(anyhow::Error::from(TorrentError::MetadataTimeout(id_hex)));
            }
        }
        let name = handle.name().unwrap_or_else(|| id_hex.clone());
        Ok((id_hex, name))
    }

    /// Snapshot of all managed torrents (never blocks on the network).
    pub fn list(&self) -> Vec<TorrentInfo> {
        self.session
            .with_torrents(|torrents| torrents.map(|(_, mgr)| self.describe(mgr)).collect())
    }

    /// Read `range` (`end` **inclusive**) of file `file_idx`.
    /// Returns `(bytes, file_total)`.
    ///
    /// Reads go through librqbit's streaming engine, which prioritizes the
    /// requested file's 32 MiB lookahead window and pends until the pieces
    /// arrive -- i.e. reads ahead of the download **block** (like rqbit's own
    /// `/stream` endpoint) rather than failing. There is no timeout: a stalled
    /// swarm stalls the read, and the HTTP layer should apply its own deadline.
    pub async fn read_range(
        &self,
        id: &str,
        file_idx: usize,
        range: Range<u64>,
    ) -> anyhow::Result<(Vec<u8>, u64)> {
        let handle = self.resolve(id)?;
        let total = match handle
            .with_metadata(|m| m.info.iter_file_details().nth(file_idx).map(|d| d.len))
        {
            Ok(Some(len)) => len,
            Ok(None) => {
                return Err(anyhow::Error::from(TorrentError::BadFile {
                    id: id.to_string(),
                    file: file_idx,
                }));
            }
            Err(_) => {
                return Err(anyhow::Error::from(TorrentError::NoMetadata(
                    id.to_string(),
                )));
            }
        };
        let (start, len) = resolve_range(range.start, range.end, total, MAX_RANGE_BYTES)
            .map_err(anyhow::Error::from)?;
        let mut stream = handle
            .stream(file_idx)
            .await
            .with_context(|| format!("error opening stream for torrent {id} file {file_idx}"))?;
        if total != stream.len() {
            anyhow::bail!("torrent {id} file {file_idx} changed size mid-read");
        }
        stream
            .seek(SeekFrom::Start(start))
            .await
            .context("error seeking torrent stream")?;
        // len <= MAX_RANGE_BYTES (64 MiB): always fits in memory address space.
        let mut buf = vec![0u8; len as usize];
        stream
            .read_exact(&mut buf)
            .await
            .context("error reading torrent data")?;
        Ok((buf, total))
    }

    /// Download only `file_idx`, deprioritizing the rest of the torrent.
    ///
    /// librqbit exposes no explicit "critical window" API; the prioritization
    /// this relies on is (a) `update_only_files`, which stops fetching other
    /// files' pieces entirely, and (b) the streaming engine's 32 MiB lookahead
    /// around the active [`read_range`][Self::read_range] position, which is
    /// what actually pulls the selected file first.
    pub async fn select_file(&self, id: &str, file_idx: usize) -> anyhow::Result<()> {
        let handle = self.resolve(id)?;
        let count = match handle.with_metadata(|m| m.info.iter_file_details().count()) {
            Ok(n) => n,
            Err(_) => {
                return Err(anyhow::Error::from(TorrentError::NoMetadata(
                    id.to_string(),
                )));
            }
        };
        if file_idx >= count {
            return Err(anyhow::Error::from(TorrentError::BadFile {
                id: id.to_string(),
                file: file_idx,
            }));
        }
        // `update_only_files` rejects initializing torrents; metadata is
        // present, so init is (nearly) done -- wait for the transition.
        match tokio::time::timeout(SELECT_INIT_WAIT, handle.wait_until_initialized()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e).context(format!("torrent {id} failed")),
            Err(_) => {
                return Err(anyhow::Error::from(TorrentError::MetadataTimeout(
                    id.to_string(),
                )));
            }
        }
        self.session
            .update_only_files(&handle, &HashSet::from([file_idx]))
            .await
            .with_context(|| format!("error selecting file {file_idx} of torrent {id}"))
    }

    /// Live counters for one torrent.
    pub fn stats(&self, id: &str) -> anyhow::Result<TorrentStats> {
        let handle = self.resolve(id)?;
        let s = handle.stats();
        Ok(TorrentStats {
            peers: s
                .live
                .as_ref()
                .map(|l| l.snapshot.peer_stats.live)
                .unwrap_or(0),
            down_bps: s
                .live
                .as_ref()
                .map(|l| l.download_speed.as_bytes())
                .unwrap_or(0),
            downloaded: s.progress_bytes,
            progress: progress_fraction(s.progress_bytes, s.total_bytes),
        })
    }

    /// Drop the torrent and delete its data. Returns `false` when the id is
    /// unknown. Also returns `false` (without touching anything) when metadata
    /// never resolved: upstream `Session::delete` panics on metadata-less
    /// torrents, so refusing is the only safe option -- retry once metadata
    /// arrives or the torrent errors out and is re-added.
    pub async fn remove(&self, id: &str) -> bool {
        let handle = match self.resolve(id) {
            Ok(h) => h,
            Err(_) => return false,
        };
        if handle.metadata.load().as_ref().is_none() {
            let _ = tokio::time::timeout(REMOVE_INIT_WAIT, handle.wait_until_initialized()).await;
            if handle.metadata.load().as_ref().is_none() {
                return false;
            }
        }
        let hash = handle.info_hash();
        self.session
            .delete(TorrentIdOrHash::Hash(hash), true)
            .await
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librqbit::TorrentStatsState;

    #[test]
    fn id_parsing_accepts_lowercase_hex() {
        let id = "cab507494d02ebb1178b38f2e9d7be299c86b862";
        let parsed = parse_id_hex(id).unwrap();
        assert_eq!(parsed.as_string(), id);
    }

    #[test]
    fn id_parsing_normalizes_uppercase() {
        let upper = "CAB507494D02EBB1178B38F2E9D7BE299C86B862";
        let parsed = parse_id_hex(upper).unwrap();
        assert_eq!(parsed.as_string(), upper.to_ascii_lowercase());
    }

    #[test]
    fn id_parsing_rejects_garbage() {
        for bad in [
            "",
            "3",
            "cab507494d02ebb1178b38f2e9d7be299c86b86",
            "cab507494d02ebb1178b38f2e9d7be299c86b86200",
            "zab507494d02ebb1178b38f2e9d7be299c86b862",
            "cab507494d02ebb1178b38f2e9d7be299c86b86 ",
        ] {
            assert!(parse_id_hex(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn id_parsing_tolerates_surrounding_whitespace() {
        let padded = "  cab507494d02ebb1178b38f2e9d7be299c86b862\n";
        assert_eq!(parse_id_hex(padded).unwrap().as_string(), padded.trim());
    }

    #[test]
    fn range_math_normal_and_edges() {
        // Inclusive end: bytes 0-499 of 1000 -> (0, 500).
        assert_eq!(resolve_range(0, 499, 1000, u64::MAX).unwrap(), (0, 500));
        // Single last byte.
        assert_eq!(resolve_range(999, 999, 1000, u64::MAX).unwrap(), (999, 1));
        // Whole file.
        assert_eq!(resolve_range(0, 999, 1000, u64::MAX).unwrap(), (0, 1000));
        // Single-byte file.
        assert_eq!(resolve_range(0, 0, 1, u64::MAX).unwrap(), (0, 1));
    }

    #[test]
    fn range_math_rejects_unsatisfiable() {
        // end past EOF, start past EOF, inverted, empty file.
        for (start, end, total) in [
            (0, 1000, 1000),
            (1000, 1000, 1000),
            (500, 499, 1000),
            (0, 0, 0),
        ] {
            let err = resolve_range(start, end, total, u64::MAX).unwrap_err();
            assert!(
                matches!(err, TorrentError::Unsatisfiable { .. }),
                "({start}, {end}, {total}) -> {err:?}"
            );
        }
    }

    #[test]
    fn range_math_enforces_cap() {
        let err = resolve_range(0, 100, 1000, 64).unwrap_err();
        assert!(
            matches!(
                err,
                TorrentError::RangeTooLarge {
                    requested: 101,
                    max: 64
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn progress_fraction_edges() {
        assert_eq!(progress_fraction(0, 0), 0.0);
        assert_eq!(progress_fraction(50, 100), 0.5);
        assert_eq!(progress_fraction(100, 100), 1.0);
        assert_eq!(progress_fraction(150, 100), 1.0);
    }

    #[test]
    fn state_names_match_contract() {
        // Enum struct-variant fields are implicitly public, so all arms are
        // constructible here; these strings are what `TorrentInfo.state`
        // surfaces on the wire.
        assert_eq!(
            TorrentStatsState::Initializing { paused: false }.to_string(),
            "initializing"
        );
        assert_eq!(TorrentStatsState::Live.to_string(), "live");
        assert_eq!(TorrentStatsState::Paused.to_string(), "paused");
        assert_eq!(TorrentStatsState::Error.to_string(), "error");
    }

    #[test]
    fn typed_errors_survive_anyhow_for_handler_mapping() {
        let err: anyhow::Error = TorrentError::NotFound("abc".into()).into();
        assert!(err.downcast_ref::<TorrentError>().is_some());
        let err: anyhow::Error = TorrentError::Unsatisfiable {
            start: 0,
            end: 9,
            total: 5,
        }
        .into();
        assert!(matches!(
            err.downcast_ref::<TorrentError>(),
            Some(TorrentError::Unsatisfiable { .. })
        ));
    }

    #[test]
    fn serde_shapes_match_loopback_contract() {
        let info = TorrentInfo {
            id: "abc".into(),
            name: "n".into(),
            size: 10,
            progress: 0.5,
            state: "live".into(),
            down_bps: 7,
            files: vec![TorrentFile {
                index: 0,
                path: "a/b".into(),
                size: 10,
            }],
        };
        let v = serde_json::to_value(&info).unwrap();
        for key in [
            "id", "name", "size", "progress", "state", "downBps", "files",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert!(v.get("down_bps").is_none());
        let f = &v["files"][0];
        for key in ["index", "path", "size"] {
            assert!(f.get(key).is_some(), "missing file {key}");
        }

        let s = TorrentStats {
            peers: 3,
            down_bps: 9,
            downloaded: 5,
            progress: 0.5,
        };
        let v = serde_json::to_value(&s).unwrap();
        for key in ["peers", "downBps", "downloaded", "progress"] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
    }
}

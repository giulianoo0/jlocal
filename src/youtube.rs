//! YouTube links prepared on this machine: yt-dlp resolves, FFmpeg remuxes,
//! the HLS goes to the room's bucket exactly as the fleet would send it, but
//! from a residential address the CDN does not refuse. The tools are not in
//! the app bundle: they are fetched once, pinned by hash, into the app's
//! data directory.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use futures::StreamExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One file to fetch; `extract` names archive members and where they land.
struct Download {
    url: &'static str,
    sha256: &'static str,
    /// Empty means the download itself is the tool, saved under `dest`.
    extract: &'static [(&'static str, &'static str)],
    dest: &'static str,
}

/// Bumps when any pinned tool changes; a manifest with another set is stale.
const TOOL_SET: &str = "ytdlp-2026.08.19+ffmpeg-9.0.1";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const DOWNLOADS: &[Download] = &[
    Download {
        url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.08.19/yt-dlp_macos",
        sha256: "0f192b7ec147ab6288885d6351d9ab67367640029b4377576ef46dd79cf7b202",
        extract: &[],
        dest: "yt-dlp",
    },
    Download {
        url: "https://ffmpeg.martin-riedl.de/download/macos/arm64/1787073674_9.0.1/ffmpeg.zip",
        sha256: "8287a1b2229e05eb41859f073e18e6c52c60a778f2f5e6881070fe51b79407fe",
        extract: &[("ffmpeg", "ffmpeg")],
        dest: "",
    },
    Download {
        url: "https://ffmpeg.martin-riedl.de/download/macos/arm64/1787073674_9.0.1/ffprobe.zip",
        sha256: "102a26b8940a053298d9929bfaae71e4b6ef65ba5f19a99a88c433108560741a",
        extract: &[("ffprobe", "ffprobe")],
        dest: "",
    },
];

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const DOWNLOADS: &[Download] = &[
    Download {
        url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.08.19/yt-dlp_macos",
        sha256: "0f192b7ec147ab6288885d6351d9ab67367640029b4377576ef46dd79cf7b202",
        extract: &[],
        dest: "yt-dlp",
    },
    Download {
        url: "https://ffmpeg.martin-riedl.de/download/macos/amd64/1787081194_9.0.1/ffmpeg.zip",
        sha256: "5bdead62ff504ab9b447cc72b212c4fb481e3f7de5877d427a51bee8136dda40",
        extract: &[("ffmpeg", "ffmpeg")],
        dest: "",
    },
    Download {
        url: "https://ffmpeg.martin-riedl.de/download/macos/amd64/1787081194_9.0.1/ffprobe.zip",
        sha256: "34511bbcf1988ad2886023bf5ace4f44cf62e6defeb3d194d6f7619e5b061f7f",
        extract: &[("ffprobe", "ffprobe")],
        dest: "",
    },
];

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const DOWNLOADS: &[Download] = &[
    Download {
        url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.08.19/yt-dlp.exe",
        sha256: "66674953fe251b89f4d08c5f0e35e0728679bd67ab3d7d05c0562af101dd3e7a",
        extract: &[],
        dest: "yt-dlp.exe",
    },
    Download {
        url: "https://github.com/yt-dlp/FFmpeg-Builds/releases/download/autobuild-2026-09-11-17-43/ffmpeg-N-126504-g1b8a2b690b-win64-gpl.zip",
        sha256: "2d2b30a1e31bbc3dde699d95e699f6a32178febffdf2bf552e5d6384fed97859",
        extract: &[("bin/ffmpeg.exe", "ffmpeg.exe"), ("bin/ffprobe.exe", "ffprobe.exe")],
        dest: "",
    },
];

#[cfg(not(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "windows", target_arch = "x86_64")
)))]
const DOWNLOADS: &[Download] = &[];

#[cfg(windows)]
const EXE: &str = ".exe";
#[cfg(not(windows))]
const EXE: &str = "";

/// Where the tools live: the app's own data directory, never the bundle.
pub fn tools_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join("Library/Application Support/jlocal/tools")
    }
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA")
            .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
        PathBuf::from(base).join("jlocal").join("tools")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".local/share/jlocal/tools")
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ToolsStatus {
    /// This platform has no pinned tools.
    Unsupported,
    Missing,
    Downloading {
        done: u64,
        total: u64,
    },
    Ready,
    Failed {
        error: String,
    },
}

#[derive(Serialize)]
struct Manifest {
    set: String,
}

/// What the site sends to start or replace a run for a room.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    pub url: String,
    pub run_id: String,
    pub claim: String,
    pub room_id: String,
    pub media_generation: u64,
    #[serde(default)]
    pub region: u64,
    #[serde(default)]
    pub start_ms: u64,
    pub api_base: String,
}

pub struct Youtube {
    dir: PathBuf,
    status: Mutex<ToolsStatus>,
    remux: tokio::sync::Mutex<Option<Arc<ss_remux::Remux>>>,
    /// When each room's live was last asked about: the host tab polls every
    /// couple of seconds, so silence means the tab is gone and the live with it.
    live_seen: Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

impl std::fmt::Debug for Youtube {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Youtube")
            .field("dir", &self.dir)
            .field("status", &self.status())
            .finish()
    }
}

impl Youtube {
    pub fn new() -> Arc<Self> {
        let dir = tools_dir();
        let status = if DOWNLOADS.is_empty() {
            ToolsStatus::Unsupported
        } else if Self::installed(&dir) {
            ToolsStatus::Ready
        } else {
            ToolsStatus::Missing
        };
        Arc::new(Self {
            dir,
            status: Mutex::new(status),
            remux: tokio::sync::Mutex::new(None),
            live_seen: Mutex::new(std::collections::HashMap::new()),
        })
    }

    fn installed(dir: &Path) -> bool {
        let manifest = std::fs::read_to_string(dir.join("manifest.json")).ok();
        let set = manifest
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| v["set"].as_str().map(str::to_string));
        set.as_deref() == Some(TOOL_SET)
            && ["yt-dlp", "ffmpeg", "ffprobe"]
                .iter()
                .all(|name| dir.join(format!("{name}{EXE}")).is_file())
    }

    pub fn status(&self) -> ToolsStatus {
        self.status.lock().clone()
    }

    pub fn ready(&self) -> bool {
        matches!(*self.status.lock(), ToolsStatus::Ready)
    }

    fn tool(&self, name: &str) -> String {
        self.dir
            .join(format!("{name}{EXE}"))
            .to_string_lossy()
            .into_owned()
    }

    /// Starts the download unless one is running or the set is already there.
    pub fn start_install(self: &Arc<Self>) -> ToolsStatus {
        {
            let mut status = self.status.lock();
            match *status {
                ToolsStatus::Unsupported | ToolsStatus::Ready | ToolsStatus::Downloading { .. } => {
                    return status.clone()
                }
                _ => *status = ToolsStatus::Downloading { done: 0, total: 0 },
            }
        }
        let this = self.clone();
        tokio::spawn(async move {
            match this.install().await {
                Ok(()) => {
                    *this.status.lock() = ToolsStatus::Ready;
                    tracing::info!(dir = %this.dir.display(), "youtube tools installed");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "youtube tools install failed");
                    *this.status.lock() = ToolsStatus::Failed {
                        error: e.to_string().chars().take(300).collect(),
                    };
                }
            }
        });
        self.status()
    }

    async fn install(&self) -> anyhow::Result<()> {
        let scratch = self.dir.join(".dl");
        tokio::fs::create_dir_all(&scratch)
            .await
            .context("tools dir")?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .user_agent(concat!("jlocal/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let mut done_before = 0u64;
        for (index, download) in DOWNLOADS.iter().enumerate() {
            let path = scratch.join(format!("{index}.bin"));
            let response = client.get(download.url).send().await?.error_for_status()?;
            let total_this = response.content_length().unwrap_or(0);
            // The total only counts what is known; earlier files add their real size.
            let mut hasher = Sha256::new();
            let mut file = tokio::fs::File::create(&path).await?;
            let mut stream = response.bytes_stream();
            let mut got = 0u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                hasher.update(&chunk);
                tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
                got += chunk.len() as u64;
                *self.status.lock() = ToolsStatus::Downloading {
                    done: done_before + got,
                    total: done_before + total_this.max(got),
                };
            }
            drop(file);
            let digest = hex::encode(hasher.finalize());
            if digest != download.sha256 {
                let _ = tokio::fs::remove_file(&path).await;
                anyhow::bail!(
                    "{} does not match its pinned hash",
                    download.url.rsplit('/').next().unwrap_or(download.url)
                );
            }
            done_before += got;
            if download.extract.is_empty() {
                let dest = self.dir.join(download.dest);
                tokio::fs::rename(&path, &dest).await?;
                set_executable(&dest)?;
            } else {
                let bytes = tokio::fs::read(&path).await?;
                for (member, dest_name) in download.extract {
                    let out = extract_member(&bytes, member)
                        .with_context(|| format!("{member} in {}", download.url))?;
                    let dest = self.dir.join(dest_name);
                    tokio::fs::write(&dest, out).await?;
                    set_executable(&dest)?;
                }
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
        let _ = tokio::fs::remove_dir_all(&scratch).await;
        let manifest = serde_json::to_vec(&Manifest {
            set: TOOL_SET.into(),
        })?;
        tokio::fs::write(self.dir.join("manifest.json"), manifest).await?;
        Ok(())
    }

    async fn remux(&self) -> anyhow::Result<Arc<ss_remux::Remux>> {
        if !self.ready() {
            anyhow::bail!("tools_missing");
        }
        let mut held = self.remux.lock().await;
        if let Some(remux) = held.as_ref() {
            return Ok(remux.clone());
        }
        let cfg = ss_remux::RemuxConfig {
            data_dir: std::env::temp_dir().join("jlocal-remux"),
            ffmpeg_path: self.tool("ffmpeg"),
            ffprobe_path: self.tool("ffprobe"),
            slots: 2,
            spool_bytes: 512 << 20,
            object_bytes: 256 << 20,
            put_concurrency: 4,
            put_global: 8,
            live_slots: 1,
            youtube: Some(ss_remux::youtube::Config {
                ytdlp_path: self.tool("yt-dlp"),
                proxy: None,
                cookies_file: None,
                ca_file: self.ca_bundle(),
            }),
        };
        let remux = ss_remux::Remux::new(cfg).await;
        if !remux.enabled() {
            anyhow::bail!("ffmpeg does not run");
        }
        if remux.youtube.is_none() {
            anyhow::bail!("yt-dlp does not run");
        }
        *held = Some(remux.clone());
        Ok(remux)
    }

    pub async fn resolve(
        &self,
        url: &str,
    ) -> Result<ss_remux::youtube::Summary, ss_remux::youtube::Error> {
        let remux = self
            .remux()
            .await
            .map_err(|e| ss_remux::youtube::Error::Tool(e.to_string()))?;
        let resolver = remux
            .youtube
            .clone()
            .ok_or_else(|| ss_remux::youtube::Error::Tool("yt-dlp missing".into()))?;
        resolver.summary(url).await
    }

    /// Starts (or replaces, per room) a run; the crate cancels the room's
    /// previous run itself, which is how a seek arrives.
    pub async fn run(&self, req: RunRequest) -> anyhow::Result<()> {
        let remux = self.remux().await?;
        let spec: ss_remux::protocol::Spec = serde_json::from_value(serde_json::json!({
            "protocolVersion": ss_remux::protocol::PROTOCOL_VERSION,
            "runId": req.run_id,
            "claim": req.claim,
            "mediaGeneration": req.media_generation,
            "region": req.region,
            "startMs": req.start_ms,
            "apiBase": req.api_base.trim_end_matches('/'),
            "roomId": req.room_id,
        }))?;
        remux
            .start(
                ss_remux::RunInput::Youtube(ss_remux::youtube::Request { url: req.url }),
                spec,
            )
            .await
    }

    pub async fn run_status(&self, run_id: &str) -> Option<ss_remux::RunStatus> {
        let remux = self.remux.lock().await.clone()?;
        remux.status(run_id)
    }

    pub async fn cancel(&self, run_id: &str) -> bool {
        let Some(remux) = self.remux.lock().await.clone() else {
            return false;
        };
        remux.cancel(run_id).await
    }

    pub async fn runs(&self) -> Vec<serde_json::Value> {
        match self.remux.lock().await.clone() {
            Some(remux) => remux.runs(),
            None => Vec::new(),
        }
    }
}

fn extract_member(archive: &[u8], member: &str) -> anyhow::Result<Vec<u8>> {
    let cursor = std::io::Cursor::new(archive);
    let mut zip = zip::ZipArchive::new(cursor)?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        let name = file.name().replace('\\', "/");
        if name == member || name.ends_with(&format!("/{member}")) {
            let mut out = Vec::with_capacity(file.size().min(256 << 20) as usize);
            std::io::Read::read_to_end(&mut file, &mut out)?;
            return Ok(out);
        }
    }
    anyhow::bail!("archive has no {member}")
}

#[cfg(unix)]
fn set_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// The runs a room may still be producing, for tests and the health line.
pub fn runs_by_room(runs: &[serde_json::Value]) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for run in runs {
        if let Some(room) = run["roomId"].as_str() {
            *out.entry(room.to_string()).or_insert(0) += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zip_member_is_found_by_suffix() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();
            writer
                .start_file("ffmpeg-N-1/bin/ffmpeg.exe", options)
                .unwrap();
            std::io::Write::write_all(&mut writer, b"MZ-fake").unwrap();
            writer.start_file("ffmpeg", options).unwrap();
            std::io::Write::write_all(&mut writer, b"ELF-fake").unwrap();
            writer.finish().unwrap();
        }
        let archive = buf.into_inner();
        assert_eq!(
            extract_member(&archive, "bin/ffmpeg.exe").unwrap(),
            b"MZ-fake"
        );
        assert_eq!(extract_member(&archive, "ffmpeg").unwrap(), b"ELF-fake");
        assert!(extract_member(&archive, "ffprobe").is_err());
    }

    #[test]
    fn an_empty_dir_is_missing_and_a_manifest_alone_is_not_ready() {
        let dir = std::env::temp_dir().join(format!("jlocal-tools-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!Youtube::installed(&dir));
        std::fs::write(
            dir.join("manifest.json"),
            format!("{{\"set\":\"{TOOL_SET}\"}}"),
        )
        .unwrap();
        assert!(!Youtube::installed(&dir));
        for name in ["yt-dlp", "ffmpeg", "ffprobe"] {
            std::fs::write(dir.join(format!("{name}{EXE}")), b"x").unwrap();
        }
        assert!(Youtube::installed(&dir));
        std::fs::write(dir.join("manifest.json"), "{\"set\":\"old\"}").unwrap();
        assert!(!Youtube::installed(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A live nobody asked about for this long has lost its tab.
const LIVE_ORPHAN_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

/// A live to put on the relay: the page, the relay URL carrying the publish
/// token the site fetched, and the broadcast name the server assigned.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveRequest {
    pub url: String,
    pub relay: String,
    pub broadcast: String,
    pub room_id: String,
}

impl Youtube {
    /// Stops every live whose tab has not asked about it for a while: a
    /// host closing the browser leaves no other trace, and the slot must
    /// come back.
    async fn reap_lives(&self, remux: &ss_remux::Remux) {
        let stale: Vec<String> = self
            .live_seen
            .lock()
            .iter()
            .filter(|(_, seen)| seen.elapsed() > LIVE_ORPHAN_AFTER)
            .map(|(room, _)| room.clone())
            .collect();
        for room in stale {
            tracing::info!(room, "live orphaned by its tab; stopping");
            remux.stop_live(&room);
            self.live_seen.lock().remove(&room);
        }
    }

    /// The periodic sweep: nothing to do while no remux exists.
    pub async fn reap_orphans(&self) {
        let remux = self.remux.lock().await.clone();
        if let Some(remux) = remux {
            self.reap_lives(&remux).await;
        }
    }

    pub async fn live_start(&self, req: LiveRequest) -> anyhow::Result<()> {
        let remux = self.remux().await?;
        self.reap_lives(&remux).await;
        self.live_seen
            .lock()
            .insert(req.room_id.clone(), std::time::Instant::now());
        remux
            .start_live(
                &req.room_id,
                ss_remux::live::Request {
                    url: req.url,
                    relay: req.relay,
                    broadcast: req.broadcast,
                },
            )
            .map_err(|code| anyhow::anyhow!("{code}"))
    }

    pub async fn live_state(&self, room: &str) -> Option<ss_remux::live::State> {
        let remux = self.remux.lock().await.clone()?;
        if let Some(seen) = self.live_seen.lock().get_mut(room) {
            *seen = std::time::Instant::now();
        }
        self.reap_lives(&remux).await;
        remux.live_state(room)
    }

    pub async fn live_stop(&self, room: &str) -> bool {
        self.live_seen.lock().remove(room);
        let Some(remux) = self.remux.lock().await.clone() else {
            return false;
        };
        remux.stop_live(room)
    }
}

impl Youtube {
    /// The system's trust roots as a PEM file for the static FFmpeg, which
    /// carries none of its own: written once next to the tools, refreshed
    /// after a month. None when nothing could be exported; FFmpeg then fails
    /// verification and the site falls back to the fleet.
    fn ca_bundle(&self) -> Option<std::path::PathBuf> {
        let path = self.dir.join("ca.pem");
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(|m| {
                m.elapsed()
                    .map(|age| age.as_secs() < 30 * 24 * 3600)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if fresh {
            return Some(path);
        }
        use base64::Engine as _;
        let mut pem = String::new();
        let mut count = 0;
        for cert in rustls_native_certs::load_native_certs().certs {
            let encoded = base64::engine::general_purpose::STANDARD.encode(cert.as_ref());
            pem.push_str("-----BEGIN CERTIFICATE-----\n");
            for line in encoded.as_bytes().chunks(64) {
                pem.push_str(std::str::from_utf8(line).unwrap_or(""));
                pem.push('\n');
            }
            pem.push_str("-----END CERTIFICATE-----\n");
            count += 1;
        }
        if count == 0 {
            tracing::warn!("no system trust roots to hand ffmpeg");
            return None;
        }
        match std::fs::write(&path, pem) {
            Ok(()) => {
                tracing::info!(roots = count, path = %path.display(), "trust roots written for ffmpeg");
                Some(path)
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not write the trust roots for ffmpeg");
                None
            }
        }
    }
}

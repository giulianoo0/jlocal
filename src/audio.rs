//! Audio mode + app list (best-effort).
//!
//! Honesty contract — read before extending this file:
//!
//! - [`list_apps`] returns a **process-label list**, not an audio-session
//!   tap. It enumerates running processes via `sysinfo` and applies a small
//!   GUI heuristic ([`is_gui_candidate`]). No OS exposes per-application
//!   audio sessions to a plain user-space process, so the returned entries
//!   MUST be presented as "apps we can label", never as "apps we can
//!   isolate in the mix".
//! - System-audio capture is NOT implemented here. [`probe_inputs`] only
//!   lists what `cpal` can see (microphones / offered inputs). See
//!   [`MATRIX`] for the per-OS reality and
//!   [`AudioState::desired_loopback_kind`] for the intent a future capture
//!   pipeline should read.
//!
//! The parent (`api.rs` / `status.rs`) owns HTTP + shared state; this module
//! is pure logic plus best-effort OS probing, so there are no axum imports.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// What the user wants to hear in the shared/streamed mix.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioMode {
    /// Capture the full mix (whatever the future pipeline can tap).
    #[default]
    All,
    /// Capture nothing.
    None,
    /// Capture the mix minus [`AudioState::muted_apps`].
    Custom,
}

/// Mutable audio selection. Lives in shared state (parent-owned); the mode
/// persists across sessions only as long as the parent keeps it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioState {
    pub mode: AudioMode,
    /// Muted application ids (see [`AppEntry::id`]). Only consulted when
    /// `mode == AudioMode::Custom`; preserved across mode switches.
    pub muted_apps: HashSet<String>,
}

impl Default for AudioState {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioState {
    pub fn new() -> Self {
        Self {
            mode: AudioMode::All,
            muted_apps: HashSet::new(),
        }
    }

    /// Switch capture mode. The mute set is preserved so toggling back to
    /// `Custom` restores the previous selection.
    pub fn set_mode(&mut self, mode: AudioMode) {
        self.mode = mode;
    }

    /// Flip the muted flag for one app id. Returns `true` when the app is
    /// muted after the toggle. Does not touch [`AudioState::mode`]: muting
    /// while in `All`/`None` only takes effect once the mode is `Custom`.
    pub fn toggle_app(&mut self, app_id: &str) -> bool {
        if self.muted_apps.remove(app_id) {
            false
        } else {
            self.muted_apps.insert(app_id.to_string());
            true
        }
    }

    /// Whether `app_id` would be audible in the current mix.
    pub fn is_audible(&self, app_id: &str) -> bool {
        match self.mode {
            AudioMode::All => true,
            AudioMode::None => false,
            AudioMode::Custom => !self.muted_apps.contains(app_id),
        }
    }

    /// Capture intent for the future ingest pipeline (which owns the actual
    /// device handling). The pipeline reads `mode` for the coarse intent and
    /// `muted_apps` for the [`LoopbackKind::Filtered`] exclusion set.
    pub fn desired_loopback_kind(&self) -> LoopbackKind {
        match self.mode {
            AudioMode::All => LoopbackKind::All,
            AudioMode::None => LoopbackKind::None,
            AudioMode::Custom => LoopbackKind::Filtered,
        }
    }
}

/// Capture intent derived from [`AudioMode`]. Consumed by the future capture
/// pipeline (owned by the publish side), not acted on here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum LoopbackKind {
    /// Ingest the full system mix.
    All,
    /// Ingest nothing (stay silent).
    None,
    /// Ingest the mix and drop `muted_apps` at the mix stage. Where the OS
    /// offers no per-app taps (see [`MATRIX`]) the pipeline can only honour
    /// this as `All` and MUST surface that downgrade to the UI.
    Filtered,
}

/// One listable "app". `id` is the stable mute key (lowercased process name);
/// `name` is the display label (original process name). Both come from the
/// process table — label-only, NOT an audio-session handle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
}

/// Per-OS capture reality. Keep conservative: a flag flips only on real
/// readiness, and this table is the evidence for why a tap is claimed or not.
pub struct OsAudioSupport {
    pub os: &'static str,
    pub system_audio: &'static str,
    pub microphone: &'static str,
}

pub const MATRIX: &[OsAudioSupport] = &[
    OsAudioSupport {
        os: "windows",
        system_audio: "REAL via WASAPI loopback (AUDCLNT_STREAMFLAGS_LOOPBACK), \
            but cpal does not set the loopback flag, so this needs the `wasapi` \
            crate or raw WASAPI bindings — not cpal alone.",
        microphone: "Available via cpal input devices.",
    },
    OsAudioSupport {
        os: "macos",
        system_audio: "UNAVAILABLE via cpal. System-audio tap needs a virtual \
            device (e.g. BlackHole) or ScreenCaptureKit audio (macOS 13+, native \
            bindings — out of scope for this module).",
        microphone: "Available via cpal input devices (mic only).",
    },
    OsAudioSupport {
        os: "linux",
        system_audio: "PipeWire monitor sources act as loopback where PipeWire \
            runs; plain ALSA has no loopback (needs snd-aloop). cpal only sees \
            whatever source the host offers.",
        microphone: "Available via cpal input devices.",
    },
];

/// Best-effort app list from the process table. Never fails: returns an
/// empty vec when the table is unreadable. Sorted by (lowercased) name and
/// capped so the HTTP payload stays small.
///
/// This is a label list for the mute UI, NOT proof of per-app audio taps.
pub fn list_apps() -> Vec<AppEntry> {
    list_apps_from_snapshot(&sysinfo_snapshot())
}

/// GUI heuristic over one `(process name, executable path)` pair.
///
/// - Empty names and `[bracketed]` kernel threads (Linux) are never apps.
/// - On macOS, real GUI apps run from inside an `*.app` bundle; CLI daemons
///   do not. Requiring the bundle marker keeps the list genuinely app-like.
/// - Elsewhere `sysinfo` offers no GUI marker, so every named user process
///   is listed — honestly label-only (see module docs).
fn is_gui_candidate(name: &str, exe: Option<&std::path::Path>) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.starts_with('[') && name.ends_with(']') {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        match exe {
            Some(path) => path.to_string_lossy().contains(".app/"),
            None => false,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = exe;
        true
    }
}

fn sysinfo_snapshot() -> Vec<(String, Option<std::path::PathBuf>)> {
    let mut system = sysinfo::System::new_all();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    system
        .processes()
        .values()
        .map(|process| {
            (
                process.name().to_string_lossy().into_owned(),
                process.exe().map(|path| path.to_path_buf()),
            )
        })
        .collect()
}

fn list_apps_from_snapshot(snapshot: &[(String, Option<std::path::PathBuf>)]) -> Vec<AppEntry> {
    const MAX_ENTRIES: usize = 256;
    let mut seen: HashSet<String> = HashSet::new();
    let mut entries: HashMap<String, AppEntry> = HashMap::new();
    for (name, exe) in snapshot {
        if !is_gui_candidate(name, exe.as_deref()) {
            continue;
        }
        let id = name.to_lowercase();
        if seen.insert(id.clone()) {
            entries.insert(
                id.clone(),
                AppEntry {
                    id,
                    name: name.clone(),
                },
            );
        }
    }
    let mut apps: Vec<AppEntry> = entries.into_values().collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps.truncate(MAX_ENTRIES);
    apps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&AudioMode::All).unwrap(), "\"all\"");
        assert_eq!(serde_json::to_string(&AudioMode::None).unwrap(), "\"none\"");
        assert_eq!(
            serde_json::to_string(&AudioMode::Custom).unwrap(),
            "\"custom\""
        );
        assert_eq!(
            serde_json::from_str::<AudioMode>("\"custom\"").unwrap(),
            AudioMode::Custom
        );
    }

    #[test]
    fn default_state_hears_everything() {
        let state = AudioState::new();
        assert_eq!(state.mode, AudioMode::All);
        assert!(state.is_audible("safari"));
        assert!(state.is_audible("anything-unknown"));
    }

    #[test]
    fn none_mode_mutes_everything() {
        let mut state = AudioState::new();
        state.set_mode(AudioMode::None);
        assert!(!state.is_audible("safari"));
        assert!(!state.is_audible("anything-unknown"));
    }

    #[test]
    fn custom_mode_honours_mute_set() {
        let mut state = AudioState::new();
        state.set_mode(AudioMode::Custom);
        assert!(state.is_audible("safari"));
        assert!(state.toggle_app("safari"));
        assert!(!state.is_audible("safari"));
        assert!(state.is_audible("music"));
        // Toggling again unmutes.
        assert!(!state.toggle_app("safari"));
        assert!(state.is_audible("safari"));
    }

    #[test]
    fn mute_set_survives_mode_switches() {
        let mut state = AudioState::new();
        state.set_mode(AudioMode::Custom);
        state.toggle_app("safari");
        state.set_mode(AudioMode::All);
        assert!(state.is_audible("safari"));
        state.set_mode(AudioMode::Custom);
        assert!(!state.is_audible("safari"));
    }

    #[test]
    fn loopback_kind_follows_mode() {
        let mut state = AudioState::new();
        assert_eq!(state.desired_loopback_kind(), LoopbackKind::All);
        state.set_mode(AudioMode::None);
        assert_eq!(state.desired_loopback_kind(), LoopbackKind::None);
        state.set_mode(AudioMode::Custom);
        assert_eq!(state.desired_loopback_kind(), LoopbackKind::Filtered);
    }

    #[test]
    fn gui_heuristic_rejects_noise() {
        assert!(!is_gui_candidate("", None));
        assert!(!is_gui_candidate("[kthreadd]", None));
        #[cfg(not(target_os = "macos"))]
        {
            assert!(is_gui_candidate("firefox", None));
        }
    }

    #[test]
    fn list_dedupes_sorts_and_caps() {
        let snapshot = vec![
            ("b-app".to_string(), None),
            ("A-app".to_string(), None),
            ("b-app".to_string(), None),
            ("".to_string(), None),
            ("[kworker]".to_string(), None),
        ];
        let apps = list_apps_from_snapshot(&snapshot);
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(apps.len(), 2);
            assert_eq!(apps[0].name, "A-app");
            assert_eq!(apps[0].id, "a-app");
            assert_eq!(apps[1].id, "b-app");
        }
        #[cfg(target_os = "macos")]
        {
            // No bundle paths in this snapshot, so nothing qualifies.
            assert!(apps.is_empty());
        }
    }

    #[test]
    fn matrix_covers_three_desktop_oses() {
        let oss: Vec<&str> = MATRIX.iter().map(|entry| entry.os).collect();
        assert!(oss.contains(&"windows"));
        assert!(oss.contains(&"macos"));
        assert!(oss.contains(&"linux"));
        for entry in MATRIX {
            assert!(!entry.system_audio.is_empty());
            assert!(!entry.microphone.is_empty());
        }
    }
}

//! Per-app system-audio capture mixed to one s16le 48kHz stereo PCM stream.
//!
//! Stream contract (read before extending this file):
//!
//! - Format: raw little-endian `i16`, 48 kHz, stereo interleaved. No framing
//!   headers, no container — the body `GET /audio/stream` serves is exactly
//!   [`BYTES_PER_CHUNK`] bytes per ~20 ms tick ([`FRAMES_PER_CHUNK`] frames).
//! - Lifecycle: the tap runs while a [`crate::capture::CaptureSession`] is
//!   live. No session → `404 {error:"idle"}`; Linux (unwired) → `501`.
//!   Silence keeps the stream open so the browser `AudioContext` graph never
//!   starves: `none` mode emits silence, and so does any tick with no tap
//!   data (tap starting, permission blocked, momentary underflow).
//! - Mute semantics: `all` = full mix, `none` = silence, `custom` = mix
//!   minus [`crate::audio::AudioState::muted_apps`]. App names are the same
//!   ids `/capture/windows` reports in its `app` field (lowercased process
//!   names). Mode/mute changes take effect live: the HTTP loop snapshots
//!   [`crate::audio::AudioState`] every tick, and the OS-level exclusion set
//!   is pushed via [`AudioTap::set_excluded`].
//!
//! Platform reality:
//!
//! - macOS: one ScreenCaptureKit `SCStream` (`capturesAudio`, 48 kHz stereo)
//!   over the primary display. SCK delivers a single system mix — not
//!   per-app streams — so `custom` mutes are enforced with an
//!   `excludingApplications` content filter rebuilt live when the mute set
//!   changes. Needs Screen Recording consent (the same TCC bit the video
//!   path already prompts for, so no new prompt).
//! - Windows: one WASAPI process-loopback tap per audible GUI process
//!   (`ActivateAudioInterfaceAsync` + `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_
//!   PROCESS_TREE`), each decoded to 48 kHz stereo and mixed here with the
//!   muted set dropped. A tap that fails to open is skipped — the mix plays
//!   on with whoever is audible.
//! - Linux: unwired stub — [`capture_supported`] is false and
//!   [`AudioTap::start`] errors, so the route stays `501`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::audio::{AudioMode, AudioState};

/// Output sample rate (Hz). Matches `SCStreamConfiguration.sampleRate` and
/// the WASAPI mix-format target: taps convert at the edge, the mixer and
/// the HTTP framing only ever see this rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Output channel count (stereo, interleaved L R L R …).
pub const CHANNELS: u32 = 2;
/// One stream tick. The HTTP handler emits one chunk per tick so browser
/// latency stays ~20 ms.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(20);
/// PCM frames per tick (`48000 * 20 / 1000`).
pub const FRAMES_PER_CHUNK: usize = 960;
/// `i16` samples per tick (frames × stereo).
pub const SAMPLES_PER_CHUNK: usize = FRAMES_PER_CHUNK * CHANNELS as usize;
/// Body bytes per tick (samples × 2 bytes, little-endian).
pub const BYTES_PER_CHUNK: usize = SAMPLES_PER_CHUNK * 2;
/// `Content-Type` for the raw PCM body (RFC 2046 `audio/L16` parameters).
pub const CONTENT_TYPE: &str = "audio/L16; rate=48000; channels=2";
/// Whether this OS has a capture backend. macOS only for now: Windows is
/// stubbed until its loopback taps can be compile-checked on a Windows host
/// (see the Windows note at `platform_start`); Linux was never wired. The
/// caps flag `audio.capture` rides on this.
pub fn capture_supported() -> bool {
    cfg!(target_os = "macos")
}

/// One tap's decoded audio: stereo-interleaved `i16` at [`SAMPLE_RATE`].
/// `app` is the `/capture/windows` `app` id (lowercased process name); the
/// macOS SCK tap reports the whole-system mix as `"system"`.
#[derive(Clone, Debug)]
pub struct AppFrame {
    pub app: String,
    pub samples: Vec<i16>,
}

/// All-silence chunk samples (one tick).
pub fn silence_frame() -> Vec<i16> {
    vec![0; SAMPLES_PER_CHUNK]
}

/// Encode `i16` samples as little-endian bytes.
pub fn i16_to_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Fit any mixed sample run to exactly one stream tick: truncate overflow
/// (bounds latency under tap bursts), zero-pad underflow (the graph never
/// starves).
pub fn fit_chunk(samples: &[i16]) -> [u8; BYTES_PER_CHUNK] {
    let mut chunk = [0u8; BYTES_PER_CHUNK];
    let take = samples.len().min(SAMPLES_PER_CHUNK);
    for (i, s) in samples[..take].iter().enumerate() {
        chunk[2 * i..2 * i + 2].copy_from_slice(&s.to_le_bytes());
    }
    chunk
}

/// One stream tick's worth of framing: append freshly mixed samples to the
/// backlog, keep at most two ticks (older audio is dropped so a tap burst
/// can't build latency), and emit exactly one [`BYTES_PER_CHUNK`] chunk,
/// padding underflow with silence. Pure: unit-tested with fake frames.
pub fn push_tick(pending: &mut Vec<i16>, mixed: Vec<i16>) -> [u8; BYTES_PER_CHUNK] {
    pending.extend(mixed);
    let cap = 2 * SAMPLES_PER_CHUNK;
    if pending.len() > cap {
        pending.drain(..pending.len() - cap);
    }
    let take = pending.len().min(SAMPLES_PER_CHUNK);
    let samples: Vec<i16> = pending.drain(..take).collect();
    fit_chunk(&samples)
}

/// Duplicate mono to stereo interleaved (`L R` with `R == L`).
pub fn mono_to_stereo(mono: &[i16]) -> Vec<i16> {
    let mut out = Vec::with_capacity(mono.len() * 2);
    for s in mono {
        out.push(*s);
        out.push(*s);
    }
    out
}

/// Resample `input` (interleaved, `channels` ∈ {1, 2}) from `from_rate` to
/// [`SAMPLE_RATE`] stereo, with linear interpolation. Degenerate input
/// (empty, zero rate, other channel counts) yields silence (empty — the
/// caller pads). Pure math: fully unit-testable without any OS tap.
pub fn resample_to_stereo_48k(input: &[i16], from_rate: u32, channels: u16) -> Vec<i16> {
    if input.is_empty() || from_rate == 0 || (channels != 1 && channels != 2) {
        return Vec::new();
    }
    let channels = channels as usize;
    let frames_in = input.len() / channels;
    if frames_in == 0 {
        return Vec::new();
    }
    if from_rate == SAMPLE_RATE && channels == 2 {
        return input[..frames_in * 2].to_vec();
    }
    if from_rate == SAMPLE_RATE {
        // Mono at target rate: just duplicate.
        return mono_to_stereo(&input[..frames_in]);
    }
    let frames_out = (frames_in as u64 * u64::from(SAMPLE_RATE) / u64::from(from_rate)) as usize;
    if frames_out == 0 {
        return Vec::new();
    }
    let at = |frame: usize, ch: usize| -> i32 { input[frame * channels + ch] as i32 };
    let mut out = Vec::with_capacity(frames_out * 2);
    for i in 0..frames_out {
        // Fixed-point source position with linear blend.
        let pos = i as u64 * (frames_in as u64 - 1) / (frames_out as u64).max(1);
        let idx = (pos as usize).min(frames_in - 1);
        let next = (idx + 1).min(frames_in - 1);
        let frac_num = (i as u64 * (frames_in as u64 - 1) % (frames_out as u64).max(1)) as i32;
        let frac_den = frames_out.max(1) as i32;
        for ch in 0..2 {
            let src = if channels == 2 { ch } else { 0 };
            let a = at(idx, src);
            let b = at(next, src);
            out.push((a + (b - a) * frac_num / frac_den) as i16);
        }
    }
    out
}

/// Convert planar `f32` channels (`-1.0..=1.0`) at `from_rate` to stereo
/// `i16` at [`SAMPLE_RATE`]: takes L/R (duplicates mono, drops beyond
/// stereo), clamps, scales, then resamples. This is the SCK/WASAPI edge —
/// the OS hands us float audio, the stream wants `s16le`.
pub fn f32_planar_to_s16_stereo(channels: &[&[f32]], from_rate: u32) -> Vec<i16> {
    if channels.is_empty() || from_rate == 0 {
        return Vec::new();
    }
    let frames = channels.iter().map(|c| c.len()).min().unwrap_or(0);
    if frames == 0 {
        return Vec::new();
    }
    let left = channels[0];
    let right = if channels.len() > 1 {
        channels[1]
    } else {
        channels[0]
    };
    let mut stereo = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        stereo.push((left[i].clamp(-1.0, 1.0) * 32767.0) as i16);
        stereo.push((right[i].clamp(-1.0, 1.0) * 32767.0) as i16);
    }
    resample_to_stereo_48k(&stereo, from_rate, 2)
}

/// Mix tap frames per the live [`AudioState`]: `all` mixes everything,
/// `none` yields silence (empty — the framer pads), `custom` drops frames
/// whose `app` is muted. Samples saturate on overflow; short frames are
/// zero-padded to the longest audible frame. Pure math over synthetic
/// frames: unit-tested below, no OS needed.
pub fn mix_frames(frames: &[AppFrame], state: &AudioState) -> Vec<i16> {
    if state.mode == AudioMode::None {
        return Vec::new();
    }
    let mut out: Vec<i16> = Vec::new();
    for frame in frames {
        if !state.is_audible(&frame.app) {
            continue;
        }
        if frame.samples.len() > out.len() {
            out.resize(frame.samples.len(), 0);
        }
        for (mixed, sample) in out.iter_mut().zip(frame.samples.iter()) {
            *mixed = mixed.saturating_add(*sample);
        }
    }
    out
}

// ---- Shared tap handle ----

/// System-audio tap feeding `GET /audio/stream`. Platform worker threads
/// own every OS handle and push decoded [`AppFrame`]s over a channel; this
/// side only owns the stop flag, the join handle, and the receive end, so
/// the type stays `Send` on every OS without leaking ObjC/COM objects
/// across threads.
#[derive(Clone, Debug)]
pub struct AudioTap {
    inner: Arc<parking_lot::Mutex<AudioTapState>>,
}

#[derive(Debug, Default)]
struct AudioTapState {
    running: bool,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    frames_tx: Option<crossbeam_channel::Sender<AppFrame>>,
    frames_rx: Option<crossbeam_channel::Receiver<AppFrame>>,
    /// Live exclusion push for backends that filter in the OS (macOS SCK).
    /// `None` while idle.
    #[cfg(target_os = "macos")]
    exclude_tx: Option<crossbeam_channel::Sender<Vec<String>>>,
    /// Per-process tap stops (Windows). Joined via `worker`.
    #[cfg(target_os = "windows")]
    extra_stops: Vec<Arc<AtomicBool>>,
}

impl AudioTap {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(parking_lot::Mutex::new(AudioTapState::default())),
        }
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().running
    }

    /// Start the platform tap (idempotent: a running tap just refreshes its
    /// exclusion set). `muted` is the current mute set — backends that
    /// filter in the OS (macOS) bake it into the capture filter; the mixer
    /// enforces it again per tick, so a stale filter only ever over-captures
    /// into a mix the mute math still drops.
    pub fn start(&self, muted: &HashSet<String>) -> anyhow::Result<()> {
        {
            let state = self.inner.lock();
            if state.running {
                drop(state);
                self.set_excluded(muted);
                return Ok(());
            }
        }
        platform_start(self, muted)?;
        self.inner.lock().running = true;
        Ok(())
    }

    /// Push a live exclusion-set update at the running tap. No-op while
    /// idle. Never blocks, never fails: worst case the next tick's mixer
    /// math (which reads the same set) still enforces the mute.
    pub fn set_excluded(&self, muted: &HashSet<String>) {
        platform_set_excluded(self, muted);
    }

    /// Drain freshly captured frames (non-blocking, capped so one slow HTTP
    /// tick can't pile up unbounded work). Empty while idle or starved —
    /// the caller pads with silence.
    pub fn drain_frames(&self) -> Vec<AppFrame> {
        let rx = self.inner.lock().frames_rx.clone();
        let Some(rx) = rx else { return Vec::new() };
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            out.push(frame);
            if out.len() >= 32 {
                break;
            }
        }
        out
    }

    /// Stop the tap and join the worker. Safe while idle.
    pub fn stop(&self) {
        let (worker, tx) = {
            let mut state = self.inner.lock();
            state.running = false;
            state.stop.store(true, Ordering::SeqCst);
            #[cfg(target_os = "windows")]
            for stop in state.extra_stops.drain(..) {
                stop.store(true, Ordering::SeqCst);
            }
            (state.worker.take(), state.frames_tx.take())
        };
        drop(tx);
        if let Some(handle) = worker {
            let _ = handle.join();
        }
        let mut state = self.inner.lock();
        state.frames_rx = None;
        #[cfg(target_os = "macos")]
        {
            state.exclude_tx = None;
        }
        state.stop = Arc::new(AtomicBool::new(false));
    }
}

impl Default for AudioTap {
    fn default() -> Self {
        Self::new()
    }
}

/// Only the macOS backend below uses this; every other platform goes through
/// parking_lot directly, so the definition is gated the same way.
#[cfg(target_os = "macos")]
fn lock<T>(m: &parking_lot::Mutex<T>) -> parking_lot::MutexGuard<'_, T> {
    m.lock()
}

// ---- Platform backends ----

#[cfg(target_os = "macos")]
fn platform_start(tap: &AudioTap, muted: &HashSet<String>) -> anyhow::Result<()> {
    let excluded: Vec<String> = muted.iter().cloned().collect();
    let (frames_tx, frames_rx) = crossbeam_channel::bounded::<AppFrame>(64);
    let (exclude_tx, exclude_rx) = crossbeam_channel::bounded::<Vec<String>>(8);
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("jlocal-audio-tap".into())
        .spawn(move || macos::run_tap(worker_stop, frames_tx, exclude_rx, excluded))
        .map_err(|e| anyhow::anyhow!("failed to spawn audio tap thread: {e}"))?;
    let mut state = lock(&tap.inner);
    state.stop = stop;
    state.worker = Some(thread);
    state.frames_tx = None;
    state.frames_rx = Some(frames_rx);
    state.exclude_tx = Some(exclude_tx);
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_set_excluded(tap: &AudioTap, muted: &HashSet<String>) {
    let tx = lock(&tap.inner).exclude_tx.clone();
    if let Some(tx) = tx {
        // Full queue means a refresh is already pending; drop this one.
        let _ = tx.try_send(muted.iter().cloned().collect());
    }
}

/// Windows: stubbed for now. The WASAPI per-process loopback design is
/// sketched in git history, but blind cross-compiled COM cannot be verified
/// here — revive it with a Windows host that can compile and listen. Until
/// then Windows answers 501 on `/audio/stream` exactly like Linux, and
/// `caps.audio.capture` stays false there.
#[cfg(target_os = "windows")]
fn platform_start(_tap: &AudioTap, _muted: &HashSet<String>) -> anyhow::Result<()> {
    anyhow::bail!("system-audio capture is not implemented on this OS yet")
}

#[cfg(target_os = "windows")]
fn platform_set_excluded(_tap: &AudioTap, _muted: &HashSet<String>) {}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_start(_tap: &AudioTap, _muted: &HashSet<String>) -> anyhow::Result<()> {
    anyhow::bail!("system-audio capture is not implemented on this OS")
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_set_excluded(_tap: &AudioTap, _muted: &HashSet<String>) {}

// ---- macOS: ScreenCaptureKit system mix ----

/// macOS backend: one `SCStream` over the primary display with
/// `capturesAudio` at 48 kHz stereo. SCK hands us a single system mix.
/// Live-probed (Sep 2026, dev mac): rebuilding the content filter with
/// `excludingApplications` does NOT scope the captured audio — a muted GUI
/// app keeps playing in the mix. Content filters are video-scoped; true
/// per-app muting on macOS needs one stream per app (future work). Until
/// then `custom` mode on macOS behaves as `all` at the tap, while the tick
/// mixer still enforces the mute set for every per-app-tagged frame it gets
/// (Windows taps tag each process today).
///
/// Runs entirely on the tap thread: every `Retained<SC*>` object lives and
/// dies here, so nothing thread-hostile crosses into the HTTP layer. Only
/// decoded [`AppFrame`]s (plain bytes) leave over the channel.
///
/// Not live-probed in CI/dev here (no Screen Recording grant): any SCK
/// failure just ends the thread and the stream serves silence.
#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{NSObjectProtocol, ProtocolObject};
    use objc2::AnyThread as _;
    use objc2::DefinedClass as _;
    use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer};
    use objc2_foundation::{NSArray, NSError};
    use objc2_screen_capture_kit::{
        SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCStream,
        SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
    };

    /// `SCStreamOutput` receiver: decodes audio sample buffers to 48 kHz
    /// stereo `i16` and forwards them. Runs on the SCK sample-handler queue.
    struct OutputIvars {
        tx: crossbeam_channel::Sender<AppFrame>,
    }

    objc2::define_class!(
        // SAFETY:
        // - `NSObject` has no subclassing requirements.
        // - `AudioOutput` does not implement `Drop`.
        #[unsafe(super(objc2::runtime::NSObject))]
        #[name = "JLocalAudioOutput"]
        #[ivars = OutputIvars]
        struct AudioOutput;

        unsafe impl NSObjectProtocol for AudioOutput {}
        unsafe impl SCStreamOutput for AudioOutput {
            // Selector-dictated name (`stream:didOutputSampleBuffer:ofType:`).
            #[allow(non_snake_case)]
            #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
            unsafe fn stream_didOutputSampleBuffer_ofType(
                &self,
                _stream: &SCStream,
                sample_buffer: &CMSampleBuffer,
                output_type: SCStreamOutputType,
            ) {
                if output_type != SCStreamOutputType::Audio {
                    return;
                }
                if let Some(samples) = decode_audio_buffer(sample_buffer) {
                    let _ = self.ivars().tx.try_send(AppFrame {
                        app: "system".to_string(),
                        samples,
                    });
                }
            }
        }
    );

    impl AudioOutput {
        fn new(tx: crossbeam_channel::Sender<AppFrame>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(OutputIvars { tx });
            // SAFETY: `init` on a freshly allocated `NSObject` subclass.
            unsafe { objc2::msg_send![super(this), init] }
        }
    }

    /// Decode one SCK audio `CMSampleBuffer` to stereo `i16` at 48 kHz.
    /// Reads the raw PCM out of the sample's block buffer (no AudioBufferList
    /// round-trip) and interprets it per the format description. Returns
    /// `None` for non-float-LPCM or unreadable buffers (caller skips).
    fn decode_audio_buffer(sample_buffer: &CMSampleBuffer) -> Option<Vec<i16>> {
        // SAFETY: SCK audio buffers are LPCM; every step is optional and a
        // failure degrades to `None`, never to UB.
        unsafe {
            let fmt = sample_buffer.format_description()?;
            let asbd_ptr = CMAudioFormatDescriptionGetStreamBasicDescription(&fmt);
            if asbd_ptr.is_null() {
                return None;
            }
            let asbd = *asbd_ptr;
            if asbd.mFormatID != objc2_core_audio_types::kAudioFormatLinearPCM {
                return None;
            }
            // SCK delivers 32-bit float; anything else is a future format
            // we refuse rather than misdecode.
            if asbd.mFormatFlags & objc2_core_audio_types::kAudioFormatFlagIsFloat == 0
                || asbd.mBitsPerChannel != 32
            {
                return None;
            }
            let rate = asbd.mSampleRate as u32;
            let chans = asbd.mChannelsPerFrame.clamp(1, 2) as usize;
            if rate == 0 {
                return None;
            }
            let block = sample_buffer.data_buffer()?;
            let len = block.data_length();
            if len == 0 || len % 4 != 0 {
                return None;
            }
            let mut bytes = vec![0u8; len];
            let dest = std::ptr::NonNull::new(bytes.as_mut_ptr().cast::<std::ffi::c_void>())?;
            if block.copy_data_bytes(0, len, dest) != 0 {
                return None;
            }
            let floats: &[f32] = std::slice::from_raw_parts(bytes.as_ptr().cast::<f32>(), len / 4);
            let interleaved =
                asbd.mFormatFlags & objc2_core_audio_types::kAudioFormatFlagIsNonInterleaved == 0;
            if interleaved {
                let frames = floats.len() / chans;
                if frames == 0 {
                    return None;
                }
                let (left, right): (Vec<f32>, Vec<f32>) = if chans == 2 {
                    (
                        floats.iter().step_by(2).copied().collect(),
                        floats.iter().skip(1).step_by(2).copied().collect(),
                    )
                } else {
                    (floats[..frames].to_vec(), floats[..frames].to_vec())
                };
                Some(super::f32_planar_to_s16_stereo(&[&left, &right], rate))
            } else {
                // Planar: one contiguous channel run after another.
                let per_channel = floats.len() / chans;
                if per_channel == 0 {
                    return None;
                }
                let mut planar: Vec<&[f32]> = Vec::new();
                for ch in 0..chans {
                    planar.push(&floats[ch * per_channel..(ch + 1) * per_channel]);
                }
                Some(super::f32_planar_to_s16_stereo(&planar, rate))
            }
        }
    }

    /// Blocking fetch of the shareable content (called on the tap thread).
    fn shareable_content(timeout: Duration) -> anyhow::Result<Retained<SCShareableContent>> {
        let (tx, rx) = std::sync::mpsc::channel::<Result<*mut SCShareableContent, String>>();
        let block: RcBlock<dyn Fn(*mut SCShareableContent, *mut NSError)> = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                let result = if !content.is_null() {
                    // SAFETY: SCK passes a valid object on success; retain
                    // it for the trip across the channel as a raw pointer.
                    match unsafe { Retained::retain(content) } {
                        Some(retained) => Ok(Retained::into_raw(retained)),
                        None => Err("shareable content was null".to_string()),
                    }
                } else if !error.is_null() {
                    // SAFETY: SCK passes a valid error on failure.
                    Err(unsafe { &*error }.localizedDescription().to_string())
                } else {
                    Err("unknown shareable-content error".to_string())
                };
                let _ = tx.send(result);
            },
        );
        // SAFETY: SCK copies the completion handler and releases its copy
        // after invoking it; the `RcBlock` outlives the call.
        unsafe {
            SCShareableContent::getShareableContentWithCompletionHandler(&block);
        }
        let raw = rx
            .recv_timeout(timeout)
            .map_err(|_| anyhow::anyhow!("timed out listing shareable content"))?
            .map_err(|e| anyhow::anyhow!("shareable content failed: {e}"))?;
        // SAFETY: retained above, single owner, reconstructed exactly once.
        unsafe { Retained::from_raw(raw) }
            .ok_or_else(|| anyhow::anyhow!("shareable content pointer was null"))
    }

    fn app_matches_mute(app: &SCRunningApplication, muted_id: &str) -> bool {
        // SAFETY: accessors on a valid shareable-content object.
        unsafe {
            let name = app.applicationName().to_string().to_lowercase();
            if name == muted_id {
                return true;
            }
            let bundle = app.bundleIdentifier().to_string().to_lowercase();
            if bundle == muted_id {
                return true;
            }
            // `com.apple.Safari` mutes when the UI id is `safari`.
            if let Some(last) = bundle.rsplit('.').next() {
                if last == muted_id {
                    return true;
                }
            }
        }
        false
    }

    fn build_filter(
        display: &SCDisplay,
        apps: &[Retained<SCRunningApplication>],
        excluded: &[String],
    ) -> anyhow::Result<Retained<SCContentFilter>> {
        let muted: Vec<Retained<SCRunningApplication>> = apps
            .iter()
            .filter(|app| excluded.iter().any(|id| app_matches_mute(app, id)))
            .cloned()
            .collect();
        // SAFETY: valid display/apps from the same shareable snapshot.
        unsafe {
            if muted.is_empty() {
                let empty = NSArray::<objc2_screen_capture_kit::SCWindow>::new();
                Ok(SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    display,
                    &empty,
                ))
            } else {
                let arr = NSArray::from_retained_slice(&muted);
                let empty = NSArray::<objc2_screen_capture_kit::SCWindow>::new();
                Ok(
                    SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                        SCContentFilter::alloc(),
                        display,
                        &arr,
                        &empty,
                    ),
                )
            }
        }
    }
    fn start_stream(
        filter: &SCContentFilter,
        output: &ProtocolObject<dyn SCStreamOutput>,
    ) -> anyhow::Result<Retained<SCStream>> {
        // Serial queue so decode never runs on the main thread.
        let queue = dispatch2::DispatchQueue::new("lol.juntos.jlocal.audio", None);
        // SAFETY: fresh config/stream objects on the tap thread.
        unsafe {
            let config = SCStreamConfiguration::new();
            config.setCapturesAudio(true);
            config.setSampleRate(super::SAMPLE_RATE as _);
            config.setChannelCount(2);
            config.setExcludesCurrentProcessAudio(true);
            // No video output is attached, so keep the video side tiny.
            config.setWidth(64);
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                filter,
                &config,
                None,
            );
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    output,
                    SCStreamOutputType::Audio,
                    Some(&queue),
                )
                .map_err(|e| anyhow::anyhow!("SCStream audio output refused: {e}"))?;
            let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
            let block: RcBlock<dyn Fn(*mut NSError)> = RcBlock::new(move |error: *mut NSError| {
                let err = if error.is_null() {
                    None
                } else {
                    // SAFETY: SCK passes a valid error on failure (covered by
                    // the outer block: fresh stream objects, tap thread).
                    Some((&*error).localizedDescription().to_string())
                };
                let _ = tx.send(err);
            });
            stream.startCaptureWithCompletionHandler(Some(&block));
            if let Some(err) = rx
                .recv_timeout(Duration::from_secs(10))
                .map_err(|_| anyhow::anyhow!("timed out starting audio capture"))?
            {
                anyhow::bail!("audio capture refused: {err}");
            }
            Ok(stream)
        }
    }
    pub(super) fn run_tap(
        stop: Arc<AtomicBool>,
        frames_tx: crossbeam_channel::Sender<AppFrame>,
        exclude_rx: crossbeam_channel::Receiver<Vec<String>>,
        initial_excluded: Vec<String>,
    ) {
        // One pool for the tap thread's setup-phase autoreleases; the
        // sample/completion callbacks run on SCK queues (pooled by GCD).
        objc2::rc::autoreleasepool(|_| {
            run_tap_inner(stop, frames_tx, exclude_rx, initial_excluded);
        });
    }

    fn run_tap_inner(
        stop: Arc<AtomicBool>,
        frames_tx: crossbeam_channel::Sender<AppFrame>,
        exclude_rx: crossbeam_channel::Receiver<Vec<String>>,
        initial_excluded: Vec<String>,
    ) {
        let content = match shareable_content(Duration::from_secs(10)) {
            Ok(content) => content,
            Err(e) => {
                tracing::warn!("jlocal audio tap: {e:#}");
                return;
            }
        };
        // SAFETY: valid shareable-content object on the tap thread.
        let (display, apps): (Retained<SCDisplay>, Vec<Retained<SCRunningApplication>>) = unsafe {
            let displays = content.displays().to_vec();
            let Some(display) = displays.into_iter().next() else {
                tracing::warn!("jlocal audio tap: no displays to attach audio capture to");
                return;
            };
            (display, content.applications().to_vec())
        };
        tracing::info!(
            "jlocal audio tap: capturing system mix ({} sharable apps)",
            apps.len()
        );
        let output = AudioOutput::new(frames_tx);
        let output_obj = ProtocolObject::from_retained(output);
        let mut excluded = initial_excluded;
        let stream = match build_filter(&display, &apps, &excluded)
            .and_then(|filter| start_stream(&filter, &output_obj))
        {
            Ok(stream) => stream,
            Err(e) => {
                tracing::warn!("jlocal audio tap: {e:#}");
                return;
            }
        };
        // Live exclusion updates: rebuild the filter, never the stream.
        while !stop.load(Ordering::SeqCst) {
            match exclude_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(next) => {
                    excluded = next;
                    match build_filter(&display, &apps, &excluded) {
                        Ok(filter) => {
                            let block: RcBlock<dyn Fn(*mut NSError)> =
                                RcBlock::new(move |_error: *mut NSError| {});
                            // SAFETY: live stream + fresh filter; errors
                            // surface on the next tick's mixer math anyway.
                            unsafe {
                                stream.updateContentFilter_completionHandler(&filter, Some(&block));
                            }
                        }
                        Err(e) => {
                            tracing::warn!("jlocal audio tap: filter rebuild failed: {e:#}");
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        let block: RcBlock<dyn Fn(*mut NSError)> = RcBlock::new(move |_error: *mut NSError| {});
        // SAFETY: stopping our own live stream; fire-and-forget (the
        // thread — and the tap — ends here either way).
        unsafe {
            stream.stopCaptureWithCompletionHandler(Some(&block));
        }
    }
}

// ---- Windows: stubbed (see platform_start above) ----

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(mode: AudioMode, muted: &[&str]) -> AudioState {
        AudioState {
            mode,
            muted_apps: muted.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn frame(app: &str, samples: &[i16]) -> AppFrame {
        AppFrame {
            app: app.to_string(),
            samples: samples.to_vec(),
        }
    }

    #[test]
    fn stream_format_constants_agree() {
        assert_eq!(SAMPLE_RATE, 48_000);
        assert_eq!(CHANNELS, 2);
        assert_eq!(FRAME_INTERVAL, Duration::from_millis(20));
        assert_eq!(FRAMES_PER_CHUNK, 960);
        assert_eq!(SAMPLES_PER_CHUNK, 1920);
        assert_eq!(BYTES_PER_CHUNK, 3840);
        assert_eq!(silence_frame().len(), SAMPLES_PER_CHUNK);
        assert!(silence_frame().iter().all(|s| *s == 0));
    }

    #[test]
    fn i16_encoding_is_little_endian() {
        assert_eq!(i16_to_bytes(&[1, -1, 0x0102]), vec![1, 0, 255, 255, 2, 1]);
    }

    #[test]
    fn fit_chunk_truncates_and_pads() {
        let long = vec![7i16; SAMPLES_PER_CHUNK + 100];
        let chunk = fit_chunk(&long);
        assert_eq!(chunk.len(), BYTES_PER_CHUNK);
        assert_eq!(&chunk[..8], &[7, 0, 7, 0, 7, 0, 7, 0]);
        let short = vec![9i16; 4];
        let chunk = fit_chunk(&short);
        assert_eq!(&chunk[..8], &[9, 0, 9, 0, 9, 0, 9, 0]);
        assert!(chunk[8..].iter().all(|b| *b == 0));
        let empty = fit_chunk(&[]);
        assert!(empty.iter().all(|b| *b == 0));
    }

    #[test]
    fn push_tick_emits_exact_chunks_and_carries() {
        // Empty tick: full silence, backlog stays empty.
        let mut pending = Vec::new();
        let chunk = push_tick(&mut pending, Vec::new());
        assert_eq!(chunk.len(), BYTES_PER_CHUNK);
        assert!(chunk.iter().all(|b| *b == 0));
        assert!(pending.is_empty());
        // Partial tick pads; the silence is zeros, not garbage.
        let chunk = push_tick(&mut pending, vec![5i16, 6]);
        assert_eq!(&chunk[..4], &[5, 0, 6, 0]);
        assert!(chunk[4..].iter().all(|b| *b == 0));
        assert!(pending.is_empty());
        // Overflow tick emits one chunk and carries the rest.
        let chunk = push_tick(&mut pending, vec![7i16; SAMPLES_PER_CHUNK + 4]);
        assert_eq!(&chunk[..4], &[7, 0, 7, 0]);
        assert_eq!(pending, vec![7i16; 4]);
        // Carried samples lead the next chunk.
        let chunk = push_tick(&mut pending, vec![8i16; 2]);
        assert_eq!(&chunk[..8], &[7, 0, 7, 0, 7, 0, 7, 0]);
        assert_eq!(&chunk[8..12], &[8, 0, 8, 0]);
        assert!(pending.is_empty());
    }

    #[test]
    fn push_tick_bounds_burst_latency() {
        // A tap burst far beyond two ticks drops the oldest audio instead
        // of building latency: only the newest two ticks survive.
        let mut pending = Vec::new();
        let chunk = push_tick(&mut pending, vec![1i16; 4 * SAMPLES_PER_CHUNK]);
        assert_eq!(pending.len(), SAMPLES_PER_CHUNK);
        assert!(pending.iter().all(|s| *s == 1));
        assert_eq!(chunk.len(), BYTES_PER_CHUNK);
    }

    #[test]
    fn mono_to_stereo_duplicates() {
        assert_eq!(mono_to_stereo(&[1, -2]), vec![1, 1, -2, -2]);
        assert!(mono_to_stereo(&[]).is_empty());
    }

    #[test]
    fn resample_passes_through_48k_stereo() {
        let input = vec![1i16, 2, 3, 4];
        assert_eq!(resample_to_stereo_48k(&input, 48_000, 2), input);
    }

    #[test]
    fn resample_upsamples_mono_to_stereo() {
        // 24 kHz mono ramp 0..4 → 48 kHz stereo: frame count doubles, both
        // channels track the ramp.
        let mono: Vec<i16> = vec![0, 1000, 2000, 3000];
        let out = resample_to_stereo_48k(&mono, 24_000, 1);
        assert_eq!(out.len(), 8 * 2);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 0);
        // Last output frame sits 5/8 of the way from 2000 to 3000
        // (7 * (4 - 1) / 8 = 2 rem 5): linear, never overshooting.
        assert_eq!(out[out.len() - 2], 2625);
        assert_eq!(out[out.len() - 1], 2625);
        let left: Vec<i16> = out.iter().step_by(2).copied().collect();
        assert!(left.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn resample_downsamples_stereo() {
        // 96 kHz stereo → 48 kHz: frame count halves, channels preserved.
        let stereo: Vec<i16> = (0..8).flat_map(|i| [i * 100, -i * 100]).collect();
        let out = resample_to_stereo_48k(&stereo, 96_000, 2);
        assert_eq!(out.len(), 4 * 2);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 0);
    }

    #[test]
    fn resample_rejects_degenerate_input() {
        assert!(resample_to_stereo_48k(&[], 48_000, 2).is_empty());
        assert!(resample_to_stereo_48k(&[1, 2], 0, 2).is_empty());
        assert!(resample_to_stereo_48k(&[1, 2, 3], 48_000, 3).is_empty());
    }

    #[test]
    fn f32_planar_scales_clamps_and_resamples() {
        let left = [0.0f32, 0.5, 1.0, 2.0];
        let right = [0.0f32, -0.5, -1.0, -2.0];
        let out = f32_planar_to_s16_stereo(&[&left, &right], 48_000);
        assert_eq!(out.len(), 8);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 0);
        assert_eq!(out[2], 16383);
        assert_eq!(out[3], -16383);
        // Clipping, not wraparound.
        assert_eq!(out[6], 32767);
        assert_eq!(out[7], -32767);
    }

    #[test]
    fn f32_planar_dupes_mono() {
        let mono = [0.5f32, 0.5];
        let out = f32_planar_to_s16_stereo(&[&mono], 48_000);
        assert_eq!(out, vec![16383, 16383, 16383, 16383]);
    }

    #[test]
    fn mix_all_sums_everything_with_saturation() {
        let state = state_with(AudioMode::All, &[]);
        let out = mix_frames(
            &[frame("a", &[1000, 2000]), frame("b", &[500, -3000])],
            &state,
        );
        assert_eq!(out, vec![1500, -1000]);
        // Saturates instead of wrapping.
        let out = mix_frames(&[frame("a", &[30000]), frame("b", &[30000])], &state);
        assert_eq!(out, vec![32767]);
        let out = mix_frames(&[frame("a", &[-30000]), frame("b", &[-30000])], &state);
        assert_eq!(out, vec![-32768]);
    }

    #[test]
    fn mix_none_is_silence_even_with_input() {
        let state = state_with(AudioMode::None, &[]);
        let out = mix_frames(&[frame("a", &[1000, 2000])], &state);
        assert!(out.is_empty());
    }

    #[test]
    fn mix_custom_drops_muted_apps() {
        let state = state_with(AudioMode::Custom, &["discord"]);
        let out = mix_frames(
            &[frame("discord", &[1000, 1000]), frame("music", &[500, 500])],
            &state,
        );
        assert_eq!(out, vec![500, 500]);
        // Unknown apps are audible (fail-open: muting is opt-out).
        let state = state_with(AudioMode::Custom, &[]);
        let out = mix_frames(&[frame("anything", &[42])], &state);
        assert_eq!(out, vec![42]);
    }

    #[test]
    fn mix_zero_pads_short_frames() {
        let state = state_with(AudioMode::All, &[]);
        let out = mix_frames(&[frame("a", &[10, 20, 30]), frame("b", &[1])], &state);
        assert_eq!(out, vec![11, 20, 30]);
    }

    #[test]
    fn mix_empty_input_is_empty() {
        let state = state_with(AudioMode::All, &[]);
        assert!(mix_frames(&[], &state).is_empty());
    }

    #[test]
    fn tap_starts_idle_and_stops_cleanly() {
        let tap = AudioTap::new();
        assert!(!tap.is_running());
        assert!(tap.drain_frames().is_empty());
        tap.stop(); // safe while idle
        tap.set_excluded(&HashSet::new()); // no-op while idle
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert!(tap.start(&HashSet::new()).is_err());
    }

    #[test]
    fn capture_flag_matches_platform() {
        assert_eq!(
            capture_supported(),
            cfg!(any(target_os = "macos", target_os = "windows"))
        );
    }
}

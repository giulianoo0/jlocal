//! Screen capture session for the loopback preview endpoint.
//!
//! - [`list_displays`] enumerates monitors via `xcap` (no capture started).
//! - [`list_windows`] enumerates app windows via `xcap` (no capture started).
//! - [`CaptureSession`] grabs RGBA frames from one monitor *or* one window on
//!   a worker thread at the requested fps, downscales to the requested size,
//!   and caches the latest frame for [`CaptureSession::latest_jpeg`].
//! - [`snapshot_display`] / [`snapshot_window`] grab exactly one frame,
//!   downscale it to the requested width (aspect kept, never upscaled), and
//!   return it as JPEG. Stateless: they never touch [`CaptureSession`].
//! - JPEG encoding uses the `image` crate's pure-Rust encoder (already pulled
//!   in transitively by `xcap`; no system libs, no new native deps).
//!
//! Threading: one `std::thread` per running session; `stop()` signals it and
//! joins. A display that disconnects mid-session records an error string and
//! ends the worker loop — it never panics, and the last good frame stays
//! available for the preview endpoint. A window that closes mid-session ends
//! the worker loop and drops the cached frame, so the preview 404s instead
//! of serving a stale window.
//!
//! Wiring (done by the parent, not here): store one session in shared state,
//! e.g. `capture: Arc<Mutex<Option<CaptureSession>>>`, and serve
//! `GET /capture/preview.jpg` from `latest_jpeg()`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::Serialize;

/// Bounds for the requested capture rate. `start()` clamps into this range.
pub const MIN_FPS: u32 = 1;
/// Bounds for the requested capture rate. `start()` clamps into this range.
pub const MAX_FPS: u32 = 60;

/// Consecutive grab failures tolerated before the worker gives up
/// (covers transient back-end hiccups; a truly disconnected display keeps
/// failing and trips this quickly instead of hot-spinning).
const MAX_CONSECUTIVE_ERRORS: u32 = 30;

/// EMA weight for a new grab-interval sample (0 < `ALPHA` < 1).
const FPS_EMA_ALPHA: f64 = 0.2;

/// Default JPEG quality when the caller passes `0`.
const DEFAULT_JPEG_QUALITY: u8 = 80;

/// Default one-shot snapshot width in px (`GET /capture/snapshot`).
pub const SNAPSHOT_DEFAULT_WIDTH: u32 = 960;
/// Bounds for the requested snapshot width. `snapshot_*` clamp into this range.
pub const SNAPSHOT_MIN_WIDTH: u32 = 160;
/// Bounds for the requested snapshot width. `snapshot_*` clamp into this range.
pub const SNAPSHOT_MAX_WIDTH: u32 = 1920;
/// One monitor, as reported by the OS.
#[derive(Clone, Debug, Serialize)]
pub struct Display {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// One app window, as reported by the OS.
#[derive(Clone, Debug, Serialize)]
pub struct Window {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// Latest cached frame: contiguous RGBA8 bytes at the requested size.
#[derive(Debug)]
struct SharedFrame {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

impl SharedFrame {
    fn empty() -> Self {
        Self {
            rgba: Vec::new(),
            width: 0,
            height: 0,
        }
    }
}

/// Screen capture session. Create with [`CaptureSession::new`], begin
/// grabbing with [`CaptureSession::start`], end with [`CaptureSession::stop`]
/// (`Drop` also stops). Only one grab loop runs at a time; calling `start()`
/// while running restarts the session with the new options.
pub struct CaptureSession {
    worker: Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
    frame: Arc<Mutex<SharedFrame>>,
    last_error: Arc<Mutex<Option<String>>>,
    ema_interval_secs: Arc<Mutex<f64>>,
    display_id: Option<u32>,
    window_id: Option<u32>,
    width: u32,
    height: u32,
    fps: u32,
}

// Manually implemented: `JoinHandle` has no `Debug` impl, and derived debug
// would leak nothing useful anyway.
impl std::fmt::Debug for CaptureSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureSession")
            .field("running", &self.is_running())
            .field("display_id", &self.display_id)
            .field("window_id", &self.window_id)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fps", &self.fps)
            .field("actual_fps", &self.actual_fps())
            .finish()
    }
}

/// Enumerate the monitors currently visible to the OS.
pub fn list_displays() -> anyhow::Result<Vec<Display>> {
    let monitors = xcap::Monitor::all().context("failed to enumerate displays")?;
    let mut out = Vec::with_capacity(monitors.len());
    for m in &monitors {
        let id = m.id().context("display is missing its id")?;
        let (w, h) = dimension_of(m)?;
        // Prefer the human-friendly label; fall back to the short name, then
        // to a synthetic label so callers always have something to show.
        let name = m
            .friendly_name()
            .or_else(|_| m.name())
            .unwrap_or_else(|_| format!("Display {id}"));
        out.push(Display {
            id,
            name,
            width: w,
            height: h,
        });
    }
    Ok(out)
}

/// Enumerate the app windows currently visible to the OS.
///
/// Skips minimized and zero-area entries: they can never yield frames, so
/// listing them would offer targets that start and immediately end. Unlike
/// [`list_displays`], one unreadable window never fails the whole list —
/// windows come and go at any moment, so entries missing an id or
/// dimensions are skipped instead of erroring.
pub fn list_windows() -> anyhow::Result<Vec<Window>> {
    let windows = xcap::Window::all().context("failed to enumerate windows")?;
    let mut out = Vec::with_capacity(windows.len());
    for w in &windows {
        let Ok(id) = w.id() else { continue };
        let Ok((width, height)) = window_dimensions(w) else {
            continue;
        };
        if width == 0 || height == 0 {
            continue;
        }
        if w.is_minimized().unwrap_or(false) {
            continue;
        }
        // Prefer the window title; fall back to the app name, then to a
        // synthetic label so callers always have something to show.
        let title = w.title().unwrap_or_default();
        let name = if title.trim().is_empty() {
            match w.app_name() {
                Ok(app) if !app.trim().is_empty() => app,
                _ => format!("Window {id}"),
            }
        } else {
            title
        };
        out.push(Window {
            id,
            name,
            width,
            height,
        });
    }
    Ok(out)
}

/// Clamp a requested snapshot width into
/// `SNAPSHOT_MIN_WIDTH..=SNAPSHOT_MAX_WIDTH`.
pub fn clamp_snapshot_width(width: u32) -> u32 {
    width.clamp(SNAPSHOT_MIN_WIDTH, SNAPSHOT_MAX_WIDTH)
}

/// Output dimensions for a one-shot snapshot: `target_width` px wide (clamped)
/// keeping the aspect ratio, never upscaled (a narrower source keeps its
/// native size), height at least 1.
pub fn snapshot_output_size(src_width: u32, src_height: u32, target_width: u32) -> (u32, u32) {
    let target = clamp_snapshot_width(target_width);
    if src_width <= target || src_width == 0 || src_height == 0 {
        return (src_width, src_height);
    }
    let height = ((u64::from(src_height) * u64::from(target)) / u64::from(src_width)).max(1) as u32;
    (target, height)
}

/// Grab exactly one frame from `display_id` and return it as JPEG.
///
/// Downscales to `target_width` px keeping the aspect ratio (see
/// [`snapshot_output_size`]). An unknown id is an error, as is any grab
/// failure — the caller maps unknown ids to 400 and failures to 503.
/// Stateless: never touches [`CaptureSession`] (a running session keeps
/// grabbing undisturbed).
pub fn snapshot_display(display_id: u32, target_width: u32) -> anyhow::Result<Vec<u8>> {
    let monitor = xcap::Monitor::all()
        .context("failed to enumerate displays")?
        .into_iter()
        .find(|m| m.id().unwrap_or(u32::MAX) == display_id);
    let monitor = monitor.ok_or_else(|| anyhow::anyhow!("display {display_id} not found"))?;
    let img = monitor
        .capture_image()
        .map_err(|e| anyhow::anyhow!("capture failed (display {display_id}): {e}"))?;
    snapshot_jpeg(img, target_width)
}

/// Grab exactly one frame from `window_id` and return it as JPEG.
///
/// Same contract as [`snapshot_display`], against the window instead of a
/// monitor. A window closed between listing and here is a capture error
/// (503), not a panic.
pub fn snapshot_window(window_id: u32, target_width: u32) -> anyhow::Result<Vec<u8>> {
    let window = xcap::Window::all()
        .context("failed to enumerate windows")?
        .into_iter()
        .find(|w| w.id().unwrap_or(u32::MAX) == window_id);
    let window = window.ok_or_else(|| anyhow::anyhow!("window {window_id} not found"))?;
    let img = window
        .capture_image()
        .map_err(|e| anyhow::anyhow!("capture failed (window {window_id}): {e}"))?;
    snapshot_jpeg(img, target_width)
}

/// Downscale `img` per [`snapshot_output_size`] and encode it as JPEG at the
/// default quality (same encoder and filter as the session worker).
fn snapshot_jpeg(img: image::RgbaImage, target_width: u32) -> anyhow::Result<Vec<u8>> {
    let (src_w, src_h) = (img.width(), img.height());
    let (out_w, out_h) = snapshot_output_size(src_w, src_h, target_width);
    let rgba = if (out_w, out_h) == (src_w, src_h) {
        img.into_raw()
    } else {
        image::imageops::resize(&img, out_w, out_h, image::imageops::FilterType::Triangle).into_raw()
    };
    encode_jpeg_rgba(&rgba, out_w, out_h, DEFAULT_JPEG_QUALITY)
}

impl CaptureSession {
    /// Idle session; call [`CaptureSession::start`] (monitor) or
    /// [`CaptureSession::start_window`] (app window) to begin grabbing.
    pub fn new() -> Self {
        Self {
            worker: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            frame: Arc::new(Mutex::new(SharedFrame::empty())),
            last_error: Arc::new(Mutex::new(None)),
            ema_interval_secs: Arc::new(Mutex::new(0.0)),
            display_id: None,
            window_id: None,
            width: 0,
            height: 0,
            fps: 0,
        }
    }

    /// Begin (or restart) grabbing `display_id` at `width`x`height` @ `fps`.
    ///
    /// - `fps` is clamped to `1..=60`.
    /// - `width`/`height` must be non-zero and fit inside the display;
    ///   anything larger is rejected (no upscaling).
    /// - Unknown `display_id` (e.g. unplugged since [`list_displays`]) is an
    pub fn start(
        &mut self,
        display_id: u32,
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<()> {
        validate_size(width, height)?;

        let monitor = xcap::Monitor::all()
            .context("failed to enumerate displays")?
            .into_iter()
            .find(|m| m.id().unwrap_or(u32::MAX) == display_id);
        let monitor = monitor.ok_or_else(|| anyhow::anyhow!("display {display_id} not found"))?;
        let (disp_w, disp_h) = dimension_of(&monitor)?;
        if width > disp_w || height > disp_h {
            anyhow::bail!(
                "requested {width}x{height} exceeds display {display_id} size {disp_w}x{disp_h}"
            );
        }

        // Validation passed: safe to replace any running session.
        self.launch(CaptureTarget::Display(display_id), width, height, fps)
    }

    /// Begin (or restart) grabbing `window_id` at `width`x`height` @ `fps`.
    ///
    /// Same contract as [`CaptureSession::start`], against the window's
    /// dimensions instead of a monitor's. A window closed between
    /// [`list_windows`] and here is an error, not a panic. Failed validation
    /// leaves a running session untouched.
    pub fn start_window(
        &mut self,
        window_id: u32,
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<()> {
        validate_size(width, height)?;

        let window = xcap::Window::all()
            .context("failed to enumerate windows")?
            .into_iter()
            .find(|w| w.id().unwrap_or(u32::MAX) == window_id);
        let window = window.ok_or_else(|| anyhow::anyhow!("window {window_id} not found"))?;
        let (win_w, win_h) = window_dimensions(&window)?;
        if width > win_w || height > win_h {
            anyhow::bail!(
                "requested {width}x{height} exceeds window {window_id} size {win_w}x{win_h}"
            );
        }

        // Validation passed: safe to replace any running session.
        self.launch(CaptureTarget::Window(window_id), width, height, fps)
    }

    /// Shared launcher: both targets validated above, so this clamps the
    /// rate, clears stale state, and spawns the worker.
    fn launch(
        &mut self,
        target: CaptureTarget,
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<()> {
        let fps = clamp_fps(fps);
        self.shutdown();

        self.stop_flag.store(false, Ordering::SeqCst);
        *lock(&self.last_error) = None;
        *lock(&self.ema_interval_secs) = 0.0;
        *lock(&self.frame) = SharedFrame::empty();

        let stop_flag = Arc::clone(&self.stop_flag);
        let frame = Arc::clone(&self.frame);
        let last_error = Arc::clone(&self.last_error);
        let ema_interval_secs = Arc::clone(&self.ema_interval_secs);
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps));

        // NOTE: the target is re-resolved inside the worker (by id) so the
        // spawned closure only carries plain data + Arcs and never requires
        // `xcap::Monitor` / `xcap::Window: Send`. A target gone between
        // validation and thread start records an error and exits instead of
        // panicking.
        let handle = std::thread::Builder::new()
            .name("jlocal-capture".into())
            .spawn(move || {
                grab_loop(
                    target,
                    width,
                    height,
                    interval,
                    &stop_flag,
                    &frame,
                    &last_error,
                    &ema_interval_secs,
                );
            })
            .context("failed to spawn capture thread")?;

        self.worker = Some(handle);
        match target {
            CaptureTarget::Display(id) => {
                self.display_id = Some(id);
                self.window_id = None;
            }
            CaptureTarget::Window(id) => {
                self.window_id = Some(id);
                self.display_id = None;
            }
        }
        self.width = width;
        self.height = height;
        self.fps = fps;
        Ok(())
    }

    /// Encode the latest cached frame as JPEG. Returns `None` while idle or
    /// before the first frame lands (the preview handler maps this to 404).
    pub fn latest_jpeg(&self, quality: u8) -> Option<Vec<u8>> {
        let cached = lock(&self.frame);
        if cached.rgba.is_empty() {
            return None;
        }
        encode_jpeg_rgba(&cached.rgba, cached.width, cached.height, quality).ok()
    }

    /// Signal the worker to stop and join it. Safe to call while idle.
    pub fn stop(&mut self) {
        self.shutdown();
    }

    /// `true` while the grab thread is alive (a worker that exited on error
    /// reports `false`; see `last_error` for why).
    pub fn is_running(&self) -> bool {
        self.worker.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Smoothed grab rate in frames/sec; `0.0` before the first frames land.
    pub fn actual_fps(&self) -> f64 {
        let ema = *lock(&self.ema_interval_secs);
        if ema > 0.0 && ema.is_finite() {
            1.0 / ema
        } else {
            0.0
        }
    }

    /// Last grab failure, if any (e.g. display disconnected or window closed
    /// mid-session).
    pub fn last_error(&self) -> Option<String> {
        lock(&self.last_error).clone()
    }

    pub fn display_id(&self) -> Option<u32> {
        self.display_id
    }

    pub fn window_id(&self) -> Option<u32> {
        self.window_id
    }
    /// Requested (clamped) capture rate; `0` while never started.
    pub fn fps(&self) -> u32 {
        self.fps
    }

    fn shutdown(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Default for CaptureSession {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// What the worker grabs: one monitor or one app window, by OS id.
#[derive(Clone, Copy, Debug)]
enum CaptureTarget {
    Display(u32),
    Window(u32),
}

/// Worker body: resolve the target, then grab at `interval`, downscale to
/// the requested size, and cache the latest frame. Ends when `stop_flag` is
/// set or the error budget is exhausted (disconnected display, closed
/// window). Records errors instead of panicking. A closed window also drops
/// the cached frame, so the preview 404s rather than serving a stale
/// window; a disconnected display keeps its last good frame.
#[allow(clippy::too_many_arguments)]
fn grab_loop(
    target: CaptureTarget,
    width: u32,
    height: u32,
    interval: Duration,
    stop_flag: &AtomicBool,
    frame: &Mutex<SharedFrame>,
    last_error: &Mutex<Option<String>>,
    ema_interval_secs: &Mutex<f64>,
) {
    enum Source {
        Display(xcap::Monitor),
        Window(xcap::Window),
    }
    let source = match target {
        CaptureTarget::Display(display_id) => match xcap::Monitor::all()
            .unwrap_or_default()
            .into_iter()
            .find(|m| m.id().unwrap_or(u32::MAX) == display_id)
        {
            Some(m) => Source::Display(m),
            None => {
                *lock(last_error) = Some(format!(
                    "display {display_id} not found (disconnected before capture started)"
                ));
                return;
            }
        },
        CaptureTarget::Window(window_id) => match xcap::Window::all()
            .unwrap_or_default()
            .into_iter()
            .find(|w| w.id().unwrap_or(u32::MAX) == window_id)
        {
            Some(w) => Source::Window(w),
            None => {
                *lock(last_error) = Some(format!(
                    "window {window_id} not found (closed before capture started)"
                ));
                return;
            }
        },
    };

    let mut ema: Option<f64> = None;
    let mut last_tick = Instant::now();
    let mut consecutive_errors: u32 = 0;

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        let tick = Instant::now();
        if tick > last_tick {
            let sample = tick.duration_since(last_tick).as_secs_f64();
            if sample > 0.0 && sample.is_finite() {
                ema = Some(match ema {
                    Some(prev) => prev + FPS_EMA_ALPHA * (sample - prev),
                    None => sample,
                });
                *lock(ema_interval_secs) = ema.unwrap_or(0.0);
            }
        }
        last_tick = tick;

        let grabbed = match &source {
            Source::Display(m) => m.capture_image(),
            Source::Window(w) => w.capture_image(),
        };
        match grabbed {
            Ok(img) => {
                consecutive_errors = 0;
                let rgba = if img.width() == width && img.height() == height {
                    img.into_raw()
                } else {
                    image::imageops::resize(
                        &img,
                        width,
                        height,
                        image::imageops::FilterType::Triangle,
                    )
                    .into_raw()
                };
                *lock(frame) = SharedFrame {
                    rgba,
                    width,
                    height,
                };
                *lock(last_error) = None;
            }
            Err(e) => {
                consecutive_errors += 1;
                *lock(last_error) = Some(match target {
                    CaptureTarget::Display(display_id) => {
                        format!("capture failed (display {display_id} may be disconnected): {e}")
                    }
                    CaptureTarget::Window(window_id) => {
                        format!("capture failed (window {window_id} may be closed): {e}")
                    }
                });
                if consecutive_errors > MAX_CONSECUTIVE_ERRORS {
                    if matches!(target, CaptureTarget::Window(_)) {
                        *lock(frame) = SharedFrame::empty();
                    }
                    break;
                }
            }
        }

        let elapsed = tick.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}

/// Clamp the requested rate into the supported `1..=60` range.
fn clamp_fps(fps: u32) -> u32 {
    fps.clamp(MIN_FPS, MAX_FPS)
}

/// Reject empty requests up front (oversize-vs-target is checked in
/// [`CaptureSession::start`] / [`CaptureSession::start_window`], which know
/// the target's dimensions).
fn validate_size(width: u32, height: u32) -> anyhow::Result<()> {
    if width == 0 || height == 0 {
        anyhow::bail!("capture size must be non-zero, got {width}x{height}");
    }
    Ok(())
}

fn dimension_of(monitor: &xcap::Monitor) -> anyhow::Result<(u32, u32)> {
    let w = monitor.width().context("display is missing its width")?;
    let h = monitor.height().context("display is missing its height")?;
    Ok((w, h))
}

fn window_dimensions(window: &xcap::Window) -> anyhow::Result<(u32, u32)> {
    let w = window.width().context("window is missing its width")?;
    let h = window.height().context("window is missing its height")?;
    Ok((w, h))
}

/// Encode raw RGBA8 pixels as JPEG. Pure-Rust encoder from the `image` crate
/// (no system libraries); alpha is dropped per the JPEG spec.
fn encode_jpeg_rgba(rgba: &[u8], width: u32, height: u32, quality: u8) -> anyhow::Result<Vec<u8>> {
    let expected = width as usize * height as usize * 4;
    if rgba.len() != expected {
        anyhow::bail!(
            "frame buffer size {} does not match {width}x{height} RGBA ({expected})",
            rgba.len()
        );
    }
    let quality = if quality == 0 {
        DEFAULT_JPEG_QUALITY
    } else {
        quality.clamp(1, 100)
    };
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    // The encoder has no RGBA8 path: drop alpha explicitly (JPEG has none).
    let (pixels, _) = rgba.as_chunks::<4>();
    let rgb: Vec<u8> = pixels.iter().flat_map(|px| [px[0], px[1], px[2]]).collect();
    enc.encode(&rgb, width, height, image::ExtendedColorType::Rgb8)
        .context("jpeg encode failed")?;
    Ok(out)
}

/// Lock ignoring poisoning: a failed grab must never panic the worker (or a
/// request handler) just because a previous holder died mid-critical-section.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_clamps_to_supported_range() {
        assert_eq!(clamp_fps(0), MIN_FPS);
        assert_eq!(clamp_fps(1), 1);
        assert_eq!(clamp_fps(30), 30);
        assert_eq!(clamp_fps(60), MAX_FPS);
        assert_eq!(clamp_fps(240), MAX_FPS);
        assert_eq!(clamp_fps(u32::MAX), MAX_FPS);
    }

    #[test]
    fn zero_size_is_rejected() {
        assert!(validate_size(0, 1080).is_err());
        assert!(validate_size(1920, 0).is_err());
        assert!(validate_size(0, 0).is_err());
        assert!(validate_size(640, 480).is_ok());
    }

    #[test]
    fn oversize_request_reads_back_in_error() {
        // Mirrors the check `start()` applies once it knows the display size:
        // anything larger than the panel is unsupported (no upscaling).
        let (disp_w, disp_h) = (1920, 1080);
        for (w, h) in [(1920, 1080), (1280, 720), (1, 1)] {
            assert!(w <= disp_w && h <= disp_h, "{w}x{h} should fit");
        }
        for (w, h) in [(1921, 1080), (1920, 1081), (3840, 2160)] {
            assert!(w > disp_w || h > disp_h, "{w}x{h} should not fit");
        }
    }

    #[test]
    fn synthetic_frame_encodes_to_real_jpeg() {
        // 16x16 gradient: non-trivial pixels so the encoder has work to do.
        let (w, h) = (16, 16);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x * 16) as u8, (y * 16) as u8, 128, 255]);
            }
        }
        let jpeg = encode_jpeg_rgba(&rgba, w, h, 80).expect("encode must succeed");
        // SOI marker + JFIF/APP0: a real JPEG, not a renamed blob.
        assert!(
            jpeg.len() > 4 && jpeg[0] == 0xFF && jpeg[1] == 0xD8 && jpeg[2] == 0xFF,
            "missing JPEG SOI magic"
        );
        // EOI marker closes the stream.
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9], "missing JPEG EOI");
    }

    #[test]
    fn jpeg_quality_zero_means_default() {
        let rgba = vec![200u8; 8 * 8 * 4];
        let a = encode_jpeg_rgba(&rgba, 8, 8, 0).expect("q=0 must succeed");
        let b = encode_jpeg_rgba(&rgba, 8, 8, DEFAULT_JPEG_QUALITY).expect("q=80 must succeed");
        assert_eq!(a, b);
        // Out-of-range quality clamps instead of failing.
        assert!(encode_jpeg_rgba(&rgba, 8, 8, 255).is_ok());
    }

    #[test]
    fn mismatched_buffer_is_rejected() {
        let rgba = vec![0u8; 10];
        assert!(encode_jpeg_rgba(&rgba, 8, 8, 80).is_err());
    }

    #[test]
    fn idle_session_has_no_frame_and_zero_fps() {
        let mut session = CaptureSession::new();
        assert!(!session.is_running());
        assert_eq!(session.actual_fps(), 0.0);
        assert!(session.latest_jpeg(80).is_none());
        assert!(session.last_error().is_none());
        // Stopping (and dropping) an idle session is a safe no-op.
        session.stop();
        assert!(!session.is_running());
    }

    #[test]
    fn window_start_rejects_zero_size_before_touching_os() {
        let mut session = CaptureSession::new();
        // Zero size is rejected before any window is touched (headless-safe),
        // and a failed start never leaves a worker behind.
        assert!(session.start_window(0, 0, 1080, 30).is_err());
        assert!(session.start_window(0, 1920, 0, 30).is_err());
        assert!(!session.is_running());
        assert!(session.window_id().is_none());
    }

    #[test]
    fn listed_windows_can_all_yield_frames() {
        // Enumeration is machine-dependent (permissions, headless CI), so a
        // bare machine may answer Err or an empty list. Whatever comes back
        // must already be filtered: no zero-area entries, which could never
        // produce a frame.
        if let Ok(windows) = list_windows() {
            assert!(windows.iter().all(|w| w.width > 0 && w.height > 0));
        }
    }

    #[test]
    fn invalid_start_options_fail_without_display() {
        let mut session = CaptureSession::new();
        // Zero size is rejected before any display is touched (headless-safe).
        assert!(session.start(0, 0, 1080, 30).is_err());
        assert!(session.start(0, 1920, 0, 30).is_err());
        assert!(!session.is_running());
    }

    #[test]
    fn snapshot_width_clamps_to_supported_range() {
        assert_eq!(clamp_snapshot_width(0), SNAPSHOT_MIN_WIDTH);
        assert_eq!(clamp_snapshot_width(159), SNAPSHOT_MIN_WIDTH);
        assert_eq!(clamp_snapshot_width(160), 160);
        assert_eq!(clamp_snapshot_width(960), 960);
        assert_eq!(clamp_snapshot_width(1920), SNAPSHOT_MAX_WIDTH);
        assert_eq!(clamp_snapshot_width(3840), SNAPSHOT_MAX_WIDTH);
        assert_eq!(clamp_snapshot_width(u32::MAX), SNAPSHOT_MAX_WIDTH);
    }

    #[test]
    fn snapshot_output_size_keeps_aspect_and_never_upscales() {
        // 1920x1080 down to 960 wide halves the height (16:9 kept).
        assert_eq!(snapshot_output_size(1920, 1080, 960), (960, 540));
        // Clamp applies before scaling: 3840 wide at 5000 requested → 1920.
        assert_eq!(snapshot_output_size(3840, 2160, 5000), (1920, 1080));
        // Narrower than the target: native size, never upscaled.
        assert_eq!(snapshot_output_size(800, 600, 960), (800, 600));
        assert_eq!(snapshot_output_size(960, 540, 960), (960, 540));
        // Odd heights truncate (integer math), never to zero.
        assert_eq!(snapshot_output_size(1920, 1080, 161), (161, 90));
        assert_eq!(snapshot_output_size(3000, 1, 160), (160, 1));
    }

    #[test]
    fn snapshot_downscale_encodes_real_jpeg_at_scaled_size() {
        // 320x160 gradient down to 160 wide must come back a real JPEG whose
        // decoded size matches the aspect-kept output (160x80).
        let (w, h) = (320, 160);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x / 2) as u8, (y * 3 / 2) as u8, 128, 255]);
            }
        }
        let img: image::RgbaImage =
            image::ImageBuffer::from_raw(w, h, rgba).expect("test frame must build");
        let jpeg = snapshot_jpeg(img, 16).expect("downscale+encode must succeed");
        assert!(
            jpeg.len() > 4 && jpeg[0] == 0xFF && jpeg[1] == 0xD8 && jpeg[2] == 0xFF,
            "missing JPEG SOI magic"
        );
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9], "missing JPEG EOI");
        let decoded = image::load_from_memory(&jpeg).expect("snapshot must decode");
        assert_eq!((decoded.width(), decoded.height()), (160, 80));
    }
}

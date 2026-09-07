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
/// Icon edge in px for [`Window::icon`]: icons are downscaled to fit this
/// box (aspect kept, never upscaled), so a data URL stays ~1–4 KiB and the
/// whole `/capture/windows` payload stays small. Never larger than 64.
pub const ICON_SIZE: u32 = 32;
/// One monitor, as reported by the OS.
#[derive(Clone, Debug, Serialize)]
pub struct Display {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// One app window, as reported by the OS. `app` is the owning application
/// (empty when the OS would not say); `name` stays the human label. `icon`
/// is the owning app's icon as a `data:image/png;base64,…` URL at
/// [`ICON_SIZE`] px ("" when the OS has no icon to give — Linux always).
#[derive(Clone, Debug, Serialize)]
pub struct Window {
    pub id: u32,
    pub name: String,
    pub app: String,
    pub icon: String,
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
/// dimensions are skipped instead of erroring. A window whose icon cannot
/// be read keeps `icon: ""` — one bad icon never drops the window.
///
/// Cost: icons resolve lazily on every call (one OS lookup per window —
/// `NSRunningApplication` on macOS, exe icon on Windows). Lists are small
/// (tens of windows), so this stays in the noise next to enumeration.
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
        // synthetic label so callers always have something to show. The raw
        // app name always rides along for icons/avatars.
        let title = w.title().unwrap_or_default();
        let app = w.app_name().unwrap_or_default().trim().to_string();
        let name = if title.trim().is_empty() {
            if app.is_empty() {
                format!("Window {id}")
            } else {
                app.clone()
            }
        } else {
            title
        };
        out.push(Window {
            id,
            name,
            app,
            // Best-effort per window: unknown pid or unreadable icon → "".
            icon: w
                .pid()
                .ok()
                .and_then(window_icon_for_pid)
                .unwrap_or_default(),
            width,
            height,
        });
    }
    Ok(out)
}

/// Owning-app icon for `pid` as a PNG data URL, or `None` when the OS has
/// nothing to give. Per-window best-effort — see [`list_windows`]: every
/// step is optional (apps come and go; daemons and bare CLI tools have no
/// GUI icon), so failures degrade to `None`, never to an error.
#[cfg(target_os = "macos")]
fn window_icon_for_pid(pid: u32) -> Option<String> {
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSRunningApplication};
    use objc2_foundation::{NSCopying, NSDictionary};

    // pid → running app → icon.
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32)?;
    let icon = app.icon()?;
    // App icons ship reps up to 2048px as 16-bit-float TIFFs (tens of MB,
    // undecodable outside AppKit). Work on a copy with the giants stripped:
    // the remaining ≤256px TIFF stays a few MB and parses fast.
    let small = icon.copy();
    for rep in icon.representations().to_vec() {
        if rep.pixelsWide() > 256 || rep.pixelsHigh() > 256 {
            small.removeRepresentation(&rep);
        }
    }
    let tiff = small.TIFFRepresentation()?;
    // AppKit parses its own float TIFFs; ask the bitmap back as PNG (the
    // empty properties dict's generics infer from the call below).
    let bitmap = NSBitmapImageRep::imageRepWithData(&tiff)?;
    let props = NSDictionary::new();
    // SAFETY: empty properties are trivially well-typed; a nil return
    // becomes `None` via the `?` below.
    let png =
        unsafe { bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }?;
    let bytes = png.to_vec();
    if bytes.is_empty() {
        return None;
    }
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    Some(icon_data_url(img))
}

/// Owning-app icon for `pid` as a PNG data URL, or `None`.
/// pid → exe path → large (32px) shell icon → RGBA → shared tail below.
///
/// Compiled on Windows only (this mac can't check it — CI's windows-latest
/// runner does), so it stays small and straight-line on purpose: one unsafe
/// block per Win32 step, `?`/`None` on any failure, GDI objects released.
#[cfg(target_os = "windows")]
fn window_icon_for_pid(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
    use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;
    use windows_core::{PCWSTR, PWSTR};

    // pid → exe path (nul-terminated for the shell call below).
    let path = unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; MAX_PATH as usize];
        let mut len = buf.len() as u32;
        let exe = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(process);
        if !exe {
            return None;
        }
        buf.truncate(len as usize);
        buf.push(0);
        buf
    };
    // exe path → large shell icon (SHGFI_LARGEICON is the 32px default).
    let hicon = unsafe {
        let mut info: SHFILEINFOW = std::mem::zeroed();
        let found = SHGetFileInfoW(
            PCWSTR(path.as_ptr()),
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info as *mut SHFILEINFOW),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if found == 0 || info.hIcon.is_invalid() {
            return None;
        }
        info.hIcon
    };
    // HICON → RGBA pixels, then the shared tail (downscale + PNG + URL).
    let img = unsafe { hicon_to_rgba(hicon) }?;
    unsafe {
        let _ = DestroyIcon(hicon);
    }
    Some(icon_data_url(img))
}

/// Read an `HICON`'s pixels as RGBA. Prefers the color bitmap's own alpha
/// (correct for modern icons); legacy icons store no alpha there, so an
/// all-zero alpha plane falls back to the monochrome mask (white = glass).
/// Releases the DC and both bitmaps before returning.
#[cfg(target_os = "windows")]
unsafe fn hicon_to_rgba(
    hicon: windows::Win32::UI::WindowsAndMessaging::HICON,
) -> Option<image::RgbaImage> {
    use windows::Win32::Graphics::Gdi::{
        GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};

    unsafe {
        let mut info: ICONINFO = std::mem::zeroed();
        GetIconInfo(hicon, &mut info).ok()?;
        let release = DropBitmaps {
            color: info.hbmColor,
            mask: info.hbmMask,
        };
        let mut bmp: BITMAP = std::mem::zeroed();
        if GetObjectW(
            info.hbmColor.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut BITMAP as *mut core::ffi::c_void),
        ) == 0
        {
            return None;
        }
        let (w, h) = (bmp.bmWidth.max(1) as u32, bmp.bmHeight.max(1) as u32);
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return None;
        }
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // top-down: rows land in display order
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            hdc,
            info.hbmColor,
            0,
            h,
            Some(bgra.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let mut mask = vec![0u8; (w * h * 4) as usize];
        let mask_lines = GetDIBits(
            hdc,
            info.hbmMask,
            0,
            h,
            Some(mask.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        drop(release);
        if lines == 0 {
            return None;
        }
        // GDI hands back BGRA: swap to RGBA, and where the color bitmap
        // carries no alpha at all, derive it from the mask instead.
        let no_alpha = bgra.chunks_exact(4).all(|px| px[3] == 0);
        let mut rgba = Vec::with_capacity(bgra.len());
        for (i, px) in bgra.chunks_exact(4).enumerate() {
            let alpha = if no_alpha && mask_lines > 0 {
                // Mask converts to 32-bit white (glass) / black (solid).
                if mask[i * 4] == 0xFF {
                    0
                } else {
                    0xFF
                }
            } else {
                px[3]
            };
            rgba.extend_from_slice(&[px[2], px[1], px[0], alpha]);
        }
        image::RgbaImage::from_raw(w, h, rgba)
    }
}

/// Release the two bitmaps `GetIconInfo` hands out (the caller owns them).
#[cfg(target_os = "windows")]
struct DropBitmaps {
    color: windows::Win32::Graphics::Gdi::HBITMAP,
    mask: windows::Win32::Graphics::Gdi::HBITMAP,
}

#[cfg(target_os = "windows")]
impl Drop for DropBitmaps {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(self.color.into());
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(self.mask.into());
        }
    }
}

/// Linux has no icon source in scope (explicitly out), so every window
/// reports `icon: ""` and the web keeps its letter avatar.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn window_icon_for_pid(_pid: u32) -> Option<String> {
    None
}

/// Only the macOS/Windows icon paths (and their tests) use these; the Linux
/// stub never does, so the definitions are gated the same way.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn icon_thumb_size(w: u32, h: u32) -> (u32, u32) {
    let m = w.max(h).max(1);
    ((w * ICON_SIZE / m).max(1), (h * ICON_SIZE / m).max(1))
}

/// Shared tail for every OS: fit `img` in the [`ICON_SIZE`] box (aspect
/// kept, never upscaled — small icons pass through untouched), encode a
/// tight PNG, and wrap it as a data URL. Infallible by construction for
/// real images: the PNG encoder only fails on OOM, and `from_raw` above
/// already validated the buffer.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn icon_data_url(img: image::RgbaImage) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use image::ImageEncoder;

    let (w, h) = (img.width(), img.height());
    let small = if w > ICON_SIZE || h > ICON_SIZE {
        let (tw, th) = icon_thumb_size(w, h);
        image::imageops::resize(&img, tw, th, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let mut png = Vec::new();
    let enc = image::codecs::png::PngEncoder::new(&mut png);
    // `write_image` consumes the encoder and takes the buffer as-is: no
    // color-type conversion surprises — it is RGBA8 by construction.
    if enc
        .write_image(
            small.as_raw(),
            small.width(),
            small.height(),
            image::ExtendedColorType::Rgba8,
        )
        .is_err()
    {
        return String::new();
    }
    let mut url = String::with_capacity(22 + png.len() * 4 / 3 + 4);
    url.push_str("data:image/png;base64,");
    STANDARD.encode_string(&png, &mut url);
    url
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
        image::imageops::resize(&img, out_w, out_h, image::imageops::FilterType::Triangle)
            .into_raw()
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
    /// - `width`/`height` must be non-zero and within the encode ceiling
    ///   ([`MAX_CAPTURE_WIDTH`]x[`MAX_CAPTURE_HEIGHT`]); larger than the
    ///   target is fine — the worker upscales, so quality presets (4K on a
    ///   smaller display, like Discord) always work.
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
        let (_disp_w, _disp_h) = dimension_of(&monitor)?;
        validate_encode_size(width, height)?;

        // Validation passed: safe to replace any running session.
        self.launch(CaptureTarget::Display(display_id), width, height, fps)
    }

    /// Begin (or restart) grabbing `window_id` at `width`x`height` @ `fps`.
    ///
    /// Same contract as [`CaptureSession::start`]: the encode size may exceed
    /// the window (upscaled, Discord-style). A window closed between
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
        let (_win_w, _win_h) = window_dimensions(&window)?;
        validate_encode_size(width, height)?;

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

        // Eager first grab, synchronously: a start that cannot capture (no
        // permission, target gone) fails HERE instead of reporting success
        // and publishing black frames forever. This is also the single place
        // that may trigger the OS permission prompt — i.e. only on explicit
        // user intent (confirm), never on a preview/snapshot poll.
        let source = resolve_source(target)?;
        let (rgba, width, height) = grab_sized(&source, target, width, height)?;

        self.stop_flag.store(false, Ordering::SeqCst);
        *lock(&self.last_error) = None;
        *lock(&self.ema_interval_secs) = 0.0;
        *lock(&self.frame) = SharedFrame {
            rgba,
            width,
            height,
        };

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

/// A resolved grab source: the xcap handle behind a [`CaptureTarget`].
/// xcap handles are not `Send`, so resolution happens per thread/use and the
/// worker re-resolves by id (see `launch`); the eager first grab below
/// resolves on the caller's thread instead.
enum Source {
    Display(xcap::Monitor),
    Window(xcap::Window),
}

/// Resolve `target` to its xcap handle, or describe why it cannot be grabbed.
fn resolve_source(target: CaptureTarget) -> anyhow::Result<Source> {
    match target {
        CaptureTarget::Display(display_id) => xcap::Monitor::all()
            .context("failed to enumerate displays")?
            .into_iter()
            .find(|m| m.id().unwrap_or(u32::MAX) == display_id)
            .map(Source::Display)
            .ok_or_else(|| anyhow::anyhow!("display {display_id} not found")),
        CaptureTarget::Window(window_id) => xcap::Window::all()
            .context("failed to enumerate windows")?
            .into_iter()
            .find(|w| w.id().unwrap_or(u32::MAX) == window_id)
            .map(Source::Window)
            .ok_or_else(|| anyhow::anyhow!("window {window_id} not found")),
    }
}

/// One grab downscaled to exactly `width`x`height` (the session's contract;
/// see `grab_loop`). Fails when the OS refuses the grab — notably when
/// Screen Recording permission is missing, which is also what triggers the
/// one system prompt, so this must only run on explicit user intent (start),
/// never on a poll (snapshot/preview).
fn grab_sized(
    source: &Source,
    target: CaptureTarget,
    width: u32,
    height: u32,
) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let img = match source {
        Source::Display(m) => m.capture_image(),
        Source::Window(w) => w.capture_image(),
    }
    .map_err(|e| match target {
        CaptureTarget::Display(display_id) => {
            anyhow::anyhow!("capture failed (display {display_id}): {e}")
        }
        CaptureTarget::Window(window_id) => {
            anyhow::anyhow!("capture failed (window {window_id}): {e}")
        }
    })?;
    let rgba = if img.width() == width && img.height() == height {
        img.into_raw()
    } else {
        image::imageops::resize(&img, width, height, image::imageops::FilterType::Triangle)
            .into_raw()
    };
    Ok((rgba, width, height))
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
    let source = match resolve_source(target) {
        Ok(source) => source,
        Err(_) => {
            // Enumeration failed or the target vanished between validation
            // and thread start: record and exit instead of panicking.
            *lock(last_error) = Some(match target {
                CaptureTarget::Display(display_id) => {
                    format!("display {display_id} not found (disconnected before capture started)")
                }
                CaptureTarget::Window(window_id) => {
                    format!("window {window_id} not found (closed before capture started)")
                }
            });
            return;
        }
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

        match grab_sized(&source, target, width, height) {
            Ok((rgba, width, height)) => {
                consecutive_errors = 0;
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

/// Reject empty requests up front (encode-ceiling checks live in
/// [`validate_encode_size`], used by start paths that upscale).
fn validate_size(width: u32, height: u32) -> anyhow::Result<()> {
    if width == 0 || height == 0 {
        anyhow::bail!("capture size must be non-zero, got {width}x{height}");
    }
    Ok(())
}

/// Encode ceiling: quality presets may exceed the target (upscaled
/// Discord-style), but never exceed what the encoder pipeline is built for.
pub const MAX_CAPTURE_WIDTH: u32 = 3840;
pub const MAX_CAPTURE_HEIGHT: u32 = 2160;

/// Non-zero size within the encode ceiling. Larger than the target is fine
/// (the worker upscales); larger than the ceiling is a 400.
fn validate_encode_size(width: u32, height: u32) -> anyhow::Result<()> {
    validate_size(width, height)?;
    if width > MAX_CAPTURE_WIDTH || height > MAX_CAPTURE_HEIGHT {
        anyhow::bail!(
            "requested {width}x{height} exceeds encode ceiling {}x{}",
            MAX_CAPTURE_WIDTH,
            MAX_CAPTURE_HEIGHT
        );
    }
    Ok(())
}

/// Clamp the requested rate into the supported `1..=60` range.
fn clamp_fps(fps: u32) -> u32 {
    fps.clamp(1, 60)
}

fn window_dimensions(window: &xcap::Window) -> anyhow::Result<(u32, u32)> {
    let w = window.width().context("window is missing its width")?;
    let h = window.height().context("window is missing its height")?;
    Ok((w, h))
}

fn dimension_of(monitor: &xcap::Monitor) -> anyhow::Result<(u32, u32)> {
    let w = monitor.width().context("display is missing its width")?;
    let h = monitor.height().context("display is missing its height")?;
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

    fn solid_rgba(w: u32, h: u32, px: [u8; 4]) -> image::RgbaImage {
        image::RgbaImage::from_pixel(w, h, image::Rgba(px))
    }

    fn decode_icon_data_url(url: &str) -> Vec<u8> {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let b64 = url
            .strip_prefix("data:image/png;base64,")
            .expect("icon must be a PNG data URL");
        STANDARD.decode(b64).expect("icon base64 must decode")
    }

    #[test]
    fn icon_url_is_small_valid_png_at_icon_size() {
        // 128x128 solid red must come back a real PNG that fits the ICON_SIZE
        // box — this is the whole /capture/windows payload story per window.
        let url = icon_data_url(solid_rgba(128, 128, [255, 0, 0, 255]));
        let png = decode_icon_data_url(&url);
        assert_eq!(
            &png[0..8],
            &[137, 80, 78, 71, 13, 10, 26, 10],
            "missing PNG magic"
        );
        let decoded = image::load_from_memory(&png).expect("icon PNG must decode");
        assert!(decoded.width() <= ICON_SIZE && decoded.height() <= ICON_SIZE);
        assert!(decoded.width() <= 64 && decoded.height() <= 64);
        assert_eq!((decoded.width(), decoded.height()), (32, 32));
    }

    #[test]
    fn icon_url_keeps_aspect_and_never_upscales() {
        // Wide source keeps its ratio inside the box…
        let wide = icon_data_url(solid_rgba(128, 64, [0, 255, 0, 255]));
        let decoded = image::load_from_memory(&decode_icon_data_url(&wide)).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (32, 16));
        // …while a small source passes through at native size.
        let tiny = icon_data_url(solid_rgba(16, 16, [0, 0, 255, 255]));
        let decoded = image::load_from_memory(&decode_icon_data_url(&tiny)).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (16, 16));
    }

    #[test]
    fn window_serializes_its_icon_for_old_and_new_clients() {
        // Old web clients ignore unknown keys; new ones read `icon`, so the
        // field must always be present (possibly "").
        let w = Window {
            id: 7,
            name: "n".into(),
            app: "a".into(),
            icon: String::new(),
            width: 800,
            height: 600,
        };
        let v = serde_json::to_value(&w).unwrap();
        assert_eq!(v["icon"], "");
        let w2 = Window {
            icon: icon_data_url(solid_rgba(16, 16, [1, 2, 3, 255])),
            ..w
        };
        let v2 = serde_json::to_value(&w2).unwrap();
        assert!(v2["icon"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
    }
}

//! Hardware H.264 screen encoding on macOS: ScreenCaptureKit hands frames at
//! the requested rate, VideoToolbox compresses them on the media engine, and
//! what leaves is Annex-B access units the browser injects straight into its
//! MoQ publisher. The JPEG path in `capture` stays for previews and for
//! platforms without this pipeline.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// One encoded access unit, Annex-B framed (parameter sets in-band on keyframes).
#[derive(Clone, Debug)]
pub struct H264Frame {
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum H264Target {
    Display(u32),
    Window(u32),
}

#[derive(Clone, Copy, Debug)]
pub struct H264Config {
    pub target: H264Target,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

/// Frames fan out to whoever is streaming them; a reader that falls behind
/// skips ahead and waits for the next keyframe.
pub const FRAME_QUEUE: usize = 180;

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub struct H264Session {
    id: u64,
    config: H264Config,
    frames: tokio::sync::broadcast::Sender<Arc<H264Frame>>,
    stop: Arc<AtomicBool>,
    keyframe_wanted: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Set by the worker once capture and encoder are up, or with the reason they are not.
    ready: std::sync::mpsc::Receiver<Result<(), String>>,
}

impl std::fmt::Debug for H264Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H264Session").field("id", &self.id).field("config", &self.config).finish()
    }
}

impl H264Session {
    pub fn supported() -> bool {
        cfg!(target_os = "macos")
    }

    /// Starts capture + encoder on a thread of their own and waits until the
    /// first frame flowed, so a refusal (permission, bad id) surfaces here.
    pub fn start(config: H264Config) -> anyhow::Result<Self> {
        let (frames, _) = tokio::sync::broadcast::channel(FRAME_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let keyframe_wanted = Arc::new(AtomicBool::new(true));
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let worker = {
            let frames = frames.clone();
            let stop = Arc::clone(&stop);
            let keyframe_wanted = Arc::clone(&keyframe_wanted);
            std::thread::Builder::new()
                .name("jlocal-h264".into())
                .spawn(move || platform::run(config, frames, stop, keyframe_wanted, ready_tx))?
        };
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let session = Self { id, config, frames, stop, keyframe_wanted, worker: Some(worker), ready };
        match session.ready.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(session),
            Ok(Err(reason)) => anyhow::bail!("{reason}"),
            Err(_) => anyhow::bail!("h264 capture did not start in time"),
        }
    }

    pub fn config(&self) -> H264Config {
        self.config
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// A new reader needs a keyframe before anything else makes sense.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<H264Frame>> {
        self.keyframe_wanted.store(true, Ordering::Release);
        self.frames.subscribe()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for H264Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Turns a length-prefixed (AVCC) access unit into Annex-B, prepending the
/// parameter sets when asked. Pure, so it is testable without a GPU.
pub fn annex_b(avcc: &[u8], header_len: usize, parameter_sets: &[&[u8]]) -> Vec<u8> {
    const START: [u8; 4] = [0, 0, 0, 1];
    let mut out = Vec::with_capacity(avcc.len() + parameter_sets.iter().map(|p| p.len() + 4).sum::<usize>() + 16);
    for set in parameter_sets {
        out.extend_from_slice(&START);
        out.extend_from_slice(set);
    }
    let mut at = 0;
    while at + header_len <= avcc.len() {
        let mut len = 0usize;
        for byte in &avcc[at..at + header_len] {
            len = (len << 8) | *byte as usize;
        }
        at += header_len;
        if len == 0 || at + len > avcc.len() {
            break;
        }
        out.extend_from_slice(&START);
        out.extend_from_slice(&avcc[at..at + len]);
        at += len;
    }
    out
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{NSObjectProtocol, ProtocolObject};
    use objc2::AnyThread as _;
    use objc2::DefinedClass as _;
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
    use objc2_core_media::{
        kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_H264, CMFormatDescription, CMSampleBuffer, CMTime,
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    };
    use objc2_core_video::kCVPixelFormatType_32BGRA;
    use objc2_foundation::{NSArray, NSError};
    use objc2_screen_capture_kit::{
        SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
    };
    use objc2_video_toolbox::{
        kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
        kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
        kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
        kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_High_AutoLevel,
        kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, VTCompressionSession,
        VTEncodeInfoFlags, VTSession, VTSessionSetProperty,
    };
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::time::Duration;

    /// What both callbacks (SCK frames in, VT frames out) share.
    struct Shared {
        session: parking_lot::Mutex<Option<CFRetained<VTCompressionSession>>>,
        frames: tokio::sync::broadcast::Sender<Arc<H264Frame>>,
        stop: Arc<AtomicBool>,
        keyframe_wanted: Arc<AtomicBool>,
        first_frame: std::sync::mpsc::Sender<()>,
        fps: u32,
        captured: std::sync::atomic::AtomicU64,
        encoded: std::sync::atomic::AtomicU64,
        idle: std::sync::atomic::AtomicU64,
    }

    objc2::define_class!(
        // SAFETY: NSObject has no subclassing requirements; no Drop.
        #[unsafe(super(objc2::runtime::NSObject))]
        #[name = "JLocalVideoOutput"]
        #[ivars = Arc<Shared>]
        struct VideoOutput;

        unsafe impl NSObjectProtocol for VideoOutput {}
        unsafe impl SCStreamOutput for VideoOutput {
            #[allow(non_snake_case)]
            #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
            unsafe fn stream_didOutputSampleBuffer_ofType(
                &self,
                _stream: &SCStream,
                sample_buffer: &CMSampleBuffer,
                output_type: SCStreamOutputType,
            ) {
                if output_type != SCStreamOutputType::Screen {
                    return;
                }
                encode(self.ivars(), sample_buffer);
            }
        }
    );

    impl VideoOutput {
        fn new(shared: Arc<Shared>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(shared);
            // SAFETY: `init` on a fresh NSObject subclass.
            unsafe { objc2::msg_send![super(this), init] }
        }
    }

    fn encode(shared: &Shared, sample_buffer: &CMSampleBuffer) {
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: accessors on a valid sample buffer; an idle frame has no image and is skipped.
        let Some(image) = (unsafe { sample_buffer.image_buffer() }) else {
            shared.idle.fetch_add(1, Ordering::Relaxed);
            return;
        };
        shared.captured.fetch_add(1, Ordering::Relaxed);
        let pts = unsafe { sample_buffer.presentation_time_stamp() };
        let duration = unsafe { CMTime::new(1, shared.fps as i32) };
        let force = shared.keyframe_wanted.swap(false, Ordering::AcqRel);
        let props: Option<CFRetained<CFDictionary<CFString, CFType>>> = force.then(|| {
            let key: &CFString = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
            let yes: &CFType = CFBoolean::new(true).as_ref();
            CFDictionary::from_slices(&[key], &[yes])
        });
        let guard = shared.session.lock();
        let Some(session) = guard.as_ref() else { return };
        let mut flags = VTEncodeInfoFlags::empty();
        // SAFETY: session is alive under the lock; the buffer is valid for the call.
        let status = unsafe {
            session.encode_frame(&image, pts, duration, props.as_deref().map(|d| d.as_opaque()), std::ptr::null_mut(), &mut flags)
        };
        if status != 0 {
            tracing::debug!(status, "vt encode_frame refused a frame");
        }
    }

    unsafe extern "C-unwind" fn on_encoded(
        refcon: *mut c_void,
        _frame_refcon: *mut c_void,
        status: i32,
        _flags: VTEncodeInfoFlags,
        sample: *mut CMSampleBuffer,
    ) {
        if refcon.is_null() || status != 0 || sample.is_null() {
            return;
        }
        // SAFETY: refcon is the Arc<Shared> leaked in `run`, alive until the
        // session is invalidated; `sample` is valid for this callback.
        let shared = unsafe { &*(refcon as *const Shared) };
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        let sample = unsafe { &*sample };
        if let Some(frame) = unsafe { access_unit(sample) } {
            shared.encoded.fetch_add(1, Ordering::Relaxed);
            let _ = shared.first_frame.send(());
            let _ = shared.frames.send(Arc::new(frame));
        }
    }

    /// Pulls one encoded sample out as an Annex-B frame.
    unsafe fn access_unit(sample: &CMSampleBuffer) -> Option<H264Frame> {
        // A sample without a NotSync attachment is a sync sample (keyframe).
        let keyframe = match sample.sample_attachments_array(false) {
            Some(attachments) if attachments.len() > 0 => {
                let first = attachments.value_at_index(0) as *const CFDictionary<CFString, CFType>;
                if first.is_null() {
                    true
                } else {
                    let key: &CFString = kCMSampleAttachmentKey_NotSync;
                    match (*first).get(key) {
                        Some(value) => !value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool).unwrap_or(false),
                        None => true,
                    }
                }
            }
            _ => true,
        };
        let pts = sample.presentation_time_stamp();
        let pts_us = if pts.timescale > 0 { (pts.value as i128 * 1_000_000 / pts.timescale as i128).max(0) as u64 } else { 0 };
        let data = sample.data_buffer()?;
        let len = data.data_length();
        let mut avcc = vec![0u8; len];
        if len == 0 || data.copy_data_bytes(0, len, NonNull::new_unchecked(avcc.as_mut_ptr() as *mut c_void)) != 0 {
            return None;
        }
        let mut header_len = 4usize;
        let mut sets: Vec<Vec<u8>> = Vec::new();
        if keyframe {
            if let Some(desc) = sample.format_description() {
                sets = parameter_sets(&desc, &mut header_len);
            }
        }
        let refs: Vec<&[u8]> = sets.iter().map(Vec::as_slice).collect();
        Some(H264Frame { pts_us, keyframe, data: annex_b(&avcc, header_len, &refs) })
    }

    unsafe fn parameter_sets(desc: &CMFormatDescription, header_len: &mut usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut count = 0usize;
        let mut nal_len: std::ffi::c_int = 4;
        let mut ptr: *const u8 = std::ptr::null();
        let mut size = 0usize;
        if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(desc, 0, &mut ptr, &mut size, &mut count, &mut nal_len) != 0 {
            return out;
        }
        *header_len = nal_len.max(1) as usize;
        for index in 0..count {
            let mut ptr: *const u8 = std::ptr::null();
            let mut size = 0usize;
            if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(desc, index, &mut ptr, &mut size, std::ptr::null_mut(), std::ptr::null_mut()) != 0
                || ptr.is_null()
            {
                continue;
            }
            out.push(std::slice::from_raw_parts(ptr, size).to_vec());
        }
        out
    }

    fn set(session: &VTSession, key: &CFString, value: &CFType) -> anyhow::Result<()> {
        // SAFETY: key/value pairs below are the documented types for each property.
        let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
        if status != 0 {
            anyhow::bail!("vt property {key} refused ({status})");
        }
        Ok(())
    }

    unsafe fn make_encoder(config: &H264Config, refcon: *mut c_void) -> anyhow::Result<CFRetained<VTCompressionSession>> {
        let spec_key: &CFString = kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder;
        let spec: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[spec_key], &[CFBoolean::new(true).as_ref()]);
        let mut raw: *mut VTCompressionSession = std::ptr::null_mut();
        let status = VTCompressionSession::create(
            None,
            config.width as i32,
            config.height as i32,
            kCMVideoCodecType_H264,
            Some(spec.as_opaque()),
            None,
            None,
            Some(on_encoded),
            refcon,
            NonNull::from(&mut raw),
        );
        if status != 0 || raw.is_null() {
            anyhow::bail!("no hardware H.264 encoder ({status})");
        }
        let session = CFRetained::from_raw(NonNull::new_unchecked(raw));
        let s: &VTSession = session.as_ref();
        set(s, kVTCompressionPropertyKey_RealTime, CFBoolean::new(true).as_ref())?;
        set(s, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel.as_ref())?;
        set(s, kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false).as_ref())?;
        set(s, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, CFNumber::new_f64(2.0).as_ref())?;
        set(s, kVTCompressionPropertyKey_ExpectedFrameRate, CFNumber::new_i32(config.fps as i32).as_ref())?;
        set(s, kVTCompressionPropertyKey_AverageBitRate, CFNumber::new_i32(config.bitrate as i32).as_ref())?;
        let status = session.prepare_to_encode_frames();
        if status != 0 {
            anyhow::bail!("vt prepare refused ({status})");
        }
        Ok(session)
    }

    fn shareable_content(timeout: Duration) -> anyhow::Result<Retained<SCShareableContent>> {
        let (tx, rx) = std::sync::mpsc::channel::<Result<*mut SCShareableContent, String>>();
        let block: RcBlock<dyn Fn(*mut SCShareableContent, *mut NSError)> = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                let result = if !content.is_null() {
                    // SAFETY: SCK passes a valid object on success.
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
        // SAFETY: SCK copies the handler; the block outlives the call.
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
        let raw = rx
            .recv_timeout(timeout)
            .map_err(|_| anyhow::anyhow!("timed out listing shareable content"))?
            .map_err(|e| anyhow::anyhow!("permission: {e}"))?;
        // SAFETY: retained above, reconstructed once.
        unsafe { Retained::from_raw(raw) }.ok_or_else(|| anyhow::anyhow!("shareable content pointer was null"))
    }

    unsafe fn build_filter(content: &SCShareableContent, target: H264Target) -> anyhow::Result<Retained<SCContentFilter>> {
        match target {
            H264Target::Display(id) => {
                let display = content
                    .displays()
                    .iter()
                    .find(|display| display.displayID() == id)
                    .ok_or_else(|| anyhow::anyhow!("display {id} not found"))?;
                let empty = NSArray::<objc2_screen_capture_kit::SCWindow>::new();
                Ok(SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &empty))
            }
            H264Target::Window(id) => {
                let window = content
                    .windows()
                    .iter()
                    .find(|window| window.windowID() == id)
                    .ok_or_else(|| anyhow::anyhow!("window {id} not found"))?;
                Ok(SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window))
            }
        }
    }

    unsafe fn start_stream(
        filter: &SCContentFilter,
        config: &H264Config,
        output: &ProtocolObject<dyn SCStreamOutput>,
    ) -> anyhow::Result<Retained<SCStream>> {
        let queue = dispatch2::DispatchQueue::new("lol.juntos.jlocal.video", None);
        let sc = SCStreamConfiguration::new();
        sc.setWidth(config.width as usize);
        sc.setHeight(config.height as usize);
        sc.setMinimumFrameInterval(CMTime::new(1, config.fps as i32));
        sc.setPixelFormat(kCVPixelFormatType_32BGRA);
        sc.setShowsCursor(true);
        sc.setQueueDepth(6);
        sc.setCapturesAudio(false);
        let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), filter, &sc, None);
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(output, SCStreamOutputType::Screen, Some(&queue))
            .map_err(|e| anyhow::anyhow!("SCStream video output refused: {e}"))?;
        let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
        let block: RcBlock<dyn Fn(*mut NSError)> = RcBlock::new(move |error: *mut NSError| {
            let err = if error.is_null() { None } else { Some((&*error).localizedDescription().to_string()) };
            let _ = tx.send(err);
        });
        stream.startCaptureWithCompletionHandler(Some(&block));
        if let Some(err) = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow::anyhow!("timed out starting screen capture"))?
        {
            anyhow::bail!("screen capture refused: {err}");
        }
        Ok(stream)
    }

    unsafe fn stop_stream(stream: &SCStream) {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let block: RcBlock<dyn Fn(*mut NSError)> = RcBlock::new(move |_error: *mut NSError| {
            let _ = tx.send(());
        });
        stream.stopCaptureWithCompletionHandler(Some(&block));
        let _ = rx.recv_timeout(Duration::from_secs(5));
    }

    pub(super) fn run(
        config: H264Config,
        frames: tokio::sync::broadcast::Sender<Arc<H264Frame>>,
        stop: Arc<AtomicBool>,
        keyframe_wanted: Arc<AtomicBool>,
        ready: std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        objc2::rc::autoreleasepool(|_| {
            let (first_tx, first_rx) = std::sync::mpsc::channel();
            let shared = Arc::new(Shared {
                session: parking_lot::Mutex::new(None),
                frames,
                stop: Arc::clone(&stop),
                keyframe_wanted,
                first_frame: first_tx,
                fps: config.fps.max(1),
                captured: std::sync::atomic::AtomicU64::new(0),
                encoded: std::sync::atomic::AtomicU64::new(0),
                idle: std::sync::atomic::AtomicU64::new(0),
            });
            // The encoder callback gets a raw pointer to the same Shared; the
            // Arc below keeps it alive until the session is invalidated.
            let refcon = Arc::into_raw(Arc::clone(&shared)) as *mut c_void;
            let outcome: anyhow::Result<(Retained<SCStream>, Retained<VideoOutput>)> = (|| {
                // SAFETY: all objects are created and driven on this thread.
                unsafe {
                    let encoder = make_encoder(&config, refcon)?;
                    *shared.session.lock() = Some(encoder);
                    let content = shareable_content(Duration::from_secs(10))?;
                    let filter = build_filter(&content, config.target)?;
                    let output = VideoOutput::new(Arc::clone(&shared));
                    let stream = start_stream(&filter, &config, ProtocolObject::from_ref(&*output))?;
                    Ok((stream, output))
                }
            })();
            let (stream, _output) = match outcome {
                Ok(pair) => pair,
                Err(e) => {
                    *shared.session.lock() = None;
                    // SAFETY: undoing the into_raw above; no callback can fire without a session.
                    drop(unsafe { Arc::from_raw(refcon as *const Shared) });
                    let _ = ready.send(Err(e.to_string()));
                    return;
                }
            };
            let started = first_rx.recv_timeout(Duration::from_secs(10)).is_ok();
            let _ = ready.send(if started { Ok(()) } else { Err("capture produced no frames".to_string()) });
            if started {
                while !stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            tracing::info!(
                captured = shared.captured.load(Ordering::Relaxed),
                encoded = shared.encoded.load(Ordering::Relaxed),
                idle = shared.idle.load(Ordering::Relaxed),
                "h264 session ending"
            );
            // SAFETY: same thread that started it.
            unsafe { stop_stream(&stream) };
            let session = shared.session.lock().take();
            if let Some(session) = session {
                // SAFETY: no more frames arrive once the stream stopped and the slot is empty.
                unsafe { session.invalidate() };
            }
            // SAFETY: the encoder is gone, so its callback cannot use refcon anymore.
            drop(unsafe { Arc::from_raw(refcon as *const Shared) });
        });
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub(super) fn run(
        _config: H264Config,
        _frames: tokio::sync::broadcast::Sender<Arc<H264Frame>>,
        _stop: Arc<AtomicBool>,
        _keyframe_wanted: Arc<AtomicBool>,
        ready: std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        let _ = ready.send(Err("h264 capture is macOS only".to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_rewrites_lengths_and_prepends_parameter_sets() {
        let avcc = [0, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 1, 0x41];
        let sps = [0x67, 0x64, 0x00, 0x28];
        let pps = [0x68, 0xEE];
        let out = annex_b(&avcc, 4, &[&sps, &pps]);
        assert_eq!(
            out,
            vec![0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0, 0, 0, 1, 0x68, 0xEE, 0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x41]
        );
    }

    #[test]
    fn annex_b_stops_at_a_truncated_nal() {
        let avcc = [0, 0, 0, 9, 0x65];
        assert!(annex_b(&avcc, 4, &[]).is_empty());
    }
}

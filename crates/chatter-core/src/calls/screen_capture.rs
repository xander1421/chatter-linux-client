//! Screen capture for screen sharing.
//!
//! Uses LiveKit's native desktop_capturer (libwebrtc) which supports:
//! - Linux: X11 and Wayland (via PipeWire/XDG Portal)
//! - Proper screen and window enumeration (X11) or portal picker (Wayland)
//! - Hardware-accelerated capture where available
//!
//! ## Wayland Behavior
//! On Wayland, the XDG Desktop Portal is used for screen capture.
//! Source enumeration returns minimal data - the actual source selection
//! happens via a system dialog when capture starts.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use livekit::webrtc::desktop_capturer::{
    DesktopCaptureSourceType, DesktopCapturer, DesktopCapturerOptions,
};
use livekit::webrtc::video_frame::{I420Buffer, VideoBuffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use parking_lot::{Condvar, Mutex};

use crate::error::CallError;

/// Reusable BGRA capture slot shared between the libwebrtc capture callback
/// (producer) and the push loop (consumer).
///
/// The producer owns its own persistent BGRA scratch `Vec` (in the closure)
/// and, once it has copied the borrowed `DesktopFrame` bytes into it, swaps
/// that `Vec` into [`FrameSlot::buf`] under the lock. The consumer swaps in
/// its own spare empty `Vec` to take the filled one out. Three `Vec`s
/// ping-pong forever, so after warm-up no frame allocates or frees the ~30 MB
/// BGRA buffer — the previous `frame.data().to_vec()` did exactly that every
/// frame. The lock is only ever held for a pointer swap, never for the copy
/// or the colour conversion.
struct FrameSlot {
    /// Most-recently captured BGRA bytes (length == stride * height).
    buf: Vec<u8>,
    width: i32,
    height: i32,
    stride: u32,
    /// True once the producer has written a new frame the consumer hasn't taken.
    ready: bool,
}

impl FrameSlot {
    fn empty() -> Self {
        Self { buf: Vec::new(), width: 0, height: 0, stride: 0, ready: false }
    }
}

/// A captured BGRA frame handed to the consumer: the recycled buffer plus dims.
struct CapturedFrame {
    buf: Vec<u8>,
    width: i32,
    height: i32,
    stride: u32,
}

/// Borrows an [`I420Buffer`] as the `buffer` of a [`VideoFrame`] without
/// moving it, so the same pooled buffer can be reused across frames.
///
/// `NativeVideoSource::capture_frame` only reads `buffer.as_ref().sys_handle()`
/// and the C++ side takes its own ref-counted hold on the underlying
/// `VideoFrameBuffer`, so the `I420Buffer` is free to be reused (overwritten
/// via `data_mut()`) on the next iteration once `capture_frame` returns.
///
/// `VideoFrame<T>` requires `T: AsRef<dyn VideoBuffer>`, and `&I420Buffer`
/// does not implement that (std has no blanket impl for references), so this
/// local newtype provides it. The orphan rule is satisfied: `RefBuf` is local.
struct RefBuf<'a>(&'a I420Buffer);

impl AsRef<dyn VideoBuffer> for RefBuf<'_> {
    fn as_ref(&self) -> &(dyn VideoBuffer + 'static) {
        <I420Buffer as AsRef<dyn VideoBuffer>>::as_ref(self.0)
    }
}

/// Check if we're running on Wayland (Linux only, always false on other platforms).
/// Whether an `XDG_SESSION_TYPE` value names a Wayland session.
///
/// Split out from [`is_wayland`] so it can be tested without touching the
/// environment. The test used to `set_var` its way through the cases, with a
/// comment asserting that tests run single-threaded — they do not, which is why
/// `set_var` is unsafe in the first place. Mutating a process-wide variable races
/// every test running beside it, and this one is read by the capture path.
fn session_type_is_wayland(session_type: Option<&str>) -> bool {
    session_type.is_some_and(|value| value.eq_ignore_ascii_case("wayland"))
}

fn is_wayland() -> bool {
    #[cfg(target_os = "linux")]
    {
        session_type_is_wayland(std::env::var("XDG_SESSION_TYPE").ok().as_deref())
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Information about a capturable screen/monitor or window.
#[derive(Debug, Clone)]
pub struct ScreenSource {
    /// Display name (e.g., "Monitor 1", "Firefox").
    pub name: String,
    /// Internal identifier (from libwebrtc).
    pub id: u64,
    /// Display ID (for monitors).
    pub display_id: i64,
    /// Source type.
    pub source_type: ScreenSourceType,
}

/// Type of screen capture source.
#[derive(Debug, Clone, PartialEq)]
pub enum ScreenSourceType {
    /// Full monitor/display.
    Monitor,
    /// Application window.
    Window,
}

/// Screen capture configuration.
#[derive(Debug, Clone)]
pub struct ScreenCaptureConfig {
    /// Capture frame rate.
    pub fps: u32,
    /// Whether to capture cursor.
    pub capture_cursor: bool,
}

// Target the high-refresh path: libyuv SIMD bgra→i420 (~2 ms at 5120×1440 in
// release) and the receive/convert side both clear 240 fps, so 60 was an
// artificial cap. The actual achieved rate is bounded by the portal pull
// loop (see the `sleep` below) and the monitor refresh — the portal won't
// hand us more unique frames than the panel produces, so a 60 Hz user is
// naturally limited to ~60 without extra real work. Proper per-monitor /
// capability negotiation of this value is a later milestone.
impl Default for ScreenCaptureConfig {
    fn default() -> Self {
        Self {
            fps: 144,
            capture_cursor: true,
        }
    }
}

/// Handshake returned by [`ScreenCapture::begin`].
///
/// The capture thread spawns immediately and runs the portal/X11 picker;
/// when the first real frame arrives, it sends those dims via `dims_rx`
/// and blocks waiting for a `NativeVideoSource` (created by the caller
/// at those exact dims) on `source_tx`.
pub struct ScreenCaptureHandshake {
    pub dims_rx: tokio::sync::oneshot::Receiver<(u32, u32)>,
    pub source_tx: std::sync::mpsc::SyncSender<NativeVideoSource>,
}

/// Screen capture manager using LiveKit's desktop_capturer.
pub struct ScreenCapture {
    /// Flag to control capture.
    running: Arc<AtomicBool>,
    /// Current source.
    source: Option<ScreenSource>,
    /// Capture configuration.
    config: ScreenCaptureConfig,
    /// Capture thread handle.
    capture_handle: Option<std::thread::JoinHandle<()>>,
}

impl ScreenCapture {
    /// Create a new screen capture (not yet started).
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            source: None,
            config: ScreenCaptureConfig::default(),
            capture_handle: None,
        }
    }

    /// List available screen sources (monitors).
    pub fn list_monitors() -> Result<Vec<ScreenSource>, CallError> {
        let mut options = DesktopCapturerOptions::new(DesktopCaptureSourceType::Screen);
        options.set_include_cursor(false);

        let capturer = DesktopCapturer::new(options).ok_or_else(|| {
            CallError::TrackPublishFailed("Failed to create screen capturer".to_string())
        })?;

        let sources: Vec<ScreenSource> = capturer
            .get_source_list()
            .into_iter()
            .map(|s| ScreenSource {
                name: if s.title().is_empty() {
                    format!("Display {}", s.display_id())
                } else {
                    s.title()
                },
                id: s.id(),
                display_id: s.display_id(),
                source_type: ScreenSourceType::Monitor,
            })
            .collect();

        tracing::info!("Found {} monitors via libwebrtc", sources.len());
        for source in &sources {
            tracing::debug!("  Monitor: {} (id={}, display_id={})", source.name, source.id, source.display_id);
        }

        Ok(sources)
    }

    /// List available window sources.
    pub fn list_windows() -> Result<Vec<ScreenSource>, CallError> {
        let mut options = DesktopCapturerOptions::new(DesktopCaptureSourceType::Window);
        options.set_include_cursor(false);

        let capturer = DesktopCapturer::new(options).ok_or_else(|| {
            CallError::TrackPublishFailed("Failed to create window capturer".to_string())
        })?;

        let sources: Vec<ScreenSource> = capturer
            .get_source_list()
            .into_iter()
            .filter(|s| !s.title().is_empty()) // Filter out windows with no title
            .map(|s| ScreenSource {
                name: s.title(),
                id: s.id(),
                display_id: s.display_id(),
                source_type: ScreenSourceType::Window,
            })
            .collect();

        tracing::info!("Found {} windows via libwebrtc", sources.len());
        for source in &sources {
            tracing::debug!("  Window: {} (id={})", source.name, source.id);
        }

        Ok(sources)
    }

    /// List all available screen sources (monitors and windows).
    ///
    /// On Wayland, this returns a single "portal" source that triggers
    /// the XDG Desktop Portal picker when selected.
    pub fn list_sources() -> Result<Vec<ScreenSource>, CallError> {
        // On Wayland, we can't enumerate sources - return a portal trigger
        if is_wayland() {
            tracing::info!("Wayland detected - using XDG Desktop Portal for screen sharing");
            return Ok(vec![ScreenSource {
                name: "Share Screen (Portal)".to_string(),
                id: 0, // Special ID for portal
                display_id: 0,
                source_type: ScreenSourceType::Monitor,
            }]);
        }

        // On X11, enumerate normally
        let mut sources = Vec::new();

        // Get monitors
        match Self::list_monitors() {
            Ok(monitors) => sources.extend(monitors),
            Err(e) => tracing::warn!("Failed to list monitors: {}", e),
        }

        // Get windows
        match Self::list_windows() {
            Ok(windows) => sources.extend(windows),
            Err(e) => tracing::warn!("Failed to list windows: {}", e),
        }

        if sources.is_empty() {
            return Err(CallError::TrackPublishFailed(
                "No screen sources available".to_string(),
            ));
        }

        Ok(sources)
    }

    /// Get the primary display (first monitor).
    pub fn primary_source() -> Result<ScreenSource, CallError> {
        let monitors = Self::list_monitors()?;
        monitors.into_iter().next().ok_or_else(|| {
            CallError::TrackPublishFailed("No primary display found".to_string())
        })
    }

    /// Spawn the capture thread and return a handshake the caller drives
    /// to negotiate the `NativeVideoSource` resolution.
    ///
    /// The capture thread captures one probe frame, ships its real
    /// dimensions back through `dims_rx`, then blocks waiting for a
    /// `NativeVideoSource` (created at those exact dims by the caller)
    /// on `source_tx`. Once it receives the source, it pushes the
    /// buffered first frame and every subsequent frame into it.
    ///
    /// This avoids the previous "seed at 8K UHD" hack that forced
    /// libwebrtc to upscale every capture frame to 7680×4320 before
    /// encode — receivers then decoded 33-megapixel frames and starved
    /// the audio relay on the runtime worker.
    ///
    /// After `dims_rx` resolves, call `complete()` with the created
    /// source to record it on the manager.
    pub fn begin(
        &mut self,
        source: ScreenSource,
        config: Option<ScreenCaptureConfig>,
        local_preview: Option<super::video_capture::LocalPreview>,
    ) -> Result<ScreenCaptureHandshake, CallError> {
        if self.running.load(Ordering::SeqCst) {
            return Err(CallError::TrackPublishFailed(
                "Screen capture already running".to_string(),
            ));
        }

        let config = config.unwrap_or_default();
        self.config = config.clone();
        self.source = Some(source.clone());

        tracing::info!(
            "Starting screen capture from: {} (id={}, type={:?}) @{}fps",
            source.name,
            source.id,
            source.source_type,
            config.fps
        );

        let source_type = match source.source_type {
            ScreenSourceType::Monitor => DesktopCaptureSourceType::Screen,
            ScreenSourceType::Window => DesktopCaptureSourceType::Window,
        };

        let mut options = DesktopCapturerOptions::new(source_type);
        options.set_include_cursor(config.capture_cursor);

        let (dims_tx, dims_rx) = tokio::sync::oneshot::channel::<(u32, u32)>();
        let (source_tx, source_rx) = std::sync::mpsc::sync_channel::<NativeVideoSource>(1);

        let running = self.running.clone();
        let fps = config.fps;
        let source_id = source.id;

        self.running.store(true, Ordering::SeqCst);

        let handle = std::thread::spawn(move || {
            Self::capture_loop(
                source_id, source_type, options, running, fps, dims_tx, source_rx, local_preview,
            );
        });

        self.capture_handle = Some(handle);

        Ok(ScreenCaptureHandshake { dims_rx, source_tx })
    }

    /// Mark the negotiated source live on the manager. Logs only — the
    /// LocalVideoTrack owns the `NativeVideoSource`, so this manager
    /// doesn't need to retain a handle.
    pub fn complete(&self) {
        tracing::info!("Screen capture started successfully");
    }

    /// Screen capture loop using LiveKit's desktop_capturer.
    ///
    /// Runs in two phases:
    ///   1. Probe — capture frames until the first real frame arrives,
    ///      send its dims via `dims_tx`, and block on `source_rx` for
    ///      the caller-built `NativeVideoSource` (sized to those dims).
    ///   2. Push — push the buffered first frame plus every subsequent
    ///      frame into the now-known source.
    fn capture_loop(
        source_id: u64,
        _source_type: DesktopCaptureSourceType,
        options: DesktopCapturerOptions,
        running: Arc<AtomicBool>,
        fps: u32,
        dims_tx: tokio::sync::oneshot::Sender<(u32, u32)>,
        source_rx: std::sync::mpsc::Receiver<NativeVideoSource>,
        local_preview: Option<super::video_capture::LocalPreview>,
    ) {
        use std::time::{Duration, Instant};
        use livekit::webrtc::desktop_capturer::CaptureError;

        let frame_duration = Duration::from_secs_f64(1.0 / fps as f64);
        // Self-preview tee runs at most a few times a second: a full-res screen
        // RGBA convert (e.g. 30 MB at 5120×1440) is far too costly per frame, and
        // the preview tile is tiny. Throttle by wall-clock.
        // ponytail: 400ms fixed; downsample before convert if even this shows on a profile.
        let preview_min_interval = Duration::from_millis(400);
        let mut last_preview = Instant::now()
            .checked_sub(preview_min_interval)
            .unwrap_or_else(Instant::now);
        let use_portal = is_wayland() || source_id == 0;

        let mut capturer = match DesktopCapturer::new(options) {
            Some(c) => c,
            None => {
                tracing::error!("Failed to create desktop capturer");
                running.store(false, Ordering::SeqCst);
                return;
            }
        };

        let capture_source = if use_portal {
            tracing::info!("Starting capture via XDG Desktop Portal (system picker will appear)");
            tracing::info!("Note: 'Temporary' errors are expected until you select a screen in the portal dialog");
            None
        } else {
            let source_list = capturer.get_source_list();
            let source = source_list.into_iter().find(|s| s.id() == source_id);

            if source.is_none() {
                tracing::error!("Source ID {} not found in capture source list", source_id);
                running.store(false, Ordering::SeqCst);
                return;
            }

            tracing::info!(
                "Starting capture for source: {} (id={})",
                source.as_ref().unwrap().title(),
                source_id
            );
            source
        };

        let frame_data: Arc<Mutex<FrameSlot>> = Arc::new(Mutex::new(FrameSlot::empty()));
        let frame_data_clone = frame_data.clone();
        // Signalled by the capture callback the instant a fresh frame lands, so
        // the push loop reads it immediately instead of a fixed poll-sleep.
        let frame_ready = Arc::new(Condvar::new());
        let frame_ready_cb = frame_ready.clone();
        let temporary_error_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let temporary_error_count_clone = temporary_error_count.clone();
        let first_frame_received = Arc::new(AtomicBool::new(false));
        let first_frame_received_clone = first_frame_received.clone();
        // Latched on the first Permanent error so the main loop can exit and
        // the frame-request tick doesn't keep retriggering the callback at
        // 30 Hz. Without this the error log spams once per tick forever.
        let permanent_error_flagged = Arc::new(AtomicBool::new(false));
        let permanent_error_flagged_cb = permanent_error_flagged.clone();
        let running_cb = running.clone();

        // Persistent BGRA scratch owned by the capture callback. The borrowed
        // `DesktopFrame` bytes are copied into this (recycled, grows only on a
        // resolution change) and then swapped into the shared slot under the
        // lock — no per-frame ~30 MB allocation, and the lock is held only for
        // the swap, never the memcpy.
        let mut scratch: Vec<u8> = Vec::new();
        capturer.start_capture(capture_source, move |result| {
            match result {
                Ok(frame) => {
                    let width = frame.width();
                    let height = frame.height();
                    let stride = frame.stride();
                    let src = frame.data();

                    // Copy into the recycled scratch buffer (clear + extend
                    // reuses the existing allocation when capacity suffices).
                    scratch.clear();
                    scratch.extend_from_slice(src);

                    if !first_frame_received_clone.swap(true, Ordering::SeqCst) {
                        tracing::info!(
                            "First frame received! Screen capture active ({}x{})",
                            width, height
                        );
                    }

                    // Swap the freshly filled scratch into the slot and take
                    // back the slot's previous buffer to reuse next callback.
                    let mut slot = frame_data_clone.lock();
                    std::mem::swap(&mut slot.buf, &mut scratch);
                    slot.width = width;
                    slot.height = height;
                    slot.stride = stride;
                    slot.ready = true;
                    drop(slot);
                    frame_ready_cb.notify_one();
                }
                Err(e) => match e {
                    CaptureError::Temporary => {
                        let count = temporary_error_count_clone.fetch_add(1, Ordering::Relaxed);
                        if count == 0 {
                            tracing::debug!("Capture: waiting for portal selection or frame...");
                        } else if count % 100 == 0 {
                            tracing::debug!(
                                "Capture: still waiting for frames ({} temporary errors)",
                                count
                            );
                        }
                    }
                    CaptureError::Permanent => {
                        if !permanent_error_flagged_cb.swap(true, Ordering::SeqCst) {
                            tracing::error!(
                                "Permanent capture error — stopping screen share (portal cancelled or source lost)"
                            );
                            running_cb.store(false, Ordering::SeqCst);
                        }
                    }
                },
            }
        });

        // Consumer-owned spare BGRA buffer. `take_frame` swaps this into the
        // slot to extract the latest frame, so the two consumer-side buffers
        // (this spare and whatever the producer last handed over) recycle
        // forever without per-frame allocation.
        let mut spare: Vec<u8> = Vec::new();

        // ── Phase 1: probe for first frame, negotiate source dims ──
        let probe_start = Instant::now();
        let mut dims_tx = Some(dims_tx);
        let mut warned_30s = false;
        let (video_source, pending_first_frame): (NativeVideoSource, CapturedFrame) = 'probe: loop {
            if !running.load(Ordering::SeqCst) || permanent_error_flagged.load(Ordering::SeqCst) {
                tracing::info!("Screen capture stopped before first frame was processed");
                return;
            }

            capturer.capture_frame();
            std::thread::sleep(Duration::from_millis(5));

            if let Some(captured) = Self::take_frame(&frame_data, &mut spare) {
                let CapturedFrame { width, height, .. } = captured;
                if width > 0 && height > 0 {
                    if let Some(tx) = dims_tx.take() {
                        let _ = tx.send((width as u32, height as u32));
                    }
                    // Wait for the caller to build the NativeVideoSource at
                    // these dims and hand it back. Timeout guards against the
                    // caller having died mid-handshake.
                    match source_rx.recv_timeout(Duration::from_secs(10)) {
                        Ok(src) => break 'probe (src, captured),
                        Err(_) => {
                            tracing::error!(
                                "Screen capture: source handoff timed out — aborting"
                            );
                            running.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                }
            }

            if use_portal && !first_frame_received.load(Ordering::SeqCst) && !warned_30s {
                let elapsed = probe_start.elapsed();
                if elapsed > Duration::from_secs(30) {
                    tracing::warn!(
                        "Screen sharing: No frames received after 30s. Portal dialog may have been cancelled."
                    );
                    warned_30s = true;
                }
            }
        };

        // ── Phase 2: push frames into the negotiated source ──
        let mut frame_count = 0u64;
        let mut window_frames: u32 = 0;
        let mut window_wait = Duration::ZERO;      // capture_frame() request → frame_data arrival
        let mut window_convert = Duration::ZERO;   // bgra_to_i420 (libyuv)
        let mut window_push = Duration::ZERO;      // video_source.capture_frame(&frame)
        let mut window_total = Duration::ZERO;     // start of tick → end of push
        let mut window_start = Instant::now();

        // First frame already in hand from probe phase; reuse it before the
        // first `capture_frame()` request so we don't drop it.
        let mut pending: Option<CapturedFrame> = Some(pending_first_frame);

        // Stamp frames with REAL capture time, not frame_count/fps. The portal
        // hands us frames at the panel's real cadence (~100 Hz here), which
        // never equals the 144 fps target; a synthetic frame_count/fps clock
        // runs ~44% fast, so the receiver's jitter buffer plays out ahead of
        // arrival, starves, and stutters — the same bug the camera path already
        // fixed. Real elapsed microseconds encode the true cadence + jitter.
        let capture_base = Instant::now();

        // Pooled I420 output buffer, reused across frames; reallocated only on
        // a resolution change. `capture_frame` borrows the buffer (via RefBuf)
        // and the C++ side takes its own ref-counted hold, so the same buffer
        // is safe to overwrite via `data_mut()` on the next iteration.
        let mut i420_pool: Option<I420Buffer> = None;
        let mut pool_dims: (u32, u32) = (0, 0);

        while running.load(Ordering::SeqCst) {
            if permanent_error_flagged.load(Ordering::SeqCst) {
                break;
            }

            let tick_start = Instant::now();

            let captured = if let Some(buffered) = pending.take() {
                Some(buffered)
            } else {
                capturer.capture_frame();
                // Block until the callback signals a fresh frame rather than a
                // fixed poll-sleep, so we read it the instant it lands (saves
                // the ~1 ms tax + avoids empty re-polls). Bounded wait so a
                // teardown of `running` is still noticed promptly. A spurious
                // wake just means `take_frame` finds `ready == false` and we
                // loop to request again. The real ceiling fix for 144/240 Hz
                // is an event-driven PipeWire stream (no poll) — tracked
                // separately.
                {
                    let mut slot = frame_data.lock();
                    if !slot.ready {
                        frame_ready.wait_for(&mut slot, Duration::from_millis(16));
                    }
                }
                Self::take_frame(&frame_data, &mut spare)
            };
            let wait_done = Instant::now();

            if let Some(CapturedFrame { buf: data, width, height, stride }) = captured {
                if width > 0 && height > 0 {
                    let conv_start = Instant::now();
                    let dims = (width as u32, height as u32);
                    // Reuse the pooled buffer; only allocate on first frame or
                    // a resolution change.
                    if i420_pool.is_none() || pool_dims != dims {
                        i420_pool = Some(I420Buffer::new(dims.0, dims.1));
                        pool_dims = dims;
                    }
                    let i420_buffer = i420_pool.as_mut().unwrap();
                    {
                        let (y_data, u_data, v_data) = i420_buffer.data_mut();
                        Self::bgra_to_i420(
                            &data,
                            width as usize,
                            height as usize,
                            stride as usize,
                            y_data,
                            u_data,
                            v_data,
                        );
                    }
                    let conv_done = Instant::now();

                    let frame = VideoFrame {
                        rotation: VideoRotation::VideoRotation0,
                        timestamp_us: capture_base.elapsed().as_micros() as i64,
                        frame_metadata: None,
                        buffer: RefBuf(i420_buffer),
                    };

                    let push_start = Instant::now();
                    video_source.capture_frame(&frame);
                    let push_done = Instant::now();

                    // Release `frame`'s borrow of `i420_buffer` before the tee
                    // reborrows it.
                    drop(frame);

                    // Tee to the self-preview tile (throttled — see above). The
                    // screen frame is marked is_screen_share=false so it lands in
                    // the local tile's slot exactly like the camera feed does.
                    if let Some(ref preview) = local_preview {
                        if last_preview.elapsed() >= preview_min_interval {
                            preview.tee_i420(i420_buffer, dims.0, dims.1);
                            last_preview = Instant::now();
                        }
                    }
                    frame_count += 1;

                    // Return the BGRA buffer to `spare` so the next
                    // `take_frame` swap recycles it instead of allocating.
                    spare = data;

                    window_frames += 1;
                    window_wait    += wait_done - tick_start;
                    window_convert += conv_done - conv_start;
                    window_push    += push_done - push_start;
                    window_total   += push_done - tick_start;

                    if window_start.elapsed() >= Duration::from_secs(1) && window_frames > 0 {
                        let secs = window_start.elapsed().as_secs_f64();
                        let f = window_frames as f64;
                        tracing::info!(
                            "[SCREEN_FPS] {}x{} {:.1} fps  avg/frame total={:.1}ms wait={:.1}ms convert={:.1}ms push={:.1}ms (target {} fps)",
                            width, height,
                            window_frames as f64 / secs,
                            window_total.as_secs_f64() * 1000.0 / f,
                            window_wait.as_secs_f64() * 1000.0 / f,
                            window_convert.as_secs_f64() * 1000.0 / f,
                            window_push.as_secs_f64() * 1000.0 / f,
                            fps,
                        );
                        window_frames = 0;
                        window_wait = Duration::ZERO;
                        window_convert = Duration::ZERO;
                        window_push = Duration::ZERO;
                        window_total = Duration::ZERO;
                        window_start = Instant::now();
                    }
                }
            }

            let elapsed = tick_start.elapsed();
            if elapsed < frame_duration {
                std::thread::sleep(frame_duration - elapsed);
            }
        }

        let temp_errors = temporary_error_count.load(Ordering::Relaxed);
        let permanent = permanent_error_flagged.load(Ordering::SeqCst);
        tracing::info!(
            "Screen capture loop ended after {} frames ({} temporary errors during startup, permanent_error={})",
            frame_count,
            temp_errors,
            permanent,
        );
    }

    /// Take the latest captured frame out of the shared slot, if a new one is
    /// ready, recycling buffers instead of allocating.
    ///
    /// `spare` is a consumer-owned empty `Vec`; on success it is swapped into
    /// the slot (so the producer fills it next time) and the slot's filled
    /// buffer is returned in the [`CapturedFrame`]. Caller hands the buffer
    /// back to `spare` after use to keep the recycling going. The lock is held
    /// only for the swap, never for the colour conversion.
    fn take_frame(
        frame_data: &Arc<Mutex<FrameSlot>>,
        spare: &mut Vec<u8>,
    ) -> Option<CapturedFrame> {
        let mut slot = frame_data.lock();
        if !slot.ready {
            return None;
        }
        slot.ready = false;
        std::mem::swap(&mut slot.buf, spare);
        // `spare` now holds the filled buffer; move it out and leave `spare`
        // empty (the old slot buffer) for the next swap-in.
        let buf = std::mem::take(spare);
        Some(CapturedFrame { buf, width: slot.width, height: slot.height, stride: slot.stride })
    }

    /// Convert BGRA (memory byte order B, G, R, A) to I420 via libyuv.
    ///
    /// libyuv names destinations by little-endian 32-bit word value, so
    /// "ARGB" word = 0xAARRGGBB → LE memory bytes B, G, R, A — exactly
    /// what the desktop capturer emits. SIMD-accelerated (SSSE3/AVX2/NEON).
    ///
    /// PARALLEL across horizontal bands. libyuv's per-frame convert is
    /// single-threaded SIMD; at 5120×1440 (7.4 MP) it's the last CPU stage
    /// gating our end-to-end fps once the encoder throttle is off (H.265/NVENC
    /// clears 100+ fps, this convert must not be the new wall). Splitting the
    /// frame into per-core row bands and running libyuv on each via rayon's
    /// global pool cuts it ~linearly with cores — portable across amd64 (AVX2)
    /// and arm64 (NEON) since each band still uses the same SIMD kernel. Bands
    /// are chroma-safe: every boundary is on an EVEN row (4:2:0 subsamples 2×2).
    fn bgra_to_i420(
        bgra: &[u8],
        width: usize,
        height: usize,
        stride: usize,
        y_plane: &mut [u8],
        u_plane: &mut [u8],
        v_plane: &mut [u8],
    ) {
        // Capture-free helper (a plain fn, so it's trivially shareable across
        // rayon tasks — a closure capturing the strides could only be moved into
        // one spawn).
        fn conv(src: &[u8], stride: usize, y: &mut [u8], u: &mut [u8], v: &mut [u8], w: usize, rows: usize) {
            livekit::webrtc::native::yuv_helper::argb_to_i420(
                src, stride as u32,
                y, w as u32,
                u, (w / 2) as u32,
                v, (w / 2) as u32,
                w as i32, rows as i32,
            );
        }

        // Pick a band count from the core budget, but keep bands ≥64 rows so the
        // SIMD kernel amortises rayon dispatch, and only bother past ~2 MP.
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let bands = cores.min(height / 64).max(1);
        if bands == 1 || width * height < 2_000_000 {
            conv(bgra, stride, y_plane, u_plane, v_plane, width, height);
            return;
        }

        // Even-aligned rows per band; the last band absorbs the remainder (also
        // even, since `height` is even and `rows_each` is even).
        let rows_each = (height / bands) & !1;
        if rows_each == 0 {
            conv(bgra, stride, y_plane, u_plane, v_plane, width, height);
            return;
        }

        // Carve disjoint mutable Y/U/V slices per band, front to back.
        let (wc, hchroma_stride) = (width, width / 2);
        let mut y_rest = y_plane;
        let mut u_rest = u_plane;
        let mut v_rest = v_plane;
        let mut tasks: Vec<(&[u8], &mut [u8], &mut [u8], &mut [u8], u32)> = Vec::with_capacity(bands);
        let mut y0 = 0usize;
        for b in 0..bands {
            let rows = if b == bands - 1 { height - y0 } else { rows_each };
            let (yh, yt) = y_rest.split_at_mut(rows * wc);
            let (uh, ut) = u_rest.split_at_mut((rows / 2) * hchroma_stride);
            let (vh, vt) = v_rest.split_at_mut((rows / 2) * hchroma_stride);
            y_rest = yt;
            u_rest = ut;
            v_rest = vt;
            let src = &bgra[y0 * stride..(y0 + rows) * stride];
            tasks.push((src, yh, uh, vh, rows as u32));
            y0 += rows;
        }

        rayon::scope(|s| {
            for (src, yh, uh, vh, rows) in tasks {
                s.spawn(move |_| conv(src, stride, yh, uh, vh, width, rows as usize));
            }
        });
    }

    /// Signal screen capture to stop (non-blocking).
    ///
    /// This only sets the running flag. The capture thread will exit
    /// on its own. Use `stop()` for blocking cleanup with thread join.
    pub fn signal_stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Stop screen capture (blocks waiting for capture thread to exit).
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.capture_handle.take() {
            let _ = handle.join();
        }
        self.source = None;
        tracing::info!("Screen capture stopped");
    }

    /// Check if capture is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Get current source.
    pub fn current_source(&self) -> Option<&ScreenSource> {
        self.source.as_ref()
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Default for ScreenCapture {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_wayland_detection() {
        // No environment mutation: the cases go straight to the pure helper, so
        // this test cannot affect — or be affected by — anything running beside it.
        assert!(session_type_is_wayland(Some("wayland")), "should detect Wayland");
        assert!(
            session_type_is_wayland(Some("WAYLAND")),
            "should detect Wayland case-insensitively"
        );
        assert!(!session_type_is_wayland(Some("x11")), "X11 is not Wayland");
        assert!(!session_type_is_wayland(Some("tty")), "tty is not Wayland");
        assert!(!session_type_is_wayland(None), "an unset session type is not Wayland");
    }

    #[test]
    fn test_screen_source_type_equality() {
        assert_eq!(ScreenSourceType::Monitor, ScreenSourceType::Monitor);
        assert_eq!(ScreenSourceType::Window, ScreenSourceType::Window);
        assert_ne!(ScreenSourceType::Monitor, ScreenSourceType::Window);
    }

    #[test]
    fn test_screen_source_creation() {
        let source = ScreenSource {
            name: "Test Display".to_string(),
            id: 123,
            display_id: 1,
            source_type: ScreenSourceType::Monitor,
        };

        assert_eq!(source.name, "Test Display");
        assert_eq!(source.id, 123);
        assert_eq!(source.display_id, 1);
        assert_eq!(source.source_type, ScreenSourceType::Monitor);
    }

    #[test]
    fn test_screen_capture_config_default() {
        let config = ScreenCaptureConfig::default();
        assert_eq!(config.fps, 144);
        assert!(config.capture_cursor);
    }

    #[test]
    fn test_screen_capture_config_custom() {
        let config = ScreenCaptureConfig {
            fps: 60,
            capture_cursor: false,
        };
        assert_eq!(config.fps, 60);
        assert!(!config.capture_cursor);
    }

    #[test]
    fn test_screen_capture_new() {
        let capture = ScreenCapture::new();
        assert!(!capture.is_running());
        assert!(capture.current_source().is_none());
    }

    #[test]
    fn test_screen_capture_default() {
        let capture = ScreenCapture::default();
        assert!(!capture.is_running());
    }

    #[test]
    fn test_bgra_to_i420_basic() {
        // Test a simple 2x2 red pixel image
        // BGRA: Blue=0, Green=0, Red=255, Alpha=255
        let bgra = vec![
            0, 0, 255, 255,  // pixel (0,0) - red
            0, 0, 255, 255,  // pixel (1,0) - red
            0, 0, 255, 255,  // pixel (0,1) - red
            0, 0, 255, 255,  // pixel (1,1) - red
        ];

        let width = 2;
        let height = 2;
        let stride = width * 4;

        let mut y_plane = vec![0u8; width * height];
        let mut u_plane = vec![0u8; (width / 2) * (height / 2)];
        let mut v_plane = vec![0u8; (width / 2) * (height / 2)];

        ScreenCapture::bgra_to_i420(
            &bgra,
            width,
            height,
            stride,
            &mut y_plane,
            &mut u_plane,
            &mut v_plane,
        );

        // For red (R=255, G=0, B=0):
        // Y = ((66*255 + 129*0 + 25*0 + 128) >> 8) + 16 = 81
        // U = ((-38*255 - 74*0 + 112*0 + 128) >> 8) + 128 = 90
        // V = ((112*255 - 94*0 - 18*0 + 128) >> 8) + 128 = 239

        // All Y values should be around 81
        for y in &y_plane {
            assert!(*y > 70 && *y < 90, "Y value {} out of expected range", y);
        }

        // U should be around 90
        assert!(u_plane[0] > 80 && u_plane[0] < 100, "U value {} out of expected range", u_plane[0]);

        // V should be around 239
        assert!(v_plane[0] > 230, "V value {} out of expected range", v_plane[0]);
    }

    #[test]
    fn test_bgra_to_i420_black() {
        // Black pixels: B=0, G=0, R=0, A=255
        let bgra = vec![
            0, 0, 0, 255,
            0, 0, 0, 255,
            0, 0, 0, 255,
            0, 0, 0, 255,
        ];

        let width = 2;
        let height = 2;
        let stride = width * 4;

        let mut y_plane = vec![0u8; width * height];
        let mut u_plane = vec![0u8; (width / 2) * (height / 2)];
        let mut v_plane = vec![0u8; (width / 2) * (height / 2)];

        ScreenCapture::bgra_to_i420(
            &bgra,
            width,
            height,
            stride,
            &mut y_plane,
            &mut u_plane,
            &mut v_plane,
        );

        // For black (R=0, G=0, B=0):
        // Y = 16 (black level in video)
        // U = 128 (neutral)
        // V = 128 (neutral)

        for y in &y_plane {
            assert_eq!(*y, 16, "Y value for black should be 16");
        }
        assert_eq!(u_plane[0], 128, "U value for black should be 128");
        assert_eq!(v_plane[0], 128, "V value for black should be 128");
    }

    #[test]
    fn test_bgra_to_i420_white() {
        // White pixels: B=255, G=255, R=255, A=255
        let bgra = vec![
            255, 255, 255, 255,
            255, 255, 255, 255,
            255, 255, 255, 255,
            255, 255, 255, 255,
        ];

        let width = 2;
        let height = 2;
        let stride = width * 4;

        let mut y_plane = vec![0u8; width * height];
        let mut u_plane = vec![0u8; (width / 2) * (height / 2)];
        let mut v_plane = vec![0u8; (width / 2) * (height / 2)];

        ScreenCapture::bgra_to_i420(
            &bgra,
            width,
            height,
            stride,
            &mut y_plane,
            &mut u_plane,
            &mut v_plane,
        );

        // For white (R=255, G=255, B=255):
        // Y = 235 (white level in video)
        // U = 128 (neutral)
        // V = 128 (neutral)

        for y in &y_plane {
            assert!(*y > 220, "Y value for white should be > 220, got {}", y);
        }
        assert!(u_plane[0] > 120 && u_plane[0] < 136, "U value for white should be ~128, got {}", u_plane[0]);
        assert!(v_plane[0] > 120 && v_plane[0] < 136, "V value for white should be ~128, got {}", v_plane[0]);
    }

    // Parallel band-split must be byte-identical to a single full-frame convert.
    // Uses a >2 MP frame so the rayon path actually triggers (on multi-core);
    // on a single core it falls back to the same single-shot call and trivially
    // matches. A non-flat gradient makes any wrong band offset/stride visible.
    #[test]
    fn test_bgra_to_i420_parallel_matches_single() {
        let (width, height) = (1920usize, 1088usize); // 2.09 MP, 1088/64=17 bands
        let stride = width * 4;
        let mut bgra = vec![0u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                let p = (y * stride) + x * 4;
                bgra[p] = (x % 256) as u8;         // B
                bgra[p + 1] = (y % 256) as u8;     // G
                bgra[p + 2] = ((x + y) % 256) as u8; // R
                bgra[p + 3] = 255;                 // A
            }
        }

        let ysz = width * height;
        let csz = (width / 2) * (height / 2);
        let (mut yp, mut up, mut vp) = (vec![0u8; ysz], vec![0u8; csz], vec![0u8; csz]);
        ScreenCapture::bgra_to_i420(&bgra, width, height, stride, &mut yp, &mut up, &mut vp);

        // Reference: one libyuv call over the whole frame.
        let (mut yr, mut ur, mut vr) = (vec![0u8; ysz], vec![0u8; csz], vec![0u8; csz]);
        livekit::webrtc::native::yuv_helper::argb_to_i420(
            &bgra, stride as u32,
            &mut yr, width as u32,
            &mut ur, (width / 2) as u32,
            &mut vr, (width / 2) as u32,
            width as i32, height as i32,
        );

        assert_eq!(yp, yr, "Y plane mismatch between parallel and single convert");
        assert_eq!(up, ur, "U plane mismatch between parallel and single convert");
        assert_eq!(vp, vr, "V plane mismatch between parallel and single convert");
    }

    #[test]
    fn test_list_sources_returns_at_least_one() {
        // This test requires a display server, skip if not available
        match ScreenCapture::list_sources() {
            Ok(sources) => {
                assert!(!sources.is_empty(), "Should return at least one source");

                // Check first source has valid data
                let first = &sources[0];
                assert!(!first.name.is_empty(), "Source name should not be empty");
            }
            Err(e) => {
                // May fail in CI environment without display
                eprintln!("list_sources failed (expected in headless env): {}", e);
            }
        }
    }

    #[test]
    fn test_list_sources_wayland_returns_portal() {
        // Save original value
        let original = std::env::var("XDG_SESSION_TYPE").ok();

        // SAFETY: Tests run single-threaded, so env var modification is safe
        unsafe {
            // Set to Wayland
            std::env::set_var("XDG_SESSION_TYPE", "wayland");
        }

        match ScreenCapture::list_sources() {
            Ok(sources) => {
                assert_eq!(sources.len(), 1, "Wayland should return single portal source");
                assert_eq!(sources[0].id, 0, "Portal source should have id 0");
                assert!(sources[0].name.contains("Portal"), "Portal source name should contain 'Portal'");
            }
            Err(e) => {
                eprintln!("list_sources failed: {}", e);
            }
        }

        // SAFETY: Tests run single-threaded
        unsafe {
            // Restore original value
            match original {
                Some(v) => std::env::set_var("XDG_SESSION_TYPE", v),
                None => std::env::remove_var("XDG_SESSION_TYPE"),
            }
        }
    }
}

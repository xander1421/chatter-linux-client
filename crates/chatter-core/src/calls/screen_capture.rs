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
use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::VideoResolution;
use parking_lot::Mutex;

use crate::error::CallError;

/// Frame data: (RGBA buffer, width, height, stride)
type FrameData = (Vec<u8>, i32, i32, u32);

/// Check if we're running on Wayland (Linux only, always false on other platforms).
fn is_wayland() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::env::var("XDG_SESSION_TYPE")
            .map(|s| s.to_lowercase() == "wayland")
            .unwrap_or(false)
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

impl Default for ScreenCaptureConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            capture_cursor: true,
        }
    }
}

/// Screen capture manager using LiveKit's desktop_capturer.
pub struct ScreenCapture {
    /// Flag to control capture.
    running: Arc<AtomicBool>,
    /// Current source.
    source: Option<ScreenSource>,
    /// LiveKit video source for publishing.
    video_source: Option<NativeVideoSource>,
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
            video_source: None,
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

    /// Start screen capture from the specified source.
    ///
    /// Returns the NativeVideoSource that can be used to create a LiveKit track.
    pub fn start(
        &mut self,
        source: ScreenSource,
        config: Option<ScreenCaptureConfig>,
    ) -> Result<NativeVideoSource, CallError> {
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

        // Create capturer options based on source type
        let source_type = match source.source_type {
            ScreenSourceType::Monitor => DesktopCaptureSourceType::Screen,
            ScreenSourceType::Window => DesktopCaptureSourceType::Window,
        };

        let mut options = DesktopCapturerOptions::new(source_type);
        options.set_include_cursor(config.capture_cursor);

        // We'll create the video source with initial dimensions, then update on first frame
        // Use reasonable defaults for screen sharing
        let video_source = NativeVideoSource::new(
            VideoResolution {
                width: 1920,
                height: 1080,
            },
            true, // is_screencast
        );

        let source_clone = video_source.clone();
        let running = self.running.clone();
        let fps = config.fps;
        let source_id = source.id;

        self.running.store(true, Ordering::SeqCst);

        // Spawn capture thread
        let handle = std::thread::spawn(move || {
            Self::capture_loop(source_id, source_type, options, source_clone, running, fps);
        });

        self.capture_handle = Some(handle);
        self.video_source = Some(video_source.clone());

        tracing::info!("Screen capture started successfully");
        Ok(video_source)
    }

    /// Screen capture loop using LiveKit's desktop_capturer.
    fn capture_loop(
        source_id: u64,
        _source_type: DesktopCaptureSourceType,
        options: DesktopCapturerOptions,
        video_source: NativeVideoSource,
        running: Arc<AtomicBool>,
        fps: u32,
    ) {
        use std::time::{Duration, Instant};
        use livekit::webrtc::desktop_capturer::CaptureError;

        let frame_duration = Duration::from_secs_f64(1.0 / fps as f64);
        let use_portal = is_wayland() || source_id == 0;

        // Create the desktop capturer
        let mut capturer = match DesktopCapturer::new(options) {
            Some(c) => c,
            None => {
                tracing::error!("Failed to create desktop capturer");
                return;
            }
        };

        // On Wayland/portal mode, don't specify a source - let portal picker handle it
        let capture_source = if use_portal {
            tracing::info!("Starting capture via XDG Desktop Portal (system picker will appear)");
            tracing::info!("Note: 'Temporary' errors are expected until you select a screen in the portal dialog");
            None
        } else {
            // On X11, find the source by ID
            let source_list = capturer.get_source_list();
            let source = source_list.into_iter().find(|s| s.id() == source_id);

            if source.is_none() {
                tracing::error!("Source ID {} not found in capture source list", source_id);
                return;
            }

            tracing::info!(
                "Starting capture for source: {} (id={})",
                source.as_ref().unwrap().title(),
                source_id
            );
            source
        };

        // Shared state for frame data and error tracking
        let frame_data: Arc<Mutex<Option<FrameData>>> = Arc::new(Mutex::new(None));
        let frame_data_clone = frame_data.clone();
        let temporary_error_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let temporary_error_count_clone = temporary_error_count.clone();
        let first_frame_received = Arc::new(AtomicBool::new(false));
        let first_frame_received_clone = first_frame_received.clone();

        // Start capture with callback
        capturer.start_capture(capture_source, move |result| {
            match result {
                Ok(frame) => {
                    let width = frame.width();
                    let height = frame.height();
                    let stride = frame.stride();
                    let data = frame.data().to_vec();

                    // Log first frame received (portal selection completed)
                    if !first_frame_received_clone.swap(true, Ordering::SeqCst) {
                        tracing::info!(
                            "First frame received! Screen capture active ({}x{})",
                            width, height
                        );
                    }

                    // Store frame data for processing
                    *frame_data_clone.lock() = Some((data, width, height, stride));
                }
                Err(e) => {
                    match e {
                        CaptureError::Temporary => {
                            // Only log every 100th temporary error to avoid spam
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
                            tracing::error!("Permanent capture error - screen sharing failed");
                        }
                    }
                }
            }
        });

        let mut frame_count = 0u64;
        let capture_start = Instant::now();

        while running.load(Ordering::SeqCst) {
            let start = Instant::now();

            // Request a frame
            capturer.capture_frame();

            // Small delay to allow callback to process
            std::thread::sleep(Duration::from_millis(5));

            // Check if we have a frame
            if let Some((data, width, height, stride)) = frame_data.lock().take() {
                if width > 0 && height > 0 {
                    // Convert BGRA to I420 and send to LiveKit
                    let mut i420_buffer = I420Buffer::new(width as u32, height as u32);
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

                    let frame = VideoFrame {
                        rotation: VideoRotation::VideoRotation0,
                        timestamp_us: (frame_count * 1_000_000 / fps as u64) as i64,
                        buffer: i420_buffer,
                    };

                    video_source.capture_frame(&frame);
                    frame_count += 1;
                }
            }

            // Warn if portal selection is taking too long (30+ seconds with no frames)
            if use_portal && !first_frame_received.load(Ordering::SeqCst) {
                let elapsed = capture_start.elapsed();
                if elapsed > Duration::from_secs(30) && elapsed < Duration::from_secs(31) {
                    tracing::warn!(
                        "Screen sharing: No frames received after 30s. Portal dialog may have been cancelled."
                    );
                }
            }

            // Maintain frame rate
            let elapsed = start.elapsed();
            if elapsed < frame_duration {
                std::thread::sleep(frame_duration - elapsed);
            }
        }

        let temp_errors = temporary_error_count.load(Ordering::Relaxed);
        tracing::info!(
            "Screen capture loop ended after {} frames ({} temporary errors during startup)",
            frame_count,
            temp_errors
        );
    }

    /// Convert BGRA to I420 (YUV 4:2:0).
    fn bgra_to_i420(
        bgra: &[u8],
        width: usize,
        height: usize,
        stride: usize,
        y_plane: &mut [u8],
        u_plane: &mut [u8],
        v_plane: &mut [u8],
    ) {
        for row in 0..height {
            for col in 0..width {
                let bgra_idx = row * stride + col * 4;
                let y_idx = row * width + col;

                if bgra_idx + 2 < bgra.len() && y_idx < y_plane.len() {
                    let b = bgra[bgra_idx] as i32;
                    let g = bgra[bgra_idx + 1] as i32;
                    let r = bgra[bgra_idx + 2] as i32;

                    // RGB to Y conversion
                    let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                    y_plane[y_idx] = y.clamp(0, 255) as u8;

                    // Subsample U and V (every 2x2 block)
                    if row % 2 == 0 && col % 2 == 0 {
                        let uv_row = row / 2;
                        let uv_col = col / 2;
                        let uv_idx = uv_row * (width / 2) + uv_col;

                        if uv_idx < u_plane.len() {
                            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                            u_plane[uv_idx] = u.clamp(0, 255) as u8;
                            v_plane[uv_idx] = v.clamp(0, 255) as u8;
                        }
                    }
                }
            }
        }
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
        self.video_source = None;
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
        // Save original value
        let original = std::env::var("XDG_SESSION_TYPE").ok();

        // SAFETY: Tests run single-threaded, so env var modification is safe
        unsafe {
            // Test Wayland detection
            std::env::set_var("XDG_SESSION_TYPE", "wayland");
            assert!(is_wayland(), "Should detect Wayland session");

            std::env::set_var("XDG_SESSION_TYPE", "WAYLAND");
            assert!(is_wayland(), "Should detect Wayland (case insensitive)");

            // Test X11 detection
            std::env::set_var("XDG_SESSION_TYPE", "x11");
            assert!(!is_wayland(), "Should not detect X11 as Wayland");

            std::env::set_var("XDG_SESSION_TYPE", "tty");
            assert!(!is_wayland(), "Should not detect tty as Wayland");

            // Restore original value
            match original {
                Some(v) => std::env::set_var("XDG_SESSION_TYPE", v),
                None => std::env::remove_var("XDG_SESSION_TYPE"),
            }
        }
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
        assert_eq!(config.fps, 30);
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

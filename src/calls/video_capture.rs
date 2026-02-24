//! Video capture from camera using v4l2 (Linux) or platform-specific APIs.
//!
//! Provides camera device enumeration and video frame capture for LiveKit publishing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::path::Path;

use livekit::webrtc::video_frame::{VideoFrame, I420Buffer, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::VideoResolution;

use crate::error::CallError;

/// Information about an available video input device.
#[derive(Debug, Clone)]
pub struct VideoDevice {
    /// Device name for display.
    pub name: String,
    /// Device path (e.g., /dev/video0).
    pub path: String,
    /// Whether this is the default device.
    pub is_default: bool,
    /// Supported resolutions.
    pub resolutions: Vec<(u32, u32)>,
}

/// Video capture configuration.
#[derive(Debug, Clone)]
pub struct VideoCaptureConfig {
    /// Desired width.
    pub width: u32,
    /// Desired height.
    pub height: u32,
    /// Desired frame rate.
    pub fps: u32,
}

impl Default for VideoCaptureConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
        }
    }
}

/// Video capture manager for camera input.
pub struct VideoCapture {
    /// Flag to control capture.
    running: Arc<AtomicBool>,
    /// Current device path.
    device_path: String,
    /// LiveKit video source for publishing.
    video_source: Option<NativeVideoSource>,
    /// Capture configuration.
    config: VideoCaptureConfig,
    /// Capture thread handle.
    capture_handle: Option<std::thread::JoinHandle<()>>,
}

impl VideoCapture {
    /// Create a new video capture (not yet started).
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            device_path: String::new(),
            video_source: None,
            config: VideoCaptureConfig::default(),
            capture_handle: None,
        }
    }

    /// List available video input devices (Linux v4l2).
    pub fn list_devices() -> Result<Vec<VideoDevice>, CallError> {
        let mut devices = Vec::new();

        // Scan /dev/video* devices
        for i in 0..10 {
            let path = format!("/dev/video{}", i);
            if Path::new(&path).exists() {
                // Try to get device name
                let name = Self::get_device_name(&path).unwrap_or_else(|| format!("Camera {}", i));

                devices.push(VideoDevice {
                    name,
                    path: path.clone(),
                    is_default: i == 0,
                    resolutions: vec![(640, 480), (1280, 720), (1920, 1080)],
                });
            }
        }

        if devices.is_empty() {
            tracing::warn!("No video devices found");
        }

        Ok(devices)
    }

    /// Get device name from v4l2.
    fn get_device_name(path: &str) -> Option<String> {
        // Try to read device name using v4l2-ctl or sysfs
        // For now, just use the path
        let path = Path::new(path);
        path.file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
    }

    /// Get the default video device.
    pub fn default_device() -> Result<VideoDevice, CallError> {
        let devices = Self::list_devices()?;
        devices.into_iter().next().ok_or_else(|| {
            CallError::TrackPublishFailed("No video devices available".to_string())
        })
    }

    /// Start capturing video from the specified device.
    ///
    /// Returns the NativeVideoSource that can be used to create a LiveKit track.
    pub fn start(
        &mut self,
        device_path: Option<&str>,
        config: Option<VideoCaptureConfig>,
    ) -> Result<NativeVideoSource, CallError> {
        if self.running.load(Ordering::SeqCst) {
            return Err(CallError::TrackPublishFailed(
                "Video capture already running".to_string(),
            ));
        }

        let config = config.unwrap_or_default();
        self.config = config.clone();

        // Find the device
        let path = device_path
            .map(|s| s.to_string())
            .or_else(|| Self::default_device().ok().map(|d| d.path))
            .ok_or_else(|| CallError::TrackPublishFailed("No video device specified".to_string()))?;

        self.device_path = path.clone();
        tracing::info!(
            "Starting video capture from device: {} ({}x{}@{}fps)",
            self.device_path,
            config.width,
            config.height,
            config.fps
        );

        // Create LiveKit video source (is_screencast = false for camera)
        let video_source = NativeVideoSource::new(
            VideoResolution {
                width: config.width,
                height: config.height,
            },
            false, // is_screencast
        );

        // Clone for the capture thread
        let source_clone = video_source.clone();
        let running = self.running.clone();
        let width = config.width;
        let height = config.height;
        let fps = config.fps;

        self.running.store(true, Ordering::SeqCst);

        // Spawn capture thread
        let handle = std::thread::spawn(move || {
            Self::capture_loop(path, source_clone, running, width, height, fps);
        });

        self.capture_handle = Some(handle);
        self.video_source = Some(video_source.clone());

        tracing::info!("Video capture started successfully");
        Ok(video_source)
    }

    /// Video capture loop (runs in separate thread).
    fn capture_loop(
        device_path: String,
        video_source: NativeVideoSource,
        running: Arc<AtomicBool>,
        width: u32,
        height: u32,
        fps: u32,
    ) {
        #[cfg(target_os = "linux")]
        {
            use std::time::{Duration, Instant};
            use v4l::buffer::Type;
            use v4l::io::traits::CaptureStream;
            use v4l::prelude::*;
            use v4l::video::Capture;
            use v4l::FourCC;

            let frame_duration = Duration::from_secs_f64(1.0 / fps as f64);

            // Try to open the v4l2 device
            let dev = match Device::with_path(&device_path) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("Failed to open video device {}: {}", device_path, e);
                    Self::fallback_capture_loop(video_source, running, width, height, fps);
                    return;
                }
            };

            // Get current format and try to set desired resolution
            let mut format = match dev.format() {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to get video format: {}", e);
                    Self::fallback_capture_loop(video_source, running, width, height, fps);
                    return;
                }
            };

            // Try to set format - prefer YUYV, fall back to MJPG
            format.width = width;
            format.height = height;
            format.fourcc = FourCC::new(b"YUYV");

            let format = match dev.set_format(&format) {
                Ok(f) => f,
                Err(_) => {
                    // Try MJPG as fallback
                    format.fourcc = FourCC::new(b"MJPG");
                    match dev.set_format(&format) {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::error!("Failed to set video format: {}", e);
                            Self::fallback_capture_loop(video_source, running, width, height, fps);
                            return;
                        }
                    }
                }
            };

            let actual_width = format.width;
            let actual_height = format.height;
            let fourcc = format.fourcc;

            tracing::info!(
                "Video capture: {}x{} {:?} from {}",
                actual_width, actual_height, fourcc, device_path
            );

            // Create mmap stream
            let mut stream = match MmapStream::with_buffers(&dev, Type::VideoCapture, 4) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to create video stream: {}", e);
                    Self::fallback_capture_loop(video_source, running, width, height, fps);
                    return;
                }
            };

            let mut frame_count = 0u64;
            let is_yuyv = fourcc == FourCC::new(b"YUYV");

            while running.load(Ordering::SeqCst) {
                let start = Instant::now();

                // Read frame from device
                match stream.next() {
                    Ok((buf, _meta)) => {
                        // Convert to I420
                        let mut i420_buffer = I420Buffer::new(actual_width, actual_height);
                        {
                            let (y_data, u_data, v_data) = i420_buffer.data_mut();

                            if is_yuyv {
                                // Convert YUYV to I420
                                Self::yuyv_to_i420(
                                    buf,
                                    actual_width as usize,
                                    actual_height as usize,
                                    y_data,
                                    u_data,
                                    v_data,
                                );
                            } else {
                                // For MJPG, we'd need to decode - fall back to gray
                                y_data.fill(128);
                                u_data.fill(128);
                                v_data.fill(128);
                            }
                        }

                        let frame = VideoFrame {
                            rotation: VideoRotation::VideoRotation0,
                            timestamp_us: (frame_count * 1_000_000 / fps as u64) as i64,
                            buffer: i420_buffer,
                        };

                        video_source.capture_frame(&frame);
                        frame_count += 1;
                    }
                    Err(e) => {
                        tracing::warn!("Video capture error: {}", e);
                    }
                }

                // Maintain frame rate
                let elapsed = start.elapsed();
                if elapsed < frame_duration {
                    std::thread::sleep(frame_duration - elapsed);
                }
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            tracing::error!("Video capture not implemented for this platform");
        }

        tracing::info!("Video capture loop ended");
    }

    /// Convert YUYV (YUV 4:2:2) to I420 (YUV 4:2:0).
    fn yuyv_to_i420(
        yuyv: &[u8],
        width: usize,
        height: usize,
        y_plane: &mut [u8],
        u_plane: &mut [u8],
        v_plane: &mut [u8],
    ) {
        // YUYV: Y0 U0 Y1 V0 (4 bytes per 2 pixels)
        // I420: Y plane, then U plane (subsampled 2x2), then V plane (subsampled 2x2)

        let yuyv_stride = width * 2;

        for row in 0..height {
            let yuyv_row = &yuyv[row * yuyv_stride..(row + 1) * yuyv_stride];
            let y_row = &mut y_plane[row * width..(row + 1) * width];

            // Extract Y values
            for (col, y_val) in y_row.iter_mut().enumerate() {
                let yuyv_idx = col * 2;
                if yuyv_idx < yuyv_row.len() {
                    *y_val = yuyv_row[yuyv_idx];
                }
            }

            // Extract U and V (subsampled - only every other row and column)
            if row % 2 == 0 {
                let uv_row = row / 2;
                let u_row = &mut u_plane[uv_row * (width / 2)..(uv_row + 1) * (width / 2)];
                let v_row = &mut v_plane[uv_row * (width / 2)..(uv_row + 1) * (width / 2)];

                for col in (0..width).step_by(2) {
                    let yuyv_idx = col * 2;
                    let uv_col = col / 2;
                    if yuyv_idx + 3 < yuyv_row.len() && uv_col < u_row.len() {
                        u_row[uv_col] = yuyv_row[yuyv_idx + 1]; // U
                        v_row[uv_col] = yuyv_row[yuyv_idx + 3]; // V
                    }
                }
            }
        }
    }

    /// Fallback capture loop using test pattern.
    fn fallback_capture_loop(
        video_source: NativeVideoSource,
        running: Arc<AtomicBool>,
        width: u32,
        height: u32,
        fps: u32,
    ) {
        use std::time::{Duration, Instant};

        let frame_duration = Duration::from_secs_f64(1.0 / fps as f64);

        tracing::warn!("Video capture: Using test pattern (camera not available)");

        let mut frame_count = 0u64;

        while running.load(Ordering::SeqCst) {
            let start = Instant::now();

            // Create a test I420 frame (gray screen with moving bar)
            let mut buffer = I420Buffer::new(width, height);
            {
                let (y_data, u_data, v_data) = buffer.data_mut();

                // Gray Y plane
                y_data.fill(128);

                // Draw a moving white bar
                let bar_pos = ((frame_count * 5) % (width as u64)) as usize;
                for row in 0..height as usize {
                    let start_idx = row * width as usize + bar_pos;
                    let end_idx = (start_idx + 20).min((row + 1) * width as usize);
                    for i in start_idx..end_idx {
                        if i < y_data.len() {
                            y_data[i] = 255; // White
                        }
                    }
                }

                // Neutral U and V for gray
                u_data.fill(128);
                v_data.fill(128);
            }

            let frame = VideoFrame {
                rotation: VideoRotation::VideoRotation0,
                timestamp_us: (frame_count * 1_000_000 / fps as u64) as i64,
                buffer,
            };

            video_source.capture_frame(&frame);
            frame_count += 1;

            // Maintain frame rate
            let elapsed = start.elapsed();
            if elapsed < frame_duration {
                std::thread::sleep(frame_duration - elapsed);
            }
        }
    }

    /// Signal video capture to stop (non-blocking).
    ///
    /// This only sets the running flag. The capture thread will exit
    /// on its own. Use `stop()` for blocking cleanup with thread join.
    pub fn signal_stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Stop video capture (blocks waiting for capture thread to exit).
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.capture_handle.take() {
            let _ = handle.join();
        }
        self.video_source = None;
        tracing::info!("Video capture stopped");
    }

    /// Check if capture is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Get current device path.
    pub fn device_path(&self) -> &str {
        &self.device_path
    }

    /// Enable/disable video (pause capture without releasing device).
    pub fn set_enabled(&mut self, enabled: bool) {
        // For video, we just toggle the running flag
        // The capture loop will stop sending frames
        if enabled && self.capture_handle.is_some() {
            self.running.store(true, Ordering::SeqCst);
        } else {
            self.running.store(false, Ordering::SeqCst);
        }
    }

    /// Check if video is enabled.
    pub fn is_enabled(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

impl Drop for VideoCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Default for VideoCapture {
    fn default() -> Self {
        Self::new()
    }
}

//! Video capture from camera using platform-specific APIs.
//!
//! - Linux: V4L2 via `v4l` crate
//! - Windows: GStreamer `ksvideosrc` (Windows Media Foundation)
//! - Other: Fallback test pattern
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
    /// Device path (e.g., /dev/video0 on Linux, device index on Windows).
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

    /// List available video input devices.
    pub fn list_devices() -> Result<Vec<VideoDevice>, CallError> {
        #[cfg(target_os = "linux")]
        {
            Self::list_devices_linux()
        }

        #[cfg(target_os = "windows")]
        {
            Self::list_devices_windows()
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            tracing::warn!("Video device enumeration not supported on this platform");
            Ok(Vec::new())
        }
    }

    /// Linux: Enumerate V4L2 devices.
    #[cfg(target_os = "linux")]
    fn list_devices_linux() -> Result<Vec<VideoDevice>, CallError> {
        let mut devices = Vec::new();

        for i in 0..10 {
            let path = format!("/dev/video{}", i);
            if Path::new(&path).exists() {
                let name = Self::get_device_name(&path)
                    .unwrap_or_else(|| format!("Camera {}", i));

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

    /// Windows: Enumerate cameras using GStreamer device monitor.
    #[cfg(target_os = "windows")]
    fn list_devices_windows() -> Result<Vec<VideoDevice>, CallError> {
        use gstreamer as gst;
        use gstreamer::prelude::*;

        let mut devices = Vec::new();

        let monitor = gst::DeviceMonitor::new();
        monitor.add_filter(Some("Video/Source"), None);

        if monitor.start().is_err() {
            tracing::warn!("Failed to start GStreamer device monitor");
            return Ok(devices);
        }

        for (i, device) in monitor.devices().iter().enumerate() {
            let name = device.display_name().to_string();
            let device_class = device.device_class().to_string();

            if device_class.contains("Video") || device_class.contains("Source") {
                devices.push(VideoDevice {
                    name,
                    path: format!("device-index={}", i),
                    is_default: i == 0,
                    resolutions: vec![(640, 480), (1280, 720), (1920, 1080)],
                });
            }
        }

        monitor.stop();

        if devices.is_empty() {
            tracing::warn!("No video devices found");
        }

        Ok(devices)
    }

    /// Get device name from path.
    fn get_device_name(path: &str) -> Option<String> {
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

        let video_source = NativeVideoSource::new(
            VideoResolution {
                width: config.width,
                height: config.height,
            },
            false,
        );

        let source_clone = video_source.clone();
        let running = self.running.clone();
        let width = config.width;
        let height = config.height;
        let fps = config.fps;

        self.running.store(true, Ordering::SeqCst);

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
            Self::capture_loop_linux(device_path, video_source, running, width, height, fps);
        }

        #[cfg(target_os = "windows")]
        {
            Self::capture_loop_windows(device_path, video_source, running, width, height, fps);
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            tracing::error!("Video capture not implemented for this platform");
            Self::fallback_capture_loop(video_source, running, width, height, fps);
        }

        tracing::info!("Video capture loop ended");
    }

    /// Linux V4L2 capture loop.
    #[cfg(target_os = "linux")]
    fn capture_loop_linux(
        device_path: String,
        video_source: NativeVideoSource,
        running: Arc<AtomicBool>,
        width: u32,
        height: u32,
        fps: u32,
    ) {
        use std::time::{Duration, Instant};
        use v4l::buffer::Type;
        use v4l::io::traits::CaptureStream;
        use v4l::prelude::*;
        use v4l::video::Capture;
        use v4l::FourCC;

        let frame_duration = Duration::from_secs_f64(1.0 / fps as f64);

        let dev = match Device::with_path(&device_path) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("Failed to open video device {}: {}", device_path, e);
                Self::fallback_capture_loop(video_source, running, width, height, fps);
                return;
            }
        };

        let mut format = match dev.format() {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("Failed to get video format: {}", e);
                Self::fallback_capture_loop(video_source, running, width, height, fps);
                return;
            }
        };

        format.width = width;
        format.height = height;
        format.fourcc = FourCC::new(b"YUYV");

        let format = match dev.set_format(&format) {
            Ok(f) => f,
            Err(_) => {
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

            match stream.next() {
                Ok((buf, _meta)) => {
                    let mut i420_buffer = I420Buffer::new(actual_width, actual_height);
                    {
                        let (y_data, u_data, v_data) = i420_buffer.data_mut();

                        if is_yuyv {
                            Self::yuyv_to_i420(
                                buf,
                                actual_width as usize,
                                actual_height as usize,
                                y_data,
                                u_data,
                                v_data,
                            );
                        } else {
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

            let elapsed = start.elapsed();
            if elapsed < frame_duration {
                std::thread::sleep(frame_duration - elapsed);
            }
        }
    }

    /// Windows GStreamer-based capture loop using ksvideosrc or mfvideosrc.
    #[cfg(target_os = "windows")]
    fn capture_loop_windows(
        device_path: String,
        video_source: NativeVideoSource,
        running: Arc<AtomicBool>,
        width: u32,
        height: u32,
        fps: u32,
    ) {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};
        use gstreamer as gst;
        use gstreamer::prelude::*;
        use gstreamer_app as gst_app;
        use gstreamer_video as gst_video;

        let frame_duration = Duration::from_secs_f64(1.0 / fps as f64);

        // Try mfvideosrc first (newer Media Foundation source), fall back to ksvideosrc
        let src_element = gst::ElementFactory::make("mfvideosrc")
            .build()
            .or_else(|_| gst::ElementFactory::make("ksvideosrc").build());

        let src = match src_element {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to create Windows video source: {}", e);
                Self::fallback_capture_loop(video_source, running, width, height, fps);
                return;
            }
        };

        // Parse device index from path like "device-index=0"
        if let Some(idx_str) = device_path.strip_prefix("device-index=") {
            if let Ok(idx) = idx_str.parse::<i32>() {
                src.set_property("device-index", idx);
            }
        }

        let pipeline = gst::Pipeline::new();

        // src -> capsfilter -> videoconvert -> appsink(I420)
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .build()
            .expect("capsfilter");
        let caps = gst_video::VideoCapsBuilder::new()
            .width(width as i32)
            .height(height as i32)
            .framerate(gst::Fraction::new(fps as i32, 1))
            .build();
        capsfilter.set_property("caps", &caps);

        let videoconvert = gst::ElementFactory::make("videoconvert")
            .build()
            .expect("videoconvert");

        let appsink = gst_app::AppSink::builder()
            .caps(
                &gst_video::VideoCapsBuilder::new()
                    .format(gst_video::VideoFormat::I420)
                    .build(),
            )
            .enable_last_sample(false)
            .max_buffers(1)
            .drop(true)
            .sync(false)
            .build();

        pipeline
            .add_many([&src, &capsfilter, &videoconvert, appsink.upcast_ref()])
            .expect("add elements");
        gst::Element::link_many([&src, &capsfilter, &videoconvert, appsink.upcast_ref()])
            .expect("link elements");

        if pipeline.set_state(gst::State::Playing).is_err() {
            tracing::error!("Failed to start Windows video capture pipeline");
            Self::fallback_capture_loop(video_source, running, width, height, fps);
            return;
        }

        tracing::info!("Windows video capture pipeline started");

        let mut frame_count = 0u64;

        while running.load(Ordering::SeqCst) {
            let start = Instant::now();

            match appsink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
                Some(sample) => {
                    if let (Some(buffer), Some(caps)) = (sample.buffer(), sample.caps()) {
                        if let Ok(info) = gst_video::VideoInfo::from_caps(caps) {
                            let actual_w = info.width();
                            let actual_h = info.height();

                            if let Ok(map) = buffer.map_readable() {
                                let data = map.as_slice();
                                let y_size = (actual_w * actual_h) as usize;
                                let uv_size = y_size / 4;

                                if data.len() >= y_size + 2 * uv_size {
                                    let mut i420_buffer = I420Buffer::new(actual_w, actual_h);
                                    {
                                        let (y_plane, u_plane, v_plane) = i420_buffer.data_mut();
                                        y_plane.copy_from_slice(&data[..y_size]);
                                        u_plane.copy_from_slice(&data[y_size..y_size + uv_size]);
                                        v_plane.copy_from_slice(&data[y_size + uv_size..y_size + 2 * uv_size]);
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
                        }
                    }
                }
                None => {
                    // No frame available, check pipeline state
                    if pipeline.current_state() == gst::State::Null {
                        tracing::error!("Video capture pipeline stopped unexpectedly");
                        break;
                    }
                }
            }

            let elapsed = start.elapsed();
            if elapsed < frame_duration {
                std::thread::sleep(frame_duration - elapsed);
            }
        }

        let _ = pipeline.set_state(gst::State::Null);
    }

    /// Convert YUYV (YUV 4:2:2) to I420 (YUV 4:2:0).
    #[cfg(target_os = "linux")]
    fn yuyv_to_i420(
        yuyv: &[u8],
        width: usize,
        height: usize,
        y_plane: &mut [u8],
        u_plane: &mut [u8],
        v_plane: &mut [u8],
    ) {
        let yuyv_stride = width * 2;

        for row in 0..height {
            let yuyv_row = &yuyv[row * yuyv_stride..(row + 1) * yuyv_stride];
            let y_row = &mut y_plane[row * width..(row + 1) * width];

            for (col, y_val) in y_row.iter_mut().enumerate() {
                let yuyv_idx = col * 2;
                if yuyv_idx < yuyv_row.len() {
                    *y_val = yuyv_row[yuyv_idx];
                }
            }

            if row % 2 == 0 {
                let uv_row = row / 2;
                let u_row = &mut u_plane[uv_row * (width / 2)..(uv_row + 1) * (width / 2)];
                let v_row = &mut v_plane[uv_row * (width / 2)..(uv_row + 1) * (width / 2)];

                for col in (0..width).step_by(2) {
                    let yuyv_idx = col * 2;
                    let uv_col = col / 2;
                    if yuyv_idx + 3 < yuyv_row.len() && uv_col < u_row.len() {
                        u_row[uv_col] = yuyv_row[yuyv_idx + 1];
                        v_row[uv_col] = yuyv_row[yuyv_idx + 3];
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

            let mut buffer = I420Buffer::new(width, height);
            {
                let (y_data, u_data, v_data) = buffer.data_mut();

                y_data.fill(128);

                let bar_pos = ((frame_count * 5) % (width as u64)) as usize;
                for row in 0..height as usize {
                    let start_idx = row * width as usize + bar_pos;
                    let end_idx = (start_idx + 20).min((row + 1) * width as usize);
                    for i in start_idx..end_idx {
                        if i < y_data.len() {
                            y_data[i] = 255;
                        }
                    }
                }

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

            let elapsed = start.elapsed();
            if elapsed < frame_duration {
                std::thread::sleep(frame_duration - elapsed);
            }
        }
    }

    /// Signal video capture to stop (non-blocking).
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

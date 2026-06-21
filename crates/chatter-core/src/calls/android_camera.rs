//! Android camera bridge.
//!
//! Slint/Rust can't drive the camera on Android, and the UI crate has no
//! LiveKit dependency in its `mobile` build — so the platform layer (chatter-ui,
//! via CameraX over JNI) captures frames and hands us tightly-packed I420, which
//! we push into the LiveKit `NativeVideoSource`. The webrtc types stay on this
//! side of the crate boundary.
//!
//! Flow: video-capture loop → `run_capture` registers the sink + asks the
//! platform to `start()` CameraX → each frame arrives via `push_frame_i420` →
//! `NativeVideoSource::capture_frame`. CameraX uses the modern Camera2 stack
//! (not the deprecated `android.hardware.Camera`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;

/// Implemented by the platform layer (chatter-ui) to start/stop CameraX.
pub trait PlatformCamera: Send + Sync {
    /// `front` selects the front (selfie) vs back lens.
    fn start(&self, width: u32, height: u32, fps: u32, front: bool);
    fn stop(&self);
}

static PLATFORM_CAMERA: OnceLock<Box<dyn PlatformCamera>> = OnceLock::new();
static ACTIVE_SINK: Mutex<Option<NativeVideoSource>> = Mutex::new(None);

/// Register the platform camera driver. Called once by chatter-ui at startup.
pub fn register_platform_camera(cam: Box<dyn PlatformCamera>) {
    if PLATFORM_CAMERA.set(cam).is_err() {
        tracing::warn!("Android camera: platform camera already registered");
    }
}

/// Drive camera capture for the lifetime of `running`. Returns `false` if no
/// platform camera is registered (caller should fall back to a test pattern).
pub fn run_capture(
    sink: NativeVideoSource,
    running: Arc<AtomicBool>,
    width: u32,
    height: u32,
    fps: u32,
    device_id: &str,
) -> bool {
    let Some(cam) = PLATFORM_CAMERA.get() else {
        return false;
    };
    // device_id comes from list_devices() ("front"/"back"); default to front.
    let front = device_id != "back";
    *ACTIVE_SINK.lock().unwrap() = Some(sink);
    tracing::info!("Android camera: starting CameraX {width}x{height}@{fps} front={front}");
    cam.start(width, height, fps, front);

    while running.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }

    cam.stop();
    *ACTIVE_SINK.lock().unwrap() = None;
    tracing::info!("Android camera: capture stopped");
    true
}

/// Push one tightly-packed I420 frame from the platform camera callback.
/// `rotation_degrees` is the CameraX sensor rotation (0/90/180/270); we forward
/// it as frame metadata rather than rotating pixels.
pub fn push_frame_i420(data: &[u8], width: u32, height: u32, rotation_degrees: i32, ts_us: i64) {
    let guard = ACTIVE_SINK.lock().unwrap();
    let Some(sink) = guard.as_ref() else {
        return; // capture not active
    };

    let y_size = (width as usize) * (height as usize);
    let uv_size = ((width as usize) / 2) * ((height as usize) / 2);
    if data.len() < y_size + 2 * uv_size {
        tracing::warn!(
            "Android camera: short frame ({} < {})",
            data.len(),
            y_size + 2 * uv_size
        );
        return;
    }

    let mut buffer = I420Buffer::new(width, height);
    {
        let (y, u, v) = buffer.data_mut();
        // Tightly-packed source → planes are width-strided (matches the
        // Windows capture path); copy plane by plane.
        y.copy_from_slice(&data[..y_size]);
        u.copy_from_slice(&data[y_size..y_size + uv_size]);
        v.copy_from_slice(&data[y_size + uv_size..y_size + 2 * uv_size]);
    }

    let rotation = match rotation_degrees {
        90 => VideoRotation::VideoRotation90,
        180 => VideoRotation::VideoRotation180,
        270 => VideoRotation::VideoRotation270,
        _ => VideoRotation::VideoRotation0,
    };

    let frame = VideoFrame {
        rotation,
        timestamp_us: ts_us,
        frame_metadata: None,
        buffer,
    };
    sink.capture_frame(&frame);
}

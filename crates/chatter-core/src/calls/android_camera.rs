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
//! `NativeVideoSource::capture_frame` and the local Slint preview store. CameraX uses the modern Camera2 stack
//! (not the deprecated `android.hardware.Camera`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::calls::video_capture::LocalPreview;

use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;

/// Implemented by the platform layer (chatter-ui) to start/stop CameraX.
pub trait PlatformCamera: Send + Sync {
    /// `front` selects the front (selfie) vs back lens.
    fn start(&self, width: u32, height: u32, fps: u32, front: bool);
    fn stop(&self);
}

static PLATFORM_CAMERA: OnceLock<Box<dyn PlatformCamera>> = OnceLock::new();
static ACTIVE_SINK: Mutex<Option<ActiveCapture>> = Mutex::new(None);

struct ActiveCapture {
    sink: NativeVideoSource,
    preview: Option<LocalPreview>,
    front: bool,
    last_preview: Option<Instant>,
    stats_started: Instant,
    captured_frames: u32,
    preview_frames: u32,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
    last_frame: Instant,
    received_frame: bool,
}

// The self view should track a 30 fps camera without making the UI upload
// capture-resolution pixels (the same frame is also shown in the call pill).
// Allow a few milliseconds of camera timestamp jitter so a nominal 30 fps
// stream does not accidentally become 15 fps when one frame arrives early.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(30);
const PREVIEW_MAX_EDGE: u32 = 640;

/// Register the platform camera driver. Called once by chatter-ui at startup.
pub fn register_platform_camera(cam: Box<dyn PlatformCamera>) {
    if PLATFORM_CAMERA.set(cam).is_err() {
        tracing::warn!("Android camera: platform camera already registered");
    }
}

/// Drive camera capture for the lifetime of `running`. Returns `false` if no
/// platform camera is registered.
pub fn run_capture(
    sink: NativeVideoSource,
    running: Arc<AtomicBool>,
    width: u32,
    height: u32,
    fps: u32,
    device_id: &str,
    local_preview: Option<LocalPreview>,
    ready_tx: tokio::sync::oneshot::Sender<()>,
) -> bool {
    let Some(cam) = PLATFORM_CAMERA.get() else {
        return false;
    };
    // device_id comes from list_devices() ("front"/"back"); default to front.
    let front = device_id != "back";
    *ACTIVE_SINK.lock().unwrap() = Some(ActiveCapture {
        sink,
        preview: local_preview,
        front,
        last_preview: None,
        stats_started: Instant::now(),
        captured_frames: 0,
        preview_frames: 0,
        ready_tx: Some(ready_tx),
        last_frame: Instant::now(),
        received_frame: false,
    });
    tracing::info!("Android camera: starting CameraX {width}x{height}@{fps} front={front}");
    cam.start(width, height, fps, front);

    while running.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
        let stalled = ACTIVE_SINK.lock().unwrap().as_ref().is_some_and(|capture| {
            capture.last_frame.elapsed() > if capture.received_frame {
                Duration::from_secs(5)
            } else {
                Duration::from_secs(60)
            }
        });
        if stalled {
            tracing::error!("Android camera stopped delivering frames");
            break;
        }
    }

    cam.stop();
    if let Some(capture) = ACTIVE_SINK.lock().unwrap().take() {
        if let Some(preview) = capture.preview {
            preview.shared.remove_frame(&preview.local_id);
        }
    }
    tracing::info!("Android camera: capture stopped");
    true
}

/// Push one tightly-packed I420 frame from the platform camera callback.
/// `rotation_degrees` is the CameraX sensor rotation (0/90/180/270); we forward
/// it as LiveKit frame metadata. The local CPU preview rotates its own pixels.
pub fn push_frame_i420(data: &[u8], width: u32, height: u32, rotation_degrees: i32, ts_us: i64) {
    let mut guard = ACTIVE_SINK.lock().unwrap();
    let Some(capture) = guard.as_mut() else {
        return; // capture not active
    };

    // CameraX produces even-sized YUV420 frames. Reject invalid JNI dimensions
    // before allocating a native buffer or copying differently-sized planes.
    if width == 0
        || height == 0
        || width % 2 != 0
        || height % 2 != 0
        || width > i32::MAX as u32
        || height > i32::MAX as u32
    {
        return;
    }
    let Some(y_size) = (width as usize).checked_mul(height as usize) else {
        return;
    };
    let Some(uv_size) = (width as usize / 2).checked_mul(height as usize / 2) else {
        return;
    };
    let Some(frame_size) = uv_size.checked_mul(2).and_then(|uv| y_size.checked_add(uv)) else {
        return;
    };
    // The preview needs four bytes per pixel; check that size on 32-bit JNI too.
    if y_size.checked_mul(4).is_none() {
        return;
    }
    if data.len() < frame_size {
        tracing::warn!(
            "Android camera: short frame ({} < {})",
            data.len(),
            frame_size
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

    let mut frame = VideoFrame {
        rotation,
        timestamp_us: ts_us,
        frame_metadata: None,
        buffer,
    };
    capture.sink.capture_frame(&frame);
    capture.last_frame = Instant::now();
    capture.received_frame = true;
    capture.captured_frames += 1;
    if let Some(ready_tx) = capture.ready_tx.take() {
        let _ = ready_tx.send(());
    }
    capture.publish_preview(&mut frame.buffer, width, height, rotation_degrees);
    let elapsed = capture.stats_started.elapsed();
    if elapsed >= Duration::from_secs(5) {
        tracing::info!(
            "Android camera preview: capture_fps={:.1} preview_fps={:.1} source={}x{}",
            capture.captured_frames as f64 / elapsed.as_secs_f64(),
            capture.preview_frames as f64 / elapsed.as_secs_f64(),
            width,
            height,
        );
        capture.stats_started = Instant::now();
        capture.captured_frames = 0;
        capture.preview_frames = 0;
    }
}

impl ActiveCapture {
    fn publish_preview(&mut self, buffer: &mut I420Buffer, width: u32, height: u32, rotation: i32) {
        let Some(preview) = self.preview.as_ref() else {
            return;
        };
        let now = Instant::now();
        if self
            .last_preview
            .is_some_and(|last| now.duration_since(last) < PREVIEW_INTERVAL)
        {
            return;
        }
        // Scale only the preview after LiveKit has received the original frame.
        // libwebrtc's I420 scaler avoids a full-size RGBA conversion and cuts
        // Android's per-frame Slint buffer copy and texture upload as well.
        let (preview_width, preview_height) = if width.max(height) > PREVIEW_MAX_EDGE {
            let divisor = width.max(height);
            let scaled_width =
                ((width as u64 * PREVIEW_MAX_EDGE as u64 / divisor as u64) as u32 & !1).max(2);
            let scaled_height =
                ((height as u64 * PREVIEW_MAX_EDGE as u64 / divisor as u64) as u32 & !1).max(2);
            (scaled_width, scaled_height)
        } else {
            (width, height)
        };
        let scaled = (preview_width != width || preview_height != height)
            .then(|| buffer.scale(preview_width as i32, preview_height as i32));
        let preview_buffer = scaled.as_ref().unwrap_or(buffer);
        let rgba = crate::calls::video_playback::convert_i420_into(
            None,
            preview_buffer,
            preview_width,
            preview_height,
            preview_width as usize * preview_height as usize * 4,
        );
        // Android's Slint CPU image consumer does not apply rotation metadata.
        // Rotate pixels upright and mirror only the front-camera self view;
        // the published LiveKit frame above retains its original sensor metadata.
        let (rgba, width, height) =
            orient_preview(&rgba, preview_width, preview_height, rotation, self.front);
        preview.shared.set_frame(
            preview.local_id.clone(),
            Arc::new(rgba),
            width,
            height,
            false,
        );
        self.last_preview = Some(now);
        self.preview_frames += 1;
    }
}

fn orient_preview(
    rgba: &[u8],
    width: u32,
    height: u32,
    rotation: i32,
    mirror: bool,
) -> (Vec<u8>, u32, u32) {
    let (out_width, out_height) = match rotation {
        90 | 270 => (height, width),
        _ => (width, height),
    };
    let mut out = vec![0; rgba.len()];
    for y in 0..height {
        for x in 0..width {
            let (mut dx, dy) = match rotation {
                90 => (height - 1 - y, x),
                180 => (width - 1 - x, height - 1 - y),
                270 => (y, width - 1 - x),
                _ => (x, y),
            };
            if mirror {
                dx = out_width - 1 - dx;
            }
            let src = (y as usize * width as usize + x as usize) * 4;
            let dst = (dy as usize * out_width as usize + dx as usize) * 4;
            out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    (out, out_width, out_height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::SharedVideoFrames;
    use livekit::webrtc::video_source::VideoResolution;

    #[test]
    fn android_camera_preview_rotation_and_front_mirroring() {
        // Six synthetic labelled pixels make every orientation distinguishable.
        let pixels: Vec<u8> = (1..=6).flat_map(|n| [n, n, n, 255]).collect();
        for (rotation, expected, mirrored) in [
            (0, vec![1, 2, 3, 4, 5, 6], vec![3, 2, 1, 6, 5, 4]),
            (90, vec![4, 1, 5, 2, 6, 3], vec![1, 4, 2, 5, 3, 6]),
            (180, vec![6, 5, 4, 3, 2, 1], vec![4, 5, 6, 1, 2, 3]),
            (270, vec![3, 6, 2, 5, 1, 4], vec![6, 3, 5, 2, 4, 1]),
        ] {
            for (front, expected) in [(false, expected), (true, mirrored)] {
                let (out, w, h) = orient_preview(&pixels, 3, 2, rotation, front);
                assert_eq!(
                    (w, h),
                    if rotation == 90 || rotation == 270 {
                        (2, 3)
                    } else {
                        (3, 2)
                    }
                );
                assert_eq!(
                    out.chunks_exact(4).map(|p| p[0]).collect::<Vec<_>>(),
                    expected
                );
                assert!(out.chunks_exact(4).all(|p| p[3] == 255));
            }
        }
    }

    #[test]
    fn android_camera_preview_jni_delivery_throttle_and_stop() {
        struct Camera(std::sync::mpsc::Sender<bool>, Arc<AtomicBool>);
        impl PlatformCamera for Camera {
            fn start(&self, _: u32, _: u32, _: u32, front: bool) {
                self.0.send(front).unwrap();
            }
            fn stop(&self) {
                self.1.store(true, Ordering::SeqCst);
            }
        }
        let (started, ready) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        register_platform_camera(Box::new(Camera(started, stopped.clone())));
        let shared = SharedVideoFrames::new();
        let preview = LocalPreview {
            shared: shared.clone(),
            local_id: "synthetic-local-camera".into(),
            screen_share: false,
        };
        let running = Arc::new(AtomicBool::new(true));
        let run_flag = running.clone();
        // NativeVideoSource starts LiveKit runtime tasks, just as the production
        // actor does; supply a reactor while constructing and delivering frames.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let sink = NativeVideoSource::new(
            VideoResolution {
                width: 4,
                height: 2,
            },
            false,
        );
        let worker = std::thread::spawn(move || {
            run_capture(sink, run_flag, 4, 2, 30, "front", Some(preview), ready_tx)
        });
        assert!(ready.recv_timeout(Duration::from_secs(5)).unwrap());
        let mut data = vec![16, 235, 16, 235, 235, 16, 235, 16];
        data.extend([128; 4]);
        // This is the same entry point the production JNI camera callback uses.
        push_frame_i420(&data, 4, 2, 90, 1000);
        runtime
            .block_on(tokio::time::timeout(Duration::from_secs(5), ready_rx))
            .expect("first frame must signal readiness promptly")
            .expect("capture must keep readiness sender alive");
        let first = shared
            .get_frame("synthetic-local-camera")
            .expect("JNI frame must reach Slint's shared store");
        assert_eq!(
            (first.width, first.height, first.rotation_degrees),
            (2, 4, 0)
        );
        assert_eq!(first.rgba_data.len(), 32);
        assert!(!first.is_screen_share);
        assert!(first.rgba_data.chunks_exact(4).all(|p| p[3] == 255));
        // Clockwise rotation plus front self-view mirror maps row0 x0 first.
        assert!(first.rgba_data[0] < 5);
        assert!(first.rgba_data[4] > 250);
        push_frame_i420(&data, 4, 2, 90, 2000);
        assert_eq!(
            shared.get_frame("synthetic-local-camera").unwrap().frame_id,
            first.frame_id,
            "preview conversions must be rate limited"
        );
        ACTIVE_SINK.lock().unwrap().as_mut().unwrap().last_preview =
            Some(Instant::now() - PREVIEW_INTERVAL);
        push_frame_i420(&data[..3], 4, 2, 90, 3000);
        push_frame_i420(&data, 3, 2, 90, 3000);
        push_frame_i420(&data, u32::MAX - 1, u32::MAX - 1, 90, 3000);
        push_frame_i420(&data, i32::MAX as u32 - 1, i32::MAX as u32 - 1, 90, 3000);
        push_frame_i420(&data, 0, 2, 90, 3000);
        assert_eq!(
            shared.get_frame("synthetic-local-camera").unwrap().frame_id,
            first.frame_id,
            "invalid JNI frame must not publish"
        );
        push_frame_i420(&data, 4, 2, 90, 4000);
        assert!(shared.get_frame("synthetic-local-camera").unwrap().frame_id > first.frame_id);
        running.store(false, Ordering::SeqCst);
        assert!(worker.join().unwrap());
        assert!(stopped.load(Ordering::SeqCst));
        assert!(
            shared.get_frame("synthetic-local-camera").is_none(),
            "stop must release self-preview pixels"
        );
        push_frame_i420(&data, 4, 2, 90, 5000);
        assert!(shared.get_frame("synthetic-local-camera").is_none());
    }
}

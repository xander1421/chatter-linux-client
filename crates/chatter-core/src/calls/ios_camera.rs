//! AVFoundation capture owned by the video-capture thread. The shell retains
//! the native session; stop drains its serial callback queue before we drop
//! the Rust context or its LiveKit source.
use std::ffi::c_void;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use super::video_capture::{LocalPreview, VideoDevice};
use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;

type FrameCallback =
    unsafe extern "C" fn(*mut c_void, *const u8, usize, *const u8, usize, u32, u32, i64);
unsafe extern "C" {
    fn chatter_ios_camera_available(front: bool) -> bool;
    fn chatter_ios_camera_start(
        front: bool,
        width: u32,
        fps: u32,
        context: *mut c_void,
        callback: FrameCallback,
    ) -> *mut c_void;
    fn chatter_ios_camera_stop(session: *mut c_void);
}

pub(super) fn list_devices() -> Vec<VideoDevice> {
    [
        (true, "front", "Front Camera"),
        (false, "back", "Back Camera"),
    ]
    .into_iter()
    .filter_map(|(front, path, name)| {
        // SAFETY: discovery does not retain any Rust state.
        unsafe { chatter_ios_camera_available(front) }.then(|| VideoDevice {
            name: name.into(),
            path: path.into(),
            is_default: front,
            resolutions: vec![(640, 480), (1280, 720)],
        })
    })
    .collect()
}

struct Capture {
    source: NativeVideoSource,
    preview: Option<LocalPreview>,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
    frames: u64,
}

unsafe extern "C" fn frame(
    context: *mut c_void,
    y: *const u8,
    y_stride: usize,
    uv: *const u8,
    uv_stride: usize,
    width: u32,
    height: u32,
    timestamp: i64,
) {
    // Never unwind through AVFoundation. Native buffers stay locked for this
    // call only; copy their padded planes before returning to the delegate.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if context.is_null()
            || y.is_null()
            || uv.is_null()
            || width == 0
            || height == 0
            || width > 4096
            || height > 4096
            || width % 2 != 0
            || height % 2 != 0
            || y_stride < width as usize
            || uv_stride < width as usize
        {
            return;
        }
        // SAFETY: start supplies a live Box, callbacks are serial, and stop
        // drains that queue before this Box is released.
        let capture = unsafe { &mut *context.cast::<Capture>() };
        let mut buffer = I420Buffer::new(width, height);
        let (ys, us, vs) = buffer.strides();
        let (yp, up, vp) = buffer.data_mut();
        for row in 0..height as usize {
            // SAFETY: AVFoundation supplies height complete NV12 luma rows.
            let src = unsafe { std::slice::from_raw_parts(y.add(row * y_stride), width as usize) };
            yp[row * ys as usize..row * ys as usize + width as usize].copy_from_slice(src);
        }
        for row in 0..height as usize / 2 {
            // SAFETY: NV12 supplies height/2 complete interleaved UV rows.
            let src =
                unsafe { std::slice::from_raw_parts(uv.add(row * uv_stride), width as usize) };
            for col in 0..width as usize / 2 {
                up[row * us as usize + col] = src[col * 2];
                vp[row * vs as usize + col] = src[col * 2 + 1];
            }
        }
        let frame = VideoFrame {
            rotation: VideoRotation::VideoRotation0,
            timestamp_us: timestamp,
            buffer,
            frame_metadata: None,
        };
        capture.source.capture_frame(&frame);
        if let Some(ready) = capture.ready.take() {
            tracing::info!(
                width,
                height,
                "First iPhone camera frame delivered to LiveKit"
            );
            let _ = ready.send(());
        }
        if capture.frames % 2 == 0 {
            if let Some(preview) = &capture.preview {
                preview.tee_i420(&frame.buffer, width, height);
            }
        }
        capture.frames += 1;
    }));
}

pub(super) fn run_capture(
    path: String,
    source: NativeVideoSource,
    running: Arc<AtomicBool>,
    width: u32,
    fps: u32,
    preview: Option<LocalPreview>,
    ready: tokio::sync::oneshot::Sender<()>,
) {
    let mut capture = Box::new(Capture {
        source,
        preview,
        ready: Some(ready),
        frames: 0,
    });
    // SAFETY: the context has a stable address for the entire native session.
    let session = unsafe {
        chatter_ios_camera_start(
            path != "back",
            width,
            fps,
            (&mut *capture as *mut Capture).cast(),
            frame,
        )
    };
    if session.is_null() {
        tracing::warn!("iPhone camera unavailable or permission refused");
        return;
    }
    while running.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: exactly one stop for this retained native session. No callbacks
    // can access capture after stop returns.
    unsafe {
        chatter_ios_camera_stop(session);
    }
    tracing::info!(frames = capture.frames, "iPhone camera stopped");
}

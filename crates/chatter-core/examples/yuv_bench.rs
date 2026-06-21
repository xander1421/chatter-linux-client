//! Throughput of the i420 → RGBA conversion used by the call render path.
//!
//! Generates a synthetic I420 buffer for a few common screen-share
//! resolutions, runs `i420_to_abgr` (the same call `video_playback.rs`
//! uses), and reports max conversions/sec. If the bench cap is well above
//! the wire-side frame rate, the conversion isn't the bottleneck.
//!
//! Run release for an apples-to-apples measurement:
//!     cargo run --release --example yuv_bench -p chatter-core
//!
//! Debug-mode numbers are real (Slint debug app uses them), but the
//! conversion crate itself is C/SIMD so debug penalty is small.

use std::time::Instant;

fn bench(w: u32, h: u32, iters: u32) {
    let pixels = (w * h) as usize;
    let mut y = vec![0u8; pixels];
    let mut u = vec![0u8; pixels / 4];
    let mut v = vec![0u8; pixels / 4];

    // Fill with a non-trivial gradient so the SIMD path doesn't optimize
    // around a constant input.
    for i in 0..pixels {
        y[i] = (i & 0xff) as u8;
    }
    for i in 0..u.len() {
        u[i] = ((i * 7) & 0xff) as u8;
        v[i] = ((i * 13) & 0xff) as u8;
    }

    let mut dst = vec![0u8; pixels * 4];
    let dst_stride = w * 4;

    // Warm-up
    livekit::webrtc::native::yuv_helper::i420_to_abgr(
        &y, w, &u, w / 2, &v, w / 2,
        &mut dst, dst_stride, w as i32, h as i32,
    );

    let start = Instant::now();
    for _ in 0..iters {
        livekit::webrtc::native::yuv_helper::i420_to_abgr(
            &y, w, &u, w / 2, &v, w / 2,
            &mut dst, dst_stride, w as i32, h as i32,
        );
    }
    let elapsed = start.elapsed();

    let per_frame_us = elapsed.as_secs_f64() * 1_000_000.0 / iters as f64;
    let fps = 1_000_000.0 / per_frame_us;
    let mb_per_s = (pixels as f64 * 4.0 * iters as f64) / elapsed.as_secs_f64() / (1024.0 * 1024.0);

    println!(
        "{}x{} ({} iters): {:.2} ms/frame, {:.1} fps cap, {:.0} MB/s",
        w, h, iters, per_frame_us / 1000.0, fps, mb_per_s,
    );
}

fn main() {
    // Common screen-share resolutions
    bench(1280, 720, 500);
    bench(1920, 1080, 200);
    bench(2560, 1440, 100);
    bench(3840, 2160, 60);
}

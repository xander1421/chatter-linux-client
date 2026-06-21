//! Compare three I420 → RGBA conversion strategies head-to-head.
//!
//! Stages tested per resolution:
//!   1. Scalar Rust loop  — the version `video_playback.rs` shipped before
//!      the libyuv swap. Per-pixel f32 math + bounds checks. Establishes
//!      the "before" baseline.
//!   2. Single-thread libyuv (`i420_to_abgr`) — what `video_playback.rs`
//!      used between the libyuv swap and the rayon-chunked variant.
//!   3. Multi-thread libyuv (rayon row chunks) — current shipping path.
//!
//! Run with:
//!   cargo run --release --example yuv_threaded_bench -p chatter-core
//!
//! Debug numbers are still meaningful — libyuv is C/SIMD, so it doesn't
//! suffer the debug penalty. The scalar Rust loop does though, so the
//! debug speedup vs scalar is inflated; release narrows the gap to ~5–10×.

use std::time::Instant;

use livekit::webrtc::native::yuv_helper::i420_to_abgr;

/// Stage 1 — pre-libyuv scalar loop (matches the previous `i420_to_rgba`).
fn scalar_i420_to_rgba(
    y_data: &[u8], y_stride: u32,
    u_data: &[u8], u_stride: u32,
    v_data: &[u8], v_stride: u32,
    rgba: &mut [u8],
    width: u32, height: u32,
) {
    let width = width as usize;
    let height = height as usize;
    let y_stride = y_stride as usize;
    let u_stride = u_stride as usize;
    let v_stride = v_stride as usize;

    for row in 0..height {
        for col in 0..width {
            let y_idx = row * y_stride + col;
            let uv_row = row / 2;
            let uv_col = col / 2;
            let u_idx = uv_row * u_stride + uv_col;
            let v_idx = uv_row * v_stride + uv_col;

            let y = y_data.get(y_idx).copied().unwrap_or(0) as f32;
            let u = u_data.get(u_idx).copied().unwrap_or(128) as f32;
            let v = v_data.get(v_idx).copied().unwrap_or(128) as f32;

            let r = (y + 1.402 * (v - 128.0)).clamp(0.0, 255.0) as u8;
            let g = (y - 0.344136 * (u - 128.0) - 0.714136 * (v - 128.0)).clamp(0.0, 255.0) as u8;
            let b = (y + 1.772 * (u - 128.0)).clamp(0.0, 255.0) as u8;

            let dst = (row * width + col) * 4;
            rgba[dst] = r;
            rgba[dst + 1] = g;
            rgba[dst + 2] = b;
            rgba[dst + 3] = 255;
        }
    }
}

/// Stage 3 — rayon-chunked libyuv (mirror of `video_playback::i420_to_rgba`).
fn threaded_i420_to_rgba(
    y_data: &[u8], y_stride: u32,
    u_data: &[u8], u_stride: u32,
    v_data: &[u8], v_stride: u32,
    rgba: &mut [u8], dst_stride: u32,
    width: u32, height: u32,
) {
    let threads = rayon::current_num_threads().clamp(2, 8);
    let mut chunk_h = (height + threads as u32 - 1) / threads as u32;
    if chunk_h % 2 != 0 {
        chunk_h += 1;
    }

    let y_stride_u = y_stride as usize;
    let u_stride_u = u_stride as usize;
    let v_stride_u = v_stride as usize;
    let dst_stride_u = dst_stride as usize;

    let mut tasks: Vec<(&[u8], &[u8], &[u8], &mut [u8], i32)> = Vec::with_capacity(threads);
    let mut y_rest = y_data;
    let mut u_rest = u_data;
    let mut v_rest = v_data;
    let mut dst_rest: &mut [u8] = rgba;
    let mut row_cursor = 0u32;
    while row_cursor < height {
        let rows = chunk_h.min(height - row_cursor);
        let uv_rows = (rows + 1) / 2;
        let (y_chunk, y_after) = y_rest.split_at(rows as usize * y_stride_u);
        let (u_chunk, u_after) = u_rest.split_at(uv_rows as usize * u_stride_u);
        let (v_chunk, v_after) = v_rest.split_at(uv_rows as usize * v_stride_u);
        let (dst_chunk, dst_after) = dst_rest.split_at_mut(rows as usize * dst_stride_u);
        tasks.push((y_chunk, u_chunk, v_chunk, dst_chunk, rows as i32));
        y_rest = y_after;
        u_rest = u_after;
        v_rest = v_after;
        dst_rest = dst_after;
        row_cursor += rows;
    }

    use rayon::prelude::*;
    tasks.into_par_iter().for_each(|(y, u, v, dst, rows)| {
        i420_to_abgr(
            y, y_stride, u, u_stride, v, v_stride,
            dst, dst_stride, width as i32, rows,
        );
    });
}

fn bench(label: &str, w: u32, h: u32, iters: u32, run_scalar: bool) {
    let pixels = (w * h) as usize;
    let mut y = vec![0u8; pixels];
    let mut u = vec![0u8; pixels / 4];
    let mut v = vec![0u8; pixels / 4];
    for i in 0..pixels {
        y[i] = (i & 0xff) as u8;
    }
    for i in 0..u.len() {
        u[i] = ((i * 7) & 0xff) as u8;
        v[i] = ((i * 13) & 0xff) as u8;
    }

    let mut rgba = vec![0u8; pixels * 4];
    let dst_stride = w * 4;

    // Stage 2 — single-thread libyuv
    i420_to_abgr(
        &y, w, &u, w / 2, &v, w / 2,
        &mut rgba, dst_stride, w as i32, h as i32,
    );
    let t = Instant::now();
    for _ in 0..iters {
        i420_to_abgr(
            &y, w, &u, w / 2, &v, w / 2,
            &mut rgba, dst_stride, w as i32, h as i32,
        );
    }
    let lib_ms = t.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    // Stage 3 — threaded libyuv
    threaded_i420_to_rgba(
        &y, w, &u, w / 2, &v, w / 2,
        &mut rgba, dst_stride, w, h,
    );
    let t = Instant::now();
    for _ in 0..iters {
        threaded_i420_to_rgba(
            &y, w, &u, w / 2, &v, w / 2,
            &mut rgba, dst_stride, w, h,
        );
    }
    let thread_ms = t.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    // Stage 1 — scalar Rust (very slow at large sizes; cap iters)
    let scalar_ms = if run_scalar {
        scalar_i420_to_rgba(&y, w, &u, w / 2, &v, w / 2, &mut rgba, w, h);
        let scalar_iters = (iters / 20).max(2);
        let t = Instant::now();
        for _ in 0..scalar_iters {
            scalar_i420_to_rgba(&y, w, &u, w / 2, &v, w / 2, &mut rgba, w, h);
        }
        Some(t.elapsed().as_secs_f64() * 1000.0 / scalar_iters as f64)
    } else {
        None
    };

    println!("─── {} ({}×{}) ─────────────────────────────", label, w, h);
    if let Some(s) = scalar_ms {
        println!("  scalar Rust     : {:>7.2} ms  ({:>6.1} fps cap)", s, 1000.0 / s);
        println!("  libyuv 1-thread : {:>7.2} ms  ({:>6.1} fps cap)  speedup vs scalar = {:.1}×", lib_ms, 1000.0 / lib_ms, s / lib_ms);
        println!("  libyuv rayon    : {:>7.2} ms  ({:>6.1} fps cap)  speedup vs scalar = {:.1}× / vs 1-thread = {:.2}×", thread_ms, 1000.0 / thread_ms, s / thread_ms, lib_ms / thread_ms);
    } else {
        println!("  libyuv 1-thread : {:>7.2} ms  ({:>6.1} fps cap)", lib_ms, 1000.0 / lib_ms);
        println!("  libyuv rayon    : {:>7.2} ms  ({:>6.1} fps cap)  speedup vs 1-thread = {:.2}×", thread_ms, 1000.0 / thread_ms, lib_ms / thread_ms);
    }
    println!();
}

fn main() {
    println!(
        "rayon threads available: {}",
        rayon::current_num_threads()
    );
    println!();

    bench("720p",     1280,  720,  500, true);
    bench("1080p",    1920, 1080,  300, true);
    bench("1440p",    2560, 1440,  150, true);
    bench("ultrawide 5120×1440", 5120, 1440, 80, true);
    bench("4K",       3840, 2160,  60, false); // scalar too slow even at 60 iters
}

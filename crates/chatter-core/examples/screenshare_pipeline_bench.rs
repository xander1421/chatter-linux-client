//! End-to-end CPU-stage benchmark for the screen-share pipeline.
//!
//! The wire/decode/encode stages are owned by libwebrtc; the capture
//! source rate (PipeWire/portal) and the Slint GPU render rate need a live
//! harness. This bench isolates the *CPU conversion + copy* stages we own,
//! at several resolutions, single- vs multi-threaded, so we can see which
//! stage caps below a 144 / 244 Hz target.
//!
//! Run release (libyuv is SIMD C, but the harness around it isn't):
//!     cargo run --release --example screenshare_pipeline_bench -p chatter-core
//!
//! Stages measured (all use the exact libyuv entry points the call path uses):
//!   - sender   bgra/argb → i420   (`argb_to_i420`)         screen_capture.rs
//!   - receiver i420 → rgba         (`i420_to_abgr`)         video_playback.rs
//!   - receiver nv12 → rgba DIRECT  (`nv12_to_abgr`)         (skips to_i420 pass)
//!   - receiver nv12 → i420 → rgba  (two-pass, current HW-decode path)
//!   - UI       30 MB deep copy     (memcpy, == clone_from_slice)
//!   - receiver i420 → rgba, N threads (row-band split)

use std::time::Instant;

use livekit::webrtc::native::yuv_helper;

const TARGETS: &[(f64, &str)] = &[(60.0, "60"), (144.0, "144"), (244.0, "244")];

fn verdict(fps: f64) -> String {
    let mut hits = Vec::new();
    for (hz, label) in TARGETS {
        hits.push(format!("{}:{}", label, if fps >= *hz { "OK" } else { "MISS" }));
    }
    hits.join(" ")
}

fn report(stage: &str, w: u32, h: u32, iters: u32, elapsed: std::time::Duration, bytes_per_frame: usize) {
    let per_frame_ms = elapsed.as_secs_f64() * 1000.0 / iters as f64;
    let fps = 1000.0 / per_frame_ms;
    let gb_s = (bytes_per_frame as f64 * iters as f64) / elapsed.as_secs_f64() / 1e9;
    println!(
        "  {:<26} {:>5.2} ms/frame  {:>7.1} fps  {:>5.2} GB/s   [{}]",
        stage, per_frame_ms, fps, gb_s, verdict(fps)
    );
}

/// Synthetic planar buffers for a resolution.
struct Frame {
    w: u32,
    h: u32,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    nv12_uv: Vec<u8>, // interleaved UV for NV12
    rgba: Vec<u8>,
    bgra: Vec<u8>,
}

fn make_frame(w: u32, h: u32) -> Frame {
    let px = (w * h) as usize;
    let mut y = vec![0u8; px];
    let mut u = vec![0u8; px / 4];
    let mut v = vec![0u8; px / 4];
    let mut nv12_uv = vec![0u8; px / 2];
    for i in 0..px {
        y[i] = (i & 0xff) as u8;
    }
    for i in 0..u.len() {
        u[i] = ((i * 7) & 0xff) as u8;
        v[i] = ((i * 13) & 0xff) as u8;
        nv12_uv[i * 2] = u[i];
        nv12_uv[i * 2 + 1] = v[i];
    }
    let rgba = vec![0u8; px * 4];
    let mut bgra = vec![0u8; px * 4];
    for i in 0..bgra.len() {
        bgra[i] = (i & 0xff) as u8;
    }
    Frame { w, h, y, u, v, nv12_uv, rgba, bgra }
}

fn bench_resolution(w: u32, h: u32) {
    let iters = 200u32;
    let mut f = make_frame(w, h);
    let px = (w * h) as usize;
    let rgba_bytes = px * 4;
    let i420_bytes = px * 3 / 2;
    let dst_stride = w * 4;

    println!("\n=== {}x{}  ({:.1} MP, {:.1} MB RGBA / {:.1} MB I420) ===",
        w, h, px as f64 / 1e6, rgba_bytes as f64 / 1e6, i420_bytes as f64 / 1e6);

    // sender: bgra(argb word order) -> i420
    {
        let (y, u, v) = (&mut f.y, &mut f.u, &mut f.v);
        // warm
        yuv_helper::argb_to_i420(&f.bgra, dst_stride, y, w, u, w / 2, v, w / 2, w as i32, h as i32);
        let t = Instant::now();
        for _ in 0..iters {
            yuv_helper::argb_to_i420(&f.bgra, dst_stride, y, w, u, w / 2, v, w / 2, w as i32, h as i32);
        }
        report("sender bgra->i420", w, h, iters, t.elapsed(), rgba_bytes + i420_bytes);
    }

    // receiver: i420 -> rgba (current path)
    {
        yuv_helper::i420_to_abgr(&f.y, w, &f.u, w / 2, &f.v, w / 2, &mut f.rgba, dst_stride, w as i32, h as i32);
        let t = Instant::now();
        for _ in 0..iters {
            yuv_helper::i420_to_abgr(&f.y, w, &f.u, w / 2, &f.v, w / 2, &mut f.rgba, dst_stride, w as i32, h as i32);
        }
        report("recv i420->rgba (1thr)", w, h, iters, t.elapsed(), i420_bytes + rgba_bytes);
    }

    // receiver: nv12 -> rgba DIRECT (one pass — what we *could* do)
    {
        yuv_helper::nv12_to_abgr(&f.y, w, &f.nv12_uv, w, &mut f.rgba, dst_stride, w as i32, h as i32);
        let t = Instant::now();
        for _ in 0..iters {
            yuv_helper::nv12_to_abgr(&f.y, w, &f.nv12_uv, w, &mut f.rgba, dst_stride, w as i32, h as i32);
        }
        report("recv nv12->rgba direct", w, h, iters, t.elapsed(), i420_bytes + rgba_bytes);
    }

    // receiver: nv12 -> i420 -> rgba (two-pass, current HW-decode path via to_i420)
    {
        let mut y2 = vec![0u8; px];
        let mut u2 = vec![0u8; px / 4];
        let mut v2 = vec![0u8; px / 4];
        let t = Instant::now();
        for _ in 0..iters {
            yuv_helper::nv12_to_i420(&f.y, w, &f.nv12_uv, w, &mut y2, w, &mut u2, w / 2, &mut v2, w / 2, w as i32, h as i32);
            yuv_helper::i420_to_abgr(&y2, w, &u2, w / 2, &v2, w / 2, &mut f.rgba, dst_stride, w as i32, h as i32);
        }
        report("recv nv12->i420->rgba", w, h, iters, t.elapsed(), i420_bytes * 2 + rgba_bytes);
    }

    // UI: 30 MB deep copy (clone_from_slice == memcpy)
    {
        let src = f.rgba.clone();
        let mut dst = vec![0u8; rgba_bytes];
        dst.copy_from_slice(&src);
        let t = Instant::now();
        for _ in 0..iters {
            dst.copy_from_slice(&src);
        }
        report("UI deep copy (memcpy)", w, h, iters, t.elapsed(), rgba_bytes);
    }

    // receiver: i420 -> rgba multi-threaded (row-band split)
    for threads in [2usize, 4, 8] {
        // split into even-row bands
        let t = Instant::now();
        for _ in 0..iters {
            convert_threaded(&f.y, &f.u, &f.v, &mut f.rgba, w, h, threads);
        }
        report(&format!("recv i420->rgba ({}thr)", threads), w, h, iters, t.elapsed(), i420_bytes + rgba_bytes);
    }
}

/// Split the frame into `threads` horizontal bands (even row boundaries for
/// 4:2:0) and run `i420_to_abgr` per band in parallel.
fn convert_threaded(y: &[u8], u: &[u8], v: &[u8], rgba: &mut [u8], w: u32, h: u32, threads: usize) {
    let yw = w as usize;
    let cw = (w / 2) as usize;
    let dst_stride = (w * 4) as usize;

    // band height in rows, rounded to even
    let mut band = (h as usize).div_ceil(threads);
    if band % 2 == 1 { band += 1; }

    std::thread::scope(|s| {
        let mut rgba_rest = rgba;
        let mut row0 = 0usize;
        let mut handles = Vec::new();
        // pre-split the rgba mutable buffer into non-overlapping band slices
        let mut bands: Vec<(usize, usize, &mut [u8])> = Vec::new();
        while row0 < h as usize {
            let rows = band.min(h as usize - row0);
            let (this, rest) = rgba_rest.split_at_mut(rows * dst_stride);
            bands.push((row0, rows, this));
            rgba_rest = rest;
            row0 += rows;
        }
        for (r0, rows, dst) in bands {
            let y_off = r0 * yw;
            let c_off = (r0 / 2) * cw;
            let y_s = &y[y_off..y_off + rows * yw];
            let u_s = &u[c_off..c_off + (rows / 2) * cw];
            let v_s = &v[c_off..c_off + (rows / 2) * cw];
            handles.push(s.spawn(move || {
                yuv_helper::i420_to_abgr(
                    y_s, w, u_s, w / 2, v_s, w / 2,
                    dst, w * 4, w as i32, rows as i32,
                );
            }));
        }
        for h in handles { let _ = h.join(); }
    });
}

fn main() {
    println!("Screen-share CPU pipeline bench — verdict columns: target Hz : OK/MISS (single-frame ceiling)");
    println!("NOTE: capture source rate + Slint GPU render are NOT measured here (need live harness).");
    for (w, h) in [(5120, 1440), (3840, 2160), (3440, 1440), (2560, 1440), (1920, 1080)] {
        bench_resolution(w, h);
    }
    println!("\nReminder: ceilings are per-stage. Real pipeline fps = 1 / sum(serial stage times) unless pipelined across threads.");
}

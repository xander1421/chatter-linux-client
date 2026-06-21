//! Proof that pushing 60 Hz × 29 MB-payload closures onto Slint's
//! single-threaded event loop congests the UI thread.
//!
//! Slint's `invoke_from_event_loop` signature pins this:
//!     pub fn invoke_from_event_loop(func: impl FnOnce() + Send + 'static)
//! Every closure is drained serially on the thread the platform was bound
//! to. So whatever per-call cost we put inside that closure is paid
//! sequentially. The closure in `video_playback`'s push subscriber does
//! `Image::from_rgba8(SharedPixelBuffer::clone_from_slice(...))`, which
//! internally is an `alloc + memcpy` of `w*h*4` bytes. At 5120×1440 that
//! is ~29 MB.
//!
//! This bench measures the raw cost using `Vec<u8>` allocation +
//! `copy_from_slice` (the exact two operations `SharedPixelBuffer::
//! clone_from_slice` performs — Slint just wraps `Rc<[u8]>` over them).
//! No Slint dep is needed to prove the bound: if the per-call work
//! exceeds 16.6 ms, no single-threaded event loop on this hardware
//! sustains 60 Hz.
//!
//! Run:
//!   cargo run --release --example slint_invoke_congestion -p chatter-core

use std::time::{Duration, Instant};

/// Simulate `SharedPixelBuffer::clone_from_slice` cost: alloc a fresh
/// w*h*4 buffer and memcpy the source into it. Returns the buffer to
/// prevent the optimizer from deleting the work.
#[inline(never)]
fn make_buffer(src: &[u8]) -> Vec<u8> {
    let mut dst = Vec::with_capacity(src.len());
    dst.extend_from_slice(src);
    std::hint::black_box(&dst);
    dst
}

fn bench(label: &str, w: u32, h: u32, iters: u32) {
    let pixels = (w * h) as usize;
    let src = vec![0u8; pixels * 4];

    // Warm-up
    let _ = make_buffer(&src);

    let start = Instant::now();
    for _ in 0..iters {
        let buf = make_buffer(&src);
        drop(buf);
    }
    let elapsed = start.elapsed();
    let per_call_ms = elapsed.as_secs_f64() * 1000.0 / iters as f64;
    let fps_cap = 1000.0 / per_call_ms;
    let mb = pixels as f64 * 4.0 / (1024.0 * 1024.0);

    println!("─── {} ({}×{}, {:.1} MB/frame) ─────", label, w, h, mb);
    println!("  alloc + memcpy of one frame  : {:.2} ms", per_call_ms);
    println!("  fps cap if UI thread only:    {:.1}", fps_cap);
    if per_call_ms > 33.3 {
        println!("  > 33.3 ms — even 30 Hz unsustainable.");
    } else if per_call_ms > 16.6 {
        println!("  > 16.6 ms — single-threaded event loop CANNOT sustain 60 Hz.");
    } else if per_call_ms > 8.0 {
        println!("  > 8 ms — 60 Hz possible only if NOTHING else runs on UI thread.");
    } else {
        println!("  Fits in 60 Hz budget (assuming nothing else runs).");
    }
    println!();
}

/// Simulate "fire at fixed Hz, do the work serially" — exactly the
/// behaviour of a single-threaded event loop draining queued invokes.
/// Track how far behind schedule each tick is. Schedule slip = congestion.
fn congestion_run(label: &str, w: u32, h: u32, hz: u32, secs: u64) {
    let pixels = (w * h) as usize;
    let src = vec![0u8; pixels * 4];
    let interval = Duration::from_secs_f64(1.0 / hz as f64);

    let mut emitted = 0u64;
    let mut behind_total = Duration::ZERO;
    let mut max_behind = Duration::ZERO;

    let start = Instant::now();
    let stop_at = start + Duration::from_secs(secs);
    let mut due = start;

    while Instant::now() < stop_at {
        let now = Instant::now();
        if now > due {
            let behind = now - due;
            behind_total += behind;
            if behind > max_behind {
                max_behind = behind;
            }
        } else {
            std::thread::sleep(due - now);
        }
        let _ = make_buffer(&src);
        emitted += 1;
        due += interval;
    }

    let elapsed = start.elapsed().as_secs_f64();
    let achieved = emitted as f64 / elapsed;
    println!("─── {} @ {} Hz target for {} s ─────────", label, hz, secs);
    println!(
        "  target ticks: {}   achieved: {} ({:.1} Hz)",
        (hz as u64 * secs).max(1),
        emitted,
        achieved
    );
    println!(
        "  avg behind: {:.2} ms   max behind: {:.2} ms",
        behind_total.as_secs_f64() * 1000.0 / emitted.max(1) as f64,
        max_behind.as_secs_f64() * 1000.0
    );
    if achieved < hz as f64 * 0.95 {
        println!(
            "  ⇒ congested — achieved {:.1} Hz vs {} Hz target ({:.0}% of plan).",
            achieved,
            hz,
            achieved * 100.0 / hz as f64
        );
    } else {
        println!("  Kept up with schedule.");
    }
    println!();
}

fn main() {
    println!("Per-call cost of the UI-thread work fired by the video push subscriber:");
    println!();
    bench("1080p",              1920, 1080, 200);
    bench("1440p",              2560, 1440, 100);
    bench("5120×1440 ultrawide", 5120, 1440, 60);
    bench("4K",                 3840, 2160, 60);

    println!("Single-threaded schedule-slip simulation:");
    println!();
    congestion_run("5120×1440", 5120, 1440, 60, 3);
    congestion_run("5120×1440", 5120, 1440, 30, 3);
    congestion_run("1080p",     1920, 1080, 60, 3);
}

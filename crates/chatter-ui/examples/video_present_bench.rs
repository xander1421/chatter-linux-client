//! Present-throughput bench for the remote screen-share render path.
//!
//! Measures the *real* GPU present ceiling at 5120x1440 through the exact
//! renderer the app uses (Skia + winit + GLES), driving continuous redraw
//! and counting actual presents via the AfterRendering rendering-notifier.
//!
//! It reproduces today's hot path: a fresh `Image::from_rgba8` every frame
//! (which defeats Slint's texture cache — `ImageCacheKey::Invalid` — forcing
//! a full 30 MB CPU copy + GPU re-upload each present).
//!
//!   # vsync-limited ceiling (compositor refresh)
//!   cargo run --release -p chatter-ui --example video_present_bench --features desktop
//!
//!   # reuse one backing buffer (isolates alloc/fill from upload cost)
//!   cargo run --release -p chatter-ui --example video_present_bench --features desktop -- persist
//!
//!   # try to defeat vsync on Mesa to see the upload-limited ceiling
//!   vblank_mode=0 cargo run --release -p chatter-ui --example video_present_bench --features desktop
//!
//! Run it on the actual high-refresh monitor. If avg_fps pins at the panel
//! refresh, the render path can sustain it; if it sags below, the per-frame
//! 30 MB copy+upload is the wall (→ persistent-texture fix).

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};

slint::slint! {
    export component Bench inherits Window {
        in property <image> frame;
        preferred-width: 2560px;
        preferred-height: 720px;
        title: "video_present_bench";
        background: #101014;
        Image {
            source: root.frame;
            image-fit: contain;
            width: 100%;
            height: 100%;
        }
    }
}

const W: u32 = 5120;
const H: u32 = 1440;

/// ~29.5 MB. Moving pattern so motion is visible and the GPU can't dedupe.
fn make_buffer(tick: u32) -> SharedPixelBuffer<Rgba8Pixel> {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(W, H);
    let w = W as usize;
    let t = tick as usize;
    for (i, p) in buf.make_mut_slice().iter_mut().enumerate() {
        let x = (i % w) + t;
        let y = i / w;
        *p = Rgba8Pixel { r: x as u8, g: y as u8, b: (x ^ y) as u8, a: 255 };
    }
    buf
}

fn main() {
    let _ = slint::BackendSelector::new()
        .backend_name("winit".into())
        .require_opengl_es()
        .select();

    let persist = std::env::args().any(|a| a == "persist");

    let win = Bench::new().unwrap();

    let presents = Rc::new(Cell::new(0u64));
    let window_count = Rc::new(Cell::new(0u64));
    let last_report = Rc::new(Cell::new(Instant::now()));
    let start = Instant::now();

    {
        let presents = presents.clone();
        let window_count = window_count.clone();
        let last_report = last_report.clone();
        win.window()
            .set_rendering_notifier(move |state, _api| {
                if matches!(state, slint::RenderingState::AfterRendering) {
                    presents.set(presents.get() + 1);
                    window_count.set(window_count.get() + 1);
                    let now = Instant::now();
                    let dt = now.duration_since(last_report.get()).as_secs_f64();
                    if dt >= 1.0 {
                        eprintln!(
                            "instant_fps={:>6.1}  avg_fps={:>6.1}  presents={}",
                            window_count.get() as f64 / dt,
                            presents.get() as f64 / start.elapsed().as_secs_f64(),
                            presents.get(),
                        );
                        window_count.set(0);
                        last_report.set(now);
                    }
                }
            })
            .expect("set_rendering_notifier (needs a GPU backend)");
    }

    let persistent = make_buffer(0);
    let tick = Rc::new(Cell::new(0u32));
    let timer = slint::Timer::default();
    {
        let weak = win.as_weak();
        let tick = tick.clone();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(0),
            move || {
                let Some(win) = weak.upgrade() else { return };
                let t = tick.get().wrapping_add(1);
                tick.set(t);
                // from_rgba8 each frame == current app behaviour (cache miss → re-upload).
                let img = if persist {
                    Image::from_rgba8(persistent.clone())
                } else {
                    Image::from_rgba8(make_buffer(t))
                };
                win.set_frame(img);
                win.window().request_redraw();
            },
        );
    }

    win.run().unwrap();
}

//! Picker GIF animation.
//!
//! Manages animated GIF playback for the GIF picker panel.

use slint::ComponentHandle;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use slint::{Image, Model, Timer, TimerMode};

use super::inline::InlineGifState;

static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) fn picker_generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::SeqCst)
}

thread_local! {
    /// Picker GIF animations (gif_id -> frames).
    static PICKER_GIFS: RefCell<HashMap<String, InlineGifState>> = RefCell::new(HashMap::new());
    /// Global animation timer for picker GIFs.
    static PICKER_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
}

/// Register a GIF for the picker animation.
pub fn register_picker_gif(
    gif_id: String,
    prepared_frames: Vec<(slint::SharedPixelBuffer<slint::Rgba8Pixel>, u64)>,
) {
    if prepared_frames.len() <= 1 {
        return;
    }

    let (frames, delays): (Vec<Image>, Vec<u64>) = prepared_frames
        .into_iter()
        .map(|(pixels, delay)| (Image::from_rgba8(pixels), delay))
        .unzip();

    let state = InlineGifState {
        frames,
        delays,
        current_frame: 0,
        accumulated_ms: 0,
    };

    PICKER_GIFS.with(|gifs| {
        let mut gifs = gifs.borrow_mut();
        const BUDGET: usize = 16 * 1024 * 1024;
        let bytes = state.bytes();
        gifs.remove(&gif_id);
        while gifs.values().map(InlineGifState::bytes).sum::<usize>() > BUDGET.saturating_sub(bytes)
        {
            let Some(id) = gifs.keys().next().cloned() else {
                break;
            };
            gifs.remove(&id);
        }
        if bytes <= BUDGET {
            gifs.insert(gif_id, state);
        }
    });

    start_picker_timer_if_needed();
}

/// Clear all picker GIF animations.
pub fn clear_picker_gifs() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    PICKER_GIFS.with(|gifs| {
        gifs.borrow_mut().clear();
    });
    stop_picker_timer();
}

fn start_picker_timer_if_needed() {
    PICKER_TIMER.with(|timer_cell| {
        if timer_cell.borrow().is_some() {
            return;
        }

        let timer = Timer::default();
        let mut last_tick = Instant::now();
        timer.start(TimerMode::Repeated, Duration::from_millis(33), move || {
            let now = Instant::now();
            let elapsed = now.duration_since(last_tick).as_millis() as u64;
            last_tick += Duration::from_millis(elapsed);
            tick_picker_gifs(elapsed);
        });

        *timer_cell.borrow_mut() = Some(timer);
    });
}

fn stop_picker_timer() {
    PICKER_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });
}

fn tick_picker_gifs(elapsed_ms: u64) {
    let updates: Vec<(String, Image)> = PICKER_GIFS.with(|gifs| {
        let mut gifs = gifs.borrow_mut();
        let mut updates = Vec::new();

        for (id, state) in gifs.iter_mut() {
            if state.tick(elapsed_ms) {
                updates.push((id.clone(), state.current_image()));
            }
        }

        updates
    });

    if updates.is_empty() {
        return;
    }

    super::inline::INLINE_WINDOW.with(|window_cell| {
        if let Some(ref window_weak) = *window_cell.borrow() {
            if let Some(w) = window_weak.upgrade() {
                update_picker_thumbnails(&w, &updates);
            }
        }
    });
}

fn update_picker_thumbnails(window: &crate::MainWindow, updates: &[(String, Image)]) {
    // A GIF can sit in both sections (sent before, trending now).
    let changed: HashMap<&str, &Image> = updates
        .iter()
        .map(|(id, image)| (id.as_str(), image))
        .collect();
    for results in [
        window
            .global::<crate::ChatPane>()
            .get_composer_gif_picker_recent(),
        window
            .global::<crate::ChatPane>()
            .get_composer_gif_picker_results(),
    ] {
        let Some(model) = results
            .as_any()
            .downcast_ref::<slint::VecModel<crate::GifData>>()
        else {
            continue;
        };
        for idx in 0..model.row_count() {
            if let Some(mut gif) = model.row_data(idx) {
                if let Some(image) = changed.get(gif.id.as_str()) {
                    gif.preview = (*image).clone();
                    model.set_row_data(idx, gif);
                }
            }
        }
    }
}

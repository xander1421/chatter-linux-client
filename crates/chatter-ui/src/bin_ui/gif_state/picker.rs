//! Picker GIF animation.
//!
//! Manages animated GIF playback for the GIF picker panel.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use slint::{Image, Model, Timer, TimerMode};

use super::inline::InlineGifState;

thread_local! {
    /// Picker GIF animations (gif_id -> frames).
    static PICKER_GIFS: RefCell<HashMap<String, InlineGifState>> = RefCell::new(HashMap::new());
    /// Global animation timer for picker GIFs.
    static PICKER_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
}

/// Register a GIF for the picker animation.
pub fn register_picker_gif(gif_id: String, raw_frames: Vec<(Vec<u8>, u32, u32, u64)>) {
    if raw_frames.len() <= 1 {
        return;
    }

    // Convert raw frames to Slint Images
    let frames: Vec<Image> = raw_frames
        .iter()
        .map(|(data, width, height, _)| {
            crate::bin_ui::slint_conversions::rgba_to_slint_image(data, *width, *height)
        })
        .collect();

    let delays: Vec<u64> = raw_frames.iter().map(|(_, _, _, d)| *d).collect();

    let state = InlineGifState {
        frames,
        delays,
        current_frame: 0,
        accumulated_ms: 0,
    };

    PICKER_GIFS.with(|gifs| {
        gifs.borrow_mut().insert(gif_id, state);
    });

    start_picker_timer_if_needed();
}

/// Clear all picker GIF animations.
pub fn clear_picker_gifs() {
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
        timer.start(TimerMode::Repeated, Duration::from_millis(33), move || {
            tick_picker_gifs(33);
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
    // A GIF can sit in both sections (sent before, trending now); each tile
    // showing it animates.
    for results in [
        window.get_composer_gif_picker_recent(),
        window.get_composer_gif_picker_results(),
    ] {
        let Some(model) = results
            .as_any()
            .downcast_ref::<slint::VecModel<crate::GifData>>()
        else {
            continue;
        };
        for (update_id, new_image) in updates {
            for idx in 0..model.row_count() {
                if let Some(mut gif) = model.row_data(idx) {
                    if gif.id.as_str() == update_id {
                        gif.preview = new_image.clone();
                        model.set_row_data(idx, gif);
                        break;
                    }
                }
            }
        }
    }
}

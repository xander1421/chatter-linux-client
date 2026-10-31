//! Picker GIF animation.
//!
//! Manages animated GIF playback for the GIF picker panel.

use slint::ComponentHandle;
use std::cell::RefCell;
use std::collections::HashMap;
use slint::{Image, Model};

use super::inline::InlineGifState;

static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) fn picker_generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::SeqCst)
}

thread_local! {
    /// Picker GIF animations (gif_id -> frames).
    static PICKER_GIFS: RefCell<HashMap<String, InlineGifState>> = RefCell::new(HashMap::new());
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

    super::inline::start_inline_timer_if_needed();
}

/// Clear all picker GIF animations.
pub fn clear_picker_gifs() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    PICKER_GIFS.with(|gifs| {
        gifs.borrow_mut().clear();
    });
}

pub(super) fn has_picker_gifs() -> bool {
    PICKER_GIFS.with(|gifs| !gifs.borrow().is_empty())
}

pub(super) fn tick_picker_gifs(elapsed_ms: u64) {
    let Some(window) = super::inline::INLINE_WINDOW
        .with(|cell| cell.borrow().as_ref().and_then(|weak| weak.upgrade()))
    else {
        return;
    };
    let pane = window.global::<crate::ChatPane>();
    if !pane.get_composer_gif_picker_visible() {
        return;
    }
    let mut models = Vec::new();
    if pane.get_composer_gif_picker_search().is_empty() {
        models.push(pane.get_composer_gif_picker_recent());
    }
    if !pane.get_composer_gif_picker_loading() {
        models.push(pane.get_composer_gif_picker_results());
    }
    let mut advanced = HashMap::<String, Option<Image>>::new();
    PICKER_GIFS.with(|gifs| {
        let mut gifs = gifs.borrow_mut();
        for model in models {
            for index in 0..model.row_count() {
                let Some(mut gif) = model.row_data(index) else {
                    continue;
                };
                let frame = advanced.entry(gif.id.to_string()).or_insert_with(|| {
                    gifs.get_mut(gif.id.as_str())
                        .and_then(|state| state.tick(elapsed_ms).then(|| state.current_image()))
                });
                if let Some(frame) = frame {
                    gif.preview = frame.clone();
                    model.set_row_data(index, gif);
                }
            }
        }
    });
}

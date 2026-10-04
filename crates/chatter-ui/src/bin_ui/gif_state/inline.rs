//! Inline message GIF animation.
//!
//! Manages animated GIF playback for message thumbnails in the chat view.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use slint::{Image, Model, Timer, TimerMode, Weak};

use crate::MainWindow;
use super::MAX_INLINE_GIFS;

thread_local! {
    /// Inline GIF animations for message thumbnails (attachment_id -> frames).
    pub(super) static INLINE_GIFS: RefCell<HashMap<String, InlineGifState>> = RefCell::new(HashMap::new());
    /// LRU order for inline GIFs (front = oldest, back = newest).
    static INLINE_LRU: RefCell<VecDeque<String>> = RefCell::new(VecDeque::new());
    /// Global animation timer for inline GIFs.
    static INLINE_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Window weak reference for inline GIF updates.
    pub(super) static INLINE_WINDOW: RefCell<Option<Weak<MainWindow>>> = const { RefCell::new(None) };
}

/// State for an inline animated GIF in message list.
#[derive(Clone)]
pub(crate) struct InlineGifState {
    /// Pre-converted Slint images for each frame.
    pub(crate) frames: Vec<Image>,
    /// Frame delays in milliseconds.
    pub(crate) delays: Vec<u64>,
    /// Current frame index.
    pub(crate) current_frame: usize,
    /// Accumulated time since last frame change.
    pub(crate) accumulated_ms: u64,
}

impl InlineGifState {
    pub(crate) fn current_image(&self) -> Image {
        self.frames.get(self.current_frame).cloned().unwrap_or_default()
    }

    fn current_delay_ms(&self) -> u64 {
        self.delays.get(self.current_frame).copied().unwrap_or(100)
    }

    /// Advance time and return true if frame changed.
    pub(crate) fn tick(&mut self, elapsed_ms: u64) -> bool {
        self.accumulated_ms += elapsed_ms;
        let delay = self.current_delay_ms();

        if self.accumulated_ms >= delay {
            self.accumulated_ms -= delay;
            if !self.frames.is_empty() {
                self.current_frame = (self.current_frame + 1) % self.frames.len();
            }
            true
        } else {
            false
        }
    }
}

/// Insert an inline GIF with LRU eviction (used by viewer module).
pub(super) fn insert_inline_gif(att_id: String, state: InlineGifState) {
    // Evict oldest GIFs if at capacity (LRU)
    INLINE_LRU.with(|lru| {
        let mut lru = lru.borrow_mut();

        // Remove from LRU if already exists
        lru.retain(|id| id != &att_id);

        // Evict oldest if at capacity
        while lru.len() >= MAX_INLINE_GIFS {
            if let Some(oldest) = lru.pop_front() {
                INLINE_GIFS.with(|gifs| {
                    gifs.borrow_mut().remove(&oldest);
                });
            }
        }

        lru.push_back(att_id.clone());
    });

    INLINE_GIFS.with(|gifs| {
        gifs.borrow_mut().insert(att_id, state);
    });
}

/// Get a clone of an inline GIF state (used by viewer module).
pub(super) fn get_inline_gif(attachment_id: &str) -> Option<InlineGifState> {
    INLINE_GIFS.with(|gifs| {
        gifs.borrow().get(attachment_id).cloned()
    })
}

/// Initialize inline GIF animation system.
/// Call this once during window setup.
pub fn init_inline_animations(window: Weak<MainWindow>) {
    INLINE_WINDOW.with(|cell| {
        *cell.borrow_mut() = Some(window);
    });
}

/// Register an inline GIF from pre-converted Slint pixel buffers.
///
/// This is called on the UI thread after background decoding completes.
/// Takes Slint pixel buffers that were created on a background thread.
///
/// Returns the first frame image if successful.
pub fn register_inline_gif_from_buffers(
    attachment_id: &str,
    buffers: Vec<slint::SharedPixelBuffer<slint::Rgba8Pixel>>,
    delays: Vec<u64>,
) -> Option<Image> {
    if buffers.len() <= 1 {
        return None;
    }

    // Convert pixel buffers to Slint Images (fast, just ref-counting)
    let frames: Vec<Image> = buffers
        .into_iter()
        .map(|buffer| slint::Image::from_rgba8(buffer))
        .collect();

    register_inline_gif_from_images(attachment_id, frames, delays)
}

/// Register an inline GIF from pre-converted Slint Images.
///
/// Must be called from the UI thread.
pub fn register_inline_gif_from_images(
    attachment_id: &str,
    frames: Vec<Image>,
    delays: Vec<u64>,
) -> Option<Image> {
    if frames.len() <= 1 {
        return None;
    }

    let first_frame = frames.first().cloned().unwrap_or_default();
    let frame_count = frames.len();
    let att_id_string = attachment_id.to_string();

    // Check if this GIF already exists (progressive update) - preserve playback position
    let (current_frame, accumulated_ms) = INLINE_GIFS.with(|gifs| {
        if let Some(existing) = gifs.borrow().get(&att_id_string) {
            // Preserve position, but clamp to new frame count
            let frame = existing.current_frame.min(frame_count.saturating_sub(1));
            (frame, existing.accumulated_ms)
        } else {
            (0, 0)
        }
    });

    let state = InlineGifState {
        frames,
        delays,
        current_frame,
        accumulated_ms,
    };

    // Evict oldest GIFs if at capacity (LRU)
    INLINE_LRU.with(|lru| {
        let mut lru = lru.borrow_mut();

        // Remove from LRU if already exists (will re-add at end)
        lru.retain(|id| id != &att_id_string);

        // Evict oldest if at capacity
        while lru.len() >= MAX_INLINE_GIFS {
            if let Some(oldest) = lru.pop_front() {
                INLINE_GIFS.with(|gifs| {
                    gifs.borrow_mut().remove(&oldest);
                });
                tracing::debug!("Evicted oldest inline GIF: {}", oldest);
            }
        }

        // Add new GIF to LRU (most recent)
        lru.push_back(att_id_string.clone());
    });

    INLINE_GIFS.with(|gifs| {
        gifs.borrow_mut().insert(att_id_string, state);
    });

    tracing::info!(
        "Registered inline GIF animation: id={}, frames={}",
        attachment_id,
        frame_count
    );

    start_inline_timer_if_needed();

    Some(first_frame)
}

/// Unregister an inline GIF (when message is removed from view).
pub fn unregister_inline_gif(attachment_id: &str) {
    INLINE_GIFS.with(|gifs| {
        gifs.borrow_mut().remove(attachment_id);
    });
    INLINE_LRU.with(|lru| {
        lru.borrow_mut().retain(|id| id != attachment_id);
    });

    // Stop timer if no more GIFs
    let should_stop = INLINE_GIFS.with(|gifs| gifs.borrow().is_empty());
    if should_stop {
        stop_inline_timer();
    }
}

/// Start the global inline animation timer if not already running.
pub(super) fn start_inline_timer_if_needed() {
    INLINE_TIMER.with(|timer_cell| {
        if timer_cell.borrow().is_some() {
            tracing::trace!("Inline GIF timer already running");
            return; // Already running
        }

        let timer = Timer::default();
        // Run at ~30fps for smooth animation
        timer.start(TimerMode::Repeated, Duration::from_millis(33), move || {
            tick_inline_gifs(33);
        });

        *timer_cell.borrow_mut() = Some(timer);
        tracing::info!("Started inline GIF animation timer (30fps)");
    });
}

/// Stop the global inline animation timer.
fn stop_inline_timer() {
    INLINE_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });
    tracing::debug!("Stopped inline GIF animation timer");
}

/// Advance the inline GIFs on the rows the message list has on screen, and
/// write the frames that changed into their attachment rows.
///
/// Off-screen GIFs keep their frame: advancing them would only rewrite model
/// rows nobody draws (each a whole MessageItemData copy) 30 times a second.
/// The range is the list's own estimate (`message-*-visible-row`, each within
/// a row), widened by one row either side.
fn tick_inline_gifs(elapsed_ms: u64) {
    let Some(w) = INLINE_WINDOW.with(|cell| cell.borrow().as_ref().and_then(|weak| weak.upgrade())) else {
        return;
    };
    let messages = w.get_messages();
    let Some(rows) = rows_to_animate(
        w.get_message_first_visible_row(),
        w.get_message_last_visible_row(),
        messages.row_count(),
    ) else {
        return;
    };

    // Every visible occurrence of a registered GIF: (its attachments, index, row).
    let mut shown = Vec::new();
    for msg_idx in rows {
        let Some(msg) = messages.row_data(msg_idx) else { continue };
        for att_idx in 0..msg.attachments.row_count() {
            if let Some(att) = msg.attachments.row_data(att_idx) {
                if INLINE_GIFS.with(|gifs| gifs.borrow().contains_key(att.id.as_str())) {
                    shown.push((msg.attachments.clone(), att_idx, att));
                }
            }
        }
    }
    if shown.is_empty() {
        return;
    }

    // Each GIF advances once per tick, however many rows show it.
    let mut changed: HashMap<String, Option<Image>> = HashMap::new();
    INLINE_GIFS.with(|gifs| {
        let mut gifs = gifs.borrow_mut();
        for (_, _, att) in &shown {
            if changed.contains_key(att.id.as_str()) {
                continue;
            }
            let frame = gifs
                .get_mut(att.id.as_str())
                .and_then(|state| state.tick(elapsed_ms).then(|| state.current_image()));
            changed.insert(att.id.to_string(), frame);
        }
    });

    for (attachments, att_idx, mut att) in shown {
        let Some(Some(frame)) = changed.get(att.id.as_str()) else { continue };
        let Some(model) = attachments.as_any().downcast_ref::<slint::VecModel<crate::MessageAttachmentData>>() else {
            continue;
        };
        att.thumbnail = frame.clone();
        att.has_thumbnail = true;
        model.set_row_data(att_idx, att);
    }
}

/// The message rows whose GIFs advance: the list's visible range widened by
/// one row either side, clamped to the model. `None` when the list reports no
/// rows (`last == -1`: no list, or a conversation still loading) or the model
/// is empty.
fn rows_to_animate(first: i32, last: i32, row_count: usize) -> Option<std::ops::RangeInclusive<usize>> {
    if last < 0 || row_count == 0 {
        return None;
    }
    let first = first.saturating_sub(1).max(0) as usize;
    let last = (last.saturating_add(1) as usize).min(row_count - 1);
    (first <= last).then_some(first..=last)
}

/// Show `image` as the thumbnail of attachment `attachment_id`, wherever it
/// is in the message model. A one-off scan (a GIF's first frame); the
/// animation itself goes through `tick_inline_gifs`, which looks only at the
/// visible rows.
fn set_attachment_thumbnail(window: &MainWindow, attachment_id: &str, image: Image) {
    let messages = window.get_messages();
    for msg_idx in 0..messages.row_count() {
        let Some(msg) = messages.row_data(msg_idx) else { continue };
        for att_idx in 0..msg.attachments.row_count() {
            let Some(mut att) = msg.attachments.row_data(att_idx) else { continue };
            if att.id.as_str() != attachment_id {
                continue;
            }
            if let Some(model) = msg.attachments.as_any().downcast_ref::<slint::VecModel<crate::MessageAttachmentData>>() {
                att.thumbnail = image;
                att.has_thumbnail = true;
                model.set_row_data(att_idx, att);
            }
            return;
        }
    }
}

/// Clear all inline GIF animations.
pub fn clear_inline_gifs() {
    INLINE_GIFS.with(|gifs| {
        gifs.borrow_mut().clear();
    });
    INLINE_LRU.with(|lru| {
        lru.borrow_mut().clear();
    });
    stop_inline_timer();
}

/// Auto-register all GIF attachments from current messages for animation.
///
/// Scans the message list in the UI for GIF attachments and registers them
/// for inline animation. Call this after loading messages into the UI.
///
/// Must be called from the UI thread (inside invoke_from_event_loop).
pub fn auto_register_gifs_from_messages(
    window_weak: Weak<MainWindow>,
    ui_ctx: std::sync::Arc<chatter_core::client::ChatterClient>,
) {
    let Some(w) = window_weak.upgrade() else {
        return;
    };

    let messages = w.get_messages();
    let mut gif_ids = Vec::new();

    for msg_idx in 0..messages.row_count() {
        if let Some(msg) = messages.row_data(msg_idx) {
            for att_idx in 0..msg.attachments.row_count() {
                if let Some(att) = msg.attachments.row_data(att_idx) {
                    let id_str = att.id.as_str();

                    // QUICK CHECK: Already registered? Skip expensive string conversions/checks
                    if INLINE_GIFS.with(|gifs| gifs.borrow().contains_key(id_str)) {
                        continue;
                    }

                    let id = id_str.to_string();
                    let mime = att.mime_type.to_string();
                    let filename = att.filename.to_string();

                    // GIFs are now real attachments (uploaded to our CDN)
                    if mime == "image/gif" || filename.to_lowercase().ends_with(".gif") {
                        gif_ids.push(id);
                    }
                }
            }
        }
    }
    if gif_ids.is_empty() {
        return;
    }

    tracing::debug!("Auto-registering {} GIF attachments", gif_ids.len());

    // Register each GIF (spawns background threads)
    for att_id in gif_ids {
        register_gif_attachment(att_id, ui_ctx.clone(), window_weak.clone());
    }
}

/// Register a single GIF attachment for inline animation.
///
/// This is called after sending a message with GIF attachments.
/// Spawns an independent background thread to decrypt/decode, then registers on UI thread.
///
/// This function returns immediately - all work happens in background.
/// Safe to call from any context (including nested runtimes).
pub fn register_gif_attachment(
    attachment_id: String,
    ui_ctx: std::sync::Arc<chatter_core::client::ChatterClient>,
    window_weak: slint::Weak<crate::MainWindow>,
) {
    // Optimization: Check if already registered (O(1) in UI thread)
    let already_registered = INLINE_GIFS.with(|gifs| gifs.borrow().contains_key(&attachment_id));
    if already_registered {
        return;
    }

    // Spawn independent thread to avoid runtime context issues
    std::thread::spawn(move || {
        use chatter_core::domain::attachment::ContentLocation;

        // Parse attachment ID
        let att_id: chatter_core::domain::ids::AttachmentId = match attachment_id.parse() {
            Ok(id) => id,
            Err(_) => {
                tracing::debug!("Invalid attachment ID for GIF registration: {}", attachment_id);
                return;
            }
        };

        // Check if attachment has content on filesystem
        let attachment = match ui_ctx.get_attachment(&att_id) {
            Some(att) if att.content_location == ContentLocation::OnFilesystem => att,
            _ => {
                tracing::debug!("GIF {} not ready on filesystem yet", attachment_id);
                return;
            }
        };

        // Check if it's a GIF
        if attachment.metadata.mime_type != "image/gif" {
            return;
        }

        // Decrypt to memory (uses streaming for large files)
        let data = match ui_ctx.decrypt_attachment_auto(&att_id) {
            Ok(buffer) => buffer.as_slice().to_vec(),
            Err(e) => {
                tracing::debug!("Could not decrypt GIF {}: {}", attachment_id, e);
                return;
            }
        };

        // Decode first frame immediately for instant display
        use chatter_core::media::gif::decode_first_frame_only;

        let first_frame = match decode_first_frame_only(&data) {
            Ok(Some(f)) => f,
            Ok(None) => return, // Not a GIF or empty
            Err(e) => {
                tracing::debug!("Failed to decode GIF first frame {}: {}", attachment_id, e);
                return;
            }
        };

        tracing::debug!(
            "Decoded first frame of GIF {} immediately",
            attachment_id
        );

        // Register first frame on UI thread immediately (for instant display)
        let att_id_for_ui = attachment_id.clone();
        let window_weak_clone = window_weak.clone();
        let first_rgba = first_frame.data.clone();
        let first_width = first_frame.width;
        let first_height = first_frame.height;

        let _ = slint::invoke_from_event_loop(move || {
            // Show first frame immediately
            let first_image = crate::bin_ui::slint_conversions::rgba_to_slint_image(&first_rgba,
                first_width,
                first_height,);

            if let Some(w) = window_weak_clone.upgrade() {
                set_attachment_thumbnail(&w, &att_id_for_ui, first_image);
            }
            tracing::info!("Displayed sent GIF first frame: {}", att_id_for_ui);
        });

        // Now decode remaining frames progressively and start animation
        use image::codecs::gif::GifDecoder;
        use image::AnimationDecoder;
        use std::io::Cursor;

        let cursor = Cursor::new(&data);
        let decoder = match GifDecoder::new(cursor) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Failed to create GIF decoder for {}: {}", attachment_id, e);
                return;
            }
        };

        let frames_iter = decoder.into_frames();
        let mut all_buffers: Vec<slint::SharedPixelBuffer<slint::Rgba8Pixel>> = vec![];
        let mut all_delays: Vec<u64> = vec![];
        let mut animation_started = false;
        const BATCH_SIZE: usize = 5;

        for (_idx, frame_result) in frames_iter.enumerate() {
            let frame = match frame_result {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("GIF frame {} error for {}: {}", _idx, attachment_id, e);
                    continue;
                }
            };

            let delay = frame.delay();
            let (numer, denom) = delay.numer_denom_ms();
            let delay_ms = if denom == 0 { 100 } else { (numer as u64) / (denom as u64) };
            let delay_ms = delay_ms.max(20);

            let buffer = frame.into_buffer();
            let (width, height) = buffer.dimensions();
            let raw_data = buffer.into_raw();

            // Convert to Slint pixel buffer IN THE BACKGROUND THREAD (Send)
            let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &raw_data,
                width,
                height,
            );

            all_buffers.push(pixel_buffer);
            all_delays.push(delay_ms);

            // Start animation as soon as we have enough frames
            if !animation_started && all_buffers.len() >= 2 {
                animation_started = true;
                let buffers_batch = all_buffers.clone();
                let delays_batch = all_delays.clone();
                let att_id_batch = attachment_id.clone();
                let weak_batch = window_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(first_image) = register_inline_gif_from_buffers(&att_id_batch, buffers_batch, delays_batch) {
                        if let Some(w) = weak_batch.upgrade() {
                            set_attachment_thumbnail(&w, &att_id_batch, first_image);
                        }
                        tracing::info!("Started sent GIF animation: {}", att_id_batch);
                    }
                });
            }
            // Update animation periodically with new frames
            else if animation_started && all_buffers.len() % BATCH_SIZE == 0 {
                let buffers_batch = all_buffers.clone();
                let delays_batch = all_delays.clone();
                let att_id_batch = attachment_id.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    register_inline_gif_from_buffers(&att_id_batch, buffers_batch, delays_batch);
                });
            }
        }

        // Final update with all frames
        if all_buffers.len() > 1 {
            let att_id_full = attachment_id.clone();
            let weak_full = window_weak.clone();
            let buffers_full = all_buffers.clone();
            let delays_full = all_delays.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(first_image) = register_inline_gif_from_buffers(&att_id_full, buffers_full, delays_full) {
                    if let Some(w) = weak_full.upgrade() {
                        set_attachment_thumbnail(&w, &att_id_full, first_image);
                    }
                }
                tracing::info!("Completed sent GIF animation: {}", att_id_full);
            });
        }
    });
}

// Note: legacy unit tests that constructed an `AttachmentService` directly were
// removed here. They were already disabled via `#[cfg(any())]` and their
// function signature was stale (the live `register_gif_attachment` now takes
// an `Arc<ChatterClient>` rather than `Arc<AttachmentService>`), so keeping
// the dead block only kept a forbidden direct reference to the service.
// End-to-end coverage lives in the integration suite.

#[cfg(test)]
mod tests {
    use super::rows_to_animate;

    #[test]
    fn no_list_animates_nothing() {
        // -1 is the "no list" sentinel (message_list.slint); it must not be
        // read as row 0 once widened by a row.
        assert_eq!(rows_to_animate(0, -1, 10), None);
        assert_eq!(rows_to_animate(5, -1, 10), None);
    }

    #[test]
    fn empty_model_animates_nothing() {
        assert_eq!(rows_to_animate(0, 3, 0), None);
    }

    #[test]
    fn visible_range_widens_by_one_and_clamps() {
        assert_eq!(rows_to_animate(4, 7, 20), Some(3..=8));
        assert_eq!(rows_to_animate(0, 2, 20), Some(0..=3));
        assert_eq!(rows_to_animate(17, 19, 20), Some(16..=19));
    }

    #[test]
    fn range_past_a_shrunken_model_is_empty() {
        // The estimate is from the last layout pass; the model may have
        // shrunk since.
        assert_eq!(rows_to_animate(30, 35, 10), None);
    }
}

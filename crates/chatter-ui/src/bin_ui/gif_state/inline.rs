//! Inline message GIF animation.
//!
//! Manages animated GIF playback for message thumbnails in the chat view.

use slint::ComponentHandle;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use slint::{Image, Model, Timer, TimerMode, Weak};

use super::{INLINE_ANIMATION_BYTES, INLINE_FRAME_PX, MAX_INLINE_GIF_BYTES, MAX_INLINE_GIFS};
use crate::MainWindow;

thread_local! {
    /// Inline GIF animations for message thumbnails (attachment_id -> frames).
    pub(super) static INLINE_GIFS: RefCell<HashMap<String, InlineGifState>> = RefCell::new(HashMap::new());
    /// LRU order for inline GIFs (front = oldest, back = newest).
    static INLINE_LRU: RefCell<VecDeque<String>> = RefCell::new(VecDeque::new());
    /// Global animation timer for inline GIFs.
    static INLINE_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Window weak reference for inline GIF updates.
    pub(super) static INLINE_WINDOW: RefCell<Option<Weak<MainWindow>>> = const { RefCell::new(None) };
    static INLINE_CLIENT: RefCell<Option<std::sync::Weak<chatter_core::client::ChatterClient>>> = const { RefCell::new(None) };
    static INLINE_REQUESTED: RefCell<HashMap<String, Instant>> = RefCell::new(HashMap::new());
    /// Last message-list offset seen by the GIF timer. A changed offset means
    /// the user is dragging or a fling is moving rows this frame.
    static INLINE_LAST_SCROLL_POSITION: Cell<Option<f32>> = const { Cell::new(None) };
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
    pub(super) fn bytes(&self) -> usize {
        self.frames
            .iter()
            .map(|frame| {
                let size = frame.size();
                size.width as usize * size.height as usize * 4
            })
            .sum()
    }
    pub(crate) fn current_image(&self) -> Image {
        self.frames
            .get(self.current_frame)
            .cloned()
            .unwrap_or_default()
    }

    /// Advance time and return true if frame changed.
    pub(crate) fn tick(&mut self, elapsed_ms: u64) -> bool {
        chatter_core::media::gif::animator::advance_playback(
            &self.delays,
            &mut self.current_frame,
            &mut self.accumulated_ms,
            elapsed_ms,
        )
    }
}

/// Insert an inline GIF with LRU eviction (used by viewer module).
pub(super) fn insert_inline_gif(att_id: String, state: InlineGifState) {
    let new_bytes = state.bytes();
    if new_bytes > MAX_INLINE_GIF_BYTES {
        return;
    }
    INLINE_LRU.with(|lru| {
        let mut lru = lru.borrow_mut();
        lru.retain(|id| id != &att_id);
        INLINE_GIFS.with(|gifs| {
            gifs.borrow_mut().remove(&att_id);
        });
        while lru.len() >= MAX_INLINE_GIFS
            || INLINE_GIFS.with(|gifs| {
                gifs.borrow()
                    .values()
                    .map(InlineGifState::bytes)
                    .sum::<usize>()
                    > MAX_INLINE_GIF_BYTES.saturating_sub(new_bytes)
            })
        {
            if let Some(oldest) = lru.pop_front() {
                INLINE_GIFS.with(|gifs| {
                    gifs.borrow_mut().remove(&oldest);
                });
            } else {
                break;
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
    INLINE_GIFS.with(|gifs| gifs.borrow().get(attachment_id).cloned())
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
    if buffers.is_empty() {
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
    if frames.is_empty() {
        return None;
    }

    let first_frame = frames.first().cloned().unwrap_or_default();
    let frame_count = frames.len();
    let att_id_string = attachment_id.to_string();

    // Duplicate completions keep the current playback position.
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

    insert_inline_gif(att_id_string, state);

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
        let mut last_tick = Instant::now();
        // Run at ~30fps for smooth animation
        timer.start(TimerMode::Repeated, Duration::from_millis(33), move || {
            let now = Instant::now();
            let elapsed = now.duration_since(last_tick).as_millis() as u64;
            last_tick += Duration::from_millis(elapsed);
            tick_inline_gifs(elapsed);
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
    let Some(w) = INLINE_WINDOW.with(|cell| cell.borrow().as_ref().and_then(|weak| weak.upgrade()))
    else {
        return;
    };
    // Replacing a GIF's attachment row schedules another full render while
    // the message list is already re-laying out under the finger. Hold the
    // current frame whenever its offset moves; playback resumes at rest.
    let position = w.global::<crate::ChatPane>().get_message_scroll_position() as f32;
    let moved = INLINE_LAST_SCROLL_POSITION.with(|last| {
        let moved = last.get().is_some_and(|previous| previous != position);
        last.set(Some(position));
        moved
    });
    if moved {
        return;
    }
    let messages = w.global::<crate::ChatPane>().get_messages();
    let Some(rows) = rows_to_animate(
        w.global::<crate::ChatPane>()
            .get_message_first_visible_row(),
        w.global::<crate::ChatPane>().get_message_last_visible_row(),
        messages.row_count(),
    ) else {
        return;
    };

    // Every visible occurrence of a registered GIF: (its attachments, index, row).
    let mut shown = Vec::new();
    for msg_idx in rows {
        let Some(msg) = messages.row_data(msg_idx) else {
            continue;
        };
        for att_idx in 0..msg.attachments.row_count() {
            if let Some(att) = msg.attachments.row_data(att_idx) {
                if INLINE_GIFS.with(|gifs| gifs.borrow().contains_key(att.id.as_str())) {
                    shown.push((msg.attachments.clone(), att_idx, att));
                } else if att.mime_type.as_str() == "image/gif"
                    || att.filename.to_lowercase().ends_with(".gif")
                {
                    // Promote newly visible queued work; retry a dropped or failed
                    // request at most once per five seconds while it stays visible.
                    let request = INLINE_REQUESTED.with(|requests| {
                        let mut requests = requests.borrow_mut();
                        if requests
                            .get(att.id.as_str())
                            .is_some_and(|last| last.elapsed() < Duration::from_secs(5))
                        {
                            return false;
                        }
                        requests.insert(att.id.to_string(), Instant::now());
                        true
                    });
                    if request {
                        if let Some(client) =
                            INLINE_CLIENT.with(|c| c.borrow().as_ref().and_then(|c| c.upgrade()))
                        {
                            let weak = INLINE_WINDOW.with(|w| w.borrow().clone());
                            if let Some(weak) = weak {
                                register_gif_attachment(att.id.to_string(), client, weak);
                            }
                        }
                    }
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
        let Some(Some(frame)) = changed.get(att.id.as_str()) else {
            continue;
        };
        let Some(model) = attachments
            .as_any()
            .downcast_ref::<slint::VecModel<crate::MessageAttachmentData>>()
        else {
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
fn rows_to_animate(
    first: i32,
    last: i32,
    row_count: usize,
) -> Option<std::ops::RangeInclusive<usize>> {
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
    let pane = window.global::<crate::ChatPane>();
    for messages in [pane.get_messages(), pane.get_thread_replies()] {
        for msg_idx in 0..messages.row_count() {
            let Some(msg) = messages.row_data(msg_idx) else {
                continue;
            };
            for att_idx in 0..msg.attachments.row_count() {
                let Some(mut att) = msg.attachments.row_data(att_idx) else {
                    continue;
                };
                if att.id.as_str() != attachment_id {
                    continue;
                }
                if let Some(model) = msg
                    .attachments
                    .as_any()
                    .downcast_ref::<slint::VecModel<crate::MessageAttachmentData>>()
                {
                    if att.width <= 0 || att.height <= 0 {
                        let size = image.size();
                        att.width = size.width as i32;
                        att.height = size.height as i32;
                    }
                    att.thumbnail = image.clone();
                    att.is_downloading = false;
                    att.progress = 1.0;
                    att.has_thumbnail = true;
                    model.set_row_data(att_idx, att);
                }
            }
        }
    }
}

/// Clear all inline GIF animations.
pub fn clear_inline_gifs() {
    INLINE_CLIENT.with(|c| c.borrow_mut().take());
    INLINE_REQUESTED.with(|r| r.borrow_mut().clear());
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

    let messages = w.global::<crate::ChatPane>().get_messages();
    let mut gif_ids = Vec::new();
    for msg_idx in 0..messages.row_count() {
        if let Some(msg) = messages.row_data(msg_idx) {
            collect_unregistered_gif_ids(&msg, &mut gif_ids);
        }
    }
    register_gif_ids(gif_ids, ui_ctx, window_weak);
}

/// Append the not-yet-registered GIF attachment ids in one message.
fn collect_unregistered_gif_ids(msg: &crate::MessageItemData, out: &mut Vec<String>) {
    use slint::Model;
    for att_idx in 0..msg.attachments.row_count() {
        if let Some(att) = msg.attachments.row_data(att_idx) {
            let id_str = att.id.as_str();
            // QUICK CHECK: already registered? skip the string conversions.
            if INLINE_GIFS.with(|gifs| gifs.borrow().contains_key(id_str)) {
                continue;
            }
            let mime = att.mime_type.to_string();
            let filename = att.filename.to_string();
            // GIFs are real attachments (uploaded to our CDN).
            if mime == "image/gif" || filename.to_lowercase().ends_with(".gif") {
                out.push(id_str.to_string());
            }
        }
    }
}

fn register_gif_ids(
    gif_ids: Vec<String>,
    ui_ctx: std::sync::Arc<chatter_core::client::ChatterClient>,
    window_weak: Weak<MainWindow>,
) {
    if gif_ids.is_empty() {
        return;
    }
    tracing::debug!("Auto-registering {} GIF attachments", gif_ids.len());
    for att_id in gif_ids {
        register_gif_attachment(att_id, ui_ctx.clone(), window_weak.clone());
    }
}

/// Register GIFs from a single message — the hot receive/send path. Avoids
/// rescanning the whole model on every added message (older messages' GIFs are
/// already registered from their own receive/open).
pub fn auto_register_gifs_in_message(
    window_weak: Weak<MainWindow>,
    ui_ctx: std::sync::Arc<chatter_core::client::ChatterClient>,
    msg: &crate::MessageItemData,
) {
    let mut gif_ids = Vec::new();
    collect_unregistered_gif_ids(msg, &mut gif_ids);
    register_gif_ids(gif_ids, ui_ctx, window_weak);
}

/// Register a single GIF attachment for inline animation.
///
/// This is called after sending a message with GIF attachments.
/// Queues decryption/decode on the bounded GIF pool, then registers on the UI thread.
///
/// This function returns immediately - all work happens in background.
/// Call on the UI thread; this does not enter or block an async runtime.
pub fn register_gif_attachment(
    attachment_id: String,
    ui_ctx: std::sync::Arc<chatter_core::client::ChatterClient>,
    window_weak: slint::Weak<crate::MainWindow>,
) {
    let owner = crate::bin_ui::features::history_owner::HistoryOwner::capture(
        &ui_ctx,
        ui_ctx.active_conversation(),
    );
    register_gif_attachment_guarded(attachment_id, ui_ctx, window_weak, owner);
}

pub(crate) fn register_gif_attachment_guarded(
    attachment_id: String,
    ui_ctx: std::sync::Arc<chatter_core::client::ChatterClient>,
    window_weak: slint::Weak<crate::MainWindow>,
    owner: crate::bin_ui::features::history_owner::HistoryOwner,
) {
    INLINE_CLIENT.with(|c| *c.borrow_mut() = Some(std::sync::Arc::downgrade(&ui_ctx)));
    start_inline_timer_if_needed();
    // Optimization: Check if already registered (O(1) in UI thread)
    let already_registered = INLINE_GIFS.with(|gifs| gifs.borrow().contains_key(&attachment_id));
    if already_registered {
        return;
    }

    let generation = crate::bin_ui::decode_cache::thumbnail_generation();
    let visible = window_weak.upgrade().is_some_and(|w| {
        let messages = w.global::<crate::ChatPane>().get_messages();
        rows_to_animate(
            w.global::<crate::ChatPane>()
                .get_message_first_visible_row(),
            w.global::<crate::ChatPane>().get_message_last_visible_row(),
            messages.row_count(),
        )
        .is_some_and(|rows| {
            rows.into_iter().any(|row| {
                messages.row_data(row).is_some_and(|msg| {
                    (0..msg.attachments.row_count()).any(|i| {
                        msg.attachments
                            .row_data(i)
                            .is_some_and(|att| att.id.as_str() == attachment_id)
                    })
                })
            })
        })
    });
    chatter_core::media::gif::workers::submit(
        format!("inline:{attachment_id}:{generation}"),
        visible,
        move || {
            if !owner.is_current()
                || generation != crate::bin_ui::decode_cache::thumbnail_generation()
            {
                return;
            }
            use chatter_core::domain::attachment::ContentLocation;

            // Decoded on an earlier open: no decrypt, no decode.
            if let Some(cached) = crate::bin_ui::decode_cache::gif(&attachment_id) {
                let _ = slint::invoke_from_event_loop(move || {
                    if !owner.is_current()
                        || generation != crate::bin_ui::decode_cache::thumbnail_generation()
                    {
                        return;
                    }
                    let first = register_inline_gif_from_buffers(
                        &attachment_id,
                        cached.frames.to_vec(),
                        cached.delays.to_vec(),
                    );
                    if let (Some(first), Some(w)) = (first, window_weak.upgrade()) {
                        set_attachment_thumbnail(&w, &attachment_id, first);
                    }
                });
                return;
            }

            // Parse attachment ID
            let att_id: chatter_core::domain::ids::AttachmentId = match attachment_id.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::debug!(
                        "Invalid attachment ID for GIF registration: {}",
                        attachment_id
                    );
                    return;
                }
            };

            // Check if attachment has content on filesystem
            let attachment = match ui_ctx.get_attachment_metadata(&att_id) {
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

            // Full HMAC authentication precedes all decoding. Pull bounded plaintext
            // ranges; never retain or clone the complete decrypted original.
            let range = match ui_ctx.open_attachment_range(&att_id) {
                Ok(range) => std::sync::Arc::new(range),
                Err(_) => return,
            };
            if !owner.is_current() {
                return;
            }
            use image::AnimationDecoder;
            let reader = std::io::BufReader::new(
                chatter_core::services::attachment::CbcRangeReader::new(range),
            );
            let decoder = match chatter_core::media::gif::bounded_gif_decoder(reader) {
                Ok(decoder) => decoder,
                Err(_) => return,
            };
            let frames_iter = decoder.into_frames();
            // Keep decoding bounded in memory until the retained set is final.
            // Uploaded batches cannot be thinned after a later frame raises the stride.
            let mut selected = chatter_core::media::gif::DecimatedGif::default();
            const MAX_FRAMES: usize = 100;

            for (_idx, frame_result) in frames_iter.enumerate() {
                if !owner.is_current()
                    || generation != crate::bin_ui::decode_cache::thumbnail_generation()
                {
                    return;
                }
                let frame = match frame_result {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!("GIF frame {} error for {}: {}", _idx, attachment_id, e);
                        continue;
                    }
                };

                let delay = frame.delay();
                let (numer, denom) = delay.numer_denom_ms();
                let delay_ms = if denom == 0 {
                    100
                } else {
                    (numer as u64) / (denom as u64)
                };
                let delay_ms = chatter_core::media::gif::animator::normalize_delay_ms(delay_ms);

                let buffer = frame.into_buffer();
                let (width, height) = buffer.dimensions();
                let (raw_data, width, height) = super::fit_frame(
                    buffer.into_raw(),
                    width,
                    height,
                    INLINE_FRAME_PX,
                    INLINE_FRAME_PX,
                );

                if raw_data.is_empty() {
                    continue;
                }
                let bytes = raw_data.len();
                let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &raw_data, width, height,
                );
                if _idx == 0 {
                    let first = pixel_buffer.clone();
                    let weak = window_weak.clone();
                    let id = attachment_id.clone();
                    let first_owner = owner.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if !first_owner.is_current()
                            || generation != crate::bin_ui::decode_cache::thumbnail_generation()
                        {
                            return;
                        }
                        if let Some(w) = weak.upgrade() {
                            set_attachment_thumbnail(&w, &id, Image::from_rgba8(first));
                        }
                    });
                }
                selected = chatter_core::media::gif::decimate_gif_frame(
                    selected,
                    pixel_buffer,
                    delay_ms,
                    bytes,
                    MAX_FRAMES,
                    INLINE_ANIMATION_BYTES,
                );
            }

            if selected.frames.len() > 1 {
                let (all_buffers, all_delays): (Vec<_>, Vec<_>) = selected
                    .frames
                    .into_iter()
                    .map(|sample| (sample.frame, sample.delay_ms))
                    .unzip();
                if !owner.is_current() {
                    return;
                }
                crate::bin_ui::decode_cache::put_gif_guarded(
                    &attachment_id,
                    generation,
                    all_buffers.clone(),
                    all_delays.clone(),
                );
                let att_id_full = attachment_id.clone();
                let weak = window_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if !owner.is_current()
                        || generation != crate::bin_ui::decode_cache::thumbnail_generation()
                    {
                        return;
                    }
                    if let Some(first_image) =
                        register_inline_gif_from_buffers(&att_id_full, all_buffers, all_delays)
                    {
                        if let Some(w) = weak.upgrade() {
                            set_attachment_thumbnail(&w, &att_id_full, first_image);
                        }
                    }
                });
            }
        },
    );
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

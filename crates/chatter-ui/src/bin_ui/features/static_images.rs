//! Viewport observation and pixel publication only; loading lives in core.
use crate::{ChatPane, MainWindow};
use chatter_core::media::performance_trace as trace;
use chatter_core::{client::ChatterClient, media::static_preview::StaticPreviewLoader};
use slint::{ComponentHandle, Model, Timer, TimerMode};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
thread_local! { static TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) }; }

pub fn wire(window: &MainWindow, client: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let client = client.clone();
    let mut loader = Arc::new(StaticPreviewLoader::default());
    let mut revision = client.session_revision();
    let mut pending: HashMap<String, tokio_util::sync::CancellationToken> = HashMap::new();
    let mut retry: HashMap<String, std::time::Instant> = HashMap::new();
    let mut settled: Option<(slint::ModelRc<crate::MessageItemData>, usize, usize, u64)> = None;
    // Previous first-visible row, for scroll-direction-aware prewarm.
    let mut last_first: usize = usize::MAX;
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(80), move || {
        let Some(window) = weak.upgrade() else { return };
        if client.session_revision() != revision || !client.is_app_visible() || !loader.is_current()
        {
            let _reset = trace::section(c"chatter.media.static.ui_reset");
            for token in pending.values() {
                token.cancel();
            }
            pending.clear();
            retry.clear();
            settled = None;
            loader = Arc::new(StaticPreviewLoader::default());
            revision = client.session_revision();
        }
        if !client.is_app_visible() {
            return;
        }
        let pane = window.global::<ChatPane>();
        let messages = pane.get_messages();
        if pane.get_message_last_visible_row() < 0 {
            let _hidden = trace::section(c"chatter.media.static.ui_no_rows");
            for token in pending.values() {
                token.cancel();
            }
            pending.clear();
            return;
        }
        let first = pane.get_message_first_visible_row().max(0) as usize;
        let last = pane.get_message_last_visible_row().max(0) as usize;
        let model_revision = messages
            .as_any()
            .downcast_ref::<crate::bin_ui::message_model::MessageModel>()
            .map(|model| model.revision.get());
        let current = model_revision.map(|revision| (messages.clone(), first, last, revision));
        if current.is_some() && current == settled {
            return;
        }
        let mut visible = HashSet::new();
        let mut candidates = Vec::new();
        // Scroll-direction-aware prewarm cushion (was a symmetric ±8). Decode a
        // generous band ahead in the travel direction plus a small trailing band,
        // so opening a conversation and scrolling hit warm RAM (~2ms) and paint on
        // the first frame instead of decoding reactively after the row is on
        // screen (the "pop-in"). Variable-height rows make indices approximate.
        // Bounded: rows beyond the band are still cancelled below, so peak decode
        // and memory stay ~band-sized. The per-album cap (album_index >= 10) and
        // the pending cap of 12 bound it further.
        // ponytail: band sizes are a heuristic; widen the lead if flings still pop.
        let (above, below) = if last_first == usize::MAX || first == last_first {
            (16usize, 16usize) // open / at rest: symmetric
        } else if first > last_first {
            (8usize, 24usize) // scrolling toward newer/higher rows: lead below
        } else {
            (24usize, 8usize) // scrolling toward older/lower rows: lead above
        };
        last_first = first;
        let last_row = messages.row_count().saturating_sub(1);
        // Viewport first so a scroll during prewarm fills the visible rows before
        // the cushion (core's Semaphore(3) takes candidates in dispatch order).
        let rows = (first..=last.min(last_row))
            .chain(first.saturating_sub(above)..first)
            .chain((last + 1)..=(last + below).min(last_row));
        for row in rows {
            let Some(message) = messages.row_data(row) else {
                continue;
            };
            for attachment in message.attachments.iter() {
                if !attachment.is_image
                    || attachment.mime_type == "image/gif"
                    || attachment.filename.to_lowercase().ends_with(".gif")
                    || (attachment.album_count >= 2 && attachment.album_index >= 10)
                    || attachment.has_thumbnail
                {
                    continue;
                }
                let id = attachment.id.to_string();
                visible.insert(id.clone());
                candidates.push(id);
            }
        }
        if candidates.is_empty() && pending.is_empty() {
            settled = current;
        }
        pending.retain(|id, token| {
            if !visible.contains(id) {
                let _cancel = trace::section(c"chatter.media.static.ui_cancel_offscreen");
                token.cancel();
                false
            } else {
                true
            }
        });
        retry.retain(|id, _| visible.contains(id));
        pending.retain(|_, token| !token.is_cancelled());
        for id in candidates {
            // Dispatch the whole visible set at once instead of 3 per 80ms tick,
            // so an album stops filling in visible waves (the "pop"). This is not
            // the decode-concurrency bound: core's StaticPreviewLoader holds a
            // Semaphore(3) (media/static_preview.rs), so at most 3 actually decode
            // at a time regardless — the extra dispatches just queue there, which
            // removes the inter-tick stall without raising peak CPU/memory.
            if pending.len() >= 12 {
                break;
            }
            if pending.contains_key(&id)
                || retry
                    .get(&id)
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
            {
                continue;
            }
            let Ok(attachment_id) = id.parse() else {
                continue;
            };
            let token = tokio_util::sync::CancellationToken::new();
            let job = trace::Job::new();
            job.event(c"chatter.media.static.demand");
            let demand = job.span(c"chatter.media.static.demand_to_publish");
            let queued = job.span(c"chatter.media.static.runtime_queue");
            pending.insert(id.clone(), token.clone());
            retry.insert(id.clone(), std::time::Instant::now());
            let generation = crate::bin_ui::decode_cache::thumbnail_generation();
            let loader = loader.clone();
            let client = client.clone();
            let weak = weak.clone();
            let owner =
                super::history_owner::HistoryOwner::capture(&client, client.active_conversation());
            crate::rt_handle().spawn(async move {
                drop(queued);
                let result = loader
                    .clone()
                    .load_traced(client.clone(), attachment_id, 640, token.clone(), job)
                    .await;
                // Mark completion even on failure; the next tick can retry sidecars
                // that arrived after the row was first displayed.
                token.cancel();
                let Some(result) = result else {
                    job.event(c"chatter.media.static.no_result");
                    return;
                };
                let pixels = {
                    let _copy = trace::section(c"chatter.media.static.pixel_copy");
                    slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &result.rgba,
                        result.width,
                        result.height,
                    )
                };
                // Warm RAM cache (ref-shared with the row below), so scrolling the
                // row off and back restores synchronously in refresh_visible_media
                // instead of re-decoding here — the re-entry "pop-in".
                crate::bin_ui::decode_cache::put_thumbnail_pixels(&id, generation, &pixels);
                let publication = job.span(c"chatter.media.static.ui_queue");
                crate::run_history_ui!(weak, owner, |window| {
                    drop(publication);
                    let _demand = demand;
                    let _publish = trace::section(c"chatter.media.static.ui_publish");
                    if !client.is_app_visible() || !loader.is_current() {
                        job.event(c"chatter.media.static.ui_invalid");
                        return;
                    }
                    let messages = window.global::<ChatPane>().get_messages();
                    let first = window
                        .global::<ChatPane>()
                        .get_message_first_visible_row()
                        .max(0) as usize;
                    let last = window
                        .global::<ChatPane>()
                        .get_message_last_visible_row()
                        .max(0) as usize;
                    for row in first.saturating_sub(8)
                        ..=(last + 8).min(messages.row_count().saturating_sub(1))
                    {
                        let Some(message) = messages.row_data(row) else {
                            continue;
                        };
                        let attachments = message.attachments;
                        for index in 0..attachments.row_count() {
                            let Some(mut attachment) = attachments.row_data(index) else {
                                continue;
                            };
                            if attachment.id.as_str() != id.as_str() {
                                continue;
                            }
                            attachment.thumbnail = slint::Image::from_rgba8(pixels.clone());
                            attachment.has_thumbnail = true;
                            // Dimensions come from stored metadata in the core
                            // mapping (converters/attachment.rs sets width/height
                            // from meta.width/height); the reserved box is final
                            // on first layout, so no post-decode write-back / reflow.
                            attachments.set_row_data(index, attachment);
                            job.event(c"chatter.media.static.published");
                        }
                    }
                });
            });
        }
        pending.retain(|_, token| !token.is_cancelled());
        trace::counter(c"chatter.media.static.ui_pending", pending.len() as i64);
    });
    TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

//! The message-list viewport: settle tracing, visible-row thumbnail
//! hydration and off-window eviction.

use std::sync::Arc;

use slint::{ComponentHandle, Model};

use chatter_core::client::ChatterClient;
use chatter_core::media::performance_trace as viewport_trace;

use crate::MainWindow;
use super::history::maintain_history_window;

// Slint invokes both phases synchronously around settle-inner, on the same UI
// thread. A fixed stack supports reentrant callbacks without heap allocation,
// including while tracing is disabled. It retains no UI/model/message data.
struct SettleTrace {
    depth: usize,
    sections: [Option<viewport_trace::Section>; 32],
}

thread_local! {
    static SETTLE_TRACE: std::cell::RefCell<SettleTrace> = const {
        std::cell::RefCell::new(SettleTrace { depth: 0, sections: [const { None }; 32] })
    };
}

pub(super) fn trace_viewport_settle(begin: bool) {
    SETTLE_TRACE.with(|state| {
        let mut state = state.borrow_mut();
        if begin {
            let index = state.depth;
            state.depth = state.depth.saturating_add(1);
            if viewport_trace::enabled() {
                if let Some(slot) = state.sections.get_mut(index) {
                    *slot = Some(viewport_trace::section(c"Chatter viewport settle"));
                } else {
                    // Never change UI behavior if the diagnostic stack fills.
                    viewport_trace::counter(
                        c"Chatter viewport settle overflow depth",
                        state.depth as i64,
                    );
                }
            }
        } else if state.depth > 0 {
            state.depth -= 1;
            let index = state.depth;
            if let Some(slot) = state.sections.get_mut(index) {
                drop(slot.take());
            }
        }
    });
}

thread_local! {
    static PENDING_VISIBLE_THUMBNAILS: std::cell::RefCell<std::collections::HashMap<String, crate::bin_ui::features::history_owner::HistoryOwner>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

thread_local! {
    // (start, end, row_count) of the media window the last eviction pass saw.
    // Lets the hot per-frame call evict only the rows that scrolled out of the
    // window, instead of rescanning the whole model every frame.
    static LAST_MEDIA_WINDOW: std::cell::Cell<Option<(usize, usize, usize)>> =
        const { std::cell::Cell::new(None) };
}

/// Row indices whose thumbnails should be dropped this pass. When the model
/// size is unchanged since the last pass, only the rows that left the window
/// are returned (`prev \ current`) — O(delta). When the size changed (history
/// prepended, a page loaded, a conversation switched) the previous indices no
/// longer map to the same rows, so a full sweep (`0..row_count` minus the
/// current window) is returned instead. Evicting an off-window thumbnail is
/// always safe — it reloads when the row is scrolled back — so a stale diff can
/// only cost an extra reload or defer one eviction, never show a wrong row.
fn media_evict_rows(
    prev: Option<(usize, usize, usize)>,
    cur_start: usize,
    cur_end: usize,
    row_count: usize,
) -> Vec<usize> {
    let in_window = |i: usize| i >= cur_start && i <= cur_end;
    match prev {
        Some((p_start, p_end, p_count)) if p_count == row_count => (p_start..=p_end)
            .filter(|&i| i < row_count && !in_window(i))
            .collect(),
        _ => (0..row_count).filter(|&i| !in_window(i)).collect(),
    }
}


pub(super) fn wire_visible_media(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_visible_media_rows(move |first, last| {
            // Emitted by MessageList::settle only when its estimated row range
            // changes. This measures the callback, not every Slint settle pass.
            let _trace = viewport_trace::section(c"Chatter viewport range callback");
            viewport_trace::counter(c"Chatter viewport first row", i64::from(first));
            viewport_trace::counter(c"Chatter viewport last row", i64::from(last));
            if let Some(w) = weak.upgrade() {
                if maintain_history_window(&w, &client, first, last) { return; }
                refresh_visible_media(&w, &client, first, last);
            }
        });
}

// Keep one small scroll cushion so a thumbnail is ready before its row enters
// the viewport. Indices change when older history is prepended.
fn media_window(first: i32, last: i32, row_count: usize) -> Option<std::ops::RangeInclusive<usize>> {
    if row_count == 0 || last < first || last < 0 {
        return None;
    }
    let start = (first.max(0) as usize).saturating_sub(8).min(row_count - 1);
    let end = (last as usize).saturating_add(8).min(row_count - 1);
    Some(start..=end)
}

#[cfg(test)]
mod media_window_tests {
    use super::{media_evict_rows, media_window};

    #[test]
    fn scroll_cushion_clamps_at_model_edges_and_rejects_empty_ranges() {
        assert_eq!(media_window(0, 3, 20), Some(0..=11));
        assert_eq!(media_window(17, 19, 20), Some(9..=19));
        assert_eq!(media_window(0, -1, 20), None);
        assert_eq!(media_window(0, 0, 0), None);
    }

    #[test]
    fn evict_full_sweep_on_first_pass_and_size_change() {
        // First pass (no previous window): every off-window row.
        assert_eq!(media_evict_rows(None, 2, 5, 10), vec![0, 1, 6, 7, 8, 9]);
        // Model grew (history prepended): previous indices are stale, so sweep.
        assert_eq!(
            media_evict_rows(Some((0, 5, 10)), 0, 5, 12),
            vec![6, 7, 8, 9, 10, 11]
        );
    }

    #[test]
    fn evict_only_rows_that_left_the_window_when_size_unchanged() {
        // Scrolled down from [0,5] to [3,8]: only 0,1,2 left the window.
        assert_eq!(media_evict_rows(Some((0, 5, 10)), 3, 8, 10), vec![0, 1, 2]);
        // No movement: nothing to evict.
        assert!(media_evict_rows(Some((3, 8, 10)), 3, 8, 10).is_empty());
    }
}

/// Keep only viewport thumbnails plus a small scroll cushion in the model.
/// Older pages can have a downloaded original but no sidecar thumbnail in
/// their metadata; opening the viewer also refreshes those visible rows.
pub(crate) fn refresh_visible_media(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    first: i32,
    last: i32,
) {
    // Synchronous selection/eviction/scheduling only; no guard enters the
    // spawned future or its eventual event-loop publication callback.
    let _trace = viewport_trace::section(c"Chatter refresh_visible_media");
    crate::bin_ui::gif_state::auto_register_gifs_from_messages(window.as_weak(), ui_ctx.clone());
    if window.global::<crate::ChatPane>().get_loading_messages() || last < first || last < 0 {
        return;
    }
    let conversation = ui_ctx.active_conversation();
    if conversation.channel_id().is_none() {
        return;
    }
    let owner = crate::bin_ui::features::history_owner::HistoryOwner::capture(ui_ctx, conversation);
    PENDING_VISIBLE_THUMBNAILS.with(|pending| {
        pending
            .borrow_mut()
            .retain(|_, request| request.is_current())
    });
    let model = window.global::<crate::ChatPane>().get_messages();
    let row_count = model.row_count();
    let Some(window_rows) = media_window(first, last, row_count) else {
        return;
    };
    let tracing = viewport_trace::enabled();
    viewport_trace::counter(c"Chatter viewport media model rows", row_count as i64);
    viewport_trace::counter(
        c"Chatter viewport media window begin",
        *window_rows.start() as i64,
    );
    viewport_trace::counter(
        c"Chatter viewport media window end",
        *window_rows.end() as i64,
    );
    // Slint rows own the decoded pixel buffer. Drop it as soon as a row is
    // outside the cushion; the bounded decode cache can then evict it. Only the
    // rows that left the window since the last pass are visited (O(delta)) — the
    // old loop rescanned the whole model on every scroll frame, which is what
    // made a long conversation scroll janky.
    let prev = LAST_MEDIA_WINDOW.with(|w| w.get());
    let mut cleared = 0usize;
    for index in media_evict_rows(prev, *window_rows.start(), *window_rows.end(), row_count) {
        let Some(row) = model.row_data(index) else {
            continue;
        };
        if let Some(attachments) = row
            .attachments
            .as_any()
            .downcast_ref::<slint::VecModel<crate::MessageAttachmentData>>()
        {
            for attachment_index in 0..attachments.row_count() {
                let Some(mut attachment) = attachments.row_data(attachment_index) else {
                    continue;
                };
                if attachment.has_thumbnail {
                    let _clear = viewport_trace::section(c"Chatter viewport thumbnail clear");
                    viewport_trace::counter(c"Chatter viewport media row", index as i64);
                    viewport_trace::counter(
                        c"Chatter viewport media attachment index",
                        attachment_index as i64,
                    );
                    attachment.thumbnail = slint::Image::default();
                    attachment.has_thumbnail = false;
                    attachments.set_row_data(attachment_index, attachment);
                    if tracing {
                        cleared += 1;
                    }
                }
            }
        }
    }
    viewport_trace::counter(c"Chatter viewport thumbnails cleared", cleared as i64);
    LAST_MEDIA_WINDOW.with(|w| w.set(Some((*window_rows.start(), *window_rows.end(), row_count))));

    // Scroll-back restore: a static-image row whose thumbnail was evicted paints
    // from the warm RAM cache THIS frame, instead of waiting for the 80ms
    // static-image poll to re-decode it (the re-entry "pop-in"). Synchronous
    // RAM lookup only, no IO/decode; a miss falls through to that loader as
    // before. GIFs/video are handled by the hydrate loop below.
    let mut restored = 0usize;
    for index in window_rows.clone() {
        let Some(row) = model.row_data(index) else {
            continue;
        };
        let Some(attachments) = row
            .attachments
            .as_any()
            .downcast_ref::<slint::VecModel<crate::MessageAttachmentData>>()
        else {
            continue;
        };
        for attachment_index in 0..attachments.row_count() {
            let Some(mut attachment) = attachments.row_data(attachment_index) else {
                continue;
            };
            if attachment.has_thumbnail
                || !attachment.is_image
                || attachment.mime_type == "image/gif"
                || attachment.filename.to_lowercase().ends_with(".gif")
            {
                continue;
            }
            if let Some(pixels) =
                crate::bin_ui::decode_cache::cached_thumbnail(&attachment.id.to_string(), 0)
            {
                attachment.thumbnail = slint::Image::from_rgba8(pixels);
                attachment.has_thumbnail = true;
                attachments.set_row_data(attachment_index, attachment);
                if tracing {
                    restored += 1;
                }
            }
        }
    }
    viewport_trace::counter(c"Chatter viewport thumbnails restored", restored as i64);

    let mut ids = Vec::new();
    for index in window_rows {
        let Some(row) = model.row_data(index) else {
            continue;
        };
        for attachment in row.attachments.iter() {
            // The grid paints at most ten media tiles. Hidden overflow remains
            // available to the viewer but needs no inline thumbnail texture.
            if (attachment.is_image || attachment.is_video)
                // Static images use the bounded, cancellable core preview loader.
                && (!attachment.is_image || attachment.mime_type == "image/gif"
                    || attachment.filename.to_lowercase().ends_with(".gif"))
                && (attachment.album_count < 2 || attachment.album_index < 10)
                && !attachment.has_thumbnail
            {
                let id = attachment.id.to_string();
                let inserted = PENDING_VISIBLE_THUMBNAILS.with(|pending| {
                    let mut pending = pending.borrow_mut();
                    if pending.contains_key(&id) {
                        false
                    } else {
                        pending.insert(id.clone(), owner.clone());
                        true
                    }
                });
                if inserted {
                    ids.push(id);
                }
            }
        }
    }

    viewport_trace::counter(c"Chatter viewport hydrate demands", ids.len() as i64);
    for id in ids {
        let weak = window.as_weak();
        let client = ui_ctx.clone();
        let owner = owner.clone();
        let session = ui_ctx.session_guard();
        let generation = crate::bin_ui::decode_cache::thumbnail_generation();
        let job = viewport_trace::Job::new();
        let lifetime = job.span(c"Chatter viewport hydrate lifetime");
        let queue = job.span(c"Chatter viewport hydrate queue");
        crate::rt_handle().spawn(viewport_trace::scope(job, async move {
            drop(queue);
            // AsyncSection is safe across awaits; scope installs CurrentJob
            // only for each synchronous poll and restores it on Pending.
            let prepare = job.span(c"Chatter viewport hydrate prepare");
            let (pixels, dimensions) =
                match hydrate_visible_media(&client, &id, session, &owner, generation).await {
                    Some((pixels, dimensions)) => (Some(pixels), dimensions),
                    None => (None, None),
                };
            drop(prepare);
            let publication_queue = job.span(c"Chatter viewport hydrate UI queue");
            let scheduled = slint::invoke_from_event_loop(move || {
                let _lifetime = lifetime;
                drop(publication_queue);
                let _current = job.enter();
                let _publish = viewport_trace::section(c"Chatter viewport hydrate publish");
                PENDING_VISIBLE_THUMBNAILS.with(|pending| {
                    let mut pending = pending.borrow_mut();
                    if owner.is_current() {
                        pending.remove(&id);
                    }
                });
                let Some(w) = weak.upgrade() else {
                    job.event(c"Chatter viewport hydrate no window");
                    return;
                };
                if !owner.is_current() {
                    job.event(c"Chatter viewport hydrate stale");
                    return;
                }
                let Some(pixels) = pixels else {
                    job.event(c"Chatter viewport hydrate no pixels");
                    return;
                };
                let model = w.global::<crate::ChatPane>().get_messages();
                let Some(window_rows) = media_window(
                    w.global::<crate::ChatPane>()
                        .get_message_first_visible_row(),
                    w.global::<crate::ChatPane>().get_message_last_visible_row(),
                    model.row_count(),
                ) else {
                    job.event(c"Chatter viewport hydrate no range");
                    return;
                };
                let tracing = viewport_trace::enabled();
                let mut assigned = 0usize;
                for row_index in window_rows {
                    let Some(row) = model.row_data(row_index) else {
                        continue;
                    };
                    for attachment_index in 0..row.attachments.row_count() {
                        let Some(mut attachment) = row.attachments.row_data(attachment_index)
                        else {
                            continue;
                        };
                        if attachment.id.as_str() == id && !attachment.has_thumbnail {
                            let _assign = viewport_trace::section(c"Chatter viewport thumbnail assign");
                            viewport_trace::counter(c"Chatter viewport media row", row_index as i64);
                            viewport_trace::counter(
                                c"Chatter viewport media attachment index",
                                attachment_index as i64,
                            );
                            if let Some((width, height)) = dimensions {
                                attachment.width = width as i32;
                                attachment.height = height as i32;
                            }
                            attachment.thumbnail = slint::Image::from_rgba8(pixels.clone());
                            attachment.has_thumbnail = true;
                            if let Some(attachments) = row
                                .attachments
                                .as_any()
                                .downcast_ref::<slint::VecModel<crate::MessageAttachmentData>>()
                            {
                                attachments.set_row_data(attachment_index, attachment);
                                if tracing {
                                    assigned += 1;
                                }
                            }
                        }
                    }
                }
                viewport_trace::counter(c"Chatter viewport hydrate assignments", assigned as i64);
                if tracing {
                    job.event(if assigned > 0 {
                        c"Chatter viewport hydrate published"
                    } else {
                        c"Chatter viewport hydrate offscreen"
                    });
                }
            });
            if scheduled.is_err() {
                job.event(c"Chatter viewport hydrate UI unavailable");
            }
        }));
    }
}

/// Core owns routing/authentication/bounded decoding for every viewport miss.
async fn hydrate_visible_media(
    client: &Arc<ChatterClient>,
    id: &str,
    session: chatter_core::client::SessionGuard,
    owner: &crate::bin_ui::features::history_owner::HistoryOwner,
    generation: u64,
) -> Option<(crate::bin_ui::decode_cache::Pixels, Option<(u32, u32)>)> {
    if !owner.is_current() || !session.is_current() {
        return None;
    }
    if let Some(pixels) = crate::bin_ui::decode_cache::cached_thumbnail(id, 0) {
        return Some((pixels, None));
    }
    let preview = client
        .prepare_visible_attachment_preview(id.parse().ok()?, session)
        .await?;
    if !owner.is_current() {
        return None;
    }
    let dimensions = Some((preview.width, preview.height));
    let pixels = crate::bin_ui::decode_cache::prepared_thumbnail(
        id,
        generation,
        image::DynamicImage::ImageRgba8(preview.pixels),
    )?;
    Some((pixels, dimensions))
}

#[cfg(test)]
mod visible_preview_tests {
    use super::*;
    use chatter_core::state::ActiveConversation;

    #[test]
    fn visible_hydration_is_small_durable_and_cannot_adopt_a_late_view_or_session() {
        const CHILD: &str = "CHATTER_VISIBLE_IMAGE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let test = format!(
                "{}::visible_hydration_is_small_durable_and_cannot_adopt_a_late_view_or_session",
                module_path!().split_once("::").unwrap().1
            );
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _runtime = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let database = chatter_core::system::Directories::with_base_dir(dir.path().to_path_buf())
            .unwrap()
            .database_path();
        std::fs::write(
            database
                .parent()
                .unwrap()
                .join(chatter_core::system::keyring::NO_KEYRING_MARKER),
            b"",
        )
        .unwrap();
        let client = ChatterClient::builder()
            .data_dir(dir.path().to_string_lossy().to_string())
            .build()
            .unwrap();
        let a = ActiveConversation::Channel(chatter_core::domain::ids::ChannelId::new());
        let b = ActiveConversation::Channel(chatter_core::domain::ids::ChannelId::new());
        client.select_conversation(a);
        let mut plaintext = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2048, 1024)
            .write_to(&mut plaintext, image::ImageFormat::Jpeg)
            .unwrap();
        let encrypted =
            chatter_attachment_crypto::AttachmentCrypto::encrypt(plaintext.get_ref()).unwrap();
        let source = chatter_core::domain::attachment::Attachment {
            id: chatter_core::domain::ids::AttachmentId::new(),
            metadata: chatter_core::domain::attachment::FileMetadata::new(
                "visible.jpg".into(),
                "image/jpeg".into(),
                plaintext.get_ref().len() as u64,
            ),
            status: chatter_core::domain::attachment::AttachmentStatus::Ready,
            content_location: chatter_core::domain::attachment::ContentLocation::OnFilesystem,
            remote_url: Some("owned-visible-image".into()),
            encryption_key: Some(encrypted.key.to_vec()),
            encryption_iv: Some(encrypted.digest.to_vec()),
            created_at: chrono::Utc::now(),
            secret_chat: false,
        };
        client
            .attachments()
            .attachment_repo()
            .insert(&source)
            .unwrap();
        chatter_core::services::attachment::ensure_attachments_dir().unwrap();
        let path = chatter_core::services::attachment::attachment_file_path(&source.id);
        std::fs::write(&path, &encrypted.ciphertext).unwrap();
        let id = source.id.to_string();
        // Construct requests before the executor gets a chance to poll them.
        // Neither an A->B->A selection nor a changed account may be adopted.
        for logout in [false, true] {
            let owner = crate::bin_ui::features::history_owner::HistoryOwner::begin(&client, a);
            let pending = hydrate_visible_media(
                &client,
                &id,
                client.session_guard(),
                &owner,
                crate::bin_ui::decode_cache::thumbnail_generation(),
            );
            if logout {
                client.testing_invalidate_session();
            } else {
                client.select_conversation(b);
                crate::bin_ui::features::history_owner::HistoryOwner::begin(&client, b);
                client.select_conversation(a);
                crate::bin_ui::features::history_owner::HistoryOwner::begin(&client, a);
            }
            assert!(runtime.block_on(pending).is_none());
            assert!(
                client
                    .get_attachment_metadata(&source.id)
                    .unwrap()
                    .metadata
                    .thumbnail
                    .is_none()
            );
        }
        let owner = crate::bin_ui::features::history_owner::HistoryOwner::begin(&client, a);
        let (pixels, dimensions) = runtime
            .block_on(hydrate_visible_media(
                &client,
                &id,
                client.session_guard(),
                &owner,
                crate::bin_ui::decode_cache::thumbnail_generation(),
            ))
            .unwrap();
        assert_eq!((pixels.width(), pixels.height()), (320, 160));
        assert_eq!(dimensions, Some((2048, 1024)));
        assert_eq!(std::fs::read(&path).unwrap(), encrypted.ciphertext);
        // Simulate eviction/reopen: the authenticated original must not be
        // needed again, even after the decoded-memory cache has been wiped.
        std::fs::remove_file(&path).unwrap();
        crate::bin_ui::decode_cache::on_lock();
        let (pixels, dimensions) = runtime
            .block_on(hydrate_visible_media(
                &client,
                &id,
                client.session_guard(),
                &owner,
                crate::bin_ui::decode_cache::thumbnail_generation(),
            ))
            .unwrap();
        assert_eq!((pixels.width(), pixels.height()), (320, 160));
        assert_eq!(dimensions, Some((2048, 1024)));
    }
}

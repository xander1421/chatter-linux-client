//! GIF picker for the composer.

use std::rc::Rc;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::{ChatterClient, ComposerSend};
use chatter_core::state::PendingAttachment;

use crate::{GifData, MainWindow};

pub(crate) fn wire_gif_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    window.on_composer_gif_closed(crate::bin_ui::gif_state::clear_picker_gifs);
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_gif_clicked(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        tracing::debug!("GIF picker clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if w.get_composer_gif_picker_visible() {
                    // Allocate here, before launching the request, rather than
                    // relying on a deferred Slint visibility change handler.
                    w.set_composer_gif_generation(w.get_composer_gif_generation() + 1);
                    w.set_composer_gif_picker_search("".into());
                    let generation = w.get_composer_gif_generation();
                    w.set_composer_gif_picker_loading(true);
                    w.set_composer_gif_picker_failed(false);

                    let weak_load = weak.clone();
                    let ui_ctx_load = ui_ctx.clone();
                    tokio::spawn(async move {
                        let raw_gifs = fetch_trending_gifs_raw(&ui_ctx_load).await;
                        load_gif_previews(raw_gifs, weak_load, ui_ctx_load, generation);
                    });
                } else {
                    // Stop animations when picker is hidden
                    crate::bin_ui::gif_state::clear_picker_gifs();
                }
            }
        });
    });
}

pub(crate) fn wire_gif_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_composer_gif_selected(move |gif_id: SharedString, gif_url: SharedString| {
        let Some(w) = weak_window.upgrade() else { return };
        let id = gif_id.to_string();
        let url = gif_url.to_string();
        tracing::debug!("GIF selected: id={}, url={}", id, url);

        // Hide picker immediately for responsiveness
        w.set_composer_gif_picker_visible(false);
        w.invoke_release_text_focus();
        crate::bin_ui::gif_state::clear_picker_gifs();

        let Some(send) = begin_gif_send(&w, &ui_ctx) else { return };
        let (weak, ctx) = (w.as_weak(), ui_ctx.clone());
        tokio::spawn(async move {
            let upload = upload_gif(ctx.clone(), url, id);
            if let Some(failed) = finish_gif_send(weak.clone(), ctx.clone(), send, upload).await {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        recover_gif_send(&w, &ctx, failed);
                    }
                });
            }
        });
    });
}

/// Freeze the composer for a GIF send, at the moment the GIF is picked: the
/// conversation, the caption, the reply and whatever is staged go with it,
/// and leave the composer as Send leaves it. `None` (told to the user, nothing
/// consumed) where it cannot be sent.
pub fn begin_gif_send(w: &MainWindow, ui_ctx: &ChatterClient) -> Option<ComposerSend> {
    match crate::bin_ui::features::chat::send_message::take_composer_send(w, ui_ctx) {
        Ok(send) => Some(send),
        Err(message) => {
            toast(w, &message);
            None
        }
    }
}

/// Download the GIF, encrypt it and upload it to our CDN, as an attachment
/// that is already uploaded.
async fn upload_gif(ui_ctx: Arc<ChatterClient>, url: String, gif_id: String) -> Result<PendingAttachment, String> {
    let upload = ui_ctx.download_and_upload_gif(&url, &gif_id).await.map_err(|e| e.to_string())?;
    tracing::info!(
        "GIF uploaded to CDN: {} -> {} (encrypted on filesystem)",
        gif_id,
        upload.attachment_id
    );
    Ok(PendingAttachment::with_pointer(
        upload.attachment_id,
        std::path::PathBuf::new(), // No temp file - already uploaded
        upload.file_name.clone(),
        upload.plaintext_size,
        "image/gif".to_string(),
        upload.to_attachment_pointer(),
    ))
}

/// Complete a GIF send begun by [`begin_gif_send`] once `upload` resolves: the
/// frozen send plus the GIF, to the conversation it was picked in, whatever
/// is open or typed by then. `Some` is the send handed back unsent — with the
/// GIF if the upload got that far — for [`recover_gif_send`].
pub async fn finish_gif_send(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    mut send: ComposerSend,
    upload: impl std::future::Future<Output = Result<PendingAttachment, String>>,
) -> Option<ComposerSend> {
    match upload.await {
        Ok(gif) => send.attachments.push(gif),
        Err(error) => {
            tracing::error!(%error, "GIF upload failed");
            return Some(send);
        }
    }
    let started = std::time::Instant::now();
    if crate::bin_ui::features::chat::send_message::deliver(weak, ui_ctx, send.clone(), started).await {
        return None;
    }
    Some(send)
}

/// A GIF send that did not go: back into its own composer if that is open
/// and empty, otherwise kept by core until it is (never over newer input).
pub fn recover_gif_send(w: &MainWindow, ui_ctx: &ChatterClient, send: ComposerSend) {
    use crate::bin_ui::features::chat::send_message::{composer_is_empty, restore_composer};
    match ui_ctx.recover_composer_send(send, composer_is_empty(w)) {
        Some(send) => {
            restore_composer(w, ui_ctx, send);
            toast(w, "The GIF was not sent. Your message is back in the composer.");
        }
        None => toast(
            w,
            "The GIF was not sent. Your message is kept: open its chat again with an empty composer to get it back.",
        ),
    }
}

fn toast(w: &MainWindow, message: &str) {
    w.set_toast_message(message.into());
    w.set_toast_variant("error".into());
    w.set_toast_visible(true);
}

pub(crate) fn wire_gif_search(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_composer_gif_search_changed(move |query: SharedString| {
        let weak = weak_window.clone();
        let query_str = query.to_string();
        let ui_ctx = ui_ctx.clone();
        tracing::debug!("GIF search requested");
        let Some(w) = weak.upgrade() else { return };
        w.set_composer_gif_generation(w.get_composer_gif_generation() + 1);
        let generation = w.get_composer_gif_generation();

        if let Some(w) = weak.upgrade() {
            w.set_composer_gif_picker_loading(true);
            w.set_composer_gif_picker_failed(false);
            crate::bin_ui::gif_state::clear_picker_gifs();
        }

        tokio::spawn(async move {
            let raw_gifs = if query_str.is_empty() {
                fetch_trending_gifs_raw(&ui_ctx).await
            } else {
                search_gifs_raw(&query_str, &ui_ctx).await
            };
            load_gif_previews(raw_gifs, weak, ui_ctx, generation);
        });
    });
}

type RawGifData = (String, String, String, u32, u32);

/// Raw decoded frames (Send-safe).
struct RawGifFrames {
    id: String,
    frames: Vec<(Vec<u8>, u32, u32, u64)>,
}

async fn fetch_full_gif_frames(
    id: String,
    url: &str,
    ui_ctx: &std::sync::Arc<ChatterClient>,
) -> Option<RawGifFrames> {
    let bytes = match ui_ctx.fetch_gif_media(url).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(gif_id = %id, error = %e, "GIF preview fetch failed");
            return None;
        }
    };

    use image::AnimationDecoder;
    use image::codecs::gif::GifDecoder;
    use std::io::Cursor;

    let cursor = Cursor::new(&bytes);
    let decoder = match GifDecoder::new(cursor) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(gif_id = %id, error = %e, "GIF preview decode failed");
            return None;
        }
    };

    let frames_iter = decoder.into_frames();
    let mut frames = Vec::new();

    for frame_result in frames_iter {
        let frame = match frame_result {
            Ok(f) => f,
            _ => continue,
        };

        let delay = frame.delay();
        let (numer, denom) = delay.numer_denom_ms();
        let delay_ms = if denom == 0 {
            100
        } else {
            (numer as u64) / (denom as u64)
        };
        let delay_ms = delay_ms.max(20);

        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        let raw_data = buffer.into_raw();

        frames.push((raw_data, width, height, delay_ms));

        // Limit frames for picker performance
        if frames.len() >= 30 {
            break;
        }
    }

    if frames.is_empty() {
        return None;
    }

    Some(RawGifFrames { id, frames })
}

fn load_gif_previews(
    raw_gifs: Result<Vec<RawGifData>, chatter_core::error::NetworkError>,
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: std::sync::Arc<ChatterClient>,
    generation: i32,
) {
    let raw_gifs = match raw_gifs {
        Ok(gifs) => gifs,
        Err(error) => {
            tracing::warn!(%error, "GIF request failed");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_window.upgrade() {
                    if !w.get_composer_gif_picker_visible()
                        || w.get_composer_gif_generation() != generation
                    {
                        return;
                    }
                    w.set_composer_gif_picker_results(ModelRc::new(VecModel::<GifData>::default()));
                    w.set_composer_gif_picker_failed(true);
                    w.set_composer_gif_picker_loading(false);
                }
            });
            return;
        }
    };
    let raw_gifs_clone: Vec<_> = raw_gifs
        .iter()
        .map(|(id, _, full_url, width, height)| (id.clone(), full_url.clone(), *width, *height))
        .collect();

    {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if !w.get_composer_gif_picker_visible()
                    || w.get_composer_gif_generation() != generation
                {
                    return;
                }
                let gifs: Vec<GifData> = raw_gifs_clone
                    .into_iter()
                    .map(|(id, full_url, width, height)| GifData {
                        id: id.into(),
                        preview: slint::Image::default(),
                        full_url: full_url.into(),
                        width: width as i32,
                        height: height as i32,
                    })
                    .collect();
                let model = Rc::new(VecModel::from(gifs));
                w.set_composer_gif_picker_results(ModelRc::from(model));
                w.set_composer_gif_picker_failed(false);
                w.set_composer_gif_picker_loading(false);
            }
        });
    }

    for (id, preview_url, _full_url, _width, _height) in raw_gifs {
        let weak = weak_window.clone();
        let gif_id = id.clone();
        let ui_ctx_fetch = ui_ctx.clone();

        tokio::spawn(async move {
            if let Some(decoded) = fetch_full_gif_frames(gif_id, &preview_url, &ui_ctx_fetch).await
            {
                let id_for_ui = decoded.id.clone();
                let first_frame_raw = decoded.frames.first().unwrap();
                let frame_bytes = first_frame_raw.0.clone();
                let width = first_frame_raw.1;
                let height = first_frame_raw.2;

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        if !w.get_composer_gif_picker_visible()
                            || w.get_composer_gif_generation() != generation
                        {
                            return;
                        }
                        let first_image = crate::bin_ui::slint_conversions::rgba_to_slint_image(
                            &frame_bytes,
                            width,
                            height,
                        );

                        let model = w.get_composer_gif_picker_results();
                        for i in 0..model.row_count() {
                            if let Some(mut gif) = model.row_data(i) {
                                if gif.id.as_str() == id_for_ui {
                                    gif.preview = first_image;
                                    model.set_row_data(i, gif);
                                    break;
                                }
                            }
                        }

                        crate::bin_ui::gif_state::register_picker_gif(decoded.id, decoded.frames);
                    }
                });
            }
        });
    }
}

async fn fetch_trending_gifs_raw(
    ui_ctx: &ChatterClient,
) -> Result<Vec<RawGifData>, chatter_core::error::NetworkError> {
    Ok(ui_ctx
        .trending_gifs(20)
        .await?
        .into_iter()
        .map(|r| {
            let width = r.dims.first().copied().unwrap_or(0) as u32;
            let height = r.dims.get(1).copied().unwrap_or(0) as u32;
            (r.id, r.preview_url, r.url, width, height)
        })
        .collect())
}

async fn search_gifs_raw(
    query: &str,
    ui_ctx: &ChatterClient,
) -> Result<Vec<RawGifData>, chatter_core::error::NetworkError> {
    Ok(ui_ctx
        .search_gifs(query, 20)
        .await?
        .into_iter()
        .map(|r| {
            let width = r.dims.first().copied().unwrap_or(0) as u32;
            let height = r.dims.get(1).copied().unwrap_or(0) as u32;
            (r.id, r.preview_url, r.url, width, height)
        })
        .collect())
}

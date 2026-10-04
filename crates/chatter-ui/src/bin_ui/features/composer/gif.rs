//! GIF picker for the composer.

use std::rc::Rc;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;

use crate::{GifData, MainWindow};

pub(crate) fn wire_gif_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_gif_clicked(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        tracing::debug!("GIF picker clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if w.get_composer_gif_picker_visible() {
                    w.set_composer_gif_picker_loading(true);
                    w.set_composer_gif_picker_failed(false);
                    
                    let weak_load = weak.clone();
                    let ui_ctx_load = ui_ctx.clone();
                    tokio::spawn(async move {
                        let raw_gifs = fetch_trending_gifs_raw(&ui_ctx_load).await;
                        load_gif_previews(raw_gifs, weak_load, ui_ctx_load);
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
        let weak = weak_window.clone();
        let id = gif_id.to_string();
        let url = gif_url.to_string();
        let ui_ctx = ui_ctx.clone();
        tracing::debug!("GIF selected: id={}, url={}", id, url);

        // Hide picker immediately for responsiveness
        if let Some(w) = weak.upgrade() {
            w.set_composer_gif_picker_visible(false);
        }
        crate::bin_ui::gif_state::clear_picker_gifs();

        // Download GIF, upload to our CDN, and send as real attachment
        tokio::spawn(async move {
            match download_and_send_gif(&url, &id, ui_ctx, weak).await {
                Ok(()) => tracing::info!("GIF sent successfully: {}", id),
                Err(e) => tracing::error!("Failed to send GIF {}: {}", id, e),
            }
        });
    });
}

/// Download GIF from external CDN, encrypt, upload to our CDN, and send.
///
/// Flow:
/// 1. Download GIF bytes from Klipy/Tenor CDN
/// 2. Encrypt and upload to our CDN (stored encrypted on filesystem)
/// 3. Add as pending attachment with pointer
/// 4. Trigger send
async fn download_and_send_gif(
    url: &str,
    gif_id: &str,
    ui_ctx: Arc<ChatterClient>,
    weak: slint::Weak<MainWindow>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Download GIF from CDN and upload to our CDN (via core façade)
    let upload_result = ui_ctx.download_and_upload_gif(url, gif_id).await?;

    tracing::info!(
        "GIF uploaded to CDN: {} -> {} (encrypted on filesystem)",
        gif_id,
        upload_result.attachment_id
    );

    // Create pending attachment with pre-uploaded pointer
    let gif_filename = upload_result.file_name.clone();
    let pending = chatter_core::state::PendingAttachment::with_pointer(
        upload_result.attachment_id,
        std::path::PathBuf::new(), // No temp file - already uploaded
        gif_filename,
        upload_result.plaintext_size,
        "image/gif".to_string(),
        upload_result.to_attachment_pointer(),
    );

    // Add to chat state and trigger send
    ui_ctx.chat_state().add_pending_attachment(pending);

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Get current text and invoke send
            let current_text = w.get_composer_text().to_string();
            w.invoke_send(current_text.into());
        }
    });

    Ok(())
}

pub(crate) fn wire_gif_search(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_composer_gif_search_changed(move |query: SharedString| {
        let weak = weak_window.clone();
        let query_str = query.to_string();
        let ui_ctx = ui_ctx.clone();
        tracing::debug!("GIF search: {}", query_str);

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
            load_gif_previews(raw_gifs, weak, ui_ctx);
        });
    });
}

type RawGifData = (String, String, String, u32, u32);

/// Raw decoded frames (Send-safe).
struct RawGifFrames {
    id: String,
    frames: Vec<(Vec<u8>, u32, u32, u64)>,
}

async fn fetch_full_gif_frames(id: String, url: &str, ui_ctx: &std::sync::Arc<ChatterClient>) -> Option<RawGifFrames> {
    let bytes = match ui_ctx.fetch_gif_media(url).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(gif_id = %id, error = %e, "GIF preview fetch failed");
            return None;
        }
    };

    use image::codecs::gif::GifDecoder;
    use image::AnimationDecoder;
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
        let delay_ms = if denom == 0 { 100 } else { (numer as u64) / (denom as u64) };
        let delay_ms = delay_ms.max(20);

        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        let raw_data = buffer.into_raw();

        frames.push((raw_data, width, height, delay_ms));
        
        // Limit frames for picker performance
        if frames.len() >= 30 { break; }
    }

    if frames.is_empty() { return None; }

    Some(RawGifFrames { id, frames })
}

fn load_gif_previews(
    raw_gifs: Result<Vec<RawGifData>, chatter_core::error::NetworkError>,
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: std::sync::Arc<ChatterClient>,
) {
    let raw_gifs = match raw_gifs {
        Ok(gifs) => gifs,
        Err(error) => {
            tracing::warn!(%error, "GIF request failed");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_window.upgrade() {
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
        .map(|(id, _, full_url, width, height)| {
            (id.clone(), full_url.clone(), *width, *height)
        })
        .collect();

    {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
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
            if let Some(decoded) = fetch_full_gif_frames(gif_id, &preview_url, &ui_ctx_fetch).await {
                let id_for_ui = decoded.id.clone();
                let first_frame_raw = decoded.frames.first().unwrap();
                let frame_bytes = first_frame_raw.0.clone();
                let width = first_frame_raw.1;
                let height = first_frame_raw.2;

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let first_image = crate::bin_ui::slint_conversions::rgba_to_slint_image(&frame_bytes,
                            width,
                            height,);

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
    Ok(ui_ctx.trending_gifs(20).await?
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
    Ok(ui_ctx.search_gifs(query, 20).await?
        .into_iter()
        .map(|r| {
            let width = r.dims.first().copied().unwrap_or(0) as u32;
            let height = r.dims.get(1).copied().unwrap_or(0) as u32;
            (r.id, r.preview_url, r.url, width, height)
        })
        .collect())
}

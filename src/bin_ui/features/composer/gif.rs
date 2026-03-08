//! GIF picker for the composer.

use std::rc::Rc;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::UIContext;

use crate::{GifData, MainWindow};
use chatter_linux::mailbox::blind_signature::UnblindedToken;

pub(crate) fn wire_gif_clicked(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
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
                    
                    let weak_load = weak.clone();
                    let ui_ctx_load = ui_ctx.clone();
                    tokio::spawn(async move {
                        let raw_gifs = fetch_trending_gifs_raw(&ui_ctx_load).await;
                        load_gif_previews(raw_gifs, weak_load);
                    });
                } else {
                    // Stop animations when picker is hidden
                    crate::bin_ui::gif_state::clear_picker_gifs();
                }
            }
        });
    });
}

pub(crate) fn wire_gif_selected(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_composer_gif_selected(move |gif_id: SharedString, gif_url: SharedString| {
        let weak = weak_window.clone();
        let id = gif_id.to_string();
        let url = gif_url.to_string();
        tracing::debug!("GIF selected: id={}, url={}", id, url);

        // Before invoking the send callback, promote the picker GIF to an inline GIF.
        // We know it will become a virtual attachment with id format: virtual-gif-{url}
        let virtual_id = format!("virtual-gif-{}", url);
        crate::bin_ui::gif_state::promote_picker_gif_to_inline(&id, &virtual_id);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Get current text and add GIF marker for instant send
                let current_text = w.get_composer_text().to_string();
                let gif_marker = format!("[GIF:{}]", url);
                let new_text = if current_text.trim().is_empty() {
                    gif_marker
                } else {
                    format!("{} {}", current_text.trim(), gif_marker)
                };
                
                // Instant send: Invoke the send callback registered in send_message.rs
                w.invoke_send(new_text.into());
                
                // Hide picker and clear animations
                w.set_composer_gif_picker_visible(false);
                crate::bin_ui::gif_state::clear_picker_gifs();
            }
        });
    });
}

pub(crate) fn wire_gif_search(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_composer_gif_search_changed(move |query: SharedString| {
        let weak = weak_window.clone();
        let query_str = query.to_string();
        let ui_ctx = ui_ctx.clone();
        tracing::debug!("GIF search: {}", query_str);

        if let Some(w) = weak.upgrade() {
            w.set_composer_gif_picker_loading(true);
            crate::bin_ui::gif_state::clear_picker_gifs();
        }

        tokio::spawn(async move {
            let raw_gifs = if query_str.is_empty() {
                fetch_trending_gifs_raw(&ui_ctx).await
            } else {
                search_gifs_raw(&query_str, &ui_ctx).await
            };
            load_gif_previews(raw_gifs, weak);
        });
    });
}

type RawGifData = (String, String, String, u32, u32);

/// Raw decoded frames (Send-safe).
struct RawGifFrames {
    id: String,
    frames: Vec<(Vec<u8>, u32, u32, u64)>,
}

async fn fetch_full_gif_frames(id: String, url: &str) -> Option<RawGifFrames> {
    let client = reqwest::Client::new();
    let response = match client.get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return None,
    };

    let bytes = match response.bytes().await {
        Ok(b) => b,
        _ => return None,
    };

    use image::codecs::gif::GifDecoder;
    use image::AnimationDecoder;
    use std::io::Cursor;

    let cursor = Cursor::new(bytes.as_ref());
    let decoder = match GifDecoder::new(cursor) {
        Ok(d) => d,
        _ => return None,
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
    raw_gifs: Vec<RawGifData>,
    weak_window: slint::Weak<MainWindow>,
) {
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
                w.set_composer_gif_picker_loading(false);
            }
        });
    }

    for (id, preview_url, _full_url, _width, _height) in raw_gifs {
        let weak = weak_window.clone();
        let gif_id = id.clone();

        tokio::spawn(async move {
            if let Some(decoded) = fetch_full_gif_frames(gif_id, &preview_url).await {
                let id_for_ui = decoded.id.clone();
                let first_frame_raw = decoded.frames.first().unwrap();
                let frame_bytes = first_frame_raw.0.clone();
                let width = first_frame_raw.1;
                let height = first_frame_raw.2;

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            &frame_bytes,
                            width,
                            height,
                        );
                        let first_image = slint::Image::from_rgba8(buffer);

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

async fn fetch_trending_gifs_raw(ui_ctx: &UIContext) -> Vec<RawGifData> {
    let mut client = match ui_ctx.gif_client() {
        Some(c) => c,
        None => return Vec::new(),
    };

    let token = ui_ctx.token_manager.get_available_token().ok().flatten();
    let unblinded = token.as_ref().map(|t| UnblindedToken {
        public_key: t.public_key.clone(),
        signature: t.signature.clone(),
        created_at: 0,
    });

    let interceptor = ui_ctx.grpc_pool.auth_clone();
    let old_mode = interceptor.current_mode();
    if let Some(t) = unblinded.clone() {
        interceptor.set_anonymous(t);
    }

    let result = match client.trending(20, unblinded).await {
        Ok(results) => {
            if let Some(t) = token {
                let _ = ui_ctx.token_manager.mark_token_used(&t.id);
            }
            results
                .into_iter()
                .map(|r| {
                    let width = r.dims.first().copied().unwrap_or(0) as u32;
                    let height = r.dims.get(1).copied().unwrap_or(0) as u32;
                    (r.id, r.preview_url, r.url, width, height)
                })
                .collect()
        }
        Err(_) => Vec::new(),
    };

    interceptor.set_mode(old_mode);
    result
}

async fn search_gifs_raw(query: &str, ui_ctx: &UIContext) -> Vec<RawGifData> {
    let mut client = match ui_ctx.gif_client() {
        Some(c) => c,
        None => return Vec::new(),
    };

    let token = ui_ctx.token_manager.get_available_token().ok().flatten();
    let unblinded = token.as_ref().map(|t| UnblindedToken {
        public_key: t.public_key.clone(),
        signature: t.signature.clone(),
        created_at: 0,
    });

    let interceptor = ui_ctx.grpc_pool.auth_clone();
    let old_mode = interceptor.current_mode();
    if let Some(t) = unblinded.clone() {
        interceptor.set_anonymous(t);
    }

    let result = match client.search(query, 20, unblinded).await {
        Ok(results) => {
            if let Some(t) = token {
                let _ = ui_ctx.token_manager.mark_token_used(&t.id);
            }
            results
                .into_iter()
                .map(|r| {
                    let width = r.dims.first().copied().unwrap_or(0) as u32;
                    let height = r.dims.get(1).copied().unwrap_or(0) as u32;
                    (r.id, r.preview_url, r.url, width, height)
                })
                .collect()
        }
        Err(_) => Vec::new(),
    };

    interceptor.set_mode(old_mode);
    result
}

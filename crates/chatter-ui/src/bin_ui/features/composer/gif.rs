//! GIF picker for the composer.
//!
//! Two models feed the drawer: `composer-gif-picker-recent` (the signed-in
//! account's sent GIFs, local, shown at once) and
//! `composer-gif-picker-results` (trending, or a search). Previews load per
//! tile; a tile stays unselectable until its preview is on screen, and a
//! preview that cannot be fetched is marked failed rather than left blank.

use std::rc::Rc;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::{ChatterClient, ComposerSend};
use chatter_core::domain::ids::AttachmentId;
use chatter_core::state::PendingAttachment;
use chatter_core::ui::picker_history::RecentGif;

use crate::{GifData, MainWindow};

pub(crate) fn wire_gif_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    window.on_composer_gif_closed(crate::bin_ui::gif_state::clear_picker_gifs);
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    // The drawer's GIFs tab. The query it kept is re-run, not cleared: a
    // tab switch must not throw away what was typed (an empty query shows
    // Recent and Trending).
    window.on_gif_clicked(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        tracing::debug!("GIF picker clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if w.get_composer_gif_picker_visible() {
                    let query = w.get_composer_gif_picker_search().to_string();
                    request_gifs(&w, &ui_ctx, query);
                } else {
                    // Stop animations when picker is hidden
                    crate::bin_ui::gif_state::clear_picker_gifs();
                }
            }
        });
    });
}

pub(crate) fn wire_gif_search(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_composer_gif_search_changed(move |query: SharedString| {
        tracing::debug!("GIF search requested");
        let Some(w) = weak_window.upgrade() else { return };
        request_gifs(&w, &ui_ctx, query.to_string());
    });
}

/// One request for `query`: an empty query shows the account's recent GIFs
/// at once and asks for trending; anything else is a search. A new
/// generation retires every earlier completion.
fn request_gifs(w: &MainWindow, ui_ctx: &Arc<ChatterClient>, query: String) {
    // Allocate here, before launching the request, rather than relying on a
    // deferred Slint visibility change handler.
    w.set_composer_gif_generation(w.get_composer_gif_generation() + 1);
    let generation = w.get_composer_gif_generation();
    let session = ui_ctx.auth_state().get().session_revision;
    w.set_composer_gif_picker_loading(true);
    w.set_composer_gif_picker_failed(false);
    crate::bin_ui::gif_state::clear_picker_gifs();

    // Recents are local and never depend on the provider answering. They are
    // read for the account signed in now and published only while it still
    // is: core returns nothing for a stale scope, and a scope that went stale
    // between that read and this publish publishes no history either.
    let scope = ui_ctx.picker_history_scope();
    let mut recent: Vec<RawGifData> = match scope {
        Some(scope) if query.is_empty() => ui_ctx
            .recent_gifs(scope)
            .into_iter()
            .map(|g| (g.id, g.preview_url, g.url, g.width, g.height))
            .collect(),
        _ => Vec::new(),
    };
    if !scope.is_some_and(|scope| ui_ctx.picker_scope_is_current(scope)) {
        recent.clear();
    }
    w.set_composer_gif_picker_recent(rows_model(&recent));
    spawn_previews(&recent, w.as_weak(), ui_ctx.clone(), generation, session);

    let weak = w.as_weak();
    let ctx = ui_ctx.clone();
    tokio::spawn(async move {
        let raw_gifs = if query.is_empty() {
            fetch_trending_gifs_raw(&ctx).await
        } else {
            search_gifs_raw(&query, &ctx).await
        };
        load_gif_previews(raw_gifs, weak, ctx, generation, session);
    });
}

pub(crate) fn wire_gif_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_composer_gif_selected(move |gif_id: SharedString, gif_url: SharedString| {
        let Some(w) = weak_window.upgrade() else {
            return;
        };
        let id = gif_id.to_string();
        let url = gif_url.to_string();
        tracing::debug!("GIF selected");
        // What the picker knew about the tile, read while the models are up.
        let picked = find_gif_row(&w, &id);
        // Who is picking, before anything is consumed.
        let scope = ui_ctx.picker_history_scope();

        // Validate FIRST. An editing session, a read-only channel or a
        // changed conversation refuses the send with a toast, and the picker
        // stays open with the tile still there — nothing was consumed and
        // nothing is remembered.
        let Some(send) = begin_gif_send(&w, &ui_ctx) else {
            return;
        };

        // Accepted: the drawer gives way, and the pick is remembered for the
        // account it was made under, so a failed upload can be repicked.
        w.set_composer_gif_picker_visible(false);
        w.invoke_release_text_focus();
        crate::bin_ui::gif_state::clear_picker_gifs();
        if let Some(scope) = scope {
            ui_ctx.record_recent_gif(
                scope,
                RecentGif {
                    id: id.clone(),
                    url: url.clone(),
                    preview_url: picked
                        .as_ref()
                        .map(|g| g.preview_url.to_string())
                        .unwrap_or_default(),
                    width: picked.as_ref().map_or(0, |g| g.width.max(0) as u32),
                    height: picked.as_ref().map_or(0, |g| g.height.max(0) as u32),
                },
            );
        }
        // The pending row goes in now, not when `deliver` starts: the first
        // and slowest part of a GIF send (fetch, encrypt, upload) left the
        // composer empty and the drawer closed with nothing on screen saying
        // a send was under way.
        show_gif_pending(&w, &ui_ctx, &send);
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

/// Show `send` as a "sending" row in its conversation, with a placeholder
/// GIF attachment (a box labelled "GIF" and the indeterminate sending line)
/// until the upload returns the real one. The row id is `deliver`'s, so the
/// upload's end replaces it in place with the GIF, the stored message
/// replaces that, and [`recover_gif_send`] removes it on failure. Shown only
/// while this is the open conversation of the session it was picked in
/// (`show_pending_bubble` checks both).
fn show_gif_pending(w: &MainWindow, ui_ctx: &Arc<ChatterClient>, send: &ComposerSend) {
    let auth = ui_ctx.auth_state().get();
    let user_id = auth
        .current_user_id
        .map(|id| id.to_string())
        .unwrap_or_default();
    let user_name = auth.display_name.unwrap_or_else(|| "Me".to_string());
    let mut shown = send.clone();
    shown.attachments.push(PendingAttachment::new(
        AttachmentId::new(),
        "GIF".to_string(),
        std::path::PathBuf::new(),
        0,
        "image/gif".to_string(),
    ));
    crate::bin_ui::features::chat::send_message::show_pending_bubble(
        &w.as_weak(),
        ui_ctx,
        &shown,
        &user_id,
        &user_name,
    );
}

/// Download the GIF, encrypt it and upload it to our CDN, as an attachment
/// that is already uploaded.
async fn upload_gif(
    ui_ctx: Arc<ChatterClient>,
    url: String,
    gif_id: String,
) -> Result<PendingAttachment, String> {
    let upload = ui_ctx
        .download_and_upload_gif(&url, &gif_id)
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!("GIF uploaded to CDN (encrypted on filesystem)");
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
        Err(_) => {
            tracing::error!("GIF upload failed");
            return Some(send);
        }
    }
    // A logout/login may complete while the upload is in flight. An old
    // account's content must neither be sent nor restored in the new session.
    if !ui_ctx.composer_session_is_current(send.session_revision) {
        return None;
    }
    let started = std::time::Instant::now();
    if crate::bin_ui::features::chat::send_message::deliver(weak, ui_ctx, send.clone(), started)
        .await
    {
        return None;
    }
    Some(send)
}

/// A GIF send that did not go: back into its own composer if that is open
/// and empty, otherwise kept by core until it is (never over newer input).
pub fn recover_gif_send(w: &MainWindow, ui_ctx: &ChatterClient, send: ComposerSend) {
    // The row `show_gif_pending` put up. A failure past the upload has had it
    // removed by `deliver` already; before the upload it is still there. The
    // id is this attempt's alone, so a list that has since been reloaded for
    // another conversation or account matches nothing.
    crate::bin_ui::features::chat::send_message::remove_pending_bubble(w, &send);
    if !ui_ctx.composer_session_is_current(send.session_revision) {
        return;
    }
    use crate::bin_ui::features::chat::send_message::{composer_is_empty, restore_composer};
    match ui_ctx.recover_composer_send(send, composer_is_empty(w)) {
        Some(send) => {
            restore_composer(w, ui_ctx, send);
            toast(
                w,
                "The GIF was not sent. Your message is back in the composer; the GIF is under Recent.",
            );
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

/// (id, preview url, full url, width, height)
type RawGifData = (String, String, String, u32, u32);

fn rows_model(rows: &[RawGifData]) -> ModelRc<GifData> {
    let gifs: Vec<GifData> = rows
        .iter()
        .map(|(id, preview_url, full_url, width, height)| GifData {
            id: id.clone().into(),
            preview: slint::Image::default(),
            preview_url: preview_url.clone().into(),
            full_url: full_url.clone().into(),
            width: *width as i32,
            height: *height as i32,
            failed: false,
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(gifs)))
}

/// The tile the picker shows for `id`, from either section.
fn find_gif_row(w: &MainWindow, id: &str) -> Option<GifData> {
    [
        w.get_composer_gif_picker_recent(),
        w.get_composer_gif_picker_results(),
    ]
    .iter()
    .find_map(|model| model.iter().find(|gif| gif.id.as_str() == id))
}

/// Apply `change` to every tile with `id`, in both sections.
fn update_gif_rows(w: &MainWindow, id: &str, change: impl Fn(&mut GifData)) {
    for model in [
        w.get_composer_gif_picker_recent(),
        w.get_composer_gif_picker_results(),
    ] {
        for i in 0..model.row_count() {
            if let Some(mut gif) = model.row_data(i) {
                if gif.id.as_str() == id {
                    change(&mut gif);
                    model.set_row_data(i, gif);
                }
            }
        }
    }
}

/// Still the request these rows belong to: the drawer is open on the GIF
/// tab, nothing newer was asked for, and the account has not changed.
fn request_is_current(w: &MainWindow, ui_ctx: &ChatterClient, generation: i32, session: u64) -> bool {
    w.get_composer_gif_picker_visible()
        && w.get_composer_gif_generation() == generation
        && ui_ctx.composer_session_is_current(session)
}

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
        Err(_) => {
            tracing::warn!("GIF preview fetch failed");
            return None;
        }
    };

    use image::AnimationDecoder;
    use image::codecs::gif::GifDecoder;
    use std::io::Cursor;

    let cursor = Cursor::new(&bytes);
    let decoder = match GifDecoder::new(cursor) {
        Ok(d) => d,
        Err(_) => {
            tracing::warn!("GIF preview decode failed");
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

/// Publish the provider's answer as the results section, then fetch its
/// previews. On failure the section is empty and `failed` is set; the
/// recent section is untouched either way.
fn load_gif_previews(
    raw_gifs: Result<Vec<RawGifData>, chatter_core::error::NetworkError>,
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: std::sync::Arc<ChatterClient>,
    generation: i32,
    session: u64,
) {
    let raw_gifs = match raw_gifs {
        Ok(gifs) => gifs,
        Err(_) => {
            tracing::warn!("GIF request failed");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_window.upgrade() {
                    if !request_is_current(&w, &ui_ctx, generation, session) {
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

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_window.upgrade() {
            if !request_is_current(&w, &ui_ctx, generation, session) {
                return;
            }
            w.set_composer_gif_picker_results(rows_model(&raw_gifs));
            w.set_composer_gif_picker_failed(false);
            w.set_composer_gif_picker_loading(false);
            spawn_previews(&raw_gifs, w.as_weak(), ui_ctx.clone(), generation, session);
        }
    });
}

/// Fetch and decode each row's preview; the first frame goes into the tile,
/// the rest into the picker animation. A row whose preview cannot be had is
/// marked failed so it never offers a blank hit target.
fn spawn_previews(
    rows: &[RawGifData],
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: std::sync::Arc<ChatterClient>,
    generation: i32,
    session: u64,
) {
    for (id, preview_url, _full_url, _width, _height) in rows {
        let weak = weak_window.clone();
        let gif_id = id.clone();
        let preview_url = preview_url.clone();
        let ui_ctx_fetch = ui_ctx.clone();

        tokio::spawn(async move {
            let decoded = fetch_full_gif_frames(gif_id.clone(), &preview_url, &ui_ctx_fetch).await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                if !request_is_current(&w, &ui_ctx_fetch, generation, session) {
                    return;
                }
                let Some(decoded) = decoded else {
                    update_gif_rows(&w, &gif_id, |gif| gif.failed = true);
                    return;
                };
                let (frame_bytes, width, height, _) = decoded.frames.first().unwrap();
                let first_image = crate::bin_ui::slint_conversions::rgba_to_slint_image(
                    frame_bytes,
                    *width,
                    *height,
                );
                update_gif_rows(&w, &gif_id, |gif| {
                    gif.preview = first_image.clone();
                    gif.failed = false;
                });
                if !w.global::<crate::Appearance>().get_reduce_motion() {
                    crate::bin_ui::gif_state::register_picker_gif(decoded.id, decoded.frames);
                }
            });
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

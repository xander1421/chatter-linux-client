//! Shared attachment download handling for all channel types.
//!
//! Downloads encrypted content and handles GIF/image processing.
//! Used by DMs, anon DMs, channels, and anon groups.

use slint::ComponentHandle;
use std::sync::Arc;

use slint::{Model, VecModel};

use chatter_core::domain::Attachment;
use chatter_core::domain::ids::AttachmentId;

use crate::MainWindow;

/// Fetch media previews, then auto-download small images and process them.
///
/// This is the single entry point for all channel types.
/// Takes resolved `Attachment` domain objects (not proto types).
pub async fn download_attachments(
    attachments: &[Attachment],
    ui_ctx: Arc<chatter_core::client::ChatterClient>,
    weak_window: slint::Weak<MainWindow>,
    conversation_id: String,
    own_sent: bool,
    session: chatter_core::client::SessionGuard,
    owner: crate::bin_ui::features::history_owner::HistoryOwner,
) {
    for att in attachments {
        if !session.is_current() {
            return;
        }
        let cdn_key = match &att.remote_url {
            Some(url) if !url.is_empty() => url.clone(),
            _ => continue,
        };
        let att_id = att.id;
        // Media-kind classification is domain behavior on the attachment metadata.
        let is_image = att.metadata.is_image();
        let is_video = att.metadata.is_video();
        let is_gif = att.metadata.is_gif();
        let mut has_thumbnail = att
            .metadata
            .thumbnail
            .as_ref()
            .map(|t| !t.is_empty())
            .unwrap_or(false);

        // Verify attachment record exists
        if ui_ctx.get_attachment_metadata(&att_id).is_none() {
            tracing::error!("Attachment metadata unavailable for download");
            continue;
        }

        // Instant preview from the thumbnail sidecar — a small, fast fetch that
        // shows a preview BEFORE the (potentially huge) full blob downloads.
        // GIFs included: `ensure_attachment_thumbnail` PERSISTS the decrypted
        // sidecar into the attachment record, so the bubble renders a static
        // preview durably on reopen/restart (the DB record's `thumbnail` field
        // is otherwise None on receive). Without this a GIF showed a bare
        // placeholder until clicked, because the live-only first-frame push in
        // `handle_gif` is lost on any re-render. The animated frames still come
        // from the full download below.
        if (is_image || is_video) && !has_thumbnail {
            if let Some(preview) = ui_ctx
                .ensure_attachment_preview(att_id, session.clone())
                .await
            {
                has_thumbnail = true;
                queue_image_preview(
                    weak_window.clone(),
                    ui_ctx.clone(),
                    owner.clone(),
                    att_id.to_string(),
                    conversation_id.clone(),
                    preview,
                );
            }
        }
        if !session.is_current() {
            return;
        }

        // The originating device already has the video, but its linked device
        // may have received no sidecar (older sends or a failed sidecar upload).
        // Recover that device's own preview from the authenticated full blob.
        if !att.should_auto_download() && !(own_sent && is_video && !has_thumbnail) {
            continue;
        }

        tracing::debug!("Auto-downloading media attachment");

        // Download encrypted content to filesystem
        match ui_ctx.download_attachment_to_filesystem(&att_id).await {
            Ok(()) => {
                if !session.is_current() {
                    return;
                }
                tracing::debug!("Attachment downloaded");

                if is_gif {
                    handle_gif(
                        &ui_ctx,
                        &att_id,
                        &cdn_key,
                        &weak_window,
                        &conversation_id,
                        owner.clone(),
                    );
                } else if is_image && !has_thumbnail {
                    handle_image(
                        &ui_ctx,
                        &att_id,
                        &cdn_key,
                        &weak_window,
                        &conversation_id,
                        owner.clone(),
                        session.clone(),
                    );
                } else if is_video && !has_thumbnail {
                    handle_video(&ui_ctx, &att_id, &cdn_key, &weak_window, &conversation_id);
                }
            }
            Err(_) => {
                tracing::warn!("Attachment download failed");
            }
        }
    }
}

/// Incoming and reopened GIFs use the same authenticated, admitted range path.
fn handle_gif(
    ui_ctx: &Arc<chatter_core::client::ChatterClient>,
    att_id: &AttachmentId,
    _cdn_key: &str,
    weak_window: &slint::Weak<MainWindow>,
    _conversation_id: &str,
    owner: crate::bin_ui::features::history_owner::HistoryOwner,
) {
    let id = att_id.to_string();
    let client = ui_ctx.clone();
    let weak = weak_window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if owner.is_current() {
            crate::bin_ui::gif_state::register_gif_attachment_guarded(id, client, weak, owner);
        }
    });
}

/// Handle still images: original bytes/pixels never enter the event loop.
fn handle_image(
    ui_ctx: &Arc<chatter_core::client::ChatterClient>,
    att_id: &AttachmentId,
    cdn_key: &str,
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: &str,
    owner: crate::bin_ui::features::history_owner::HistoryOwner,
    session: chatter_core::client::SessionGuard,
) {
    let client = ui_ctx.clone();
    let id = *att_id;
    let cdn_key = cdn_key.to_string();
    let conversation = conversation_id.to_string();
    let weak = weak_window.clone();
    crate::rt_handle().spawn(async move {
        if !owner.is_current() {
            return;
        }
        let preview = match client.prepare_downloaded_image_preview(id, session).await {
            Ok(preview) => preview,
            Err(_) => {
                tracing::debug!("Image preview unavailable");
                return;
            }
        };
        queue_image_preview(weak, client, owner, cdn_key, conversation, preview);
    });
}

/// Publish only prepared pixels, fenced again after event-loop queueing.
fn queue_image_preview(
    weak: slint::Weak<MainWindow>,
    client: Arc<chatter_core::client::ChatterClient>,
    owner: crate::bin_ui::features::history_owner::HistoryOwner,
    attachment_key: String,
    conversation: String,
    preview: chatter_core::services::attachment::ImagePreview,
) {
    if !owner.is_current() {
        return;
    }
    let pixels = crate::bin_ui::decode_cache::Pixels::clone_from_slice(
        preview.pixels.as_raw(),
        preview.pixels.width(),
        preview.pixels.height(),
    );
    let _ = slint::invoke_from_event_loop(move || {
        if !owner.is_current() {
            return;
        }
        if let Some(window) = weak.upgrade() {
            update_attachment_thumbnail_with_dims(
                &window,
                &client,
                &attachment_key,
                slint::Image::from_rgba8(pixels),
                preview.width as i32,
                preview.height as i32,
                &conversation,
            );
        }
    });
}

/// Handle video: extract a thumbnail frame if missing.
///
/// Server-managed DMs strip the inline thumbnail before sealing (sealed
/// envelope is padded to small buckets for metadata privacy), so the receiver
/// derives one from the downloaded video. Platform decode is blocking and runs
/// on a background thread; Android persists the frame for pin/profile reuse.
fn handle_video(
    ui_ctx: &Arc<chatter_core::client::ChatterClient>,
    att_id: &AttachmentId,
    cdn_key: &str,
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: &str,
) {
    let ui_ctx_video = ui_ctx.clone();
    let att_id = *att_id;
    let cdn_key = cdn_key.to_string();
    let weak = weak_window.clone();
    let conv_id = conversation_id.to_string();
    let revision = ui_ctx.auth_snapshot().session_revision;

    tokio::task::spawn_blocking(move || {
        let thumb = match ui_ctx_video.decrypt_and_extract_video_thumbnail(&att_id) {
            Ok(result) => result,
            Err(e) => {
                tracing::error!(cdn_key = %cdn_key, error = %e, "Failed to extract video thumbnail");
                return;
            }
        };

        if ui_ctx_video.auth_snapshot().session_revision != revision {
            return;
        }
        let width = thumb.width;
        let height = thumb.height;
        let thumb_bytes = thumb.image_bytes;

        let _ = slint::invoke_from_event_loop(move || {
            if ui_ctx_video.auth_snapshot().session_revision != revision {
                return;
            }
            if let Some(window) = weak.upgrade() {
                match crate::bin_ui::slint_conversions::memory_to_slint_image(&thumb_bytes) {
                    Ok(image) => {
                        update_attachment_thumbnail_with_dims(
                            &window,
                            &ui_ctx_video,
                            &cdn_key,
                            image,
                            width as i32,
                            height as i32,
                            &conv_id,
                        );
                        crate::bin_ui::features::header::refresh_pinned_ui(&window, &ui_ctx_video);
                    }
                    Err(e) => {
                        tracing::error!(cdn_key = %cdn_key, error = ?e, "Failed to create video thumbnail image");
                    }
                }
            }
        });
    });
}

/// Update attachment thumbnail in message list.
pub fn update_attachment_thumbnail(
    window: &MainWindow,
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
    attachment_id: &str,
    image: slint::Image,
    conversation_id: &str,
) {
    if !is_conversation_active(ui_ctx, conversation_id) && !conversation_id.is_empty() {
        return;
    }

    let pane = window.global::<crate::ChatPane>();

    // The open thread panel keeps its own model; its images load too.
    for messages in [pane.get_messages(), pane.get_thread_replies()] {
        for msg_idx in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(msg_idx) {
                let attachments = &msg.attachments;

                for att_idx in 0..attachments.row_count() {
                    if let Some(att) = attachments.row_data(att_idx) {
                        if att.id.as_str() == attachment_id {
                            let mut updated_att = att.clone();
                            let size = image.size();
                            if updated_att.width <= 0 || updated_att.height <= 0 {
                                updated_att.width = size.width as i32;
                                updated_att.height = size.height as i32;
                            }
                            updated_att.thumbnail = image;
                            updated_att.has_thumbnail = true;
                            updated_att.is_downloading = false;
                            updated_att.progress = 1.0;

                            if let Some(att_model) = attachments
                                .as_any()
                                .downcast_ref::<VecModel<crate::MessageAttachmentData>>()
                            {
                                att_model.set_row_data(att_idx, updated_att);
                            }
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Update attachment thumbnail with dimensions.
pub fn update_attachment_thumbnail_with_dims(
    window: &MainWindow,
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
    cdn_key: &str,
    image: slint::Image,
    width: i32,
    height: i32,
    conversation_id: &str,
) {
    if !is_conversation_active(ui_ctx, conversation_id) {
        return;
    }

    let pane = window.global::<crate::ChatPane>();
    let att_id = AttachmentId::from_cdn_key(cdn_key);
    let att_id_str = att_id.to_string();

    // The open thread panel keeps its own model; its images load too.
    for messages in [pane.get_messages(), pane.get_thread_replies()] {
        for msg_idx in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(msg_idx) {
                let attachments = msg.attachments.clone();

                for att_idx in 0..attachments.row_count() {
                    if let Some(att) = attachments.row_data(att_idx) {
                        if att.id.as_str() == att_id_str || att.id.as_str() == cdn_key {
                            let mut updated_att = att.clone();
                            updated_att.has_thumbnail = true;
                            updated_att.thumbnail = image.clone();
                            updated_att.width = width;
                            updated_att.height = height;
                            updated_att.is_downloading = false;
                            updated_att.progress = 1.0;

                            if let Some(model) = attachments
                                .as_any()
                                .downcast_ref::<VecModel<crate::MessageAttachmentData>>()
                            {
                                model.set_row_data(att_idx, updated_att);
                            }
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Check if the given (stringly-typed) conversation identifier refers to the
/// currently-viewed conversation. Delegates to the typed `matches_*`
/// predicates on `ActiveConversation` in `chatter-core`.
fn is_conversation_active(
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
    conversation_id: &str,
) -> bool {
    let active = ui_ctx.active_conversation();
    // A conversation_id string from the UI may be any of: ChannelId uuid,
    // AnonDmId uuid, AnonGroupId uuid, or peer UserId uuid. Try each shape
    // against the matching predicate — exactly one (or none) will hit.
    let channel_match = conversation_id
        .parse()
        .map(|id| active.matches_channel(&id))
        .unwrap_or(false);
    let dm_channel_match = conversation_id
        .parse()
        .map(|id| match &active {
            chatter_core::state::chat_state::ActiveConversation::ServerDm { peer_id } => {
                chatter_core::domain::ids::ChannelId::from(*peer_id) == id
            }
            _ => false,
        })
        .unwrap_or(false);
    let anon_dm_match = conversation_id
        .parse()
        .map(|id| active.matches_anon_dm(&id))
        .unwrap_or(false);
    let anon_group_match = conversation_id
        .parse()
        .map(|id| active.matches_anon_group(&id))
        .unwrap_or(false);
    let peer_match = conversation_id
        .parse()
        .map(|id| active.matches_dm_peer(&id))
        .unwrap_or(false);
    channel_match || dm_channel_match || anon_dm_match || anon_group_match || peer_match
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bin_ui::features::history_owner::HistoryOwner;
    use chatter_core::state::ActiveConversation;

    #[test]
    fn downloaded_image_publication_keeps_small_pixels_and_rejects_stale_view_or_session() {
        const CHILD: &str = "CHATTER_DOWNLOADED_PREVIEW_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let test = format!(
                "{}::downloaded_image_publication_keeps_small_pixels_and_rejects_stale_view_or_session",
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
        i_slint_backend_testing::init_integration_test_with_system_time();
        let runtime = tokio::runtime::Runtime::new().unwrap();
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
        let client = chatter_core::client::ChatterClient::builder()
            .data_dir(dir.path().to_string_lossy().to_string())
            .build()
            .unwrap();
        let a = ActiveConversation::Channel(chatter_core::domain::ids::ChannelId::new());
        let b = ActiveConversation::Channel(chatter_core::domain::ids::ChannelId::new());
        let key = AttachmentId::new().to_string();
        let window = MainWindow::new().unwrap();
        for case in 0..3 {
            client.select_conversation(a);
            let owner = HistoryOwner::begin(&client, a);
            let attachments =
                slint::ModelRc::new(VecModel::from(vec![crate::MessageAttachmentData {
                    id: key.clone().into(),
                    ..Default::default()
                }]));
            window
                .global::<crate::ChatPane>()
                .set_messages(slint::ModelRc::new(VecModel::from(vec![
                    crate::MessageItemData {
                        message_id: "current-row".into(),
                        attachments: attachments.clone(),
                        ..Default::default()
                    },
                ])));
            queue_image_preview(
                window.as_weak(),
                client.clone(),
                owner,
                key.clone(),
                a.channel_id().unwrap().to_string(),
                chatter_core::services::attachment::ImagePreview {
                    pixels: image::RgbaImage::new(320, 160),
                    width: 2048,
                    height: 1024,
                },
            );
            if case == 1 {
                client.select_conversation(b);
                HistoryOwner::begin(&client, b);
                client.select_conversation(a);
                HistoryOwner::begin(&client, a);
            } else if case == 2 {
                client.testing_invalidate_session();
            }
            slint::invoke_from_event_loop(|| slint::quit_event_loop().unwrap()).unwrap();
            slint::run_event_loop().unwrap();
            let row = attachments.row_data(0).unwrap();
            if case == 0 {
                assert!(row.has_thumbnail);
                assert_eq!(
                    (row.thumbnail.size().width, row.thumbnail.size().height),
                    (320, 160)
                );
                assert_eq!((row.width, row.height), (2048, 1024));
            } else {
                assert!(!row.has_thumbnail, "stale preview changed newer view");
            }
        }
    }
}

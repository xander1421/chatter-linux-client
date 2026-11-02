//! Attachment handling for the composer.

use std::path::PathBuf;

use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel, Weak};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::AttachmentId;
use chatter_core::state::ActiveConversation;
use chatter_core::state::chat_state::PendingAttachment;
use std::sync::Arc;

use crate::bin_ui::converters::pending_attachment_to_slint;
use crate::{MainWindow, PendingAttachmentData};

thread_local! {
    /// The tray is rendered from several UI paths which hold only a client
    /// reference. Keep a weak handle for background preview work.
    static PREVIEW_JOBS: std::cell::RefCell<std::collections::HashSet<(u64, AttachmentId, PathBuf)>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
    static PREVIEW_CLIENT: std::cell::RefCell<std::sync::Weak<chatter_core::client::ChatterClient>> =
        std::cell::RefCell::new(std::sync::Weak::new());
}

pub(crate) fn register_preview_client(
    client: &std::sync::Arc<chatter_core::client::ChatterClient>,
) {
    PREVIEW_CLIENT.with(|slot| *slot.borrow_mut() = std::sync::Arc::downgrade(client));
}

/// Process a single file and add it to pending attachments.
/// Used by both file picker and drag/drop.
pub(crate) async fn process_and_add_file(
    file_path: PathBuf,
    staged_size: Option<u64>,
    chat_state: &Arc<ChatterClient>,
    conversation: ActiveConversation,
    revision: u64,
    weak: &Weak<MainWindow>,
) -> Result<(), String> {
    chat_state
        .prepare_pending_file(file_path, staged_size, conversation, revision)
        .await?;

    // Update UI immediately
    update_attachments_ui(chat_state, weak);

    Ok(())
}

/// Update the UI with current pending attachments.
pub(crate) fn update_attachments_ui(chat_state: &Arc<ChatterClient>, weak: &Weak<MainWindow>) {
    let chat_state = chat_state.clone();
    let weak_update = weak.clone();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_update.upgrade() {
            render_attachments(&w, &chat_state.pending_attachments());
        }
    });
}

/// Show `attachments` in the composer's tray.
pub(crate) fn render_attachments(
    w: &MainWindow,
    attachments: &[chatter_core::state::PendingAttachment],
) {
    // Retain ready images on refresh rather than discarding all thumbnails
    // whenever the next file is staged or an entry is removed.
    let old = w.global::<crate::ChatPane>().get_pending_attachments();
    let old_rows: std::collections::HashMap<_, _> =
        old.iter().map(|row| (row.id.to_string(), row)).collect();
    let rows: Vec<_> = attachments
        .iter()
        .map(|attachment| {
            old_rows
                .get(&attachment.id.to_string())
                .cloned()
                .unwrap_or_else(|| pending_attachment_to_slint(attachment))
        })
        .collect();
    w.global::<crate::ChatPane>()
        .set_pending_attachments(ModelRc::new(VecModel::from(rows)));

    let client = PREVIEW_CLIENT.with(|slot| slot.borrow().upgrade());
    if let Some(client) = client {
        let revision = client.session_revision();
        let current: std::collections::HashSet<_> = attachments
            .iter()
            .map(|a| (revision, a.id, a.path.clone()))
            .collect();
        PREVIEW_JOBS.with(|jobs| jobs.borrow_mut().retain(|key| current.contains(key)));
        for (index, attachment) in ChatterClient::pending_preview_files(attachments) {
            let path = attachment.path.clone();
            let id = attachment.id;
            let key = (revision, id, path.clone());
            if !PREVIEW_JOBS.with(|jobs| jobs.borrow_mut().insert(key)) {
                continue;
            }
            let weak = w.as_weak();
            let client = client.clone();
            let session = crate::bin_ui::features::history_owner::SessionOwner::capture(&client);
            crate::rt_handle().spawn(async move {
                let worker_session = session.clone();
                let worker_path = path.clone();
                let worker_client = client.clone();
                let pixels = chatter_core::services::attachment::run_attachment_work(move || {
                    if !worker_session.is_current()
                        || !worker_client
                            .pending_attachments()
                            .iter()
                            .any(|a| a.id == id && a.path == worker_path)
                    {
                        return None;
                    }
                    crate::bin_ui::decode_cache::thumbnail(&id.to_string(), 0, || {
                        worker_client
                            .pending_image_pixels(&worker_path)
                            .map(image::DynamicImage::ImageRgba8)
                    })
                })
                .await
                .ok()
                .flatten();
                let Some(pixels) = pixels else { return };
                let _ = slint::invoke_from_event_loop(move || {
                    if !session.is_current()
                        || !client
                            .pending_attachments()
                            .iter()
                            .any(|a| a.id == id && a.path == path)
                    {
                        return;
                    }
                    let Some(w) = weak.upgrade() else { return };
                    let rows = w.global::<crate::ChatPane>().get_pending_attachments();
                    let Some(model) = rows
                        .as_any()
                        .downcast_ref::<VecModel<PendingAttachmentData>>()
                    else {
                        return;
                    };
                    let id = id.to_string();
                    let index = if model.row_data(index).is_some_and(|row| row.id == id) {
                        Some(index)
                    } else {
                        (0..model.row_count())
                            .find(|&i| model.row_data(i).is_some_and(|row| row.id == id))
                    };
                    if let Some(index) = index {
                        if let Some(mut row) = model.row_data(index) {
                            row.thumbnail = Image::from_rgba8(pixels);
                            row.has_thumbnail = true;
                            model.set_row_data(index, row);
                        }
                    }
                });
            });
        }
    }

    // Update placeholder text
    let pending_count = attachments.len();
    let placeholder = if pending_count == 0 {
        "Type a message...".to_string()
    } else if pending_count == 1 {
        "1 file attached - type a message...".to_string()
    } else {
        format!("{} files attached - type a message...", pending_count)
    };
    w.global::<crate::ChatPane>()
        .set_composer_placeholder(placeholder.into());
}

pub(crate) fn wire_attachment_clicked(
    window: &MainWindow,
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
) {
    let weak_window = window.as_weak();
    let ui_ctx_attach = ui_ctx.clone();
    let chat_state = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_attachment_clicked(move || {
            let weak = weak_window.clone();
            let chat_state = chat_state.clone();
            let ui_ctx_attach = ui_ctx_attach.clone();
            tracing::debug!("Attachment clicked");
            let Some(w) = weak.upgrade() else { return };
            if w.get_composer_attachment_picker_open() {
                return;
            }
            // Settle IME and retain native selection BEFORE the picker can pause
            // Android or take desktop focus. This is the real callback boundary,
            // covering both Attach and the attachment tray's Add action.
            w.invoke_prepare_attachment_picker();
            i_slint_core::properties::ChangeTracker::run_change_handlers();

            // Before the active-conversation check below, not after: a mesh room or
            // cell is not an `ActiveConversation` at all — that enum is the
            // server-side conversation — so the `None` early return would swallow
            // the tap and the picker would never open. `intercept_send` sits in the
            // same position in the send path, for the same reason.
            //
            // A mesh file is fragmented onto the radio, never uploaded, and never
            // staged: staging exists for a later send with text attached.
            #[cfg(feature = "mesh")]
            if crate::bin_ui::features::mesh::intercept_attachment(&ui_ctx_attach, weak.clone()) {
                return;
            }

            // Check if we have an active conversation BEFORE opening file picker
            let snapshot = chat_state.chat_snapshot();
            let conversation = snapshot.active_conversation;
            let revision = snapshot.attachment_revision;
            let picker_session = chat_state.session_guard();

            tracing::info!(
                conversation = ?conversation,
                "Attachment clicked - checking context"
            );

            if matches!(conversation, ActiveConversation::None) {
                tracing::warn!("Cannot upload: no active conversation selected");
                w.invoke_finish_attachment_picker(true);
                return;
            }

            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                // Desktop picks the user's own files by path; nothing to shelter.
                let _ = &ui_ctx_attach;
                tokio::spawn(async move {
                    use chatter_core::media::file_picker::{
                        FileFilter, FilePicker, FilePickerConfig,
                    };

                    let config = FilePickerConfig::new()
                        .with_title("Select attachment")
                        .with_filter(FileFilter::all()) // Default to all files
                        .with_filter(FileFilter::images())
                        .with_filter(FileFilter::videos())
                        .with_filter(FileFilter::audio())
                        .with_filter(FileFilter::documents())
                        .with_multiple(true);

                    let selected_files = FilePicker::pick_files(config).await;
                    finish_picker(&weak, &chat_state, conversation, revision);

                    if selected_files.is_empty() {
                        tracing::debug!("File picker cancelled or no files selected");
                        return;
                    }

                    tracing::info!("Selected {} file(s) for attachment", selected_files.len());

                    for file_path in selected_files {
                        if !picker_session.is_current() {
                            break;
                        }
                        if let Err(e) = process_and_add_file(
                            file_path,
                            None,
                            &chat_state,
                            conversation,
                            revision,
                            &weak,
                        )
                        .await
                        {
                            tracing::error!("{}", e);
                        }
                    }

                    tracing::info!("Added file(s) to pending attachments");
                });
            }
            #[cfg(target_os = "android")]
            {
                tokio::spawn(async move {
                    // Telegram-style source chooser: gallery photo picker (no
                    // permission), the SAF document picker, or the in-app camera.
                    // Whatever the source, Kotlin hands back cache-dir paths
                    // through the same callback the plain picker uses, so the
                    // staging loop below is identical for all three.
                    let selected_files =
                        crate::platform::android::file_picker::pick_via_chooser().await;
                    finish_picker(&weak, &chat_state, conversation, revision);

                    if selected_files.is_empty() {
                        tracing::debug!("File picker cancelled or no files selected");
                        return;
                    }

                    tracing::info!("Selected {} file(s) for attachment", selected_files.len());

                    for file_path in selected_files {
                        // The SAF copy sits in the purgeable cache dir and is ours:
                        // move it where a restart finds it (core owns the dir).
                        let source = file_path;
                        if !picker_session.is_current() {
                            let _ = tokio::fs::remove_file(&source).await;
                            if let Some(parent) = source.parent() {
                                let _ = tokio::fs::remove_dir(parent).await;
                            }
                            continue;
                        }
                        let file_path =
                            match ui_ctx_attach.stage_owned_file_async(source.clone()).await {
                                Ok(path) => path,
                                Err(error) => {
                                    let _ = tokio::fs::remove_file(&source).await;
                                    tracing::error!(%error, "Attachment staging failed");
                                    continue;
                                }
                            };
                        if file_path == source {
                            let _ = tokio::fs::remove_file(&source).await;
                            tracing::warn!("Could not encrypt selected attachment");
                            continue;
                        }
                        if let Err(e) = process_and_add_file(
                            file_path.clone(),
                            None,
                            &chat_state,
                            conversation,
                            revision,
                            &weak,
                        )
                        .await
                        {
                            ui_ctx_attach.release_staged_files(&[PendingAttachment::new(
                                AttachmentId::new(),
                                String::new(),
                                file_path,
                                0,
                                String::new(),
                            )]);
                            tracing::error!("{}", e);
                        }
                    }
                });
            }
            // iOS has no picker yet: chatter-core's file_picker is desktop-only
            // (rfd) and there's no UIDocumentPickerViewController bridge. Warn
            // rather than no-op silently.
            #[cfg(target_os = "ios")]
            {
                let _ = (&chat_state, &weak, &ui_ctx_attach);
                tracing::warn!("File picker not yet implemented on iOS");
                w.invoke_finish_attachment_picker(true);
            }
        });
}

/// A cancelled or failed chooser still completes the input handover. A picker
/// opened in another conversation/session must never focus that new composer.
fn finish_picker(
    weak: &Weak<MainWindow>,
    chat_state: &Arc<ChatterClient>,
    conversation: ActiveConversation,
    revision: u64,
) {
    let weak = weak.clone();
    let chat_state = chat_state.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            let restore = chat_state.active_conversation() == conversation
                && chat_state.chat_snapshot().attachment_revision == revision;
            w.invoke_finish_attachment_picker(restore);
        }
    });
}

/// Wire the paste-image callback: on Ctrl/Cmd+V, attach an image from the
/// clipboard (if any) as a pending attachment. Text paste is handled by the
/// TextInput itself; this only fires the image path.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn wire_paste_image(
    window: &MainWindow,
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.clone();
    let ui_ctx = ui_ctx.clone();

    window.global::<crate::ChatPane>().on_paste_image(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let ui_ctx = ui_ctx.clone();

        let snapshot = chat_state.chat_snapshot();
        let conversation = snapshot.active_conversation;
        let revision = snapshot.attachment_revision;
        // No point reading the clipboard if there's nowhere to attach.
        if matches!(conversation, ActiveConversation::None) {
            return;
        }

        tokio::spawn(async move {
            // arboard is blocking — keep it off the async runtime threads.
            let path = match tokio::task::spawn_blocking(read_clipboard_image).await {
                Ok(Ok(path)) => path,
                Ok(Err(e)) => {
                    // Common, benign case: clipboard holds text, not an image.
                    tracing::debug!("Clipboard paste: no image to attach ({e})");
                    return;
                }
                Err(e) => {
                    tracing::error!("Clipboard read task failed: {e}");
                    return;
                }
            };

            // Clipboard bytes are app-owned. Move the PNG out of the shared
            // system temp directory before keeping it in a recoverable tray.
            let staged = match ui_ctx.stage_owned_file_async(path.clone()).await {
                Ok(staged) => staged,
                Err(error) => {
                    let _ = tokio::fs::remove_file(&path).await;
                    tracing::error!(%error, "Paste staging failed");
                    return;
                }
            };
            if staged == path {
                let _ = tokio::fs::remove_file(&path).await;
                tracing::warn!("Could not stage pasted image");
                return;
            }
            match process_and_add_file(
                staged.clone(),
                None,
                &chat_state,
                conversation,
                revision,
                &weak,
            )
            .await
            {
                Ok(()) => tracing::info!("Attached image pasted from clipboard"),
                Err(e) => {
                    ui_ctx.release_staged_files(&[PendingAttachment::new(
                        AttachmentId::new(),
                        String::new(),
                        staged,
                        0,
                        String::new(),
                    )]);
                    tracing::error!("{e}");
                }
            }
        });
    });
}

/// Read an image off the clipboard and write it to a temp PNG, returning its
/// path so it can flow through the normal attachment pipeline. Errors (incl.
/// "no image on clipboard") are returned as strings for the caller to log.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn read_clipboard_image() -> Result<PathBuf, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let img = clipboard.get_image().map_err(|e| e.to_string())?;

    let width = img.width as u32;
    let height = img.height as u32;
    // arboard yields raw RGBA8; rebuild an image buffer to re-encode as PNG.
    let buffer = image::RgbaImage::from_raw(width, height, img.bytes.into_owned())
        .ok_or_else(|| "clipboard image dimensions don't match buffer".to_string())?;

    let mut file = tempfile::Builder::new()
        .prefix("chatter-paste-")
        .suffix(".png")
        .tempfile()
        .map_err(|e| format!("failed to create pasted image: {e}"))?;
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut file, image::ImageFormat::Png)
        .map_err(|e| format!("failed to encode pasted image: {e}"))?;
    let (_file, path) = file
        .keep()
        .map_err(|e| format!("failed to keep pasted image: {e}"))?;
    Ok(path)
}

/// Wire up the remove-attachment callback.
pub(crate) fn wire_remove_attachment(
    window: &MainWindow,
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_remove_attachment(move |attachment_id: SharedString| {
            let chat_state = &ui_ctx;
            let id_str = attachment_id.to_string();
            tracing::debug!("Remove attachment clicked: {}", id_str);

            // Parse the attachment ID
            let parsed_id: Result<AttachmentId, _> = id_str.parse();
            match parsed_id {
                Ok(id) => {
                    let removed: Vec<PendingAttachment> = chat_state
                        .pending_attachments()
                        .into_iter()
                        .filter(|a| a.id == id)
                        .collect();
                    chat_state.remove_pending_attachment(id);
                    // A sheltered copy leaves with its tray entry; a user's
                    // original is only a reference and stays.
                    ui_ctx.release_staged_files(&removed);
                    tracing::info!("Removed pending attachment: {}", id_str);

                    // Refresh the UI
                    update_attachments_ui(chat_state, &weak_window);
                }
                Err(e) => {
                    tracing::error!("Failed to parse attachment ID '{}': {}", id_str, e);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    #[test]
    fn batch_preparation_inside_slint_future_keeps_event_loop_responsive() {
        const CHILD: &str = "CHATTER_BATCH_PREPARATION_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let test = format!(
                "{}::batch_preparation_inside_slint_future_keeps_event_loop_responsive",
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
        let ui_thread = std::thread::current().id();
        let responsive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let event_seen = responsive.clone();
        let notifier = std::thread::spawn(move || {
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            slint::invoke_from_event_loop(move || {
                event_seen.store(true, std::sync::atomic::Ordering::SeqCst);
                release_tx.send(()).unwrap();
            })
            .unwrap();
        });
        let completed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let results = completed.clone();
        slint::spawn_local(async move {
            let first = chatter_core::services::attachment::run_attachment_work(move || {
                assert_ne!(std::thread::current().id(), ui_thread);
                entered_tx.send(()).unwrap();
                // ONLY the event loop can release this worker. Inline work
                // deadlocks until the watchdog fails, independent of speed.
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                assert!(responsive.load(std::sync::atomic::Ordering::SeqCst));
                0
            })
            .await
            .unwrap();
            results.borrow_mut().push(first);
            for index in 1..8 {
                results.borrow_mut().push(
                    chatter_core::services::attachment::run_attachment_work(move || index)
                        .await
                        .unwrap(),
                );
            }
            slint::quit_event_loop().unwrap();
        })
        .unwrap();
        slint::run_event_loop().unwrap();
        notifier.join().unwrap();
        assert_eq!(*completed.borrow(), (0..8).collect::<Vec<_>>());
    }
}

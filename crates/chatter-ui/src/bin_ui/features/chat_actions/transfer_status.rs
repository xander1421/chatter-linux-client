//! Attachment transfer status updates.
//!
//! Updates download/upload progress in the UI.

use slint::ComponentHandle;
use slint::Model;

use crate::{MainWindow, MessageAttachmentData};

/// Update attachment transfer status in the UI.
/// Must be called from the UI thread via invoke_from_event_loop.
pub fn update_attachment_transfer_status(
    window: &MainWindow,
    attachment_id: &str,
    is_downloading: bool,
    is_uploading: bool,
    progress: f32,
    is_failed: bool,
) {
    let pane = window.global::<crate::ChatPane>();
    for messages in [pane.get_messages(), pane.get_thread_replies()] {
        for msg_idx in 0..messages.row_count() {
            let Some(msg) = messages.row_data(msg_idx) else {
                continue;
            };
            let attachments = msg.attachments.clone();
            for att_idx in 0..attachments.row_count() {
                if let Some(att) = attachments.row_data(att_idx) {
                    if att.id.as_str() == attachment_id {
                        // Update the attachment status
                        let mut updated_att = att.clone();
                        updated_att.is_downloading = is_downloading;
                        updated_att.is_uploading = is_uploading;
                        updated_att.is_download_failed = is_failed;
                        updated_att.progress = progress;

                        // Update in the attachments model
                        if let Some(att_model) = attachments
                            .as_any()
                            .downcast_ref::<slint::VecModel<MessageAttachmentData>>()
                        {
                            att_model.set_row_data(att_idx, updated_att);
                        }
                        // attachment_id is unique across both models; stop scanning.
                        return;
                    }
                }
            }
        }
    }
}

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
) {
    let messages = window.global::<crate::ChatPane>().get_messages();
    for msg_idx in 0..messages.row_count() {
        if let Some(msg) = messages.row_data(msg_idx) {
            let attachments = msg.attachments.clone();
            let mut found = false;

            for att_idx in 0..attachments.row_count() {
                if let Some(att) = attachments.row_data(att_idx) {
                    if att.id.as_str() == attachment_id {
                        found = true;

                        // Update the attachment status
                        let mut updated_att = att.clone();
                        updated_att.is_downloading = is_downloading;
                        updated_att.is_uploading = is_uploading;
                        updated_att.progress = progress;

                        // Update in the attachments model
                        if let Some(att_model) = attachments
                            .as_any()
                            .downcast_ref::<slint::VecModel<MessageAttachmentData>>()
                        {
                            att_model.set_row_data(att_idx, updated_att);
                        }
                        break;
                    }
                }
            }

            if found {
                break;
            }
        }
    }
}

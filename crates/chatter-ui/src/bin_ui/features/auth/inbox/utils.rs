//! Shared utilities for inbox event handling.

/// Generate a preview string from a Slint `MessageItemData`.
///
/// Checks text content first, then falls back to attachment descriptions.
/// Used by receiver-side pin handlers that only have access to the UI model.
pub fn generate_preview_from_ui_message(msg: &crate::MessageItemData) -> String {
    use slint::Model;

    let text = msg.content.to_string();
    if !text.is_empty() {
        return chatter_core::services::gateway::shared::truncate_preview(&text);
    }

    if msg.has_attachments {
        let att_model: &dyn Model<Data = crate::MessageAttachmentData> = &msg.attachments;
        let count = att_model.row_count();
        if count == 1 {
            if let Some(att) = att_model.row_data(0) {
                let cat = att.category.to_string();
                let filename = att.filename.to_string();
                let fname = filename.trim();
                if !fname.is_empty() && cat != "image" && cat != "video" {
                    let emoji = match cat.as_str() {
                        "audio" => "\u{1f3b5}",
                        "document" => "\u{1f4c4}",
                        "code" => "\u{1f4dd}",
                        "archive" => "\u{1f4e6}",
                        _ => "\u{1f4ce}",
                    };
                    return format!("{} {}", emoji, fname);
                }
                let (emoji, name) = match cat.as_str() {
                    "image" => ("\u{1f4f7}", "Photo"),
                    "video" => ("\u{1f3ac}", "Video"),
                    "audio" => ("\u{1f3b5}", "Voice message"),
                    "document" => ("\u{1f4c4}", "Document"),
                    "code" => ("\u{1f4dd}", "Code"),
                    "archive" => ("\u{1f4e6}", "Archive"),
                    _ => ("\u{1f4ce}", "File"),
                };
                return format!("{} {}", emoji, name);
            }
        }
        if count > 1 {
            return format!("\u{1f4ce} {} attachments", count);
        }
        // Fallback
        if msg.attachment_count == 1 {
            return "\u{1f4ce} Attachment".to_string();
        }
        return format!("\u{1f4ce} {} attachments", msg.attachment_count);
    }

    "[Pinned message]".to_string()
}

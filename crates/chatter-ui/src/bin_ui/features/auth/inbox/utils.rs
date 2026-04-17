//! Shared utilities for inbox event handling.

/// Generate a preview string from a Slint `MessageItemData`.
///
/// Checks text content first, then falls back to attachment descriptions.
/// Used by receiver-side pin handlers that only have access to the UI model.
pub fn generate_preview_from_ui_message(msg: &crate::MessageItemData) -> String {
    use slint::Model;

    let text = msg.content.to_string();
    if !text.is_empty() {
        return if text.len() > 80 {
            format!("{}...", &text[..77])
        } else {
            text
        };
    }

    if msg.has_attachments {
        let att_model: &dyn Model<Data = crate::MessageAttachmentData> = &msg.attachments;
        let count = att_model.row_count();
        if count == 1 {
            if let Some(att) = att_model.row_data(0) {
                let cat = att.category.to_string();
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

/// Format a nanosecond timestamp for display.
pub fn format_timestamp(timestamp_nanos: u64) -> String {
    use chrono::{Local, TimeZone};

    if timestamp_nanos == 0 {
        return "".to_string();
    }

    let secs = (timestamp_nanos / 1_000_000_000) as i64;
    let nsecs = (timestamp_nanos % 1_000_000_000) as u32;

    match Local.timestamp_opt(secs, nsecs).single() {
        Some(dt) => {
            let now = Local::now();
            if dt.date_naive() == now.date_naive() {
                dt.format("%H:%M").to_string()
            } else {
                dt.format("%b %d, %H:%M").to_string()
            }
        }
        None => "".to_string(),
    }
}

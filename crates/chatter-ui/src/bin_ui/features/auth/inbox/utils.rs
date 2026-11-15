//! Shared utilities for inbox event handling.

/// Generate a preview string from a Slint `MessageItemData`.
///
/// Checks text content first, then falls back to attachment descriptions.
/// Used by receiver-side pin handlers that only have access to the UI model.
pub fn generate_preview_from_ui_message(msg: &crate::MessageItemData) -> String {
    use slint::Model;

    let text = msg.content.to_string();
    if !text.is_empty() {
        return chatter_core::ui::truncate_preview(&text);
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
                    return match cat.as_str() {
                        "audio" => chatter_i18n::trf("🎵 {}", &[&fname]),
                        "document" => chatter_i18n::trf("📄 {}", &[&fname]),
                        "code" => chatter_i18n::trf("📝 {}", &[&fname]),
                        "archive" => chatter_i18n::trf("📦 {}", &[&fname]),
                        _ => chatter_i18n::trf("📎 {}", &[&fname]),
                    };
                }
                return match cat.as_str() {
                    "image" => chatter_i18n::tr("📷 Photo"),
                    "video" => chatter_i18n::tr("🎬 Video"),
                    "audio" => chatter_i18n::tr("🎵 Voice message"),
                    "document" => chatter_i18n::tr("📄 Document"),
                    "code" => chatter_i18n::tr("📝 Code"),
                    "archive" => chatter_i18n::tr("📦 Archive"),
                    _ => chatter_i18n::tr("📎 File"),
                };
            }
        }
        if count > 1 {
            return chatter_i18n::trf("📎 {} attachments", &[&count]);
        }
        // Fallback
        if msg.attachment_count == 1 {
            return chatter_i18n::tr("📎 Attachment");
        }
        return chatter_i18n::trf("📎 {} attachments", &[&msg.attachment_count]);
    }

    chatter_i18n::tr("[Pinned message]")
}

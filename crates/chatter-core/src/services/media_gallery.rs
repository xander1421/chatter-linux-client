//! Ordered media for the conversation currently displayed by a client.

#[derive(Clone, Debug)]
pub struct GalleryAttachment {
    pub id: String,
    pub is_image: bool,
    pub is_video: bool,
}

#[derive(Clone, Debug)]
pub struct GalleryMessage {
    pub sent_at_ns: u64,
    pub attachments: Vec<GalleryAttachment>,
}

/// Keep message order stable for equal timestamps and album order within each
/// message. Callers pass only messages from the active conversation.
pub fn ordered_media(messages: impl IntoIterator<Item = GalleryMessage>) -> Vec<String> {
    let mut messages: Vec<_> = messages.into_iter().enumerate().collect();
    messages.sort_by_key(|(position, message)| (message.sent_at_ns, *position));
    let mut items: Vec<_> = messages
        .into_iter()
        .flat_map(|(_, message)| message.attachments)
        .filter(|attachment| attachment.is_image || attachment.is_video)
        .map(|attachment| attachment.id)
        .collect();
    let mut seen = std::collections::HashSet::new();
    items.retain(|id| seen.insert(id.clone()));
    items
}

/// Read the full local history for the open conversation. This can touch many
/// records and must be called away from the UI thread. Secret conversations
/// use the same device-local message store as their ordinary history view.
pub fn conversation_media(
    client: &crate::client::ChatterClient,
    conversation: &crate::state::ActiveConversation,
) -> Result<Vec<String>, crate::services::message::MessageError> {
    use crate::domain::attachment::FileCategory;
    let messages = client.get_messages(conversation, i32::MAX as u32)?;
    Ok(ordered_media(messages.into_iter().map(|message| {
        GalleryMessage {
            sent_at_ns: message.sent_at,
            attachments: message
                .attachments
                .into_iter()
                .filter_map(|id| {
                    let attachment = client.get_attachment(&id)?;
                    let category = FileCategory::from_mime_and_filename(
                        &attachment.metadata.mime_type,
                        &attachment.metadata.name,
                    );
                    Some(GalleryAttachment {
                        id: id.to_string(),
                        is_image: category == FileCategory::Image,
                        is_video: category == FileCategory::Video,
                    })
                })
                .collect(),
        }
    })))
}

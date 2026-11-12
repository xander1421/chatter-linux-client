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

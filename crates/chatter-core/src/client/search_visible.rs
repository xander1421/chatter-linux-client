//! Visible local search results for the unified search palette.

use std::collections::HashSet;

use chrono::{DateTime, Utc};

use super::ChatterClient;
use crate::services::search::{SearchCategory, SearchDocument, SearchQuery};

pub struct SearchPage {
    pub hits: Vec<VisibleSearchHit>,
    pub counts: [i32; 7],
}

pub struct VisibleSearchHit {
    pub message_id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub sender_name: String,
    pub preview: String,
    pub created_at: DateTime<Utc>,
    pub category: SearchCategory,
    pub thumbnail: Option<Vec<u8>>,
    text: String,
    size: u64,
    sender_id: Option<String>,
}

impl SearchDocument for VisibleSearchHit {
    fn category(&self) -> SearchCategory {
        self.category
    }
    fn search_text(&self) -> &str {
        &self.text
    }
    fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
    fn size_bytes(&self) -> u64 {
        self.size
    }
    fn sender_id(&self) -> Option<&str> {
        self.sender_id.as_deref()
    }
    fn chat_id(&self) -> &str {
        &self.channel_id
    }
}

fn category_slot(category: SearchCategory) -> Option<usize> {
    match category {
        SearchCategory::Messages => Some(2),
        SearchCategory::Media => Some(3),
        SearchCategory::Files => Some(4),
        SearchCategory::Links => Some(5),
        SearchCategory::Voice => Some(6),
        _ => None,
    }
}

impl ChatterClient {
    /// Merge decrypted messages and shared content, then filter, count and page
    /// only results visible in the current mode. Source methods enforce the
    /// secret-chat exclusion before either result enters this pipeline.
    pub fn search_visible(&self, request: SearchQuery, page_limits: &[i32]) -> SearchPage {
        let messages = self.search_messages_visible(&request.text, 250);
        let mut candidates = Vec::with_capacity(messages.len());
        for msg in &messages {
            let attachment = msg
                .attachments
                .first()
                .and_then(|id| self.get_attachment_metadata(id));
            let (attachment_name, size) = attachment
                .as_ref()
                .map(|file| (file.metadata.name.clone(), file.metadata.size))
                .unwrap_or_default();
            let category = SearchCategory::from_message(
                attachment
                    .as_ref()
                    .map(|file| file.metadata.mime_type.as_str()),
                attachment
                    .as_ref()
                    .is_some_and(|file| file.metadata.waveform_samples.is_some()),
                msg.as_text(),
            );
            candidates.push(VisibleSearchHit {
                message_id: msg.id.to_string(),
                channel_id: msg.conversation_id.to_string(),
                channel_name: self
                    .search_conversation_display_name(&msg.conversation_id, &msg.conversation_type),
                sender_name: self.search_sender_display_name(msg),
                preview: self.search_result_preview(
                    msg.as_text().unwrap_or(&attachment_name),
                    &request.text,
                ),
                created_at: DateTime::<Utc>::from_timestamp_nanos(msg.sent_at as i64),
                category,
                thumbnail: attachment
                    .as_ref()
                    .and_then(|file| file.metadata.thumbnail.clone()),
                text: format!("{} {}", msg.as_text().unwrap_or(""), attachment_name),
                size,
                sender_id: msg.sender_id.map(|id| id.to_string()),
            });
        }
        let mut all_request = request.clone();
        all_request.category = SearchCategory::All;
        if request.category != SearchCategory::Chats {
            let mut seen: HashSet<String> = candidates
                .iter()
                .map(|hit| hit.message_id.clone())
                .collect();
            for item in self.search_shared_content_visible(&all_request, 250) {
                if !seen.insert(item.message_id.clone()) {
                    continue;
                }
                let Ok(id) = item.message_id.parse() else {
                    continue;
                };
                let Some(msg) = self.get_message_local(&id) else {
                    continue;
                };
                let label = if item.size_bytes > 0 {
                    format!(
                        "{} · {}",
                        item.title,
                        crate::domain::attachment::format_bytes(item.size_bytes)
                    )
                } else {
                    item.title.clone()
                };
                candidates.push(VisibleSearchHit {
                    message_id: item.message_id,
                    channel_id: msg.conversation_id.to_string(),
                    channel_name: self.search_conversation_display_name(
                        &msg.conversation_id,
                        &msg.conversation_type,
                    ),
                    sender_name: self.search_sender_display_name(&msg),
                    preview: self.search_result_preview(&label, &request.text),
                    created_at: DateTime::<Utc>::from_timestamp_nanos(msg.sent_at as i64),
                    category: SearchCategory::from_shared_kind(&item.category),
                    thumbnail: item.thumbnail,
                    text: item.title,
                    size: item.size_bytes,
                    sender_id: msg.sender_id.map(|id| id.to_string()),
                });
            }
        }
        let candidates = all_request.apply(candidates);
        let mut counts = [0; 7];
        for hit in &candidates {
            counts[0] += 1;
            if let Some(slot) = category_slot(hit.category) {
                counts[slot] += 1;
            }
        }
        let mut shown = [0; 7];
        let hits = request
            .apply(candidates)
            .into_iter()
            .filter(|hit| {
                let Some(slot) = category_slot(hit.category) else {
                    return false;
                };
                if shown[slot] >= page_limits.get(slot).copied().unwrap_or(50) {
                    return false;
                }
                shown[slot] += 1;
                true
            })
            .collect();
        SearchPage { hits, counts }
    }
}

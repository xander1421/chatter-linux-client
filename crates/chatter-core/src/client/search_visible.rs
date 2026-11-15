//! Visible local search results for the unified search palette.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use super::ChatterClient;
use crate::services::search::{SearchCategory, SearchDocument, SearchQuery, preview_fragments};
use unicode_segmentation::UnicodeSegmentation;

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

struct SearchCandidate {
    message: crate::domain::message::Message,
    attachment: Option<crate::domain::attachment::Attachment>,
    shared: Option<crate::storage::repositories::SharedContentRow>,
    category: SearchCategory,
    text: String,
    size: u64,
    sender_id: Option<String>,
    chat_id: String,
}

impl SearchDocument for SearchCandidate {
    fn category(&self) -> SearchCategory {
        self.category
    }
    fn search_text(&self) -> &str {
        &self.text
    }
    fn created_at(&self) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_nanos(self.message.sent_at as i64)
    }
    fn size_bytes(&self) -> u64 {
        self.size
    }
    fn sender_id(&self) -> Option<&str> {
        self.sender_id.as_deref()
    }
    fn chat_id(&self) -> &str {
        &self.chat_id
    }
}

impl ChatterClient {
    /// Merge decrypted messages and shared content, then filter, count and page
    /// only results visible in the current mode. Source methods enforce the
    /// secret-chat exclusion before either result enters this pipeline.
    pub fn search_visible(&self, request: SearchQuery, page_limits: &[i32]) -> SearchPage {
        let mut messages = Vec::new();
        let mut offset = 0;
        const BATCH: u32 = 250;
        loop {
            let (batch, count) = self.search_messages_visible_page(&request, BATCH, offset);
            messages.extend(batch);
            if count < BATCH as usize {
                break;
            }
            offset += BATCH;
        }
        let mut candidates = Vec::with_capacity(messages.len());
        let mut seen: HashSet<_> = messages.iter().map(|msg| msg.id).collect();
        for msg in messages {
            let attachment = msg
                .attachments
                .first()
                .and_then(|id| self.get_attachment_metadata(id));
            let category = SearchCategory::from_message(
                attachment
                    .as_ref()
                    .map(|file| file.metadata.mime_type.as_str()),
                attachment
                    .as_ref()
                    .is_some_and(|file| file.metadata.waveform_samples.is_some()),
                msg.as_text(),
            );
            let name = attachment
                .as_ref()
                .map(|file| file.metadata.name.as_str())
                .unwrap_or("");
            let size = attachment
                .as_ref()
                .map(|file| file.metadata.size)
                .unwrap_or_default();
            let text = format!("{} {}", msg.as_text().unwrap_or(""), name);
            let sender_id = msg.sender_id.map(|id| id.to_string());
            let chat_id = msg.conversation_id.to_string();
            candidates.push(SearchCandidate {
                message: msg,
                attachment,
                shared: None,
                category,
                text,
                size,
                sender_id,
                chat_id,
            });
        }
        let mut all_request = request.clone();
        all_request.category = SearchCategory::All;
        let shared_counts = if request.category == SearchCategory::Chats {
            [0; 7]
        } else {
            self.count_shared_content_visible(&all_request, &seen)
        };
        let shared_categories = [SearchCategory::Media, SearchCategory::Files,
            SearchCategory::Links, SearchCategory::Voice];
        for shared_category in shared_categories {
            if request.category != SearchCategory::All && request.category != shared_category {
                continue;
            }
            let slot = category_slot(shared_category).expect("shared category has a slot");
            let shared_limit = page_limits.get(slot).copied().unwrap_or(50).max(0) as usize;
            let mut shared_request = request.clone();
            shared_request.category = shared_category;
            for item in self.search_shared_content_visible(&shared_request, shared_limit, &seen) {
                let Ok(id) = item.message_id.parse() else {
                    continue;
                };
                if !seen.insert(id) {
                    continue;
                }
                let Some(message) = self.get_message_local(&id) else {
                    continue;
                };
                let sender_id = message.sender_id.map(|id| id.to_string());
                let chat_id = message.conversation_id.to_string();
                candidates.push(SearchCandidate {
                    sender_id,
                    chat_id,
                    category: SearchCategory::from_shared_kind(&item.category),
                    text: item.title.clone(),
                    size: item.size_bytes,
                    message,
                    attachment: None,
                    shared: Some(item),
                });
            }
        }
        // Count the same filtered set that provides the rows. Category
        // selection retains the first pass's stable sort within a section.
        let candidates = all_request.apply(candidates);
        let mut counts = shared_counts;
        for candidate in &candidates {
            if candidate.shared.is_some() { continue; }
            counts[0] += 1;
            if let Some(slot) = category_slot(candidate.category) {
                counts[slot] += 1;
            }
        }
        let mut shown = [0; 7];
        let selected = candidates
            .into_iter()
            .filter_map(|candidate| {
                if request.category != SearchCategory::All && request.category != candidate.category
                {
                    return None;
                }
                let slot = category_slot(candidate.category)?;
                if shown[slot] >= page_limits.get(slot).copied().unwrap_or(50) {
                    return None;
                }
                shown[slot] += 1;
                Some(candidate)
            })
            .collect::<Vec<_>>();
        let messages = selected
            .iter()
            .map(|candidate| candidate.message.clone())
            .collect::<Vec<_>>();
        let sender_names = self.search_sender_display_names(&messages);
        let mut conversation_names = HashMap::new();
        let hits = selected
            .into_iter()
            .zip(sender_names)
            .map(|(candidate, sender_name)| {
                let msg = &candidate.message;
                let (preview, thumbnail) = if let Some(item) = &candidate.shared {
                    let label = if item.size_bytes > 0 {
                        chatter_i18n::trf(
                            "{} · {}",
                            &[
                                &item.title,
                                &crate::domain::attachment::format_bytes(item.size_bytes),
                            ],
                        )
                    } else {
                        item.title.clone()
                    };
                    let thumbnail = item.attachment_id.parse().ok()
                        .and_then(|id| self.get_attachment_metadata(&id))
                        .and_then(|attachment| attachment.metadata.thumbnail);
                    (self.search_result_preview(&label, &request.text), thumbnail)
                } else {
                    let name = candidate
                        .attachment
                        .as_ref()
                        .map(|file| file.metadata.name.as_str())
                        .unwrap_or("");
                    (
                        self.search_result_preview(msg.as_text().unwrap_or(name), &request.text),
                        candidate
                            .attachment
                            .as_ref()
                            .and_then(|file| file.metadata.thumbnail.clone()),
                    )
                };
                VisibleSearchHit {
                    message_id: msg.id.to_string(),
                    channel_id: msg.conversation_id.to_string(),
                    channel_name: conversation_names
                        .entry((msg.conversation_id.clone(), msg.conversation_type.clone()))
                        .or_insert_with(|| self.search_conversation_display_name(
                            &msg.conversation_id,
                            &msg.conversation_type,
                        ))
                        .clone(),
                    sender_name,
                    preview,
                    created_at: candidate.created_at(),
                    category: candidate.category,
                    thumbnail,
                }
            })
            .collect();
        SearchPage { hits, counts }
    }
}

impl ChatterClient {
    /// Resolve result labels from the same local record as secret-chat rows.
    pub fn search_conversation_display_name(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> String {
        #[cfg(feature = "mesh")]
        if matches!(
            conversation_type,
            crate::domain::message::ConversationType::Mesh
                | crate::domain::message::ConversationType::MeshDm
        ) {
            return self
                .mesh_conversation_name(conversation_id)
                .unwrap_or_else(|| "Mesh chat".to_string());
        }
        if *conversation_type == crate::domain::message::ConversationType::AnonDm {
            let id = crate::domain::anon_dm::AnonDmId::from_uuid(*conversation_id.as_uuid());
            if let Some(dm) = self.get_anon_dm_conversation(&id) {
                return dm
                    .peer_display_name
                    .unwrap_or_else(|| self.fallback_display_name(&dm.peer_id));
            }
        }
        self.conversation_display_name(conversation_id, conversation_type)
    }

    /// Keep the first query match inside the compact one-line result preview.
    pub fn search_result_preview(&self, text: &str, query: &str) -> String {
        let flat = self.plain_message_preview(text);
        let grapheme_count = flat.graphemes(true).count();
        if grapheme_count <= 100 {
            return crate::domain::message::format_preview_body("", &flat, None);
        }
        let (_, matched, _, start, _) = preview_fragments(&flat, query);
        let match_at = flat[..start as usize].graphemes(true).count();
        let start = if matched.is_empty() || match_at < 70 {
            0
        } else {
            match_at.saturating_sub(30)
        };
        let end = (start + 99).min(grapheme_count);
        let body = format!(
            "{}{}{}",
            if start > 0 { "…" } else { "" },
            flat.graphemes(true)
                .skip(start)
                .take(end - start)
                .collect::<String>(),
            if end < grapheme_count { "…" } else { "" }
        );
        crate::domain::message::format_preview_body("", &body, None)
    }

    /// Resolve search labels in one directory lookup, preserving the special
    /// labels used by mesh, self messages and sealed-sender DMs.
    pub fn search_sender_display_names(
        &self,
        messages: &[crate::domain::message::Message],
    ) -> Vec<String> {
        use crate::domain::message::ConversationType;
        let mut names = self.message_sender_display_names(messages);
        for (message, name) in messages.iter().zip(names.iter_mut()) {
            #[cfg(feature = "mesh")]
            if matches!(
                message.conversation_type,
                ConversationType::Mesh | ConversationType::MeshDm
            ) {
                *name = message
                    .sender_id
                    .as_ref()
                    .map(|sender| {
                        if *sender == self.mesh_local_user_id() {
                            chatter_i18n::tr("You")
                        } else {
                            self.mesh_sender_display_name(sender)
                                .unwrap_or_else(|| chatter_i18n::tr("Unknown peer"))
                        }
                    })
                    .unwrap_or_else(|| chatter_i18n::tr("Unknown peer"));
                continue;
            }
            if message.sender_id.is_some() && message.sender_id == self.session_current_user_id() {
                *name = chatter_i18n::tr("You");
            } else if message.sender_id.is_none()
                && message.conversation_type == ConversationType::ServerDm
            {
                *name = self.conversation_display_name(
                    &message.conversation_id,
                    &message.conversation_type,
                );
            }
        }
        names
    }
}

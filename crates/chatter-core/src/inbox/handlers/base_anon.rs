//! Shared utilities for anonymous DM and group content processing.
//!
//! Services (`AnonDmService`, `AnonGroupService`) use these helpers to build
//! unified `Message` instances, extract reply/quote metadata, and dispatch the
//! content types that behave identically across DM and Group conversations
//! (data messages, reactions, edits, pins, typing, receipts).

use std::fmt;
use std::str::FromStr;

use crate::domain::UserId;
use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::domain::message::{ConversationType, Message, MessageContent};
use crate::proto::content_v1::{self, AttachmentPointer, Content, receipt_message, typing_message};

use super::DomainEvent;

// ============================================================================
// Quote / reply context (extracted from the inbound Content proto)
// ============================================================================

/// Reply/quote metadata persisted alongside an anonymous data message so
/// reply bubbles survive a channel reload on the receiver side.
#[derive(Debug, Clone)]
pub struct QuoteContext {
    pub reply_to: Option<MessageId>,
    pub reply_to_author_id: Option<UserId>,
    pub reply_to_text: Option<String>,
    /// JPEG/PNG thumbnail bytes of the first quoted attachment.
    pub reply_to_thumbnail: Option<Vec<u8>>,
    /// MIME type of the quoted thumbnail (e.g. `image/jpeg`).
    pub reply_to_thumbnail_mime: Option<String>,
}

impl QuoteContext {
    /// Extract reply context from a Content proto's DataMessage quote.
    pub fn from_content(inner_content: &Content) -> Option<Self> {
        let dm = match &inner_content.content {
            Some(content_v1::content::Content::DataMessage(dm)) => dm,
            _ => return None,
        };
        let quote = dm.quote.as_ref()?;
        let reply_to = Some(MessageId::from_timestamp(quote.id));
        let reply_to_author_id = if quote.author_aci.is_empty() {
            None
        } else {
            UserId::from_str(&quote.author_aci).ok()
        };
        let reply_to_text = if quote.text.is_empty() {
            None
        } else {
            Some(quote.text.clone())
        };
        let (reply_to_thumbnail, reply_to_thumbnail_mime) = quote
            .attachments
            .first()
            .map(|qa| {
                let thumb = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
                let mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
                (thumb, mime)
            })
            .unwrap_or((None, None));
        if reply_to.is_none()
            && reply_to_author_id.is_none()
            && reply_to_text.is_none()
            && reply_to_thumbnail.is_none()
        {
            return None;
        }
        Some(Self {
            reply_to,
            reply_to_author_id,
            reply_to_text,
            reply_to_thumbnail,
            reply_to_thumbnail_mime,
        })
    }
}

// ============================================================================
// Message construction
// ============================================================================

/// Build a unified `Message` from anonymous content handler data.
pub fn build_message_from_content(
    message_id_str: &str,
    conversation_id: ChannelId,
    conversation_type: ConversationType,
    sender_id: UserId,
    inner_content: &Content,
    sent_at: u64,
) -> Message {
    let msg_id = message_id_str
        .parse::<uuid::Uuid>()
        .map(MessageId::from_uuid)
        .unwrap_or_else(|_| MessageId::new());

    let content = content_to_message_content(inner_content);
    let created_at = crate::util::time::TimestampNanos::from_nanos(sent_at)
        .to_datetime()
        .unwrap_or_else(chrono::Utc::now);

    let data_msg = match &inner_content.content {
        Some(content_v1::content::Content::DataMessage(dm)) => Some(dm),
        _ => None,
    };

    let reply_to = data_msg
        .and_then(|dm| dm.quote.as_ref())
        .map(|q| MessageId::from_timestamp(q.id));

    let reply_to_author_id = data_msg.and_then(|dm| dm.quote.as_ref()).and_then(|q| {
        if q.author_aci.is_empty() {
            None
        } else {
            UserId::from_str(&q.author_aci).ok()
        }
    });

    let reply_to_text = data_msg
        .and_then(|dm| dm.quote.as_ref())
        .map(|q| q.text.clone())
        .filter(|t| !t.is_empty());

    let (reply_to_thumbnail, reply_to_thumbnail_mime) = data_msg
        .and_then(|dm| dm.quote.as_ref())
        .and_then(|q| q.attachments.first())
        .map(|qa| {
            let thumb = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
            let mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
            (thumb, mime)
        })
        .unwrap_or((None, None));

    let attachments: Vec<AttachmentId> = data_msg
        .map(|dm| {
            dm.attachments
                .iter()
                .filter(|ptr| !ptr.cdn_key.is_empty())
                .map(|ptr| AttachmentId::from_cdn_key(&ptr.cdn_key))
                .collect()
        })
        .unwrap_or_default();

    Message {
        id: msg_id,
        conversation_id,
        conversation_type,
        sender_id: Some(sender_id),
        device_id: None,
        content,
        encrypted_content: None,
        thread_id: None,
        reply_to,
        reply_to_author_id,
        reply_to_text,
        reply_to_thumbnail,
        reply_to_thumbnail_mime,
        attachments,
        is_pinned: false,
        is_edited: false,
        is_deleted: false,
        is_read: false,
        sent_at,
        created_at,
        updated_at: created_at,
        edited_at: None,
        reaction_counts: Vec::new(),
        current_user_reactions: Vec::new(),
        mentioned_integrations: Vec::new(),
        targets_ai_bot: false,
        delivery_status: String::new(),
        failure_reason: None,
    }
}

/// Convert Content proto to MessageContent for the unified Message struct.
///
/// Preserves Mixed (text + attachments) when the inbound DataMessage has
/// both body and attachments — otherwise the receiver's unified Message
/// would drop the attachments from the content variant, and
/// `message_reply_preview` / anything that pattern-matches on
/// `MessageContent::Mixed` would miss them on reload.
pub fn content_to_message_content(content: &Content) -> MessageContent {
    match &content.content {
        Some(content_v1::content::Content::DataMessage(data_msg)) => {
            let attachment_ids: Vec<AttachmentId> = data_msg
                .attachments
                .iter()
                .filter(|ptr| !ptr.cdn_key.is_empty())
                .map(|ptr| AttachmentId::from_cdn_key(&ptr.cdn_key))
                .collect();
            crate::services::anon_shared::message_content_from_text_and_attachments(
                &data_msg.body,
                &attachment_ids,
            )
        }
        _ => MessageContent::Text(String::new()),
    }
}

// ============================================================================
// Shared backend trait
// ============================================================================

/// Persistence + blocking ops used by the common content dispatcher.
///
/// Implemented for both `AnonDmService` and `AnonGroupService` so the
/// dispatcher can persist + emit events without caring which conversation
/// flavor it's operating on.
pub trait AnonInboxBackend {
    /// The conversation/group ID type (`AnonDmId` or `AnonGroupId`).
    type Id: Copy + fmt::Display + Into<ChannelId>;

    fn conversation_type(&self) -> ConversationType;
    fn label(&self) -> &'static str;

    /// Default: false. DM overrides to check its block list.
    fn is_sender_blocked(&self, _sender_id: &UserId) -> bool {
        false
    }

    /// Default: false. DM overrides to check per-conversation blocking.
    fn is_conversation_blocked(&self, _id: &Self::Id) -> bool {
        false
    }

    fn create_attachment_records(&self, pointers: &[AttachmentPointer]) -> Vec<AttachmentId>;

    fn store_data_message(
        &self,
        id: &Self::Id,
        sender: &UserId,
        text: &str,
        attachments: &[AttachmentId],
        sent_at: u64,
        quote: Option<QuoteContext>,
    ) -> Result<String, String>;

    fn delete_message(&self, id: &Self::Id, sender: &UserId, ts: u64) -> Result<bool, String>;

    /// Apply `sender`'s edit of its own message sent at `target_ts` in `id`,
    /// through the one edit choke point
    /// ([`MessageService::apply_channel_edit`](crate::services::message::MessageService::apply_channel_edit)):
    /// author only, ordered and dated by `edit_ts`, Signal's 48h, held until
    /// the message arrives. `sender` is the one the mailbox authenticated.
    /// Returns the edited message when it changed.
    fn apply_edit(
        &self,
        id: &Self::Id,
        sender: &UserId,
        target_ts: u64,
        text: &str,
        edit_ts: u64,
    ) -> Result<Option<Message>, String>;

    /// Returns `true` if the reaction was newly inserted, `false` if it
    /// was already present (per the unified-reactions UNIQUE constraint).
    /// Callers gate the `DomainEvent::ReactionAdded` emit on this so the
    /// UI doesn't double-count when the same reaction arrives twice (e.g.
    /// snapshot + delta from the same poll cycle).
    fn add_reaction(
        &self,
        id: &Self::Id,
        target_sender: &UserId,
        target_ts_ns: u64,
        reactor: &UserId,
        emoji: &str,
    ) -> Result<bool, String>;

    /// Returns `true` if a reaction was actually removed.
    fn remove_reaction(
        &self,
        id: &Self::Id,
        target_sender: &UserId,
        target_ts_ns: u64,
        reactor: &UserId,
        emoji: &str,
    ) -> Result<bool, String>;

    /// Durable exact-scope admission for an authenticated anonymous transport.
    #[allow(clippy::too_many_arguments)]
    fn receive_reaction(
        &self,
        id: &Self::Id,
        author: &UserId,
        target_ts: u64,
        reactor: &UserId,
        emoji: &str,
        remove: bool,
        action_ts: u64,
    ) -> Result<bool, crate::error::StorageError>;

    fn set_pinned(&self, id: &Self::Id, ts: u64, pinned: bool) -> Result<(), String>;

    fn get_message_id_by_timestamp(&self, id: &Self::Id, ts: u64) -> Option<MessageId>;

    /// Default: no-op. DM overrides to persist read/delivered status to its table.
    fn persist_delivery_status(&self, _id: &Self::Id, _timestamps: &[u64], _status: &str) {}

    /// Whether receipts should emit `MessagesRead` / `MessagesDelivered` events.
    /// Default: true. Group overrides to suppress (receipts are debug-only there).
    fn emit_receipts(&self) -> bool {
        true
    }
}

// ============================================================================
// Shared dispatcher
// ============================================================================

/// Outcome of dispatching a content proto through the common handler.
pub enum CommonDispatchOutcome {
    /// Content handled by common dispatcher. Caller emits these events.
    Handled(Vec<DomainEvent>),
    /// Content type not handled here. Caller must dispatch per-service logic
    /// (e.g. calls, profile updates, group management).
    NotHandled,
    /// Persistent admission/application failed; caller must retain the envelope.
    Failed(crate::error::StorageError),
    /// Content intentionally dropped (blocked sender, blocked conversation).
    Dropped,
}

/// Dispatch the content types that behave identically across DM and Group.
///
/// Handled: DataMessage (store + delete), Typing, Reaction, Receipt, Pin, Edit.
/// Everything else returns `NotHandled` so the caller can dispatch per-service
/// content (calls, profile updates, group management, anon-group invites, etc.).
pub fn dispatch_common_content<B: AnonInboxBackend>(
    id: &B::Id,
    sender_id: &UserId,
    inner_content: &Content,
    backend: &B,
) -> CommonDispatchOutcome {
    use content_v1::content::Content as ContentKind;

    if backend.is_sender_blocked(sender_id) {
        tracing::info!(
            sender_id = %sender_id,
            id = %id,
            label = backend.label(),
            "Dropped content from blocked sender"
        );
        return CommonDispatchOutcome::Dropped;
    }
    if backend.is_conversation_blocked(id) {
        tracing::info!(
            id = %id,
            label = backend.label(),
            "Dropped content for blocked conversation"
        );
        return CommonDispatchOutcome::Dropped;
    }

    let channel_id: ChannelId = (*id).into();
    let mut events = Vec::new();

    match &inner_content.content {
        Some(ContentKind::DataMessage(data_msg)) => {
            // Delete-for-everyone wrapped inside DataMessage
            if let Some(delete) = &data_msg.delete {
                if delete.target_call_id.is_some()
                    || delete.destination_user_id.is_some()
                    || delete
                        .target_author_aci
                        .as_deref()
                        .is_some_and(|author| author.parse::<UserId>().ok() != Some(*sender_id))
                {
                    return CommonDispatchOutcome::Dropped;
                }
                // Announce only a delete that removed a row: the backend
                // matches (sender, timestamp), so a member naming someone
                // else's message deletes nothing and must not blank it on
                // screen either.
                match backend.delete_message(id, sender_id, delete.target_sent_timestamp) {
                    Ok(true) => {
                        tracing::info!(
                            timestamp = delete.target_sent_timestamp,
                            sender_id = %sender_id,
                            id = %id,
                            "Deleted message (delete-for-everyone)"
                        );
                        events.push(DomainEvent::MessageDeleted {
                            message_id: MessageId::from_timestamp(delete.target_sent_timestamp),
                            author_id: *sender_id,
                            channel_id: Some(channel_id),
                            target_sent_timestamp: delete.target_sent_timestamp,
                        });
                    }
                    Ok(false) => tracing::debug!(
                        timestamp = delete.target_sent_timestamp,
                        "Delete target not found"
                    ),
                    Err(e) => tracing::warn!(error = %e, "Failed to delete message"),
                }
                return CommonDispatchOutcome::Handled(events);
            }

            let has_text = !data_msg.body.is_empty();
            let has_attachments = !data_msg.attachments.is_empty();
            if !has_text && !has_attachments {
                return CommonDispatchOutcome::Handled(events);
            }

            let attachment_ids = if has_attachments {
                backend.create_attachment_records(&data_msg.attachments)
            } else {
                Vec::new()
            };

            let sent_at = inner_content.timestamp;
            let quote = QuoteContext::from_content(inner_content);

            match backend.store_data_message(
                id,
                sender_id,
                &data_msg.body,
                &attachment_ids,
                sent_at,
                quote,
            ) {
                Ok(message_id_str) => {
                    tracing::info!(
                        id = %id,
                        message_id = %message_id_str,
                        sent_at = sent_at,
                        has_text = has_text,
                        attachment_count = attachment_ids.len(),
                        label = backend.label(),
                        "Stored data message"
                    );
                    let message = build_message_from_content(
                        &message_id_str,
                        channel_id,
                        backend.conversation_type(),
                        *sender_id,
                        inner_content,
                        sent_at,
                    );
                    events.push(DomainEvent::MessageReceived { message });
                    // Clear typing indicator when message arrives — prevents stuck indicators.
                    events.push(DomainEvent::TypingStopped {
                        channel_id,
                        user_id: *sender_id,
                        timestamp: sent_at,
                    });
                }
                Err(e) => {
                    tracing::error!(
                        id = %id,
                        error = %e,
                        label = backend.label(),
                        "Failed to store data message"
                    );
                }
            }
            CommonDispatchOutcome::Handled(events)
        }

        Some(ContentKind::Typing(typing)) => {
            let started = typing.action == typing_message::Action::Started as i32;
            let (user_id, timestamp) = (*sender_id, typing.timestamp);
            let event = if started {
                DomainEvent::TypingStarted {
                    channel_id,
                    user_id,
                    timestamp,
                }
            } else {
                DomainEvent::TypingStopped {
                    channel_id,
                    user_id,
                    timestamp,
                }
            };
            events.push(event);
            CommonDispatchOutcome::Handled(events)
        }

        Some(ContentKind::Reaction(reaction)) => {
            let Ok(target_sender) = reaction.target_author_aci.parse::<UserId>() else {
                tracing::warn!("Rejected invalid anonymous reaction author");
                return CommonDispatchOutcome::Handled(events);
            };
            let sent_at_ns = reaction.target_sent_timestamp;

            // Admission can apply a batch of pending actors/emojis. Emit only
            // persisted snapshot identity, never an invented incoming-actor delta.
            let changed = match backend.receive_reaction(
                id,
                &target_sender,
                sent_at_ns,
                sender_id,
                &reaction.emoji,
                reaction.remove,
                reaction.timestamp,
            ) {
                Ok(changed) => changed,
                Err(error) => return CommonDispatchOutcome::Failed(error),
            };
            if changed {
                let event = DomainEvent::ReactionSnapshotChanged {
                    scope: crate::domain::reaction::ReactionScope::new(
                        (*id).into(),
                        backend.conversation_type(),
                    ),
                    sender_id: target_sender,
                    sent_at_ns,
                };
                events.push(event);
            }
            CommonDispatchOutcome::Handled(events)
        }

        Some(ContentKind::Receipt(receipt)) => {
            if receipt.r#type == receipt_message::Type::Read as i32 {
                backend.persist_delivery_status(id, &receipt.timestamps, "read");
                if backend.emit_receipts() {
                    events.push(DomainEvent::MessagesRead {
                        reader_id: *sender_id,
                        message_timestamps: receipt.timestamps.clone(),
                        receipt_timestamp: receipt.timestamp,
                        channel_id: Some(channel_id),
                    });
                }
            } else if receipt.r#type == receipt_message::Type::Delivery as i32 {
                backend.persist_delivery_status(id, &receipt.timestamps, "delivered");
                if backend.emit_receipts() {
                    events.push(DomainEvent::MessagesDelivered {
                        recipient_id: *sender_id,
                        message_timestamps: receipt.timestamps.clone(),
                        receipt_timestamp: receipt.timestamp,
                    });
                }
            }
            CommonDispatchOutcome::Handled(events)
        }

        Some(ContentKind::Pin(pin)) => {
            let pinned = !pin.unpin;
            if let Err(e) = backend.set_pinned(id, pin.target_sent_timestamp, pinned) {
                tracing::warn!(id = %id, error = %e, "Failed to persist pin state");
            }
            let message_id = backend
                .get_message_id_by_timestamp(id, pin.target_sent_timestamp)
                .unwrap_or_else(|| MessageId::from_timestamp(pin.target_sent_timestamp));

            if pin.unpin {
                events.push(DomainEvent::MessageUnpinned {
                    message_id,
                    unpinner_id: *sender_id,
                    channel_id,
                    conversation_type: backend.conversation_type(),
                    target_sent_timestamp: pin.target_sent_timestamp,
                });
            } else {
                events.push(DomainEvent::MessagePinned {
                    message_id,
                    pinner_id: *sender_id,
                    channel_id,
                    conversation_type: backend.conversation_type(),
                    target_sent_timestamp: pin.target_sent_timestamp,
                });
            }
            CommonDispatchOutcome::Handled(events)
        }

        Some(ContentKind::Edit(edit)) => {
            let target_timestamp = edit.target_sent_timestamp;
            let (new_text, edit_ts) = edit
                .data_message
                .as_ref()
                .map(|dm| (dm.body.as_str(), dm.timestamp))
                .unwrap_or_default();
            // A mailbox carries no server receive time: the edit's own
            // timestamp measures the 48h window.
            match backend.apply_edit(id, sender_id, target_timestamp, new_text, edit_ts) {
                Ok(Some(edited)) => {
                    events.push(DomainEvent::MessageEdited {
                        message_id: edited.id,
                        new_content: edited.content,
                        channel_id,
                        conversation_type: backend.conversation_type(),
                        author_id: *sender_id,
                        target_sent_timestamp: target_timestamp,
                    });
                }
                Ok(None) => tracing::debug!(
                    id = %id,
                    target_timestamp,
                    "Edit held for its message, refused, or already shown"
                ),
                Err(e) => tracing::warn!(id = %id, error = %e, "Failed to apply an edit"),
            }
            CommonDispatchOutcome::Handled(events)
        }

        _ => CommonDispatchOutcome::NotHandled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::anon_group::AnonGroupId;

    /// Backend whose delete reports whether a row matched.
    struct DeleteOnly {
        row_matched: bool,
    }

    impl AnonInboxBackend for DeleteOnly {
        type Id = AnonGroupId;
        fn conversation_type(&self) -> ConversationType {
            ConversationType::AnonGroup
        }
        fn label(&self) -> &'static str {
            "test"
        }
        fn create_attachment_records(&self, _: &[AttachmentPointer]) -> Vec<AttachmentId> {
            vec![]
        }
        fn store_data_message(
            &self,
            _: &AnonGroupId,
            _: &UserId,
            _: &str,
            _: &[AttachmentId],
            _: u64,
            _: Option<QuoteContext>,
        ) -> Result<String, String> {
            unreachable!()
        }
        fn delete_message(&self, _: &AnonGroupId, _: &UserId, _: u64) -> Result<bool, String> {
            Ok(self.row_matched)
        }
        fn apply_edit(
            &self,
            _: &AnonGroupId,
            _: &UserId,
            _: u64,
            _: &str,
            _: u64,
        ) -> Result<Option<Message>, String> {
            unreachable!()
        }
        fn add_reaction(
            &self,
            _: &AnonGroupId,
            _: &UserId,
            _: u64,
            _: &UserId,
            _: &str,
        ) -> Result<bool, String> {
            unreachable!()
        }
        fn remove_reaction(
            &self,
            _: &AnonGroupId,
            _: &UserId,
            _: u64,
            _: &UserId,
            _: &str,
        ) -> Result<bool, String> {
            unreachable!()
        }
        fn receive_reaction(
            &self,
            _: &Self::Id,
            _: &UserId,
            _: u64,
            _: &UserId,
            _: &str,
            _: bool,
            _: u64,
        ) -> Result<bool, crate::error::StorageError> {
            unreachable!()
        }
        fn set_pinned(&self, _: &AnonGroupId, _: u64, _: bool) -> Result<(), String> {
            unreachable!()
        }
        fn get_message_id_by_timestamp(&self, _: &AnonGroupId, _: u64) -> Option<MessageId> {
            None
        }
    }

    fn delete_events(row_matched: bool) -> usize {
        let content = Content {
            content: Some(content_v1::content::Content::DataMessage(
                content_v1::DataMessage {
                    delete: Some(content_v1::Delete {
                        target_sent_timestamp: 42,
                        target_author_aci: None,
                        destination_user_id: None,
                        target_call_id: None,
                    }),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        match dispatch_common_content(
            &AnonGroupId::new(),
            &UserId::new(),
            &content,
            &DeleteOnly { row_matched },
        ) {
            CommonDispatchOutcome::Handled(events) => events
                .iter()
                .filter(|e| matches!(e, DomainEvent::MessageDeleted { .. }))
                .count(),
            _ => panic!("delete must be handled"),
        }
    }

    #[test]
    fn delete_is_announced_only_when_a_row_was_deleted() {
        assert_eq!(delete_events(true), 1);
        // A member naming someone else's timestamp deletes nothing.
        assert_eq!(delete_events(false), 0);
    }

    #[test]
    fn group_delete_rejects_forged_author_even_when_sender_row_exists() {
        let sender = UserId::new();
        let wire = crate::services::gateway::builders::build_targeted_delete_content(
            42,
            Some(&UserId::new().to_string()),
            None,
        );
        let bytes = prost::Message::encode_to_vec(&wire);
        let content = <Content as prost::Message>::decode(bytes.as_slice()).unwrap();
        assert!(matches!(
            dispatch_common_content(
                &AnonGroupId::new(),
                &sender,
                &content,
                &DeleteOnly { row_matched: true }
            ),
            CommonDispatchOutcome::Dropped
        ));
    }

    /// An anonymous-group edit goes through the one edit choke point: held
    /// until its message arrives, author only, ordered by the edit's own
    /// time, within 48h, and a re-delivery announces nothing.
    #[tokio::test]
    async fn anon_group_edits_follow_the_edit_rules() {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let groups = client.anon_groups();
        let (group, author, stranger) = (AnonGroupId::new(), UserId::new(), UserId::new());
        let sent = 1_700_000_000_000_000_000u64;
        let window = crate::services::message::MessageService::EDIT_MAX_AGE_NS;
        let edit = |by: &UserId, body: &str, edit_ts: u64| {
            let content = Content {
                content: Some(content_v1::content::Content::Edit(
                    content_v1::EditMessage {
                        target_sent_timestamp: sent,
                        data_message: Some(content_v1::DataMessage {
                            body: body.into(),
                            timestamp: edit_ts,
                            ..Default::default()
                        }),
                    },
                )),
                ..Default::default()
            };
            match dispatch_common_content(&group, by, &content, groups.as_ref()) {
                CommonDispatchOutcome::Handled(events) => events
                    .iter()
                    .filter(|e| matches!(e, DomainEvent::MessageEdited { .. }))
                    .count(),
                _ => panic!("an edit is handled"),
            }
        };
        let stored = || {
            client
                .messages()
                .message_repo
                .get_by_sender_timestamp(&author, sent)
                .unwrap()
                .unwrap()
        };

        assert_eq!(edit(&author, "early", sent + 10), 0, "held for its message");
        groups
            .store_data_message(&group, &author, "original", &[], sent, None)
            .unwrap();
        assert_eq!(stored().content, MessageContent::Text("early".into()));

        assert_eq!(edit(&author, "newer", sent + 30), 1);
        assert_eq!(
            edit(&author, "older", sent + 20),
            0,
            "older than the edit shown"
        );
        assert_eq!(edit(&author, "newer", sent + 30), 0, "re-delivered");
        assert_eq!(edit(&stranger, "hijack", sent + 40), 0, "not the author");
        assert_eq!(edit(&author, "late", sent + window + 1), 0, "past 48h");
        let m = stored();
        assert_eq!(m.content, MessageContent::Text("newer".into()));
        assert_eq!(
            m.edited_at,
            Some(chrono::DateTime::from_timestamp_nanos((sent + 30) as i64))
        );
    }
}

#[cfg(test)]
mod reaction_receive_tests {
    use super::*;
    use crate::domain::anon_dm::{AnonDm, AnonDmStatus};
    use crate::domain::anon_group::{AnonGroup, AnonGroupMember, AnonGroupRole};
    use crate::domain::reaction::ReactionScope;
    #[tokio::test]
    async fn actual_anonymous_handlers_hold_early_reactions_and_surface_storage_failure() {
        static MOCK: std::sync::Once = std::sync::Once::new();
        MOCK.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder())
        });
        let dir = tempfile::tempdir().unwrap();
        let client = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .unwrap();
        let own = UserId::new();
        let actor = UserId::new();
        let author = UserId::new();
        let timestamp = 100;
        client
            .messages()
            .settings_repo
            .set("auth.current_user_id", &own.to_string())
            .unwrap();
        let dm_service = client.anon_dms();
        let groups = client.anon_groups();
        let mut dm = AnonDm::new_initiated(actor);
        dm.status = AnonDmStatus::Active;
        dm.peer_mailbox_pk = Some([1; 32]);
        dm_service.anon_dm_repo.insert_conversation(&dm).unwrap();
        let group = AnonGroup::new_as_owner("fixture group".into(), own);
        groups.anon_group_repo.insert_group(&group).unwrap();
        groups
            .anon_group_repo
            .add_member(&AnonGroupMember::new(
                group.id,
                actor,
                AnonGroupRole::Member,
            ))
            .unwrap();
        let content = Content {
            content: Some(content_v1::content::Content::Reaction(
                content_v1::Reaction {
                    emoji: "👍".into(),
                    target_author_aci: author.to_string(),
                    target_sent_timestamp: timestamp,
                    timestamp: 10,
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        assert!(
            dm_service
                .process_incoming_content(&dm.id, &actor, &content)
                .unwrap()
                .is_empty()
        );
        assert!(
            groups
                .process_incoming_content(&group.id, &actor, &content)
                .unwrap()
                .is_empty()
        );
        let scope_dm = ReactionScope::new(dm.id.into(), ConversationType::AnonDm);
        let scope_group = ReactionScope::new(group.id.into(), ConversationType::AnonGroup);
        for scope in [&scope_dm, &scope_group] {
            client
                .messages()
                .store_message_local(&Message::new_text(
                    scope.id,
                    scope.kind.clone(),
                    Some(author),
                    "same author and timestamp".into(),
                    timestamp,
                ))
                .unwrap();
            assert_eq!(
                client
                    .messages()
                    .message_repo
                    .get_reactions(scope, &author, timestamp)
                    .unwrap()
                    .len(),
                1
            );
        }
        assert!(
            dm_service
                .process_incoming_content(&dm.id, &actor, &content)
                .unwrap()
                .is_empty(),
            "duplicate does not increment"
        );
        let mut remove = content.clone();
        if let Some(content_v1::content::Content::Reaction(r)) = &mut remove.content {
            r.remove = true;
            r.timestamp = 20;
        }
        dm_service
            .process_incoming_content(&dm.id, &actor, &remove)
            .unwrap();
        assert!(
            client
                .messages()
                .message_repo
                .get_reactions(&scope_dm, &author, timestamp)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            client
                .messages()
                .message_repo
                .get_reactions(&scope_group, &author, timestamp)
                .unwrap()
                .len(),
            1
        );
        client.messages().settings_repo.database().connection().unwrap().execute_batch("CREATE TRIGGER fail_anon_projection BEFORE UPDATE OF reaction_counts_blob ON messages BEGIN SELECT RAISE(ABORT,'injected anonymous projection failure'); END;").unwrap();
        assert!(matches!(
            groups.process_incoming_content(&group.id, &actor, &remove),
            Err(crate::services::anon_group::AnonGroupError::Storage(_))
        ));
        assert_eq!(
            client
                .messages()
                .message_repo
                .get_reactions(&scope_group, &author, timestamp)
                .unwrap()
                .len(),
            1
        );
        client
            .messages()
            .settings_repo
            .database()
            .connection()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_anon_projection")
            .unwrap();
        groups
            .process_incoming_content(&group.id, &actor, &remove)
            .unwrap();
        assert!(
            client
                .messages()
                .message_repo
                .get_by_reaction_scope(&scope_group, &author, timestamp)
                .unwrap()
                .unwrap()
                .reaction_counts
                .is_empty()
        );
    }
}

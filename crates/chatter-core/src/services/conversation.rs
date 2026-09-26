//! Polymorphic conversation operations.
//!
//! `ConversationHandle` wraps `ActiveConversation` + `ChatterClient` to provide
//! a single API for messaging operations across all conversation types
//! (Channel, DM, Anon DM, Anon Group).
//!
//! # Usage
//!
//! ```ignore
//! let handle = ConversationHandle::current(&ui_ctx);
//! handle.send_reaction("👍", &author, timestamp, false).await?;
//! ```
//!
//! This eliminates the 4-way match blocks in the chat UI layer.

use std::sync::Arc;

use crate::client::ChatterClient;
use crate::domain::ids::{ChannelId, UserId};
use crate::domain::message::{ConversationType, UnreadAnchor};
use crate::state::ActiveConversation;

/// Unified error type for conversation operations.
pub type ConversationResult<T = ()> = Result<T, ConversationError>;

#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("No active conversation")]
    NoConversation,
    #[error("Anon DM error: {0}")]
    AnonDm(#[from] crate::services::AnonDmError),
    #[error("Anon group error: {0}")]
    AnonGroup(#[from] crate::services::AnonGroupError),
    #[error("Message error: {0}")]
    Message(#[from] crate::services::MessageError),
    #[error("{0}")]
    Other(String),
}

/// Re-shape stored attachments into send inputs that reuse the CDN blob they
/// already have, instead of re-uploading a local file that a forwarded message
/// (received, never on this disk) does not have in the first place.
///
/// An attachment whose local record lost its CDN key, blob key or digest cannot
/// be re-pointed and is dropped — a pointer the receiver cannot decrypt is
/// worse than a missing one.
fn forwardable_attachments(
    attachments: &[crate::domain::attachment::Attachment],
) -> Vec<crate::state::chat_state::PendingAttachment> {
    use crate::state::chat_state::PendingAttachment;
    attachments
        .iter()
        .filter_map(|att| {
            let pointer = att.to_pointer()?;
            Some(PendingAttachment::with_pointer(
                att.id,
                std::path::PathBuf::new(),
                att.metadata.name.clone(),
                att.metadata.size,
                att.metadata.mime_type.clone(),
                pointer,
            ))
        })
        .collect()
}

/// Polymorphic conversation operations.
///
/// Wraps `ActiveConversation` + `ChatterClient` to provide a single API
/// for all conversation types. The correct service is dispatched
/// internally based on the conversation variant.
#[derive(Clone)]
pub struct ConversationHandle {
    conversation: ActiveConversation,
    client: Arc<ChatterClient>,
}

impl ConversationHandle {
    /// Create a handle for a specific conversation.
    pub fn new(conversation: ActiveConversation, client: Arc<ChatterClient>) -> Self {
        Self { conversation, client }
    }

    /// Create for the current active conversation.
    pub fn current(client: &Arc<ChatterClient>) -> Self {
        Self {
            conversation: client.chat_state().active_conversation(),
            client: client.clone(),
        }
    }

    /// A handle for another conversation on the same client.
    pub fn for_conversation(&self, conversation: ActiveConversation) -> Self {
        Self { conversation, client: self.client.clone() }
    }

    /// Get the underlying conversation variant.
    pub fn conversation(&self) -> &ActiveConversation {
        &self.conversation
    }

    // ========================================================================
    // Ephemeral operations (reaction, typing, pin, edit, delete)
    // ========================================================================

    /// Toggle a reaction: query local state, decide add vs remove, then
    /// dispatch. Returns `true` if the new state is "reacted".
    ///
    /// UI callers should prefer this over [`send_reaction`] — it eliminates
    /// the "query `user_has_reacted` then send remove flag" pattern that
    /// used to live in the UI.
    pub async fn toggle_reaction(
        &self,
        emoji: &str,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> Result<bool, ConversationError> {
        // Per-user check — the summary aggregate would flip other users'
        // reactions off.
        let already = self
            .client
            .messages()
            .user_has_reacted(target_author, target_sent_timestamp, emoji)
            .unwrap_or(false);
        let remove = already;
        self.send_reaction(emoji, target_author, target_sent_timestamp, remove).await?;
        Ok(!remove)
    }

    /// Send a reaction.
    pub async fn send_reaction(
        &self,
        emoji: &str,
        target_author: &UserId,
        target_sent_timestamp: u64,
        remove: bool,
    ) -> ConversationResult {
        // Sub-services on the wire-format side still take &str (proto field
        // type); convert once and reuse.
        let target_author_str = target_author.to_string();
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.client.anon_dms()
                    .send_reaction(id, emoji, &target_author_str, target_sent_timestamp, remove)
                    .await?;
                // Persist own reaction
                if let Some(user_id) = self.client.auth_state().get().current_user_id {
                    if remove {
                        let _ = self.client.anon_dms().remove_reaction(id, target_author, target_sent_timestamp, &user_id, emoji);
                    } else {
                        let _ = self.client.anon_dms().add_reaction(id, target_author, target_sent_timestamp, &user_id, emoji);
                    }
                }
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.client.anon_groups()
                    .send_reaction(id, emoji, &target_author_str, target_sent_timestamp, remove)
                    .await?;
                // Persist own reaction
                if let Some(user_id) = self.client.auth_state().get().current_user_id {
                    if remove {
                        let _ = self.client.anon_groups().remove_reaction(id, target_author, target_sent_timestamp, &user_id, emoji);
                    } else {
                        let _ = self.client.anon_groups().add_reaction(id, target_author, target_sent_timestamp, &user_id, emoji);
                    }
                }
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            // DM (mailbox/server) and channel share build + persist; only the
            // channel embeds a channel_id and the transport differs.
            _ => {
                // Wire payload only needs target_author + target_sent_timestamp,
                // so don't block the send on a local DB lookup.
                let content = crate::services::build_reaction_content_with_channel(
                    emoji, remove, &target_author_str, target_sent_timestamp,
                    self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await?;
                // Best-effort local persist: no-op if the target message
                // isn't in our DB (receiver may have scrolled past it).
                //
                // MUST go through `record_reaction`, which writes
                // `unified_reactions` and rebuilds BOTH blobs on the message
                // row. The old path (`update_reaction_by_sender`) only bumped
                // the aggregate `reaction_counts`, leaving
                // `current_user_reactions_blob` empty — so `user_has_reacted`
                // always said no, `toggle_reaction` could only ever ADD, and
                // tapping your own reaction in a DM or channel incremented the
                // count instead of removing it. Observed on device: 1 → 2
                // (§112w).
                if let Some(user_id) = self.client.auth_state().get().current_user_id {
                    let _ = self.client.messages().record_reaction(
                        target_author, target_sent_timestamp, &user_id, emoji, remove,
                    );
                }
                Ok(())
            }
        }
    }

    /// Send a typing indicator.
    ///
    /// Privacy gate: honours the local "show typing indicators" setting. When
    /// the user has disabled it (`typing_indicators_enabled() == false`) we do
    /// NOT emit typing to the peer. Fail-safe default is ON (the shipped
    /// default and the getter's `unwrap_or(true)`), so a settings read error
    /// keeps the current behaviour rather than silently muting a user who never
    /// opted out.
    pub async fn send_typing(&self, started: bool) -> ConversationResult {
        if !self.client.typing_indicators_enabled().unwrap_or(true) {
            return Ok(());
        }
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.client.anon_dms().send_typing_indicator(id, started).await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.client.anon_groups().send_typing_indicator(id, started).await?;
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = crate::services::build_typing_content_with_channel(
                    started, self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await
            }
        }
    }

    /// Send a pin/unpin and persist locally.
    pub async fn send_pin(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
        unpin: bool,
    ) -> ConversationResult {
        let new_pinned = !unpin;
        // Sub-services on the proto side still take &str.
        let target_author_str = target_author.to_string();
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                match self.client.anon_dms().set_message_pinned(id, target_sent_timestamp, new_pinned) {
                    Ok(false) => tracing::warn!(conversation_id = %id, target_sent_timestamp, "AnonDm pin: 0 rows affected"),
                    Err(e) => tracing::warn!(conversation_id = %id, error = %e, "AnonDm pin: local persist failed"),
                    _ => {}
                }
                self.client.anon_dms()
                    .send_pin(id, &target_author_str, target_sent_timestamp, unpin)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                match self.client.anon_groups().set_message_pinned(id, target_sent_timestamp, new_pinned) {
                    Ok(false) => tracing::warn!(group_id = %id, target_sent_timestamp, "AnonGroup pin: 0 rows affected"),
                    Err(e) => tracing::warn!(group_id = %id, error = %e, "AnonGroup pin: local persist failed"),
                    _ => {}
                }
                self.client.anon_groups()
                    .send_pin(id, &target_author_str, target_sent_timestamp, unpin)
                    .await?;
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_pin_content_with_channel(
                    &author_aci, target_sent_timestamp, unpin,
                    self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await?;
                let _ = self.client.messages().set_message_pinned(&msg.id, new_pinned);
                Ok(())
            }
        }
    }

    /// Send an edit and persist locally.
    ///
    /// Emits `DomainEvent::MessageEdited` on success so UI subscribers
    /// refresh uniformly across conversation kinds — no UI-side branching
    /// on anon vs DM/channel needed.
    pub async fn send_edit(
        &self,
        target_sent_timestamp: u64,
        new_text: &str,
    ) -> ConversationResult {
        let result = self.send_edit_inner(target_sent_timestamp, new_text).await;
        if result.is_ok() {
            self.emit_local_edit_event(target_sent_timestamp, new_text);
        }
        result
    }

    fn emit_local_edit_event(&self, target_sent_timestamp: u64, new_text: &str) {
        use crate::domain::message::{ConversationType as CT, MessageContent};
        use crate::domain::ids::{ChannelId, MessageId};
        use crate::events::DomainEvent;

        let (channel_id, conversation_type) = match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                let cid: ChannelId = (*id).into();
                (cid, CT::AnonDm)
            }
            ActiveConversation::AnonGroup(id) => {
                let cid: ChannelId = (*id).into();
                (cid, CT::AnonGroup)
            }
            ActiveConversation::ServerDm { peer_id } => (ChannelId::from(*peer_id), CT::ServerDm),
            ActiveConversation::Channel(channel_id) => (*channel_id, CT::Channel),
            ActiveConversation::None => return,
        };
        let Some(author_id) = self.client.auth_state().get().current_user_id else { return };

        self.client.event_bus().emit_domain(DomainEvent::MessageEdited {
            message_id: MessageId::new(),
            new_content: MessageContent::Text(new_text.to_string()),
            channel_id,
            conversation_type,
            author_id,
            target_sent_timestamp,
        });
    }

    async fn send_edit_inner(
        &self,
        target_sent_timestamp: u64,
        new_text: &str,
    ) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                // anon_dms().send_edit() persists internally
                self.client.anon_dms()
                    .send_edit(id, target_sent_timestamp, new_text)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                // anon_groups().send_edit() persists internally
                self.client.anon_groups()
                    .send_edit(id, target_sent_timestamp, new_text)
                    .await?;
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = crate::services::gateway::builders::build_edit_content_with_channel(
                    target_sent_timestamp, new_text, self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await?;
                self.persist_edit(target_sent_timestamp, new_text);
                Ok(())
            }
        }
    }

    /// Send a delete-for-everyone and persist locally.
    pub async fn send_delete(&self, target_sent_timestamp: u64) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.client.anon_dms()
                    .send_delete_for_everyone(id, target_sent_timestamp)
                    .await?;
                // Persist locally
                if let Some(user_id) = self.client.auth_state().get().current_user_id {
                    let _ = self.client.anon_dms().delete_message_by_timestamp(id, &user_id, target_sent_timestamp);
                }
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.client.anon_groups()
                    .send_delete_for_everyone(id, target_sent_timestamp)
                    .await?;
                // Persist locally
                if let Some(user_id) = self.client.auth_state().get().current_user_id {
                    let _ = self.client.anon_groups().delete_message_by_timestamp(id, &user_id, target_sent_timestamp);
                }
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = crate::services::build_delete_for_everyone_content_with_channel(
                    target_sent_timestamp, self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await?;
                // Persist deletion locally (sender doesn't get their own delete back)
                self.delete_local_by_timestamp(target_sent_timestamp);
                Ok(())
            }
        }
    }

    /// Delete-for-me: delete locally + sync to own devices.
    pub async fn delete_for_me(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.client.anon_dms()
                    .delete_message_by_timestamp(id, target_author, target_sent_timestamp)
                    .map(|_| ())
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::AnonGroup(id) => {
                self.client.anon_groups()
                    .delete_message_by_timestamp(id, target_author, target_sent_timestamp)
                    .map(|_| ())
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_delete_for_me_content(
                    &self.dm_or_channel_id(), &author_aci, target_sent_timestamp,
                );
                // Our own other devices only: a peer's (or channel member's)
                // device would apply it and delete its own copy.
                let messages = self.client.messages();
                messages.sync_to_own_devices(&mut messages.auth_client(), content).await;
                messages.delete_message_local(&msg.id)
                    .map_err(ConversationError::Message)
            }
        }
    }

    /// Get message text by sender + timestamp across any conversation type.
    pub fn get_message_text(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<Option<String>> {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                match self.client.anon_dms().get_message_by_sender_timestamp(id, target_author, target_sent_timestamp) {
                    Ok(Some(msg)) => Ok(msg.as_text().map(|s| s.to_string())),
                    Ok(None) => Ok(None),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::AnonGroup(id) => {
                // Use timestamp-only lookup: the 3-field lookup (group_id + sender_id + sent_at)
                // fails due to sender_id byte encoding mismatch between storage and lookup paths.
                // Nanosecond timestamps are unique per group so timestamp-only is safe.
                match self.client.anon_groups().get_message_by_timestamp(id, target_sent_timestamp) {
                    Ok(Some(msg)) => Ok(msg.as_text().map(|s| s.to_string())),
                    Ok(None) => Ok(None),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_) => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                Ok(msg.as_text().map(|s| s.to_string()))
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Forward a message from this conversation to another one.
    ///
    /// Re-sends the source message's text AND its attachments to `target` as a
    /// fresh message (Signal-style — no forwarding envelope, the recipient sees
    /// a normal message). Attachments ride the pointer they were already
    /// uploaded with, so forwarding a photo costs no second upload.
    ///
    /// The quoted parent of a reply is deliberately not carried: the target
    /// conversation has no such message, and Signal drops it too.
    pub async fn forward_to(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
        target: ActiveConversation,
    ) -> ConversationResult {
        let text = self
            .get_message_text(target_author, target_sent_timestamp)?
            .unwrap_or_default();
        let attachment_ids =
            self.get_message_attachment_ids(target_author, target_sent_timestamp)?;
        let attachments: Vec<crate::domain::attachment::Attachment> = attachment_ids
            .iter()
            .filter_map(|id| self.client.attachments().get(id).ok().flatten())
            .collect();
        let pending = forwardable_attachments(&attachments);
        // An attachment-only message whose blobs can no longer be re-pointed
        // would send as an empty message; say so instead.
        if text.is_empty() && pending.is_empty() {
            return Err(ConversationError::Other(
                "message to forward has no content".into(),
            ));
        }
        self.client
            .send_message(target, &text, &pending, None, None)
            .await
            .map_err(|e| ConversationError::Other(e.to_string()))?;
        Ok(())
    }

    /// Get attachment IDs for a message by sender + timestamp across any conversation type.
    pub fn get_message_attachment_ids(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<Vec<crate::domain::AttachmentId>> {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                match self.client.anon_dms().get_message_by_sender_timestamp(id, target_author, target_sent_timestamp) {
                    Ok(Some(msg)) => Ok(msg.attachments),
                    Ok(None) => Ok(vec![]),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::AnonGroup(id) => {
                match self.client.anon_groups().get_message_by_timestamp(id, target_sent_timestamp) {
                    Ok(Some(msg)) => Ok(msg.attachments),
                    Ok(None) => Ok(vec![]),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_) => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                Ok(msg.attachments)
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Check if a message is owned by the current user.
    pub fn is_own_message(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<bool> {
        let current_user_id = self.client.auth_state().get().current_user_id;
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                match self.client.anon_dms().get_message_by_sender_timestamp(id, target_author, target_sent_timestamp) {
                    Ok(Some(msg)) => Ok(msg.sender_id == current_user_id),
                    Ok(None) => Err(ConversationError::Other("Message not found".into())),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::AnonGroup(id) => {
                // Use timestamp-only lookup to avoid sender_id byte mismatch.
                // Ownership is determined from the stored message's sender_id field.
                match self.client.anon_groups().get_message_by_timestamp(id, target_sent_timestamp) {
                    Ok(Some(msg)) => Ok(msg.sender_id == current_user_id),
                    Ok(None) => Err(ConversationError::Other("Message not found".into())),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_) => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                Ok(msg.sender_id == current_user_id)
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Whether the current user may delete a message for everyone.
    ///
    /// Only your own message, in every conversation (Signal). Keeps the
    /// capability rule in core.
    pub fn can_delete_for_everyone(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<bool> {
        // Signal: only your own messages, in every conversation. A peer's
        // device honours a delete only for the sender's own message, so
        // offering it on theirs would promise something that never happens.
        self.is_own_message(target_author, target_sent_timestamp)
    }

    // ========================================================================
    // Receipts
    // ========================================================================

    pub async fn send_read_receipt(&self, timestamps: &[u64]) -> ConversationResult {
        if timestamps.is_empty() { return Ok(()); }
        // Nothing is read while the app is off screen (backgrounded, locked,
        // unfocused) — even with this chat open behind it.
        if !self.client.chat_state().is_app_visible() {
            return Ok(());
        }
        // Privacy gate: honours the local "send read receipts" setting. When
        // disabled (`read_receipts_enabled() == false`) we do NOT tell the peer
        // we've read their message. Fail-safe default is ON (shipped default +
        // getter `unwrap_or(true)`).
        if !self.client.read_receipts_enabled().unwrap_or(true) {
            return Ok(());
        }
        match &self.conversation {
            ActiveConversation::AnonDm(dm_id) => {
                self.client.anon_dms()
                    .send_read_receipt(dm_id, timestamps.to_vec())
                    .await
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            // Groups don't track per-user read state to preserve sender anonymity.
            ActiveConversation::AnonGroup(_) => Ok(()),
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = crate::services::gateway::build_read_receipt_content_with_channel(
                    timestamps.to_vec(), self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await
            }
        }
    }

    pub async fn send_delivery_receipt(&self, timestamps: &[u64]) -> ConversationResult {
        if timestamps.is_empty() { return Ok(()); }
        match &self.conversation {
            ActiveConversation::AnonDm(dm_id) => {
                self.client.anon_dms()
                    .send_delivery_receipt(dm_id, timestamps.to_vec())
                    .await
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            // Groups don't track per-user delivery state to preserve sender anonymity.
            ActiveConversation::AnonGroup(_) => Ok(()),
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = crate::services::gateway::build_delivery_receipt_content_with_channel(
                    timestamps.to_vec(), self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await
            }
        }
    }

    /// Mark all messages in this conversation as read.
    ///
    /// Two halves, and the sidebar only ever sees the first one: the unread
    /// badge is a `COUNT(is_read = 0)` over the messages table (see
    /// `list_regular_dm_sidebar`, `channel_repo`, `conversation_stats`), so a
    /// receipt on the wire clears nothing locally — the badge came back on the
    /// next rebuild and survived restarts. The local write covers every
    /// conversation type; only the receipt is per-transport, and anon groups
    /// have no receipt at all (per-user read state would defeat the sender
    /// anonymity the group is for).
    /// Returns the [`UnreadAnchor`] that was just cleared, so the caller can
    /// open the conversation where the user left off. Read-receipt failures do
    /// not swallow it — the anchor is the caller's only chance at the boundary.
    pub async fn mark_as_read(&self) -> Result<UnreadAnchor, ConversationError> {
        // Off screen, nothing has been read. `ChatterClient::set_app_visible`
        // re-runs this for the open conversation when the app comes back.
        if !self.client.chat_state().is_app_visible() {
            return Ok(UnreadAnchor::default());
        }
        let (Some(conversation_id), Some(conversation_type)) = (
            self.conversation.channel_id(),
            self.client.conversation_type_of(&self.conversation),
        ) else {
            return Err(ConversationError::NoConversation);
        };

        let anchor = self
            .client
            .messages()
            .mark_conversation_read(&conversation_id, &conversation_type)?;

        if matches!(self.conversation, ActiveConversation::AnonGroup(_)) {
            return Ok(anchor);
        }

        // Receipt the peer's messages only — receipting our own back to
        // ourselves is noise. `send_read_receipt` holds the privacy gate and
        // picks the transport.
        let my_user_id = self.client.auth_state().get().current_user_id;
        let Ok(messages) = self.client.messages().get_messages_local(
            &conversation_id,
            &conversation_type,
            None,
            50,
        ) else {
            return Ok(anchor);
        };
        let timestamps: Vec<u64> = messages
            .iter()
            .filter(|m| m.sender_id != my_user_id)
            .map(|m| m.sent_at)
            .collect();
        // Off the caller's path: opening a conversation awaits this call for
        // its anchor, and a receipt is a network send (session, sealed
        // sender, or the Shield mailbox). Awaited here, a slow peer held the
        // previous conversation on screen until it finished.
        let handle = self.clone();
        tokio::spawn(async move {
            let _ = handle.send_read_receipt(&timestamps).await;
        });
        Ok(anchor)
    }

    // ========================================================================
    // Conversation-level operations
    // ========================================================================

    /// Mute or unmute this conversation's notifications.
    ///
    /// For anon groups, uses the group status (Muted vs Active). Everything
    /// else stores a per-conversation settings key.
    pub fn mute_conversation(&self, muted: bool) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                let key = format!("conv.muted.anon_dm.{}", id);
                self.client.channels().settings_repo
                    .set(&key, &muted)
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::AnonGroup(id) => {
                if muted {
                    self.client.anon_groups().mute_group(id)?;
                } else {
                    self.client.anon_groups().unmute_group(id)?;
                }
                Ok(())
            }
            ActiveConversation::ServerDm { peer_id } => {
                // Channel-free DM: no membership row to flip. Mirrors the
                // AnonDm pattern — a per-peer settings key.
                let key = format!("conv.muted.server_dm.{}", peer_id);
                self.client.channels().settings_repo
                    .set(&key, &muted)
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::Channel(channel_id) => {
                // Not the membership's `is_muted`: that column is the
                // moderator mute (server `ChannelMember.muted`, moderation
                // events), and sharing it let either one overwrite the other.
                let key = format!("conv.muted.channel.{}", channel_id);
                self.client.channels().settings_repo
                    .set(&key, &muted)
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Read this conversation's persisted mute state (the inverse of
    /// [`Self::mute_conversation`]). Reads exactly the same store the setter
    /// wrote — the group status for anon groups, otherwise the settings key. Defaults to `false` (unmuted) when nothing has been persisted.
    pub fn is_muted(&self) -> Result<bool, ConversationError> {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                let key = format!("conv.muted.anon_dm.{}", id);
                Ok(self.client.channels().settings_repo
                    .get_bool(&key)
                    .map_err(|e| ConversationError::Other(e.to_string()))?
                    .unwrap_or(false))
            }
            ActiveConversation::AnonGroup(id) => {
                Ok(self.client.anon_groups()
                    .get_group(id)
                    .map_err(|e| ConversationError::Other(e.to_string()))?
                    .map(|g| g.status == crate::domain::anon_group::AnonGroupStatus::Muted)
                    .unwrap_or(false))
            }
            ActiveConversation::ServerDm { peer_id } => {
                let key = format!("conv.muted.server_dm.{}", peer_id);
                Ok(self.client.channels().settings_repo
                    .get_bool(&key)
                    .map_err(|e| ConversationError::Other(e.to_string()))?
                    .unwrap_or(false))
            }
            ActiveConversation::Channel(channel_id) => {
                let key = format!("conv.muted.channel.{}", channel_id);
                Ok(self.client.channels().settings_repo
                    .get_bool(&key)
                    .map_err(|e| ConversationError::Other(e.to_string()))?
                    .unwrap_or(false))
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Delete a DM on this device (the sidebar's "Delete chat"). Local only;
    /// the peer is not told, as in Signal.
    ///
    /// A friend DM loses its history and stays listed (the friendship is what
    /// lists it); an anon DM goes entirely, its peer block staying; a channel
    /// we are no longer in goes entirely. Groups and live channels are
    /// refused: a membership outlives a local delete — the next sync or group
    /// message brings it back — so those are left (Channel Settings › Leave),
    /// never deleted here.
    pub fn delete_conversation(&self) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                Ok(self.client.anon_dms().delete_conversation(id)?)
            }
            ActiveConversation::ServerDm { peer_id } => {
                // Channel-free DM: no channel/membership row to cascade —
                // delete the local message history directly, then the
                // attachments only this chat carried (row and file).
                let channel_id: ChannelId = (*peer_id).into();
                let messages = self.client.messages()
                    .get_messages_local(&channel_id, &ConversationType::ServerDm, None, u32::MAX)
                    .map_err(ConversationError::Message)?;
                let attachments: Vec<_> = messages
                    .iter()
                    .flat_map(|m| m.attachments.iter().copied())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                let ids: Vec<_> = messages.into_iter().map(|m| m.id).collect();
                let storage = |e: crate::error::StorageError| ConversationError::Other(e.to_string());
                self.client.messages().message_repo.delete_batch(&ids).map_err(storage)?;
                self.client.attachments().attachment_repo()
                    .delete_unreferenced(&attachments)
                    .map_err(storage)
            }
            // A channel we are no longer in: its history, then the channel.
            ActiveConversation::Channel(channel_id) if self.client.is_former_channel(channel_id) => {
                let storage = |e: crate::error::StorageError| ConversationError::Other(e.to_string());
                let messages = self.client.messages()
                    .get_messages_local(channel_id, &ConversationType::Channel, None, u32::MAX)
                    .map_err(ConversationError::Message)?;
                let attachments: Vec<_> = messages
                    .iter()
                    .flat_map(|m| m.attachments.iter().copied())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                let ids: Vec<_> = messages.into_iter().map(|m| m.id).collect();
                self.client.messages().message_repo.delete_batch(&ids).map_err(storage)?;
                self.client.attachments().attachment_repo()
                    .delete_unreferenced(&attachments)
                    .map_err(storage)?;
                self.client.channels().channel_repo.delete_channel_cascade(channel_id).map_err(storage)
            }
            ActiveConversation::AnonGroup(_) | ActiveConversation::Channel(_) => Err(
                ConversationError::Other("a group is left, not deleted".into()),
            ),
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// This conversation's disappearing-messages timer in seconds (`0` = off).
    ///
    /// Friend DMs hold it per conversation (`services::disappearing`); a
    /// channel's is its server-held `config.ttl`. Anonymous conversations have
    /// no timer.
    pub fn disappearing_timer(&self) -> u32 {
        match &self.conversation {
            ActiveConversation::ServerDm { peer_id } => crate::services::disappearing::server_dm_timer(
                &self.client.channels().settings_repo,
                peer_id,
            ),
            ActiveConversation::Channel(channel_id) => self
                .client
                .channels()
                .channel_repo
                .get(channel_id)
                .ok()
                .flatten()
                .and_then(|c| c.config.default_ttl_seconds)
                .unwrap_or(0),
            _ => 0,
        }
    }

    /// Whether this user can change the timer here: friend DMs only. A
    /// channel's timer is fixed when it is created (the server refuses to
    /// change it after), and anonymous conversations do not carry one.
    pub fn can_set_disappearing_timer(&self) -> bool {
        matches!(self.conversation, ActiveConversation::ServerDm { .. })
    }

    /// Set this conversation's disappearing-messages timer (`0` = off).
    ///
    /// Friend DM: Signal semantics — adopted locally, a "You set disappearing
    /// messages to …" notice is filed and shown, and the friend is sent a
    /// timer-update so both sides expire messages on the same timer.
    ///
    /// Channel: writes the local copy of the channel's ttl only. The server
    /// holds the real value and refuses to change it after creation, so this
    /// exists for the V29 proof (`tests/validated.rs`), not for the UI —
    /// `can_set_disappearing_timer` is false for channels.
    pub async fn set_disappearing_timer(&self, seconds: u32) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::ServerDm { peer_id } => {
                let notice = self
                    .client
                    .messages()
                    .set_server_dm_disappearing_timer(peer_id, seconds)
                    .await
                    .map_err(ConversationError::Message)?;
                if let Some(message) = notice {
                    self.client.event_bus().emit_domain(
                        crate::inbox::handlers::DomainEvent::MessageReceived { message },
                    );
                }
                Ok(())
            }
            ActiveConversation::Channel(channel_id) => {
                let ttl = if seconds == 0 { None } else { Some(seconds) };
                self.client.channels().channel_repo
                    .set_default_ttl(channel_id, ttl)
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::AnonDm(_) | ActiveConversation::AnonGroup(_) => Err(
                ConversationError::Other(
                    "disappearing messages are not available in anonymous conversations".into(),
                ),
            ),
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    // ========================================================================
    // Internal helpers
    // ========================================================================

    /// Look up a message by sender + timestamp (for standard DM/Channel paths).
    /// Persist an edit to local DB (for sender's own edits).
    fn persist_edit(&self, sent_timestamp: u64, new_text: &str) {
        if let Some(user_id) = self.client.auth_state().get().current_user_id {
            if let Ok(Some(msg)) = self.client.messages().get_by_sender_timestamp(&user_id, sent_timestamp) {
                let new_content = crate::domain::message::MessageContent::Text(new_text.to_string());
                let _ = self.client.messages().update_message_body(&msg.id, &new_content);
            }
        }
    }

    /// Delete a message from local DB by timestamp (for sender's own deletes).
    fn delete_local_by_timestamp(&self, sent_timestamp: u64) {
        if let Some(user_id) = self.client.auth_state().get().current_user_id {
            if let Ok(Some(msg)) = self.client.messages().get_by_sender_timestamp(&user_id, sent_timestamp) {
                let _ = self.client.messages().delete_message_local(&msg.id);
            }
        }
    }

    fn lookup_message(
        &self,
        sender_id: &UserId,
        sent_timestamp: u64,
    ) -> ConversationResult<crate::domain::Message> {
        self.client.messages()
            .get_by_sender_timestamp(sender_id, sent_timestamp)
            .map_err(|e| ConversationError::Message(e))?
            .ok_or_else(|| ConversationError::Other("Message not found".into()))
    }

    /// Send content to a friend DM. The transport (Shield: mailbox; normal:
    /// sealed-sender streaming) is `send_friend_dm_content`'s decision.
    async fn send_friend_dm_content(
        &self,
        content: crate::proto::content_v1::Content,
        peer_id: &UserId,
    ) -> ConversationResult {
        self.client.messages()
            .send_friend_dm_content(peer_id, content)
            .await
            .map_err(ConversationError::Message)
    }

    /// Send control content via channel (sender keys).
    async fn send_channel_content(
        &self,
        content: crate::proto::content_v1::Content,
        channel_id: &ChannelId,
    ) -> ConversationResult {
        self.client.messages()
            .send_control(channel_id, content, None)
            .await
            .map(|_| ())
            .map_err(ConversationError::Message)
    }

    /// The active channel's id as a string, or `None` for DM conversations.
    /// Channel builders embed it for routing; DM builders leave it empty.
    fn active_channel_id(&self) -> Option<String> {
        match &self.conversation {
            ActiveConversation::Channel(cid) => Some(cid.to_string()),
            _ => None,
        }
    }

    /// The conversation-id payload field for delete-for-me sync: the peer's
    /// id for DMs, the channel id for channels.
    fn dm_or_channel_id(&self) -> String {
        match &self.conversation {
            ActiveConversation::ServerDm { peer_id } => peer_id.to_string(),
            ActiveConversation::Channel(cid) => cid.to_string(),
            _ => String::new(),
        }
    }

    /// Route already-built `Content` to the transport for the active DM or
    /// channel conversation. Valid only for ServerDm / Channel;
    /// anon and None return `NoConversation`.
    async fn dispatch_content(
        &self,
        content: crate::proto::content_v1::Content,
    ) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::ServerDm { peer_id } => self.send_friend_dm_content(content, peer_id).await,
            ActiveConversation::Channel(channel_id) => self.send_channel_content(content, channel_id).await,
            _ => Err(ConversationError::NoConversation),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attachment::{Attachment, FileMetadata};
    use crate::domain::ids::AttachmentId;

    fn received_photo() -> Attachment {
        // The shape a received attachment has locally: no file on this disk,
        // only the CDN blob and the material to decrypt it.
        let mut att = Attachment::new(
            AttachmentId::from_cdn_key("cdn-key-1"),
            FileMetadata::new("photo.jpg".into(), "image/jpeg".into(), 4096),
        );
        att.remote_url = Some("cdn-key-1".into());
        att.encryption_key = Some(vec![7u8; 64]);
        att.encryption_iv = Some(vec![9u8; 32]);
        att
    }

    /// Forwarding a photo must re-send the CDN pointer it already has. Before
    /// this, `forward_to` was text-only: an attachment-only message resolved to
    /// no text and the forward failed silently.
    #[test]
    fn forwarded_attachment_reuses_its_existing_pointer() {
        let out = forwardable_attachments(&[received_photo()]);
        assert_eq!(out.len(), 1, "the photo was dropped from the forward");
        let pointer = out[0].pointer.as_ref().expect("forward must not re-upload");
        assert_eq!(pointer.cdn_key, "cdn-key-1");
        assert_eq!(pointer.file_name, "photo.jpg");
        assert_eq!(pointer.plaintext_size, 4096);
        assert!(out[0].path.as_os_str().is_empty(), "there is no local file to upload");
    }

    /// A record that can no longer be re-pointed (no digest, so the receiver
    /// could not verify the blob) is dropped rather than shipped broken.
    #[test]
    fn unpointable_attachment_is_dropped() {
        let mut att = received_photo();
        att.encryption_iv = None;
        assert!(forwardable_attachments(&[att]).is_empty());
    }

    // ========================================================================
    // Read state
    // ========================================================================

    /// A client with a temp datastore and nothing behind its server URL — the
    /// storage half of `mark_as_read` is what these tests are about. Keyring is
    /// pointed at its in-memory mock (same reason as `client::tests`).
    fn test_client() -> (Arc<ChatterClient>, tempfile::TempDir) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });

        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("http://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        // Turn the wire half off at its privacy gate — there is no server here.
        client
            .set_read_receipts_enabled(false)
            .expect("read-receipt setting");
        (client, dir)
    }

    fn store_peer_message(
        client: &ChatterClient,
        conversation_id: ChannelId,
        conversation_type: ConversationType,
        peer: UserId,
        n: u64,
    ) {
        let message = crate::domain::message::Message::new_text(
            conversation_id,
            conversation_type,
            Some(peer),
            format!("message {n}"),
            n,
        );
        client.messages().message_repo.insert(&message).expect("store message");
    }

    fn unread(client: &ChatterClient, id: &ChannelId, conv_type: &ConversationType) -> u64 {
        client
            .messages()
            .message_repo
            .get_unread_count(id, conv_type)
            .expect("unread count")
    }

    /// A friend DM's transport is Shield mode's call, made in one place.
    /// Shield: the mailbox — here first contact with no cached device, so the
    /// mailbox bootstrap refuses (`NotReachablePrivately`) and it never falls
    /// back to sealed-sender streaming. Normal: sealed sender, and nothing
    /// touches the mailbox queue.
    #[tokio::test]
    async fn friend_dm_transport_follows_shield_mode() {
        use crate::storage::repositories::settings_repo::keys;
        use std::sync::atomic::Ordering;

        let (client, _dir) = test_client();
        let ms = client.messages();
        // We are the higher UUID: the Shield send would ask the friend to
        // initiate — but no device of theirs is cached yet.
        let me = UserId::from_uuid(uuid::Uuid::max());
        let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
        ms.settings_repo.set_string(keys::CURRENT_USER_ID, &me.to_string()).unwrap();
        ms.settings_repo.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();
        let content = || crate::services::build_text_content("hi", 1);

        ms.shield_mode.store(true, Ordering::Release);
        let shield = ms.send_friend_dm_content(&friend, content()).await;
        assert!(
            matches!(shield, Err(crate::services::message::MessageError::NotReachablePrivately(_))),
            "Shield must take the mailbox, got {shield:?}"
        );
        assert!(ms.sesame_manager.drain_pending_messages(&friend).unwrap().is_empty());

        ms.shield_mode.store(false, Ordering::Release);
        let normal = ms.send_friend_dm_content(&friend, content()).await;
        assert!(
            !matches!(normal, Err(crate::services::message::MessageError::BootstrapInitiated)),
            "normal mode must not take the mailbox"
        );
        assert!(ms.sesame_manager.drain_pending_messages(&friend).unwrap().is_empty());
    }

    /// Opening a DM has to leave the sidebar badge at zero. The badge is a
    /// `COUNT(is_read = 0)` rebuilt from storage on every refresh and every
    /// restart, so a `mark_as_read` that only put a receipt on the wire left it
    /// lit forever — the DM stuck on 5 that reopening would not clear.
    #[tokio::test]
    async fn opening_a_server_dm_clears_its_unread_count() {
        let (client, _dir) = test_client();
        let peer = UserId::new();
        let conversation_id: ChannelId = peer.into();
        for n in 1..=5 {
            store_peer_message(&client, conversation_id, ConversationType::ServerDm, peer, n);
        }
        assert_eq!(
            unread(&client, &conversation_id, &ConversationType::ServerDm),
            5,
            "positive control: the peer's messages must start unread"
        );

        client
            .conversation(ActiveConversation::ServerDm { peer_id: peer })
            .mark_as_read()
            .await
            .expect("mark as read");

        assert_eq!(
            unread(&client, &conversation_id, &ConversationType::ServerDm),
            0,
            "opening the DM must persist the read state, not only receipt it"
        );
    }

    /// Same defect, same fix, on the channel path.
    #[tokio::test]
    async fn opening_a_channel_clears_its_unread_count() {
        let (client, _dir) = test_client();
        let channel_id = ChannelId::new();
        let peer = UserId::new();
        for n in 1..=3 {
            store_peer_message(&client, channel_id, ConversationType::Channel, peer, n);
        }
        assert_eq!(unread(&client, &channel_id, &ConversationType::Channel), 3);

        client
            .conversation(ActiveConversation::Channel(channel_id))
            .mark_as_read()
            .await
            .expect("mark as read");

        assert_eq!(
            unread(&client, &channel_id, &ConversationType::Channel),
            0,
            "opening the channel must persist the read state"
        );
    }

    /// A message we sent is read by definition. Stored unread, it badged the
    /// conversation the user was typing in — invisibly until the next sidebar
    /// rebuild, then permanently.
    #[tokio::test]
    async fn our_own_sent_message_never_counts_as_unread() {
        let (client, _dir) = test_client();
        let me = UserId::new();
        let conversation_id: ChannelId = UserId::new().into();
        let message = crate::domain::message::Message::new_text(
            conversation_id,
            ConversationType::ServerDm,
            Some(me),
            "sent by us".to_string(),
            1,
        );
        client
            .messages()
            .message_repo
            .insert_outgoing(&message)
            .expect("store local echo");

        assert_eq!(
            unread(&client, &conversation_id, &ConversationType::ServerDm),
            0,
            "our own local echo must not badge the conversation"
        );
    }

    /// Deleting an anon DM takes its whole local footprint: messages, the
    /// attachments only it carried (row and file), the conversation row. An
    /// attachment a forward put in another conversation stays, and a blocked
    /// peer keeps their name in Friends › Blocked.
    #[tokio::test]
    async fn delete_anon_dm_removes_messages_and_files_and_keeps_blocked_name() {
        use crate::domain::anon_dm::AnonDm;
        use crate::domain::ids::ChannelId;
        use crate::domain::message::{ConversationType, Message};

        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("http://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");

        let peer = UserId::from_uuid(uuid::Uuid::from_u128(7));
        let mut dm = AnonDm::new_initiated(peer);
        dm.peer_display_name = Some("Robin".into());
        client.anon_dms().anon_dm_repo().insert_conversation(&dm).unwrap();
        client.anon_dms().block_user(&peer, None).unwrap();

        let attachments = client.attachments().attachment_repo();
        let stored = |name: &str| {
            let id = AttachmentId::new();
            let path = dir.path().join(format!("{id}.enc"));
            std::fs::write(&path, b"ciphertext").unwrap();
            let att = Attachment::new(id, FileMetadata::new(name.into(), "image/jpeg".into(), 10));
            attachments.insert_with_file_path(&att, &path).unwrap();
            (id, path)
        };
        let (only_here, only_here_path) = stored("a.jpg");
        let (forwarded, forwarded_path) = stored("b.jpg");

        let conv: ChannelId = dm.id.into();
        let mut msg = Message::new_text(conv, ConversationType::AnonDm, Some(peer), "hi".into(), 1);
        msg.attachments = vec![only_here, forwarded];
        client.messages().message_repo.insert(&msg).unwrap();
        let elsewhere: ChannelId = UserId::from_uuid(uuid::Uuid::from_u128(8)).into();
        let mut fwd = Message::new_text(elsewhere, ConversationType::ServerDm, None, String::new(), 2);
        fwd.attachments = vec![forwarded];
        client.messages().message_repo.insert(&fwd).unwrap();

        client.delete_conversation(ActiveConversation::AnonDm(dm.id)).unwrap();

        assert!(client.anon_dms().get_conversation(&dm.id).unwrap().is_none());
        assert!(client.messages().message_repo.get(&msg.id).unwrap().is_none());
        assert!(attachments.get(&only_here).unwrap().is_none());
        assert!(!only_here_path.exists(), "the deleted chat's file stayed on disk");
        assert!(attachments.get(&forwarded).unwrap().is_some());
        assert!(forwarded_path.exists(), "a file another chat still shows was deleted");
        assert!(client.messages().message_repo.get(&fwd.id).unwrap().is_some());
        assert_eq!(client.display_name(&peer), "Robin");
        assert!(client.is_profile_user_blocked(&peer));
    }

    /// Deleting a friend DM empties its history and removes the attachment
    /// files only it carried; a file a forward shares elsewhere stays.
    #[tokio::test]
    async fn delete_friend_dm_removes_messages_and_own_files() {
        use crate::domain::message::Message;

        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("http://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");

        let attachments = client.attachments().attachment_repo();
        let stored = |name: &str| {
            let id = AttachmentId::new();
            let path = dir.path().join(format!("{id}.enc"));
            std::fs::write(&path, b"ciphertext").unwrap();
            let att = Attachment::new(id, FileMetadata::new(name.into(), "image/jpeg".into(), 10));
            attachments.insert_with_file_path(&att, &path).unwrap();
            (id, path)
        };
        let (only_here, only_here_path) = stored("a.jpg");
        let (forwarded, forwarded_path) = stored("b.jpg");

        let peer = UserId::from_uuid(uuid::Uuid::from_u128(11));
        let conv: ChannelId = peer.into();
        let mut msg = Message::new_text(conv, ConversationType::ServerDm, Some(peer), "hi".into(), 1);
        msg.attachments = vec![only_here, forwarded];
        client.messages().message_repo.insert(&msg).unwrap();
        let elsewhere: ChannelId = UserId::from_uuid(uuid::Uuid::from_u128(12)).into();
        let mut fwd = Message::new_text(elsewhere, ConversationType::ServerDm, None, String::new(), 2);
        fwd.attachments = vec![forwarded];
        client.messages().message_repo.insert(&fwd).unwrap();

        client.delete_conversation(ActiveConversation::ServerDm { peer_id: peer }).unwrap();

        assert!(client.messages().message_repo.get(&msg.id).unwrap().is_none());
        assert!(attachments.get(&only_here).unwrap().is_none());
        assert!(!only_here_path.exists(), "the deleted chat's file stayed on disk");
        assert!(attachments.get(&forwarded).unwrap().is_some());
        assert!(forwarded_path.exists(), "a file another chat still shows was deleted");
        assert!(client.messages().message_repo.get(&fwd.id).unwrap().is_some());
    }
}

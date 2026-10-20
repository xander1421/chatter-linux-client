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

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::client::ChatterClient;
use crate::domain::ids::{ChannelId, UserId};
use crate::domain::message::{ConversationType, MessageMenu, UnreadAnchor};
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
    #[error("Channel error: {0}")]
    Channel(#[from] crate::services::channel::ChannelError),
    #[error("Storage error: {0}")]
    Storage(#[from] crate::error::StorageError),
    #[error("{0}")]
    Other(String),
}

/// Scope and outcome of a selected-message deletion. The input IDs are the
/// stored message IDs in display order, rather than sender/timestamp pairs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectedDeleteScope {
    ForMe,
    ForEveryone,
}

#[derive(Debug)]
pub enum SelectedDeleteFailureReason {
    Unavailable,
    NotEligible,
    SessionChanged,
    Operation(ConversationError),
    NotAttempted,
}

impl std::fmt::Display for SelectedDeleteFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => write!(f, "The selected messages are no longer available."),
            Self::NotEligible => write!(f, "These messages can no longer be deleted for everyone."),
            Self::SessionChanged => write!(f, "The login changed before deletion finished."),
            Self::Operation(error) => write!(f, "{error}"),
            Self::NotAttempted => write!(f, "Deletion stopped after an earlier failure."),
        }
    }
}

#[derive(Debug)]
pub struct SelectedDeleteFailure {
    pub id: String,
    pub reason: SelectedDeleteFailureReason,
}

#[derive(Debug, Default)]
pub struct SelectedDeleteResult {
    pub deleted_ids: Vec<String>,
    pub failed_ids: Vec<SelectedDeleteFailure>,
    pub partial: bool,
}

impl SelectedDeleteResult {
    fn fail_all(ids: &[String], reason: SelectedDeleteFailureReason) -> Self {
        let mut reason = Some(reason);
        let failed_ids = ids
            .iter()
            .map(|id| SelectedDeleteFailure {
                id: id.clone(),
                reason: reason
                    .take()
                    .unwrap_or(SelectedDeleteFailureReason::NotAttempted),
            })
            .collect();
        Self {
            failed_ids,
            ..Self::default()
        }
    }
}

#[derive(Clone)]
pub(crate) struct PendingReceipt {
    conversation: ActiveConversation,
    read: bool,
    revision: u64,
    // Only channel delivery survives same-account publication revisions.
    delivery_owner: Option<crate::domain::ids::UserId>,
    shield: bool,
    timestamps: BTreeSet<u64>,
}

impl PendingReceipt {
    pub(crate) fn is_read(&self) -> bool {
        self.read
    }
}

fn retryable_receipt_error(error: &ConversationError) -> bool {
    match error {
        ConversationError::Message(error) => error.permanent_send_failure_reason().is_none(),
        // The Shield mailbox currently stringifies transport errors at its boundary.
        ConversationError::AnonDm(crate::services::AnonDmError::MessageError(_)) => true,
        _ => false,
    }
}

fn record_sent_receipt(client: &ChatterClient, entry: &PendingReceipt) {
    let mut sent = client.sent_receipts.lock();
    sent.retain(|item| item.revision == entry.revision);
    if let Some(existing) = sent
        .iter_mut()
        .find(|item| item.conversation == entry.conversation && item.read == entry.read)
    {
        existing.timestamps.extend(entry.timestamps.iter().copied());
        // Reopened chats load only the newest page. Keep recent timestamps so
        // this session's receipt cache stays bounded during long runs.
        while existing.timestamps.len() > 256 {
            existing.timestamps.pop_first();
        }
    } else {
        let mut entry = entry.clone();
        while entry.timestamps.len() > 256 {
            entry.timestamps.pop_first();
        }
        sent.push(entry);
    }
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

/// Resolve a UI channel to Saved storage only with authenticated own identity
/// and matching local channel metadata. Never probe alternative message kinds.
fn message_storage_scope(
    conversation: &ActiveConversation,
    own_user: Option<UserId>,
    channel: Option<&crate::domain::channel::Channel>,
) -> ConversationResult<crate::domain::reaction::ReactionScope> {
    let (mut kind, id) = conversation
        .routing_key()
        .ok_or(ConversationError::NoConversation)?;
    let id: ChannelId = id
        .parse()
        .map_err(|_| ConversationError::Other("Invalid conversation".into()))?;
    if matches!(conversation, ActiveConversation::Channel(_))
        && own_user.is_some_and(|own| ChannelId::from(own) == id)
        && channel.is_some_and(|channel| {
            channel.id == id
                && channel.channel_type == crate::domain::channel::ChannelType::SavedMessages
        })
    {
        kind = ConversationType::SavedMessages;
    }
    Ok(crate::domain::reaction::ReactionScope::new(id, kind))
}

fn scoped_stored_message(
    repo: &crate::storage::MessageRepository,
    scope: &crate::domain::reaction::ReactionScope,
    author: &UserId,
    sent_at: u64,
) -> ConversationResult<crate::domain::Message> {
    repo.get_by_reaction_scope(scope, author, sent_at)?
        .ok_or_else(|| ConversationError::Other("Message not found".into()))
}

fn stored_message_menu(message: &crate::domain::Message, is_own: bool, now_ns: u64) -> MessageMenu {
    let mut actions = message.actions(is_own, now_ns);
    if !message.conversation_type.can_forward_from() {
        actions.forward = false;
    }
    if !matches!(
        message.conversation_type,
        ConversationType::ServerDm | ConversationType::Channel
    ) {
        actions.resend = false;
    }
    // Saved records have only a local/own-device deletion path. Their UI
    // Channel variant must never grant a public-channel remote delete. Edit
    // is a local rewrite of our own note (`send_edit`), so the remote 24h
    // window does not apply: own, delivered, undeleted, with a body. Pin is
    // the same local flip (`send_pin`) and keeps `decide`'s rule: a stored,
    // undeleted row.
    if message.conversation_type == ConversationType::SavedMessages {
        actions.delete_for_everyone = false;
        actions.react = false;
        actions.edit = is_own
            && !message.is_deleted
            && !matches!(message.delivery_status.as_str(), "failed" | "sending")
            && message.as_text().is_some_and(|t| !t.trim().is_empty());
    }
    MessageMenu {
        actions,
        pinned: message.is_pinned,
    }
}

/// The full Saved row as it is stored after our edit: the same row with only
/// its body (the text, or the caption of a `Mixed` body whose attachment ids
/// stay) replaced, `is_edited` set and `edited_at` the real edit time. Id,
/// `sent_at`, `attachments`, reply identity and every other column are the
/// stored values. Gated exactly like the menu's `edit` flag for Saved
/// (`stored_message_menu`: own, delivered, undeleted, has a body; no age
/// window), plus a non-empty new text, so the menu and the send cannot drift.
fn edited_saved_record(
    message: &crate::domain::Message,
    own: UserId,
    new_text: &str,
) -> ConversationResult<crate::domain::Message> {
    use crate::domain::message::{MessageContent, MixedContent};
    if message.conversation_type != ConversationType::SavedMessages
        || message.sender_id != Some(own)
    {
        return Err(ConversationError::Other(
            "Only your own Saved message can be edited".into(),
        ));
    }
    let trimmed = new_text.trim();
    if trimmed.is_empty() {
        return Err(ConversationError::Other(
            "An edited Saved message needs text".into(),
        ));
    }
    let now_ns = crate::util::time::TimestampNanos::now().as_u64();
    if !stored_message_menu(message, true, now_ns).actions.edit {
        return Err(ConversationError::Other(
            "This Saved message cannot be edited".into(),
        ));
    }
    let mut edited = message.clone();
    edited.content = match &message.content {
        MessageContent::Mixed(mixed) => MessageContent::Mixed(MixedContent {
            text: Some(trimmed.to_string()),
            attachment_ids: mixed.attachment_ids.clone(),
        }),
        _ => MessageContent::Text(trimmed.to_string()),
    };
    edited.mark_edited();
    Ok(edited)
}

/// The Saved row as it reads after our pin or unpin: only `is_pinned`
/// flipped and `updated_at` moved. The write itself is the repository's
/// column-scoped `set_pinned_scoped` (`pinned_at`, the pinned list's order,
/// set on the transition to pinned and cleared on unpin); this is the gate
/// and the shape, exactly the menu's `pin` flag for Saved
/// (`stored_message_menu`), so the menu and the write cannot drift.
fn pinned_saved_record(
    message: &crate::domain::Message,
    own: UserId,
    pinned: bool,
) -> ConversationResult<crate::domain::Message> {
    if message.conversation_type != ConversationType::SavedMessages
        || message.sender_id != Some(own)
    {
        return Err(ConversationError::Other(
            "Only your own Saved message can be pinned".into(),
        ));
    }
    let now_ns = crate::util::time::TimestampNanos::now().as_u64();
    if !stored_message_menu(message, true, now_ns).actions.pin {
        return Err(ConversationError::Other(
            "This Saved message cannot be pinned".into(),
        ));
    }
    let mut row = message.clone();
    row.is_pinned = pinned;
    row.updated_at = chrono::Utc::now();
    Ok(row)
}

fn require_remote_message_kind(kind: &ConversationType) -> ConversationResult {
    if *kind == ConversationType::SavedMessages {
        return Err(ConversationError::Other(
            "Saved message action is unsupported".into(),
        ));
    }
    Ok(())
}

/// Persisted reaction presentation for one scoped message. Never a UI delta.
#[derive(Clone, Debug)]
pub struct ReactionSnapshot {
    pub scope: crate::domain::reaction::ReactionScope,
    pub conversation: ActiveConversation,
    pub author: UserId,
    pub sent_at: u64,
    pub counts: Vec<crate::domain::message::ReactionCount>,
    pub own_emojis: Vec<String>,
    pub summary: String,
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
        Self {
            conversation,
            client,
        }
    }

    /// Create for the current active conversation.
    pub fn current(client: &Arc<ChatterClient>) -> Self {
        Self {
            conversation: client.chat_state().active_conversation(),
            client: client.clone(),
        }
    }

    /// Where this conversation's rows are stored (`SavedMessages` only with
    /// authenticated own identity and matching channel metadata). A Channel
    /// handle on a Saved channel that is not the signed-in account's — built
    /// before a sign-out or account switch — is refused outright rather than
    /// resolved to a plain channel, which would let a remote control be sent
    /// to a user id. A DM or anonymous handle sharing that id is unaffected.
    fn storage_scope(&self) -> ConversationResult<crate::domain::reaction::ReactionScope> {
        self.storage_scope_as(self.client.auth_state().get().current_user_id)
    }

    /// [`Self::storage_scope`] under one account snapshot, so a caller that
    /// also writes under that account resolves and writes as the same user.
    fn storage_scope_as(
        &self,
        own_user: Option<UserId>,
    ) -> ConversationResult<crate::domain::reaction::ReactionScope> {
        let channel = self
            .conversation
            .channel_id()
            .and_then(|id| self.client.get_channel_local(&id));
        let scope = message_storage_scope(&self.conversation, own_user, channel.as_ref())?;
        if matches!(self.conversation, ActiveConversation::Channel(_))
            && scope.kind != ConversationType::SavedMessages
            && channel.is_some_and(|c| {
                c.channel_type == crate::domain::channel::ChannelType::SavedMessages
            })
        {
            return Err(ConversationError::Other(
                "Saved Messages belongs to another account".into(),
            ));
        }
        Ok(scope)
    }

    /// The storage kind this handle resolves to (see [`Self::storage_scope`]).
    pub fn storage_kind(&self) -> ConversationResult<ConversationType> {
        Ok(self.storage_scope()?.kind)
    }

    fn require_remote_message_actions(&self) -> ConversationResult {
        require_remote_message_kind(&self.storage_scope()?.kind)
    }

    /// A handle for another conversation on the same client.
    pub fn for_conversation(&self, conversation: ActiveConversation) -> Self {
        Self {
            conversation,
            client: self.client.clone(),
        }
    }

    /// Get the underlying conversation variant.
    pub fn conversation(&self) -> &ActiveConversation {
        &self.conversation
    }
}

mod message_actions;
mod messages;
mod reactions;
mod receipts;
pub(crate) mod recovery;
mod settings;
mod transport;

#[cfg(test)]
pub(crate) use reactions::complete_reaction_dispatch;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "../reaction_dispatch_tests.rs"]
mod reaction_dispatch_tests;

#[cfg(test)]
#[path = "../saved_conversation_tests.rs"]
mod saved_conversation_tests;

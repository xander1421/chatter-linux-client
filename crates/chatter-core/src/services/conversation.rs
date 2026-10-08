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
    if !matches!(
        message.conversation_type,
        ConversationType::ServerDm | ConversationType::Channel
    ) {
        actions.resend = false;
    }
    // Saved records have only a local/own-device deletion path. Their UI
    // Channel variant must never grant a public-channel remote delete. Edit
    // is a local rewrite of our own note (`send_edit`), so the remote 24h
    // window does not apply: own, delivered, undeleted, with a body.
    if message.conversation_type == ConversationType::SavedMessages {
        actions.delete_for_everyone = false;
        actions.pin = false;
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
    /// authenticated own identity and matching channel metadata). A Saved
    /// channel that is not the signed-in account's — a handle built before a
    /// sign-out or account switch — is refused outright rather than resolved
    /// to a plain channel, which would let a remote control be sent to a
    /// user id.
    fn storage_scope(&self) -> ConversationResult<crate::domain::reaction::ReactionScope> {
        let channel = self
            .conversation
            .channel_id()
            .and_then(|id| self.client.get_channel_local(&id));
        let scope = message_storage_scope(
            &self.conversation,
            self.client.auth_state().get().current_user_id,
            channel.as_ref(),
        )?;
        if scope.kind != ConversationType::SavedMessages
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

    // ========================================================================
    // Ephemeral operations (reaction, typing, pin, edit, delete)
    // ========================================================================

    /// Read the persisted message at this exact conversation/author/timestamp.
    /// Encrypted server-channel messages use the AnonGroup storage kind while
    /// retaining the real server ChannelId; unrelated kinds never match.
    pub fn reaction_snapshot(
        &self,
        author: &UserId,
        sent_at: u64,
    ) -> ConversationResult<Option<ReactionSnapshot>> {
        let id = self
            .conversation
            .channel_id()
            .ok_or(ConversationError::NoConversation)?;
        let kinds = match self.conversation {
            ActiveConversation::Channel(_) => {
                vec![ConversationType::Channel, ConversationType::AnonGroup]
            }
            ActiveConversation::ServerDm { .. } => vec![ConversationType::ServerDm],
            ActiveConversation::AnonDm(_) => vec![ConversationType::AnonDm],
            ActiveConversation::AnonGroup(_) => vec![ConversationType::AnonGroup],
            ActiveConversation::None => return Err(ConversationError::NoConversation),
        };
        let mut found = None;
        for kind in kinds {
            let scope = crate::domain::reaction::ReactionScope::new(id, kind);
            if let Some(message) = self
                .client
                .messages()
                .message_repo
                .get_by_reaction_scope(&scope, author, sent_at)?
            {
                if found.is_some() {
                    return Err(ConversationError::Other(
                        "Ambiguous reaction storage kind for channel".into(),
                    ));
                }
                found = Some(message);
            }
        }
        let Some(message) = found else {
            return Ok(None);
        };
        let scope = crate::domain::reaction::ReactionScope::of(&message);
        self.client
            .messages()
            .message_repo
            .ensure_reaction_scope_resolved(&scope, author, sent_at)?;
        Ok(Some(ReactionSnapshot {
            scope,
            conversation: self.conversation,
            author: *author,
            sent_at: message.sent_at,
            summary: crate::domain::message::ReactionCount::summary_string(
                &message.reaction_counts,
            ),
            counts: message.reaction_counts,
            own_emojis: message.current_user_reactions,
        }))
    }

    /// Bind a domain event to this exact persisted scope before exposing a UI snapshot.
    pub fn reaction_snapshot_for_scope(
        &self,
        expected: &crate::domain::reaction::ReactionScope,
        author: &UserId,
        sent_at: u64,
    ) -> ConversationResult<Option<ReactionSnapshot>> {
        Ok(self
            .reaction_snapshot(author, sent_at)?
            .filter(|snapshot| snapshot.scope == *expected))
    }

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
        self.require_remote_message_actions()?;
        // Per-user check — the summary aggregate would flip other users'
        // reactions off.
        let snapshot = self
            .reaction_snapshot(target_author, target_sent_timestamp)?
            .ok_or_else(|| {
                ConversationError::Other(
                    "Reaction target is not stored in this conversation".into(),
                )
            })?;
        self.toggle_reaction_for_scope(&snapshot.scope, emoji, target_author, target_sent_timestamp)
            .await
    }

    /// Toggle only the message scope captured by the caller. A switched or
    /// replaced conversation cannot redirect an already queued UI intent.
    pub async fn toggle_reaction_for_scope(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        emoji: &str,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> Result<bool, ConversationError> {
        let snapshot = self
            .reaction_snapshot_for_scope(scope, target_author, target_sent_timestamp)?
            .ok_or_else(|| {
                ConversationError::Other(
                    "Reaction target no longer stored in captured scope".into(),
                )
            })?;
        let remove = snapshot.own_emojis.iter().any(|own| own == emoji);
        self.send_reaction_for_scope(scope, emoji, target_author, target_sent_timestamp, remove)
            .await?;
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
        self.require_remote_message_actions()?;
        let scope = self
            .reaction_snapshot(target_author, target_sent_timestamp)?
            .ok_or_else(|| {
                ConversationError::Other("Reaction target not stored in this conversation".into())
            })?
            .scope;
        self.send_reaction_for_scope(&scope, emoji, target_author, target_sent_timestamp, remove)
            .await
    }

    async fn send_reaction_for_scope(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        emoji: &str,
        target_author: &UserId,
        target_sent_timestamp: u64,
        remove: bool,
    ) -> ConversationResult {
        self.require_remote_message_actions()?;
        require_remote_message_kind(&scope.kind)?;
        self.reaction_snapshot_for_scope(scope, target_author, target_sent_timestamp)?
            .ok_or_else(|| {
                ConversationError::Other(
                    "Reaction target no longer stored in captured scope".into(),
                )
            })?;
        let target_author_str = target_author.to_string();
        if self.conversation == ActiveConversation::None {
            return Err(ConversationError::NoConversation);
        }
        let auth = self.client.auth_state().get();
        let session_revision = auth.session_revision;
        let user_id = auth
            .current_user_id
            .ok_or_else(|| ConversationError::Other("Reaction requires a signed-in user".into()))?;
        let action_ts = self
            .client
            .messages()
            .message_repo
            .allocate_reaction_clock(&user_id)?;
        let dispatch = async {
            match &self.conversation {
                ActiveConversation::AnonDm(id) => {
                    self.client
                        .anon_dms()
                        .send_reaction_at(
                            id,
                            emoji,
                            &target_author_str,
                            target_sent_timestamp,
                            remove,
                            action_ts,
                        )
                        .await?;
                }
                ActiveConversation::AnonGroup(id) => {
                    self.client
                        .anon_groups()
                        .send_reaction_at(
                            id,
                            emoji,
                            &target_author_str,
                            target_sent_timestamp,
                            remove,
                            action_ts,
                        )
                        .await?;
                }
                ActiveConversation::None => return Err(ConversationError::NoConversation),
                // DM (mailbox/server) and channel share build + persist; only the
                // channel embeds a channel_id and the transport differs.
                _ => {
                    // Scope and local storage were validated before any remote effect.
                    let wire_author = match self.conversation {
                        ActiveConversation::ServerDm { peer_id } => {
                            crate::domain::reaction::encode_dm_reaction_target(
                                &peer_id,
                                target_author,
                            )
                        }
                        _ => target_author_str.clone(),
                    };
                    let wire_clock = if matches!(self.conversation, ActiveConversation::Channel(_))
                    {
                        let target = self
                            .client
                            .messages()
                            .message_repo
                            .get_by_reaction_scope(&scope, target_author, target_sent_timestamp)?
                            .ok_or_else(|| {
                                ConversationError::Other("Reaction target no longer stored".into())
                            })?;
                        self.client
                            .messages()
                            .message_repo
                            .authenticated_channel_reaction_clock(&target)?
                    } else {
                        target_sent_timestamp
                    };
                    let content =
                        crate::services::gateway::builders::build_reaction_content_with_channel_at(
                            emoji,
                            remove,
                            &wire_author,
                            wire_clock,
                            self.active_channel_id().as_deref(),
                            action_ts,
                        );
                    self.dispatch_content(content).await?;
                }
            }
            Ok(())
        };
        complete_reaction_dispatch(action_ts, dispatch, |clock| {
            let auth = self.client.auth_state().get();
            if auth.session_revision != session_revision || auth.current_user_id != Some(user_id) {
                return Err(ConversationError::Other(
                    "Reaction auth session changed while sending".into(),
                ));
            }

            self.client
                .messages()
                .record_reaction_completion_at(
                    &scope,
                    target_author,
                    target_sent_timestamp,
                    &user_id,
                    emoji,
                    remove,
                    clock,
                )
                .map_err(ConversationError::Message)
        })
        .await
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
                self.client
                    .anon_dms()
                    .send_typing_indicator(id, started)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.client
                    .anon_groups()
                    .send_typing_indicator(id, started)
                    .await?;
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = crate::services::build_typing_content_with_channel(
                    started,
                    self.active_channel_id().as_deref(),
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
        self.require_remote_message_actions()?;
        let new_pinned = !unpin;
        // Sub-services on the proto side still take &str.
        let target_author_str = target_author.to_string();
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                match self.client.anon_dms().set_message_pinned(
                    id,
                    target_sent_timestamp,
                    new_pinned,
                ) {
                    Ok(false) => {
                        tracing::warn!(conversation_id = %id, target_sent_timestamp, "AnonDm pin: 0 rows affected")
                    }
                    Err(e) => {
                        tracing::warn!(conversation_id = %id, error = %e, "AnonDm pin: local persist failed")
                    }
                    _ => {}
                }
                self.client
                    .anon_dms()
                    .send_pin(id, &target_author_str, target_sent_timestamp, unpin)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                match self.client.anon_groups().set_message_pinned(
                    id,
                    target_sent_timestamp,
                    new_pinned,
                ) {
                    Ok(false) => {
                        tracing::warn!(group_id = %id, target_sent_timestamp, "AnonGroup pin: 0 rows affected")
                    }
                    Err(e) => {
                        tracing::warn!(group_id = %id, error = %e, "AnonGroup pin: local persist failed")
                    }
                    _ => {}
                }
                self.client
                    .anon_groups()
                    .send_pin(id, &target_author_str, target_sent_timestamp, unpin)
                    .await?;
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_pin_content_with_channel(
                    &author_aci,
                    target_sent_timestamp,
                    unpin,
                    self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await?;
                let _ = self
                    .client
                    .messages()
                    .set_message_pinned(&msg.id, new_pinned);
                Ok(())
            }
        }
    }

    /// Send an edit and persist locally.
    ///
    /// Emits `DomainEvent::MessageEdited` on success so UI subscribers
    /// refresh uniformly across conversation kinds — no UI-side branching
    /// on anon vs DM/channel needed.
    ///
    /// `target_author` is the key the menu was opened with. A Saved edit
    /// refuses any author but the signed-in user before looking anything up,
    /// so a stale or malformed menu key cannot pick a Saved row by clock
    /// alone. The other kinds keep their own-message lookup (`persist_edit`)
    /// and the peer's authorship check; an anonymous group's menu key is not
    /// byte-identical to its stored sender id (see `stored_message`).
    pub async fn send_edit(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
        new_text: &str,
    ) -> ConversationResult {
        let scope = self.storage_scope()?;
        if scope.kind == ConversationType::SavedMessages {
            if self.client.auth_state().get().current_user_id != Some(*target_author) {
                return Err(ConversationError::Other(
                    "Only your own Saved message can be edited".into(),
                ));
            }
            return self.edit_saved_message(&scope, target_sent_timestamp, new_text);
        }
        let result = self.send_edit_inner(target_sent_timestamp, new_text).await;
        if result.is_ok() {
            self.emit_local_edit_event(target_sent_timestamp, new_text);
        }
        result
    }

    /// Edit one of our Saved records in place. Local only: the stored row is
    /// read in the Saved scope, rewritten by [`edited_saved_record`] and
    /// written back whole (`MessageRepository::update`), so a storage
    /// failure is this call's error, never a silent success. Nothing goes to
    /// the server — the Saved id is our own user id, not a channel a
    /// sender-key control may be sent to — and nothing reaches linked
    /// devices: `SavedMessageSync` has no edit form and a re-sent one is
    /// dropped as a duplicate on arrival (`store_synced_saved_message`), so a
    /// sibling device keeps the old text until that proto grows an edit. The
    /// emitted `MessageEdited` carries the real row id and the
    /// `SavedMessages` kind, so a same-id channel or DM row is never patched.
    fn edit_saved_message(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        target_sent_timestamp: u64,
        new_text: &str,
    ) -> ConversationResult {
        let auth = self.client.auth_state().get();
        let own = auth
            .current_user_id
            .ok_or_else(|| ConversationError::Other("Editing requires a signed-in user".into()))?;
        let repo = &self.client.messages().message_repo;
        let stored = scoped_stored_message(repo, scope, &own, target_sent_timestamp)?;
        let edited = edited_saved_record(&stored, own, new_text)?;
        // The scope was resolved under this account and session; the write
        // happens under the same ones or not at all.
        let now = self.client.auth_state().get();
        if now.current_user_id != Some(own) || now.session_revision != auth.session_revision {
            return Err(ConversationError::Other(
                "Account changed while editing".into(),
            ));
        }
        repo.update(&edited)?;
        self.client
            .event_bus()
            .emit_domain(crate::events::DomainEvent::MessageEdited {
                message_id: edited.id,
                new_content: edited.content,
                channel_id: scope.id,
                conversation_type: ConversationType::SavedMessages,
                author_id: own,
                target_sent_timestamp,
            });
        Ok(())
    }

    fn emit_local_edit_event(&self, target_sent_timestamp: u64, new_text: &str) {
        use crate::domain::ids::{ChannelId, MessageId};
        use crate::domain::message::{ConversationType as CT, MessageContent};
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
        let Some(author_id) = self.client.auth_state().get().current_user_id else {
            return;
        };

        self.client
            .event_bus()
            .emit_domain(DomainEvent::MessageEdited {
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
        self.require_remote_message_actions()?;
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                // anon_dms().send_edit() persists internally
                self.client
                    .anon_dms()
                    .send_edit(id, target_sent_timestamp, new_text)
                    .await?;
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                // anon_groups().send_edit() persists internally
                self.client
                    .anon_groups()
                    .send_edit(id, target_sent_timestamp, new_text)
                    .await?;
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = crate::services::gateway::builders::build_edit_content_with_channel(
                    target_sent_timestamp,
                    new_text,
                    self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await?;
                self.persist_edit(target_sent_timestamp, new_text);
                Ok(())
            }
        }
    }

    /// Prepare the same scoped control the send path dispatches, from a locally
    /// selected stored record. The peer never resolves our local timestamp.
    pub(crate) fn prepare_delete_for_everyone(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<(crate::domain::Message, crate::proto::content_v1::Content)> {
        let target = self.stored_message(target_author, target_sent_timestamp)?;
        if !self.can_delete_for_everyone(target_author, target_sent_timestamp)? {
            return Err(ConversationError::Other(
                "cannot delete this message for everyone".into(),
            ));
        }
        let content = if matches!(
            target.conversation_type,
            ConversationType::ServerDm | ConversationType::AnonDm
        ) {
            crate::services::gateway::builders::build_stored_dm_delete_content(
                &target,
                &self.client.messages().current_user_id()?,
            )?
        } else {
            crate::services::gateway::builders::build_targeted_delete_content(
                target_sent_timestamp,
                Some(&target_author.to_string()),
                self.active_channel_id().as_deref(),
            )
        };
        Ok((target, content))
    }

    /// Send a delete-for-everyone and persist locally.
    ///
    /// `target_author` is the message's sender. In a 1:1 DM this may be the
    /// peer; channels still only succeed for our own messages.
    pub async fn send_delete(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult {
        let (target, prepared_content) =
            self.prepare_delete_for_everyone(target_author, target_sent_timestamp)?;
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                self.client
                    .anon_dms()
                    .send_targeted_delete_for_everyone(id, target_author, target_sent_timestamp)
                    .await?;
                let _ = self.client.anon_dms().delete_message_by_timestamp(
                    id,
                    target_author,
                    target_sent_timestamp,
                );
                Ok(())
            }
            ActiveConversation::AnonGroup(id) => {
                self.client
                    .anon_groups()
                    .send_targeted_delete_for_everyone(id, target_author, target_sent_timestamp)
                    .await?;
                let _ = self.client.anon_groups().delete_message_by_timestamp(
                    id,
                    target_author,
                    target_sent_timestamp,
                );
                Ok(())
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = prepared_content;
                self.dispatch_content(content).await?;
                // The server keeps an authorless copy of a sealed-sender
                // message; take it down too, or a device fetching history
                // gets it back. Best-effort: the E2E delete already went out.
                if let Err(e) = self
                    .client
                    .messages()
                    .delete_sealed_server_copy(&target)
                    .await
                {
                    tracing::warn!(message_id = %target.id, error = %e,
                        "delete-for-everyone: server copy of sealed message not deleted");
                }
                if target.conversation_type == ConversationType::ServerDm
                    && crate::storage::repositories::message_modifier_repo::canonical_call_record_id(&target).is_none()
                {
                    if let Some(author) = target.sender_id {
                        crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(self.client.messages().database.clone())
                            .queue_dm_delete(&author, target.sent_at, Some(target.conversation_id), crate::util::time::TimestampNanos::now().as_u64())?;
                    }
                }
                if let Some(call_id) =
                    crate::storage::repositories::message_modifier_repo::canonical_call_record_id(
                        &target,
                    )
                {
                    crate::storage::repositories::message_modifier_repo::apply_call_delete(
                        self.client.messages().database.clone(),
                        &self.client.messages().message_repo,
                        &crate::domain::reaction::ReactionScope::of(&target),
                        &call_id.to_string(),
                    )?;
                } else {
                    self.client.messages().delete_message_local(&target.id)?;
                }
                Ok(())
            }
        }
    }

    fn deletion_retry_scope(&self) -> ConversationResult<String> {
        Ok(match self.conversation {
            ActiveConversation::Channel(id) => format!("channel:{id}"),
            ActiveConversation::ServerDm { peer_id } => format!("server_dm:{peer_id}"),
            ActiveConversation::AnonDm(id) => format!("anon_dm:{id}"),
            ActiveConversation::AnonGroup(id) => format!("anon_group:{id}"),
            ActiveConversation::None => return Err(ConversationError::NoConversation),
        })
    }

    /// Persist the immutable original batch before any remote action. Recovery
    /// uses exact saved controls and minimal cleanup identities, never live rows.
    pub async fn send_delete_batch(&self, targets: &[(UserId, u64)]) -> ConversationResult {
        let auth = self.client.auth_state().get();
        let account = auth
            .current_user_id
            .ok_or(crate::services::MessageError::NotLoggedIn)?;
        self.send_delete_batch_in_session(targets, Some((account, auth.session_revision)))
            .await
    }

    /// UI callers pass the account/revision captured at the original click,
    /// before spawning a task, so a delayed first poll cannot adopt a new login.
    pub async fn send_delete_batch_fenced(
        &self,
        targets: &[(UserId, u64)],
        account: UserId,
        revision: u64,
    ) -> ConversationResult {
        self.send_delete_batch_in_session(targets, Some((account, revision)))
            .await
    }

    async fn send_delete_batch_in_session(
        &self,
        targets: &[(UserId, u64)],
        expected: Option<(UserId, u64)>,
    ) -> ConversationResult {
        use crate::storage::repositories::delete_batch_retry_repo::{
            DELETE_BATCH_LOCK, DeleteBatchRetryRepository, PreparedBatch, PreparedDelete,
        };
        use prost::Message as _;
        if targets.is_empty() {
            return Ok(());
        }
        if targets.len() > 500 {
            return Err(ConversationError::Other(
                "at most 500 messages may be deleted at once".into(),
            ));
        }
        let _execution = DELETE_BATCH_LOCK.lock().await;
        let auth = self.client.auth_state().get();
        let account = auth
            .current_user_id
            .ok_or(crate::services::MessageError::NotLoggedIn)?;
        let revision = auth.session_revision;
        if self.client.messages().current_user_id()? != account
            || expected.is_some_and(|e| e != (account, revision))
        {
            return Err(crate::services::MessageError::NotLoggedIn.into());
        }
        let scope = self.deletion_retry_scope()?;
        let journal = DeleteBatchRetryRepository::new(self.client.messages().database.clone());
        let selection = DeleteBatchRetryRepository::selection_key(targets)?;
        let mut batch = if let Some(saved) = journal.get(&account, &scope, &selection)? {
            saved
        } else {
            let mut seen = std::collections::HashSet::new();
            let mut prepared = Vec::with_capacity(targets.len());
            for &(author, timestamp) in targets {
                if !seen.insert((author, timestamp)) {
                    continue;
                }
                let (message, content) = self.prepare_delete_for_everyone(&author, timestamp)?;
                let deletion_token = self
                    .client
                    .messages()
                    .message_repo
                    .deletion_token(&message.id)?
                    .map(|t| t.to_vec())
                    .unwrap_or_default();
                prepared.push(PreparedDelete {
                    author,timestamp,message_id:message.id,conversation_id:message.conversation_id,
                    conversation_type:message.conversation_type.clone(),
                    call_id:crate::storage::repositories::message_modifier_repo::canonical_call_record_id(&message).map(|id|id.to_string()),
                    deletion_token,control:content.encode_to_vec(),control_sent:false,local_completed:false,
                });
            }
            let batch = PreparedBatch {
                id: uuid::Uuid::new_v4().to_string(),
                targets: prepared,
                server_committed: false,
            };
            journal.insert(&account, &scope, &selection, &batch)?;
            batch
        };
        // Cancel all remaining network/local work if logout or a new login
        // changes the account/session; recovery never adopts another account.
        let result = tokio::select! {
            biased;
            _ = self.deletion_session_changed(account,revision) => Err(crate::services::MessageError::NotLoggedIn.into()),
            result = tokio::time::timeout(std::time::Duration::from_secs(120), self.execute_prepared_delete_batch(&account,revision,&scope,&journal,&mut batch)) => {
                match result { Ok(result) => result, Err(_) => Err(crate::services::MessageError::Network(crate::error::NetworkError::Timeout).into()) }
            },
        };
        if let Err(error) = &result {
            let permanent = matches!(
                error,
                ConversationError::Message(crate::services::MessageError::Network(
                    crate::error::NetworkError::PermissionDenied
                        | crate::error::NetworkError::InvalidRequest(_)
                        | crate::error::NetworkError::DataLoss(_)
                )) | ConversationError::AnonDm(
                    crate::services::AnonDmError::ConversationBlocked(_)
                        | crate::services::AnonDmError::UserBlocked(_)
                        | crate::services::AnonDmError::ConversationNotFound(_)
                        | crate::services::AnonDmError::DeliveryUnconfirmed
                ) | ConversationError::AnonGroup(
                    crate::services::AnonGroupError::InsufficientPermissions(_)
                        | crate::services::AnonGroupError::GroupBlocked(_)
                        | crate::services::AnonGroupError::UserBlocked(_)
                        | crate::services::AnonGroupError::GroupNotFound(_)
                        | crate::services::AnonGroupError::DeliveryUnconfirmed
                )
            );
            journal.defer(&account, &scope, &batch.id, permanent)?;
        }
        result
    }

    fn require_deletion_session(&self, account: &UserId, revision: u64) -> ConversationResult {
        let state = self.client.auth_state().get();
        if state.current_user_id != Some(*account) || state.session_revision != revision {
            return Err(crate::services::MessageError::NotLoggedIn.into());
        }
        Ok(())
    }

    async fn deletion_session_changed(&self, account: UserId, revision: u64) {
        let mut changes = self.client.auth_state().subscribe();
        loop {
            let state = self.client.auth_state().get();
            if state.current_user_id != Some(account) || state.session_revision != revision {
                return;
            }
            if changes.recv().await.is_err() {
                return;
            }
        }
    }

    async fn execute_prepared_delete_batch(
        &self,
        account: &UserId,
        revision: u64,
        scope: &str,
        journal: &crate::storage::repositories::delete_batch_retry_repo::DeleteBatchRetryRepository,
        batch: &mut crate::storage::repositories::delete_batch_retry_repo::PreparedBatch,
    ) -> ConversationResult {
        use prost::Message as _;
        self.require_deletion_session(account, revision)?;
        if !batch.server_committed {
            // Preserve the entire original selected set in this one RPC. Never
            // combine journal rows from different submissions or split a batch.
            let server_targets = batch
                .targets
                .iter()
                .filter(|t| {
                    matches!(self.conversation, ActiveConversation::Channel(_))
                        || matches!(self.conversation, ActiveConversation::ServerDm { .. })
                            && !t.deletion_token.is_empty()
                })
                .map(|t| {
                    (
                        t.message_id.to_string(),
                        t.conversation_id.to_string(),
                        t.deletion_token.clone(),
                    )
                })
                .collect::<Vec<_>>();
            if !server_targets.is_empty() {
                self.client
                    .messages()
                    .message_client()
                    .delete_messages(server_targets)
                    .await
                    .map_err(crate::services::MessageError::from)?;
            }
            self.require_deletion_session(account, revision)?;
            batch.server_committed = true;
            for target in &mut batch.targets {
                target.deletion_token.clear();
            }
            journal.progress(account, scope, batch)?;
        }
        for i in 0..batch.targets.len() {
            self.require_deletion_session(account, revision)?;
            let target = &batch.targets[i];
            if target.control_sent || target.local_completed {
                continue;
            }
            let content = crate::proto::content_v1::Content::decode(target.control.as_slice())
                .map_err(|e| {
                    ConversationError::Other(format!("invalid prepared deletion control: {e}"))
                })?;
            match &self.conversation {
                ActiveConversation::AnonDm(id) => {
                    self.client.anon_dms().send_message(id, content).await?
                }
                ActiveConversation::AnonGroup(id) => {
                    self.client
                        .anon_groups()
                        .send_ephemeral_message(id, content)
                        .await?
                }
                _ => self.dispatch_content(content).await?,
            }
            self.require_deletion_session(account, revision)?;
            batch.targets[i].control_sent = true;
            batch.targets[i].control.clear();
            journal.progress(account, scope, batch)?;
        }
        for i in 0..batch.targets.len() {
            self.require_deletion_session(account, revision)?;
            if batch.targets[i].local_completed {
                continue;
            }
            self.complete_prepared_delete(&batch.targets[i])?;
            batch.targets[i].local_completed = true;
            journal.progress(account, scope, batch)?;
        }
        self.require_deletion_session(account, revision)?;
        journal.finish(account, scope, batch)?;
        for target in &batch.targets {
            self.client
                .event_bus()
                .emit_domain(crate::events::DomainEvent::MessageDeleted {
                    message_id: target.message_id,
                    author_id: target.author,
                    channel_id: Some(target.conversation_id),
                    target_sent_timestamp: target.timestamp,
                });
        }
        Ok(())
    }

    fn complete_prepared_delete(
        &self,
        target: &crate::storage::repositories::delete_batch_retry_repo::PreparedDelete,
    ) -> ConversationResult {
        use crate::storage::repositories::message_modifier_repo::{
            MessageModifierRepository, apply_call_delete,
        };
        if let Some(call_id) = &target.call_id {
            apply_call_delete(
                self.client.messages().database.clone(),
                &self.client.messages().message_repo,
                &crate::domain::reaction::ReactionScope::new(
                    target.conversation_id,
                    target.conversation_type.clone(),
                ),
                call_id,
            )?;
            return Ok(());
        }
        if matches!(
            target.conversation_type,
            ConversationType::ServerDm | ConversationType::AnonDm
        ) {
            MessageModifierRepository::new(self.client.messages().database.clone())
                .queue_typed_dm_delete(
                    &target.author,
                    target.timestamp,
                    Some(target.conversation_id),
                    target.conversation_type.clone(),
                    crate::util::time::TimestampNanos::now().as_u64(),
                )?;
        }
        if let ActiveConversation::AnonGroup(id) = self.conversation {
            self.client.anon_groups().delete_message_by_timestamp(
                &id,
                &target.author,
                target.timestamp,
            )?;
            return Ok(());
        }
        match self
            .client
            .messages()
            .delete_message_local(&target.message_id)
        {
            Ok(())
            | Err(crate::services::MessageError::Storage(crate::error::StorageError::NotFound(
                _,
            ))) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Delete-for-me: delete locally + sync to own devices.
    pub async fn delete_for_me(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => self
                .client
                .anon_dms()
                .delete_message_by_timestamp(id, target_author, target_sent_timestamp)
                .map(|_| ())
                .map_err(|e| ConversationError::Other(e.to_string())),
            ActiveConversation::AnonGroup(id) => self
                .client
                .anon_groups()
                .delete_message_by_timestamp(id, target_author, target_sent_timestamp)
                .map(|_| ())
                .map_err(|e| ConversationError::Other(e.to_string())),
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                let author_aci = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                let content = crate::services::build_delete_for_me_content(
                    &self.dm_or_channel_id(),
                    &author_aci,
                    target_sent_timestamp,
                );
                // Our own other devices only: a peer's (or channel member's)
                // device would apply it and delete its own copy.
                let messages = self.client.messages();
                messages
                    .sync_to_own_devices(&mut messages.auth_client(), content)
                    .await;
                messages
                    .delete_message_local(&msg.id)
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
                match self.client.anon_dms().get_message_by_sender_timestamp(
                    id,
                    target_author,
                    target_sent_timestamp,
                ) {
                    Ok(Some(msg)) => Ok(msg.as_text().map(|s| s.to_string())),
                    Ok(None) => Ok(None),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::AnonGroup(id) => {
                // Use timestamp-only lookup: the 3-field lookup (group_id + sender_id + sent_at)
                // fails due to sender_id byte encoding mismatch between storage and lookup paths.
                // Nanosecond timestamps are unique per group so timestamp-only is safe.
                match self
                    .client
                    .anon_groups()
                    .get_message_by_timestamp(id, target_sent_timestamp)
                {
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
                match self.client.anon_dms().get_message_by_sender_timestamp(
                    id,
                    target_author,
                    target_sent_timestamp,
                ) {
                    Ok(Some(msg)) => Ok(msg.attachments),
                    Ok(None) => Ok(vec![]),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::AnonGroup(id) => {
                match self
                    .client
                    .anon_groups()
                    .get_message_by_timestamp(id, target_sent_timestamp)
                {
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
                match self.client.anon_dms().get_message_by_sender_timestamp(
                    id,
                    target_author,
                    target_sent_timestamp,
                ) {
                    Ok(Some(msg)) => Ok(msg.sender_id == current_user_id),
                    Ok(None) => Err(ConversationError::Other("Message not found".into())),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::AnonGroup(id) => {
                // Use timestamp-only lookup to avoid sender_id byte mismatch.
                // Ownership is determined from the stored message's sender_id field.
                match self
                    .client
                    .anon_groups()
                    .get_message_by_timestamp(id, target_sent_timestamp)
                {
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
    /// Own message, already on the server, inside the remote-action window.
    /// Delegates to [`crate::domain::message::MessageActions`] so the menu
    /// and this helper cannot drift.
    pub fn can_delete_for_everyone(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<bool> {
        Ok(self
            .message_menu(target_author, target_sent_timestamp)?
            .actions
            .delete_for_everyone)
    }

    /// Action flags for the message menu. One lookup for every conversation
    /// type; the UI renders `actions` and does not re-decide them.
    pub fn message_menu(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<MessageMenu> {
        let message = self.stored_message(target_author, target_sent_timestamp)?;
        let is_own = message.sender_id == self.client.auth_state().get().current_user_id;
        let now_ns = crate::util::time::TimestampNanos::now().as_u64();
        Ok(stored_message_menu(&message, is_own, now_ns))
    }

    fn stored_message(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<crate::domain::Message> {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => self
                .client
                .anon_dms()
                .get_message_by_sender_timestamp(id, target_author, target_sent_timestamp)?
                .ok_or_else(|| ConversationError::Other("Message not found".into())),
            // Timestamp-only: anon-group sender ids are not byte-identical to
            // the id the menu was opened with (see `is_own_message`).
            ActiveConversation::AnonGroup(id) => self
                .client
                .anon_groups()
                .get_message_by_timestamp(id, target_sent_timestamp)?
                .ok_or_else(|| ConversationError::Other("Message not found".into())),
            ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_) => {
                self.lookup_message(target_author, target_sent_timestamp)
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    // ========================================================================
    // Receipts
    // ========================================================================

    pub async fn send_read_receipt(&self, timestamps: &[u64]) -> ConversationResult {
        if timestamps.is_empty() {
            return Ok(());
        }
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
            ActiveConversation::AnonDm(dm_id) => self
                .client
                .anon_dms()
                .send_read_receipt(dm_id, timestamps.to_vec())
                .await
                .map_err(|e| ConversationError::Other(e.to_string())),
            // Groups don't track per-user read state to preserve sender anonymity.
            ActiveConversation::AnonGroup(_) => Ok(()),
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = crate::services::gateway::build_read_receipt_content_with_channel(
                    timestamps.to_vec(),
                    self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await
            }
        }
    }

    pub async fn send_delivery_receipt(&self, timestamps: &[u64]) -> ConversationResult {
        if timestamps.is_empty() {
            return Ok(());
        }
        match &self.conversation {
            ActiveConversation::AnonDm(dm_id) => self
                .client
                .anon_dms()
                .send_delivery_receipt(dm_id, timestamps.to_vec())
                .await
                .map_err(|e| ConversationError::Other(e.to_string())),
            // Groups don't track per-user delivery state to preserve sender anonymity.
            ActiveConversation::AnonGroup(_) => Ok(()),
            ActiveConversation::None => Err(ConversationError::NoConversation),
            _ => {
                let content = crate::services::gateway::build_delivery_receipt_content_with_channel(
                    timestamps.to_vec(),
                    self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await
            }
        }
    }

    /// A persisted incoming message is read only while its conversation is on
    /// screen. Reuse the open-chat read path for local state, linked-device
    /// sync, and peer receipts (including the receipt privacy setting).
    pub(crate) async fn mark_received_as_read(
        client: &Arc<ChatterClient>,
        message: &crate::domain::message::Message,
    ) -> Result<(), ConversationError> {
        let active = client.chat_state().active_conversation();
        if client.chat_state().is_app_visible() && active.is_for_channel(message.conversation_id) {
            client.conversation(active).mark_as_read().await?;
        }
        Ok(())
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
        let anchor = self.mark_read_local()?;
        if let Some((conversation_id, conversation_type)) = self.receipt_target() {
            if let Ok(page) = self.client.messages().get_messages_local(
                &conversation_id,
                &conversation_type,
                None,
                crate::config::MESSAGE_PAGE_SIZE as u32,
            ) {
                self.send_read_receipts_for_page(&page);
            }
        }
        Ok(anchor)
    }

    /// The local half of [`Self::mark_as_read`]: clears `is_read` and returns
    /// the anchor, sending nothing. A conversation open calls this, loads its
    /// newest page, and hands that page to
    /// [`Self::send_read_receipts_for_page`] — the same rows `mark_as_read`
    /// would have queried a second time.
    pub fn mark_read_local(&self) -> Result<UnreadAnchor, ConversationError> {
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
        Ok(self
            .client
            .messages()
            .mark_conversation_read(&conversation_id, &conversation_type)?)
    }

    /// Where a read receipt for this conversation goes, if one goes at all:
    /// nothing while off screen, and anon groups have no receipt (per-user read
    /// state would defeat the sender anonymity the group is for).
    fn receipt_target(
        &self,
    ) -> Option<(
        crate::domain::ids::ChannelId,
        crate::domain::message::ConversationType,
    )> {
        if !self.client.chat_state().is_app_visible()
            || matches!(self.conversation, ActiveConversation::AnonGroup(_))
        {
            return None;
        }
        Some((
            self.conversation.channel_id()?,
            self.client.conversation_type_of(&self.conversation)?,
        ))
    }

    /// Receipt the peer's messages in `page` — the conversation's newest page,
    /// as the open just loaded it. Receipting our own back to ourselves is
    /// noise. `send_read_receipt` holds the privacy gate and picks the
    /// transport.
    pub fn send_read_receipts_for_page(&self, page: &[crate::domain::message::Message]) {
        if self.receipt_target().is_none() {
            return;
        }
        let my_user_id = self.client.auth_state().get().current_user_id;
        let timestamps: Vec<u64> = page
            .iter()
            .filter(|m| m.sender_id != my_user_id)
            .map(|m| m.sent_at)
            .collect();
        // Off the caller's path: a receipt is a network send (session, sealed
        // sender, or the Shield mailbox). Awaited, a slow peer held the
        // previous conversation on screen until it finished.
        let handle = self.clone();
        tokio::spawn(async move {
            let _ = handle.send_read_receipt(&timestamps).await;
        });
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
                self.client
                    .channels()
                    .settings_repo
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
                self.client
                    .channels()
                    .settings_repo
                    .set(&key, &muted)
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::Channel(channel_id) => {
                // Not the membership's `is_muted`: that column is the
                // moderator mute (server `ChannelMember.muted`, moderation
                // events), and sharing it let either one overwrite the other.
                let key = format!("conv.muted.channel.{}", channel_id);
                self.client
                    .channels()
                    .settings_repo
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
                Ok(self
                    .client
                    .channels()
                    .settings_repo
                    .get_bool(&key)
                    .map_err(|e| ConversationError::Other(e.to_string()))?
                    .unwrap_or(false))
            }
            ActiveConversation::AnonGroup(id) => Ok(self
                .client
                .anon_groups()
                .get_group(id)
                .map_err(|e| ConversationError::Other(e.to_string()))?
                .map(|g| g.status == crate::domain::anon_group::AnonGroupStatus::Muted)
                .unwrap_or(false)),
            ActiveConversation::ServerDm { peer_id } => {
                let key = format!("conv.muted.server_dm.{}", peer_id);
                Ok(self
                    .client
                    .channels()
                    .settings_repo
                    .get_bool(&key)
                    .map_err(|e| ConversationError::Other(e.to_string()))?
                    .unwrap_or(false))
            }
            ActiveConversation::Channel(channel_id) => {
                let key = format!("conv.muted.channel.{}", channel_id);
                Ok(self
                    .client
                    .channels()
                    .settings_repo
                    .get_bool(&key)
                    .map_err(|e| ConversationError::Other(e.to_string()))?
                    .unwrap_or(false))
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Leave this conversation (Settings › Leave, the sidebar row menu).
    ///
    /// A channel is left on the server and stays listed, read-only. A private
    /// group tells every other member over their anonymous mailboxes and is
    /// then dropped from this device. DMs have nothing to leave.
    pub async fn leave(&self) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::Channel(id) => Ok(self.client.leave_channel(id).await?),
            ActiveConversation::AnonGroup(id) => {
                Ok(self.client.anon_groups().leave_group(id).await?)
            }
            _ => Err(ConversationError::Other(
                "only channels and groups can be left".into(),
            )),
        }
    }

    /// Remove `user` from this conversation (Settings › Members › Remove).
    ///
    /// A channel kick is authorized by the server. A private-group kick is
    /// refused here unless we may kick (admin/owner) and the target is not the
    /// owner; it is signed with our group mailbox key, and every member's
    /// client verifies that signature before applying it (`handle_kick`).
    pub async fn remove_member(&self, user: &UserId) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::Channel(id) => Ok(self.client.kick_user(id, user, None).await?),
            ActiveConversation::AnonGroup(id) => {
                Ok(self.client.anon_groups().kick_member(id, user).await?)
            }
            _ => Err(ConversationError::Other(
                "only channels and groups have members to remove".into(),
            )),
        }
    }

    /// Hand this channel or private group to `new_owner`; we become an admin
    /// and may then leave. The last owner of a group with other members
    /// cannot leave without doing this first (`leave_group`).
    pub async fn transfer_ownership(&self, new_owner: &UserId) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::Channel(id) => Ok(self
                .client
                .transfer_channel_ownership(id, new_owner)
                .await?),
            ActiveConversation::AnonGroup(id) => Ok(self
                .client
                .anon_groups()
                .transfer_ownership(id, new_owner)
                .await?),
            _ => Err(ConversationError::Other(
                "only channels and groups have owners".into(),
            )),
        }
    }

    /// Block or unblock this private group on this device: a blocked group's
    /// content is dropped on receive (`process_incoming_content`).
    pub fn set_blocked(&self, blocked: bool) -> ConversationResult {
        let ActiveConversation::AnonGroup(id) = &self.conversation else {
            return Err(ConversationError::Other(
                "only private groups can be blocked here".into(),
            ));
        };
        let groups = self.client.anon_groups();
        Ok(if blocked {
            groups.block_group(id)?
        } else {
            groups.unblock_group(id)?
        })
    }

    /// Block or unblock `user` within this private group: their posts,
    /// reactions, typing, calls and profile shares are dropped; their leave
    /// still applies.
    pub fn set_member_blocked(&self, user: &UserId, blocked: bool) -> ConversationResult {
        let ActiveConversation::AnonGroup(id) = &self.conversation else {
            return Err(ConversationError::Other(
                "only private-group members can be blocked here".into(),
            ));
        };
        let groups = self.client.anon_groups();
        Ok(if blocked {
            groups.block_user_in_group(id, user, None)?
        } else {
            groups.unblock_user_in_group(id, user)?
        })
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
            ActiveConversation::AnonDm(id) => Ok(self.client.anon_dms().delete_conversation(id)?),
            ActiveConversation::ServerDm { peer_id } => {
                // Channel-free DM: no channel/membership row to cascade —
                // delete the local message history directly, then the
                // attachments only this chat carried (row and file).
                let channel_id: ChannelId = (*peer_id).into();
                let messages = self
                    .client
                    .messages()
                    .get_messages_local(&channel_id, &ConversationType::ServerDm, None, u32::MAX)
                    .map_err(ConversationError::Message)?;
                let attachments: Vec<_> = messages
                    .iter()
                    .flat_map(|m| m.attachments.iter().copied())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                let ids: Vec<_> = messages.into_iter().map(|m| m.id).collect();
                let storage =
                    |e: crate::error::StorageError| ConversationError::Other(e.to_string());
                self.client
                    .messages()
                    .message_repo
                    .delete_batch(&ids)
                    .map_err(storage)?;
                self.client
                    .attachments()
                    .attachment_repo()
                    .delete_unreferenced(&attachments)
                    .map_err(storage)
            }
            // A channel we are no longer in: its history, then the channel.
            ActiveConversation::Channel(channel_id)
                if self.client.is_former_channel(channel_id) =>
            {
                let storage =
                    |e: crate::error::StorageError| ConversationError::Other(e.to_string());
                let messages = self
                    .client
                    .messages()
                    .get_messages_local(channel_id, &ConversationType::Channel, None, u32::MAX)
                    .map_err(ConversationError::Message)?;
                let attachments: Vec<_> = messages
                    .iter()
                    .flat_map(|m| m.attachments.iter().copied())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                let ids: Vec<_> = messages.into_iter().map(|m| m.id).collect();
                self.client
                    .messages()
                    .message_repo
                    .delete_batch(&ids)
                    .map_err(storage)?;
                self.client
                    .attachments()
                    .attachment_repo()
                    .delete_unreferenced(&attachments)
                    .map_err(storage)?;
                self.client
                    .channels()
                    .channel_repo
                    .delete_channel_cascade(channel_id)
                    .map_err(storage)
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
            ActiveConversation::ServerDm { peer_id } => {
                crate::services::disappearing::server_dm_timer(
                    &self.client.channels().settings_repo,
                    peer_id,
                )
            }
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

    /// Persisted activation state; channels have an indefinite server-held TTL.
    pub fn disappearing_state(&self) -> crate::services::disappearing::DmTimer {
        match &self.conversation {
            ActiveConversation::ServerDm { peer_id } => {
                crate::services::disappearing::server_dm_state(
                    &self.client.channels().settings_repo,
                    peer_id,
                )
            }
            _ => crate::services::disappearing::DmTimer {
                seconds: self.disappearing_timer(),
                version: crate::services::disappearing::INITIAL_TIMER_VERSION,
                enabled_until: 0,
                activation_seconds: 0,
            },
        }
    }

    pub async fn set_disappearing_window(
        &self,
        seconds: u32,
        activation_seconds: u32,
    ) -> ConversationResult {
        let ActiveConversation::ServerDm { peer_id } = &self.conversation else {
            return Err(ConversationError::Other(
                "activation windows are available for friend DMs".into(),
            ));
        };
        let notice = self
            .client
            .messages()
            .set_server_dm_disappearing_window(peer_id, seconds, activation_seconds)
            .await
            .map_err(ConversationError::Message)?;
        if let Some(message) = notice {
            self.client
                .event_bus()
                .emit_domain(crate::inbox::handlers::DomainEvent::MessageReceived { message });
        }
        Ok(())
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
                self.client
                    .channels()
                    .channel_repo
                    .set_default_ttl(channel_id, ttl)
                    .map_err(|e| ConversationError::Other(e.to_string()))
            }
            ActiveConversation::AnonDm(_) | ActiveConversation::AnonGroup(_) => {
                Err(ConversationError::Other(
                    "disappearing messages are not available in anonymous conversations".into(),
                ))
            }
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
            if let Ok(msg) = self.lookup_message(&user_id, sent_timestamp) {
                let new_content =
                    crate::domain::message::MessageContent::Text(new_text.to_string());
                let _ = self
                    .client
                    .messages()
                    .update_message_body(&msg.id, &new_content);
            }
        }
    }

    fn lookup_message(
        &self,
        sender_id: &UserId,
        sent_timestamp: u64,
    ) -> ConversationResult<crate::domain::Message> {
        scoped_stored_message(
            &self.client.messages().message_repo,
            &self.storage_scope()?,
            sender_id,
            sent_timestamp,
        )
    }

    /// Send content to a friend DM. The transport (Shield: mailbox; normal:
    /// sealed-sender streaming) is `send_friend_dm_content`'s decision.
    async fn send_friend_dm_content(
        &self,
        content: crate::proto::content_v1::Content,
        peer_id: &UserId,
    ) -> ConversationResult {
        self.client
            .messages()
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
        self.client
            .messages()
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
            ActiveConversation::ServerDm { peer_id } => {
                self.send_friend_dm_content(content, peer_id).await
            }
            ActiveConversation::Channel(channel_id) => {
                self.send_channel_content(content, channel_id).await
            }
            _ => Err(ConversationError::NoConversation),
        }
    }
}

/// Lifecycle-owned recovery; at most four due ORIGINAL batches per tick, with
/// exponential backoff and paused denied/exhausted batches retained locally.
pub(crate) fn spawn_delete_batch_recovery(
    client: &Arc<ChatterClient>,
) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(client);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let Some(client) = weak.upgrade() else {
                break;
            };
            let state = client.auth_state().get();
            let Some(account) = state.current_user_id else {
                continue;
            };
            let journal=crate::storage::repositories::delete_batch_retry_repo::DeleteBatchRetryRepository::new(client.messages().database.clone());
            let due = match journal.due(&account) {
                Ok(due) => due,
                Err(error) => {
                    tracing::warn!(%error,"Cannot load deletion retries");
                    continue;
                }
            };
            for (scope, batch) in due {
                let Some((kind, id)) = scope.split_once(':') else {
                    continue;
                };
                let conversation = match kind {
                    "channel" => id.parse().ok().map(ActiveConversation::Channel),
                    "server_dm" => id
                        .parse()
                        .ok()
                        .map(|peer_id| ActiveConversation::ServerDm { peer_id }),
                    "anon_dm" => id.parse().ok().map(ActiveConversation::AnonDm),
                    "anon_group" => id.parse().ok().map(ActiveConversation::AnonGroup),
                    _ => None,
                };
                let Some(conversation) = conversation else {
                    continue;
                };
                let targets = batch
                    .targets
                    .iter()
                    .map(|t| (t.author, t.timestamp))
                    .collect::<Vec<_>>();
                if let Err(error) = client
                    .conversation(conversation)
                    .send_delete_batch_in_session(&targets, Some((account, state.session_revision)))
                    .await
                {
                    tracing::warn!(%error,%scope,batch_id=%batch.id,"Deletion retry remains pending");
                }
            }
        }
    })
}

/// Shared by actual dispatch and regression transport injection. The caller has
/// already allocated the action clock; completion cannot allocate another one.
pub(crate) async fn complete_reaction_dispatch<F, P>(
    action_ts: u64,
    dispatch: F,
    persist: P,
) -> ConversationResult
where
    F: std::future::Future<Output = ConversationResult>,
    P: FnOnce(u64) -> Result<bool, ConversationError>,
{
    dispatch.await?;
    if !persist(action_ts)? {
        return Err(ConversationError::Other(
            "Reaction action superseded while sending".into(),
        ));
    }
    Ok(())
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
        assert!(
            out[0].path.as_os_str().is_empty(),
            "there is no local file to upload"
        );
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
            .server_url("https://localhost:1")
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
        client
            .messages()
            .message_repo
            .insert(&message)
            .expect("store message");
    }

    fn unread(client: &ChatterClient, id: &ChannelId, conv_type: &ConversationType) -> u64 {
        client
            .messages()
            .message_repo
            .get_unread_count(id, conv_type)
            .expect("unread count")
    }

    #[tokio::test]
    async fn live_receive_marks_only_the_foreground_conversation_read() {
        let (client, _dir) = test_client();
        let peer = UserId::new();
        let id: ChannelId = peer.into();
        let message = crate::domain::message::Message::new_text(
            id,
            ConversationType::ServerDm,
            Some(peer),
            "live".into(),
            1,
        );
        client.messages().message_repo.insert(&message).unwrap();
        client
            .chat_state()
            .set_active_conversation(ActiveConversation::Channel(ChannelId::new()));
        ConversationHandle::mark_received_as_read(&client, &message)
            .await
            .unwrap();
        assert_eq!(unread(&client, &id, &ConversationType::ServerDm), 1);

        client
            .chat_state()
            .set_active_conversation(ActiveConversation::ServerDm { peer_id: peer });
        client.chat_state().set_app_visible(false);
        ConversationHandle::mark_received_as_read(&client, &message)
            .await
            .unwrap();
        assert_eq!(unread(&client, &id, &ConversationType::ServerDm), 1);

        client.chat_state().set_app_visible(true);
        ConversationHandle::mark_received_as_read(&client, &message)
            .await
            .unwrap();
        assert_eq!(unread(&client, &id, &ConversationType::ServerDm), 0);

        // A subsequent arrival is read without reopening the conversation,
        // even when sending peer read receipts is disabled.
        store_peer_message(&client, id, ConversationType::ServerDm, peer, 2);
        ConversationHandle::mark_received_as_read(&client, &message)
            .await
            .unwrap();
        assert_eq!(unread(&client, &id, &ConversationType::ServerDm), 0);
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
        ms.settings_repo
            .set_string(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        ms.settings_repo
            .set(keys::CURRENT_DEVICE_ID, &1u32)
            .unwrap();
        let content = || crate::services::build_text_content("hi", 1);

        ms.shield_mode.store(true, Ordering::Release);
        let shield = ms.send_friend_dm_content(&friend, content()).await;
        assert!(
            matches!(
                shield,
                Err(crate::services::message::MessageError::NotReachablePrivately(_))
            ),
            "Shield must take the mailbox, got {shield:?}"
        );
        assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());

        ms.shield_mode.store(false, Ordering::Release);
        let normal = ms.send_friend_dm_content(&friend, content()).await;
        assert!(
            !matches!(
                normal,
                Err(crate::services::message::MessageError::BootstrapInitiated)
            ),
            "normal mode must not take the mailbox"
        );
        assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());
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
            store_peer_message(
                &client,
                conversation_id,
                ConversationType::ServerDm,
                peer,
                n,
            );
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
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");

        let peer = UserId::from_uuid(uuid::Uuid::from_u128(7));
        let mut dm = AnonDm::new_initiated(peer);
        dm.peer_display_name = Some("Robin".into());
        client
            .anon_dms()
            .anon_dm_repo()
            .insert_conversation(&dm)
            .unwrap();
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
        let mut fwd = Message::new_text(
            elsewhere,
            ConversationType::ServerDm,
            None,
            String::new(),
            2,
        );
        fwd.attachments = vec![forwarded];
        client.messages().message_repo.insert(&fwd).unwrap();

        client
            .delete_conversation(ActiveConversation::AnonDm(dm.id))
            .unwrap();

        assert!(client
            .anon_dms()
            .get_conversation(&dm.id)
            .unwrap()
            .is_none());
        assert!(client
            .messages()
            .message_repo
            .get(&msg.id)
            .unwrap()
            .is_none());
        assert!(attachments.get(&only_here).unwrap().is_none());
        assert!(
            !only_here_path.exists(),
            "the deleted chat's file stayed on disk"
        );
        assert!(attachments.get(&forwarded).unwrap().is_some());
        assert!(
            forwarded_path.exists(),
            "a file another chat still shows was deleted"
        );
        assert!(client
            .messages()
            .message_repo
            .get(&fwd.id)
            .unwrap()
            .is_some());
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
            .server_url("https://localhost:1")
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
        let mut msg =
            Message::new_text(conv, ConversationType::ServerDm, Some(peer), "hi".into(), 1);
        msg.attachments = vec![only_here, forwarded];
        client.messages().message_repo.insert(&msg).unwrap();
        let elsewhere: ChannelId = UserId::from_uuid(uuid::Uuid::from_u128(12)).into();
        let mut fwd = Message::new_text(
            elsewhere,
            ConversationType::ServerDm,
            None,
            String::new(),
            2,
        );
        fwd.attachments = vec![forwarded];
        client.messages().message_repo.insert(&fwd).unwrap();

        client
            .delete_conversation(ActiveConversation::ServerDm { peer_id: peer })
            .unwrap();

        assert!(client
            .messages()
            .message_repo
            .get(&msg.id)
            .unwrap()
            .is_none());
        assert!(attachments.get(&only_here).unwrap().is_none());
        assert!(
            !only_here_path.exists(),
            "the deleted chat's file stayed on disk"
        );
        assert!(attachments.get(&forwarded).unwrap().is_some());
        assert!(
            forwarded_path.exists(),
            "a file another chat still shows was deleted"
        );
        assert!(client
            .messages()
            .message_repo
            .get(&fwd.id)
            .unwrap()
            .is_some());
    }
    #[tokio::test]
    async fn reaction_snapshot_is_scoped_and_repairs_persisted_add_remove_duplicates() {
        use crate::domain::anon_dm::AnonDmId;
        use crate::domain::anon_group::AnonGroupId;
        let (client, _dir) = test_client();
        let own = UserId::new();
        let author = UserId::new();
        let other = UserId::new();
        client
            .auth_state()
            .set_logged_in(own, 1, "reaction fixture".into());
        crate::storage::repositories::settings_repo::SettingsRepository::new(
            client.messages().database(),
        )
        .set_string(
            crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
            &own.to_string(),
        )
        .unwrap();
        let encrypted_channel = ChannelId::new();
        let scopes = [
            (
                ActiveConversation::Channel(ChannelId::new()),
                ConversationType::Channel,
            ),
            (
                ActiveConversation::Channel(encrypted_channel),
                ConversationType::AnonGroup,
            ),
            (
                ActiveConversation::ServerDm {
                    peer_id: UserId::new(),
                },
                ConversationType::ServerDm,
            ),
            (
                ActiveConversation::AnonDm(AnonDmId::new()),
                ConversationType::AnonDm,
            ),
            (
                ActiveConversation::AnonGroup(AnonGroupId::new()),
                ConversationType::AnonGroup,
            ),
        ];
        for (index, (scope, kind)) in scopes.into_iter().enumerate() {
            let sent_at = 400 + index as u64;
            let id = scope.channel_id().unwrap();
            store_peer_message(&client, id, kind.clone(), author, sent_at);
            let handle = client.conversation(scope);
            let snapshot = handle.reaction_snapshot(&author, sent_at).unwrap().unwrap();
            assert!(snapshot.counts.is_empty());
            assert_eq!(snapshot.conversation, scope);
            assert_eq!(snapshot.author, author);
            assert_eq!(snapshot.sent_at, sent_at);
            for reactor in [own, own, other] {
                client
                    .messages()
                    .record_reaction(
                        &crate::domain::reaction::ReactionScope::new(id, kind.clone()),
                        &author,
                        sent_at,
                        &reactor,
                        "👍",
                        false,
                    )
                    .unwrap();
            }
            let snapshot = handle.reaction_snapshot(&author, sent_at).unwrap().unwrap();
            assert_eq!(snapshot.counts[0].count, 2);
            assert_eq!(snapshot.own_emojis, vec!["👍".to_string()]);
            for reactor in [own, own] {
                client
                    .messages()
                    .record_reaction(
                        &crate::domain::reaction::ReactionScope::new(id, kind.clone()),
                        &author,
                        sent_at,
                        &reactor,
                        "👍",
                        true,
                    )
                    .unwrap();
            }
            let snapshot = handle.reaction_snapshot(&author, sent_at).unwrap().unwrap();
            assert_eq!(snapshot.counts[0].count, 1);
            assert!(snapshot.own_emojis.is_empty());
            client
                .messages()
                .record_reaction(
                    &crate::domain::reaction::ReactionScope::new(id, kind.clone()),
                    &author,
                    sent_at,
                    &other,
                    "👍",
                    true,
                )
                .unwrap();
            let snapshot = handle.reaction_snapshot(&author, sent_at).unwrap().unwrap();
            assert!(snapshot.counts.is_empty());
            assert!(snapshot.summary.is_empty());
            assert!(client
                .conversation(ActiveConversation::Channel(ChannelId::new()))
                .reaction_snapshot(&author, sent_at)
                .unwrap()
                .is_none());
            assert!(handle.reaction_snapshot(&other, sent_at).unwrap().is_none());
            assert!(handle
                .reaction_snapshot(&author, sent_at + 100)
                .unwrap()
                .is_none());
        }
        // Same UUID does not make an anonymous group into a friend DM.
        assert!(client
            .conversation(ActiveConversation::ServerDm {
                peer_id: encrypted_channel.to_string().parse().unwrap()
            })
            .reaction_snapshot(&author, 401)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn reaction_toggle_does_not_invent_success_for_missing_target() {
        let (client, _dir) = test_client();
        let own = UserId::new();
        client
            .auth_state()
            .set_logged_in(own, 1, "reaction fixture".into());
        crate::storage::repositories::settings_repo::SettingsRepository::new(
            client.messages().database(),
        )
        .set_string(
            crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
            &own.to_string(),
        )
        .unwrap();
        let handle = client.conversation(ActiveConversation::Channel(ChannelId::new()));
        assert!(handle.toggle_reaction("👍", &own, 700).await.is_err());
    }
}

#[cfg(test)]
#[path = "reaction_dispatch_tests.rs"]
mod reaction_dispatch_tests;

#[cfg(test)]
#[path = "saved_conversation_tests.rs"]
mod saved_conversation_tests;

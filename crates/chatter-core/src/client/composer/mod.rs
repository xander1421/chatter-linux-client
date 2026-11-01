//! A send frozen at the moment it was asked for.
//!
//! Send reads the composer once and clears it. A send that only completes
//! later — a GIF, which is downloaded and uploaded first — must deliver what
//! the composer held THEN, to the conversation it was asked for in, never
//! whatever the composer and the active conversation hold when the upload
//! finishes.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use prost::Message as _;
use unicode_segmentation::UnicodeSegmentation;

pub(super) mod queue;

use super::ChatterClient;
use crate::domain::anon_dm::AnonDmId;
use crate::domain::anon_group::AnonGroupId;
use crate::domain::ids::{AttachmentId, ChannelId, MessageId, UserId};
use crate::state::{ActiveConversation, PendingAttachment};

/// The message being replied to, as the composer's reply bar holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ComposerReply {
    pub sender_id: String,
    pub timestamp: String,
    pub sender_name: String,
    pub preview: String,
    /// Presentation-only RGBA thumbnail, retained across delayed send recovery
    /// but not across a restart: a restored reply bar shows text only.
    #[serde(skip)]
    pub thumbnail: Option<(Vec<u8>, u32, u32)>,
}

/// What a conversation's composer holds while another conversation is open:
/// unsent text and the reply bar's target. Staged files live in the
/// per-conversation attachment trays.
///
/// Drafts outlive a restart: the signed-in account's map is one row of the
/// encrypted settings table (`ui.composer_drafts.<owner>`), loaded on the
/// first use after sign-in and rewritten on every change. Sign-out deletes
/// the row. Accounts never share a key, so one account cannot see another's
/// unsent text.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ComposerDraft {
    pub text: String,
    pub reply: Option<ComposerReply>,
}

impl ComposerDraft {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.reply.is_none()
    }
}

/// Everything one send commits to: where it goes and what it carries.
#[derive(Clone, Debug)]
pub struct ComposerSend {
    /// Captured before uploads/deposits; cloning a frozen send keeps its identity.
    pub attempt_id: uuid::Uuid,
    pub session_revision: u64,
    pub conversation: ActiveConversation,
    pub text: String,
    pub reply: Option<ComposerReply>,
    pub attachments: Vec<PendingAttachment>,
    pub(crate) queue_ticket: Option<std::sync::Arc<queue::QueueTicket>>,
}

/// Keep encoded text below half the sealed inbox cap. The other half is for
/// encryption, profile keys, quote metadata, and attachment pointers.
fn split_composer_text(text: &str) -> Vec<String> {
    let budget = crate::crypto::signal::sealed_sender::MAX_SEALED_BODY_BYTES / 2;
    let encoded_len = |body: &str| {
        crate::services::build_data_message_content(body, vec![], 0).encoded_len()
    };
    if encoded_len(text) <= budget {
        return vec![text.to_owned()];
    }
    let mut parts = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if encoded_len(rest) <= budget {
            parts.push(rest.to_owned());
            break;
        }
        let boundaries: Vec<usize> = rest.grapheme_indices(true)
            .map(|(index, grapheme)| index + grapheme.len()).collect();
        let mut low = 0;
        let mut high = boundaries.len();
        while low < high {
            let middle = (low + high) / 2;
            if encoded_len(&rest[..boundaries[middle]]) <= budget {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let limit = boundaries[low.saturating_sub(1)];
        let candidate = &rest[..limit];
        let boundary = candidate.rfind('\n').map(|i| i + 1)
            .or_else(|| candidate.char_indices().rev()
                .find(|(_, c)| c.is_whitespace()).map(|(i, c)| i + c.len_utf8()))
            .unwrap_or(limit);
        parts.push(rest[..boundary].to_owned());
        rest = &rest[boundary..];
    }
    parts
}

impl ComposerSend {
    pub fn is_upload_queued(&self) -> bool {
        self.queue_ticket.as_ref().is_some_and(|ticket| ticket.is_waiting())
    }

    /// Normal messages, in wire order. Attachments and the quote belong to the
    /// first part; each later part receives its own send identity.
    pub fn ordered_parts(self) -> Vec<Self> {
        let texts = split_composer_text(&self.text);
        if texts.len() == 1 { return vec![self]; }
        texts.into_iter().enumerate().map(|(index, text)| Self {
            attempt_id: if index == 0 { self.attempt_id } else { uuid::Uuid::new_v4() },
            session_revision: self.session_revision,
            conversation: self.conversation,
            text,
            reply: if index == 0 { self.reply.clone() } else { None },
            attachments: if index == 0 { self.attachments.clone() } else { vec![] },
            queue_ticket: self.queue_ticket.clone(),
        }).collect()
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;

    #[test]
    fn long_text_parts_fit_and_preserve_graphemes() {
        let budget = crate::crypto::signal::sealed_sender::MAX_SEALED_BODY_BYTES / 2;
        for text in ["a".repeat(60_000), "👩🏽‍💻éक्‍ष".repeat(3_000)] {
            let parts = split_composer_text(&text);
            assert!(parts.len() > 1);
            assert_eq!(parts.concat(), text);
            assert_eq!(
                parts.iter().flat_map(|part| part.graphemes(true)).collect::<Vec<_>>(),
                text.graphemes(true).collect::<Vec<_>>()
            );
            for part in parts {
                assert!(crate::services::build_data_message_content(&part, vec![], 0).encoded_len() <= budget);
            }
        }
    }
}

// ── Recovery row ────────────────────────────────────────────────────
//
// What a restart would otherwise lose, per account, in one encrypted
// settings row (`ui.composer_recovery.<owner>`): the staged attachment trays
// of every conversation and the failed sends parked for later. Rewritten
// whenever a tray or the parked list changes — rare events, unlike
// keystrokes — and on Android's pause edge through the draft autosave.
//
// Files the app itself produced (an Android SAF copy, a pasted image) live
// under `data_dir/composer-staging/<owner>/<uuid>/<name>`, which no cache
// purge reaches, until their send is accepted, they are removed from the
// tray, or the account signs out. A file the user owns (a desktop original
// picked by path) is only ever referenced, never moved or deleted.

/// A staged attachment as the recovery row stores it: `PendingAttachment`
/// with the pre-uploaded pointer (GIF, voice note) as its prost bytes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct StagedAttachment {
    id: AttachmentId,
    filename: String,
    path: PathBuf,
    size: u64,
    mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pointer: Option<Vec<u8>>,
}

impl From<&PendingAttachment> for StagedAttachment {
    fn from(a: &PendingAttachment) -> Self {
        Self {
            id: a.id,
            filename: a.filename.clone(),
            path: a.path.clone(),
            size: a.size,
            mime_type: a.mime_type.clone(),
            pointer: a.pointer.as_ref().map(|pointer| {
                <crate::proto::content_v1::AttachmentPointer as prost::Message>::encode_to_vec(
                    pointer,
                )
            }),
        }
    }
}

impl StagedAttachment {
    /// Back into a tray entry, as the row recorded it. An attachment whose
    /// file is gone, or whose stored pointer no longer decodes, comes back
    /// too: it is what the user staged, the chip is the evidence of it, and
    /// the send it is part of fails as a whole (`upload_attachments` stops
    /// at the first unreadable file) until the user removes it. Dropping it
    /// here would send the remainder as if it were everything.
    fn into_pending(self) -> PendingAttachment {
        let pointer = self.pointer.and_then(|bytes| {
            <crate::proto::content_v1::AttachmentPointer as prost::Message>::decode(
                bytes.as_slice(),
            )
            .map_err(|e| {
                tracing::warn!(
                    filename = %self.filename,
                    error = %e,
                    "staged attachment pointer no longer decodes; kept as unsendable evidence"
                );
            })
            .ok()
        });
        PendingAttachment {
            id: self.id,
            filename: self.filename,
            path: self.path,
            size: self.size,
            mime_type: self.mime_type,
            pointer,
        }
    }
}

/// `ActiveConversation` minus `None`, as stored. Kept apart from the state
/// enum so the row format does not follow every navigation refactor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum ConversationKey {
    Channel(ChannelId),
    ServerDm(UserId),
    AnonDm(AnonDmId),
    AnonGroup(AnonGroupId),
}

impl ConversationKey {
    fn of(conversation: ActiveConversation) -> Option<Self> {
        Some(match conversation {
            ActiveConversation::None => return None,
            ActiveConversation::Channel(id) => Self::Channel(id),
            ActiveConversation::ServerDm { peer_id } => Self::ServerDm(peer_id),
            ActiveConversation::AnonDm(id) => Self::AnonDm(id),
            ActiveConversation::AnonGroup(id) => Self::AnonGroup(id),
        })
    }

    fn into_active(self) -> ActiveConversation {
        match self {
            Self::Channel(id) => ActiveConversation::Channel(id),
            Self::ServerDm(peer_id) => ActiveConversation::ServerDm { peer_id },
            Self::AnonDm(id) => ActiveConversation::AnonDm(id),
            Self::AnonGroup(id) => ActiveConversation::AnonGroup(id),
        }
    }
}

/// A failed send kept for its conversation, as stored. The attempt id and
/// session are not: a restored send is a new attempt in the new session.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ParkedSend {
    conversation: ConversationKey,
    text: String,
    reply: Option<ComposerReply>,
    attachments: Vec<StagedAttachment>,
}

impl ParkedSend {
    fn of(send: &ComposerSend) -> Option<Self> {
        Some(Self {
            conversation: ConversationKey::of(send.conversation)?,
            text: send.text.clone(),
            reply: send.reply.clone(),
            attachments: send
                .attachments
                .iter()
                .map(StagedAttachment::from)
                .collect(),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ComposerRecovery {
    /// Staged attachments per conversation id (the tray key), the active
    /// conversation's tray included.
    trays: HashMap<String, Vec<StagedAttachment>>,
    thread_trays: HashMap<String, Vec<StagedAttachment>>,
    parked: Vec<ParkedSend>,
}

/// File encryption material lives in SQLCipher, as it does for downloaded
/// attachments. The staged file itself contains only authenticated ciphertext.
#[derive(serde::Serialize, serde::Deserialize)]
struct StagingCrypto {
    key: Vec<u8>,
    digest: Vec<u8>,
    plaintext_size: u64,
}

impl ComposerRecovery {
    fn is_empty(&self) -> bool {
        self.trays.is_empty() && self.thread_trays.is_empty() && self.parked.is_empty()
    }
}

impl ChatterClient {
    pub fn recording_context_is_current(
        &self,
        conversation: crate::state::ActiveConversation,
    ) -> bool {
        crate::services::voice_recording::recording_context_is_current(
            &self.services.chat_state,
            conversation,
        )
    }

    pub fn start_voice_recording(
        &self,
        conversation: crate::state::ActiveConversation,
    ) -> (
        crate::services::voice_recording::RecordingSession,
        tokio::sync::mpsc::UnboundedReceiver<crate::services::voice_recording::RecordingEvent>,
    ) {
        crate::services::voice_recording::RecordingSession::start(
            self.services.chat_state.clone(),
            conversation,
            self.directories().cache_dir().to_path_buf(),
        )
    }

    pub fn pending_attachments(&self) -> Vec<crate::state::PendingAttachment> {
        self.services.chat_state.get_pending_attachments()
    }

    pub fn add_pending_attachment_for(
        &self,
        conversation: crate::state::ActiveConversation,
        revision: u64,
        attachment: crate::state::PendingAttachment,
    ) -> bool {
        self.services
            .chat_state
            .add_pending_attachment_for(conversation, revision, attachment)
    }

    pub fn add_pending_attachment(&self, attachment: crate::state::PendingAttachment) {
        self.services.chat_state.add_pending_attachment(attachment);
    }

    pub fn thread_pending_attachments(&self, root: &MessageId) -> Vec<PendingAttachment> {
        self.services.chat_state.thread_pending_attachments(root)
    }

    pub fn add_thread_pending_attachment_for(
        &self,
        root: MessageId,
        revision: u64,
        attachment: PendingAttachment,
    ) {
        self.services.chat_state.add_thread_attachment_for(root, revision, attachment);
    }

    pub fn remove_thread_pending_attachment(&self, root: MessageId, id: AttachmentId) {
        self.services.chat_state.remove_thread_attachment(root, id);
    }

    pub fn clear_pending_attachments(&self) {
        self.services.chat_state.clear_pending_attachments();
    }

    pub fn remove_pending_attachment(&self, id: crate::domain::ids::AttachmentId) {
        self.services.chat_state.remove_pending_attachment(id);
    }

    /// Freeze the composer into a send to the active conversation, taking the
    /// staged attachments with it. `Err` carries what to tell the user, and
    /// consumes nothing: no conversation open (a mesh room is none), Jarvis's
    /// local channel or a Jarvis request in the text (both answered locally,
    /// never posted), or a channel we may not post in.
    pub fn begin_composer_send(
        &self,
        text: String,
        reply: Option<ComposerReply>,
    ) -> Result<ComposerSend, String> {
        let text = text.trim().to_owned();
        let session_revision = self.auth_state().get().session_revision;
        let chat = self.chat_state().get();
        let conversation = chat.active_conversation;
        if let ActiveConversation::None = conversation {
            return Err("This can't be sent here.".into());
        }
        if chat.active_channel_id != conversation.channel_id() || !self.composer_shows_active() {
            return Err("The conversation is changing. Nothing was sent.".into());
        }
        if let ActiveConversation::Channel(id) = conversation {
            #[cfg(feature = "jarvis")]
            if super::jarvis::is_jarvis_channel(&id) {
                return Err("This can't be sent here.".into());
            }
            let notice = self.channel_send_notice(&id);
            if !notice.is_empty() {
                return Err(notice);
            }
            if self.is_former_channel(&id) {
                return Err("You can no longer post in this channel.".into());
            }
        }
        #[cfg(feature = "jarvis")]
        if crate::services::jarvis::parse_slash_command(&text).is_some()
            || self.jarvis_mention(&conversation, &text).is_some()
        {
            return Err("Send the Jarvis request on its own first.".into());
        }
        let ticket = self.reserve_composer_turn(conversation, session_revision)?;
        if !self.composer_session_is_current(session_revision) {
            return Err("The account session changed. Nothing was sent.".into());
        }
        let attachments = self
            .chat_state()
            .take_pending_attachments_for(conversation, chat.attachment_revision)
            .ok_or_else(|| "The conversation changed. Nothing was sent.".to_string())?;
        let send = ComposerSend {
            attempt_id: uuid::Uuid::new_v4(),
            session_revision,
            conversation,
            text,
            reply,
            attachments,
            queue_ticket: Some(ticket),
        };
        if let Err(error) = self.persist_queued_composer(&send) {
            for attachment in send.attachments {
                self.chat_state().add_pending_attachment_for(conversation, chat.attachment_revision, attachment);
            }
            return Err(error);
        }
        Ok(send)
    }

    /// Freeze a thread composer without touching the main conversation's tray.
    pub fn begin_thread_composer_send(
        &self,
        root: MessageId,
        text: String,
        reply: Option<ComposerReply>,
    ) -> Result<ComposerSend, String> {
        let thread = self.active_thread().filter(|thread| thread.root_message_id == root)
            .ok_or_else(|| "The thread changed. Nothing was sent.".to_string())?;
        let chat = self.chat_state().get();
        let conversation = chat.active_conversation;
        if conversation.channel_id() != Some(thread.channel_id) {
            return Err("The conversation changed. Nothing was sent.".into());
        }
        if let ActiveConversation::Channel(channel) = conversation {
            let notice = self.channel_send_notice(&channel);
            if !notice.is_empty() { return Err(notice); }
            if self.is_former_channel(&channel) {
                return Err("You can no longer post in this channel.".into());
            }
        }
        let text = text.trim().to_owned();
        let attachments = self.chat_state().thread_pending_attachments(&root);
        if text.is_empty() && attachments.is_empty() {
            return Err("Add a message or attachment first.".into());
        }
        self.ensure_send_available(conversation).map_err(|e| e.to_string())?;
        let session_revision = self.auth_state().get().session_revision;
        if !self.composer_session_is_current(session_revision) {
            return Err("The account session changed. Nothing was sent.".into());
        }
        let attachments = self.chat_state()
            .take_thread_attachments_for(root, chat.attachment_revision)
            .ok_or_else(|| "The thread changed. Nothing was sent.".to_string())?;
        Ok(ComposerSend {
            attempt_id: uuid::Uuid::new_v4(),
            session_revision,
            conversation,
            text,
            reply,
            attachments,
            queue_ticket: None,
        })
    }

    pub fn composer_session_is_current(&self, revision: u64) -> bool {
        self.auth_state().get().session_revision == revision
    }

    /// The draft currently presented, which may lag core navigation.
    pub fn composer_owner_key(&self) -> String {
        self.composer_owner.lock().clone()
    }

    /// Reject an expired intent before entering the authenticated send path.
    pub async fn send_composer_message(
        &self,
        send: &ComposerSend,
        quote: Option<crate::services::gateway::QuoteIntent>,
    ) -> Option<Result<super::messaging::SendMessageUiResult, crate::services::gateway::SendError>>
    {
        if !self.await_composer_turn(send).await {
            return None;
        }
        Some(
            self.send_message_ui_attempt(
                send.attempt_id,
                send.conversation,
                &send.text,
                &send.attachments,
                quote,
                None,
            )
            .await,
        )
    }

    pub async fn send_thread_composer_message(
        &self,
        send: &ComposerSend,
        root: MessageId,
        quote: Option<crate::services::gateway::QuoteIntent>,
    ) -> Result<crate::domain::message::Message, String> {
        if !self.composer_session_is_current(send.session_revision) {
            return Err("The account session changed. Nothing was sent.".into());
        }
        let channel = send.conversation.channel_id()
            .ok_or_else(|| "The conversation changed. Nothing was sent.".to_string())?;
        let mut result = None;
        for (index, part) in send.clone().ordered_parts().into_iter().enumerate() {
            let uploaded = super::upload_attachments(
                &part.attachments,
                &self.attachments().for_conversation(&part.conversation),
                Some(self),
            ).await.map_err(|error| error.to_string())?;
            if !self.composer_session_is_current(part.session_revision) {
                return Err("The account session changed. Nothing was sent.".into());
            }
            let ids = uploaded.pointers.iter()
                .map(|pointer| AttachmentId::from_cdn_key(&pointer.cdn_key))
                .collect();
            result = Some(self.send_channel_message_with_quote(
                None, &channel, &part.text, ids, uploaded.pointers,
                Some(&root), vec![], false, if index == 0 { quote.as_ref() } else { None },
            ).await.map_err(|error| error.to_string())?);
        }
        result.ok_or_else(|| "Add a message or attachment first.".into())
    }

    /// Account-local drafts and failed sends never survive sign-out, on disk
    /// included. Both metadata rows are deleted for the signed-in account, whether
    /// or not its drafts were ever loaded this session (sign-out straight
    /// from the list, before any conversation was opened), or, when the
    /// caller already signed the account out (`delete_account`), the one the
    /// map was loaded for. A loaded owner that is not the signed-in account
    /// is a previous session's; that account is not signing out and keeps
    /// its row. Only the explicit sign-out paths call this; `shutdown`
    /// invalidates the session without touching the row.
    pub fn clear_composer_session(&self) {
        let mut drafts = self.composer_drafts.lock();
        let signed_in = self.auth_state().get().current_user_id;
        self.auth_state().invalidate_session();
        self.parked_sends.lock().clear();
        drafts.clear();
        let loaded = self.composer_draft_owner.lock().take();
        if let Some(owner) = signed_in.or(loaded) {
            let repo = self.settings_repo();
            let drafts_deleted = repo.delete(&Self::composer_drafts_key(owner));
            let recovery_deleted = repo.delete(&Self::composer_recovery_key(owner));
            self.delete_queued_composers(owner);
            if drafts_deleted.is_err() || recovery_deleted.is_err() {
                tracing::warn!("persisted composer drafts not deleted at sign-out");
            }
            // The account's staged copies go with its row.
            self.wipe_staging_dir(owner);
        }
        self.composer_drafts_dirty.store(false, Ordering::SeqCst);
        self.composer_recovery_dirty.store(false, Ordering::SeqCst);
        self.composer_drafts_unread.store(false, Ordering::SeqCst);
        self.composer_recovery_unread.store(false, Ordering::SeqCst);
        self.composer_owner.lock().clear();
        self.chat_state().clear_attachment_trays();
        self.chat_state()
            .set_active_conversation(ActiveConversation::None);
        drop(drafts);
    }

    /// Hand a failed send back. `Some`: restore it now — its conversation is
    /// open and `composer_empty` (nothing typed, replied to, or edited since)
    /// with nothing staged. Otherwise it is kept, never written over newer
    /// input, until [`Self::take_parked_send`] finds room for it.
    pub fn recover_composer_send(
        &self,
        mut send: ComposerSend,
        composer_empty: bool,
    ) -> Option<ComposerSend> {
        if !self.composer_session_is_current(send.session_revision) {
            return None;
        }
        if send.queue_ticket.as_ref().is_some_and(|ticket| ticket.is_cancelled()) {
            return None;
        }
        match self.park_unstarted_queued_send(&send) {
            Ok(false) => {},
            Ok(true) | Err(()) => return None,
        }
        // A completed failed task no longer owns admission. Keeping its
        // recoverable draft must not hold later accepted batches forever.
        let ticket = send.queue_ticket.take();
        if self.composer_has_room(send.conversation, composer_empty) {
            return Some(send);
        }
        let (_drafts, _) = self.composer_drafts_for_session();
        if ticket.as_ref().is_some_and(|ticket| ticket.is_cancelled()) {
            return None;
        }
        if !self.composer_session_is_current(send.session_revision) {
            return None;
        }
        // Owner before the parked lock: the session loader holds the drafts
        // lock while it fills the parked list, and asks for nothing else.
        let owner = self.recovery_owner();
        let mut parked = self.parked_sends.lock();
        parked.push(send);
        self.composer_recovery_dirty.store(true, Ordering::SeqCst);
        if let Some(owner) = owner {
            self.persist_recovery_with(owner, &parked);
        }
        None
    }

    /// A kept failed send for the active conversation, if the composer has
    /// room for it now. Checked on entering a conversation.
    pub fn take_parked_send(&self, composer_empty: bool) -> Option<ComposerSend> {
        let active = self.chat_state().active_conversation();
        if !self.composer_has_room(active, composer_empty) {
            return None;
        }
        let (mut drafts, _) = self.composer_drafts_for_session();
        if !self.composer_has_room(active, composer_empty) {
            return None;
        }
        let owner = self.recovery_owner()?;
        if self.composer_drafts_unread.load(Ordering::SeqCst) {
            return None;
        }
        let mut parked = self.parked_sends.lock();
        let revision = self.auth_state().get().session_revision;
        let at = parked
            .iter()
            .position(|s| s.conversation == active && s.session_revision == revision)?;
        let send = parked[at].clone();
        let key = active.channel_id()?.to_string();
        let mut next_drafts = drafts.clone();
        next_drafts.insert(
            key.clone(),
            ComposerDraft {
                text: send.text.clone(),
                reply: send.reply.clone(),
            },
        );
        let mut next_parked = parked.clone();
        next_parked.remove(at);
        let mut recovery = self.composer_recovery_snapshot(&next_parked);
        if !send.attachments.is_empty() {
            recovery.trays.insert(
                key,
                send.attachments
                    .iter()
                    .map(StagedAttachment::from)
                    .collect(),
            );
        }
        // Both destinations replace the parked intent in one encrypted DB
        // transaction, before the UI takes possession of it.
        let written = (|| -> Result<(), crate::error::StorageError> {
            let mut conn = self.services.database.connection()?;
            let tx = conn.transaction()?;
            let now = chrono::Utc::now().to_rfc3339();
            for (key, value) in [
                (
                    Self::composer_drafts_key(owner),
                    serde_json::to_string(&next_drafts),
                ),
                (
                    Self::composer_recovery_key(owner),
                    serde_json::to_string(&recovery),
                ),
            ] {
                let value =
                    value.map_err(|e| crate::error::StorageError::InvalidData(e.to_string()))?;
                tx.execute("INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at", rusqlite::params![key, value, now])?;
            }
            tx.commit()?;
            Ok(())
        })();
        if written.is_err() {
            self.composer_drafts_dirty.store(true, Ordering::SeqCst);
            self.composer_recovery_dirty.store(true, Ordering::SeqCst);
            tracing::warn!("parked composer transfer not saved; intent remains parked");
            return None;
        }
        let chat_revision = self.chat_state().get().attachment_revision;
        for attachment in send.attachments.iter().cloned() {
            self.chat_state().add_pending_attachment_for(
                send.conversation,
                chat_revision,
                attachment,
            );
        }
        *drafts = next_drafts;
        *parked = next_parked;
        self.composer_drafts_dirty.store(false, Ordering::SeqCst);
        self.composer_recovery_dirty.store(false, Ordering::SeqCst);
        Some(send)
    }

    /// Mid-switch the active conversation is already the new one while the
    /// composer still holds the old one's draft (the UI's watcher has not
    /// swapped it yet): the composer belongs to neither.
    fn composer_shows_active(&self) -> bool {
        let shown = self
            .chat_state()
            .get()
            .active_channel_id
            .map(|id| id.to_string())
            .unwrap_or_default();
        *self.composer_owner.lock() == shown
    }

    fn composer_has_room(&self, conversation: ActiveConversation, composer_empty: bool) -> bool {
        composer_empty
            && self.chat_state().active_conversation() == conversation
            && self.composer_shows_active()
            && self.chat_state().get_pending_attachments().is_empty()
    }

    // ── Persistence ─────────────────────────────────────────────────

    fn composer_drafts_key(owner: crate::domain::UserId) -> String {
        format!("ui.composer_drafts.{}", owner.0)
    }

    fn settings_repo(&self) -> crate::storage::repositories::settings_repo::SettingsRepository {
        crate::storage::repositories::settings_repo::SettingsRepository::new(
            self.services.database.clone(),
        )
    }

    /// The draft map for the current session. A session change (sign-in,
    /// sign-out, re-login) discards whatever the map held and loads the
    /// signed-in account's persisted drafts, or nothing while signed out.
    /// Returns the guard and whether the session changed on this call.
    pub(super) fn composer_drafts_for_session(
        &self,
    ) -> (
        parking_lot::MutexGuard<'_, std::collections::HashMap<String, ComposerDraft>>,
        bool,
    ) {
        let mut drafts = self.composer_drafts.lock();
        let auth = self.auth_state().get();
        let mut draft_revision = self.composer_draft_revision.lock();
        let changed_session = *draft_revision != auth.session_revision;
        if changed_session {
            *draft_revision = auth.session_revision;
            *self.composer_draft_owner.lock() = auth.current_user_id;
            self.composer_drafts_dirty.store(false, Ordering::SeqCst);
            self.composer_recovery_dirty.store(false, Ordering::SeqCst);
            self.composer_drafts_unread.store(false, Ordering::SeqCst);
            *drafts = match auth.current_user_id {
                Some(owner) => match self.settings_repo().get(&Self::composer_drafts_key(owner)) {
                    Ok(row) => row.unwrap_or_default(),
                    Err(e) => {
                        self.composer_drafts_unread.store(true, Ordering::SeqCst);
                        tracing::warn!(error = %e, "draft row unread; existing row protected");
                        HashMap::new()
                    }
                },
                None => HashMap::new(),
            };
            // Nothing staged or parked crosses a session: the previous
            // account's trays would otherwise be shown to, and from here on
            // persisted under, the new one. Its parked sends can never be
            // restored anyway (`take_parked_send` checks the revision). The
            // new account's persisted ones take their place.
            self.chat_state().clear_attachment_trays();
            self.parked_sends.lock().clear();
            if let Some(owner) = auth.current_user_id {
                self.restore_composer_recovery(owner, auth.session_revision);
                self.restore_queued_composers(owner, auth.session_revision);
            }
        }
        (drafts, changed_session)
    }

    /// Write the map under the account it was loaded for. Nothing is written
    /// while signed out; the in-memory map still serves the session.
    pub(super) fn persist_composer_drafts(
        &self,
        drafts: &std::collections::HashMap<String, ComposerDraft>,
    ) {
        self.composer_drafts_dirty.store(true, Ordering::SeqCst);
        if self.composer_drafts_unread.load(Ordering::SeqCst) {
            return;
        }
        let auth = self.auth_state().get();
        if *self.composer_draft_revision.lock() != auth.session_revision {
            return;
        }
        let Some(owner) = (*self.composer_draft_owner.lock())
            .filter(|owner| Some(*owner) == auth.current_user_id)
        else {
            return;
        };
        if self
            .settings_repo()
            .set(&Self::composer_drafts_key(owner), drafts)
            .is_err()
        {
            tracing::warn!("composer draft not saved");
        } else {
            self.composer_drafts_dirty.store(false, Ordering::SeqCst);
        }
    }

    /// Live snapshot of the composer the UI shows, stored under the
    /// conversation that composer belongs to (its owner, which may lag core
    /// navigation) so a restart restores it. Nothing is stored when no
    /// conversation owns the composer, or after sign-out has moved the
    /// session on: a stale autosave from the old account's UI lands nowhere.
    pub fn autosave_composer_draft(&self, draft: ComposerDraft) {
        let owner = self.composer_owner.lock().clone();
        if owner.is_empty() {
            return;
        }
        let (mut drafts, changed_session) = self.composer_drafts_for_session();
        if changed_session {
            // The composer still shows the previous session's text.
            return;
        }
        let same = drafts
            .get(&owner)
            .map_or(draft.is_empty(), |held| *held == draft);
        if same {
            if self.composer_drafts_dirty.load(Ordering::SeqCst) {
                self.persist_composer_drafts(&drafts);
            }
            return;
        }
        if draft.is_empty() {
            drafts.remove(&owner);
        } else {
            drafts.insert(owner, draft);
        }
        self.persist_composer_drafts(&drafts);
    }

    /// Retry every dirty row at a lifecycle checkpoint, including parked replies.
    pub fn flush_composer_persistence(&self) -> bool {
        self.flush_composer_rows(true)
    }

    pub fn retry_dirty_composer_persistence(&self) -> bool {
        self.flush_composer_rows(false)
    }

    fn flush_composer_rows(&self, recovery_checkpoint: bool) -> bool {
        {
            let (drafts, _) = self.composer_drafts_for_session();
            if self.composer_drafts_dirty.load(Ordering::SeqCst) {
                self.persist_composer_drafts(&drafts);
            }
        }
        if recovery_checkpoint || self.composer_recovery_dirty.load(Ordering::SeqCst) {
            self.persist_composer_recovery();
        }
        self.composer_persistence_pending()
    }

    pub fn composer_persistence_pending(&self) -> bool {
        self.composer_drafts_dirty.load(Ordering::SeqCst)
            || self.composer_recovery_dirty.load(Ordering::SeqCst)
            || self.composer_drafts_unread.load(Ordering::SeqCst)
            || self.composer_recovery_unread.load(Ordering::SeqCst)
    }

    // ── Thread drafts ───────────────────────────────────────────────

    fn thread_draft_key(root: &MessageId) -> String {
        format!("thread:{root}")
    }

    /// The unsent text of the thread panel for `root`, kept in the drafts map
    /// under its own key so it survives a restart like the composer's.
    pub fn thread_draft(&self, root: &MessageId) -> String {
        let (drafts, _) = self.composer_drafts_for_session();
        drafts
            .get(&Self::thread_draft_key(root))
            .map(|draft| draft.text.clone())
            .unwrap_or_default()
    }

    pub fn thread_draft_reply(&self, root: &MessageId) -> Option<ComposerReply> {
        let (drafts, _) = self.composer_drafts_for_session();
        drafts.get(&Self::thread_draft_key(root)).and_then(|draft| draft.reply.clone())
    }

    pub fn set_thread_draft_reply(&self, root: &MessageId, reply: Option<ComposerReply>) {
        let (mut drafts, changed_session) = self.composer_drafts_for_session();
        if changed_session { return; }
        let key = Self::thread_draft_key(root);
        let draft = drafts.entry(key.clone()).or_default();
        if draft.reply == reply { return; }
        draft.reply = reply;
        if draft.is_empty() { drafts.remove(&key); }
        self.persist_composer_drafts(&drafts);
    }

    /// Store the thread panel's text for `root`; empty text clears it. A
    /// save after the session moved on is dropped, like an autosave.
    pub fn set_thread_draft(&self, root: &MessageId, text: &str) {
        let (mut drafts, changed_session) = self.composer_drafts_for_session();
        if changed_session {
            return;
        }
        let key = Self::thread_draft_key(root);
        let same = drafts.get(&key).map_or(text.is_empty(), |held| held.text == text);
        if same {
            if self.composer_drafts_dirty.load(Ordering::SeqCst) {
                self.persist_composer_drafts(&drafts);
            }
            return;
        }
        let draft = drafts.entry(key.clone()).or_default();
        draft.text = text.to_owned();
        if draft.is_empty() { drafts.remove(&key); }
        self.persist_composer_drafts(&drafts);
    }

    /// A failed reply parked beside the thread's draft, under its own key,
    /// so neither a newer draft nor a newer sent reply writes over it.
    fn failed_thread_reply_key(root: &MessageId) -> String {
        format!("thread-failed:{root}")
    }

    /// Give a thread reply that did not leave back to its thread. It becomes
    /// the thread's draft when that is empty; when text typed since is there,
    /// it is parked under [`Self::failed_thread_reply_key`] instead (a second
    /// failure joins the first), where [`Self::take_failed_thread_reply`]
    /// finds it. Nothing is written when the session that composed it is
    /// over. Newer input is never written over; nothing is ever resent on
    /// its own. Returns whether the text was kept.
    pub fn restore_thread_draft(
        &self,
        root: &MessageId,
        text: &str,
        session_revision: u64,
    ) -> bool {
        self.keep_failed_thread_reply(root, text, None, session_revision, false)
    }

    /// The UI's box may contain newer text that has not reached autosave.
    /// Park independently of the last persisted draft in that case.
    pub fn park_failed_thread_reply(
        &self,
        root: &MessageId,
        text: &str,
        session_revision: u64,
    ) -> bool {
        self.keep_failed_thread_reply(root, text, None, session_revision, true)
    }

    pub fn park_failed_thread_composer_reply(
        &self,
        root: &MessageId,
        text: &str,
        reply: Option<ComposerReply>,
        session_revision: u64,
    ) -> bool {
        self.keep_failed_thread_reply(root, text, reply, session_revision, true)
    }

    fn keep_failed_thread_reply(
        &self,
        root: &MessageId,
        text: &str,
        reply: Option<ComposerReply>,
        session_revision: u64,
        always_park: bool,
    ) -> bool {
        if text.is_empty() || !self.composer_session_is_current(session_revision) {
            return false;
        }
        let (mut drafts, changed_session) = self.composer_drafts_for_session();
        if changed_session || !self.composer_session_is_current(session_revision) {
            return false;
        }
        let key = Self::thread_draft_key(root);
        if always_park || drafts.get(&key).is_some_and(|held| !held.is_empty()) {
            let parked = drafts
                .entry(Self::failed_thread_reply_key(root))
                .or_default();
            if !parked.text.is_empty() {
                parked.text.push_str("\n\n");
                parked.reply = None;
            } else {
                parked.reply = reply;
            }
            parked.text.push_str(text);
        } else {
            drafts.insert(
                key,
                ComposerDraft {
                    text: text.to_owned(),
                    reply,
                },
            );
        }
        self.persist_composer_drafts(&drafts);
        true
    }

    /// Whether a failed reply is parked for `root` (the draft was busy when
    /// it failed). For the UI's feedback; it does not move anything.
    pub fn has_failed_thread_reply(&self, root: &MessageId) -> bool {
        let (drafts, _) = self.composer_drafts_for_session();
        drafts
            .get(&Self::failed_thread_reply_key(root))
            .is_some_and(|held| !held.text.is_empty())
    }

    /// Move the parked failed reply for `root` into the thread's draft, if
    /// that draft is empty (nothing typed since), and return its text for the
    /// box. It is the draft from here on, so a kill before the UI's autosave
    /// loses nothing. `None` when nothing is parked or the draft is busy.
    pub fn take_failed_thread_reply(&self, root: &MessageId) -> Option<String> {
        let (mut drafts, _) = self.composer_drafts_for_session();
        let key = Self::thread_draft_key(root);
        if drafts.get(&key).is_some_and(|held| !held.is_empty()) {
            return None;
        }
        let parked = drafts.remove(&Self::failed_thread_reply_key(root))?;
        if parked.text.is_empty() {
            self.persist_composer_drafts(&drafts);
            return None;
        }
        let text = parked.text.clone();
        drafts.insert(key, parked);
        self.persist_composer_drafts(&drafts);
        Some(text)
    }
}

mod recovery;

#[cfg(test)]
mod tests;

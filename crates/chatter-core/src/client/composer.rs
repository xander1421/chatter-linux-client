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
        self.trays.is_empty() && self.parked.is_empty()
    }
}

impl ChatterClient {
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
        self.ensure_send_available(conversation)
            .map_err(|e| e.to_string())?;
        if !self.composer_session_is_current(session_revision) {
            return Err("The account session changed. Nothing was sent.".into());
        }
        let attachments = self
            .chat_state()
            .take_pending_attachments_for(conversation, chat.attachment_revision)
            .ok_or_else(|| "The conversation changed. Nothing was sent.".to_string())?;
        Ok(ComposerSend {
            attempt_id: uuid::Uuid::new_v4(),
            session_revision,
            conversation,
            text,
            reply,
            attachments,
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
        if !self.composer_session_is_current(send.session_revision) {
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
        send: ComposerSend,
        composer_empty: bool,
    ) -> Option<ComposerSend> {
        if !self.composer_session_is_current(send.session_revision) {
            return None;
        }
        if self.composer_has_room(send.conversation, composer_empty) {
            return Some(send);
        }
        let (_drafts, _) = self.composer_drafts_for_session();
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

    /// Store the thread panel's text for `root`; empty text clears it. A
    /// save after the session moved on is dropped, like an autosave.
    pub fn set_thread_draft(&self, root: &MessageId, text: &str) {
        let (mut drafts, changed_session) = self.composer_drafts_for_session();
        if changed_session {
            return;
        }
        let key = Self::thread_draft_key(root);
        let same = drafts.get(&key).map_or(text.is_empty(), |held| {
            held.text == text && held.reply.is_none()
        });
        if same {
            if self.composer_drafts_dirty.load(Ordering::SeqCst) {
                self.persist_composer_drafts(&drafts);
            }
            return;
        }
        if text.is_empty() {
            drafts.remove(&key);
        } else {
            drafts.insert(
                key,
                ComposerDraft {
                    text: text.to_owned(),
                    reply: None,
                },
            );
        }
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
        self.keep_failed_thread_reply(root, text, session_revision, false)
    }

    /// The UI's box may contain newer text that has not reached autosave.
    /// Park independently of the last persisted draft in that case.
    pub fn park_failed_thread_reply(
        &self,
        root: &MessageId,
        text: &str,
        session_revision: u64,
    ) -> bool {
        self.keep_failed_thread_reply(root, text, session_revision, true)
    }

    fn keep_failed_thread_reply(
        &self,
        root: &MessageId,
        text: &str,
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
        if always_park || drafts.get(&key).is_some_and(|held| !held.text.is_empty()) {
            let parked = drafts
                .entry(Self::failed_thread_reply_key(root))
                .or_default();
            if !parked.text.is_empty() {
                parked.text.push_str("\n\n");
            }
            parked.text.push_str(text);
            parked.reply = None;
        } else {
            drafts.insert(
                key,
                ComposerDraft {
                    text: text.to_owned(),
                    reply: None,
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
        if drafts.get(&key).is_some_and(|held| !held.text.is_empty()) {
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

    // ── Recovery: staged files and parked sends ─────────────────────

    fn composer_recovery_key(owner: UserId) -> String {
        format!("ui.composer_recovery.{}", owner.0)
    }

    fn staging_root(&self) -> PathBuf {
        self.directories().data_dir().join("composer-staging")
    }

    fn staging_dir(&self, owner: UserId) -> PathBuf {
        self.staging_root().join(owner.0.to_string())
    }

    fn staging_files(&self, owner: UserId) -> Vec<PathBuf> {
        let Ok(dirs) = std::fs::read_dir(self.staging_dir(owner)) else {
            return Vec::new();
        };
        dirs.flatten()
            .flat_map(|dir| {
                std::fs::read_dir(dir.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|entry| entry.path())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn wipe_staging_dir(&self, owner: UserId) {
        for path in self.staging_files(owner) {
            if let Some(key) = self.staging_crypto_key(&path) {
                let _ = self.settings_repo().delete(&key);
            }
        }
        let _ = std::fs::remove_dir_all(self.staging_dir(owner));
    }

    /// A clean break from the former plaintext layout: only files whose
    /// encryption metadata is readable in SQLCipher may survive startup.
    pub(super) fn discard_legacy_staging_all(&self) {
        let Ok(accounts) = std::fs::read_dir(self.staging_root()) else { return };
        for account in accounts.flatten() {
            let Ok(dirs) = std::fs::read_dir(account.path()) else { continue };
            for dir in dirs.flatten() {
                let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
                for file in files.flatten() {
                    let path = file.path();
                    let Some(key) = self.staging_crypto_key(&path) else { continue };
                    if matches!(self.settings_repo().get::<StagingCrypto>(&key), Ok(None)) {
                        let _ = std::fs::remove_file(&path);
                        let _ = std::fs::remove_dir(dir.path());
                    }
                }
            }
        }
    }

    fn staging_crypto_key(&self, path: &std::path::Path) -> Option<String> {
        let relative = path.strip_prefix(self.staging_root()).ok()?;
        let mut parts = relative.components();
        let std::path::Component::Normal(owner) = parts.next()? else {
            return None;
        };
        let std::path::Component::Normal(token) = parts.next()? else {
            return None;
        };
        let std::path::Component::Normal(_) = parts.next()? else {
            return None;
        };
        if parts.next().is_some() {
            return None;
        }
        let owner = owner.to_str()?;
        let token = token.to_str()?;
        Some(format!("ui.composer_staging.{owner}.{token}"))
    }

    /// Return the real size for a staged ciphertext file, for the tray UI.
    pub fn staged_file_size(&self, path: &std::path::Path) -> Option<u64> {
        let key = self.staging_crypto_key(path)?;
        Some(
            self.settings_repo()
                .get::<StagingCrypto>(&key)
                .ok()??
                .plaintext_size,
        )
    }

    fn restorable_staged_file(&self, path: &std::path::Path) -> bool {
        let Some(key) = self.staging_crypto_key(path) else { return true };
        !matches!(self.settings_repo().get::<StagingCrypto>(&key), Ok(None))
    }

    /// Verify and decrypt a staged file for the duration of one upload.
    /// The existing attachment uploader and media sanitizer require a file path;
    /// its private temporary plaintext file is removed when the send returns.
    pub(crate) fn staged_upload_file(
        &self,
        path: &std::path::Path,
    ) -> Result<Option<(tempfile::TempDir, PathBuf)>, crate::services::attachment::AttachmentError>
    {
        use crate::services::attachment::AttachmentError;
        let Some(key_name) = self.staging_crypto_key(path) else {
            return Ok(None);
        };
        let crypto: StagingCrypto = self
            .settings_repo()
            .get(&key_name)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
            .ok_or_else(|| {
                AttachmentError::DecryptionError("staged file has no encryption key".into())
            })?;
        let key: [u8; 64] = crypto
            .key
            .try_into()
            .map_err(|_| AttachmentError::DecryptionError("invalid staged file key".into()))?;
        let digest: [u8; 32] = crypto
            .digest
            .try_into()
            .map_err(|_| AttachmentError::DecryptionError("invalid staged file digest".into()))?;
        let source =
            std::fs::File::open(path).map_err(|e| AttachmentError::FileError(e.to_string()))?;
        let size = source
            .metadata()
            .map_err(|e| AttachmentError::FileError(e.to_string()))?
            .len() as usize;
        let mut decryptor = crate::crypto::streaming_decrypt::StreamingDecryptor::new(
            std::io::BufReader::new(source),
            &key,
            &digest,
            crypto.plaintext_size as usize,
            size,
        );
        decryptor
            .verify()
            .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;
        decryptor
            .init_decryption()
            .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;
        let temp = tempfile::tempdir().map_err(|e| AttachmentError::FileError(e.to_string()))?;
        let output_path = temp.path().join(
            path.file_name()
                .ok_or_else(|| AttachmentError::FileError("staged filename missing".into()))?,
        );
        let mut output = std::fs::File::create(&output_path)
            .map_err(|e| AttachmentError::FileError(e.to_string()))?;
        let mut chunk = vec![0u8; 512 * 1024];
        loop {
            let read = decryptor
                .read_decrypted(&mut chunk)
                .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&chunk[..read])
                .map_err(|e| AttachmentError::FileError(e.to_string()))?;
        }
        drop(output);
        Ok(Some((temp, output_path)))
    }

    /// The account whose recovery row may be written: the drafts' owner,
    /// only while the session that loaded it is the current one. `None`
    /// between a sign-out and the next composer use, so nothing of the old
    /// account lands under the new one.
    fn recovery_owner(&self) -> Option<UserId> {
        let auth = self.auth_state().get();
        if *self.composer_draft_revision.lock() != auth.session_revision {
            return None;
        }
        if self
            .composer_recovery_unread
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            // The row could not be read this session: whatever it holds
            // must not be written over. See `restore_composer_recovery`.
            return None;
        }
        let owner = *self.composer_draft_owner.lock();
        owner.filter(|owner| Some(*owner) == auth.current_user_id)
    }

    /// Encrypt an app-owned picker or paste copy under the signed-in account.
    /// The random file key is stored in SQLCipher before ciphertext is published.
    /// A failed stage returns the original path; callers must reject that path.
    pub fn stage_owned_file(&self, path: PathBuf) -> PathBuf {
        let (_drafts, _) = self.composer_drafts_for_session();
        let Some(owner) = self.recovery_owner() else {
            return path;
        };
        let Some(name) = path.file_name() else {
            return path;
        };
        let dir = self
            .staging_dir(owner)
            .join(uuid::Uuid::new_v4().to_string());
        if std::fs::create_dir_all(&dir).is_err() {
            return path;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).is_err() {
                let _ = std::fs::remove_dir(&dir);
                return path;
            }
        }
        let dest = dir.join(name);
        let staging = match tempfile::NamedTempFile::new_in(&dir) {
            Ok(file) => file,
            Err(_) => {
                let _ = std::fs::remove_dir(&dir);
                return path;
            }
        };
        let encrypted =
            match crate::crypto::attachment_crypto::AttachmentCrypto::encrypt_file_to_file(
                &path,
                staging.path(),
            ) {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(%error, "could not encrypt staged attachment");
                    drop(staging);
                    let _ = std::fs::remove_dir(&dir);
                    return path;
                }
            };
        let Some(key_name) = self.staging_crypto_key(&dest) else {
            return path;
        };
        let crypto = StagingCrypto {
            key: encrypted.key.to_vec(),
            digest: encrypted.digest.to_vec(),
            plaintext_size: encrypted.plaintext_size,
        };
        if self.settings_repo().set(&key_name, &crypto).is_err() {
            drop(staging);
            let _ = std::fs::remove_dir(&dir);
            return path;
        }
        if staging.persist(&dest).is_err() {
            let _ = self.settings_repo().delete(&key_name);
            let _ = std::fs::remove_dir_all(&dir);
            return path;
        }
        let _ = std::fs::remove_file(&path);
        // The Android picker copies each selection into a directory of its
        // own (`MainActivity.copyToCache`); it is empty now.
        if let Some(parent) = path.parent() {
            if parent
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("selection_"))
            {
                let _ = std::fs::remove_dir(parent);
            }
        }
        dest
    }

    /// Delete the staged copies behind `attachments`, once their send was
    /// taken by core or they left the tray. Only files under the staging
    /// root go; a user's original referenced by path is never touched.
    pub fn release_staged_files(&self, attachments: &[PendingAttachment]) {
        let root = self.staging_root();
        for attachment in attachments {
            let path = &attachment.path;
            if path.as_os_str().is_empty() || !path.starts_with(&root) {
                continue;
            }
            let _ = std::fs::remove_file(path);
            if let Some(key) = self.staging_crypto_key(path) {
                let _ = self.settings_repo().delete(&key);
            }
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir);
            }
        }
    }

    /// A deleted conversation cannot leave recoverable staged copies behind.
    pub(crate) fn clear_composer_conversation(&self, conversation: ActiveConversation) {
        let (_drafts, _) = self.composer_drafts_for_session();
        drop(_drafts);
        let tray = self.chat_state().take_attachment_tray_for(conversation);
        self.release_staged_files(&tray);
        let mut parked = self.parked_sends.lock();
        parked.retain(|send| {
            if send.conversation == conversation {
                self.release_staged_files(&send.attachments);
                false
            } else {
                true
            }
        });
        drop(parked);
        let _ = self.persist_composer_recovery();
    }

    /// Write the trays and the parked sends under the signed-in account.
    /// The UI's chat-state watcher calls this on every tray change; the
    /// parked-send paths call it themselves.
    pub fn persist_composer_recovery(&self) -> bool {
        let (_drafts, _) = self.composer_drafts_for_session();
        if self.auth_state().get().current_user_id.is_none() {
            return true;
        }
        self.composer_recovery_dirty.store(true, Ordering::SeqCst);
        let Some(owner) = self.recovery_owner() else {
            return false;
        };
        let parked = self.parked_sends.lock();
        self.persist_recovery_with(owner, &parked)
    }

    fn composer_recovery_snapshot(&self, parked: &[ComposerSend]) -> ComposerRecovery {
        let chat = self.chat_state().get();
        let mut recovery = ComposerRecovery::default();
        let mut trays: Vec<(Option<ChannelId>, &[PendingAttachment])> =
            vec![(chat.active_channel_id, chat.pending_attachments.as_slice())];
        trays.extend(
            chat.attachment_trays
                .iter()
                .map(|(k, v)| (*k, v.as_slice())),
        );
        for (key, attachments) in trays {
            // A mesh scope keys its tray as `None`; it is no conversation
            // core can restore.
            let Some(id) = key else {
                continue;
            };
            if attachments.is_empty() {
                continue;
            }
            recovery.trays.insert(
                id.to_string(),
                attachments.iter().map(StagedAttachment::from).collect(),
            );
        }
        recovery.parked = parked.iter().filter_map(ParkedSend::of).collect();
        recovery
    }

    fn persist_recovery_with(&self, owner: UserId, parked: &[ComposerSend]) -> bool {
        self.composer_recovery_dirty.store(true, Ordering::SeqCst);
        if self.recovery_owner() != Some(owner) {
            return false;
        }
        let recovery = self.composer_recovery_snapshot(parked);
        let repo = self.settings_repo();
        let key = Self::composer_recovery_key(owner);
        let written = if recovery.is_empty() {
            repo.delete(&key).map(|_| ())
        } else {
            repo.set(&key, &recovery)
        };
        if written.is_err() {
            tracing::warn!("composer recovery state not saved");
            false
        } else {
            self.composer_recovery_dirty.store(false, Ordering::SeqCst);
            true
        }
    }

    /// Load `owner`'s row into the trays and the parked list, preserving
    /// files not named by a successfully acknowledged row. Runs once per
    /// session, under the drafts lock; nothing in here takes it again.
    ///
    /// Only a row that was read, or confirmed absent, is acted on. A read
    /// or decode failure is unknown state: nothing is restored, nothing is
    /// swept, and `recovery_owner` returns `None` for the rest of the
    /// session so no tray change writes over the row. The next session
    /// reads it again; sign-out deletes it as always.
    fn restore_composer_recovery(&self, owner: UserId, session_revision: u64) {
        let key = Self::composer_recovery_key(owner);
        let recovery = match self.settings_repo().get::<ComposerRecovery>(&key) {
            Ok(row) => row.unwrap_or_default(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "composer recovery row not read; row and staged files left as they are"
                );
                self.composer_recovery_unread
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        };
        self.composer_recovery_unread
            .store(false, std::sync::atomic::Ordering::SeqCst);

        let chat = self.chat_state();
        let revision = chat.get().attachment_revision;
        for (id, attachments) in recovery.trays {
            let Ok(channel) = id.parse::<ChannelId>() else {
                tracing::warn!(tray = %id, "unparsable tray key; recovery row and files protected");
                self.composer_recovery_unread.store(true, Ordering::SeqCst);
                continue;
            };
            for attachment in attachments
                .into_iter()
                .filter(|attachment| self.restorable_staged_file(&attachment.path))
                .map(StagedAttachment::into_pending)
            {
                // Trays are keyed by channel id alone, so the variant does
                // not matter here: a DM's tray has the DM's derived id.
                chat.add_pending_attachment_for(
                    ActiveConversation::Channel(channel),
                    revision,
                    attachment,
                );
            }
        }
        let mut parked = self.parked_sends.lock();
        for send in recovery.parked {
            let attachments: Vec<PendingAttachment> = send
                .attachments
                .into_iter()
                .filter(|attachment| self.restorable_staged_file(&attachment.path))
                .map(StagedAttachment::into_pending)
                .collect();
            if send.text.is_empty() && send.reply.is_none() && attachments.is_empty() {
                continue;
            }
            parked.push(ComposerSend {
                attempt_id: uuid::Uuid::new_v4(),
                session_revision,
                conversation: send.conversation.into_active(),
                text: send.text,
                reply: send.reply,
                attachments,
            });
        }
        drop(parked);
        // Unreferenced files can be unacknowledged staging writes. Keep them
        // until explicit removal/send/sign-out rather than guessing ownership.
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::domain::ids::{AttachmentId, ChannelId};

    fn client() -> (Arc<ChatterClient>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        // Persistent opt-out beside the database, before anything opens it.
        let db = crate::system::Directories::with_base_dir(dir.path().to_path_buf())
            .unwrap()
            .database_path();
        std::fs::write(
            db.parent()
                .unwrap()
                .join(crate::system::keyring::NO_KEYRING_MARKER),
            b"",
        )
        .unwrap();
        let client = client_in(&dir);
        client
            .auth_state()
            .set_logged_in(crate::domain::ids::UserId::new(), 1, "Test user".into());
        (client, dir)
    }

    /// A fresh process over the same data directory.
    fn client_in(dir: &tempfile::TempDir) -> Arc<ChatterClient> {
        ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client")
    }

    fn staged(name: &str) -> PendingAttachment {
        PendingAttachment::new(
            AttachmentId::new(),
            name.into(),
            Default::default(),
            1,
            "image/png".into(),
        )
    }

    /// Navigate as the app does: core state, then the UI's draft swap.
    fn open(client: &ChatterClient, id: ChannelId) {
        client
            .chat_state()
            .set_active_conversation(ActiveConversation::Channel(id));
        client.swap_composer_draft("", ComposerDraft::default(), &id.to_string());
    }

    #[tokio::test]
    async fn begin_freezes_the_composer_and_takes_what_is_staged() {
        let (client, _dir) = client();
        let a = ChannelId::new();
        open(&client, a);
        client
            .chat_state()
            .add_pending_attachment(staged("cat.png"));
        let send = client.begin_composer_send("caption".into(), None).unwrap();
        assert_eq!(send.conversation, ActiveConversation::Channel(a));
        assert_eq!(send.text, "caption");
        assert_eq!(send.attachments.len(), 1);
        assert!(
            client.chat_state().get_pending_attachments().is_empty(),
            "the staged file went to two sends"
        );

        // Later navigation and staging do not reach into the frozen send.
        open(&client, ChannelId::new());
        client
            .chat_state()
            .add_pending_attachment(staged("later.png"));
        assert_eq!(send.conversation, ActiveConversation::Channel(a));
        assert_eq!(send.attachments[0].filename, "cat.png");
    }

    #[tokio::test]
    async fn an_expired_session_cannot_send_restore_or_recover_its_caption() {
        let (client, _dir) = client();
        let user = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(user, 1, "A".into());
        let channel = ChannelId::new();
        open(&client, channel);
        let send = client
            .begin_composer_send("private caption".into(), None)
            .unwrap();
        assert!(client.recover_composer_send(send.clone(), false).is_none());

        // Sign-out as `logout` does it: the session's composer state goes
        // first. Even another login of the same account is a new session.
        client.clear_composer_session();
        client.auth_state().set_logged_out();
        client.auth_state().set_logged_in(user, 1, "A again".into());
        assert!(!client.composer_session_is_current(send.session_revision));
        assert!(
            client.send_composer_message(&send, None).await.is_none(),
            "expired intent entered the send path"
        );
        assert!(client.recover_composer_send(send, true).is_none());
        assert!(
            client.take_parked_send(true).is_none(),
            "old caption restored in a new session"
        );
    }

    #[tokio::test]
    async fn sign_out_clears_account_local_composer_state() {
        let (client, _dir) = client();
        let channel = ChannelId::new();
        open(&client, channel);
        let send = client
            .begin_composer_send("failed caption".into(), None)
            .unwrap();
        assert!(client.recover_composer_send(send.clone(), false).is_none());
        client
            .chat_state()
            .add_pending_attachment(staged("private.png"));
        client.swap_composer_draft(
            &channel.to_string(),
            ComposerDraft {
                text: "private draft".into(),
                reply: None,
            },
            "elsewhere",
        );

        client.clear_composer_session();
        assert_eq!(
            client.chat_state().active_conversation(),
            ActiveConversation::None
        );
        assert!(client.chat_state().get_pending_attachments().is_empty());
        assert!(!client.composer_session_is_current(send.session_revision));
        client.swap_composer_draft(
            &channel.to_string(),
            ComposerDraft {
                text: "late old UI draft".into(),
                reply: None,
            },
            "",
        );
        client
            .auth_state()
            .set_logged_in(crate::domain::ids::UserId::new(), 1, "B".into());
        open(&client, channel);
        assert!(
            client
                .swap_composer_draft("", ComposerDraft::default(), &channel.to_string())
                .is_empty()
        );
        assert!(client.take_parked_send(true).is_none());
    }

    #[tokio::test]
    async fn begin_with_no_conversation_consumes_nothing() {
        let (client, _dir) = client();
        client
            .chat_state()
            .add_pending_attachment(staged("cat.png"));
        assert!(client.begin_composer_send("x".into(), None).is_err());
        assert_eq!(client.chat_state().get_pending_attachments().len(), 1);
    }

    #[cfg(feature = "jarvis")]
    #[tokio::test]
    async fn begin_refuses_a_jarvis_request_before_consuming() {
        let (client, _dir) = client();
        open(&client, ChannelId::new());
        client
            .chat_state()
            .add_pending_attachment(staged("cat.png"));
        assert!(client.begin_composer_send("/ask why".into(), None).is_err());
        assert_eq!(client.chat_state().get_pending_attachments().len(), 1);
    }

    #[tokio::test]
    async fn a_failure_is_restored_only_into_its_own_empty_composer() {
        let (client, _dir) = client();
        let (a, b) = (ChannelId::new(), ChannelId::new());
        open(&client, a);
        let send = client.begin_composer_send("caption".into(), None).unwrap();

        // Same chat, newer draft typed: kept, not written over it.
        assert!(client.recover_composer_send(send.clone(), false).is_none());
        assert!(
            client.take_parked_send(false).is_none(),
            "newer draft still there"
        );

        // Navigated to B: B's empty composer is no home for A's send.
        open(&client, b);
        assert!(
            client.take_parked_send(true).is_none(),
            "A's send restored into B"
        );

        // Back in A, composer empty but a file staged since: still kept.
        open(&client, a);
        client
            .chat_state()
            .add_pending_attachment(staged("new.png"));
        assert!(client.take_parked_send(true).is_none());
        client.chat_state().clear_pending_attachments();

        let back = client
            .take_parked_send(true)
            .expect("A's composer has room now");
        assert_eq!(back.text, "caption");
        assert!(client.take_parked_send(true).is_none(), "restored twice");

        // Failing while its own composer is empty restores at once.
        assert!(client.recover_composer_send(back, true).is_some());
    }

    #[tokio::test]
    async fn a_failure_while_elsewhere_waits_for_its_conversation() {
        let (client, _dir) = client();
        let (a, b) = (ChannelId::new(), ChannelId::new());
        open(&client, a);
        let send = client.begin_composer_send("for A".into(), None).unwrap();
        open(&client, b);
        assert!(
            client.recover_composer_send(send, true).is_none(),
            "restored into B"
        );
        open(&client, a);
        assert_eq!(client.take_parked_send(true).unwrap().text, "for A");
    }

    #[tokio::test]
    async fn staged_files_are_taken_only_for_the_conversation_still_open() {
        let (client, _dir) = client();
        let (a, b) = (ChannelId::new(), ChannelId::new());
        open(&client, b);
        client.chat_state().add_pending_attachment(staged("b.png"));
        assert!(
            client
                .chat_state()
                .take_pending_attachments_for(
                    ActiveConversation::Channel(a),
                    client.chat_state().get().attachment_revision,
                )
                .is_none()
        );
        assert_eq!(
            client.chat_state().get_pending_attachments().len(),
            1,
            "B's file was taken for A"
        );
    }

    #[tokio::test]
    async fn begin_mid_switch_refuses_before_consuming() {
        let (client, _dir) = client();
        let (a, b) = (ChannelId::new(), ChannelId::new());
        open(&client, a);
        // Core has moved to B; the composer still holds A's draft.
        client
            .chat_state()
            .set_active_conversation(ActiveConversation::Channel(b));
        client.chat_state().add_pending_attachment(staged("a.png"));
        assert!(client.begin_composer_send("for A".into(), None).is_err());
        assert_eq!(client.chat_state().get_pending_attachments().len(), 1);
    }

    #[tokio::test]
    async fn a_failure_mid_switch_is_kept_not_restored_into_the_old_composer() {
        let (client, _dir) = client();
        let (a, b) = (ChannelId::new(), ChannelId::new());
        open(&client, a);
        let send = client.begin_composer_send("for A".into(), None).unwrap();
        open(&client, b);
        // Back to A in core; the composer still shows B's (empty) draft.
        client
            .chat_state()
            .set_active_conversation(ActiveConversation::Channel(a));
        assert!(
            client.recover_composer_send(send, true).is_none(),
            "A's send restored into B's composer"
        );
        client.swap_composer_draft(&b.to_string(), ComposerDraft::default(), &a.to_string());
        assert_eq!(client.take_parked_send(true).unwrap().text, "for A");
    }

    #[tokio::test]
    async fn drafts_survive_a_restart_for_their_account_only_until_sign_out() {
        let (client, dir) = client();
        let user = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(user, 1, "A".into());
        let (a, b) = (ChannelId::new(), ChannelId::new());
        open(&client, a);
        client.autosave_composer_draft(ComposerDraft {
            text: "half a thought".into(),
            reply: Some(ComposerReply {
                sender_id: "peer".into(),
                timestamp: "1".into(),
                sender_name: "Peer".into(),
                preview: "quoted".into(),
                thumbnail: Some((vec![0; 4], 1, 1)),
            }),
        });
        // Switching away parks it; the parked copy is what the next process sees.
        client.swap_composer_draft(
            &a.to_string(),
            ComposerDraft {
                text: "half a thought".into(),
                reply: None,
            },
            &b.to_string(),
        );
        client.autosave_composer_draft(ComposerDraft {
            text: "for b".into(),
            reply: None,
        });
        drop(client);

        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        let restored = client.swap_composer_draft("", ComposerDraft::default(), &b.to_string());
        assert_eq!(restored.text, "for b", "live text lost over restart");
        let restored = client.swap_composer_draft(&b.to_string(), restored, &a.to_string());
        assert_eq!(restored.text, "half a thought");
        assert!(restored.reply.is_none(), "parked copy had no reply");

        // Another account on the same installation sees none of it.
        client.auth_state().set_logged_out();
        client
            .auth_state()
            .set_logged_in(crate::domain::ids::UserId::new(), 1, "B".into());
        assert!(
            client
                .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
                .is_empty()
        );

        // Back as A, the drafts are still there; explicit sign-out removes them.
        client.auth_state().set_logged_out();
        client.auth_state().set_logged_in(user, 1, "A".into());
        assert_eq!(
            client
                .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
                .text,
            "half a thought"
        );
        client.clear_composer_session();
        drop(client);

        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        assert!(
            client
                .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
                .is_empty(),
            "drafts survived sign-out"
        );
    }

    #[tokio::test]
    async fn a_reply_target_survives_a_restart_without_its_thumbnail() {
        let (client, dir) = client();
        let user = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(user, 1, "A".into());
        let a = ChannelId::new();
        open(&client, a);
        client.autosave_composer_draft(ComposerDraft {
            text: String::new(),
            reply: Some(ComposerReply {
                sender_id: "peer".into(),
                timestamp: "1".into(),
                sender_name: "Peer".into(),
                preview: "quoted".into(),
                thumbnail: Some((vec![0; 4], 1, 1)),
            }),
        });
        drop(client);
        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        let reply = client
            .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
            .reply
            .expect("reply target lost over restart");
        assert_eq!(reply.preview, "quoted");
        assert!(reply.thumbnail.is_none());
    }

    #[tokio::test]
    async fn an_autosave_after_sign_out_lands_nowhere() {
        let (client, _dir) = client();
        let a = ChannelId::new();
        open(&client, a);
        client.clear_composer_session();
        // The old UI's debounced autosave fires after core moved on.
        client.autosave_composer_draft(ComposerDraft {
            text: "stale".into(),
            reply: None,
        });
        client
            .auth_state()
            .set_logged_in(crate::domain::ids::UserId::new(), 1, "B".into());
        assert!(
            client
                .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
                .is_empty()
        );
    }

    #[tokio::test]
    async fn sign_out_before_any_conversation_opens_still_deletes_the_row() {
        let (client, dir) = client();
        let user = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(user, 1, "A".into());
        let a = ChannelId::new();
        open(&client, a);
        client.autosave_composer_draft(ComposerDraft {
            text: "kept".into(),
            reply: None,
        });
        drop(client);

        // Next launch: signed in, drafts never loaded, sign out from the list.
        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        client.clear_composer_session();
        drop(client);

        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        assert!(
            client
                .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
                .is_empty(),
            "row survived a sign-out that never loaded it"
        );
    }

    #[tokio::test]
    async fn signing_out_another_account_keeps_the_last_loaded_owners_row() {
        let (client, dir) = client();
        let a_user = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(a_user, 1, "A".into());
        let a = ChannelId::new();
        open(&client, a);
        client.autosave_composer_draft(ComposerDraft {
            text: "a's thought".into(),
            reply: None,
        });

        // B signs in and out again without ever touching the composer, so the
        // map is still the one loaded for A. Only B's row may go.
        client.auth_state().set_logged_out();
        client
            .auth_state()
            .set_logged_in(crate::domain::ids::UserId::new(), 1, "B".into());
        client.clear_composer_session();
        drop(client);

        let client = client_in(&dir);
        client.auth_state().set_logged_in(a_user, 1, "A".into());
        assert_eq!(
            client
                .swap_composer_draft("", ComposerDraft::default(), &a.to_string())
                .text,
            "a's thought",
            "another account's sign-out deleted A's drafts"
        );
    }

    /// A file the app produced, as the Android picker leaves it: in a
    /// one-selection directory under the cache dir.
    fn picked(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
        let selection = dir.path().join("cache").join("picked").join("selection_1");
        std::fs::create_dir_all(&selection).unwrap();
        let path = selection.join(name);
        std::fs::write(&path, b"bytes").unwrap();
        path
    }

    fn stage(client: &ChatterClient, path: std::path::PathBuf) -> PendingAttachment {
        let path = client.stage_owned_file(path);
        let chat = client.chat_state().get();
        let pending = PendingAttachment::new(
            AttachmentId::new(),
            path.file_name().unwrap().to_str().unwrap().into(),
            path,
            5,
            "image/png".into(),
        );
        client.chat_state().add_pending_attachment_for(
            chat.active_conversation,
            chat.attachment_revision,
            pending.clone(),
        );
        // The UI's state watcher does this on every tray change.
        client.persist_composer_recovery();
        pending
    }

    #[tokio::test]
    async fn an_owned_file_is_sheltered_under_the_account_and_a_users_file_is_not() {
        let (client, dir) = client();
        let a = ChannelId::new();
        open(&client, a);
        let cache_copy = picked(&dir, "cat.png");
        let staged = client.stage_owned_file(cache_copy.clone());
        assert_ne!(staged, cache_copy);
        assert!(staged.starts_with(client.staging_root()));
        assert!(staged.is_file() && !cache_copy.exists());
        assert!(
            !cache_copy.parent().unwrap().exists(),
            "the picker's selection directory was left behind"
        );

        // Release touches only what lives under the staging root.
        let original = dir.path().join("my-photo.png");
        std::fs::write(&original, b"mine").unwrap();
        let mine = PendingAttachment::new(
            AttachmentId::new(),
            "my-photo.png".into(),
            original.clone(),
            4,
            "image/png".into(),
        );
        let ours = PendingAttachment::new(
            AttachmentId::new(),
            "cat.png".into(),
            staged.clone(),
            5,
            "image/png".into(),
        );
        client.release_staged_files(&[mine, ours]);
        assert!(original.is_file(), "the user's original was deleted");
        assert!(!staged.exists() && !staged.parent().unwrap().exists());
    }

    #[tokio::test]
    async fn staged_files_survive_a_restart_in_their_own_conversation_only() {
        let (client, dir) = client();
        let user = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(user, 1, "A".into());
        let (a, b) = (ChannelId::new(), ChannelId::new());
        open(&client, a);
        let kept = stage(&client, picked(&dir, "for-a.png"));
        open(&client, b);
        // A GIF: pre-uploaded, no file behind it.
        client
            .chat_state()
            .add_pending_attachment(PendingAttachment::with_pointer(
                AttachmentId::new(),
                Default::default(),
                "fun.gif".into(),
                9,
                "image/gif".into(),
                crate::proto::content_v1::AttachmentPointer {
                    cdn_key: "k".into(),
                    ..Default::default()
                },
            ));
        client.persist_composer_recovery();
        drop(client);

        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        open(&client, b);
        let tray = client.chat_state().get_pending_attachments();
        assert_eq!(tray.len(), 1, "B's GIF lost over restart");
        assert_eq!(tray[0].pointer.as_ref().unwrap().cdn_key, "k");
        open(&client, a);
        let tray = client.chat_state().get_pending_attachments();
        assert_eq!(tray.len(), 1, "A's file lost over restart");
        assert_eq!(tray[0].path, kept.path);
        assert!(kept.path.is_file());

        // Another account sees none of it, and its sign-out leaves A's files.
        client.auth_state().set_logged_out();
        client
            .auth_state()
            .set_logged_in(crate::domain::ids::UserId::new(), 1, "B".into());
        open(&client, a);
        assert!(client.chat_state().get_pending_attachments().is_empty());
        client.clear_composer_session();
        assert!(kept.path.is_file(), "B's sign-out deleted A's staged file");
    }

    #[tokio::test]
    async fn a_file_deleted_from_disk_stays_as_evidence_and_unacknowledged_files_are_kept() {
        let (client, dir) = client();
        let user = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(user, 1, "A".into());
        let a = ChannelId::new();
        open(&client, a);
        let gone = stage(&client, picked(&dir, "gone.png"));
        std::fs::remove_file(&gone.path).unwrap();
        // Staged, then killed before the tray was written.
        let orphan = client.stage_owned_file(picked(&dir, "orphan.png"));
        drop(client);

        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        open(&client, a);
        // The chip comes back so the user sees what was staged; its send
        // fails whole at upload until they remove it.
        let tray = client.chat_state().get_pending_attachments();
        assert_eq!(tray.len(), 1);
        assert_eq!(tray[0].path, gone.path);
        // Not named by the row, but it may be a staging write whose row
        // update never landed: kept until removal, send or sign-out.
        assert!(orphan.exists(), "unacknowledged staged file was deleted");
    }

    #[tokio::test]
    async fn a_parked_send_with_a_missing_file_keeps_the_file_in_the_restored_send() {
        let (client, dir) = client();
        let user = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(user, 1, "A".into());
        let (a, b) = (ChannelId::new(), ChannelId::new());
        open(&client, a);
        let kept = stage(&client, picked(&dir, "kept.png"));
        let lost = stage(&client, picked(&dir, "lost.png"));
        let send = client.begin_composer_send("both".into(), None).unwrap();
        open(&client, b);
        assert!(client.recover_composer_send(send, true).is_none());
        std::fs::remove_file(&lost.path).unwrap();
        drop(client);

        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        open(&client, a);
        let back = client.take_parked_send(true).expect("parked send lost");
        assert_eq!(back.text, "both");
        let paths: Vec<_> = back.attachments.iter().map(|a| a.path.clone()).collect();
        assert_eq!(
            paths,
            vec![kept.path.clone(), lost.path.clone()],
            "partial send prepared"
        );
        assert!(kept.path.is_file());
    }

    #[tokio::test]
    async fn an_unreadable_recovery_row_is_neither_swept_nor_written_over() {
        let (client, dir) = client();
        let user = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(user, 1, "A".into());
        let a = ChannelId::new();
        open(&client, a);
        let file = stage(&client, picked(&dir, "safe.png"));
        // Whatever wrote this row, this build cannot decode it.
        let key = ChatterClient::composer_recovery_key(user);
        client
            .settings_repo()
            .set(&key, &"not a recovery row")
            .unwrap();
        drop(client);

        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        open(&client, a);
        assert!(client.chat_state().get_pending_attachments().is_empty());
        assert!(file.path.is_file(), "file of an unread row was swept");
        // A tray change this session must not replace the unread row.
        stage(&client, picked(&dir, "new.png"));
        let row: Option<String> = client.settings_repo().get(&key).unwrap();
        assert_eq!(row.as_deref(), Some("not a recovery row"));
        // Sign-out is the one explicit cleanup and still clears it.
        client.clear_composer_session();
        let row: Option<String> = client.settings_repo().get(&key).unwrap();
        assert!(row.is_none());
    }

    #[tokio::test]
    async fn a_parked_failed_send_survives_a_restart_for_its_conversation() {
        let (client, dir) = client();
        let user = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(user, 1, "A".into());
        let (a, b) = (ChannelId::new(), ChannelId::new());
        open(&client, a);
        let file = stage(&client, picked(&dir, "late.png"));
        let send = client
            .begin_composer_send(
                "caption".into(),
                Some(ComposerReply {
                    sender_id: "peer".into(),
                    timestamp: "1".into(),
                    sender_name: "Peer".into(),
                    preview: "quoted".into(),
                    thumbnail: None,
                }),
            )
            .unwrap();
        // Failed while B is open: parked, and the parked copy is persisted.
        open(&client, b);
        assert!(client.recover_composer_send(send, true).is_none());
        drop(client);

        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        open(&client, b);
        assert!(
            client.take_parked_send(true).is_none(),
            "A's send restored into B"
        );
        open(&client, a);
        let back = client
            .take_parked_send(true)
            .expect("parked send lost over restart");
        assert_eq!(back.text, "caption");
        assert_eq!(back.reply.unwrap().preview, "quoted");
        assert_eq!(back.attachments.len(), 1);
        assert_eq!(back.attachments[0].path, file.path);
        assert!(file.path.is_file(), "the parked send's file was swept");
        assert!(client.take_parked_send(true).is_none(), "restored twice");

        // Taken: the row no longer holds it.
        drop(client);
        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        open(&client, a);
        assert!(
            client.take_parked_send(true).is_none(),
            "taken send came back"
        );
    }

    #[tokio::test]
    async fn sign_out_removes_the_recovery_row_and_the_staged_files() {
        let (client, dir) = client();
        let user = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(user, 1, "A".into());
        let a = ChannelId::new();
        open(&client, a);
        let file = stage(&client, picked(&dir, "private.png"));
        client.clear_composer_session();
        assert!(!file.path.exists(), "staged file survived sign-out");
        drop(client);

        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        open(&client, a);
        assert!(client.chat_state().get_pending_attachments().is_empty());
    }

    #[tokio::test]
    async fn a_thread_draft_survives_a_restart_and_clears_when_sent() {
        let (client, dir) = client();
        let user = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(user, 1, "A".into());
        let a = ChannelId::new();
        open(&client, a);
        let root = crate::domain::ids::MessageId::new();
        client.set_thread_draft(&root, "half a reply");
        drop(client);

        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        assert_eq!(client.thread_draft(&root), "half a reply");
        assert!(
            client
                .thread_draft(&crate::domain::ids::MessageId::new())
                .is_empty()
        );
        client.set_thread_draft(&root, "");
        drop(client);

        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        assert!(
            client.thread_draft(&root).is_empty(),
            "sent reply came back"
        );

        // A reply that failed after the tap goes back to its thread, unless
        // something newer was typed there or the session is over.
        let revision = client.auth_state().get().session_revision;
        assert!(client.restore_thread_draft(&root, "failed reply", revision));
        assert_eq!(client.thread_draft(&root), "failed reply");
        client.set_thread_draft(&root, "typed since");
        // Newer text is busy: the failed reply is parked beside it, and a
        // second failure joins the first. Neither touches the draft.
        assert!(client.restore_thread_draft(&root, "failed reply", revision));
        assert!(client.restore_thread_draft(&root, "and again", revision));
        assert_eq!(client.thread_draft(&root), "typed since");
        assert!(client.has_failed_thread_reply(&root));
        assert!(
            client.take_failed_thread_reply(&root).is_none(),
            "took over typed text"
        );
        // The newer reply goes out: its draft clears, the parked one survives
        // a restart and comes back only into the empty draft.
        client.set_thread_draft(&root, "");
        drop(client);
        let client = client_in(&dir);
        client.auth_state().set_logged_in(user, 1, "A".into());
        assert_eq!(
            client.take_failed_thread_reply(&root).as_deref(),
            Some("failed reply\n\nand again")
        );
        assert_eq!(client.thread_draft(&root), "failed reply\n\nand again");
        assert!(
            client.take_failed_thread_reply(&root).is_none(),
            "restored twice"
        );
        client.set_thread_draft(&root, "");
        assert!(!client.restore_thread_draft(&root, "failed reply", revision + 1));
        assert!(client.thread_draft(&root).is_empty());
        assert!(!client.has_failed_thread_reply(&root));
    }
}

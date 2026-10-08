//! A send frozen at the moment it was asked for.
//!
//! Send reads the composer once and clears it. A send that only completes
//! later — a GIF, which is downloaded and uploaded first — must deliver what
//! the composer held THEN, to the conversation it was asked for in, never
//! whatever the composer and the active conversation hold when the upload
//! finishes.

use super::ChatterClient;
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
    /// included. The row deleted is the signed-in account's, whether or not
    /// its drafts were ever loaded this session (sign-out straight from the
    /// list, before any conversation was opened), or the one the map was
    /// loaded for when the caller already signed the account out
    /// (`delete_account`). Only the explicit sign-out paths call this;
    /// `shutdown` invalidates the session without touching the row.
    pub fn clear_composer_session(&self) {
        let signed_in = self.auth_state().get().current_user_id;
        self.auth_state().invalidate_session();
        self.parked_sends.lock().clear();
        let mut drafts = self.composer_drafts.lock();
        drafts.clear();
        let loaded = self.composer_draft_owner.lock().take();
        let mut owners: Vec<_> = signed_in.into_iter().chain(loaded).collect();
        owners.dedup();
        for owner in owners {
            if self
                .settings_repo()
                .delete(&Self::composer_drafts_key(owner))
                .is_err()
            {
                tracing::warn!("persisted composer drafts not deleted at sign-out");
            }
        }
        drop(drafts);
        self.composer_owner.lock().clear();
        self.chat_state().clear_attachment_trays();
        self.chat_state()
            .set_active_conversation(ActiveConversation::None);
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
        self.parked_sends.lock().push(send);
        None
    }

    /// A kept failed send for the active conversation, if the composer has
    /// room for it now. Checked on entering a conversation.
    pub fn take_parked_send(&self, composer_empty: bool) -> Option<ComposerSend> {
        let active = self.chat_state().active_conversation();
        if !self.composer_has_room(active, composer_empty) {
            return None;
        }
        let mut parked = self.parked_sends.lock();
        let revision = self.auth_state().get().session_revision;
        parked.retain(|send| send.session_revision == revision);
        let at = parked.iter().position(|s| s.conversation == active)?;
        Some(parked.remove(at))
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
            *drafts = auth
                .current_user_id
                .and_then(|owner| {
                    self.settings_repo()
                        .get(&Self::composer_drafts_key(owner))
                        .ok()
                        .flatten()
                })
                .unwrap_or_default();
        }
        (drafts, changed_session)
    }

    /// Write the map under the account it was loaded for. Nothing is written
    /// while signed out; the in-memory map still serves the session.
    pub(super) fn persist_composer_drafts(
        &self,
        drafts: &std::collections::HashMap<String, ComposerDraft>,
    ) {
        let Some(owner) = *self.composer_draft_owner.lock() else {
            return;
        };
        if self
            .settings_repo()
            .set(&Self::composer_drafts_key(owner), drafts)
            .is_err()
        {
            tracing::warn!("composer draft not saved");
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
            return;
        }
        if draft.is_empty() {
            drafts.remove(&owner);
        } else {
            drafts.insert(owner, draft);
        }
        self.persist_composer_drafts(&drafts);
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

        client.auth_state().set_logged_out();
        // Even another login of the same account is a new session.
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
}

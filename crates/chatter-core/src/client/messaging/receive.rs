//! A message arriving live: its row and everything the view does about it.
//!
//! Two steps because the UI decodes the row's images between them, and the
//! "is it on screen" answer must be read after that await, not before: the
//! user may switch conversations while the row decodes.

use std::sync::Arc;

use super::ChatterClient;
use crate::domain::attachment::Attachment;
use crate::domain::ids::MessageId;
use crate::domain::message::{CallMessageType, ConversationType, Message, MessageContent};
use crate::state::ActiveConversation;
use crate::ui::message_row::{MessageRow, mark_disappearing};

/// What the view does with one live message, decided by
/// [`ChatterClient::incoming_message`].
#[derive(Clone, Debug)]
pub struct IncomingUpdate {
    /// The message's conversation is on screen: insert the row (or route it
    /// to the thread panel). The conversation was marked read.
    pub is_active: bool,
    /// An existing row with the same key is replaced, not skipped: Saved
    /// Messages snapshots and ended calls rewrite their row.
    pub replaces_existing: bool,
    /// The inserted row shows "delivered" (every kind but anonymous ones,
    /// which have their own delivery logic).
    pub marks_delivered: bool,
    /// Channel rows group consecutive messages; re-check continuation
    /// around the insert.
    pub groups_continuations: bool,
    /// A thread reply: it belongs to this root's thread, not the main list.
    pub thread_root: Option<MessageId>,
    /// The thread panel shows [`Self::thread_root`].
    pub panel_shows_thread: bool,
    /// The user the row's `is_own` is judged against.
    pub current_user_id: String,
    /// The sidebar row this message belongs to.
    pub sidebar_conversation: ActiveConversation,
    /// Sidebar preview, from the conversation's newest message.
    pub preview: String,
    /// Sidebar time, from the conversation's newest message.
    pub preview_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Absolute badges, read after the mark-read above.
    pub badges: crate::client::UnreadBadges,
    pub mention_count: u32,
    /// Attachments to fetch now (their media sidecars; small images in full).
    pub downloads: Vec<Attachment>,
    /// The downloads are our own sent media (a server DM we sent from
    /// another device).
    pub own_sent: bool,
}

impl ChatterClient {
    /// The row for a live `message`, built by the same converter the
    /// conversation's reload uses, so a live row and a reloaded row cannot
    /// differ. Returns the message the row was built from with it.
    ///
    /// `None` when there is nothing to show: a timer-change notice, or a
    /// Saved Messages snapshot / ended call whose stored copy is gone or no
    /// longer matches (those render from storage, not the event). Unarchives
    /// the chat as a side effect.
    pub fn incoming_message_row(
        self: &Arc<Self>,
        message: Message,
    ) -> Option<(Message, MessageRow)> {
        let message = if replaces_existing(&message) {
            self.get_message_local(&message.id).filter(|stored| {
                stored.conversation_type == message.conversation_type
                    && stored.conversation_id == message.conversation_id
                    && stored.sender_id == message.sender_id
                    && stored.sent_at == message.sent_at
                    && !stored.is_deleted
            })?
        } else {
            message
        };
        if crate::domain::disappearing::is_timer_notice(&message) {
            return None;
        }
        if let Err(error) = self.unarchive_on_incoming_message(&message) {
            tracing::warn!(%error, "Failed to update archived chat after message");
        }
        // Every kind gets its disappearing mark live (reload skips it only
        // when opening a private group); a fresh message has no thread
        // replies to count.
        let mut rows = self.message_rows(std::slice::from_ref(&message));
        mark_disappearing(self, &mut rows);
        let row = rows.pop()?;
        Some((message, row))
    }

    /// Decide what the view does with a live `message` whose row
    /// ([`Self::incoming_message_row`]) is ready. Marks the conversation read
    /// when it is on screen and ends the sender's typing.
    ///
    /// Sends no read receipts: those go out when a conversation is opened
    /// ([`Self::open_conversation`]).
    pub fn incoming_message(self: &Arc<Self>, message: &Message) -> IncomingUpdate {
        let conversation_id = message.conversation_id;
        let conv_type = &message.conversation_type;
        let sender_id = message.sender_id;

        let current_user_id = self
            .auth_snapshot()
            .current_user_id
            .or_else(|| self.session_current_user_id())
            .map(|id| id.to_string())
            .unwrap_or_default();

        // Sidebar preview and time come from the conversation's NEWEST
        // message: a late-delivered older one must not move them backwards.
        let latest = self.latest_message(&conversation_id, conv_type);
        let (preview, preview_at) = self.sidebar_activity(Some(latest.as_ref().unwrap_or(message)));

        let is_active = self.is_showing(message);
        tracing::info!(msg_conv = %conversation_id, is_active, ?conv_type, "[UI_RECV] handle_message_received");
        // On screen: flip it (and earlier unreads) to read so the badge
        // doesn't accumulate.
        if is_active {
            if let Err(e) = self.mark_conversation_read(&conversation_id, conv_type) {
                tracing::warn!(error = %e, "Failed to mark active conversation as read on incoming message");
            }
        }

        let sidebar_conversation =
            self.classify_active_conversation(&conversation_id, conv_type, sender_id);
        let badges = self.unread_badges(&sidebar_conversation);
        let mention_count = self.sidebar_mention_count(&conversation_id, conv_type);

        let downloads: Vec<Attachment> = message
            .attachments
            .iter()
            .filter_map(|id| self.get_attachment(id))
            .filter(Attachment::needs_download)
            .collect();
        let own_sent = *conv_type == ConversationType::ServerDm
            && sender_id.is_some()
            && sender_id == self.session_current_user_id();

        // The message ends its sender's typing: record it so a STARTED sent
        // before it but delivered after it is dropped rather than shown.
        if let Some(sid) = sender_id {
            self.update_typing_indicator(conversation_id, sid, message.sent_at, false);
        }

        let thread_root = message.thread_id;
        let panel_shows_thread = thread_root
            .zip(self.active_thread())
            .is_some_and(|(root, panel)| panel.root_message_id == root);

        IncomingUpdate {
            is_active,
            replaces_existing: replaces_existing(message),
            marks_delivered: !conv_type.is_anon(),
            groups_continuations: *conv_type == ConversationType::Channel,
            thread_root,
            panel_shows_thread,
            current_user_id,
            sidebar_conversation,
            preview,
            preview_at,
            badges,
            mention_count,
            downloads,
            own_sent,
        }
    }
}

/// Saved Messages snapshots and ended calls rewrite an existing row from
/// their stored copy.
fn replaces_existing(message: &Message) -> bool {
    message.conversation_type == ConversationType::SavedMessages
        || matches!(
            &message.content,
            MessageContent::Call(call) if call.call_type == CallMessageType::Ended
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::PageAnchor;
    use crate::domain::ids::{ChannelId, UserId};

    fn client() -> (Arc<ChatterClient>, tempfile::TempDir) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        (client, dir)
    }

    /// A live row is the row a reload of the same conversation shows, and a
    /// Saved Messages snapshot with no stored copy shows nothing.
    #[tokio::test]
    async fn live_row_matches_the_reloaded_row() {
        let (client, _dir) = client();
        let peer = UserId::new();
        let msg = Message::new_text(
            ChannelId::from(peer),
            ConversationType::ServerDm,
            Some(peer),
            "hi".into(),
            7,
        );
        client.messages().store_message_local(&msg).expect("store");

        let (_, live) = client.incoming_message_row(msg.clone()).expect("a row");
        let page = client
            .open_conversation(
                ActiveConversation::ServerDm { peer_id: peer },
                PageAnchor::Newest,
                || false,
            )
            .expect("reload");
        assert_eq!(format!("{live:?}"), format!("{:?}", page.rows[0]));

        let unsaved = Message::new_text(
            ChannelId::new(),
            ConversationType::SavedMessages,
            Some(peer),
            "note".into(),
            8,
        );
        assert!(client.incoming_message_row(unsaved).is_none());
    }
}

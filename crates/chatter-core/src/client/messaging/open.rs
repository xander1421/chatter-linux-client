//! Opening a conversation and paging its history: one method, every kind.

use std::sync::Arc;

use super::ChatterClient;
use crate::domain::ids::MessageId;
use crate::domain::message::{ConversationType, Message, UnreadAnchor};
use crate::client::ResultScope;
use crate::client::conversation::ConversationError;
use crate::state::ActiveConversation;
use crate::ui::message_row::{MessageRow, apply_reply_counts, mark_disappearing};

/// Which page of a conversation's main list to load.
#[derive(Clone, Debug)]
pub enum PageAnchor {
    /// The conversation is being opened: the newest rows, grown older until
    /// they hold the first message that was unread
    /// ([`UnreadAnchor::opening_page`]).
    Opening(UnreadAnchor),
    /// One page older than this message.
    Older(MessageId),
    /// One page newer than this message.
    Newer(MessageId),
    /// The newest page.
    Newest,
    /// The newest rows grown back to this message (a jump target).
    Reaching(MessageId),
}

/// One page of a conversation, ready for the list.
#[derive(Clone, Debug)]
pub struct ConversationPage {
    /// The session and load of the conversation this page was made for.
    pub scope: ResultScope,
    /// Display rows, oldest first. Timer-change notices are not rows.
    pub rows: Vec<MessageRow>,
    /// [`PageAnchor::Opening`] only: the row the unread divider sits above;
    /// `None` opens at the newest row.
    pub unread_index: Option<usize>,
    /// [`PageAnchor::Opening`] only: how many messages were unread.
    pub unread_count: u32,
    /// The newest stored message the read returned, timer notices included.
    /// `None` when the read found nothing at all.
    pub last_stored: Option<MessageId>,
}

impl ConversationPage {
    /// A page with no rows, for `scope`.
    pub fn empty(scope: ResultScope) -> Self {
        Self {
            scope,
            rows: Vec::new(),
            unread_index: None,
            unread_count: 0,
            last_stored: None,
        }
    }
}

impl ChatterClient {
    /// Load one page of `conversation` and shape it into rows: read
    /// receipts, disappearing marks, thread reply counts and timer-notice
    /// filtering all happen here, so every open and every history page
    /// follows the same rules.
    ///
    /// `scope` is the load the page is for ([`Self::view_scope`]); the page
    /// carries it. Receipts go out only while it is current, so a page
    /// loaded for a conversation the user has already left receipts nothing.
    pub fn open_conversation(
        self: &Arc<Self>,
        conversation: ActiveConversation,
        anchor: PageAnchor,
        scope: ResultScope,
    ) -> Result<ConversationPage, ConversationError> {
        let (Some(channel_id), Some(kind)) = (
            conversation.channel_id(),
            self.conversation_type_of(&conversation),
        ) else {
            return Err(ConversationError::NoConversation);
        };
        let page_size = crate::config::MESSAGE_PAGE_SIZE as u32;
        let messages: Vec<Message> = match &anchor {
            PageAnchor::Opening(unread) => match conversation {
                ActiveConversation::AnonDm(id) => unread.opening_page(page_size, |limit| {
                    self.get_anon_dm_messages_asc(&id, None, limit as i32)
                })?,
                ActiveConversation::AnonGroup(id) => unread.opening_page(page_size, |limit| {
                    self.get_anon_group_messages_asc(&id, None, limit as i32)
                })?,
                _ => unread.opening_page(page_size, |limit| {
                    self.get_messages_local_asc(&channel_id, &kind, None, limit)
                })?,
            },
            PageAnchor::Older(before) => {
                self.get_older_messages_local_asc(&channel_id, &kind, before)?
            }
            PageAnchor::Newer(after) => {
                self.get_newer_messages_local_asc(&channel_id, &kind, after)?
            }
            PageAnchor::Newest => {
                self.get_messages_local_asc(&channel_id, &kind, None, page_size)?
            }
            PageAnchor::Reaching(target) => {
                self.get_messages_local_reaching(&channel_id, &kind, target)?
            }
        };
        let opening = matches!(anchor, PageAnchor::Opening(_));

        // Receipts go out for the page an open shows, never for history
        // paging. Saved Messages is the user's own notepad, and private groups
        // keep no per-user read state (`receipt_target` refuses them too).
        if opening
            && !matches!(
                kind,
                ConversationType::SavedMessages | ConversationType::AnonGroup
            )
            && scope.is_current()
        {
            self.conversation(conversation)
                .send_read_receipts_for_page(&messages);
        }

        let mut rows = self.message_rows(&messages);

        // Every kind marks its disappearing rows. Only channels — Saved
        // Messages included — count thread replies on open; history paging
        // counts them for every kind.
        mark_disappearing(self, &mut rows);
        if !opening || matches!(conversation, ActiveConversation::Channel(_)) {
            apply_reply_counts(&mut rows, &self.thread_reply_counts(&channel_id, &kind));
        }

        let (unread_index, unread_count) = match anchor {
            PageAnchor::Opening(unread) => {
                let unread = self.opening_unread_anchor(unread);
                (unread.divider_index(&messages), unread.unread_count)
            }
            _ => (None, 0),
        };
        Ok(ConversationPage {
            scope,
            rows,
            unread_index,
            unread_count,
            last_stored: messages.last().map(|m| m.id),
        })
    }
}

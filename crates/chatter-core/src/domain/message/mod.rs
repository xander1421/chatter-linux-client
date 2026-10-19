//! Message domain entity.
//!
//! Represents chat messages, threads, and message content.

mod content;
mod conversation;
mod entity;
mod system;
mod thread;

pub use content::{
    AIMessage, CallMessage, CallMessageType, EncryptedContent, EncryptedMessageType,
    MessageContent, MixedContent, ReactionCount, SealedSenderContent,
};
pub use conversation::{newest_page_reaching, ConversationType, UnreadAnchor};
pub use entity::{
    truncate_preview_text, Message, MessageActions, MessageMenu, REMOTE_ACTION_WINDOW_NS,
};
pub use system::SystemMessage;
pub use thread::{ordered_insert_position, InsertPosition, Thread};

#[cfg(test)]
use crate::domain::ids::{AttachmentId, ChannelId, MessageId, UserId};

#[cfg(test)]
mod tests {
    use super::*;

    /// Secret chats: nothing is forwarded out; forwards in are text only; no
    /// GIF picker. Every other conversation is unaffected.
    #[test]
    fn secret_chat_forward_and_gif_rules() {
        for ct in [ConversationType::AnonDm, ConversationType::AnonGroup] {
            assert!(ct.is_secret_chat());
            assert!(!ct.can_forward_from());
            assert!(ct.accepts_forward(false));
            assert!(!ct.accepts_forward(true));
            assert!(!ct.gif_picker_available());
        }
        for ct in [
            ConversationType::Channel,
            ConversationType::ServerDm,
            ConversationType::SavedMessages,
            ConversationType::Mesh,
            ConversationType::MeshDm,
        ] {
            assert!(!ct.is_secret_chat());
            assert!(ct.can_forward_from());
            assert!(ct.accepts_forward(true) && ct.accepts_forward(false));
            assert!(ct.gif_picker_available());
        }
    }

    #[test]
    fn ordered_insert_position_dedup_and_ordering() {
        let existing = [(10u64, "b"), (20, "a"), (20, "c")];
        // Duplicate key.
        assert_eq!(
            ordered_insert_position(&existing, 20, "a"),
            InsertPosition::Duplicate
        );
        // Earliest → front.
        assert_eq!(
            ordered_insert_position(&existing, 5, "z"),
            InsertPosition::At(0)
        );
        // Latest → end.
        assert_eq!(
            ordered_insert_position(&existing, 30, "a"),
            InsertPosition::At(3)
        );
        // Tie at ts=20: "b" sits between "a" and "c".
        assert_eq!(
            ordered_insert_position(&existing, 20, "b"),
            InsertPosition::At(2)
        );
        // Empty list.
        assert_eq!(ordered_insert_position(&[], 1, "a"), InsertPosition::At(0));
    }

    /// The unread divider must land on the first row that was unread, never
    /// "somewhere near it". Every case here is a way the chat opens on the
    /// wrong row if `divider_index` drifts.
    #[test]
    fn unread_anchor_divider_index() {
        let cid = ChannelId::new();
        let them = Some(UserId::new());
        let page: Vec<Message> = [10u64, 20, 30, 40, 50]
            .into_iter()
            .map(|ts| Message::new_text(cid, ConversationType::ServerDm, them, "m".into(), ts))
            .collect();
        let anchor = |unread: &[u64]| UnreadAnchor {
            unread_sent_at: unread.to_vec(),
            unread_count: unread.len() as u32,
        };

        // Nothing unread → open at the newest message, no divider.
        assert_eq!(UnreadAnchor::default().divider_index(&page), None);

        // The unread tail of the page.
        assert_eq!(anchor(&[30, 40, 50]).divider_index(&page), Some(2));

        // Whole page unread, and more above it → open at the top, divider
        // above row 0. Not None: "everything is new" is exactly when the
        // divider matters most.
        assert_eq!(
            anchor(&[1, 2, 10, 20, 30, 40, 50]).divider_index(&page),
            Some(0)
        );

        // Only the newest row is unread.
        assert_eq!(anchor(&[50]).divider_index(&page), Some(4));

        // Two unread messages older than the page (device sweep RC25: a badge
        // of 2 put the divider 50 rows back, above a page that was all read).
        assert_eq!(anchor(&[1, 2]).divider_index(&page), None);

        // One unread message older than the page and one on it: the divider
        // goes above the one that is here, not above row 0.
        assert_eq!(anchor(&[1, 50]).divider_index(&page), Some(4));

        // Unread newer than everything loaded → nothing to anchor on; open
        // at the bottom rather than off the end of the model.
        assert_eq!(anchor(&[60]).divider_index(&page), None);

        // Empty page never yields an index.
        assert_eq!(anchor(&[10]).divider_index(&[]), None);
    }

    #[test]
    fn test_message_new_text() {
        let conversation_id = ChannelId::new();
        let sender_id = Some(UserId::new());
        let msg = Message::new_text(
            conversation_id,
            ConversationType::Channel,
            sender_id,
            "Hello, world!".to_string(),
            0,
        );

        assert_eq!(msg.conversation_id, conversation_id);
        assert_eq!(msg.sender_id, sender_id);
        assert_eq!(msg.as_text(), Some("Hello, world!"));
        assert!(!msg.is_pinned);
        assert!(!msg.is_edited);
        assert!(!msg.is_deleted);
    }

    #[test]
    fn test_message_as_text() {
        let conversation_id = ChannelId::new();

        // Text content
        let text_msg = Message::new_text(
            conversation_id,
            ConversationType::Channel,
            None,
            "Test".to_string(),
            0,
        );
        assert_eq!(text_msg.as_text(), Some("Test"));

        // Mixed content
        let mut mixed_msg = Message::new_text(
            conversation_id,
            ConversationType::Channel,
            None,
            "".to_string(),
            0,
        );
        mixed_msg.content =
            MessageContent::Mixed(MixedContent::text_only("Mixed text".to_string()));
        assert_eq!(mixed_msg.as_text(), Some("Mixed text"));

        // Encrypted content (no text extraction)
        let mut enc_msg = Message::new_text(
            conversation_id,
            ConversationType::Channel,
            None,
            "".to_string(),
            0,
        );
        enc_msg.content = MessageContent::Encrypted(EncryptedContent::new(
            vec![1, 2, 3],
            EncryptedMessageType::Signal,
        ));
        assert_eq!(enc_msg.as_text(), None);
    }

    #[test]
    fn test_thread_lifecycle() {
        let root_id = MessageId::new();
        let conversation_id = ChannelId::new();
        let mut thread = Thread::new(root_id, conversation_id);

        assert!(!thread.has_replies());
        assert_eq!(thread.reply_count, 0);

        let user1 = UserId::new();
        let user2 = UserId::new();

        thread.add_reply(user1);
        assert!(thread.has_replies());
        assert_eq!(thread.reply_count, 1);
        assert_eq!(thread.participant_ids.len(), 1);

        thread.add_reply(user2);
        assert_eq!(thread.reply_count, 2);
        assert_eq!(thread.participant_ids.len(), 2);

        // Same user replies again - shouldn't add to participants
        thread.add_reply(user1);
        assert_eq!(thread.reply_count, 3);
        assert_eq!(thread.participant_ids.len(), 2);
    }

    #[test]
    fn test_system_message_description() {
        let user_id = UserId::new();
        let msg = SystemMessage::UserJoined(user_id);
        assert_eq!(msg.description(), "joined the channel");

        let rename_msg = SystemMessage::ChannelRenamed {
            old_name: "old".to_string(),
            new_name: "new".to_string(),
        };
        assert!(rename_msg.description().contains("old"));
        assert!(rename_msg.description().contains("new"));

        let removed = |removed_name: Option<&str>| SystemMessage::UserRemoved {
            by: user_id,
            removed: UserId::new(),
            removed_name: removed_name.map(str::to_string),
        };
        assert_eq!(
            removed(Some("Bob Tester")).description(),
            "removed Bob Tester"
        );
        assert_eq!(removed(None).description(), "removed a member");
        let owner = |to_name: Option<&str>| SystemMessage::OwnershipTransferred {
            to: UserId::new(),
            to_name: to_name.map(str::to_string),
        };
        assert_eq!(owner(Some("Bob")).description(), "made Bob the owner");
        assert_eq!(owner(None).description(), "made a member the owner");
        let banned = SystemMessage::UserBanned {
            by: user_id,
            banned: UserId::new(),
            banned_name: None,
        };
        assert_eq!(banned.description(), "banned a member");
    }

    #[test]
    fn test_call_message_duration() {
        let call = CallMessage::started("call-123".to_string(), UserId::new(), "Alice".to_string());
        assert_eq!(call.call_type, CallMessageType::Started);
        assert!(call.started_at.is_some());
        assert!(call.duration.is_none());
    }

    #[test]
    fn test_reaction_count() {
        let conversation_id = ChannelId::new();
        let mut msg = Message::new_text(
            conversation_id,
            ConversationType::Channel,
            None,
            "Test".to_string(),
            0,
        );
        msg.reaction_counts
            .push(ReactionCount::new("👍".to_string(), 5));
        msg.reaction_counts
            .push(ReactionCount::new("❤️".to_string(), 3));

        assert_eq!(msg.total_reactions(), 8);
    }

    /// The sidebar's second line is the one place every activity kind has to
    /// come out as *something*. Each branch of `sidebar_preview` is a row a
    /// user would otherwise see blank.
    #[test]
    fn test_sidebar_preview_names_the_call() {
        let me = UserId::new();
        let them = UserId::new();
        let call = |initiator: UserId, call_type: CallMessageType| {
            let mut m = Message::new_text(
                ChannelId::from(them),
                ConversationType::ServerDm,
                Some(them),
                String::new(),
                1,
            );
            let mut c = CallMessage::started("c".into(), initiator, String::new());
            c.call_type = call_type;
            m.content = MessageContent::Call(c);
            m
        };
        assert_eq!(
            call(them, CallMessageType::Missed).sidebar_preview(Some(&me), None),
            "\u{1F4DE} Missed call"
        );
        assert_eq!(
            call(them, CallMessageType::Ended).sidebar_preview(Some(&me), None),
            "\u{1F4DE} Incoming call"
        );
        assert_eq!(
            call(me, CallMessageType::Ended).sidebar_preview(Some(&me), None),
            "\u{1F4DE} Outgoing call"
        );
        assert_eq!(
            call(them, CallMessageType::Declined).sidebar_preview(Some(&me), None),
            "\u{1F4DE} Declined call"
        );
        // The caller's side of a declined call says so too, as its Calls tab does.
        assert_eq!(
            call(me, CallMessageType::Declined).sidebar_preview(Some(&me), None),
            "\u{1F4DE} Declined call"
        );
    }

    #[test]
    fn test_sidebar_preview_covers_every_activity_kind() {
        let conv = ChannelId::new();
        let me = UserId::new();
        let them = UserId::new();
        let text = |sender, body: &str| {
            Message::new_text(
                conv,
                ConversationType::Channel,
                Some(sender),
                body.to_string(),
                0,
            )
        };

        // Plain text, and the same text from us.
        assert_eq!(
            text(them, "hello").sidebar_preview(Some(&me), None),
            "hello"
        );
        assert_eq!(
            text(me, "hello").sidebar_preview(Some(&me), None),
            "You: hello"
        );
        // Emoji-only is just text — it must survive, not become "[Message]".
        assert_eq!(
            text(them, "\u{1F389}").sidebar_preview(Some(&me), None),
            "\u{1F389}"
        );

        // Attachment with no caption: typed label when resolved, count when not.
        let mut file = text(them, "");
        file.attachments = vec![AttachmentId::new(), AttachmentId::new()];
        assert_eq!(
            file.sidebar_preview(Some(&me), Some("\u{1F4F7} Photo +1")),
            "\u{1F4F7} Photo +1"
        );
        assert_eq!(
            file.sidebar_preview(Some(&me), None),
            "\u{1F4CE} 2 attachments"
        );
        // Ours says so too — a voice note sent by us was a bare "You:".
        let mut voice = text(me, "");
        voice.attachments = vec![AttachmentId::new()];
        assert_eq!(
            voice.sidebar_preview(Some(&me), Some("\u{1F3A4} Voice message")),
            "You: \u{1F3A4} Voice message"
        );
        assert_eq!(
            voice.sidebar_preview(Some(&me), None),
            "You: \u{1F4CE} 1 attachment"
        );

        // A caption keeps its attachment's icon.
        let mut captioned = text(me, "beach");
        captioned.attachments = vec![AttachmentId::new()];
        assert_eq!(
            captioned.sidebar_preview(Some(&me), Some("\u{1F4F7} Photo")),
            "You: \u{1F4F7} beach"
        );

        // Nothing renderable at all still names the row.
        assert_eq!(text(them, "").sidebar_preview(Some(&me), None), "[Message]");

        // A pin marks every kind, and does not eat the body.
        let mut pinned = text(them, "hello");
        pinned.is_pinned = true;
        assert_eq!(pinned.sidebar_preview(Some(&me), None), "\u{1F4CC} hello");
    }

    #[test]
    fn test_mixed_content() {
        let mixed = MixedContent::new("Caption".to_string(), vec![AttachmentId::new()]);
        assert!(!mixed.is_empty());
        assert_eq!(mixed.text, Some("Caption".to_string()));

        let empty = MixedContent {
            text: None,
            attachment_ids: Vec::new(),
        };
        assert!(empty.is_empty());
    }

    fn bubble(status: &str, body: Option<&str>, attachments: bool, sent_at: u64) -> Message {
        let mut message = Message::new_text(
            ChannelId::new(),
            ConversationType::ServerDm,
            Some(UserId::new()),
            body.unwrap_or("").into(),
            sent_at,
        );
        if attachments {
            message.attachments = vec![AttachmentId::new()];
            let caption = body.unwrap_or("").trim();
            message.content = if caption.is_empty() {
                MessageContent::Mixed(MixedContent::attachments_only(message.attachments.clone()))
            } else {
                MessageContent::Mixed(MixedContent::new(
                    caption.to_string(),
                    message.attachments.clone(),
                ))
            };
        }
        message.delivery_status = status.into();
        message
    }

    /// Menu eligibility: failed vs sent, text vs attachment-only, own vs peer.
    #[test]
    fn message_actions_follow_send_state() {
        const NOW: u64 = 1_700_000_000_000_000_000;
        const HOUR_NS: u64 = 60 * 60 * 1_000_000_000;
        let young = NOW - HOUR_NS;
        let old = NOW - 25 * HOUR_NS;

        let mut failed_voice = bubble("failed", None, true, young);
        failed_voice.failure_reason = Some("Not delivered".into());
        assert!(failed_voice.can_retry_send());
        let actions = failed_voice.actions(true, NOW);
        assert!(actions.resend && actions.forward && actions.copy && actions.download);
        assert!(actions.delete_for_me && actions.select);
        assert!(!actions.react && !actions.reply && !actions.edit && !actions.pin);
        assert!(!actions.delete_for_everyone && !actions.info);

        let failed_text = bubble("failed", Some("hello"), false, young);
        let actions = failed_text.actions(true, NOW);
        assert!(actions.resend && !actions.download && !actions.edit);
        assert!(!actions.react && !actions.delete_for_everyone && !actions.info);

        let sent_text = bubble("sent", Some("hello"), false, young);
        assert!(!sent_text.can_retry_send());
        let actions = sent_text.actions(true, NOW);
        assert!(!actions.resend && !actions.download);
        assert!(actions.react && actions.reply && actions.edit && actions.pin);
        assert!(actions.delete_for_everyone && actions.info && actions.forward && actions.copy);
        assert!(actions.select && actions.delete_for_me);

        let old_text = bubble("sent", Some("hello"), false, old);
        let actions = old_text.actions(true, NOW);
        assert!(!actions.edit && actions.delete_for_everyone);
        assert!(actions.react && actions.reply && actions.pin && actions.info);
        assert!(old_text.actions(false, NOW).delete_for_everyone);
        let mut old_channel = old_text.clone();
        old_channel.conversation_type = ConversationType::Channel;
        assert!(!old_channel.actions(true, NOW).delete_for_everyone);
        assert!(!old_channel.actions(false, NOW).delete_for_everyone);
        let mut old_anonymous_dm = old_text.clone();
        old_anonymous_dm.conversation_type = ConversationType::AnonDm;
        assert!(old_anonymous_dm.actions(false, NOW).delete_for_everyone);

        let sent_voice = bubble("delivered", None, true, young);
        let actions = sent_voice.actions(true, NOW);
        assert!(!actions.edit && actions.delete_for_everyone && actions.download);

        let captioned = bubble("sent", Some("listen"), true, young);
        let actions = captioned.actions(true, NOW);
        assert!(actions.edit && actions.download && actions.delete_for_everyone);

        let peer = bubble("sent", Some("hello"), false, young);
        let actions = peer.actions(false, NOW);
        assert!(!actions.resend && !actions.edit && !actions.info);
        assert!(actions.delete_for_everyone);
        assert!(actions.react && actions.reply && actions.pin && actions.forward && actions.copy);
        assert!(actions.select && actions.delete_for_me);

        let mut channel_peer = bubble("sent", Some("hello"), false, young);
        channel_peer.conversation_type = ConversationType::Channel;
        let actions = channel_peer.actions(false, NOW);
        assert!(!actions.delete_for_everyone);

        let sending = bubble("sending", Some("hello"), false, young);
        let actions = sending.actions(true, NOW);
        assert!(
            !actions.resend && !actions.react && !actions.reply && !actions.edit && !actions.pin
        );
        assert!(!actions.delete_for_everyone && !actions.info);
        assert!(actions.forward && actions.delete_for_me);

        let mut deleted = bubble("failed", Some("hello"), false, young);
        deleted.is_deleted = true;
        let actions = deleted.actions(true, NOW);
        assert!(
            !actions.resend
                && !actions.delete_for_me
                && !actions.forward
                && !deleted.can_retry_send()
        );
    }
}

//! Disappearing messages: the per-conversation timer for 1:1 friend DMs.
//!
//! Signal semantics (`DataMessage.expireTimer` + `Flags.EXPIRATION_TIMER_UPDATE`):
//!
//! - The timer is a property of the CONVERSATION, held on both ends. Either
//!   side may change it; the change is announced with an empty-bodied
//!   `DataMessage` carrying the timer-update flag.
//! - Every visible message also carries the sender's current timer
//!   (`ephemeral_ttl_seconds`, 0 = off), and a receiver whose own value
//!   differs adopts it — so a lost update message heals on the next message.
//! - Each change leaves a conversation notice ("Alice set disappearing
//!   messages to 1 hour") in the transcript on both sides.
//!
//! Expiry is `EphemeralService` (arming) plus `reap_expired`: a message armed with a ttl is reaped
//! locally once it elapses, on the sender's and every receiver's device.
//!
//! Server channels are not covered here: their timer is the channel's
//! server-held `config.ttl`, fixed at creation (the server refuses to change
//! it afterwards — `channel/service/channels.go` UpdateChannel).

use crate::domain::ids::{ChannelId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent, SystemMessage};
use crate::error::StorageError;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::MessageRepository;

/// The picker's choices, in seconds (Signal's presets). `0` = off.
pub const TIMER_PRESETS: [u32; 8] = [0, 30, 300, 3_600, 28_800, 86_400, 604_800, 2_419_200];

/// Human label for a timer value: "Off", "30 seconds", "1 hour", "4 weeks"…
/// Any value (a peer may send one that is not a preset) is named by the
/// largest unit that divides it exactly.
#[must_use]
pub fn timer_label(seconds: u32) -> String {
    if seconds == 0 {
        return "Off".to_string();
    }
    const UNITS: [(u32, &str); 5] = [
        (604_800, "week"),
        (86_400, "day"),
        (3_600, "hour"),
        (60, "minute"),
        (1, "second"),
    ];
    let (size, name) = UNITS
        .iter()
        .copied()
        .find(|(size, _)| seconds % size == 0)
        .unwrap_or((1, "second"));
    let n = seconds / size;
    if n == 1 {
        format!("1 {name}")
    } else {
        format!("{n} {name}s")
    }
}

/// Compact form for the chat header: "30s", "5m", "1h", "1d", "4w".
#[must_use]
pub fn timer_short_label(seconds: u32) -> String {
    if seconds == 0 {
        return String::new();
    }
    const UNITS: [(u32, &str); 5] = [(604_800, "w"), (86_400, "d"), (3_600, "h"), (60, "m"), (1, "s")];
    let (size, suffix) = UNITS
        .iter()
        .copied()
        .find(|(size, _)| seconds % size == 0)
        .unwrap_or((1, "s"));
    format!("{}{suffix}", seconds / size)
}

const TIMER_UPDATE_FLAG: u32 =
    crate::proto::content_v1::data_message::Flags::ExpirationTimerUpdate as u32;

/// Whether `dm` is a timer-update announcement (Signal's
/// `EXPIRATION_TIMER_UPDATE`) rather than a message to show.
#[must_use]
pub fn is_timer_update(dm: &crate::proto::content_v1::DataMessage) -> bool {
    dm.flags & TIMER_UPDATE_FLAG != 0
}

/// The timer a received `DataMessage` states for its conversation (`0` =
/// off). Every visible friend-DM message and every timer update carries it.
#[must_use]
pub fn stated_timer(dm: &crate::proto::content_v1::DataMessage) -> u32 {
    if dm.ephemeral {
        dm.ephemeral_ttl_seconds
    } else {
        0
    }
}

/// The empty-bodied announcement of a new timer for a friend DM.
#[must_use]
pub fn timer_update_content(seconds: u32, sent_at: u64) -> crate::proto::content_v1::Content {
    use crate::proto::content_v1::{content, Content, DataMessage};
    Content {
        timestamp: sent_at,
        content: Some(content::Content::DataMessage(DataMessage {
            timestamp: sent_at,
            flags: TIMER_UPDATE_FLAG,
            ephemeral: seconds > 0,
            ephemeral_ttl_seconds: seconds,
            ..Default::default()
        })),
        ..Default::default()
    }
}

fn server_dm_key(peer_id: &UserId) -> String {
    format!("conv.disappearing.server_dm.{peer_id}")
}

/// This DM's timer in seconds; `0` = off (also when nothing was ever set).
#[must_use]
pub fn server_dm_timer(settings: &SettingsRepository, peer_id: &UserId) -> u32 {
    settings
        .get::<u32>(&server_dm_key(peer_id))
        .ok()
        .flatten()
        .unwrap_or(0)
}

/// Make `seconds` this DM's timer. If that CHANGED it, persist the
/// conversation notice authored by `author` (us for our own change, the peer
/// for one they announced) and return it; `None` means the timer already had
/// that value and nothing was written.
///
/// `sent_at` is the announcing message's timestamp. The notice is filed one
/// nanosecond before it, so it sorts just ahead of a visible message that
/// carried the change and never collides with that message's
/// `(sender, sent_at)` key.
pub fn adopt_server_dm_timer(
    settings: &SettingsRepository,
    message_repo: &MessageRepository,
    peer_id: &UserId,
    author: &UserId,
    seconds: u32,
    sent_at: u64,
) -> Result<Option<Message>, StorageError> {
    if server_dm_timer(settings, peer_id) == seconds {
        return Ok(None);
    }
    settings.set(&server_dm_key(peer_id), &seconds)?;

    let mut notice = Message::new_text(
        ChannelId::from(*peer_id),
        ConversationType::ServerDm,
        Some(*author),
        String::new(),
        sent_at.saturating_sub(1),
    );
    notice.content = MessageContent::System(SystemMessage::DisappearingTimerChanged { seconds });
    // A conversation update, not something to read: it must not raise the
    // unread badge.
    notice.is_read = true;
    message_repo.insert_or_ignore(&notice)?;
    Ok(Some(notice))
}

/// Delete every message whose disappearing timer has run out, with the
/// attachments only those messages carried (row and file), and return what
/// was deleted so the open chat can drop the bubbles. Driven by the reaper in
/// `ChatterClient::start`; a message hard-deleted here is tombstoned
/// (`delete_batch`) so a re-fetch cannot bring it back.
pub fn reap_expired(db: &std::sync::Arc<crate::storage::Database>) -> Result<Vec<Message>, StorageError> {
    use crate::storage::repositories::attachment_repo::AttachmentRepository;
    use crate::storage::repositories::ephemeral_repo::EphemeralRepository;

    let ephemeral = EphemeralRepository::new(db.clone());
    let messages = MessageRepository::new(db.clone());
    let ids = ephemeral.list_expired()?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let gone: Vec<Message> = ids
        .iter()
        .filter_map(|id| messages.get(id).ok().flatten())
        .collect();
    messages.delete_batch(&ids)?;
    // The rows cascade with their message; this clears any whose message was
    // already gone.
    for id in &ids {
        match ephemeral.delete(id) {
            Ok(()) | Err(StorageError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    let attachments: Vec<_> = gone
        .iter()
        .flat_map(|m| m.attachments.iter().copied())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    AttachmentRepository::new(db.clone()).delete_unreferenced(&attachments)?;
    Ok(gone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;
    use std::sync::Arc;

    #[test]
    fn labels_name_every_preset() {
        let labels: Vec<String> = TIMER_PRESETS.iter().map(|s| timer_label(*s)).collect();
        assert_eq!(
            labels,
            ["Off", "30 seconds", "5 minutes", "1 hour", "8 hours", "1 day", "1 week", "4 weeks"]
        );
        assert_eq!(timer_label(90), "90 seconds");
        assert_eq!(timer_short_label(28_800), "8h");
        assert_eq!(timer_short_label(0), "");
    }

    /// Signal: our own message's timer runs from the send, a received one's
    /// only from when it is read.
    #[test]
    fn received_messages_disappear_only_after_they_are_read() {
        use crate::storage::repositories::ephemeral_repo::EphemeralRepository;
        use crate::storage::repositories::settings_repo::keys;

        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("t.db"), "testpass").expect("db"));
        db.run_migrations().expect("migrations");
        let (me, peer) = (UserId::new(), UserId::new());
        SettingsRepository::new(db.clone())
            .set_string(keys::CURRENT_USER_ID, &me.to_string())
            .expect("me");
        let messages = MessageRepository::new(db.clone());
        let ephemeral = EphemeralRepository::new(db.clone());
        let conv = ChannelId::from(peer);
        let store = |sender: UserId, sent_at: u64| {
            let m = Message::new_text(conv, ConversationType::ServerDm, Some(sender), "hi".into(), sent_at);
            messages.insert(&m).expect("insert");
            // A zero timer: expired the moment its clock starts.
            ephemeral.insert_armed(&m.id, 0).expect("arm");
            m.id
        };

        let received = store(peer, 1);
        assert!(reap_expired(&db).expect("reap").is_empty(), "an unread message's clock has not started");

        let own = store(me, 2);
        let reaped: Vec<_> = reap_expired(&db).expect("reap").iter().map(|m| m.id).collect();
        assert_eq!(reaped, [own], "our own message's clock starts at once");

        messages.mark_conversation_read(&conv, &ConversationType::ServerDm).expect("read");
        let reaped: Vec<_> = reap_expired(&db).expect("reap").iter().map(|m| m.id).collect();
        assert_eq!(reaped, [received], "reading it starts the clock");
        assert!(messages.get(&received).expect("get").is_none(), "deleted from storage");
    }

    #[test]
    fn adopting_a_timer_writes_one_notice_per_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("t.db"), "testpass").expect("db"));
        db.run_migrations().expect("migrations");
        let settings = SettingsRepository::new(db.clone());
        let messages = MessageRepository::new(db);
        let peer = UserId::new();

        assert_eq!(server_dm_timer(&settings, &peer), 0);

        let notice = adopt_server_dm_timer(&settings, &messages, &peer, &peer, 3_600, 1_000)
            .expect("adopt")
            .expect("a change writes a notice");
        assert_eq!(server_dm_timer(&settings, &peer), 3_600);
        assert_eq!(
            notice.content,
            MessageContent::System(SystemMessage::DisappearingTimerChanged { seconds: 3_600 })
        );
        assert_eq!(notice.sent_at, 999, "filed just before the announcing message");

        // The same value again (a redelivery, or every later message carrying
        // it) is not a change.
        assert!(adopt_server_dm_timer(&settings, &messages, &peer, &peer, 3_600, 2_000)
            .expect("adopt")
            .is_none());

        // Turning it off is a change.
        assert!(adopt_server_dm_timer(&settings, &messages, &peer, &peer, 0, 3_000)
            .expect("adopt")
            .is_some());
        assert_eq!(server_dm_timer(&settings, &peer), 0);
    }
}

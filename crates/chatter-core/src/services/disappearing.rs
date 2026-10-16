//! Disappearing messages: per-conversation timers for 1:1 DMs.
//!
//! Signal semantics (`DataMessage.expireTimer` + `Flags.EXPIRATION_TIMER_UPDATE`):
//!
//! - The timer is a property of the CONVERSATION, held on both ends. Either
//!   side may change it; the change is announced with an empty-bodied
//!   `DataMessage` carrying the timer-update flag.
//! - Every visible message also carries the sender's current timer
//!   (`ephemeral_ttl_seconds`, 0 = off) and its version
//!   (`expire_timer_version`, Signal's `expireTimerVersion`), so a lost
//!   update message heals on the next message.
//! - The version orders changes (Signal-Desktop
//!   `ConversationModel.updateExpirationTimer`): a conversation starts at
//!   version 1 and each LOCAL change increments it. A received timer is
//!   ignored when its version is 0 or below ours — a late or replayed message
//!   (a resend, an offline backlog, a stale linked device) cannot revert a
//!   newer setting — adopted with its version when higher, and on an equal
//!   version, the larger (deadline, TTL, activation duration) wins, so
//!   simultaneous changes converge regardless of delivery order.
//! - Changes retain internal notice rows for event delivery; presentation
//!   filters those rows out using `is_timer_notice` and shows live status below
//!   the chat header. Activation expiry stops arming new messages and does not
//!   modify the lifetime of existing messages.
//!
//! Expiry is `EphemeralService` (arming) plus `reap_expired`: a message armed with a ttl is reaped
//! locally once it elapses, on the sender's and every receiver's device.
//!
//! Server channels are not covered here: their timer is the channel's
//! server-held `config.ttl`, fixed at creation (the server refuses to change
//! it afterwards — `channel/service/channels.go` UpdateChannel).

use crate::domain::anon_dm::AnonDmId;
use crate::domain::ids::{ChannelId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent, SystemMessage};
use crate::error::StorageError;
use crate::storage::repositories::MessageRepository;
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Parse the settings field in core; reject empty, fractional and overflowing durations.
pub fn activation_minutes(text: &str) -> Option<u32> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|m| (1..=525_600).contains(m))
        .and_then(|m| m.checked_mul(60))
}

pub const DEFAULT_ACTIVATION_SECONDS: u32 = 300;
pub const SECRET_CHAT_DEFAULT_SECONDS: u32 = 86_400;

/// Typed presentation filter for internal timer change notices. Keep storage
/// intact: these rows still drive receive-side timer notifications.
#[must_use]
pub fn is_timer_notice(message: &Message) -> bool {
    message.conversation_type != ConversationType::AnonDm
        && matches!(
            message.content,
            MessageContent::System(SystemMessage::DisappearingTimerChanged { .. })
        )
}

/// Keep internal timer-change rows out of the displayed message history.
/// The caller may check whether the raw page was empty before shaping it.
pub fn visible_history_messages(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .filter(|message| !is_timer_notice(message))
        .collect()
}

/// The picker's choices, in seconds. `0` = off.
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
    const UNITS: [(u32, &str); 5] = [
        (604_800, "w"),
        (86_400, "d"),
        (3_600, "h"),
        (60, "m"),
        (1, "s"),
    ];
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

/// The ttl a `DataMessage` was sent under (`0` = off): how long THIS
/// message's copies live. Not a statement about the conversation's timer —
/// see [`stated_timer`].
#[must_use]
pub fn message_ttl(dm: &crate::proto::content_v1::DataMessage) -> u32 {
    if dm.ephemeral {
        dm.ephemeral_ttl_seconds
    } else {
        0
    }
}

/// A conversation timer as stated by a message: the value and its version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DmTimer {
    /// Seconds; `0` = off.
    pub seconds: u32,
    /// Signal's `expireTimerVersion`; [`INITIAL_TIMER_VERSION`] for a
    /// conversation nobody has changed.
    pub version: u32,
    /// Absolute nanosecond deadline for enabling TTL on NEW messages; 0 = indefinite.
    #[serde(default)]
    pub enabled_until: u64,
    /// Chosen activation duration, independent of each message's lifetime.
    #[serde(default)]
    pub activation_seconds: u32,
}

impl DmTimer {
    #[must_use]
    pub fn effective_seconds(self, now: u64) -> u32 {
        if self.enabled_until != 0 && now >= self.enabled_until {
            0
        } else {
            self.seconds
        }
    }

    #[must_use]
    pub fn status_label(self, now: u64) -> String {
        if self.effective_seconds(now) == 0 {
            return String::new();
        }
        let ttl = format!("Messages disappear {} after read", timer_label(self.seconds));
        if self.enabled_until == 0 {
            return ttl;
        }
        let remaining = self
            .enabled_until
            .saturating_sub(now)
            .div_ceil(1_000_000_000);
        format!(
            "{ttl} · enabled for {} · {} left",
            timer_label(self.activation_seconds),
            timer_short_label(remaining.min(u32::MAX as u64) as u32)
        )
    }
}

/// The version of a conversation whose timer was never changed (Signal's
/// `INITIAL_EXPIRE_TIMER_VERSION`).
pub const INITIAL_TIMER_VERSION: u32 = 1;

/// The timer a received `DataMessage` states for its conversation, or `None`
/// when it states none (version 0: a resend of an old message, a channel
/// message). Every visible friend-DM message and every timer update carries
/// one.
#[must_use]
pub fn stated_timer(dm: &crate::proto::content_v1::DataMessage) -> Option<DmTimer> {
    (dm.expire_timer_version > 0).then(|| DmTimer {
        seconds: dm.ephemeral_ttl_seconds,
        version: dm.expire_timer_version,
        enabled_until: dm.disappearing_enabled_until,
        activation_seconds: dm.disappearing_activation_seconds,
    })
}

/// Stamp `dm` with the conversation's timer: the ttl this message is sent
/// under and the version a receiver orders it by.
pub fn stamp_timer(dm: &mut crate::proto::content_v1::DataMessage, timer: DmTimer) {
    dm.ephemeral = timer.effective_seconds(dm.timestamp) > 0;
    dm.ephemeral_ttl_seconds = timer.seconds;
    dm.expire_timer_version = timer.version;
    dm.disappearing_enabled_until = timer.enabled_until;
    dm.disappearing_activation_seconds = timer.activation_seconds;
}

/// The empty-bodied announcement of a new timer for a friend DM.
#[must_use]
pub fn timer_update_content(timer: DmTimer, sent_at: u64) -> crate::proto::content_v1::Content {
    use crate::proto::content_v1::{Content, DataMessage, content};
    let mut dm = DataMessage {
        timestamp: sent_at,
        flags: TIMER_UPDATE_FLAG,
        ..Default::default()
    };
    stamp_timer(&mut dm, timer);
    Content {
        timestamp: sent_at,
        content: Some(content::Content::DataMessage(dm)),
        ..Default::default()
    }
}

fn server_dm_key(peer_id: &UserId) -> String {
    format!("conv.disappearing.server_dm.{peer_id}")
}

fn anon_dm_key(id: &AnonDmId) -> String {
    format!("conv.disappearing.anon_dm.{id}")
}

/// Persisted secret-chat timer. An existing chat with no timer remains off.
pub fn anon_dm_state(settings: &SettingsRepository, id: &AnonDmId) -> DmTimer {
    settings
        .get::<DmTimer>(&anon_dm_key(id))
        .ok()
        .flatten()
        .unwrap_or(DmTimer {
            seconds: 0,
            version: INITIAL_TIMER_VERSION,
            enabled_until: 0,
            activation_seconds: 0,
        })
}

/// Install the requested one-day timer when a secret chat is first created.
pub fn initialize_anon_dm_timer(
    settings: &SettingsRepository,
    messages: &MessageRepository,
    id: &AnonDmId,
    author: &UserId,
    sent_at: u64,
) -> Result<(), StorageError> {
    settings.set(&anon_dm_key(id), &DmTimer {
        seconds: SECRET_CHAT_DEFAULT_SECONDS,
        version: INITIAL_TIMER_VERSION,
        enabled_until: 0,
        activation_seconds: 0,
    })?;
    file_anon_notice(messages, id, author, SECRET_CHAT_DEFAULT_SECONDS, sent_at)?;
    Ok(())
}

pub fn change_anon_dm_timer(
    settings: &SettingsRepository,
    messages: &MessageRepository,
    id: &AnonDmId,
    author: &UserId,
    seconds: u32,
    sent_at: u64,
) -> Result<Option<(Message, DmTimer)>, StorageError> {
    let current = anon_dm_state(settings, id);
    if current.seconds == seconds {
        return Ok(None);
    }
    let next = DmTimer {
        seconds,
        version: current.version.saturating_add(1),
        enabled_until: 0,
        activation_seconds: 0,
    };
    settings.set(&anon_dm_key(id), &next)?;
    Ok(Some((
        file_anon_notice(messages, id, author, seconds, sent_at)?,
        next,
    )))
}

pub fn adopt_anon_dm_timer(
    settings: &SettingsRepository,
    messages: &MessageRepository,
    id: &AnonDmId,
    author: &UserId,
    stated: DmTimer,
    sent_at: u64,
) -> Result<Option<Message>, StorageError> {
    let current = anon_dm_state(settings, id);
    if stated.version == 0
        || stated.version < current.version
        || (stated.version == current.version && stated.seconds <= current.seconds)
    {
        return Ok(None);
    }
    settings.set(&anon_dm_key(id), &stated)?;
    Ok(Some(file_anon_notice(
        messages,
        id,
        author,
        stated.seconds,
        sent_at,
    )?))
}

fn file_anon_notice(
    messages: &MessageRepository,
    id: &AnonDmId,
    author: &UserId,
    seconds: u32,
    sent_at: u64,
) -> Result<Message, StorageError> {
    let mut notice = Message::new_text(
        (*id).into(),
        ConversationType::AnonDm,
        Some(*author),
        String::new(),
        sent_at.saturating_sub(1),
    );
    notice.content = MessageContent::System(SystemMessage::DisappearingTimerChanged { seconds });
    notice.is_read = true;
    messages.insert_or_ignore(&notice)?;
    Ok(notice)
}

/// This DM's timer and version (`0` seconds at [`INITIAL_TIMER_VERSION`]
/// when nothing was ever set).
#[must_use]
pub fn server_dm_state(settings: &SettingsRepository, peer_id: &UserId) -> DmTimer {
    settings
        .get::<DmTimer>(&server_dm_key(peer_id))
        .ok()
        .flatten()
        .unwrap_or(DmTimer {
            seconds: 0,
            version: INITIAL_TIMER_VERSION,
            enabled_until: 0,
            activation_seconds: 0,
        })
}

/// This DM's timer in seconds; `0` = off (also when nothing was ever set).
#[must_use]
pub fn server_dm_timer(settings: &SettingsRepository, peer_id: &UserId) -> u32 {
    server_dm_state(settings, peer_id)
        .effective_seconds(crate::util::time::TimestampNanos::now().as_u64())
}

/// Enable this DM's timer for five minutes (or turn it off), incrementing
/// the version and retaining a non-visible notice for receive event plumbing.
pub fn change_server_dm_timer(
    settings: &SettingsRepository,
    message_repo: &MessageRepository,
    peer_id: &UserId,
    me: &UserId,
    seconds: u32,
    sent_at: u64,
) -> Result<Option<(Message, DmTimer)>, StorageError> {
    change_server_dm_window(
        settings,
        message_repo,
        peer_id,
        me,
        seconds,
        DEFAULT_ACTIVATION_SECONDS,
        sent_at,
    )
}

pub fn change_server_dm_window(
    settings: &SettingsRepository,
    message_repo: &MessageRepository,
    peer_id: &UserId,
    me: &UserId,
    seconds: u32,
    activation_seconds: u32,
    sent_at: u64,
) -> Result<Option<(Message, DmTimer)>, StorageError> {
    let current = server_dm_state(settings, peer_id);
    if (seconds == 0 && current.seconds == 0)
        || (activation_seconds == 0 && current.seconds == seconds && current.enabled_until == 0)
    {
        return Ok(None);
    }
    let next = DmTimer {
        seconds,
        version: current.version.saturating_add(1),
        enabled_until: if seconds == 0 || activation_seconds == 0 {
            0
        } else {
            sent_at.saturating_add(u64::from(activation_seconds) * 1_000_000_000)
        },
        activation_seconds: if seconds == 0 { 0 } else { activation_seconds },
    };
    settings.set(&server_dm_key(peer_id), &next)?;
    Ok(Some((
        file_notice(message_repo, peer_id, me, seconds, sent_at)?,
        next,
    )))
}

/// Adopt a timer `stated` by a received message (the peer's, or our own from
/// another device), by Signal's version rule — see the module doc. If that
/// changed the persisted state, retain an internal notice authored by `author`
/// and return it. Equal-version ties use a deterministic total ordering;
/// older and redelivered statements cannot extend an activation window.
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
    stated: DmTimer,
    sent_at: u64,
) -> Result<Option<Message>, StorageError> {
    let current = server_dm_state(settings, peer_id);
    if stated.version == 0 || stated.version < current.version {
        return Ok(None);
    }
    if stated == current
        || (stated.version == current.version
            && (
                stated.enabled_until,
                stated.seconds,
                stated.activation_seconds,
            ) <= (
                current.enabled_until,
                current.seconds,
                current.activation_seconds,
            ))
    {
        return Ok(None);
    }
    settings.set(&server_dm_key(peer_id), &stated)?;
    Ok(Some(file_notice(
        message_repo,
        peer_id,
        author,
        stated.seconds,
        sent_at,
    )?))
}

fn file_notice(
    message_repo: &MessageRepository,
    peer_id: &UserId,
    author: &UserId,
    seconds: u32,
    sent_at: u64,
) -> Result<Message, StorageError> {
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
    Ok(notice)
}

/// Delete every message whose disappearing timer has run out, with the
/// attachments only those messages carried (row and file), and return what
/// was deleted so the open chat can drop the bubbles. Driven by the reaper in
/// `ChatterClient::start`; a message hard-deleted here is tombstoned
/// (`delete_batch`) so a re-fetch cannot bring it back.
pub fn reap_expired(
    db: &std::sync::Arc<crate::storage::Database>,
) -> Result<Vec<Message>, StorageError> {
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
            [
                "Off",
                "30 seconds",
                "5 minutes",
                "1 hour",
                "8 hours",
                "1 day",
                "1 week",
                "4 weeks"
            ]
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
            let m = Message::new_text(
                conv,
                ConversationType::ServerDm,
                Some(sender),
                "hi".into(),
                sent_at,
            );
            messages.insert(&m).expect("insert");
            // A zero timer: expired the moment its clock starts.
            ephemeral.insert_armed(&m.id, 0).expect("arm");
            m.id
        };

        let received = store(peer, 1);
        assert!(
            reap_expired(&db).expect("reap").is_empty(),
            "an unread message's clock has not started"
        );

        let own = store(me, 2);
        let reaped: Vec<_> = reap_expired(&db)
            .expect("reap")
            .iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(reaped, [own], "our own message's clock starts at once");

        messages
            .mark_conversation_read(&conv, &ConversationType::ServerDm)
            .expect("read");
        let reaped: Vec<_> = reap_expired(&db)
            .expect("reap")
            .iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(reaped, [received], "reading it starts the clock");
        assert!(
            messages.get(&received).expect("get").is_none(),
            "deleted from storage"
        );
    }

    /// Signal-Desktop `expirationStartTimestamp`: an own DM that is queued
    /// or failed keeps its stamped TTL with the clock stopped, through reads
    /// too, and the clock starts when any path confirms the send.
    #[test]
    fn own_unsent_messages_do_not_disappear_until_sent() {
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
        let mut ids = Vec::new();
        for (status, sent_at) in [("sending", 1), ("failed", 2)] {
            let mut m = Message::new_text(
                conv,
                ConversationType::ServerDm,
                Some(me),
                "hi".into(),
                sent_at,
            );
            m.delivery_status = status.into();
            messages.insert_outgoing(&m).expect("insert");
            ephemeral.insert_armed(&m.id, 0).expect("arm");
            ids.push(m.id);
        }
        messages
            .mark_conversation_read(&conv, &ConversationType::ServerDm)
            .expect("read");
        assert!(
            reap_expired(&db).expect("reap").is_empty(),
            "unsent: clock stopped"
        );

        messages
            .update_delivery_status_by_timestamps(&conv, &ConversationType::ServerDm, &[2], "sent")
            .expect("confirm");
        let reaped: Vec<_> = reap_expired(&db)
            .expect("reap")
            .iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(reaped, [ids[1]], "confirmation starts the stamped clock");
    }

    fn repos() -> (tempfile::TempDir, SettingsRepository, MessageRepository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("t.db"), "testpass").expect("db"));
        db.run_migrations().expect("migrations");
        (
            dir,
            SettingsRepository::new(db.clone()),
            MessageRepository::new(db),
        )
    }

    const fn t(seconds: u32, version: u32) -> DmTimer {
        DmTimer {
            seconds,
            version,
            enabled_until: 0,
            activation_seconds: 0,
        }
    }

    #[test]
    fn a_newer_version_is_adopted_with_one_notice_per_change() {
        let (_dir, settings, messages) = repos();
        let peer = UserId::new();
        assert_eq!(
            server_dm_state(&settings, &peer),
            t(0, INITIAL_TIMER_VERSION)
        );

        let notice = adopt_server_dm_timer(&settings, &messages, &peer, &peer, t(3_600, 2), 1_000)
            .expect("adopt")
            .expect("a change writes a notice");
        assert_eq!(server_dm_state(&settings, &peer), t(3_600, 2));
        assert_eq!(
            notice.content,
            MessageContent::System(SystemMessage::DisappearingTimerChanged { seconds: 3_600 })
        );
        assert_eq!(
            notice.sent_at, 999,
            "filed just before the announcing message"
        );

        // The same statement again (a redelivery, or every later message
        // carrying it) is not a change.
        assert!(
            adopt_server_dm_timer(&settings, &messages, &peer, &peer, t(3_600, 2), 2_000)
                .expect("adopt")
                .is_none()
        );

        // A newer version turning it off is a change.
        assert!(
            adopt_server_dm_timer(&settings, &messages, &peer, &peer, t(0, 3), 3_000)
                .expect("adopt")
                .is_some()
        );
        assert_eq!(server_dm_state(&settings, &peer), t(0, 3));
    }

    /// The bug the version exists for: a late message (a send-log resend,
    /// offline backlog, stale linked device) stating an older timer must not
    /// revert a newer one, nor log a notice.
    #[test]
    fn an_older_or_absent_version_is_ignored() {
        let (_dir, settings, messages) = repos();
        let peer = UserId::new();
        adopt_server_dm_timer(&settings, &messages, &peer, &peer, t(86_400, 5), 1_000)
            .expect("adopt");

        assert!(
            adopt_server_dm_timer(&settings, &messages, &peer, &peer, t(0, 4), 2_000)
                .expect("adopt")
                .is_none()
        );
        assert!(
            adopt_server_dm_timer(&settings, &messages, &peer, &peer, t(30, 0), 3_000)
                .expect("adopt")
                .is_none()
        );
        assert_eq!(server_dm_state(&settings, &peer), t(86_400, 5), "unchanged");
    }

    /// Equal-version conflict: the larger tuple wins; the version stays.
    #[test]
    fn an_equal_version_takes_the_stated_timer() {
        let (_dir, settings, messages) = repos();
        let (me, peer) = (UserId::new(), UserId::new());
        change_server_dm_window(&settings, &messages, &peer, &me, 300, 0, 1_000).expect("change");
        assert_eq!(server_dm_state(&settings, &peer), t(300, 2));

        let notice = adopt_server_dm_timer(&settings, &messages, &peer, &peer, t(3_600, 2), 2_000)
            .expect("adopt");
        assert!(
            notice.is_some(),
            "a different timer at the same version is adopted"
        );
        assert_eq!(server_dm_state(&settings, &peer), t(3_600, 2));
    }

    #[test]
    fn a_local_change_increments_the_version() {
        let (_dir, settings, messages) = repos();
        let (me, peer) = (UserId::new(), UserId::new());

        let (notice, announced) =
            change_server_dm_window(&settings, &messages, &peer, &me, 3_600, 0, 1_000)
                .expect("change")
                .expect("a change");
        assert_eq!(announced, t(3_600, 2));
        assert_eq!(notice.sender_id, Some(me));
        assert!(
            change_server_dm_window(&settings, &messages, &peer, &me, 3_600, 0, 2_000)
                .expect("change")
                .is_none(),
            "same value: no change, no bump"
        );
        assert_eq!(server_dm_state(&settings, &peer), t(3_600, 2));

        let (_, announced) = change_server_dm_timer(&settings, &messages, &peer, &me, 0, 3_000)
            .expect("change")
            .expect("a change");
        assert_eq!(announced, t(0, 3));

        // What goes on the wire states that version, and reads back as it.
        let content = timer_update_content(announced, 3_000);
        let Some(crate::proto::content_v1::content::Content::DataMessage(dm)) = content.content
        else {
            panic!("a DataMessage");
        };
        assert!(is_timer_update(&dm));
        assert_eq!(stated_timer(&dm), Some(announced));
    }
    #[test]
    fn activation_is_persisted_and_does_not_restart_on_peer_delivery() {
        let (_dir, settings, messages) = repos();
        let (_peer_dir, peer_settings, peer_messages) = repos();
        let (me, peer) = (UserId::new(), UserId::new());
        let start = 1_000_000_000;
        let (_, timer) = change_server_dm_window(&settings, &messages, &peer, &me, 30, 300, start)
            .expect("change")
            .expect("new activation");
        assert_eq!(timer.enabled_until, 301_000_000_000);
        assert_eq!(server_dm_state(&settings, &peer), timer);
        let mut message = crate::proto::content_v1::DataMessage {
            timestamp: start,
            ..Default::default()
        };
        stamp_timer(&mut message, timer);
        assert_eq!(message_ttl(&message), 30);
        assert_eq!(stated_timer(&message), Some(timer));
        adopt_server_dm_timer(
            &peer_settings,
            &peer_messages,
            &me,
            &me,
            timer,
            start + 60_000_000_000,
        )
        .expect("peer adopts");
        assert_eq!(server_dm_state(&peer_settings, &me), timer);
        assert!(
            adopt_server_dm_timer(
                &peer_settings,
                &peer_messages,
                &me,
                &me,
                timer,
                start + 120_000_000_000
            )
            .expect("redelivery")
            .is_none()
        );
        assert_eq!(timer.effective_seconds(timer.enabled_until - 1), 30);
        assert_eq!(timer.effective_seconds(timer.enabled_until), 0);
        message.timestamp = timer.enabled_until;
        stamp_timer(&mut message, timer);
        assert_eq!(
            message_ttl(&message),
            0,
            "new messages are not armed after activation ends"
        );
        assert_eq!(
            stated_timer(&message),
            Some(timer),
            "expiry does not rewrite configuration at the same version"
        );
    }

    #[test]
    fn reopening_storage_keeps_deadline_and_activation_expiry_keeps_user_messages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("restart.db");
        let (me, peer) = (UserId::new(), UserId::new());
        let (timer, id) = {
            let db = Arc::new(Database::open(&path, "testpass").expect("db"));
            db.run_migrations().expect("migrations");
            let settings = SettingsRepository::new(db.clone());
            let messages = MessageRepository::new(db);
            let message = Message::new_text(
                ChannelId::from(peer),
                ConversationType::ServerDm,
                Some(me),
                "keep this message".into(),
                5,
            );
            messages.insert(&message).expect("insert");
            let (_, timer) = change_server_dm_window(&settings, &messages, &peer, &me, 30, 300, 10)
                .expect("change")
                .expect("activation");
            (timer, message.id)
        };
        let db = Arc::new(Database::open(&path, "testpass").expect("reopen"));
        let settings = SettingsRepository::new(db.clone());
        let stored = server_dm_state(&settings, &peer);
        assert_eq!(stored, timer, "reopen cannot restart the activation window");
        assert_eq!(stored.effective_seconds(stored.enabled_until), 0);
        assert!(
            reap_expired(&db).expect("reap").is_empty(),
            "activation expiry cannot delete unarmed user messages"
        );
        assert!(MessageRepository::new(db).get(&id).expect("get").is_some());
    }

    #[test]
    fn simultaneous_changes_converge_in_either_delivery_order() {
        let (_dir, settings, messages) = repos();
        let (_dir2, settings2, messages2) = repos();
        let peer = UserId::new();
        let first = DmTimer {
            enabled_until: 300_000_000_000,
            activation_seconds: 300,
            ..t(30, 2)
        };
        let second = DmTimer {
            enabled_until: 600_000_000_000,
            activation_seconds: 600,
            ..t(60, 2)
        };
        for timer in [first, second] {
            adopt_server_dm_timer(&settings, &messages, &peer, &peer, timer, 1).expect("adopt");
        }
        for timer in [second, first] {
            adopt_server_dm_timer(&settings2, &messages2, &peer, &peer, timer, 1).expect("adopt");
        }
        assert_eq!(server_dm_state(&settings, &peer), second);
        assert_eq!(server_dm_state(&settings2, &peer), second);
    }

    #[test]
    fn activation_field_validation_and_existing_settings_preserve_data() {
        assert_eq!(activation_minutes("5"), Some(300));
        assert_eq!(activation_minutes(" 12 "), Some(720));
        for bad in ["", "0", "-1", "1.5", "525601", "999999999999"] {
            assert_eq!(activation_minutes(bad), None);
        }
        let existing: DmTimer =
            serde_json::from_str(r#"{"seconds":30,"version":4}"#).expect("existing timer");
        assert_eq!(existing, t(30, 4));
        assert_eq!(existing.effective_seconds(u64::MAX), 30);
    }
}

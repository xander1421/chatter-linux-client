//! Edits, deletes-for-everyone, reactions and pins that arrived before the
//! message they target (migrations v078, v080).
//!
//! Signal-Desktop keeps these in maps that the arriving message consults
//! (`ts/messageModifiers/Deletes.preload.ts:28` `deletes`, `forMessage`;
//! `Edits.preload.ts` likewise; `Reactions.preload.ts`
//! `findReactionsForMessage` keeps an early reaction until its message
//! arrives). Here they are persisted, keyed by the target's
//! `(author, sent_at)`, and applied by
//! [`UnifiedMessageRepository::insert`](super::UnifiedMessageRepository::insert)
//! / `insert_or_ignore` — the one place every stored message passes — via
//! [`take_on_arrival`].

use std::borrow::Cow;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};

use crate::domain::ids::ChannelId;
use crate::domain::message::{ConversationType, Message, MessageContent};
use crate::domain::UserId;
use crate::error::StorageError;
use crate::storage::repositories::MessageRepository;
use crate::storage::Database;

/// How long a modifier waits for its target: the server keeps a queued
/// message for 30 days, so a target can arrive that late and no later.
pub const MAX_AGE_NS: u64 = 30 * 24 * 60 * 60 * 1_000_000_000;

/// Modifiers kept at most; past this the oldest received go first.
pub const MAX_PENDING: i64 = 1_000;

/// How far from its message an edit may be and still apply: Signal's
/// `isTooOldToEditMessage.std.ts` (`DAY * 2`).
pub const EDIT_MAX_AGE_NS: u64 = 2 * 24 * 60 * 60 * 1_000_000_000;

fn edit_time(edit_ts: u64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_nanos(edit_ts as i64)
}

/// An edit replaces the body only: a text+files message keeps its files.
fn set_edited(message: &mut Message, body: &str, edited_at: chrono::DateTime<chrono::Utc>) {
    match &mut message.content {
        MessageContent::Mixed(mixed) => mixed.text = Some(body.to_string()),
        content => *content = MessageContent::Text(body.to_string()),
    }
    message.is_edited = true;
    message.edited_at = Some(edited_at);
}

/// A reaction held for its message, to be applied once that message is stored.
pub(crate) struct PendingReaction {
    pub reactor: UserId,
    pub emoji: String,
    pub remove: bool,
}

/// What a message arriving now becomes.
pub(crate) enum Arrival<'a> {
    /// Store it — with a pending edit and pin applied, if any were waiting —
    /// then apply `reactions` to the stored row.
    Store { message: Cow<'a, Message>, reactions: Vec<PendingReaction> },
    /// Its author deleted it for everyone before it got here: do not store.
    Deleted,
}

pub struct MessageModifierRepository {
    db: Arc<Database>,
}

impl MessageModifierRepository {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Hold `author`'s edit of its message sent at `target_sent_at` until the
    /// message arrives. `conversation` is where that message must turn up
    /// (`None`: anywhere — an edit from our own other device). Of two pending
    /// edits of one message the one with the later `edit_ts` is kept.
    pub fn queue_edit(
        &self,
        author: &UserId,
        target_sent_at: u64,
        conversation: Option<ChannelId>,
        body: &str,
        edit_ts: u64,
        now: u64,
    ) -> Result<(), StorageError> {
        self.upsert(author, target_sent_at, "edit", None, "", conversation, Some(body), edit_ts, false, now)
    }

    /// Receive `author`'s edit of its own message sent at `target_sent_at` —
    /// the one place every received edit (friend DM, channel sender-key edit,
    /// server `message_updated`, anonymous DM and group) is applied. Each
    /// caller authenticates `author` its own way first. `conversation` is
    /// where the message must be (`None`: anywhere — our own other device).
    /// `edit_ts` is the edit's own timestamp (epoch nanos); `server_ts` is
    /// when the server received it (0 when unknown).
    ///
    /// Signal-Desktop `handleEditMessage.preload.ts:97` /
    /// `isTooOldToEditMessage.std.ts`: an edit whose *server* timestamp is more
    /// than [`EDIT_MAX_AGE_NS`] from its message (`getMessageAge.std.ts`:
    /// `|serverTimestamp - (target.serverTimestamp ?? target.sent_at)|`; we
    /// keep no server time for a stored message, so its `sent_at`) is dropped.
    /// Signal skips that check when the edit has no server timestamp; here the
    /// edit's own timestamp stands in instead, so a missing stamp cannot lift
    /// the window. An edit of a deleted message is dropped. Edits apply in
    /// `edit_ts` order, not arrival order — one not newer than the edit shown
    /// is dropped (Signal drops an edit whose timestamp is already in the
    /// history), which also makes a re-delivered edit a no-op — and
    /// `edited_at` is `edit_ts`. One whose message is not here yet waits for
    /// it ([`take_on_arrival`]). Returns the edited message when it changed.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_received_edit(
        &self,
        messages: &MessageRepository,
        author: &UserId,
        conversation: Option<ChannelId>,
        target_sent_at: u64,
        new_body: &str,
        edit_ts: u64,
        server_ts: u64,
    ) -> Result<Option<Message>, StorageError> {
        let received = if server_ts > 0 { server_ts } else { edit_ts };
        if received.abs_diff(target_sent_at) > EDIT_MAX_AGE_NS {
            tracing::info!(%author, target_sent_at, edit_ts, server_ts, "Edit more than 48h from its message dropped");
            return Ok(None);
        }
        let Some(mut message) = messages.get_by_sender_timestamp(author, target_sent_at)? else {
            self.queue_edit(author, target_sent_at, conversation, new_body, edit_ts, crate::util::time::TimestampNanos::now().as_u64())?;
            return Ok(None);
        };
        let edited_at = edit_time(edit_ts);
        if conversation.is_some_and(|c| c != message.conversation_id)
            || message.is_deleted
            || message.edited_at.is_some_and(|shown| shown >= edited_at)
        {
            return Ok(None);
        }
        set_edited(&mut message, new_body, edited_at);
        message.updated_at = chrono::Utc::now();
        messages.update(&message)?;
        Ok(Some(message))
    }

    /// Hold `author`'s delete-for-everyone of its message sent at
    /// `target_sent_at`, so the message is never stored when it arrives.
    pub fn queue_delete(
        &self,
        author: &UserId,
        target_sent_at: u64,
        conversation: Option<ChannelId>,
        now: u64,
    ) -> Result<(), StorageError> {
        self.upsert(author, target_sent_at, "delete", None, "", conversation, None, 0, false, now)
    }

    /// Hold `reactor`'s add (or `remove`) of `emoji` on `author`'s message sent
    /// at `target_sent_at`. `conversation` is the one the reaction arrived in
    /// (`None`: from our own other device). Per reactor and emoji only the
    /// action with the later `reaction_ts` is kept, so a remove that overtook
    /// its add on the wire still wins.
    #[allow(clippy::too_many_arguments)]
    pub fn queue_reaction(
        &self,
        author: &UserId,
        target_sent_at: u64,
        reactor: &UserId,
        emoji: &str,
        remove: bool,
        conversation: Option<ChannelId>,
        reaction_ts: u64,
        now: u64,
    ) -> Result<(), StorageError> {
        self.upsert(author, target_sent_at, "reaction", Some(reactor), emoji, conversation, None, reaction_ts, remove, now)
    }

    /// Hold `pinner`'s pin (or `unpin`) of `author`'s message sent at
    /// `target_sent_at`; of the pending pins of one message the latest
    /// `pin_ts` applies.
    #[allow(clippy::too_many_arguments)]
    pub fn queue_pin(
        &self,
        author: &UserId,
        target_sent_at: u64,
        pinner: &UserId,
        unpin: bool,
        conversation: Option<ChannelId>,
        pin_ts: u64,
        now: u64,
    ) -> Result<(), StorageError> {
        self.upsert(author, target_sent_at, "pin", Some(pinner), "", conversation, None, pin_ts, unpin, now)
    }

    /// One row per `(author, target, kind, actor, emoji)`; a newer `action_ts`
    /// (or an equal one received later) replaces an older one.
    #[allow(clippy::too_many_arguments)]
    fn upsert(
        &self,
        author: &UserId,
        target_sent_at: u64,
        kind: &str,
        actor: Option<&UserId>,
        emoji: &str,
        conversation: Option<ChannelId>,
        body: Option<&str>,
        action_ts: u64,
        remove: bool,
        now: u64,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO pending_message_modifiers
                 (author_id, target_sent_at, kind, actor_id, emoji, conversation_id, body, action_ts, remove, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (author_id, target_sent_at, kind, actor_id, emoji) DO UPDATE SET
                 conversation_id = excluded.conversation_id, body = excluded.body,
                 action_ts = excluded.action_ts, remove = excluded.remove, received_at = excluded.received_at
             WHERE excluded.action_ts > pending_message_modifiers.action_ts
                OR (excluded.kind <> 'edit' AND excluded.action_ts = pending_message_modifiers.action_ts)",
            params![
                author.to_string(),
                target_sent_at as i64,
                kind,
                actor.map(|a| a.to_string()).unwrap_or_default(),
                emoji,
                conversation.map(|c| c.to_string()),
                body,
                action_ts as i64,
                remove,
                now as i64
            ],
        )?;
        prune(&tx, now)?;
        tx.commit()?;
        Ok(())
    }
}

/// Whether `actor` may modify `message` (a reaction or pin held for it).
/// [`MessageService::resolve_inbound_target`](crate::services::message::MessageService::resolve_inbound_target)'s
/// rule, at arrival: from our own device (`conversation` NULL) anything we
/// hold; from anyone else only a message in the conversation it arrived in
/// (already matched by the caller), where they are the DM peer or a channel
/// member.
fn may_modify(conn: &Connection, message: &Message, actor: &str, from_own_device: bool) -> Result<bool, StorageError> {
    if from_own_device {
        return Ok(true);
    }
    Ok(match message.conversation_type {
        ConversationType::ServerDm => actor.parse::<UserId>().is_ok_and(|a| message.conversation_id == ChannelId::from(a)),
        ConversationType::Channel => conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memberships WHERE channel_id = ?1 AND user_id = ?2)",
            params![message.conversation_id.to_string(), actor],
            |row| row.get(0),
        )?,
        _ => false,
    })
}

/// Consume the modifiers waiting for `message`, on the caller's connection
/// (inside its transaction, so a failed insert puts them back). A delete wins
/// over everything. An edit and a pin are applied to the returned message;
/// reactions are returned for the caller to record once the row exists. Only
/// modifiers for the conversation it is in match; edits and deletes only by
/// its author, reactions and pins by anyone allowed to make them
/// ([`may_modify`]).
pub(crate) fn take_on_arrival<'a>(conn: &Connection, message: &'a Message) -> Result<Arrival<'a>, StorageError> {
    let Some(author) = message.sender_id else {
        return Ok(Arrival::Store { message: Cow::Borrowed(message), reactions: Vec::new() });
    };
    let author = author.to_string();
    let conversation = message.conversation_id.to_string();
    let matching = "author_id = ?1 AND target_sent_at = ?2 AND (conversation_id IS NULL OR conversation_id = ?3)";
    let key = params![author, message.sent_at as i64, conversation];

    let deleted: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pending_message_modifiers WHERE kind = 'delete' AND {matching})"),
        key,
        |row| row.get(0),
    )?;
    if deleted {
        conn.execute(&format!("DELETE FROM pending_message_modifiers WHERE {matching}"), key)?;
        return Ok(Arrival::Deleted);
    }

    let edit: Option<(String, i64)> = conn
        .query_row(
            &format!("SELECT body, action_ts FROM pending_message_modifiers WHERE kind = 'edit' AND {matching}"),
            key,
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    // (kind, actor, emoji, remove, from own device), newest pin last.
    let mut others: Vec<(String, String, String, bool, bool)> = Vec::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT kind, actor_id, emoji, remove, conversation_id IS NULL FROM pending_message_modifiers
             WHERE kind IN ('reaction', 'pin') AND {matching} ORDER BY action_ts, received_at"
        ))?;
        let rows = stmt.query_map(key, |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))?;
        for row in rows {
            others.push(row?);
        }
    }
    conn.execute(&format!("DELETE FROM pending_message_modifiers WHERE {matching}"), key)?;

    let mut stored = Cow::Borrowed(message);
    // A message can arrive already carrying a newer edit (a server-managed
    // channel's catch-up copy); a held edit that is not newer must not undo it.
    if let Some((body, edit_ts)) = edit {
        let edited_at = edit_time(edit_ts as u64);
        if message.edited_at.is_none_or(|shown| shown < edited_at) {
            set_edited(stored.to_mut(), &body, edited_at);
        }
    }
    let mut reactions = Vec::new();
    for (kind, actor, emoji, remove, own) in others {
        if !may_modify(conn, message, &actor, own)? {
            tracing::debug!(%actor, kind, "Held reaction/pin: sender may not modify its target — dropped");
            continue;
        }
        if kind == "pin" {
            stored.to_mut().is_pinned = !remove;
        } else if let Ok(reactor) = actor.parse() {
            reactions.push(PendingReaction { reactor, emoji, remove });
        }
    }
    Ok(Arrival::Store { message: stored, reactions })
}

/// Modifiers older than [`MAX_AGE_NS`], then everything past the newest
/// [`MAX_PENDING`].
fn prune(conn: &Connection, now: u64) -> Result<(), StorageError> {
    conn.execute(
        "DELETE FROM pending_message_modifiers WHERE received_at <= ?1",
        params![now.saturating_sub(MAX_AGE_NS) as i64],
    )?;
    conn.execute(
        "DELETE FROM pending_message_modifiers WHERE rowid IN (
             SELECT rowid FROM pending_message_modifiers ORDER BY received_at DESC LIMIT -1 OFFSET ?1)",
        params![MAX_PENDING],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::ConversationType;
    use crate::storage::repositories::MessageRepository;

    fn repos() -> (tempfile::TempDir, MessageModifierRepository, MessageRepository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("m.db"), "testpass").expect("open"));
        db.run_migrations().expect("migrations");
        (dir, MessageModifierRepository::new(db.clone()), MessageRepository::new(db))
    }

    fn dm(peer: UserId, body: &str, sent_at: u64) -> Message {
        Message::new_text(peer.into(), ConversationType::ServerDm, Some(peer), body.into(), sent_at)
    }

    #[test]
    fn an_early_edit_is_applied_on_arrival_and_the_newer_edit_wins() {
        let (_dir, modifiers, messages) = repos();
        let peer = UserId::new();
        let now = 2_000_000_000_000_000_000;
        let scope = Some(ChannelId::from(peer));
        modifiers.queue_edit(&peer, 100, scope, "second", 300, now).unwrap();
        modifiers.queue_edit(&peer, 100, scope, "first", 200, now).unwrap();

        assert!(messages.insert_or_ignore(&dm(peer, "original", 100)).unwrap());
        let stored = messages.get_by_sender_timestamp(&peer, 100).unwrap().expect("stored");
        assert_eq!(stored.content, MessageContent::Text("second".into()));
        assert!(stored.is_edited);
        assert_eq!(stored.edited_at, Some(chrono::DateTime::from_timestamp_nanos(300)));
    }

    #[test]
    fn an_early_delete_keeps_the_message_from_being_stored() {
        let (_dir, modifiers, messages) = repos();
        let peer = UserId::new();
        let now = 2_000_000_000_000_000_000;
        modifiers.queue_edit(&peer, 100, Some(ChannelId::from(peer)), "edit", 150, now).unwrap();
        modifiers.queue_delete(&peer, 100, Some(ChannelId::from(peer)), now).unwrap();

        let message = dm(peer, "regret", 100);
        assert!(!messages.insert_or_ignore(&message).unwrap(), "deleted before it arrived");
        assert!(messages.get_by_sender_timestamp(&peer, 100).unwrap().is_none());
        assert!(messages.is_tombstoned(&message.id).unwrap(), "a re-fetch stays deleted");

        messages.insert(&dm(peer, "later", 101)).unwrap();
        assert!(messages.get_by_sender_timestamp(&peer, 101).unwrap().is_some(), "only its target");
    }

    #[test]
    fn a_modifier_matches_only_its_author_in_its_conversation() {
        let (_dir, modifiers, messages) = repos();
        let (peer, other) = (UserId::new(), UserId::new());
        let now = 2_000_000_000_000_000_000;
        // `other` claims to delete peer's message; peer's edit names another
        // conversation.
        modifiers.queue_delete(&other, 100, Some(ChannelId::from(other)), now).unwrap();
        modifiers.queue_edit(&peer, 100, Some(ChannelId::from(other)), "x", 150, now).unwrap();
        assert!(messages.insert_or_ignore(&dm(peer, "kept", 100)).unwrap());
        let stored = messages.get_by_sender_timestamp(&peer, 100).unwrap().unwrap();
        assert_eq!(stored.content, MessageContent::Text("kept".into()));
    }

    #[test]
    fn pending_modifiers_expire_and_are_bounded() {
        let (_dir, modifiers, messages) = repos();
        let peer = UserId::new();
        let now = 2_000_000_000_000_000_000;
        modifiers.queue_delete(&peer, 1, None, now).unwrap();
        // A later write prunes what has waited past MAX_AGE.
        modifiers.queue_delete(&peer, 2, None, now + MAX_AGE_NS).unwrap();
        assert!(messages.insert_or_ignore(&dm(peer, "arrived late", 1)).unwrap());

        for i in 0..=MAX_PENDING as u64 {
            modifiers.queue_delete(&peer, 1_000 + i, None, now + MAX_AGE_NS + 1 + i).unwrap();
        }
        assert!(messages.insert_or_ignore(&dm(peer, "evicted", 1_000)).unwrap(), "oldest evicted");
        assert!(!messages.insert_or_ignore(&dm(peer, "deleted", 1_001)).unwrap());
    }

    /// A `MessageService` on a fresh store, logged in as `me`.
    fn service(me: UserId) -> (tempfile::TempDir, Arc<crate::client::ChatterClient>) {
        use crate::storage::repositories::settings_repo::keys;
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = crate::client::ChatterClient::builder()
            .server_url("http://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        client.messages().settings_repo.set(keys::CURRENT_USER_ID, &me.to_string()).unwrap();
        (dir, client)
    }

    /// The receive side of an edit, through `MessageService`: ordered by the
    /// edit's own time, dated by it, and refused past Signal's 48h.
    #[tokio::test]
    async fn edits_order_by_their_timestamp_and_expire_after_48h() {
        let (_dir, client) = service(UserId::new());
        let m = client.messages();
        let peer = UserId::new();
        let sent = 1_700_000_000_000_000_000u64;
        let window = crate::services::message::MessageService::EDIT_MAX_AGE_NS;

        // Early: held, then applied when the message arrives.
        assert!(m.apply_dm_edit(&peer, sent, "early", sent + 10, sent + 10).unwrap().is_none());
        m.message_repo.insert(&dm(peer, "original", sent)).unwrap();
        let text = |m: &crate::services::message::MessageService| {
            m.message_repo.get_by_sender_timestamp(&peer, sent).unwrap().unwrap().content
        };
        assert_eq!(text(m), MessageContent::Text("early".into()));

        let newer = m.apply_dm_edit(&peer, sent, "newer", sent + 30, sent + 31).unwrap().expect("applied");
        assert_eq!(newer.edited_at, Some(chrono::DateTime::from_timestamp_nanos((sent + 30) as i64)));
        assert!(m.apply_dm_edit(&peer, sent, "older", sent + 20, sent + 40).unwrap().is_none(), "older than the one shown");
        assert!(m.apply_dm_edit(&peer, sent, "same", sent + 30, sent + 40).unwrap().is_none(), "already shown");
        // Signal measures the window on the edit's SERVER timestamp: a sender
        // cannot back-date an edit into it.
        assert!(m.apply_dm_edit(&peer, sent, "backdated", sent + 40, sent + window + 1).unwrap().is_none(), "past 48h");
        // With no server timestamp the edit's own one stands in.
        assert!(m.apply_dm_edit(&peer, sent, "late", sent + window + 1, 0).unwrap().is_none(), "past 48h");
        assert_eq!(text(m), MessageContent::Text("newer".into()));
        assert!(m.apply_dm_edit(&peer, sent, "in time", sent + window, 0).unwrap().is_some());

        // Someone else's DM cannot name it.
        assert!(m.apply_dm_delete(&UserId::new(), sent).unwrap().is_none());
        assert!(m.apply_dm_delete(&peer, sent).unwrap().is_some());
    }

    /// What a receive path announces is the row as stored: an early edit
    /// applied, and nothing for a message deleted before it arrived.
    #[test]
    fn a_received_message_is_returned_as_stored() {
        let (_dir, modifiers, messages) = repos();
        let peer = UserId::new();
        let now = 2_000_000_000_000_000_000;
        let scope = Some(ChannelId::from(peer));
        modifiers.queue_edit(&peer, 100, scope, "edited", 150, now).unwrap();
        modifiers.queue_delete(&peer, 200, scope, now).unwrap();

        let stored = messages.insert_received(&dm(peer, "original", 100)).unwrap().expect("stored");
        assert_eq!(stored.content, MessageContent::Text("edited".into()));
        assert!(stored.is_edited);
        assert!(messages.insert_received(&dm(peer, "regret", 200)).unwrap().is_none());

        let fresh = messages.insert_or_ignore_received(&dm(peer, "plain", 300)).unwrap().expect("stored");
        assert_eq!(fresh.content, MessageContent::Text("plain".into()));
        assert!(messages.insert_or_ignore_received(&dm(peer, "plain", 300)).unwrap().is_none(), "duplicate");
    }

    /// Signal keeps a reaction until its message arrives
    /// (`Reactions.preload.ts`); so do we, ordered by the reaction's own time
    /// and bound by the same rule as one whose message is here.
    #[tokio::test]
    async fn an_early_reaction_waits_for_its_message() {
        let me = UserId::new();
        let (_dir, client) = service(me);
        let m = client.messages();
        let (peer, stranger) = (UserId::new(), UserId::new());
        let dm_with_peer = ChannelId::from(peer);
        let reactors = |emoji: &str| -> Vec<UserId> {
            m.message_repo.get_reactions(&peer, 100).unwrap().into_iter()
                .filter(|r| r.emoji.value == emoji).map(|r| r.user_id).collect()
        };

        // The peer's 👍, removed (sent at 20) before the add (sent at 10)
        // reached us: the remove is newer and wins.
        assert!(!m.record_inbound_reaction(&dm_with_peer, &peer, &peer, 100, "a", true, 20).unwrap());
        assert!(!m.record_inbound_reaction(&dm_with_peer, &peer, &peer, 100, "a", false, 10).unwrap());
        // Added (30) after a remove (25) that arrived later: the add stands.
        assert!(!m.record_inbound_reaction(&dm_with_peer, &peer, &peer, 100, "b", false, 30).unwrap());
        assert!(!m.record_inbound_reaction(&dm_with_peer, &peer, &peer, 100, "b", true, 25).unwrap());
        // Ours, from another of our devices.
        assert!(!m.record_inbound_reaction(&ChannelId::from(me), &me, &peer, 100, "c", false, 5).unwrap());
        // A stranger, from their own DM, naming the peer's message.
        assert!(!m.record_inbound_reaction(&ChannelId::from(stranger), &stranger, &peer, 100, "d", false, 5).unwrap());

        let stored = m.message_repo.insert_received(&dm(peer, "hi", 100)).unwrap().expect("stored");
        assert!(reactors("a").is_empty(), "the later remove wins");
        assert_eq!(reactors("b"), vec![peer]);
        assert_eq!(reactors("c"), vec![me]);
        assert!(reactors("d").is_empty(), "not the stranger's conversation");
        let mut counts: Vec<_> = stored.reaction_counts.iter().map(|r| (r.emoji.clone(), r.count)).collect();
        counts.sort();
        assert_eq!(counts, vec![("b".to_string(), 1), ("c".to_string(), 1)], "the announced row carries them");
        assert_eq!(stored.current_user_reactions, vec!["c".to_string()]);

        // Once the message is here, a reaction applies directly.
        assert!(m.record_inbound_reaction(&dm_with_peer, &peer, &peer, 100, "e", false, 40).unwrap());
    }

    /// An early pin is applied on arrival — the latest action winning — and a
    /// channel pin only from a member of that channel.
    #[tokio::test]
    async fn an_early_pin_waits_for_its_message() {
        use crate::domain::channel::{Channel, ChannelType, MemberRole, Membership, NotificationPreference};
        let me = UserId::new();
        let (_dir, client) = service(me);
        let m = client.messages();
        let (peer, member, outsider, channel) = (UserId::new(), UserId::new(), UserId::new(), ChannelId::new());
        m.channel_repo.insert(&Channel::new(channel, "g".into(), ChannelType::Private, me)).unwrap();
        for user_id in [peer, member] {
            m.channel_repo.insert_membership(&Membership {
                channel_id: channel,
                user_id,
                role: MemberRole::Member,
                joined_at: chrono::Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
                notification_preference: NotificationPreference::default(),
            }).unwrap();
        }
        let in_channel = |sent_at| Message::new_text(channel, ConversationType::Channel, Some(peer), "c".into(), sent_at);

        // DM: pinned at 10, unpinned at 20 but delivered first.
        let dm_with_peer = ChannelId::from(peer);
        assert!(m.resolve_inbound_pin(&dm_with_peer, &peer, &peer, 100, true, 20).unwrap().is_none());
        assert!(m.resolve_inbound_pin(&dm_with_peer, &peer, &peer, 100, false, 10).unwrap().is_none());
        assert!(!m.message_repo.insert_received(&dm(peer, "x", 100)).unwrap().unwrap().is_pinned);
        assert!(m.resolve_inbound_pin(&dm_with_peer, &peer, &peer, 101, false, 10).unwrap().is_none());
        assert!(m.message_repo.insert_received(&dm(peer, "y", 101)).unwrap().unwrap().is_pinned);

        // Channel: an outsider's pin is refused when the message arrives; a member's lands.
        assert!(m.resolve_inbound_pin(&channel, &outsider, &peer, 200, false, 10).unwrap().is_none());
        assert!(!m.message_repo.insert_received(&in_channel(200)).unwrap().unwrap().is_pinned);
        assert!(m.resolve_inbound_pin(&channel, &member, &peer, 201, false, 10).unwrap().is_none());
        assert!(m.message_repo.insert_received(&in_channel(201)).unwrap().unwrap().is_pinned);
        // A pin naming another conversation than the message's does not apply.
        assert!(m.resolve_inbound_pin(&dm_with_peer, &peer, &peer, 202, false, 10).unwrap().is_none());
        assert!(!m.message_repo.insert_received(&in_channel(202)).unwrap().unwrap().is_pinned);
    }
}

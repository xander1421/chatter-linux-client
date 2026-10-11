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

use rusqlite::{Connection, OptionalExtension, params};

use crate::domain::UserId;
use crate::domain::ids::ChannelId;
use crate::domain::message::{ConversationType, Message, MessageContent};
use crate::domain::reaction::ReactionScope;
use crate::error::StorageError;
use crate::storage::Database;
use crate::storage::repositories::MessageRepository;

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
    pub action_ts: u64,
}

/// What a message arriving now becomes.
pub(crate) enum Arrival<'a> {
    /// Store it — with a pending edit and pin applied, if any were waiting —
    /// then apply `reactions` to the stored row.
    Store {
        message: Cow<'a, Message>,
        reactions: Vec<PendingReaction>,
    },
    /// Its author deleted it for everyone before it got here: do not store.
    Deleted,
    /// Scoped call tombstone; never create a global message-ID tombstone.
    CallDeleted,
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
        self.upsert(
            author,
            target_sent_at,
            "edit",
            None,
            "",
            conversation,
            Some(body),
            edit_ts,
            false,
            now,
        )
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
            self.queue_edit(
                author,
                target_sent_at,
                conversation,
                new_body,
                edit_ts,
                crate::util::time::TimestampNanos::now().as_u64(),
            )?;
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

    /// Author-only channel tombstone, isolated by both channel and type.
    /// A separate uniqueness namespace keeps it apart from typed DM deletes.
    pub fn queue_channel_delete(
        &self,
        author: &UserId,
        target_sent_at: u64,
        channel: ChannelId,
        now: u64,
    ) -> Result<(), StorageError> {
        let scope_key = format!("channel-delete:{}", channel);
        self.upsert(
            author,
            target_sent_at,
            "delete",
            None,
            &scope_key,
            Some(channel),
            Some(ConversationType::Channel.as_str()),
            0,
            false,
            now,
        )
    }

    /// Typed DM tombstone: author is one of the authenticated DM participants.
    /// The existing body field carries the type; emoji namespaces distinct DMs
    /// in the uniqueness key without changing the schema.
    pub fn queue_dm_delete(
        &self,
        author: &UserId,
        target_sent_at: u64,
        conversation: Option<ChannelId>,
        now: u64,
    ) -> Result<(), StorageError> {
        self.queue_typed_dm_delete(
            author,
            target_sent_at,
            conversation,
            ConversationType::ServerDm,
            now,
        )
    }

    pub fn queue_typed_dm_delete(
        &self,
        author: &UserId,
        target_sent_at: u64,
        conversation: Option<ChannelId>,
        kind: ConversationType,
        now: u64,
    ) -> Result<(), StorageError> {
        if !matches!(kind, ConversationType::ServerDm | ConversationType::AnonDm) {
            return Err(StorageError::InvalidData(
                "Delete scope must be a one-to-one DM".into(),
            ));
        }
        let scope_key = format!(
            "dm-delete:{}:{}",
            kind.as_str(),
            conversation.map(|id| id.to_string()).unwrap_or_default()
        );
        self.upsert(
            author,
            target_sent_at,
            "delete",
            None,
            &scope_key,
            conversation,
            Some(kind.as_str()),
            0,
            false,
            now,
        )
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
        scope: &ReactionScope,
        reaction_ts: u64,
        now: u64,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        queue_reaction_with_conn(
            &tx,
            author,
            target_sent_at,
            reactor,
            emoji,
            remove,
            scope,
            reaction_ts,
            now,
        )?;
        tx.commit()?;
        Ok(())
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
        self.upsert(
            author,
            target_sent_at,
            "pin",
            Some(pinner),
            "",
            conversation,
            None,
            pin_ts,
            unpin,
            now,
        )
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

/// Wire claims must use one unambiguous UUID encoding; empty/nil are refused.
pub(crate) fn canonical_call_claim(value: &str) -> Option<uuid::Uuid> {
    let id = uuid::Uuid::parse_str(value).ok()?;
    (!id.is_nil() && value == id.to_string()).then_some(id)
}

/// Only genuine canonical 1:1 Call records confer shared-call delete authority.
pub(crate) fn canonical_call_record_id(message: &Message) -> Option<uuid::Uuid> {
    let MessageContent::Call(call) = &message.content else {
        return None;
    };
    let raw = match message.conversation_type {
        ConversationType::ServerDm => call.call_id.as_str(),
        _ => return None,
    };
    let id = canonical_call_claim(raw)?;
    (message.id.as_uuid() == &id).then_some(id)
}

/// Exact scope AND typed Call identity, never a global author/timestamp lookup.
pub(crate) fn apply_call_delete(
    db: Arc<Database>,
    messages: &MessageRepository,
    scope: &ReactionScope,
    claim: &str,
) -> Result<Option<Message>, StorageError> {
    let Some(id) = canonical_call_claim(claim) else {
        return Ok(None);
    };
    if scope.kind != ConversationType::ServerDm {
        return Ok(None);
    }
    let conn = db.connection()?;
    let tx = conn.unchecked_transaction()?;
    let target =
        messages.get_with_connection(&tx, &crate::domain::ids::MessageId::from_uuid(id))?;
    if target.as_ref().is_some_and(|m| {
        m.conversation_id != scope.id
            || m.conversation_type != scope.kind
            || canonical_call_record_id(m) != Some(id)
    }) {
        return Ok(None);
    }
    let now = crate::util::time::TimestampNanos::now().as_u64();
    tx.execute(
        "INSERT INTO pending_message_modifiers
            (author_id,target_sent_at,kind,actor_id,emoji,conversation_id,body,action_ts,remove,received_at)
         VALUES (?1,0,'delete','',?2,?3,?4,0,0,?5)
         ON CONFLICT(author_id,target_sent_at,kind,actor_id,emoji) DO UPDATE SET received_at=excluded.received_at",
        params![id.to_string(), format!("call-delete:{}:{}:{}",scope.kind.as_str(),scope.id,id),
            scope.id.to_string(), format!("call:{}",scope.kind.as_str()), now as i64],
    )?;
    prune(&tx, now)?;
    if let Some(message) = &target {
        tx.execute(
            "DELETE FROM messages WHERE id=?1 AND conversation_id=?2 AND conversation_type=?3",
            params![
                message.id.to_string(),
                scope.id.to_string(),
                scope.kind.as_str()
            ],
        )?;
    }
    tx.commit()?;
    Ok(target)
}

/// Connection-aware admission for the receive-artifact application transaction.
#[allow(clippy::too_many_arguments)]
pub(crate) fn queue_reaction_with_conn(
    conn: &Connection,
    author: &UserId,
    target_sent_at: u64,
    reactor: &UserId,
    emoji: &str,
    remove: bool,
    scope: &ReactionScope,
    reaction_ts: u64,
    now: u64,
) -> Result<(), StorageError> {
    if target_sent_at == 0
        || target_sent_at > i64::MAX as u64
        || reaction_ts == 0
        || reaction_ts > i64::MAX as u64
        || emoji.is_empty()
        || emoji.len() > 128
    {
        return Err(StorageError::InvalidData("Invalid held reaction".into()));
    }
    let existing:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM pending_scoped_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 AND actor_id=?5 AND emoji=?6)",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target_sent_at as i64,reactor.to_string(),emoji],|r|r.get(0))?;
    let count: i64 = conn.query_row("SELECT count(*) FROM pending_scoped_reactions", [], |r| {
        r.get(0)
    })?;
    if !existing && count >= MAX_PENDING {
        return Err(StorageError::InvalidData(
            "Scoped reaction admission full; evidence retained".into(),
        ));
    }
    // Latest add must survive a later same-emoji tombstone in the compressed
    // queue, otherwise held arrival and ordinary delivery select differently.
    if !remove {
        conn.execute("INSERT INTO pending_reaction_add_selections(conversation_id,conversation_type,author_id,target_sent_at,actor_id,emoji,action_ts) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(conversation_type,conversation_id,author_id,target_sent_at,actor_id) DO UPDATE SET emoji=excluded.emoji,action_ts=excluded.action_ts WHERE excluded.action_ts>pending_reaction_add_selections.action_ts OR (excluded.action_ts=pending_reaction_add_selections.action_ts AND excluded.emoji>pending_reaction_add_selections.emoji)",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target_sent_at as i64,reactor.to_string(),emoji,reaction_ts as i64])?;
    }
    conn.execute("INSERT INTO pending_scoped_reactions
            (conversation_id,conversation_type,author_id,target_sent_at,actor_id,emoji,remove,action_ts,received_at)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
            ON CONFLICT(conversation_type,conversation_id,author_id,target_sent_at,actor_id,emoji)
            DO UPDATE SET remove=excluded.remove,action_ts=excluded.action_ts,received_at=excluded.received_at
            WHERE excluded.action_ts > pending_scoped_reactions.action_ts
            OR (excluded.action_ts = pending_scoped_reactions.action_ts AND excluded.remove > pending_scoped_reactions.remove)",
            params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target_sent_at as i64,reactor.to_string(),emoji,remove,reaction_ts as i64,now as i64])?;
    Ok(())
}

/// Whether `actor` may modify `message` (a reaction or pin held for it).
/// [`MessageService::resolve_inbound_target`](crate::services::message::MessageService::resolve_inbound_target)'s
/// rule, at arrival: from our own device (`conversation` NULL) anything we
/// hold; from anyone else only a message in the conversation it arrived in
/// (already matched by the caller), where they are the DM peer or a channel
/// member.
pub(crate) fn may_modify(
    conn: &Connection,
    message: &Message,
    actor: &str,
    from_own_device: bool,
) -> Result<bool, StorageError> {
    let own: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key='auth.current_user_id'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(own) = own {
        let own: String = serde_json::from_str(&own)
            .map_err(|_| StorageError::InvalidData("Invalid modifier account".into()))?;
        let blocked:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM friendships WHERE user_id=?1 AND friend_id=?2 AND status=2)",params![own,actor],|r|r.get(0))?;
        if blocked {
            return Ok(false);
        }
    }
    if from_own_device {
        return Ok(true);
    }
    Ok(match message.conversation_type {
        ConversationType::ServerDm => actor
            .parse::<UserId>()
            .is_ok_and(|a| message.conversation_id == ChannelId::from(a)),
        ConversationType::Channel => conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memberships WHERE channel_id=?1 AND user_id=?2)",params![message.conversation_id.to_string(),actor],|row|row.get(0))?,
        ConversationType::AnonGroup => conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM anon_group_members g JOIN anon_groups a ON a.id=g.group_id WHERE g.group_id=?1 AND g.user_id=?2 AND a.status<>2) AND NOT EXISTS(SELECT 1 FROM anon_group_blocked_users WHERE group_id=?1 AND user_id=?2)",
            params![message.conversation_id.to_string(), actor],
            |row| row.get(0),
        )?,
        ConversationType::AnonDm => conn.query_row("SELECT EXISTS(SELECT 1 FROM anon_dm_conversations WHERE id=?1 AND peer_id=?2 AND status=1) AND NOT EXISTS(SELECT 1 FROM anon_dm_blocked_users WHERE user_id=?2)",params![message.conversation_id.to_string(),actor],|r|r.get(0))?,
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
pub(crate) fn take_on_arrival<'a>(
    conn: &Connection,
    message: &'a Message,
) -> Result<Arrival<'a>, StorageError> {
    // Call identity is shared across replicas; authors and timestamps are local.
    // Check before the author fallback.
    if let Some(call_id) = canonical_call_record_id(message) {
        let deleted: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pending_message_modifiers WHERE kind='delete' AND author_id=?1 AND target_sent_at=0 AND conversation_id=?2 AND body=?3 AND emoji=?4)",
            params![call_id.to_string(), message.conversation_id.to_string(),
                format!("call:{}", message.conversation_type.as_str()),
                format!("call-delete:{}:{}:{}", message.conversation_type.as_str(), message.conversation_id, call_id)],
            |row| row.get(0),
        )?;
        if deleted {
            return Ok(Arrival::CallDeleted);
        }
    }
    let Some(author) = message.sender_id else {
        return Ok(Arrival::Store {
            message: Cow::Borrowed(message),
            reactions: Vec::new(),
        });
    };
    let author = author.to_string();
    let conversation = message.conversation_id.to_string();
    let matching = "author_id = ?1 AND target_sent_at = ?2 AND (conversation_id IS NULL OR conversation_id = ?3)";
    let key = params![author, message.sent_at as i64, conversation];

    let deleted: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pending_message_modifiers WHERE kind = 'delete' AND {matching} AND body = ?4)"),
        params![author, message.sent_at as i64, conversation, message.conversation_type.as_str()],
        |row| row.get(0),
    )?;
    if deleted {
        conn.execute(
            &format!("DELETE FROM pending_message_modifiers WHERE {matching} AND kind != 'delete'"),
            key,
        )?;
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
             WHERE kind = 'pin' AND {matching} ORDER BY action_ts, received_at"
        ))?;
        let rows = stmt.query_map(key, |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        for row in rows {
            others.push(row?);
        }
    }
    conn.execute(
        &format!("DELETE FROM pending_message_modifiers WHERE {matching} AND kind != 'delete'"),
        key,
    )?;

    let mut stored = Cow::Borrowed(message);
    // A message can arrive already carrying a newer edit (a server-managed
    // channel's catch-up copy); a held edit that is not newer must not undo it.
    if let Some((body, edit_ts)) = edit {
        let edited_at = edit_time(edit_ts as u64);
        if message.edited_at.is_none_or(|shown| shown < edited_at) {
            set_edited(stored.to_mut(), &body, edited_at);
        }
    }
    for (_, actor, _, remove, own) in &others {
        if may_modify(conn, message, actor, *own)? {
            stored.to_mut().is_pinned = !*remove;
        }
    }
    others.clear();
    let mut scoped = conn.prepare("SELECT actor_id,emoji,remove,action_ts FROM pending_scoped_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 UNION ALL SELECT actor_id,emoji,0,action_ts FROM pending_reaction_add_selections WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 ORDER BY action_ts,remove,emoji")?;
    let scoped_rows = scoped
        .query_map(
            params![
                conversation,
                message.conversation_type.as_str(),
                author,
                message.sent_at as i64
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, i64>(3)? as u64,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(scoped);
    conn.execute("DELETE FROM pending_scoped_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4",params![conversation,message.conversation_type.as_str(),author,message.sent_at as i64])?;
    conn.execute("DELETE FROM pending_reaction_add_selections WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4",params![conversation,message.conversation_type.as_str(),author,message.sent_at as i64])?;
    let own: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key='auth.current_user_id'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let own = own
        .map(|v| {
            serde_json::from_str::<String>(&v).map_err(|_| {
                StorageError::InvalidData("Invalid current-user identity for held reactions".into())
            })
        })
        .transpose()?;
    let mut reactions = Vec::new();
    for (actor, emoji, remove, action_ts) in scoped_rows {
        // Identity is exact even for another local device; authorization never widens scope.
        if may_modify(
            conn,
            message,
            &actor,
            own.as_deref() == Some(actor.as_str()),
        )? {
            let reactor = actor
                .parse()
                .map_err(|_| StorageError::InvalidData("Invalid held actor".into()))?;
            reactions.push(PendingReaction {
                reactor,
                emoji,
                remove,
                action_ts,
            });
        }
    }
    Ok(Arrival::Store {
        message: stored,
        reactions,
    })
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

    fn repos() -> (
        tempfile::TempDir,
        MessageModifierRepository,
        MessageRepository,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("m.db"), "testpass").expect("open"));
        db.run_migrations().expect("migrations");
        (
            dir,
            MessageModifierRepository::new(db.clone()),
            MessageRepository::new(db),
        )
    }

    fn dm(peer: UserId, body: &str, sent_at: u64) -> Message {
        Message::new_text(
            peer.into(),
            ConversationType::ServerDm,
            Some(peer),
            body.into(),
            sent_at,
        )
    }

    #[test]
    fn an_early_edit_is_applied_on_arrival_and_the_newer_edit_wins() {
        let (_dir, modifiers, messages) = repos();
        let peer = UserId::new();
        let now = 2_000_000_000_000_000_000;
        let scope = Some(ChannelId::from(peer));
        modifiers
            .queue_edit(&peer, 100, scope, "second", 300, now)
            .unwrap();
        modifiers
            .queue_edit(&peer, 100, scope, "first", 200, now)
            .unwrap();

        assert!(messages
            .insert_or_ignore(&dm(peer, "original", 100))
            .unwrap());
        let stored = messages
            .get_by_sender_timestamp(&peer, 100)
            .unwrap()
            .expect("stored");
        assert_eq!(stored.content, MessageContent::Text("second".into()));
        assert!(stored.is_edited);
        assert_eq!(
            stored.edited_at,
            Some(chrono::DateTime::from_timestamp_nanos(300))
        );
    }

    #[test]
    fn an_early_delete_keeps_the_message_from_being_stored() {
        let (_dir, modifiers, messages) = repos();
        let peer = UserId::new();
        let now = 2_000_000_000_000_000_000;
        modifiers
            .queue_edit(&peer, 100, Some(ChannelId::from(peer)), "edit", 150, now)
            .unwrap();
        modifiers
            .queue_dm_delete(&peer, 100, Some(ChannelId::from(peer)), now)
            .unwrap();

        let message = dm(peer, "regret", 100);
        assert!(
            !messages.insert_or_ignore(&message).unwrap(),
            "deleted before it arrived"
        );
        assert!(messages
            .get_by_sender_timestamp(&peer, 100)
            .unwrap()
            .is_none());
        assert!(
            messages.is_tombstoned(&message.id).unwrap(),
            "a re-fetch stays deleted"
        );

        messages.insert(&dm(peer, "later", 101)).unwrap();
        assert!(
            messages
                .get_by_sender_timestamp(&peer, 101)
                .unwrap()
                .is_some(),
            "only its target"
        );
    }

    #[test]
    fn a_modifier_matches_only_its_author_in_its_conversation() {
        let (_dir, modifiers, messages) = repos();
        let (peer, other) = (UserId::new(), UserId::new());
        let now = 2_000_000_000_000_000_000;
        // `other` claims to delete peer's message; peer's edit names another
        // conversation.
        modifiers
            .queue_dm_delete(&other, 100, Some(ChannelId::from(other)), now)
            .unwrap();
        modifiers
            .queue_edit(&peer, 100, Some(ChannelId::from(other)), "x", 150, now)
            .unwrap();
        assert!(messages.insert_or_ignore(&dm(peer, "kept", 100)).unwrap());
        let stored = messages
            .get_by_sender_timestamp(&peer, 100)
            .unwrap()
            .unwrap();
        assert_eq!(stored.content, MessageContent::Text("kept".into()));
    }

    #[test]
    fn pending_modifiers_expire_and_are_bounded() {
        let (_dir, modifiers, messages) = repos();
        let peer = UserId::new();
        let now = 2_000_000_000_000_000_000;
        modifiers.queue_dm_delete(&peer, 1, None, now).unwrap();
        // A later write prunes what has waited past MAX_AGE.
        modifiers
            .queue_dm_delete(&peer, 2, None, now + MAX_AGE_NS)
            .unwrap();
        assert!(messages
            .insert_or_ignore(&dm(peer, "arrived late", 1))
            .unwrap());

        for i in 0..=MAX_PENDING as u64 {
            modifiers
                .queue_dm_delete(&peer, 1_000 + i, None, now + MAX_AGE_NS + 1 + i)
                .unwrap();
        }
        assert!(
            messages
                .insert_or_ignore(&dm(peer, "evicted", 1_000))
                .unwrap(),
            "oldest evicted"
        );
        assert!(!messages
            .insert_or_ignore(&dm(peer, "deleted", 1_001))
            .unwrap());
    }

    /// A `MessageService` on a fresh store, logged in as `me`.
    fn service(me: UserId) -> (tempfile::TempDir, Arc<crate::client::ChatterClient>) {
        use crate::storage::repositories::settings_repo::keys;
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        client
            .messages()
            .settings_repo
            .set(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
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
        assert!(m
            .apply_dm_edit(&peer, sent, "early", sent + 10, sent + 10)
            .unwrap()
            .is_none());
        m.message_repo.insert(&dm(peer, "original", sent)).unwrap();
        let text = |m: &crate::services::message::MessageService| {
            m.message_repo
                .get_by_sender_timestamp(&peer, sent)
                .unwrap()
                .unwrap()
                .content
        };
        assert_eq!(text(m), MessageContent::Text("early".into()));

        let newer = m
            .apply_dm_edit(&peer, sent, "newer", sent + 30, sent + 31)
            .unwrap()
            .expect("applied");
        assert_eq!(
            newer.edited_at,
            Some(chrono::DateTime::from_timestamp_nanos((sent + 30) as i64))
        );
        assert!(
            m.apply_dm_edit(&peer, sent, "older", sent + 20, sent + 40)
                .unwrap()
                .is_none(),
            "older than the one shown"
        );
        assert!(
            m.apply_dm_edit(&peer, sent, "same", sent + 30, sent + 40)
                .unwrap()
                .is_none(),
            "already shown"
        );
        // Signal measures the window on the edit's SERVER timestamp: a sender
        // cannot back-date an edit into it.
        assert!(
            m.apply_dm_edit(&peer, sent, "backdated", sent + 40, sent + window + 1)
                .unwrap()
                .is_none(),
            "past 48h"
        );
        // With no server timestamp the edit's own one stands in.
        assert!(
            m.apply_dm_edit(&peer, sent, "late", sent + window + 1, 0)
                .unwrap()
                .is_none(),
            "past 48h"
        );
        assert_eq!(text(m), MessageContent::Text("newer".into()));
        assert!(m
            .apply_dm_edit(&peer, sent, "in time", sent + window, 0)
            .unwrap()
            .is_some());

        // Someone else's DM cannot name it.
        let stranger = UserId::new();
        assert!(m
            .apply_targeted_dm_delete(&stranger, Some(&stranger.to_string()), sent)
            .unwrap()
            .is_none());
        assert!(m
            .apply_targeted_dm_delete(&peer, Some(&peer.to_string()), sent)
            .unwrap()
            .is_some());
        // Deleted before it is announced, not by a later event subscriber.
        assert!(m
            .message_repo
            .get_by_sender_timestamp(&peer, sent)
            .unwrap()
            .is_none());
    }

    /// What a receive path announces is the row as stored: an early edit
    /// applied, and nothing for a message deleted before it arrived.
    #[test]
    fn a_received_message_is_returned_as_stored() {
        let (_dir, modifiers, messages) = repos();
        let peer = UserId::new();
        let now = 2_000_000_000_000_000_000;
        let scope = Some(ChannelId::from(peer));
        modifiers
            .queue_edit(&peer, 100, scope, "edited", 150, now)
            .unwrap();
        modifiers.queue_dm_delete(&peer, 200, scope, now).unwrap();

        let stored = messages
            .insert_received(&dm(peer, "original", 100))
            .unwrap()
            .expect("stored");
        assert_eq!(stored.content, MessageContent::Text("edited".into()));
        assert!(stored.is_edited);
        assert!(messages
            .insert_received(&dm(peer, "regret", 200))
            .unwrap()
            .is_none());

        let fresh = messages
            .insert_or_ignore_received(&dm(peer, "plain", 300))
            .unwrap()
            .expect("stored");
        assert_eq!(fresh.content, MessageContent::Text("plain".into()));
        assert!(
            messages
                .insert_or_ignore_received(&dm(peer, "plain", 300))
                .unwrap()
                .is_none(),
            "duplicate"
        );
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
        let reaction_scope = ReactionScope::new(dm_with_peer, ConversationType::ServerDm);
        let reactors = |emoji: &str| -> Vec<UserId> {
            m.message_repo
                .get_reactions(&reaction_scope, &peer, 100)
                .unwrap()
                .into_iter()
                .filter(|r| r.emoji.value == emoji)
                .map(|r| r.user_id)
                .collect()
        };

        // The peer's 👍, removed (sent at 20) before the add (sent at 10)
        // reached us: the remove is newer and wins.
        assert!(!m
            .record_inbound_reaction(&reaction_scope, &peer, &peer, 100, "a", true, 20)
            .unwrap());
        assert!(!m
            .record_inbound_reaction(&reaction_scope, &peer, &peer, 100, "a", false, 10)
            .unwrap());
        // Added (30) after a remove (25) that arrived later: the add stands.
        assert!(!m
            .record_inbound_reaction(&reaction_scope, &peer, &peer, 100, "b", false, 30)
            .unwrap());
        assert!(!m
            .record_inbound_reaction(&reaction_scope, &peer, &peer, 100, "b", true, 25)
            .unwrap());
        // Ours, from another of our devices.
        assert!(!m
            .record_inbound_reaction(&reaction_scope, &me, &peer, 100, "c", false, 5)
            .unwrap());
        // A stranger, from their own DM, naming the peer's message.
        assert!(!m
            .record_inbound_reaction(
                &ReactionScope::new(ChannelId::from(stranger), ConversationType::ServerDm),
                &stranger,
                &peer,
                100,
                "d",
                false,
                5
            )
            .unwrap());

        let stored = m
            .message_repo
            .insert_received(&dm(peer, "hi", 100))
            .unwrap()
            .expect("stored");
        assert!(reactors("a").is_empty(), "the later remove wins");
        assert_eq!(reactors("b"), vec![peer]);
        assert_eq!(reactors("c"), vec![me]);
        assert!(reactors("d").is_empty(), "not the stranger's conversation");
        let mut counts: Vec<_> = stored
            .reaction_counts
            .iter()
            .map(|r| (r.emoji.clone(), r.count))
            .collect();
        counts.sort();
        assert_eq!(
            counts,
            vec![("b".to_string(), 1), ("c".to_string(), 1)],
            "the announced row carries them"
        );
        assert_eq!(stored.current_user_reactions, vec!["c".to_string()]);

        // Once the message is here, a reaction applies directly.
        assert!(m
            .record_inbound_reaction(&reaction_scope, &peer, &peer, 100, "e", false, 40)
            .unwrap());
    }

    /// An early pin is applied on arrival — the latest action winning — and a
    /// channel pin only from a member of that channel.
    #[tokio::test]
    async fn an_early_pin_waits_for_its_message() {
        use crate::domain::channel::{
            Channel, ChannelType, MemberRole, Membership, NotificationPreference,
        };
        let me = UserId::new();
        let (_dir, client) = service(me);
        let m = client.messages();
        let (peer, member, outsider, channel) = (
            UserId::new(),
            UserId::new(),
            UserId::new(),
            ChannelId::new(),
        );
        m.channel_repo
            .insert(&Channel::new(channel, "g".into(), ChannelType::Private, me))
            .unwrap();
        for user_id in [peer, member] {
            m.channel_repo
                .insert_membership(&Membership {
                    channel_id: channel,
                    user_id,
                    role: MemberRole::Member,
                    joined_at: chrono::Utc::now(),
                    last_read_message_id: None,
                    is_muted: false,
                    muted_until: None,
                    notification_preference: NotificationPreference::default(),
                })
                .unwrap();
        }
        let in_channel = |sent_at| {
            Message::new_text(
                channel,
                ConversationType::Channel,
                Some(peer),
                "c".into(),
                sent_at,
            )
        };

        // DM: pinned at 10, unpinned at 20 but delivered first.
        let dm_with_peer = ChannelId::from(peer);
        assert!(m
            .resolve_inbound_pin(&dm_with_peer, &peer, &peer, 100, true, 20)
            .unwrap()
            .is_none());
        assert!(m
            .resolve_inbound_pin(&dm_with_peer, &peer, &peer, 100, false, 10)
            .unwrap()
            .is_none());
        assert!(
            !m.message_repo
                .insert_received(&dm(peer, "x", 100))
                .unwrap()
                .unwrap()
                .is_pinned
        );
        assert!(m
            .resolve_inbound_pin(&dm_with_peer, &peer, &peer, 101, false, 10)
            .unwrap()
            .is_none());
        assert!(
            m.message_repo
                .insert_received(&dm(peer, "y", 101))
                .unwrap()
                .unwrap()
                .is_pinned
        );

        // Channel: an outsider's pin is refused when the message arrives; a member's lands.
        assert!(m
            .resolve_inbound_pin(&channel, &outsider, &peer, 200, false, 10)
            .unwrap()
            .is_none());
        assert!(
            !m.message_repo
                .insert_received(&in_channel(200))
                .unwrap()
                .unwrap()
                .is_pinned
        );
        assert!(m
            .resolve_inbound_pin(&channel, &member, &peer, 201, false, 10)
            .unwrap()
            .is_none());
        assert!(
            m.message_repo
                .insert_received(&in_channel(201))
                .unwrap()
                .unwrap()
                .is_pinned
        );
        // A pin naming another conversation than the message's does not apply.
        assert!(m
            .resolve_inbound_pin(&dm_with_peer, &peer, &peer, 202, false, 10)
            .unwrap()
            .is_none());
        assert!(
            !m.message_repo
                .insert_received(&in_channel(202))
                .unwrap()
                .unwrap()
                .is_pinned
        );
    }
}

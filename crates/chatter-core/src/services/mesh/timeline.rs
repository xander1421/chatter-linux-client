//! Where mesh messages land, and how a mesh conversation reaches the sidebar.
//!
//! Two types live here and they are really one mechanism:
//!
//! - [`MeshTimeline`] owns the message-store side of the mesh — persisting rows,
//!   reading history back, and shaping the conversations the unified sidebar
//!   renders.
//! - [`MeshEvents`] is the fan-out every runtime in this module already sends
//!   through, with the timeline wired *in front of* the broadcast channel.
//!
//! # Why persistence sits inside the send, rather than in a subscriber task
//!
//! The obvious alternative is a task that subscribes to [`MeshEvent`]s and
//! writes what it sees. It is also lossy: `tokio::sync::broadcast` drops the
//! oldest events for a subscriber that falls behind, and a crowded room
//! produces exactly the bursts that make a subscriber fall behind. A missed
//! repaint is a glitch; a missed write is a message that never existed. Sending
//! through the store instead makes "the service surfaced a message" and "the
//! message is on disk" the same event, and gives the whole module one
//! persistence call site to read.
//!
//! # No server, here least of all
//!
//! Nothing in this file may reach for a gRPC client, a `SpiceDB` check, or a
//! server-minted identifier. Mesh conversations are client-managed end to end:
//! their IDs are derived in [`mapping`](super::mapping), their membership is
//! whoever is in radio range, and no server has ever heard of them.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::domain::ids::ChannelId;
use crate::domain::message::{ConversationType, Message};
use crate::domain::UnifiedConversation;
use crate::error::StorageError;
use chatter_mesh::ids::PeerId;
use crate::storage::repositories::{
    MeshConversationRepository, MessageRepository, StoredMeshConversation,
};

use super::events::{MeshEvent, MeshMessage};
use super::mapping::MeshConversation;

/// Display name for the public BLE mesh room.
///
/// Not user-configurable: every device on the mesh is in the same single room,
/// so a per-device name would describe nothing.
const ROOM_LABEL: &str = "#mesh";

/// Status value for a live mesh conversation, matching the `1 = active` the
/// unified conversation SQL uses for an unarchived channel. A mesh conversation
/// is never archived — there is no server-side flag to archive it with.
const STATUS_ACTIVE: i32 = 1;

/// One conversation the mesh currently offers, before the message store fills
/// in its counts.
#[derive(Clone, Debug)]
struct Entry {
    conversation: MeshConversation,
    /// What the sidebar calls it.
    ///
    /// For a DM this is the peer's *self-declared* nickname, which no signature
    /// covers on public traffic — a display string, never an identity. It is
    /// only ever taken from a message the peer sent, never from one we sent, or
    /// every DM would end up named after ourselves.
    label: String,
}


/// Storage row -> service type. Unknown kinds are skipped rather than guessed:
/// a row we cannot map is a row from a newer build, and inventing a conversation
/// for it would put something unopenable in the sidebar.
fn conversation_from_stored(row: &StoredMeshConversation) -> Option<MeshConversation> {
    match row.kind.as_str() {
        "room" => Some(MeshConversation::Room),
        "geohash" => row.wire_id.clone().map(MeshConversation::Geohash),
        "dm" => row
            .wire_id
            .as_deref()
            .map(|id| MeshConversation::Dm(PeerId::from_str(id))),
        _ => None,
    }
}

/// Service type -> storage row.
fn stored_from_conversation(
    conversation: &MeshConversation,
    label: &str,
) -> StoredMeshConversation {
    let (kind, wire_id) = match conversation {
        MeshConversation::Room => ("room", None),
        MeshConversation::Geohash(cell) => ("geohash", Some(cell.clone())),
        MeshConversation::Dm(peer) => ("dm", Some(peer.to_string())),
    };
    StoredMeshConversation {
        conversation_id: conversation.conversation_id().to_string(),
        kind: kind.to_string(),
        wire_id,
        label: label.to_string(),
    }
}

/// The mesh's half of the message store.
///
/// Holds the durable side (rows in the unified `messages` table, shared with
/// every other conversation type) and the live side (which mesh conversations
/// currently exist).
pub struct MeshTimeline {
    messages: Arc<MessageRepository>,
    /// Which conversations the sidebar should show.
    ///
    /// Cached in memory and mirrored to `mesh_conversations`.
    ///
    /// The mirror matters because the mapping from a conversation UUID back to
    /// the peer or cell that produced it is one-way (UUID v5 over the wire
    /// identifier), so it cannot be recovered from the stored messages. Without
    /// it a mesh DM's history sat on disk with nothing in the sidebar pointing
    /// at it until that peer was heard from again, which reads as lost data.
    open: Mutex<HashMap<ChannelId, Entry>>,
    /// Durable mirror of `open`.
    directory: Arc<MeshConversationRepository>,
}

impl MeshTimeline {
    /// Wire the timeline to the message store.
    #[must_use]
    pub fn new(
        messages: Arc<MessageRepository>,
        directory: Arc<MeshConversationRepository>,
    ) -> Self {
        let timeline = Self {
            messages,
            open: Mutex::new(HashMap::new()),
            directory,
        };
        // Restore what was open before the restart. A directory read that fails
        // is logged and tolerated: starting with an empty sidebar is a degraded
        // session, whereas refusing to start is a broken one.
        match timeline.directory.list() {
            Ok(stored) => {
                let mut open = timeline.open.lock();
                for row in stored {
                    let Some(conversation) = conversation_from_stored(&row) else {
                        continue;
                    };
                    open.insert(
                        conversation.conversation_id(),
                        Entry {
                            conversation,
                            label: row.label,
                        },
                    );
                }
            }
            Err(error) => {
                tracing::warn!(%error, "mesh: could not restore the conversation directory");
            }
        }
        // The public room is not something you join — a running radio is in it.
        // So it exists from construction rather than from the first message.
        timeline.register(&MeshConversation::Room, None);
        timeline
    }

    /// Persist one mesh message, and remember the conversation it belongs to.
    ///
    /// Returns whether the row was new. Idempotent on message ID, which is what
    /// makes the mesh's redundancy free: the same message legitimately arrives
    /// over the radio, from a courier, and from a relay drop, and
    /// [`wire_message_id`](super::mapping::wire_message_id) maps all three onto
    /// the same row. Writing it three times would turn the redundancy that
    /// makes delivery work into three bubbles in the timeline.
    ///
    /// # This is a deliberate divergence from bitchat
    ///
    /// bitchat's whitepaper states ephemerality as a design goal: no plaintext
    /// message content is ever written to disk, and everything its
    /// store-and-forward stack persists is either sealed ciphertext or
    /// already-public broadcast traffic. The line below writes mesh plaintext to
    /// disk, and does so knowingly:
    ///
    /// - **Selective ephemerality is a half-measure.** Mesh messages evaporating
    ///   while the user's Signal messages sit in the same database offers a
    ///   feeling of safety rather than safety, which is worse than either
    ///   consistent option.
    /// - **A chat that silently forgets its history is indistinguishable from a
    ///   bug** to everyone who has not read the whitepaper.
    /// - **The row lands in SQLCipher under the OS-keyring passphrase**, the
    ///   same protection every other message in this app gets. It is not a
    ///   weaker store; it is the same store.
    ///
    /// The consequence is not optional: a user who reaches for the mesh in the
    /// situations bitchat targets is relying on being able to destroy this, so
    /// panic wipe has to reach these rows. Reversing the decision means
    /// confining mesh messages to an in-memory timeline and losing history
    /// across restarts — not weakening what is written here.
    pub fn record(&self, event: &MeshMessage) -> Result<bool, StorageError> {
        let mut row = event.message.clone();
        // Our own message is read the moment it is composed. Leaving it unread
        // would put an unread badge on the conversation the user just typed in.
        row.is_read = event.is_local;
        // Deliberately left empty. `delivery_status` carries server semantics —
        // "sent" there means a server accepted it — and a mesh message handed to
        // a courier is genuinely sent while no server has ever seen it. Mesh
        // delivery has its own vocabulary in [`MeshDelivery`](super::MeshDelivery)
        // and rides the event stream, rather than being flattened into a field
        // that means something else.
        row.delivery_status.clear();

        // Plaintext, to disk, on purpose. See "This is a deliberate divergence
        // from bitchat" above before changing it.
        let stored = self.messages.insert_or_ignore(&row)?;

        // A message proves the conversation exists. Only a message we received
        // carries a nickname worth labelling a DM with.
        let nickname = (!event.is_local).then(|| event.nickname.as_str());
        self.register(&event.conversation, nickname);

        Ok(stored)
    }

    /// Note that a geohash cell is joined, so it shows up in the sidebar before
    /// anyone has said anything in it.
    pub fn note_geohash(&self, cell: &str) {
        self.register(&MeshConversation::Geohash(cell.to_string()), None);
    }

    /// Drop a geohash cell from the sidebar when it is left.
    ///
    /// Only the sidebar entry goes: the messages stay on disk, and rejoining the
    /// cell brings the conversation back with its history intact.
    pub fn forget_geohash(&self, cell: &str) {
        let conversation = MeshConversation::Geohash(cell.to_string());
        let id = conversation.conversation_id();
        self.open.lock().remove(&id);
        if let Err(error) = self.directory.remove(&id.to_string()) {
            tracing::warn!(%error, "mesh: could not forget a cell durably");
        }
    }

    /// History for one mesh conversation, oldest first — ready to render.
    ///
    /// `before_sent_at` pages backwards through it, in the sender's nanosecond
    /// timestamps, exactly as the other conversation types do.
    pub fn history(
        &self,
        conversation: &MeshConversation,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        self.messages.list_for_conversation_asc(
            &conversation.conversation_id(),
            &conversation.conversation_type(),
            before_sent_at,
            limit,
        )
    }

    /// Mark everything in a mesh conversation read locally.
    ///
    /// Local only: it moves no read receipt onto the wire. Telling a peer their
    /// message was read is a separate act with a separate cost (it puts traffic
    /// on the radio) and lives on
    /// [`MeshService::mark_read`](super::MeshService::mark_read).
    pub fn mark_read(&self, conversation: &MeshConversation) -> Result<(), StorageError> {
        self.messages.mark_conversation_read(
            &conversation.conversation_id(),
            &conversation.conversation_type(),
        )
    }

    /// Every mesh conversation, shaped for the unified sidebar.
    ///
    /// Ordered the way `UnifiedConversationRepository::list_all` orders its own
    /// rows — most recent message first, conversations with no messages last —
    /// so a caller can merge the two lists without re-sorting either.
    pub fn conversations(&self) -> Result<Vec<UnifiedConversation>, StorageError> {
        let entries: Vec<Entry> = self.open.lock().values().cloned().collect();
        let mut conversations = Vec::with_capacity(entries.len());
        for entry in entries {
            conversations.push(self.shape(&entry)?);
        }
        conversations.sort_by(|a, b| {
            b.last_message_at
                .cmp(&a.last_message_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(conversations)
    }

    /// The mesh conversation a sidebar row refers to.
    ///
    /// [`UnifiedConversation::id`] is a string, and the derivation that produced
    /// it is one-way, so the round trip from a rendered row back to something
    /// sendable has to come from here rather than being recomputed. `None` means
    /// the id belongs to some other conversation type, which is the answer a
    /// caller dispatching on a mixed sidebar wants.
    #[must_use]
    pub fn conversation_for(&self, id: &ChannelId) -> Option<MeshConversation> {
        self.open
            .lock()
            .get(id)
            .map(|entry| entry.conversation.clone())
    }

    /// Panic wipe: destroy every mesh message and forget every conversation.
    ///
    /// Deletes by conversation *type*, not by walking the open directory.
    /// Leaving a geohash cell keeps its history by design, so a wipe driven from
    /// the sidebar would silently spare everything said in every cell the user
    /// ever left — and a panic wipe that leaves messages behind is worse than no
    /// panic wipe, because the user believes it worked.
    ///
    /// Both conversation types are cleared even if one fails, and the first
    /// error is returned afterwards: stopping at the first failure would leave
    /// the rest on disk, which is the worst outcome for the one operation whose
    /// entire purpose is leaving nothing behind.
    pub fn wipe(&self) -> Result<(), StorageError> {
        self.open.lock().clear();

        let mut first_error = None;
        for conversation_type in [ConversationType::Mesh, ConversationType::MeshDm] {
            if let Err(error) = self.messages.delete_by_conversation_type(&conversation_type)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Err(error) = self.directory.wipe()
            && first_error.is_none()
        {
            first_error = Some(error);
        }

        // The room is not something you join, so it comes back immediately —
        // an empty room, with nothing in it.
        self.register(&MeshConversation::Room, None);

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Mirror one directory entry to disk.
    ///
    /// Failures are logged, not propagated: the conversation is already open in
    /// memory and usable, and refusing a message because the sidebar could not
    /// be persisted would be the wrong trade.
    fn persist_entry(&self, conversation: &MeshConversation, label: &str) {
        if let Err(error) = self
            .directory
            .upsert(&stored_from_conversation(conversation, label))
        {
            tracing::warn!(%error, "mesh: could not persist a conversation entry");
        }
    }

    /// One conversation, with its counts read from the message store.
    fn shape(&self, entry: &Entry) -> Result<UnifiedConversation, StorageError> {
        let id = entry.conversation.conversation_id();
        let conversation_type: ConversationType = entry.conversation.conversation_type();
        let latest = self
            .messages
            .get_latest_for_conversation(&id, &conversation_type)?;
        let unread = self.messages.get_unread_count(&id, &conversation_type)?;
        let last_message_at: Option<DateTime<Utc>> = latest.map(|message| message.created_at);

        Ok(UnifiedConversation {
            id: id.to_string(),
            conversation_type,
            name: entry.label.clone(),
            description: None,
            // No server ever minted this conversation, so it has no owner and we
            // hold no role in it. Both fields stay empty rather than being
            // filled with the local user, which would claim a server-side
            // relationship that does not exist.
            owner_id: None,
            my_role: String::new(),
            status: STATUS_ACTIVE,
            archived: false,
            last_message_at,
            unread_count: u32::try_from(unread).unwrap_or(u32::MAX),
            // Nothing records when a mesh conversation began: there is no row to
            // hold it and no server to have minted it. The last message is what
            // is available, and both fields only ever break a tie in the sort,
            // which `last_message_at` has already decided.
            created_at: last_message_at.unwrap_or_else(Utc::now),
            updated_at: last_message_at.unwrap_or_else(Utc::now),
        })
    }

    /// Record that a conversation exists, refreshing a DM's display name when a
    /// message from the peer carries a newer one.
    fn register(&self, conversation: &MeshConversation, nickname: Option<&str>) {
        let id = conversation.conversation_id();
        let mut open = self.open.lock();
        match open.get_mut(&id) {
            Some(entry) => {
                if let Some(label) = dm_label(conversation, nickname) {
                    entry.label = label;
                }
            }
            None => {
                open.insert(
                    id,
                    Entry {
                        conversation: conversation.clone(),
                        label: label_for(conversation, nickname),
                    },
                );
            }
        }
        let label = open
            .get(&id)
            .map(|entry| entry.label.clone())
            .unwrap_or_default();
        drop(open);
        self.persist_entry(conversation, &label);
    }
}

/// The sidebar name for a conversation.
fn label_for(conversation: &MeshConversation, nickname: Option<&str>) -> String {
    match conversation {
        MeshConversation::Room => ROOM_LABEL.to_string(),
        // The cell is public alongside every message sent into it, so the user
        // should never have to guess which one they are speaking into.
        MeshConversation::Geohash(cell) => format!("#{cell}"),
        MeshConversation::Dm(peer) => dm_label(conversation, nickname)
            .unwrap_or_else(|| peer.as_str().to_string()),
    }
}

/// A DM's display name from a nickname, when there is one worth using.
fn dm_label(conversation: &MeshConversation, nickname: Option<&str>) -> Option<String> {
    if !matches!(conversation, MeshConversation::Dm(_)) {
        return None;
    }
    nickname
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// The mesh's event fan-out, with the message store in front of it.
///
/// Cloned into every runtime, which sends through it exactly as it would send
/// through the underlying [`broadcast::Sender`] — the persistence is not
/// something a call site has to remember.
#[derive(Clone)]
pub struct MeshEvents {
    events: broadcast::Sender<MeshEvent>,
    timeline: Arc<MeshTimeline>,
}

impl MeshEvents {
    /// A fan-out buffering `capacity` events, storing through `timeline`.
    #[must_use]
    pub fn new(capacity: usize, timeline: Arc<MeshTimeline>) -> Self {
        let (events, _) = broadcast::channel(capacity);
        Self { events, timeline }
    }

    /// Store what needs storing, then tell everyone.
    ///
    /// The store runs first so that a subscriber woken by this event can read
    /// the row back immediately. A failed write is logged and the event still
    /// goes out: the message did arrive, and dropping it from the live timeline
    /// as well would turn a storage problem into a lost message.
    pub fn send(&self, event: MeshEvent) {
        match &event {
            MeshEvent::Message(message) => {
                if let Err(error) = self.timeline.record(message) {
                    tracing::error!(
                        target: "mesh",
                        %error,
                        "a mesh message could not be stored; it is live-only"
                    );
                }
            }
            MeshEvent::GeohashJoined(cell) => self.timeline.note_geohash(cell),
            MeshEvent::GeohashLeft(cell) => self.timeline.forget_geohash(cell),
            _ => {}
        }
        // No subscribers is the normal state before a UI attaches, not an error.
        let _ = self.events.send(event);
    }

    /// Subscribe to the fan-out.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<MeshEvent> {
        self.events.subscribe()
    }
}

/// A timeline over a throwaway SQLCipher database.
///
/// Mirrors the temp-file pattern the repository tests use: SQLCipher's
/// `:memory:` handling needs a passphrase after attach and `Database::open`
/// assumes a real path, so a temp file is as close to in-memory as this gets.
/// The temp dir is leaked so it outlives the test that made it.
/// A throwaway identity store on its own temp database.
///
/// The panic wipe destroys the keypair, so the service needs a real store even
/// in tests — making the field optional to suit a test would put a
/// test-shaped hole in production code.
#[cfg(test)]
pub(crate) fn temp_identity_store() -> Arc<crate::storage::mesh_identity_store::MeshIdentityStore> {
    use crate::storage::Database;

    let dir = tempfile::tempdir().expect("temp dir");
    let db = Arc::new(
        Database::open(&dir.path().join("mesh_identity.db"), "testpass").expect("open database"),
    );
    db.run_migrations().expect("migrations");
    Box::leak(Box::new(dir));
    Arc::new(crate::storage::mesh_identity_store::MeshIdentityStore::new(db))
}

#[cfg(test)]
pub(crate) fn temp_timeline() -> Arc<MeshTimeline> {
    use crate::storage::Database;

    let dir = tempfile::tempdir().expect("temp dir");
    let db = Arc::new(
        Database::open(&dir.path().join("mesh_timeline.db"), "testpass").expect("open database"),
    );
    db.run_migrations().expect("migrations");
    Box::leak(Box::new(dir));
    Arc::new(MeshTimeline::new(
        Arc::new(MessageRepository::new(db.clone())),
        Arc::new(MeshConversationRepository::new(db)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use chatter_mesh::ids::PeerId;

    use crate::domain::message::MessageContent;
    use crate::services::mesh::mapping::{self, InboundMessage};

    fn peer(id: &str) -> PeerId {
        PeerId::from_str(id)
    }

    /// The defect this directory exists to fix.
    ///
    /// A mesh DM's messages are ordinary rows and always survived a restart, but
    /// the sidebar entry pointing at them did not — the conversation UUID is
    /// derived one-way from the peer ID, so it cannot be recovered from the
    /// stored messages. The history was on disk and unreachable, which reads as
    /// lost data.
    #[test]
    fn a_dm_conversation_survives_a_restart() {
        use crate::storage::Database;

        let dir = tempfile::tempdir().expect("temp dir");
        let db = Arc::new(
            Database::open(&dir.path().join("restart.db"), "testpass").expect("open"),
        );
        db.run_migrations().expect("migrations");

        let messages = Arc::new(MessageRepository::new(db.clone()));
        let directory = Arc::new(MeshConversationRepository::new(db.clone()));
        let conversation = MeshConversation::Dm(peer("00112233445566778899aabbccddeeff"));

        {
            let timeline = MeshTimeline::new(messages.clone(), directory.clone());
            timeline.register(&conversation, Some("alice"));
            assert!(
                timeline.conversation_for(&conversation.conversation_id()).is_some(),
                "the DM should be open in the session that created it"
            );
        }

        // A second timeline over the same database is what a restart looks like.
        let restarted = MeshTimeline::new(messages, directory);
        let recovered = restarted
            .conversation_for(&conversation.conversation_id())
            .expect("the DM must still be reachable after a restart");
        assert_eq!(recovered, conversation);

        let listed = restarted.conversations().expect("conversations");
        assert!(
            listed.iter().any(|c| c.name == "alice"),
            "the peer's name should come back too, not just the row"
        );
    }

    /// The case a naive panic wipe misses.
    ///
    /// Leaving a cell keeps its history by design, so a wipe that walks the open
    /// sidebar spares everything said in every cell the user ever left — while
    /// telling them it worked.
    #[test]
    fn a_wipe_destroys_history_from_cells_that_were_already_left() {
        let timeline = temp_timeline();

        timeline.note_geohash("dr5rsj7");
        let cell = MeshConversation::Geohash("dr5rsj7".to_string());
        timeline
            .record(&inbound(
                "wire-cell-1",
                cell.clone(),
                &peer("aabbccddeeff00112233445566778899"),
                "stranger",
                "in the square",
                1_700_000_000_000,
            ))
            .expect("record");
        timeline.forget_geohash("dr5rsj7");

        // Gone from the sidebar, still on disk — that is the design.
        assert!(timeline.conversation_for(&cell.conversation_id()).is_none());
        assert_eq!(
            timeline.history(&cell, None, 50).expect("history").len(),
            1,
            "leaving a cell must keep its history, or this test proves nothing"
        );

        timeline.wipe().expect("wipe");

        assert!(
            timeline.history(&cell, None, 50).expect("history").is_empty(),
            "a panic wipe must reach history the sidebar can no longer see"
        );
    }

    /// A wipe leaves the room present but empty: it is not something you join,
    /// so a running radio is always in it.
    #[test]
    fn a_wipe_empties_everything_and_keeps_only_the_empty_room() {
        let timeline = temp_timeline();
        let room = MeshConversation::Room;
        let dm = MeshConversation::Dm(peer("00112233445566778899aabbccddeeff"));

        timeline
            .record(&inbound(
                "wire-room-1",
                room.clone(),
                &peer("aabbccddeeff00112233445566778899"),
                "stranger",
                "hello room",
                1_700_000_000_000,
            ))
            .expect("record");
        timeline
            .record(&inbound(
                "wire-dm-1",
                dm.clone(),
                &peer("00112233445566778899aabbccddeeff"),
                "alice",
                "hello dm",
                1_700_000_000_001,
            ))
            .expect("record");
        timeline.note_geohash("u4pruy");

        timeline.wipe().expect("wipe");

        assert!(timeline.history(&room, None, 50).expect("history").is_empty());
        assert!(timeline.history(&dm, None, 50).expect("history").is_empty());
        assert!(
            timeline.conversation_for(&dm.conversation_id()).is_none(),
            "the DM must be gone from the directory too"
        );

        let listed = timeline.conversations().expect("conversations");
        assert_eq!(listed.len(), 1, "only the room should remain");
    }

    /// Leaving a cell forgets it durably, rather than only for this session.
    #[test]
    fn a_left_cell_stays_left_across_a_restart() {
        use crate::storage::Database;

        let dir = tempfile::tempdir().expect("temp dir");
        let db = Arc::new(
            Database::open(&dir.path().join("left.db"), "testpass").expect("open"),
        );
        db.run_migrations().expect("migrations");

        let messages = Arc::new(MessageRepository::new(db.clone()));
        let directory = Arc::new(MeshConversationRepository::new(db.clone()));

        {
            let timeline = MeshTimeline::new(messages.clone(), directory.clone());
            timeline.note_geohash("dr5rsj7");
            timeline.forget_geohash("dr5rsj7");
        }

        let restarted = MeshTimeline::new(messages, directory);
        let cell = MeshConversation::Geohash("dr5rsj7".to_string());
        assert!(
            restarted.conversation_for(&cell.conversation_id()).is_none(),
            "a cell that was left must not come back on restart"
        );
    }

    /// One inbound message, as a runtime would surface it.
    fn inbound(
        wire_id: &str,
        conversation: MeshConversation,
        sender: &PeerId,
        nickname: &str,
        content: &str,
        timestamp_ms: u64,
    ) -> MeshMessage {
        let decoded = InboundMessage {
            wire_id: wire_id.to_string(),
            conversation: conversation.clone(),
            sender: sender.clone(),
            nickname: nickname.to_string(),
            content: content.to_string(),
            timestamp_ms,
        };
        MeshMessage {
            message: decoded.to_domain(),
            conversation,
            sender: sender.clone(),
            nickname: nickname.to_string(),
            is_local: false,
            sender_verified: false,
        }
    }

    /// One message this device authored.
    fn outbound(
        wire_id: &str,
        conversation: MeshConversation,
        local: &PeerId,
        content: &str,
        timestamp_ms: u64,
    ) -> MeshMessage {
        MeshMessage {
            message: mapping::outbound_message(
                &conversation,
                wire_id,
                local,
                content,
                timestamp_ms,
            ),
            conversation,
            sender: local.clone(),
            nickname: "me".to_string(),
            is_local: true,
            sender_verified: true,
        }
    }

    fn texts(messages: &[Message]) -> Vec<&str> {
        messages
            .iter()
            .filter_map(|m| match &m.content {
                MessageContent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_inbound_message_is_stored_and_reads_back() {
        let timeline = temp_timeline();
        let sender = peer("abcdef0123456789");
        let message = inbound(
            "MSG-1",
            MeshConversation::Room,
            &sender,
            "wanderer",
            "hello room",
            1_700_000_000_000,
        );

        assert!(timeline.record(&message).expect("stored"));

        let history = timeline
            .history(&MeshConversation::Room, None, 50)
            .expect("history");
        assert_eq!(texts(&history), vec!["hello room"]);
        assert_eq!(history[0].conversation_type, ConversationType::Mesh);
        assert_eq!(
            history[0].sender_id,
            Some(mapping::peer_user_id(&sender)),
            "a mesh sender keeps its derived id; None would mean sealed sender"
        );
        assert!(!history[0].is_read, "someone else's message arrives unread");
    }

    #[test]
    fn the_same_message_arriving_three_ways_is_stored_once() {
        // The mesh delivers redundantly on purpose: over the radio, from a
        // courier, and from a relay drop. Dedup by message ID is what makes that
        // redundancy free.
        let timeline = temp_timeline();
        let sender = peer("abcdef0123456789");
        let message = inbound(
            "MSG-DUP",
            MeshConversation::Dm(sender.clone()),
            &sender,
            "wanderer",
            "are you there",
            1_700_000_000_000,
        );

        assert!(timeline.record(&message).expect("first"));
        assert!(!timeline.record(&message).expect("second"), "already stored");
        assert!(!timeline.record(&message).expect("third"), "still stored");

        let history = timeline
            .history(&MeshConversation::Dm(sender.clone()), None, 50)
            .expect("history");
        assert_eq!(history.len(), 1, "one row, not three bubbles");
    }

    #[test]
    fn a_sent_message_is_readable_immediately_and_counts_as_read() {
        let timeline = temp_timeline();
        let local = peer("0123456789abcdef");
        let sent = outbound(
            "MSG-OUT",
            MeshConversation::Room,
            &local,
            "anyone out there",
            1_700_000_000_000,
        );

        assert!(timeline.record(&sent).expect("stored"));

        let history = timeline
            .history(&MeshConversation::Room, None, 50)
            .expect("history");
        assert_eq!(texts(&history), vec!["anyone out there"]);
        assert!(
            history[0].is_read,
            "our own message must not badge the conversation we just typed in"
        );

        let room = timeline
            .conversations()
            .expect("conversations")
            .into_iter()
            .find(|c| c.conversation_type == ConversationType::Mesh)
            .expect("the room is there");
        assert_eq!(room.unread_count, 0);
        assert!(room.last_message_at.is_some());
    }

    #[test]
    fn the_room_a_cell_and_a_dm_all_surface_with_the_right_type() {
        let timeline = temp_timeline();
        let sender = peer("abcdef0123456789");
        timeline.note_geohash("u4pruy");
        timeline
            .record(&inbound(
                "MSG-DM",
                MeshConversation::Dm(sender.clone()),
                &sender,
                "wanderer",
                "hi",
                1_700_000_000_000,
            ))
            .expect("stored");

        let conversations = timeline.conversations().expect("conversations");
        assert_eq!(conversations.len(), 3, "room, cell, dm");

        let room = conversations
            .iter()
            .find(|c| c.name == "#mesh")
            .expect("the public room");
        assert_eq!(room.conversation_type, ConversationType::Mesh);
        assert_eq!(
            room.id,
            MeshConversation::Room.conversation_id().to_string()
        );

        let cell = conversations
            .iter()
            .find(|c| c.name == "#u4pruy")
            .expect("the joined cell");
        assert_eq!(cell.conversation_type, ConversationType::Mesh);
        assert!(
            cell.last_message_at.is_none(),
            "a joined cell with no traffic is still a conversation"
        );

        let dm = conversations
            .iter()
            .find(|c| c.conversation_type == ConversationType::MeshDm)
            .expect("the dm");
        assert_eq!(dm.name, "wanderer", "named by what the peer calls itself");
        assert_eq!(dm.unread_count, 1);

        // Every one of them answers the serverless predicate, so anything keyed
        // on "the server knows about this conversation" skips all three.
        assert!(conversations
            .iter()
            .all(|c| c.conversation_type.is_mesh()));
    }

    #[test]
    fn a_mesh_conversation_is_never_given_server_side_identifiers() {
        let timeline = temp_timeline();
        let sender = peer("abcdef0123456789");
        timeline
            .record(&inbound(
                "MSG-1",
                MeshConversation::Dm(sender.clone()),
                &sender,
                "wanderer",
                "hi",
                1_700_000_000_000,
            ))
            .expect("stored");

        for conversation in timeline.conversations().expect("conversations") {
            assert!(
                conversation.owner_id.is_none(),
                "no server account owns a mesh conversation"
            );
            assert!(
                conversation.my_role.is_empty(),
                "roles are a server-side authorization concept"
            );
            assert!(!conversation.archived);
            assert_eq!(conversation.status, STATUS_ACTIVE);
        }
    }

    #[test]
    fn a_dm_is_never_labelled_with_our_own_nickname() {
        // The outbound mirror carries *our* nickname. Naming the conversation
        // from it would label every DM after ourselves.
        let timeline = temp_timeline();
        let remote = peer("abcdef0123456789");
        let local = peer("0123456789abcdef");
        // `outbound` carries our own nickname, "me", exactly as the send path does.
        let sent = outbound(
            "MSG-OUT",
            MeshConversation::Dm(remote.clone()),
            &local,
            "first contact",
            1_700_000_000_000,
        );
        timeline.record(&sent).expect("stored");

        let dm = timeline
            .conversations()
            .expect("conversations")
            .into_iter()
            .find(|c| c.conversation_type == ConversationType::MeshDm)
            .expect("the dm");
        assert_eq!(dm.name, remote.as_str(), "the peer, until they name themselves");

        // Their reply names it.
        timeline
            .record(&inbound(
                "MSG-IN",
                MeshConversation::Dm(remote.clone()),
                &remote,
                "wanderer",
                "hello",
                1_700_000_000_001,
            ))
            .expect("stored");
        let dm = timeline
            .conversations()
            .expect("conversations")
            .into_iter()
            .find(|c| c.conversation_type == ConversationType::MeshDm)
            .expect("the dm");
        assert_eq!(dm.name, "wanderer");
    }

    #[test]
    fn leaving_a_cell_takes_it_off_the_sidebar_and_leaves_its_history_alone() {
        let timeline = temp_timeline();
        let sender = peer("abcdef0123456789");
        let cell = MeshConversation::Geohash("u4pruy".to_string());
        timeline.note_geohash("u4pruy");
        timeline
            .record(&inbound(
                "MSG-GEO",
                cell.clone(),
                &sender,
                "wanderer",
                "anyone in this cell",
                1_700_000_000_000,
            ))
            .expect("stored");

        timeline.forget_geohash("u4pruy");
        let conversations = timeline.conversations().expect("conversations");
        assert!(
            !conversations.iter().any(|c| c.name == "#u4pruy"),
            "a cell you left is not a conversation you are in"
        );

        let history = timeline.history(&cell, None, 50).expect("history");
        assert_eq!(
            texts(&history),
            vec!["anyone in this cell"],
            "leaving a cell is not deleting what was said in it"
        );
    }

    #[test]
    fn a_sidebar_row_maps_back_to_something_sendable() {
        let timeline = temp_timeline();
        let sender = peer("abcdef0123456789");
        timeline
            .record(&inbound(
                "MSG-1",
                MeshConversation::Dm(sender.clone()),
                &sender,
                "wanderer",
                "hi",
                1_700_000_000_000,
            ))
            .expect("stored");

        let dm = timeline
            .conversations()
            .expect("conversations")
            .into_iter()
            .find(|c| c.conversation_type == ConversationType::MeshDm)
            .expect("the dm");
        let id: ChannelId = dm.id.parse().expect("the row id is a uuid");

        assert_eq!(
            timeline.conversation_for(&id),
            Some(MeshConversation::Dm(sender)),
            "clicking the row has to yield something a send can address"
        );
        assert!(
            timeline.conversation_for(&ChannelId::new()).is_none(),
            "an id from some other conversation type is not ours to answer for"
        );
    }

    #[test]
    fn reading_a_conversation_clears_its_unread_count() {
        let timeline = temp_timeline();
        let sender = peer("abcdef0123456789");
        let dm = MeshConversation::Dm(sender.clone());
        for (index, wire_id) in ["MSG-1", "MSG-2"].iter().enumerate() {
            timeline
                .record(&inbound(
                    wire_id,
                    dm.clone(),
                    &sender,
                    "wanderer",
                    "hi",
                    1_700_000_000_000 + index as u64,
                ))
                .expect("stored");
        }

        let unread = timeline
            .conversations()
            .expect("conversations")
            .into_iter()
            .find(|c| c.conversation_type == ConversationType::MeshDm)
            .expect("the dm")
            .unread_count;
        assert_eq!(unread, 2);

        timeline.mark_read(&dm).expect("mark read");
        let read = timeline
            .conversations()
            .expect("conversations")
            .into_iter()
            .find(|c| c.conversation_type == ConversationType::MeshDm)
            .expect("the dm")
            .unread_count;
        assert_eq!(read, 0);
    }

    #[test]
    fn conversations_are_ordered_most_recent_first_with_silent_ones_last() {
        let timeline = temp_timeline();
        let sender = peer("abcdef0123456789");
        timeline.note_geohash("u4pruy");
        timeline
            .record(&inbound(
                "MSG-OLD",
                MeshConversation::Room,
                &sender,
                "wanderer",
                "old",
                1_700_000_000_000,
            ))
            .expect("stored");
        timeline
            .record(&inbound(
                "MSG-NEW",
                MeshConversation::Dm(sender.clone()),
                &sender,
                "wanderer",
                "new",
                1_700_000_100_000,
            ))
            .expect("stored");

        let names: Vec<String> = timeline
            .conversations()
            .expect("conversations")
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["wanderer", "#mesh", "#u4pruy"]);
    }

    #[test]
    fn a_message_still_reaches_subscribers_and_the_store_through_the_fan_out() {
        let events = MeshEvents::new(8, temp_timeline());
        let mut subscriber = events.subscribe();
        let sender = peer("abcdef0123456789");

        events.send(MeshEvent::Message(Box::new(inbound(
            "MSG-1",
            MeshConversation::Room,
            &sender,
            "wanderer",
            "hello room",
            1_700_000_000_000,
        ))));

        match subscriber.try_recv() {
            Ok(MeshEvent::Message(message)) => {
                assert_eq!(mapping::text_of(&message.message), Some("hello room"));
            }
            other => panic!("expected the message, got {other:?}"),
        }
    }

    #[test]
    fn joining_and_leaving_a_cell_through_the_fan_out_moves_the_sidebar() {
        let timeline = temp_timeline();
        let events = MeshEvents::new(8, Arc::clone(&timeline));

        events.send(MeshEvent::GeohashJoined("u4pruy".to_string()));
        assert!(timeline
            .conversations()
            .expect("conversations")
            .iter()
            .any(|c| c.name == "#u4pruy"));

        events.send(MeshEvent::GeohashLeft("u4pruy".to_string()));
        assert!(!timeline
            .conversations()
            .expect("conversations")
            .iter()
            .any(|c| c.name == "#u4pruy"));
    }
}

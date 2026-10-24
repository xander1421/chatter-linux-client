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

use crate::crypto::attachment_crypto::AttachmentCrypto;
use crate::domain::UnifiedConversation;
use crate::domain::attachment::{Attachment, AttachmentStatus, ContentLocation, FileMetadata};
use crate::domain::ids::{AttachmentId, ChannelId, UserId};
use crate::domain::message::{ConversationType, Message};
use crate::error::StorageError;
use crate::services::attachment::{attachment_file_path, attachments_dir, ensure_attachments_dir};
use crate::storage::repositories::{
    AttachmentRepository, MeshConversationRepository, MeshDeliveryRepository, MessageRepository,
    StoredMeshConversation,
};
use chatter_mesh::ids::PeerId;

use super::error::MeshError;
use super::events::{MeshDelivery, MeshEvent, MeshMessage};
use super::mapping::MeshConversation;

/// Display name for the public BLE mesh room.
///
/// Not user-configurable: every device on the mesh is in the same single room,
/// so a per-device name would describe nothing.
const ROOM_LABEL: &str = "#mesh";

/// What a file with no usable name on the wire is called.
///
/// Both metadata TLVs are optional in `BitchatFilePacket`, and a nameless row
/// renders as a blank line rather than as a file.
const UNNAMED_FILE: &str = "mesh-file";

/// What a file with no MIME type on the wire is called.
///
/// `FileCategory::from_mime_and_filename` falls back to the extension, so an
/// unhelpful type here still categorises correctly when the name is present.
const DEFAULT_MIME: &str = "application/octet-stream";

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
        "channel" => row.wire_id.clone().map(MeshConversation::Channel),
        "private-channel" => row.wire_id.clone().map(MeshConversation::PrivateChannel),
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
        MeshConversation::Channel(name) => ("channel", Some(name.clone())),
        MeshConversation::PrivateChannel(id) => ("private-channel", Some(id.clone())),
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
    nearby_channels: Mutex<std::collections::BTreeMap<String, std::time::Instant>>,
    /// Durable mirror of `open`.
    directory: Arc<MeshConversationRepository>,
    /// Where each message this device sent has got to, in the mesh's own
    /// vocabulary. Separate from `messages.delivery_status`, which means
    /// something else — see V060.
    delivery: Arc<MeshDeliveryRepository>,
    /// Files carried by mesh messages. The same table every other conversation
    /// type's attachments live in — a mesh file is not a second kind of
    /// attachment, it just never had a CDN.
    attachments: Arc<AttachmentRepository>,
}

impl MeshTimeline {
    /// Wire the timeline to the message store.
    #[must_use]
    pub fn new(
        messages: Arc<MessageRepository>,
        directory: Arc<MeshConversationRepository>,
        delivery: Arc<MeshDeliveryRepository>,
        attachments: Arc<AttachmentRepository>,
    ) -> Self {
        let timeline = Self {
            messages,
            open: Mutex::new(HashMap::new()),
            nearby_channels: Mutex::new(std::collections::BTreeMap::new()),
            directory,
            delivery,
            attachments,
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
}

impl MeshTimeline {
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
                if let Some(label) = dm_label(conversation, nickname).or_else(|| {
                    matches!(conversation, MeshConversation::PrivateChannel(_))
                        .then(|| nickname.map(str::to_string))
                        .flatten()
                }) {
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

/// The display name for a file, from whatever the sender put on the wire.
///
/// Only the final path component survives, and `.`/`..` do not. The bytes
/// themselves land at a path derived from the attachment id, so a hostile name
/// cannot steer the write — but the name *is* rendered and passed on to
/// whatever the user saves it as, and handing `../../.bashrc` to that is not a
/// question worth leaving open one layer further out.
fn display_name(file_name: Option<&str>) -> String {
    file_name
        .and_then(|name| name.rsplit(['/', '\\']).next())
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .unwrap_or(UNNAMED_FILE)
        .to_string()
}

/// The sidebar name for a conversation.
fn label_for(conversation: &MeshConversation, nickname: Option<&str>) -> String {
    match conversation {
        MeshConversation::Room => ROOM_LABEL.to_string(),
        // The cell is public alongside every message sent into it, so the user
        // should never have to guess which one they are speaking into.
        MeshConversation::Geohash(cell) => format!("#{cell}"),
        MeshConversation::Channel(name) => format!("#{name}"),
        MeshConversation::PrivateChannel(_) => {
            nickname.unwrap_or("Private channel").to_string()
        }
        MeshConversation::Dm(peer) => {
            dm_label(conversation, nickname).unwrap_or_else(|| peer.as_str().to_string())
        }
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

mod directory;
mod event_bus;
mod persistence;
mod queries;
mod wipe;
pub use event_bus::MeshEvents;

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
    crate::test_tempdir::hold(dir);
    Arc::new(crate::storage::mesh_identity_store::MeshIdentityStore::new(
        db,
    ))
}

#[cfg(test)]
pub(crate) fn temp_timeline() -> Arc<MeshTimeline> {
    use crate::storage::Database;

    let dir = tempfile::tempdir().expect("temp dir");
    let db = Arc::new(
        Database::open(&dir.path().join("mesh_timeline.db"), "testpass").expect("open database"),
    );
    db.run_migrations().expect("migrations");
    crate::test_tempdir::hold(dir);
    Arc::new(MeshTimeline::new(
        Arc::new(MessageRepository::new(db.clone())),
        Arc::new(MeshConversationRepository::new(db.clone())),
        Arc::new(MeshDeliveryRepository::new(db.clone())),
        Arc::new(AttachmentRepository::new(db)),
    ))
}

#[cfg(test)]
mod tests;

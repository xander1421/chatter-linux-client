//! The only place `chatter-ui` reaches for the mesh.
//!
//! Every mesh call the UI makes goes through this file, so the boundary
//! between the view and `chatter-core` is one screenful rather than scattered
//! across the feature. Nothing here decides anything: it forwards calls and
//! renames core's values into the shapes the Slint models want.
//!
//! # What this expects from `chatter-core`
//!
//! `MeshService` is `pub(crate)` on `ChatterClient` (see `service_accessor!`),
//! so the UI cannot and must not hold it. The methods below are the façade the
//! client is expected to expose — all of them one-line forwards to methods
//! `MeshService` already has, except `mesh_send`, which is the one genuinely
//! new decision and belongs in core precisely because it is one:
//!
//! ```ignore
//! #[cfg(feature = "mesh")]
//! impl ChatterClient {
//!     // Straight forwards to MeshService.
//!     pub fn mesh_subscribe(&self) -> broadcast::Receiver<MeshEvent>;
//!     pub fn mesh_status(&self) -> MeshStatus;
//!     pub fn mesh_peers(&self) -> Vec<MeshPeer>;
//!     pub fn mesh_conversations(&self) -> Result<Vec<UnifiedConversation>, MeshError>;
//!     pub fn mesh_conversation_for(&self, id: &str) -> Option<MeshConversation>;
//!     pub fn mesh_history(&self, conversation: &MeshConversation, before: Option<u64>, limit: u32)
//!         -> Result<Vec<Message>, MeshError>;
//!     pub fn mesh_mark_conversation_read(&self, conversation: &MeshConversation)
//!         -> Result<(), MeshError>;
//!     pub fn mesh_join_geohash(&self, cell: &str) -> Result<(), MeshError>;
//!     pub fn mesh_leave_geohash(&self, cell: &str) -> Result<(), MeshError>;
//!
//!     /// `mapping::peer_user_id(&mesh.local_peer())` — the derived account id
//!     /// our own mesh messages carry.
//!     pub fn mesh_local_user_id(&self) -> Option<UserId>;
//!
//!     /// Picks the transport from the scope: `send_room` / `send_geohash` /
//!     /// `send_dm`. The UI must not make that choice.
//!     pub async fn mesh_send(&self, conversation: &MeshConversation, text: &str)
//!         -> Result<MeshDelivery, MeshError>;
//! }
//! ```
//!
//! Ordering, titling and unread counts are core's calls: the view renders
//! `Vec<MeshRow>` in the order it is handed.

use std::sync::Arc;

use tokio::sync::broadcast::Receiver;

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;
use chatter_core::domain::message::Message;
use chatter_core::services::mesh::{
    MeshConversation, MeshDelivery, MeshError, MeshEvent, MeshPeer, MeshStatus,
};

/// One sidebar row, as the Slint model wants it.
#[derive(Clone, Debug)]
pub struct MeshRow {
    /// The scope itself. Carried through so selection and sending never have to
    /// reverse a derived conversation id back into a transport scope — that
    /// derivation is one-way, and core owns the lookup.
    pub conversation: MeshConversation,
    /// The conversation id, stringified: the key Slint hands back on click.
    pub id: String,
    /// "room" | "geohash" | "dm".
    pub kind: &'static str,
    /// The geohash cell, or the peer id, or empty for the room. Published
    /// alongside every message in this scope, so it is shown verbatim.
    pub scope: String,
    pub title: String,
    pub timestamp: String,
    pub unread_count: i32,
    /// DM rows only: some transport can carry to this peer right now. False is
    /// not an error — it means what you send is held and offered to couriers.
    pub reachable: bool,
    /// DM rows only: the peer's announce signature verified. Nicknames on
    /// public traffic are unauthenticated and never set this.
    pub verified: bool,
}

/// Which glyph and which wording a scope gets. Pure presentation over a core
/// enum — no facts are decided here.
#[must_use]
pub fn kind_of(conversation: &MeshConversation) -> &'static str {
    match conversation {
        MeshConversation::Room => "room",
        MeshConversation::Geohash(_) => "geohash",
        MeshConversation::Dm(_) => "dm",
    }
}

/// The scope string the user must be able to read at all times.
#[must_use]
pub fn scope_of(conversation: &MeshConversation) -> String {
    match conversation {
        MeshConversation::Room => String::new(),
        MeshConversation::Geohash(cell) => cell.clone(),
        MeshConversation::Dm(peer) => peer.as_str().to_string(),
    }
}

/// What a message this device sent should say about its own fate.
///
/// The three mesh-only strings ("waiting", "carried", "undelivered") render as
/// words rather than ticks in `message_item.slint`, because over couriers
/// "sent" and "delivered" can be hours apart and two ticks would claim an
/// arrival nobody has confirmed.
#[must_use]
pub fn delivery_label(state: MeshDelivery) -> &'static str {
    match state {
        // Handed to a live link. It has left this device.
        MeshDelivery::Secure | MeshDelivery::Insecure | MeshDelivery::Reachable => "sent",
        // Nothing could carry it; it is retained and offered to couriers.
        MeshDelivery::Unreachable => "waiting",
        // A courier or a relay drop took a sealed copy.
        MeshDelivery::Carried => "carried",
        MeshDelivery::Delivered => "delivered",
        MeshDelivery::Read => "read",
        MeshDelivery::Dropped => "undelivered",
        // A broadcast has no recipient to acknowledge it, so there is no state
        // beyond "it went out", and no later state to be upgraded to. Even
        // "sent" is borrowed from a one-to-one vocabulary that does not apply:
        // nobody ever confirms a room or cell message.
        MeshDelivery::Broadcast => "broadcast",
    }
}

/// Subscribe to everything the mesh reports.
pub fn subscribe(ui_ctx: &Arc<ChatterClient>) -> Receiver<MeshEvent> {
    ui_ctx.mesh_subscribe()
}

/// Transport state: radio up, peers in range, relays connected.
pub fn status(ui_ctx: &Arc<ChatterClient>) -> MeshStatus {
    ui_ctx.mesh_status()
}

/// Whether the service itself is up, which is what decides whether there are
/// mesh conversations to open. Separate from [`status`] on purpose — see
/// `MeshService::is_running`.
pub fn service_running(ui_ctx: &Arc<ChatterClient>) -> bool {
    ui_ctx.mesh_running()
}

/// Every mesh conversation, in the order core wants them shown.
///
/// A row whose id core cannot resolve back to a scope is skipped rather than
/// guessed at: it would be unopenable, and a sidebar entry that does nothing is
/// worse than no entry.
pub fn rows(ui_ctx: &Arc<ChatterClient>) -> Vec<MeshRow> {
    let conversations = match ui_ctx.mesh_conversations() {
        Ok(conversations) => conversations,
        Err(error) => {
            tracing::warn!(%error, "mesh: could not list conversations");
            return Vec::new();
        }
    };
    let peers: Vec<MeshPeer> = ui_ctx.mesh_peers();

    conversations
        .into_iter()
        .filter_map(|conversation| {
            let scope = ui_ctx.mesh_conversation_for(&conversation.id)?;
            // `MeshPeer` is handed to the UI whole, by design ("everything here
            // is already shaped"), so a DM row reads its peer's own flags
            // rather than inferring anything about them.
            let (reachable, verified) = match &scope {
                MeshConversation::Dm(peer) => peers
                    .iter()
                    .find(|known| known.peer == *peer)
                    .map(|known| (known.connected || known.reachable, known.verified))
                    .unwrap_or((false, false)),
                _ => (false, false),
            };
            Some(MeshRow {
                id: conversation.id,
                kind: kind_of(&scope),
                scope: scope_of(&scope),
                title: conversation.name,
                timestamp: conversation
                    .last_message_at
                    .map(chatter_core::ui::format_timestamp)
                    .unwrap_or_default(),
                unread_count: i32::try_from(conversation.unread_count).unwrap_or(i32::MAX),
                reachable,
                verified,
                conversation: scope,
            })
        })
        .collect()
}

/// History for one mesh conversation, oldest first.
pub fn history(
    ui_ctx: &Arc<ChatterClient>,
    conversation: &MeshConversation,
    limit: u32,
) -> Result<Vec<Message>, MeshError> {
    ui_ctx.mesh_history(conversation, None, limit)
}

/// This device's mesh account id — the derived `UserId` that mesh messages we
/// authored carry. Not the chatter user id: the mesh identity is a separate
/// keypair by design, so "is this mine?" is answered against this instead.
pub fn local_user_id(ui_ctx: &Arc<ChatterClient>) -> Option<UserId> {
    Some(ui_ctx.mesh_local_user_id())
}

/// Send into a mesh scope. Core picks the transport — room, cell or peer —
/// which is exactly the decision the UI must not be making.
pub async fn send(
    ui_ctx: &Arc<ChatterClient>,
    conversation: &MeshConversation,
    text: &str,
) -> Result<MeshDelivery, MeshError> {
    ui_ctx.mesh_send(conversation, text).await
}

/// Join a geohash cell. Whether the string is a cell at all is core's call.
pub fn join(ui_ctx: &Arc<ChatterClient>, cell: &str) -> Result<(), MeshError> {
    ui_ctx.mesh_join_geohash(cell)
}

/// Leave a geohash cell.
pub fn leave(ui_ctx: &Arc<ChatterClient>, cell: &str) -> Result<(), MeshError> {
    ui_ctx.mesh_leave_geohash(cell)
}

/// Mark a mesh conversation read on this device.
///
/// Local only, and deliberately so: telling the peer costs airtime, and
/// `MeshService::mark_read` is the separate act that puts a receipt on the
/// wire.
pub fn mark_read(
    ui_ctx: &Arc<ChatterClient>,
    conversation: &MeshConversation,
) -> Result<(), MeshError> {
    ui_ctx.mesh_mark_conversation_read(conversation)
}

/// How a mesh message's author should be named.
///
/// The generic `message_sender_display_name` cannot answer this — a mesh peer
/// exists in no user repository, so that path ends at `user_id.to_string()` and
/// history rendered every sender as a raw UUID. Core matches the id back to a
/// known peer; when it knows none, the fallback is a word, not an identifier.
pub fn sender_display_name(ui_ctx: &Arc<ChatterClient>, message: &Message) -> String {
    message
        .sender_id
        .as_ref()
        .and_then(|id| ui_ctx.mesh_sender_display_name(id))
        // A peer that has drifted out of range since it spoke. Its id is a hash
        // of a routing address and means nothing to anyone, so showing it is
        // strictly worse than admitting we no longer know who this was.
        .unwrap_or_else(|| "Unknown peer".to_string())
}

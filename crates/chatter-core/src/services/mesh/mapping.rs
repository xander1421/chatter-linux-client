//! Wire traffic → [`Message`] domain objects.
//!
//! The mesh has no server, so nothing hands us a `ChannelId` or a `UserId`. Both
//! are derived here, deterministically, from identifiers the wire already
//! carries. Determinism is the whole requirement: the same geohash cell, the
//! same mesh peer, and the same wire message ID must map to the same UUIDs on
//! every launch and on every device, or the timeline forks.
//!
//! UUID v5 over the nil namespace is the derivation, matching
//! [`ChannelId::from_dm_participants`] and [`MessageId::from_timestamp`], which
//! already do exactly this for the server-side flavours.
//!
//! # Why `sender_id` is a derived UUID and not `None`
//!
//! `Message::sender_id` is `Option<UserId>`, and `None` means "sealed sender —
//! the sender is genuinely unknown". A mesh sender is *known*; it just is not a
//! chatter account. Using `None` would make mesh messages indistinguishable
//! from sealed-sender ones and would collapse every mesh sender into a single
//! anonymous bucket in any grouping the UI does. So each mesh peer gets a
//! stable derived `UserId`, and the human-readable side (nickname, peer ID)
//! rides alongside on [`MeshMessage`](super::events::MeshMessage).

use chatter_mesh::ids::PeerId;
use uuid::Uuid;

use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent};

use super::events::MeshDelivery;

/// Namespace prefix for the single public BLE mesh room.
const ROOM_KEY: &str = "chatter-mesh:room";
/// Namespace prefix for a geohash channel conversation.
const GEOHASH_KEY: &str = "chatter-mesh:geohash:";
/// Namespace prefix for a 1:1 mesh conversation.
const DM_KEY: &str = "chatter-mesh:dm:";
/// Namespace prefix for a mesh peer's derived account ID.
const PEER_KEY: &str = "chatter-mesh:peer:";
/// Namespace prefix for a wire message ID.
const MESSAGE_KEY: &str = "chatter-mesh:msg:";

fn derive(input: &str) -> Uuid {
    Uuid::new_v5(&Uuid::nil(), input.as_bytes())
}

/// Which mesh conversation a message belongs to.
///
/// This is the scope the design doc keeps out of `ConversationType`: the enum
/// there has two arms because the *storage* discriminator only needs two, while
/// the transport scope lives in the identifiers. Making that scope explicit here
/// means the UI never has to re-derive it by sniffing `PeerId` prefixes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MeshConversation {
    /// The public BLE mesh room — everyone in radio range.
    Room,
    /// A public geohash channel, addressed by its cell.
    Geohash(String),
    /// A 1:1 conversation with one peer, over Noise or over a gift wrap.
    Dm(PeerId),
}

impl MeshConversation {
    /// The stable conversation UUID this scope maps to.
    #[must_use]
    pub fn conversation_id(&self) -> ChannelId {
        match self {
            Self::Room => ChannelId::from_uuid(derive(ROOM_KEY)),
            Self::Geohash(cell) => {
                ChannelId::from_uuid(derive(&format!("{GEOHASH_KEY}{cell}")))
            }
            Self::Dm(peer) => {
                ChannelId::from_uuid(derive(&format!("{DM_KEY}{}", peer.as_str())))
            }
        }
    }

    /// Which `ConversationType` this scope stores as.
    #[must_use]
    pub fn conversation_type(&self) -> ConversationType {
        match self {
            Self::Room | Self::Geohash(_) => ConversationType::Mesh,
            Self::Dm(_) => ConversationType::MeshDm,
        }
    }

    /// Whether this scope is public (broadcast) rather than 1:1.
    #[must_use]
    pub fn is_public(&self) -> bool {
        matches!(self, Self::Room | Self::Geohash(_))
    }

    /// The geohash cell, when this scope has one.
    #[must_use]
    pub fn geohash(&self) -> Option<&str> {
        match self {
            Self::Geohash(cell) => Some(cell.as_str()),
            _ => None,
        }
    }

    /// The remote peer, when this scope has exactly one.
    #[must_use]
    pub fn peer(&self) -> Option<&PeerId> {
        match self {
            Self::Dm(peer) => Some(peer),
            _ => None,
        }
    }
}

/// The stable derived account ID for a mesh peer.
#[must_use]
pub fn peer_user_id(peer: &PeerId) -> UserId {
    UserId::from_uuid(derive(&format!("{PEER_KEY}{}", peer.as_str())))
}

/// The stable derived message ID for a wire message ID.
///
/// bitchat message IDs are real, globally-unique strings carried in the payload,
/// so this is a straight renaming rather than a synthetic identity — a resend of
/// the same message maps onto the same row, which is exactly the dedup the
/// store-and-forward layer relies on.
#[must_use]
pub fn wire_message_id(wire_id: &str) -> MessageId {
    MessageId::from_uuid(derive(&format!("{MESSAGE_KEY}{wire_id}")))
}

/// Wire timestamps are milliseconds; [`Message::sent_at`] is nanoseconds.
///
/// Saturating rather than wrapping: a peer with a broken clock announcing a
/// timestamp near `u64::MAX` must not wrap around into 1970 and sort to the top
/// of the timeline.
#[must_use]
pub fn millis_to_nanos(timestamp_ms: u64) -> u64 {
    timestamp_ms.saturating_mul(1_000_000)
}

/// One decoded mesh message, before it becomes a [`Message`].
///
/// Deliberately not `Message` itself: the nickname and the peer ID have nowhere
/// to live on the domain type, and the mapping needs both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundMessage {
    /// The wire message ID — used verbatim for dedup and for acks.
    pub wire_id: String,
    /// Which conversation this lands in.
    pub conversation: MeshConversation,
    /// Who sent it.
    pub sender: PeerId,
    /// Their self-declared nickname. Unauthenticated for public traffic.
    pub nickname: String,
    /// The message body.
    pub content: String,
    /// Sender's clock, milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
}

impl InboundMessage {
    /// Build the domain [`Message`] this maps to.
    #[must_use]
    pub fn to_domain(&self) -> Message {
        let mut message = Message::new_text(
            self.conversation.conversation_id(),
            self.conversation.conversation_type(),
            Some(peer_user_id(&self.sender)),
            self.content.clone(),
            millis_to_nanos(self.timestamp_ms),
        );
        // `new_text` mints a random id. Mesh messages carry a real one, and
        // overwriting it here is what makes a courier resend land on the row it
        // already created rather than beside it.
        message.id = wire_message_id(&self.wire_id);
        message
    }
}

/// An outbound message we authored, mapped into the timeline immediately.
///
/// The mesh has no server round-trip to echo the message back, so the local
/// copy *is* the message. It is created here rather than in the UI so that the
/// local and remote paths produce byte-identical rows.
#[must_use]
pub fn outbound_message(
    conversation: &MeshConversation,
    wire_id: &str,
    sender: &PeerId,
    content: &str,
    timestamp_ms: u64,
) -> Message {
    let mut message = Message::new_text(
        conversation.conversation_id(),
        conversation.conversation_type(),
        Some(peer_user_id(sender)),
        content.to_string(),
        millis_to_nanos(timestamp_ms),
    );
    message.id = wire_message_id(wire_id);

    // A broadcast's delivery state is known the instant it is composed and never
    // changes: nobody acknowledges a room or cell message, so there is no later
    // event to correct it with. Setting it here is what stops the live row from
    // falling through to the converter's "own message with no status means sent"
    // default — which put the one-to-one word "sent" on a broadcast, and only
    // until the conversation was reopened, at which point the durable state (V060)
    // said "broadcast" instead. Two labels for one message, minutes apart.
    //
    // A DM is deliberately left blank: its outcome is genuinely unknown at this
    // point and arrives moments later as a `MeshEvent::Delivery`.
    //
    // This is display state. `MeshTimeline::record` still clears the column before
    // storing, because there it would mean something else — see V060.
    if matches!(
        conversation,
        MeshConversation::Room | MeshConversation::Geohash(_)
    ) {
        message.delivery_status = MeshDelivery::Broadcast.as_str().to_string();
    }

    message
}

/// Extract the plain text of a mapped message, for callers that only want the
/// body back out.
#[must_use]
pub fn text_of(message: &Message) -> Option<&str> {
    match &message.content {
        MessageContent::Text(text) => Some(text.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(s: &str) -> PeerId {
        PeerId::from_str(s)
    }

    #[test]
    fn conversation_ids_are_stable_and_distinct() {
        let room = MeshConversation::Room.conversation_id();
        let a = MeshConversation::Geohash("u4pruy".into()).conversation_id();
        let b = MeshConversation::Geohash("u4pruz".into()).conversation_id();
        let dm = MeshConversation::Dm(peer("abcdef0123456789")).conversation_id();

        assert_eq!(room, MeshConversation::Room.conversation_id());
        assert_eq!(a, MeshConversation::Geohash("u4pruy".into()).conversation_id());
        assert_ne!(a, b);
        assert_ne!(room, a);
        assert_ne!(dm, room);
    }

    #[test]
    fn public_scopes_are_mesh_and_dms_are_mesh_dm() {
        assert_eq!(
            MeshConversation::Room.conversation_type(),
            ConversationType::Mesh
        );
        assert_eq!(
            MeshConversation::Geohash("u4".into()).conversation_type(),
            ConversationType::Mesh
        );
        assert_eq!(
            MeshConversation::Dm(peer("abcdef0123456789")).conversation_type(),
            ConversationType::MeshDm
        );
        // Both arms must answer the serverless predicate, or anything keyed on
        // "the server knows about this" will try an RPC for a mesh row.
        assert!(MeshConversation::Room.conversation_type().is_mesh());
        assert!(
            MeshConversation::Dm(peer("abcdef0123456789"))
                .conversation_type()
                .is_mesh()
        );
    }

    #[test]
    fn mesh_conversations_are_not_anon_conversations() {
        // is_anon() gates the "replies carry a synthetic parent id" behaviour.
        // Mesh replies reference real IDs, so claiming anon would skip lookups
        // that work.
        assert!(!MeshConversation::Room.conversation_type().is_anon());
        assert!(
            !MeshConversation::Dm(peer("abcdef0123456789"))
                .conversation_type()
                .is_anon()
        );
    }

    #[test]
    fn geohash_and_room_are_different_conversations_for_the_same_string() {
        // A cell literally named "room" must not collide with the mesh room.
        assert_ne!(
            MeshConversation::Geohash("room".into()).conversation_id(),
            MeshConversation::Room.conversation_id()
        );
    }

    #[test]
    fn distinct_peers_get_distinct_user_ids() {
        let a = peer_user_id(&peer("abcdef0123456789"));
        let b = peer_user_id(&peer("0123456789abcdef"));
        assert_ne!(a, b);
        assert_eq!(a, peer_user_id(&peer("abcdef0123456789")));
    }

    #[test]
    fn a_resend_maps_onto_the_same_row() {
        let inbound = InboundMessage {
            wire_id: "MSG-1".into(),
            conversation: MeshConversation::Room,
            sender: peer("abcdef0123456789"),
            nickname: "anon".into(),
            content: "hello".into(),
            timestamp_ms: 1_700_000_000_000,
        };
        let first = inbound.to_domain();
        let second = inbound.to_domain();
        assert_eq!(first.id, second.id);
        assert_eq!(first.conversation_id, second.conversation_id);
    }

    #[test]
    fn timestamps_become_nanoseconds_without_wrapping() {
        assert_eq!(millis_to_nanos(1_700_000_000_000), 1_700_000_000_000_000_000);
        assert_eq!(millis_to_nanos(u64::MAX), u64::MAX);
    }

    #[test]
    fn inbound_and_outbound_produce_the_same_row() {
        let conversation = MeshConversation::Geohash("u4pruy".into());
        let sender = peer("abcdef0123456789");
        let inbound = InboundMessage {
            wire_id: "MSG-2".into(),
            conversation: conversation.clone(),
            sender: sender.clone(),
            nickname: "me".into(),
            content: "hi".into(),
            timestamp_ms: 42_000,
        }
        .to_domain();
        let outbound = outbound_message(&conversation, "MSG-2", &sender, "hi", 42_000);

        assert_eq!(inbound.id, outbound.id);
        assert_eq!(inbound.conversation_id, outbound.conversation_id);
        assert_eq!(inbound.conversation_type, outbound.conversation_type);
        assert_eq!(inbound.sender_id, outbound.sender_id);
        assert_eq!(text_of(&inbound), text_of(&outbound));
        assert_eq!(inbound.sent_at, outbound.sent_at);
    }

    #[test]
    fn scope_accessors_agree_with_the_variant() {
        let geo = MeshConversation::Geohash("u4pruy".into());
        assert!(geo.is_public());
        assert_eq!(geo.geohash(), Some("u4pruy"));
        assert!(geo.peer().is_none());

        let dm = MeshConversation::Dm(peer("nostr_ab"));
        assert!(!dm.is_public());
        assert_eq!(dm.peer().map(PeerId::as_str), Some("nostr_ab"));
        assert!(dm.geohash().is_none());
    }

    #[test]
    fn a_broadcast_carries_its_delivery_state_from_the_moment_it_is_composed() {
        // The bug this pins showed the same message under two different labels
        // minutes apart: "sent" while it was live, because an own message with an
        // empty status falls through to that default, then "broadcast" after a
        // reopen, from the durable state. "sent" is one-to-one vocabulary and a
        // broadcast has no recipient to have sent it *to*.
        let sender = PeerId::from_str("aabbccddeeff0011");
        for conversation in [
            MeshConversation::Room,
            MeshConversation::Geohash("u4pruy".to_string()),
        ] {
            let message = outbound_message(&conversation, "MSG-B", &sender, "hi", 42_000);
            assert_eq!(
                message.delivery_status,
                MeshDelivery::Broadcast.as_str(),
                "{conversation:?} is a broadcast"
            );
            assert_ne!(message.delivery_status, "sent");
        }
    }

    #[test]
    fn a_dm_is_left_blank_because_its_outcome_is_not_known_yet() {
        // The opposite case, and the reason this is not unconditional: a DM's
        // delivery arrives moments later as a `MeshEvent::Delivery`, so claiming
        // anything here would be a guess.
        let sender = PeerId::from_str("aabbccddeeff0011");
        let dm = MeshConversation::Dm(PeerId::from_str("1122334455667788"));
        let message = outbound_message(&dm, "MSG-D", &sender, "hi", 42_000);
        assert!(message.delivery_status.is_empty());
    }
}

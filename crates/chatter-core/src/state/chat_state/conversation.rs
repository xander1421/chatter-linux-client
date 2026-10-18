use crate::domain::anon_dm::AnonDmId;
use crate::domain::anon_group::AnonGroupId;
use crate::domain::ids::{ChannelId, UserId};

// ============================================================================
// Active Conversation (typed, replaces string parsing)
// ============================================================================

/// The currently active conversation, fully typed.
///
/// Replaces the pattern of checking `get_selected_dm_id()` string prefixes
/// and `get_selected_private_group_id().is_empty()` scattered across 109 sites.
/// Set once at sidebar selection time, read everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveConversation {
    /// No conversation selected.
    None,
    /// A server-managed channel (public/private).
    Channel(ChannelId),
    /// A DM to a friend (Signal Protocol E2EE). Channel-free — there is no
    /// server-side DM channel. Keyed entirely by the peer's `UserId`; the
    /// local `conversation_id` (`ChannelId`) is derived via
    /// `ChannelId::from(peer_id)`. ONE conversation whichever transport
    /// carries it: sealed-sender streaming normally, the NDSS'21 mailbox in
    /// Shield mode (`MessageService::send_friend_dm_content`).
    ServerDm { peer_id: UserId },
    /// An anonymous DM (NDSS'21 mailbox, pairwise, HPKE bootstrap).
    /// Server cannot link conversation to registered users.
    AnonDm(AnonDmId),
    /// An anonymous group (NDSS'21 mailbox, fanout).
    AnonGroup(AnonGroupId),
}

impl Default for ActiveConversation {
    fn default() -> Self {
        Self::None
    }
}

impl ActiveConversation {
    /// Get the ChannelId for this conversation (if applicable).
    ///
    /// Channels and DMs have server-assigned ChannelIds.
    /// Anon types can be converted to synthetic ChannelIds.
    pub fn channel_id(&self) -> Option<ChannelId> {
        match self {
            Self::Channel(id) => Some(*id),
            Self::ServerDm { peer_id } => Some(ChannelId::from(*peer_id)),
            Self::AnonDm(id) => Some((*id).into()),
            Self::AnonGroup(id) => Some((*id).into()),
            Self::None => None,
        }
    }

    /// Whether an event or message keyed by `id` belongs to this
    /// conversation's open view. The ids of different conversation kinds are
    /// distinct UUIDs, so the id alone decides.
    pub fn is_for_channel(&self, id: ChannelId) -> bool {
        self.channel_id() == Some(id)
    }

    /// The id the SIDEBAR uses for this conversation — the same string the
    /// context menus send to `move_to_section` / the pin service.
    ///
    /// Not the same as [`channel_id`](Self::channel_id): a server DM is
    /// channel-free and is keyed by the peer's `UserId`, which is what the DM
    /// row carries. Pin state keyed any other way would not match the row.
    pub fn sidebar_key(&self) -> Option<String> {
        match self {
            Self::Channel(id) => Some(id.to_string()),
            Self::ServerDm { peer_id } => Some(peer_id.to_string()),
            Self::AnonDm(id) => Some(id.to_string()),
            Self::AnonGroup(id) => Some(id.to_string()),
            Self::None => None,
        }
    }

    /// The `(type, id)` pair that names this conversation in the sidebar's
    /// routing vocabulary, and the pair call history stores so a past call
    /// can be redialled.
    ///
    /// The type here is the *variant's* type, not the message-storage type
    /// [`conversation_type`](Self::conversation_type) returns — an encrypted
    /// channel stores its messages as `AnonGroup` but is still routed as
    /// `Channel(id)`, and round-tripping through the storage type would
    /// reopen it as an anonymous group.
    pub fn routing_key(&self) -> Option<(crate::domain::message::ConversationType, String)> {
        use crate::domain::message::ConversationType as CT;
        let ct = match self {
            Self::Channel(_) => CT::Channel,
            Self::ServerDm { .. } => CT::ServerDm,
            Self::AnonDm(_) => CT::AnonDm,
            Self::AnonGroup(_) => CT::AnonGroup,
            Self::None => return None,
        };
        self.sidebar_key().map(|id| (ct, id))
    }

    /// Inverse of [`routing_key`](Self::routing_key).
    pub fn from_routing_key(
        conversation_type: &crate::domain::message::ConversationType,
        id: &str,
    ) -> Option<Self> {
        use crate::domain::message::ConversationType as CT;
        match conversation_type {
            CT::Channel | CT::SavedMessages => id.parse().ok().map(Self::Channel),
            CT::ServerDm => id.parse().ok().map(|peer_id| Self::ServerDm { peer_id }),
            CT::AnonDm => id.parse().ok().map(Self::AnonDm),
            CT::AnonGroup => id.parse().ok().map(Self::AnonGroup),
            CT::Mesh | CT::MeshDm => None,
        }
    }

    /// Check if any conversation is active.
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Check if this is an anonymous conversation (AnonDm or AnonGroup).
    pub fn is_anon(&self) -> bool {
        matches!(self, Self::AnonDm(_) | Self::AnonGroup(_))
    }

    /// Get the ConversationType for this active conversation.
    ///
    /// Returns None for ActiveConversation::None.
    ///
    /// `channel_repo` is needed for the `Channel(id)` variant — its
    /// ConversationType comes from the channel row (`Channel`, `ServerDm`
    /// or `SavedMessages`). Encryption does not change it: an encrypted
    /// server channel is still `Channel`, never `AnonGroup`.
    pub fn conversation_type(
        &self,
        channel_repo: &crate::storage::ChannelRepository,
    ) -> Option<crate::domain::message::ConversationType> {
        use crate::domain::message::ConversationType;
        match self {
            Self::Channel(id) => Some(
                crate::services::message::conversion::conversation_type_for_channel(
                    channel_repo,
                    id,
                ),
            ),
            Self::ServerDm { .. } => Some(ConversationType::ServerDm),
            Self::AnonDm(_) => Some(ConversationType::AnonDm),
            Self::AnonGroup(_) => Some(ConversationType::AnonGroup),
            Self::None => None,
        }
    }

    // ========================================================================
    // Selection predicates
    //
    // Typed "is this conversation the active one?" checks used by the UI to
    // highlight sidebar items, gate unread increments, and route receipts
    // without string-comparing Slint window properties.
    // ========================================================================

    /// True if the active conversation is the given regular channel.
    pub fn matches_channel(&self, channel_id: &ChannelId) -> bool {
        matches!(self, Self::Channel(id) if id == channel_id)
    }

    /// True if the active conversation is a DM matching either the channel
    /// or the peer. Used by the DM sidebar where items are identified by
    /// `channel_id` and the header by `peer_id`.
    pub fn matches_dm(&self, channel_id: &ChannelId, peer_id: &UserId) -> bool {
        match self {
            Self::ServerDm { peer_id: pid } => {
                ChannelId::from(*pid) == *channel_id || pid == peer_id
            }
            _ => false,
        }
    }

    /// True if the active conversation is a DM with the given peer.
    pub fn matches_dm_peer(&self, peer_id: &UserId) -> bool {
        matches!(self, Self::ServerDm { peer_id: pid } if pid == peer_id)
    }

    /// True if the active conversation is the given anonymous DM.
    pub fn matches_anon_dm(&self, id: &AnonDmId) -> bool {
        matches!(self, Self::AnonDm(anon) if anon == id)
    }

    /// True if the active conversation is the given anonymous group.
    pub fn matches_anon_group(&self, id: &AnonGroupId) -> bool {
        matches!(self, Self::AnonGroup(g) if g == id)
    }
}

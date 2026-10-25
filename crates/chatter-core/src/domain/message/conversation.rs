use super::Message;
use crate::error::StorageError;
use serde::{Deserialize, Serialize};

// ============================================================================
// ConversationType
// ============================================================================

/// Discriminant for the type of conversation a message belongs to.
///
/// Used in the unified `messages` table to distinguish between channel messages,
/// direct messages, anonymous DMs, anonymous groups, etc.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConversationType {
    /// Server-managed channel (public, private, or group).
    Channel,
    /// Friend DM (1:1, channel-free, keyed by the peer). One conversation
    /// whichever transport carried a message — sealed-sender streaming or,
    /// in Shield mode, the NDSS'21 mailbox.
    ServerDm,
    /// Anonymous DM (NDSS'21 style, client-managed).
    AnonDm,
    /// Anonymous group (NDSS'21 style, client-managed).
    AnonGroup,
    /// Saved messages / personal vault.
    SavedMessages,
    /// Public bitchat mesh room, or a geohash channel. Client-managed and
    /// serverless: these messages never reach the server, so no server-side
    /// authorization applies to them.
    ///
    /// The transport scope lives in the conversation id rather than here —
    /// `PeerId` is already namespaced (`nostr:` for geohash chat, plain hex for
    /// the BLE mesh room), and a second variant would give it two sources of
    /// truth.
    Mesh,
    /// One-to-one bitchat conversation, end-to-end encrypted over the mesh
    /// (Noise) or over Nostr (gift-wrapped). Also serverless.
    MeshDm,
}

impl ConversationType {
    /// 1:1 DMs: either authenticated participant may request delete-for-everyone.
    /// Channels, groups, saved messages, and mesh rooms stay author-only.
    #[must_use]
    pub fn is_one_to_one_dm(&self) -> bool {
        matches!(self, Self::ServerDm | Self::AnonDm | Self::MeshDm)
    }

    /// Convert to database string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::ServerDm => "server_dm",
            Self::AnonDm => "anon_dm",
            Self::AnonGroup => "anon_group",
            Self::SavedMessages => "saved_messages",
            Self::Mesh => "mesh",
            Self::MeshDm => "mesh_dm",
        }
    }

    /// Serverless conversations carried by the bitchat mesh.
    ///
    /// These never make a server RPC, so nothing about them is authorized
    /// server-side. Anything keyed on "the server knows about this conversation"
    /// must exclude them.
    pub fn is_mesh(&self) -> bool {
        matches!(self, Self::Mesh | Self::MeshDm)
    }

    /// Anonymous conversations (NDSS'21 style, client-managed). Replies here
    /// carry a self-contained quote snapshot and reference their parent via a
    /// synthetic timestamp-derived `MessageId` that never matches the parent's
    /// random stored id — so parent-by-id lookups (e.g. the "Original message
    /// not found" check) are meaningless and must be skipped.
    pub fn is_anon(&self) -> bool {
        matches!(self, Self::AnonDm | Self::AnonGroup)
    }

    /// A Telegram-style secret chat: the anonymous DMs and groups, bound to
    /// this device and carried over Tor only (owner decision 2026-10-10).
    pub fn is_secret_chat(&self) -> bool {
        self.is_anon()
    }

    /// Whether a message in this conversation may be forwarded anywhere.
    /// Nothing leaves a secret chat: a forward would carry its content (and
    /// its attachments' CDN objects) into a conversation tied to the account.
    pub fn can_forward_from(&self) -> bool {
        !self.is_secret_chat()
    }

    /// Whether a forward may land here. A secret chat takes forwarded text
    /// only: a forwarded attachment reuses the original's CDN pointer, an
    /// object uploaded (and minted) outside the secret chat, which would tie
    /// the two conversations together on the server.
    pub fn accepts_forward(&self, has_attachments: bool) -> bool {
        !(self.is_secret_chat() && has_attachments)
    }

    /// Whether the GIF picker is offered here. GIF search and media go through
    /// the server's GifService, which takes only account (Basic) auth, so
    /// using it from a secret chat would tie that activity to the account even
    /// over Tor.
    pub fn gif_picker_available(&self) -> bool {
        !self.is_secret_chat()
    }

    /// Parse from database string representation.
    ///
    /// Returns an error for unknown conversation types instead of silently
    /// falling back to `Channel`.
    pub fn from_str_value(s: &str) -> Result<Self, StorageError> {
        match s {
            "channel" => Ok(Self::Channel),
            "server_dm" => Ok(Self::ServerDm),
            "anon_dm" => Ok(Self::AnonDm),
            "anon_group" => Ok(Self::AnonGroup),
            "saved_messages" => Ok(Self::SavedMessages),
            "mesh" => Ok(Self::Mesh),
            "mesh_dm" => Ok(Self::MeshDm),
            _ => Err(StorageError::InvalidData(format!(
                "Unknown conversation type: {s}"
            ))),
        }
    }
}

impl Default for ConversationType {
    fn default() -> Self {
        Self::Channel
    }
}

impl std::fmt::Display for ConversationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ============================================================================
// UnreadAnchor
// ============================================================================

/// Where the user left off in a conversation.
///
/// Opening a conversation clears `is_read` for the whole conversation, which
/// destroys the boundary — so this is captured *inside*
/// `UnifiedMessageRepository::mark_conversation_read`, in the same call, before
/// the `UPDATE`. There is deliberately no way to ask for it afterwards: that
/// ordering bug is what makes "open where I left off" quietly degrade to "open
/// at the bottom" and nobody notices.
///
/// Our own sent messages land `is_read = 1` (`insert_outgoing`), so the
/// boundary always points at a message somebody else sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnreadAnchor {
    /// `sent_at` of every message that was unread, ascending. Empty when
    /// nothing was unread — the common case, and the caller should open at the
    /// newest message. The messages themselves rather than the oldest one's
    /// time: the list is ordered by `sent_at`, not by arrival, so an unread
    /// message can sit anywhere in it (delivered late, a thread reply, older
    /// than the loaded page) and everything after it is not thereby unread.
    pub unread_sent_at: Vec<u64>,
    /// How many unread messages the conversation held. May exceed one page.
    pub unread_count: u32,
}

impl UnreadAnchor {
    /// Index of the unread divider in an ascending (oldest-first) page.
    ///
    /// The divider sits *above* the first row of the page that was unread, so
    /// the returned index is the row the viewport should open on. `None` means
    /// open at the newest message: either nothing was unread, or none of what
    /// was unread is on this page.
    ///
    /// Row 0 is a legitimate answer — a conversation whose entire loaded page
    /// is unread opens at the top with the divider above the first row.
    #[must_use]
    pub fn divider_index(&self, page: &[Message]) -> Option<usize> {
        page.iter()
            .position(|m| self.unread_sent_at.binary_search(&m.sent_at).is_ok())
    }

    /// The page a conversation opens on: the newest rows, grown older until
    /// it holds the first unread message — [`newest_page_reaching`] with that
    /// message as the target.
    ///
    /// A fixed newest page put the divider above its own row 0 whenever more
    /// unread messages arrived than fit in it (device ui135: 122 unread, the
    /// divider sat above the newest 50 and the chat opened 72 messages past
    /// where the user left off). With the first unread on the page,
    /// `divider_index` lands on it and the divider and the pill agree.
    pub fn opening_page<E>(
        &self,
        page_size: u32,
        load: impl FnMut(u32) -> Result<Vec<Message>, E>,
    ) -> Result<Vec<Message>, E> {
        newest_page_reaching(self.unread_sent_at.first().copied(), page_size, load)
    }
}

/// The newest rows of a conversation's main list, oldest first, grown older
/// until the oldest row is older than `target_sent_at` — so the message sent
/// then is on the page, whatever ties on `sent_at` there are — or history
/// runs out. `None` is just the newest page.
///
/// One mechanism for every "open the list at this message" case: the first
/// unread message on a conversation open (`UnreadAnchor::opening_page`) and a
/// jump to a message older than what is loaded (a search hit, a quote, a pin).
///
/// `load(limit)` returns the newest `limit` rows, oldest first. The limit
/// doubles per step, so a target any distance back costs a logarithmic number
/// of reads and the page carries context above it. A target that is not on
/// this list ends when history does (fewer rows than asked for), never spins.
pub fn newest_page_reaching<E>(
    target_sent_at: Option<u64>,
    page_size: u32,
    mut load: impl FnMut(u32) -> Result<Vec<Message>, E>,
) -> Result<Vec<Message>, E> {
    let mut limit = page_size.max(1);
    loop {
        let page = load(limit)?;
        let reached = match (target_sent_at, page.first()) {
            (Some(target), Some(oldest)) => oldest.sent_at < target,
            _ => true,
        };
        if reached || (page.len() as u32) < limit {
            return Ok(page);
        }
        limit = limit.saturating_mul(2);
    }
}

/// Walk toward an older target with cursor pages and retain only the target's
/// nearby rows. No repository read or returned window exceeds 200 messages.
pub fn newest_window_reaching<E>(
    target_sent_at: u64,
    page_size: u32,
    mut load: impl FnMut(Option<&crate::domain::ids::MessageId>, u32) -> Result<Vec<Message>, E>,
) -> Result<Vec<Message>, E> {
    const MAX_WINDOW: usize = 200;
    let mut before = None;
    let mut newer = Vec::new();
    let mut fetch = page_size.clamp(1, MAX_WINDOW as u32);
    loop {
        let mut page = load(before.as_ref(), fetch)?;
        if page.is_empty() {
            return Ok(newer);
        }
        let count = page.len();
        if page.first().is_some_and(|oldest| oldest.sent_at < target_sent_at) {
            let start = page.iter().position(|message| message.sent_at >= target_sent_at)
                .unwrap_or(page.len());
            let mut window = page.split_off(start);
            window.extend(newer);
            window.truncate(MAX_WINDOW);
            return Ok(window);
        }
        before = page.first().map(|message| message.id);
        newer = page;
        if count < fetch as usize {
            return Ok(newer);
        }
        fetch = MAX_WINDOW as u32;
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use crate::domain::ids::{ChannelId, UserId};

    #[test]
    fn distant_target_uses_bounded_cursor_pages() {
        let channel = ChannelId::new();
        let messages: Vec<_> = (0..650u64)
            .map(|sent_at| Message::new_text(channel, ConversationType::Channel,
                Some(UserId::new()), "row".into(), sent_at))
            .collect();
        let mut max_fetch = 0;
        let window = newest_window_reaching(100, 50, |before, limit| {
            max_fetch = max_fetch.max(limit);
            let end = before.and_then(|id| messages.iter().position(|m| &m.id == id))
                .unwrap_or(messages.len());
            let start = end.saturating_sub(limit as usize);
            Ok::<_, ()>(messages[start..end].to_vec())
        }).unwrap();
        assert!(max_fetch <= 200);
        assert_eq!(window.first().map(|m| m.sent_at), Some(100));
        assert!(window.len() <= 200);
    }
}

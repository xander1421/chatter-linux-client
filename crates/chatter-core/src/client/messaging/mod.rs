//! messaging façade — thin delegates to services.

use std::sync::Arc;

use super::ChatterClient;
use crate::domain::channel::Channel;
use crate::domain::message::ConversationType;
use crate::state::ActiveConversation;

/// Channel invitation text resolved from the current user's local contacts.
pub struct ChannelInviteRow {
    pub channel_id: String,
    pub channel_name: String,
    pub private_channel: bool,
    pub invited_by: String,
    pub created_at: u64,
    pub note: String,
}

/// Make every chat-list preview a single plain-text line, including notices,
/// attachment labels and pending requests. The row elides to its own width.
pub(crate) fn plain_sidebar_preview(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut remove = vec![false; chars.len()];
    let mut i = 0;
    while i < chars.len() {
        let marker = chars[i];
        if marker == '>'
            && chars[..i]
                .iter()
                .rev()
                .take_while(|c| **c != '\n')
                .all(|c| c.is_whitespace())
        {
            remove[i] = true;
            i += 1;
            continue;
        }
        if matches!(marker, '*' | '_' | '`' | '~') {
            let start = i;
            while i < chars.len() && chars[i] == marker {
                i += 1;
            }
            let width = i - start;
            let standalone = (start == 0 || chars[start - 1].is_whitespace())
                && (i == chars.len() || chars[i].is_whitespace());
            if standalone
                && ((marker == '`' && width >= 3) || (matches!(marker, '*' | '_') && width >= 2))
            {
                for index in start..i {
                    remove[index] = true;
                }
                continue;
            }
            let left_boundary = start == 0
                || (!chars[start - 1].is_alphanumeric()
                    && chars[start - 1] != '/'
                    && chars[start - 1] != '\\');
            let content_follows = i < chars.len() && !chars[i].is_whitespace();
            // Strikethrough is only ever `~~`, as the bubble renders it; a
            // lone `~` ("~5 min") is text.
            if left_boundary && content_follows && (marker != '~' || width == 2) {
                let mut end = i + 1;
                while end + width <= chars.len() {
                    if chars[end..end + width].iter().all(|c| *c == marker)
                        && (!chars[end - 1].is_whitespace() || (marker == '`' && width >= 3))
                        && (end + width == chars.len()
                            || (!chars[end + width].is_alphanumeric()
                                && !(marker == '_'
                                    && chars[end + width] == '.'
                                    && end + width + 1 < chars.len()
                                    && chars[end + width + 1].is_alphanumeric())))
                    {
                        for index in start..i {
                            remove[index] = true;
                        }
                        for index in end..end + width {
                            remove[index] = true;
                        }
                        break;
                    }
                    end += 1;
                }
            }
            continue;
        }
        i += 1;
    }
    let mut output = String::with_capacity(input.len());
    let mut pending_space = false;
    for (index, ch) in chars.into_iter().enumerate() {
        if ch.is_whitespace() {
            pending_space = !output.is_empty();
        } else if !remove[index] && !ch.is_control() {
            if pending_space {
                output.push(' ');
                pending_space = false;
            }
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod sidebar_preview_tests {
    use super::plain_sidebar_preview;

    #[test]
    fn flattening_keeps_directional_isolates() {
        let preview = crate::domain::message::format_preview_body(
            "You: ", "uni1 שלום עולם مرحبا Привет", None,
        );
        assert_eq!(plain_sidebar_preview(&preview), preview);
    }

    #[test]
    fn markdown_fences_and_lines_become_one_plain_line() {
        assert_eq!(
            plain_sidebar_preview("  You: **hello**\n```\n> _world_  `again`\n```  "),
            "You: hello world again"
        );
    }

    #[test]
    fn strikethrough_pairs_strip_but_lone_tildes_stay() {
        assert_eq!(
            plain_sidebar_preview("You: ~~gone~~ in ~5 min"),
            "You: gone in ~5 min"
        );
    }

    #[test]
    fn emoji_clusters_survive_without_truncation() {
        let emoji = "👩🏽‍💻";
        let source = format!("{}{}{}\n**done**", emoji, emoji, emoji);
        assert_eq!(
            plain_sidebar_preview(&source),
            format!("{}{}{} done", emoji, emoji, emoji)
        );
    }
}

/// Actions available for the current stored selection.
pub struct SelectionActions {
    pub can_delete_everyone: bool,
    pub can_forward: bool,
}

/// Complete, display-ordered selection resolved away from the UI thread.
pub struct SelectionSnapshot {
    pub actions: SelectionActions,
    pub targets: Vec<(crate::domain::ids::UserId, u64)>,
    pub copy_text: String,
}

mod channels;
mod display;
mod friends;
mod history;
mod search;
mod sidebar;
mod threads;

/// Media descriptor of a reply's quoted parent, shaped by
/// [`ChatterClient::reply_parent_media`]. Lets the quote bubble render the
/// parent's media richly (an inline audio player for audio parents, a thumbnail
/// for visual parents) instead of plain text. All fields are empty/`None` when
/// the message is not a reply, the parent or its first attachment can't be
/// resolved locally, or the parent carries no renderable media.
#[derive(Debug, Clone, Default)]
pub struct ReplyParentMedia {
    /// `"audio"`/`"image"`/`"gif"`/`"video"` — `"gif"` splits out of image so
    /// the quote can badge it as animated. Empty when no renderable media.
    pub category: String,
    /// Parent attachment id (for play/seek callbacks).
    pub attachment_id: String,
    /// Formatted `"m:ss"` for audio, else empty.
    pub duration: String,
    /// Parent attachment's locally-stored thumbnail bytes for visual categories
    /// — a fallback the converter uses when the wire quote thumbnail (capped at
    /// ~12KB and often dropped) is absent, so image/gif/video quotes still show
    /// a preview instead of degrading to text.
    pub thumbnail: Option<Vec<u8>>,
    /// Parent attachment filename, shown in the inline audio quote player.
    pub filename: String,
    /// Audio waveform (32 values, 0-100) for the inline quote player; empty when
    /// the parent carries none.
    pub waveform: Vec<i32>,
}

/// One destination row in the forward picker, shaped by
/// [`ChatterClient::forward_destinations`].
///
/// `kind` is core's own conversation vocabulary, so the confirm step maps a
/// picked row back with [`ActiveConversation::from_routing_key`] instead of the
/// UI inventing a tag of its own.
#[derive(Debug, Clone)]
pub struct ForwardDestination {
    pub id: String,
    pub name: String,
    pub kind: ConversationType,
}

/// A person the user can invite to a channel, shaped by
/// [`ChatterClient::list_channel_invite_candidates`]. UI maps this straight to
/// its row struct — no sourcing or dedupe logic in the UI layer.
#[derive(Debug, Clone)]
pub struct InviteCandidate {
    pub user_id: crate::domain::ids::UserId,
    pub display_name: String,
    pub username: String,
    /// Peer avatar bytes (decrypted profile image), empty when none cached.
    pub avatar_bytes: Vec<u8>,
    pub already_member: bool,
    /// Actively banned: not invitable until unbanned.
    pub banned: bool,
}

impl InviteCandidate {
    /// Whether this candidate matches a search query (case-insensitive
    /// substring over display name or username). Keeps the "what fields a
    /// query matches" rule out of the UI filter.
    pub fn matches(&self, query: &str) -> bool {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return true;
        }
        self.display_name.to_lowercase().contains(&needle)
            || self.username.to_lowercase().contains(&needle)
    }
}

/// UI-facing send result returned by [`ChatterClient::send_message_ui`].
///
/// Consolidates the post-send shaping that used to live in the UI send
/// pipeline: effective message id (synthetic for bootstrap), delivery
/// status string, sidebar preview with "You: " prefix + truncation,
/// local-time `HH:MM` stamp, attachments resolved by id, and the quoted
/// message's thumbnail bytes (if any).
///
/// UI code renders this struct directly — no `get_attachment` loops, no
/// preview string building, no "pending-{ts}" synthesis.
#[derive(Debug, Clone)]
pub struct SendMessageUiResult {
    /// Effective message id to render in the UI: the stored message's id,
    /// also for a send queued behind a DM bootstrap; `"pending-{sent_at}"`
    /// only when nothing was stored.
    pub message_id_effective: String,
    /// Server-assigned sent-at timestamp in nanoseconds.
    pub sent_at: u64,
    /// Local-time `HH:MM` string for message header rendering.
    pub timestamp_display: String,
    /// `"sent"` (delivered), `"sending"` (DM bootstrap in flight),
    /// `"unconfirmed"` (submitted, outcome unknown; secret posts retry retained ciphertext), or
    /// `"failed"` (stored, not delivered — see `failure_notice`).
    pub delivery_status: &'static str,
    /// True when a DM bootstrap was kicked off — UI should show the
    /// "Setting up secure channel..." composer placeholder.
    pub bootstrap_initiated: bool,
    /// Resolved attachments for the outgoing message. Empty during DM
    /// bootstrap (media can't flow until the key exchange completes).
    pub attachments: Vec<crate::domain::attachment::Attachment>,
    /// Raw thumbnail bytes for the quoted message (JPEG/PNG), if any.
    pub reply_thumbnail_bytes: Option<Vec<u8>>,
    /// Quoted parent's media descriptor (category/attachment/duration/filename/
    /// waveform) so the sender's own bubble renders the audio player / media
    /// preview immediately, without waiting for a reload.
    pub reply_parent_media: crate::client::ReplyParentMedia,
    /// Why the message was not delivered, for the user, when
    /// `delivery_status` is `"failed"`.
    pub failure_notice: Option<String>,
    pub failure_reason: Option<String>,
}

/// Enriched mention suggestion surfaced by
/// [`ChatterClient::search_mention_suggestions`].
#[derive(Debug, Clone)]
pub struct MentionSuggestionInfo {
    pub id: String,
    /// Bare name; the composer adds the `@` prefix.
    pub name: String,
    pub kind: MentionSuggestionKind,
    /// Decrypted profile image, when available for this member.
    pub avatar_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionSuggestionKind {
    /// Real user in the channel.
    User,
    /// Online members broadcast.
    Here,
    /// All members broadcast.
    All,
}

impl MentionSuggestionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Here => "here",
            Self::All => "all",
        }
    }
}

#[cfg(test)]
mod tests;

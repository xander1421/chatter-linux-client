//! Audio track lookups for the persistent player.
//!
//! The player keeps running after the user navigates away from the
//! conversation a voice note came from, so it can't ask the visible message
//! model what to play next — that model belongs to whatever conversation is
//! on screen. These lookups go straight to storage instead.

use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::domain::message::ConversationType;

use super::{MessageError, MessageService};

/// A playable audio attachment plus the context the player needs: what to
/// label it with, and where to jump back to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioTrack {
    pub attachment_id: AttachmentId,
    pub message_id: MessageId,
    pub conversation_id: ChannelId,
    pub conversation_type: ConversationType,
    /// Display name of the sender. Empty when the sender is unknown locally
    /// (sealed sender with no cached user row).
    pub sender_name: String,
    pub filename: String,
    /// Sender timestamp — the cursor for finding the track after this one.
    pub sent_at: u64,
}

// ponytail: a single fixed-size window, no pagination. Two voice notes more
// than this many messages apart end the chain instead of continuing it. Widen
// the constant if that ever comes up in practice.
const SCAN_WINDOW: u32 = 200;

impl MessageService {
    /// Find the next audio attachment in a conversation after a given message.
    ///
    /// Scans forward from `after_sent_at` and returns the first `audio/*`
    /// attachment found, or `None` if there is none within the scan window.
    pub fn next_audio_after(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        after_sent_at: u64,
    ) -> Result<Option<AudioTrack>, MessageError> {
        let messages = self.message_repo.list_after_asc(
            conversation_id,
            conversation_type,
            after_sent_at,
            SCAN_WINDOW,
        )?;

        for message in messages {
            for attachment_id in &message.attachments {
                let Some(attachment) = self.attachment_repo.get(attachment_id)? else {
                    continue;
                };
                if !attachment.metadata.mime_type.starts_with("audio/") {
                    continue;
                }
                return Ok(Some(AudioTrack {
                    attachment_id: attachment_id.clone(),
                    message_id: message.id.clone(),
                    conversation_id: message.conversation_id.clone(),
                    conversation_type: message.conversation_type,
                    sender_name: self.sender_display_name(message.sender_id.as_ref()),
                    filename: attachment.metadata.name,
                    sent_at: message.sent_at,
                }));
            }
        }

        Ok(None)
    }

    /// Best-effort display name for a message sender: profile display name,
    /// falling back to username, then to empty for unknown senders.
    fn sender_display_name(&self, sender_id: Option<&crate::domain::ids::UserId>) -> String {
        let Some(id) = sender_id else {
            return String::new();
        };
        let profile_name = self
            .user_repo
            .get_profile(id)
            .ok()
            .flatten()
            .map(|profile| profile.display_name)
            .filter(|name| !name.is_empty());

        profile_name
            .or_else(|| self.user_repo.get(id).ok().flatten().map(|user| user.username))
            .unwrap_or_default()
    }
}

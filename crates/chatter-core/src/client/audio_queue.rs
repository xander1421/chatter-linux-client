//! What the audio player has loaded, and what it plays next.
//!
//! Playback outlives the conversation it started in, so the track's origin is
//! kept here rather than read off whatever conversation is on screen: enough
//! to label the mini player, return to the source message, and continue with
//! the next audio attachment of the same conversation.

use crate::domain::attachment::{Attachment, SharedCategory};
use crate::domain::ids::{AttachmentId, MessageId};
use crate::domain::message::Message;
use crate::state::ActiveConversation;

use super::ChatterClient;

/// How far past the finished track the queue looks for the next one.
const QUEUE_SCAN_WINDOW: u32 = 200;

/// The loaded audio attachment and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioTrack {
    pub attachment_id: AttachmentId,
    /// Message carrying the attachment. `None` when it could not be located
    /// (played from outside its conversation); such a track has no message to
    /// return to and no queue.
    pub message_id: Option<MessageId>,
    /// Conversation of that message; `ActiveConversation::None` with it.
    pub conversation: ActiveConversation,
    /// "Voice message" for a recorded note, the file name otherwise.
    pub title: String,
    /// Sender's display name ("You" for own messages); empty when unknown.
    pub sender: String,
    /// Auth session the track belongs to: it is forgotten once that ends.
    session: u64,
}

/// The audio attachment that follows `after` in `messages` (oldest first):
/// later attachments of the same message, then later messages. Deleted
/// messages are skipped; `after` missing from the list ends the queue.
fn next_audio<'a>(
    messages: &'a [Message],
    after: &AttachmentId,
    is_audio: impl Fn(&AttachmentId) -> bool,
) -> Option<(&'a Message, &'a AttachmentId)> {
    messages
        .iter()
        .filter(|message| !message.is_deleted)
        .flat_map(|message| message.attachments.iter().map(move |id| (message, id)))
        .skip_while(|(_, id)| *id != after)
        .skip(1)
        .find(|(_, id)| is_audio(id))
}

impl ChatterClient {
    /// The loaded track, if any. `None` once the session it was started in
    /// has ended, so playback never carries over a logout or account switch.
    pub fn now_playing(&self) -> Option<AudioTrack> {
        let session = self.session_revision();
        self.now_playing
            .lock()
            .clone()
            .filter(|track| track.session == session)
    }

    /// Whether the loaded track should be controlled from the mini player:
    /// its conversation is not the one on screen, where the message's own
    /// player is.
    pub fn now_playing_elsewhere(&self) -> bool {
        self.now_playing().is_some_and(|track| {
            !track.conversation.is_active() || track.conversation != self.active_conversation()
        })
    }

    /// Record that `attachment_id` started playing from `conversation`.
    /// `message_hint` is the message the UI showed it under, when known.
    /// A track the queue already advanced to is left as it is.
    pub fn audio_track_started(
        &self,
        attachment_id: &AttachmentId,
        conversation: &ActiveConversation,
        message_hint: Option<&MessageId>,
    ) {
        if self
            .now_playing()
            .is_some_and(|track| track.attachment_id == *attachment_id)
        {
            return;
        }
        let track = self.get_attachment(attachment_id).map(|attachment| {
            let message = self.attachment_owner(conversation, message_hint, attachment_id);
            self.audio_track(&attachment, message.as_ref(), conversation)
        });
        *self.now_playing.lock() = track;
    }

    /// The finished track's successor becomes the loaded track and is
    /// returned for the player to load; `None` (and nothing loaded) at the
    /// end of the queue.
    pub fn advance_audio_queue(&self) -> Option<AudioTrack> {
        let next = self.now_playing().and_then(|track| self.audio_track_after(&track));
        *self.now_playing.lock() = next.clone();
        next
    }

    /// Forget the loaded track (the player was stopped).
    pub fn clear_now_playing(&self) {
        *self.now_playing.lock() = None;
    }

    fn audio_track_after(&self, track: &AudioTrack) -> Option<AudioTrack> {
        let current = self.get_message_local(track.message_id.as_ref()?)?;
        // A thread reply is not a main-list cursor: the lookup fails and the
        // queue ends with the current message.
        let newer = self
            .messages()
            .get_newer_messages_local_asc(
                &current.conversation_id,
                &current.conversation_type,
                &current.id,
                QUEUE_SCAN_WINDOW,
            )
            .unwrap_or_default();
        let mut messages = vec![current];
        messages.extend(newer);
        let (message, attachment_id) = next_audio(&messages, &track.attachment_id, |id| {
            self.get_attachment(id)
                .is_some_and(|attachment| attachment.metadata.is_audio())
        })?;
        let attachment = self.get_attachment(attachment_id)?;
        Some(self.audio_track(&attachment, Some(message), &track.conversation))
    }

    fn audio_track(
        &self,
        attachment: &Attachment,
        message: Option<&Message>,
        conversation: &ActiveConversation,
    ) -> AudioTrack {
        AudioTrack {
            attachment_id: attachment.id.clone(),
            message_id: message.map(|message| message.id.clone()),
            conversation: if message.is_some() {
                conversation.clone()
            } else {
                ActiveConversation::None
            },
            title: if SharedCategory::of(&attachment.metadata) == SharedCategory::Voice {
                chatter_i18n::tr("Voice message")
            } else {
                attachment.metadata.name.clone()
            },
            sender: message
                .map(|message| self.message_sender_label(message))
                .unwrap_or_default(),
            session: self.session_revision(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::ChannelId;
    use crate::domain::message::ConversationType;

    fn message(sent_at: u64, attachments: &[AttachmentId]) -> Message {
        let mut message = Message::new_text(
            ChannelId::new(),
            ConversationType::Channel,
            None,
            "row".into(),
            sent_at,
        );
        message.attachments = attachments.to_vec();
        message
    }

    #[test]
    fn queue_continues_with_the_next_audio_and_never_replays() {
        let ids: Vec<AttachmentId> = (0..6).map(|_| AttachmentId::new()).collect();
        let (first, second, photo, deleted, third, last) =
            (&ids[0], &ids[1], &ids[2], &ids[3], &ids[4], &ids[5]);
        let mut removed = message(300, std::slice::from_ref(deleted));
        removed.is_deleted = true;
        let messages = vec![
            // Two audio files in one message play in order.
            message(100, &[first.clone(), second.clone()]),
            message(200, std::slice::from_ref(photo)),
            removed,
            message(400, &[]),
            message(500, std::slice::from_ref(third)),
            message(600, std::slice::from_ref(last)),
        ];
        let is_audio = |id: &AttachmentId| id != photo;
        let next = |after: &AttachmentId| {
            next_audio(&messages, after, is_audio).map(|(message, id)| (message.sent_at, id.clone()))
        };

        assert_eq!(next(first), Some((100, second.clone())));
        // Past a photo, a deleted voice note and a text message.
        assert_eq!(next(second), Some((500, third.clone())));
        assert_eq!(next(third), Some((600, last.clone())));
        assert_eq!(next(last), None, "the last track ends the queue");
        // A track whose message is gone (deleted, expired) has no successor
        // rather than restarting the conversation from its first audio.
        assert_eq!(next(deleted), None);
        assert_eq!(next(&AttachmentId::new()), None);
    }
}

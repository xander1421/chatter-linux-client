use super::ConversationType;
use crate::domain::ids::UserId;
use serde::{Deserialize, Serialize};

/// System message types.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SystemMessage {
    UserJoined(UserId),
    UserLeft(UserId),
    /// Anonymous group leave. The member's group name must be captured before
    /// their roster entry is removed so the notice survives a chat reload.
    AnonGroupUserLeft {
        left_name: Option<String>,
    },
    /// This member is gone from the roster and no verified signed change says
    /// why: "{user} is no longer a member". Never worded as a leave.
    UserGone(UserId),
    ChannelCreated,
    ChannelRenamed {
        old_name: String,
        new_name: String,
    },
    UserAdded {
        by: UserId,
        added: UserId,
    },
    /// Rendered with the moderator as sender: "{by} removed {removed_name}".
    /// The name is snapshotted when the event lands, while the member is still
    /// in the local stores; `None` (unknown) reads "removed a member".
    UserRemoved {
        by: UserId,
        removed: UserId,
        removed_name: Option<String>,
    },
    /// Rendered with the moderator as sender: "{by} banned {banned_name}".
    UserBanned {
        by: UserId,
        banned: UserId,
        banned_name: Option<String>,
    },
    /// Rendered with the former owner as sender: "{from} made {to_name} the
    /// owner". Name snapshotted like [`Self::UserRemoved`]'s.
    OwnershipTransferred {
        to: UserId,
        to_name: Option<String>,
    },
    /// WE were made the owner; rendered with the former owner as sender:
    /// "{from} made you the owner".
    MadeYouOwner,
    /// WE were kicked; rendered with the moderator as sender, so it reads
    /// "{by} removed you" (Signal's wording).
    SelfRemoved {
        by: UserId,
    },
    /// WE were banned; rendered with the moderator as sender: "{by} banned you".
    SelfBanned {
        by: UserId,
    },
    /// We are no longer in this channel and the server kept no reason: a kick
    /// that a sync found after the fact (a kick leaves no record) or a leave
    /// from another of our devices. Rendered with us as sender: "You are no
    /// longer a member of this channel".
    NoLongerMember,
    /// The channel was deleted; rendered with its owner as sender.
    ChannelDeleted,
    /// A message from this peer arrived but could not be decrypted, and the
    /// ciphertext is gone (the inbox acks every polled envelope, so it is not
    /// redelivered). The session has been healed via `DecryptionErrorMessage`,
    /// but THAT message is lost — without this entry the user just sees a gap.
    ///
    /// Signal's own receiver-side answer to a bad decrypt: Signal-Android
    /// inserts a "Delivery issue" conversation update
    /// (`ThreadRecord_delivery_issue`).
    DeliveryIssue,
    /// This peer's identity key changed (a reinstall — or a MITM), and the
    /// message that surfaced it could not be decrypted. Like `DeliveryIssue`
    /// the ciphertext is gone, but unlike `DeliveryIssue` the SESSION cannot
    /// heal itself: no `DecryptionErrorMessage` is sent, resending fails
    /// identically forever, and the conversation stays wedged in both
    /// directions until a human accepts the new key.
    ///
    /// **Deliberately not folded into `DeliveryIssue`.** That variant's text
    /// tells the user the session has been reset and to ask for a resend, which
    /// is exactly the wrong advice here, and its resolution is automatic where
    /// this one requires a trust decision.
    ///
    /// `attributed` mirrors [`crate::error::IdentityChangeSource::is_authenticated`]:
    /// false means the peer id came from server-supplied envelope metadata and
    /// nothing proved it, so the wording must not assert who this was.
    IdentityChanged {
        attributed: bool,
    },
    /// One of OUR messages never reached this member (the system message's
    /// sender): its anonymous-mailbox send kept failing and was given up.
    /// `sent_at` is the undelivered message's timestamp; `preview` a short
    /// excerpt so the line says which message.
    Undelivered {
        sent_at: u64,
        preview: String,
    },
    /// The system message's sender changed this conversation's
    /// disappearing-messages timer (Signal's timer-update notice). `seconds`
    /// is the new value; `0` = turned off.
    DisappearingTimerChanged {
        seconds: u32,
    },
}

impl SystemMessage {
    /// Describe a membership notice in its conversation's terms.
    #[must_use]
    pub fn description_for(&self, conversation_type: &ConversationType) -> String {
        if *conversation_type == ConversationType::AnonGroup {
            match self {
                Self::UserJoined(_) => "joined the group".to_string(),
                Self::AnonGroupUserLeft { .. } => "left the group".to_string(),
                Self::SelfRemoved { .. } => "removed you from the group".to_string(),
                Self::UserRemoved { removed_name, .. } => format!(
                    "removed {} from the group",
                    removed_name.as_deref().unwrap_or("a member")
                ),
                _ => self.description(),
            }
        } else {
            self.description()
        }
    }

    /// Get a human-readable description of the system message.
    #[must_use]
    pub fn description(&self) -> String {
        match self {
            Self::UserJoined(_) => "joined the channel".to_string(),
            Self::UserLeft(_) => "left the channel".to_string(),
            Self::AnonGroupUserLeft { .. } => "left the group".to_string(),
            Self::UserGone(_) => "is no longer a member".to_string(),
            Self::ChannelCreated => "created the channel".to_string(),
            Self::ChannelRenamed { old_name, new_name } => {
                format!(
                    "renamed the channel from \"{}\" to \"{}\"",
                    old_name, new_name
                )
            }
            Self::UserAdded { .. } => "added a member".to_string(),
            Self::UserRemoved { removed_name, .. } => {
                format!("removed {}", removed_name.as_deref().unwrap_or("a member"))
            }
            Self::UserBanned { banned_name, .. } => {
                format!("banned {}", banned_name.as_deref().unwrap_or("a member"))
            }
            Self::OwnershipTransferred { to_name, .. } => {
                format!(
                    "made {} the owner",
                    to_name.as_deref().unwrap_or("a member")
                )
            }
            Self::MadeYouOwner => "made you the owner".to_string(),
            Self::SelfRemoved { .. } => "removed you".to_string(),
            Self::SelfBanned { .. } => "banned you".to_string(),
            Self::NoLongerMember => "are no longer a member of this channel".to_string(),
            Self::ChannelDeleted => "deleted the channel".to_string(),
            // Rendered as "{sender_name} {description}", so this reads
            // "Alice sent a message that couldn't be delivered. ...".
            Self::DeliveryIssue => "sent a message that couldn't be delivered. The secure \
                 session has been reset — ask them to send it again."
                .to_string(),
            // Also rendered as "{sender_name} {description}". The unattributed
            // wording must not state as fact that this was them: the only thing
            // that named them was the server.
            Self::IdentityChanged { attributed: true } => "changed their safety number, so \
                 their message could not be read. Usually this means they reinstalled the \
                 app, but it can also mean someone is intercepting this conversation. \
                 Messages stay blocked until you accept the new safety number."
                .to_string(),
            Self::IdentityChanged { attributed: false } => "may have changed their safety \
                 number, so the message could not be read. The server says it came from \
                 them, but nothing proved that. Compare safety numbers over another channel \
                 before you accept."
                .to_string(),
            // Rendered as "{member_name} {description}".
            Self::Undelivered { preview, .. } => format!(
                "did not get your message \"{preview}\" — it could not be sent after several \
                 tries. Send it again if it mattered."
            ),
            // Rendered as "{sender_name} {description}".
            Self::DisappearingTimerChanged { seconds: 0 } => {
                "turned off disappearing messages".to_string()
            }
            Self::DisappearingTimerChanged { seconds } => format!(
                "set disappearing messages to {}",
                crate::services::disappearing::timer_label(*seconds)
            ),
        }
    }
}

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
    /// "{actor} changed the channel photo" (Telegram's "Channel photo updated").
    ChannelPhotoChanged,
    /// Rendered with the adder as sender: "{by} added {added_name}". The name
    /// is snapshotted like [`Self::UserRemoved`]'s; "you" when it is us.
    UserAdded {
        by: UserId,
        added: UserId,
        added_name: Option<String>,
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
    ///
    /// The predicate alone. The chat row and the sidebar compose it with the
    /// actor via [`Self::notice_line`].
    #[must_use]
    pub fn description_for(&self, conversation_type: &ConversationType) -> String {
        if *conversation_type == ConversationType::AnonGroup {
            match self {
                Self::UserJoined(_) => chatter_i18n::tr("joined the group"),
                Self::AnonGroupUserLeft { .. } => chatter_i18n::tr("left the group"),
                Self::SelfRemoved { .. } => chatter_i18n::tr("removed you from the group"),
                Self::UserRemoved { removed_name, .. } => match removed_name.as_deref() {
                    Some(name) => chatter_i18n::trf("removed {} from the group", &[&name]),
                    None => chatter_i18n::tr("removed a member from the group"),
                },
                _ => self.description(),
            }
        } else {
            self.description()
        }
    }

    /// "{actor} {predicate}" as one sentence, for the chat row and the sidebar.
    #[must_use]
    pub fn notice_line(&self, actor: &str, conversation_type: &ConversationType) -> String {
        if *conversation_type == ConversationType::AnonGroup {
            match self {
                Self::UserJoined(_) => {
                    return chatter_i18n::trf("{} joined the group", &[&actor]);
                }
                Self::AnonGroupUserLeft { .. } => {
                    return chatter_i18n::trf("{} left the group", &[&actor]);
                }
                Self::SelfRemoved { .. } => {
                    return chatter_i18n::trf("{} removed you from the group", &[&actor]);
                }
                Self::UserRemoved { removed_name, .. } => {
                    return match removed_name.as_deref() {
                        Some(name) => {
                            chatter_i18n::trf("{} removed {} from the group", &[&actor, &name])
                        }
                        None => chatter_i18n::trf("{} removed a member from the group", &[&actor]),
                    };
                }
                _ => {}
            }
        }
        match self {
            Self::UserJoined(_) => chatter_i18n::trf("{} joined the channel", &[&actor]),
            Self::UserLeft(_) => chatter_i18n::trf("{} left the channel", &[&actor]),
            Self::AnonGroupUserLeft { .. } => chatter_i18n::trf("{} left the group", &[&actor]),
            Self::UserGone(_) => chatter_i18n::trf("{} is no longer a member", &[&actor]),
            Self::ChannelCreated => chatter_i18n::trf("{} created the channel", &[&actor]),
            Self::ChannelPhotoChanged => {
                chatter_i18n::trf("{} changed the channel photo", &[&actor])
            }
            Self::ChannelRenamed { old_name, new_name } => chatter_i18n::trf(
                "{} renamed the channel from \"{}\" to \"{}\"",
                &[&actor, &old_name, &new_name],
            ),
            Self::UserAdded { added_name, .. } => match added_name.as_deref() {
                Some(name) => chatter_i18n::trf("{} added {}", &[&actor, &name]),
                None => chatter_i18n::trf("{} added a member", &[&actor]),
            },
            Self::UserRemoved { removed_name, .. } => match removed_name.as_deref() {
                Some(name) => chatter_i18n::trf("{} removed {}", &[&actor, &name]),
                None => chatter_i18n::trf("{} removed a member", &[&actor]),
            },
            Self::UserBanned { banned_name, .. } => match banned_name.as_deref() {
                Some(name) => chatter_i18n::trf("{} banned {}", &[&actor, &name]),
                None => chatter_i18n::trf("{} banned a member", &[&actor]),
            },
            Self::OwnershipTransferred { to_name, .. } => match to_name.as_deref() {
                Some(name) => chatter_i18n::trf("{} made {} the owner", &[&actor, &name]),
                None => chatter_i18n::trf("{} made a member the owner", &[&actor]),
            },
            Self::MadeYouOwner => chatter_i18n::trf("{} made you the owner", &[&actor]),
            Self::SelfRemoved { .. } => chatter_i18n::trf("{} removed you", &[&actor]),
            Self::SelfBanned { .. } => chatter_i18n::trf("{} banned you", &[&actor]),
            Self::NoLongerMember => {
                chatter_i18n::trf("{} are no longer a member of this channel", &[&actor])
            }
            Self::ChannelDeleted => chatter_i18n::trf("{} deleted the channel", &[&actor]),
            Self::DeliveryIssue => chatter_i18n::trf(
                "{} sent a message that couldn't be delivered. The secure session has been \
                 reset — ask them to send it again.",
                &[&actor],
            ),
            Self::IdentityChanged { attributed: true } => chatter_i18n::trf(
                "{} changed their safety number, so their message could not be read. Usually \
                 this means they reinstalled the app, but it can also mean someone is \
                 intercepting this conversation. Messages stay blocked until you accept the \
                 new safety number.",
                &[&actor],
            ),
            Self::IdentityChanged { attributed: false } => chatter_i18n::trf(
                "{} may have changed their safety number, so the message could not be read. \
                 The server says it came from them, but nothing proved that. Compare safety \
                 numbers over another channel before you accept.",
                &[&actor],
            ),
            Self::Undelivered { preview, .. } => chatter_i18n::trf(
                "{} did not get your message \"{}\" — it could not be sent after several \
                 tries. Send it again if it mattered.",
                &[&actor, &preview],
            ),
            Self::DisappearingTimerChanged { seconds: 0 } => {
                chatter_i18n::trf("{} turned off disappearing messages", &[&actor])
            }
            Self::DisappearingTimerChanged { seconds } => chatter_i18n::trf(
                "{} set disappearing messages to {}",
                &[&actor, &crate::domain::disappearing::timer_label(*seconds)],
            ),
        }
    }

    /// Get a human-readable description of the system message.
    #[must_use]
    pub fn description(&self) -> String {
        match self {
            Self::UserJoined(_) => chatter_i18n::tr("joined the channel"),
            Self::UserLeft(_) => chatter_i18n::tr("left the channel"),
            Self::AnonGroupUserLeft { .. } => chatter_i18n::tr("left the group"),
            Self::UserGone(_) => chatter_i18n::tr("is no longer a member"),
            Self::ChannelCreated => chatter_i18n::tr("created the channel"),
            Self::ChannelPhotoChanged => chatter_i18n::tr("changed the channel photo"),
            Self::ChannelRenamed { old_name, new_name } => chatter_i18n::trf(
                "renamed the channel from \"{}\" to \"{}\"",
                &[&old_name, &new_name],
            ),
            Self::UserAdded { added_name, .. } => match added_name.as_deref() {
                Some(name) => chatter_i18n::trf("added {}", &[&name]),
                None => chatter_i18n::tr("added a member"),
            },
            Self::UserRemoved { removed_name, .. } => match removed_name.as_deref() {
                Some(name) => chatter_i18n::trf("removed {}", &[&name]),
                None => chatter_i18n::tr("removed a member"),
            },
            Self::UserBanned { banned_name, .. } => match banned_name.as_deref() {
                Some(name) => chatter_i18n::trf("banned {}", &[&name]),
                None => chatter_i18n::tr("banned a member"),
            },
            Self::OwnershipTransferred { to_name, .. } => match to_name.as_deref() {
                Some(name) => chatter_i18n::trf("made {} the owner", &[&name]),
                None => chatter_i18n::tr("made a member the owner"),
            },
            Self::MadeYouOwner => chatter_i18n::tr("made you the owner"),
            Self::SelfRemoved { .. } => chatter_i18n::tr("removed you"),
            Self::SelfBanned { .. } => chatter_i18n::tr("banned you"),
            Self::NoLongerMember => chatter_i18n::tr("are no longer a member of this channel"),
            Self::ChannelDeleted => chatter_i18n::tr("deleted the channel"),
            // Rendered as "{sender_name} {description}", so this reads
            // "Alice sent a message that couldn't be delivered. ...".
            Self::DeliveryIssue => chatter_i18n::tr(
                "sent a message that couldn't be delivered. The secure session has been \
                 reset — ask them to send it again.",
            ),
            // Also rendered as "{sender_name} {description}". The unattributed
            // wording must not state as fact that this was them: the only thing
            // that named them was the server.
            Self::IdentityChanged { attributed: true } => chatter_i18n::tr(
                "changed their safety number, so their message could not be read. Usually \
                 this means they reinstalled the app, but it can also mean someone is \
                 intercepting this conversation. Messages stay blocked until you accept the \
                 new safety number.",
            ),
            Self::IdentityChanged { attributed: false } => chatter_i18n::tr(
                "may have changed their safety number, so the message could not be read. The \
                 server says it came from them, but nothing proved that. Compare safety \
                 numbers over another channel before you accept.",
            ),
            // Rendered as "{member_name} {description}".
            Self::Undelivered { preview, .. } => chatter_i18n::trf(
                "did not get your message \"{}\" — it could not be sent after several tries. \
                 Send it again if it mattered.",
                &[&preview],
            ),
            // Rendered as "{sender_name} {description}".
            Self::DisappearingTimerChanged { seconds: 0 } => {
                chatter_i18n::tr("turned off disappearing messages")
            }
            Self::DisappearingTimerChanged { seconds } => chatter_i18n::trf(
                "set disappearing messages to {}",
                &[&crate::domain::disappearing::timer_label(*seconds)],
            ),
        }
    }
}

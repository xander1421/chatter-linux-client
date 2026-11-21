//! Conversations offered to the OS share sheet (Android Direct Share).
//!
//! Everything published here leaves the app: the system stores each label with
//! the app's shortcuts and shows it inside other apps' share sheets. The policy
//! is therefore decided in this one place and is deliberately narrower than the
//! chat list. It never reveals more than the notification detail level the user
//! already chose:
//!
//! * nothing while signed out, and nothing at all while an app lock is
//!   *enabled* (the lock engages after a timeout nothing here can observe, so
//!   "currently locked" is not a safe test);
//! * nothing at the default level, `None` ("nothing leaks to the OS");
//! * friend DMs from `NameOnly` up: a notification there already shows that
//!   person's name;
//! * channels only at `NameAndMessage`: no level ever shows a channel's name in
//!   a notification, but that top level already lets message previews leave the
//!   app, which a channel name is no more sensitive than;
//! * never secret chats, private groups (the anonymity features), Saved
//!   messages or mesh rooms, and never a channel the user has left;
//! * the few newest, in the chat list's own order (pinned first, then newest
//!   activity), as a name only: no avatar, no preview.
//!
//! The id handed to the OS is the opaque per-account notification key, never a
//! user or channel id, and the same policy gates resolving a tapped target back
//! to a conversation ([`ChatterClient::conversation_for_share_target`]).

use super::*;
use crate::ui::sidebar_data::ChatRowSendData;

/// How many chats the share sheet may show. Android surfaces only a handful.
pub const MAX_SHARE_TARGETS: usize = 4;

/// One chat offered to the system share sheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareTarget {
    /// Opaque per-account key (the notification key); also the shortcut id.
    pub key: String,
    /// What the share sheet shows: the chat's name.
    pub label: String,
}

/// Whether a conversation of `kind` may be offered at `level`. The single
/// place the per-kind rule lives, so publishing and resolving cannot drift.
fn shareable(kind: &ConversationType, level: NotificationDetailLevel) -> bool {
    match kind {
        ConversationType::ServerDm => level != NotificationDetailLevel::None,
        ConversationType::Channel => level == NotificationDetailLevel::NameAndMessage,
        _ => false,
    }
}

/// Which rows may be published, in order. Pure so the privacy rules are tested
/// without a signed-in client.
pub(crate) fn select_share_rows(
    rows: &[ChatRowSendData],
    level: NotificationDetailLevel,
) -> Vec<&ChatRowSendData> {
    rows.iter()
        .filter(|row| shareable(&row.kind, level))
        .filter(|row| !row.display_name.trim().is_empty())
        .take(MAX_SHARE_TARGETS)
        .collect()
}

impl ChatterClient {
    /// An app lock that is enabled can engage while the app sits in the
    /// background, where nothing refreshes the published list. Treat an
    /// unreadable setting as enabled.
    fn app_lock_blocks_sharing(&self) -> bool {
        self.app_lock_is_locked() || self.app_lock_settings().map_or(true, |s| s.enabled)
    }

    /// The chats to publish to the share sheet right now. Empty whenever the
    /// policy in the module docs says nothing may leave the app.
    pub fn share_targets(&self) -> Vec<ShareTarget> {
        if !self.is_logged_in() || self.app_lock_blocks_sharing() {
            return Vec::new();
        }
        let level = self.notification_detail_level();
        if level == NotificationDetailLevel::None {
            return Vec::new();
        }
        // A channel the user has left is still a row, but not somewhere to share.
        let rows: Vec<_> = self
            .sidebar_rows()
            .into_iter()
            .filter(|row| {
                !(row.kind == ConversationType::Channel
                    && row.id.parse().is_ok_and(|id| self.is_former_channel(&id)))
            })
            .collect();
        select_share_rows(&rows, level)
            .into_iter()
            .filter_map(|row| {
                let id = row.id.parse().ok()?;
                let key = self.services.notification_service.current_key(&id).ok()?;
                Some(ShareTarget {
                    key,
                    label: row.display_name.clone(),
                })
            })
            .collect()
    }

    /// Resolve a tapped share target (its opaque key) to a conversation, under
    /// the same policy as publishing: the published list is refreshed
    /// asynchronously, so a stale target (the level lowered, the app lock
    /// enabled, a channel left since) must not stage anything. `None` sends the
    /// caller to the destination picker.
    pub fn conversation_for_share_target(
        &self,
        key: &str,
    ) -> Option<crate::state::ActiveConversation> {
        use crate::state::ActiveConversation;
        if !self.is_logged_in() || self.app_lock_blocks_sharing() {
            return None;
        }
        let level = self.notification_detail_level();
        self.conversation_for_notification_key(key)
            .filter(|conversation| match conversation {
                ActiveConversation::ServerDm { .. } => {
                    shareable(&ConversationType::ServerDm, level)
                }
                ActiveConversation::Channel(id) => {
                    shareable(&ConversationType::Channel, level) && !self.is_former_channel(id)
                }
                _ => false,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: ConversationType, id: &str, name: &str) -> ChatRowSendData {
        ChatRowSendData::new(kind, id, name)
    }

    fn names(rows: &[&ChatRowSendData]) -> Vec<String> {
        rows.iter().map(|r| r.display_name.clone()).collect()
    }

    fn mixed() -> [ChatRowSendData; 8] {
        [
            row(ConversationType::ServerDm, "1", "Alice"),
            row(ConversationType::AnonDm, "2", "Secret peer"),
            row(ConversationType::Channel, "3", "general"),
            row(ConversationType::AnonGroup, "4", "Private group"),
            row(ConversationType::SavedMessages, "5", "Saved messages"),
            row(ConversationType::ServerDm, "6", "Bob"),
            row(ConversationType::Channel, "7", "random"),
            row(ConversationType::ServerDm, "8", "Carol"),
        ]
    }

    #[test]
    fn nothing_is_published_at_the_private_default() {
        assert!(select_share_rows(&mixed(), NotificationDetailLevel::None).is_empty());
    }

    #[test]
    fn name_only_offers_friend_dms_and_never_a_channel_name() {
        // A notification at this level shows a person's name, never a channel's.
        assert_eq!(
            names(&select_share_rows(
                &mixed(),
                NotificationDetailLevel::NameOnly
            )),
            ["Alice", "Bob", "Carol"]
        );
    }

    #[test]
    fn name_and_message_adds_channels_in_chat_list_order_up_to_the_cap() {
        assert_eq!(
            names(&select_share_rows(
                &mixed(),
                NotificationDetailLevel::NameAndMessage
            )),
            ["Alice", "general", "Bob", "random"],
            "cap is {MAX_SHARE_TARGETS}; secret chats, private groups and Saved messages never"
        );
    }

    #[test]
    fn a_row_without_a_name_is_skipped() {
        let rows = [
            row(ConversationType::ServerDm, "1", "  "),
            row(ConversationType::ServerDm, "2", "Bob"),
        ];
        assert_eq!(
            names(&select_share_rows(&rows, NotificationDetailLevel::NameOnly)),
            ["Bob"]
        );
    }

    #[test]
    fn the_resolver_rule_matches_the_publishing_rule() {
        use NotificationDetailLevel::{NameAndMessage, NameOnly, None};
        assert!(!shareable(&ConversationType::ServerDm, None));
        assert!(shareable(&ConversationType::ServerDm, NameOnly));
        assert!(!shareable(&ConversationType::Channel, NameOnly));
        assert!(shareable(&ConversationType::Channel, NameAndMessage));
        for kind in [
            ConversationType::AnonDm,
            ConversationType::AnonGroup,
            ConversationType::SavedMessages,
        ] {
            assert!(!shareable(&kind, NameAndMessage), "{kind:?}");
        }
    }
}

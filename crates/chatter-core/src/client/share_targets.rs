//! Conversations offered to the OS share sheet (Android Direct Share).
//!
//! Everything published here leaves the app: the system stores each label with
//! the app's shortcuts and shows it inside other apps' share sheets. The policy
//! is therefore decided in this one place and is deliberately narrower than the
//! chat list:
//!
//! * never while signed out or app-locked;
//! * never at the default notification detail level, `None` ("nothing leaks to
//!   the OS"): a name is published only where a notification would show it;
//! * identified chats only — friend DMs and channels. Never secret chats,
//!   private groups (the anonymity features), Saved messages or mesh rooms;
//! * the few newest, in the chat list's own order (pinned first, then newest
//!   activity), as a name only: no avatar, no preview.
//!
//! The id handed to the OS is the opaque per-account notification key, never a
//! user or channel id, and the same key resolves a tapped target back to a
//! conversation ([`ChatterClient::conversation_for_share_target`]).

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

/// Which rows may be published, in order. Pure so the privacy rules are tested
/// without a signed-in client.
pub(crate) fn select_share_rows(
    rows: &[ChatRowSendData],
    level: NotificationDetailLevel,
) -> Vec<&ChatRowSendData> {
    if level == NotificationDetailLevel::None {
        return Vec::new();
    }
    rows.iter()
        .filter(|row| {
            matches!(
                row.kind,
                ConversationType::ServerDm | ConversationType::Channel
            )
        })
        .filter(|row| !row.display_name.trim().is_empty())
        .take(MAX_SHARE_TARGETS)
        .collect()
}

impl ChatterClient {
    /// The chats to publish to the share sheet right now. Empty whenever the
    /// policy in the module docs says nothing may leave the app.
    pub fn share_targets(&self) -> Vec<ShareTarget> {
        if !self.is_logged_in() || self.app_lock_is_locked() {
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

    /// Resolve a tapped share target (its opaque key) to a conversation. Only
    /// the kinds [`share_targets`](Self::share_targets) publishes, and never
    /// while locked or signed out; anything else (a stale or foreign key)
    /// returns `None` and the caller falls back to the destination picker.
    pub fn conversation_for_share_target(
        &self,
        key: &str,
    ) -> Option<crate::state::ActiveConversation> {
        if self.app_lock_is_locked() {
            return None;
        }
        self.conversation_for_notification_key(key)
            .filter(|conversation| {
                matches!(
                    conversation,
                    crate::state::ActiveConversation::ServerDm { .. }
                        | crate::state::ActiveConversation::Channel(_)
                )
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

    #[test]
    fn nothing_is_published_at_the_private_default() {
        let rows = [row(ConversationType::ServerDm, "1", "Alice")];
        assert!(select_share_rows(&rows, NotificationDetailLevel::None).is_empty());
    }

    #[test]
    fn only_identified_chats_are_published_newest_first_and_capped() {
        let rows = [
            row(ConversationType::ServerDm, "1", "Alice"),
            row(ConversationType::AnonDm, "2", "Secret peer"),
            row(ConversationType::Channel, "3", "general"),
            row(ConversationType::AnonGroup, "4", "Private group"),
            row(ConversationType::SavedMessages, "5", "Saved messages"),
            row(ConversationType::ServerDm, "6", "Bob"),
            row(ConversationType::Channel, "7", "random"),
            row(ConversationType::ServerDm, "8", "Carol"),
        ];
        for level in [
            NotificationDetailLevel::NameOnly,
            NotificationDetailLevel::NameAndMessage,
        ] {
            assert_eq!(
                names(&select_share_rows(&rows, level)),
                ["Alice", "general", "Bob", "random"],
                "secret chats, private groups and Saved messages are never offered; the cap is {MAX_SHARE_TARGETS}"
            );
        }
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
}

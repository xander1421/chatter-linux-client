//! profile façade — thin delegates to services.

use std::sync::Arc;

use super::{ChatterClient, ClientError};
use crate::domain::message::ConversationType;
use crate::services::current_user::CurrentUserProvider;
use crate::state::ActiveConversation;

/// One Friends › Blocked row (`blocked_users`). `anon` marks an anon-DM peer
/// block; `anon_chat` that its conversation is still stored locally, which is
/// what the row's "Delete chat" removes (`remove_blocked_contact`).
pub struct BlockedContact {
    pub id: crate::domain::ids::UserId,
    pub anon: bool,
    pub anon_chat: bool,
}

/// The result of redeeming a code in Contacts › Add Friend.
pub enum FriendCodeConnection {
    FriendRequest(crate::domain::user::Friendship),
    SecretChat(crate::domain::anon_dm::AnonDm),
}

#[derive(Debug, thiserror::Error)]
pub enum FriendCodeConnectionError {
    #[error("This code isn't valid.")]
    InvalidCode,
    #[error("That's a username — ask for a friend code or username link.")]
    UsernameInsteadOfCode,
    #[error("Server-managed friend codes can't be used in Shield mode. Ask for a private code.")]
    ServerCodeInShield,
    #[error("Secret chats aren't available on this device.")]
    SecretChatUnavailable,
    #[error("You already have a secret chat with them.")]
    ExistingSecretChat(crate::domain::anon_dm::AnonDmId),
    #[error(transparent)]
    FriendRequest(#[from] crate::services::UserError),
    #[error(transparent)]
    SecretChat(#[from] crate::services::anon_dm::AnonDmError),
}

impl FriendCodeConnectionError {
    pub fn user_message(&self) -> String {
        match self {
            Self::FriendRequest(error) => error.user_message(),
            Self::SecretChat(_) => "Couldn't start the secret chat. Try again.".into(),
            _ => self.to_string(),
        }
    }
}

mod anonymous;
mod attachments;
mod blocking;
mod display;
mod friend_codes;
mod own_profile;
mod presence;

#[cfg(test)]
mod tests;

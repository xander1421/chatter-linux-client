//! Secret chats (Telegram-style) outside Shield.
//!
//! A secret chat is an anon DM (`services/anon_dm`) started with a known
//! user: device-bound (never synced or backed up), carried over the isolated
//! onion channel, and listed as its own row beside the friend DM with the
//! same person (`list_dm_sidebar`). Shield keeps its own entry point, the
//! private friend code (`start_anon_dm_conversation_private`).

use super::ChatterClient;
use crate::domain::anon_dm::{AnonDm, AnonDmStatus};
use crate::domain::ids::UserId;
use crate::services::anon_dm::AnonDmError;

/// Why a secret chat could not be started.
#[derive(Debug, thiserror::Error)]
pub enum SecretChatError {
    /// This build cannot reach the server's onion service (no `onion`
    /// feature, or no `.onion` pinned), and a secret chat never goes direct.
    #[error("secret chats are not available in this build")]
    Unavailable,
    #[error("you cannot start a secret chat with yourself")]
    SelfPeer,
    #[error("this user is blocked")]
    Blocked,
    #[error(transparent)]
    AnonDm(#[from] AnonDmError),
}

impl ChatterClient {
    /// Whether this build can carry secret chats: the onion channel exists
    /// (same check as Shield's availability).
    pub fn secret_chats_available(&self) -> bool {
        self.shield_available()
    }

    /// Whether the profile / chat header should offer "Start secret chat"
    /// for `peer`. Not in Shield: Shield's own anon-DM entry point is the
    /// private friend code, and its screens are unchanged.
    pub fn can_start_secret_chat(&self, peer: &UserId) -> bool {
        self.secret_chats_available()
            && !self.anon_mode_enabled()
            && self.secret_chat_peer_check(peer).is_ok()
    }

    /// Start a secret chat with `peer`, or return the one that already exists
    /// (the anon DM service keeps one per peer). A declined one is restarted,
    /// and so is our own request that provably never left this device (it
    /// failed before the send RPC); one that may have left is returned as is,
    /// never blindly resent. The caller opens the returned conversation
    /// (`ActiveConversation::AnonDm`).
    pub async fn start_secret_chat(&self, peer: UserId) -> Result<AnonDm, SecretChatError> {
        self.secret_chat_peer_check(&peer)?;
        if !self.secret_chats_available() {
            return Err(SecretChatError::Unavailable);
        }
        if let Some(existing) = self
            .find_anon_dm_by_peer(&peer)
            .and_then(|id| self.get_anon_dm_conversation(&id))
        {
            match existing.status {
                AnonDmStatus::Pending if existing.is_unsent_request() => {}
                AnonDmStatus::Active | AnonDmStatus::Pending => return Ok(existing),
                AnonDmStatus::Blocked => return Err(SecretChatError::Blocked),
                AnonDmStatus::Declined => {}
            }
        }
        Ok(self.start_anon_dm(peer, None).await?)
    }

    fn secret_chat_peer_check(&self, peer: &UserId) -> Result<(), SecretChatError> {
        if self.session_current_user_id().as_ref() == Some(peer) {
            return Err(SecretChatError::SelfPeer);
        }
        if self.is_profile_user_blocked(peer) {
            return Err(SecretChatError::Blocked);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::ConversationType;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    fn build_test_client() -> (tempfile::TempDir, Arc<ChatterClient>) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        (dir, client)
    }

    fn sign_in(client: &ChatterClient) -> UserId {
        let me = UserId::new();
        client
            .messages()
            .settings_repo
            .set_string(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &me.to_string(),
            )
            .unwrap();
        me
    }

    /// A secret chat (anon DM) is its own row in the normal-mode chat list,
    /// beside the friend DM, and stays listed in Shield as before.
    #[tokio::test]
    async fn secret_chat_is_listed_outside_shield() {
        let (_dir, client) = build_test_client();
        sign_in(&client);
        let peer = UserId::new();
        let mut dm = AnonDm::new_initiated(peer);
        dm.status = AnonDmStatus::Active;
        client
            .anon_dms()
            .anon_dm_repo()
            .insert_conversation(&dm)
            .unwrap();

        assert!(!client.anon_mode_enabled());
        assert!(client.visible_in_mode(&ConversationType::AnonDm));
        let has_row = |c: &ChatterClient| {
            c.sidebar_rows()
                .iter()
                .any(|r| r.kind == ConversationType::AnonDm && r.id == dm.id.to_string())
        };
        assert!(has_row(&client));
        assert!(
            client
                .sidebar_row_keys()
                .contains(&(dm.id.to_string(), ConversationType::AnonDm))
        );
        // Opening it needs no Shield.
        assert_eq!(
            client.resolve_dm_conversation(&dm.id.to_string()),
            Some(crate::state::ActiveConversation::AnonDm(dm.id))
        );

        client.anon_mode.store(true, Ordering::Release);
        assert!(client.visible_in_mode(&ConversationType::AnonDm));
        assert!(has_row(&client));
    }

    /// Self and blocked peers are refused before anything else; otherwise a
    /// build without the onion channel refuses rather than going direct.
    #[tokio::test]
    async fn start_secret_chat_refusals() {
        let (_dir, client) = build_test_client();
        let me = sign_in(&client);

        assert!(matches!(
            client.start_secret_chat(me).await,
            Err(SecretChatError::SelfPeer)
        ));
        assert!(!client.can_start_secret_chat(&me));

        let blocked = UserId::new();
        client.block_anon_dm_user(&blocked, None).unwrap();
        assert!(matches!(
            client.start_secret_chat(blocked).await,
            Err(SecretChatError::Blocked)
        ));
        assert!(!client.can_start_secret_chat(&blocked));

        let peer = UserId::new();
        if !client.secret_chats_available() {
            assert!(matches!(
                client.start_secret_chat(peer).await,
                Err(SecretChatError::Unavailable)
            ));
            assert!(!client.can_start_secret_chat(&peer));
            // Nothing was stored.
            assert!(client.find_anon_dm_by_peer(&peer).is_none());
        }
        // Shield never offers it: its entry point is the private friend code.
        client.anon_mode.store(true, Ordering::Release);
        assert!(!client.can_start_secret_chat(&peer));
    }
}

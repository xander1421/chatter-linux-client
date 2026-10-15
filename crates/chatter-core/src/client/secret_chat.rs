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

impl ChatterClient {
    /// The secret chat was opened: fetch a peer avatar that never came
    /// through (Tor failures outlasted the backoff, or the app restarted
    /// meanwhile). Runs in the background; a new avatar is announced like
    /// any profile update.
    pub fn retry_secret_chat_avatar(&self, conversation_id: crate::domain::anon_dm::AnonDmId) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        #[cfg(feature = "onion")]
        {
            let pool = self.services.grpc_pool.clone();
            tokio::spawn(async move {
                if let Err(error) = pool.preconnect_secret().await {
                    tracing::debug!(%error, "secret onion preconnect will retry on use");
                }
            });
        }
        let (service, bus) = (self.anon_dms().clone(), self.event_bus().clone());
        tokio::spawn(async move {
            match service.retry_pending_peer_avatar(&conversation_id).await {
                Ok(Some(avatar)) => {
                    if let Ok(Some(dm)) = service.get_conversation(&conversation_id) {
                        bus.emit_domain(crate::inbox::DomainEvent::AnonDmProfileUpdated {
                            conversation_id,
                            user_id: dm.peer_id,
                            display_name: None,
                            avatar: Some(avatar),
                            avatar_url: None,
                            bio: None,
                            timestamp: crate::util::time::TimestampNanos::now().as_u64(),
                        });
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "Failed to store secret chat avatar"),
            }
        });
    }
}

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

/// Why a secret chat's composer is closed. A chat that is not one of these
/// (accepted, not blocked) takes messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretChatGate {
    /// The peer asked; we have not answered. The chat view offers Accept /
    /// Decline, like Contacts › Requests › Private.
    AwaitingOurAnswer,
    /// We asked; the peer has not accepted yet.
    AwaitingPeer,
    Declined,
    Blocked,
}

impl SecretChatGate {
    /// The line that replaces the composer.
    pub fn notice(&self) -> &'static str {
        match self {
            Self::AwaitingOurAnswer => "Secret chat request. Accept it to reply.",
            Self::AwaitingPeer => "Waiting for them to accept your secret chat request.",
            Self::Declined => "This secret chat request was declined.",
            Self::Blocked => "You blocked this secret chat.",
        }
    }

    /// Whether the chat view shows Accept / Decline.
    pub fn offers_answer(&self) -> bool {
        matches!(self, Self::AwaitingOurAnswer)
    }
}

impl ChatterClient {
    /// What keeps the composer of secret chat `id` closed, or `None` when it
    /// is open (or `id` is no secret chat). Sending is refused for the same
    /// states (`AnonDmService::send_message` requires `is_ready`).
    pub fn secret_chat_gate(&self, id: &crate::domain::anon_dm::AnonDmId) -> Option<SecretChatGate> {
        let dm = self.get_anon_dm_conversation(id)?;
        match dm.status {
            AnonDmStatus::Active if dm.is_ready() => None,
            AnonDmStatus::Active => Some(SecretChatGate::AwaitingPeer),
            AnonDmStatus::Pending if dm.initiated_by_us => Some(SecretChatGate::AwaitingPeer),
            AnonDmStatus::Pending => Some(SecretChatGate::AwaitingOurAnswer),
            AnonDmStatus::Declined => Some(SecretChatGate::Declined),
            AnonDmStatus::Blocked => Some(SecretChatGate::Blocked),
        }
    }

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

    /// Start a secret chat with `peer`, or return the live one that already
    /// exists (`find_anon_dm_by_peer`). A declined one is replaced,
    /// and so is our own request that provably never left this device; one
    /// whose send outcome is unknown is resent under its original request id
    /// (the server queues it at most once); one the server queued is returned
    /// as is. The caller opens the returned conversation
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
                AnonDmStatus::Pending
                    if existing.is_unsent_request() || existing.is_uncertain_request() => {}
                AnonDmStatus::Active => return Ok(existing),
                AnonDmStatus::Pending => {}
                AnonDmStatus::Blocked => return Err(SecretChatError::Blocked),
                AnonDmStatus::Declined => {}
            }
        }
        let mut dm = self.start_anon_dm(peer, None).await?;
        // This entry point has a known friend on this device. Keep that local
        // card on the conversation so both the pending sidebar row and the
        // header can render it before the peer sends their own profile.
        let name = self.display_name(&peer);
        let name = (name != self.fallback_display_name(&peer)).then_some(name);
        let avatar = self
            .get_profile_local(&peer)
            .and_then(|profile| profile.avatar_bytes);
        if dm.peer_display_name.is_none() && name.is_some()
            || dm.peer_avatar.is_none() && avatar.is_some()
        {
            dm.peer_display_name = dm.peer_display_name.or(name);
            dm.peer_avatar = dm.peer_avatar.or(avatar);
            self.anon_dms()
                .anon_dm_repo()
                .update_conversation(&dm)
                .map_err(AnonDmError::from)?;
        }
        Ok(dm)
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
    use crate::services::anon_dm::PeerEnd;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    fn build_test_client() -> (tempfile::TempDir, Arc<ChatterClient>) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
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

    fn insert(client: &ChatterClient, dm: &AnonDm) {
        client
            .anon_dms()
            .anon_dm_repo()
            .insert_conversation(dm)
            .unwrap();
    }

    fn insert_mailbox(client: &ChatterClient, dm: &AnonDm) {
        client
            .anon_dms()
            .anon_dm_repo()
            .insert_mailbox(&crate::domain::anon_dm::AnonDmMailbox {
                id: dm.id.to_string(),
                conversation_id: dm.id,
                public_key: [4; 32],
                private_key: [5; 64],
                token_id: "t".into(),
                created_at: chrono::Utc::now(),
                expires_at: chrono::Utc::now() + chrono::Duration::days(1),
            })
            .unwrap();
    }

    /// RC108 O2: an unanswered request opened from the chat list shows its
    /// state with Accept / Decline instead of a composer; our own request
    /// waits for the peer; an accepted chat takes messages.
    #[tokio::test]
    async fn composer_gate_follows_request_state() {
        use crate::domain::ids::ChannelId;
        let (_dir, client) = build_test_client();
        sign_in(&client);

        let incoming = AnonDm::from_request(
            crate::domain::anon_dm::AnonDmId::new(),
            UserId::new(),
            [1; 32],
        );
        insert(&client, &incoming);
        let gate = client.secret_chat_gate(&incoming.id).unwrap();
        assert_eq!(gate, SecretChatGate::AwaitingOurAnswer);
        assert!(gate.offers_answer());
        let as_channel: ChannelId = incoming.id.into();
        assert_eq!(client.channel_send_notice(&as_channel), gate.notice());
        assert_eq!(
            client.refresh_channel_send_notice(&as_channel).await,
            gate.notice()
        );

        let outgoing = AnonDm::new_initiated(UserId::new());
        insert(&client, &outgoing);
        let gate = client.secret_chat_gate(&outgoing.id).unwrap();
        assert_eq!(gate, SecretChatGate::AwaitingPeer);
        assert!(!gate.offers_answer());

        let mut active = AnonDm::new_initiated(UserId::new());
        active.status = AnonDmStatus::Active;
        active.peer_mailbox_pk = Some([2; 32]);
        insert(&client, &active);
        assert_eq!(client.secret_chat_gate(&active.id), None);
        assert_eq!(client.channel_send_notice(&active.id.into()), "");

        client
            .anon_dms()
            .block_conversation(&active.id)
            .unwrap();
        assert_eq!(
            client.secret_chat_gate(&active.id),
            Some(SecretChatGate::Blocked)
        );
        // Not a secret chat at all: no gate.
        assert_eq!(
            client.secret_chat_gate(&crate::domain::anon_dm::AnonDmId::new()),
            None
        );
    }

    /// RC108 O5: our own last message in a secret-chat row reads "You: …",
    /// like every other row.
    #[tokio::test]
    async fn own_last_message_in_secret_chat_row_has_you_prefix() {
        use crate::domain::message::Message;
        let (_dir, client) = build_test_client();
        let me = sign_in(&client);
        client.auth_state().set_logged_in(me, 1, "me".into());
        let mut dm = AnonDm::new_initiated(UserId::new());
        dm.status = AnonDmStatus::Active;
        dm.peer_mailbox_pk = Some([2; 32]);
        insert(&client, &dm);
        let msg = Message::new_text(
            dm.id.into(),
            ConversationType::AnonDm,
            Some(me),
            "new s25-to-s9".into(),
            1,
        );
        client.messages().message_repo.insert(&msg).unwrap();
        let row = client
            .list_dm_sidebar()
            .into_iter()
            .find(|r| r.id == dm.id.to_string())
            .unwrap();
        assert_eq!(row.last_message, "You: new s25-to-s9");
    }

    /// RC108 O4: deleting a secret chat ends it for both sides. An accepted
    /// chat must tell the peer first, over Tor: with no route (this build)
    /// nothing is deleted. A request nobody accepted has no peer to tell and
    /// goes locally, mailbox included.
    #[tokio::test]
    async fn delete_ends_secret_chat_for_both_or_keeps_it() {
        use crate::state::ActiveConversation;
        let (_dir, client) = build_test_client();
        sign_in(&client);
        client
            .messages()
            .settings_repo
            .set(
                crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID,
                &1u32,
            )
            .unwrap();

        let mut active = AnonDm::new_initiated(UserId::new());
        active.status = AnonDmStatus::Active;
        active.peer_mailbox_pk = Some([2; 32]);
        active.peer_device_id = Some(1);
        insert(&client, &active);
        insert_mailbox(&client, &active);
        if !client.secret_chats_available() {
            assert!(
                client
                    .delete_conversation(ActiveConversation::AnonDm(active.id))
                    .await
                    .is_err()
            );
            assert!(client.get_anon_dm_conversation(&active.id).is_some());
            assert!(
                client
                    .anon_dms()
                    .anon_dm_repo()
                    .get_mailbox(&active.id)
                    .unwrap()
                    .is_some()
            );
        }

        let pending = AnonDm::new_initiated(UserId::new());
        insert(&client, &pending);
        insert_mailbox(&client, &pending);
        client
            .delete_conversation(ActiveConversation::AnonDm(pending.id))
            .await
            .unwrap();
        assert!(client.get_anon_dm_conversation(&pending.id).is_none());
        assert!(
            client
                .anon_dms()
                .anon_dm_repo()
                .get_mailbox(&pending.id)
                .unwrap()
                .is_none()
        );
    }

    /// The peer's end signal deletes our copy; one from anyone else touches
    /// nothing.
    #[tokio::test]
    async fn only_the_peer_ends_a_secret_chat() {
        let (_dir, client) = build_test_client();
        sign_in(&client);
        let peer = UserId::new();
        let mut dm = AnonDm::new_initiated(peer);
        dm.status = AnonDmStatus::Active;
        dm.peer_mailbox_pk = Some([2; 32]);
        insert(&client, &dm);
        insert_mailbox(&client, &dm);
        let svc = client.anon_dms();

        assert_eq!(
            svc.handle_peer_end(&dm.id, &UserId::new()).await.unwrap(),
            PeerEnd::NotPeer
        );
        assert!(client.get_anon_dm_conversation(&dm.id).is_some());

        assert_eq!(svc.handle_peer_end(&dm.id, &peer).await.unwrap(), PeerEnd::Ended);
        assert!(client.get_anon_dm_conversation(&dm.id).is_none());
        assert!(svc.anon_dm_repo().get_mailbox(&dm.id).unwrap().is_none());
        // Already gone: nothing to do.
        assert_eq!(svc.handle_peer_end(&dm.id, &peer).await.unwrap(), PeerEnd::NotPeer);
    }

    /// An end signal on our own unanswered request is the peer declining it:
    /// the chat stays, Declined (its composer says so), and its mailbox goes.
    #[tokio::test]
    async fn peer_end_on_our_request_marks_it_declined() {
        let (_dir, client) = build_test_client();
        sign_in(&client);
        let peer = UserId::new();
        let dm = AnonDm::new_initiated(peer);
        insert(&client, &dm);
        insert_mailbox(&client, &dm);
        let svc = client.anon_dms();

        assert_eq!(svc.handle_peer_end(&dm.id, &peer).await.unwrap(), PeerEnd::Declined);
        let after = client.get_anon_dm_conversation(&dm.id).unwrap();
        assert_eq!(after.status, AnonDmStatus::Declined);
        assert_eq!(client.secret_chat_gate(&dm.id), Some(SecretChatGate::Declined));
        assert!(svc.anon_dm_repo().get_mailbox(&dm.id).unwrap().is_none());
        // A Declined chat is no longer "the chat with this peer".
        assert!(client.find_anon_dm_by_peer(&peer).is_none());
    }

    /// Declining tells the initiator over Tor first and fails closed: with no
    /// Tor route nothing is deleted, so the user can retry.
    #[tokio::test]
    async fn decline_fails_closed_without_tor() {
        let (_dir, client) = build_test_client();
        sign_in(&client);
        if client.secret_chats_available() {
            return;
        }
        let peer = UserId::new();
        let mut dm = AnonDm::from_request(crate::domain::anon_dm::AnonDmId::new(), peer, [3; 32]);
        dm.peer_device_id = Some(1);
        insert(&client, &dm);

        assert!(client.decline_anon_dm(&dm.id).await.is_err());
        assert_eq!(
            client.get_anon_dm_conversation(&dm.id).unwrap().status,
            AnonDmStatus::Pending
        );
    }

    /// Several rows for one peer (RC110 retest: a stale Declined chat next to
    /// the live one): the peer lookup is deterministic and never the
    /// Declined one — Active first, then Blocked, then Pending, newest first.
    #[tokio::test]
    async fn peer_lookup_with_several_chats_picks_the_live_one() {
        let (_dir, client) = build_test_client();
        sign_in(&client);
        let peer = UserId::new();
        let at = |secs: i64| chrono::Utc::now() + chrono::Duration::seconds(secs);
        let row = |status, secs| {
            let mut dm = AnonDm::new_initiated(peer);
            dm.status = status;
            dm.created_at = at(secs);
            dm
        };

        let declined = row(AnonDmStatus::Declined, 30);
        insert(&client, &declined);
        assert!(client.find_anon_dm_by_peer(&peer).is_none());

        let pending = row(AnonDmStatus::Pending, 20);
        insert(&client, &pending);
        assert_eq!(client.find_anon_dm_by_peer(&peer), Some(pending.id));

        let blocked = row(AnonDmStatus::Blocked, 0);
        insert(&client, &blocked);
        assert_eq!(client.find_anon_dm_by_peer(&peer), Some(blocked.id));

        let old_active = row(AnonDmStatus::Active, -20);
        let new_active = row(AnonDmStatus::Active, -10);
        insert(&client, &old_active);
        insert(&client, &new_active);
        for _ in 0..5 {
            assert_eq!(client.find_anon_dm_by_peer(&peer), Some(new_active.id));
        }
    }

    /// Starting a secret chat clears the peer's Declined ones, whatever then
    /// happens to the new request (here it fails: no Tor route).
    #[tokio::test]
    async fn restart_drops_declined_chats() {
        let (_dir, client) = build_test_client();
        sign_in(&client);
        let peer = UserId::new();
        let mut declined = AnonDm::new_initiated(peer);
        declined.status = AnonDmStatus::Declined;
        insert(&client, &declined);

        let _ = client.anon_dms().start_conversation(peer, None).await;
        assert!(client.get_anon_dm_conversation(&declined.id).is_none());
    }
}

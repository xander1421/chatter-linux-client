//! Key rolling.
//!
//! Ephemeral key rotation and expiration for mailbox forward secrecy.
//!
//! Periodically rotates mailbox keys so that old mailboxes are eventually forgotten
//! by the server, limiting the window during which the server can correlate message
//! patterns.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::domain::{ChannelId, UserId};
use crate::error::MailboxError;
use crate::mailbox::TokenManager;
use crate::mailbox::dm::{DmConversationManager, DmMailbox};
use crate::mailbox::group::{GroupMailbox, GroupMailboxManager};
use crate::network::grpc::client_pool::GrpcClientPool;

/// Default rotation check interval (30 minutes). Must be ≪ threshold so
/// proactive rotation always fires inside the overlap window.
pub const DEFAULT_CHECK_INTERVAL_SECS: u64 = 1800;

/// Default threshold for rotation (12 hours before expiry). With a 48h TTL
/// this yields a ~36h effective epoch and a 12h old/new overlap window.
/// Invariant: TTL − threshold must exceed 2×check_interval + offline slack.
pub const DEFAULT_ROTATION_THRESHOLD_HOURS: i64 = 12;

/// Result of a mailbox rotation.
#[derive(Debug)]
pub struct RotationResult {
    /// The conversation/group that was rotated.
    pub target_id: RotationTarget,
    /// The new mailbox that was created.
    pub new_mailbox_public_key: [u8; 32],
    /// When the old mailbox will be deleted.
    pub old_mailbox_deletion_at: DateTime<Utc>,
}

/// Target of a rotation operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotationTarget {
    /// DM conversation with a peer.
    Dm(UserId),
    /// Group mailbox.
    Group(ChannelId),
}

/// Pending rotation notification to be sent to peers.
#[derive(Debug, Clone)]
pub struct RotationNotification {
    /// Target (peer for DM, members for group).
    pub target: RotationTarget,
    /// New mailbox public key to send.
    pub new_mailbox_public_key: [u8; 32],
    /// When the old mailbox expires.
    pub old_mailbox_expires_at: DateTime<Utc>,
}

/// Key rolling service for mailbox forward secrecy.
///
/// Periodically rotates mailbox keys to limit the window during which
/// the server can correlate message patterns.
pub struct KeyRollingService {
    token_manager: Arc<TokenManager>,
    dm_manager: Arc<DmConversationManager>,
    group_manager: Arc<GroupMailboxManager>,
    /// For creating the rotated mailbox server-side (anonymous, no auth).
    grpc_pool: Arc<GrpcClientPool>,
}

impl KeyRollingService {
    /// Create a new KeyRollingService.
    pub fn new(
        token_manager: Arc<TokenManager>,
        dm_manager: Arc<DmConversationManager>,
        group_manager: Arc<GroupMailboxManager>,
        grpc_pool: Arc<GrpcClientPool>,
    ) -> Self {
        Self {
            token_manager,
            dm_manager,
            group_manager,
            grpc_pool,
        }
    }

    /// Check which DM conversations need rotation.
    ///
    /// Returns list of peer IDs whose mailboxes expire within rotation threshold.
    pub fn get_dm_rotation_candidates(&self) -> Result<Vec<UserId>, MailboxError> {
        self.dm_manager
            .get_expiring_mailboxes(DEFAULT_ROTATION_THRESHOLD_HOURS)
    }

    /// Rotate a DM mailbox for a conversation.
    ///
    /// Creates a new mailbox, schedules old one for deletion, and returns
    /// notification that caller should send to peer.
    ///
    /// # Arguments
    /// * `peer_id` - The conversation peer
    ///
    /// # Returns
    /// RotationNotification to send to peer, or error if rotation fails.
    pub async fn rotate_dm_mailbox(
        &self,
        peer_id: &UserId,
    ) -> Result<RotationNotification, MailboxError> {
        // 1. Load current conversation state
        let conversation = self
            .dm_manager
            .load_conversation(peer_id)?
            .ok_or(MailboxError::ConversationNotFound)?;

        let old_mailbox = conversation
            .our_mailbox
            .ok_or(MailboxError::NoMailboxConfigured)?;

        // 2. Atomically claim a fresh token for the new mailbox
        let token = self
            .token_manager
            .claim_available_token()?
            .ok_or(MailboxError::NoTokensAvailable)?;

        // 3. Create the new mailbox server-side using the TOKEN-BOUND keypair.
        //    The blind signature signs the token's public key, so the mailbox key
        //    MUST be the token key (a fresh random key would be rejected) — this is
        //    also exactly what keeps creation unlinkable to the account.
        if token.public_key.len() != 32 || token.private_key.len() != 64 {
            return Err(MailboxError::InvalidServerKey(
                "rotation token key size".to_string(),
            ));
        }
        let mut client = self.grpc_pool.anonymous_mailbox_client();
        let response = client
            .create_ephemeral_mailbox(
                token.public_key.clone(),
                token.signature.clone(),
                crate::mailbox::dm::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS,
                token.randomizer.clone().unwrap_or_default(),
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // 4. Build the token-bound mailbox (key copied from the token, id from server).
        let mut public_key = [0u8; 32];
        let mut private_key = [0u8; 64];
        public_key.copy_from_slice(&token.public_key);
        private_key.copy_from_slice(&token.private_key);
        // A fresh X25519 encryption key for the new mailbox, never the
        // signing key's conversion.
        let (encryption_private_key, _) =
            crate::crypto::envelope_crypto::generate_mailbox_encryption_key();
        let new_mailbox = DmMailbox {
            id: response.mailbox_id.clone(),
            public_key,
            private_key,
            encryption_private_key,
            signature: token.signature.clone(),
            randomizer: token.randomizer.clone(),
            token_id: token.id.clone(),
            created_at: Utc::now(),
            expires_at: Utc::now()
                + chrono::Duration::seconds(
                    crate::mailbox::dm::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS as i64,
                ),
            last_polled_at: None,
        };

        // 5. Save the new mailbox ALONGSIDE the old one (schema now allows two rows).
        //    The old mailbox is NOT deleted early — it stays pollable until its own
        //    expiry, giving a loss-free overlap for a peer that hasn't switched yet.
        //    Sends target the newest (load_our_mailbox_internal ORDER BY created_at).
        self.dm_manager.save_our_mailbox(peer_id, &new_mailbox)?;

        // 6. Return notification; the caller sends a DmMailboxRotation to the peer's
        //    CURRENT mailbox so they switch to depositing into the new one.
        Ok(RotationNotification {
            target: RotationTarget::Dm(*peer_id),
            new_mailbox_public_key: public_key,
            old_mailbox_expires_at: old_mailbox.expires_at,
        })
    }

    /// Check which group mailboxes need rotation.
    ///
    /// Returns list of group IDs whose mailboxes expire within rotation threshold.
    pub fn get_group_rotation_candidates(&self) -> Result<Vec<ChannelId>, MailboxError> {
        self.group_manager
            .get_expiring_group_mailboxes(DEFAULT_ROTATION_THRESHOLD_HOURS)
    }

    /// Rotate a group mailbox.
    ///
    /// Similar to DM rotation but returns notification for all group members.
    pub async fn rotate_group_mailbox(
        &self,
        group_id: &ChannelId,
    ) -> Result<RotationNotification, MailboxError> {
        // 1. Load current group mailbox state
        let state = self
            .group_manager
            .load_group_state(group_id)?
            .ok_or(MailboxError::GroupNotFound)?;

        let old_mailbox = state.our_mailbox.ok_or(MailboxError::NoMailboxConfigured)?;

        // 2. Atomically claim a fresh token
        let token = self
            .token_manager
            .claim_available_token()?
            .ok_or(MailboxError::NoTokensAvailable)?;

        // 3. Create the new group mailbox server-side using the TOKEN-BOUND key
        //    (a fresh random key would fail the blind-signature check).
        if token.public_key.len() != 32 || token.private_key.len() != 64 {
            return Err(MailboxError::InvalidServerKey(
                "rotation token key size".to_string(),
            ));
        }
        let mut client = self.grpc_pool.anonymous_mailbox_client();
        let response = client
            .create_ephemeral_mailbox(
                token.public_key.clone(),
                token.signature.clone(),
                crate::mailbox::dm::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS,
                token.randomizer.clone().unwrap_or_default(),
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // 4. Build the token-bound group mailbox.
        let mut public_key = [0u8; 32];
        let mut private_key = [0u8; 64];
        public_key.copy_from_slice(&token.public_key);
        private_key.copy_from_slice(&token.private_key);
        let new_mailbox = GroupMailbox {
            id: response.mailbox_id.clone(),
            group_id: *group_id,
            public_key,
            private_key,
            signature: token.signature.clone(),
            randomizer: token.randomizer.clone(),
            token_id: token.id.clone(),
            created_at: Utc::now(),
            expires_at: Utc::now()
                + chrono::Duration::seconds(
                    crate::mailbox::dm::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS as i64,
                ),
            last_polled_at: None,
        };

        // 5. Save alongside the old (old expires naturally → loss-free overlap).
        self.group_manager.save_our_mailbox(&new_mailbox)?;

        // 6. Return notification; caller re-announces via GroupMailboxAddress.
        //    This method does not pick recipients. Membership is not consulted
        //    here, and a departed member's mailbox row is not deleted on leave,
        //    so the caller must announce only to [`group_rotation_recipients`].
        Ok(RotationNotification {
            target: RotationTarget::Group(*group_id),
            new_mailbox_public_key: public_key,
            old_mailbox_expires_at: old_mailbox.expires_at,
        })
    }

    /// One scheduler tick: expiring DM mailboxes, then expiring group
    /// mailboxes. A failure to list one kind does not drop the other.
    pub fn scheduler_due(&self) -> Vec<RotationTarget> {
        let dm = match self.get_dm_rotation_candidates() {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!("KeyRolling: failed to list DM rotation candidates: {}", e);
                Vec::new()
            }
        };
        let groups = match self.get_group_rotation_candidates() {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(
                    "KeyRolling: failed to list group rotation candidates: {}",
                    e
                );
                Vec::new()
            }
        };
        schedule_rotation_targets(dm, groups)
    }

    /// Mailbox public keys we can deposit into anonymously, keyed by member.
    /// One row per user (the latest announcement). Absent after
    /// [`GroupMailboxManager::drop_departed_member_mailboxes`].
    pub fn member_mailbox_keys(
        &self,
        group_id: &ChannelId,
    ) -> Result<HashMap<UserId, [u8; 32]>, MailboxError> {
        let Some(state) = self.group_manager.load_group_state(group_id)? else {
            return Ok(HashMap::new());
        };
        Ok(state
            .member_mailboxes
            .iter()
            .map(|(id, mailbox)| (*id, mailbox.public_key))
            .collect())
    }

    /// See [`crate::mailbox::group::GroupMailboxManager::drop_departed_member_mailboxes`].
    pub fn drop_departed_member_mailboxes(
        &self,
        group_id: &ChannelId,
        current: &[UserId],
    ) -> Result<u32, MailboxError> {
        self.group_manager
            .drop_departed_member_mailboxes(group_id, current)
    }

    // Old mailboxes are not deleted explicitly: after rotation the old row stays
    // pollable (loss-free overlap for a peer that hasn't switched) until its own
    // expiry, then the server reaps it by TTL and the client's cleanup_expired
    // drops the local row. There is no server Delete RPC and none is needed.

    /// Get all rotation candidates (DM + group).
    pub fn get_all_rotation_candidates(&self) -> Result<Vec<RotationTarget>, MailboxError> {
        Ok(schedule_rotation_targets(
            self.get_dm_rotation_candidates()?,
            self.get_group_rotation_candidates()?,
        ))
    }

    /// Check if any rotations are needed.
    pub fn has_pending_rotations(&self) -> Result<bool, MailboxError> {
        Ok(!self.get_all_rotation_candidates()?.is_empty())
    }
}

/// DM candidates first, then group candidates. The key-rolling loop rotates
/// exactly this list, on one interval, so a group mailbox is not a second
/// schedule.
pub fn schedule_rotation_targets(dm: Vec<UserId>, groups: Vec<ChannelId>) -> Vec<RotationTarget> {
    let mut due = Vec::with_capacity(dm.len() + groups.len());
    due.extend(dm.into_iter().map(RotationTarget::Dm));
    due.extend(groups.into_iter().map(RotationTarget::Group));
    due
}

/// One device that may be told the rotated group mailbox address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupRotationRecipient {
    pub user_id: UserId,
    pub device_id: u32,
}

/// Devices that receive a rotated group mailbox key.
///
/// `current_members` is the entitlement (local channel membership). A
/// departed member is absent from it even when `session_devices` still has
/// their sessions and their mailbox row has not been deleted. Every session
/// device of each remaining member is included, so those devices converge on
/// the new address. `self_id` is excluded, including our other devices: this
/// device already holds the key, and each of our devices rotates its own
/// mailbox.
///
/// A current member with no entry in `session_devices` is omitted. Learning
/// their devices would be a prekey-bundle fetch, an identified RPC that names
/// them on the anonymous rotation path.
pub fn group_rotation_recipients(
    self_id: &UserId,
    current_members: &[UserId],
    session_devices: &[(UserId, u32)],
) -> Vec<GroupRotationRecipient> {
    use std::collections::HashSet;
    let entitled: HashSet<UserId> = current_members
        .iter()
        .copied()
        .filter(|id| id != self_id)
        .collect();
    let mut recipients: Vec<GroupRotationRecipient> = session_devices
        .iter()
        .copied()
        .filter(|(user_id, _)| entitled.contains(user_id))
        .map(|(user_id, device_id)| GroupRotationRecipient { user_id, device_id })
        .collect();
    recipients.sort_by(|a, b| (a.user_id.0, a.device_id).cmp(&(b.user_id.0, b.device_id)));
    recipients.dedup();
    recipients
}

/// Sealed-sender plaintext for a group mailbox rotation: a
/// `GroupMailboxAddress` padded to a server bucket (256/512/1024/4096).
pub fn group_mailbox_rotation_plaintext(
    group_id: &ChannelId,
    new_mailbox_pk: &[u8; 32],
) -> Vec<u8> {
    use prost::Message;
    let now = crate::util::time::TimestampNanos::now().as_u64();
    let mut content = crate::proto::content_v1::Content {
        content: Some(
            crate::proto::content_v1::content::Content::GroupMailboxAddress(
                crate::proto::content_v1::GroupMailboxAddress {
                    channel_id: group_id.to_string(),
                    mailbox_pk: new_mailbox_pk.to_vec(),
                    timestamp: now,
                },
            ),
        ),
        ephemeral_reply_to: None,
        timestamp: now,
        padding: vec![],
    };
    crate::services::message::MessageService::pad_content_to_bucket(&mut content);
    content.encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn test_rotation_target_dm() {
        let user_id = UserId::new();
        let target = RotationTarget::Dm(user_id);

        match target {
            RotationTarget::Dm(id) => assert_eq!(id, user_id),
            _ => panic!("Expected Dm variant"),
        }
    }

    #[test]
    fn test_rotation_target_group() {
        let channel_id = ChannelId::new();
        let target = RotationTarget::Group(channel_id);

        match target {
            RotationTarget::Group(id) => assert_eq!(id, channel_id),
            _ => panic!("Expected Group variant"),
        }
    }

    #[test]
    fn test_rotation_notification_structure() {
        let user_id = UserId::new();
        let notification = RotationNotification {
            target: RotationTarget::Dm(user_id),
            new_mailbox_public_key: [1u8; 32],
            old_mailbox_expires_at: Utc::now() + Duration::days(7),
        };

        assert_eq!(notification.new_mailbox_public_key, [1u8; 32]);
        assert!(notification.old_mailbox_expires_at > Utc::now());
    }

    #[test]
    fn test_rotation_result_structure() {
        let user_id = UserId::new();
        let result = RotationResult {
            target_id: RotationTarget::Dm(user_id),
            new_mailbox_public_key: [2u8; 32],
            old_mailbox_deletion_at: Utc::now() + Duration::hours(1),
        };

        assert_eq!(result.new_mailbox_public_key, [2u8; 32]);
        assert!(result.old_mailbox_deletion_at > Utc::now());
    }

    #[test]
    fn test_default_constants() {
        assert_eq!(DEFAULT_CHECK_INTERVAL_SECS, 1800);
        assert_eq!(DEFAULT_ROTATION_THRESHOLD_HOURS, 12);

        // Epoch safety invariant: TTL − threshold must leave room for at least
        // two rotation checks plus offline slack, or a mailbox can expire before
        // members switch. 48h − 12h = 36h ≫ 2×30m.
        let ttl_hours = crate::mailbox::dm::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS as i64 / 3600;
        let two_checks_hours = 2 * DEFAULT_CHECK_INTERVAL_SECS as i64 / 3600;
        assert!(ttl_hours - DEFAULT_ROTATION_THRESHOLD_HOURS > two_checks_hours);
    }

    #[test]
    fn test_rotation_notification_clone() {
        let user_id = UserId::new();
        let notification = RotationNotification {
            target: RotationTarget::Dm(user_id),
            new_mailbox_public_key: [3u8; 32],
            old_mailbox_expires_at: Utc::now(),
        };

        let cloned = notification.clone();
        assert_eq!(
            cloned.new_mailbox_public_key,
            notification.new_mailbox_public_key
        );
    }

    #[test]
    fn test_rotation_target_clone() {
        let user_id = UserId::new();
        let target = RotationTarget::Dm(user_id);
        let cloned = target.clone();

        match (target, cloned) {
            (RotationTarget::Dm(a), RotationTarget::Dm(b)) => assert_eq!(a, b),
            _ => panic!("Clone mismatch"),
        }
    }

    #[test]
    fn scheduler_picks_group_candidates() {
        let peer = UserId::new();
        let group = ChannelId::new();
        let due = schedule_rotation_targets(vec![peer], vec![group]);
        assert_eq!(
            due,
            vec![RotationTarget::Dm(peer), RotationTarget::Group(group)],
            "a tick rotates due DMs and due groups, groups included"
        );
        assert_eq!(
            schedule_rotation_targets(vec![], vec![group]),
            vec![RotationTarget::Group(group)],
            "a tick with no due DM still rotates the group"
        );
    }

    /// Departed members stay in the session list (and used to stay in
    /// `group_member_mailboxes`). They must not be told the new key. Every
    /// device of a member who stayed is told, so those devices converge.
    #[test]
    fn departed_member_devices_are_excluded_and_remaining_devices_converge() {
        let me = UserId::new();
        let stayed = UserId::new();
        let departed = UserId::new();
        let stranger = UserId::new();
        let members = vec![me, stayed];
        let devices = vec![
            (me, 1),
            (me, 2),
            (stayed, 3),
            (stayed, 1),
            (stayed, 1),
            (departed, 1),
            (departed, 4),
            (stranger, 2),
        ];

        let got = group_rotation_recipients(&me, &members, &devices);

        assert_eq!(
            got,
            vec![
                GroupRotationRecipient {
                    user_id: stayed,
                    device_id: 1
                },
                GroupRotationRecipient {
                    user_id: stayed,
                    device_id: 3
                },
            ],
            "only every device of members who stayed"
        );
        assert!(
            got.iter()
                .all(|r| r.user_id != departed && r.user_id != me && r.user_id != stranger)
        );
    }

    #[test]
    fn rotation_plaintext_names_the_new_key_and_a_server_bucket() {
        use prost::Message;
        let group = ChannelId::new();
        let pk = [9u8; 32];
        let bytes = group_mailbox_rotation_plaintext(&group, &pk);
        assert!(
            matches!(bytes.len(), 256 | 512 | 1024 | 4096),
            "plaintext length {}, not a sealed-sender bucket",
            bytes.len()
        );
        let content = crate::proto::content_v1::Content::decode(bytes.as_slice()).unwrap();
        match content.content {
            Some(crate::proto::content_v1::content::Content::GroupMailboxAddress(ann)) => {
                assert_eq!(ann.channel_id, group.to_string());
                assert_eq!(ann.mailbox_pk, pk);
            }
            other => panic!("expected GroupMailboxAddress, got {other:?}"),
        }
    }

    #[test]
    fn scheduler_due_picks_an_expiring_group_mailbox() {
        let db = setup_db();
        let peer = UserId::new();
        let group = ChannelId::new();
        let now = Utc::now();

        let dm = crate::mailbox::dm::DmConversationManager::new(db.clone());
        dm.save_our_mailbox(
            &peer,
            &crate::mailbox::dm::DmMailbox {
                id: "dm-old".into(),
                public_key: [1u8; 32],
                private_key: [2u8; 64],
                encryption_private_key: [2u8; 32],
                signature: vec![1, 2, 3],
                randomizer: None,
                token_id: TEST_TOKEN_ID.into(),
                created_at: now - Duration::hours(40),
                expires_at: now + Duration::hours(6),
                last_polled_at: None,
            },
        )
        .unwrap();

        let groups = crate::mailbox::group::GroupMailboxManager::new(db.clone());
        groups
            .save_our_mailbox(&crate::mailbox::group::GroupMailbox {
                id: "grp-old".into(),
                group_id: group,
                public_key: [3u8; 32],
                private_key: [4u8; 64],
                signature: vec![1, 2, 3],
                randomizer: None,
                token_id: TEST_TOKEN_ID.into(),
                created_at: now - Duration::hours(40),
                expires_at: now + Duration::hours(6),
                last_polled_at: None,
            })
            .unwrap();

        let svc = KeyRollingService::new(
            Arc::new(crate::mailbox::TokenManager::new(db.clone())),
            Arc::new(dm),
            Arc::new(groups),
            Arc::new(
                crate::network::grpc::client_pool::GrpcClientPool::new("https://127.0.0.1:9")
                    .unwrap(),
            ),
        );

        assert_eq!(
            svc.scheduler_due(),
            vec![RotationTarget::Dm(peer), RotationTarget::Group(group)],
            "the scheduler's candidate list includes the expiring group"
        );
    }

    const TEST_TOKEN_ID: &str = "test-token";

    fn setup_db() -> Arc<crate::storage::Database> {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("key_rolling_test.db");
        let db = Arc::new(crate::storage::Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();
        let conn = db.connection().unwrap();
        conn.execute(
            "INSERT INTO mailbox_server_keys (key_id, public_key_der, valid_until, created_at)
             VALUES ('test-key', X'00', 0, ?1)",
            rusqlite::params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mailbox_tokens
             (id, ephemeral_public_key, ephemeral_private_key, signature, server_key_id, created_at)
             VALUES (?1, X'01', X'02', X'03', 'test-key', ?2)",
            rusqlite::params![TEST_TOKEN_ID, Utc::now().to_rfc3339()],
        )
        .unwrap();
        drop(conn);
        crate::test_tempdir::hold(temp_dir);
        db
    }
}

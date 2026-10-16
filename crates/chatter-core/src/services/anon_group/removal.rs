//! Leave, kick and ownership hand-off for anonymous groups.

use crate::domain::UserId;
use crate::domain::anon_group::{AnonGroupId, AnonGroupRole};
use crate::proto::content_v1::{
    self, AnonGroupKick, AnonGroupLeave, AnonGroupRoleChange, AnonGroupUpdate, Content,
    anon_group_update,
};
use crate::util::time::TimestampNanos;

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    /// Forget a group after we were removed. The conversation layer clears
    /// unified messages first; deleting the group cascades its mailbox data.
    pub fn delete_removed_group(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        let group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        if group.status != crate::domain::anon_group::AnonGroupStatus::Removed {
            return Err(AnonGroupError::InvalidData(
                "Only removed groups can be deleted locally".into(),
            ));
        }
        self.anon_group_repo.delete_group(group_id)?;
        Ok(())
    }

    /// Leave a group voluntarily.
    pub async fn leave_group(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;

        let group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        if !group.status.can_send() {
            return Err(AnonGroupError::GroupBlocked(*group_id));
        }

        if !self.may_leave(&group, &current_user)? {
            return Err(AnonGroupError::OwnerMustTransfer);
        }

        // Get confirmed members before we remove ourselves
        let members = self.anon_group_repo.get_members_with_mailbox(group_id)?;

        let timestamp = self
            .anon_group_repo
            .next_membership_timestamp(group_id, TimestampNanos::now().as_u64())?;
        // Build leave message
        let leave = AnonGroupLeave {
            group_id: group_id.to_vec(),
            timestamp,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupLeave(leave)),
            ephemeral_reply_to: None,
            timestamp,
            padding: Self::generate_padding(160),
        };

        if !self.anon_group_repo.remove_ordered_member(
            group_id,
            &current_user,
            timestamp,
            None,
            true,
        )? {
            return Err(AnonGroupError::InvalidData(
                "Leave superseded by membership change".into(),
            ));
        }
        self.held_posts.forget(group_id);

        // Notify other members via their anonymous mailboxes
        for member in members {
            if member.user_id == current_user {
                continue;
            }

            // Member has confirmed mailbox (from get_members_with_mailbox)
            let mailbox_pk = member
                .mailbox_pk
                .expect("get_members_with_mailbox guarantees mailbox_pk");

            if let Err(e) = self
                .send_to_anon_mailbox(
                    &member.user_id,
                    member.device_id,
                    &mailbox_pk,
                    content.clone(),
                )
                .await
            {
                tracing::warn!("Failed to notify of leave: {}", e);
            }
        }

        // Local group data and removals were committed before network fanout.

        tracing::info!("Left group");
        Ok(())
    }

    /// Whether `me` may leave `group` now. The owner is the only one who can
    /// issue member lists, kicks and role changes, so leaving without a
    /// successor would freeze the group for everyone else; the owner must
    /// [`transfer_ownership`](Self::transfer_ownership) first. A last
    /// remaining member may always leave.
    pub(crate) fn may_leave(
        &self,
        group: &crate::domain::anon_group::AnonGroup,
        me: &UserId,
    ) -> Result<bool, AnonGroupError> {
        Ok(group.owner_id != *me
            || !self
                .anon_group_repo
                .get_members(&group.id)?
                .iter()
                .any(|m| m.user_id != *me))
    }

    /// [`may_leave`](Self::may_leave) for the signed-in user; false when the
    /// group or the session cannot be read (the leave itself would fail).
    pub fn can_leave(&self, group_id: &AnonGroupId) -> bool {
        let (Ok(me), Ok(Some(group))) = (
            self.current_user_id(),
            self.anon_group_repo.get_group(group_id),
        ) else {
            return false;
        };
        group.status != crate::domain::anon_group::AnonGroupStatus::Removed
            && self.may_leave(&group, &me).unwrap_or(false)
    }

    /// Hand the group to `new_owner` (owner only). Every member is told by
    /// an owner-sent `AnonGroupUpdate` role change to Owner, which
    /// `handle_update` applies as a transfer; we become an admin.
    pub async fn transfer_ownership(
        &self,
        group_id: &AnonGroupId,
        new_owner: &UserId,
    ) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;
        let group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;
        if !group.status.can_send() {
            return Err(AnonGroupError::GroupBlocked(*group_id));
        }
        if group.owner_id != current_user {
            return Err(AnonGroupError::InsufficientPermissions(
                "Only the owner can transfer ownership".to_string(),
            ));
        }
        if *new_owner == current_user || !self.anon_group_repo.is_member(group_id, new_owner)? {
            return Err(AnonGroupError::MemberNotFound(*new_owner));
        }

        let timestamp = self
            .anon_group_repo
            .next_membership_timestamp(group_id, TimestampNanos::now().as_u64())?;
        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupUpdate(
                AnonGroupUpdate {
                    group_id: group_id.to_vec(),
                    timestamp,
                    update: Some(anon_group_update::Update::RoleChange(AnonGroupRoleChange {
                        user_id: new_owner.as_bytes().to_vec(),
                        new_role: AnonGroupRole::Owner.to_i32(),
                        timestamp,
                    })),
                },
            )),
            ephemeral_reply_to: None,
            timestamp,
            padding: Self::generate_padding(160),
        };
        let recipients = self.anon_group_repo.get_members_with_mailbox(group_id)?;
        let Some(content_v1::content::Content::AnonGroupUpdate(update)) = &content.content else {
            unreachable!()
        };
        if !self.handle_update(*group_id, current_user, update)? {
            return Err(AnonGroupError::InvalidData(
                "Ownership transfer superseded".into(),
            ));
        }
        for member in recipients {
            if member.user_id == current_user {
                continue;
            }
            let mailbox_pk = member
                .mailbox_pk
                .expect("get_members_with_mailbox guarantees mailbox_pk");
            if let Err(e) = self
                .send_to_anon_mailbox(
                    &member.user_id,
                    member.device_id,
                    &mailbox_pk,
                    content.clone(),
                )
                .await
            {
                tracing::warn!("Failed to notify of ownership transfer: {}", e);
            }
        }

        tracing::info!("Transferred group to");
        Ok(())
    }

    /// Kick a member from the group (admin/owner only).
    pub async fn kick_member(
        &self,
        group_id: &AnonGroupId,
        target_user_id: &UserId,
    ) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Can't kick yourself
        if *target_user_id == current_user {
            return Err(AnonGroupError::CannotKickSelf);
        }

        // Verify group exists and we have permission
        let group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        if !group.status.can_send() {
            return Err(AnonGroupError::GroupBlocked(*group_id));
        }

        if !group.my_role.can_kick() {
            return Err(AnonGroupError::InsufficientPermissions(
                "Only admins can kick members".to_string(),
            ));
        }

        // Can't kick the owner
        if *target_user_id == group.owner_id {
            return Err(AnonGroupError::CannotKickOwner);
        }

        let members = self.anon_group_repo.get_members_with_mailbox(group_id)?;

        // Build kick message with signature
        let timestamp = self
            .anon_group_repo
            .next_membership_timestamp(group_id, TimestampNanos::now().as_u64())?;
        let admin_signature = self.sign_kick(group_id, target_user_id, timestamp)?;

        let admin_signature_for_local = admin_signature.clone();
        let kick = AnonGroupKick {
            group_id: group_id.to_vec(),
            kicked_user_id: target_user_id.as_bytes().to_vec(),
            timestamp,
            admin_signature,
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupKick(kick)),
            ephemeral_reply_to: None,
            timestamp,
            padding: Self::generate_padding(160),
        };

        if !self.handle_kick(
            *group_id,
            *target_user_id,
            current_user,
            &admin_signature_for_local,
            timestamp,
        )? {
            return Err(AnonGroupError::InvalidData(
                "Kick superseded by membership change".into(),
            ));
        }

        // Notify every other member, the kicked one included: their client
        // verifies the same signature and retains its history read-only (`handle_kick`),
        // instead of posting into a group that no longer lists them.
        for member in members {
            if member.user_id == current_user {
                continue;
            }

            // Member has confirmed mailbox (from get_members_with_mailbox)
            let mailbox_pk = member
                .mailbox_pk
                .expect("get_members_with_mailbox guarantees mailbox_pk");

            if let Err(e) = self
                .send_to_anon_mailbox(
                    &member.user_id,
                    member.device_id,
                    &mailbox_pk,
                    content.clone(),
                )
                .await
            {
                tracing::warn!("Failed to notify of kick: {}", e);
            }
        }

        // Removal and its tombstone were committed before network fanout.

        tracing::info!("Removed member from group");
        Ok(())
    }

    /// Sign a kick message using our group mailbox private key.
    ///
    /// Uses the Ed25519 mailbox key for this group to sign the kick message.
    /// Recipients can verify using the sender's mailbox public key.
    pub(crate) fn sign_kick(
        &self,
        group_id: &AnonGroupId,
        target_user_id: &UserId,
        timestamp: u64,
    ) -> Result<Vec<u8>, AnonGroupError> {
        use ed25519_dalek::{Signer, SigningKey};

        // Build message to sign: group_id || kicked_user_id || timestamp
        let mut message = Vec::with_capacity(16 + 16 + 8);
        message.extend_from_slice(group_id.as_bytes());
        message.extend_from_slice(target_user_id.as_bytes());
        message.extend_from_slice(&timestamp.to_le_bytes());

        // Get our mailbox for this group
        let mailbox = self
            .anon_group_repo
            .get_mailbox(group_id)?
            .ok_or_else(|| AnonGroupError::CryptoError("No mailbox for group".to_string()))?;

        // Extract the Ed25519 signing key (first 32 bytes of the 64-byte keypair)
        let signing_key = SigningKey::from_bytes(
            &mailbox.private_key[..32]
                .try_into()
                .map_err(|_| AnonGroupError::CryptoError("Invalid private key".to_string()))?,
        );

        // Sign the message
        let signature = signing_key.sign(&message);

        Ok(signature.to_bytes().to_vec())
    }

    /// Verify a kick signature against the sender's mailbox public key.
    pub(crate) fn verify_kick_signature_with_key(
        &self,
        group_id: &AnonGroupId,
        kicked_user_id: &UserId,
        timestamp: u64,
        sender_mailbox_pk: &[u8; 32],
        signature: &[u8],
    ) -> Result<bool, AnonGroupError> {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let mut message = Vec::with_capacity(40);
        message.extend_from_slice(group_id.as_bytes());
        message.extend_from_slice(kicked_user_id.as_bytes());
        message.extend_from_slice(&timestamp.to_le_bytes());

        // Parse the signature
        if signature.len() != 64 {
            return Ok(false);
        }
        let sig_bytes: [u8; 64] = signature
            .try_into()
            .map_err(|_| AnonGroupError::CryptoError("Invalid signature length".to_string()))?;
        let sig = Signature::from_bytes(&sig_bytes);

        // Parse the verifying key
        let verifying_key = VerifyingKey::from_bytes(sender_mailbox_pk)
            .map_err(|e| AnonGroupError::CryptoError(format!("Invalid public key: {}", e)))?;

        // Verify
        Ok(verifying_key.verify(&message, &sig).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ed25519_dalek::{Signer, SigningKey};

    use crate::client::ChatterClient;
    use crate::domain::UserId;
    use crate::domain::anon_group::{
        AnonGroup, AnonGroupId, AnonGroupMember, AnonGroupRole, AnonGroupStatus,
    };
    use crate::inbox::handlers::DomainEvent;
    use crate::proto::content_v1::{self, AnonGroupKick, AnonGroupLeave, Content};
    use crate::services::anon_group::AnonGroupError;
    use crate::services::conversation::ConversationError;
    use crate::state::ActiveConversation;
    use crate::storage::repositories::settings_repo::keys;

    /// We are a member (role `my_role`) of a group owned by `owner`, whose
    /// mailbox key is `owner_key`; `mallory` is a plain member.
    struct Fixture {
        client: Arc<ChatterClient>,
        _dir: tempfile::TempDir,
        group_id: AnonGroupId,
        owner: UserId,
        owner_key: SigningKey,
        mallory: UserId,
    }

    fn fixture(my_role: AnonGroupRole) -> Fixture {
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
        let me = UserId::new();
        let s = &client.messages().settings_repo;
        s.set(keys::CURRENT_USER_ID, &me.to_string()).unwrap();
        s.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();

        let (owner, mallory) = (UserId::new(), UserId::new());
        let owner_key = SigningKey::from_bytes(&[5u8; 32]);
        let group_id = AnonGroupId::new();
        let repo = &client.anon_groups().anon_group_repo;
        let mut group = AnonGroup::from_invite(group_id, "g".into(), owner);
        group.my_role = my_role;
        repo.insert_group(&group).unwrap();
        repo.add_member(&AnonGroupMember::with_mailbox(
            group_id,
            owner,
            AnonGroupRole::Owner,
            owner_key.verifying_key().to_bytes(),
        ))
        .unwrap();
        repo.add_member(&AnonGroupMember::with_mailbox(
            group_id,
            mallory,
            AnonGroupRole::Member,
            [2u8; 32],
        ))
        .unwrap();
        repo.add_member(&AnonGroupMember::with_mailbox(
            group_id, me, my_role, [3u8; 32],
        ))
        .unwrap();
        Fixture {
            client,
            _dir: dir,
            group_id,
            owner,
            owner_key,
            mallory,
        }
    }

    fn kick(f: &Fixture, signer: &SigningKey) -> Content {
        let timestamp = 42u64;
        let mut msg = Vec::new();
        msg.extend_from_slice(f.group_id.as_bytes());
        msg.extend_from_slice(f.mallory.as_bytes());
        msg.extend_from_slice(&timestamp.to_le_bytes());
        Content {
            content: Some(content_v1::content::Content::AnonGroupKick(AnonGroupKick {
                group_id: f.group_id.to_vec(),
                kicked_user_id: f.mallory.as_bytes().to_vec(),
                timestamp,
                admin_signature: signer.sign(&msg).to_bytes().to_vec(),
            })),
            ..Default::default()
        }
    }

    fn leave(f: &Fixture) -> Content {
        Content {
            content: Some(content_v1::content::Content::AnonGroupLeave(
                AnonGroupLeave {
                    group_id: f.group_id.to_vec(),
                    timestamp: 1,
                },
            )),
            ..Default::default()
        }
    }

    fn post() -> Content {
        crate::services::gateway::builders::build_text_content("hi", 7)
    }

    fn stored(f: &Fixture) -> usize {
        f.client
            .anon_groups()
            .get_messages_asc(&f.group_id, None, 50)
            .unwrap()
            .len()
    }

    fn is_member(f: &Fixture, user: &UserId) -> bool {
        f.client
            .anon_groups()
            .anon_group_repo
            .is_member(&f.group_id, user)
            .unwrap()
    }

    /// The owner's member list naming `members` (mailbox keys are dummies).
    fn member_list(f: &Fixture, members: &[UserId]) -> Content {
        use crate::proto::content_v1::{AnonGroupMemberInfo, AnonGroupMemberList};
        Content {
            content: Some(content_v1::content::Content::AnonGroupMemberList(
                AnonGroupMemberList {
                    group_id: f.group_id.to_vec(),
                    members: members
                        .iter()
                        .map(|u| AnonGroupMemberInfo {
                            user_id: u.as_bytes().to_vec(),
                            mailbox_pk: vec![4u8; 32],
                            // A roster must name exactly the owner as Owner.
                            role: if *u == f.owner {
                                AnonGroupRole::Owner
                            } else {
                                AnonGroupRole::Member
                            }
                            .to_i32(),
                            ..Default::default()
                        })
                        .collect(),
                    timestamp: 1,
                },
            )),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_kicked_members_post_is_not_delivered() {
        let f = fixture(AnonGroupRole::Member);
        let svc = f.client.anon_groups();
        svc.process_incoming_content(&f.group_id, &f.owner, &kick(&f, &f.owner_key))
            .unwrap();
        assert!(!is_member(&f, &f.mallory));

        // Mallory kept the mailbox keys and posts anyway.
        assert!(
            svc.process_incoming_content(&f.group_id, &f.mallory, &post())
                .unwrap()
                .is_empty()
        );
        assert_eq!(stored(&f), 0);
        // A later roster that still leaves her out delivers nothing either.
        let events = svc
            .process_incoming_content(&f.group_id, &f.owner, &member_list(&f, &[f.owner]))
            .unwrap();
        assert!(
            events
                .iter()
                .all(|e| matches!(e, DomainEvent::AnonGroupMemberListReceived { .. })),
            "{events:?}"
        );
        assert_eq!(stored(&f), 0);
    }

    #[tokio::test]
    async fn a_new_members_post_waits_for_the_roster_that_adds_them() {
        let f = fixture(AnonGroupRole::Member);
        let svc = f.client.anon_groups();
        let newbie = UserId::new();

        // Their first post beats the owner's member list here.
        assert!(
            svc.process_incoming_content(&f.group_id, &newbie, &post())
                .unwrap()
                .is_empty()
        );
        assert_eq!(stored(&f), 0);

        // A forged list from a non-owner releases nothing.
        svc.process_incoming_content(&f.group_id, &f.mallory, &member_list(&f, &[newbie]))
            .unwrap();
        assert_eq!(stored(&f), 0);

        svc.process_incoming_content(
            &f.group_id,
            &f.owner,
            &member_list(&f, &[f.owner, f.mallory, newbie]),
        )
        .unwrap();
        assert!(is_member(&f, &newbie));
        assert_eq!(
            stored(&f),
            1,
            "the held post is delivered once the roster lists its sender"
        );
        // Released once, not again on the next list.
        svc.process_incoming_content(
            &f.group_id,
            &f.owner,
            &member_list(&f, &[f.owner, f.mallory, newbie]),
        )
        .unwrap();
        assert_eq!(stored(&f), 1);
    }

    fn transfer_to(f: &Fixture, to: UserId) -> Content {
        use crate::proto::content_v1::{AnonGroupRoleChange, AnonGroupUpdate, anon_group_update};
        Content {
            content: Some(content_v1::content::Content::AnonGroupUpdate(
                AnonGroupUpdate {
                    group_id: f.group_id.to_vec(),
                    timestamp: 1,
                    update: Some(anon_group_update::Update::RoleChange(AnonGroupRoleChange {
                        user_id: to.as_bytes().to_vec(),
                        new_role: AnonGroupRole::Owner.to_i32(),
                        timestamp: 1,
                    })),
                },
            )),
            ..Default::default()
        }
    }

    /// The owner of a group with other members must hand it on first; after
    /// the hand-off (applied locally even with no server to tell the others)
    /// they are an admin and may leave.
    #[tokio::test]
    async fn the_owner_leaves_only_after_transferring() {
        let f = fixture(AnonGroupRole::Member);
        let svc = f.client.anon_groups();
        let me = svc.current_user_id().unwrap();
        // Make us the owner of the fixture's group: the owner hands it to us.
        svc.process_incoming_content(&f.group_id, &f.owner, &transfer_to(&f, me))
            .unwrap();
        let conv = ActiveConversation::AnonGroup(f.group_id);
        assert!(!svc.can_leave(&f.group_id));
        let row = f
            .client
            .sidebar_rows()
            .into_iter()
            .find(|r| r.id == f.group_id.to_string())
            .unwrap();
        assert!(
            !row.can_leave,
            "the owner is not offered Leave while others remain"
        );
        let res = f.client.leave_conversation(conv).await;
        assert!(
            matches!(
                res,
                Err(ConversationError::AnonGroup(
                    AnonGroupError::OwnerMustTransfer
                ))
            ),
            "{res:?}"
        );

        // Only a member can receive it.
        assert!(
            f.client
                .transfer_conversation_ownership(conv, &UserId::new())
                .await
                .is_err()
        );
        f.client
            .transfer_conversation_ownership(conv, &f.mallory)
            .await
            .unwrap();
        let group = svc.get_group(&f.group_id).unwrap().unwrap();
        assert_eq!(group.owner_id, f.mallory);
        assert_eq!(group.my_role, AnonGroupRole::Admin);
        assert!(svc.can_leave(&f.group_id));
        f.client.leave_conversation(conv).await.unwrap();
    }

    /// A member applies the owner's hand-off; anyone else's is refused.
    #[tokio::test]
    async fn only_the_owner_can_hand_the_group_on() {
        let f = fixture(AnonGroupRole::Member);
        let svc = f.client.anon_groups();
        let repo = &svc.anon_group_repo;

        let events = svc
            .process_incoming_content(&f.group_id, &f.mallory, &transfer_to(&f, f.mallory))
            .unwrap();
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(
            svc.get_group(&f.group_id).unwrap().unwrap().owner_id,
            f.owner
        );

        let events = svc
            .process_incoming_content(&f.group_id, &f.owner, &transfer_to(&f, f.mallory))
            .unwrap();
        assert!(
            matches!(events.as_slice(), [DomainEvent::AnonGroupUpdated { .. }]),
            "{events:?}"
        );
        assert_eq!(
            svc.get_group(&f.group_id).unwrap().unwrap().owner_id,
            f.mallory
        );
        assert_eq!(
            repo.get_member_role(&f.group_id, &f.mallory).unwrap(),
            Some(AnonGroupRole::Owner)
        );
        assert_eq!(
            repo.get_member_role(&f.group_id, &f.owner).unwrap(),
            Some(AnonGroupRole::Admin)
        );

        // The new owner's member list is now the authoritative one.
        let newbie = UserId::new();
        svc.process_incoming_content(
            &f.group_id,
            &f.mallory,
            &member_list(&f, &[f.mallory, newbie]),
        )
        .unwrap();
        assert!(is_member(&f, &newbie));
    }

    #[tokio::test]
    async fn forged_kick_is_neither_applied_nor_announced() {
        let f = fixture(AnonGroupRole::Member);
        let svc = f.client.anon_groups();
        let forger = SigningKey::from_bytes(&[9u8; 32]);
        let events = svc
            .process_incoming_content(&f.group_id, &f.owner, &kick(&f, &forger))
            .unwrap();
        assert!(
            events.is_empty(),
            "a refused kick must not reach the UI: {events:?}"
        );
        assert!(is_member(&f, &f.mallory));

        // Validly signed, but by a non-admin: refused the same way.
        let events = svc
            .process_incoming_content(&f.group_id, &f.mallory, &kick(&f, &f.owner_key))
            .unwrap();
        assert!(events.is_empty());
        assert!(is_member(&f, &f.mallory));
    }

    #[tokio::test]
    async fn signed_kick_from_owner_is_applied_and_announced() {
        let f = fixture(AnonGroupRole::Member);
        let events = f
            .client
            .anon_groups()
            .process_incoming_content(&f.group_id, &f.owner, &kick(&f, &f.owner_key))
            .unwrap();
        assert!(
            matches!(
                events.as_slice(),
                [DomainEvent::AnonGroupMemberKicked { user_id, kicked_by, .. }]
                    if *user_id == f.mallory && *kicked_by == f.owner
            ),
            "{events:?}"
        );
        assert!(!is_member(&f, &f.mallory));
    }

    #[tokio::test]
    async fn blocked_group_drops_everything() {
        let f = fixture(AnonGroupRole::Member);
        let svc = f.client.anon_groups();
        svc.block_group(&f.group_id).unwrap();
        assert!(matches!(
            svc.check_group_not_blocked(&f.group_id),
            Err(AnonGroupError::GroupBlocked(_))
        ));

        assert!(
            svc.process_incoming_content(&f.group_id, &f.mallory, &post())
                .unwrap()
                .is_empty()
        );
        assert!(
            svc.process_incoming_content(&f.group_id, &f.mallory, &leave(&f))
                .unwrap()
                .is_empty()
        );
        assert_eq!(stored(&f), 0);
        assert!(is_member(&f, &f.mallory));

        svc.unblock_group(&f.group_id).unwrap();
        assert_eq!(
            svc.get_group(&f.group_id).unwrap().unwrap().status,
            AnonGroupStatus::Active
        );
        svc.process_incoming_content(&f.group_id, &f.mallory, &post())
            .unwrap();
        assert_eq!(stored(&f), 1);
    }

    #[tokio::test]
    async fn member_blocked_in_group_is_muted_but_their_leave_applies() {
        let f = fixture(AnonGroupRole::Member);
        let svc = f.client.anon_groups();
        svc.block_user_in_group(&f.group_id, &f.mallory, None)
            .unwrap();

        assert!(
            svc.process_incoming_content(&f.group_id, &f.mallory, &post())
                .unwrap()
                .is_empty()
        );
        assert_eq!(stored(&f), 0, "a blocked member's post must not be stored");
        // Others are unaffected.
        svc.process_incoming_content(&f.group_id, &f.owner, &post())
            .unwrap();
        assert_eq!(stored(&f), 1);

        let events = svc
            .process_incoming_content(&f.group_id, &f.mallory, &leave(&f))
            .unwrap();
        assert!(
            matches!(events.as_slice(), [DomainEvent::AnonGroupMemberLeft { .. }]),
            "{events:?}"
        );
        assert!(!is_member(&f, &f.mallory));
    }

    #[tokio::test]
    async fn leave_from_non_member_is_not_announced() {
        let f = fixture(AnonGroupRole::Member);
        let stranger = UserId::new();
        let events = f
            .client
            .anon_groups()
            .process_incoming_content(&f.group_id, &stranger, &leave(&f))
            .unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn plain_member_cannot_kick_and_nobody_kicks_the_owner() {
        let f = fixture(AnonGroupRole::Member);
        let conv = ActiveConversation::AnonGroup(f.group_id);
        let res = f.client.remove_conversation_member(conv, &f.mallory).await;
        assert!(
            matches!(
                res,
                Err(ConversationError::AnonGroup(
                    AnonGroupError::InsufficientPermissions(_)
                ))
            ),
            "{res:?}"
        );
        assert!(is_member(&f, &f.mallory));

        let f = fixture(AnonGroupRole::Admin);
        let conv = ActiveConversation::AnonGroup(f.group_id);
        let res = f.client.remove_conversation_member(conv, &f.owner).await;
        assert!(
            matches!(
                res,
                Err(ConversationError::AnonGroup(
                    AnonGroupError::CannotKickOwner
                ))
            ),
            "{res:?}"
        );
        assert!(is_member(&f, &f.owner));
    }

    #[tokio::test]
    async fn leaving_drops_the_group_from_this_device() {
        let f = fixture(AnonGroupRole::Member);
        let row = f
            .client
            .sidebar_rows()
            .into_iter()
            .find(|r| r.id == f.group_id.to_string())
            .unwrap();
        assert!(row.can_leave, "a non-owner member is offered Leave");

        let conv = f
            .client
            .resolve_row_conversation(
                &row.id,
                &crate::domain::message::ConversationType::AnonGroup,
            )
            .unwrap();
        assert_eq!(conv, ActiveConversation::AnonGroup(f.group_id));
        // The notices cannot be delivered here (no server); the leave still
        // completes locally.
        f.client.leave_conversation(conv).await.unwrap();
        assert!(f.client.get_anon_group(&f.group_id).is_none());
    }
}

//! Leave and kick operations for anonymous groups.

use crate::domain::anon_group::AnonGroupId;
use crate::domain::UserId;
use crate::proto::content_v1::{self, AnonGroupKick, AnonGroupLeave, Content};
use crate::util::time::TimestampNanos;

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    /// Leave a group voluntarily.
    pub async fn leave_group(&self, group_id: &AnonGroupId) -> Result<(), AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Verify group exists
        let _group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        // Get confirmed members before we remove ourselves
        let members = self.anon_group_repo.get_members_with_mailbox(group_id)?;

        // Build leave message
        let leave = AnonGroupLeave {
            group_id: group_id.to_vec(),
            timestamp: TimestampNanos::now().as_u64(),
        };

        let content = Content {
            content: Some(content_v1::content::Content::AnonGroupLeave(leave)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: Self::generate_padding(160),
        };

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
                .send_to_anon_mailbox(&member.user_id, &mailbox_pk, content.clone())
                .await
            {
                tracing::warn!("Failed to notify {} of leave: {}", member.user_id, e);
            }
        }

        // Delete local group data
        self.anon_group_repo.delete_group(group_id)?;

        tracing::info!("Left group {}", group_id);
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
        let timestamp = TimestampNanos::now().as_u64();
        let admin_signature = self.sign_kick(group_id, target_user_id, timestamp)?;

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

        // Notify all members except the kicked user (via anonymous mailboxes)
        for member in members {
            if member.user_id == current_user || member.user_id == *target_user_id {
                continue;
            }

            // Member has confirmed mailbox (from get_members_with_mailbox)
            let mailbox_pk = member
                .mailbox_pk
                .expect("get_members_with_mailbox guarantees mailbox_pk");

            if let Err(e) = self
                .send_to_anon_mailbox(&member.user_id, &mailbox_pk, content.clone())
                .await
            {
                tracing::warn!("Failed to notify {} of kick: {}", member.user_id, e);
            }
        }

        // Remove from local member list
        self.anon_group_repo
            .remove_member(group_id, target_user_id)?;

        tracing::info!("Kicked {} from group {}", target_user_id, group_id);
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

    /// Verify a kick signature using the sender's mailbox public key.
    pub(crate) fn verify_kick_signature(
        &self,
        group_id: &AnonGroupId,
        kicked_user_id: &UserId,
        timestamp: u64,
        sender_id: &UserId,
        signature: &[u8],
    ) -> Result<bool, AnonGroupError> {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};

        // Build message that was signed
        let mut message = Vec::with_capacity(16 + 16 + 8);
        message.extend_from_slice(group_id.as_bytes());
        message.extend_from_slice(kicked_user_id.as_bytes());
        message.extend_from_slice(&timestamp.to_le_bytes());

        // Get sender's mailbox public key
        let members = self.anon_group_repo.get_members(group_id)?;
        let sender_mailbox_pk = members
            .iter()
            .find(|m| m.user_id == *sender_id)
            .and_then(|m| m.mailbox_pk)
            .ok_or_else(|| AnonGroupError::MemberNotFound(*sender_id))?;

        // Parse the signature
        if signature.len() != 64 {
            return Ok(false);
        }
        let sig_bytes: [u8; 64] = signature
            .try_into()
            .map_err(|_| AnonGroupError::CryptoError("Invalid signature length".to_string()))?;
        let sig = Signature::from_bytes(&sig_bytes);

        // Parse the verifying key
        let verifying_key = VerifyingKey::from_bytes(&sender_mailbox_pk)
            .map_err(|e| AnonGroupError::CryptoError(format!("Invalid public key: {}", e)))?;

        // Verify
        Ok(verifying_key.verify(&message, &sig).is_ok())
    }
}

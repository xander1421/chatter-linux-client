//! Handlers for incoming group messages (leave, kick, update).

use crate::domain::UserId;
use crate::domain::anon_group::AnonGroupId;
use crate::proto::content_v1::AnonGroupUpdate;

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    /// Handle an incoming leave message. `Ok(true)` only when a member was
    /// actually removed, so the caller announces nothing it did not apply.
    pub fn handle_leave(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
    ) -> Result<bool, AnonGroupError> {
        self.handle_leave_ordered(group_id, sender_id, 0)
    }

    pub fn handle_leave_ordered(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        timestamp: u64,
    ) -> Result<bool, AnonGroupError> {
        Ok(self
            .anon_group_repo
            .remove_ordered_member(&group_id, &sender_id, timestamp, None, false)?)
    }

    /// Handle an incoming kick message. `Ok(true)` only when the kick was
    /// authorized, carried a valid signature, and was applied; every refusal
    /// is `Ok(false)` (or an error), so the caller announces nothing.
    pub fn handle_kick(
        &self,
        group_id: AnonGroupId,
        kicked_user_id: UserId,
        sender_id: UserId,
        admin_signature: &[u8],
        timestamp: u64,
    ) -> Result<bool, AnonGroupError> {
        let current_user = self.current_user_id()?;

        // Verify group exists
        let _group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        // Verify sender has kick permission
        let sender_role = self
            .anon_group_repo
            .get_member_role(&group_id, &sender_id)?
            .ok_or(AnonGroupError::MemberNotFound(sender_id))?;

        if sender_id != _group.owner_id && !sender_role.can_kick() {
            tracing::warn!(
                "Kick from unauthorized member {} in group {} - ignoring",
                sender_id,
                group_id
            );
            return Ok(false);
        }

        // SECURITY: The owner can never be kicked.
        if kicked_user_id == _group.owner_id {
            tracing::warn!(
                sender_id = %sender_id,
                group_id = %group_id,
                "Rejected attempt to kick the group owner"
            );
            return Ok(false);
        }

        let verified_key = self
            .anon_group_repo
            .get_member(&group_id, &sender_id)?
            .and_then(|member| member.mailbox_pk)
            .ok_or(AnonGroupError::MemberNotFound(sender_id))?;

        // Verify the signature using sender's mailbox public key
        // SECURITY: Signature verification is MANDATORY for kicks to prevent spoofing.
        match self.verify_kick_signature_with_key(
            &group_id,
            &kicked_user_id,
            timestamp,
            &verified_key,
            admin_signature,
        ) {
            Ok(true) => {
                tracing::debug!("Kick signature verified for group {}", group_id);
            }
            Ok(false) => {
                tracing::warn!(
                    "Invalid kick signature from {} in group {} - ignoring",
                    sender_id,
                    group_id
                );
                return Ok(false);
            }
            Err(e) => {
                tracing::error!("Failed to verify kick signature: {} - rejecting kick", e);
                return Err(AnonGroupError::CryptoError(format!(
                    "Kick signature verification failed: {}",
                    e
                )));
            }
        }

        let applied = self.anon_group_repo.remove_ordered_member(
            &group_id,
            &kicked_user_id,
            timestamp,
            Some((&sender_id, &verified_key)),
            kicked_user_id == current_user,
        )?;
        if applied && kicked_user_id == current_user {
            self.held_posts.forget(&group_id);
        }
        Ok(applied)
    }

    /// Handle an incoming group update message (name change, role change,
    /// ownership hand-off). `Ok(true)` when it was authorized and applied.
    pub fn handle_update(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        update: &AnonGroupUpdate,
    ) -> Result<bool, AnonGroupError> {
        Ok(self.anon_group_repo.apply_ordered_update(
            &group_id,
            &sender_id,
            &self.current_user_id()?,
            update,
        )?)
    }
}

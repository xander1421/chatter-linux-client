//! Handlers for incoming group messages (leave, kick, update).

use crate::domain::anon_group::{AnonGroupId, AnonGroupRole};
use crate::domain::UserId;
use crate::proto::content_v1::{anon_group_update, AnonGroupUpdate};

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    /// Handle an incoming leave message. `Ok(true)` only when a member was
    /// actually removed, so the caller announces nothing it did not apply.
    pub fn handle_leave(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
    ) -> Result<bool, AnonGroupError> {
        // Verify group exists
        let _group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        // SECURITY: A member can only leave for themselves.
        // If sender_id != current_member_leaving, it's an invalid leave message.
        // (For anonymous groups, the leave message doesn't contain a target ID, 
        // it implicitly means the sender is leaving).
        // Since sender_id is authenticated via Sealed Sender, this is safe.

        // Verify sender is a member
        if !self.anon_group_repo.is_member(&group_id, &sender_id)? {
            tracing::debug!("Leave from non-member {} in group {}", sender_id, group_id);
            return Ok(false);
        }

        // Remove member
        self.anon_group_repo.remove_member(&group_id, &sender_id)?;

        tracing::info!("Member {} left group {}", sender_id, group_id);
        Ok(true)
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

        if !sender_role.can_kick() {
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

        // Verify the signature using sender's mailbox public key
        // SECURITY: Signature verification is MANDATORY for kicks to prevent spoofing.
        match self.verify_kick_signature(
            &group_id,
            &kicked_user_id,
            timestamp,
            &sender_id,
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

        // If we're being kicked, leave the group
        if kicked_user_id == current_user {
            self.anon_group_repo.delete_group(&group_id)?;
            tracing::info!("We were kicked from group {}", group_id);
            return Ok(true);
        }

        // Remove the kicked member from our local list
        if self.anon_group_repo.is_member(&group_id, &kicked_user_id)? {
            self.anon_group_repo
                .remove_member(&group_id, &kicked_user_id)?;
        }

        tracing::info!(
            "Member {} was kicked from group {} by {}",
            kicked_user_id,
            group_id,
            sender_id
        );
        Ok(true)
    }

    /// Handle an incoming group update message (name change, role change).
    pub fn handle_update(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        update: &AnonGroupUpdate,
    ) -> Result<(), AnonGroupError> {
        // Verify group exists
        let group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        // Verify sender is a member with appropriate permissions
        let sender_role = self
            .anon_group_repo
            .get_member_role(&group_id, &sender_id)?
            .ok_or(AnonGroupError::MemberNotFound(sender_id))?;

        match &update.update {
            Some(anon_group_update::Update::NewName(new_name)) => {
                // Only admins/owners can rename
                if !sender_role.can_kick() {
                    tracing::warn!(
                        "Name change from non-admin {} in group {} - ignoring",
                        sender_id,
                        group_id
                    );
                    return Ok(());
                }

                self.anon_group_repo
                    .update_group_name(&group_id, new_name)?;
                tracing::info!(
                    "Group {} renamed to '{}' by {}",
                    group_id,
                    new_name,
                    sender_id
                );
            }
            Some(anon_group_update::Update::RoleChange(role_change)) => {
                // SECURITY: Only the group owner can change roles.
                // We must check if the SENDER is the owner, not if WE are the owner.
                if sender_id != group.owner_id {
                    tracing::warn!(
                        sender_id = %sender_id,
                        owner_id = %group.owner_id,
                        group_id = %group_id,
                        "Rejected role change from non-owner"
                    );
                    return Ok(());
                }

                let target_user_id = UserId::try_from_slice(&role_change.user_id).ok_or_else(|| {
                    AnonGroupError::InvalidData("Invalid user ID in role change".to_string())
                })?;

                let new_role = AnonGroupRole::from_i32(role_change.new_role);

                // If we are the owner, we shouldn't be receiving role changes from others
                // except potentially from our own other devices (sync).
                let current_user = self.current_user_id()?;
                if group.is_owner() && sender_id != current_user {
                    tracing::warn!(
                        sender_id = %sender_id,
                        group_id = %group_id,
                        "Owner received role change from another member - ignoring"
                    );
                    return Ok(());
                }

                self.anon_group_repo
                    .update_member_role(&group_id, &target_user_id, new_role)?;
                tracing::info!(
                    "Member {} role changed to {:?} in group {} by {}",
                    target_user_id,
                    new_role,
                    group_id,
                    sender_id
                );
            }
            None => {
                tracing::debug!("Empty update for group {} - ignoring", group_id);
            }
        }

        Ok(())
    }
}

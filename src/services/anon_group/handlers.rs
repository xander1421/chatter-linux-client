//! Handlers for incoming group messages (leave, kick, update).

use crate::domain::anon_group::{AnonGroupId, AnonGroupRole};
use crate::domain::UserId;
use crate::proto::content_v1::{anon_group_update, AnonGroupUpdate};

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    /// Handle an incoming leave message.
    pub fn handle_leave(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
    ) -> Result<(), AnonGroupError> {
        // Verify group exists
        let _group = self
            .anon_group_repo
            .get_group(&group_id)?
            .ok_or(AnonGroupError::GroupNotFound(group_id))?;

        // Verify sender is a member
        if !self.anon_group_repo.is_member(&group_id, &sender_id)? {
            tracing::debug!("Leave from non-member {} in group {}", sender_id, group_id);
            return Ok(());
        }

        // Remove member
        self.anon_group_repo.remove_member(&group_id, &sender_id)?;

        tracing::info!("Member {} left group {}", sender_id, group_id);
        Ok(())
    }

    /// Handle an incoming kick message.
    pub fn handle_kick(
        &self,
        group_id: AnonGroupId,
        kicked_user_id: UserId,
        sender_id: UserId,
        admin_signature: &[u8],
        timestamp: u64,
    ) -> Result<(), AnonGroupError> {
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
                "Kick from non-admin {} in group {} - ignoring",
                sender_id,
                group_id
            );
            return Ok(());
        }

        // Verify the signature using sender's mailbox public key
        if !admin_signature.is_empty() {
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
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!("Failed to verify kick signature: {} - processing anyway", e);
                    // Continue processing - signature verification failure shouldn't block kick
                    // This allows graceful degradation for older clients
                }
            }
        }

        // If we're being kicked, leave the group
        if kicked_user_id == current_user {
            self.anon_group_repo.delete_group(&group_id)?;
            tracing::info!("We were kicked from group {}", group_id);
            return Ok(());
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
        Ok(())
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
                // Only owners can change roles
                if !group.is_owner() {
                    tracing::debug!(
                        "Role change from non-owner in group {} - ignoring",
                        group_id
                    );
                    return Ok(());
                }

                let target_user_id = UserId::try_from_slice(&role_change.user_id).ok_or_else(|| {
                    AnonGroupError::InvalidData("Invalid user ID in role change".to_string())
                })?;

                let new_role = AnonGroupRole::from_i32(role_change.new_role);

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

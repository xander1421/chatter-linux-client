//! Handlers for incoming group messages (leave, kick, update).

use crate::domain::UserId;
use crate::domain::anon_group::AnonGroupId;
use crate::proto::content_v1::AnonGroupUpdate;

use super::{AnonGroupError, AnonGroupService};

impl AnonGroupService {
    /// Handle an incoming leave message. `Ok(true)` only when a member was
    /// actually removed, so the caller announces nothing it did not apply.
    pub fn handle_leave_ordered(
        &self,
        group_id: AnonGroupId,
        sender_id: UserId,
        timestamp: u64,
    ) -> Result<bool, AnonGroupError> {
        // A non-member's leave still records its removal barrier (it may beat
        // the roster that adds it), but there is no one to announce as gone.
        let was_member = self.anon_group_repo.is_member(&group_id, &sender_id)?;
        let left_name = if was_member {
            self.anon_group_repo
                .get_member_display_name(&group_id, &sender_id)?
                .filter(|name| !name.is_empty())
        } else {
            None
        };
        let removed = self
            .anon_group_repo
            .remove_ordered_member(&group_id, &sender_id, timestamp, None, false)?;
        if removed && was_member {
            self.store_system_message(
                &group_id,
                Some(sender_id),
                crate::domain::message::SystemMessage::AnonGroupUserLeft { left_name },
                timestamp,
            )?;
        }
        Ok(removed && was_member)
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
            tracing::warn!("Kick from unauthorized member in group - ignoring");
            return Ok(false);
        }

        // SECURITY: The owner can never be kicked.
        if kicked_user_id == _group.owner_id {
            tracing::warn!("Rejected attempt to kick the group owner");
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
                tracing::debug!("Kick signature verified for group");
            }
            Ok(false) => {
                tracing::warn!("Invalid kick signature from in group - ignoring");
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

        let removed_name = self
            .anon_group_repo
            .get_member_display_name(&group_id, &kicked_user_id)?;
        let applied = self.anon_group_repo.remove_ordered_member(
            &group_id,
            &kicked_user_id,
            timestamp,
            Some((&sender_id, &verified_key)),
            false,
        )?;
        if applied {
            let content = if kicked_user_id == current_user {
                self.anon_group_repo.update_status(
                    &group_id,
                    crate::domain::anon_group::AnonGroupStatus::Removed,
                )?;
                self.held_posts.forget(&group_id);
                crate::domain::message::SystemMessage::SelfRemoved { by: sender_id }
            } else {
                crate::domain::message::SystemMessage::UserRemoved {
                    by: sender_id,
                    removed: kicked_user_id,
                    removed_name,
                }
            };
            self.store_system_message(&group_id, Some(sender_id), content, timestamp)?;
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

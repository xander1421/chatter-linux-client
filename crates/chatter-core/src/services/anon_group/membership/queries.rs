use super::*;

impl AnonGroupService {
    /// Get members of a group.
    pub fn get_members(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Vec<AnonGroupMember>, AnonGroupError> {
        Ok(self.anon_group_repo.get_members(group_id)?)
    }

    /// Count joined members and pending invitees from the local roster.
    pub fn member_counts(&self, group_id: &AnonGroupId) -> Result<(usize, usize), AnonGroupError> {
        let members = self.get_members(group_id)?;
        let joined = members
            .iter()
            .filter(|member| member.mailbox_pk.is_some())
            .count();
        Ok((joined, members.len() - joined))
    }

    /// Get a member of a group.
    pub fn get_member(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<Option<AnonGroupMember>, AnonGroupError> {
        Ok(self.anon_group_repo.get_member(group_id, user_id)?)
    }

    /// Get a member's display name from their group profile.
    pub fn get_member_display_name(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<Option<String>, AnonGroupError> {
        Ok(self
            .anon_group_repo
            .get_member_display_name(group_id, user_id)?)
    }

    /// Retain the last shared avatar for departed members' message history.
    pub fn get_member_avatar(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<Option<Vec<u8>>, AnonGroupError> {
        Ok(self.anon_group_repo.get_member_avatar(group_id, user_id)?)
    }

    /// Check if a user is a member of a group.
    pub fn is_member(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<bool, AnonGroupError> {
        Ok(self.anon_group_repo.is_member(group_id, user_id)?)
    }

    /// Get a member's role in a group.
    pub fn get_member_role(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<Option<AnonGroupRole>, AnonGroupError> {
        Ok(self.anon_group_repo.get_member_role(group_id, user_id)?)
    }
    /// Get a group by ID.
    pub fn get_group(&self, group_id: &AnonGroupId) -> Result<Option<AnonGroup>, AnonGroupError> {
        Ok(self.anon_group_repo.get_group(group_id)?)
    }

    /// List all groups.
    pub fn list_groups(&self) -> Result<Vec<AnonGroup>, AnonGroupError> {
        Ok(self.anon_group_repo.list_groups()?)
    }

    /// List only active (non-blocked) groups.
    pub fn list_active_groups(&self) -> Result<Vec<AnonGroup>, AnonGroupError> {
        Ok(self.anon_group_repo.list_active_groups()?)
    }
}

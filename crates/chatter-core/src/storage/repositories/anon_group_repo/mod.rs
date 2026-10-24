//! Anonymous group management repository.
//!
//! Data access for NDSS'21 style anonymous groups.
//! Message operations are handled by the unified MessageRepository.

use crate::domain::UserId;
use crate::domain::anon_group::{
    AnonGroup, AnonGroupId, AnonGroupMailbox, AnonGroupMember, AnonGroupPending, AnonGroupRole,
    PendingActionType,
};
use crate::error::StorageError;
use crate::storage::Database;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use std::sync::Arc;

/// Anonymous group data repository.
///
/// Provides CRUD operations for anonymous groups and their members.
pub struct AnonGroupRepository {
    db: Arc<Database>,
}

/// Invitation retained until the recipient accepts or declines it.
#[derive(Debug, Clone)]
pub struct PendingAnonGroupInvite {
    pub group_id: AnonGroupId,
    pub invite_epoch: u64,
    pub group_name: String,
    pub inviter_id: UserId,
    pub inviter_device_id: u32,
    pub inviter_mailbox_pk: [u8; 32],
    pub profile_key: Option<[u8; 32]>,
    pub inviter_display_name: Option<String>,
    pub inviter_avatar: Option<Vec<u8>>,
}

mod groups;
mod helpers;
mod invites;
mod mailbox;
mod members;
mod ordering;
mod pending;
mod status;

use self::helpers::{
    membership_timestamp, record_member_change, row_to_group, row_to_mailbox, row_to_member,
    write_ordered_member,
};

impl AnonGroupRepository {
    /// Create a new AnonGroupRepository with the given database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }
}

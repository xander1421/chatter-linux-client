//! Channel repository.
//!
//! Data access for channel and membership entities.

use crate::domain::channel::{
    Channel, ChannelConfig, ChannelType, MemberRole, Membership, MessagePersistence,
    NotificationPreference,
};
use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use std::collections::HashSet;
use std::sync::Arc;

/// What a full roster refresh changed. `added` and `removed` are the
/// membership rows that were not already the local set.
#[derive(Debug, Default)]
pub struct MembershipRefresh {
    pub added: Vec<Membership>,
    pub removed: Vec<Membership>,
}

/// Channel data repository.
///
/// Provides CRUD operations for channels and memberships.
pub struct ChannelRepository {
    db: Arc<Database>,
}

/// The columns [`row_to_channel`] reads, for the queries that list channels.
const LISTED_CHANNEL_SELECT: &str = "SELECT c.id, c.channel_type, c.name, c.description, c.icon, c.owner_id,
        c.persistence, c.max_members, c.default_ttl_seconds,
        c.allow_reactions, c.allow_threads, c.allow_attachments, c.require_encryption,
        c.created_at, c.updated_at, c.archived,
        (SELECT COUNT(*) FROM messages WHERE conversation_id = c.id AND conversation_type = 'channel' AND is_read = 0 AND is_deleted = 0) as unread_count,
        (SELECT MAX(created_at) FROM messages WHERE conversation_id = c.id AND conversation_type = 'channel' AND is_deleted = 0) as last_message_at,
        (SELECT COUNT(*) FROM memberships WHERE channel_id = c.id) as member_count
     FROM channels c";

impl ChannelRepository {
    /// Create a new ChannelRepository with the given database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Channel CRUD
    // ========================================================================

    /// Insert a new channel.
    pub fn insert(&self, channel: &Channel) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO channels (
                id, channel_type, name, description, icon, owner_id,
                persistence, max_members, default_ttl_seconds,
                allow_reactions, allow_threads, allow_attachments, require_encryption,
                created_at, updated_at, archived
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                channel.id.to_string(),
                channel_type_to_i32(channel.channel_type),
                channel.name,
                channel.description,
                channel.icon,
                channel.owner_id.to_string(),
                persistence_to_i32(channel.config.persistence),
                channel.config.max_members,
                channel.config.default_ttl_seconds,
                channel.config.allow_reactions as i32,
                channel.config.allow_threads as i32,
                channel.config.allow_attachments as i32,
                channel.config.require_encryption as i32,
                channel.created_at.to_rfc3339(),
                channel.updated_at.to_rfc3339(),
                channel.archived as i32,
            ],
        )?;
        Ok(())
    }

    /// Insert a channel if it doesn't already exist.
    ///
    /// Returns Ok(true) if inserted, Ok(false) if already existed.
    pub fn insert_or_ignore(&self, channel: &Channel) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "INSERT OR IGNORE INTO channels (
                id, channel_type, name, description, icon, owner_id,
                persistence, max_members, default_ttl_seconds,
                allow_reactions, allow_threads, allow_attachments, require_encryption,
                created_at, updated_at, archived
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                channel.id.to_string(),
                channel_type_to_i32(channel.channel_type),
                channel.name,
                channel.description,
                channel.icon,
                channel.owner_id.to_string(),
                persistence_to_i32(channel.config.persistence),
                channel.config.max_members,
                channel.config.default_ttl_seconds,
                channel.config.allow_reactions as i32,
                channel.config.allow_threads as i32,
                channel.config.allow_attachments as i32,
                channel.config.require_encryption as i32,
                channel.created_at.to_rfc3339(),
                channel.updated_at.to_rfc3339(),
                channel.archived as i32,
            ],
        )?;
        Ok(rows > 0)
    }

    /// Get a channel by ID.
    pub fn get(&self, id: &ChannelId) -> Result<Option<Channel>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_type, name, description, icon, owner_id,
                    persistence, max_members, default_ttl_seconds,
                    allow_reactions, allow_threads, allow_attachments, require_encryption,
                    created_at, updated_at, archived,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = channels.id AND conversation_type = 'channel' AND is_read = 0 AND is_deleted = 0) as unread_count,
                    (SELECT MAX(created_at) FROM messages WHERE conversation_id = channels.id AND conversation_type = 'channel' AND is_deleted = 0) as last_message_at,
                    (SELECT COUNT(*) FROM memberships WHERE channel_id = channels.id) as member_count
             FROM channels WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.to_string()], row_to_channel);

        match result {
            Ok(channel) => Ok(Some(channel)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Update an existing channel.
    pub fn update(&self, channel: &Channel) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE channels SET
                channel_type = ?1, name = ?2, description = ?3, icon = ?4, owner_id = ?5,
                persistence = ?6, max_members = ?7, default_ttl_seconds = ?8,
                allow_reactions = ?9, allow_threads = ?10, allow_attachments = ?11, require_encryption = ?12,
                updated_at = ?13, archived = ?14
             WHERE id = ?15",
            params![
                channel_type_to_i32(channel.channel_type),
                channel.name,
                channel.description,
                channel.icon,
                channel.owner_id.to_string(),
                persistence_to_i32(channel.config.persistence),
                channel.config.max_members,
                channel.config.default_ttl_seconds,
                channel.config.allow_reactions as i32,
                channel.config.allow_threads as i32,
                channel.config.allow_attachments as i32,
                channel.config.require_encryption as i32,
                channel.updated_at.to_rfc3339(),
                channel.archived as i32,
                channel.id.to_string(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Channel {}", channel.id)));
        }
        Ok(())
    }

    /// Insert or update a channel (upsert).
    ///
    /// If the channel exists, updates it. If not, inserts it.
    pub fn upsert(&self, channel: &Channel) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO channels (
                id, channel_type, name, description, icon, owner_id,
                persistence, max_members, default_ttl_seconds,
                allow_reactions, allow_threads, allow_attachments, require_encryption,
                created_at, updated_at, archived
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            ON CONFLICT(id) DO UPDATE SET
                channel_type = excluded.channel_type,
                name = excluded.name,
                description = excluded.description,
                icon = excluded.icon,
                owner_id = excluded.owner_id,
                persistence = excluded.persistence,
                max_members = excluded.max_members,
                default_ttl_seconds = excluded.default_ttl_seconds,
                allow_reactions = excluded.allow_reactions,
                allow_threads = excluded.allow_threads,
                allow_attachments = excluded.allow_attachments,
                require_encryption = excluded.require_encryption,
                updated_at = excluded.updated_at,
                archived = excluded.archived",
            params![
                channel.id.to_string(),
                channel_type_to_i32(channel.channel_type),
                channel.name,
                channel.description,
                channel.icon,
                channel.owner_id.to_string(),
                persistence_to_i32(channel.config.persistence),
                channel.config.max_members,
                channel.config.default_ttl_seconds,
                channel.config.allow_reactions as i32,
                channel.config.allow_threads as i32,
                channel.config.allow_attachments as i32,
                channel.config.require_encryption as i32,
                channel.created_at.to_rfc3339(),
                channel.updated_at.to_rfc3339(),
                channel.archived as i32,
            ],
        )?;
        Ok(())
    }

    /// Delete a channel by ID.
    pub fn delete(&self, id: &ChannelId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM channels WHERE id = ?1",
            params![id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Channel {}", id)));
        }
        Ok(())
    }

    /// Remove local channel state after the owner's delete is confirmed.
    /// Keep message history; received deletion events use the read-only path.
    /// Idempotent so retrying local cleanup after a concurrent event is safe.
    pub fn remove_confirmed_deleted_channel(&self, id: &ChannelId) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let id = id.to_string();
        tx.execute("DELETE FROM memberships WHERE channel_id = ?1", params![id])?;
        tx.execute(
            "DELETE FROM verified_channel_state WHERE channel_id = ?1",
            params![id],
        )?;
        tx.execute("DELETE FROM channels WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    // ========================================================================
    // Channel queries
    // ========================================================================

    /// Maximum channels returned per query to prevent unbounded memory usage.
    const MAX_CHANNELS_PER_QUERY: u32 = 500;

    /// List all channels for a user (via membership), ordered by last activity.
    ///
    /// Results are limited to MAX_CHANNELS_PER_QUERY (500) to prevent unbounded memory.
    /// Use `list_for_user_paginated` for pagination if needed.
    pub fn list_for_user(&self, user_id: &UserId) -> Result<Vec<Channel>, StorageError> {
        self.list_for_user_paginated(user_id, Self::MAX_CHANNELS_PER_QUERY, 0)
    }

    /// List channels for a user with pagination.
    ///
    /// # Arguments
    /// * `user_id` - The user to list channels for
    /// * `limit` - Maximum number of channels to return
    /// * `offset` - Number of channels to skip
    pub fn list_for_user_paginated(
        &self,
        user_id: &UserId,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Channel>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "{LISTED_CHANNEL_SELECT}
             INNER JOIN memberships m ON c.id = m.channel_id
             WHERE m.user_id = ?1
             ORDER BY last_message_at DESC NULLS LAST
             LIMIT ?2 OFFSET ?3"
        ))?;

        let mut rows = stmt.query(params![user_id.to_string(), limit, offset])?;
        let mut channels = Vec::new();

        while let Some(row) = rows.next()? {
            channels.push(row_to_channel(row)?);
        }

        Ok(channels)
    }

    /// Channels `user_id` used to be in: left, removed, banned or deleted
    /// ([`Self::mark_former`]). They stay listed read-only until the chat is
    /// deleted; a rejoin (our membership back) takes one off this list.
    pub fn list_former_for_user(&self, user_id: &UserId) -> Result<Vec<Channel>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!(
            "{LISTED_CHANNEL_SELECT}
             INNER JOIN former_channels f ON c.id = f.channel_id
             WHERE NOT EXISTS (SELECT 1 FROM memberships m WHERE m.channel_id = c.id AND m.user_id = ?1)
             ORDER BY last_message_at DESC NULLS LAST"
        ))?;
        let mut rows = stmt.query(params![user_id.to_string()])?;
        let mut channels = Vec::new();
        while let Some(row) = rows.next()? {
            channels.push(row_to_channel(row)?);
        }
        Ok(channels)
    }

    /// Remember that we are no longer in `channel_id`. Once deleted it stays
    /// deleted: a later "left" for the same channel does not undo that. A
    /// channel not stored here has nothing to keep listed and is skipped.
    pub fn mark_former(&self, channel_id: &ChannelId, deleted: bool) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO former_channels (channel_id, deleted) SELECT id, ?2 FROM channels WHERE id = ?1
             ON CONFLICT(channel_id) DO UPDATE SET deleted = deleted OR excluded.deleted",
            params![channel_id.to_string(), deleted],
        )?;
        Ok(())
    }

    /// `Some(deleted)` for a channel we used to be in, `None` otherwise.
    pub fn former_state(&self, channel_id: &ChannelId) -> Result<Option<bool>, StorageError> {
        let conn = self.db.read_connection()?;
        Ok(conn
            .query_row(
                "SELECT deleted FROM former_channels WHERE channel_id = ?1",
                params![channel_id.to_string()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// List all channels of a specific type.
    pub fn list_by_type(&self, channel_type: ChannelType) -> Result<Vec<Channel>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_type, name, description, icon, owner_id,
                    persistence, max_members, default_ttl_seconds,
                    allow_reactions, allow_threads, allow_attachments, require_encryption,
                    created_at, updated_at, archived,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = channels.id AND conversation_type = 'channel' AND is_read = 0 AND is_deleted = 0) as unread_count,
                    (SELECT MAX(created_at) FROM messages WHERE conversation_id = channels.id AND conversation_type = 'channel' AND is_deleted = 0) as last_message_at,
                    (SELECT COUNT(*) FROM memberships WHERE channel_id = channels.id) as member_count
             FROM channels WHERE channel_type = ?1
             ORDER BY last_message_at DESC NULLS LAST",
        )?;

        let mut rows = stmt.query(params![channel_type_to_i32(channel_type)])?;
        let mut channels = Vec::new();

        while let Some(row) = rows.next()? {
            channels.push(row_to_channel(row)?);
        }

        Ok(channels)
    }

    // ========================================================================
    // Unread tracking
    // ========================================================================

    /// Update the unread count for a channel.
    pub fn update_unread_count(&self, _id: &ChannelId, _count: u32) -> Result<(), StorageError> {
        // No-op: unread count is computed dynamically from the messages table
        Ok(())
    }

    /// Increment the unread count for a channel by 1.
    pub fn increment_unread_count(&self, _id: &ChannelId) -> Result<u32, StorageError> {
        // No-op: unread count is computed dynamically from the messages table
        // Return 0 as we don't have the current count here and it shouldn't be relied upon
        Ok(0)
    }

    /// Update the last message timestamp for a channel.
    pub fn update_last_message(
        &self,
        _id: &ChannelId,
        _at: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        // No-op: last message timestamp is computed dynamically from the messages table
        Ok(())
    }

    // ========================================================================
    // Membership operations
    // ========================================================================

    /// Record a membership. Idempotent: the same join can arrive more than
    /// once (the MemberAdded broadcast reaches the joiner too, then the
    /// invite-accept path writes it), so an existing row only takes the new
    /// role and keeps the member's local state (read position, notification
    /// choice).
    pub fn insert_membership(&self, membership: &Membership) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO memberships (
                channel_id, user_id, role, joined_at, last_read_message_id,
                is_muted, muted_until, notification_preference
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(channel_id, user_id) DO UPDATE SET role = excluded.role",
            params![
                membership.channel_id.to_string(),
                membership.user_id.to_string(),
                role_to_i32(membership.role),
                membership.joined_at.to_rfc3339(),
                membership.last_read_message_id.map(|id| id.to_string()),
                membership.is_muted as i32,
                membership.muted_until.map(|dt| dt.to_rfc3339()),
                notification_pref_to_i32(membership.notification_preference),
            ],
        )?;
        Ok(())
    }

    /// Insert a membership if it doesn't already exist.
    ///
    /// Returns Ok(true) if inserted, Ok(false) if already existed.
    pub fn insert_or_ignore_membership(
        &self,
        membership: &Membership,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "INSERT OR IGNORE INTO memberships (
                channel_id, user_id, role, joined_at, last_read_message_id,
                is_muted, muted_until, notification_preference
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                membership.channel_id.to_string(),
                membership.user_id.to_string(),
                role_to_i32(membership.role),
                membership.joined_at.to_rfc3339(),
                membership.last_read_message_id.map(|id| id.to_string()),
                membership.is_muted as i32,
                membership.muted_until.map(|dt| dt.to_rfc3339()),
                notification_pref_to_i32(membership.notification_preference),
            ],
        )?;
        Ok(rows > 0)
    }

    /// Get a membership.
    pub fn get_membership(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
    ) -> Result<Option<Membership>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT channel_id, user_id, role, joined_at, last_read_message_id,
                    is_muted, muted_until, notification_preference
             FROM memberships WHERE channel_id = ?1 AND user_id = ?2",
        )?;

        let result = stmt.query_row(
            params![channel_id.to_string(), user_id.to_string()],
            row_to_membership,
        );

        match result {
            Ok(membership) => Ok(Some(membership)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Update a membership.
    pub fn update_membership(&self, membership: &Membership) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE memberships SET
                role = ?1, last_read_message_id = ?2, is_muted = ?3,
                muted_until = ?4, notification_preference = ?5
             WHERE channel_id = ?6 AND user_id = ?7",
            params![
                role_to_i32(membership.role),
                membership.last_read_message_id.map(|id| id.to_string()),
                membership.is_muted as i32,
                membership.muted_until.map(|dt| dt.to_rfc3339()),
                notification_pref_to_i32(membership.notification_preference),
                membership.channel_id.to_string(),
                membership.user_id.to_string(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Membership {} in channel {}",
                membership.user_id, membership.channel_id
            )));
        }
        Ok(())
    }

    /// Replace the local roster with `members`, the server's full list.
    ///
    /// Inserts anyone new, refreshes server-owned columns on anyone still
    /// present (`role`, `joined_at`, moderator mute), and deletes anyone
    /// absent. `last_read_message_id` and `notification_preference` are
    /// local-only (the member proto does not carry them) and are left as
    /// stored. An empty `members` deletes nothing: a successful empty read
    /// is not a roster, and wiping it would also make departed-mailbox
    /// cleanup skip every address.
    pub fn replace_memberships(
        &self,
        channel_id: &ChannelId,
        members: &[Membership],
    ) -> Result<MembershipRefresh, StorageError> {
        if members.is_empty() {
            tracing::warn!(%channel_id, "membership refresh was empty; local roster left unchanged");
            return Ok(MembershipRefresh::default());
        }
        let keep: HashSet<UserId> = members.iter().map(|m| m.user_id).collect();
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let local = {
            let mut stmt = tx.prepare(
                "SELECT channel_id, user_id, role, joined_at, last_read_message_id,
                        is_muted, muted_until, notification_preference
                 FROM memberships WHERE channel_id = ?1",
            )?;
            let rows = stmt
                .query_map(params![channel_id.to_string()], row_to_membership)?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut removed = Vec::new();
        for member in local {
            if keep.contains(&member.user_id) {
                continue;
            }
            tx.execute(
                "DELETE FROM memberships WHERE channel_id = ?1 AND user_id = ?2",
                params![channel_id.to_string(), member.user_id.to_string()],
            )?;
            removed.push(member);
        }
        let mut added = Vec::new();
        for member in members {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memberships WHERE channel_id = ?1 AND user_id = ?2)",
                params![channel_id.to_string(), member.user_id.to_string()],
                |row| row.get(0),
            )?;
            if exists {
                tx.execute(
                    "UPDATE memberships SET role = ?1, joined_at = ?2, is_muted = ?3, muted_until = ?4
                     WHERE channel_id = ?5 AND user_id = ?6",
                    params![
                        role_to_i32(member.role),
                        member.joined_at.to_rfc3339(),
                        member.is_muted as i32,
                        member.muted_until.map(|dt| dt.to_rfc3339()),
                        channel_id.to_string(),
                        member.user_id.to_string(),
                    ],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO memberships (
                        channel_id, user_id, role, joined_at, last_read_message_id,
                        is_muted, muted_until, notification_preference
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        channel_id.to_string(),
                        member.user_id.to_string(),
                        role_to_i32(member.role),
                        member.joined_at.to_rfc3339(),
                        member.last_read_message_id.map(|id| id.to_string()),
                        member.is_muted as i32,
                        member.muted_until.map(|dt| dt.to_rfc3339()),
                        notification_pref_to_i32(member.notification_preference),
                    ],
                )?;
                added.push(member.clone());
            }
        }
        tx.commit()?;
        Ok(MembershipRefresh { added, removed })
    }

    /// Delete a membership.
    pub fn delete_membership(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM memberships WHERE channel_id = ?1 AND user_id = ?2",
            params![channel_id.to_string(), user_id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Membership {} in channel {}",
                user_id, channel_id
            )));
        }
        Ok(())
    }

    /// List all members of a channel.
    pub fn list_members(&self, channel_id: &ChannelId) -> Result<Vec<Membership>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT channel_id, user_id, role, joined_at, last_read_message_id,
                    is_muted, muted_until, notification_preference
             FROM memberships WHERE channel_id = ?1
             ORDER BY joined_at",
        )?;

        let mut rows = stmt.query(params![channel_id.to_string()])?;
        let mut memberships = Vec::new();

        while let Some(row) = rows.next()? {
            memberships.push(row_to_membership(row)?);
        }

        Ok(memberships)
    }

    /// Return the channel IDs where `user_id` holds a membership. Used by
    /// the PEER_DEVICES_CHANGED handler to enumerate groups that must be
    /// marked for sender-key rotation when a peer's device set changes.
    /// Returns only channel IDs (not full channel rows) since the caller
    /// only needs the set for per-channel rotation-flag writes.
    pub fn list_channels_for_user(&self, user_id: &UserId) -> Result<Vec<ChannelId>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare("SELECT channel_id FROM memberships WHERE user_id = ?1")?;

        let mut rows = stmt.query(params![user_id.to_string()])?;
        let mut channel_ids = Vec::new();

        while let Some(row) = rows.next()? {
            let id_str: String = row.get(0)?;
            match id_str.parse::<ChannelId>() {
                Ok(id) => channel_ids.push(id),
                Err(e) => {
                    tracing::warn!(
                        value = %id_str,
                        error = %e,
                        "list_channels_for_user: skipping row with invalid channel_id"
                    );
                }
            }
        }

        Ok(channel_ids)
    }

    /// Update the last read message for a user in a channel.
    pub fn update_last_read(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        message_id: &MessageId,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE memberships SET last_read_message_id = ?1 WHERE channel_id = ?2 AND user_id = ?3",
            params![message_id.to_string(), channel_id.to_string(), user_id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Membership {} in channel {}",
                user_id, channel_id
            )));
        }
        Ok(())
    }

    /// Update the default TTL (disappearing timer) for a channel.
    pub fn set_default_ttl(
        &self,
        channel_id: &ChannelId,
        ttl_seconds: Option<u32>,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE channels SET default_ttl_seconds = ?1 WHERE id = ?2",
            params![ttl_seconds, channel_id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Channel {}", channel_id)));
        }
        Ok(())
    }

    /// Delete a channel and all its memberships locally.
    pub fn delete_channel_cascade(&self, channel_id: &ChannelId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM memberships WHERE channel_id = ?1",
            params![channel_id.to_string()],
        )?;
        conn.execute(
            "DELETE FROM channels WHERE id = ?1",
            params![channel_id.to_string()],
        )?;
        Ok(())
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// Parse an RFC3339 datetime string, falling back to now if invalid.
fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// Convert a row to a Channel.
fn row_to_channel(row: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
    let last_message_at: Option<String> = row.get(17)?;

    Ok(Channel {
        id: row
            .get::<_, String>(0)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        channel_type: channel_type_from_i32(row.get(1)?),
        name: row.get(2)?,
        description: row.get(3)?,
        icon: row.get(4)?,
        owner_id: row
            .get::<_, String>(5)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        config: ChannelConfig {
            persistence: persistence_from_i32(row.get(6)?),
            max_members: row.get(7)?,
            default_ttl_seconds: row.get(8)?,
            allow_reactions: row.get::<_, i32>(9)? != 0,
            allow_threads: row.get::<_, i32>(10)? != 0,
            allow_attachments: row.get::<_, i32>(11)? != 0,
            require_encryption: row.get::<_, i32>(12)? != 0,
        },
        created_at: parse_datetime(&row.get::<_, String>(13)?),
        updated_at: parse_datetime(&row.get::<_, String>(14)?),
        archived: row.get::<_, i32>(15)? != 0,
        unread_count: row.get(16)?,
        last_message_at: last_message_at.map(|s| parse_datetime(&s)),
        member_count: row.get(18)?,
    })
}

/// Convert a row to a Membership.
fn row_to_membership(row: &rusqlite::Row<'_>) -> rusqlite::Result<Membership> {
    let last_read: Option<String> = row.get(4)?;
    let muted_until: Option<String> = row.get(6)?;

    Ok(Membership {
        channel_id: row
            .get::<_, String>(0)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        user_id: row
            .get::<_, String>(1)?
            .parse()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        role: role_from_i32(row.get(2)?),
        joined_at: parse_datetime(&row.get::<_, String>(3)?),
        last_read_message_id: last_read
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
        is_muted: row.get::<_, i32>(5)? != 0,
        muted_until: muted_until.map(|s| parse_datetime(&s)),
        notification_preference: notification_pref_from_i32(row.get(7)?),
    })
}

// Enum conversion helpers

fn channel_type_to_i32(ct: ChannelType) -> i32 {
    match ct {
        ChannelType::Public => 0,
        ChannelType::Private => 1,
        ChannelType::SavedMessages => 3,
    }
}

fn channel_type_from_i32(value: i32) -> ChannelType {
    match value {
        1 => ChannelType::Private,
        3 => ChannelType::SavedMessages,
        _ => ChannelType::Public,
    }
}

fn persistence_to_i32(p: MessagePersistence) -> i32 {
    match p {
        MessagePersistence::Permanent => 0,
        MessagePersistence::Ephemeral => 1,
        MessagePersistence::Hybrid => 2,
    }
}

fn persistence_from_i32(value: i32) -> MessagePersistence {
    match value {
        1 => MessagePersistence::Ephemeral,
        2 => MessagePersistence::Hybrid,
        _ => MessagePersistence::Permanent,
    }
}

fn role_to_i32(r: MemberRole) -> i32 {
    match r {
        MemberRole::Member => 0,
        MemberRole::Moderator => 1,
        MemberRole::Admin => 2,
        MemberRole::Owner => 3,
    }
}

fn role_from_i32(value: i32) -> MemberRole {
    match value {
        1 => MemberRole::Moderator,
        2 => MemberRole::Admin,
        3 => MemberRole::Owner,
        _ => MemberRole::Member,
    }
}

fn notification_pref_to_i32(np: NotificationPreference) -> i32 {
    match np {
        NotificationPreference::All => 0,
        NotificationPreference::Mentions => 1,
        NotificationPreference::None => 2,
    }
}

fn notification_pref_from_i32(value: i32) -> NotificationPreference {
    match value {
        1 => NotificationPreference::Mentions,
        2 => NotificationPreference::None,
        _ => NotificationPreference::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;
    use tempfile::tempdir;

    fn create_test_repo() -> (ChannelRepository, tempfile::TempDir) {
        let temp_dir = tempdir().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("test.db");
        let db = Database::open(&db_path, "test-passphrase").expect("Failed to open test database");

        {
            let conn = db.connection().expect("Failed to get connection");
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS channels (
                    id TEXT PRIMARY KEY,
                    channel_type INTEGER NOT NULL DEFAULT 0,
                    name TEXT NOT NULL,
                    description TEXT,
                    icon BLOB,
                    owner_id TEXT NOT NULL,
                    persistence INTEGER NOT NULL DEFAULT 0,
                    max_members INTEGER,
                    default_ttl_seconds INTEGER,
                    allow_reactions INTEGER NOT NULL DEFAULT 1,
                    allow_threads INTEGER NOT NULL DEFAULT 1,
                    allow_attachments INTEGER NOT NULL DEFAULT 1,
                    require_encryption INTEGER NOT NULL DEFAULT 1,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE IF NOT EXISTS memberships (
                    channel_id TEXT NOT NULL,
                    user_id TEXT NOT NULL,
                    role INTEGER NOT NULL DEFAULT 0,
                    joined_at TEXT NOT NULL,
                    last_read_message_id TEXT,
                    is_muted INTEGER NOT NULL DEFAULT 0,
                    muted_until TEXT,
                    notification_preference INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (channel_id, user_id)
                );
                CREATE TABLE IF NOT EXISTS messages (
                    id TEXT PRIMARY KEY,
                    conversation_type TEXT NOT NULL,
                    conversation_id TEXT NOT NULL,
                    is_read INTEGER NOT NULL DEFAULT 0,
                    is_deleted INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL
                );
                "#,
            )
            .expect("Failed to create tables");
        }

        let repo = ChannelRepository::new(Arc::new(db));
        (repo, temp_dir)
    }

    fn make_channel(id: ChannelId) -> Channel {
        Channel::new(
            id,
            "test-channel".into(),
            ChannelType::Public,
            UserId::new(),
        )
    }

    #[test]
    fn the_same_membership_recorded_twice_is_one_row() {
        // The MemberAdded broadcast and the invite-accept path both record the
        // joiner; the second write must not fail (it used to abort the accept).
        let (repo, _dir) = create_test_repo();
        let channel = ChannelId::new();
        let user = UserId::new();
        repo.insert_membership(&make_membership(channel, user))
            .unwrap();
        let mut again = make_membership(channel, user);
        again.role = MemberRole::Moderator;
        repo.insert_membership(&again).unwrap();
        let stored = repo.get_membership(&channel, &user).unwrap().unwrap();
        assert_eq!(stored.role, MemberRole::Moderator);
    }

    #[test]
    fn a_full_refresh_deletes_members_absent_from_the_list_and_keeps_local_state() {
        use crate::domain::ids::MessageId;

        let (repo, _dir) = create_test_repo();
        let channel = ChannelId::new();
        repo.insert(&make_channel(channel)).unwrap();
        let (alice, bob, carol, dave) =
            (UserId::new(), UserId::new(), UserId::new(), UserId::new());
        let mut alice_row = make_membership(channel, alice);
        alice_row.notification_preference = NotificationPreference::Mentions;
        repo.insert_membership(&alice_row).unwrap();
        let read = MessageId::new();
        repo.update_last_read(&channel, &alice, &read).unwrap();
        repo.insert_membership(&make_membership(channel, bob))
            .unwrap();
        repo.insert_membership(&make_membership(channel, carol))
            .unwrap();

        let mut alice_now = make_membership(channel, alice);
        alice_now.role = MemberRole::Admin;
        alice_now.notification_preference = NotificationPreference::None;
        alice_now.last_read_message_id = None;
        let refresh = repo
            .replace_memberships(
                &channel,
                &[
                    alice_now,
                    make_membership(channel, bob),
                    make_membership(channel, dave),
                ],
            )
            .unwrap();
        let ids = |rows: &[Membership]| {
            let mut ids: Vec<_> = rows.iter().map(|m| m.user_id).collect();
            ids.sort_by_key(|id| id.to_string());
            ids
        };
        assert_eq!(ids(&refresh.removed), vec![carol]);
        assert_eq!(ids(&refresh.added), vec![dave]);

        let left = repo.list_members(&channel).unwrap();
        assert_eq!(ids(&left), {
            let mut ids = vec![alice, bob, dave];
            ids.sort_by_key(|id| id.to_string());
            ids
        });
        let stored = repo.get_membership(&channel, &alice).unwrap().unwrap();
        assert_eq!(stored.role, MemberRole::Admin);
        assert_eq!(stored.last_read_message_id, Some(read));
        assert_eq!(
            stored.notification_preference,
            NotificationPreference::Mentions
        );
        assert!(repo.get_membership(&channel, &carol).unwrap().is_none());

        let untouched = repo.replace_memberships(&channel, &[]).unwrap();
        assert!(untouched.removed.is_empty());
        assert_eq!(
            repo.list_members(&channel).unwrap().len(),
            3,
            "an empty refresh must not wipe the roster"
        );
    }

    fn make_membership(channel_id: ChannelId, user_id: UserId) -> Membership {
        Membership {
            channel_id,
            user_id,
            role: MemberRole::Member,
            joined_at: Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
            notification_preference: NotificationPreference::All,
        }
    }

    #[test]
    fn test_set_default_ttl() {
        let (repo, _dir) = create_test_repo();
        let channel_id = ChannelId::new();
        let channel = make_channel(channel_id);

        repo.insert(&channel).unwrap();

        // Initially no TTL
        let ch = repo.get(&channel_id).unwrap().unwrap();
        assert_eq!(ch.config.default_ttl_seconds, None);

        // Set to 3600
        repo.set_default_ttl(&channel_id, Some(3600)).unwrap();
        let ch = repo.get(&channel_id).unwrap().unwrap();
        assert_eq!(ch.config.default_ttl_seconds, Some(3600));

        // Disable (set to None)
        repo.set_default_ttl(&channel_id, None).unwrap();
        let ch = repo.get(&channel_id).unwrap().unwrap();
        assert_eq!(ch.config.default_ttl_seconds, None);
    }

    #[test]
    fn test_set_default_ttl_not_found() {
        let (repo, _dir) = create_test_repo();
        let result = repo.set_default_ttl(&ChannelId::new(), Some(300));
        assert!(result.is_err());
    }

    #[test]
    fn test_delete_channel_cascade() {
        let (repo, _dir) = create_test_repo();
        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let channel = make_channel(channel_id);
        let membership = make_membership(channel_id, user_id);

        repo.insert(&channel).unwrap();
        repo.insert_membership(&membership).unwrap();

        // Verify exists
        assert!(repo.get(&channel_id).unwrap().is_some());
        assert!(
            repo.get_membership(&channel_id, &user_id)
                .unwrap()
                .is_some()
        );

        // Cascade delete
        repo.delete_channel_cascade(&channel_id).unwrap();

        // Both should be gone
        assert!(repo.get(&channel_id).unwrap().is_none());
        assert!(
            repo.get_membership(&channel_id, &user_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn confirmed_delete_is_atomic_scoped_and_keeps_history() {
        let (repo, _dir) = create_test_repo();
        let id = ChannelId::new();
        let other = ChannelId::new();
        let me = UserId::new();
        for target in [id, other] {
            repo.insert(&make_channel(target)).unwrap();
            repo.insert_membership(&make_membership(target, me))
                .unwrap();
        }
        {
            let conn = repo.db.connection().unwrap();
            conn.execute_batch("CREATE TABLE IF NOT EXISTS verified_channel_state(channel_id TEXT PRIMARY KEY);
                CREATE TRIGGER reject_channel_delete BEFORE DELETE ON channels BEGIN SELECT RAISE(ABORT, 'fixture storage failure'); END;").unwrap();
            for target in [id, other] {
                conn.execute("INSERT INTO messages(id,conversation_type,conversation_id,created_at) VALUES(?1,'channel',?2,?3)", params![uuid::Uuid::new_v4().to_string(), target.to_string(), Utc::now().to_rfc3339()]).unwrap();
            }
        }
        assert!(repo.remove_confirmed_deleted_channel(&id).is_err());
        assert!(repo.get(&id).unwrap().is_some());
        assert!(
            repo.get_membership(&id, &me).unwrap().is_some(),
            "failed cleanup must roll back membership removal"
        );
        repo.db
            .connection()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_channel_delete;")
            .unwrap();
        repo.remove_confirmed_deleted_channel(&id).unwrap();
        repo.remove_confirmed_deleted_channel(&id).unwrap();
        assert!(repo.get(&id).unwrap().is_none());
        assert!(repo.get_membership(&id, &me).unwrap().is_none());
        assert!(repo.get(&other).unwrap().is_some());
        assert!(repo.get_membership(&other, &me).unwrap().is_some());
        let count: i64 = repo
            .db
            .connection()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2, "owner cleanup must not delete message history");
    }

    #[test]
    fn test_delete_channel_cascade_nonexistent() {
        let (repo, _dir) = create_test_repo();
        // Should succeed even if nothing to delete (no error)
        let result = repo.delete_channel_cascade(&ChannelId::new());
        assert!(result.is_ok());
    }
}

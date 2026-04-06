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
use rusqlite::params;
use std::sync::Arc;

/// Channel data repository.
///
/// Provides CRUD operations for channels and memberships.
pub struct ChannelRepository {
    db: Arc<Database>,
}

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
                id, channel_type, name, description, image_url, owner_id,
                persistence, max_members, default_ttl_seconds,
                allow_reactions, allow_threads, allow_attachments, require_encryption,
                created_at, updated_at, archived
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                channel.id.to_string(),
                channel_type_to_i32(channel.channel_type),
                channel.name,
                channel.description,
                channel.image_url,
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
                id, channel_type, name, description, image_url, owner_id,
                persistence, max_members, default_ttl_seconds,
                allow_reactions, allow_threads, allow_attachments, require_encryption,
                created_at, updated_at, archived
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                channel.id.to_string(),
                channel_type_to_i32(channel.channel_type),
                channel.name,
                channel.description,
                channel.image_url,
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
            "SELECT id, channel_type, name, description, image_url, owner_id,
                    persistence, max_members, default_ttl_seconds,
                    allow_reactions, allow_threads, allow_attachments, require_encryption,
                    created_at, updated_at, archived,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = channels.id AND conversation_type = 'channel' AND is_read = 0 AND is_deleted = 0) as unread_count,
                    (SELECT MAX(created_at) FROM messages WHERE conversation_id = channels.id AND conversation_type = 'channel' AND is_deleted = 0) as last_message_at
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
                channel_type = ?1, name = ?2, description = ?3, image_url = ?4, owner_id = ?5,
                persistence = ?6, max_members = ?7, default_ttl_seconds = ?8,
                allow_reactions = ?9, allow_threads = ?10, allow_attachments = ?11, require_encryption = ?12,
                updated_at = ?13, archived = ?14
             WHERE id = ?15",
            params![
                channel_type_to_i32(channel.channel_type),
                channel.name,
                channel.description,
                channel.image_url,
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
                id, channel_type, name, description, image_url, owner_id,
                persistence, max_members, default_ttl_seconds,
                allow_reactions, allow_threads, allow_attachments, require_encryption,
                created_at, updated_at, archived
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            ON CONFLICT(id) DO UPDATE SET
                channel_type = excluded.channel_type,
                name = excluded.name,
                description = excluded.description,
                image_url = excluded.image_url,
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
                channel.image_url,
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
        let rows = conn.execute("DELETE FROM channels WHERE id = ?1", params![id.to_string()])?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Channel {}", id)));
        }
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
        let mut stmt = conn.prepare(
            "SELECT c.id, c.channel_type, c.name, c.description, c.image_url, c.owner_id,
                    c.persistence, c.max_members, c.default_ttl_seconds,
                    c.allow_reactions, c.allow_threads, c.allow_attachments, c.require_encryption,
                    c.created_at, c.updated_at, c.archived,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = c.id AND conversation_type = 'channel' AND is_read = 0 AND is_deleted = 0) as unread_count,
                    (SELECT MAX(created_at) FROM messages WHERE conversation_id = c.id AND conversation_type = 'channel' AND is_deleted = 0) as last_message_at
             FROM channels c
             INNER JOIN memberships m ON c.id = m.channel_id
             WHERE m.user_id = ?1
             ORDER BY last_message_at DESC NULLS LAST
             LIMIT ?2 OFFSET ?3",
        )?;

        let mut rows = stmt.query(params![user_id.to_string(), limit, offset])?;
        let mut channels = Vec::new();

        while let Some(row) = rows.next()? {
            channels.push(row_to_channel(row)?);
        }

        Ok(channels)
    }

    /// Get DM channel by peer user ID.
    ///
    /// Looks up the DM channel where both the current user and the peer are members.
    /// This returns the server-assigned channel ID, NOT a derived one.
    pub fn get_dm_channel_by_peer(
        &self,
        current_user_id: &UserId,
        peer_id: &UserId,
    ) -> Result<Option<Channel>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT c.id, c.channel_type, c.name, c.description, c.image_url, c.owner_id,
                    c.persistence, c.max_members, c.default_ttl_seconds,
                    c.allow_reactions, c.allow_threads, c.allow_attachments, c.require_encryption,
                    c.created_at, c.updated_at, c.archived,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = c.id AND conversation_type = 'channel' AND is_read = 0 AND is_deleted = 0) as unread_count,
                    (SELECT MAX(created_at) FROM messages WHERE conversation_id = c.id AND conversation_type = 'channel' AND is_deleted = 0) as last_message_at
             FROM channels c
             INNER JOIN memberships m1 ON c.id = m1.channel_id AND m1.user_id = ?1
             INNER JOIN memberships m2 ON c.id = m2.channel_id AND m2.user_id = ?2
             WHERE c.channel_type = ?3",
        )?;

        let result = stmt.query_row(
            params![
                current_user_id.to_string(),
                peer_id.to_string(),
                channel_type_to_i32(ChannelType::DirectMessage)
            ],
            row_to_channel,
        );

        match result {
            Ok(channel) => Ok(Some(channel)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// List all channels of a specific type.
    pub fn list_by_type(&self, channel_type: ChannelType) -> Result<Vec<Channel>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_type, name, description, image_url, owner_id,
                    persistence, max_members, default_ttl_seconds,
                    allow_reactions, allow_threads, allow_attachments, require_encryption,
                    created_at, updated_at, archived,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = channels.id AND conversation_type = 'channel' AND is_read = 0 AND is_deleted = 0) as unread_count,
                    (SELECT MAX(created_at) FROM messages WHERE conversation_id = channels.id AND conversation_type = 'channel' AND is_deleted = 0) as last_message_at
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

    /// Insert a new membership.
    pub fn insert_membership(&self, membership: &Membership) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO memberships (
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
        Ok(())
    }

    /// Insert a membership if it doesn't already exist.
    ///
    /// Returns Ok(true) if inserted, Ok(false) if already existed.
    pub fn insert_or_ignore_membership(&self, membership: &Membership) -> Result<bool, StorageError> {
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

    /// Update the muted state for a user's membership in a channel.
    pub fn set_membership_muted(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        muted: bool,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE memberships SET is_muted = ?1, muted_until = NULL WHERE channel_id = ?2 AND user_id = ?3",
            params![muted as i32, channel_id.to_string(), user_id.to_string()],
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
        image_url: row.get(4)?,
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
        ChannelType::DirectMessage => 2,
        ChannelType::SavedMessages => 3,
    }
}

fn channel_type_from_i32(value: i32) -> ChannelType {
    match value {
        1 => ChannelType::Private,
        2 => ChannelType::DirectMessage,
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
        let db = Database::open(&db_path, "test-passphrase")
            .expect("Failed to open test database");

        {
            let conn = db.connection().expect("Failed to get connection");
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS channels (
                    id TEXT PRIMARY KEY,
                    channel_type INTEGER NOT NULL DEFAULT 0,
                    name TEXT NOT NULL,
                    description TEXT,
                    image_url TEXT,
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
        Channel::new(id, "test-channel".into(), ChannelType::Public, UserId::new())
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
    fn test_set_membership_muted() {
        let (repo, _dir) = create_test_repo();
        let channel_id = ChannelId::new();
        let user_id = UserId::new();
        let channel = make_channel(channel_id);
        let membership = make_membership(channel_id, user_id);

        repo.insert(&channel).unwrap();
        repo.insert_membership(&membership).unwrap();

        // Initially not muted
        let m = repo.get_membership(&channel_id, &user_id).unwrap().unwrap();
        assert!(!m.is_muted);

        // Mute
        repo.set_membership_muted(&channel_id, &user_id, true).unwrap();
        let m = repo.get_membership(&channel_id, &user_id).unwrap().unwrap();
        assert!(m.is_muted);
        assert!(m.muted_until.is_none()); // cleared by set_membership_muted

        // Unmute
        repo.set_membership_muted(&channel_id, &user_id, false).unwrap();
        let m = repo.get_membership(&channel_id, &user_id).unwrap().unwrap();
        assert!(!m.is_muted);
    }

    #[test]
    fn test_set_membership_muted_not_found() {
        let (repo, _dir) = create_test_repo();
        let result = repo.set_membership_muted(&ChannelId::new(), &UserId::new(), true);
        assert!(result.is_err());
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
        assert!(repo.get_membership(&channel_id, &user_id).unwrap().is_some());

        // Cascade delete
        repo.delete_channel_cascade(&channel_id).unwrap();

        // Both should be gone
        assert!(repo.get(&channel_id).unwrap().is_none());
        assert!(repo.get_membership(&channel_id, &user_id).unwrap().is_none());
    }

    #[test]
    fn test_delete_channel_cascade_nonexistent() {
        let (repo, _dir) = create_test_repo();
        // Should succeed even if nothing to delete (no error)
        let result = repo.delete_channel_cascade(&ChannelId::new());
        assert!(result.is_ok());
    }
}

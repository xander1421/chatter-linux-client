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
                created_at, updated_at, archived, unread_count, last_message_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
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
                channel.unread_count,
                channel.last_message_at.map(|dt| dt.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    /// Get a channel by ID.
    pub fn get(&self, id: &ChannelId) -> Result<Option<Channel>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, channel_type, name, description, image_url, owner_id,
                    persistence, max_members, default_ttl_seconds,
                    allow_reactions, allow_threads, allow_attachments, require_encryption,
                    created_at, updated_at, archived, unread_count, last_message_at
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
                updated_at = ?13, archived = ?14, unread_count = ?15, last_message_at = ?16
             WHERE id = ?17",
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
                channel.unread_count,
                channel.last_message_at.map(|dt| dt.to_rfc3339()),
                channel.id.to_string(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Channel {}", channel.id)));
        }
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

    /// List all channels for a user (via membership), ordered by last activity.
    pub fn list_for_user(&self, user_id: &UserId) -> Result<Vec<Channel>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT c.id, c.channel_type, c.name, c.description, c.image_url, c.owner_id,
                    c.persistence, c.max_members, c.default_ttl_seconds,
                    c.allow_reactions, c.allow_threads, c.allow_attachments, c.require_encryption,
                    c.created_at, c.updated_at, c.archived, c.unread_count, c.last_message_at
             FROM channels c
             INNER JOIN memberships m ON c.id = m.channel_id
             WHERE m.user_id = ?1
             ORDER BY c.last_message_at DESC NULLS LAST",
        )?;

        let mut rows = stmt.query(params![user_id.to_string()])?;
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
                    c.created_at, c.updated_at, c.archived, c.unread_count, c.last_message_at
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
                    created_at, updated_at, archived, unread_count, last_message_at
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
    pub fn update_unread_count(&self, id: &ChannelId, count: u32) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE channels SET unread_count = ?1 WHERE id = ?2",
            params![count, id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Channel {}", id)));
        }
        Ok(())
    }

    /// Increment the unread count for a channel by 1.
    ///
    /// Returns the new unread count, or creates the channel with count 1 if it doesn't exist.
    pub fn increment_unread_count(&self, id: &ChannelId) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;

        // Atomically increment unread_count
        let rows = conn.execute(
            "UPDATE channels SET unread_count = unread_count + 1 WHERE id = ?1",
            params![id.to_string()],
        )?;

        if rows == 0 {
            // Channel doesn't exist in local storage - this is expected for DMs
            // where we might receive messages before the channel is synced
            return Ok(1);
        }

        // Get the new count
        let count: u32 = conn.query_row(
            "SELECT unread_count FROM channels WHERE id = ?1",
            params![id.to_string()],
            |row| row.get(0),
        )?;

        Ok(count)
    }

    /// Update the last message timestamp for a channel.
    pub fn update_last_message(
        &self,
        id: &ChannelId,
        at: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE channels SET last_message_at = ?1 WHERE id = ?2",
            params![at.to_rfc3339(), id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("Channel {}", id)));
        }
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

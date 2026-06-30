//! User repository.
//!
//! Data access for user, profile, device, and friendship entities.

use crate::domain::ids::{DeviceId, UserId};
use crate::domain::user::{Device, Friendship, FriendshipStatus, PresenceStatus, Profile, StatusMessage, User};
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::sync::Arc;

/// User data repository.
///
/// Provides CRUD operations for users, profiles, devices, and friendships.
pub struct UserRepository {
    db: Arc<Database>,
}

impl UserRepository {
    /// Create a new UserRepository with the given database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // User CRUD
    // ========================================================================

    /// Insert a new user.
    pub fn insert(&self, user: &User) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO users (id, username, svr_public_key, last_active, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                user.id.to_string(),
                user.username,
                user.svr_public_key,
                user.last_active.to_rfc3339(),
                user.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Ensure a user record exists (for FK constraints).
    ///
    /// Creates a minimal placeholder record if the user doesn't exist.
    /// This is needed because the profiles table has a FK to users.
    /// The placeholder uses the user_id as username and empty svr_public_key.
    pub fn ensure_user_exists(&self, user_id: &UserId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;

        // Use INSERT OR IGNORE to only insert if not exists
        conn.execute(
            "INSERT OR IGNORE INTO users (id, username, svr_public_key, last_active, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                user_id.to_string(),
                user_id.to_string(), // Use ID as placeholder username
                Vec::<u8>::new(),    // Empty SVR public key placeholder
                Utc::now().to_rfc3339(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get a user by ID.
    pub fn get(&self, id: &UserId) -> Result<Option<User>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, username, svr_public_key, last_active, created_at
             FROM users WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.to_string()], |row| {
            Ok(User {
                id: row.get::<_, String>(0)?.parse().map_err(|_| {
                    rusqlite::Error::InvalidParameterName("Invalid UUID".into())
                })?,
                username: row.get(1)?,
                svr_public_key: row.get(2)?,
                last_active: parse_datetime(&row.get::<_, String>(3)?),
                created_at: parse_datetime(&row.get::<_, String>(4)?),
            })
        });

        match result {
            Ok(user) => Ok(Some(user)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Count total number of users in the database.
    pub fn count(&self) -> Result<u64, StorageError> {
        let conn = self.db.connection()?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    /// Get a user by username.
    pub fn get_by_username(&self, username: &str) -> Result<Option<User>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, username, svr_public_key, last_active, created_at
             FROM users WHERE username = ?1",
        )?;

        let result = stmt.query_row(params![username], |row| {
            Ok(User {
                id: row.get::<_, String>(0)?.parse().map_err(|_| {
                    rusqlite::Error::InvalidParameterName("Invalid UUID".into())
                })?,
                username: row.get(1)?,
                svr_public_key: row.get(2)?,
                last_active: parse_datetime(&row.get::<_, String>(3)?),
                created_at: parse_datetime(&row.get::<_, String>(4)?),
            })
        });

        match result {
            Ok(user) => Ok(Some(user)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Update an existing user.
    pub fn update(&self, user: &User) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE users SET username = ?1, svr_public_key = ?2, last_active = ?3
             WHERE id = ?4",
            params![
                user.username,
                user.svr_public_key,
                user.last_active.to_rfc3339(),
                user.id.to_string(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("User {}", user.id)));
        }
        Ok(())
    }

    /// Delete a user by ID.
    pub fn delete(&self, id: &UserId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute("DELETE FROM users WHERE id = ?1", params![id.to_string()])?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("User {}", id)));
        }
        Ok(())
    }

    // ========================================================================
    // Profile operations
    // ========================================================================

    /// Get a user's profile.
    pub fn get_profile(&self, user_id: &UserId) -> Result<Option<Profile>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT user_id, display_name, status_text, status_emoji, status_expires_at, presence
             FROM profiles WHERE user_id = ?1",
        )?;

        let result = stmt.query_row(params![user_id.to_string()], |row| {
            let status_text: Option<String> = row.get(2)?;
            let status_emoji: Option<String> = row.get(3)?;
            let status_expires_at: Option<String> = row.get(4)?;

            let status_message = status_text.map(|text| StatusMessage {
                text,
                emoji: status_emoji,
                expires_at: status_expires_at.map(|s| parse_datetime(&s)),
            });

            Ok(Profile {
                user_id: row.get::<_, String>(0)?.parse().map_err(|_| {
                    rusqlite::Error::InvalidParameterName("Invalid UUID".into())
                })?,
                display_name: row.get(1)?,
                status_message,
                presence: presence_from_i32(row.get(5)?),
            })
        });

        match result {
            Ok(profile) => Ok(Some(profile)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Insert or update a user's profile.
    pub fn upsert_profile(&self, profile: &Profile) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let (status_text, status_emoji, status_expires_at) = match &profile.status_message {
            Some(msg) => (
                Some(msg.text.clone()),
                msg.emoji.clone(),
                msg.expires_at.map(|dt| dt.to_rfc3339()),
            ),
            None => (None, None, None),
        };

        conn.execute(
            "INSERT INTO profiles (user_id, display_name, status_text, status_emoji, status_expires_at, presence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(user_id) DO UPDATE SET
                display_name = excluded.display_name,
                status_text = excluded.status_text,
                status_emoji = excluded.status_emoji,
                status_expires_at = excluded.status_expires_at,
                presence = excluded.presence",
            params![
                profile.user_id.to_string(),
                profile.display_name,
                status_text,
                status_emoji,
                status_expires_at,
                presence_to_i32(profile.presence),
            ],
        )?;
        Ok(())
    }

    // ========================================================================
    // Device operations
    // ========================================================================

    /// Insert a new device.
    pub fn insert_device(&self, device: &Device) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO devices (id, user_id, name, registration_id, created_at, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                device.id.as_u32(),
                device.user_id.to_string(),
                device.name,
                device.registration_id,
                device.created_at.to_rfc3339(),
                device.last_seen.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get a specific device.
    pub fn get_device(
        &self,
        user_id: &UserId,
        device_id: DeviceId,
    ) -> Result<Option<Device>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, name, registration_id, created_at, last_seen
             FROM devices WHERE user_id = ?1 AND id = ?2",
        )?;

        let result = stmt.query_row(params![user_id.to_string(), device_id.as_u32()], |row| {
            Ok(Device {
                id: DeviceId::new(row.get(0)?),
                user_id: row.get::<_, String>(1)?.parse().map_err(|_| {
                    rusqlite::Error::InvalidParameterName("Invalid UUID".into())
                })?,
                name: row.get(2)?,
                registration_id: row.get(3)?,
                created_at: parse_datetime(&row.get::<_, String>(4)?),
                last_seen: parse_datetime(&row.get::<_, String>(5)?),
            })
        });

        match result {
            Ok(device) => Ok(Some(device)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// List all devices for a user.
    pub fn list_devices(&self, user_id: &UserId) -> Result<Vec<Device>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, name, registration_id, created_at, last_seen
             FROM devices WHERE user_id = ?1 ORDER BY id",
        )?;

        let mut rows = stmt.query(params![user_id.to_string()])?;
        let mut devices = Vec::new();

        while let Some(row) = rows.next()? {
            devices.push(Device {
                id: DeviceId::new(row.get(0)?),
                user_id: row.get::<_, String>(1)?.parse().map_err(|_| {
                    rusqlite::Error::InvalidParameterName("Invalid UUID".into())
                })?,
                name: row.get(2)?,
                registration_id: row.get(3)?,
                created_at: parse_datetime(&row.get::<_, String>(4)?),
                last_seen: parse_datetime(&row.get::<_, String>(5)?),
            });
        }

        Ok(devices)
    }

    /// Delete a device.
    pub fn delete_device(&self, user_id: &UserId, device_id: DeviceId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM devices WHERE user_id = ?1 AND id = ?2",
            params![user_id.to_string(), device_id.as_u32()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Device {} for user {}",
                device_id, user_id
            )));
        }
        Ok(())
    }

    // ========================================================================
    // Friendship operations
    // ========================================================================

    /// Insert a new friendship.
    pub fn insert_friendship(&self, friendship: &Friendship) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO friendships (user_id, friend_id, status, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                friendship.requester_id.to_string(),
                friendship.addressee_id.to_string(),
                friendship_status_to_i32(friendship.status),
                friendship.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get a friendship between two users.
    pub fn get_friendship(
        &self,
        user_id: &UserId,
        friend_id: &UserId,
    ) -> Result<Option<Friendship>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT user_id, friend_id, status, created_at
             FROM friendships WHERE user_id = ?1 AND friend_id = ?2",
        )?;

        let result = stmt.query_row(
            params![user_id.to_string(), friend_id.to_string()],
            |row| {
                Ok(Friendship {
                    requester_id: row.get::<_, String>(0)?.parse().map_err(|_| {
                        rusqlite::Error::InvalidParameterName("Invalid UUID".into())
                    })?,
                    addressee_id: row.get::<_, String>(1)?.parse().map_err(|_| {
                        rusqlite::Error::InvalidParameterName("Invalid UUID".into())
                    })?,
                    status: friendship_status_from_i32(row.get(2)?),
                    created_at: parse_datetime(&row.get::<_, String>(3)?),
                })
            },
        );

        match result {
            Ok(friendship) => Ok(Some(friendship)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Update a friendship status.
    pub fn update_friendship(&self, friendship: &Friendship) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE friendships SET status = ?1 WHERE user_id = ?2 AND friend_id = ?3",
            params![
                friendship_status_to_i32(friendship.status),
                friendship.requester_id.to_string(),
                friendship.addressee_id.to_string(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Friendship {} -> {}",
                friendship.requester_id, friendship.addressee_id
            )));
        }
        Ok(())
    }

    /// List all friendships for a user.
    pub fn list_friends(&self, user_id: &UserId) -> Result<Vec<Friendship>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT user_id, friend_id, status, created_at
             FROM friendships WHERE user_id = ?1 ORDER BY created_at",
        )?;

        let mut rows = stmt.query(params![user_id.to_string()])?;
        let mut friendships = Vec::new();

        while let Some(row) = rows.next()? {
            friendships.push(Friendship {
                requester_id: row.get::<_, String>(0)?.parse().map_err(|_| {
                    rusqlite::Error::InvalidParameterName("Invalid UUID".into())
                })?,
                addressee_id: row.get::<_, String>(1)?.parse().map_err(|_| {
                    rusqlite::Error::InvalidParameterName("Invalid UUID".into())
                })?,
                status: friendship_status_from_i32(row.get(2)?),
                created_at: parse_datetime(&row.get::<_, String>(3)?),
            });
        }

        Ok(friendships)
    }

    /// Delete a friendship.
    pub fn delete_friendship(
        &self,
        user_id: &UserId,
        friend_id: &UserId,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM friendships WHERE user_id = ?1 AND friend_id = ?2",
            params![user_id.to_string(), friend_id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Friendship {} -> {}",
                user_id, friend_id
            )));
        }
        Ok(())
    }

    // ========================================================================
    // Profile key sharing operations
    // ========================================================================

    /// Get a friend's profile key for decrypting their encrypted display_name.
    ///
    /// Returns None if we don't have their profile key yet.
    pub fn get_friend_profile_key(
        &self,
        user_id: &UserId,
        friend_id: &UserId,
    ) -> Result<Option<[u8; 32]>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT friend_profile_key FROM friendships WHERE user_id = ?1 AND friend_id = ?2",
        )?;

        let result: Result<Option<Vec<u8>>, _> = stmt.query_row(
            params![user_id.to_string(), friend_id.to_string()],
            |row| row.get(0),
        );

        match result {
            Ok(Some(key_bytes)) if key_bytes.len() == 32 => {
                let mut key = [0u8; 32];
                key.copy_from_slice(&key_bytes);
                Ok(Some(key))
            }
            Ok(_) => Ok(None), // No key or wrong size
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Store a friend's profile key received from an E2E message.
    ///
    /// This key allows us to decrypt their encrypted display_name.
    pub fn set_friend_profile_key(
        &self,
        user_id: &UserId,
        friend_id: &UserId,
        key: &[u8; 32],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE friendships SET friend_profile_key = ?1 WHERE user_id = ?2 AND friend_id = ?3",
            params![key.as_slice(), user_id.to_string(), friend_id.to_string()],
        )?;

        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Friendship {} -> {}",
                user_id, friend_id
            )));
        }
        Ok(())
    }

    /// Check if profile sharing is enabled for a friendship.
    ///
    /// When true, our profile key will be included in messages to this friend.
    pub fn is_profile_sharing(
        &self,
        user_id: &UserId,
        friend_id: &UserId,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT profile_sharing FROM friendships WHERE user_id = ?1 AND friend_id = ?2",
        )?;

        let result: Result<i32, _> = stmt.query_row(
            params![user_id.to_string(), friend_id.to_string()],
            |row| row.get(0),
        );

        match result {
            Ok(value) => Ok(value != 0),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Enable or disable profile sharing for a friendship.
    ///
    /// When enabled, our profile key will be sent with messages to this friend,
    /// allowing them to decrypt our display_name.
    pub fn set_profile_sharing(
        &self,
        user_id: &UserId,
        friend_id: &UserId,
        enabled: bool,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE friendships SET profile_sharing = ?1 WHERE user_id = ?2 AND friend_id = ?3",
            params![enabled as i32, user_id.to_string(), friend_id.to_string()],
        )?;

        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Friendship {} -> {}",
                user_id, friend_id
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

/// Convert PresenceStatus to integer for storage.
fn presence_to_i32(status: PresenceStatus) -> i32 {
    match status {
        PresenceStatus::Offline => 0,
        PresenceStatus::Online => 1,
        PresenceStatus::Away => 2,
        PresenceStatus::DoNotDisturb => 3,
    }
}

/// Convert integer to PresenceStatus.
fn presence_from_i32(value: i32) -> PresenceStatus {
    match value {
        1 => PresenceStatus::Online,
        2 => PresenceStatus::Away,
        3 => PresenceStatus::DoNotDisturb,
        _ => PresenceStatus::Offline,
    }
}

/// Convert FriendshipStatus to integer for storage.
fn friendship_status_to_i32(status: FriendshipStatus) -> i32 {
    match status {
        FriendshipStatus::Pending => 0,
        FriendshipStatus::Accepted => 1,
        FriendshipStatus::Blocked => 2,
    }
}

/// Convert integer to FriendshipStatus.
fn friendship_status_from_i32(value: i32) -> FriendshipStatus {
    match value {
        1 => FriendshipStatus::Accepted,
        2 => FriendshipStatus::Blocked,
        _ => FriendshipStatus::Pending,
    }
}

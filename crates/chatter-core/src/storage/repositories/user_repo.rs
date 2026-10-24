//! User repository.
//!
//! Data access for user, profile, device, and friendship entities.

use crate::domain::ids::{DeviceId, UserId};
use crate::domain::user::{
    Device, Friendship, FriendshipStatus, PresenceStatus, Profile, StatusMessage, User,
};
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;
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

    /// Remember a complete handle learned from a successful exact lookup or
    /// a decrypted username link. Other user fields stay untouched.
    pub fn set_known_username(&self, user_id: &UserId, username: &str) -> Result<(), StorageError> {
        self.ensure_user_exists(user_id)?;
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE users SET username = ?1 WHERE id = ?2",
            params![username, user_id.to_string()],
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
                id: row
                    .get::<_, String>(0)?
                    .parse()
                    .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
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

    /// Read this account's local override for a peer.
    pub fn contact_name(&self, owner: &UserId, peer: &UserId) -> Result<Option<String>, StorageError> {
        let conn = self.db.read_connection()?;
        conn.query_row(
            "SELECT name FROM contact_names WHERE owner_id = ?1 AND peer_id = ?2",
            params![owner.to_string(), peer.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(StorageError::Database)
    }

    /// An empty name removes the local override. No profile or server row is changed.
    pub fn set_contact_name(&self, owner: &UserId, peer: &UserId, name: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        if name.is_empty() {
            conn.execute(
                "DELETE FROM contact_names WHERE owner_id = ?1 AND peer_id = ?2",
                params![owner.to_string(), peer.to_string()],
            )?;
        } else {
            conn.execute(
                "INSERT INTO contact_names (owner_id, peer_id, name) VALUES (?1, ?2, ?3)
                 ON CONFLICT(owner_id, peer_id) DO UPDATE SET name = excluded.name",
                params![owner.to_string(), peer.to_string(), name],
            )?;
        }
        Ok(())
    }

    /// Batched local override, profile name, and username sources for display.
    /// The owner key keeps overrides isolated between accounts on this device.
    pub fn display_name_sources(
        &self,
        owner: Option<&UserId>,
        ids: &[UserId],
    ) -> Result<std::collections::HashMap<UserId, (Option<String>, Option<String>, Option<String>)>, StorageError>
    {
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        let conn = self.db.read_connection()?;
        for chunk in ids.chunks(500) {
            let values = vec!["(?)"; chunk.len()].join(",");
            let mut stmt = conn.prepare(&format!(
                "WITH ids(id) AS (VALUES {values}) \
                 SELECT ids.id, n.name, p.display_name, u.username FROM ids \
                 LEFT JOIN contact_names n ON n.owner_id = ? AND n.peer_id = ids.id \
                 LEFT JOIN profiles p ON p.user_id = ids.id \
                 LEFT JOIN users u ON u.id = ids.id"
            ))?;
            let mut keys: Vec<String> = chunk.iter().map(|id| id.to_string()).collect();
            keys.push(owner.map(ToString::to_string).unwrap_or_default());
            let rows = stmt.query_map(rusqlite::params_from_iter(keys.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            for row in rows {
                let (id, contact_name, display_name, username) = row?;
                if let Ok(id) = id.parse::<UserId>() {
                    out.insert(id, (contact_name, display_name, username));
                }
            }
        }
        Ok(out)
    }

    /// Handles and decrypted profile photos for a roster page, in one read per
    /// chunk. Missing directory/profile rows remain missing in the result.
    pub fn roster_profile_sources(
        &self,
        ids: &[UserId],
    ) -> Result<std::collections::HashMap<UserId, (Option<String>, Option<Vec<u8>>)>, StorageError> {
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        let conn = self.db.read_connection()?;
        for chunk in ids.chunks(500) {
            let values = vec!["(?)"; chunk.len()].join(",");
            let mut stmt = conn.prepare(&format!(
                "WITH ids(id) AS (VALUES {values}) \
                 SELECT ids.id, u.username, p.avatar_bytes FROM ids \
                 LEFT JOIN users u ON u.id = ids.id \
                 LEFT JOIN profiles p ON p.user_id = ids.id"
            ))?;
            let keys: Vec<String> = chunk.iter().map(ToString::to_string).collect();
            let rows = stmt.query_map(rusqlite::params_from_iter(keys.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                ))
            })?;
            for row in rows {
                let (id, username, avatar) = row?;
                if let Ok(id) = id.parse::<UserId>() {
                    out.insert(id, (username, avatar));
                }
            }
        }
        Ok(out)
    }

    /// Count total number of users in the database.
    pub fn count(&self) -> Result<u64, StorageError> {
        let conn = self.db.connection()?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        Ok(count as u64)
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
            "SELECT user_id, display_name, avatar_bytes, status_text, status_emoji, status_expires_at, presence
             FROM profiles WHERE user_id = ?1",
        )?;

        let result = stmt.query_row(params![user_id.to_string()], |row| {
            let status_text: Option<String> = row.get(3)?;
            let status_emoji: Option<String> = row.get(4)?;
            let status_expires_at: Option<String> = row.get(5)?;

            let status_message = status_text.map(|text| StatusMessage {
                text,
                emoji: status_emoji,
                expires_at: status_expires_at.map(|s| parse_datetime(&s)),
            });

            Ok(Profile {
                user_id: row
                    .get::<_, String>(0)?
                    .parse()
                    .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
                display_name: row.get(1)?,
                avatar_bytes: row.get(2)?,
                status_message,
                presence: presence_from_i32(row.get(6)?),
            })
        });

        match result {
            Ok(profile) => Ok(Some(profile)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Insert or update a user's profile. `true` when the write changed what
    /// any screen shows for this user — name, status text, or avatar (an
    /// avatar of `None` keeps the stored one, so it changes nothing) — or the
    /// user had no profile yet. Every writer gets the answer, so a writer that
    /// changes a friend's name can say so (`UserProfileUpdated`): a silent
    /// write left rows painted earlier showing the old name.
    pub fn upsert_profile(&self, profile: &Profile) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let before: Option<(String, Option<String>, Option<Vec<u8>>)> = match conn.query_row(
            "SELECT display_name, status_text, avatar_bytes FROM profiles WHERE user_id = ?1",
            params![profile.user_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ) {
            Ok(row) => Some(row),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(StorageError::Database(e)),
        };
        let changed = before.map_or(true, |(name, status, avatar)| {
            name != profile.display_name
                || status.as_deref() != profile.status_message.as_ref().map(|s| s.text.as_str())
                || (profile.avatar_bytes.is_some() && avatar != profile.avatar_bytes)
        });
        let (status_text, status_emoji, status_expires_at) = match &profile.status_message {
            Some(msg) => (
                Some(msg.text.clone()),
                msg.emoji.clone(),
                msg.expires_at.map(|dt| dt.to_rfc3339()),
            ),
            None => (None, None, None),
        };

        conn.execute(
            "INSERT INTO profiles (user_id, display_name, avatar_bytes, status_text, status_emoji, status_expires_at, presence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(user_id) DO UPDATE SET
                display_name = excluded.display_name,
                -- Preserve an existing decrypted avatar when this upsert carries
                -- none (e.g. sync/stream paths that don't decrypt the avatar).
                avatar_bytes = COALESCE(excluded.avatar_bytes, profiles.avatar_bytes),
                status_text = excluded.status_text,
                status_emoji = excluded.status_emoji,
                status_expires_at = excluded.status_expires_at,
                presence = excluded.presence",
            params![
                profile.user_id.to_string(),
                profile.display_name,
                profile.avatar_bytes,
                status_text,
                status_emoji,
                status_expires_at,
                presence_to_i32(profile.presence),
            ],
        )?;
        Ok(changed)
    }

    /// Update only a user's presence.
    ///
    /// Presence arrives on its own event (StreamUserEvents) carrying none of
    /// the rest of the profile, so it must NOT go through `upsert_profile` —
    /// that would write an empty display name over the decrypted one. A user
    /// with no profile row is a user we render nowhere, so there is nothing
    /// to store.
    ///
    /// `last_seen_at` is the unix-seconds stamp the presence update carried
    /// (`PresenceUpdate.timestamp`). It is what the profile screen's "last
    /// seen" line reads, and the only reason we know when an offline contact
    /// was last around. `None` leaves the stored value alone.
    pub fn set_presence(
        &self,
        user_id: &UserId,
        presence: PresenceStatus,
        last_seen_at: Option<i64>,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE profiles
             SET presence = ?2, last_seen_at = COALESCE(?3, last_seen_at)
             WHERE user_id = ?1",
            params![user_id.to_string(), presence_to_i32(presence), last_seen_at],
        )?;
        Ok(())
    }

    /// When we last saw a presence update for this user, in unix seconds.
    /// `None` when we have never seen one (or have no profile row).
    pub fn last_seen_at(&self, user_id: &UserId) -> Result<Option<i64>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare("SELECT last_seen_at FROM profiles WHERE user_id = ?1")?;
        match stmt.query_row(params![user_id.to_string()], |row| row.get(0)) {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    // ========================================================================
    // Device operations
    // ========================================================================

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
                user_id: row
                    .get::<_, String>(1)?
                    .parse()
                    .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
                name: row.get(2)?,
                registration_id: row.get(3)?,
                created_at: parse_datetime(&row.get::<_, String>(4)?),
                last_seen: parse_datetime(&row.get::<_, String>(5)?),
            });
        }

        Ok(devices)
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

        let result =
            stmt.query_row(params![user_id.to_string(), friend_id.to_string()], |row| {
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
            });

        match result {
            Ok(friendship) => Ok(Some(friendship)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Has `user_id` blocked `peer_id`?
    ///
    /// The durable answer, read from `friendships`. This is the ONLY source
    /// worth asking: `state::user_state::UserState::blocked_users` is an
    /// in-memory `HashSet` that nothing populates in production, so consulting
    /// it answers "no" for every server-side block — a set that fails OPEN.
    ///
    /// Direction matters and is deliberately not symmetric. `list_friends`
    /// matches either column because a friendship is mutual; a block is not.
    /// The row `block_user` writes is `(user_id = us, friend_id = them)`, so
    /// only that orientation means "we blocked them". The mirror row means
    /// *they* blocked *us*, which is their enforcement to do, not ours —
    /// treating it as ours would silently discard mail from someone the user
    /// never blocked.
    pub fn is_blocked(&self, user_id: &UserId, peer_id: &UserId) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let status: Option<i32> = conn
            .query_row(
                "SELECT status FROM friendships WHERE user_id = ?1 AND friend_id = ?2",
                params![user_id.to_string(), peer_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(status.map(friendship_status_from_i32) == Some(FriendshipStatus::Blocked))
    }

    /// Everyone `user_id` has blocked, in the same orientation [`Self::is_blocked`]
    /// reads (`user_id = us, friend_id = them`).
    pub fn list_blocked_by(&self, user_id: &UserId) -> Result<Vec<UserId>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT friend_id FROM friendships WHERE user_id = ?1 AND status = ?2 ORDER BY created_at",
        )?;
        let ids = stmt
            .query_map(
                params![
                    user_id.to_string(),
                    friendship_status_to_i32(FriendshipStatus::Blocked)
                ],
                |row| row.get::<_, String>(0),
            )?
            .filter_map(|r| r.ok()?.parse().ok())
            .collect();
        Ok(ids)
    }

    /// Record `user_id → friend_id` as accepted, leaving a block alone. The
    /// server keeps a blocked friend in `ListFriends` (a block keeps the
    /// friendship, Signal-style), so a friends refresh that overwrote the row
    /// lifted every block it saw. Only the status changes, so the row's
    /// profile key survives.
    pub fn accept_friendship_unless_blocked(
        &self,
        user_id: &UserId,
        friend_id: &UserId,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO friendships (user_id, friend_id, status, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(user_id, friend_id) DO UPDATE SET status = excluded.status
             WHERE friendships.status != ?5",
            params![
                user_id.to_string(),
                friend_id.to_string(),
                friendship_status_to_i32(FriendshipStatus::Accepted),
                Utc::now().to_rfc3339(),
                friendship_status_to_i32(FriendshipStatus::Blocked),
            ],
        )?;
        Ok(())
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
        // Match either direction: a friendship row may be stored as
        // (user_id=us, friend_id=peer) or (user_id=peer, friend_id=us)
        // depending on who initiated. The channel-free DM sidebar resolves the
        // peer as whichever party isn't us, so returning either row is correct.
        let mut stmt = conn.prepare(
            "SELECT user_id, friend_id, status, created_at
             FROM friendships WHERE user_id = ?1 OR friend_id = ?1 ORDER BY created_at",
        )?;

        let mut rows = stmt.query(params![user_id.to_string()])?;
        let mut friendships = Vec::new();

        while let Some(row) = rows.next()? {
            friendships.push(Friendship {
                requester_id: row
                    .get::<_, String>(0)?
                    .parse()
                    .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
                addressee_id: row
                    .get::<_, String>(1)?
                    .parse()
                    .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid UUID".into()))?,
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

    /// Get a peer's profile key for decrypting their encrypted profile
    /// (display name, status, avatar).
    ///
    /// Held per peer, not per friendship (Signal keeps it on the recipient):
    /// a channel co-member who is not a friend shares theirs too. Returns None
    /// if we don't have their profile key yet.
    pub fn get_friend_profile_key(
        &self,
        user_id: &UserId,
        friend_id: &UserId,
    ) -> Result<Option<[u8; 32]>, StorageError> {
        let conn = self.db.connection()?;
        let result: Result<Vec<u8>, _> = conn.query_row(
            "SELECT profile_key FROM peer_profile_keys WHERE user_id = ?1 AND peer_id = ?2",
            params![user_id.to_string(), friend_id.to_string()],
            |row| row.get(0),
        );

        match result {
            Ok(key_bytes) => Ok(<[u8; 32]>::try_from(key_bytes.as_slice()).ok()),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Store a peer's profile key received from an E2E message.
    ///
    /// Callers pass only a key the peer delivered themselves (sealed-sender
    /// authenticated as that peer) — the same rule as Signal's
    /// `handleDataMessage`, which stores a message's profile key against its
    /// sender and nobody else — so a different key is the peer's rotated one
    /// and replaces ours. Returns whether the stored key CHANGED (none
    /// before, or a different one) — the caller's cue to re-fetch the peer's
    /// profile, which it could not decrypt (or decrypted with a stale key)
    /// until now.
    pub fn set_friend_profile_key(
        &self,
        user_id: &UserId,
        friend_id: &UserId,
        key: &[u8; 32],
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let changed = conn.execute(
            "INSERT INTO peer_profile_keys (user_id, peer_id, profile_key) VALUES (?1, ?2, ?3) \
             ON CONFLICT (user_id, peer_id) DO UPDATE SET profile_key = excluded.profile_key \
             WHERE profile_key IS NOT excluded.profile_key",
            params![user_id.to_string(), friend_id.to_string(), key.as_slice()],
        )?;
        Ok(changed > 0)
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

        let result: Result<i32, _> = stmt
            .query_row(params![user_id.to_string(), friend_id.to_string()], |row| {
                row.get(0)
            });

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;

    fn setup() -> (tempfile::TempDir, UserRepository) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("users.db"), "testpass").unwrap());
        db.run_migrations().unwrap();
        (dir, UserRepository::new(db))
    }

    fn friendship(from: UserId, to: UserId, status: FriendshipStatus) -> Friendship {
        Friendship {
            requester_id: from,
            addressee_id: to,
            status,
            created_at: Utc::now(),
        }
    }

    /// A friends refresh must neither lift a block nor drop the profile key.
    #[test]
    fn accept_friendship_keeps_block_and_profile_key() {
        let (_d, repo) = setup();
        let me = UserId::new();
        let them = UserId::new();
        repo.ensure_user_exists(&me).unwrap();
        repo.ensure_user_exists(&them).unwrap();
        repo.accept_friendship_unless_blocked(&me, &them).unwrap();
        assert!(repo.set_friend_profile_key(&me, &them, &[7; 32]).unwrap());

        repo.update_friendship(&friendship(me, them, FriendshipStatus::Blocked))
            .unwrap();
        repo.accept_friendship_unless_blocked(&me, &them).unwrap();
        assert!(
            repo.is_blocked(&me, &them).unwrap(),
            "refresh lifted the block"
        );

        // Unblock flips the status back; the key is still there.
        repo.update_friendship(&friendship(me, them, FriendshipStatus::Accepted))
            .unwrap();
        assert!(!repo.is_blocked(&me, &them).unwrap());
        assert_eq!(
            repo.get_friend_profile_key(&me, &them).unwrap(),
            Some([7; 32])
        );
    }

    /// `set_friend_profile_key` reports a change only when the stored key
    /// actually changed — the trigger for re-fetching the friend's profile.
    #[test]
    fn set_friend_profile_key_reports_only_real_changes() {
        let (_d, repo) = setup();
        let me = UserId::new();
        let them = UserId::new();
        repo.ensure_user_exists(&me).unwrap();
        repo.ensure_user_exists(&them).unwrap();
        repo.insert_friendship(&friendship(me, them, FriendshipStatus::Accepted))
            .unwrap();

        assert!(
            repo.set_friend_profile_key(&me, &them, &[1; 32]).unwrap(),
            "first key"
        );
        assert!(
            !repo.set_friend_profile_key(&me, &them, &[1; 32]).unwrap(),
            "same key"
        );
        assert!(
            repo.set_friend_profile_key(&me, &them, &[2; 32]).unwrap(),
            "rotated key"
        );
        assert_eq!(
            repo.get_friend_profile_key(&me, &them).unwrap(),
            Some([2; 32])
        );
        // A channel co-member with no friendship row still gets theirs kept.
        let member = UserId::new();
        assert!(repo.set_friend_profile_key(&me, &member, &[3; 32]).unwrap());
        assert_eq!(
            repo.get_friend_profile_key(&me, &member).unwrap(),
            Some([3; 32])
        );
    }

    /// The durable read that client-side block enforcement stands on.
    ///
    /// `UserService::block_user` writes exactly this row. If the read cannot
    /// see it, every incoming message from a blocked sender is delivered —
    /// which is the state the in-memory `blocked_users` set leaves the client
    /// in, and why `is_profile_user_blocked` reports false for server blocks.
    /// The Blocked tab lists server blocks from this read: only our own
    /// blocks, never theirs on us, never accepted friends.
    #[test]
    fn list_blocked_by_returns_only_our_blocks() {
        let (_d, repo) = setup();
        let (me, blocked, friend, blocker) =
            (UserId::new(), UserId::new(), UserId::new(), UserId::new());
        for u in [me, blocked, friend, blocker] {
            repo.ensure_user_exists(&u).unwrap();
        }
        repo.insert_friendship(&friendship(me, blocked, FriendshipStatus::Blocked))
            .unwrap();
        repo.insert_friendship(&friendship(me, friend, FriendshipStatus::Accepted))
            .unwrap();
        repo.insert_friendship(&friendship(blocker, me, FriendshipStatus::Blocked))
            .unwrap();

        assert_eq!(repo.list_blocked_by(&me).unwrap(), vec![blocked]);
    }

    #[test]
    fn a_block_written_to_friendships_is_visible_to_is_blocked() {
        let (_d, repo) = setup();
        let me = UserId::new();
        let them = UserId::new();
        repo.ensure_user_exists(&me).unwrap();
        repo.ensure_user_exists(&them).unwrap();

        repo.insert_friendship(&friendship(me, them, FriendshipStatus::Blocked))
            .unwrap();

        assert!(
            repo.is_blocked(&me, &them).unwrap(),
            "block_user persisted FriendshipStatus::Blocked and the read says no — \
             enforcement built on this answer delivers every blocked sender's mail"
        );
    }

    /// Fail-open in the other direction is just as bad: dropping mail from
    /// someone who was never blocked is unrecoverable data loss.
    #[test]
    fn a_stranger_and_a_friend_are_not_blocked() {
        let (_d, repo) = setup();
        let me = UserId::new();
        let stranger = UserId::new();
        let friend = UserId::new();
        let pending = UserId::new();
        for u in [&me, &stranger, &friend, &pending] {
            repo.ensure_user_exists(u).unwrap();
        }

        repo.insert_friendship(&friendship(me, friend, FriendshipStatus::Accepted))
            .unwrap();
        repo.insert_friendship(&friendship(me, pending, FriendshipStatus::Pending))
            .unwrap();

        assert!(
            !repo.is_blocked(&me, &stranger).unwrap(),
            "no row is not a block"
        );
        assert!(
            !repo.is_blocked(&me, &friend).unwrap(),
            "Accepted is not Blocked"
        );
        assert!(
            !repo.is_blocked(&me, &pending).unwrap(),
            "Pending is not Blocked"
        );
    }

    /// A block is one-directional. `list_friends` matches either column
    /// because friendship is mutual; reusing that shape here would make "they
    /// blocked me" silently delete their messages from my inbox.
    #[test]
    fn their_block_on_me_does_not_make_them_blocked_by_me() {
        let (_d, repo) = setup();
        let me = UserId::new();
        let them = UserId::new();
        repo.ensure_user_exists(&me).unwrap();
        repo.ensure_user_exists(&them).unwrap();

        // Row stored in the mirror orientation: THEY blocked ME.
        repo.insert_friendship(&friendship(them, me, FriendshipStatus::Blocked))
            .unwrap();

        assert!(
            !repo.is_blocked(&me, &them).unwrap(),
            "a block recorded in the peer's direction is their enforcement, not \
             ours — reading it as ours drops mail the user never blocked"
        );
    }

    /// `block_user` UPDATEs an existing row when one is there, so the common
    /// path is Accepted -> Blocked rather than a fresh insert.
    #[test]
    fn upgrading_an_accepted_friendship_to_blocked_is_visible() {
        let (_d, repo) = setup();
        let me = UserId::new();
        let them = UserId::new();
        repo.ensure_user_exists(&me).unwrap();
        repo.ensure_user_exists(&them).unwrap();

        repo.insert_friendship(&friendship(me, them, FriendshipStatus::Accepted))
            .unwrap();
        assert!(!repo.is_blocked(&me, &them).unwrap());

        repo.update_friendship(&friendship(me, them, FriendshipStatus::Blocked))
            .unwrap();
        assert!(
            repo.is_blocked(&me, &them).unwrap(),
            "blocking an existing friend takes the UPDATE branch in block_user"
        );
    }

    /// Presence has to SURVIVE, and it has to survive alone.
    ///
    /// Every list that shows a contact's dot (the DM sidebar, the Contacts
    /// tab, the chat header on reopen) rebuilds from `profiles.presence`, so a
    /// presence event that only patched the on-screen rows went grey again at
    /// the next rebuild. The event carries presence and nothing else, so the
    /// write must not travel through `upsert_profile` — that would put an
    /// empty display name and a null avatar over the decrypted ones.
    #[test]
    fn set_presence_persists_without_erasing_the_rest_of_the_profile() {
        let (_d, repo) = setup();
        let them = UserId::new();
        repo.ensure_user_exists(&them).unwrap();
        repo.upsert_profile(&Profile {
            user_id: them,
            display_name: "Alice".to_string(),
            avatar_bytes: Some(vec![1, 2, 3]),
            status_message: None,
            presence: PresenceStatus::Offline,
        })
        .unwrap();

        repo.set_presence(&them, PresenceStatus::Online, None)
            .unwrap();

        let stored = repo.get_profile(&them).unwrap().expect("profile row");
        assert_eq!(
            stored.presence,
            PresenceStatus::Online,
            "presence did not persist — every rebuilt list shows this contact offline \
             again the moment anything refreshes"
        );
        assert_eq!(
            stored.display_name, "Alice",
            "a presence write erased the decrypted display name"
        );
        assert_eq!(
            stored.avatar_bytes,
            Some(vec![1, 2, 3]),
            "a presence write erased the decrypted avatar"
        );
    }

    /// No profile row means a user we render nowhere, so a presence event for
    /// them is a no-op — not an error, and not a half-built row whose empty
    /// display name would later read as their name.
    #[test]
    fn set_presence_for_an_unknown_user_stores_nothing() {
        let (_d, repo) = setup();
        let stranger = UserId::new();

        repo.set_presence(&stranger, PresenceStatus::Online, None)
            .unwrap();

        assert!(
            repo.get_profile(&stranger).unwrap().is_none(),
            "presence for an unknown user invented a profile row"
        );
    }
}

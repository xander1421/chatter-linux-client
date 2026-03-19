//! Sesame persistence layer.
//!
//! Storage trait and SQLite implementation for Sesame session data.

use std::collections::VecDeque;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::domain::UserId;
use crate::error::StorageError;
use crate::storage::Database;

use super::types::{DeviceRecord, PendingMessage, SessionEntry, UserRecord};

/// Sesame store trait for persistence.
///
/// This trait abstracts the storage layer for Sesame session data.
pub trait SesameStore {
    /// Load a user record.
    fn load_user(&self, user_id: &UserId) -> Result<Option<UserRecord>, StorageError>;

    /// Save a user record.
    fn save_user(&self, record: &UserRecord) -> Result<(), StorageError>;

    /// Delete a user record.
    fn delete_user(&self, user_id: &UserId) -> Result<(), StorageError>;

    /// Load all user records.
    fn load_all_users(&self) -> Result<Vec<UserRecord>, StorageError>;

    /// Save a pending message for a user.
    fn queue_pending_message(
        &self,
        user_id: &UserId,
        content: &[u8],
        timestamp: DateTime<Utc>,
        attachment_ids: &[crate::domain::ids::AttachmentId],
        channel_id: Option<&crate::domain::ids::ChannelId>,
    ) -> Result<(), StorageError>;

    /// Load and remove all pending messages for a user.
    fn drain_pending_messages(&self, user_id: &UserId) -> Result<Vec<PendingMessage>, StorageError>;
}

/// SQLite-backed Sesame store.
pub struct SqliteSesameStore {
    db: Arc<Database>,
}

impl SqliteSesameStore {
    /// Create a new SqliteSesameStore.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Initialize the database tables.
    pub fn init_tables(&self) -> Result<(), StorageError> {
        let conn = self.db.connection()?;

        // User records table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sesame_users (
                user_id TEXT PRIMARY KEY,
                stale INTEGER NOT NULL DEFAULT 0,
                last_directory_check TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )",
            [],
        )?;

        // Device records table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sesame_devices (
                user_id TEXT NOT NULL,
                device_id INTEGER NOT NULL,
                stale INTEGER NOT NULL DEFAULT 0,
                stale_timestamp TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (user_id, device_id),
                FOREIGN KEY (user_id) REFERENCES sesame_users(user_id) ON DELETE CASCADE
            )",
            [],
        )?;

        // Sessions table (both active and inactive)
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sesame_sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id TEXT NOT NULL,
                device_id INTEGER NOT NULL,
                session_id BLOB NOT NULL,
                session_record BLOB NOT NULL,
                is_active INTEGER NOT NULL DEFAULT 0,
                position INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                last_send_at TEXT,
                last_recv_at TEXT,
                send_count INTEGER NOT NULL DEFAULT 0,
                recv_count INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY (user_id, device_id) REFERENCES sesame_devices(user_id, device_id) ON DELETE CASCADE
            )",
            [],
        )?;

        // Index for fast session lookups
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_sesame_sessions_user_device
             ON sesame_sessions(user_id, device_id, is_active)",
            [],
        )?;

        // Pending messages table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sesame_pending_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id TEXT NOT NULL,
                content BLOB NOT NULL,
                timestamp TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                attachment_ids_blob BLOB,
                channel_id TEXT,
                FOREIGN KEY (user_id) REFERENCES sesame_users(user_id) ON DELETE CASCADE
            )",
            [],
        )?;

        // Index for pending messages
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_sesame_pending_user
             ON sesame_pending_messages(user_id)",
            [],
        )?;

        Ok(())
    }
}

impl SesameStore for SqliteSesameStore {
    fn load_user(&self, user_id: &UserId) -> Result<Option<UserRecord>, StorageError> {
        use rusqlite::params;

        let conn = self.db.connection()?;
        let user_id_str = user_id.to_string();

        // Load user record
        let mut stmt = conn.prepare(
            "SELECT stale, last_directory_check FROM sesame_users WHERE user_id = ?1",
        )?;

        let user_row: Result<(bool, Option<String>), _> = stmt.query_row(
            params![user_id_str],
            |row| {
                let stale: i32 = row.get(0)?;
                Ok((stale != 0, row.get(1)?))
            },
        );

        let (stale, last_directory_check) = match user_row {
            Ok(data) => data,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(StorageError::Database(e)),
        };

        let mut record = UserRecord::new(*user_id);
        record.stale = stale;
        record.last_directory_check = last_directory_check
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        // Load devices
        let mut stmt = conn.prepare(
            "SELECT device_id, stale, stale_timestamp FROM sesame_devices WHERE user_id = ?1",
        )?;

        let devices: Vec<(u32, bool, Option<String>)> = stmt
            .query_map(params![user_id_str], |row| {
                let stale: i32 = row.get(1)?;
                Ok((row.get(0)?, stale != 0, row.get(2)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        for (device_id, stale, stale_timestamp) in devices {
            let mut device = DeviceRecord::new(device_id);
            device.stale = stale;
            device.stale_timestamp = stale_timestamp
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            // Load sessions for this device
            let mut stmt = conn.prepare(
                "SELECT session_id, session_record, is_active, position, created_at,
                        last_send_at, last_recv_at, send_count, recv_count
                 FROM sesame_sessions
                 WHERE user_id = ?1 AND device_id = ?2
                 ORDER BY is_active DESC, position ASC",
            )?;

            let sessions: Vec<SessionEntry> = stmt
                .query_map(params![user_id_str, device_id], |row| {
                    let created_at_str: String = row.get(4)?;
                    let last_send_at: Option<String> = row.get(5)?;
                    let last_recv_at: Option<String> = row.get(6)?;

                    Ok(SessionEntry {
                        session_id: row.get(0)?,
                        session_record: row.get(1)?,
                        created_at: DateTime::parse_from_rfc3339(&created_at_str)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        last_send_at: last_send_at.and_then(|s| {
                            DateTime::parse_from_rfc3339(&s)
                                .ok()
                                .map(|dt| dt.with_timezone(&Utc))
                        }),
                        last_recv_at: last_recv_at.and_then(|s| {
                            DateTime::parse_from_rfc3339(&s)
                                .ok()
                                .map(|dt| dt.with_timezone(&Utc))
                        }),
                        send_count: row.get(7)?,
                        recv_count: row.get(8)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();

            for session in sessions {
                let is_active: i32 = conn.query_row(
                    "SELECT is_active FROM sesame_sessions
                     WHERE user_id = ?1 AND device_id = ?2 AND session_id = ?3",
                    params![user_id_str, device_id, &session.session_id],
                    |row| row.get(0),
                ).unwrap_or(0);

                if is_active != 0 {
                    device.active_session = Some(session);
                } else {
                    device.inactive_sessions.push_back(session);
                }
            }

            record.devices.insert(device_id, device);
        }

        // Load pending messages
        let mut stmt = conn.prepare(
            "SELECT content, timestamp, attempts, attachment_ids_blob, channel_id FROM sesame_pending_messages
             WHERE user_id = ?1 ORDER BY id ASC",
        )?;

        let messages: Vec<PendingMessage> = stmt
            .query_map(params![user_id_str], |row| {
                let timestamp_str: String = row.get(1)?;
                let attachment_ids_blob: Option<Vec<u8>> = row.get(3)?;
                let channel_id_str: Option<String> = row.get(4)?;

                // Deserialize attachment IDs from bincode
                let attachment_ids: Vec<crate::domain::ids::AttachmentId> = attachment_ids_blob
                    .and_then(|blob| bincode::deserialize(&blob).ok())
                    .unwrap_or_default();

                // Parse channel ID
                let channel_id = channel_id_str.and_then(|s| s.parse().ok());

                Ok(PendingMessage {
                    content: row.get(0)?,
                    timestamp: DateTime::parse_from_rfc3339(&timestamp_str)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                    attempts: row.get(2)?,
                    attachment_ids,
                    channel_id,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        record.pending_messages = VecDeque::from(messages);

        Ok(Some(record))
    }

    fn save_user(&self, record: &UserRecord) -> Result<(), StorageError> {
        use rusqlite::params;

        let conn = self.db.connection()?;
        let user_id_str = record.user_id.to_string();
        let now = Utc::now().to_rfc3339();

        // Upsert user record
        conn.execute(
            "INSERT INTO sesame_users (user_id, stale, last_directory_check, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(user_id) DO UPDATE SET
                 stale = excluded.stale,
                 last_directory_check = excluded.last_directory_check,
                 updated_at = excluded.updated_at",
            params![
                user_id_str,
                record.stale as i32,
                record.last_directory_check.map(|dt| dt.to_rfc3339()),
                now
            ],
        )?;

        // Save devices and sessions
        for (device_id, device) in &record.devices {
            // Upsert device record
            conn.execute(
                "INSERT INTO sesame_devices (user_id, device_id, stale, stale_timestamp, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(user_id, device_id) DO UPDATE SET
                     stale = excluded.stale,
                     stale_timestamp = excluded.stale_timestamp,
                     updated_at = excluded.updated_at",
                params![
                    user_id_str,
                    device_id,
                    device.stale as i32,
                    device.stale_timestamp.map(|dt| dt.to_rfc3339()),
                    now
                ],
            )?;

            // Delete existing sessions for this device
            conn.execute(
                "DELETE FROM sesame_sessions WHERE user_id = ?1 AND device_id = ?2",
                params![user_id_str, device_id],
            )?;

            // Insert active session
            if let Some(ref session) = device.active_session {
                conn.execute(
                    "INSERT INTO sesame_sessions
                     (user_id, device_id, session_id, session_record, is_active, position,
                      created_at, last_send_at, last_recv_at, send_count, recv_count)
                     VALUES (?1, ?2, ?3, ?4, 1, 0, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        user_id_str,
                        device_id,
                        &session.session_id,
                        &session.session_record,
                        session.created_at.to_rfc3339(),
                        session.last_send_at.map(|dt| dt.to_rfc3339()),
                        session.last_recv_at.map(|dt| dt.to_rfc3339()),
                        session.send_count,
                        session.recv_count
                    ],
                )?;
            }

            // Insert inactive sessions
            for (position, session) in device.inactive_sessions.iter().enumerate() {
                conn.execute(
                    "INSERT INTO sesame_sessions
                     (user_id, device_id, session_id, session_record, is_active, position,
                      created_at, last_send_at, last_recv_at, send_count, recv_count)
                     VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        user_id_str,
                        device_id,
                        &session.session_id,
                        &session.session_record,
                        position as i32,
                        session.created_at.to_rfc3339(),
                        session.last_send_at.map(|dt| dt.to_rfc3339()),
                        session.last_recv_at.map(|dt| dt.to_rfc3339()),
                        session.send_count,
                        session.recv_count
                    ],
                )?;
            }
        }

        Ok(())
    }

    fn delete_user(&self, user_id: &UserId) -> Result<(), StorageError> {
        use rusqlite::params;

        let conn = self.db.connection()?;
        let user_id_str = user_id.to_string();

        // CASCADE will handle devices, sessions, and pending messages
        conn.execute(
            "DELETE FROM sesame_users WHERE user_id = ?1",
            params![user_id_str],
        )?;

        Ok(())
    }

    fn load_all_users(&self) -> Result<Vec<UserRecord>, StorageError> {
        let conn = self.db.connection()?;

        let mut stmt = conn.prepare("SELECT user_id FROM sesame_users")?;
        let user_ids: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let mut records = Vec::new();
        for user_id_str in user_ids {
            if let Ok(user_id) = user_id_str.parse::<UserId>() {
                if let Ok(Some(record)) = self.load_user(&user_id) {
                    records.push(record);
                }
            }
        }

        Ok(records)
    }

    fn queue_pending_message(
        &self,
        user_id: &UserId,
        content: &[u8],
        timestamp: DateTime<Utc>,
        attachment_ids: &[crate::domain::ids::AttachmentId],
        channel_id: Option<&crate::domain::ids::ChannelId>,
    ) -> Result<(), StorageError> {
        use rusqlite::params;

        let conn = self.db.connection()?;
        let user_id_str = user_id.to_string();

        // Ensure user exists
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO sesame_users (user_id, stale, created_at, updated_at)
             VALUES (?1, 0, ?2, ?2)
             ON CONFLICT(user_id) DO NOTHING",
            params![user_id_str, now],
        )?;

        // Serialize attachment IDs using bincode
        let attachment_ids_blob = bincode::serialize(&attachment_ids)
            .map_err(|e| StorageError::InvalidData(format!("bincode: {}", e)))?;
        let channel_id_str = channel_id.map(|c| c.to_string());

        conn.execute(
            "INSERT INTO sesame_pending_messages (user_id, content, timestamp, attempts, attachment_ids_blob, channel_id)
             VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            params![user_id_str, content, timestamp.to_rfc3339(), attachment_ids_blob, channel_id_str],
        )?;

        Ok(())
    }

    fn drain_pending_messages(&self, user_id: &UserId) -> Result<Vec<PendingMessage>, StorageError> {
        use rusqlite::params;

        let conn = self.db.connection()?;
        let user_id_str = user_id.to_string();

        // Load messages with attachment_ids_blob and channel_id
        let mut stmt = conn.prepare(
            "SELECT content, timestamp, attempts, attachment_ids_blob, channel_id FROM sesame_pending_messages
             WHERE user_id = ?1 ORDER BY id ASC",
        )?;

        let messages: Vec<PendingMessage> = stmt
            .query_map(params![user_id_str], |row| {
                let timestamp_str: String = row.get(1)?;
                let attachment_ids_blob: Option<Vec<u8>> = row.get(3)?;
                let channel_id_str: Option<String> = row.get(4)?;

                // Deserialize attachment IDs from bincode
                let attachment_ids: Vec<crate::domain::ids::AttachmentId> = attachment_ids_blob
                    .and_then(|blob| bincode::deserialize(&blob).ok())
                    .unwrap_or_default();

                // Parse channel ID
                let channel_id = channel_id_str.and_then(|s| s.parse().ok());

                Ok(PendingMessage {
                    content: row.get(0)?,
                    timestamp: DateTime::parse_from_rfc3339(&timestamp_str)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                    attempts: row.get(2)?,
                    attachment_ids,
                    channel_id,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Delete messages
        conn.execute(
            "DELETE FROM sesame_pending_messages WHERE user_id = ?1",
            params![user_id_str],
        )?;

        Ok(messages)
    }
}

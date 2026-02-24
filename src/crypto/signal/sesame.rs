//! Sesame session management.
//!
//! Implementation of Signal's Sesame algorithm for managing multiple concurrent
//! sessions with the same user across multiple devices.
//!
//! Sesame provides:
//! - Multiple concurrent sessions per device (active + inactive)
//! - Session convergence: try all sessions, promote successful one to active
//! - Message queue for pending messages during bootstrap
//! - Retry request mechanism for failed decryptions
//!
//! Reference: Signal Protocol Sesame algorithm
//! https://signal.org/docs/

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use libsignal_protocol::SessionRecord;

use crate::domain::UserId;
use crate::error::StorageError;
use crate::storage::Database;

/// Maximum number of inactive sessions to keep per device.
/// Signal uses 5, but we'll be a bit more conservative.
pub const MAX_INACTIVE_SESSIONS: usize = 5;

/// Maximum number of pending messages to queue per peer.
pub const MAX_PENDING_MESSAGES: usize = 100;

/// Maximum age for pending messages before they expire (1 hour).
pub const PENDING_MESSAGE_TTL_SECS: i64 = 3600;

/// Tracks all devices for a user we communicate with.
///
/// This is the top-level structure for a conversation partner.
/// Each user may have multiple devices, each with its own sessions.
#[derive(Debug, Clone)]
pub struct UserRecord {
    /// User ID.
    pub user_id: UserId,
    /// Map of device ID to device record.
    pub devices: HashMap<u32, DeviceRecord>,
    /// Whether the user's device list is stale and needs refresh.
    pub stale: bool,
    /// When we last checked the server for the user's devices.
    pub last_directory_check: Option<DateTime<Utc>>,
    /// Pending messages waiting for any session to be established.
    pub pending_messages: VecDeque<PendingMessage>,
}

impl UserRecord {
    /// Create a new UserRecord for a user.
    pub fn new(user_id: UserId) -> Self {
        Self {
            user_id,
            devices: HashMap::new(),
            stale: false,
            last_directory_check: None,
            pending_messages: VecDeque::new(),
        }
    }

    /// Get the device record for a specific device, or None if not found.
    pub fn get_device(&self, device_id: u32) -> Option<&DeviceRecord> {
        self.devices.get(&device_id)
    }

    /// Get mutable device record for a specific device, or None if not found.
    pub fn get_device_mut(&mut self, device_id: u32) -> Option<&mut DeviceRecord> {
        self.devices.get_mut(&device_id)
    }

    /// Get or create device record for a device.
    pub fn get_or_create_device(&mut self, device_id: u32) -> &mut DeviceRecord {
        self.devices
            .entry(device_id)
            .or_insert_with(|| DeviceRecord::new(device_id))
    }

    /// Mark all devices as stale.
    pub fn mark_all_stale(&mut self) {
        self.stale = true;
        for device in self.devices.values_mut() {
            device.stale = true;
        }
    }

    /// Check if any device has an active session.
    pub fn has_any_active_session(&self) -> bool {
        self.devices.values().any(|d| d.active_session.is_some())
    }

    /// Get all devices that have active sessions.
    pub fn devices_with_sessions(&self) -> Vec<u32> {
        self.devices
            .iter()
            .filter(|(_, d)| d.active_session.is_some())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Add a pending message to the queue.
    pub fn queue_message(
        &mut self,
        content: Vec<u8>,
        timestamp: DateTime<Utc>,
        attachment_ids: Vec<crate::domain::ids::AttachmentId>,
        channel_id: Option<crate::domain::ids::ChannelId>,
    ) {
        // Limit queue size
        while self.pending_messages.len() >= MAX_PENDING_MESSAGES {
            self.pending_messages.pop_front();
        }

        self.pending_messages.push_back(PendingMessage {
            content,
            timestamp,
            attempts: 0,
            attachment_ids,
            channel_id,
        });
    }

    /// Drain all pending messages.
    pub fn drain_pending_messages(&mut self) -> Vec<PendingMessage> {
        let now = Utc::now();
        let ttl = chrono::Duration::seconds(PENDING_MESSAGE_TTL_SECS);

        // Filter out expired messages
        self.pending_messages
            .drain(..)
            .filter(|msg| now.signed_duration_since(msg.timestamp) < ttl)
            .collect()
    }

    /// Check if there are pending messages.
    pub fn has_pending_messages(&self) -> bool {
        !self.pending_messages.is_empty()
    }
}

/// Tracks sessions for a specific device.
///
/// Each device can have one active session and multiple inactive sessions.
/// Inactive sessions are kept to handle out-of-order message delivery and
/// session race conditions.
#[derive(Debug, Clone)]
pub struct DeviceRecord {
    /// Device ID (typically 1 for primary, 2+ for linked devices).
    pub device_id: u32,
    /// The currently active session for this device.
    pub active_session: Option<SessionEntry>,
    /// Previous sessions kept for decryption of old/out-of-order messages.
    /// Ordered from most recent to oldest.
    pub inactive_sessions: VecDeque<SessionEntry>,
    /// Whether this device record is stale and needs refresh.
    pub stale: bool,
    /// When the device was marked stale.
    pub stale_timestamp: Option<DateTime<Utc>>,
}

impl DeviceRecord {
    /// Create a new DeviceRecord.
    pub fn new(device_id: u32) -> Self {
        Self {
            device_id,
            active_session: None,
            inactive_sessions: VecDeque::new(),
            stale: false,
            stale_timestamp: None,
        }
    }

    /// Archive the current active session.
    ///
    /// Moves the active session to inactive and clears active.
    /// Used when identity key changes or session reset is needed.
    pub fn archive_active_session(&mut self) {
        if let Some(session) = self.active_session.take() {
            // Add to front of inactive sessions (most recent first)
            self.inactive_sessions.push_front(session);

            // Limit number of inactive sessions
            while self.inactive_sessions.len() > MAX_INACTIVE_SESSIONS {
                self.inactive_sessions.pop_back();
            }
        }
    }

    /// Set a new active session.
    ///
    /// If there's an existing active session, it's archived first.
    pub fn set_active_session(&mut self, session: SessionEntry) {
        self.archive_active_session();
        self.active_session = Some(session);
    }

    /// Promote an inactive session to active.
    ///
    /// Used when a message successfully decrypts with an inactive session.
    /// The current active session (if any) is archived.
    pub fn promote_session(&mut self, session_id: &[u8]) -> bool {
        // Find the session in inactive list
        let position = self
            .inactive_sessions
            .iter()
            .position(|s| s.session_id == session_id);

        if let Some(pos) = position {
            // Remove from inactive
            if let Some(session) = self.inactive_sessions.remove(pos) {
                // Archive current active and promote this one
                self.archive_active_session();
                self.active_session = Some(session);
                return true;
            }
        }

        false
    }

    /// Get all sessions (active + inactive) for convergence attempts.
    pub fn all_sessions(&self) -> Vec<&SessionEntry> {
        let mut sessions = Vec::new();

        if let Some(ref active) = self.active_session {
            sessions.push(active);
        }

        for inactive in &self.inactive_sessions {
            sessions.push(inactive);
        }

        sessions
    }

    /// Get session by ID from either active or inactive.
    pub fn get_session(&self, session_id: &[u8]) -> Option<&SessionEntry> {
        if let Some(ref active) = self.active_session {
            if active.session_id == session_id {
                return Some(active);
            }
        }

        self.inactive_sessions
            .iter()
            .find(|s| s.session_id == session_id)
    }

    /// Update stats for a session after successful send.
    pub fn record_send(&mut self, session_id: &[u8]) {
        if let Some(ref mut active) = self.active_session {
            if active.session_id == session_id {
                active.last_send_at = Some(Utc::now());
                active.send_count += 1;
                return;
            }
        }

        for session in &mut self.inactive_sessions {
            if session.session_id == session_id {
                session.last_send_at = Some(Utc::now());
                session.send_count += 1;
                return;
            }
        }
    }

    /// Update stats for a session after successful receive.
    pub fn record_receive(&mut self, session_id: &[u8]) {
        if let Some(ref mut active) = self.active_session {
            if active.session_id == session_id {
                active.last_recv_at = Some(Utc::now());
                active.recv_count += 1;
                return;
            }
        }

        for session in &mut self.inactive_sessions {
            if session.session_id == session_id {
                session.last_recv_at = Some(Utc::now());
                session.recv_count += 1;
                return;
            }
        }
    }

    /// Mark device as stale.
    pub fn mark_stale(&mut self) {
        self.stale = true;
        self.stale_timestamp = Some(Utc::now());
    }
}

/// Wrapper around libsignal session with metadata.
///
/// Tracks session usage statistics for debugging and convergence.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    /// Unique identifier for this session (hash of session state).
    pub session_id: Vec<u8>,
    /// The serialized libsignal SessionRecord.
    pub session_record: Vec<u8>,
    /// When this session was created.
    pub created_at: DateTime<Utc>,
    /// When we last sent using this session.
    pub last_send_at: Option<DateTime<Utc>>,
    /// When we last received using this session.
    pub last_recv_at: Option<DateTime<Utc>>,
    /// Number of messages sent with this session.
    pub send_count: u32,
    /// Number of messages received with this session.
    pub recv_count: u32,
}

impl SessionEntry {
    /// Create a new SessionEntry from a libsignal SessionRecord.
    pub fn from_record(record: &SessionRecord) -> Result<Self, libsignal_protocol::SignalProtocolError> {
        let serialized = record.serialize()?;
        let session_id = Self::compute_session_id(&serialized);

        Ok(Self {
            session_id,
            session_record: serialized,
            created_at: Utc::now(),
            last_send_at: None,
            last_recv_at: None,
            send_count: 0,
            recv_count: 0,
        })
    }

    /// Compute a unique ID for the session based on its state.
    ///
    /// Uses SHA256 hash of the serialized session.
    fn compute_session_id(serialized: &[u8]) -> Vec<u8> {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(serialized);
        hasher.finalize().to_vec()
    }

    /// Deserialize the session record.
    pub fn to_record(&self) -> Result<SessionRecord, libsignal_protocol::SignalProtocolError> {
        SessionRecord::deserialize(&self.session_record)
    }

    /// Update the session record.
    pub fn update_record(&mut self, record: &SessionRecord) -> Result<(), libsignal_protocol::SignalProtocolError> {
        let serialized = record.serialize()?;
        self.session_id = Self::compute_session_id(&serialized);
        self.session_record = serialized;
        Ok(())
    }
}

/// A message waiting to be sent once a session is established.
#[derive(Debug, Clone)]
pub struct PendingMessage {
    /// Serialized message content (encrypted at app layer, needs Signal layer).
    pub content: Vec<u8>,
    /// When the message was queued.
    pub timestamp: DateTime<Utc>,
    /// Number of send attempts.
    pub attempts: u32,
    /// Local attachment IDs for storage after send (JSON-serialized).
    pub attachment_ids: Vec<crate::domain::ids::AttachmentId>,
    /// DM channel ID for local storage.
    pub channel_id: Option<crate::domain::ids::ChannelId>,
}

/// Result of session convergence attempt.
#[derive(Debug)]
pub enum ConvergenceResult {
    /// Successfully decrypted with the active session.
    ActiveSession {
        /// The decrypted plaintext.
        plaintext: Vec<u8>,
        /// Session ID that was used.
        session_id: Vec<u8>,
    },
    /// Successfully decrypted with an inactive session.
    /// The session has been promoted to active.
    PromotedSession {
        /// The decrypted plaintext.
        plaintext: Vec<u8>,
        /// Session ID that was promoted.
        session_id: Vec<u8>,
    },
    /// All sessions failed to decrypt.
    AllFailed {
        /// Errors from each session attempt.
        errors: Vec<String>,
    },
}

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
                attachment_ids TEXT,
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
            "SELECT content, timestamp, attempts, attachment_ids, channel_id FROM sesame_pending_messages
             WHERE user_id = ?1 ORDER BY id ASC",
        )?;

        let messages: Vec<PendingMessage> = stmt
            .query_map(params![user_id_str], |row| {
                let timestamp_str: String = row.get(1)?;
                let attachment_ids_json: Option<String> = row.get(3)?;
                let channel_id_str: Option<String> = row.get(4)?;

                // Deserialize attachment IDs from JSON
                let attachment_ids: Vec<crate::domain::ids::AttachmentId> = attachment_ids_json
                    .and_then(|json| serde_json::from_str(&json).ok())
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

        // Serialize attachment IDs and channel ID as JSON
        let attachment_ids_json = serde_json::to_string(&attachment_ids).unwrap_or_default();
        let channel_id_str = channel_id.map(|c| c.to_string());

        conn.execute(
            "INSERT INTO sesame_pending_messages (user_id, content, timestamp, attempts, attachment_ids, channel_id)
             VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            params![user_id_str, content, timestamp.to_rfc3339(), attachment_ids_json, channel_id_str],
        )?;

        Ok(())
    }

    fn drain_pending_messages(&self, user_id: &UserId) -> Result<Vec<PendingMessage>, StorageError> {
        use rusqlite::params;

        let conn = self.db.connection()?;
        let user_id_str = user_id.to_string();

        // Load messages with attachment_ids and channel_id
        let mut stmt = conn.prepare(
            "SELECT content, timestamp, attempts, attachment_ids, channel_id FROM sesame_pending_messages
             WHERE user_id = ?1 ORDER BY id ASC",
        )?;

        let messages: Vec<PendingMessage> = stmt
            .query_map(params![user_id_str], |row| {
                let timestamp_str: String = row.get(1)?;
                let attachment_ids_json: Option<String> = row.get(3)?;
                let channel_id_str: Option<String> = row.get(4)?;

                // Deserialize attachment IDs from JSON
                let attachment_ids: Vec<crate::domain::ids::AttachmentId> = attachment_ids_json
                    .and_then(|json| serde_json::from_str(&json).ok())
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

// =============================================================================
// SesameManager - High-level session management
// =============================================================================

/// Sesame session manager.
///
/// Provides high-level operations for:
/// - Session establishment with message queuing
/// - Message encryption with fan-out to multiple devices
/// - Message decryption with session convergence
/// - Pending message handling
///
/// Note: The actual decryption convergence logic is delegated to the signal_service
/// which has access to the full protocol store. This manager handles the Sesame
/// metadata (session tracking, pending messages, device records).
pub struct SesameManager<S: SesameStore> {
    store: S,
}

impl<S: SesameStore> SesameManager<S> {
    /// Create a new SesameManager with the given store.
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// Get or create a user record.
    pub fn get_or_create_user(&self, user_id: &UserId) -> Result<UserRecord, StorageError> {
        match self.store.load_user(user_id)? {
            Some(record) => Ok(record),
            None => Ok(UserRecord::new(*user_id)),
        }
    }

    /// Save a user record.
    pub fn save_user(&self, record: &UserRecord) -> Result<(), StorageError> {
        self.store.save_user(record)
    }

    /// Queue a message for sending once a session is established.
    ///
    /// Use this when bootstrap is in progress and we can't send yet.
    pub fn queue_pending_message(
        &self,
        user_id: &UserId,
        content: Vec<u8>,
        attachment_ids: Vec<crate::domain::ids::AttachmentId>,
        channel_id: Option<crate::domain::ids::ChannelId>,
    ) -> Result<(), StorageError> {
        self.store.queue_pending_message(
            user_id,
            &content,
            Utc::now(),
            &attachment_ids,
            channel_id.as_ref(),
        )
    }

    /// Get and clear all pending messages for a user.
    ///
    /// Call this after a session is established to retry sending.
    pub fn drain_pending_messages(&self, user_id: &UserId) -> Result<Vec<PendingMessage>, StorageError> {
        self.store.drain_pending_messages(user_id)
    }

    /// Check if a user has an active session on any device.
    pub fn has_session(&self, user_id: &UserId) -> Result<bool, StorageError> {
        let record = self.store.load_user(user_id)?;
        Ok(record.map(|r| r.has_any_active_session()).unwrap_or(false))
    }

    /// Check if a user has pending messages.
    pub fn has_pending_messages(&self, user_id: &UserId) -> Result<bool, StorageError> {
        let record = self.store.load_user(user_id)?;
        Ok(record.map(|r| r.has_pending_messages()).unwrap_or(false))
    }

    /// Store a new session after establishment.
    ///
    /// Archives any existing active session and sets this as the new active.
    pub fn store_session(
        &self,
        user_id: &UserId,
        device_id: u32,
        record: &SessionRecord,
    ) -> Result<(), StorageError> {
        let mut user_record = self.get_or_create_user(user_id)?;
        let device = user_record.get_or_create_device(device_id);

        let session_entry = SessionEntry::from_record(record)
            .map_err(|e| StorageError::InvalidData(e.to_string()))?;

        device.set_active_session(session_entry);
        self.store.save_user(&user_record)?;

        Ok(())
    }

    /// Get the active session for encryption.
    ///
    /// Returns None if no session exists.
    pub fn get_session_for_encrypt(
        &self,
        user_id: &UserId,
        device_id: u32,
    ) -> Result<Option<SessionRecord>, StorageError> {
        let record = self.store.load_user(user_id)?;

        let session = record
            .and_then(|r| r.get_device(device_id).cloned())
            .and_then(|d| d.active_session)
            .and_then(|s| s.to_record().ok());

        Ok(session)
    }

    /// Get all sessions for a device (active + inactive) for convergence attempts.
    ///
    /// Returns session records in order: active first, then inactive by recency.
    pub fn get_all_sessions_for_convergence(
        &self,
        user_id: &UserId,
        device_id: u32,
    ) -> Result<Vec<(Vec<u8>, SessionRecord)>, StorageError> {
        let record = match self.store.load_user(user_id)? {
            Some(r) => r,
            None => return Ok(Vec::new()),
        };

        let device = match record.get_device(device_id) {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };

        let sessions: Vec<(Vec<u8>, SessionRecord)> = device
            .all_sessions()
            .iter()
            .filter_map(|s| {
                s.to_record()
                    .ok()
                    .map(|r| (s.session_id.clone(), r))
            })
            .collect();

        Ok(sessions)
    }

    /// Record successful decryption with a session.
    ///
    /// If the session was inactive, it gets promoted to active.
    /// Updates session statistics.
    pub fn record_successful_decrypt(
        &self,
        user_id: &UserId,
        device_id: u32,
        session_id: &[u8],
        updated_record: &SessionRecord,
    ) -> Result<bool, StorageError> {
        let mut user_record = self.get_or_create_user(user_id)?;
        let device = user_record.get_or_create_device(device_id);

        let active_session_id = device
            .active_session
            .as_ref()
            .map(|s| s.session_id.clone());

        let was_promoted = if active_session_id.as_deref() == Some(session_id) {
            // Update active session stats
            if let Some(ref mut active) = device.active_session {
                active.update_record(updated_record)
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                active.last_recv_at = Some(Utc::now());
                active.recv_count += 1;
            }
            false
        } else {
            // Promote inactive session to active
            device.promote_session(session_id);
            if let Some(ref mut active) = device.active_session {
                active.update_record(updated_record)
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                active.last_recv_at = Some(Utc::now());
                active.recv_count += 1;
            }
            true
        };

        self.store.save_user(&user_record)?;
        Ok(was_promoted)
    }

    /// Record a successful send on a session.
    pub fn record_send(
        &self,
        user_id: &UserId,
        device_id: u32,
    ) -> Result<(), StorageError> {
        let mut user_record = self.get_or_create_user(user_id)?;

        if let Some(device) = user_record.get_device_mut(device_id) {
            // Clone session_id to avoid borrow conflict
            let session_id = device.active_session.as_ref().map(|s| s.session_id.clone());
            if let Some(sid) = session_id {
                device.record_send(&sid);
                self.store.save_user(&user_record)?;
            }
        }

        Ok(())
    }

    /// Mark a user's device list as stale.
    ///
    /// Called when we detect the user may have new devices.
    pub fn mark_user_stale(&self, user_id: &UserId) -> Result<(), StorageError> {
        let mut record = self.get_or_create_user(user_id)?;
        record.mark_all_stale();
        self.store.save_user(&record)?;
        Ok(())
    }

    /// Archive all sessions for a device.
    ///
    /// Called when identity key changes.
    pub fn archive_all_sessions(&self, user_id: &UserId, device_id: u32) -> Result<(), StorageError> {
        let mut record = self.get_or_create_user(user_id)?;

        if let Some(device) = record.get_device_mut(device_id) {
            device.archive_active_session();
        }

        self.store.save_user(&record)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_record_new() {
        let user_id = UserId::new();
        let record = UserRecord::new(user_id.clone());

        assert_eq!(record.user_id, user_id);
        assert!(record.devices.is_empty());
        assert!(!record.stale);
        assert!(record.last_directory_check.is_none());
        assert!(record.pending_messages.is_empty());
    }

    #[test]
    fn test_user_record_get_or_create_device() {
        let user_id = UserId::new();
        let mut record = UserRecord::new(user_id);

        // Create device
        let device = record.get_or_create_device(1);
        assert_eq!(device.device_id, 1);

        // Get same device
        let device2 = record.get_or_create_device(1);
        assert_eq!(device2.device_id, 1);

        // One device in map
        assert_eq!(record.devices.len(), 1);
    }

    #[test]
    fn test_device_record_archive_session() {
        let mut device = DeviceRecord::new(1);

        // Create a mock session entry
        let session = SessionEntry {
            session_id: vec![1, 2, 3],
            session_record: vec![4, 5, 6],
            created_at: Utc::now(),
            last_send_at: None,
            last_recv_at: None,
            send_count: 0,
            recv_count: 0,
        };

        device.active_session = Some(session);
        assert!(device.active_session.is_some());
        assert!(device.inactive_sessions.is_empty());

        // Archive
        device.archive_active_session();
        assert!(device.active_session.is_none());
        assert_eq!(device.inactive_sessions.len(), 1);
    }

    #[test]
    fn test_device_record_promote_session() {
        let mut device = DeviceRecord::new(1);

        // Create sessions
        let session1 = SessionEntry {
            session_id: vec![1],
            session_record: vec![],
            created_at: Utc::now(),
            last_send_at: None,
            last_recv_at: None,
            send_count: 0,
            recv_count: 0,
        };

        let session2 = SessionEntry {
            session_id: vec![2],
            session_record: vec![],
            created_at: Utc::now(),
            last_send_at: None,
            last_recv_at: None,
            send_count: 0,
            recv_count: 0,
        };

        device.active_session = Some(session1);
        device.inactive_sessions.push_back(session2);

        // Promote session 2
        assert!(device.promote_session(&[2]));
        assert_eq!(device.active_session.as_ref().unwrap().session_id, vec![2]);
        assert_eq!(device.inactive_sessions.len(), 1);
        assert_eq!(device.inactive_sessions[0].session_id, vec![1]);
    }

    #[test]
    fn test_pending_message_queue() {
        let user_id = UserId::new();
        let mut record = UserRecord::new(user_id);

        // Queue some messages
        record.queue_message(vec![1, 2, 3], Utc::now(), vec![], None);
        record.queue_message(vec![4, 5, 6], Utc::now(), vec![], None);

        assert_eq!(record.pending_messages.len(), 2);
        assert!(record.has_pending_messages());

        // Drain
        let messages = record.drain_pending_messages();
        assert_eq!(messages.len(), 2);
        assert!(record.pending_messages.is_empty());
    }

    #[test]
    fn test_max_inactive_sessions() {
        let mut device = DeviceRecord::new(1);

        // Add more than MAX_INACTIVE_SESSIONS
        for i in 0..(MAX_INACTIVE_SESSIONS + 3) {
            let session = SessionEntry {
                session_id: vec![i as u8],
                session_record: vec![],
                created_at: Utc::now(),
                last_send_at: None,
                last_recv_at: None,
                send_count: 0,
                recv_count: 0,
            };
            device.set_active_session(session);
        }

        // Should be limited
        assert!(device.inactive_sessions.len() <= MAX_INACTIVE_SESSIONS);
    }
}

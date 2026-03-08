//! Sesame data types.
//!
//! Types for session tracking, device records, and pending messages.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use libsignal_protocol::SessionRecord;

use crate::domain::UserId;

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

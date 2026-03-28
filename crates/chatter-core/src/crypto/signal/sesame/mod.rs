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
//!
//! This module is organized into:
//! - `types` - Data structures (UserRecord, DeviceRecord, SessionEntry, etc.)
//! - `store` - Persistence trait and SQLite implementation

mod store;
mod types;

pub use store::{SesameStore, SqliteSesameStore};
pub use types::{
    ConvergenceResult, DeviceRecord, PendingMessage, SessionEntry, SessionUsability,
    UserRecord, MAX_INACTIVE_SESSIONS, MAXLATENCY_SECS, MAX_PENDING_MESSAGES,
    MAXRECV_SECS, MAXSEND_SECS, PENDING_MESSAGE_TTL_SECS, STALE_DEVICE_HARD_DELETE_SECS,
};

use chrono::Utc;
use libsignal_protocol::SessionRecord;

use crate::domain::UserId;
use crate::error::StorageError;

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

    /// Get the underlying store.
    pub fn store(&self) -> &S {
        &self.store
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

    /// Check if a user has a *usable* active session on any device.
    ///
    /// Unlike `has_session()`, this also validates that the session hasn't
    /// exceeded MAXSEND and can be deserialized.
    pub fn has_usable_session(&self, user_id: &UserId) -> Result<bool, StorageError> {
        let record = self.store.load_user(user_id)?;
        Ok(record
            .map(|r| r.devices.values().any(|d| d.has_usable_session()))
            .unwrap_or(false))
    }

    /// Check usability of a specific device's active session.
    ///
    /// Returns `SessionUsability::Usable` if the session can be used for sending,
    /// or the reason it cannot.
    pub fn check_session_usability(
        &self,
        user_id: &UserId,
        device_id: u32,
    ) -> Result<SessionUsability, StorageError> {
        let record = self.store.load_user(user_id)?;

        let usability = record
            .and_then(|r| r.get_device(device_id).cloned())
            .and_then(|d| d.active_session)
            .map(|s| s.usability())
            .unwrap_or(SessionUsability::Corrupted);

        Ok(usability)
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

    /// Get the active session for encryption, only if usable.
    ///
    /// Returns `None` if no session exists or the session is not usable for
    /// sending (expired past MAXSEND, corrupted, etc.). Expired sessions are
    /// automatically moved to the inactive list.
    pub fn get_session_for_encrypt(
        &self,
        user_id: &UserId,
        device_id: u32,
    ) -> Result<Option<SessionRecord>, StorageError> {
        let mut user_record = match self.store.load_user(user_id)? {
            Some(r) => r,
            None => return Ok(None),
        };

        let device = match user_record.get_device_mut(device_id) {
            Some(d) => d,
            None => return Ok(None),
        };

        // Check usability before returning
        let usability = device
            .active_session
            .as_ref()
            .map(|s| s.usability());

        match usability {
            Some(SessionUsability::Usable) => {
                // Session is good — return it
                Ok(device.active_session.as_ref().and_then(|s| s.to_record().ok()))
            }
            Some(SessionUsability::ExpiredForSend) | Some(SessionUsability::ExpiredForRecv) => {
                // Session expired for sending — archive it so a fresh session is established
                tracing::info!(
                    user_id = %user_id,
                    device_id,
                    "Active session expired (MAXSEND), archiving"
                );
                device.archive_active_session();
                self.store.save_user(&user_record)?;
                Ok(None)
            }
            Some(SessionUsability::Corrupted) => {
                tracing::warn!(
                    user_id = %user_id,
                    device_id,
                    "Active session corrupted, removing"
                );
                device.active_session = None;
                self.store.save_user(&user_record)?;
                Ok(None)
            }
            None => Ok(None),
        }
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

    /// Mark a specific device as stale.
    ///
    /// Called when the server rejects a send with "device not found".
    /// The device record will be hard-deleted after MAXLATENCY.
    pub fn mark_device_stale(
        &self,
        user_id: &UserId,
        device_id: u32,
    ) -> Result<(), StorageError> {
        let mut record = self.get_or_create_user(user_id)?;

        if let Some(device) = record.get_device_mut(device_id) {
            tracing::info!(
                user_id = %user_id,
                device_id,
                "Marking device as stale (server reports device not found)"
            );
            device.mark_stale();
            self.store.save_user(&record)?;
        }

        Ok(())
    }

    /// Clean up expired sessions and stale devices for all users.
    ///
    /// Per Sesame spec:
    /// - Sessions past MAXRECV are deleted
    /// - Stale devices past MAXLATENCY are hard-deleted
    ///
    /// Returns (sessions_removed, devices_removed).
    pub fn cleanup_expired(&self) -> Result<(usize, usize), StorageError> {
        let users = self.store.load_all_users()?;
        let mut total_sessions = 0;
        let mut total_devices = 0;

        for mut user_record in users {
            let mut modified = false;

            // Collect device IDs to remove (stale + past MAXLATENCY)
            let devices_to_remove: Vec<u32> = user_record
                .devices
                .values()
                .filter(|d| d.should_hard_delete())
                .map(|d| d.device_id)
                .collect();

            for device_id in &devices_to_remove {
                tracing::info!(
                    user_id = %user_record.user_id,
                    device_id,
                    "Hard-deleting stale device (past MAXLATENCY)"
                );
                user_record.devices.remove(device_id);
                total_devices += 1;
                modified = true;
            }

            // Clean up expired sessions on remaining devices
            for device in user_record.devices.values_mut() {
                let removed = device.cleanup_expired_sessions();
                if removed > 0 {
                    total_sessions += removed;
                    modified = true;
                }
            }

            if modified {
                self.store.save_user(&user_record)?;
            }
        }

        if total_sessions > 0 || total_devices > 0 {
            tracing::info!(
                sessions_removed = total_sessions,
                devices_removed = total_devices,
                "Sesame cleanup complete"
            );
        }

        Ok((total_sessions, total_devices))
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

    #[test]
    fn test_session_usability_fresh() {
        let session = SessionEntry {
            session_id: vec![1, 2, 3],
            session_record: vec![4, 5, 6], // invalid bytes, will be corrupted
            created_at: Utc::now(),
            last_send_at: None,
            last_recv_at: None,
            send_count: 0,
            recv_count: 0,
        };

        // Invalid session_record bytes => corrupted
        assert_eq!(session.usability(), SessionUsability::Corrupted);
        assert!(!session.is_usable_for_send());
    }

    #[test]
    fn test_session_usability_expired_maxsend() {
        use chrono::Duration;

        let session = SessionEntry {
            session_id: vec![1],
            session_record: vec![], // will fail deserialize => corrupted
            // Set created_at to MAXSEND + 1 hour ago
            created_at: Utc::now() - Duration::seconds(MAXSEND_SECS + 3600),
            last_send_at: None,
            last_recv_at: None,
            send_count: 5,
            recv_count: 3,
        };

        // Past MAXSEND and also corrupted (invalid bytes), so Corrupted takes priority
        // since we check deserialization first
        let usability = session.usability();
        assert_ne!(usability, SessionUsability::Usable);
    }

    #[test]
    fn test_device_has_usable_session() {
        let mut device = DeviceRecord::new(1);

        // No session => not usable
        assert!(!device.has_usable_session());

        // Add a session with invalid bytes => corrupted => not usable
        device.active_session = Some(SessionEntry {
            session_id: vec![1],
            session_record: vec![99, 99],
            created_at: Utc::now(),
            last_send_at: None,
            last_recv_at: None,
            send_count: 0,
            recv_count: 0,
        });
        assert!(!device.has_usable_session());
    }

    #[test]
    fn test_device_cleanup_expired_sessions() {
        use chrono::Duration;

        let mut device = DeviceRecord::new(1);

        // Active session that's past MAXRECV
        device.active_session = Some(SessionEntry {
            session_id: vec![1],
            session_record: vec![],
            created_at: Utc::now() - Duration::seconds(MAXRECV_SECS + 100),
            last_send_at: None,
            last_recv_at: None,
            send_count: 0,
            recv_count: 0,
        });

        // Inactive session that's fresh
        device.inactive_sessions.push_back(SessionEntry {
            session_id: vec![2],
            session_record: vec![],
            created_at: Utc::now(),
            last_send_at: None,
            last_recv_at: None,
            send_count: 0,
            recv_count: 0,
        });

        // Inactive session that's past MAXRECV
        device.inactive_sessions.push_back(SessionEntry {
            session_id: vec![3],
            session_record: vec![],
            created_at: Utc::now() - Duration::seconds(MAXRECV_SECS + 200),
            last_send_at: None,
            last_recv_at: None,
            send_count: 0,
            recv_count: 0,
        });

        let removed = device.cleanup_expired_sessions();
        // Active (expired) + inactive #3 (expired) = 2 removed
        assert_eq!(removed, 2);
        assert!(device.active_session.is_none());
        assert_eq!(device.inactive_sessions.len(), 1);
        assert_eq!(device.inactive_sessions[0].session_id, vec![2]);
    }

    #[test]
    fn test_device_should_hard_delete() {
        use chrono::Duration;

        let mut device = DeviceRecord::new(1);

        // Not stale => no delete
        assert!(!device.should_hard_delete());

        // Stale but just marked => no delete
        device.mark_stale();
        assert!(!device.should_hard_delete());

        // Stale and past MAXLATENCY => should delete
        device.stale_timestamp = Some(Utc::now() - Duration::seconds(STALE_DEVICE_HARD_DELETE_SECS + 10));
        assert!(device.should_hard_delete());
    }

    #[test]
    fn test_session_age() {
        use chrono::Duration;

        let session = SessionEntry {
            session_id: vec![1],
            session_record: vec![],
            created_at: Utc::now() - Duration::seconds(3600),
            last_send_at: None,
            last_recv_at: None,
            send_count: 0,
            recv_count: 0,
        };

        // Should be approximately 3600 seconds
        let age = session.age_secs();
        assert!(age >= 3599 && age <= 3601);
    }
}

//! Key synchronization.
//!
//! Encryption key sync is handled by SignalService (prekey upload, session management).
//! This module provides a thin wrapper for the sync engine to trigger key checks.

/// Key sync handler.
///
/// Key synchronization (prekey replenishment, signed prekey rotation) is handled
/// by `SignalService`. This struct exists as a coordination point for the sync
/// engine to request key health checks during sync cycles.
pub struct KeySync;

impl KeySync {
    pub fn new() -> Self {
        Self
    }
}

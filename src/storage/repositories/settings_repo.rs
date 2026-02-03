//! Settings repository.
//!
//! Data access for application settings with typed key-value storage.

use crate::error::StorageError;
use crate::storage::Database;
use chrono::Utc;
use rusqlite::params;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::sync::Arc;

/// Well-known setting keys.
pub mod keys {
    // App settings
    pub const THEME: &str = "app.theme";
    pub const NOTIFICATION_SOUND: &str = "app.notification_sound";
    pub const NOTIFICATION_ENABLED: &str = "app.notification_enabled";

    // Privacy settings
    pub const SEND_READ_RECEIPTS: &str = "privacy.send_read_receipts";
    pub const SHOW_TYPING_INDICATORS: &str = "privacy.show_typing_indicators";

    // Sync settings
    pub const LAST_SYNC_TIMESTAMP: &str = "sync.last_timestamp";

    // Auth settings
    pub const CURRENT_USER_ID: &str = "auth.current_user_id";
    pub const CURRENT_DEVICE_ID: &str = "auth.current_device_id";

    // UI settings
    pub const WINDOW_WIDTH: &str = "ui.window_width";
    pub const WINDOW_HEIGHT: &str = "ui.window_height";
    pub const WINDOW_MAXIMIZED: &str = "ui.window_maximized";
    pub const SIDEBAR_WIDTH: &str = "ui.sidebar_width";
    pub const FONT_SIZE: &str = "ui.font_size";
    pub const MESSAGE_FONT_SIZE: &str = "ui.message_font_size";
    pub const COMPACT_MODE: &str = "ui.compact_mode";
}

/// Settings data repository.
///
/// Provides typed access to application settings stored as key-value pairs.
/// Values are JSON-serialized for consistent parsing across all types.
pub struct SettingsRepository {
    db: Arc<Database>,
}

impl SettingsRepository {
    /// Create a new SettingsRepository.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Generic Typed Accessors
    // ========================================================================

    /// Get a setting by key, deserializing from JSON.
    ///
    /// Returns `None` if the key doesn't exist.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare("SELECT value_json FROM settings WHERE key = ?1")?;

        let result: Result<String, _> = stmt.query_row(params![key], |row| row.get(0));

        match result {
            Ok(json) => {
                let value: T = serde_json::from_str(&json)
                    .map_err(|e| StorageError::InvalidData(format!("Failed to deserialize setting '{}': {}", key, e)))?;
                Ok(Some(value))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Set a setting by key, serializing to JSON.
    ///
    /// Uses upsert semantics: creates if new, updates if exists.
    pub fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let json = serde_json::to_string(value)
            .map_err(|e| StorageError::InvalidData(format!("Failed to serialize setting '{}': {}", key, e)))?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO settings (key, value_json, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET
                 value_json = excluded.value_json,
                 updated_at = excluded.updated_at",
            params![key, json, now],
        )?;

        Ok(())
    }

    /// Delete a setting by key.
    ///
    /// Returns `true` if the key existed and was deleted, `false` if not found.
    pub fn delete(&self, key: &str) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute("DELETE FROM settings WHERE key = ?1", params![key])?;
        Ok(rows > 0)
    }

    // ========================================================================
    // Convenience String Methods
    // ========================================================================

    /// Get a string setting.
    pub fn get_string(&self, key: &str) -> Result<Option<String>, StorageError> {
        self.get(key)
    }

    /// Set a string setting.
    pub fn set_string(&self, key: &str, value: &str) -> Result<(), StorageError> {
        self.set(key, &value)
    }

    // ========================================================================
    // Convenience Boolean Methods
    // ========================================================================

    /// Get a boolean setting.
    pub fn get_bool(&self, key: &str) -> Result<Option<bool>, StorageError> {
        self.get(key)
    }

    /// Set a boolean setting.
    pub fn set_bool(&self, key: &str, value: bool) -> Result<(), StorageError> {
        self.set(key, &value)
    }

    // ========================================================================
    // Convenience Integer Methods
    // ========================================================================

    /// Get an i64 setting.
    pub fn get_i64(&self, key: &str) -> Result<Option<i64>, StorageError> {
        self.get(key)
    }

    /// Set an i64 setting.
    pub fn set_i64(&self, key: &str, value: i64) -> Result<(), StorageError> {
        self.set(key, &value)
    }

    // ========================================================================
    // Convenience Float Methods
    // ========================================================================

    /// Get an f64 setting.
    pub fn get_f64(&self, key: &str) -> Result<Option<f64>, StorageError> {
        self.get(key)
    }

    /// Set an f64 setting.
    pub fn set_f64(&self, key: &str, value: f64) -> Result<(), StorageError> {
        self.set(key, &value)
    }

    // ========================================================================
    // Bulk Operations
    // ========================================================================

    /// Get all settings as (key, value_json) pairs.
    pub fn get_all(&self) -> Result<Vec<(String, String)>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare("SELECT key, value_json FROM settings ORDER BY key")?;
        let mut rows = stmt.query([])?;

        let mut settings = Vec::new();
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let value_json: String = row.get(1)?;
            settings.push((key, value_json));
        }
        Ok(settings)
    }

    /// Clear all settings.
    ///
    /// Returns the number of settings deleted.
    pub fn clear_all(&self) -> Result<u64, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute("DELETE FROM settings", [])?;
        Ok(rows as u64)
    }
}

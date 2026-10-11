//! Settings repository.
//!
//! Data access for application settings with typed key-value storage.

use crate::error::StorageError;
use crate::storage::Database;
use chrono::Utc;
use rusqlite::params;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;

/// Well-known setting keys.
pub mod keys {
    // App settings
    pub const THEME: &str = "app.theme";
    /// Settings › Appearance › Text size — `services::appearance::TextSize`.
    pub const TEXT_SIZE: &str = "app.text_size";
    /// Settings › Appearance › Message spacing — `services::appearance::MessageDensity`.
    pub const MESSAGE_DENSITY: &str = "app.message_density";
    /// Device-local chrome effects preference, enabled by default.
    pub const GLASS_EFFECTS: &str = "app.glass_effects";
    /// Device-local motion preference: skip decorative animation.
    pub const REDUCE_MOTION: &str = "app.reduce_motion";
    pub const NOTIFICATION_SOUND: &str = "app.notification_sound";
    /// Opaque ntfy push topic issued by the server in AuthResponse. The client
    /// subscribes to this instead of deriving `chatter_{user_id}` locally
    /// (which leaked the user_id and didn't match the server's publish topic).
    pub const DEVICE_WAKE_TOPIC: &str = "push.device_wake_topic";
    pub const PUSH_TOPIC: &str = "push.topic";

    // Privacy settings
    pub const SEND_READ_RECEIPTS: &str = "privacy.send_read_receipts";
    pub const SHOW_TYPING_INDICATORS: &str = "privacy.show_typing_indicators";
    /// Manual presence override: "auto" | "online" | "away" | "dnd" | "invisible".
    /// "auto" (default) = start online, idle detector may set away.
    pub const PRESENCE_MODE: &str = "presence.mode";

    // Auth settings
    pub const CURRENT_USER_ID: &str = "auth.current_user_id";
    pub const CURRENT_DEVICE_ID: &str = "auth.current_device_id";
    /// Device authentication token (32 bytes, base64 encoded)
    pub const DEVICE_AUTH_TOKEN: &str = "auth.device_token";
    /// Prefix of the per-account trusted-device token (server OPEN-ITEMS
    /// §134, OWASP "device cookie"): `auth.trusted_device_token.{login
    /// identifier}`. Kept across logout on purpose — it is what lets this
    /// device sign back in while strangers burn the account's shared login
    /// ceiling. In this (SQLCipher) database like every other credential.
    pub const TRUSTED_DEVICE_TOKEN_PREFIX: &str = "auth.trusted_device_token.";

    /// Settings key of this device's trusted-device token for the login
    /// identifier `identifier`.
    pub fn trusted_device_token_key(identifier: &str) -> String {
        format!("{TRUSTED_DEVICE_TOKEN_PREFIX}{identifier}")
    }
    /// Current user's plaintext display name
    pub const DISPLAY_NAME: &str = "auth.display_name";
    /// Full Signal-style `nickname.discriminator` chosen at registration.
    /// Used as the input to `hash_username_lookup` for discovery flows; the
    /// raw nickname alone is insufficient (Signal's lookup hash binds the
    /// random discriminator to defeat wordlist enumeration).
    pub const FULL_USERNAME: &str = "auth.full_username";
    /// The exact username string the user typed at register/login — the salt
    /// for every PIN → Argon2id derivation (`derive_svr_secret`). Persisted so
    /// PIN-gated flows launched from settings (server message-backup
    /// enable/restore) can re-derive the SVR secret without asking the user to
    /// retype their username. NOT the same as [`FULL_USERNAME`], which carries
    /// the server-assigned `.discriminator` suffix.
    pub const SVR_SALT_USERNAME: &str = "auth.svr_salt_username";
    /// Current user's avatar (raw image bytes, stored locally, encrypted when shared)
    pub const AVATAR: &str = "profile.avatar";
    /// The user's active username-link URL (`chatter://u/{id}#{entropy}`).
    /// Local convenience copy so the share modal can re-show it without a new
    /// upload; the authoritative blob lives server-side under the link id.
    pub const USERNAME_LINK_URL: &str = "profile.username_link_url";

    // Crypto keys
    /// Profile key for encrypting saved messages (32 bytes, base64 encoded)
    pub const PROFILE_KEY: &str = "crypto.profile_key";
    /// Our identity public key (33 bytes - 0x05 + Curve25519, base64 encoded)
    /// Used for NDSS'21 private friend codes that embed cryptographic material.
    pub const IDENTITY_KEY_PUBLIC: &str = "crypto.identity_key_public";
    /// Set while this device owes the server a key-material republish, and
    /// cleared once the server has acknowledged it.
    ///
    /// A recovery hands this device an identity key whose published prekeys
    /// belong to a device that no longer exists. If the republish that fixes
    /// that is interrupted — server down, connection dropped — the local keys
    /// are already in place, so the next login would see a local identity, skip
    /// the restore, and never retry. The account would sit there serving
    /// prekeys nobody can complete a handshake with: the original bug, reached
    /// by a different road. This flag is what makes the retry happen.
    pub const KEY_REPUBLISH_PENDING: &str = "crypto.key_republish_pending";
    /// Who receives our profile key ("everyone" | "contacts" | "nobody"),
    /// see `services::profile_visibility`. Absent = everyone.
    pub const PROFILE_VISIBILITY: &str = "privacy.profile_visibility";
    /// A rotated profile key not yet committed: generated and being written
    /// to the SVR backup. Reused by a retry so the backup and the local key
    /// can never name two different fresh keys.
    pub const PROFILE_KEY_NEXT: &str = "crypto.profile_key_next";
    /// Set while a committed profile-key rotation still owes our other
    /// devices, the server UAK and the server profile the new key; nothing is
    /// shared with anyone until it clears.
    pub const PROFILE_KEY_ROTATION_PENDING: &str = "crypto.profile_key_rotation_pending";
    /// Cached sender certificate (serialized bytes, base64 encoded)
    pub const SENDER_CERTIFICATE: &str = "crypto.sender_certificate";
    /// When to refresh sender certificate (Unix timestamp ms)
    pub const SENDER_CERTIFICATE_REFRESH_AFTER: &str = "crypto.sender_certificate_refresh_after";
    /// Server key ID from the sender certificate (u32, for SealedSenderMessage envelope)
    pub const SENDER_CERTIFICATE_SERVER_KEY_ID: &str = "crypto.sender_certificate_server_key_id";
    /// Local-only PIN fingerprint for re-auth gates (32 bytes, encrypted-at-rest).
    /// Set on register / change-pin via `crypto::svr::derive_pin_fingerprint`.
    /// Never sent to the server. Used by [`LinkDeviceService::verify_pin`].
    pub const PIN_FINGERPRINT: &str = "crypto.pin_fingerprint";

    // UI settings
    pub const SIDEBAR_COLLAPSED: &str = "ui.sidebar_collapsed";
    pub const SEARCH_SHORTCUT: &str = "ui.search_shortcut";

    /// Whether the user enabled the BLE mesh. Absent means off; radio use is opt-in.
    pub const MESH_ENABLED: &str = "mesh.enabled";

    /// Group resends this device asked for — see `SolicitedResends`.
    pub const SOLICITED_RESENDS: &str = "message.solicited_resends";

    // Notification privacy settings
    // Controls what metadata leaks to the OS notification system.
    // Signal Desktop approach: default to no content, user opts in.
    /// Notification detail level: "none", "name_only", "name_and_message"
    /// Default: "none" (just "Chatter: New message" — no sender, no content)
    pub const NOTIFICATION_DETAIL_LEVEL: &str = "notification.detail_level";

    // Local AI settings
    pub const LOCAL_AI_ENABLED: &str = "local_ai.enabled";
    pub const LOCAL_AI_MODEL_ID: &str = "local_ai.model_id";
    pub const LOCAL_AI_QUANTIZED_FILE: &str = "local_ai.quantized_file";
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

    /// Get a reference to the underlying database.
    ///
    /// Used for operations that need direct database access (e.g., TokenManager).
    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }

    // ========================================================================
    // Generic Typed Accessors
    // ========================================================================

    /// Get a setting by key, deserializing from JSON.
    ///
    /// Returns `None` if the key doesn't exist.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = ?1")?;

        let result: Result<String, _> = stmt.query_row(params![key], |row| row.get(0));

        match result {
            Ok(json) => {
                let value: T = serde_json::from_str(&json).map_err(|e| {
                    StorageError::InvalidData(format!(
                        "Failed to deserialize setting '{}': {}",
                        key, e
                    ))
                })?;
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
        let json = serde_json::to_string(value).map_err(|e| {
            StorageError::InvalidData(format!("Failed to serialize setting '{}': {}", key, e))
        })?;
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO settings (key, value, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET
                 value = excluded.value,
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
    // Convenience Bytes Methods (for cryptographic keys)
    // ========================================================================

    /// Get a raw bytes setting (stored as base64).
    ///
    /// Used for cryptographic keys like profile_key that should be stored as raw bytes.
    pub fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

        let b64: Option<String> = self.get(key)?;
        match b64 {
            Some(encoded) => {
                let bytes = BASE64.decode(&encoded).map_err(|e| {
                    StorageError::InvalidData(format!(
                        "Failed to decode bytes for '{}': {}",
                        key, e
                    ))
                })?;
                Ok(Some(bytes))
            }
            None => Ok(None),
        }
    }

    /// Set a raw bytes setting (stored as base64).
    ///
    /// Used for cryptographic keys like profile_key that should be stored as raw bytes.
    pub fn set_bytes(&self, key: &str, value: &[u8]) -> Result<(), StorageError> {
        use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

        let encoded = BASE64.encode(value);
        self.set(key, &encoded)
    }

    // ========================================================================
    // Bulk Operations
    // ========================================================================

    /// Get all settings as (key, value_json) pairs.
    pub fn get_all(&self) -> Result<Vec<(String, String)>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare("SELECT key, value FROM settings ORDER BY key")?;
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

    // ========================================================================
    // Local AI Settings
    // ========================================================================

    /// Check if local AI is enabled. Off by default — it downloads and runs
    /// a multi-GB model on-device; users opt in explicitly rather than
    /// discovering the download/CPU cost after the fact.
    pub fn get_local_ai_enabled(&self) -> Result<bool, StorageError> {
        Ok(self.get_bool(keys::LOCAL_AI_ENABLED)?.unwrap_or(false))
    }

    /// Set whether local AI is enabled.
    pub fn set_local_ai_enabled(&self, enabled: bool) -> Result<(), StorageError> {
        self.set_bool(keys::LOCAL_AI_ENABLED, enabled)
    }

    /// Get the configured local AI model.
    ///
    /// Returns (model_id, quantized_file) if configured.
    pub fn get_local_ai_model(&self) -> Result<Option<(String, Option<String>)>, StorageError> {
        let model_id = self.get_string(keys::LOCAL_AI_MODEL_ID)?;
        let quantized = self.get_string(keys::LOCAL_AI_QUANTIZED_FILE)?;
        Ok(model_id.map(|id| (id, quantized)))
    }

    /// Set the local AI model configuration.
    pub fn set_local_ai_model(
        &self,
        model_id: &str,
        quantized_file: Option<&str>,
    ) -> Result<(), StorageError> {
        self.set_string(keys::LOCAL_AI_MODEL_ID, model_id)?;
        if let Some(qf) = quantized_file {
            self.set_string(keys::LOCAL_AI_QUANTIZED_FILE, qf)?;
        } else {
            self.delete(keys::LOCAL_AI_QUANTIZED_FILE)?;
        }
        Ok(())
    }
}

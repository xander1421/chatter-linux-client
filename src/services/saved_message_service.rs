//! Saved message service.
//!
//! Orchestrates saved message operations for the personal vault using the
//! SavedMessagesClient for server-side operations. No local storage for v1 -
//! all data is server-side and encrypted with the user's profile key.

use std::sync::Arc;

use base64::Engine;

use crate::crypto::{SavedMessageCrypto, SavedMessageSourcePlain};
use crate::domain::ids::{ChannelId, MessageId, SavedMessageId, UserId};
use crate::domain::saved_message::{SavedContent, SavedMessage};
use crate::error::{CryptoError, NetworkError};
use crate::network::grpc::SavedMessagesClient;
use crate::proto::saved_messages_v1::{
    ListSavedMessagesResponse, PinnedSortBy, SavedMessage as ProtoSavedMessage,
    SavedMessageContent, SavedMessageSource, SortDirection,
};
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// Saved message service errors.
#[derive(Debug, thiserror::Error)]
pub enum SavedMessageError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Requested message not found.
    #[error("message not found")]
    NotFound,

    /// Invalid data in request or response.
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// Encryption/decryption error.
    #[error("encryption error: {0}")]
    Encryption(#[from] CryptoError),
}

/// Saved message service.
///
/// Orchestrates saved message operations for the personal vault using
/// network client for server-side operations. Content is encrypted
/// server-side with the user's profile key.
///
/// # Example
///
/// ```ignore
/// let service = SavedMessageService::new(settings_repo);
///
/// // Save a message
/// let saved = service.save_message(&mut client, content, None).await?;
///
/// // List saved messages
/// let response = service.list_messages(&mut client, None, None, None, None).await?;
///
/// // Pin a message
/// let pinned = service.pin_message(&mut client, &message_id).await?;
/// ```
pub struct SavedMessageService {
    settings_repo: Arc<SettingsRepository>,
}

impl SavedMessageService {
    /// Create a new SavedMessageService with dependencies.
    pub fn new(settings_repo: Arc<SettingsRepository>) -> Self {
        Self { settings_repo }
    }

    /// Get the currently logged-in user ID.
    fn current_user_id(&self) -> Result<String, SavedMessageError> {
        self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(|_| SavedMessageError::NotLoggedIn)?
            .ok_or(SavedMessageError::NotLoggedIn)
    }

    /// Get the profile key for encryption/decryption.
    ///
    /// Returns the 32-byte profile key stored in settings (base64 encoded).
    fn get_profile_key(&self) -> Result<[u8; 32], SavedMessageError> {
        let key_b64 = self
            .settings_repo
            .get_string(keys::PROFILE_KEY)
            .map_err(|_| SavedMessageError::NotLoggedIn)?
            .ok_or(SavedMessageError::NotLoggedIn)?;

        let key_bytes = base64::engine::general_purpose::STANDARD
            .decode(&key_b64)
            .map_err(|_| SavedMessageError::InvalidData("invalid profile key encoding".to_string()))?;

        if key_bytes.len() != 32 {
            return Err(SavedMessageError::InvalidData(format!(
                "profile key wrong length: expected 32, got {}",
                key_bytes.len()
            )));
        }

        let mut key = [0u8; 32];
        key.copy_from_slice(&key_bytes);
        Ok(key)
    }

    // ========================================================================
    // Encryption Helpers
    // ========================================================================

    /// Encrypt text content for saving.
    ///
    /// Encrypts text using the user's profile key and wraps in SavedMessageContent.
    ///
    /// # Arguments
    ///
    /// * `text` - The plaintext content to encrypt
    pub fn encrypt_text(&self, text: &str) -> Result<SavedMessageContent, SavedMessageError> {
        let profile_key = self.get_profile_key()?;
        SavedMessageCrypto::encrypt_text(text, &profile_key).map_err(SavedMessageError::Encryption)
    }

    /// Encrypt source metadata for saving.
    ///
    /// Encrypts source information using the user's profile key.
    ///
    /// # Arguments
    ///
    /// * `source` - The plaintext source metadata
    pub fn encrypt_source(
        &self,
        source: &SavedMessageSourcePlain,
    ) -> Result<SavedMessageSource, SavedMessageError> {
        let profile_key = self.get_profile_key()?;
        SavedMessageCrypto::encrypt_source(source, &profile_key)
            .map_err(SavedMessageError::Encryption)
    }

    /// Decrypt message content after fetching.
    ///
    /// Decrypts content using the user's profile key.
    ///
    /// # Arguments
    ///
    /// * `content` - The encrypted SavedMessageContent
    pub fn decrypt_content(
        &self,
        content: &SavedMessageContent,
    ) -> Result<String, SavedMessageError> {
        let profile_key = self.get_profile_key()?;
        SavedMessageCrypto::decrypt_content(content, &profile_key)
            .map_err(SavedMessageError::Encryption)
    }

    /// Decrypt source metadata after fetching.
    ///
    /// Decrypts source using the user's profile key.
    ///
    /// # Arguments
    ///
    /// * `source` - The encrypted SavedMessageSource
    pub fn decrypt_source(
        &self,
        source: &SavedMessageSource,
    ) -> Result<SavedMessageSourcePlain, SavedMessageError> {
        let profile_key = self.get_profile_key()?;
        SavedMessageCrypto::decrypt_source(source, &profile_key)
            .map_err(SavedMessageError::Encryption)
    }

    // ========================================================================
    // Server Operations
    // ========================================================================

    /// Save a message to the personal vault.
    ///
    /// Creates a new saved message with the provided content and optional source.
    /// Generates a UUID for idempotency.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to saved messages client
    /// * `content` - The content to save (already encrypted)
    /// * `source` - Optional source information (where it came from)
    pub async fn save_message(
        &self,
        client: &mut SavedMessagesClient,
        content: SavedMessageContent,
        source: Option<SavedMessageSource>,
    ) -> Result<ProtoSavedMessage, SavedMessageError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Generate idempotency key
        let client_request_id = uuid::Uuid::new_v4().to_string();

        // Call server
        let message = client
            .save_message(content, source, &client_request_id)
            .await?;

        Ok(message)
    }

    /// List saved messages with pagination.
    ///
    /// Returns saved messages sorted newest first by default.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to saved messages client
    /// * `limit` - Maximum number of messages to return (default: 50)
    /// * `page_token` - Cursor token for pagination
    /// * `from_timestamp` - Optional start of time range filter (Unix ms)
    /// * `to_timestamp` - Optional end of time range filter (Unix ms)
    pub async fn list_messages(
        &self,
        client: &mut SavedMessagesClient,
        limit: Option<i32>,
        page_token: Option<&str>,
        from_timestamp: Option<u64>,
        to_timestamp: Option<u64>,
    ) -> Result<ListSavedMessagesResponse, SavedMessageError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Use defaults
        let limit = limit.unwrap_or(50);
        let page_token = page_token.unwrap_or("");

        // Call server with newest first
        let response = client
            .list_messages(
                limit,
                page_token,
                SortDirection::NewestFirst,
                from_timestamp,
                to_timestamp,
            )
            .await?;

        Ok(response)
    }

    /// Delete a saved message.
    ///
    /// Permanently removes a message from the personal vault.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to saved messages client
    /// * `message_id` - ID of the message to delete
    pub async fn delete_message(
        &self,
        client: &mut SavedMessagesClient,
        message_id: &str,
    ) -> Result<(), SavedMessageError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Generate idempotency key
        let request_id = uuid::Uuid::new_v4().to_string();

        // Call server
        client.delete_message(message_id, &request_id).await?;

        Ok(())
    }

    /// Pin a saved message.
    ///
    /// Pins a message for quick access. Returns the updated message.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to saved messages client
    /// * `message_id` - ID of the message to pin
    pub async fn pin_message(
        &self,
        client: &mut SavedMessagesClient,
        message_id: &str,
    ) -> Result<ProtoSavedMessage, SavedMessageError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Generate idempotency key
        let request_id = uuid::Uuid::new_v4().to_string();

        // Call server
        let message = client.pin_message(message_id, &request_id).await?;

        Ok(message)
    }

    /// Unpin a saved message.
    ///
    /// Removes the pin from a message.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to saved messages client
    /// * `message_id` - ID of the message to unpin
    pub async fn unpin_message(
        &self,
        client: &mut SavedMessagesClient,
        message_id: &str,
    ) -> Result<(), SavedMessageError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Generate idempotency key
        let request_id = uuid::Uuid::new_v4().to_string();

        // Call server
        client.unpin_message(message_id, &request_id).await?;

        Ok(())
    }

    /// List pinned saved messages.
    ///
    /// Returns only pinned messages, sorted by pin time (newest first).
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to saved messages client
    /// * `limit` - Maximum number of messages to return (default: 50)
    /// * `page_token` - Cursor token for pagination
    pub async fn list_pinned_messages(
        &self,
        client: &mut SavedMessagesClient,
        limit: Option<i32>,
        page_token: Option<&str>,
    ) -> Result<ListSavedMessagesResponse, SavedMessageError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Use defaults
        let limit = limit.unwrap_or(50);
        let page_token = page_token.unwrap_or("");

        // Call server with pin time sort, newest first
        let response = client
            .list_pinned_messages(
                limit,
                page_token,
                PinnedSortBy::PinnedAt,
                SortDirection::NewestFirst,
            )
            .await?;

        Ok(response)
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Convert proto SavedMessage to domain SavedMessage.
///
/// Note: Proto content is encrypted, domain type expects decrypted.
/// For v1, we parse the proto metadata and let the UI handle
/// content display/decryption as needed.
///
/// This conversion is best-effort: if the proto lacks certain fields,
/// they will be set to default values.
pub fn proto_to_domain_saved_message(
    proto: &ProtoSavedMessage,
) -> Result<SavedMessage, SavedMessageError> {
    // Parse IDs
    let id: SavedMessageId = proto
        .id
        .parse()
        .map_err(|_| SavedMessageError::InvalidData(format!("Invalid saved message ID: {}", proto.id)))?;

    let user_id: UserId = proto
        .user_id
        .parse()
        .map_err(|_| SavedMessageError::InvalidData(format!("Invalid user ID: {}", proto.user_id)))?;

    // Convert timestamps (proto uses u64 millis)
    let saved_at = chrono::DateTime::from_timestamp_millis(proto.created_at as i64)
        .unwrap_or_else(chrono::Utc::now);

    // For v1, we use placeholder content - actual content is encrypted in proto
    // and UI layer handles decryption and display
    let content = SavedContent::Text {
        content: "[encrypted content]".to_string(),
    };

    // Build domain SavedMessage with minimal info
    // Note: source info is encrypted in proto.source and not parsed here
    let domain_msg = SavedMessage {
        id,
        user_id,
        content,
        source_channel_id: None,
        source_message_id: None,
        note: None,
        tags: Vec::new(),
        saved_at,
    };

    // If we could parse source, we would do it here
    // For now, leave as None since source is encrypted
    // Future enhancement: add decryption support here

    Ok(domain_msg)
}

impl SavedMessageService {
    /// Convert proto SavedMessage to domain SavedMessage with decryption.
    ///
    /// Decrypts the content and source using the user's profile key.
    ///
    /// # Arguments
    ///
    /// * `proto` - The encrypted proto SavedMessage
    pub fn proto_to_domain_saved_message_decrypted(
        &self,
        proto: &ProtoSavedMessage,
    ) -> Result<SavedMessage, SavedMessageError> {
        // Parse IDs
        let id: SavedMessageId = proto
            .id
            .parse()
            .map_err(|_| SavedMessageError::InvalidData(format!("Invalid saved message ID: {}", proto.id)))?;

        let user_id: UserId = proto
            .user_id
            .parse()
            .map_err(|_| SavedMessageError::InvalidData(format!("Invalid user ID: {}", proto.user_id)))?;

        // Convert timestamps (proto uses u64 millis)
        let saved_at = chrono::DateTime::from_timestamp_millis(proto.created_at as i64)
            .unwrap_or_else(chrono::Utc::now);

        // Decrypt content
        let content = if let Some(ref proto_content) = proto.content {
            let decrypted_text = self.decrypt_content(proto_content)?;
            SavedContent::Text { content: decrypted_text }
        } else {
            SavedContent::Text {
                content: String::new(),
            }
        };

        // Decrypt source if present
        let (source_channel_id, source_message_id) = if let Some(ref proto_source) = proto.source {
            let decrypted_source = self.decrypt_source(proto_source)?;
            let channel_id: Option<ChannelId> = decrypted_source.channel_id.parse().ok();
            let message_id: Option<MessageId> = decrypted_source.message_id.parse().ok();
            (channel_id, message_id)
        } else {
            (None, None)
        };

        let domain_msg = SavedMessage {
            id,
            user_id,
            content,
            source_channel_id,
            source_message_id,
            note: None,
            tags: Vec::new(),
            saved_at,
        };

        Ok(domain_msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;
    use tempfile::tempdir;

    /// Create a test SavedMessageService with a proper temporary database.
    fn create_test_service() -> (SavedMessageService, tempfile::TempDir) {
        let temp_dir = tempdir().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("test.db");

        let db = Database::open(&db_path, "test-passphrase")
            .expect("Failed to open test database");

        // Create minimal schema
        {
            let conn = db.connection().expect("Failed to get connection");
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS settings (
                    key TEXT PRIMARY KEY,
                    value_json TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                "#,
            )
            .expect("Failed to create tables");
        }

        let db_arc = Arc::new(db);
        let settings_repo = Arc::new(SettingsRepository::new(Arc::clone(&db_arc)));

        let service = SavedMessageService::new(settings_repo);
        (service, temp_dir)
    }

    /// Helper to set up a logged-in user.
    fn set_logged_in_user(service: &SavedMessageService, user_id: &str) {
        service
            .settings_repo
            .set_string(keys::CURRENT_USER_ID, user_id)
            .expect("Failed to set user ID");
    }

    #[test]
    fn test_saved_message_error_display() {
        let err = SavedMessageError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = SavedMessageError::NotFound;
        assert!(err.to_string().contains("not found"));

        let err = SavedMessageError::InvalidData("test error".to_string());
        assert!(err.to_string().contains("invalid data"));
        assert!(err.to_string().contains("test error"));

        let network_err = SavedMessageError::Network(NetworkError::Timeout);
        assert!(network_err.to_string().contains("network error"));
    }

    #[test]
    fn test_saved_message_error_from_network() {
        let network_err = NetworkError::Unauthenticated;
        let svc_err: SavedMessageError = network_err.into();
        assert!(matches!(svc_err, SavedMessageError::Network(_)));
    }

    #[test]
    fn test_saved_message_service_construction() {
        // Verify the SavedMessageService struct type signatures compile correctly.
        fn _verify_types(settings_repo: Arc<SettingsRepository>) {
            let _service = SavedMessageService::new(settings_repo);
        }
    }

    #[test]
    fn test_current_user_id_not_logged_in() {
        let (service, _temp_dir) = create_test_service();

        // Should return error when not logged in
        let result = service.current_user_id();
        assert!(matches!(result, Err(SavedMessageError::NotLoggedIn)));
    }

    #[test]
    fn test_current_user_id_logged_in() {
        let (service, _temp_dir) = create_test_service();

        let user_id = "550e8400-e29b-41d4-a716-446655440000";
        set_logged_in_user(&service, user_id);

        // Should return the user ID
        let result = service.current_user_id();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), user_id);
    }

    #[test]
    fn test_proto_to_domain_saved_message() {
        let proto = ProtoSavedMessage {
            id: "550e8400-e29b-41d4-a716-446655440001".to_string(),
            user_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            content: None,
            created_at: 1234567890000,
            updated_at: 1234567890000,
            pinned: false,
            pinned_at: 0,
            source: None,
        };

        let domain = proto_to_domain_saved_message(&proto).unwrap();
        assert_eq!(domain.id.to_string(), proto.id);
        assert_eq!(domain.user_id.to_string(), proto.user_id);
        assert!(!domain.has_source());
        assert!(!domain.has_tags());
    }

    #[test]
    fn test_proto_to_domain_saved_message_invalid_id() {
        let proto = ProtoSavedMessage {
            id: "not-a-uuid".to_string(),
            user_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            content: None,
            created_at: 1234567890000,
            updated_at: 1234567890000,
            pinned: false,
            pinned_at: 0,
            source: None,
        };

        let result = proto_to_domain_saved_message(&proto);
        assert!(matches!(result, Err(SavedMessageError::InvalidData(_))));
    }

    #[test]
    fn test_proto_to_domain_saved_message_invalid_user_id() {
        let proto = ProtoSavedMessage {
            id: "550e8400-e29b-41d4-a716-446655440001".to_string(),
            user_id: "invalid-user-id".to_string(),
            content: None,
            created_at: 1234567890000,
            updated_at: 1234567890000,
            pinned: false,
            pinned_at: 0,
            source: None,
        };

        let result = proto_to_domain_saved_message(&proto);
        assert!(matches!(result, Err(SavedMessageError::InvalidData(_))));
    }

    // ========================================================================
    // Encryption Tests
    // ========================================================================

    /// Helper to set profile key for encryption tests.
    fn set_profile_key(service: &SavedMessageService) {
        // Generate a test profile key (32 bytes)
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(7);
        }
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(key);

        service
            .settings_repo
            .set_string(keys::PROFILE_KEY, &key_b64)
            .expect("Failed to set profile key");
    }

    #[test]
    fn test_encrypt_text_decrypt_content_roundtrip() {
        let (service, _temp_dir) = create_test_service();
        set_logged_in_user(&service, "550e8400-e29b-41d4-a716-446655440000");
        set_profile_key(&service);

        let text = "This is a saved message!";

        // Encrypt
        let encrypted = service.encrypt_text(text).expect("encrypt should succeed");

        // Decrypt
        let decrypted = service
            .decrypt_content(&encrypted)
            .expect("decrypt should succeed");

        assert_eq!(decrypted, text);
    }

    #[test]
    fn test_encrypt_source_decrypt_source_roundtrip() {
        let (service, _temp_dir) = create_test_service();
        set_logged_in_user(&service, "550e8400-e29b-41d4-a716-446655440000");
        set_profile_key(&service);

        let source = SavedMessageSourcePlain {
            channel_id: "550e8400-e29b-41d4-a716-446655440001".to_string(),
            message_id: "550e8400-e29b-41d4-a716-446655440002".to_string(),
            author_id: "550e8400-e29b-41d4-a716-446655440003".to_string(),
            timestamp: 1234567890000,
        };

        // Encrypt
        let encrypted = service
            .encrypt_source(&source)
            .expect("encrypt should succeed");

        // Decrypt
        let decrypted = service
            .decrypt_source(&encrypted)
            .expect("decrypt should succeed");

        assert_eq!(decrypted, source);
    }

    #[test]
    fn test_encryption_error_when_profile_key_not_set() {
        let (service, _temp_dir) = create_test_service();
        set_logged_in_user(&service, "550e8400-e29b-41d4-a716-446655440000");
        // Note: NOT setting profile key

        let result = service.encrypt_text("test");
        assert!(matches!(result, Err(SavedMessageError::NotLoggedIn)));
    }

    #[test]
    fn test_get_profile_key_invalid_length() {
        let (service, _temp_dir) = create_test_service();
        set_logged_in_user(&service, "550e8400-e29b-41d4-a716-446655440000");

        // Set a key with wrong length (16 bytes instead of 32)
        let short_key = [0u8; 16];
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(short_key);
        service
            .settings_repo
            .set_string(keys::PROFILE_KEY, &key_b64)
            .expect("Failed to set profile key");

        let result = service.encrypt_text("test");
        assert!(matches!(result, Err(SavedMessageError::InvalidData(_))));
    }
}

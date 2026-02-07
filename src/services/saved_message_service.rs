//! Saved message service.
//!
//! Orchestrates saved message operations for the personal vault.
//! Saved messages are stored in a special channel where channel_id == user_id.
//! Content is encrypted client-side with the user's profile key before sending.

use std::sync::Arc;

use base64::Engine;

use crate::crypto::{SavedMessageCrypto, SavedMessageSource};
use crate::domain::ids::MessageId;
use crate::error::{CryptoError, NetworkError};
use crate::network::grpc::MessageClient;
use crate::proto::chat_v1::{GetMessagesResponse, Message, MessageSortDirection};
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

/// Decrypted saved message with metadata.
#[derive(Debug, Clone)]
pub struct DecryptedSavedMessage {
    /// Message ID.
    pub id: MessageId,
    /// Decrypted text content.
    pub text: String,
    /// Optional source metadata (where the message came from).
    pub source: Option<SavedMessageSource>,
    /// Timestamp when saved (Unix ms).
    pub saved_at: u64,
    /// Whether message is pinned.
    pub pinned: bool,
}

/// Saved message service.
///
/// Orchestrates saved message operations for the personal vault.
/// Uses MessageClient to send/receive messages to the saved messages channel
/// (channel_id == user_id). Content is encrypted with profile key.
///
/// # Example
///
/// ```ignore
/// let service = SavedMessageService::new(settings_repo);
///
/// // Save a message
/// let saved = service.save_message(&mut client, "Important note", None).await?;
///
/// // List saved messages
/// let messages = service.list_messages(&mut client, 50, None).await?;
///
/// // Pin a message
/// service.pin_message(&mut client, &message_id).await?;
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
    /// This is also the saved messages channel ID.
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
    // Server Operations
    // ========================================================================

    /// Save a message to the personal vault.
    ///
    /// Encrypts the content with profile key and sends to saved messages channel.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `text` - The text content to save
    /// * `source` - Optional source info (where the message came from)
    pub async fn save_message(
        &self,
        client: &mut MessageClient,
        text: &str,
        source: Option<&SavedMessageSource>,
    ) -> Result<Message, SavedMessageError> {
        // Get user ID (which is also the channel ID)
        let channel_id = self.current_user_id()?;
        let profile_key = self.get_profile_key()?;

        // Encrypt content
        let content = SavedMessageCrypto::encrypt_text(text, &profile_key, source)?;

        // Send to saved messages channel
        let message = client
            .send_message(
                &channel_id,
                content,
                None,   // parent_id
                false,  // ephemeral
                None,   // ephemeral_ttl
                false,  // encrypt (already encrypted client-side)
                None,   // crypto_options
                vec![], // mentioned_integrations
                false,  // targets_ai_bot
                vec![], // mentioned_file_ids
            )
            .await?;

        Ok(message)
    }

    /// List saved messages with pagination.
    ///
    /// Fetches messages from saved messages channel.
    /// Note: Messages are still encrypted - use decrypt_message() to decrypt.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `limit` - Maximum number of messages to return
    /// * `page_token` - Optional pagination token
    pub async fn list_messages(
        &self,
        client: &mut MessageClient,
        limit: i32,
        page_token: Option<&str>,
    ) -> Result<GetMessagesResponse, SavedMessageError> {
        let channel_id = self.current_user_id()?;

        let response = client
            .get_messages(
                &channel_id,
                limit,
                None, // thread_root_id
                page_token,
                MessageSortDirection::SortNewestFirst,
                None, // reference_message_id
                None, // reference_time
            )
            .await?;

        Ok(response)
    }

    /// Decrypt a saved message.
    ///
    /// Decrypts the message content using the profile key.
    ///
    /// # Arguments
    ///
    /// * `message` - The encrypted message from the server
    pub fn decrypt_message(&self, message: &Message) -> Result<DecryptedSavedMessage, SavedMessageError> {
        let profile_key = self.get_profile_key()?;

        let content = message
            .content
            .as_ref()
            .ok_or_else(|| SavedMessageError::InvalidData("message has no content".to_string()))?;

        let (text, source) = SavedMessageCrypto::decrypt_content(content, &profile_key)?;

        let id: MessageId = message
            .id
            .parse()
            .map_err(|_| SavedMessageError::InvalidData(format!("invalid message ID: {}", message.id)))?;

        Ok(DecryptedSavedMessage {
            id,
            text,
            source,
            saved_at: message.timestamp,
            pinned: message.pinned,
        })
    }

    /// List and decrypt saved messages.
    ///
    /// Convenience method that fetches and decrypts messages in one call.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `limit` - Maximum number of messages to return
    /// * `page_token` - Optional pagination token
    pub async fn list_decrypted_messages(
        &self,
        client: &mut MessageClient,
        limit: i32,
        page_token: Option<&str>,
    ) -> Result<(Vec<DecryptedSavedMessage>, Option<String>), SavedMessageError> {
        let response = self.list_messages(client, limit, page_token).await?;

        let mut decrypted = Vec::with_capacity(response.messages.len());
        for msg in &response.messages {
            match self.decrypt_message(msg) {
                Ok(d) => decrypted.push(d),
                Err(e) => {
                    // Log error but continue with other messages
                    tracing::warn!("Failed to decrypt saved message {}: {}", msg.id, e);
                }
            }
        }

        let next_page_token = if response.next_page_token.is_empty() {
            None
        } else {
            Some(response.next_page_token)
        };

        Ok((decrypted, next_page_token))
    }

    /// Delete a saved message.
    ///
    /// Permanently removes a message from the personal vault.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `message_id` - ID of the message to delete
    pub async fn delete_message(
        &self,
        client: &mut MessageClient,
        message_id: &str,
    ) -> Result<(), SavedMessageError> {
        let channel_id = self.current_user_id()?;

        client.delete_message(message_id, &channel_id).await?;

        Ok(())
    }

    /// Pin a saved message.
    ///
    /// Pins a message for quick access.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `message_id` - ID of the message to pin
    pub async fn pin_message(
        &self,
        client: &mut MessageClient,
        message_id: &str,
    ) -> Result<Message, SavedMessageError> {
        let channel_id = self.current_user_id()?;

        let message = client.pin_message(message_id, &channel_id).await?;

        Ok(message)
    }

    /// Unpin a saved message.
    ///
    /// Removes the pin from a message.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `message_id` - ID of the message to unpin
    pub async fn unpin_message(
        &self,
        client: &mut MessageClient,
        message_id: &str,
    ) -> Result<(), SavedMessageError> {
        let channel_id = self.current_user_id()?;

        client.unpin_message(message_id, &channel_id).await?;

        Ok(())
    }

    /// List pinned saved messages.
    ///
    /// Returns only pinned messages from the saved messages channel.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `limit` - Maximum number of messages to return
    pub async fn list_pinned_messages(
        &self,
        client: &mut MessageClient,
        limit: i32,
    ) -> Result<Vec<Message>, SavedMessageError> {
        let channel_id = self.current_user_id()?;

        let response = client.list_pinned_messages(&channel_id, limit).await?;

        Ok(response.messages)
    }

    /// Search saved messages.
    ///
    /// Searches message content within the saved messages channel.
    /// Note: Search is performed server-side on encrypted content,
    /// which may have limitations depending on server implementation.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to message client
    /// * `query` - Search query
    /// * `limit` - Maximum results to return
    pub async fn search_messages(
        &self,
        client: &mut MessageClient,
        query: &str,
        limit: i32,
    ) -> Result<Vec<Message>, SavedMessageError> {
        let channel_id = self.current_user_id()?;

        let response = client
            .search(Some(&channel_id), query, limit, 0, None, None)
            .await?;

        Ok(response.results)
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
                    value TEXT NOT NULL,
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

        // Should return the user ID (which is also the channel ID)
        let result = service.current_user_id();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), user_id);
    }

    #[test]
    fn test_get_profile_key_not_set() {
        let (service, _temp_dir) = create_test_service();
        set_logged_in_user(&service, "550e8400-e29b-41d4-a716-446655440000");
        // Note: NOT setting profile key

        let result = service.get_profile_key();
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

        let result = service.get_profile_key();
        assert!(matches!(result, Err(SavedMessageError::InvalidData(_))));
    }

    #[test]
    fn test_get_profile_key_success() {
        let (service, _temp_dir) = create_test_service();
        set_logged_in_user(&service, "550e8400-e29b-41d4-a716-446655440000");
        set_profile_key(&service);

        let result = service.get_profile_key();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 32);
    }

    #[test]
    fn test_decrypt_message_no_content() {
        let (service, _temp_dir) = create_test_service();
        set_logged_in_user(&service, "550e8400-e29b-41d4-a716-446655440000");
        set_profile_key(&service);

        let message = Message {
            id: "msg-123".to_string(),
            channel_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            author_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            content: None, // No content
            timestamp: 1234567890000,
            ..Default::default()
        };

        let result = service.decrypt_message(&message);
        assert!(matches!(result, Err(SavedMessageError::InvalidData(_))));
    }

    #[test]
    fn test_decrypted_saved_message_struct() {
        let msg = DecryptedSavedMessage {
            id: "550e8400-e29b-41d4-a716-446655440001".parse().unwrap(),
            text: "Test message".to_string(),
            source: Some(SavedMessageSource {
                channel_id: "channel-123".to_string(),
                message_id: "msg-456".to_string(),
                author_id: "user-789".to_string(),
                timestamp: 1234567890000,
            }),
            saved_at: 1234567890000,
            pinned: false,
        };

        assert_eq!(msg.text, "Test message");
        assert!(msg.source.is_some());
        assert!(!msg.pinned);
    }
}

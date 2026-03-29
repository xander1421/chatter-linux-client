//! Search service.
//!
//! Global search combining local FTS5 and server-side search for unified message discovery.

use std::sync::Arc;

use chrono::Utc;

use crate::util::time::TimestampNanos;

use crate::domain::ids::{ChannelId, IntegrationId, MessageId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent, MixedContent, ReactionCount};
use crate::error::{NetworkError, StorageError};
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1;
use crate::services::current_user::{CurrentUserError, CurrentUserProvider};
use crate::storage::repositories::MessageRepository;
use crate::storage::repositories::search_index::{SearchIndex, SearchResult};
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Search service errors.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Query validation failed.
    #[error("invalid query: {0}")]
    InvalidQuery(String),

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

impl From<CurrentUserError> for SearchError {
    fn from(err: CurrentUserError) -> Self {
        match err {
            CurrentUserError::Storage(e) => SearchError::Storage(e),
            CurrentUserError::NotLoggedIn => SearchError::NotLoggedIn,
        }
    }
}

/// Combined search results from local and server sources.
#[derive(Debug, Clone)]
pub struct CombinedSearchResults {
    /// Results from local FTS5 index.
    pub local_results: Vec<SearchResult>,
    /// Full messages fetched from server.
    pub server_results: Vec<Message>,
    /// Count of local results.
    pub local_count: usize,
    /// Count of server results.
    pub server_count: usize,
}

impl CombinedSearchResults {
    /// Create empty combined search results.
    pub fn empty() -> Self {
        Self {
            local_results: Vec::new(),
            server_results: Vec::new(),
            local_count: 0,
            server_count: 0,
        }
    }

    /// Create combined search results with only local results.
    pub fn local_only(local_results: Vec<SearchResult>) -> Self {
        let local_count = local_results.len();
        Self {
            local_results,
            server_results: Vec::new(),
            local_count,
            server_count: 0,
        }
    }
}

/// Search service for message search.
///
/// Provides unified search interface combining local FTS5 index for offline search
/// and server API for global search across the user's channels.
pub struct SearchService {
    settings_repo: Arc<SettingsRepository>,
    search_index: Arc<SearchIndex>,
    message_repo: Arc<MessageRepository>,
}

impl SearchService {
    /// Create a new SearchService with dependencies.
    pub fn new(
        settings_repo: Arc<SettingsRepository>,
        search_index: Arc<SearchIndex>,
        message_repo: Arc<MessageRepository>,
    ) -> Self {
        Self {
            settings_repo,
            search_index,
            message_repo,
        }
    }

    // ==================== Local Search Methods ====================

    /// Search messages in local FTS5 index.
    ///
    /// Returns search results with message IDs and relevance scores.
    /// Does NOT require authentication - queries locally stored data.
    ///
    /// # Arguments
    ///
    /// * `query` - Search query (non-empty, whitespace is trimmed)
    /// * `channel_id` - Optional channel filter
    /// * `limit` - Maximum results to return
    pub fn search_local(
        &self,
        query: &str,
        channel_id: Option<&ChannelId>,
        limit: u32,
    ) -> Result<Vec<SearchResult>, SearchError> {
        // Validate query is non-empty
        let trimmed_query = query.trim();
        if trimmed_query.is_empty() {
            return Err(SearchError::InvalidQuery("query cannot be empty".to_string()));
        }

        // Delegate to SearchIndex
        let results = self.search_index.search(trimmed_query, channel_id, limit)?;

        Ok(results)
    }

    /// Search local index and fetch full messages.
    ///
    /// Calls search_local(), then fetches full messages from MessageRepository.
    /// Does NOT require authentication - queries locally stored data.
    ///
    /// # Arguments
    ///
    /// * `query` - Search query
    /// * `channel_id` - Optional channel filter
    /// * `limit` - Maximum results to return
    pub fn search_local_with_messages(
        &self,
        query: &str,
        channel_id: Option<&ChannelId>,
        limit: u32,
    ) -> Result<Vec<Message>, SearchError> {
        // Search for message IDs
        let search_results = self.search_local(query, channel_id, limit)?;

        // Fetch full messages
        let mut messages = Vec::with_capacity(search_results.len());
        for result in &search_results {
            if let Ok(Some(message)) = self.message_repo.get(&result.message_id) {
                messages.push(message);
            }
        }

        Ok(messages)
    }

    /// Reindex all messages in the local FTS5 index.
    ///
    /// Delegates to SearchIndex::reindex_all() for repair/migration.
    /// Returns the number of messages indexed.
    pub fn reindex_local(&self) -> Result<u64, SearchError> {
        let count = self.search_index.reindex_all()?;
        Ok(count)
    }

    // ==================== Server Search Methods ====================

    /// Search messages on the server.
    ///
    /// Requires authentication - queries server for results.
    /// Results are NOT stored locally (discovery pattern, not ownership).
    ///
    /// # Arguments
    ///
    /// * `client` - Message client for server RPC
    /// * `channel_id` - Optional channel filter (None for global search)
    /// * `query` - Search query
    /// * `limit` - Maximum results to return
    /// * `offset` - Pagination offset
    /// * `from` - Optional start timestamp (Unix milliseconds)
    /// * `to` - Optional end timestamp (Unix milliseconds)
    #[allow(clippy::too_many_arguments)]
    pub async fn search_server(
        &self,
        client: &mut MessageClient,
        channel_id: Option<&ChannelId>,
        query: &str,
        limit: u32,
        offset: u32,
        from: Option<u64>,
        to: Option<u64>,
    ) -> Result<Vec<Message>, SearchError> {
        // Require authentication for server search
        let _ = self.settings_repo.current_user_id()?;

        // Validate query
        let trimmed_query = query.trim();
        if trimmed_query.is_empty() {
            return Err(SearchError::InvalidQuery("query cannot be empty".to_string()));
        }

        // Call server
        let response = client
            .search(
                channel_id.map(|id| id.to_string()).as_deref(),
                trimmed_query,
                limit as i32,
                offset as i32,
                from,
                to,
            )
            .await?;

        // Convert proto messages to domain messages
        let mut messages = Vec::with_capacity(response.results.len());
        for proto in &response.results {
            match proto_to_domain_message(proto) {
                Ok(message) => messages.push(message),
                Err(_) => continue, // Skip invalid messages
            }
        }

        // Do NOT store locally - search results are discovery, not ownership
        Ok(messages)
    }

    /// Combined search from local and server sources.
    ///
    /// First tries local search (does not require auth).
    /// If authenticated, also fetches from server with same query.
    /// If server call fails (not logged in or network error), returns local results only.
    ///
    /// # Arguments
    ///
    /// * `client` - Message client for server RPC
    /// * `query` - Search query
    /// * `channel_id` - Optional channel filter
    /// * `limit` - Maximum results to return from each source
    pub async fn search_combined(
        &self,
        client: &mut MessageClient,
        query: &str,
        channel_id: Option<&ChannelId>,
        limit: u32,
    ) -> Result<CombinedSearchResults, SearchError> {
        // Validate query first
        let trimmed_query = query.trim();
        if trimmed_query.is_empty() {
            return Err(SearchError::InvalidQuery("query cannot be empty".to_string()));
        }

        // Always try local search (no auth required)
        let local_results = match self.search_local(trimmed_query, channel_id, limit) {
            Ok(results) => results,
            Err(SearchError::InvalidQuery(msg)) => return Err(SearchError::InvalidQuery(msg)),
            Err(_) => Vec::new(), // Storage errors - return empty local results
        };

        // Try server search if authenticated
        let server_results = match self
            .search_server(client, channel_id, trimmed_query, limit, 0, None, None)
            .await
        {
            Ok(results) => results,
            Err(SearchError::NotLoggedIn) => Vec::new(), // Not authenticated - skip server
            Err(SearchError::Network(_)) => Vec::new(),  // Network error - skip server
            Err(e) => return Err(e),                     // Other errors propagate
        };

        let local_count = local_results.len();
        let server_count = server_results.len();

        Ok(CombinedSearchResults {
            local_results,
            server_results,
            local_count,
            server_count,
        })
    }
}

/// Convert a proto Message to a domain Message.
///
/// Helper function for server search results.
fn proto_to_domain_message(proto: &chat_v1::Message) -> Result<Message, SearchError> {
    // Parse message ID
    let id: MessageId = proto
        .id
        .parse()
        .map_err(|_| SearchError::InvalidQuery("invalid message ID".to_string()))?;

    // Parse channel ID
    let channel_id: ChannelId = proto
        .channel_id
        .parse()
        .map_err(|_| SearchError::InvalidQuery("invalid channel ID".to_string()))?;

    // Parse sender ID (optional for sealed sender)
    let sender_id: Option<UserId> = if proto.author_id.is_empty() {
        None
    } else {
        proto.author_id.parse().ok()
    };

    // Parse thread ID (parent_id in proto)
    let thread_id: Option<MessageId> = if proto.parent_id.is_empty() {
        None
    } else {
        proto.parent_id.parse().ok()
    };

    // Parse mentioned integrations
    let mentioned_integrations: Vec<IntegrationId> = proto
        .mentioned_integrations
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    // Convert proto content to domain content
    let content = proto_content_to_domain(proto.content.as_ref())?;

    // Convert reaction counts
    let reaction_counts: Vec<ReactionCount> = proto
        .reaction_counts
        .iter()
        .map(|rc| ReactionCount::new(rc.emoji.clone(), rc.count as u32))
        .collect();

    // Convert timestamp (proto uses nanos)
    let created_at = TimestampNanos::from_nanos(proto.timestamp).to_datetime().unwrap_or_else(Utc::now);
    let updated_at = if proto.updated_at > 0 {
        TimestampNanos::from_nanos(proto.updated_at).to_datetime().unwrap_or(created_at)
    } else {
        created_at
    };
    let edited_at = if proto.edited && proto.edited_at > 0 {
        TimestampNanos::from_nanos(proto.edited_at).to_datetime()
    } else {
        None
    };

    Ok(Message {
        id,
        conversation_id: channel_id,
        conversation_type: ConversationType::Channel,
        sender_id,
        device_id: None,
        content,
        encrypted_content: None,
        thread_id,
        reply_to: None, // Not exposed in proto yet
        attachments: Vec::new(), // Handled separately via mentioned_file_ids
        is_pinned: proto.pinned,
        is_edited: proto.edited,
        is_deleted: false,
        is_read: false,
        sent_at: proto.timestamp,
        created_at,
        updated_at,
        edited_at,
        reaction_counts,
        current_user_reactions: proto.current_user_reactions.clone(),
        mentioned_integrations,
        targets_ai_bot: proto.targets_ai_bot,
        delivery_status: String::new(),
    })
}

/// Convert proto content to domain MessageContent.
fn proto_content_to_domain(
    proto_content: Option<&chat_v1::message::Content>,
) -> Result<MessageContent, SearchError> {
    let content = proto_content.ok_or_else(|| {
        SearchError::InvalidQuery("message has no content".to_string())
    })?;

    match &content.r#type {
        Some(chat_v1::message::content::Type::Text(text)) => {
            Ok(MessageContent::Text(text.clone()))
        }
        Some(chat_v1::message::content::Type::Encrypted(_)) => {
            // Encrypted content - store as placeholder text for now
            Ok(MessageContent::Text("[Encrypted message]".to_string()))
        }
        Some(chat_v1::message::content::Type::File(file_meta)) => {
            // File-only message - use MixedContent with no text
            let attachment_id = file_meta
                .id
                .parse()
                .map_err(|_| SearchError::InvalidQuery("invalid file ID".to_string()))?;
            Ok(MessageContent::Mixed(MixedContent::attachments_only(vec![attachment_id])))
        }
        None => {
            // No content type - default to empty text
            Ok(MessageContent::Text(String::new()))
        }
        _ => {
            // Other types (reactions, system, call, AI response) - handle as text placeholder
            Ok(MessageContent::Text("[Special message]".to_string()))
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_error_display() {
        let err = SearchError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = SearchError::InvalidQuery("empty".to_string());
        assert!(err.to_string().contains("empty"));

        let network_err = NetworkError::Timeout;
        let err = SearchError::Network(network_err);
        assert!(err.to_string().contains("network error"));

        let storage_err = StorageError::NotFound("test".to_string());
        let err = SearchError::Storage(storage_err);
        assert!(err.to_string().contains("storage error"));
    }

    #[test]
    fn test_combined_results_empty() {
        let results = CombinedSearchResults::empty();
        assert!(results.local_results.is_empty());
        assert!(results.server_results.is_empty());
        assert_eq!(results.local_count, 0);
        assert_eq!(results.server_count, 0);
    }

    #[test]
    fn test_combined_results_local_only() {
        let local_results = vec![
            SearchResult {
                message_id: MessageId::new(),
                channel_id: ChannelId::new(),
                score: -1.5,
            },
            SearchResult {
                message_id: MessageId::new(),
                channel_id: ChannelId::new(),
                score: -2.0,
            },
        ];
        let results = CombinedSearchResults::local_only(local_results);

        assert_eq!(results.local_count, 2);
        assert_eq!(results.server_count, 0);
        assert_eq!(results.local_results.len(), 2);
        assert!(results.server_results.is_empty());
    }

    #[test]
    fn test_combined_results_counts_match_vec_lengths() {
        let local_results = vec![
            SearchResult {
                message_id: MessageId::new(),
                channel_id: ChannelId::new(),
                score: -1.0,
            },
        ];
        let server_results = vec![
            Message::new_text(ChannelId::new(), None, "test".to_string(), 0),
            Message::new_text(ChannelId::new(), None, "test2".to_string(), 0),
        ];

        let local_count = local_results.len();
        let server_count = server_results.len();

        let combined = CombinedSearchResults {
            local_results,
            server_results,
            local_count,
            server_count,
        };

        assert_eq!(combined.local_count, combined.local_results.len());
        assert_eq!(combined.server_count, combined.server_results.len());
    }

    #[test]
    fn test_nanos_to_datetime() {
        let nanos = 1706140800_000_000_000u64; // 2024-01-25 00:00:00 UTC in nanoseconds
        let dt = TimestampNanos::from_nanos(nanos).to_datetime().unwrap();
        assert_eq!(dt.timestamp_nanos_opt().unwrap(), nanos as i64);
    }

    #[test]
    fn test_proto_content_to_domain_text() {
        use chat_v1::message::content::Type;
        use chat_v1::message::Content;

        let proto_content = Content {
            r#type: Some(Type::Text("Hello world".to_string())),
        };

        let result = proto_content_to_domain(Some(&proto_content)).unwrap();
        match result {
            MessageContent::Text(text) => assert_eq!(text, "Hello world"),
            _ => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_proto_content_to_domain_none() {
        let result = proto_content_to_domain(None);
        assert!(result.is_err());
    }

    #[test]
    fn test_proto_content_to_domain_encrypted() {
        use chat_v1::message::content::Type;
        use chat_v1::message::Content;

        let proto_content = Content {
            r#type: Some(Type::Encrypted(chat_v1::EncryptedData {
                version: 1,
                nonce: vec![],
                ciphertext: vec![1, 2, 3],
                auth_tag: vec![],
                associated_data: vec![],
                signature: vec![],
                device_id: String::new(),
            })),
        };

        let result = proto_content_to_domain(Some(&proto_content)).unwrap();
        match result {
            MessageContent::Text(text) => assert_eq!(text, "[Encrypted message]"),
            _ => panic!("Expected Text variant with placeholder"),
        }
    }

    #[test]
    fn test_search_error_from_network() {
        let network_err = NetworkError::Unauthenticated;
        let search_err: SearchError = network_err.into();
        assert!(matches!(search_err, SearchError::Network(_)));
        assert!(search_err.to_string().contains("network error"));
    }

    #[test]
    fn test_search_error_from_storage() {
        let storage_err = StorageError::InvalidData("corrupted".to_string());
        let search_err: SearchError = storage_err.into();
        assert!(matches!(search_err, SearchError::Storage(_)));
        assert!(search_err.to_string().contains("storage error"));
    }
}

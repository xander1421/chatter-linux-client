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
    /// The server half of the search failed, and this is why.
    ///
    /// INFRA-3: this used to not exist. `search_combined` discarded every
    /// `SearchError::Network(_)` into an empty `server_results`, so a server
    /// that answered "your request is invalid" and a server that answered
    /// "nothing matched" produced byte-identical results and the caller had no
    /// way to tell them apart. `None` means the server half actually ran (or
    /// was legitimately skipped because the user is not logged in — see
    /// `server_skipped_not_logged_in`); `Some` means it failed and the results
    /// below are local-only.
    pub server_error: Option<String>,
    /// The server half was skipped because no one is logged in.
    ///
    /// Distinct from `server_error`: not a failure, just not applicable. Local
    /// search does not require auth, so this is the ordinary offline case.
    pub server_skipped_not_logged_in: bool,
}

impl CombinedSearchResults {
    /// Create empty combined search results.
    pub fn empty() -> Self {
        Self {
            local_results: Vec::new(),
            server_results: Vec::new(),
            local_count: 0,
            server_count: 0,
            server_error: None,
            server_skipped_not_logged_in: false,
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
            server_error: None,
            server_skipped_not_logged_in: true,
        }
    }

    /// True when the results are complete: the server half ran and succeeded,
    /// or was legitimately skipped. False means the caller is looking at a
    /// partial answer and must say so rather than render it as "no results".
    pub fn is_complete(&self) -> bool {
        self.server_error.is_none()
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

        // Try server search if authenticated.
        //
        // INFRA-3: every `SearchError::Network(_)` used to be flattened into an
        // empty Vec, so "the server rejected this request" was reported to the
        // caller as "the server found nothing". Two cases are now separated:
        //
        //   * The server said the request itself is wrong (InvalidArgument ->
        //     NetworkError::InvalidRequest, the exact code the tightened
        //     server-side search rule returns). That is a bug in the caller,
        //     never a transient condition, and retrying cannot help — it
        //     propagates.
        //   * The server could not be reached (transport, timeout,
        //     unavailable, rate limit, ...). Local results are still worth
        //     returning, but the caller MUST be able to tell it is looking at a
        //     partial answer, so the reason travels with them.
        //
        // Not being logged in is neither: local search needs no auth, so it is
        // recorded as a skip.
        let mut server_error: Option<String> = None;
        let mut server_skipped_not_logged_in = false;
        let server_results = match self
            .search_server(client, channel_id, trimmed_query, limit, 0, None, None)
            .await
        {
            Ok(results) => results,
            Err(e) => match classify_server_half_error(e) {
                ServerHalfOutcome::Skipped => {
                    server_skipped_not_logged_in = true;
                    Vec::new()
                }
                ServerHalfOutcome::Degraded(reason) => {
                    tracing::warn!(
                        error = %reason,
                        "search: server half failed; returning local results only"
                    );
                    server_error = Some(reason);
                    Vec::new()
                }
                ServerHalfOutcome::Fatal(e) => return Err(e),
            },
        };

        let local_count = local_results.len();
        let server_count = server_results.len();

        Ok(CombinedSearchResults {
            local_results,
            server_results,
            local_count,
            server_count,
            server_error,
            server_skipped_not_logged_in,
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
        conversation_type: ConversationType::AnonGroup,
        sender_id,
        device_id: None,
        content,
        encrypted_content: None,
        thread_id,
        reply_to: None, // Not exposed in proto yet
        reply_to_author_id: None,
        reply_to_text: None,
        reply_to_thumbnail: None,
        reply_to_thumbnail_mime: None,
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
        Some(chat_v1::message::content::Type::Mixed(mixed)) => {
            let attachment_ids: Vec<crate::domain::AttachmentId> = mixed
                .files
                .iter()
                .filter_map(|f| f.id.parse().ok())
                .collect();
            let text = mixed.text.clone();
            let content = match (text.is_empty(), attachment_ids.is_empty()) {
                (false, false) => MixedContent::new(text, attachment_ids),
                (false, true) => MixedContent::text_only(text),
                (true, false) => MixedContent::attachments_only(attachment_ids),
                (true, true) => MixedContent::text_only(String::new()),
            };
            Ok(MessageContent::Mixed(content))
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
                conversation_id: ChannelId::new(),
                score: -1.5,
            },
            SearchResult {
                message_id: MessageId::new(),
                conversation_id: ChannelId::new(),
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
                conversation_id: ChannelId::new(),
                score: -1.0,
            },
        ];
        let server_results = vec![
            Message::new_text(ChannelId::new(), ConversationType::Channel, None, "test".to_string(), 0),
            Message::new_text(ChannelId::new(), ConversationType::Channel, None, "test2".to_string(), 0),
        ];

        let local_count = local_results.len();
        let server_count = server_results.len();

        let combined = CombinedSearchResults {
            local_results,
            server_results,
            local_count,
            server_count,
            server_error: None,
            server_skipped_not_logged_in: false,
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
                ciphertext: vec![1, 2, 3],
                associated_data: b"sender_key".to_vec(),
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

/// What the server half of a combined search did when it did not return
/// results. Split out of `search_combined` so it can be tested without a live
/// `MessageClient` — the original bug was entirely in this decision.
#[derive(Debug)]
pub(crate) enum ServerHalfOutcome {
    /// Not applicable: nobody is logged in. Local search needs no auth, so this
    /// is the ordinary offline case, not a failure.
    Skipped,
    /// The server half failed but local results are still worth returning. The
    /// caller MUST be able to say so — this reason travels with the results.
    Degraded(String),
    /// Not recoverable and not partial: the caller asked for something invalid,
    /// or something went wrong that is not a network condition at all.
    Fatal(SearchError),
}

/// Decide what a failed server search means (INFRA-3).
///
/// `search_combined` used to flatten EVERY `SearchError::Network(_)` into an
/// empty `server_results`, so "the server rejected your request" and "the
/// server found nothing" were byte-identical to the caller.
///
/// `NetworkError::InvalidRequest` is the mapping of gRPC `InvalidArgument`
/// (`error/network_error.rs`), which is exactly what the tightened server-side
/// search rule returns. That is a caller bug, never transient, and retrying
/// cannot help — it must not be silently swallowed.
pub(crate) fn classify_server_half_error(err: SearchError) -> ServerHalfOutcome {
    match err {
        SearchError::NotLoggedIn => ServerHalfOutcome::Skipped,
        // The transport-level shape of the same "not logged in".
        SearchError::Network(NetworkError::Unauthenticated) => ServerHalfOutcome::Skipped,
        e @ SearchError::Network(NetworkError::InvalidRequest(_)) => ServerHalfOutcome::Fatal(e),
        SearchError::Network(e) => ServerHalfOutcome::Degraded(e.to_string()),
        e => ServerHalfOutcome::Fatal(e),
    }
}

#[cfg(test)]
mod combined_search_error_tests {
    use super::*;

    /// INFRA-3 regression. Before the fix every one of these mapped to
    /// "skip the server half, return an empty Vec", so the caller could not
    /// tell a rejected request from an empty result set.
    #[test]
    fn an_invalid_request_from_the_server_is_not_swallowed() {
        let outcome = classify_server_half_error(SearchError::Network(
            NetworkError::InvalidRequest("query is not valid".to_string()),
        ));
        assert!(
            matches!(outcome, ServerHalfOutcome::Fatal(_)),
            "InvalidArgument from the server must propagate, not become an \
             empty result set — got {outcome:?}"
        );
    }

    #[test]
    fn an_unreachable_server_degrades_with_a_reason() {
        for e in [
            NetworkError::Unavailable,
            NetworkError::Timeout,
            NetworkError::ConnectionFailed("dial".to_string()),
            NetworkError::RateLimited,
            NetworkError::ServerError("boom".to_string()),
        ] {
            let expected = e.to_string();
            match classify_server_half_error(SearchError::Network(e)) {
                ServerHalfOutcome::Degraded(reason) => assert_eq!(reason, expected),
                other => panic!(
                    "a transient network failure must degrade WITH a reason, so the \
                     caller can say the answer is partial — got {other:?}"
                ),
            }
        }
    }

    #[test]
    fn not_logged_in_is_a_skip_not_a_failure() {
        assert!(matches!(
            classify_server_half_error(SearchError::NotLoggedIn),
            ServerHalfOutcome::Skipped
        ));
        assert!(matches!(
            classify_server_half_error(SearchError::Network(NetworkError::Unauthenticated)),
            ServerHalfOutcome::Skipped
        ));
    }

    /// `is_complete()` is the one call a caller needs to know whether it may
    /// render the results as "no results".
    #[test]
    fn is_complete_distinguishes_partial_answers() {
        let mut r = CombinedSearchResults::empty();
        assert!(r.is_complete(), "an empty-but-successful search is complete");

        r.server_error = Some("server unavailable".to_string());
        assert!(
            !r.is_complete(),
            "results carrying a server_error are a PARTIAL answer and must not \
             be rendered as 'no results'"
        );
    }
}

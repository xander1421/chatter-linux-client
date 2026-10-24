use super::*;

impl MessageService {
    /// Keep only history rows that are not already visible in the conversation.
    /// A fetched page can overlap the current model after a live delivery.
    pub fn unseen_history_rows<T>(
        rows: impl IntoIterator<Item = T>,
        visible_ids: impl IntoIterator<Item = String>,
        row_id: impl Fn(&T) -> &str,
    ) -> Vec<T> {
        let visible: HashSet<String> = visible_ids.into_iter().collect();
        rows.into_iter()
            .filter(|row| !visible.contains(row_id(row)))
            .collect()
    }

    /// Create a new MessageService with dependencies.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        database: Arc<Database>,
        settings_repo: Arc<SettingsRepository>,
        message_repo: Arc<MessageRepository>,
        channel_repo: Arc<ChannelRepository>,
        attachment_repo: Arc<AttachmentRepository>,
        user_repo: Arc<UserRepository>,
        signal_service: Arc<SignalService>,
        group_mailbox_manager: Arc<GroupMailboxManager>,
        pending_dms: Arc<PendingDmRepository>,
        search_index: Arc<SearchIndex>,
        token_manager: Arc<TokenManager>,
        event_bus: crate::events::EventBus,
        ephemeral_service: Arc<EphemeralService>,
        mailbox_manager: Arc<MailboxManager>,
    ) -> Self {
        let solicited_resends = SolicitedResends::load(settings_repo.clone());
        Self {
            grpc_pool,
            database,
            settings_repo,
            message_repo,
            channel_repo,
            attachment_repo,
            user_repo,
            signal_service,
            group_mailbox_manager,
            pending_dms,
            search_index,
            token_manager,
            event_bus,
            ephemeral_service,
            mailbox_manager,
            shield_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_group_decrypts: Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            sender_key_requests: Arc::new(
                parking_lot::Mutex::new(std::collections::HashMap::new()),
            ),
            channel_decrypt_failures: Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            group_retry_requests: Arc::new(parking_lot::Mutex::new(
                std::collections::HashSet::new(),
            )),
            solicited_resends,
            saved_sync_gate: Arc::new(tokio::sync::Mutex::new(())),
            own_devices: Arc::new(tokio::sync::Mutex::new(None)),
            unreached_peer_devices: Arc::new(parking_lot::Mutex::new(
                std::collections::HashSet::new(),
            )),
            retired_peer_devices: Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            eviction_resends: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            channel_state_sync: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    /// Get database reference (for internal DM operations).
    pub fn database(&self) -> Arc<Database> {
        self.database.clone()
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Internal client helpers
    // ─────────────────────────────────────────────────────────────────────────────

    pub(crate) fn message_client(&self) -> MessageClient {
        self.grpc_pool.message_client()
    }

    pub(crate) fn auth_client(&self) -> AuthClient {
        self.grpc_pool.auth_client()
    }

    pub(crate) fn channel_client(&self) -> ChannelClient {
        self.grpc_pool.channel_client()
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Local repository access methods
    // ─────────────────────────────────────────────────────────────────────────────

    /// Get messages for a channel from local storage.
    /// See `UnifiedMessageRepository::latest_thread_reply`.
    pub fn latest_thread_reply(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Option<Message>, MessageError> {
        self.message_repo
            .latest_thread_reply(channel_id, conversation_type)
            .map_err(MessageError::from)
    }

    pub fn get_messages_local(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, MessageError> {
        let messages =
            self.message_repo
                .list_for_channel(channel_id, conversation_type, before, limit)?;
        Ok(self.resolve_stored_quotes(messages))
    }

    /// Re-resolve quote references at load time, including parents received after
    /// the reply. Persist only the resolved reference; retain the quoted snapshot.
    fn resolve_stored_quotes(&self, mut messages: Vec<Message>) -> Vec<Message> {
        for message in &mut messages {
            if let Ok(Some(parent)) = self.message_repo.resolve_reply_parent(message) {
                if message.reply_to != Some(parent) {
                    message.reply_to = Some(parent);
                    if let Err(error) = self.message_repo.update_reply_parent(&message.id, &parent)
                    {
                        tracing::warn!(%error, "Could not persist resolved quote");
                    }
                }
            }
        }
        messages
    }

    /// Get messages for a channel in chronological order (oldest first).
    ///
    /// Returns messages ready for chat display — no reversal needed by caller.
    pub fn get_messages_local_asc(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, MessageError> {
        let messages =
            self.message_repo
                .list_for_channel_asc(channel_id, conversation_type, before, limit)?;
        Ok(self.resolve_stored_quotes(messages))
    }

    /// One page of history older than `before`, oldest first. Empty when
    /// there is nothing older.
    pub fn get_older_messages_local_asc(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        before: &crate::domain::ids::MessageId,
        page_size: u32,
    ) -> Result<Vec<Message>, MessageError> {
        let page = self.message_repo.list_for_channel_asc(
            channel_id,
            conversation_type,
            Some(before),
            page_size,
        )?;
        Ok(self.resolve_stored_quotes(page))
    }

    /// The conversation's rows from `target` to the newest, oldest first —
    /// the list a jump to a message older than what is loaded installs (a
    /// search hit, a quote, a pin). Grown like a conversation opening on its
    /// first unread ([`crate::domain::message::newest_page_reaching`]), then
    /// cut so `target` is the first row: the list lands on row 0 exactly,
    /// where a seek into a virtualized list only lands on an estimate that
    /// keeps moving as rows materialize. Older rows page in above it.
    ///
    /// Empty when `target` is not a message of this conversation's main list
    /// — never the whole history for a target that is not in it.
    pub fn get_messages_local_reaching(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        target: &crate::domain::ids::MessageId,
        page_size: u32,
    ) -> Result<Vec<Message>, MessageError> {
        let Some(target) = self.message_repo.get(target)?.filter(|m| {
            m.conversation_id == *channel_id
                && m.conversation_type == *conversation_type
                && m.thread_id.is_none()
        }) else {
            return Ok(Vec::new());
        };
        let page = crate::domain::message::newest_page_reaching(
            Some(target.sent_at),
            page_size,
            |limit| {
                self.message_repo
                    .list_for_channel_asc(channel_id, conversation_type, None, limit)
            },
        )?;
        let page = page.into_iter().skip_while(|m| m.id != target.id).collect();
        Ok(self.resolve_stored_quotes(page))
    }

    /// Reply count per thread root for a conversation (drives the "N replies"
    /// indicator on root bubbles).
    pub fn thread_reply_counts(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<std::collections::HashMap<crate::domain::ids::MessageId, u32>, MessageError> {
        self.message_repo
            .reply_counts_for_channel(channel_id, conversation_type)
            .map_err(MessageError::from)
    }

    /// Get sent_at timestamps of messages from a specific peer in a conversation.
    ///
    /// Used for sending read receipts — returns the peer's message timestamps
    /// so the caller can acknowledge them without filtering in the UI layer.
    pub fn get_peer_message_timestamps(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        peer_id: &UserId,
        limit: u32,
    ) -> Result<Vec<u64>, MessageError> {
        self.message_repo
            .get_timestamps_from_sender(channel_id, conversation_type, peer_id, limit)
            .map_err(MessageError::from)
    }

    /// Get a message by sender ID and sent timestamp (Signal-style lookup).
    pub fn get_by_sender_timestamp(
        &self,
        sender_id: &UserId,
        sent_at: u64,
    ) -> Result<Option<Message>, MessageError> {
        self.message_repo
            .get_by_sender_timestamp(sender_id, sent_at)
            .map_err(MessageError::from)
    }

    /// Get a message by ID from local storage.
    pub fn get_message_local(
        &self,
        message_id: &crate::domain::ids::MessageId,
    ) -> Result<Option<Message>, MessageError> {
        Ok(self
            .resolve_stored_quotes(self.message_repo.get(message_id)?.into_iter().collect())
            .pop())
    }

    pub fn get_messages_by_ids_local(
        &self,
        ids: &[crate::domain::ids::MessageId],
    ) -> Result<Vec<Message>, MessageError> {
        self.message_repo
            .get_many(ids)
            .map(|messages| self.resolve_stored_quotes(messages))
            .map_err(MessageError::from)
    }

    /// Whether the parent message a reply quotes is missing or deleted.
    ///
    /// Signal-Desktop parity: a reply keeps its quoted snapshot even after the
    /// original is gone, but renders an "Original message not found" warning.
    /// Returns `true` when the parent row is absent from local storage or is a
    /// soft-delete tombstone (`is_deleted == true`). On a storage error we fall
    /// back to `false` so transient failures never produce a spurious warning.
    pub fn reply_parent_missing(&self, parent: &crate::domain::ids::MessageId) -> bool {
        match self.message_repo.get(parent) {
            Ok(Some(msg)) => msg.is_deleted,
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// Search messages using full-text search index.
    ///
    /// Returns search results with message IDs and channel IDs.
    /// Use `get_message_local` to fetch full message details.
    pub fn search_local(
        &self,
        query: &str,
        channel_id: Option<&ChannelId>,
        limit: u32,
    ) -> Result<Vec<SearchResult>, MessageError> {
        self.search_index
            .search(query, channel_id, limit)
            .map_err(MessageError::from)
    }

    pub fn search_local_scoped(
        &self,
        query: &str,
        sender: Option<&str>,
        conversation: Option<&str>,
        since: Option<i64>,
        until: Option<i64>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchResult>, MessageError> {
        self.search_index
            .search_scoped(query, sender, conversation, since, until, limit, offset)
            .map_err(MessageError::from)
    }

    /// Get thread replies from local storage.
    pub fn list_for_thread(
        &self,
        thread_root_id: &crate::domain::ids::MessageId,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<Message>, MessageError> {
        self.message_repo
            .list_for_thread(thread_root_id, before, limit)
            .map(|messages| self.resolve_stored_quotes(messages))
            .map_err(MessageError::from)
    }

    /// Number of replies in a thread (local storage).
    pub fn count_for_thread(
        &self,
        thread_root_id: &crate::domain::ids::MessageId,
    ) -> Result<u32, MessageError> {
        self.message_repo
            .count_for_thread(thread_root_id)
            .map_err(MessageError::from)
    }

    /// Update a message in local storage.
    pub fn update_message(&self, message: &Message) -> Result<(), MessageError> {
        self.message_repo
            .update(message)
            .map_err(MessageError::from)
    }

    /// Get pinned messages for a channel from local storage.
    pub fn get_pinned_messages_local(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<Vec<Message>, MessageError> {
        self.message_repo
            .list_pinned_all(channel_id, conversation_type)
            .map(|messages| self.resolve_stored_quotes(messages))
            .map_err(MessageError::from)
    }
}

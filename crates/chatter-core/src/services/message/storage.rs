use super::*;

impl MessageService {
    /// Update delivery status for a message by sent_at timestamp.
    ///
    /// Used when receipts (delivered/read) are received.
    pub fn update_delivery_status(
        &self,
        channel_id: &ChannelId,
        conversation_type: &ConversationType,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, MessageError> {
        self.message_repo
            .update_delivery_status(channel_id, conversation_type, sent_at, status)
            .map_err(MessageError::from)
    }

    /// Update delivery status by conversation + timestamp (scoped to one conversation).
    pub fn update_delivery_status_by_timestamp(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, MessageError> {
        self.message_repo
            .update_delivery_status_by_timestamp(
                conversation_id,
                conversation_type,
                sent_at,
                status,
            )
            .map_err(MessageError::from)
    }

    /// Update delivery status by timestamp with automatic conversation lookup.
    ///
    /// For callers that lack conversation context (e.g. cross-channel receipts).
    /// Looks up matching messages first, then updates each by primary key.
    pub fn update_delivery_status_by_timestamp_with_lookup(
        &self,
        sent_at: u64,
        status: &str,
    ) -> Result<bool, MessageError> {
        self.message_repo
            .update_delivery_status_by_timestamp_with_lookup(sent_at, status)
            .map_err(MessageError::from)
    }

    /// Set the stored delivery status of our friend-DM messages in
    /// `conversation_id` sent at `timestamps` — how a background send (the
    /// post-bootstrap flush) records `"sent"` or `"failed"` for bubbles it
    /// stored as `"sending"`.
    pub fn set_friend_dm_delivery_status(
        &self,
        conversation_id: &ChannelId,
        timestamps: &[u64],
        status: &str,
    ) -> Result<usize, MessageError> {
        self.message_repo
            .update_delivery_status_by_timestamps(
                conversation_id,
                &ConversationType::ServerDm,
                timestamps,
                status,
            )
            .map_err(MessageError::from)
    }

    /// Delete a message from local storage.
    pub fn delete_message_local(
        &self,
        message_id: &crate::domain::ids::MessageId,
    ) -> Result<(), MessageError> {
        self.message_repo
            .delete(message_id)
            .map_err(MessageError::from)
    }

    /// Store a message in local storage (async).
    pub async fn store_message(&self, message: &Message) -> Result<(), MessageError> {
        self.message_repo
            .insert(message)
            .map_err(MessageError::from)
    }

    /// Store a message in local storage (sync).
    pub fn store_message_local(&self, message: &Message) -> Result<(), MessageError> {
        self.message_repo
            .insert(message)
            .map_err(MessageError::from)
    }

    /// [`store_message_local`](Self::store_message_local) for a record whose
    /// id is its dedup key: `false` when it was already stored.
    pub fn store_message_local_once(&self, message: &Message) -> Result<bool, MessageError> {
        self.message_repo
            .insert_or_ignore(message)
            .map_err(MessageError::from)
    }

    /// Get the current user's ID.
    pub fn current_user_id(&self) -> Result<UserId, MessageError> {
        use crate::services::current_user::CurrentUserProvider;
        self.settings_repo
            .current_user_id()
            .map_err(|_| MessageError::NotLoggedIn)
    }

    /// This device's id; 0 when unknown. Only labels a sync copy, so its
    /// absence must not fail a send.
    pub(super) fn current_device_id_or_zero(&self) -> u32 {
        self.settings_repo
            .get::<u32>(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)
            .ok()
            .flatten()
            .unwrap_or(0)
    }

}

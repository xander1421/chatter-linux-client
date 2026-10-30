use super::*;

impl ChatterClient {
    // ── Infrastructure ──────────────────────────────────────────────────

    pub fn runtime_handle(&self) -> &tokio::runtime::Handle {
        &self.services.runtime_handle
    }
    pub fn directories(&self) -> &Directories {
        &self.services.directories
    }

    /// Post-boot call service handle (OnceLock).
    #[cfg(feature = "media")]
    pub fn call_service(&self) -> &Arc<std::sync::OnceLock<CallWorkerHandle>> {
        &self.services.call_service
    }

    /// Post-boot group fanout (OnceLock).
    pub fn group_fanout(&self) -> &Arc<std::sync::OnceLock<Arc<GroupFanout>>> {
        &self.services.group_fanout
    }

    /// Post-boot device inbox provider (OnceLock).
    pub fn device_inbox_provider(&self) -> &Arc<std::sync::OnceLock<Arc<DeviceInboxProvider>>> {
        &self.services.device_inbox_provider
    }

    // ── Delegated CoreServices methods ─────────────────────────────────────

    /// Check if connected to the server.
    pub fn is_connected(&self) -> bool {
        self.services.is_connected()
    }

    /// Start the inbox provider and return a receiver for raw inbox events.
    pub async fn start_inbox_provider(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<crate::inbox::InboxEvent>> {
        self.services.start_inbox_provider().await
    }

    /// Pre-fetch anonymous tokens for sealed sender messaging.
    pub async fn prefetch_tokens(
        &self,
        target_count: u32,
    ) -> Result<u32, crate::error::MailboxError> {
        self.services.prefetch_tokens(target_count).await
    }

    /// Decline an incoming call.
    pub async fn decline_call(&self, call_id: &str) -> Result<(), NetworkError> {
        self.services.decline_call(call_id).await
    }

    /// Whether the GIF picker may be used in a conversation of `kind`: not in
    /// a secret chat and not in Shield, where GifService's account auth would
    /// tie the activity to the account
    /// ([`crate::domain::message::ConversationType::gif_picker_available`]).
    pub fn gif_picker_available_for(
        &self,
        kind: &crate::domain::message::ConversationType,
    ) -> bool {
        !self.anon_mode_enabled() && kind.gif_picker_available()
    }

    /// [`Self::gif_picker_available_for`] the open conversation.
    pub fn gif_picker_available(&self) -> bool {
        !self.anon_mode_enabled()
            && self
                .chat_state()
                .active_conversation()
                .routing_key()
                .is_none_or(|(kind, _)| kind.gif_picker_available())
    }

    /// Search GIFs. Refused in a secret chat ([`Self::gif_picker_available`]).
    pub async fn search_gifs(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<crate::proto::chat_v1::GifResult>, NetworkError> {
        if !self.gif_picker_available() {
            return Err(NetworkError::PermissionDenied);
        }
        self.services.search_gifs(query, limit).await
    }

    /// Fetch trending GIFs. Refused in a secret chat ([`Self::gif_picker_available`]).
    pub async fn trending_gifs(
        &self,
        limit: u32,
    ) -> Result<Vec<crate::proto::chat_v1::GifResult>, NetworkError> {
        if !self.gif_picker_available() {
            return Err(NetworkError::PermissionDenied);
        }
        self.services.trending_gifs(limit).await
    }

}

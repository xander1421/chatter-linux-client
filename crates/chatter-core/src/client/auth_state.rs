use super::*;

/// A read-only handle for checking whether work still belongs to a session.
#[derive(Clone)]
pub struct SessionGuard {
    auth: AuthState,
    revision: u64,
}

impl SessionGuard {
    pub fn is_current(&self) -> bool {
        self.auth.get().session_revision == self.revision
    }
}

/// A read-only handle for checking the selected conversation and thread.
#[derive(Clone)]
pub struct ConversationGuard {
    chat: ChatState,
    conversation: crate::state::ActiveConversation,
}

impl ConversationGuard {
    pub fn is_current(&self) -> bool {
        self.chat.active_conversation() == self.conversation
    }

    pub fn shows_thread(&self, root: crate::domain::ids::MessageId) -> bool {
        self.chat
            .active_thread()
            .is_some_and(|thread| thread.root_message_id == root)
    }
}

impl ChatterClient {
    // ── Auth façade ──────────────────────────────────────────────────────

    pub fn endpoint(&self) -> String {
        self.auth().endpoint()
    }

    /// Query the dedicated signed release channel over the active network path.
    pub async fn check_for_update(
        &self,
        platform: &str,
        channel: &str,
        installed_code: u64,
    ) -> Result<Option<crate::update::UpdateManifest>, String> {
        let mut changed = self.update_transport_epoch.subscribe();
        tokio::select! {
          result = crate::update::check_latest(
            &self.services.grpc_pool,
            self.anon_mode.load(Ordering::Acquire),
            platform,
            channel,
            installed_code,
          ) => result,
          _ = changed.changed() => Err("network mode changed during update check".into()),
        }
    }

    pub async fn check_available_update(
        &self,
    ) -> Result<Option<crate::update::UpdateManifest>, String> {
        let found = self
            .check_for_update(
                crate::update::platform(),
                crate::update::channel(),
                crate::update::installed_version_code(),
            )
            .await?;
        *self.pending_update.lock() = found.clone();
        Ok(found)
    }

    pub async fn download_available_update_with_progress(
        &self,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<std::path::PathBuf, String> {
        let manifest = self
            .pending_update
            .lock()
            .clone()
            .ok_or_else(|| "no verified update is available".to_string())?;
        let mut changed = self.update_transport_epoch.subscribe();
        tokio::select! {
          result = crate::update::download_verified_with_progress(
            &self.services.grpc_pool,
            self.anon_mode.load(Ordering::Acquire),
            &manifest,
            self.directories().data_dir(),
            progress,
          ) => result,
          _ = changed.changed() => {
              let staged = self.directories().data_dir().join("updates")
                  .join(format!("{}-{}.download", manifest.platform, manifest.version_code));
              let _ = tokio::fs::remove_file(staged).await;
              Err("network mode changed during update download".into())
          },
        }
    }

    pub fn available_update(&self) -> Option<crate::update::UpdateManifest> {
        self.pending_update.lock().clone()
    }

    pub fn set_server_url(&self, url: &str) -> Result<(), crate::error::NetworkError> {
        self.auth().set_server_url(url)
    }

    pub fn is_logged_in(&self) -> bool {
        !self.auth_state().get().session_ended && self.auth().is_logged_in()
    }

    pub fn has_registered_before(&self) -> bool {
        self.auth().has_registered_before()
    }

    pub fn current_user(&self) -> Option<(crate::domain::ids::UserId, u32)> {
        self.auth().current_user()
    }

    pub fn restore_session(&self) -> bool {
        self.auth().restore_session()
    }

    /// Publish the restored account identity for subscribers at startup.
    pub fn publish_restored_auth_state(&self) {
        if let Some((user_id, device_id)) = self.current_user() {
            let display_name = self.display_name(&user_id);
            self.services
                .auth_state
                .set_logged_in(user_id, device_id, display_name);
        }
    }

    pub async fn cleanup_failed_registration(
        &self,
    ) -> Result<(), crate::services::auth_service::AuthError> {
        self.auth().cleanup_failed_registration().await
    }

    /// Local logout (best-effort server notify + clear local state).
    // ── State stores ────────────────────────────────────────────────────

    pub(crate) fn auth_state(&self) -> &AuthState {
        &self.services.auth_state
    }
    pub(crate) fn chat_state(&self) -> &ChatState {
        &self.services.chat_state
    }

    /// Resolve the current active conversation's `ConversationType`,
    /// looking up the channel row when the active conversation is a
    /// `Channel(id)` (the vault and DMs share that variant). UI
    /// callers use this rather than
    /// `ActiveConversation::conversation_type(...)` directly so they
    /// don't need channel_repo access.
    pub fn active_conversation_type(&self) -> Option<crate::domain::message::ConversationType> {
        self.conversation_type_of(&self.services.chat_state.get().active_conversation)
    }

    /// Whether the currently displayed screen must be hidden from OS capture.
    /// The caller supplies presentation visibility and the kind retained on a
    /// phone while its chat pane slides away. The active conversation takes
    /// precedence, and secret/Shield classification remains in core.
    pub fn requires_screen_protection(
        &self,
        chat_visible: bool,
        recovery_codes_visible: bool,
        exiting_conversation_kind: Option<crate::domain::message::ConversationType>,
    ) -> bool {
        recovery_codes_visible
            || self.anon_mode_enabled()
            || (chat_visible
                && self
                    .active_conversation_type()
                    .or(exiting_conversation_kind)
                    .is_some_and(|kind| kind.is_secret_chat()))
    }

    /// Same resolution for a conversation that isn't the active one — a handle
    /// can be built for any conversation, and `channel_repo` is private to this
    /// module.
    pub fn conversation_type_of(
        &self,
        conversation: &crate::state::ActiveConversation,
    ) -> Option<crate::domain::message::ConversationType> {
        conversation.conversation_type(&self.services.channel_repo)
    }

    /// Header capabilities from the same conversation type and membership state
    /// used by core operations. Mesh has no `ActiveConversation` and yields none.
    pub fn conversation_actions(
        &self,
        conversation: crate::state::ActiveConversation,
    ) -> ConversationActions {
        use crate::domain::message::ConversationType as Kind;
        let Some(kind) = self.conversation_type_of(&conversation) else {
            return ConversationActions::default();
        };
        let can_call = matches!(kind, Kind::Channel | Kind::ServerDm)
            && !conversation
                .channel_id()
                .is_some_and(|id| self.is_former_channel(&id));
        let can_invite = match (&kind, &conversation) {
            (Kind::Channel, crate::state::ActiveConversation::Channel(id)) => {
                self.can_invite_to_channel(id)
            }
            (Kind::AnonGroup, crate::state::ActiveConversation::AnonGroup(id)) => {
                self.can_invite_to_anon_group(id)
            }
            _ => false,
        };
        ConversationActions {
            can_call,
            can_invite,
            can_open_settings: matches!(kind, Kind::Channel | Kind::ServerDm | Kind::AnonGroup),
            can_open_pinned_list: matches!(kind, Kind::SavedMessages),
        }
    }

    /// `ActiveConversation` is the single source of truth for which
    /// conversation is currently being viewed.
    pub fn active_conversation(&self) -> crate::state::ActiveConversation {
        self.services.chat_state.active_conversation()
    }

    /// Current onion connection indicator, including the signed-out screens.
    pub fn tor_status_code(&self) -> i32 {
        crate::network::connection::tor_status().as_i32()
    }

    /// Whether bounded onion dial tracing was enabled for this process.
    pub fn tor_stage_trace_active() -> bool {
        #[cfg(feature = "onion")]
        {
            crate::network::grpc::onion_transport::stage_trace_active()
        }
        #[cfg(not(feature = "onion"))]
        {
            false
        }
    }

    /// Select a conversation through the client so attachment trays and thread
    /// selection move together with the active conversation.
    pub fn select_conversation(&self, conversation: crate::state::ActiveConversation) {
        self.services
            .chat_state
            .set_active_conversation(conversation);
    }

    /// Legacy channel selection keeps the channel-specific attachment tray
    /// transition before selecting the typed conversation.
    pub fn select_channel(&self, channel: crate::domain::ids::ChannelId) {
        self.services.chat_state.set_active_channel(channel);
        self.select_conversation(crate::state::ActiveConversation::Channel(channel));
    }

    /// Stage the channel tray before the UI dispatches its selection callback.
    pub fn prepare_channel_selection(&self, channel: crate::domain::ids::ChannelId) {
        self.services.chat_state.set_active_channel(channel);
    }

    /// Read-only copy of chat state for presentation consumers.
    pub fn chat_snapshot(&self) -> crate::state::ChatStateData {
        self.services.chat_state.get()
    }

    pub fn message_snapshot(&self) -> crate::state::MessageStateData {
        self.services.message_state.get()
    }

    /// Read-only account state for UI ownership and session-revision checks.
    pub fn auth_snapshot(&self) -> crate::state::AuthStateData {
        self.services.auth_state.get()
    }

    pub fn has_auth_identity(&self) -> bool {
        self.services.auth_state.is_logged_in()
    }

    #[cfg(feature = "testing")]
    pub fn testing_set_logged_in(
        &self,
        user: crate::domain::ids::UserId,
        device: u32,
        name: String,
    ) {
        self.services.auth_state.set_logged_in(user, device, name);
    }

    #[cfg(feature = "testing")]
    pub fn testing_set_logged_out(&self) {
        self.services.auth_state.set_logged_out();
    }

    #[cfg(feature = "testing")]
    pub fn testing_invalidate_session(&self) {
        self.services.auth_state.invalidate_session();
    }

    pub fn user_snapshot(&self) -> crate::state::UserStateData {
        self.services.user_state.get()
    }

    pub fn session_is_current(&self, revision: u64) -> bool {
        self.services.auth_state.session_is_current(revision)
    }

    pub fn is_app_visible(&self) -> bool {
        self.services.chat_state.is_app_visible()
    }

    pub fn session_guard(&self) -> SessionGuard {
        SessionGuard {
            auth: self.services.auth_state.clone(),
            revision: self.auth_snapshot().session_revision,
        }
    }

    pub fn conversation_guard(
        &self,
        conversation: crate::state::ActiveConversation,
    ) -> ConversationGuard {
        ConversationGuard {
            chat: self.services.chat_state.clone(),
            conversation,
        }
    }

    pub fn subscribe_auth_snapshots(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::state::StateSnapshot<crate::state::AuthStateData>>
    {
        self.services.auth_state.subscribe()
    }

    pub fn subscribe_chat_snapshots(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::state::StateSnapshot<crate::state::ChatStateData>>
    {
        self.services.chat_state.subscribe()
    }

    pub fn offline_snapshot(&self) -> crate::state::OfflineStateData {
        self.services.offline_state.get()
    }

    pub fn subscribe_offline_snapshots(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::state::StateSnapshot<crate::state::OfflineStateData>>
    {
        self.services.offline_state.subscribe()
    }

    pub fn message_is_loading(&self, channel: &crate::domain::ids::ChannelId) -> bool {
        self.services.message_state.is_loading(channel)
    }

    pub fn set_message_loading(&self, channel: crate::domain::ids::ChannelId, loading: bool) {
        self.services.message_state.set_loading(channel, loading);
    }
    pub(crate) fn message_state(&self) -> &MessageState {
        &self.services.message_state
    }
    pub(crate) fn user_state(&self) -> &UserState {
        &self.services.user_state
    }

    /// Persistent sidebar collapsed flag (settings-backed). Defaults to false.
    pub fn sidebar_collapsed(&self) -> bool {
        use crate::storage::repositories::settings_repo::SettingsRepository;
        use crate::storage::settings_keys;
        let repo = SettingsRepository::new(self.services.database.clone());
        repo.get::<bool>(settings_keys::SIDEBAR_COLLAPSED)
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    /// Persist sidebar collapsed flag.
    pub fn set_sidebar_collapsed(&self, collapsed: bool) {
        use crate::storage::repositories::settings_repo::SettingsRepository;
        use crate::storage::settings_keys;
        let repo = SettingsRepository::new(self.services.database.clone());
        let _ = repo.set(settings_keys::SIDEBAR_COLLAPSED, &collapsed);
    }

    /// Read the user's search-shortcut preference (defaults to `Both`).
    pub fn get_search_shortcut(&self) -> crate::services::search::SearchShortcut {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        let repo = SettingsRepository::new(self.services.database.clone());
        let raw = repo
            .get_string(keys::SEARCH_SHORTCUT)
            .ok()
            .flatten()
            .unwrap_or_default();
        crate::services::search::SearchShortcut::from_str(&raw)
    }

    /// The current account's full Signal-style `nickname.discriminator` handle,
    /// chosen at registration and persisted under `keys::FULL_USERNAME`. This is
    /// the exact form `lookup_by_username` expects. `None` before registration.
    pub fn full_username(&self) -> Option<String> {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        SettingsRepository::new(self.services.database.clone())
            .get_string(keys::FULL_USERNAME)
            .ok()
            .flatten()
    }

    /// Persist the user's search-shortcut preference.
    pub fn set_search_shortcut(
        &self,
        shortcut: crate::services::search::SearchShortcut,
    ) -> Result<(), crate::error::StorageError> {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        let repo = SettingsRepository::new(self.services.database.clone());
        repo.set_string(keys::SEARCH_SHORTCUT, shortcut.as_str())
    }
}

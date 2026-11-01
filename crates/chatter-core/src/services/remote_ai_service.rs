//! Remote AI configuration and explicit per-conversation consent. No local inference or indexing.
use crate::ai_policy::channel_ai::{self, SiloPreference};
use crate::cloud_ai::{CloudMode, CloudProviderKind, CloudSettings};
use crate::storage::repositories::settings_repo::SettingsRepository;
use std::sync::Arc;

pub struct RemoteAIService {
    settings_repo: Arc<SettingsRepository>,
}
impl RemoteAIService {
    pub fn new(settings_repo: Arc<SettingsRepository>) -> Self {
        Self { settings_repo }
    }
    pub fn is_enabled(&self) -> bool {
        self.silo_preference() == SiloPreference::PreferCloud
    }
    pub fn set_anthropic_api_key(&self, key: &str) -> Result<(), crate::error::StorageError> {
        let key = key.trim();
        if key.is_empty() {
            self.settings_repo.delete("cloud_ai.api_key")?;
            return Ok(());
        }
        self.settings_repo.set_string("cloud_ai.api_key", key)
    }

    /// Persist whether the cloud silo uses the user's own key or the metered
    /// server broker.
    pub fn set_cloud_mode(&self, mode: CloudMode) -> Result<(), crate::error::StorageError> {
        self.settings_repo
            .set_string("cloud_ai.mode", mode.as_str())
    }

    /// Persist the model used when the active provider is a cloud one.
    pub fn set_cloud_model(&self, model: &str) -> Result<(), crate::error::StorageError> {
        self.settings_repo.set_string("cloud_ai.model", model)
    }

    // ==================== Cloud silo ====================

    /// Current cloud-silo configuration, read fresh from the repo.
    ///
    /// Deliberately a separate struct from `LocalAISettings`: the API key is a
    /// cloud-only secret and has no business in the local provider's config.
    pub fn cloud_settings(&self) -> CloudSettings {
        Self::load_cloud_settings(&self.settings_repo)
    }

    fn load_cloud_settings(repo: &SettingsRepository) -> CloudSettings {
        let mut settings = CloudSettings::default();

        if let Ok(Some(m)) = repo.get_string("cloud_ai.mode") {
            if let Some(mode) = CloudMode::parse(&m) {
                settings.mode = mode;
            }
        }
        if let Ok(Some(p)) = repo.get_string("cloud_ai.provider") {
            if let Some(kind) = CloudProviderKind::parse(&p) {
                settings.provider = kind;
                settings.model = kind.default_model().to_string();
            }
        }
        if let Ok(Some(model)) = repo.get_string("cloud_ai.model") {
            settings.model = model;
        }
        if let Ok(Some(key)) = repo.get_string("cloud_ai.api_key") {
            let key = key.trim().to_string();
            if !key.is_empty() {
                settings.api_key = Some(key);
            }
        }

        settings
    }

    /// The user's silo preference. Local-only unless explicitly changed.
    pub fn silo_preference(&self) -> SiloPreference {
        self.settings_repo
            .get_string(channel_ai::SILO_PREFERENCE_KEY)
            .ok()
            .flatten()
            .and_then(|s| SiloPreference::parse(&s))
            .unwrap_or_default()
    }

    /// Persist the silo preference.
    pub fn set_silo_preference(
        &self,
        preference: SiloPreference,
    ) -> Result<(), crate::error::StorageError> {
        self.settings_repo
            .set_string(channel_ai::SILO_PREFERENCE_KEY, preference.as_str())
    }

    // ==================== Per-conversation AI ====================

    /// Is AI switched on for this conversation? Off unless explicitly enabled.
    pub fn ai_enabled_for(&self, conversation_id: &str) -> bool {
        self.settings_repo
            .get_bool(&channel_ai::enabled_key(conversation_id))
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    /// Turn AI on or off for one conversation.
    pub fn set_ai_enabled_for(
        &self,
        conversation_id: &str,
        enabled: bool,
    ) -> Result<(), crate::error::StorageError> {
        self.settings_repo
            .set_bool(&channel_ai::enabled_key(conversation_id), enabled)
    }

    /// What should actually run for this conversation, after policy.
    ///
    /// The single entry point a caller needs: combines the per-conversation
    /// switch, the global provider, and the mesh/DM policy clamp. A
    /// `Resolved::Use(_)` here is safe to run; anything else must not.
    pub fn resolve_ai_for(
        &self,
        conversation_id: &str,
        conversation: &crate::domain::message::ConversationType,
        delivered_via_mesh: bool,
    ) -> channel_ai::Resolved {
        let access = crate::ai_policy::policy::ai_access(conversation, delivered_via_mesh);
        let resolved = channel_ai::resolve(
            self.ai_enabled_for(conversation_id),
            self.silo_preference(),
            self.cloud_settings().is_available(),
            access,
        );
        match resolved {
            channel_ai::Resolved::Use(crate::ai_policy::policy::Silo::Local) => {
                channel_ai::Resolved::Off
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::ConversationType;
    use crate::storage::Database;

    fn service() -> (RemoteAIService, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("remote.db"), "test-only").unwrap());
        db.run_migrations().unwrap();
        (
            RemoteAIService::new(Arc::new(SettingsRepository::new(db))),
            dir,
        )
    }

    #[test]
    fn legacy_local_preference_never_becomes_remote_consent() {
        let (service, _dir) = service();
        service.settings_repo.set_local_ai_enabled(true).unwrap();
        service.set_cloud_mode(CloudMode::ServerBroker).unwrap();
        service.set_ai_enabled_for("channel-fixture", true).unwrap();
        assert!(!service.is_enabled());
        assert_eq!(
            service.resolve_ai_for("channel-fixture", &ConversationType::Channel, false),
            channel_ai::Resolved::Off
        );
        assert!(
            service.settings_repo.get_local_ai_enabled().unwrap(),
            "existing local settings must be preserved without enabling them"
        );
    }

    #[test]
    fn remote_config_uses_existing_keys_and_reads_changes_without_restart() {
        let (service, _dir) = service();
        service.set_cloud_mode(CloudMode::DirectAnthropic).unwrap();
        service
            .set_anthropic_api_key("public-test-fixture-key")
            .unwrap();
        service.set_cloud_model("fixture-model").unwrap();
        let fresh = RemoteAIService::new(service.settings_repo.clone());
        let settings = fresh.cloud_settings();
        assert_eq!(settings.mode, CloudMode::DirectAnthropic);
        assert_eq!(settings.model, "fixture-model");
        assert_eq!(settings.api_key.as_deref(), Some("public-test-fixture-key"));
        fresh.set_anthropic_api_key("").unwrap();
        assert!(service.cloud_settings().api_key.is_none());
    }

    #[test]
    fn remote_requires_global_and_conversation_consent_and_allowed_transport() {
        let (service, _dir) = service();
        service.set_cloud_mode(CloudMode::ServerBroker).unwrap();
        service
            .set_silo_preference(SiloPreference::PreferCloud)
            .unwrap();
        assert_eq!(
            service.resolve_ai_for("fixture", &ConversationType::Channel, false),
            channel_ai::Resolved::Off
        );
        service.set_ai_enabled_for("fixture", true).unwrap();
        assert_eq!(
            service.resolve_ai_for("fixture", &ConversationType::Channel, false),
            channel_ai::Resolved::Use(crate::ai_policy::policy::Silo::Cloud)
        );
        for kind in [
            ConversationType::ServerDm,
            ConversationType::AnonDm,
            ConversationType::AnonGroup,
            ConversationType::SavedMessages,
            ConversationType::Mesh,
            ConversationType::MeshDm,
        ] {
            assert_eq!(
                service.resolve_ai_for("fixture", &kind, false),
                channel_ai::Resolved::Off,
                "{kind:?}"
            );
        }
        assert_eq!(
            service.resolve_ai_for("fixture", &ConversationType::Channel, true),
            channel_ai::Resolved::Off
        );
        service.set_cloud_mode(CloudMode::DirectAnthropic).unwrap();
        assert_eq!(
            service.resolve_ai_for("fixture", &ConversationType::Channel, false),
            channel_ai::Resolved::Off
        );
    }
}

//! jarvis façade — thin delegates to services.

#![allow(unused_imports)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::app::context::{AppContext, ContextError};
use crate::app::core_services::CoreServices;
use crate::domain::attachment::Attachment;
use crate::domain::channel::{Channel, ChannelType};
use crate::domain::message::{ConversationType, Message};
use crate::error::NetworkError;
use crate::events::{DomainEvent, EventBus};
use crate::services::auth_service::{AuthError, LoginResult};
use crate::inbox::DeviceInboxProvider;
use crate::mailbox::group::GroupFanout;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::ai_service::AIService;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::AttachmentService;
use crate::services::auth_service::AuthService;
#[cfg(feature = "media")]
use crate::services::call_worker::CallWorkerHandle;
use crate::services::channel::ChannelService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::gateway::outgoing::{
    OwnedOutgoingServices, OutgoingIntent, QuoteIntent, SendError, SendResult,
    route_outgoing_owned,
};
use crate::services::jarvis::JarvisService;
use crate::services::ai::LocalAIService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::pin_service::PinService;
use crate::services::session_service::{SessionError, SessionService};
use crate::services::signal::SignalService;
use crate::services::section_service::SectionService;
use crate::services::user::UserService;
use crate::services::conversation::ConversationHandle;
use crate::state::{
    ActiveConversation, AuthState, ChatState, JarvisState, MessageState, OfflineState,
    PrivacyState, UserState,
};
use crate::state::chat_state::PendingAttachment;
use crate::storage::settings_keys;
use crate::system::Directories;
use super::{ChatterClient, ClientError};

/// Display name of the local AI channel.
pub const JARVIS_CHANNEL_NAME: &str = "Jarvis (AI)";

/// The local AI channel's fixed id: the nil UUID. Matches the
/// `JarvisContext::general()` context key, so channel history and Jarvis
/// memory share one identity. Never collides with server channels (v4 UUIDs).
pub fn jarvis_channel_id() -> crate::domain::ids::ChannelId {
    crate::domain::ids::ChannelId::from_uuid(uuid::Uuid::nil())
}

/// Whether an id refers to the local AI channel.
pub fn is_jarvis_channel(id: &crate::domain::ids::ChannelId) -> bool {
    id.0.is_nil()
}

/// The query of an `@jarvis` mention, if `text` has one. The mention must be a
/// whole word (start of text or after whitespace; end of text, whitespace or
/// `,:;!?` after it) — `me@jarvis.io` is an address, not a mention. Leading
/// punctuation and whitespace are trimmed off the query.
pub fn parse_jarvis_mention(text: &str) -> Option<&str> {
    const TAG: &str = "@jarvis";
    for (at, _) in text.match_indices('@') {
        let starts_word = text[..at].chars().next_back().map_or(true, char::is_whitespace);
        let rest = &text[at..];
        if !starts_word || !rest.get(..TAG.len()).is_some_and(|t| t.eq_ignore_ascii_case(TAG)) {
            continue;
        }
        let after = &rest[TAG.len()..];
        let ends_word = after
            .chars()
            .next()
            .map_or(true, |c| c.is_whitespace() || ",:;!?".contains(c));
        if !ends_word {
            continue;
        }
        let query = after.trim_start_matches(|c: char| c.is_whitespace() || ",:;!?".contains(c)).trim_end();
        return (!query.is_empty()).then_some(query);
    }
    None
}

impl ChatterClient {
    // ── Jarvis façade ────────────────────────────────────────────────────

    /// Whether the local AI channel should appear in the sidebar.
    ///
    /// Follows the local-AI enabled setting; a missing provider surfaces as
    /// a helpful error inside the channel rather than hiding it, so users
    /// can discover the feature and be pointed at settings.
    pub fn jarvis_channel_enabled(&self) -> bool {
        self.http_local_ai().is_enabled()
    }

    /// The virtual (client-side only) channel entry for the sidebar.
    pub fn jarvis_virtual_channel(&self) -> Channel {
        let mut channel = Channel::new(
            jarvis_channel_id(),
            JARVIS_CHANNEL_NAME.to_string(),
            ChannelType::Private,
            crate::domain::ids::UserId::from_uuid(uuid::Uuid::nil()),
        );
        channel.description = Some(self.jarvis_channel_topic());
        channel
    }

    /// Header line for the AI channel, stating where its messages actually
    /// go. The channel follows the user's silo preference, so it is only
    /// "never leaves this device" when it really runs locally.
    pub fn jarvis_channel_topic(&self) -> String {
        self.jarvis_privacy_line().0
    }

    /// The AI channel's privacy line: the same statement as its topic, and
    /// whether its messages leave this device (to a cloud provider, not end-
    /// to-end encrypted). Its kind — a private channel — would otherwise
    /// claim "Messages are end-to-end encrypted", which is false either way.
    pub fn jarvis_privacy_line(&self) -> (String, bool) {
        jarvis_privacy_for(self.jarvis_channel_silo(), &self.jarvis_engine_description())
    }

    /// Silo for the AI channel and the context-free global prompt: the
    /// user's preference, cloud only when it is also configured.
    fn jarvis_channel_silo(&self) -> crate::ai_policy::policy::Silo {
        use crate::ai_policy::channel_ai::SiloPreference;
        use crate::ai_policy::policy::Silo;
        let prefers_cloud = self.ai_silo_preference() == SiloPreference::PreferCloud;
        let available = self.http_local_ai().cloud_settings().is_available();
        if prefers_cloud && available { Silo::Cloud } else { Silo::Local }
    }

    /// If `text` sent in `active` is an `@jarvis` request that Jarvis may
    /// answer here, the context to ask in and the query. `None` means "not for
    /// Jarvis — send it as a normal message": no word-boundary mention (so
    /// `me@jarvis.io` is just text), no query after it, or AI is switched off
    /// for this conversation (`resolve_ai_for`), which is the default in DMs.
    pub fn jarvis_mention(
        &self,
        active: &crate::state::ActiveConversation,
        text: &str,
    ) -> Option<(crate::domain::jarvis::JarvisContext, String)> {
        use crate::domain::jarvis::JarvisContext;
        use crate::state::ActiveConversation as AC;

        let query = parse_jarvis_mention(text)?;
        let context = match active {
            AC::Channel(id) => JarvisContext::channel(*id),
            AC::ServerDm { .. } => JarvisContext::dm(active.channel_id()?),
            AC::AnonDm(id) => JarvisContext::anon_dm(*id),
            AC::AnonGroup(id) => JarvisContext::anon_group(*id),
            AC::None => return None,
        };
        self.check_ai_allowed_in(&context).ok()?;
        Some((context, query.to_string()))
    }

    /// Human-readable description of the active Jarvis engine ("ollama (qwen3:4b)").
    pub fn jarvis_engine_description(&self) -> String {
        self.services.jarvis_engine.describe()
    }

    /// Persisted AI-channel conversation history (chronological).
    pub fn jarvis_channel_history(
        &self,
        limit: u32,
    ) -> Result<Vec<crate::domain::jarvis::JarvisConversation>, crate::services::jarvis::JarvisError>
    {
        self.jarvis()
            .get_conversation_history(&crate::domain::jarvis::JarvisContext::general(), limit)
    }

    // ── Local AI settings façade ─────────────────────────────────────────

    /// Probe local providers (Ollama / LM Studio / llama.cpp / vLLM +
    /// compiled-in mistralrs) and list their installed models.
    pub async fn local_ai_detect_providers(
        &self,
    ) -> Vec<crate::local_ai::providers::DetectedProvider> {
        let ollama_url = self.http_local_ai().settings().ollama_url.clone();
        crate::local_ai::providers::detect_providers(&ollama_url).await
    }

    /// Current hardware profile (blocking probes — call off the UI thread).
    pub fn local_ai_hardware_profile(&self) -> crate::local_ai::hardware::HardwareProfile {
        crate::local_ai::hardware::HardwareProfile::detect()
    }

    /// The active provider's settings identifier ("ollama", "mistralrs", …).
    pub fn local_ai_active_provider(&self) -> String {
        self.http_local_ai()
            .fresh_settings()
            .active_provider
            .as_str()
            .to_string()
    }

    /// Currently configured chat model.
    pub fn local_ai_chat_model(&self) -> String {
        self.http_local_ai().fresh_settings().chat_model
    }

    /// Persist a provider choice and hot-swap the Jarvis engine.
    pub fn local_ai_apply_provider(&self, kind_str: &str) -> Result<(), String> {
        let kind = crate::local_ai::providers::ProviderKind::parse(kind_str)
            .ok_or_else(|| format!("unknown provider: {kind_str}"))?;
        self.http_local_ai()
            .set_active_provider(kind)
            .map_err(|e| e.to_string())?;
        self.rebuild_jarvis_engine();
        Ok(())
    }

    /// Persist a chat-model choice and hot-swap the Jarvis engine.
    pub fn local_ai_apply_chat_model(&self, model: &str) -> Result<(), String> {
        self.http_local_ai()
            .set_chat_model(model)
            .map_err(|e| e.to_string())?;
        self.rebuild_jarvis_engine();
        Ok(())
    }

    fn rebuild_jarvis_engine(&self) {
        let settings = self.http_local_ai().fresh_settings();
        self.services.jarvis_engine.set(
            crate::services::jarvis::JarvisEngine::from_settings(
                &settings,
                self.local_ai().actor(),
            ),
        );
        self.rebuild_cloud_engine();
        tracing::info!(engine = %self.services.jarvis_engine.describe(), "jarvis engine rebuilt");
    }

    /// Install the cloud engine for the configured mode, or clear it.
    ///
    /// Kept separate from the local engine so both are available at once: the
    /// silo is chosen per conversation, and a DM must still get the local
    /// model even while cloud is configured.
    pub fn rebuild_cloud_engine(&self) {
        use crate::cloud_ai::{AnyCloudBackend, CloudMode};
        use crate::services::jarvis::JarvisEngine;

        let cloud = self.http_local_ai().cloud_settings();
        let engine = match cloud.mode {
            // Managed tokens: the server holds the key, so this needs a live
            // authenticated client rather than settings.
            CloudMode::ServerBroker => Some(JarvisEngine::cloud(
                AnyCloudBackend::broker(self.services.grpc_pool.ai_client()),
                cloud.model.clone(),
            )),
            // Bring-your-own-key: absent or blank key means no cloud engine.
            CloudMode::DirectAnthropic => {
                JarvisEngine::from_cloud_settings(&cloud)
            }
        };

        let configured = engine.is_some();
        self.services.jarvis_engine.set_cloud(engine);
        tracing::info!(mode = %cloud.mode.as_str(), configured, "cloud jarvis engine rebuilt");
    }

    /// Pull a model through Ollama with streaming progress.
    pub async fn local_ai_pull_model(
        &self,
        tag: &str,
        on_progress: impl FnMut(crate::local_ai::ollama::PullProgress),
    ) -> Result<(), crate::local_ai::backend::BackendError> {
        let settings = self.http_local_ai().fresh_settings();
        let backend = crate::local_ai::ollama::OllamaBackend::new(
            &settings.ollama_url,
            settings.request_timeout.max(std::time::Duration::from_secs(3600)),
        );
        backend.pull_model_with_progress(tag, on_progress).await
    }

    // ── Memory browser façade ────────────────────────────────────────────

    /// All facts about the current user (current + superseded history).
    pub fn jarvis_memory_facts(&self) -> Vec<crate::domain::jarvis::JarvisFact> {
        let Some(user_id) = self.auth_state().get().current_user_id else {
            return Vec::new();
        };
        self.jarvis().list_user_facts(&user_id).unwrap_or_default()
    }

    /// Most-mentioned entities in the knowledge graph.
    pub fn jarvis_top_entities(&self, limit: u32) -> Vec<crate::domain::jarvis::JarvisEntity> {
        self.jarvis().get_top_entities(limit).unwrap_or_default()
    }

    /// Entities connected to one entity (with graph distance).
    pub fn jarvis_entity_connections(
        &self,
        entity_id: &str,
    ) -> Vec<(crate::domain::jarvis::JarvisEntity, u32)> {
        let id = crate::domain::jarvis::JarvisEntityId::from_string(entity_id.to_string());
        self.jarvis()
            .get_connected_entities(&id, 1)
            .unwrap_or_default()
    }

    pub async fn jarvis_ask(
        &self,
        context: &crate::domain::jarvis::JarvisContext,
        query: &str,
        author_id: Option<&crate::domain::ids::UserId>,
    ) -> Result<String, crate::services::jarvis::JarvisError> {
        let silo = self.check_ai_allowed_in(context)?;
        // Make sure the silo this conversation resolved to is the one actually
        // installed before the turn runs.
        if silo == crate::ai_policy::policy::Silo::Cloud
            && !self.services.jarvis_engine.has_cloud()
        {
            self.rebuild_cloud_engine();
        }
        self.jarvis().ask_in(context, query, author_id, silo).await
    }

    /// Gate a Jarvis turn on the conversation's own AI switch.
    ///
    /// The dedicated AI channel is exempt — it *is* the AI surface, and
    /// requiring it to be switched on would be circular. Everywhere else AI is
    /// opt-in per conversation (`conv.ai.enabled.<id>`), so a channel that was
    /// never enabled answers with a clear error rather than silently running a
    /// model over its contents.
    ///
    /// This is the per-conversation switch only. Which *silo* may then run —
    /// local vs cloud, and nothing at all over mesh — is decided separately by
    /// `ai_policy`, enforced down in `SharedJarvisEngine`.
    fn check_ai_allowed_in(
        &self,
        context: &crate::domain::jarvis::JarvisContext,
    ) -> Result<crate::ai_policy::policy::Silo, crate::services::jarvis::JarvisError> {
        use crate::ai_policy::channel_ai::Resolved;

        // The dedicated AI channel, and the context-free global prompt, always
        // work. They follow the user's silo preference, still clamped by
        // policy inside the engine.
        if context.context_id.is_empty()
            || context.context_id == jarvis_channel_id().to_string()
        {
            return Ok(self.jarvis_channel_silo());
        }

        match self.resolve_ai_for(
            &context.context_id,
            &context.conversation_type(),
            // A Jarvis turn is user-initiated from this device; mesh-delivered
            // traffic never reaches here.
            false,
        ) {
            Resolved::Use(silo) => Ok(silo),
            Resolved::Off => Err(crate::services::jarvis::JarvisError::NotEnabledHere),
        }
    }

    /// Execute a parsed Jarvis slash command end-to-end.
    ///
    /// Consolidates the dispatch logic that used to branch in the UI crate:
    /// records the user message in local state, routes `/ask`,
    /// `/summarize`, `/knowledge`, and `/remember` to their respective
    /// service calls, then records the assistant reply (or error).
    ///
    /// Returns `(query_display, result)` — the first value is the
    /// canonical slash-command string for UI rendering, the second is the
    /// assistant's reply (or a `JarvisError`).
    pub async fn jarvis_execute(
        &self,
        cmd: crate::services::jarvis::JarvisCommand,
        context: &crate::domain::jarvis::JarvisContext,
        author_id: Option<&crate::domain::ids::UserId>,
    ) -> (String, Result<String, crate::services::jarvis::JarvisError>) {
        use crate::services::jarvis::JarvisCommand as JC;

        use crate::domain::jarvis::ContextType;
        let channel_label = match context.context_type {
            ContextType::Channel => Some(context.context_id.clone()),
            _ => None,
        };

        let (query_display, result) = match cmd {
            JC::Ask(query) => {
                self.jarvis_state().add_user_message(&query, channel_label.as_deref());
                let r = self.jarvis_ask(context, &query, author_id).await;
                (format!("/ask {}", query), r)
            }
            JC::Summarize => {
                self.jarvis_state().add_user_message("/summarize", channel_label.as_deref());
                let r = self.jarvis_summarize_conversation(context).await;
                ("/summarize".to_string(), r)
            }
            JC::Knowledge(entity) => {
                let label = format!("/knowledge {}", entity);
                self.jarvis_state().add_user_message(&label, channel_label.as_deref());
                let query = format!("What do you know about {}?", entity);
                let r = self.jarvis_ask(context, &query, author_id).await;
                (label, r)
            }
            JC::Remember(fact) => {
                let label = format!("/remember {}", fact);
                self.jarvis_state().add_user_message(&label, channel_label.as_deref());
                let query = format!("Please remember this fact: {}", fact);
                let r = self.jarvis_ask(context, &query, author_id).await;
                (label, r)
            }
        };

        match &result {
            Ok(response) => {
                self.jarvis_state().add_assistant_message(response);
            }
            Err(e) => {
                self.jarvis_state().add_assistant_message(&format!("Error: {}", e));
            }
        }

        (query_display, result)
    }

    pub async fn jarvis_summarize_conversation(
        &self,
        context: &crate::domain::jarvis::JarvisContext,
    ) -> Result<String, crate::services::jarvis::JarvisError> {
        self.jarvis().summarize_conversation(context).await
    }

}

/// `(statement, leaves_device)` for the AI channel on `silo`.
fn jarvis_privacy_for(silo: crate::ai_policy::policy::Silo, engine: &str) -> (String, bool) {
    use crate::ai_policy::policy::Silo;
    match silo {
        Silo::Local => (format!("Local AI · {engine} · never leaves this device"), false),
        Silo::Cloud => ("Cloud AI · messages are sent to your cloud AI provider".to_string(), true),
    }
}

#[cfg(test)]
mod tests {
    use super::{jarvis_privacy_for, parse_jarvis_mention};
    use crate::ai_policy::policy::Silo;

    /// Only a local engine may say the messages stay here; cloud says where
    /// they go and flags that they leave the device.
    #[test]
    fn jarvis_privacy_line_follows_the_silo() {
        let (local, local_leaves) = jarvis_privacy_for(Silo::Local, "llama");
        assert!(local.contains("never leaves this device") && local.contains("llama"));
        assert!(!local_leaves);
        let (cloud, cloud_leaves) = jarvis_privacy_for(Silo::Cloud, "llama");
        assert!(cloud.contains("cloud AI provider") && !cloud.contains("never leaves"));
        assert!(cloud_leaves);
    }

    #[test]
    fn jarvis_mention_is_a_whole_word() {
        assert_eq!(parse_jarvis_mention("@Jarvis what time is it?"), Some("what time is it?"));
        assert_eq!(parse_jarvis_mention("Hey @jarvis, help me"), Some("help me"));
        assert_eq!(parse_jarvis_mention("  @JARVIS:  summarize this  "), Some("summarize this"));
        // An address or a longer word is not a mention.
        assert_eq!(parse_jarvis_mention("mail me@jarvis.io please"), None);
        assert_eq!(parse_jarvis_mention("@jarvis.io is down"), None);
        assert_eq!(parse_jarvis_mention("@jarvisbot do it"), None);
        // No query: nothing to ask, so the text is sent as written.
        assert_eq!(parse_jarvis_mention("@Jarvis"), None);
        assert_eq!(parse_jarvis_mention("@Jarvis   "), None);
        assert_eq!(parse_jarvis_mention("Hello world"), None);
        // A later whole-word mention still counts; multibyte text is safe.
        assert_eq!(parse_jarvis_mention("x@jarvis é @jarvis ok"), Some("ok"));
        assert_eq!(parse_jarvis_mention("é@"), None);
    }
}

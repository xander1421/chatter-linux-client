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
use crate::services::signal_service::SignalService;
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
        channel.description = Some("Local AI assistant — never leaves this device".to_string());
        channel
    }

    /// Human-readable description of the active Jarvis engine ("ollama (llama3.2:3b)").
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

    pub async fn jarvis_ask(
        &self,
        context: &crate::domain::jarvis::JarvisContext,
        query: &str,
        author_id: Option<&crate::domain::ids::UserId>,
    ) -> Result<String, crate::services::jarvis::JarvisError> {
        self.jarvis().ask(context, query, author_id).await
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

    pub fn jarvis_get_entity_knowledge(
        &self,
        entity_name: &str,
    ) -> Result<crate::domain::jarvis::EntityKnowledge, crate::services::jarvis::JarvisError> {
        self.jarvis().get_entity_knowledge(entity_name)
    }

    pub async fn jarvis_observe_message(
        &self,
        context: &crate::domain::jarvis::JarvisContext,
        message: &str,
        author_id: Option<&crate::domain::ids::UserId>,
        message_id: Option<&str>,
    ) -> Result<(), crate::services::jarvis::JarvisError> {
        self.jarvis()
            .observe_message(context, message, author_id, message_id)
            .await
    }

}

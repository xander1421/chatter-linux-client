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
use crate::services::typing_service::TypingService;
use crate::services::user::UserService;
use crate::services::conversation::ConversationHandle;
use crate::state::{
    ActiveConversation, AuthState, ChatState, JarvisState, MessageState, OfflineState,
    OrgState, PrivacyState, UserState,
};
use crate::state::chat_state::PendingAttachment;
use crate::storage::settings_keys;
use crate::system::Directories;
use super::{ChatterClient, ClientError};

impl ChatterClient {
    // ── Jarvis façade ────────────────────────────────────────────────────

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

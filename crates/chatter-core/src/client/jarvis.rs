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
#[cfg(feature = "desktop-media")]
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
    OrgState, PinnedState, PresenceState, PrivacyState, StreamingState, UIState, UserState,
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

    /// Spawn a background task that routes domain events to privacy-aware
    /// desktop notifications. Respects the active conversation (no notifications
    /// for the channel the user is currently viewing) and skips own messages.
    ///
    /// The task runs for the lifetime of the broadcast channel.
    pub fn start_notification_listener(self: &Arc<Self>) {
        let handle = crate::services::notification_service::spawn_notification_listener(
            self.event_bus().clone(),
            self.auth_state().clone(),
            self.chat_state().clone(),
            self.users().clone(),
            self.notifications().clone(),
        );
        self.tasks.lock().push(handle);
    }
}
